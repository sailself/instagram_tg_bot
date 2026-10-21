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
        Self {
            kind: MediaKind::Image,
            url: url.into(),
            local_path: None,
        }
    }
    pub fn video(url: impl Into<String>) -> Self {
        Self {
            kind: MediaKind::Video,
            url: url.into(),
            local_path: None,
        }
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

/// Minimum attachment counts observed in an earlier extraction attempt.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MediaRequirements {
    pub total: usize,
    pub images: usize,
    pub videos: usize,
}

impl MediaRequirements {
    pub(crate) fn merge(&mut self, other: Self) {
        self.total = self.total.max(other.total);
        self.images = self.images.max(other.images);
        self.videos = self.videos.max(other.videos);
        self.total = self.total.max(self.images.saturating_add(self.videos));
    }

    pub(crate) fn is_satisfied_by(&self, media: &[Media]) -> bool {
        media.len() >= self.total
            && media.iter().filter(|m| m.kind == MediaKind::Image).count() >= self.images
            && media.iter().filter(|m| m.kind == MediaKind::Video).count() >= self.videos
    }
}

/// Strict collection keeps evidence of omitted attachments beside usable media.
pub(crate) struct MetaMediaCollection {
    pub media: Vec<Media>,
    pub required: MediaRequirements,
    pub incomplete: bool,
}

#[derive(Debug, Error)]
pub enum ExtractError {
    #[error("post not found / removed")]
    NotFound,
    #[error("login or rate-limit wall")]
    Blocked,
    #[error("login required or audience-restricted content")]
    Restricted,
    #[error("HTTP 429 rate limit")]
    RateLimited,
    #[error("known attachments could not be extracted: {0:?}")]
    IncompleteMedia(MediaRequirements),
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
        Self {
            backends,
            accept_textonly: false,
        }
    }

    /// Like [`new`](Self::new) but a caption-only [`Post`] (no media) counts as
    /// success — for text-first platforms like Threads.
    pub fn new_accepting_textonly(backends: Vec<Box<dyn Extractor>>) -> Self {
        Self {
            backends,
            accept_textonly: true,
        }
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.backends.iter().map(|b| b.name()).collect()
    }

    pub async fn extract(&self, url: &str, shortcode: &str) -> Result<Post, ExtractError> {
        // `shortcode` rides on the surrounding job span, so it's omitted here.
        let mut last = ExtractError::Unavailable("no backends configured".into());
        let mut required = MediaRequirements::default();
        for b in &self.backends {
            let started = std::time::Instant::now();
            let result = b.extract(url, shortcode).await;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let result = match result {
                Err(ExtractError::IncompleteMedia(missing)) => {
                    required.merge(missing);
                    Err(ExtractError::IncompleteMedia(required))
                }
                Ok(post) if !required.is_satisfied_by(&post.media) => {
                    Err(ExtractError::IncompleteMedia(required))
                }
                other => other,
            };
            match result {
                // Success requires media — "text but no media" is a failure for
                // the media role (PLAN §4.8) …
                Ok(p) if !p.media.is_empty() => {
                    tracing::info!(
                        backend = b.name(),
                        media = p.media.len(),
                        elapsed_ms,
                        "extracted"
                    );
                    return Ok(p);
                }
                // … unless this is a text-first chain (Threads): a caption-only
                // post is a valid result, delivered as text (Threads design).
                Ok(p) if self.accept_textonly && has_text(&p) => {
                    tracing::info!(
                        backend = b.name(),
                        media = 0,
                        elapsed_ms,
                        "extracted text-only post"
                    );
                    return Ok(p);
                }
                Ok(_) => {
                    tracing::warn!(
                        backend = b.name(),
                        elapsed_ms,
                        "no media, trying next backend"
                    );
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
    media
        .into_iter()
        .filter(|m| seen.insert(m.url.clone()))
        .collect()
}

/// Collect media from a Meta/Instagram-family post node — the Polaris /
/// Barcelona `application/json` shape shared by Instagram posts and Threads
/// `thread_items` — expanding a carousel into its children. CDN URLs are
/// normalized (entity/unicode-decoded, query string left verbatim) and
/// host-gated. Shared by the IG embed scraper and the Threads scraper.
pub(crate) fn collect_meta_media(node: &Value) -> Vec<Media> {
    collect_meta_media_strict(node).media
}

pub(crate) fn collect_meta_media_strict(node: &Value) -> MetaMediaCollection {
    let mut media = Vec::new();
    let mut required = MediaRequirements::default();
    collect_meta_node(node, &mut media, &mut required);
    let incomplete = !required.is_satisfied_by(&media);
    MetaMediaCollection {
        media,
        required,
        incomplete,
    }
}

fn collect_meta_node(node: &Value, out: &mut Vec<Media>, required: &mut MediaRequirements) {
    let kind = node.get("media_type").and_then(Value::as_u64);
    let declared = node
        .get("carousel_media_count")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0);
    if let Some(children) = node.get("carousel_media").and_then(Value::as_array) {
        let before = required.total;
        for child in children {
            collect_meta_node(child, out, required);
        }
        // Each carousel slot represents an attachment, even if its payload is empty.
        required.total = required
            .total
            .max(before.saturating_add(children.len().max(declared)));
        if children.is_empty() && kind == Some(8) {
            required.total = required.total.max(before.saturating_add(1));
        }
        return;
    }
    if kind == Some(8) || declared > 0 {
        // A carousel thumbnail is not a substitute for absent child records.
        required.total = required.total.saturating_add(declared.max(1));
        return;
    }
    let videos = node.get("video_versions").and_then(Value::as_array);
    let is_video = kind == Some(2) || videos.is_some_and(|v| !v.is_empty());
    if is_video {
        required.total += 1;
        required.videos += 1;
        if let Some(url) = first_meta_rendition(videos) {
            out.push(Media::video(url));
        }
        // A video poster never establishes recovery of a missing video.
        return;
    }
    let images = node
        .pointer("/image_versions2/candidates")
        .and_then(Value::as_array);
    if kind == Some(1) || images.is_some_and(|v| !v.is_empty()) {
        required.total += 1;
        required.images += 1;
        if let Some(url) = first_meta_rendition(images) {
            out.push(Media::image(url));
        }
    } else if !node.is_object() || node.as_object().is_some_and(|n| n.is_empty()) {
        required.total += 1;
    }
}

fn first_meta_rendition(candidates: Option<&Vec<Value>>) -> Option<String> {
    candidates?
        .iter()
        .filter_map(|v| v.get("url").and_then(Value::as_str))
        .map(normalize_cdn_url)
        .find(|u| is_meta_cdn(u))
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

/// Threads distinguishes actual HTTP throttling from access denial and parsing.
pub(crate) fn threads_status(code: u16) -> ExtractError {
    match code {
        429 => ExtractError::RateLimited,
        _ => map_status(code),
    }
}

/// Run `<path> --version` to check a subprocess backend is actually installed.
/// Startup-only operator aid: `Ok(first line of output)` or `Err(reason)`.
pub fn probe_binary(path: &str) -> Result<String, String> {
    match std::process::Command::new(path).arg("--version").output() {
        Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string()),
        Ok(out) => Err(format!("`{path} --version` exited with {}", out.status)),
        Err(e) => Err(format!("cannot run `{path}`: {e}")),
    }
}

/// Startup check for the subprocess backends the configured chains will call.
/// A missing binary otherwise surfaces only per job, as a WARN buried in the
/// chain — say it once, loudly, before the first link arrives. yt-dlp is always
/// in the Instagram post chain; gallery-dl matters once cookies are set, since
/// it is then the *only* backend able to fetch image stories (yt-dlp's story
/// extractor is video-only).
pub fn report_backend_binaries(cfg: &Config) {
    match probe_binary(&cfg.yt_dlp_path) {
        Ok(v) => tracing::info!(binary = %cfg.yt_dlp_path, version = %v, "yt-dlp found"),
        Err(e) => tracing::warn!(
            binary = %cfg.yt_dlp_path,
            error = %e,
            "yt-dlp not runnable — Instagram video fallback and video stories will fail; install it or set YT_DLP_PATH"
        ),
    }
    if cfg.ig_cookies_path.is_some() {
        match probe_binary(&cfg.gallery_dl_path) {
            Ok(v) => {
                tracing::info!(binary = %cfg.gallery_dl_path, version = %v, "gallery-dl found")
            }
            Err(e) => tracing::warn!(
                binary = %cfg.gallery_dl_path,
                error = %e,
                "IG_COOKIES_PATH is set but gallery-dl is not runnable — image stories (and login-walled image posts) will fail until it is installed; set GALLERY_DL_PATH if it lives elsewhere"
            ),
        }
    }
}

/// How actionable an error is, so the chain surfaces the most useful cause
/// instead of letting a later empty-media `NotFound` mask an earlier `Blocked`.
fn severity(e: &ExtractError) -> u8 {
    match e {
        ExtractError::Restricted => 6,
        ExtractError::RateLimited => 5,
        ExtractError::Blocked => 4,
        ExtractError::IncompleteMedia(_) => 3,
        ExtractError::Transient(_) => 2,
        ExtractError::NotFound => 1,
        ExtractError::Unavailable(_) => 0,
    }
}

/// Replace `slot` with `candidate` only if it is strictly more severe, so the
/// first most-actionable error seen across backends wins.
fn keep_worst(slot: &mut ExtractError, candidate: ExtractError) {
    if let (ExtractError::IncompleteMedia(current), ExtractError::IncompleteMedia(next)) =
        (&mut *slot, &candidate)
    {
        current.merge(*next);
        return;
    }
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
        Box::new(embed::EmbedScraper::new(
            http.clone(),
            cfg.embed_user_agent.clone(),
        )),
        Box::new(yt_dlp::YtDlpExtractor::new(
            cfg.yt_dlp_path.clone(),
            cfg.ig_cookies_path.clone(),
        )),
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

/// Instagram **story** chain: gallery-dl → yt-dlp (`--no-playlist`), media
/// required. Stories are login-walled for anonymous clients, so this chain
/// exists **only when `IG_COOKIES_PATH` is set** — `None` otherwise, and the
/// worker answers story links with a "needs a login session" notice instead of
/// spawning a doomed extraction. Order: gallery-dl handles image *and* video
/// stories; yt-dlp's story extractor yields only video items.
///
/// Deliberately excluded: the embed scraper (a logged-out story page carries a
/// single `og:image` — the **profile picture** — which the OG fallback would
/// ship as if it were the story) and the external fallback (unverified for
/// story URLs).
pub fn build_ig_story_chain(cfg: &Config) -> Option<ExtractorChain> {
    let cookies = cfg.ig_cookies_path.as_ref()?;
    let backends: Vec<Box<dyn Extractor>> = vec![
        Box::new(gallery_dl::GalleryDlExtractor::new(
            cfg.gallery_dl_path.clone(),
            cookies.clone(),
        )),
        Box::new(
            yt_dlp::YtDlpExtractor::new(cfg.yt_dlp_path.clone(), Some(cookies.clone()))
                .no_playlist(),
        ),
    ];
    let chain = ExtractorChain::new(backends);
    tracing::info!(backends = ?chain.names(), "instagram story extractor chain built");
    Some(chain)
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
        assert!(is_meta_cdn(
            "https://instagram.fabc1-2.fna.fbcdn.net/v/b.mp4"
        ));
        assert!(!is_meta_cdn(
            "https://attacker.example/p?ref=cdninstagram.com"
        ));
        assert!(!is_meta_cdn("https://cdninstagram.com.evil.test/x"));
        assert!(!is_meta_cdn("ftp://scontent.cdninstagram.com/x"));
        assert!(!is_meta_cdn("not a url"));
    }

    #[test]
    fn media_by_ext_only_real_video_exts() {
        assert_eq!(
            media_by_ext("https://x.cdninstagram.com/v/a.mp4?q=1").kind,
            MediaKind::Video
        );
        assert_eq!(
            media_by_ext("https://x.cdninstagram.com/o1/v/a.jpg").kind,
            MediaKind::Image
        );
        assert_eq!(
            media_by_ext("https://x.cdninstagram.com/v/a.webp").kind,
            MediaKind::Image
        );
    }

    /// Mock backend: `Ok(n)` yields a Post with `n` media items; `Err(e)` fails.
    pub(super) struct Mock(pub(super) Result<usize, ExtractError>);
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
            ExtractError::Restricted => ExtractError::Restricted,
            ExtractError::RateLimited => ExtractError::RateLimited,
            ExtractError::Unavailable(s) => ExtractError::Unavailable(s.clone()),
            ExtractError::Transient(s) => ExtractError::Transient(s.clone()),
            ExtractError::IncompleteMedia(r) => ExtractError::IncompleteMedia(*r),
        }
    }

    #[tokio::test]
    async fn restricted_failure_survives_embed_miss() {
        let c = ExtractorChain::new_accepting_textonly(vec![
            Box::new(Mock(Err(ExtractError::Restricted))),
            Box::new(Mock(Err(ExtractError::Unavailable("empty embed".into())))),
        ]);
        assert!(matches!(
            c.extract("u", "s").await,
            Err(ExtractError::Restricted)
        ));
        assert!(matches!(threads_status(429), ExtractError::RateLimited));
        assert!(matches!(threads_status(403), ExtractError::Blocked));
        assert!(matches!(threads_status(503), ExtractError::Transient(_)));
    }

    #[tokio::test]
    async fn chain_returns_first_with_media() {
        let c = ExtractorChain::new(vec![Box::new(Mock(Ok(0))), Box::new(Mock(Ok(2)))]);
        assert_eq!(c.extract("u", "s").await.unwrap().media.len(), 2);
    }

    #[tokio::test]
    async fn chain_keeps_most_severe_error() {
        // Blocked then empty-media must surface Blocked, not NotFound.
        let c = ExtractorChain::new(vec![
            Box::new(Mock(Err(ExtractError::Blocked))),
            Box::new(Mock(Ok(0))),
        ]);
        assert!(matches!(
            c.extract("u", "s").await,
            Err(ExtractError::Blocked)
        ));
        // empty-media then Blocked also surfaces Blocked.
        let c = ExtractorChain::new(vec![
            Box::new(Mock(Ok(0))),
            Box::new(Mock(Err(ExtractError::Blocked))),
        ]);
        assert!(matches!(
            c.extract("u", "s").await,
            Err(ExtractError::Blocked)
        ));
        // only empty-media → NotFound.
        let c = ExtractorChain::new(vec![Box::new(Mock(Ok(0)))]);
        assert!(matches!(
            c.extract("u", "s").await,
            Err(ExtractError::NotFound)
        ));
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
        assert!(matches!(
            c.extract("u", "s").await,
            Err(ExtractError::NotFound)
        ));
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
        assert!(matches!(
            c.extract("u", "s").await,
            Err(ExtractError::NotFound)
        ));
        let c = ExtractorChain::new_accepting_textonly(vec![Box::new(TextOnly(None))]);
        assert!(matches!(
            c.extract("u", "s").await,
            Err(ExtractError::NotFound)
        ));
    }
}

#[cfg(test)]
mod story_chain_tests {
    use super::*;

    #[test]
    fn probe_binary_reports_missing_and_present_tools() {
        assert!(probe_binary("igbot-definitely-not-a-real-binary").is_err());
        // `CARGO` is set by cargo for every crate it compiles, so it names a
        // binary guaranteed to exist wherever these tests run.
        let v = probe_binary(env!("CARGO")).expect("cargo --version");
        assert!(v.to_lowercase().contains("cargo"), "{v}");
    }

    #[test]
    fn story_chain_requires_cookies_and_never_uses_the_embed_scraper() {
        let mut cfg = Config::test_default();
        assert!(
            build_ig_story_chain(&cfg).is_none(),
            "cookieless -> no story chain"
        );
        cfg.ig_cookies_path = Some("/etc/igbot/cookies.txt".into());
        let chain = build_ig_story_chain(&cfg).expect("cookies -> story chain");
        // gallery-dl first (it handles image stories; yt-dlp only yields video
        // items), then yt-dlp. The embed scraper is excluded: a logged-out
        // story page's og:image is the profile picture, not the story.
        assert_eq!(chain.names(), vec!["gallery-dl", "yt-dlp"]);
    }
}

#[cfg(test)]
mod completeness_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rendition_selection_preserves_signature_and_skips_invalid_first() {
        let node = json!({"media_type": 2, "video_versions": [
            {"url":"https://evil.test/video.mp4"},
            {"url":"https://x.cdninstagram.com/v.mp4?z=2&amp;a=1\\u0026sig=x%2Fy"}
        ], "image_versions2":{"candidates":[{"url":"https://x.cdninstagram.com/poster.jpg"}]}});
        let found = collect_meta_media_strict(&node);
        assert!(!found.incomplete);
        assert_eq!(found.media.len(), 1);
        assert_eq!(found.media[0].kind, MediaKind::Video);
        assert_eq!(
            found.media[0].url,
            "https://x.cdninstagram.com/v.mp4?z=2&a=1&sig=x%2Fy"
        );
        let image = json!({"media_type":1,"image_versions2":{"candidates":[
            {"url":"bad"},{"url":"https://x.fbcdn.net/image.jpg?b=2&a=1"}]}});
        assert_eq!(
            collect_meta_media(&image)[0].url,
            "https://x.fbcdn.net/image.jpg?b=2&a=1"
        );
    }

    #[test]
    fn missing_video_is_never_replaced_with_poster() {
        let found = collect_meta_media_strict(&json!({"media_type":2,
            "video_versions":[{"url":"https://evil.test/v.mp4"}],
            "image_versions2":{"candidates":[{"url":"https://x.fbcdn.net/poster.jpg"}]}}));
        assert!(found.incomplete);
        assert!(found.media.is_empty());
        assert_eq!(
            found.required,
            MediaRequirements {
                total: 1,
                videos: 1,
                images: 0
            }
        );
    }

    #[test]
    fn carousel_counts_missing_slots_and_keeps_survivors_in_order() {
        let found = collect_meta_media_strict(&json!({"media_type":8,"carousel_media_count":4,
        "carousel_media":[
            {"media_type":1,"image_versions2":{"candidates":[{"url":"https://x.fbcdn.net/one.jpg"}]}},
            {"media_type":2,"video_versions":[]},
            {}
        ]}));
        assert!(found.incomplete);
        assert_eq!(found.media.len(), 1);
        assert_eq!(
            found.required,
            MediaRequirements {
                total: 4,
                images: 1,
                videos: 1
            }
        );
        let text = collect_meta_media_strict(&json!({"media_type":19,"caption":{"text":"hello"}}));
        assert!(!text.incomplete);
        assert_eq!(text.required, MediaRequirements::default());
    }

    #[test]
    fn absent_carousel_children_do_not_become_a_thumbnail() {
        for children in [Value::Null, json!("unrecognized")] {
            let found = collect_meta_media_strict(&json!({
                "media_type":8,"carousel_media_count":3,"carousel_media":children,
                "image_versions2":{"candidates":[{"url":"https://x.fbcdn.net/thumbnail.jpg"}]}
            }));
            assert!(found.incomplete);
            assert!(found.media.is_empty());
            assert_eq!(found.required.total, 3);
        }
    }

    struct ResultBackend(Result<Vec<Media>, MediaRequirements>);
    #[async_trait]
    impl Extractor for ResultBackend {
        fn name(&self) -> &'static str {
            "completeness-test"
        }
        async fn extract(&self, _: &str, _: &str) -> Result<Post, ExtractError> {
            match &self.0 {
                Ok(media) => Ok(Post {
                    author: None,
                    caption: Some("caption".into()),
                    media: media.clone(),
                    original_url: "u".into(),
                }),
                Err(required) => Err(ExtractError::IncompleteMedia(*required)),
            }
        }
    }
    fn missing(total: usize, images: usize, videos: usize) -> Box<dyn Extractor> {
        Box::new(ResultBackend(Err(MediaRequirements {
            total,
            images,
            videos,
        })))
    }
    fn recovered(media: Vec<Media>) -> Box<dyn Extractor> {
        Box::new(ResultBackend(Ok(media)))
    }

    #[tokio::test]
    async fn incomplete_chain_rejects_text_partial_and_wrong_kind_but_recovers() {
        let cases = [
            vec![],
            vec![Media::image("i")],
            vec![Media::image("i"), Media::image("j")],
        ];
        for media in cases {
            let chain =
                ExtractorChain::new_accepting_textonly(vec![missing(2, 1, 1), recovered(media)]);
            assert!(matches!(
                chain.extract("u", "s").await,
                Err(ExtractError::IncompleteMedia(_))
            ));
        }
        let chain = ExtractorChain::new_accepting_textonly(vec![
            missing(2, 1, 1),
            recovered(vec![]),
            recovered(vec![Media::image("i"), Media::video("v")]),
        ]);
        assert_eq!(chain.extract("u", "s").await.unwrap().media.len(), 2);
    }

    #[tokio::test]
    async fn incomplete_chain_accumulates_requirements_and_preserves_restriction_priority() {
        let chain = ExtractorChain::new_accepting_textonly(vec![
            missing(1, 0, 1),
            missing(3, 2, 0),
            recovered(vec![]),
        ]);
        match chain.extract("u", "s").await {
            Err(ExtractError::IncompleteMedia(r)) => assert_eq!(
                r,
                MediaRequirements {
                    total: 3,
                    images: 2,
                    videos: 1
                }
            ),
            other => panic!("expected incomplete, got {other:?}"),
        }
        let chain = ExtractorChain::new_accepting_textonly(vec![
            missing(1, 0, 1),
            Box::new(super::tests::Mock(Err(ExtractError::Restricted))),
        ]);
        assert!(matches!(
            chain.extract("u", "s").await,
            Err(ExtractError::Restricted)
        ));
    }
}
