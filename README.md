# igbot

[![CI](https://github.com/sailself/instagram_tg_bot/actions/workflows/ci.yml/badge.svg)](https://github.com/sailself/instagram_tg_bot/actions/workflows/ci.yml)

A Rust Telegram bot that watches a group chat and, whenever someone posts an
**Instagram** link (post `/p/`, reel `/reel/`, `/tv/`) or a **Threads** link
(`threads.com` / `threads.net` `/@user/post/…`), replies to that message with the
post's **media (images/videos, incl. carousels), caption, and author** — and, for
Threads' text-first posts, the **text** itself when there's no media. Free to
run; designed for an **OCI Always Free 1 OCPU / 1 GB** VM.

## How it works

```
group msg ─(teloxide, long-poll)→ detect IG / Threads link → route by host
        → dedup → bounded queue → single worker → per-platform extractor chain
        → reply (album / photo / video / text)
```

Links are routed by **host** (never by shortcode — IG and Threads share the same
code alphabet), and dedup keys are namespaced per platform (`ig:` / `th:`).

**Instagram chain** (first backend that returns media wins):

1. **embed** — in-process, anonymous scrape of the public **post page** with a
   *crawler* User-Agent (`facebookexternalhit`, hot-config via
   `EMBED_USER_AGENT`). Current Instagram serves *browser* UAs a JS-only shell
   with no media, but serves *crawlers* the Polaris `application/json` blob
   (full media incl. reel video) plus Open Graph tags. Parses the JSON first
   (keyed per post by `code`), then OG/legacy shapes. No subprocess.
2. **yt-dlp** — subprocess fallback; the video workhorse (upstream-maintained).
3. **gallery-dl** — *only if* `IG_COOKIES_PATH` is set (images/carousels).
4. **external fallback** — *only if* `FALLBACK_PROVIDER` is set (Jina / EmbedEZ);
   fetches from a different IP when ours is blocked. Off by default.

**Threads chain** (neither yt-dlp nor gallery-dl supports Threads, so it's
in-process only):

1. **threads-json** — anonymous scrape of the public post page. Threads serves
   logged-out clients the full post JSON server-side (the same Polaris shape as
   Instagram) inside `<script type="application/json">` blocks — but *only* to a
   coherent **desktop-browser** header set (hot-config `THREADS_USER_AGENT` /
   `THREADS_SEC_CH_UA`); a naive UA gets an empty shell, which is treated as a
   failure, never a silent success. Covers images, video, carousels (up to 20),
   text-only / poll / link-card posts, and reposts/quotes.
2. **threads-embed** — fallback parse of the `/embed` SSR HTML card.

Cookieless-first: no Instagram **or** Threads login required for the default
chains.

## Prerequisites

- A bot token from **@BotFather**.
- **Runtime:** `ffmpeg` + `yt-dlp` (the standalone binary) — installed for you by
  `deploy/setup.sh` on the server.
- **To build from source:** Rust ≥ 1.85. (For deployment you can skip the build
  entirely and use the prebuilt release binary — see *Deploy* below.)

### BotFather setup (one-time, manual)

1. `/newbot` → get the token.
2. **`/setprivacy` → your bot → Disable.** Required so the bot sees normal group
   messages, not just commands.
3. Add the bot to your group, then **remove and re-add it** — the privacy change
   only takes effect on re-add. (Or make it a group admin.)
4. Put your group's chat id in `ALLOWED_CHAT_IDS` (see below).

## Run locally

```bash
cp .env.example .env          # set TELEGRAM_BOT_TOKEN (and ALLOWED_CHAT_IDS)
cargo run                     # long polling; no inbound ports needed
cargo test                    # unit tests
cargo clippy --all-targets    # must stay at 0 warnings
```

(`ffmpeg`/`yt-dlp` are only needed at runtime for the yt-dlp backend; the embed
backend works without them.)

**`ALLOWED_CHAT_IDS`** is a comma-separated list of integer chat ids, e.g.
`-1001234567890,-1009876543210` (supergroups start with `-100`). Empty = act in
any chat (logs a warning). To find yours: leave it empty, post an IG link, and
read the id off the `link detected … chat=…` log line, then set it and restart.

## Deploy to OCI (Ubuntu, x86-64, 1 GB)

Don't compile on the box — 1 GB struggles with the LTO release build. Use the
**prebuilt release binary** and let `deploy/setup.sh` do the rest. Grab the
latest from the [Releases page](https://github.com/sailself/instagram_tg_bot/releases).

```bash
# 0. confirm architecture — the release binary is x86-64 (E2.1.Micro free tier)
uname -m                                  # must print: x86_64

# 1. get the repo (for setup.sh + the systemd units) — no build needed
sudo apt-get update && sudo apt-get install -y git
git clone https://github.com/sailself/instagram_tg_bot.git
cd instagram_tg_bot

# 2. download + verify the release binary
cd /tmp
base=igbot-v0.2.1-linux-x86_64.tar.gz
url=https://github.com/sailself/instagram_tg_bot/releases/download/v0.2.1
curl -L -O "$url/$base" && curl -L -O "$url/$base.sha256"
sha256sum -c "$base.sha256" && tar -xzf "$base"   # → /tmp/igbot

# 3. install (ffmpeg, yt-dlp, 2 GB swap, service user, systemd units)
cd ~/instagram_tg_bot
sudo bash deploy/setup.sh /tmp/igbot

# 4. configure + start
sudo nano /etc/igbot/igbot.env            # set TELEGRAM_BOT_TOKEN + ALLOWED_CHAT_IDS
sudo systemctl start igbot
journalctl -u igbot -f
```

`deploy/setup.sh` also creates a **2 GB swap**, a daily **yt-dlp auto-update**
timer, and a 5-minute **keepalive** (so the always-free VM isn't reclaimed for
idleness). The bot uses **long polling**, so **no inbound ports / TLS / domain**
are required. Memory is guarded by `MemoryMax=800M` + single-worker concurrency.

## Upgrading to a new release

It's a **binary swap** — no new system deps, and new config (e.g. Threads) ships
with working defaults, so `igbot.env` needs no changes. Replace `v0.2.1` below
with the tag you're moving to.

```bash
# 1. download + verify the new release
cd /tmp
base=igbot-v0.2.1-linux-x86_64.tar.gz
url=https://github.com/sailself/instagram_tg_bot/releases/download/v0.2.1
curl -L -O "$url/$base" && curl -L -O "$url/$base.sha256"
sha256sum -c "$base.sha256" && tar -xzf "$base"   # → /tmp/igbot

# 2. back up the current binary (for instant rollback)
sudo cp -a /opt/igbot/igbot /opt/igbot/igbot.bak

# 3. swap + restart. Stop first: replacing a *running* executable in place
#    errors with "Text file busy". Long polling → downtime is a few seconds.
sudo systemctl stop igbot
sudo install -o botuser -g botuser -m 0755 /tmp/igbot /opt/igbot/igbot
sudo systemctl start igbot

# 4. verify it took
systemctl status igbot --no-pager
journalctl -u igbot -n 50 --no-pager | grep -Ei 'config loaded|extractor chain'
```

A successful upgrade logs both `instagram extractor chain built` and
`threads extractor chain built` at startup (and `threads=true` in the config
summary line). Then post a real link in your group to confirm.

**Rollback** if anything looks wrong:

```bash
sudo systemctl stop igbot
sudo mv /opt/igbot/igbot.bak /opt/igbot/igbot
sudo systemctl start igbot
```

## Logs

Logs go to the **systemd journal** (no files by default):

```bash
journalctl -u igbot -f                    # live tail
journalctl -u igbot -n 200 --no-pager     # recent
```

Set `RUST_LOG=igbot=debug` for HTTP/extraction detail. To *also* write rotating
files, set `LOG_DIR=/opt/igbot/logs` (must live under `/opt/igbot` — the unit
runs `ProtectSystem=strict`). `HEARTBEAT_SECS` controls the periodic liveness +
counters line.

## Configuration

See [`.env.example`](.env.example) for everything. Key variables:

| Variable | Purpose |
|---|---|
| `TELEGRAM_BOT_TOKEN` | **required** — BotFather token |
| `ALLOWED_CHAT_IDS` | comma-separated chat ids; empty = any chat |
| `EMBED_USER_AGENT` | crawler UA for the IG embed scraper (hot-config) |
| `THREADS_ENABLED` | set `0/false/no/off` to ignore Threads links (default on) |
| `THREADS_USER_AGENT` / `THREADS_SEC_CH_UA` | desktop-browser UA + matching client-hint for the Threads scrape (hot-config) |
| `RUST_LOG` | log filter (`igbot=info,warn` default; `igbot=debug` for detail) |
| `HEARTBEAT_SECS` / `LOG_DIR` / `LOG_MAX_FILES` | metrics heartbeat / optional rotating file logs |
| `IG_COOKIES_PATH` | enables the gallery-dl + cookie path (use a **burner** only) |
| `FALLBACK_PROVIDER` / `JINA_API_KEY` | enables the external fallback (off by default) |

Brittle bits (User-Agent, endpoints, timeouts) are hot-config via env so a break
is a config change, not a recompile.

## Limitations & notes

- Instagram extraction is inherently fragile; the chain + caching + graceful
  failure replies absorb intermittent blocks. The crawler-UA embed path is the
  primary mechanism and may need an `EMBED_USER_AGENT` change if IG shifts again.
- Threads extraction is gated on a coherent desktop-browser header set; if
  Threads shifts, change `THREADS_USER_AGENT` / `THREADS_SEC_CH_UA` (no
  recompile). An empty-shell response is classified as a failure, so users get a
  graceful reply rather than silence. The Threads scrape, repost/quote nesting,
  and poll rendering still want **live validation** against real posts.
- Albums are sent as up to 10 items (Telegram album max); extras are noted.
- Bot API upload cap is 50 MB; larger videos get a link + note instead.
- This scrapes public, logged-out content. Adding burner cookies is opt-in and
  disposable.
