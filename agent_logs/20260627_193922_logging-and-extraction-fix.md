# Logging pass + embed-extractor fix

## User request
"add logging for the bot" — then, while testing, the user reported the bot
returning "couldn't find that post" for a **public** reel
(`https://www.instagram.com/p/DaGLJzTxpFR/`). The user chose to fix extraction
and do the logging work **together**.

## Goal
1. Make cookieless extraction work against **current** Instagram (the primary
   embed scraper is broken — see investigation).
2. Comprehensive logging: per-job tracing spans, timing/metrics/heartbeat,
   fuller debug coverage, optional rotating file logs.
3. Fix two correctness bugs surfaced by the user's console output.

## Investigation (live, from the dev box's residential IP, bot UA)
Fetched the exact surfaces the bot uses and grepped the bodies:

| Fetch (UA → URL) | Result |
|---|---|
| desktop-Chrome → `/p/{code}/embed/captioned/` (what the bot does today) | HTTP 200, 604 KB, **0 media** — a JS-only `PolarisEmbed` React shell. No `__additionalDataLoaded`, no `og:`, no `scontent /v/` URLs. All 3 parser shapes miss → `NotFound`. |
| `facebookexternalhit/1.1` → `/p/{code}/` | HTTP 200, 791 KB. Contains OG tags (cover image + author + caption) **and** a `application/json` Polaris/Relay blob (line 70, 51 KB) with the full media. |

Schema of the JSON blob (confirmed against the captured payload):
- Embeds **multiple posts** (target + related) — each keyed by `"code"`. Parser
  MUST select the node whose `code == shortcode` (not grab all media).
- Target node: `"media_type"` (1=image, 2=video, 8=carousel),
  `"video_versions":[{"type","url"}]` (best first), `"image_versions2":
  {"candidates":[{"url",…}]}` (largest first), `"carousel_media":[…]` (children),
  `"caption":{"text"}`, `"user":{"username"}` / `owner`.
- Video URLs are `…fbcdn.net/o1/v/t2/…mp4` — `is_ig_cdn` (host-parse) accepts
  them, but note `external.rs` `CDN_RE` (hard-coded `/v/` after host) would MISS
  them (latent bug, external is off by default — logged as follow-up).

## Plan
**Phase A — Extraction fix (embed.rs / config.rs / urls.rs)**
- New config `EMBED_USER_AGENT` (hot-config; default `facebookexternalhit/1.1`).
- Repoint `EmbedScraper` to fetch `post_url(shortcode)` with the crawler UA.
- Add primary parse shape `parse_polaris_json`: scan `<script type=
  "application/json">` blocks, recursively find the node with `code==shortcode`,
  extract media by `media_type` (carousel→children; video→`video_versions[0]`;
  image→`image_versions2.candidates[0]`), caption (`caption.text`), author.
- Keep `parse_og_shape` as graceful fallback (cover image + author + caption).
- Tests against captured-payload-derived fixtures (offline, deterministic):
  picks the right post by code; carousel mix; video reel; og fallback.

**Phase B — Outcome logging + dedup fix (queue.rs / yt_dlp.rs)**
- `handle()` returns an outcome (`Delivered{n}` / `Unavailable` / Err) so the
  worker logs the TRUTH (delivered media=N / post unavailable / job failed),
  never a false "job done".
- Forget the dedup claim on NotFound/Unavailable too (matches the documented
  invariant: only a *delivered* post stays deduped). Bug: today NotFound returns
  Ok(()) → claim kept → re-posts silently skipped for the TTL.
- yt-dlp `classify_stderr`: map "empty media response" / "use --cookies" →
  `Blocked` (so the user reply says login-required, not rate-limited).

**Phase C — Logging infrastructure**
- New `src/metrics.rs`: atomic counters (received/succeeded/failed/timed_out) +
  uptime; `spawn_heartbeat`.
- Per-job `info_span!("job", seq, shortcode, chat)` in the worker (`.instrument`).
- Timing (`elapsed_ms`) on chain per-backend + worker total + delivery; per-
  download `bytes`/`elapsed_ms`.
- Debug HTTP/subprocess detail in embed/yt_dlp/gallery_dl/external (status,
  bytes, exit code, truncated stderr — never the cookies path/contents).
- Handler enqueue audit line (shortcode + chat + user).
- `Config::summary()` logged at startup — counts/tuning only, NEVER token/
  cookies-path/jina-key (unit-tested for redaction). New `HEARTBEAT_SECS`.
- `init_tracing` → registry with stdout layer (journald) + optional daily-
  rotating file layer (`LOG_DIR`, `LOG_MAX_FILES`, lossy non-blocking, holds a
  `WorkerGuard`); reads env before Config so logging is up before config can
  fail; appender-build failure → eprintln + stdout-only (never panics).
- `Cargo.toml` += `tracing-appender`. `.env.example` + deploy note.

**Phase D — Verify**: `cargo test`, `cargo clippy --all-targets`, release build;
live re-test of `DaGLJzTxpFR` (should now mirror the reel video).

## Design decisions
- **Crawler UA is hot-config**, per AGENTS ("brittle externals via env"). If IG
  closes the crawler-UA OG/JSON path, it's a config change, not a recompile.
- **Key media extraction on `code==shortcode`** because the page embeds related
  posts; grabbing all media would mirror the wrong content.
- **Polaris-JSON primary, OG fallback**: JSON gives full carousels + reel video;
  OG is the stable floor (cover + caption) if the JSON schema shifts.

## Open questions / caveats
- Validated against ONE post (a reel) from a **residential** IP. Needs live
  validation across an image post + a carousel, and from the **OCI datacenter
  IP** (IG may gate it differently there — PLAN's known risk).
- Want a public reel/carousel link from the user to broaden test fixtures.

## Follow-ups / TODO
- `external.rs` `CDN_RE` misses `/o1/v/` video URLs — fix when external is used.
- Full-res image selection (candidates[0]) vs bandwidth on the 1 GB box — revisit
  if uploads approach the 50 MB cap.

## Actions completed
**Extraction fix**
- `config.rs`: +`embed_user_agent` (`EMBED_USER_AGENT`, default `facebookexternalhit/1.1`).
- `extract/mod.rs build_chain`: embed scraper now gets the crawler UA.
- `urls.rs`: removed unused `embed_url`; `post_url` is now the scrape target.
- `embed.rs`: fetch the post page; new **Shape 0** `parse_polaris_json`
  (`find_post_node` keys on `code==shortcode`; `polaris_media`/`polaris_single`
  read `carousel_media`/`video_versions[0]`/`image_versions2.candidates[0]`,
  prefer video); kept legacy/OG shapes as fallback. +4 unit tests (pick-by-code,
  carousel, single-image-largest, SSRF rejection). Validated against the real
  791 KB captured payload via a throwaway `#[ignore]` test (now removed):
  extracted author `milanodascrocco`, full caption, and the reel MP4.

**Outcome logging + dedup fix + classifier**
- `queue.rs`: `Outcome` enum; worker logs `delivered media=N` / `post
  unavailable` / `job failed` (no more false "job done"); `dedup.forget()` now
  runs on Unavailable too — only a *delivered* post stays deduped.
- `yt_dlp.rs`: `classify_stderr` maps "empty media response" /
  "--cookies-from-browser" → `Blocked` (truthful user reply). +test.

**Logging infrastructure**
- New `metrics.rs` (atomic counters + `spawn_heartbeat`). +test.
- `queue.rs`: per-job `info_span!("job", seq, shortcode, chat)` via `.instrument`;
  metrics wired; total-job `elapsed_ms`.
- Per-backend `elapsed_ms` in the chain; delivery `elapsed_ms` (sender);
  per-download `bytes`/`elapsed_ms` (media.rs).
- Debug HTTP/subprocess detail in embed (status+bytes), yt_dlp/gallery_dl (exit
  code + char-safe truncated stderr + stdout bytes), external (status+bytes).
- `handler.rs`: enqueue audit line (shortcode + chat + user).
- `config.rs`: `summary()` (secrets-free, +test `summary_omits_secrets`);
  `heartbeat` (`HEARTBEAT_SECS`, 0 disables, +test).
- `main.rs`: `init_tracing` → registry with stdout layer + optional daily
  rotating file layer (`LOG_DIR`/`LOG_MAX_FILES`, lossy non-blocking, returns a
  held `WorkerGuard`; build failure → eprintln + stdout-only, never panics);
  startup config summary; heartbeat spawn.
- `Cargo.toml` += `tracing-appender = "0.2"` (resolved 0.2.5).
- `.env.example`: documented `EMBED_USER_AGENT`, `HEARTBEAT_SECS`, `LOG_DIR`,
  `LOG_MAX_FILES`.

**Cleanup**: removed the now-redundant `Post.shortcode` field (Job.shortcode is
the single source of truth; the span carries it) — cascaded through the four
backends + tests.

## Validation
- `cargo test` → **49 passed, 0 failed**.
- `cargo clippy --all-targets -- -D warnings` → **clean (0 warnings)**.
- `cargo build --release` → **ok** (panic=abort profile).
- Live: crawler-UA fetch of the post page returns the Polaris JSON (verified via
  curl); parser extracts the reel video from the real captured payload.
- NOT yet validated live: full bot→Telegram delivery (needs `cargo run` + a
  posted link), behaviour from the OCI datacenter IP, and image/carousel posts
  (only a reel was sampled).
