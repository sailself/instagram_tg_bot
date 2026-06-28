# AGENTS.md — igbot

Guidance for any AI agent (Claude Code, Codex, etc.) working in this repo.
`CLAUDE.md` imports this file, so this is the single source of truth.

## What this is

A Rust Telegram bot that mirrors **Instagram** posts/reels into a Telegram
group: it watches the chat, and when a member posts an instagram.com link
(`/p/`, `/reel/`, `/tv/`) it replies with the post's media (images/videos,
including carousels), caption, and author. Free to run; targets an **OCI Always
Free 1 OCPU / 1 GB** VM. Long polling, **cookieless-first**.

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
  → handler.rs: chat allowlist + scan text/entities for IG links → dedup claim
  → bounded mpsc → SINGLE worker (concurrency = 1)
  → ExtractorChain: embed → yt-dlp (+ gallery-dl if cookies) (+ external fallback if configured)
  → sender.rs: caption ladder + album chunking + URL→download→link delivery ladder
  → reply to the original message
```

Backends implement the pluggable `InstagramExtractor` trait (`src/extract/mod.rs`).
The embed scraper is in-process and primary; yt-dlp is the video backstop.

## Invariants & gotchas — these are landmines, respect them

- **IG CDN URLs**: capture **verbatim**. HTML-entity / JSON-unicode decode before
  fetching (use `normalize_cdn_url`), but **never trim, reorder, or re-encode
  query params** — the whole query string is the signature; touching it →
  `403 "Bad URL hash"`.
- **Telegram lengths are UTF-16 code units, not `char`s.** Use
  `utf16_len`/`truncate_utf16`/`split_utf16` in `sender.rs`; never gate caption
  (1024) or text (4096) limits on `chars().count()` (emoji silently overflow).
- **Media groups are 2–10 items.** One item must go via `send_photo`/`send_video`;
  chunk >10 into successive groups. Never build a 1-item media group.
- **Bot API upload cap is 50 MB.** Oversize media → reply with link + note.
- **`panic = "abort"` in release** → panics can't be caught; they abort the
  process. Prevent them: no `unwrap()`/`expect()` on runtime-fallible paths, and
  keep the `preflight` `get_me` guard before `dispatch()` (teloxide panics if its
  startup call fails).
- **`is_ig_cdn` must host-parse**, never substring-match (substring match is an
  SSRF/exfil vector).
- **Cookieless-first.** Do NOT enable Instagram login/cookies or the external
  fallback by default — both are config-gated (`IG_COOKIES_PATH`,
  `FALLBACK_PROVIDER`). Any cookie use is a disposable burner, never a real
  account.
- **Memory discipline on 1 GB**: concurrency = 1, capped streaming downloads,
  RAII temp dirs. Don't parallelize extraction or buffer whole files needlessly.
- **Extraction is fragile by nature.** Every failure path must produce a graceful
  user reply — never leave the user in silence. Errors are classified
  (`NotFound`/`Blocked`/`Transient`/`Unavailable`); the chain surfaces the most
  actionable one.
- **Dedup semantics**: claim a shortcode on enqueue, and `forget()` it on any
  failure (or if it's never enqueued). Only a *successfully delivered* post stays
  deduped for the TTL.

## Conventions

- Brittle externals (User-Agent, embed/GraphQL endpoints, `doc_id`) are
  **hot-config via env**, not baked-in literals — a breakage should be a config
  change, not a recompile.
- Parsers are unit-tested against **captured sample payloads** (offline,
  deterministic). Add a test when you touch parsing logic.
- Logging via `tracing`. **Never log secrets** (token, cookies).

## Deployment

Long polling → **no inbound ports / TLS / domain**. Target: OCI `E2.1.Micro`
(x86-64, 1 GB), Ubuntu. `deploy/setup.sh` installs ffmpeg + yt-dlp + 2 GB swap +
systemd units (service with `MemoryMax`, daily yt-dlp auto-update, 5-min
keepalive against idle reclaim). The bot token and chat allowlist live in
`/etc/igbot/igbot.env` (chmod 600) — never commit them.

## Verification before "done"

Run `cargo test` and `cargo clippy --all-targets` and a release build; cite the
output. Don't assert success without evidence. Note that the embed scraper and
the external/gallery-dl backends need **live validation against real Instagram
posts** — unit tests cover parsing of captured payloads, not live behavior.

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
