#!/usr/bin/env bash
# igbot deploy bootstrap for OCI Always Free Ubuntu (PLAN §7).
# Run as root from the repo root, AFTER building the release binary:
#   cargo build --release && sudo bash deploy/setup.sh
# Optionally pass the binary path as $1 (default: target/release/igbot).
set -euo pipefail

BOT_USER=botuser
APP_DIR=/opt/igbot
ETC_DIR=/etc/igbot
BIN_SRC="${1:-target/release/igbot}"

if [[ $EUID -ne 0 ]]; then echo "run as root (sudo)"; exit 1; fi
if [[ ! -f "$BIN_SRC" ]]; then echo "binary not found at $BIN_SRC — build it first"; exit 1; fi

echo "[1/7] packages (ffmpeg, curl)"
apt-get update -y
apt-get install -y ffmpeg curl ca-certificates

echo "[2/7] yt-dlp standalone binary"
curl -L https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp -o /usr/local/bin/yt-dlp
chmod a+rx /usr/local/bin/yt-dlp
/usr/local/bin/yt-dlp --version

echo "[3/7] 2 GB swap + swappiness=10"
if ! swapon --show | grep -q '/swapfile'; then
  fallocate -l 2G /swapfile || dd if=/dev/zero of=/swapfile bs=1M count=2048
  chmod 600 /swapfile
  mkswap /swapfile
  swapon /swapfile
  grep -q '/swapfile' /etc/fstab || echo '/swapfile none swap sw 0 0' >> /etc/fstab
fi
echo 'vm.swappiness=10' > /etc/sysctl.d/99-swap.conf
sysctl --system >/dev/null

echo "[4/7] service user + dirs"
id -u "$BOT_USER" >/dev/null 2>&1 || useradd --system --create-home --shell /usr/sbin/nologin "$BOT_USER"
install -d -o "$BOT_USER" -g "$BOT_USER" "$APP_DIR" "$APP_DIR/tmp"
install -d "$ETC_DIR"

echo "[5/7] install binary"
install -o "$BOT_USER" -g "$BOT_USER" -m 0755 "$BIN_SRC" "$APP_DIR/igbot"

echo "[6/7] env file"
if [[ ! -f "$ETC_DIR/igbot.env" ]]; then
  cat > "$ETC_DIR/igbot.env" <<'EOF'
TELEGRAM_BOT_TOKEN=PUT_TOKEN_HERE
ALLOWED_CHAT_IDS=
TEMP_DIR=/opt/igbot/tmp
YT_DLP_PATH=/usr/local/bin/yt-dlp
RUST_LOG=igbot=info,warn
EOF
  chmod 600 "$ETC_DIR/igbot.env"
  chown "$BOT_USER:$BOT_USER" "$ETC_DIR/igbot.env"
  echo "  >>> EDIT $ETC_DIR/igbot.env: set TELEGRAM_BOT_TOKEN and ALLOWED_CHAT_IDS"
fi

echo "[7/7] systemd units"
cp deploy/igbot.service deploy/yt-dlp-update.service deploy/yt-dlp-update.timer \
   deploy/keepalive.service deploy/keepalive.timer /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now yt-dlp-update.timer keepalive.timer
systemctl enable igbot.service

echo
echo "Done. Next:"
echo "  1) edit $ETC_DIR/igbot.env (token + allowed chat ids)"
echo "  2) sudo systemctl start igbot"
echo "  3) journalctl -u igbot -f"
