# igbot

A Rust Telegram bot that watches a group chat and, whenever someone posts an
**Instagram** link (post `/p/`, reel `/reel/`, or `/tv/`), replies to that
message with the post's **media (images/videos, incl. carousels), caption, and
author**. Free to run; designed for an **OCI Always Free 1 OCPU / 1 GB** VM.

Design & rationale: [`docs/PLAN.md`](docs/PLAN.md) · build steps:
[`docs/CHECKLIST.md`](docs/CHECKLIST.md).

## How it works

```
group msg ─(teloxide, long-poll)→ detect IG link → dedup → bounded queue
        → single worker → extractor chain → reply (album / photo / video)
```

**Extractor chain** (first backend with media wins; PLAN §3.4 / §4):

1. **embed** — in-process scrape of `…/embed/captioned/`. Anonymous, no
   subprocess, handles images + video + carousels. Tries three response shapes
   (`__additionalDataLoaded` JSON → `<img>/<video>` → OG tags) with retries.
2. **yt-dlp** — subprocess; the video workhorse (upstream-maintained).
3. **gallery-dl** — *only if* `IG_COOKIES_PATH` is set (images/carousels).
4. **external fallback** — *only if* `FALLBACK_PROVIDER` is set (EmbedEZ / Jina);
   fetches from a different IP when ours is blocked. Off by default.

Cookieless-first: no Instagram login required for the default chain.

## Prerequisites

- **Rust** ≥ 1.85, **ffmpeg**, and **yt-dlp** (the standalone binary; installed
  by `deploy/setup.sh` on the server).
- A bot token from **@BotFather**.

### BotFather setup (one-time, manual)

1. `/newbot` → get the token.
2. **`/setprivacy` → your bot → Disable.** Required so the bot can see normal
   group messages (not just commands).
3. Add the bot to your group, then **remove and re-add it** — the privacy change
   only takes effect on re-add. (Or make it a group admin.)
4. Find your group's chat id (run the bot, post a message, read it from the logs)
   and put it in `ALLOWED_CHAT_IDS`.

## Run locally

```bash
cp .env.example .env        # set TELEGRAM_BOT_TOKEN (and ALLOWED_CHAT_IDS)
cargo run                   # long polling; no inbound ports needed
cargo test                  # unit tests (URL detection, parsers, caption ladder)
```

(`ffmpeg`/`yt-dlp` are only needed at runtime for the yt-dlp backend; the embed
backend works without them.)

## Deploy to OCI (Ubuntu, 1 GB)

```bash
cargo build --release
sudo bash deploy/setup.sh             # installs ffmpeg + yt-dlp, swap, user, units
sudo nano /etc/igbot/igbot.env        # set token + ALLOWED_CHAT_IDS
sudo systemctl start igbot
journalctl -u igbot -f
```

`deploy/setup.sh` also creates a **2 GB swap**, a daily **yt-dlp auto-update**
timer, and a 5-minute **keepalive** (so the always-free VM isn't reclaimed for
idleness). The bot uses **long polling**, so no inbound ports / TLS / domain are
required. Memory is guarded by `MemoryMax=800M` + single-worker concurrency.

## Configuration

See [`.env.example`](.env.example) for all variables. Key ones:
`TELEGRAM_BOT_TOKEN` (required), `ALLOWED_CHAT_IDS`, `IG_COOKIES_PATH` (enables
the cookie path), `FALLBACK_PROVIDER` + `JINA_API_KEY` (enables the external
fallback). Brittle bits (`USER_AGENT`, timeouts) are hot-config via env so a
break is a config change, not a recompile.

## Limitations & notes

- Instagram extraction is inherently fragile; the chain + caching + graceful
  failure replies absorb intermittent blocks (PLAN §1, §4.5).
- Albums are sent as up to 10 items (Telegram album max); extras are noted.
- Bot API upload cap is 50 MB; larger videos get a link + note instead.
- This scrapes public, logged-out content (PLAN §4.7). Adding burner cookies is
  opt-in and disposable.
