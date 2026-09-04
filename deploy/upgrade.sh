#!/usr/bin/env bash
# igbot release upgrade for the deployed VM (see README "Upgrading to a new release").
#
#   sudo bash deploy/upgrade.sh              # newest GitHub release
#   sudo bash deploy/upgrade.sh v0.4.0       # a specific tag — also how you roll back
#   sudo bash deploy/upgrade.sh --dry-run    # resolve + download + verify + compare only
#
# What it does, in order:
#   1. resolve the tag (GitHub redirects /releases/latest → /releases/tag/<tag>)
#   2. download the tarball + .sha256 into a private temp dir and verify — your
#      current directory never matters, and a bad checksum refuses to install
#   3. byte-compare with the installed binary; identical → "already current", exit 0
#   4. back up → stop → install as botuser → start
#   5. wait for the service to be active and log "config loaded"; if it doesn't,
#      restore the backup, start the old binary, and exit non-zero
#
# Out of scope on purpose: one-time prerequisites a release may add. For the
# v0.4.0 gallery-dl requirement run deploy/install-gallery-dl.sh once.
set -euo pipefail

REPO=sailself/instagram_tg_bot
APP_DIR=/opt/igbot
BIN="$APP_DIR/igbot"
BOT_USER=botuser
SERVICE=igbot
HEALTH_WAIT_SECS="${HEALTH_WAIT_SECS:-20}"

usage() { sed -n '2,18p' "$0"; }
log() { printf '\n==> %s\n' "$*"; }
die() { printf '\nERROR: %s\n' "$*" >&2; exit 1; }

DRY_RUN=0
TAG=""
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    -h|--help) usage; exit 0 ;;
    v[0-9]*) TAG="$arg" ;;
    [0-9]*) TAG="v$arg" ;;
    *) usage; die "unknown argument: $arg" ;;
  esac
done

if [[ $DRY_RUN -eq 0 && $EUID -ne 0 ]]; then
  die "run as root: sudo bash $0 $*"
fi
for tool in curl tar sha256sum cmp mktemp; do
  command -v "$tool" >/dev/null 2>&1 || die "missing tool: $tool"
done
if [[ $DRY_RUN -eq 0 ]]; then
  systemctl cat "$SERVICE" >/dev/null 2>&1 \
    || die "$SERVICE.service is not installed — run deploy/setup.sh first"
fi

# 1. resolve the tag
if [[ -z "$TAG" ]]; then
  final=$(curl -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest") \
    || die "could not reach GitHub to resolve the latest release"
  TAG="${final##*/}"
  [[ "$TAG" == v[0-9]* ]] || die "unexpected latest-release URL: $final"
fi
log "target release: $TAG"

# 2. fetch + verify in a private temp dir
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
base="igbot-$TAG-linux-x86_64.tar.gz"
url="https://github.com/$REPO/releases/download/$TAG"
log "downloading $base"
curl -fsSL -o "$WORK/$base" "$url/$base" \
  || die "download failed — does release $TAG exist and ship a linux x86_64 asset?"
curl -fsSL -o "$WORK/$base.sha256" "$url/$base.sha256" \
  || die "checksum file missing for $TAG"
( cd "$WORK" && sha256sum -c --quiet "$base.sha256" ) \
  || die "checksum mismatch — refusing to install"
tar -xzf "$WORK/$base" -C "$WORK"
[[ -f "$WORK/igbot" ]] || die "tarball did not contain an 'igbot' binary"
chmod 0755 "$WORK/igbot"
log "verified $TAG: sha256 $(sha256sum "$WORK/igbot" | cut -c1-16)…  size $(du -h "$WORK/igbot" | cut -f1)"

# 3. already current?
if [[ -f "$BIN" ]] && cmp -s "$WORK/igbot" "$BIN"; then
  log "$BIN is already $TAG — nothing to do"
  exit 0
fi
if [[ -f "$BIN" ]]; then
  log "installed binary differs (sha256 $(sha256sum "$BIN" | cut -c1-16)…) — will upgrade"
else
  log "no binary at $BIN yet — will install"
fi

if [[ $DRY_RUN -eq 1 ]]; then
  log "dry run — would now: back up $BIN → $BIN.bak, stop $SERVICE, install $TAG," \
      "start, health-check for ${HEALTH_WAIT_SECS}s, roll back on failure"
  exit 0
fi

# 4. swap. Stop first: replacing a *running* executable fails with "Text file busy".
if [[ -f "$BIN" ]]; then
  log "backing up current binary → $BIN.bak"
  cp -a "$BIN" "$BIN.bak"
fi
log "stopping $SERVICE"
systemctl stop "$SERVICE"
log "installing $TAG → $BIN"
install -o "$BOT_USER" -g "$BOT_USER" -m 0755 "$WORK/igbot" "$BIN"
start_at=$(date +%s)
log "starting $SERVICE"
systemctl start "$SERVICE"

# 5. health check: active *and* past config load, or roll back
recent_log() { journalctl -u "$SERVICE" --since "@$start_at" --no-pager -q 2>/dev/null || true; }
healthy=0
for ((i = 0; i < HEALTH_WAIT_SECS; i++)); do
  sleep 1
  systemctl is-active --quiet "$SERVICE" || break
  if recent_log | grep -q 'config loaded'; then
    healthy=1
    break
  fi
done

if [[ $healthy -eq 1 ]]; then
  log "$SERVICE is up on $TAG"
  recent_log | grep -Ei 'config loaded|extractor chain|found|not runnable' || true
  printf '\nDone. Previous binary kept at %s.bak — roll back with: sudo bash %s <previous tag>\n' "$BIN" "$0"
  exit 0
fi

log "$SERVICE did not come up healthy within ${HEALTH_WAIT_SECS}s — last log lines:"
recent_log | tail -n 20
systemctl stop "$SERVICE" || true
if [[ -f "$BIN.bak" ]]; then
  log "rolling back to the previous binary"
  install -o "$BOT_USER" -g "$BOT_USER" -m 0755 "$BIN.bak" "$BIN"
  systemctl start "$SERVICE" || true
  die "rolled back; $SERVICE is now $(systemctl is-active "$SERVICE" || true) on the previous binary"
fi
die "no backup to roll back to; $SERVICE is stopped"
