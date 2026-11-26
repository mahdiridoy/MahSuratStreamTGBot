# MahSurat Stream Bot on Render (or any Docker host).
# Nothing is compiled here: the ready-made Linux binary is downloaded from the
# "bot-latest" release that the "Bot Release" GitHub workflow builds.
FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl tar \
 && rm -rf /var/lib/apt/lists/*

# Your GitHub repository (owner/name). Change it here if you rename the repo.
ARG REPO=mahdiridoy/MahSuratStreamTGBot
# Render passes the commit id; it makes every deploy download the newest binary.
ARG RENDER_GIT_COMMIT=none

RUN echo "deploy ${RENDER_GIT_COMMIT}" \
 && curl -fsSL "https://github.com/${REPO}/releases/download/bot-latest/MovieBoxBot_Linux_x64.tar.gz" \
    | tar -xz -C /usr/local/bin \
 && chmod +x /usr/local/bin/moviebox-tgbot

RUN useradd --create-home bot
USER bot
ENV HOME=/home/bot

# Render sets PORT and RENDER_EXTERNAL_URL; BOT_TOKEN and ALLOWED_CHAT_IDS are
# added in the Render dashboard.
CMD ["moviebox-tgbot"]
