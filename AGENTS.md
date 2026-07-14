# AGENTS.md — igbot

Guidance for any AI agent (Claude Code, Codex, etc.) working in this repo.
`CLAUDE.md` imports this file, so this is the single source of truth.

## What this is

A Rust Telegram bot that mirrors **Instagram** *and* **Threads** posts into a
Telegram group: it watches the chat, and when a member posts an instagram.com
link (`/p/`, `/reel/`, `/tv/`) or a threads.com / threads.net link
(`/@user/post/…`) it replies with the post's media (images/videos, including
carousels), caption, and author — and, for Threads' text-first posts, the text
itself when there's no media. Free to run; targets an **OCI Always Free 1 OCPU /
1 GB** VM. Long polling, **cookieless-first**.

## Read first

- **`docs/PLAN.md`** — full design + rationale (the "why" behind every choice).
- **`docs/CHECKLIST.md`** — build phases and current status.
- This file — the invariants and gotchas you must not violate.

## Build / test / run

```bash
cargo test                    # unit tests (parsers, caption ladder, dedup, chain)
cargo clippy --all-targets    # must stay at 0 warnings
cargo run                     # needs .env with TELEGRAM_BOT_TOKEN (copy .env.example)
cargo build --release         # produces the deploy artifact (target/release/igbot)
```

MSRV **1.85**. TLS is **rustls** (never pull in openssl). Verify with real command
output before claiming anything works.

## Architecture (the mental model)

```
teloxide long-poll dispatcher
  → handler.rs: chat allowlist + scan text/entities for IG/Threads links
       (find_links → Platform-tagged) → namespaced dedup claim (ig:/th:)
  → bounded mpsc → SINGLE worker (concurrency = 1)
  → route by Platform (queue.rs `Chains`) to the matching ExtractorChain:
       IG:      embed → yt-dlp (+ gallery-dl if cookies) (+ external if configured)
       Threads: threads-json → threads-embed   (accept_textonly = true)
  → sender.rs: captions ≤1024 each, long text continues across the album
       chunks' captions on word boundaries with "…" seam markers (tail past
       the last chunk → dropped, no follow-up) + album chunking +
       URL→download→link delivery ladder; media-less posts → text reply
       (4096-UTF-16 chunks, same word-boundary flow)
  → reply to the original message
```

Backends implement the pluggable `Extractor` trait (`src/extract/mod.rs`) — it
takes the canonical post `url` + shortcode and returns a `Post`, so it serves
both platforms (route by **host**, not shortcode). The IG embed scraper and the
Threads JSON scraper share the Polaris media-collection (`collect_meta_media`).
yt-dlp/gallery-dl do **not** support Threads, so the Threads chain is in-process
only (`threads_json` primary, `threads_embed` the `/embed`-HTML fallback).

## Invariants & gotchas — these are landmines, respect them

- **Meta CDN URLs (Instagram + Threads)**: capture **verbatim**. HTML-entity /
  JSON-unicode decode before fetching (use `normalize_cdn_url`), but **never
  trim, reorder, or re-encode query params** — the whole query string is the
  signature; touching it → `403 "Bad URL hash"`. Threads media rides the same
  signed Meta CDN as Instagram.
- **Telegram lengths are UTF-16 code units, not `char`s.** Use
  `utf16_len`/`truncate_utf16`/`split_utf16` in `sender.rs`; never gate caption
  (1024) or text (4096) limits on `chars().count()` (emoji silently overflow).
- **Media groups are 2–10 items.** One item must go via `send_photo`/`send_video`;
  chunk >10 into successive groups. Never build a 1-item media group.
- **Never blind-retry an ambiguous Telegram send.** A send that fails with a
  *post-send client timeout* may still be delivered — Telegram answers only
  after its server-side work (URL fetches, album processing) finishes.
  `sender.rs::may_have_landed` surfaces such failures instead of retrying or
  falling back to another delivery route: a duplicated album is worse than one
  failure notice. Only provably-not-posted errors (flood-wait, connect-phase)
  get the one-shot retry. Keep `TG_SEND_TIMEOUT_SECS` generous — a short client
  timeout manufactures phantom "couldn't send" failures for sends that landed.
- **Bot API upload cap is 50 MB.** Oversize media → reply with link + note.
- **`panic = "abort"` in release** → panics can't be caught; they abort the
  process. Prevent them: no `unwrap()`/`expect()` on runtime-fallible paths, and
  keep the `preflight` `get_me` guard before `dispatch()` (teloxide panics if its
  startup call fails).
- **`is_meta_cdn` must host-parse**, never substring-match (substring match is an
  SSRF/exfil vector). It allowlists `*.cdninstagram.com` / `*.fbcdn.net`, which
  serve **both** Instagram and Threads media — there is no per-platform CDN list.
- **Text-only posts are valid output** (Threads is text-first). The Threads chain
  is built `accept_textonly`, so a caption-only `Post` is success and `sender.rs`
  delivers it as text; never classify "no media but has caption" as a failure for
  Threads. The IG chain keeps media required.
- **Threads header gate**: the post JSON is served logged-out **only** to a
  *coherent desktop-browser* header set (`THREADS_USER_AGENT` /
  `THREADS_SEC_CH_UA`, hot-config). A naive/crawler UA returns HTTP 200 + an
  **empty shell** (0 `thread_items`) — classify that as a failure, **never**
  silent success.
- **Cookieless-first.** Do NOT enable Instagram/Threads login/cookies or the
  external fallback by default — config-gated (`IG_COOKIES_PATH`,
  `FALLBACK_PROVIDER`). Any cookie use is a disposable burner, never a real
  account.
- **Memory discipline on 1 GB**: concurrency = 1, capped streaming downloads,
  RAII temp dirs. Don't parallelize extraction or buffer whole files needlessly.
- **Extraction is fragile by nature.** Every failure path must produce a graceful
  user reply — never leave the user in silence. Errors are classified
  (`NotFound`/`Blocked`/`Transient`/`Unavailable`); the chain surfaces the most
  actionable one.
- **Dedup semantics**: claim on enqueue, `forget()` on any failure (or if never
  enqueued). Only a *successfully delivered* post stays deduped for the TTL. Keys
  are **namespaced per platform** (`Platform::dedup_key` → `ig:`/`th:`) — IG and
  Threads shortcodes share an alphabet and would otherwise collide across
  platforms.

## Conventions

- Brittle externals (User-Agent, embed/GraphQL endpoints, `doc_id`) are
  **hot-config via env**, not baked-in literals — a breakage should be a config
  change, not a recompile.
- Parsers are unit-tested against **captured / representative sample payloads**
  (offline, deterministic). Add a test when you touch parsing logic. The Threads
  fixtures mirror the observed JSON/HTML shape; live behavior still needs
  validation (see below).
- Logging via `tracing`. **Never log secrets** (token, cookies).

## Deployment

Long polling → **no inbound ports / TLS / domain**. Target: OCI `E2.1.Micro`
(x86-64, 1 GB), Ubuntu. `deploy/setup.sh` installs ffmpeg + yt-dlp + 2 GB swap +
systemd units (service with `MemoryMax`, daily yt-dlp auto-update, 5-min
keepalive against idle reclaim). The bot token and chat allowlist live in
`/etc/igbot/igbot.env` (chmod 600) — never commit them.

## Verification before "done"

Run `cargo test` and `cargo clippy --all-targets` and a release build; cite the
output. Don't assert success without evidence. The IG embed/external/gallery-dl
backends need **live validation against real Instagram posts**, and the
**Threads** scraper (the working header set, repost/quote JSON nesting,
poll-option visibility, age-gated/private behavior) needs **live validation
against real Threads posts** — unit tests cover parsing of captured/representative
payloads, not live behavior.

## Agent Execution Logging

- For each substantial implementation or investigation request, create a log file under `agent_logs/`.
- Name format: `<YYYYMMDD_HHMMSS>_<task-name>.md` — timestamp first so files sort chronologically and the latest logs are easy to find, followed by a descriptive task name.
- Each log file must include:
  - The user prompt/request that triggered the work.
  - The goal of this task
  - The implementation plan used.
  - The actions completed (files touched, code change summary, commands run with results).
  - Validation performed (build/tests/other checks) and outcomes.
  - Follow-ups or TODOs
- Append when relevant (skip a heading if it's empty — don't pad):
  - Design decisions — choices made when the spec/conversation was ambiguous, with the reason.
  - Deviations — intentional departures from spec or original plan, with why.
  - Tradeoffs — alternatives you'd defend to a reviewer, not micro-choices.
  - Open questions — things you want the user to confirm or revise.
  - Update at decision points, not on routine progress. At session end, re-read the originating prompt and check whether anything in the implementation contradicts a literal reading; if yes, log it under Deviations.
- Do not include secrets, API keys, or sensitive runtime credentials in log content.
