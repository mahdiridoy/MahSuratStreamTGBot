# MahSurat Stream Bot

Telegram bot that searches **all providers** and plays videos in Telegram's built-in video player (mobile-supported). Includes a web dashboard with GitHub OAuth to sync your watchlist.

## Features

- 🔍 `/search <query>` — searches every provider (mock, dramachi, bdix, moviebox) and merges results
- 🔁 Inline mode: `@mahsurat_stream_bot <query>` — results from all providers with ▶️ Play
- ▶️ Play — sends the video via `sendVideo` with `supports_streaming`, opens in Telegram video player (works on mobile phones)
- 📱 "Open in external player" link for any device's native video player
- 🐙 GitHub OAuth login + watchlist synced per GitHub account (optionally backed up to a secret GitHub Gist)
- 🌐 Web dashboard at `http://127.0.0.1:3000`

## Quick start

```bash
# 1. Install dependencies
npm install --no-audit --no-fund

# 2. Configure
cp .env.example .env
# edit .env: set BOT_TOKEN from @BotFather, GitHub OAuth app credentials, WEB_URL

# 3. Run bot + dashboard
npm start        # bot only
npm run web      # dashboard only
```

## Telegram setup

1. Message [@BotFather](https://t.me/botfather) → `/newbot`, name it `MahSurat Stream Bot`, pick username `mahsurat_stream_bot` (or update `BOT_USERNAME` in `.env`).
2. Copy the token into `BOT_TOKEN` in `.env`.
3. Optional: enable inline mode via BotFather `/setinline` so `@bot <query>` works in any chat.

## GitHub setup (watchlist sync)

1. Go to https://github.com/settings/developers → New OAuth App.
   - Homepage URL: `http://127.0.0.1:3000`
   - Authorization callback URL: `http://127.0.0.1:3000/auth/github/callback`
2. Copy Client ID / Client Secret into `GITHUB_CLIENT_ID` / `GITHUB_CLIENT_SECRET` in `.env`.
3. Optional: create a classic personal access token with `gist` scope and set `GITHUB_TOKEN` so favorites are backed up to a secret Gist per linked user.

## Deploy to GitHub

```bash
git init
git add .
git commit -m "Initial MahSurat Stream Bot"
# create a repo on GitHub, then:
git remote add origin https://github.com/YOU/mahsurat-stream-bot.git
git push -u origin main
```

Deploy the bot + dashboard to any Node host (Render, Railway, Koyeb, VPS). Set the same env vars and point `WEB_URL` at your deployed dashboard.

## Project structure

| File | Role |
|------|------|
| `bot.js` | Telegram bot (telegraf): commands, inline search, play → Telegram video player, favorites callbacks |
| `web.js` | Express dashboard + GitHub OAuth (login, callback, favorites API) |
| `providers/base.js` | Provider base class + `StreamItem`/`Stream` models |
| `providers/index.js` | Aggregates search results from ALL providers, deduplicates |
| `providers/mock.js` | Built-in sample provider (always works, no network needed) |
| `providers/dramachi.js`, `bdix.js`, `moviebox.js` | Best-effort real-site scraper stubs |
| `storage/github.js` | GitHub OAuth exchange, user/favorites store, secret Gist backup |
| `public/` | Landing page + dashboard (GitHub login, watchlist) |

## Providers

Add a new provider: extend `BaseProvider` in `providers/`, implement `search()` and `getStream()`, then register it in `providers/index.js`. One failing provider never breaks the search — errors are caught per provider.
