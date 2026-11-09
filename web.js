/**
 * MahSurat Stream Bot — web dashboard.
 *
 * Serves:
 *   GET /                          dashboard (login page if not authed)
 *   GET /auth/github               start GitHub OAuth
 *   GET /auth/github/callback      OAuth callback -> session + cookie
 *   GET /api/me                    current linked user
 *   GET /api/favorites             list favorites
 *   POST /api/favorites            add favorite {provider,id,title,year,type,video_url,thumbnail}
 *   DELETE /api/favorites/:provider/:id
 *   GET /logout
 *
 * Query param ?telegram_id= on /auth/github links the Telegram user to the GitHub account
 * in the bot's pending-link map (set via a shared in-memory bridge exposed by bot.js).
 */

require('dotenv').config();
const express = require('express');
const session = require('express-session');
const axios = require('axios');
const path = require('path');

const {
  githubAuthUrl, exchangeCode, githubUser, getOrCreateUser,
  getUser, getUserFavorites, addFavorite, removeFavorite, users,
} = require('./storage/github');

const app = express();
const WEB_URL = process.env.WEB_URL || 'http://127.0.0.1:3000';
const SESSION_SECRET = process.env.SESSION_SECRET || 'dev-secret-change-me';
const PORT = process.env.PORT || 3000;

// Bridge set by bot.js after launch: telegramId -> githubId
global.__mahsurat_linkBridge = global.__mahsurat_linkBridge || new Map();

app.use(express.static(path.join(__dirname, 'public')));
app.use(express.json());
app.use(session({
  name: 'mahsurat_sid',
  secret: SESSION_SECRET,
  resave: false,
  saveUninitialized: false,
  cookie: { httpOnly: true, sameSite: 'lax', secure: WEB_URL.startsWith('https') },
}));

/* ---------- Auth ---------- */

app.get('/auth/github', (req, res) => {
  const telegramId = req.query.telegram_id;
  if (telegramId) global.__mahsurat_linkBridge.set(telegramId, 'pending:' + telegramId);
  const state = telegramId || 'dashboard';
  res.redirect(githubAuthUrl(`${WEB_URL}/auth/github/callback`, 'read:user user:email') + `&state=${encodeURIComponent(state)}`);
});

app.get('/auth/github/callback', async (req, res) => {
  const { code, state } = req.query;
  const telegramId = state && state.startsWith('pending:') ? state.slice(8) : null;
  try {
    const tokenData = await exchangeCode(code, `${WEB_URL}/auth/github/callback`);
    const accessToken = tokenData.access_token;
    const ghUser = await githubUser(accessToken);
    const user = getOrCreateUser(ghUser.id, ghUser.login, accessToken);

    req.session.user = { githubId: ghUser.id, login: ghUser.login, token: accessToken };

    // Link Telegram user <-> GitHub user if a pending link exists
    if (telegramId && global.__mahsurat_linkBridge.has(telegramId)) {
      global.__mahsurat_linkBridge.set(telegramId, ghUser.id);
    }

    res.redirect('/dashboard');
  } catch (err) {
    console.error('[oauth callback]', err.message);
    res.redirect('/?error=oauth_failed');
  }
});

app.get('/logout', (req, res) => {
  req.session.destroy(() => res.redirect('/'));
});

app.get('/api/me', (req, res) => {
  res.json(req.session.user || null);
});

app.get('/api/favorites', (req, res) => {
  if (!req.session.user) return res.status(401).json({ error: 'not authenticated' });
  res.json(getUserFavorites(req.session.user.githubId));
});

app.post('/api/favorites', (req, res) => {
  if (!req.session.user) return res.status(401).json({ error: 'not authenticated' });
  const item = req.body;
  if (!item || !item.provider || !item.id) return res.status(400).json({ error: 'invalid item' });
  const favs = addFavorite(req.session.user.githubId, {
    ...item, login: req.session.user.login, token: req.session.user.token,
  });
  res.json(favs);
});

app.delete('/api/favorites/:provider/:id', (req, res) => {
  if (!req.session.user) return res.status(401).json({ error: 'not authenticated' });
  const favs = removeFavorite(req.session.user.githubId, req.params.provider, decodeURIComponent(req.params.id));
  res.json(favs);
});

/* ---------- Pages ---------- */

app.get('/', (req, res) => {
  if (req.session.user) return res.redirect('/dashboard');
  res.sendFile(path.join(__dirname, 'public', 'index.html'));
});

app.get('/dashboard', (req, res) => {
  if (!req.session.user) return res.redirect('/');
  res.sendFile(path.join(__dirname, 'public', 'dashboard.html'));
});

app.listen(PORT, '127.0.0.1', () => {
  console.log(`🌐 Dashboard: ${WEB_URL}`);
});

module.exports = app;
