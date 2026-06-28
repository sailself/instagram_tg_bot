//! SECONDARY backend: shell out to `yt-dlp -J` (PLAN §4.2). Video workhorse,
//! upstream-maintained. Weak on pure-image posts — that's why embed leads and
//! gallery-dl (cookies) backstops images.

use super::{truncate_chars, ExtractError, InstagramExtractor, Media, Post};
use crate::urls::post_url;
use async_trait::async_trait;
use serde_json::Value;
use std::io::ErrorKind;
use tokio::process::Command;

pub struct YtDlpExtractor {
    path: String,
    cookies: Option<String>,
}

impl YtDlpExtractor {
    pub fn new(path: String, cookies: Option<String>) -> Self {
        Self { path, cookies }
    }
}

#[async_trait]
impl InstagramExtractor for YtDlpExtractor {
    fn name(&self) -> &'static str {
        "yt-dlp"
    }

    async fn extract(&self, _url: &str, shortcode: &str) -> Result<Post, ExtractError> {
        let target = post_url(shortcode);
        let mut cmd = Command::new(&self.path);
        cmd.arg("-J")
            .arg("--no-warnings")
            .arg("--no-progress")
            .arg("--ignore-config");
        if let Some(c) = &self.cookies {
            cmd.arg("--cookies").arg(c);
        }
        cmd.arg(&target);

        let output = match cmd.output().await {
            Ok(o) => o,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(ExtractError::Unavailable(format!(
                    "yt-dlp binary not found at '{}'",
                    self.path
                )));
            }
            Err(e) => return Err(ExtractError::Transient(format!("yt-dlp spawn: {e}"))),
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
            tracing::debug!(
                code = output.status.code(),
                stderr = %truncate_chars(stderr.trim(), 300),
                "yt-dlp non-zero exit"
            );
            return Err(classify_stderr(&stderr));
        }

        tracing::debug!(stdout_bytes = output.stdout.len(), "yt-dlp ok");
        let value: Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| ExtractError::Transient(format!("yt-dlp json: {e}")))?;
        parse_info(&value, &target).ok_or(ExtractError::NotFound)
    }
}

fn classify_stderr(stderr: &str) -> ExtractError {
    if stderr.contains("login required")
        || stderr.contains("rate-limit")
        || stderr.contains("rate limit")
        || stderr.contains("requested content is not available")
        || stderr.contains("checkpoint")
        // IG's anonymous-API gate for login-walled posts: "instagram sent an
        // empty media response … use --cookies-from-browser or --cookies".
        || stderr.contains("empty media response")
        || stderr.contains("--cookies-from-browser")
    {
        ExtractError::Blocked
    } else if stderr.contains("private")
        || stderr.contains("not available")
        || stderr.contains("does not exist")
        || stderr.contains("removed")
        || stderr.contains("no video")
    {
        ExtractError::NotFound
    } else {
        ExtractError::Transient(format!("yt-dlp failed: {}", stderr.lines().last().unwrap_or("").trim()))
    }
}

fn parse_info(value: &Value, original_url: &str) -> Option<Post> {
    let mut media = Vec::new();

    // Carousel / multi-video posts come back as a playlist.
    if value.get("_type").and_then(Value::as_str) == Some("playlist") {
        if let Some(entries) = value.get("entries").and_then(Value::as_array) {
            for e in entries {
                if let Some(m) = entry_media(e) {
                    media.push(m);
                }
            }
        }
    } else if let Some(m) = entry_media(value) {
        media.push(m);
    }

    if media.is_empty() {
        return None;
    }

    let author = str_field(value, &["uploader", "channel", "uploader_id"]);
    let caption = str_field(value, &["description", "title"]);

    Some(Post {
        author,
        caption,
        media: super::dedup_media(media),
        original_url: original_url.to_string(),
    })
}

/// Direct media URL for an entry: prefer top-level `url`, else the best format.
fn best_media_url(entry: &Value) -> Option<String> {
    if let Some(u) = entry.get("url").and_then(Value::as_str) {
        if u.starts_with("http") {
            return Some(u.to_string());
        }
    }
    let formats = entry.get("formats").and_then(Value::as_array)?;
    // yt-dlp orders formats worst→best, so the last with a usable URL wins.
    formats
        .iter()
        .rev()
        .find_map(|f| f.get("url").and_then(Value::as_str))
        .map(str::to_string)
}

/// Build a `Media` for an entry, classifying image vs video from yt-dlp's
/// metadata. yt-dlp is video-centric, but image carousel children must not be
/// sent as video (Telegram rejects a still image sent via `send_video`).
fn entry_media(entry: &Value) -> Option<Media> {
    let url = best_media_url(entry)?;
    let ext = entry
        .get("ext")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let vcodec = entry.get("vcodec").and_then(Value::as_str).unwrap_or("");
    let is_image = matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp" | "heic" | "gif")
        || (vcodec == "none" && !ext.is_empty());
    Some(if is_image {
        Media::image(url)
    } else {
        Media::video(url)
    })
}

fn str_field(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| {
        v.get(*k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_reel() {
        let v: Value = serde_json::from_str(
            r#"{"description":"cap","uploader":"bob","url":"https://scontent.cdninstagram.com/v/r.mp4","formats":[]}"#,
        )
        .unwrap();
        let post = parse_info(&v, "orig").unwrap();
        assert_eq!(post.author.as_deref(), Some("bob"));
        assert_eq!(post.media.len(), 1);
        assert!(post.media[0].url.ends_with("r.mp4"));
    }

    #[test]
    fn picks_best_format_when_no_top_url() {
        let v: Value = serde_json::from_str(
            r#"{"description":"c","uploader":"u","formats":[{"url":"https://x/lo.mp4"},{"url":"https://x/hi.mp4"}]}"#,
        )
        .unwrap();
        let post = parse_info(&v, "orig").unwrap();
        assert!(post.media[0].url.ends_with("hi.mp4"));
    }

    #[test]
    fn parses_playlist_carousel() {
        let v: Value = serde_json::from_str(
            r#"{"_type":"playlist","uploader":"u","title":"t","entries":[
                {"url":"https://x/a.mp4"},{"url":"https://x/b.mp4"}]}"#,
        )
        .unwrap();
        let post = parse_info(&v, "orig").unwrap();
        assert_eq!(post.media.len(), 2);
    }

    #[test]
    fn mixed_carousel_classifies_image_and_video() {
        let v: Value = serde_json::from_str(
            r#"{"_type":"playlist","uploader":"u","entries":[
                {"url":"https://x/a.jpg","ext":"jpg"},
                {"url":"https://x/b.mp4","ext":"mp4"}]}"#,
        )
        .unwrap();
        let post = parse_info(&v, "orig").unwrap();
        assert_eq!(post.media.len(), 2);
        assert_eq!(post.media[0].kind, crate::extract::MediaKind::Image);
        assert_eq!(post.media[1].kind, crate::extract::MediaKind::Video);
    }

    #[test]
    fn no_media_returns_none() {
        let v: Value = serde_json::from_str(r#"{"description":"c","formats":[]}"#).unwrap();
        assert!(parse_info(&v, "orig").is_none());
    }

    #[test]
    fn stderr_classification() {
        assert!(matches!(classify_stderr("error: login required"), ExtractError::Blocked));
        assert!(matches!(classify_stderr("this post is private"), ExtractError::NotFound));
        assert!(matches!(classify_stderr("some weird error"), ExtractError::Transient(_)));
        // IG's anonymous gate for login-walled posts must read as Blocked, not
        // a generic Transient (which would mis-message users as rate-limited).
        assert!(matches!(
            classify_stderr("instagram sent an empty media response. use --cookies-from-browser or --cookies"),
            ExtractError::Blocked
        ));
    }
}
