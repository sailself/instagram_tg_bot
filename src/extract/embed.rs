//! PRIMARY backend: in-process scraper of the public post page (PLAN §4.1).
//! Anonymous, no subprocess. Current Instagram serves *browser* UAs a JS-only
//! shell with no media, but still serves *crawler* UAs (e.g.
//! `facebookexternalhit`) the Polaris `application/json` blob (full media) plus
//! Open Graph tags — so we fetch the post page with the configured crawler UA.
//! Parses shapes in order (modern Polaris JSON → legacy JSON → rendered HTML →
//! OG) and retries intermittent transient responses.

use super::{
    collect_meta_media, dedup_media, is_meta_cdn, map_status, normalize_cdn_url, ExtractError,
    Extractor, Media, Post,
};
use async_trait::async_trait;
use scraper::{Html, Selector};
use serde_json::Value;
use std::time::Duration;

const MAX_ATTEMPTS: u32 = 3;

pub struct EmbedScraper {
    http: reqwest::Client,
    user_agent: String,
}

impl EmbedScraper {
    pub fn new(http: reqwest::Client, user_agent: String) -> Self {
        Self { http, user_agent }
    }

    async fn fetch(&self, embed: &str) -> Result<String, ExtractError> {
        let resp = self
            .http
            .get(embed)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .header(
                reqwest::header::ACCEPT,
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .send()
            .await
            .map_err(|e| ExtractError::Transient(format!("embed net: {e}")))?;

        let status = resp.status();
        if status.is_success() {
            let body = resp
                .text()
                .await
                .map_err(|e| ExtractError::Transient(format!("embed body: {e}")))?;
            tracing::debug!(status = status.as_u16(), bytes = body.len(), "embed fetch ok");
            Ok(body)
        } else {
            tracing::debug!(status = status.as_u16(), "embed fetch non-success");
            Err(map_status(status.as_u16()))
        }
    }
}

#[async_trait]
impl Extractor for EmbedScraper {
    fn name(&self) -> &'static str {
        "embed"
    }

    async fn extract(&self, url: &str, shortcode: &str) -> Result<Post, ExtractError> {
        // Fetch the canonical post URL the detector built (Instagram's
        // `/p/<code>/` page); `shortcode` keys the embedded-JSON match.
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(300 * (1 << attempt))).await;
            }
            match self.fetch(url).await {
                Ok(html) => {
                    if let Some(post) = parse_post(&html, shortcode, url) {
                        return Ok(post);
                    }
                    if looks_login_walled(&html) {
                        return Err(ExtractError::Blocked);
                    }
                    // JS-shell with no media: retry.
                    tracing::debug!(shortcode, attempt, "embed returned no media, retrying");
                }
                // Transient → retry; hard errors → let the chain fall through.
                Err(ExtractError::Transient(_)) => continue,
                Err(other) => return Err(other),
            }
        }
        Err(ExtractError::NotFound)
    }
}

/// Try the three response shapes in order; first one with media wins.
fn parse_post(html: &str, shortcode: &str, original_url: &str) -> Option<Post> {
    let build = |author: Option<String>, caption: Option<String>, media: Vec<Media>| Post {
        author,
        caption,
        media: dedup_media(media),
        original_url: original_url.to_string(),
    };

    // Shape 0: modern Polaris `application/json` blob (current Instagram — the
    // post page carries every embedded post keyed by `code`).
    if let Some((author, caption, media)) = parse_polaris_json(html, shortcode) {
        if !media.is_empty() {
            return Some(build(author, caption, media));
        }
    }

    // Shape 1: the legacy embedded `__additionalDataLoaded(...)` JSON blob.
    if let Some((author, caption, media)) = parse_json_shape(html) {
        if !media.is_empty() {
            return Some(build(author, caption, media));
        }
    }

    let doc = Html::parse_document(html);

    // Shape 2: rendered <img>/<video> in the captioned HTML.
    let (a2, c2, m2) = parse_html_shape(&doc);
    if !m2.is_empty() {
        // Enrich missing caption/author from OG tags.
        let (a3, c3, _) = parse_og_shape(&doc);
        return Some(build(a2.or(a3), c2.or(c3), m2));
    }

    // Shape 3: Open Graph tags (last resort within the embed).
    let (a3, c3, m3) = parse_og_shape(&doc);
    if !m3.is_empty() {
        return Some(build(a3, c3, m3));
    }

    None
}

// ----------------------------------------------------------------------------
// Shape 0 — modern Polaris `application/json` payload (current Instagram).
// ----------------------------------------------------------------------------

/// Scan `<script type="application/json">` blobs for the post whose `code`
/// matches `shortcode`, then read its media. The page embeds *related* posts
/// too, so we MUST key on the shortcode rather than grab every media node.
fn parse_polaris_json(
    html: &str,
    shortcode: &str,
) -> Option<(Option<String>, Option<String>, Vec<Media>)> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse(r#"script[type="application/json"]"#).ok()?;
    for script in doc.select(&sel) {
        let text = script.text().collect::<String>();
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(node) = find_post_node(&value, shortcode) else {
            continue;
        };
        let media = collect_meta_media(node);
        if media.is_empty() {
            continue;
        }
        let caption = node
            .pointer("/caption/text")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let author = node
            .pointer("/user/username")
            .or_else(|| node.pointer("/owner/username"))
            .and_then(Value::as_str)
            .map(str::to_string);
        return Some((author, caption, media));
    }
    None
}

/// Recursively locate the object whose `code` equals `shortcode` and which
/// actually carries media (so a bare reference to the same code is skipped).
fn find_post_node<'a>(v: &'a Value, shortcode: &str) -> Option<&'a Value> {
    match v {
        Value::Object(map) => {
            if map.get("code").and_then(Value::as_str) == Some(shortcode)
                && (map.contains_key("media_type")
                    || map.contains_key("video_versions")
                    || map.contains_key("image_versions2")
                    || map.contains_key("carousel_media"))
            {
                return Some(v);
            }
            map.values().find_map(|child| find_post_node(child, shortcode))
        }
        Value::Array(arr) => arr.iter().find_map(|child| find_post_node(child, shortcode)),
        _ => None,
    }
}

/// Extract `__additionalDataLoaded('extra', { … })` and read `shortcode_media`.
fn parse_json_shape(html: &str) -> Option<(Option<String>, Option<String>, Vec<Media>)> {
    let raw = json_object_after(html, "__additionalDataLoaded(")?;
    let value: Value = serde_json::from_str(raw).ok()?;
    let node = find_shortcode_media(&value)?;

    let author = node
        .pointer("/owner/username")
        .and_then(Value::as_str)
        .map(str::to_string);
    let caption = node
        .pointer("/edge_media_to_caption/edges/0/node/text")
        .and_then(Value::as_str)
        .map(str::to_string);
    let media = media_from_node(node);
    Some((author, caption, media))
}

/// Find the first balanced `{ … }` JSON object following `marker` in `html`,
/// respecting string literals and escapes.
fn json_object_after<'a>(html: &'a str, marker: &str) -> Option<&'a str> {
    let after = &html[html.find(marker)? + marker.len()..];
    let start = after.find('{')?;
    let bytes = after.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for i in start..bytes.len() {
        let b = bytes[i];
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else {
            match b {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&after[start..=i]);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Locate the `shortcode_media` node at the various nesting depths IG has used.
fn find_shortcode_media(v: &Value) -> Option<&Value> {
    for ptr in ["/shortcode_media", "/graphql/shortcode_media", "/gql_data/shortcode_media"] {
        if let Some(n) = v.pointer(ptr) {
            return Some(n);
        }
    }
    // Some embed payloads put the media object at the top level.
    if v.get("owner").is_some() && (v.get("display_url").is_some() || v.get("video_url").is_some()) {
        return Some(v);
    }
    None
}

fn media_from_node(node: &Value) -> Vec<Media> {
    if let Some(edges) = node
        .pointer("/edge_sidecar_to_children/edges")
        .and_then(Value::as_array)
    {
        let mut out = Vec::new();
        for edge in edges {
            if let Some(child) = edge.get("node") {
                push_single(child, &mut out);
            }
        }
        return out;
    }
    let mut out = Vec::new();
    push_single(node, &mut out);
    out
}

fn push_single(node: &Value, out: &mut Vec<Media>) {
    let is_video = node.get("is_video").and_then(Value::as_bool).unwrap_or(false);
    if is_video {
        // Prefer the playable video; if it's gated (no video_url), push nothing
        // so the chain falls through to yt-dlp rather than sending a poster.
        if let Some(u) = node.get("video_url").and_then(Value::as_str) {
            out.push(Media::video(normalize_cdn_url(u)));
        }
        return;
    }
    if let Some(u) = node.get("display_url").and_then(Value::as_str) {
        out.push(Media::image(normalize_cdn_url(u)));
    }
}

fn parse_html_shape(doc: &Html) -> (Option<String>, Option<String>, Vec<Media>) {
    let mut media = Vec::new();
    for src in all_attrs(doc, "img.EmbeddedMediaImage, .EmbeddedMediaImage img, img[referrerpolicy]", "src") {
        if is_meta_cdn(&src) {
            media.push(Media::image(normalize_cdn_url(&src)));
        }
    }
    for src in all_attrs(doc, "video, video source", "src") {
        if is_meta_cdn(&src) {
            media.push(Media::video(normalize_cdn_url(&src)));
        }
    }
    let author = first_text(doc, ".UsernameText, .Username, .EmbedUsername");
    let caption = first_text(doc, ".Caption, .EmbedCaption");
    (author, caption, media)
}

fn parse_og_shape(doc: &Html) -> (Option<String>, Option<String>, Vec<Media>) {
    let mut media = Vec::new();
    let video = first_attr(doc, r#"meta[property="og:video"], meta[property="og:video:secure_url"]"#, "content");
    if let Some(v) = video.filter(|v| is_meta_cdn(v)) {
        media.push(Media::video(normalize_cdn_url(&v)));
    } else {
        for img in all_attrs(doc, r#"meta[property="og:image"]"#, "content") {
            if is_meta_cdn(&img) {
                media.push(Media::image(normalize_cdn_url(&img)));
            }
        }
    }
    let author = first_attr(doc, r#"meta[property="og:title"]"#, "content").map(|t| {
        t.split(" on Instagram")
            .next()
            .unwrap_or(&t)
            .split(" • ")
            .next()
            .unwrap_or(&t)
            .trim()
            .to_string()
    });
    let caption = first_attr(doc, r#"meta[property="og:description"]"#, "content");
    (author, caption, media)
}

fn looks_login_walled(html: &str) -> bool {
    // Called only when no media was parsed, so we key purely on wall phrases
    // (regardless of page size — a large login wall must still classify as
    // Blocked, not be retried into a false NotFound). The benign JS-shell lacks
    // these specific user-facing strings, so it falls through to retry.
    let l = html.to_ascii_lowercase();
    l.contains("log in to instagram")
        || l.contains("see this content")
        || l.contains("isn't available")
        || l.contains("login required")
}

// --- small scraper helpers ---

fn first_attr(doc: &Html, css: &str, attr: &str) -> Option<String> {
    let sel = Selector::parse(css).ok()?;
    doc.select(&sel)
        .next()
        .and_then(|e| e.value().attr(attr))
        .map(str::to_string)
}

fn first_text(doc: &Html, css: &str) -> Option<String> {
    let sel = Selector::parse(css).ok()?;
    let text = doc.select(&sel).next()?.text().collect::<String>();
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn all_attrs(doc: &Html, css: &str, attr: &str) -> Vec<String> {
    match Selector::parse(css) {
        Ok(sel) => doc
            .select(&sel)
            .filter_map(|e| e.value().attr(attr).map(str::to_string))
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::MediaKind;

    #[test]
    fn json_shape_single_image_decodes_escapes() {
        let html = r#"<script>window.__additionalDataLoaded('extra', {"shortcode_media":{"owner":{"username":"nasa"},"is_video":false,"display_url":"https://scontent.cdninstagram.com/v/t51/img.jpg?nc=1&oe=ABC","edge_media_to_caption":{"edges":[{"node":{"text":"Hello rocket world"}}]}}});</script>"#;
        let post = parse_post(html, "SC", "https://www.instagram.com/p/SC/").unwrap();
        assert_eq!(post.author.as_deref(), Some("nasa"));
        assert_eq!(post.caption.as_deref(), Some("Hello rocket world"));
        assert_eq!(post.media.len(), 1);
        assert_eq!(post.media[0].kind, MediaKind::Image);
        // & must have been decoded back to a literal &.
        assert!(post.media[0].url.contains("?nc=1&oe=ABC"), "url={}", post.media[0].url);
        assert!(is_meta_cdn(&post.media[0].url));
    }

    #[test]
    fn json_shape_carousel_mixed() {
        let html = r#"<script>window.__additionalDataLoaded('extra', {"shortcode_media":{"owner":{"username":"u"},"edge_sidecar_to_children":{"edges":[
            {"node":{"is_video":false,"display_url":"https://scontent.cdninstagram.com/v/a.jpg"}},
            {"node":{"is_video":true,"video_url":"https://scontent.cdninstagram.com/v/b.mp4","display_url":"https://scontent.cdninstagram.com/v/poster.jpg"}}
        ]}}});</script>"#;
        let post = parse_post(html, "SC", "orig").unwrap();
        assert_eq!(post.media.len(), 2);
        assert_eq!(post.media[0].kind, MediaKind::Image);
        assert_eq!(post.media[1].kind, MediaKind::Video);
        assert!(post.media[1].url.ends_with("b.mp4"));
    }

    #[test]
    fn json_shape_gated_video_yields_no_media() {
        // is_video but no video_url => push nothing => parse_post returns None
        // (chain will fall through to yt-dlp).
        let html = r#"<script>window.__additionalDataLoaded('extra', {"shortcode_media":{"owner":{"username":"u"},"is_video":true,"display_url":"https://scontent.cdninstagram.com/v/poster.jpg"}});</script>"#;
        assert!(parse_post(html, "SC", "orig").is_none());
    }

    #[test]
    fn html_shape_decodes_ampersands() {
        let html = r#"<html><body><img class="EmbeddedMediaImage" src="https://scontent.cdninstagram.com/v/abc.jpg?x=1&amp;y=2"><span class="UsernameText">someone</span></body></html>"#;
        let post = parse_post(html, "SC", "orig").unwrap();
        assert_eq!(post.media.len(), 1);
        assert!(post.media[0].url.contains("x=1&y=2"), "url={}", post.media[0].url);
        assert_eq!(post.author.as_deref(), Some("someone"));
    }

    #[test]
    fn og_shape_fallback() {
        let html = r#"<html><head>
            <meta property="og:image" content="https://scontent.cdninstagram.com/v/og.jpg?z=1&amp;w=2">
            <meta property="og:title" content="NASA on Instagram">
            <meta property="og:description" content="a caption">
        </head><body></body></html>"#;
        let post = parse_post(html, "SC", "orig").unwrap();
        assert_eq!(post.media.len(), 1);
        assert_eq!(post.author.as_deref(), Some("NASA"));
        assert_eq!(post.caption.as_deref(), Some("a caption"));
        assert!(post.media[0].url.contains("z=1&w=2"));
    }

    #[test]
    fn login_wall_detected_regardless_of_size() {
        assert!(looks_login_walled("<p>Log in to Instagram to see this content</p>"));
        // A large wall page must still be detected (not retried into NotFound).
        let big = format!("<html>{}log in to instagram</html>", "x".repeat(6000));
        assert!(looks_login_walled(&big));
        // An ordinary page without wall phrases is not a wall.
        assert!(!looks_login_walled("<html>ordinary content, no wall here</html>"));
    }

    #[test]
    fn no_media_no_wall_returns_none() {
        assert!(parse_post("<html><body>nothing here</body></html>", "SC", "orig").is_none());
    }

    // --- Shape 0: modern Polaris application/json ---

    #[test]
    fn polaris_picks_post_by_code_not_related() {
        // The page embeds the target reel (SC) AND a related post (OTHER).
        // We must extract SC's video, decoding the escaped-slash CDN URL, and
        // ignore OTHER entirely. Nested to exercise the recursive search.
        let html = r#"<html><body><script type="application/json">
        {"require":[["ScheduledServerJS","handle",null,[{"__bbox":{"result":{"data":{"items":[
          {"code":"OTHER","media_type":8,"carousel_media":[
             {"image_versions2":{"candidates":[{"url":"https://scontent.cdninstagram.com/v/other1.jpg"}]}}
          ],"caption":{"text":"other post"},"user":{"username":"someoneelse"}},
          {"code":"SC","media_type":2,
           "image_versions2":{"candidates":[{"url":"https://scontent.cdninstagram.com/v/poster.jpg"}]},
           "video_versions":[{"type":101,"url":"https:\/\/instagram.fabc1-1.fna.fbcdn.net\/o1\/v\/t2\/f2\/reel.mp4?x=1&y=2"}],
           "caption":{"text":"hello reel"},"user":{"username":"milanodascrocco"}}
        ]}}}}]]]}
        </script></body></html>"#;
        let post = parse_post(html, "SC", "https://www.instagram.com/p/SC/").unwrap();
        assert_eq!(post.author.as_deref(), Some("milanodascrocco"));
        assert_eq!(post.caption.as_deref(), Some("hello reel"));
        assert_eq!(post.media.len(), 1, "only SC's single video");
        assert_eq!(post.media[0].kind, MediaKind::Video);
        assert!(post.media[0].url.contains("reel.mp4?x=1&y=2"), "url={}", post.media[0].url);
        assert!(is_meta_cdn(&post.media[0].url));
    }

    #[test]
    fn polaris_carousel_expands_and_prefers_video() {
        let html = r#"<html><body><script type="application/json">
        {"items":[{"code":"SC","media_type":8,"carousel_media":[
           {"image_versions2":{"candidates":[{"url":"https://scontent.cdninstagram.com/v/c1.jpg"}]}},
           {"media_type":2,
            "image_versions2":{"candidates":[{"url":"https://scontent.cdninstagram.com/v/poster2.jpg"}]},
            "video_versions":[{"type":101,"url":"https://scontent.cdninstagram.com/v/c2.mp4"}]}
        ],"caption":{"text":"carousel"},"user":{"username":"u"}}]}
        </script></body></html>"#;
        let post = parse_post(html, "SC", "orig").unwrap();
        assert_eq!(post.media.len(), 2);
        assert_eq!(post.media[0].kind, MediaKind::Image);
        assert!(post.media[0].url.ends_with("c1.jpg"));
        assert_eq!(post.media[1].kind, MediaKind::Video, "video child, not its poster");
        assert!(post.media[1].url.ends_with("c2.mp4"));
    }

    #[test]
    fn polaris_single_image_takes_largest_candidate() {
        let html = r#"<html><body><script type="application/json">
        {"data":{"xdt_shortcode_media":{"code":"SC","media_type":1,"image_versions2":{"candidates":[
            {"width":1080,"url":"https://scontent.cdninstagram.com/v/full.jpg"},
            {"width":320,"url":"https://scontent.cdninstagram.com/v/small.jpg"}]},
            "caption":{"text":"pic"},"user":{"username":"u"}}}}
        </script></body></html>"#;
        let post = parse_post(html, "SC", "orig").unwrap();
        assert_eq!(post.media.len(), 1);
        assert_eq!(post.media[0].kind, MediaKind::Image);
        assert!(post.media[0].url.ends_with("full.jpg"), "candidates[0] is largest");
    }

    #[test]
    fn polaris_rejects_non_cdn_url() {
        // A media URL on a non-IG host must be dropped (no SSRF/exfil).
        let html = r#"<html><body><script type="application/json">
        {"items":[{"code":"SC","media_type":2,
          "video_versions":[{"type":101,"url":"https://evil.example/x.mp4"}],
          "user":{"username":"u"}}]}
        </script></body></html>"#;
        assert!(parse_polaris_json(html, "SC").is_none_or(|(_, _, m)| m.is_empty()));
    }
}
