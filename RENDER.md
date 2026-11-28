# Run the bot 24/7 on Render (no restarts)

The bot used to run on GitHub Actions, which stops every 6 hours. On Render it runs
all the time. UptimeRobot keeps the free Render service awake.

## Before you start
1. Your GitHub repo must be **public** (Render downloads the ready-made bot from the
   repo's "bot-latest" release).
2. Open the repo's **Releases** page and check that **Telegram Bot (latest)** exists with
   `MovieBoxBot_Linux_x64.tar.gz`. If not, run Actions > **Bot Release** > Run workflow.
3. Stop the GitHub copy of the bot (two copies fight over the same Telegram bot):
   Actions > **Telegram Bot** > cancel any running job. It is now manual-only.

## 1. Create the service on Render
1. render.com > **New +** > **Blueprint** (or **Web Service**) > connect this GitHub repo.
2. Blueprint: Render reads `render.yaml` for you. Web Service: Language **Docker**,
   Instance type **Free**, Health Check Path `/health`, Auto-Deploy **No**.
3. Add the environment variables:
   - `BOT_TOKEN` = your token from @BotFather
   - `ALLOWED_CHAT_IDS` = your chat ids, separated by commas (leave empty for everyone)
4. **Deploy**. When the log shows `Bot started (hosted)`, send the bot a movie name.

Render gives the service an address like `https://mahsurat-stream-bot.onrender.com`.
The bot uses it for Play / Download, so no tunnel is needed.

## 2. Keep it awake with UptimeRobot
1. uptimerobot.com > **Add New Monitor**.
2. Monitor Type **HTTP(s)**, URL `https://YOUR-SERVICE.onrender.com/health`,
   Monitoring Interval **5 minutes**.
3. Add your email so you are told if the bot goes down.

The bot also pings itself every 4 minutes, so there are two keep-awake layers.

## 3. Auto-update when you push code (optional)
1. Render > your service > **Settings** > **Deploy Hook** > copy the URL.
2. GitHub repo > **Settings** > **Secrets and variables** > **Actions** > New secret:
   name `RENDER_DEPLOY_HOOK`, value = that URL.

After that, each push that changes the bot builds the new binary, publishes it, and then
tells Render to redeploy (about 1 minute). Without the hook, click **Manual Deploy** on Render.

## Good to know
- **Free plan limits:** 512 MB memory, and a monthly outbound bandwidth allowance
  (check Render's pricing page for the current number). Play and Download traffic goes
  through the bot, so heavy downloading uses it up.
- **Restarts:** Render restarts services now and then, and on every deploy (including
  every time you push new code). Open search results and unlocked links are kept in memory,
  so they are lost on a restart. Before it stops, the bot edits the messages that still show
  a countdown to say "The bot was restarted", and users just tap a quality again or search
  again. Avoid deploying while people are watching.
- **One copy only:** run the bot in one place at a time (Render, GitHub or your PC).
