//! Extraction backends and the pluggable fallback chain (PLAN §3.3 / §3.4).

use crate::config::Config;
use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;
use thiserror::Error;
use url::Url;

pub mod embed;
pub mod external;
pub mod gallery_dl;
pub mod threads_embed;
pub mod threads_json;
pub mod yt_dlp;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
}

/// One media item in a post (carousels carry many).
#[derive(Debug, Clone)]
pub struct Media {
    pub kind: MediaKind,
    /// Direct CDN URL. Captured verbatim — never trim/reorder query params, and
    /// HTML-decode before use (PLAN §4.1, §6).
    pub url: String,
    /// Reserved: set when a backend pre-downloads to disk (PLAN §3.3 / §6).
    /// The delivery ladder currently downloads on demand into its own temp dir.
    #[allow(dead_code)]
    pub local_path: Option<PathBuf>,
}

impl Media {
    pub fn image(url: impl Into<String>) -> Self {
        Self { kind: MediaKind::Image, url: url.into(), local_path: None }
    }
    pub fn video(url: impl Into<String>) -> Self {
        Self { kind: MediaKind::Video, url: url.into(), local_path: None }
    }
}

/// Normalized result of any extractor backend.
#[derive(Debug, Clone)]
pub struct Post {
    pub author: Option<String>,
    pub caption: Option<String>,
    pub media: Vec<Media>,
    pub original_url: String,
}

#[derive(Debug, Error)]
pub enum ExtractError {
    #[error("post not found / removed")]
    NotFound,
    #[error("login or rate-limit wall")]
    Blocked,
    #[error("backend unavailable: {0}")]
    Unavailable(String),
    #[error("parse/transient error: {0}")]
    Transient(String),
}

/// A pluggable extraction backend. Implementations turn a post URL into a
/// [`Post`]. Named generically — it serves Instagram and Threads alike (the
/// `url` argument is the canonical post URL; backends fetch it directly).
#[async_trait]
pub trait Extractor: Send + Sync {
    /// Short name for logging ("embed", "yt-dlp", "threads-json", ...).
    fn name(&self) -> &'static str;
    async fn extract(&self, url: &str, shortcode: &str) -> Result<Post, ExtractError>;
}

/// Tries each backend in order; first one returning a usable [`Post`] wins — one
/// with media, or (when [`accept_textonly`](Self::new_accepting_textonly)) a
/// caption-only post.
pub struct ExtractorChain {
    backends: Vec<Box<dyn Extractor>>,
    /// When set, a [`Post`] with no media but a non-blank caption counts as
    /// success (Threads is text-first). Instagram keeps media required.
    accept_textonly: bool,
}

impl ExtractorChain {
    pub fn new(backends: Vec<Box<dyn Extractor>>) -> Self {
        Self { backends, accept_textonly: false }
    }

    /// Like [`new`](Self::new) but a caption-only [`Post`] (no media) counts as
    /// success — for text-first platforms like Threads.
    pub fn new_accepting_textonly(backends: Vec<Box<dyn Extractor>>) -> Self {
        Self { backends, accept_textonly: true }
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.backends.iter().map(|b| b.name()).collect()
    }

    pub async fn extract(&self, url: &str, shortcode: &str) -> Result<Post, ExtractError> {
        // `shortcode` rides on the surrounding job span, so it's omitted here.
        let mut last = ExtractError::Unavailable("no backends configured".into());
        for b in &self.backends {
            let started = std::time::Instant::now();
            let result = b.extract(url, shortcode).await;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            match result {
                // Success requires media — "text but no media" is a failure for
                // the media role (PLAN §4.8) …
                Ok(p) if !p.media.is_empty() => {
                    tracing::info!(backend = b.name(), media = p.media.len(), elapsed_ms, "extracted");
                    return Ok(p);
                }
                // … unless this is a text-first chain (Threads): a caption-only
                // post is a valid result, delivered as text (Threads design).
                Ok(p) if self.accept_textonly && has_text(&p) => {
                    tracing::info!(backend = b.name(), media = 0, elapsed_ms, "extracted text-only post");
                    return Ok(p);
                }
                Ok(_) => {
                    tracing::warn!(backend = b.name(), elapsed_ms, "no media, trying next backend");
                    keep_worst(&mut last, ExtractError::NotFound);
                }
                Err(e) => {
                    tracing::warn!(backend = b.name(), elapsed_ms, error = %e, "backend failed, trying next");
                    keep_worst(&mut last, e);
                }
            }
        }
        Err(last)
    }
}

// ----------------------------------------------------------------------------
// Shared helpers used by multiple backends.
// ----------------------------------------------------------------------------

/// True only when the URL's *host* is a Meta media CDN (`*.cdninstagram.com`,
/// `*.fbcdn.net`) — these serve both Instagram and Threads media. Parses the
/// host rather than substring-matching, so a crafted URL like
/// `https://evil.test/?ref=cdninstagram.com` is rejected (no SSRF).
pub(crate) fn is_meta_cdn(url: &str) -> bool {
    match Url::parse(url) {
        Ok(u) => {
            matches!(u.scheme(), "http" | "https")
                && u.host_str().is_some_and(|h| {
                    let h = h.to_ascii_lowercase();
                    h == "cdninstagram.com"
                        || h.ends_with(".cdninstagram.com")
                        || h == "fbcdn.net"
                        || h.ends_with(".fbcdn.net")
                })
        }
        Err(_) => false,
    }
}

/// Guess media kind from the URL path (used when the source doesn't say). Only
/// real video extensions count as video; everything else is treated as an image
/// (so an image URL that merely contains a token like `/o1/` isn't mis-sent as
/// a video, which Telegram would reject).
pub(crate) fn media_by_ext(url: &str) -> Media {
    let path = url.split('?').next().unwrap_or(url).to_ascii_lowercase();
    let is_video = path.ends_with(".mp4")
        || path.ends_with(".mov")
        || path.ends_with(".m4v")
        || path.ends_with(".webm");
    if is_video {
        Media::video(url)
    } else {
        Media::image(url)
    }
}

/// De-duplicate media by URL, preserving order.
pub(crate) fn dedup_media(media: Vec<Media>) -> Vec<Media> {
    let mut seen = std::collections::HashSet::new();
    media.into_iter().filter(|m| seen.insert(m.url.clone())).collect()
}

/// Collect media from a Meta/Instagram-family post node — the Polaris /
/// Barcelona `application/json` shape shared by Instagram posts and Threads
/// `thread_items` — expanding a carousel into its children. CDN URLs are
/// normalized (entity/unicode-decoded, query string left verbatim) and
/// host-gated. Shared by the IG embed scraper and the Threads scraper.
pub(crate) fn collect_meta_media(node: &Value) -> Vec<Media> {
    let mut out = Vec::new();
    if let Some(children) = node.pointer("/carousel_media").and_then(Value::as_array) {
        for child in children {
            push_meta_single(child, &mut out);
        }
    } else {
        push_meta_single(node, &mut out);
    }
    out
}

/// Push one image or video from a (child) node. Prefers the playable video — a
/// video node also carries an `image_versions2` poster we must not send as a
/// still (Telegram would reject a still sent via `send_video`).
fn push_meta_single(node: &Value, out: &mut Vec<Media>) {
    if let Some(u) = node
        .pointer("/video_versions/0/url")
        .and_then(Value::as_str)
        .map(normalize_cdn_url)
        .filter(|u| is_meta_cdn(u))
    {
        out.push(Media::video(u));
        return;
    }
    if let Some(u) = node
        .pointer("/image_versions2/candidates/0/url")
        .and_then(Value::as_str)
        .map(normalize_cdn_url)
        .filter(|u| is_meta_cdn(u))
    {
        out.push(Media::image(u));
    }
}

/// Decode the encodings IG applies to media URLs, so the signature stays intact
/// (PLAN §4.1): HTML entities (`&amp;`), JSON unicode escapes (`&`), and
/// escaped slashes (`\/`). Idempotent for already-clean URLs.
pub(crate) fn normalize_cdn_url(raw: &str) -> String {
    html_escape::decode_html_entities(raw)
        .replace("\\u0026", "&")
        .replace("\\u002F", "/")
        .replace("\\/", "/")
        .trim()
        .to_string()
}

/// Truncate to at most `max` *chars* (never bytes — a mid-UTF-8 byte slice
/// would panic, and release builds use `panic = "abort"`). For debug logs of
/// subprocess stderr / response bodies.
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Map an HTTP status code to an [`ExtractError`] class (PLAN §4.8).
pub(crate) fn map_status(code: u16) -> ExtractError {
    match code {
        404 | 410 => ExtractError::NotFound,
        401 | 403 | 451 => ExtractError::Blocked,
        429 | 500 | 502 | 503 | 504 => ExtractError::Transient(format!("http {code}")),
        c => ExtractError::Unavailable(format!("http {c}")),
    }
}

/// How actionable an error is, so the chain surfaces the most useful cause
/// instead of letting a later empty-media `NotFound` mask an earlier `Blocked`.
fn severity(e: &ExtractError) -> u8 {
    match e {
        ExtractError::Blocked => 3,
        ExtractError::Transient(_) => 2,
        ExtractError::NotFound => 1,
        ExtractError::Unavailable(_) => 0,
    }
}

/// Replace `slot` with `candidate` only if it is strictly more severe, so the
/// first most-actionable error seen across backends wins.
fn keep_worst(slot: &mut ExtractError, candidate: ExtractError) {
    if severity(&candidate) > severity(slot) {
        *slot = candidate;
    }
}

/// A post carries deliverable text when its caption is present and non-blank.
fn has_text(p: &Post) -> bool {
    p.caption.as_deref().is_some_and(|c| !c.trim().is_empty())
}

// ----------------------------------------------------------------------------
// Chain construction from config.
// ----------------------------------------------------------------------------

/// Instagram chain (PLAN §3.4): embed → yt-dlp (+ gallery-dl if cookies)
/// (+ external fallback if configured). Media required.
pub fn build_ig_chain(cfg: &Config, http: reqwest::Client) -> ExtractorChain {
    let mut backends: Vec<Box<dyn Extractor>> = vec![
        Box::new(embed::EmbedScraper::new(http.clone(), cfg.embed_user_agent.clone())),
        Box::new(yt_dlp::YtDlpExtractor::new(cfg.yt_dlp_path.clone(), cfg.ig_cookies_path.clone())),
    ];

    if let Some(cookies) = &cfg.ig_cookies_path {
        backends.push(Box::new(gallery_dl::GalleryDlExtractor::new(
            cfg.gallery_dl_path.clone(),
            cookies.clone(),
        )));
    }

    if let Some(provider) = &cfg.fallback_provider {
        match external::ExternalFallback::from_config(provider, cfg, http) {
            Some(ext) => backends.push(Box::new(ext)),
            None => tracing::warn!(provider, "unknown FALLBACK_PROVIDER, ignoring"),
        }
    }

    let chain = ExtractorChain::new(backends);
    tracing::info!(backends = ?chain.names(), "instagram extractor chain built");
    chain
}

/// Threads chain: in-process inline-JSON scrape → `/embed` HTML fallback. No
/// yt-dlp/gallery-dl (neither supports Threads upstream). Accepts text-only
/// posts (Threads is text-first).
pub fn build_threads_chain(cfg: &Config, http: reqwest::Client) -> ExtractorChain {
    let backends: Vec<Box<dyn Extractor>> = vec![
        Box::new(threads_json::ThreadsScraper::new(
            http.clone(),
            cfg.threads_user_agent.clone(),
            cfg.threads_sec_ch_ua.clone(),
        )),
        Box::new(threads_embed::ThreadsEmbedScraper::new(
            http,
            cfg.threads_user_agent.clone(),
            cfg.threads_sec_ch_ua.clone(),
        )),
    ];
    let chain = ExtractorChain::new_accepting_textonly(backends);
    tracing::info!(backends = ?chain.names(), "threads extractor chain built");
    chain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_meta_cdn_requires_real_host() {
        assert!(is_meta_cdn("https://scontent.cdninstagram.com/v/a.jpg?x=1"));
        assert!(is_meta_cdn("https://instagram.fabc1-2.fna.fbcdn.net/v/b.mp4"));
        assert!(!is_meta_cdn("https://attacker.example/p?ref=cdninstagram.com"));
        assert!(!is_meta_cdn("https://cdninstagram.com.evil.test/x"));
        assert!(!is_meta_cdn("ftp://scontent.cdninstagram.com/x"));
        assert!(!is_meta_cdn("not a url"));
    }

    #[test]
    fn media_by_ext_only_real_video_exts() {
        assert_eq!(media_by_ext("https://x.cdninstagram.com/v/a.mp4?q=1").kind, MediaKind::Video);
        assert_eq!(media_by_ext("https://x.cdninstagram.com/o1/v/a.jpg").kind, MediaKind::Image);
        assert_eq!(media_by_ext("https://x.cdninstagram.com/v/a.webp").kind, MediaKind::Image);
    }

    /// Mock backend: `Ok(n)` yields a Post with `n` media items; `Err(e)` fails.
    struct Mock(Result<usize, ExtractError>);
    #[async_trait]
    impl Extractor for Mock {
        fn name(&self) -> &'static str {
            "mock"
        }
        async fn extract(&self, _u: &str, _s: &str) -> Result<Post, ExtractError> {
            match &self.0 {
                Ok(n) => Ok(Post {
                    author: None,
                    caption: None,
                    media: (0..*n)
                        .map(|_| Media::image("https://scontent.cdninstagram.com/v/a.jpg"))
                        .collect(),
                    original_url: String::new(),
                }),
                Err(e) => Err(clone_err(e)),
            }
        }
    }
    fn clone_err(e: &ExtractError) -> ExtractError {
        match e {
            ExtractError::NotFound => ExtractError::NotFound,
            ExtractError::Blocked => ExtractError::Blocked,
            ExtractError::Unavailable(s) => ExtractError::Unavailable(s.clone()),
            ExtractError::Transient(s) => ExtractError::Transient(s.clone()),
        }
    }

    #[tokio::test]
    async fn chain_returns_first_with_media() {
        let c = ExtractorChain::new(vec![Box::new(Mock(Ok(0))), Box::new(Mock(Ok(2)))]);
        assert_eq!(c.extract("u", "s").await.unwrap().media.len(), 2);
    }

    #[tokio::test]
    async fn chain_keeps_most_severe_error() {
        // Blocked then empty-media must surface Blocked, not NotFound.
        let c = ExtractorChain::new(vec![Box::new(Mock(Err(ExtractError::Blocked))), Box::new(Mock(Ok(0)))]);
        assert!(matches!(c.extract("u", "s").await, Err(ExtractError::Blocked)));
        // empty-media then Blocked also surfaces Blocked.
        let c = ExtractorChain::new(vec![Box::new(Mock(Ok(0))), Box::new(Mock(Err(ExtractError::Blocked)))]);
        assert!(matches!(c.extract("u", "s").await, Err(ExtractError::Blocked)));
        // only empty-media → NotFound.
        let c = ExtractorChain::new(vec![Box::new(Mock(Ok(0)))]);
        assert!(matches!(c.extract("u", "s").await, Err(ExtractError::NotFound)));
    }

    /// Backend that returns a caption-only Post (no media) — a Threads text post.
    struct TextOnly(Option<&'static str>);
    #[async_trait]
    impl Extractor for TextOnly {
        fn name(&self) -> &'static str {
            "text"
        }
        async fn extract(&self, _u: &str, _s: &str) -> Result<Post, ExtractError> {
            Ok(Post {
                author: Some("u".into()),
                caption: self.0.map(str::to_string),
                media: vec![],
                original_url: String::new(),
            })
        }
    }

    #[tokio::test]
    async fn textonly_chain_accepts_caption_only_post() {
        // A media-required (Instagram) chain rejects a media-less post …
        let c = ExtractorChain::new(vec![Box::new(TextOnly(Some("just text")))]);
        assert!(matches!(c.extract("u", "s").await, Err(ExtractError::NotFound)));
        // … but a text-accepting (Threads) chain delivers it as text.
        let c = ExtractorChain::new_accepting_textonly(vec![Box::new(TextOnly(Some("just text")))]);
        let post = c.extract("u", "s").await.unwrap();
        assert!(post.media.is_empty());
        assert_eq!(post.caption.as_deref(), Some("just text"));
    }

    #[tokio::test]
    async fn textonly_chain_still_rejects_blank_post() {
        // No media AND a blank/absent caption is still a failure, even for a
        // text-accepting chain (don't reply with an empty message).
        let c = ExtractorChain::new_accepting_textonly(vec![Box::new(TextOnly(Some("   ")))]);
        assert!(matches!(c.extract("u", "s").await, Err(ExtractError::NotFound)));
        let c = ExtractorChain::new_accepting_textonly(vec![Box::new(TextOnly(None))]);
        assert!(matches!(c.extract("u", "s").await, Err(ExtractError::NotFound)));
    }
}
