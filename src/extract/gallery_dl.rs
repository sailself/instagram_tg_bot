//! Conditional backend: `gallery-dl -j` for image carousels (PLAN §4.3).
//! Requires cookies, so it is only added to the chain when `IG_COOKIES_PATH` is
//! set — inactive under the cookieless-first default.

use super::{
    dedup_media, is_meta_cdn, media_by_ext, normalize_cdn_url, truncate_chars, ExtractError,
    Extractor, Post,
};
use async_trait::async_trait;
use serde_json::Value;
use std::io::ErrorKind;
use tokio::process::Command;

pub struct GalleryDlExtractor {
    path: String,
    cookies: String,
}

impl GalleryDlExtractor {
    pub fn new(path: String, cookies: String) -> Self {
        Self { path, cookies }
    }
}

#[async_trait]
impl Extractor for GalleryDlExtractor {
    fn name(&self) -> &'static str {
        "gallery-dl"
    }

    async fn extract(&self, url: &str, _shortcode: &str) -> Result<Post, ExtractError> {
        let mut cmd = Command::new(&self.path);
        cmd.arg("-j")
            .arg("--cookies")
            .arg(&self.cookies)
            .arg("--sleep-request")
            .arg("2.0")
            .arg(url);

        let output = match cmd.output().await {
            Ok(o) => o,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(ExtractError::Unavailable(format!(
                    "gallery-dl binary not found at '{}'",
                    self.path
                )));
            }
            Err(e) => return Err(ExtractError::Transient(format!("gallery-dl spawn: {e}"))),
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
            tracing::debug!(
                code = output.status.code(),
                stderr = %truncate_chars(stderr.trim(), 300),
                "gallery-dl non-zero exit"
            );
            return Err(classify_stderr(&stderr));
        }

        tracing::debug!(stdout_bytes = output.stdout.len(), "gallery-dl ok");
        let value: Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| ExtractError::Transient(format!("gallery-dl json: {e}")))?;
        parse(&value, url).ok_or(ExtractError::NotFound)
    }
}

fn classify_stderr(s: &str) -> ExtractError {
    if s.contains("401") || s.contains("403") || s.contains("login") || s.contains("challenge") {
        ExtractError::Blocked
    } else if s.contains("404") || s.contains("not found") || s.contains("does not exist") {
        ExtractError::NotFound
    } else {
        ExtractError::Transient(format!(
            "gallery-dl failed: {}",
            s.lines().last().unwrap_or("").trim()
        ))
    }
}

/// `gallery-dl -j` emits an array of `[type, url, metadata]`-ish tuples. Scan
/// each tuple for an IG CDN URL and pull caption/author from the metadata.
fn parse(v: &Value, original_url: &str) -> Option<Post> {
    let arr = v.as_array()?;
    let mut media = Vec::new();
    let mut author = None;
    let mut caption = None;

    for item in arr {
        let Some(elems) = item.as_array() else { continue };
        for e in elems {
            match e {
                Value::String(s) if is_meta_cdn(s) => media.push(media_by_ext(&normalize_cdn_url(s))),
                Value::Object(o) => {
                    if caption.is_none() {
                        caption = o
                            .get("description")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string);
                    }
                    if author.is_none() {
                        author = o
                            .get("username")
                            .or_else(|| o.get("owner"))
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                }
                _ => {}
            }
        }
    }

    if media.is_empty() {
        return None;
    }
    Some(Post {
        author,
        caption,
        media: dedup_media(media),
        original_url: original_url.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_carousel_tuples() {
        let v: Value = serde_json::from_str(
            r#"[
              [3, "https://scontent.cdninstagram.com/v/a.jpg", {"username":"bob","description":"hi"}],
              [3, "https://scontent.cdninstagram.com/v/b.mp4", {"username":"bob"}]
            ]"#,
        )
        .unwrap();
        let post = parse(&v, "orig").unwrap();
        assert_eq!(post.media.len(), 2);
        assert_eq!(post.author.as_deref(), Some("bob"));
        assert_eq!(post.caption.as_deref(), Some("hi"));
        assert_eq!(post.media[1].kind, crate::extract::MediaKind::Video);
    }

    #[test]
    fn classifies_login_redirect() {
        assert!(matches!(classify_stderr("http redirect to login page 403"), ExtractError::Blocked));
    }
}
