//! FALLBACK Threads backend: the `/embed` HTML card (Threads design). When the
//! inline-JSON scrape misses (schema drift), the small server-rendered embed
//! page still carries the media + caption + author in semantic HTML. Fetched
//! with the same browser header set plus `Sec-Fetch-Dest: iframe`.
//!
//! Best-effort, like the IG embed → yt-dlp fallback philosophy: weaker than the
//! JSON path on multi-item carousels (no explicit ordering/`has_audio`), but a
//! cheap second layer that also covers text-only posts.

use super::{dedup_media, is_meta_cdn, normalize_cdn_url, ExtractError, Extractor, Media, Post};
use async_trait::async_trait;
use scraper::{ElementRef, Html, Selector};

pub struct ThreadsEmbedScraper {
    http: reqwest::Client,
    user_agent: String,
    sec_ch_ua: String,
}

impl ThreadsEmbedScraper {
    pub fn new(http: reqwest::Client, user_agent: String, sec_ch_ua: String) -> Self {
        Self { http, user_agent, sec_ch_ua }
    }

    async fn fetch(&self, embed_url: &str) -> Result<String, ExtractError> {
        let resp = self
            .http
            .get(embed_url)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .header(
                reqwest::header::ACCEPT,
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .header("sec-ch-ua", &self.sec_ch_ua)
            .header("sec-ch-ua-mobile", "?0")
            .header("sec-ch-ua-platform", "\"Windows\"")
            // The embed endpoint serves the SSR card to an iframe destination.
            .header("sec-fetch-dest", "iframe")
            .header("sec-fetch-mode", "navigate")
            .header("sec-fetch-site", "cross-site")
            .send()
            .await
            .map_err(|e| ExtractError::Transient(format!("threads embed net: {e}")))?;

        let status = resp.status();
        if status.is_success() {
            let body = resp
                .text()
                .await
                .map_err(|e| ExtractError::Transient(format!("threads embed body: {e}")))?;
            tracing::debug!(status = status.as_u16(), bytes = body.len(), "threads embed fetch ok");
            Ok(body)
        } else {
            tracing::debug!(status = status.as_u16(), "threads embed fetch non-success");
            Err(super::threads_status(status.as_u16()))
        }
    }
}

#[async_trait]
impl Extractor for ThreadsEmbedScraper {
    fn name(&self) -> &'static str {
        "threads-embed"
    }

    async fn extract(&self, url: &str, _shortcode: &str) -> Result<Post, ExtractError> {
        let embed_url = format!("{}/embed", url.trim_end_matches('/'));
        let html = self.fetch(&embed_url).await?;
        parse_response(&html, url)
    }
}

fn parse_response(html: &str, url: &str) -> Result<Post, ExtractError> {
    if let Some(post) = parse_embed(html, url) {
        return Ok(post);
    }
    if super::threads_json::looks_login_walled(html) {
        return Err(ExtractError::Restricted);
    }
    Err(ExtractError::Unavailable("Threads embed contains no recognized post content".into()))
}

/// Parse the SSR embed card: media from non-avatar `img.img` / `<video>` (CDN-gated),
/// caption from `.BodyTextContainer`, author from `.NameContainer`. Returns a
/// text-only [`Post`] when there's a caption but no media.
fn parse_embed(html: &str, original_url: &str) -> Option<Post> {
    let doc = Html::parse_document(html);

    let mut media = Vec::new();
    // The embed uses img.img for both post photos and the author's avatar.
    // AvatarContainer identifies profile images without guessing from sizes
    // or changing signed CDN URLs. Check all ancestors for nested wrappers.
    let image_selector = Selector::parse("img.img").ok()?;
    for image in doc.select(&image_selector) {
        if image.ancestors().filter_map(ElementRef::wrap).any(|ancestor| {
            ancestor.value().classes().any(|class| class == "AvatarContainer")
        }) {
            continue;
        }
        let Some(src) = image.value().attr("src") else {
            continue;
        };
        if is_meta_cdn(src) {
            media.push(Media::image(normalize_cdn_url(src)));
        }
    }
    for src in all_attrs(&doc, "video, video source", "src") {
        if is_meta_cdn(&src) {
            media.push(Media::video(normalize_cdn_url(&src)));
        }
    }
    let media = dedup_media(media);

    let caption = first_text(&doc, ".BodyTextContainer");
    let author = first_text(&doc, ".NameContainer");

    let has_text = caption.as_deref().is_some_and(|c| !c.trim().is_empty());
    if media.is_empty() && !has_text {
        return None;
    }
    Some(Post { author, caption, media, original_url: original_url.to_string() })
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
    fn embed_single_image_with_caption_and_author() {
        let html = r#"<html><body>
            <div class="NameContainer"><a href="/@zuck">zuck</a></div>
            <img class="img" src="https://scontent.cdninstagram.com/v/a.jpg?x=1&amp;y=2">
            <div class="BodyTextContainer"><span>hello from the embed</span></div>
        </body></html>"#;
        let p = parse_embed(html, "https://www.threads.com/@zuck/post/SC").unwrap();
        assert_eq!(p.author.as_deref(), Some("zuck"));
        assert_eq!(p.caption.as_deref(), Some("hello from the embed"));
        assert_eq!(p.media.len(), 1);
        assert_eq!(p.media[0].kind, MediaKind::Image);
        // HTML entity decoded; query string otherwise intact.
        assert!(p.media[0].url.contains("x=1&y=2"), "url={}", p.media[0].url);
    }

    #[test]
    fn embed_excludes_avatar_preserving_post_images() {
        // Representative structure captured from DdVO0fGCX-j's live embed.
        let html = r#"<div class="OuterContainer">
            <div class="AvatarContainer"><img class="img" alt="feel_pale"
                width="36" height="36" src="https://scontent.cdninstagram.com/avatar.jpg"></div>
            <div class="NameContainer">feel_pale</div>
            <div class="BodyContainerNoThreadLine"><div class="SoloMediaContainer">
                <div class="SingleInnerMediaContainer"><img class="img" draggable="false"
                    src="https://scontent.cdninstagram.com/post.jpg?b=2&amp;a=1"></div>
            </div></div>
            <img class="img" width="36" height="36" src="https://scontent.cdninstagram.com/small-post.jpg">
            <div class="BodyTextContainer">post caption</div>
        </div>"#;
        let p = parse_embed(html, "orig").unwrap();
        assert_eq!(p.media.len(), 2);
        assert_eq!(p.media[0].url, "https://scontent.cdninstagram.com/post.jpg?b=2&a=1");
        assert_eq!(p.media[1].url, "https://scontent.cdninstagram.com/small-post.jpg");
        assert_eq!(p.author.as_deref(), Some("feel_pale"));
        assert_eq!(p.caption.as_deref(), Some("post caption"));
    }

    #[test]
    fn embed_nested_avatar_does_not_turn_text_into_media() {
        let avatar = r#"<div class="AvatarContainer extra"><a><span>
            <img class="img" src="https://scontent.cdninstagram.com/avatar.jpg">
            </span></a></div>"#;
        assert!(parse_embed(avatar, "orig").is_none());
        let html = format!("{avatar}<div class=\"BodyTextContainer\">text only</div>");
        let p = parse_embed(&html, "orig").unwrap();
        assert!(p.media.is_empty());
        assert_eq!(p.caption.as_deref(), Some("text only"));
    }

    #[tokio::test]
    #[ignore = "live public Threads request; post availability and markup can change"]
    async fn live_reported_post_excludes_avatar() {
        let scraper = ThreadsEmbedScraper::new(
            crate::threads_http::clients(None).unwrap().0,
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36".into(),
            r#""Not/A)Brand";v="8", "Chromium";v="126", "Google Chrome";v="126""#.into(),
        );
        let html = scraper.fetch("https://www.threads.com/@feel_pale/post/DdVO0fGCX-j/embed").await.unwrap();
        assert!(html.contains("AvatarContainer"), "probe must include avatar markup");
        let p = parse_embed(&html, "https://www.threads.com/@feel_pale/post/DdVO0fGCX-j").unwrap();
        assert_eq!(p.media.len(), 1, "only the post photo, no avatar");
        assert_eq!(p.media[0].kind, MediaKind::Image);
    }

    #[tokio::test]
    #[ignore = "live anonymous Threads audience-restriction probe"]
    async fn live_audience_restriction_is_not_rate_limiting() {
        let (http, share_http) = crate::threads_http::clients(None).unwrap();
        let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
        let ch = r#""Not/A)Brand";v="8", "Chromium";v="126", "Google Chrome";v="126""#;
        let resolver = crate::threads_share::ThreadsShareResolver::with_client(share_http, ua.into(), ch.into());
        let resolved = resolver.resolve("https://www.threads.com/share/BARWV10v9i/").await.unwrap();
        let json = super::super::threads_json::ThreadsScraper::new(http.clone(), ua.into(), ch.into());
        let embed = ThreadsEmbedScraper::new(http, ua.into(), ch.into());
        assert!(matches!(json.extract(&resolved.canonical_url, &resolved.shortcode).await, Err(ExtractError::Restricted)));
        assert!(matches!(embed.extract(&resolved.canonical_url, &resolved.shortcode).await, Err(ExtractError::Unavailable(_))));
    }

    #[test]
    fn embed_response_distinguishes_wall_from_unrecognized_markup() {
        assert!(matches!(parse_response("<h1>This content isn't available to everyone</h1>", "u"), Err(ExtractError::Restricted)));
        assert!(matches!(parse_response("<script>Log in to see</script>", "u"), Err(ExtractError::Unavailable(_))));
        assert!(parse_response("<div class=\"BodyTextContainer\">Log in to see</div>", "u").is_ok());
    }

    #[test]
    fn embed_video() {
        let html = r#"<html><body>
            <div class="NameContainer">u</div>
            <video src="https://scontent.cdninstagram.com/v/clip.mp4"></video>
            <div class="BodyTextContainer">clip caption</div>
        </body></html>"#;
        let p = parse_embed(html, "orig").unwrap();
        assert_eq!(p.media.len(), 1);
        assert_eq!(p.media[0].kind, MediaKind::Video);
    }

    #[test]
    fn embed_text_only() {
        let html = r#"<html><body>
            <div class="NameContainer">someone</div>
            <div class="BodyTextContainer">a text-only thread</div>
        </body></html>"#;
        let p = parse_embed(html, "orig").unwrap();
        assert!(p.media.is_empty());
        assert_eq!(p.caption.as_deref(), Some("a text-only thread"));
        assert_eq!(p.author.as_deref(), Some("someone"));
    }

    #[test]
    fn embed_rejects_non_cdn_media() {
        let html = r#"<html><body>
            <img class="img" src="https://evil.example/x.jpg">
            <div class="BodyTextContainer">t</div>
        </body></html>"#;
        let p = parse_embed(html, "orig").unwrap();
        assert!(p.media.is_empty(), "non-CDN image dropped");
    }

    #[test]
    fn embed_with_no_usable_content_returns_none() {
        assert!(parse_embed("<html><body>nothing</body></html>", "orig").is_none());
    }
}
