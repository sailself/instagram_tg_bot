# igbot

[![CI](https://github.com/sailself/instagram_tg_bot/actions/workflows/ci.yml/badge.svg)](https://github.com/sailself/instagram_tg_bot/actions/workflows/ci.yml)
[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/sailself/instagram_tg_bot)

A Rust Telegram bot that watches a group chat and, whenever someone posts an
**Instagram** link (post `/p/`, reel `/reel/`, `/tv/`, or a **story** /
highlight `/stories/…` — stories need a login, see below) or a **Threads** link
(`threads.com` / `threads.net` `/@user/post/…` or `/share/…`), replies to that message with the
post's **media (images/videos, incl. carousels), caption, and author** — and, for
Threads' text-first posts, the **text** itself when there's no media. Free to
run; designed for an **OCI Always Free 1 OCPU / 1 GB** VM.

## How it works

```
group msg ─(teloxide, long-poll)→ skip the bot's own (forwarded) messages
        → detect IG / Threads link → ingress dedup
        → bounded queue → single worker → resolve Threads /share alias
        → route by host (+ post vs story) → canonical dedup → extractor chain
        → reply (album / photo / video / text)
```

Links are routed by **host** (never by shortcode — IG and Threads share the same
code alphabet), and dedup keys are namespaced per platform and kind (`ig:` /
`ig-story:` / `th:`). Threads `/share/<token>` aliases are resolved to their
clean canonical `/@user/post/<code>` permalink inside the single worker before
extraction. The alias and canonical post are both deduplicated after a
successful delivery.

The bot's replies end with a `🔗 <link>` footer. A message the bot itself
authored — typically one of its replies **forwarded** from another chat — is
ignored, so the bot never mirrors its own mirror. (Only *this* bot is filtered;
forwards from other users and bots are processed normally.)

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

**Instagram story chain** (`/stories/<user>/<id>/` items and
`/stories/highlights/<id>/`): Instagram serves stories **only to logged-in
sessions** — anonymously, the story page is a login wall (its lone `og:image`
is the profile picture). So this chain is **gallery-dl → yt-dlp**, both with
cookies, and exists **only when `IG_COOKIES_PATH` is set**. gallery-dl goes first
because yt-dlp's story extractor is **video-only**: with gallery-dl missing, an
image story fails with a reply that says so (never a misleading "expired").
Without cookies a
story link gets a short "needs a login session" reply (or is ignored entirely
with `IG_STORIES_ENABLED=0`). Highlights are capped at `IG_STORY_MAX_ITEMS`
(default 10, one album) with a "showing N of M" note. How to obtain and install
the cookies file: [**Cookies**](#cookies-optional--instagram-stories--gallery-dl) below.

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
- **Runtime:** `ffmpeg` + `yt-dlp` (the standalone binary) + `gallery-dl` — all
  installed for you by `deploy/setup.sh` on the server. gallery-dl only matters
  once cookies are set, but it is then the **one backend that fetches image
  stories** (yt-dlp's story extractor is video-only). Locally: `pip install gallery-dl`.
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
base=igbot-v0.4.0-linux-x86_64.tar.gz
url=https://github.com/sailself/instagram_tg_bot/releases/download/v0.4.0
curl -L -O "$url/$base" && curl -L -O "$url/$base.sha256"
sha256sum -c "$base.sha256" && tar -xzf "$base"   # → /tmp/igbot

# 3. install (ffmpeg, yt-dlp, gallery-dl, 2 GB swap, service user, systemd units)
cd ~/instagram_tg_bot
sudo bash deploy/setup.sh /tmp/igbot

# 4. configure + start
sudo nano /etc/igbot/igbot.env            # set TELEGRAM_BOT_TOKEN + ALLOWED_CHAT_IDS
sudo systemctl start igbot
journalctl -u igbot -f
```

`deploy/setup.sh` also creates a **2 GB swap**, a daily **yt-dlp + gallery-dl
auto-update** timer, and a 5-minute **keepalive** (so the always-free VM isn't reclaimed for
idleness). The bot uses **long polling**, so **no inbound ports / TLS / domain**
are required. Memory is guarded by `MemoryMax=800M` + single-worker concurrency.

## Upgrading to a new release

It's a **binary swap** — no new system deps, and new config ships with working
defaults, so `igbot.env` usually needs no changes. One exception for **v0.3.0**:
if your `igbot.env` pins `JOB_TIMEOUT_SECS=90`, remove the line (or raise it) —
the default is now 300 s so multi-album deliveries aren't cut off mid-send.
One exception for **v0.4.0**: Instagram **Stories** need `gallery-dl` on the box
(yt-dlp alone only covers *video* stories), and `deploy/setup.sh` installs it on
fresh setups only. On an existing VM, run the gallery-dl lines from step 2 of
`deploy/setup.sh` once (a venv at `/opt/gallery-dl`, symlinked into
`/usr/local/bin`), then copy the updated `deploy/yt-dlp-update.service` into
`/etc/systemd/system/` and `systemctl daemon-reload`. Stories also need cookies
(see [Cookies](#cookies-optional--instagram-stories--gallery-dl)); without
either, posts and Threads keep working exactly as before.
Replace `v0.4.0` below with the tag you're moving to.

```bash
# 1. download + verify the new release
cd /tmp
base=igbot-v0.4.0-linux-x86_64.tar.gz
url=https://github.com/sailself/instagram_tg_bot/releases/download/v0.4.0
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
summary line). From v0.4.0 it also logs `yt-dlp found`, and `gallery-dl found`
once cookies are set — a `not runnable` WARN there means a backend is missing.
Then post a real link in your group to confirm.

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
| `IG_COOKIES_PATH` | Netscape `cookies.txt` from a **burner** account; enables the gallery-dl + cookie path **and Instagram Stories**. Must be read-**write** for the service, i.e. under `/opt/igbot/` — see [Cookies](#cookies-optional--instagram-stories--gallery-dl) |
| `IG_STORIES_ENABLED` | set `0/false/no/off` to ignore story links instead of replying "needs a login session" (default on) |
| `IG_STORY_MAX_ITEMS` | max items mirrored per story/highlight link (default 10; `0` = unlimited) |
| `FALLBACK_PROVIDER` / `JINA_API_KEY` | enables the external fallback (off by default) |

Brittle bits (User-Agent, endpoints, timeouts) are hot-config via env so a break
is a config change, not a recompile.

## Cookies (optional — Instagram Stories + gallery-dl)

The default chains are **cookieless**. You need this section only for
**Instagram Stories / highlights** (login-walled) or to turn on the gallery-dl
backstop for posts. **Threads never uses cookies**: its chain is an anonymous
in-process scrape, and no code path reads Threads cookies today.

Rules first — getting these wrong burns an account or the bot's IP:

- **Burner account only.** Never your real Instagram login. A session driven
  from a datacenter IP can be challenged or banned at any time; the account is
  disposable by design.
- **Dedicated browser profile, kept logged in.** Logging out in the browser
  invalidates the `sessionid` server-side, and the bot's copy dies with it.
- **Never commit or paste the file.** `cookies.txt` is in `.gitignore`; the bot
  logs only *whether* a cookies path is set, never its contents.

### 1. Export a `cookies.txt` (Netscape format)

yt-dlp and gallery-dl both read the classic **Netscape / Mozilla `cookies.txt`**:
one cookie per line, seven **tab**-separated fields. Any of these produces it.

**Browser extension (simplest).** In the burner profile, log into
[instagram.com](https://www.instagram.com) and stay on the site, then:

- Chrome / Edge / Brave — [Get cookies.txt LOCALLY](https://chromewebstore.google.com/detail/get-cookiestxt-locally/cclelndahbckbenkjhflpdbgdldlbecc):
  click the extension → **Export** (current site) → save as `cookies.txt`.
- Firefox — [cookies.txt](https://addons.mozilla.org/firefox/addon/cookies-txt/):
  **Current Site** → save.

Both write Netscape format and include `HttpOnly` cookies (`sessionid` is one).
Some exporters prefix those lines with `#HttpOnly_` — that is normal, **don't
delete them**; both tools understand the prefix.

**yt-dlp from a Firefox profile (no extension).** On the machine that has the
burner profile:

```bash
yt-dlp --cookies-from-browser firefox --cookies cookies.txt \
       --skip-download --no-warnings "https://www.instagram.com/p/<any public post>/"
```

yt-dlp loads the browser's jar and dumps it to `cookies.txt` on exit (even if
that extraction fails). This exports **every** cookie in the profile, which is
another reason it must be a dedicated burner profile. `--cookies-from-browser
chrome` often cannot decrypt recent Chrome builds (App-Bound Encryption,
especially on Windows) — use Firefox or the extension.

**By hand (last resort).** DevTools → Application / Storage → Cookies →
`https://www.instagram.com`, and write the lines yourself:

```
# Netscape HTTP Cookie File
.instagram.com	TRUE	/	TRUE	1800000000	sessionid	<value>
.instagram.com	TRUE	/	TRUE	1800000000	csrftoken	<value>
.instagram.com	TRUE	/	TRUE	1800000000	ds_user_id	<value>
.instagram.com	TRUE	/	TRUE	1800000000	mid	<value>
.instagram.com	TRUE	/	TRUE	1800000000	ig_did	<value>
```

Fields: `domain`, `include-subdomains` (`TRUE` when the domain starts with a
dot), `path`, `secure`, `expiry` (Unix seconds; `0` = session cookie), `name`,
`value` — separated by real **tabs**, not spaces.

**What must be in it.** `sessionid` *is* the login: gallery-dl's Instagram
extractor requires it and yt-dlp uses its presence to decide it is logged in.
`csrftoken` (yt-dlp sends it as `X-CSRFToken`) and `ds_user_id` should be there
too; `mid` / `ig_did` / `datr` make the session look like the same device and
reduce "suspicious login" checkpoints. Exporting the whole site (the default)
captures all of them.

**Threads too?** It is the same Meta login, so if you want Threads cookies in
the same file, repeat the export while on
[threads.com](https://www.threads.com) — a `cookies.txt` is multi-domain, and
extra `.threads.com` / `.threads.net` lines are harmless. Just know that
**nothing in the bot reads them today**; the Threads chain is cookieless by
design and the file is only ever handed to the Instagram extractors.

### 2. Install it on the server

First make sure **gallery-dl is installed** next to yt-dlp (`deploy/setup.sh`
does this; locally, `pip install gallery-dl`). yt-dlp's story extractor is
video-only, so gallery-dl is the backend that fetches **image** stories. The bot
warns at startup if cookies are set but gallery-dl can't be run.

Put the file where the **service can read *and write* it**. yt-dlp rewrites the
cookie jar on every exit and exits with a traceback when it can't — the bot
sees that as a failed run — and gallery-dl rewrites it through a temp file in
the same directory. `deploy/igbot.service` runs under `ProtectSystem=strict`,
so the only writable location is `ReadWritePaths=/opt/igbot`. **Not
`/etc/igbot`.**

```bash
# from your machine
scp cookies.txt ubuntu@<vm>:/tmp/cookies.txt

# on the VM
sudo install -o botuser -g botuser -m 0600 /tmp/cookies.txt /opt/igbot/cookies.txt
shred -u /tmp/cookies.txt 2>/dev/null || rm -f /tmp/cookies.txt

# point the bot at it (first time only — the path is read at startup)
echo 'IG_COOKIES_PATH=/opt/igbot/cookies.txt' | sudo tee -a /etc/igbot/igbot.env
sudo systemctl restart igbot
journalctl -u igbot -n 20 | grep -o 'cookies=[a-z]*'      # expect cookies=true
```

Replacing the file's *contents* later needs **no restart**: the path is handed
to yt-dlp / gallery-dl on every run and they read it fresh. Keep ownership
`botuser:botuser` — a manual `yt-dlp --cookies …` test run as root rewrites the
file as root and the service loses write access. gallery-dl's temp-file rewrite
recreates the file with the process umask, so its mode may loosen from `0600`
to `0644` after the first story fetch; tighten it again if other users share
the VM.

### 3. Verify (as the service user)

```bash
sudo -u botuser gallery-dl -j --cookies /opt/igbot/cookies.txt \
     "https://www.instagram.com/stories/<a user with a live story>/" | head -c 400
```

JSON means the session works (an empty result just means no live story);
`401` / `403` / `login` / `challenge` in the output means the cookies are dead
or the account is being challenged. Then post a `/stories/<user>/<id>/` link in
an allowed chat.

### 4. When it stops working

The bot tells you. A story link answered with **"Instagram rejected the bot's
login session (it may have expired)"** means the cookies are dead: re-export
from the still-logged-in burner profile and overwrite `/opt/igbot/cookies.txt`.
**"Couldn't find that story — it may have expired"** is *not* a cookie problem;
stories vanish after 24 h. Instagram sessions normally live for months, but a
password change, a browser logout, or a security checkpoint on the burner
kills them immediately.

## Limitations & notes

- Instagram extraction is inherently fragile; the chain + caching + graceful
  failure replies absorb intermittent blocks. The crawler-UA embed path is the
  primary mechanism and may need an `EMBED_USER_AGENT` change if IG shifts again.
- Threads extraction is gated on a coherent desktop-browser header set; if
  Threads shifts, change `THREADS_USER_AGENT` / `THREADS_SEC_CH_UA` (no
  recompile). An empty-shell response is classified as a failure, so users get a
  graceful reply rather than silence. The Threads scrape, repost/quote nesting,
  and poll rendering still want **live validation** against real posts.
- Instagram **Stories** are login-walled: without `IG_COOKIES_PATH` the bot can
  only say so. With cookies, the story chain (gallery-dl → yt-dlp) was validated
  live on 2026-09-04: a video story via yt-dlp, an image story via gallery-dl.
  **Image stories need gallery-dl installed** — yt-dlp's story extractor is
  video-only and reports nothing for them. Stories expire after 24 h; an
  expired item reads as "not found".
- The self-forward guard keys on Telegram's forward origin. A forward whose
  origin is hidden (user privacy setting, or a channel the bot posted into) is
  not attributable to the bot and is processed like any other message.
- Albums are sent as up to 10 items (Telegram album max); extras are noted.
- Bot API upload cap is 50 MB; larger videos get a link + note instead.
- This scrapes public, logged-out content. Adding burner cookies is opt-in and
  disposable.
