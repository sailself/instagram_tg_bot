# igbot — implementation + code review

> Logged retroactively when the Agent Execution Logging convention was
> introduced (2026-06-27 ~13:05); timestamp reflects when the work ran.

## User request

1. `/goal implement the entire plan @docs/PLAN.md @docs/CHECKLIST.md`
2. `/code-review` at max effort (workflow-backed), then fix the findings.

## Goal

Turn the agreed plan (cookieless-first Instagram→Telegram mirror bot, OCI 1 GB,
long polling) into a compiling, tested codebase plus ready-to-run deploy
artifacts; then harden it against a max-effort review.

## Plan used

Phased per `docs/CHECKLIST.md`: (0) scaffold + config + URL detection,
(1) embed-scraper MVP + sender, (2) extractor chain (yt-dlp, gallery-dl,
external fallback), (3) dedup + single-worker queue + delivery ladder,
(4) deploy artifacts, (5) ops/hot-config. Compile + test after each batch.

## Actions completed

Created the full crate and supporting files:

- **Source** (`src/`): `main.rs` (dispatcher, throttle, preflight, worker spawn),
  `config.rs`, `error.rs`, `urls.rs` (link detection), `dedup.rs` (moka TTL),
  `queue.rs` (single worker), `media.rs` (capped download), `bot/{mod,handler,sender}.rs`,
  `extract/{mod,embed,yt_dlp,gallery_dl,external}.rs`.
- **Deploy** (`deploy/`): `igbot.service`, `yt-dlp-update.{service,timer}`,
  `keepalive.{service,timer}`, `setup.sh`.
- **Meta**: `Cargo.toml` (+ `Cargo.lock`), `.gitignore`, `.env.example`, `README.md`.

Commands + results:
- `cargo check` → clean (dep tree incl. teloxide 0.17 resolved).
- `cargo test` → 34 passing initially, **42 after review fixes**.
- `cargo clippy --all-targets` → **0 warnings**.
- `cargo build --release` → OK (~6 MB stripped binary).
- Runtime smoke tests: no token → clean `missing required env var` (exit 1);
  fake token → clean `Invalid bot token` via preflight (no panic).

**Code review:** ran the workflow-backed review (55 agents). 15 distinct verified
findings + 9 refuted. All 15 fixed (plus the systemd `StartLimit` item, which was
refuted but judged correct):
- UTF-16 (not char) caption/text length; truncated-caption-kept-on-media.
- Album chunking into 10s; 1-survivor → single send; removed lossy `+N more`.
- Reply on delivery failure (no silent failures).
- Chain error precedence (Blocked not masked by NotFound).
- Dedup: atomic claim + `forget()` on failure; fixed queue-full/retry contradiction.
- Login-wall detection regardless of page size.
- yt-dlp per-entry image/video classification.
- `is_ig_cdn` host-parse (SSRF fix); `media_by_ext` real video exts only.
- Disabled link previews on text replies; moved `StartLimit*` to `[Unit]`.

## Validation

`cargo test` (42 ✓), `cargo clippy --all-targets` (0 warnings), `cargo build
--release` (ok), runtime smoke tests as above. Parser logic covered by offline
tests against captured sample payloads.

## Follow-ups / TODOs

- **User action**: create bot in @BotFather, **disable privacy mode + re-add** to
  the group, set `TELEGRAM_BOT_TOKEN` + `ALLOWED_CHAT_IDS`.
- **Live-validate the embed scraper** against real posts (single / reel /
  carousel) — Phase 1 milestone; not confirmable offline.
- Provision OCI VM and run `deploy/setup.sh`.
- Validate gallery-dl / external-fallback backends only if/when enabled.
- **Not committed yet** (awaiting user go-ahead; would branch, not commit to `main`).

## Design decisions

- Module named `urls` (not `url`) to avoid clashing with the `url` crate.
- Used `std::sync::LazyLock` + system allocator (no once_cell/jemalloc) — fewer deps.
- Added a `preflight` `get_me` check before `dispatch()` — discovered via smoke
  testing that teloxide panics on a failed startup call, which `panic=abort`
  turns into a hard abort. The guard converts that into a clean error / retry.

## Deviations from plan

- `tokio` runtime uses 2 worker threads (plan mused single-thread); negligible
  memory cost on the burstable box, more robust. 
- External-fallback endpoints (EmbedEZ/Jina) are implemented best-effort and are
  **off by default**; they are explicitly unvalidated against live services.

## Tradeoffs

- Album upload fallback downloads sequentially and skips items that fail/oversize,
  preferring "deliver what we can" over all-or-nothing — favored partial delivery
  over strict fidelity, appropriate for a hobby bot on 1 GB.
- Dedup accepts keeping a claim on genuine `NotFound` (suppresses retries of
  removed/private posts for the TTL) to avoid re-hammering Instagram.

## Open questions

- Commit now (on a branch) or after live validation?
