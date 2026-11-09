/**
 * GitHub integration for MahSurat Stream Bot.
 *
 * - OAuth login URL for the web dashboard
 * * Exchange code for access token
 * - Per-user favorites stored in memory (and best-effort synced to a secret
 *   GitHub Gist when GITHUB_TOKEN is set)
 */

const axios = require('axios');

const CLIENT_ID = process.env.GITHUB_CLIENT_ID;
const CLIENT_SECRET = process.env.GITHUB_CLIENT_SECRET;
const GITHUB_TOKEN = process.env.GITHUB_TOKEN; // optional bot-owned token with gist scope

// In-memory store: githubId -> { login, token, favorites: [{provider,id,title,year,type,video_url,thumbnail}] }
const users = new Map();
// gistId -> githubId (created by this bot)
const gistByUser = new Map();

function githubAuthUrl(redirectUri, scope = 'read:user user:email') {
  const params = new URLSearchParams({
    client_id: CLIENT_ID,
    redirect_uri: redirectUri,
    scope,
    allow_signup: 'true',
  });
  return `https://github.com/login/oauth/authorize?${params.toString()}`;
}

async function exchangeCode(code, redirectUri) {
  const res = await axios.post(
    'https://github.com/login/oauth/access_token',
    { client_id: CLIENT_ID, client_secret: CLIENT_SECRET, code, redirect_uri: redirectUri },
    { headers: { Accept: 'application/json' } }
  );
  return res.data; // { access_token, scope, token_type }
}

async function githubUser(accessToken) {
  const res = await axios.get('https://api.github.com/user', {
    headers: { Authorization: `Bearer ${accessToken}`, 'User-Agent': 'MahSuratBot' },
  });
  return res.data; // { id, login, ... }
}

function getOrCreateUser(githubId, login, token) {
  let user = users.get(githubId);
  if (!user) {
    user = { githubId, login, token, favorites: [] };
    users.set(githubId, user);
  } else {
    user.login = login;
    user.token = token;
  }
  return user;
}

function getUser(githubId) {
  return users.get(githubId) || null;
}

function getUserFavorites(githubId) {
  const user = users.get(githubId);
  return user ? user.favorites : [];
}

function addFavorite(githubId, item) {
  const user = getOrCreateUser(githubId, item.login || '', item.token || '');
  const exists = user.favorites.some(
    f => f.provider === item.provider && f.id === item.id
  );
  if (!exists) {
    user.favorites.push({
      provider: item.provider,
      id: item.id,
      title: item.title,
      year: item.year,
      type: item.type,
      video_url: item.video_url,
      thumbnail: item.thumbnail,
      added_at: new Date().toISOString(),
    });
    syncGist(user);
  }
  return user.favorites;
}

function removeFavorite(githubId, provider, id) {
  const user = users.get(githubId);
  if (!user) return [];
  user.favorites = user.favorites.filter(f => !(f.provider === provider && f.id === id));
  syncGist(user);
  return user.favorites;
}

/**
 * Best-effort backup of favorites to a secret GitHub Gist per linked user.
 * Requires GITHUB_TOKEN (bot-owned, gist scope). Never fails hard.
 */
async function syncGist(user) {
  if (!GITHUB_TOKEN) return;
  try {
    const content = JSON.stringify({ login: user.login, favorites: user.favorites }, null, 2);
    const filename = `mahsurat-favorites-${user.githubId}.json`;
    const gistId = gistByUser.get(user.githubId);

    if (gistId) {
      await axios.patch(`https://api.github.com/gists/${gistId}`, {
        description: 'MahSurat Stream Bot favorites',
        files: { [filename]: { content } },
      }, {
        headers: { Authorization: `Bearer ${GITHUB_TOKEN}`, 'User-Agent': 'MahSuratBot' },
      });
    } else {
      const res = await axios.post(
        'https://api.github.com/gists',
        {
          description: 'MahSurat Stream Bot favorites',
          public: false,
          files: { [filename]: { content } },
        },
        {
          headers: { Authorization: `Bearer ${GITHUB_TOKEN}`, 'User-Agent': 'MahSuratBot' },
        }
      );
      gistByUser.set(user.githubId, res.data.id);
    }
  } catch (e) {
    console.warn('[github] gist sync skipped:', e.message);
  }
}

module.exports = {
  users,
  githubAuthUrl,
  exchangeCode,
  githubUser,
  getOrCreateUser,
  getUser,
  getUserFavorites,
  addFavorite,
  removeFavorite,
  syncGist,
};
