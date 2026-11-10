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
