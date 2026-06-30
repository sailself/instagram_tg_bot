//! Off-by-default external fallback (PLAN §4.8): a reader / purpose-built IG API
//! that fetches from a DIFFERENT IP when our datacenter IP is blocked. Enabled
//! via `FALLBACK_PROVIDER` (+ optional key). Best-effort parsers — validate live
//! before relying on them. Sits LAST in the chain.

use super::{
    dedup_media, is_meta_cdn, map_status, media_by_ext, normalize_cdn_url, ExtractError, Extractor,
    Media, Post,
};
use crate::config::Config;
use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

/// Matches IG CDN media URLs (the `/v/` path excludes static JS/CSS bundles).
static CDN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"https://[A-Za-z0-9._-]+\.(?:cdninstagram\.com|fbcdn\.net)/v/[^\s"')\]\\]+"#)
        .expect("valid CDN regex")
});

#[derive(Clone, Copy, Debug)]
pub enum Provider {
    Jina,
    EmbedEz,
}

pub struct ExternalFallback {
    provider: Provider,
    http: reqwest::Client,
    jina_key: Option<String>,
    user_agent: String,
}

impl ExternalFallback {
    pub fn from_config(name: &str, cfg: &Config, http: reqwest::Client) -> Option<Self> {
        let provider = match name {
            "jina" | "reader" => Provider::Jina,
            "embedez" | "embed_ez" => Provider::EmbedEz,
            _ => return None,
        };
        Some(Self {
            provider,
            http,
            jina_key: cfg.jina_api_key.clone(),
            user_agent: cfg.user_agent.clone(),
        })
    }

    /// Jina Reader (PLAN §4.8). Needs a free key for IG; `X-No-Cache` keeps our
    /// group's links out of the shared cache.
    async fn jina(&self, url: &str) -> Result<(Option<String>, Option<String>, Vec<Media>), ExtractError> {
        let endpoint = format!("https://r.jina.ai/{url}");
        let mut req = self
            .http
            .get(&endpoint)
            .header("Accept", "application/json")
            .header("X-Engine", "browser")
            .header("X-Retain-Images", "all")
            .header("X-With-Images-Summary", "true")
            .header("X-No-Cache", "true")
            .header(reqwest::header::USER_AGENT, &self.user_agent);
        if let Some(k) = &self.jina_key {
            req = req.bearer_auth(k).header("X-Proxy", "auto");
        }

        let resp = req
            .send()
            .await
            .map_err(|e| ExtractError::Transient(format!("jina net: {e}")))?;
        let code = resp.status().as_u16();
        if !resp.status().is_success() {
            return Err(map_status(code));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| ExtractError::Transient(format!("jina body: {e}")))?;
        tracing::debug!(provider = "jina", status = code, bytes = body.len(), "jina response");
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        if v.get("code").and_then(Value::as_u64) == Some(451) {
            return Err(ExtractError::Blocked);
        }
        let data = v.get("data").unwrap_or(&v);
        let content = data.get("content").and_then(Value::as_str).unwrap_or(&body);

        let lc = content.to_ascii_lowercase();
        if content.len() < 200 || lc.contains("log in to instagram") || lc.contains("see this content")
        {
            return Err(ExtractError::Blocked);
        }

        let mut media = collect_images_field(data);
        media.extend(cdn_urls_in(content));
        let media = dedup_media(media);

        let caption = Some(content.trim().to_string()).filter(|s| !s.is_empty());
        let author = data
            .get("title")
            .and_then(Value::as_str)
            .map(strip_author_title);
        Ok((author, caption, media))
    }

    /// EmbedEZ keyless: search → key, then embed. Best-effort (under-documented).
    async fn embedez(&self, url: &str) -> Result<(Option<String>, Option<String>, Vec<Media>), ExtractError> {
        let search = format!(
            "https://embedez.com/api/v1/search?q={}",
            percent_encode(url)
        );
        let r1 = self
            .http
            .get(&search)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .send()
            .await
            .map_err(|e| ExtractError::Transient(format!("embedez search: {e}")))?;
        if !r1.status().is_success() {
            return Err(map_status(r1.status().as_u16()));
        }
        let j1: Value = r1
            .json()
            .await
            .map_err(|e| ExtractError::Transient(format!("embedez search json: {e}")))?;
        let key = j1
            .pointer("/data/key")
            .or_else(|| j1.get("key"))
            .and_then(Value::as_str)
            .ok_or(ExtractError::NotFound)?;

        let embed = format!("https://embedez.com/api/v1/embed?key={key}");
        let r2 = self
            .http
            .get(&embed)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .send()
            .await
            .map_err(|e| ExtractError::Transient(format!("embedez embed: {e}")))?;
        if !r2.status().is_success() {
            return Err(map_status(r2.status().as_u16()));
        }
        let body = r2
            .text()
            .await
            .map_err(|e| ExtractError::Transient(format!("embedez body: {e}")))?;
        tracing::debug!(provider = "embedez", bytes = body.len(), "embedez response");

        let media = dedup_media(cdn_urls_in(&body));
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let caption = first_str(&v, &["/data/content/text", "/data/caption", "/caption"]);
        let author = first_str(&v, &["/data/user/username", "/data/author", "/user/name"]);
        Ok((author, caption, media))
    }
}

#[async_trait]
impl Extractor for ExternalFallback {
    fn name(&self) -> &'static str {
        match self.provider {
            Provider::Jina => "reader(jina)",
            Provider::EmbedEz => "embedez",
        }
    }

    async fn extract(&self, url: &str, _shortcode: &str) -> Result<Post, ExtractError> {
        let (author, caption, media) = match self.provider {
            Provider::Jina => self.jina(url).await?,
            Provider::EmbedEz => self.embedez(url).await?,
        };
        if media.is_empty() {
            return Err(ExtractError::NotFound);
        }
        Ok(Post {
            author,
            caption,
            media,
            original_url: url.to_string(),
        })
    }
}

fn cdn_urls_in(text: &str) -> Vec<Media> {
    CDN_RE
        .find_iter(text)
        .map(|m| normalize_cdn_url(m.as_str()))
        .filter(|u| is_meta_cdn(u))
        .map(|u| media_by_ext(&u))
        .collect()
}

fn collect_images_field(data: &Value) -> Vec<Media> {
    let mut out = Vec::new();
    let push = |s: &str, out: &mut Vec<Media>| {
        if is_meta_cdn(s) {
            out.push(media_by_ext(&normalize_cdn_url(s)));
        }
    };
    match data.get("images") {
        Some(Value::Object(o)) => o.values().filter_map(Value::as_str).for_each(|s| push(s, &mut out)),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).for_each(|s| push(s, &mut out)),
        _ => {}
    }
    out
}

fn strip_author_title(t: &str) -> String {
    t.split(" on Instagram")
        .next()
        .unwrap_or(t)
        .split(" • ")
        .next()
        .unwrap_or(t)
        .trim()
        .to_string()
}

fn first_str(v: &Value, ptrs: &[&str]) -> Option<String> {
    ptrs.iter().find_map(|p| {
        v.pointer(p)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_cdn_urls_from_markdown_and_decodes() {
        let md = r#"caption text ![img](https://scontent.cdninstagram.com/v/a.jpg?x=1&amp;y=2) more
            and a video https://scontent-lhr.fbcdn.net/v/b.mp4?z=1 end"#;
        let media = dedup_media(cdn_urls_in(md));
        assert_eq!(media.len(), 2);
        assert!(media[0].url.contains("x=1&y=2"), "url={}", media[0].url);
        assert_eq!(media[1].kind, crate::extract::MediaKind::Video);
    }

    #[test]
    fn ignores_static_bundles() {
        // static.cdninstagram.com/rsrc.php/... has no /v/ path → not matched.
        let md = "https://static.cdninstagram.com/rsrc.php/v3/yx/app.js";
        assert!(cdn_urls_in(md).is_empty());
    }

    #[test]
    fn images_field_object_and_array() {
        let v: Value = serde_json::from_str(
            r#"{"images":{"a":"https://scontent.cdninstagram.com/v/1.jpg","b":"https://x/no.jpg"}}"#,
        )
        .unwrap();
        assert_eq!(collect_images_field(&v).len(), 1);
    }

    #[test]
    fn percent_encode_basic() {
        assert_eq!(percent_encode("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn strip_title() {
        assert_eq!(strip_author_title("NASA on Instagram: \"hi\""), "NASA");
        assert_eq!(strip_author_title("bob • Instagram"), "bob");
    }
}
