# Instagram → Telegram Bot — Implementation Plan

A Rust Telegram bot that monitors a group chat; when any member posts an
Instagram link (post `/p/`, reel `/reel/`, or `/tv/`), the bot extracts the
post's **caption text, media (images/videos, including carousels), author, and
original URL**, and **replies to the original message** with that content.

**Constraints:** free of cost, deployed on a single OCI Always Free VM.

---

## 0. Locked decisions

| Decision | Choice | Why |
|---|---|---|
| **Deploy target** | OCI **`VM.Standard.E2.1.Micro`** — 1/8 OCPU (burstable), **1 GB RAM, AMD EPYC x86-64** | What we have. Always available; x86 = trivial tooling installs. |
| **Auth posture** | **Cookieless-first** | Zero account risk. Optional cookie hook wired but **off by default**; add a burner only if blocks are actually observed. |
| **Bot framework** | **`teloxide` 0.17** (tokio, rustls) | Only Rust lib with a built-in dispatcher + Throttle + polling/webhook. |
| **Update mechanism** | **Long polling** | No inbound ports, no TLS, no domain. Best fit for a tiny box. |
| **Extraction** | **Hybrid fallback chain** (in-process embed scraper → yt-dlp → optional gallery-dl) | Resilient + cookieless + RAM-friendly. |
| **Packaging** | **Native binary + systemd** (no Docker) | Saves RAM/disk on 1 GB; easier yt-dlp updates. |
| **Concurrency** | **1 extraction job at a time** | The decisive memory-safety lever on 1 GB. |

---

## 1. Feasibility & honest risk summary

- **The Telegram layer and OCI deployment are easy and solved.** ~20 MB RAM, no
  inbound ports.
- **Instagram extraction is the only hard part, and it is genuinely fragile.**
  Every free method rides endpoints Meta actively defends. Our **OCI datacenter
  IP is the single biggest risk** — but at *a few requests/day we will NOT be
  hard-blocked*; we'll see *intermittent* 401/429/login-walls.
- **Design principle:** fallback chain + caching + graceful degradation. This is
  **not set-and-forget** — expect to occasionally bump tooling and swap a config
  value (e.g. a rotated `doc_id`).

> **Upgrade path (noted, not chosen):** the same Always Free tier includes
> **Ampere A1 (up to 4 cores / 24 GB RAM)**, which would erase all memory
> constraints — but it's ARM64 and frequently "out of capacity" to provision.
> If you later grab one, the architecture is unchanged; just relax the
> concurrency/swap/memory-cap guards and rebuild for `aarch64`.

---

## 2. Tech stack

```toml
# Cargo.toml
[dependencies]
teloxide      = { version = "0.17", features = ["macros", "throttle"] }
tokio         = { version = "1", features = ["rt-multi-thread", "macros", "process", "fs", "sync", "time"] }
reqwest       = { version = "0.12", default-features = false, features = ["rustls-tls", "json", "stream"] }
serde         = { version = "1", features = ["derive"] }
serde_json    = "1"
scraper       = "0.20"          # HTML parsing for the embed endpoint
regex         = "1"
async-trait   = "0.1"           # dyn-dispatchable async trait methods
thiserror     = "1"
anyhow        = "1"
tracing       = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
moka          = { version = "0.12", features = ["future"] }   # async TTL dedup cache
url           = "2"

[profile.release]
opt-level     = "s"     # smaller binary, lower code-cache pressure
lto           = "thin"
codegen-units = 1
panic         = "abort" # drop unwinding tables -> smaller, slightly less RAM
strip         = "symbols"
```

- **MSRV:** 1.85 (teloxide 0.17 requirement). Install Rust via `rustup`.
- **TLS:** use **rustls** (not native-tls/openssl) to avoid OpenSSL system-dep
  pain on the minimal OCI image.
- **External tools (system-installed, not bundled):** `yt-dlp` (standalone
  binary) and `ffmpeg`. The bot shells out to them via `tokio::process::Command`
  at configured paths — this keeps the auto-update timer in charge of yt-dlp.
  (The `boul2gom/yt-dlp` crate is an alternative but auto-downloads its own
  binaries, complicating the update story — not used.)

---

## 3. Architecture

### 3.1 Data flow

```
Telegram group msg ──(teloxide, long-poll)──▶ handler
   │  allowed chat? contains /p//reel//tv/ URL?  (scan entities + regex)
   ▼  yes → canonicalize → shortcode
[dedup cache]  seen shortcode in last N min? ── yes ──▶ skip (or 👀 react)
   │  no
   ▼
[bounded mpsc queue] ──▶ SINGLE worker (concurrency = 1)   ← the key 1 GB lever
   ▼
[ExtractorChain]  embed-scraper ──▶ yt-dlp ──▶ gallery-dl(only if cookies) → Post
   │  (each returns Post or classified error; fall through on failure)
   ▼
[media delivery ladder]  InputFile::url ──▶ download+upload(≤50 MB) ──▶ link+caption note(>50 MB)
   ▼
[sender]  reply_to original: send_photo / send_video / send_media_group(2–10)
   ▼
[cleanup]  RAII temp-dir drop  +  mark shortcode served
```

### 3.2 Module layout

```
src/
  main.rs              // bootstrap: config, logging, build extractor chain, start dispatcher
  config.rs            // env: BOT_TOKEN, ALLOWED_CHAT_IDS, TEMP_DIR, cache TTL, tool paths, optional cookies path
  bot/
    mod.rs
    handler.rs         // teloxide update handler: filter group msgs, find IG URLs, enqueue jobs
    sender.rs          // build InputMedia, send album/photo/video as REPLY, failure replies
  url.rs               // detect + canonicalize IG URLs (post/reel/tv/share), extract shortcode
  queue.rs             // bounded mpsc job queue + single worker loop (concurrency = 1)
  dedup.rs             // moka TTL cache (shortcode -> recently-served)
  extract/
    mod.rs             // Post, Media, MediaKind, ExtractError, InstagramExtractor trait, ExtractorChain
    embed.rs           // EmbedScraper      (in-process HTTPS GET + parse) — PRIMARY
    yt_dlp.rs          // YtDlpExtractor     (subprocess, video workhorse)  — SECONDARY
    gallery_dl.rs      // GalleryDlExtractor (subprocess, images; only when cookies configured)
  media.rs             // temp-dir lifecycle, download helper, RAII cleanup guard
  error.rs             // unified error type
```

### 3.3 Core types & the extractor trait

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind { Image, Video }

#[derive(Debug, Clone)]
pub struct Media {
    pub kind: MediaKind,
    pub url: String,                 // direct CDN URL (preferred: hand to Telegram via InputFile::url)
    pub local_path: Option<std::path::PathBuf>, // set only if we had to download
}

#[derive(Debug, Clone)]
pub struct Post {
    pub author: Option<String>,      // IG username
    pub caption: Option<String>,     // post caption text
    pub media: Vec<Media>,           // 1..=N (carousel)
    pub original_url: String,        // the link the user posted
    pub shortcode: String,           // canonical post id, for dedup/cache keys
}

#[derive(thiserror::Error, Debug)]
pub enum ExtractError {
    #[error("post not found / removed")]   NotFound,
    #[error("login or rate-limit wall")]   Blocked,
    #[error("backend unavailable: {0}")]   Unavailable(String),
    #[error("parse/transient error: {0}")] Transient(String),
}

#[async_trait::async_trait]
pub trait InstagramExtractor: Send + Sync {
    fn name(&self) -> &'static str;
    async fn extract(&self, url: &str, shortcode: &str) -> Result<Post, ExtractError>;
}

pub struct ExtractorChain { backends: Vec<Box<dyn InstagramExtractor>> }

impl ExtractorChain {
    pub async fn extract(&self, url: &str, shortcode: &str) -> Result<Post, ExtractError> {
        let mut last = ExtractError::Unavailable("no backends".into());
        for b in &self.backends {
            match b.extract(url, shortcode).await {
                Ok(p) if !p.media.is_empty() => {
                    tracing::info!(backend = b.name(), shortcode, "extracted");
                    return Ok(p);
                }
                Ok(_)  => last = ExtractError::NotFound,
                Err(e) => { tracing::warn!(backend = b.name(), error = %e, "failed, trying next"); last = e; }
            }
        }
        Err(last)
    }
}
```

### 3.4 Backend ordering (this matters on 1 GB)

Default chain (cookieless): **`EmbedScraper` → `YtDlpExtractor`** (+ `GalleryDlExtractor` only with cookies) **→ `ExternalFallback`** (off by default; §4.8). The external/IP-shift backend sits **last** — it's slowest, least reliable on IG, and a 3rd-party dependency, so it fires only when everything cheaper failed.

- The **embed scraper is a single in-process HTTPS GET** — a few MB, no
  subprocess. It returns images *and* video URLs *and* carousels anonymously, so
  when it succeeds we **never pay the ~300 MB Python (yt-dlp) spawn cost**. It's
  both the most resilient and the lightest first try.
- **yt-dlp** is the fallback for robust video/format handling and is
  upstream-maintained (fixed within days of IG breakage).
- Ordering is config-driven and trivially reorderable (e.g. lead with yt-dlp for
  `/reel/` if you prefer guaranteed video format selection).

---

## 4. Extraction strategy (detailed)

### 4.1 Embed scraper — PRIMARY (in-process, anonymous)

> **UPDATE (2026-06-27): Instagram changed this surface — approach revised.**
> `/embed/captioned/` now serves *browser* UAs a **JS-only `PolarisEmbed` shell
> with no media** (no `__additionalDataLoaded`, no OG tags, no CDN media URLs), so
> the original recipe below returns `NotFound` on current posts. **What works
> now:** GET the **post page** `https://www.instagram.com/p/{shortcode}/` with a
> **crawler UA** (`EMBED_USER_AGENT`, default `facebookexternalhit/1.1`).
> Instagram serves crawlers the OG tags **and** an `application/json`
> Polaris/Relay blob with the full media (incl. reel video), keyed per post by
> `code`. `embed.rs` parses that blob first (`parse_polaris_json`:
> `code == shortcode` → `media_type` / `carousel_media` / `video_versions[0]` /
> `image_versions2.candidates[0]`), with the OG/legacy shapes below as fallback.
> The detail below is kept for historical context. See
> `agent_logs/20260627_193922_logging-and-extraction-fix.md`.

```
GET https://www.instagram.com/p/{shortcode}/embed/captioned/
Headers: desktop-Chrome User-Agent
```

Parse the inline **`window.__additionalDataLoaded('extra', {...})`** JSON blob
(**not** `contextJSON` — that's stale; this is the #1 gotcha). Read
`graphql.shortcode_media` / `shortcode_media`:

| Need | Field |
|---|---|
| author | `owner.username` |
| caption | `edge_media_to_caption.edges[0].node.text` |
| image | `display_url` |
| video | `video_url` |
| carousel | `edge_sidecar_to_children.edges[].node.{display_url\|video_url}` |

Also harvest `og:title`/`og:description`/`og:image` + JSON-LD as a secondary
parse path. **This endpoint still works anonymously in 2026 and survives even
when the main `/p/` page is login-walled** — it's exactly the fallback yt-dlp's
own extractor uses.

**Hardening (the endpoint is intermittent — confirmed empirically, June 2026):**
from a datacenter IP it sometimes returns the media-bearing *captioned* HTML and
sometimes a **JS-shell with no media** (no `shortcode_media`). So:
(1) **retry 2–3× with small backoff**; (2) **parse three response shapes in
order** — the `__additionalDataLoaded('extra', …)` JSON blob, then a plain
`<img src=…>` / `<video>` in the captioned HTML, then the `og:` / JSON-LD tags;
(3) treat a no-media response as a miss and fall through. **CDN URL handling
(critical):** in the embed HTML the URL's `&` separators are HTML-entity-encoded (they appear
as `&amp;`), and in the `__additionalDataLoaded` JSON they are JSON-unicode-escaped;
**decode both back to a literal `&` before fetching**, or the CDN returns
`403 "Bad URL hash"`. The
**entire query string is part of the signature** — capture media URLs *verbatim*;
never trim, reorder, or re-encode params. (Carousels are the embed's genuine weak
spot — the `captioned` view often exposes only the first item; that's where the
chain falls through to yt-dlp / the external fallback.)

### 4.2 yt-dlp — SECONDARY (subprocess, video workhorse)

```bash
yt-dlp -J --no-warnings "https://www.instagram.com/reel/SHORTCODE/"   # metadata, no download
```
- `description` → caption, `uploader`/`channel` → author, formats → video URL,
  carousel = playlist `entries[]`.
- **Weakness:** video-centric. Pure photo posts fail ("No video formats found");
  current master tends to **drop photo children of carousels**. This is why the
  embed scraper leads and gallery-dl backstops images.
- **Cookieless first**, always. (Cookie path is the optional escape hatch, §4.5.)

### 4.3 gallery-dl — conditional (best images/carousels, needs cookies)

```bash
gallery-dl -j "https://www.instagram.com/p/SHORTCODE/"   # JSON metadata
```
- `description` → caption, `username` → author, `display_url`/`video_url` per
  `carousel_media` item.
- In 2026 **requires cookies** and even then hits 401/302/429 from cloud IPs.
  **Only added to the chain when `IG_COOKIES_PATH` is configured** — so under our
  cookieless-first default it is **inactive**.

### 4.4 Hot-config (not baked-in literals)

Keep these as config/env so a break is a minutes-long swap, not a recompile:
`User-Agent`, embed/GraphQL URLs, and — if the GraphQL last-resort path is ever
implemented — the **`doc_id` (rotates every 2–4 weeks)** and
`x-ig-app-id: 936619743392459`.

### 4.5 Datacenter-IP mitigation ladder

We are **cookieless-first**, i.e. step 1 only. Higher steps are documented
escape hatches to enable *only if* blocks are observed:

1. **Heavy caching + low request rate** (active by default; free; zero risk).
2. **Optional burner-account `cookies.txt`** — set `IG_COOKIES_PATH`, which
   activates gallery-dl and adds `--cookies` to yt-dlp. A logged-in session is
   *largely exempt* from the stricter cloud-IP anonymous limits. **Burner is
   disposable** (assume it'll eventually die); **never your real account**;
   logging in binds you to ToS.
3. **Residential IP / home tunnel** — only if 1–2 fail; overkill for hobby use.
4. **Avoid free proxies** — pre-burned, make things worse.

> Per-invocation proxy hook (later contingency): pass `--proxy
> "http://…"` / `socks5h://…` to yt-dlp/gallery-dl, scoped to extraction only.

### 4.6 Caching, dedup & pacing

- **`moka` async TTL cache** keyed by shortcode; window ~10–30 min. Cache
  *result metadata / "already replied"*, **not** media bytes.
- **Pace requests** ≥1–2 s between extraction calls (gallery-dl
  `--sleep-request 2.0`) to avoid tripping rate limits.

### 4.7 Legal note

Scraping *logged-out* public content is the relatively safe zone (*Meta v.
Bright Data*, 2024). Adding burner cookies puts you under ToS. Realistic
legal risk for a personal/friends bot is near-zero; the practical consequences
are throttling and a dead burner.

### 4.8 External fallback backend (IP-shift) — reader / purpose-built IG API

The reason for *any* external backend is the **datacenter-IP problem**: when IG
rate-limits our OCI IP at extraction time, we need something that fetches from a
*different* IP. Investigation (June 2026, with live probing) found the honest
landscape:

- **"Free + reliably fetches IG from a datacenter IP" barely exists.** Generic
  readers (Exa/Tavily/Diffbot) return no media. **Firecrawl 403-blocks
  instagram.com by policy.** Self-hosting any headless-browser reader needs
  4–12 GB — impossible on 1 GB.
- **Jina Reader** (the canonical reader): media-capable, but **anonymous IG is
  hard-blocked (HTTP 451)** — needs a *free API key* + `X-Proxy: auto`, and even
  then it's *unreliable* on IG and weakest on **reel video** and **full
  carousels**. Renders to markdown (parse heuristically), ~3–15 s latency, shared
  URL cache → **must send `X-No-Cache: true`** to avoid leaking our group's links.
- **Better fit than a generic reader: a purpose-built IG API with vendor
  residential IPs** — returns full structured media (all carousel items + reels +
  caption + author) *and* shifts the IP. Free-tier candidates: **EmbedEZ keyless**
  (free, zero-signup, IG-aware — but under-documented), a **RapidAPI `$0` IG
  scraper** (simple GET, structured, residential IPs — but quota unconfirmed +
  volatile longevity), **Bright Data IG scraper** (5k credits/mo recurring, most
  robust — but async/KYC-heavy).
- **Pure.md**: reliably returns **caption + author** from IG (clears the wall) but
  **masks all media URLs** → only useful as an optional *text-rescue* enrichment
  (call only when an earlier backend got media but no caption), never a media
  source.

**Decision:** add ONE swappable external backend behind the trait, **last in the
chain**, **disabled by default** and activated by config (`FALLBACK_PROVIDER` +
key). **Spike `EmbedEZ` keyless first** (free, simplest); if it underperforms on
real posts, swap to a **RapidAPI `$0`** provider or **Jina** (free key). Document
**Bright Data** as the robustness upgrade. This is defense for the *rare* IP
block, not a primary path — the primary path is more robust than feared (§6).

**Failure detection (so the chain falls through correctly), in order:** HTTP
status (451/403 → `Blocked`; 404/410 → `NotFound`; 429/5xx → `Transient`) →
provider error code → login-wall/empty sniff (tiny body, "log in", "see this
content") → **media count**. **"Text extracted but zero `/v/…` CDN media URLs" is
a FAILURE** for the media role — return `Err` so the chain (or the graceful
reply) takes over. Wrap the call in a 20–30 s `tokio::time::timeout`, 1 retry max.

---

## 5. Telegram integration

- **Disable Privacy Mode** in BotFather (`/setprivacy` → Disable) **then remove &
  re-add the bot to the group** — the re-add is **mandatory** and the
  silent-failure trap. Without it the bot only sees commands/replies, not normal
  messages. (A bot still cannot read messages from *other bots*.)
- **Detect URLs** via message `entities` (`MessageEntityKind::Url` → slice text
  by offset/length; `TextLink(url)` → read the embedded URL), plus a regex
  fallback: `instagram\.com/(p|reel|reels|tv)/[\w-]+`. Match `instagram.com`,
  `www.instagram.com`, `instagr.am`; strip query params (`?igsh=…`). Edited
  messages arrive as a separate `edited_message` update — handling them is
  optional (deferred).
- **Reply** with `.reply_parameters(ReplyParameters::new(msg.id))`.
- **Send:** 1 item → `send_photo`/`send_video`; **2–10 → `send_media_group`**
  (carousels; batch >10 into chunks of 10; mixed photo+video allowed).
- **Caption ladder:** media caption limit is **1024 chars**. If the IG caption is
  longer, put a truncated caption (word-boundary + "…") on the media and post the
  **full caption as a separate `sendMessage` reply** (4096 limit; split if
  needed). Both replies target the original message.
- **Rate limits:** ~1 msg/s sustained per chat, ~20/min per group, 50 MB upload
  cap (cloud Bot API). Wrap the bot in teloxide's **`Throttle` adaptor**
  (innermost: `Bot::from_env().throttle(Limits::default())`) + a `RetryAfter`
  sleep-and-retry fallback. Set `link_preview_options`/`disable_web_page_preview`
  on our replies so Telegram doesn't also unfurl the echoed link.

---

## 6. Media delivery ladder

Telegram fetch-by-URL limits are ~**20 MB** (video) / ~**5 MB** (photo);
multipart upload cap is **50 MB** (verify against the live Bot API "Sending
files" section when implementing).

1. **`InputFile::url(cdn_url)`** first — Telegram fetches it; zero disk/RAM.
   Covers most photos (<5 MB) and short reels (<20 MB).
2. On failure (403 / can't-fetch / by-URL too big) → **download to
   `TEMP_DIR/{shortcode}/` then upload multipart** (≤50 MB). RAII guard deletes
   the temp dir on success/error/panic.
3. **>50 MB** → reply with caption + original link + a short "too large to mirror"
   note.

> **Empirically (June 2026):** IG CDN URLs (`*.cdninstagram.com` / `*.fbcdn.net`)
> **download fine from our datacenter IP — HTTP 200, NOT IP-bound — for ~4 days**
> (read each URL's own `oe=` param, hex→epoch; don't assume a fixed TTL). But they
> require the **`&amp;amp;`→`&` decode + verbatim query string** (§4.1), and
> **Telegram's by-URL fetcher is flaky** (returns "wrong file identifier" even on
> browser-valid URLs) **and** size-capped (5/20 MB). So step 1 is opportunistic;
> **step 2 (download-then-upload) is the reliable path.** This also means the
> primary path's own output is directly deliverable from the OCI box — the
> external fallback (§4.8) is only for the rarer case where IG blocks our IP at
> *extraction* time.

---

## 7. Deployment — OCI E2.1.Micro (x86-64)

**Shape:** 1/8 OCPU (burstable), 1 GB RAM, AMD EPYC, up to 50 Mbps, 2 free
instances per tenancy (run a spare). **OS: Ubuntu 24.04 LTS.**

### 7.1 Tooling install

```bash
sudo apt update && sudo apt install -y ffmpeg
sudo curl -L https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp \
  -o /usr/local/bin/yt-dlp && sudo chmod a+rx /usr/local/bin/yt-dlp
# Rust via rustup; build with --release on the box or cross-build x86_64-unknown-linux-gnu/musl
```
(Do **not** use the distro `apt`/`dnf` yt-dlp package — it can't self-update.)

### 7.2 Memory safety trifecta

**(a) 2 GB swap:**
```bash
sudo fallocate -l 2G /swapfile && sudo chmod 600 /swapfile
sudo mkswap /swapfile && sudo swapon /swapfile
echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab
echo 'vm.swappiness=10' | sudo tee /etc/sysctl.d/99-swap.conf && sudo sysctl --system
```
**(b) concurrency = 1** (single worker; enforced in `queue.rs`).
**(c) systemd `MemoryMax`** (below) so a runaway yt-dlp (~570 MB spike on some
media) is killed in its own cgroup, not the bot.

### 7.3 systemd service

```ini
# /etc/systemd/system/igbot.service
[Unit]
Description=Instagram Telegram Bot
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=botuser
WorkingDirectory=/opt/igbot
EnvironmentFile=/etc/igbot/igbot.env
ExecStart=/opt/igbot/igbot
Restart=always
RestartSec=5
StartLimitIntervalSec=300
StartLimitBurst=5
MemoryHigh=650M
MemoryMax=800M
OOMPolicy=continue
TasksMax=128
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/opt/igbot
StandardOutput=journal
StandardError=journal
SyslogIdentifier=igbot

[Install]
WantedBy=multi-user.target
```
Secrets in `/etc/igbot/igbot.env` (chmod 600, owned by botuser):
`TELEGRAM_BOT_TOKEN=…`, `ALLOWED_CHAT_IDS=…`, `TEMP_DIR=/opt/igbot/tmp`,
`CACHE_TTL_SECS=1200`, `YT_DLP_PATH=/usr/local/bin/yt-dlp`. (No
`IG_COOKIES_PATH` under cookieless-first.)

### 7.4 yt-dlp auto-update (daily timer)

```ini
# /etc/systemd/system/yt-dlp-update.service       # /etc/systemd/system/yt-dlp-update.timer
[Unit]                                            [Unit]
Description=Update yt-dlp                          Description=Daily yt-dlp update
After=network-online.target                        [Timer]
Wants=network-online.target                        OnCalendar=daily
[Service]                                          RandomizedDelaySec=1h
Type=oneshot                                        Persistent=true
ExecStart=/usr/local/bin/yt-dlp -U                 [Install]
                                                   WantedBy=timers.target
```
`sudo systemctl enable --now yt-dlp-update.timer`. On-demand recovery when IG
breaks: `sudo systemctl start yt-dlp-update.service` (consider `--update-to
nightly`).

### 7.5 Idle-reclaim keepalive

E2.1.Micro **is** subject to idle reclamation (CPU **and** network < 20% over
7 days; the memory threshold is Ampere-only). Long-polling traffic largely
covers it; add a tiny safety net (5-min timer/cron):
```
*/5 * * * * /usr/bin/curl -s -o /dev/null https://api.telegram.org
```

### 7.6 Networking

**Long polling = zero inbound ports** (outbound HTTPS only; works on OCI's
default-deny ingress with no changes). No domain, no TLS, no firewall edits.

> If ever switching to webhook: open the port in **both** the OCI Security
> List/NSG **and** the instance's local firewall (Ubuntu ships a restrictive
> `iptables` REJECT rule — the classic OCI gotcha), then use Caddy for auto-HTTPS.

---

## 8. Resilience & ops

- **Fallback chain** is the core defense — reorder/add a backend in one line.
- **Auto-update** yt-dlp (timer) silently fixes most breakage.
- **Hot-config** the embed UA / URLs (and `doc_id` if GraphQL is added).
- **Graceful failure reply** when the whole chain fails: "Couldn't fetch that —
  Instagram may be rate-limiting, or the post is private/removed."
- **Temp-file RAII** + startup sweep of `TEMP_DIR` for crash orphans.
- **Burner escape hatch** (deferred): set `IG_COOKIES_PATH` to activate
  cookie-backed extraction without code changes.
- **Watch** an active InstaFix fork + yt-dlp releases as early-warning for the
  scraper path.

---

## 9. Build phases

See `CHECKLIST.md` for the actionable task list. Summary:

- **Phase 0 — Scaffold:** Cargo project, config, teloxide long-poll skeleton,
  privacy off + re-add, detect & *log* IG URLs (no extraction).
- **Phase 1 — Cookieless MVP:** `EmbedScraper` + `Post` + reply with media. A
  working, useful, zero-risk bot exists here.
- **Phase 2 — Chain:** add `YtDlpExtractor` (+ conditional `GalleryDlExtractor`)
  behind the trait with fallback ordering.
- **Phase 3 — Robustness:** moka dedup cache, single-worker bounded queue,
  Throttle, graceful failure replies, RAII temp cleanup, media-delivery ladder.
- **Phase 4 — Deploy:** systemd unit, swap, MemoryMax, yt-dlp update timer,
  keepalive.
- **Phase 5 — Ops:** hot-config, light logging/monitoring.

---

## 10. Known risks & mitigations

| Risk | Likelihood | Mitigation |
|---|---|---|
| IG blocks/rate-limits the datacenter IP | Medium (intermittent) | Caching + low rate; cookieless embed survives login-walls; burner escape hatch if persistent |
| yt-dlp / embed parser breaks on IG change | Medium (periodic) | Auto-update yt-dlp; fallback chain; hot-config UA/`doc_id`; in-process embed maintained by us |
| OOM on 1 GB during media handling | Low | concurrency=1 + 2 GB swap + systemd `MemoryMax`; prefer `InputFile::url` (no download) |
| Video > 50 MB | Low–Med | Media ladder step 3: reply with link + caption note |
| Instance reclaimed for idleness | Low | Long-poll traffic + 5-min keepalive |
| Burner account banned (only if enabled) | High *if used* | Disposable by design; cookieless is the default |
| Third-party fix-domains die/monetize | N/A | We don't depend on them (in-process technique only) |

---

## 11. Open questions / future

- **Ampere A1 upgrade** (24 GB ARM) — removes memory constraints if provisionable.
- **Webhook** — only if scaling to many high-traffic chats.
- **GraphQL last-resort backend** — powerful but high-maintenance (`doc_id`
  rotation; Rust `reqwest` TLS-fingerprint acceptance to IG's GraphQL is
  *unverified*). Add only if embed+yt-dlp prove insufficient.
- **Residential proxy / home tunnel** — only if cookieless + burner both fail.

---

*Plan synthesized from a four-track investigation (Telegram/Rust stack,
Instagram extraction, OCI deployment, architecture & prior art), June 2026.
Key references: teloxide 0.17 docs; yt-dlp & gallery-dl repos/issues; InstaFix
(archived, technique only); Instaloader troubleshooting docs (cloud-IP limits);
OCI Always Free resource docs.*
