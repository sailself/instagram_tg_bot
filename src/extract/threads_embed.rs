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
        Self {
            http,
            user_agent,
            sec_ch_ua,
        }
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
            tracing::debug!(
                status = status.as_u16(),
                bytes = body.len(),
                "threads embed fetch ok"
            );
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
    match parse_embed_checked(html, url) {
        Ok(Some(post)) => return Ok(post),
        Err(ExtractError::IncompleteMedia(_)) if super::threads_json::looks_login_walled(html) => {
            return Err(ExtractError::Restricted);
        }
        Err(error) => return Err(error),
        Ok(None) => {}
    }
    if super::threads_json::looks_login_walled(html) {
        return Err(ExtractError::Restricted);
    }
    Err(ExtractError::Unavailable(
        "Threads embed contains no recognized post content".into(),
    ))
}

/// Scope attachments to semantic media containers; link previews are decoration.
#[cfg(test)]
fn parse_embed(html: &str, original_url: &str) -> Option<Post> {
    parse_embed_checked(html, original_url).ok().flatten()
}

fn parse_embed_checked(html: &str, original_url: &str) -> Result<Option<Post>, ExtractError> {
    let doc = Html::parse_document(html);
    let root = doc
        .select(&Selector::parse(".OuterContainer").unwrap())
        .next()
        .unwrap_or_else(|| doc.root_element());
    let mut media = Vec::new();
    let mut required = super::MediaRequirements::default();
    let mut incomplete = false;
    let selector = Selector::parse("img.img, video").unwrap();
    for element in root.select(&selector) {
        if excluded(element, root) || !in_media_container(element, root) {
            continue;
        }
        if element.value().name() == "video" {
            required.total += 1;
            required.videos += 1;
            let sources = Selector::parse("source[src]").unwrap();
            let src = element
                .value()
                .attr("src")
                .into_iter()
                .chain(
                    element
                        .select(&sources)
                        .filter_map(|s| s.value().attr("src")),
                )
                .map(normalize_cdn_url)
                .find(|s| is_meta_cdn(s));
            if let Some(src) = src {
                media.push(Media::video(src));
            } else {
                incomplete = true;
            }
        } else {
            // A video player's sibling image is its poster, never another attachment.
            let player = element.ancestors().filter_map(ElementRef::wrap).find(|e| {
                e.value()
                    .classes()
                    .any(|c| c == "SingleInnerMediaContainer")
            });
            if player.is_some_and(|p| {
                p.select(&Selector::parse("video").unwrap())
                    .next()
                    .is_some()
            }) {
                continue;
            }
            required.total += 1;
            required.images += 1;
            let src = element
                .value()
                .attr("src")
                .map(normalize_cdn_url)
                .filter(|s| is_meta_cdn(s));
            if let Some(src) = src {
                media.push(Media::image(src));
            } else {
                incomplete = true;
            }
        }
    }
    // A semantic attachment shell with no recognized image/player is evidence of
    // missing media, including lazy placeholders and markup drift.
    let shells = Selector::parse(
        ".SoloMediaContainer, .SingleInnerMediaContainer, .CarouselMediaContainer, .MediaContainer",
    )
    .unwrap();
    for shell in root.select(&shells) {
        if excluded(shell, root) || shell.select(&shells).next().is_some() {
            continue;
        }
        if shell.select(&selector).next().is_none() {
            required.total += 1;
            incomplete = true;
        }
    }
    for link in root.select(&Selector::parse("a[href]").unwrap()) {
        if excluded_quote(link, root) {
            continue;
        }
        let is_card = link
            .ancestors()
            .filter_map(ElementRef::wrap)
            .any(|e| e.value().classes().any(|c| c.starts_with("LinkAttachment")));
        if is_card {
            if let Some(card) =
                instagram_attachment_requirements(link.value().attr("href").unwrap_or_default())
            {
                // The card has no proven identity relationship with native attachments:
                // require its media in addition rather than silently discarding it.
                required.total += card.total;
                required.videos += card.videos;
                required.images += card.images;
                incomplete = true;
            }
        }
    }
    if incomplete {
        return Err(ExtractError::IncompleteMedia(required));
    }
    let media = dedup_media(media);
    let caption = root
        .select(&Selector::parse(".BodyTextContainer").unwrap())
        .find(|e| !excluded_quote(*e, root))
        .and_then(content_text);
    let author = root
        .select(&Selector::parse(".NameContainer a[href]").unwrap())
        .filter(|e| !excluded_quote(*e, root))
        .find_map(|e| profile_name(e.value().attr("href")?));
    if media.is_empty() && caption.is_none() {
        return Ok(None);
    }
    Ok(Some(Post {
        author,
        caption,
        media,
        original_url: original_url.to_string(),
    }))
}

fn excluded_quote(element: ElementRef<'_>, root: ElementRef<'_>) -> bool {
    element
        .ancestors()
        .filter_map(ElementRef::wrap)
        .take_while(|e| e.id() != root.id())
        .any(|e| {
            e.value().classes().any(|c| {
                c.contains("Quote")
                    || c.contains("Repost")
                    || c.contains("Recommend")
                    || c == "OuterContainer"
            })
        })
}

fn excluded(element: ElementRef<'_>, root: ElementRef<'_>) -> bool {
    excluded_quote(element, root)
        || element
            .ancestors()
            .filter_map(ElementRef::wrap)
            .take_while(|e| e.id() != root.id())
            .any(|e| {
                e.value()
                    .classes()
                    .any(|c| c == "AvatarContainer" || c.starts_with("LinkAttachment"))
            })
}

fn in_media_container(element: ElementRef<'_>, root: ElementRef<'_>) -> bool {
    element
        .ancestors()
        .filter_map(ElementRef::wrap)
        .take_while(|e| e.id() != root.id())
        .any(|e| {
            e.value().classes().any(|c| {
                matches!(
                    c,
                    "SoloMediaContainer"
                        | "SingleInnerMediaContainer"
                        | "CarouselMediaContainer"
                        | "MediaContainer"
                )
            })
        })
}

fn profile_name(href: &str) -> Option<String> {
    let url = url::Url::parse("https://www.threads.com")
        .ok()?
        .join(href)
        .ok()?;
    if url.scheme() != "https"
        || !matches!(
            url.host_str()?,
            "threads.com" | "www.threads.com" | "threads.net" | "www.threads.net"
        )
    {
        return None;
    }
    let name = url.path().trim_matches('/').strip_prefix('@')?;
    (!name.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'.'))
    .then(|| name.to_string())
}

/// Classify only known HTTPS Instagram post targets, without fetching redirects.
/// `/p/` can contain either media kind; reels and TV require playable video.
pub(super) fn instagram_attachment_requirements(href: &str) -> Option<super::MediaRequirements> {
    let mut url = url::Url::parse(href).ok()?;
    if url.scheme() == "https" && url.host_str() == Some("l.facebook.com") && url.path() == "/l.php"
    {
        let target = url.query_pairs().find(|(k, _)| k == "u")?.1.into_owned();
        url = url::Url::parse(&target).ok()?;
    }
    if url.scheme() != "https"
        || !matches!(url.host_str(), Some("instagram.com" | "www.instagram.com"))
    {
        return None;
    }
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    if parts.len() != 2
        || !matches!(parts[0], "p" | "reel" | "tv")
        || parts[1].is_empty()
        || !parts[1]
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        return None;
    }
    Some(super::MediaRequirements {
        total: 1,
        videos: usize::from(parts[0] != "p"),
        images: 0,
    })
}

fn content_text(element: ElementRef<'_>) -> Option<String> {
    fn walk(element: ElementRef<'_>, out: &mut String) {
        for node in element.children() {
            if let Some(text) = node.value().as_text() {
                out.push_str(text);
            } else if let Some(child) = ElementRef::wrap(node) {
                if matches!(child.value().name(), "script" | "style") {
                    continue;
                }
                let boundary = matches!(child.value().name(), "br" | "p" | "div");
                if boundary && !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                walk(child, out);
                if boundary && child.value().name() != "br" && !out.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
    }
    let mut text = String::new();
    walk(element, &mut text);
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::MediaKind;

    #[test]
    fn card_classifier_preserves_video_requirements_and_profile_paths() {
        let reel =
            instagram_attachment_requirements("https://www.instagram.com/reel/Video_1/").unwrap();
        assert_eq!(reel.videos, 1);
        assert!(
            !reel.is_satisfied_by(&[Media::image("https://scontent.cdninstagram.com/poster.jpg")])
        );
        assert!(
            reel.is_satisfied_by(&[Media::video("https://scontent.cdninstagram.com/video.mp4")])
        );
        assert_eq!(
            instagram_attachment_requirements("https://instagram.com/p/Post/")
                .unwrap()
                .videos,
            0
        );
        for url in [
            "https://instagram.com.evil.example/reel/V/",
            "http://instagram.com/reel/V/",
            "https://instagram.com/stories/u/1/",
            "https://evil.example/?u=https://instagram.com/reel/V/",
        ] {
            assert!(instagram_attachment_requirements(url).is_none());
        }
        assert_eq!(
            profile_name("https://www.threads.com/@meow.meow.journey?x=1").as_deref(),
            Some("meow.meow.journey")
        );
        assert!(profile_name("/intent/follow?username=u").is_none());
        assert!(profile_name("/search?q=topic").is_none());
        assert!(profile_name("https://evil.example/@u").is_none());
    }

    #[test]
    fn unresolved_shell_is_incomplete_but_visible_restriction_is_stronger() {
        let html = r#"<div class="SoloMediaContainer"><div data-media-placeholder="true"></div></div><div class="BodyTextContainer">caption</div>"#;
        assert!(matches!(
            parse_response(html, "orig"),
            Err(ExtractError::IncompleteMedia(_))
        ));
        let wall = format!("{html}<h1>This content isn't available to everyone</h1>");
        assert!(matches!(
            parse_response(&wall, "orig"),
            Err(ExtractError::Restricted)
        ));
        assert!(parse_response(
            r#"<div class="BodyTextContainer">This content isn't available to everyone</div>"#,
            "orig"
        )
        .is_ok());
    }

    #[test]
    fn captured_linked_reel_is_incomplete_and_author_is_profile_only() {
        let html = include_str!("../../tests/fixtures/threads_embed_linked_reel.html");
        assert!(matches!(
            parse_response(html, "orig"),
            Err(ExtractError::IncompleteMedia(_))
        ));
        let ordinary = html.replace(
            "https%3A%2F%2Fwww.instagram.com%2Freel%2FLinkedVideo%2F",
            "https%3A%2F%2Fexample.com%2Farticle",
        );
        let p = parse_response(&ordinary, "orig").unwrap();
        assert!(p.media.is_empty());
        assert_eq!(p.author.as_deref(), Some("meow.meow.journey"));
        assert_eq!(p.caption.as_deref(), Some("A linked Instagram reel."));
    }

    #[test]
    fn scoped_mixed_media_preserves_order_and_uses_valid_video_source() {
        let html = r#"<div class="OuterContainer">
          <div class="NameContainer"><a href="/search?q=topic">topic</a><a href="/@owner?x=1">owner</a><a href="/intent/follow">Follow</a></div>
          <div class="SoloMediaContainer"><div class="SingleInnerMediaContainer">
            <video src="https://evil.example/no.mp4" poster="https://scontent.cdninstagram.com/poster.jpg">
              <source src="https://evil.example/no2.mp4"><source src="https://scontent.cdninstagram.com/v.mp4?z=3&amp;a=2">
            </video><img class="img" src="https://scontent.cdninstagram.com/poster.jpg">
          </div><div class="SingleInnerMediaContainer"><img class="img" src="https://scontent.cdninstagram.com/p.jpg"></div></div>
          <div class="QuoteContainer"><div class="SoloMediaContainer"><img class="img" src="https://scontent.cdninstagram.com/quote.jpg"></div><div class="BodyTextContainer">quoted</div></div>
          <div class="RecommendationContainer"><div class="SoloMediaContainer"><video src="https://scontent.cdninstagram.com/recommended.mp4"></video></div></div>
          <div class="BodyTextContainer">first<br>second<p>third <a>linked words</a></p></div>
          <div class="OuterContainer"><div class="SoloMediaContainer"><img class="img" src="https://scontent.cdninstagram.com/nested.jpg"></div></div>
        </div>"#;
        let p = parse_response(html, "orig").unwrap();
        assert_eq!(p.author.as_deref(), Some("owner"));
        assert_eq!(
            p.caption.as_deref(),
            Some("first\nsecond\nthird linked words")
        );
        assert_eq!(p.media.len(), 2);
        assert_eq!(p.media[0].kind, MediaKind::Video);
        assert_eq!(
            p.media[0].url,
            "https://scontent.cdninstagram.com/v.mp4?z=3&a=2"
        );
        assert_eq!(p.media[1].kind, MediaKind::Image);
    }

    #[test]
    fn known_video_without_playable_source_is_incomplete() {
        let html = r#"<div class="SoloMediaContainer"><video poster="https://scontent.cdninstagram.com/p.jpg"><source src="https://evil.example/v.mp4"></video></div><div class="BodyTextContainer">caption</div>"#;
        assert!(matches!(
            parse_response(html, "orig"),
            Err(ExtractError::IncompleteMedia(
                super::super::MediaRequirements {
                    total: 1,
                    videos: 1,
                    ..
                }
            ))
        ));
    }

    #[test]
    fn embed_single_image_with_caption_and_author() {
        let html = r#"<html><body>
            <div class="NameContainer"><a href="/@zuck">zuck</a></div>
            <div class="SoloMediaContainer"><img class="img" src="https://scontent.cdninstagram.com/v/a.jpg?x=1&amp;y=2"></div>
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
            <div class="NameContainer"><a href="/@feel_pale">feel_pale</a></div>
            <div class="BodyContainerNoThreadLine"><div class="SoloMediaContainer">
                <div class="SingleInnerMediaContainer"><img class="img" draggable="false"
                    src="https://scontent.cdninstagram.com/post.jpg?b=2&amp;a=1"></div>
            </div></div>
            <div class="SoloMediaContainer"><img class="img" width="36" height="36" src="https://scontent.cdninstagram.com/small-post.jpg"></div>
            <div class="BodyTextContainer">post caption</div>
        </div>"#;
        let p = parse_embed(html, "orig").unwrap();
        assert_eq!(p.media.len(), 2);
        assert_eq!(
            p.media[0].url,
            "https://scontent.cdninstagram.com/post.jpg?b=2&a=1"
        );
        assert_eq!(
            p.media[1].url,
            "https://scontent.cdninstagram.com/small-post.jpg"
        );
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
        let html = scraper
            .fetch("https://www.threads.com/@feel_pale/post/DdVO0fGCX-j/embed")
            .await
            .unwrap();
        assert!(
            html.contains("AvatarContainer"),
            "probe must include avatar markup"
        );
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
        let resolver = crate::threads_share::ThreadsShareResolver::with_client(
            share_http,
            ua.into(),
            ch.into(),
        );
        let resolved = resolver
            .resolve("https://www.threads.com/share/BARWV10v9i/")
            .await
            .unwrap();
        let json =
            super::super::threads_json::ThreadsScraper::new(http.clone(), ua.into(), ch.into());
        let embed = ThreadsEmbedScraper::new(http, ua.into(), ch.into());
        assert!(matches!(
            json.extract(&resolved.canonical_url, &resolved.shortcode)
                .await,
            Err(ExtractError::Restricted)
        ));
        assert!(matches!(
            embed
                .extract(&resolved.canonical_url, &resolved.shortcode)
                .await,
            Err(ExtractError::Unavailable(_))
        ));
    }

    #[test]
    fn embed_response_distinguishes_wall_from_unrecognized_markup() {
        assert!(matches!(
            parse_response("<h1>This content isn't available to everyone</h1>", "u"),
            Err(ExtractError::Restricted)
        ));
        assert!(matches!(
            parse_response("<script>Log in to see</script>", "u"),
            Err(ExtractError::Unavailable(_))
        ));
        assert!(
            parse_response("<div class=\"BodyTextContainer\">Log in to see</div>", "u").is_ok()
        );
    }

    #[test]
    fn embed_video() {
        let html = r#"<html><body>
            <div class="NameContainer">u</div>
            <div class="SoloMediaContainer"><video src="https://scontent.cdninstagram.com/v/clip.mp4"></video></div>
            <div class="BodyTextContainer">clip caption</div>
        </body></html>"#;
        let p = parse_embed(html, "orig").unwrap();
        assert_eq!(p.media.len(), 1);
        assert_eq!(p.media[0].kind, MediaKind::Video);
    }

    #[test]
    fn embed_text_only() {
        let html = r#"<html><body>
            <div class="NameContainer"><a href="/@someone">someone</a></div>
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
