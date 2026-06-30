//! Build the reply: caption ladder (≤1024 UTF-16 units + overflow follow-up),
//! single media vs chunked albums (`send_media_group`, 2–10 items), and the
//! delivery ladder — `InputFile::url` first, then download-then-upload, then a
//! link+note fallback (PLAN §5 / §6).
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

    let (caption, overflow) = compose_caption(&post, job.platform);
    if n == 1 {
        send_one(bot, http, cfg, job, &post.media[0], &caption).await?;
    } else {
        send_album(bot, http, cfg, job, &post.media, &caption).await?;
    }

    if let Some(full) = overflow {
        for chunk in split_utf16(&full, TEXT_LIMIT) {
            reply_text(bot, job, &chunk).await?;
        }
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
    for chunk in split_utf16(&full, TEXT_LIMIT) {
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
    if let Ok(u) = Url::parse(&m.url) {
        let res = match m.kind {
            MediaKind::Image => bot
                .send_photo(job.chat_id, InputFile::url(u))
                .caption(caption)
                .reply_parameters(rp(job))
                .await
                .map(|_| ()),
            MediaKind::Video => bot
                .send_video(job.chat_id, InputFile::url(u))
                .caption(caption)
                .reply_parameters(rp(job))
                .await
                .map(|_| ()),
        };
        if res.is_ok() {
            return Ok(());
        }
        tracing::warn!(error = ?res.err(), "url-send failed; falling back to download");
    }

    // 2) Download then upload (≤ max_upload_bytes).
    let tmp = tempfile::Builder::new()
        .prefix("igbot")
        .tempdir_in(&cfg.temp_dir)?;
    let dest = tmp.path().join(temp_filename(&job.shortcode, 0, m.kind));
    match download_capped(http, &m.url, &dest, cfg.max_upload_bytes, &cfg.user_agent).await {
        Ok(()) => upload_local(bot, job, m.kind, &dest, caption).await,
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

/// Send a carousel as successive media groups of up to 10 items each. The
/// caption goes on the first item of the first chunk only.
async fn send_album(
    bot: &TgBot,
    http: &reqwest::Client,
    cfg: &Config,
    job: &Job,
    media: &[Media],
    caption: &str,
) -> anyhow::Result<()> {
    let mut first = true;
    for chunk in media.chunks(MAX_ALBUM) {
        let cap = if first { caption } else { "" };
        send_chunk(bot, http, cfg, job, chunk, cap).await?;
        first = false;
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
    if let Err(e) = try_album_urls(bot, job, items, caption).await {
        tracing::warn!(error = %e, "album url-send failed; downloading");
    } else {
        return Ok(());
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
        1 => upload_local(bot, job, locals[0].0, &locals[0].1, caption).await,
        _ => try_album_files(bot, job, &locals, caption).await,
    }
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

/// Build the media caption (≤1024 UTF-16 units, keeping a truncated preview of
/// the caption text) and, if it overflows, the full caption to send as a
/// follow-up message (PLAN §5).
fn compose_caption(post: &Post, platform: Platform) -> (String, Option<String>) {
    let header = post
        .author
        .as_ref()
        .map(|a| format!("{} @{a}\n\n", platform.emoji()))
        .unwrap_or_default();
    let body = post.caption.as_deref().unwrap_or("").trim().to_string();
    let footer = format!("\n\n🔗 {}", post.original_url);

    let everything = format!("{header}{body}{footer}");
    if utf16_len(&everything) <= CAPTION_LIMIT {
        let combined = if body.is_empty() {
            format!("{header}🔗 {}", post.original_url).trim().to_string()
        } else {
            everything
        };
        return (combined, None);
    }

    // Keep a truncated caption on the media; send the full caption as follow-up.
    let ellipsis = "…";
    let reserved = utf16_len(&header) + utf16_len(&footer) + utf16_len(ellipsis);
    let budget = CAPTION_LIMIT.saturating_sub(reserved);
    let truncated = truncate_utf16(&body, budget);
    let caption = format!("{header}{truncated}{ellipsis}{footer}");
    (caption, Some(body))
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

/// Truncate to at most `max` UTF-16 code units, on a char boundary.
fn truncate_utf16(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut n = 0;
    for c in s.chars() {
        let u = c.len_utf16();
        if n + u > max {
            break;
        }
        out.push(c);
        n += u;
    }
    out
}

/// Split into chunks each ≤ `max` UTF-16 code units, on char boundaries.
fn split_utf16(s: &str, max: usize) -> Vec<String> {
    if utf16_len(s) <= max {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut n = 0;
    for c in s.chars() {
        let u = c.len_utf16();
        if n + u > max {
            out.push(std::mem::take(&mut cur));
            n = 0;
        }
        cur.push(c);
        n += u;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
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
        let (cap, overflow) =
            compose_caption(&post_with(Some("nasa"), Some("hello world")), Platform::Instagram);
        assert!(cap.contains("@nasa"));
        assert!(cap.contains("hello world"));
        assert!(cap.contains("🔗"));
        assert!(overflow.is_none());
    }

    #[test]
    fn long_caption_truncates_on_media_and_overflows_full() {
        let long = "x".repeat(2000);
        let (cap, overflow) = compose_caption(&post_with(Some("u"), Some(&long)), Platform::Instagram);
        assert!(utf16_len(&cap) <= CAPTION_LIMIT);
        assert!(cap.contains('…'));
        assert!(cap.contains("xxxx"), "some caption text retained on media");
        assert_eq!(overflow.as_deref(), Some(long.as_str()));
    }

    #[test]
    fn emoji_caption_respects_utf16_budget() {
        // 600 cameras = 1200 UTF-16 units (>1024) though only 600 chars.
        let body = "📷".repeat(600);
        let (cap, overflow) = compose_caption(&post_with(None, Some(&body)), Platform::Instagram);
        assert!(utf16_len(&cap) <= CAPTION_LIMIT);
        assert!(overflow.is_some());
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
        assert_eq!(truncate_utf16("📷📷📷", 4), "📷📷");
    }

    #[test]
    fn split_utf16_chunks_within_limit() {
        let parts = split_utf16(&"a".repeat(9000), TEXT_LIMIT);
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| utf16_len(p) <= TEXT_LIMIT));
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
