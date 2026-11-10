#!/usr/bin/env bash
# MahSurat Stream Bot installer (Termux/Android, Linux)
set -euo pipefail

REPO="__REPO__"
BASE="https://github.com/$REPO/releases/download/bot-latest"
BIN="moviebox-tgbot"

OS="$(uname -s)"; ARCH="$(uname -m)"
if [ -n "${PREFIX:-}" ] && [[ "$PREFIX" == *com.termux* ]]; then
  [ "$ARCH" = "aarch64" ] || [ "$ARCH" = "arm64" ] || { echo "Termux: only arm64 is supported"; exit 1; }
  FILE="MovieBoxBot_Android_arm64.tar.gz"; DIR="$PREFIX/bin"; TERMUX=1
else
  TERMUX=0; DIR="$HOME/.local/bin"
  [ "$OS" = "Linux" ] || { echo "Unsupported OS: $OS (use the Windows installer on Windows)"; exit 1; }
  case "$ARCH" in
    x86_64|amd64) FILE="MovieBoxBot_Linux_x64.tar.gz" ;;
    aarch64|arm64) FILE="MovieBoxBot_Linux_arm64.tar.gz" ;;
    *) echo "Unsupported CPU: $ARCH"; exit 1 ;;
  esac
fi

mkdir -p "$DIR"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
echo "> Downloading $FILE"
curl -fsSL "$BASE/$FILE" -o "$TMP/bot.tar.gz"
tar -xzf "$TMP/bot.tar.gz" -C "$TMP"
install -m 755 "$TMP/$BIN" "$DIR/$BIN"
echo "+ Installed $DIR/$BIN"

# cloudflared = free tunnel used by the in-Telegram player
if ! command -v cloudflared >/dev/null 2>&1 && [ ! -x "$DIR/cloudflared" ]; then
  if [ "$TERMUX" = 1 ]; then
    pkg install -y cloudflared || echo "! could not install cloudflared (player buttons will be disabled)"
  else
    CF="cloudflared-linux-amd64"; [ "$ARCH" = "aarch64" ] || [ "$ARCH" = "arm64" ] && CF="cloudflared-linux-arm64"
    curl -fsSL "https://github.com/cloudflare/cloudflared/releases/latest/download/$CF" -o "$DIR/cloudflared" && chmod +x "$DIR/cloudflared" \
      || echo "! could not download cloudflared (player buttons will be disabled)"
  fi
fi

case ":$PATH:" in *":$DIR:"*) ;; *) echo "! add $DIR to your PATH:  export PATH=\"$DIR:\$PATH\"" ;; esac
[ "$TERMUX" = 1 ] && command -v termux-wake-lock >/dev/null 2>&1 && termux-wake-lock || true

echo
echo "Done. Start the bot with:  $BIN"
echo "(first run asks for your bot token, then remembers it)"
