#!/usr/bin/env bash
# One-time (idempotent) gallery-dl setup for the igbot VM.
#
#   sudo bash deploy/install-gallery-dl.sh
#
# Why: Instagram *image* stories are fetched only by gallery-dl — yt-dlp's story
# extractor is video-only. gallery-dl publishes no standalone Linux binary, so it
# lives in its own venv, which works the same on Ubuntu 22.04 and on PEP 668
# "externally managed" 24.04. setup.sh calls this on fresh installs; a box set up
# before v0.4.0 runs it once by hand. Re-running just upgrades gallery-dl.
#
# Also refreshes the daily updater unit from the repo so gallery-dl is kept
# current alongside yt-dlp.
set -euo pipefail

VENV=/opt/gallery-dl
LINK=/usr/local/bin/gallery-dl
UNIT=yt-dlp-update.service
HERE=$(cd "$(dirname "$0")" && pwd)

if [[ $EUID -ne 0 ]]; then echo "run as root: sudo bash $0" >&2; exit 1; fi

if [[ -x "$VENV/bin/gallery-dl" ]]; then
  echo "==> gallery-dl already in $VENV — upgrading in place"
else
  echo "==> installing gallery-dl into $VENV"
  apt-get update -y >/dev/null
  apt-get install -y python3-venv
  python3 -m venv "$VENV"
fi
"$VENV/bin/pip" install --quiet --upgrade pip gallery-dl
ln -sf "$VENV/bin/gallery-dl" "$LINK"
echo "==> gallery-dl $("$LINK" --version | head -n 1)   ($LINK → $VENV)"

# Keep the daily updater in sync with the repo's unit (it gained a gallery-dl line).
if [[ -f "$HERE/$UNIT" ]] && ! cmp -s "$HERE/$UNIT" "/etc/systemd/system/$UNIT"; then
  echo "==> refreshing /etc/systemd/system/$UNIT from the repo"
  cp "$HERE/$UNIT" "/etc/systemd/system/$UNIT"
  systemctl daemon-reload
fi

cat <<EOF

Done. Stories still need a cookies file (README → Cookies; IG_COOKIES_PATH).
If igbot is already running with cookies set, restart it so the startup log
shows "gallery-dl found":
  sudo systemctl restart igbot
EOF
