# Implementation Checklist

Actionable, ordered task list. See `PLAN.md` for the "why" behind each item.
A working bot exists at the **end of Phase 1**; everything after is
resilience and polish.

---

## Status — implemented 2026-06-27

**Code complete & verified.** `cargo test` → 34 passing; `cargo clippy` → 0
warnings; release build OK; runtime smoke-tested (clean startup, graceful
bad-token error, no panic). Phases 0–3 are implemented in `src/`; Phase 4–5
artifacts are in `deploy/`, `.env.example`, and `README.md`.

**Still requires you** (needs your accounts/credentials — cannot be done from
the repo):
- Create the bot in **@BotFather**, **disable privacy mode**, **re-add** it to
  the group, and copy the token.
- Set the token + `ALLOWED_CHAT_IDS` in `.env` (local) or `/etc/igbot/igbot.env`
  (server).
- Provision the OCI VM, then `cargo build --release && sudo bash deploy/setup.sh`.
- **Validate the embed scraper against live posts** (single image / reel /
  carousel) — the one thing only a real token + live network confirms
  (Phase 1 milestone).
- Optional, only if blocks appear: set `IG_COOKIES_PATH` (burner) or
  `FALLBACK_PROVIDER` (+ key).

---

## Phase 0 — Scaffold & plumbing

- [ ] Create a bot via **@BotFather**; save the token.
- [ ] **Disable Privacy Mode**: BotFather `/setprivacy` → Disable.
- [ ] Add the bot to the test group, then **remove and re-add it** (mandatory for
      privacy change to apply).
- [ ] `cargo init`; add deps + `[profile.release]` from `PLAN.md §2`.
- [ ] `config.rs`: load `TELEGRAM_BOT_TOKEN`, `ALLOWED_CHAT_IDS`, `TEMP_DIR`,
      `CACHE_TTL_SECS`, `YT_DLP_PATH`, optional `IG_COOKIES_PATH` (unset by default).
- [ ] `tracing-subscriber` logging init.
- [ ] teloxide long-poll dispatcher skeleton with two branches: command handler +
      catch-all `filter_message().endpoint(...)`.
- [ ] `url.rs`: detect IG URLs from `entities` (`Url` + `TextLink`) + regex
      fallback; canonicalize → extract `shortcode`; restrict to `ALLOWED_CHAT_IDS`.
- [ ] **Milestone:** bot logs every detected Instagram shortcode in the group.

## Phase 1 — Cookieless MVP (embed scraper)

- [ ] `extract/mod.rs`: `MediaKind`, `Media`, `Post`, `ExtractError`,
      `InstagramExtractor` trait, `ExtractorChain` (from `PLAN.md §3.3`).
- [ ] `extract/embed.rs`: `EmbedScraper` — GET `/p/{shortcode}/embed/captioned/`
      with desktop UA; parse `window.__additionalDataLoaded('extra', …)`
      (**not** `contextJSON`); map `owner.username`, `edge_media_to_caption…text`,
      `display_url`/`video_url`, `edge_sidecar_to_children[]`. OG-tag fallback parse.
      **⚠️ 2026-06-27: IG changed this surface — the scraper now fetches the post
      page with a crawler UA and parses the Polaris `application/json` blob. See
      the PLAN §4.1 update.**
  - [ ] **Hardening (endpoint is intermittent):** retry 2–3× w/ backoff; parse
        3 shapes in order (`__additionalDataLoaded` JSON → plain `<img>/<video>`
        in captioned HTML → og/JSON-LD); no-media response = miss → fall through.
  - [ ] **CDN URL rule:** decode HTML entities (`&amp;`→`&`) + JSON unicode escapes back to a literal `&` before fetch;
        keep the full query string **verbatim** (no trim/reorder) or CDN 403s.
  - [ ] Validate against live posts: single image, reel, image carousel, mixed
        carousel — confirm media + caption + author (this is the MVP gate).
- [ ] `bot/sender.rs`: reply via `reply_parameters`; `send_photo`/`send_video`
      (1 item), `send_media_group` (2–10), caption ≤1024 + full-caption follow-up.
- [ ] Media delivery: `InputFile::url` first (download+upload deferred to Phase 3).
- [ ] **Milestone:** posting a public IG link → bot replies with media + caption +
      author, cookieless. Test: single image, reel, image carousel, mixed carousel.

## Phase 2 — Fallback chain

- [ ] `extract/yt_dlp.rs`: `YtDlpExtractor` via `tokio::process::Command`
      (`yt-dlp -J --no-warnings`); parse `description`/`uploader`/formats;
      handle carousel `entries[]`; classify errors (Blocked/NotFound/Transient).
- [ ] `extract/gallery_dl.rs`: `GalleryDlExtractor` (`gallery-dl -j`,
      `--sleep-request 2.0`); **added to chain only if `IG_COOKIES_PATH` set**.
- [ ] `extract/external.rs`: `ExternalFallback` backend (PLAN §4.8) — **off by
      default**, behind `FALLBACK_PROVIDER` config; sits **last** in the chain.
      Spike **EmbedEZ keyless** first; keep provider swappable (RapidAPI `$0` /
      Jina free-key / Bright Data as alternatives). 20–30 s timeout, 1 retry.
- [ ] Failure-detection order in the backend: HTTP status → provider code →
      login-wall/empty sniff → media-count ("text but no media" = `Err`).
- [ ] (Optional) Pure.md `enrich_caption_if_missing(&mut post)` text-rescue.
- [ ] Wire chain order: `EmbedScraper` → `YtDlpExtractor` (+ gallery-dl if cookies)
      (+ `ExternalFallback` if configured).
- [ ] Per-invocation cookie/proxy flags plumbed (inactive by default).
- [ ] **Milestone:** kill the embed path in a test → yt-dlp transparently covers
      reels; chain logs which backend served each post.

## Phase 3 — Robustness

- [ ] `dedup.rs`: `moka` TTL cache keyed by shortcode (`CACHE_TTL_SECS`); skip
      (or 👀 react) on repeats.
- [ ] `queue.rs`: bounded `mpsc` + **single worker (concurrency = 1)**; reply
      "busy, try again" when full; per-job wall-time cap (60–90 s).
- [ ] teloxide **`Throttle`** adaptor (innermost) + `RetryAfter` sleep-and-retry.
- [ ] `media.rs`: download-then-upload fallback (≤50 MB) with **RAII temp-dir
      cleanup**; >50 MB → link+caption note; startup sweep of `TEMP_DIR`.
- [ ] Graceful chain-failure reply.
- [ ] Request pacing between extraction calls.
- [ ] **Milestone:** duplicate links dedup'd; two simultaneous links serialize;
      oversized video degrades gracefully; temp dir stays clean.

## Phase 4 — Deploy (OCI E2.1.Micro, Ubuntu 24.04)

- [ ] Provision instance (+ a spare); `apt install ffmpeg`; install yt-dlp
      standalone binary to `/usr/local/bin`.
- [ ] Create **2 GB swap** + `vm.swappiness=10`.
- [ ] Build release binary (on-box or cross-build `x86_64`); deploy to `/opt/igbot`.
- [ ] Create `botuser`; write `/etc/igbot/igbot.env` (chmod 600).
- [ ] Install + enable `igbot.service` (Restart, `MemoryHigh/Max`, hardening).
- [ ] Install + enable `yt-dlp-update.timer` (daily).
- [ ] Add 5-min keepalive (timer/cron).
- [ ] Confirm **no inbound ports** needed (long polling).
- [ ] **Milestone:** bot survives reboot, restarts on crash, runs unattended.

## Phase 5 — Ops & hardening

- [ ] Hot-config: embed `User-Agent` / URLs (and `doc_id` if GraphQL added) read
      from env/config — patchable without recompile.
- [ ] Light monitoring (journald + optional heartbeat log line).
- [ ] Document the burner-cookie escape hatch (set `IG_COOKIES_PATH`) for if/when
      blocks appear.
- [ ] Note watch-list: active InstaFix fork + yt-dlp releases.

---

### Definition of done (MVP)
Posting a public Instagram post/reel/carousel link in the allowed group makes the
bot reply to that message with the media + caption + author — running unattended,
cookieless, on the 1 GB OCI box, within memory limits.
