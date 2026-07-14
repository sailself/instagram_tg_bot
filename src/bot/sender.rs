//! Build the reply: caption composition (≤1024 UTF-16 units per message; long
//! text continues across the album-chunk captions and only the remainder past
//! the last chunk is dropped — never re-sent as a follow-up message), single
//! media vs chunked albums (`send_media_group`, 2–10 items), and the delivery
//! ladder — `InputFile::url` first, then download-then-upload, then a
//! link+note fallback (PLAN §5 / §6).
//!
//! Send-failure policy: a *post-send client timeout* is ambiguous — Telegram
//! may still deliver the message — so it must never be retried or re-attempted
//! via another route (duplicate risk); it surfaces as the job's failure.
//! Definite failures (flood-wait, connect-phase errors) are retried once.
//!
//! Telegram measures all length limits in **UTF-16 code units**, not Unicode
//! scalar values, so every length check here uses [`utf16_len`].

use crate::bot::TgBot;
use crate::config::Config;
use crate::extract::{Media, MediaKind, Post};
use crate::media::{download_capped, temp_filename, DownloadError};
use crate::queue::Job;
use crate::urls::Platform;
use std::path::{Path, PathBuf};
use std::time::Duration;
use teloxide::prelude::*;
use teloxide::types::{
    ChatAction, ChatId, InputFile, InputMedia, InputMediaPhoto, InputMediaVideo, LinkPreviewOptions,
    ReplyParameters,
};
use url::Url;

const CAPTION_LIMIT: usize = 1024; // UTF-16 code units
const TEXT_LIMIT: usize = 4096; // UTF-16 code units
const MAX_ALBUM: usize = 10; // Telegram media-group max

fn rp(job: &Job) -> ReplyParameters {
    ReplyParameters::new(job.reply_to)
}

pub async fn deliver(
    bot: &TgBot,
    http: &reqwest::Client,
    cfg: &Config,
    job: &Job,
    post: Post,
) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    let n = post.media.len();
    tracing::debug!(media = n, "delivering");

    // Text-only post (Threads is text-first): no media — deliver the caption +
    // author + link as text, never a media call (PLAN §4.8 / Threads design).
    if n == 0 {
        send_text_post(bot, job, &post).await?;
        tracing::debug!(
            media = 0,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "delivered text-only"
        );
        return Ok(());
    }

    // One caption per album chunk: long text continues across the chunks'
    // captions instead of being dropped after the first message.
    let captions = compose_captions(&post, job.platform, n.div_ceil(MAX_ALBUM));
    if n == 1 {
        let caption = captions.first().map(String::as_str).unwrap_or("");
        send_one(bot, http, cfg, job, &post.media[0], caption).await?;
    } else {
        send_album(bot, http, cfg, job, &post.media, &captions).await?;
    }
    tracing::debug!(media = n, elapsed_ms = started.elapsed().as_millis() as u64, "delivered");
    Ok(())
}

/// Deliver a media-less post (Threads text / poll / link-card) as text: header
/// (`🧵 @author`) + caption + link, split into ≤4096-UTF-16 chunks. The chain
/// only yields a text-only Post when the caption is non-blank, so this never
/// sends an empty message.
async fn send_text_post(bot: &TgBot, job: &Job, post: &Post) -> anyhow::Result<()> {
    let full = compose_full_text(post, job.platform);
    for chunk in split_flow(&full, TEXT_LIMIT) {
        reply_text(bot, job, &chunk).await?;
    }
    Ok(())
}

async fn send_one(
    bot: &TgBot,
    http: &reqwest::Client,
    cfg: &Config,
    job: &Job,
    m: &Media,
    caption: &str,
) -> anyhow::Result<()> {
    // 1) Opportunistic URL send (zero disk).
    match send_with_retry(|| try_single_url(bot, job, m, caption)).await {
        Ok(()) => return Ok(()),
        // May have landed on Telegram's side — re-sending via the download
        // route could deliver the media twice. Surface the failure instead.
        Err(e) if may_have_landed(&e) => return Err(e),
        // Expected ladder step (Telegram can't use every CDN URL) — not a bug.
        Err(e) => tracing::info!(error = %e, "URL send rejected; downloading and re-uploading instead"),
    }

    // 2) Download then upload (≤ max_upload_bytes).
    let tmp = tempfile::Builder::new()
        .prefix("igbot")
        .tempdir_in(&cfg.temp_dir)?;
    let dest = tmp.path().join(temp_filename(&job.shortcode, 0, m.kind));
    match download_capped(http, &m.url, &dest, cfg.max_upload_bytes, &cfg.user_agent).await {
        Ok(()) => send_with_retry(|| upload_local(bot, job, m.kind, &dest, caption)).await,
        // 3) Too big / failed → link + note.
        Err(DownloadError::TooBig) => {
            let mb = cfg.max_upload_bytes / (1024 * 1024);
            reply_text(bot, job, &format!("⚠️ Media is larger than {mb} MB — can't mirror it here.\n\n{caption}")).await
        }
        Err(DownloadError::Failed(e)) => {
            tracing::warn!(error = %e, "download failed");
            reply_text(bot, job, &format!("⚠️ Couldn't fetch the media.\n\n{caption}")).await
        }
    }
}

/// Send a single photo/video by its CDN URL (Telegram fetches it server-side).
async fn try_single_url(bot: &TgBot, job: &Job, m: &Media, caption: &str) -> anyhow::Result<()> {
    let u = Url::parse(&m.url)?;
    match m.kind {
        MediaKind::Image => {
            bot.send_photo(job.chat_id, InputFile::url(u))
                .caption(caption)
                .reply_parameters(rp(job))
                .await?;
        }
        MediaKind::Video => {
            bot.send_video(job.chat_id, InputFile::url(u))
                .caption(caption)
                .reply_parameters(rp(job))
                .await?;
        }
    }
    Ok(())
}

/// Upload an already-downloaded local file as a single photo/video.
async fn upload_local(
    bot: &TgBot,
    job: &Job,
    kind: MediaKind,
    path: &Path,
    caption: &str,
) -> anyhow::Result<()> {
    let file = InputFile::file(path);
    match kind {
        MediaKind::Image => {
            bot.send_photo(job.chat_id, file)
                .caption(caption)
                .reply_parameters(rp(job))
                .await?;
        }
        MediaKind::Video => {
            bot.send_video(job.chat_id, file)
                .caption(caption)
                .reply_parameters(rp(job))
                .await?;
        }
    }
    Ok(())
}

/// Send a carousel as successive media groups of up to 10 items each. Chunk
/// `i` carries `captions[i]` (long text continues across the chunks); chunks
/// past the last caption go uncaptioned.
async fn send_album(
    bot: &TgBot,
    http: &reqwest::Client,
    cfg: &Config,
    job: &Job,
    media: &[Media],
    captions: &[String],
) -> anyhow::Result<()> {
    for (i, chunk) in media.chunks(MAX_ALBUM).enumerate() {
        let cap = captions.get(i).map(String::as_str).unwrap_or("");
        send_chunk(bot, http, cfg, job, chunk, cap).await?;
    }
    Ok(())
}

async fn send_chunk(
    bot: &TgBot,
    http: &reqwest::Client,
    cfg: &Config,
    job: &Job,
    items: &[Media],
    caption: &str,
) -> anyhow::Result<()> {
    // A media group needs 2–10 items; a lone item must go via send_photo/video.
    if items.len() == 1 {
        return send_one(bot, http, cfg, job, &items[0], caption).await;
    }

    // 1) Try the chunk by URL.
    match send_with_retry(|| try_album_urls(bot, job, items, caption)).await {
        Ok(()) => return Ok(()),
        // May have landed on Telegram's side (it server-fetches all the URLs
        // before answering, which can outlast the client timeout) — the
        // download route could deliver the album twice. Surface the failure.
        Err(e) if may_have_landed(&e) => return Err(e),
        // Expected ladder step (Telegram can't use every CDN URL) — not a bug.
        Err(e) => tracing::info!(error = %e, "album URL send rejected; downloading and re-uploading instead"),
    }

    // 2) Download each item (skip failures / oversize), then upload.
    let tmp = tempfile::Builder::new()
        .prefix("igbot")
        .tempdir_in(&cfg.temp_dir)?;
    let mut locals: Vec<(MediaKind, PathBuf)> = Vec::new();
    for (i, m) in items.iter().enumerate() {
        let dest = tmp.path().join(temp_filename(&job.shortcode, i, m.kind));
        match download_capped(http, &m.url, &dest, cfg.max_upload_bytes, &cfg.user_agent).await {
            Ok(()) => locals.push((m.kind, dest)),
            Err(e) => tracing::warn!(idx = i, error = ?e, "skipping album item"),
        }
    }

    match locals.len() {
        0 => reply_text(bot, job, &format!("⚠️ Couldn't fetch the media.\n\n{caption}")).await,
        // Exactly one survivor can't form a group — send it as a single.
        1 => send_with_retry(|| upload_local(bot, job, locals[0].0, &locals[0].1, caption)).await,
        _ => send_with_retry(|| try_album_files(bot, job, &locals, caption)).await,
    }
}

/// Run a Telegram send, retrying once for failures where the request
/// **definitely did not post** (see [`retry_delay`]). Everything else — API
/// rejections and ambiguous post-send timeouts — surfaces immediately.
async fn send_with_retry<F, Fut>(op: F) -> anyhow::Result<()>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    match op().await {
        Ok(()) => Ok(()),
        Err(e) => match retry_delay(&e) {
            Some(d) => {
                tracing::warn!(error = %e, delay_ms = d.as_millis() as u64, "send failed; retrying once");
                tokio::time::sleep(d).await;
                op().await
            }
            None => Err(e),
        },
    }
}

/// Backoff for errors where the request provably never posted: a flood-wait
/// (Telegram rejected the call; sleep what it asked, capped so it can't eat
/// the job timeout) or a connect-phase failure (never reached the API). API
/// errors are deterministic and post-send timeouts are ambiguous — no retry.
fn retry_delay(e: &anyhow::Error) -> Option<Duration> {
    match e.downcast_ref::<teloxide::RequestError>()? {
        teloxide::RequestError::RetryAfter(s) => Some(s.duration().min(Duration::from_secs(30))),
        teloxide::RequestError::Network(n) if n.is_connect() => Some(Duration::from_secs(2)),
        _ => None,
    }
}

/// True when a send failed with a **post-send client timeout**: the request
/// reached Telegram, which may still process and deliver it. Such a send must
/// never be retried or re-attempted via another route — the user could get
/// the content twice, which is worse than the single failure notice.
fn may_have_landed(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<teloxide::RequestError>(),
        Some(teloxide::RequestError::Network(n)) if n.is_timeout() && !n.is_connect()
    )
}

async fn try_album_urls(
    bot: &TgBot,
    job: &Job,
    items: &[Media],
    caption: &str,
) -> anyhow::Result<()> {
    let mut group: Vec<InputMedia> = Vec::with_capacity(items.len());
    for (i, m) in items.iter().enumerate() {
        let file = InputFile::url(Url::parse(&m.url)?);
        group.push(build_input_media(m.kind, file, (i == 0).then_some(caption)));
    }
    bot.send_media_group(job.chat_id, group)
        .reply_parameters(rp(job))
        .await?;
    Ok(())
}

async fn try_album_files(
    bot: &TgBot,
    job: &Job,
    locals: &[(MediaKind, PathBuf)],
    caption: &str,
) -> anyhow::Result<()> {
    let mut group: Vec<InputMedia> = Vec::with_capacity(locals.len());
    for (i, (kind, path)) in locals.iter().enumerate() {
        let file = InputFile::file(path);
        group.push(build_input_media(*kind, file, (i == 0).then_some(caption)));
    }
    bot.send_media_group(job.chat_id, group)
        .reply_parameters(rp(job))
        .await?;
    Ok(())
}

fn build_input_media(kind: MediaKind, file: InputFile, caption: Option<&str>) -> InputMedia {
    match kind {
        MediaKind::Image => {
            let mut p = InputMediaPhoto::new(file);
            if let Some(c) = caption {
                p = p.caption(c);
            }
            InputMedia::Photo(p)
        }
        MediaKind::Video => {
            let mut v = InputMediaVideo::new(file);
            if let Some(c) = caption {
                v = v.caption(c);
            }
            InputMedia::Video(v)
        }
    }
}

pub async fn reply_text(bot: &TgBot, job: &Job, text: &str) -> anyhow::Result<()> {
    // Suppress Telegram's own unfurl of the echoed IG link (PLAN §5).
    bot.send_message(job.chat_id, text)
        .link_preview_options(LinkPreviewOptions {
            is_disabled: true,
            url: None,
            prefer_small_media: false,
            prefer_large_media: false,
            show_above_text: false,
        })
        .reply_parameters(rp(job))
        .await?;
    Ok(())
}

pub async fn reply_failure(bot: &TgBot, job: &Job, text: &str) -> anyhow::Result<()> {
    reply_text(bot, job, text).await
}

/// A periodic background task that is **aborted when this guard is dropped**.
/// Telegram chat actions expire after ~5 s, so we re-send on an interval for as
/// long as the guard lives (i.e. for the duration of the job).
pub struct Keepalive(tokio::task::JoinHandle<()>);

impl Keepalive {
    /// Run `tick` immediately, then every `interval`, until the guard is dropped.
    pub fn start<F, Fut>(interval: Duration, mut tick: F) -> Self
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        Self(tokio::spawn(async move {
            loop {
                tick().await;
                tokio::time::sleep(interval).await;
            }
        }))
    }
}

impl Drop for Keepalive {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Show a live "typing…" status in the chat header while a job runs, and stop
/// when the returned guard drops (delivered, failed, or timed out). Telegram
/// expires a chat action after ~5 s, so it is re-sent every 4 s. Best-effort:
/// send errors are ignored — the indicator is cosmetic and never blocks delivery.
pub fn typing_indicator(bot: &TgBot, chat_id: ChatId) -> Keepalive {
    let bot = bot.clone();
    Keepalive::start(Duration::from_secs(4), move || {
        let bot = bot.clone();
        async move {
            let _ = bot.send_chat_action(chat_id, ChatAction::Typing).await;
        }
    })
}

/// Build the media captions, one per album chunk, each ≤1024 UTF-16 units.
/// The composed text (`<emoji> @author` + caption + `🔗 url`) flows across the
/// chunk captions in order — split on word boundaries with `…` continuation
/// markers ([`split_flow`]) — so a long caption continues on the next media
/// group instead of being dropped. The footer rides at the end of the text
/// (its own trailing caption if it doesn't fit). Only when the text outgrows
/// all `max_msgs` captions is the tail dropped — last caption ends with an
/// ellipsis + the 🔗 footer, and no follow-up message is ever sent (PLAN §5).
///
/// A short caption yields fewer entries than `max_msgs`; the caller leaves the
/// remaining chunks uncaptioned.
fn compose_captions(post: &Post, platform: Platform, max_msgs: usize) -> Vec<String> {
    let header = post
        .author
        .as_ref()
        .map(|a| format!("{} @{a}\n\n", platform.emoji()))
        .unwrap_or_default();
    let body = post.caption.as_deref().unwrap_or("").trim().to_string();
    let footer = format!("\n\n🔗 {}", post.original_url);

    if body.is_empty() {
        return vec![format!("{header}🔗 {}", post.original_url).trim().to_string()];
    }

    let mut pieces = split_flow(&format!("{header}{body}"), CAPTION_LIMIT);
    if let Some(last) = pieces.last_mut() {
        if utf16_len(last) + utf16_len(&footer) <= CAPTION_LIMIT {
            last.push_str(&footer);
        } else {
            pieces.push(footer.trim_start().to_string());
        }
    }

    let max_msgs = max_msgs.max(1);
    if pieces.len() > max_msgs {
        pieces.truncate(max_msgs);
        if let Some(last) = pieces.last_mut() {
            // The kept piece ends with a flow marker — strip it, re-cut on a
            // word boundary with room for the marker + footer, re-mark.
            let kept = last.strip_suffix(FLOW_MARK).unwrap_or(last).to_string();
            let budget =
                CAPTION_LIMIT.saturating_sub(utf16_len(FLOW_MARK) + utf16_len(&footer));
            let cut = word_boundary_cut(&kept, budget);
            *last = format!("{}{FLOW_MARK}{footer}", kept[..cut].trim_end());
        }
    }
    pieces
}

/// The full text for a media-less post: `<emoji> @author` + caption + `🔗 url`,
/// untruncated — the caller splits it into ≤4096-UTF-16 chunks.
fn compose_full_text(post: &Post, platform: Platform) -> String {
    let header = post
        .author
        .as_ref()
        .map(|a| format!("{} @{a}\n\n", platform.emoji()))
        .unwrap_or_default();
    let body = post.caption.as_deref().unwrap_or("").trim();
    let footer = format!("\n\n🔗 {}", post.original_url);
    format!("{header}{body}{footer}")
}

/// Length in UTF-16 code units (how Telegram counts).
fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// The continuation marker between flowed pieces (1 UTF-16 unit).
const FLOW_MARK: &str = "…";

/// Split text into pieces of at most `max` UTF-16 units that read as one
/// continuous message flow: cuts land on word boundaries (never mid-word —
/// except inside a single word longer than the budget, e.g. unbroken CJK
/// runs, which hard-cut), every piece that continues ends with `…`, and every
/// continuation piece starts with `…`. Short text yields one unmarked piece.
fn split_flow(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text.trim();
    loop {
        let lead = if out.is_empty() { "" } else { FLOW_MARK };
        if utf16_len(rest) + utf16_len(lead) <= max {
            out.push(format!("{lead}{rest}"));
            return out;
        }
        let budget = max.saturating_sub(utf16_len(lead) + utf16_len(FLOW_MARK));
        let mut cut = word_boundary_cut(rest, budget);
        if cut == 0 {
            // Pathological budget below one char — take a char anyway so the
            // loop always progresses (never emit an empty piece / spin).
            cut = rest.chars().next().map(char::len_utf8).unwrap_or(rest.len());
        }
        let (head, tail) = rest.split_at(cut);
        out.push(format!("{lead}{}{FLOW_MARK}", head.trim_end()));
        rest = tail.trim_start();
    }
}

/// Byte index where a ≤`budget`-UTF-16-unit prefix of `s` ends, pulled back to
/// the last whitespace so words stay whole. Keeps the hard cut when it already
/// sits on a word end, and refuses to backtrack past half the budget — a long
/// unbroken run (CJK text, a huge word) after a short wordy prefix must
/// hard-cut rather than shrink the piece to almost nothing.
fn word_boundary_cut(s: &str, budget: usize) -> usize {
    let mut hard = 0; // byte index of the pure-budget cut
    let mut soft = None; // (byte index, units) just past the last whitespace
    let mut n = 0;
    for (i, c) in s.char_indices() {
        let u = c.len_utf16();
        if n + u > budget {
            break;
        }
        n += u;
        hard = i + c.len_utf8();
        if c.is_whitespace() {
            soft = Some((hard, n));
        }
    }
    // A cut followed by whitespace already ends on a whole word.
    if s[hard..].chars().next().is_none_or(char::is_whitespace) {
        return hard;
    }
    match soft {
        Some((idx, units)) if units * 2 >= n => idx,
        _ => hard,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::Media;

    fn post_with(author: Option<&str>, caption: Option<&str>) -> Post {
        Post {
            author: author.map(str::to_string),
            caption: caption.map(str::to_string),
            media: vec![Media::image("https://scontent.cdninstagram.com/v/1.jpg")],
            original_url: "https://www.instagram.com/p/SC/".into(),
        }
    }

    #[test]
    fn short_caption_fits_inline() {
        let caps = compose_captions(
            &post_with(Some("nasa"), Some("hello world")),
            Platform::Instagram,
            1,
        );
        assert_eq!(caps.len(), 1);
        assert!(caps[0].contains("@nasa"));
        assert!(caps[0].contains("hello world"));
        assert!(caps[0].contains("🔗"));
    }

    #[test]
    fn long_caption_truncates_and_drops_tail_on_single_chunk() {
        let long = "x".repeat(2000);
        let caps = compose_captions(&post_with(Some("u"), Some(&long)), Platform::Instagram, 1);
        assert_eq!(caps.len(), 1);
        let cap = &caps[0];
        assert!(utf16_len(cap) <= CAPTION_LIMIT);
        assert!(cap.contains('…'));
        assert!(cap.contains("xxxx"), "some caption text retained on media");
        assert!(cap.contains("🔗"), "link footer survives truncation");
    }

    #[test]
    fn emoji_caption_respects_utf16_budget() {
        // 600 cameras = 1200 UTF-16 units (>1024) though only 600 chars.
        let body = "📷".repeat(600);
        let caps = compose_captions(&post_with(None, Some(&body)), Platform::Instagram, 1);
        assert_eq!(caps.len(), 1);
        assert!(utf16_len(&caps[0]) <= CAPTION_LIMIT);
        assert!(caps[0].contains('…'));
    }

    #[test]
    fn long_caption_continues_across_album_chunks() {
        // ~1400 units of wordy text + 2 album chunks → nothing dropped: the
        // text flows into the second chunk's caption on a word boundary, with
        // `…` continuation markers on both sides of the seam.
        let words: Vec<String> = (0..250).map(|i| format!("wd{i}")).collect();
        let body = words.join(" ");
        let caps = compose_captions(&post_with(None, Some(&body)), Platform::Instagram, 2);
        assert_eq!(caps.len(), 2);
        assert!(caps.iter().all(|c| utf16_len(c) <= CAPTION_LIMIT));
        assert!(caps[0].ends_with('…'), "first caption marks the continuation");
        assert!(caps[1].starts_with('…'), "second caption marks the continuation");
        assert!(caps[1].ends_with("🔗 https://www.instagram.com/p/SC/"), "footer last");
        assert!(!caps[0].contains("🔗"), "footer only once");
        // Every word survives whole — nothing cut in half at the seam.
        let last = caps[1]
            .strip_suffix("\n\n🔗 https://www.instagram.com/p/SC/")
            .expect("footer present");
        let got: Vec<&str> = [caps[0].as_str(), last]
            .iter()
            .flat_map(|p| p.trim_matches('…').split_whitespace())
            .collect();
        assert_eq!(got, words);
    }

    #[test]
    fn caption_tail_still_dropped_when_chunks_exhausted() {
        // Text outgrows even two chunk captions → last one ends … + footer.
        let long = "x".repeat(5000);
        let caps = compose_captions(&post_with(None, Some(&long)), Platform::Instagram, 2);
        assert_eq!(caps.len(), 2);
        assert!(caps.iter().all(|c| utf16_len(c) <= CAPTION_LIMIT));
        assert!(caps[1].contains('…'));
        assert!(caps[1].ends_with("🔗 https://www.instagram.com/p/SC/"));
        let xs: usize = caps.iter().map(|c| c.matches('x').count()).sum();
        assert!(xs < 5000, "tail dropped");
        assert!(xs > 1500, "both captions carry text");
    }

    #[test]
    fn short_caption_on_multi_chunk_album_stays_on_first() {
        let caps = compose_captions(&post_with(Some("u"), Some("tiny")), Platform::Instagram, 3);
        assert_eq!(caps.len(), 1, "later chunks go uncaptioned");
        assert!(caps[0].contains("tiny"));
    }

    #[test]
    fn footer_gets_own_chunk_caption_when_text_fills_the_last() {
        // An unbroken 2046-unit run packs two captions to the brim (1023 + a
        // marker, then a marker + 1023) → the footer rides alone on chunk 3.
        let long = "x".repeat(2046);
        let caps = compose_captions(&post_with(None, Some(&long)), Platform::Instagram, 3);
        assert_eq!(caps.len(), 3);
        assert!(caps.iter().all(|c| utf16_len(c) <= CAPTION_LIMIT));
        let xs: usize = caps.iter().map(|c| c.matches('x').count()).sum();
        assert_eq!(xs, 2046, "no text dropped");
        assert_eq!(caps[2], "🔗 https://www.instagram.com/p/SC/");
    }

    #[test]
    fn retry_policy_classifies_errors() {
        use teloxide::types::Seconds;
        // Flood-wait: definitely not posted — retry after what Telegram asked,
        // capped so it can't silently eat the job timeout.
        let flood = anyhow::Error::from(teloxide::RequestError::RetryAfter(Seconds::from_seconds(5)));
        assert_eq!(retry_delay(&flood), Some(Duration::from_secs(5)));
        let flood_long =
            anyhow::Error::from(teloxide::RequestError::RetryAfter(Seconds::from_seconds(120)));
        assert_eq!(retry_delay(&flood_long), Some(Duration::from_secs(30)), "capped");
        // API rejection: deterministic — no retry, and it can't have landed.
        let api = anyhow::Error::from(teloxide::RequestError::Api(teloxide::ApiError::BotBlocked));
        assert_eq!(retry_delay(&api), None);
        assert!(!may_have_landed(&api));
        // Non-Telegram errors: no retry.
        let other = anyhow::anyhow!("not a telegram error");
        assert_eq!(retry_delay(&other), None);
        assert!(!may_have_landed(&other));
    }

    #[test]
    fn textonly_compose_includes_author_caption_and_link() {
        let post = Post {
            author: Some("zuck".into()),
            caption: Some("just thinking out loud".into()),
            media: vec![],
            original_url: "https://www.threads.com/@zuck/post/ABC".into(),
        };
        let text = compose_full_text(&post, Platform::Threads);
        assert!(text.contains("🧵 @zuck"), "text={text}");
        assert!(text.contains("just thinking out loud"));
        assert!(text.contains("🔗 https://www.threads.com/@zuck/post/ABC"));
    }

    #[test]
    fn utf16_helpers_count_surrogates() {
        assert_eq!(utf16_len("ab"), 2);
        assert_eq!(utf16_len("📷"), 2);
        let s = "📷📷📷";
        assert_eq!(&s[..word_boundary_cut(s, 4)], "📷📷", "cut in UTF-16 units, not chars");
    }

    #[test]
    fn split_flow_short_text_is_untouched() {
        assert_eq!(split_flow("hello world", 100), vec!["hello world"]);
    }

    #[test]
    fn split_flow_breaks_on_word_boundaries_with_markers() {
        let words: Vec<String> = (0..200).map(|i| format!("word{i}")).collect();
        let text = words.join(" ");
        let pieces = split_flow(&text, 100);
        assert!(pieces.len() > 1);
        assert!(pieces.iter().all(|p| utf16_len(p) <= 100));
        for (i, p) in pieces.iter().enumerate() {
            if i > 0 {
                assert!(p.starts_with('…'), "continuation lead missing: {p}");
            }
            if i + 1 < pieces.len() {
                assert!(p.ends_with('…'), "continuation tail missing: {p}");
            }
        }
        // Strip the markers and rejoin: every word must survive whole.
        let got: Vec<&str> = pieces
            .iter()
            .flat_map(|p| p.trim_matches('…').split_whitespace())
            .collect();
        assert_eq!(got, words);
    }

    #[test]
    fn split_flow_hard_cuts_unbroken_runs() {
        // CJK-style text has no whitespace to break at — hard-cut on the
        // budget, but nothing may be lost.
        let text = "字".repeat(9000);
        let pieces = split_flow(&text, TEXT_LIMIT);
        assert!(pieces.len() > 2);
        assert!(pieces.iter().all(|p| utf16_len(p) <= TEXT_LIMIT));
        let total: usize = pieces.iter().map(|p| p.matches('字').count()).sum();
        assert_eq!(total, 9000);
    }

    #[tokio::test]
    async fn keepalive_ticks_while_alive_then_stops_on_drop() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let guard = Keepalive::start(Duration::from_millis(5), move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });
        // Ticks while the guard is alive.
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(count.load(Ordering::SeqCst) >= 1, "should tick while alive");
        // Dropping the guard aborts the task — no further ticks.
        drop(guard);
        tokio::time::sleep(Duration::from_millis(20)).await; // let any in-flight tick settle
        let after = count.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(count.load(Ordering::SeqCst), after, "no ticks after drop");
    }
}
