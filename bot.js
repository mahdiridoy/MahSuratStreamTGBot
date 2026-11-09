/**
 * MahSurat Stream Bot — Telegram bot entry point.
 *
 * Commands:
 *   /start        welcome + inline-mode hint
 *   /search <q>   search all providers in the current chat
 *   /favorites    your GitHub-synced watchlist (private chat)
 *   /github       link your GitHub account (opens dashboard link)
 *   /help         help
 *
 * Inline mode: @bot <query> -> results from ALL providers with a "▶️ Play" button.
 * Play -> sends the video via Telegram's built-in video player (supports_streaming).
 */

require('dotenv').config();
const { Telegraf, Markup } = require('telegraf');
const { searchAllProviders, getProviderByName, listProviders } = require('./providers');
const {
  githubAuthUrl, exchangeCode, githubUser, getOrCreateUser,
  getUser, getUserFavorites, addFavorite, removeFavorite,
} = require('./storage/github');

const BOT_TOKEN = process.env.BOT_TOKEN;
const BOT_USERNAME = process.env.BOT_USERNAME || 'mahsurat_stream_bot';
const WEB_URL = process.env.WEB_URL || 'http://127.0.0.1:3000';

if (!BOT_TOKEN) {
  console.error('FATAL: BOT_TOKEN is not set. Copy .env.example to .env and fill it in.');
  process.exit(1);
}

const bot = new Telegraf(BOT_TOKEN);

// In-memory link: telegram user id -> github id (shared with web.js)
global.__mahsurat_linkBridge = global.__mahsurat_linkBridge || new Map();
const pendingLink = global.__mahsurat_linkBridge;

function isPrivate(ctx) {
  return ctx.chat && ctx.chat.type === 'private';
}

const helpText = `
🎬 *MahSurat Stream Bot*

Search across ALL providers and play videos in Telegram's built-in video player (works on mobile too).

Commands:
• /search <query> — search every provider
• /favorites — your GitHub-synced watchlist
• /github — link GitHub account
• /help — this message

Inline: @${BOT_USERNAME} <query> — pick a result, tap ▶️ Play.
`;

bot.start(ctx => {
  ctx.reply(
    `👋 Welcome to *MahSurat Stream Bot*!\n\n${helpText}`,
    { parse_mode: 'Markdown', disable_web_page_preview: true }
  );
});

bot.help(ctx => ctx.reply(helpText, { parse_mode: 'Markdown', disable_web_page_preview: true }));

bot.command('search', async ctx => {
  const query = (ctx.message.text || '').replace(/^\/search\s*/i, '').trim();
  if (!query) {
    return ctx.reply('Usage: /search <movie or series name>');
  }
  await showSearchResults(ctx, query);
});

bot.command('github', async ctx => {
  if (!isPrivate(ctx)) {
    return ctx.reply('Link GitHub in private chat with the bot.');
  }
  const url = `${WEB_URL}/auth/github?telegram_id=${ctx.from.id}`;
  ctx.reply(
    `🔗 Link your GitHub account to sync your watchlist:\n${url}`,
    { disable_web_page_preview: true }
  );
});

bot.command('favorites', async ctx => {
  if (!isPrivate(ctx)) {
    return ctx.reply('Open favorites in private chat with the bot.');
  }
  const githubId = global.__mahsurat_linkBridge.get(ctx.from.id);
  const user = githubId ? getUser(githubId) : null;
  const favs = user ? getUserFavorites(githubId) : [];
  if (!favs.length) {
    return ctx.reply('Your watchlist is empty. Search and tap ⭐ "Add to favorites" on any result.');
  }
  const text = favs.map((f, i) =>
    `${i + 1}. *${f.title}* (${f.provider}) ${f.year ? `• ${f.year}` : ''}`
  ).join('\n');
  ctx.reply(`⭐ *Your watchlist*\n\n${text}`, { parse_mode: 'Markdown' });
});

/**
 * Render search results as messages with Play buttons.
 */
async function showSearchResults(ctx, query, editMessageId) {
  const results = await searchAllProviders(query);
  if (!results.length) {
    const text = `🔍 No results for “*${query}*” across all providers.`;
    if (editMessageId) return ctx.editMessageText(text, { parse_mode: 'Markdown' });
    return ctx.reply(text, { parse_mode: 'Markdown' });
  }

  const header = `🔍 *Results for “*${query}*”* (${results.length} providers)`;
  if (editMessageId) {
    return ctx.editMessageText(header, { parse_mode: 'Markdown' });
  }

  // Send in chunks so long lists don't hit message limits
  for (let i = 0; i < results.length; i += 10) {
    const chunk = results.slice(i, i + 10);
    await ctx.reply(
      chunk.map(r =>
        `🎬 *${r.title}* ${r.year ? `(${r.year})` : ''}\n📡 ${r.provider} • ${r.type}\n${r.description ? r.description.slice(0, 120) : ''}`
      ).join('\n\n'),
      {
        parse_mode: 'Markdown',
        reply_markup: Markup.inlineKeyboard(
          chunk.map(r => [Markup.button.callback('▶️ Play', `play:${r.provider}:${encodeURIComponent(r.id)}`)])
        ),
        disable_web_page_preview: true,
      }
    );
  }
}

bot.on('message', async ctx => {
  const text = (ctx.message.text || '').trim();
  if (text.startsWith('/')) return; // commands handled above
  await showSearchResults(ctx, text);
});

/* ---------- Inline query: results from ALL providers ---------- */

bot.on('inline_query', async ctx => {
  const query = (ctx.inlineQuery.query || '').trim();
  if (!query) {
    return ctx.answerInlineQuery([
      {
        type: 'article',
        id: 'empty',
        title: 'Type a movie or series name...',
        description: 'Searches all providers',
        input_message_content: { message_text: '🔍 Send me a title to search all providers.' },
      },
    ], { cache_time: 1, is_personal: true });
  }

  try {
    const results = await searchAllProviders(query);
    const inlineResults = results.slice(0, 50).map(r => ({
      type: 'article',
      id: `${r.provider}:${encodeURIComponent(r.id)}`,
      title: r.title,
      description: `${r.provider} • ${r.year || ''} ${r.type || ''}`,
      thumb_url: r.thumbnail,
      input_message_content: {
        message_text: `🎬 *${r.title}*\n📡 ${r.provider} • ${r.year || '?'}\n${r.description || ''}\n\nTap ▶️ Play below to watch in Telegram video player.`,
        parse_mode: 'Markdown',
        disable_web_page_preview: true,
      },
      reply_markup: Markup.inlineKeyboard([
        Markup.button.callback('▶️ Play', `play:${r.provider}:${encodeURIComponent(r.id)}`),
        r.video_url ? Markup.button.url('📱 External player', r.video_url) : null,
      ].filter(Boolean)),
    }));

    await ctx.answerInlineQuery(inlineResults, { cache_time: 30, is_personal: true });
  } catch (err) {
    console.error('[inline_query]', err);
    await ctx.answerInlineQuery([], { cache_time: 1 });
  }
});

/* ---------- Play: Telegram video player ---------- */

bot.action(/^play:(.+):(.+)$/, async ctx => {
  const [, provider, id] = ctx.match;
  const decId = decodeURIComponent(id);
  await ctx.answerCbQuery('Fetching stream…');

  const prov = getProviderByName(provider);
  if (!prov) {
    return ctx.editMessageText('❌ Provider not available.');
  }

  try {
    const stream = await prov.getStream(decId);
    if (!stream || !stream.video_url) {
      return ctx.editMessageText('❌ No playable stream found for this title.');
    }

    const caption = `🎬 *${stream.title}*\n📡 ${provider} ${stream.quality ? `• ${stream.quality}` : ''}\n▶️ Opens in Telegram video player (mobile supported)`;

    await ctx.replyWithVideo(stream.video_url, {
      caption,
      parse_mode: 'Markdown',
      supports_streaming: true,
      reply_markup: Markup.inlineKeyboard([
        [Markup.button.url('📱 Open in external player', stream.video_url)],
        [
          Markup.button.callback('⭐ Add to favorites', `fav:add:${provider}:${encodeURIComponent(decId)}`),
          Markup.button.callback('➖ Remove', `fav:del:${provider}:${encodeURIComponent(decId)}`),
        ],
      ]),
    });

    try { await ctx.deleteMessage(); } catch (_) { /* ignore */ }
  } catch (err) {
    console.error('[play]', err);
    await ctx.editMessageText('❌ Failed to load stream. Try another provider/title.');
  }
});

/* ---------- Favorites callbacks ---------- */

bot.action(/^fav:add:(.+):(.+)$/, async ctx => {
  const [, provider, id] = ctx.match;
  const decId = decodeURIComponent(id);
  const githubId = global.__mahsurat_linkBridge.get(ctx.from.id);
  if (!githubId) {
    return ctx.answerCbQuery('Link your GitHub account first (/github).', true);
  }
  // Need the stream item to store title etc.; fetch via provider search is heavy,
  // so store a lightweight reference and enrich on demand.
  const prov = getProviderByName(provider);
  let title = decId, year = null, type = 'movie', video_url = '', thumbnail = '';
  try {
    const stream = await prov.getStream(decId);
    if (stream) {
      title = stream.title; year = null; type = stream.type;
      video_url = stream.video_url; thumbnail = stream.thumbnail || '';
    }
  } catch (_) { /* keep fallback */ }
  addFavorite(githubId, { provider, id: decId, title, year, type, video_url, thumbnail, login: '', token: '' });
  await ctx.answerCbQuery('⭐ Added to favorites');
});

bot.action(/^fav:del:(.+):(.+)$/, async ctx => {
  const [, provider, id] = ctx.match;
  const decId = decodeURIComponent(id);
  const githubId = global.__mahsurat_linkBridge.get(ctx.from.id);
  if (!githubId) {
    return ctx.answerCbQuery('Link your GitHub account first.', true);
  }
  removeFavorite(githubId, provider, decId);
  await ctx.answerCbQuery('➖ Removed from favorites');
});

bot.launch().then(() => {
  console.log(`🤖 MahSurat Stream Bot running. Providers: ${listProviders().join(', ')}`);
});

process.once('SIGINT', () => bot.stop('SIGINT'));
process.once('SIGTERM', () => bot.stop('SIGTERM'));
