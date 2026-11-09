# MahSurat Stream Bot — Project Notes

## How to run
- `npm install` — install dependencies
- `cp .env.example .env` — then fill in BOT_TOKEN, BOT_USERNAME, GitHub OAuth creds
- `npm run dev` — runs bot + dashboard together
- `npm run start` — bot only; `npm run web` — dashboard only

## GitHub Actions secrets (configure in repo Settings → Secrets and variables → Actions)
Required:
- `BOT_TOKEN` — from @BotFather (you already added this)
- `BOT_USERNAME` — your bot username without @
- `WEB_URL` — your deployed dashboard URL
- `SESSION_SECRET` — random string for express sessions
- `GITHUB_CLIENT_ID` — from GitHub OAuth app settings
- `GITHUB_CLIENT_SECRET` — from GitHub OAuth app settings
- `GITHUB_CALLBACK_URL` — `https://your-app.onrender.com/auth/github/callback`

Optional:
- `GITHUB_TOKEN` — bot-owned GitHub token (gist scope) for backing up favorites to secret Gists
- `SSH_HOST`, `SSH_USER`, `SSH_KEY`, `SSH_PORT`, `SSH_WORKDIR` — for SSH deploy workflow

## Bot commands
- `/search <query>` — search all providers, merged results
- `/favorites` — view your GitHub-synced watchlist
- `/github` — link Telegram account to GitHub via OAuth
- `/help` — usage info
- Inline: `@bot <query>` — search from any chat

## Play behavior
- `▶️ Play` sends the video via `replyWithVideo({ supports_streaming: true })` — opens in Telegram's built-in video player, works on mobile
- "External player" button opens the raw video URL in the device's default video player

## Providers
- `providers/mock.js` — demo provider (works out of the box)
- `providers/dramachi.js`, `bdix.js`, `moviebox.js` — scraper stubs (best-effort, add real selectors)
- `providers/index.js` — `searchAllProviders()` merges + deduplicates results from all providers

## Deployment
- `.github/workflows/bot.yml` — runs bot + dashboard in Actions (6-hour max per run)
- `.github/workflows/deploy.yml` — deploys to your server via SSH (optional)
- `Procfile` — for Heroku/Render-style deployment
