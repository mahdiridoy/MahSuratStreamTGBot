# MovieBox Telegram Bot (GitHub Actions)

1. Put this project in your GitHub repo (replace the repo contents with this folder).
2. Repo -> Settings -> Secrets and variables -> Actions
   - Secret `BOT_TOKEN` = token from @BotFather  (already added)
   - Variable `ALLOWED_CHAT_IDS` (optional) = comma-separated chat ids that may use the bot.
     Send `/id` to the bot to see yours.
3. Actions tab -> "Telegram Bot" -> Run workflow (it then restarts itself every 6 hours).

Usage: send a title -> choose provider -> title -> season/episode -> audio -> quality -> links.

Notes
- BDIX providers (CircleFTP, DhakaFlix) only work from a BDIX network, so they show as unreachable on GitHub runners.
- Bot state is kept in memory; after a 6-hour restart, old buttons say "Session expired" - just search again.
- Private repos have a monthly free Actions-minutes limit; a 24/7 bot needs a public repo or a real server.
- Public repos: GitHub disables scheduled workflows after 60 days without repo activity; re-enable or push a commit.

## Playing videos (nothing to set up)
The bot starts its own player/stream server and a free Cloudflare quick tunnel inside the GitHub runner.
Each link gets three buttons:
- Play in Telegram: opens a Telegram Mini App with a video player (works best for MP4/H.264).
- MX Player / VLC: opens a page that auto-launches the app (Android). Needs the app installed.
Because the runner fetches the stream with the right headers, "ACCESS DENIED" no longer happens.
Links are valid while the bot run is alive (up to ~6h), then search again.

## Auto update
Every push to `src/**`, `Cargo.toml` or the workflow file restarts the bot automatically with the new code
(the old run is cancelled). It also restarts itself every 6 hours.

## Run it on your own device (fixes BDIX)
BDIX providers (CircleFTP, DhakaFlix) only answer on a BDIX connection, so GitHub servers can never reach them.
Run the same bot on your BDIX PC / phone with the installers (the "Bot Release" workflow builds them automatically;
the release page "Telegram Bot (latest)" shows the exact commands for your repo):

Windows:  irm https://github.com/<OWNER>/<REPO>/releases/download/bot-latest/install-bot.ps1 | iex
Termux:   pkg update && pkg install -y curl tar
          curl -fsSL https://github.com/<OWNER>/<REPO>/releases/download/bot-latest/install-bot.sh | bash
Scoop:    scoop bucket add moviebox-bot https://github.com/<OWNER>/<REPO>
          scoop install moviebox-tgbot
Then run `moviebox-tgbot` (asks for the token once, remembers it).

If both the GitHub copy and your device run at once, your device wins (the GitHub copy backs off for 5 minutes).
