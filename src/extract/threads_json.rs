//! PRIMARY Threads backend: in-process scrape of the public post page (Threads
//! design). Anonymous, no subprocess. Threads serves logged-out clients the full
//! private-API JSON server-side inside `<script type="application/json">`
//! (`data-sjs`) blocks — BUT only when sent a coherent desktop-browser header
//! set; a crawler/naive UA gets an empty SPA shell (HTTP 200, zero
//! recognized target records), which we classify as a failure, never silent success.
//!
//! The JSON reuses Instagram's Polaris/Barcelona shape (`code`, `media_type`,
//! `image_versions2`, `video_versions`, `carousel_media`, `caption.text`,
//! `user.username`), so media collection is shared via [`collect_meta_media_strict`].
//! An Instagram post shared inline to Threads rides along as a full Polaris
//! node under `text_post_app_info/linked_inline_media` (observed live); its
//! media is appended after the post's own via [`collect_threads_media`].
//!
//! NB: the exact nesting for quote/repost (`text_post_app_info.share_info.*`) is
//! best-effort and flagged for live validation; it degrades to the outer post.

use super::{
    collect_meta_media_strict, dedup_media, ExtractError, Extractor, MediaRequirements,
    MetaMediaCollection, Post,
};
use async_trait::async_trait;
use scraper::{Html, Selector};
use serde_json::Value;
use std::time::Duration;

const MAX_ATTEMPTS: u32 = 3;

pub struct ThreadsScraper {
    http: reqwest::Client,
    user_agent: String,
    sec_ch_ua: String,
}

impl ThreadsScraper {
    pub fn new(http: reqwest::Client, user_agent: String, sec_ch_ua: String) -> Self {
        Self {
            http,
            user_agent,
            sec_ch_ua,
        }
    }

    /// Fetch the post page with a *coherent* desktop-browser header set — the
    /// load-bearing requirement (a mismatched/crawler UA returns the empty SPA
    /// shell). The UA + `sec-ch-ua` are hot-config (PLAN conventions).
    async fn fetch(&self, url: &str) -> Result<String, ExtractError> {
        let resp = self
            .http
            .get(url)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .header(
                reqwest::header::ACCEPT,
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .header("sec-ch-ua", &self.sec_ch_ua)
            .header("sec-ch-ua-mobile", "?0")
            .header("sec-ch-ua-platform", "\"Windows\"")
            .header("sec-fetch-dest", "document")
            .header("sec-fetch-mode", "navigate")
            .header("sec-fetch-site", "none")
            .header("upgrade-insecure-requests", "1")
            .send()
            .await
            .map_err(|e| ExtractError::Transient(format!("threads net: {e}")))?;

        let status = resp.status();
        if status.is_success() {
            let body = resp
                .text()
                .await
                .map_err(|e| ExtractError::Transient(format!("threads body: {e}")))?;
            tracing::debug!(
                status = status.as_u16(),
                bytes = body.len(),
                "threads fetch ok"
            );
            Ok(body)
        } else {
            tracing::debug!(status = status.as_u16(), "threads fetch non-success");
            Err(super::threads_status(status.as_u16()))
        }
    }
}

#[async_trait]
impl Extractor for ThreadsScraper {
    fn name(&self) -> &'static str {
        "threads-json"
    }

    async fn extract(&self, url: &str, shortcode: &str) -> Result<Post, ExtractError> {
        // Retry an empty shell, but do not mistake missing data for a login wall.
        let mut last = ExtractError::Transient("threads: no data".into());
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(300 * (1 << attempt))).await;
            }
            match self.fetch(url).await {
                Ok(html) => match parse_threads_post(&html, shortcode, url) {
                    ParseOutcome::Post(post) => return Ok(post),
                    ParseOutcome::LoginWalled => return Err(ExtractError::Restricted),
                    ParseOutcome::Incomplete(required) => {
                        return Err(ExtractError::IncompleteMedia(required))
                    }
                    ParseOutcome::EmptyShell => {
                        tracing::debug!(shortcode, attempt, "threads empty shell, retrying");
                        last = ExtractError::Unavailable(
                            "Threads returned no recognized post data".into(),
                        );
                    }
                },
                Err(ExtractError::RateLimited) => {
                    last = ExtractError::RateLimited;
                    continue;
                }
                Err(ExtractError::Transient(e)) => {
                    last = ExtractError::Transient(e);
                    continue;
                }
                Err(other) => return Err(other),
            }
        }
        Err(last)
    }
}

/// Classified result of parsing a fetched Threads page.
#[derive(Debug)]
enum ParseOutcome {
    /// Found the post (media and/or text).
    Post(Post),
    /// HTTP 200 but no server-rendered post data — the header gate; retry.
    EmptyShell,
    /// The page is a login / private wall.
    LoginWalled,
    /// A target exists but known attachments could not be resolved.
    Incomplete(MediaRequirements),
}

/// Inspect each JSON script once, independently of Relay operation/envelope names.
/// Recommendations are not evidence that the requested post was removed.
fn parse_threads_post(html: &str, shortcode: &str, original_url: &str) -> ParseOutcome {
    let doc = Html::parse_document(html);
    let Ok(sel) = Selector::parse(r#"script[type="application/json"]"#) else {
        return ParseOutcome::EmptyShell;
    };
    let mut posts: Vec<((usize, usize), Post)> = Vec::new();
    let mut incomplete = false;
    let mut required = MediaRequirements::default();
    let mut parsed_scripts = 0;
    let mut targets = 0;
    for script in doc.select(&sel) {
        let text = script.text().collect::<String>();
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        parsed_scripts += 1;
        let mut candidates = Vec::new();
        find_thread_posts(&value, shortcode, &mut candidates);
        targets += candidates.len();
        for node in candidates {
            match build_post(node, original_url) {
                Ok(Some(post)) => {
                    // A full Polaris record outranks a caption/user-only reference.
                    // Never merge candidates: that can duplicate or mix attachments.
                    let score = candidate_score(node, &post);
                    posts.push((score, post));
                }
                Err(missing) => {
                    incomplete = true;
                    required.merge(missing);
                }
                Ok(None) => {}
            }
        }
    }
    tracing::debug!(
        parsed_scripts,
        targets,
        incomplete,
        "threads JSON source shape"
    );
    posts.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    for (_, post) in posts {
        // An incomplete full record must not be hidden by a text-only stub.
        if required.is_satisfied_by(&post.media) {
            return ParseOutcome::Post(post);
        }
    }
    if looks_login_walled(html) {
        ParseOutcome::LoginWalled
    } else if incomplete {
        ParseOutcome::Incomplete(required)
    } else {
        ParseOutcome::EmptyShell
    }
}

fn candidate_score(node: &Value, post: &Post) -> (usize, usize) {
    // Actual resolved attachment evidence outranks caption-only projections,
    // even if the projection happens to include a media_type discriminator.
    let structure = usize::from(node.get("media_type").and_then(Value::as_u64).is_some())
        + usize::from(author_of(node).is_some())
        + usize::from(caption_of(node).is_some())
        + usize::from(node.get("text_post_app_info").is_some());
    (post.media.len(), structure)
}

/// Exact-code discovery stops at a post boundary. Embedded originals are only
/// interpreted by build_post, never promoted to standalone target candidates.
fn find_thread_posts<'a>(v: &'a Value, shortcode: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(map) => {
            if map.get("code").and_then(Value::as_str) == Some(shortcode)
                && [
                    "pk",
                    "caption",
                    "user",
                    "media_type",
                    "image_versions2",
                    "carousel_media",
                    "video_versions",
                ]
                .iter()
                .any(|key| map.contains_key(*key))
            {
                out.push(v);
                return;
            }
            for (key, child) in map {
                if !matches!(
                    key.as_str(),
                    "quoted_post" | "reposted_post" | "linked_inline_media"
                ) {
                    find_thread_posts(child, shortcode, out);
                }
            }
        }
        Value::Array(arr) => {
            for child in arr {
                find_thread_posts(child, shortcode, out);
            }
        }
        _ => {}
    }
}

/// Media of a Threads post node: its own media plus any Instagram post shared
/// inline (`text_post_app_info/linked_inline_media` — a full Polaris media
/// node riding along; observed live on share-to-Threads posts, where
/// `share_info.quoted_post`/`reposted_post` stay null). Own media first.
fn collect_threads_media(node: &Value) -> MetaMediaCollection {
    let mut media = collect_meta_media_strict(node);
    let mut linked = node
        .pointer("/text_post_app_info/linked_inline_media")
        .filter(|n| n.is_object())
        .map(collect_meta_media_strict);
    if let Some(linked) = &mut linked {
        // An explicit linked-media stub still identifies an attachment.
        linked.required.total = linked.required.total.max(1);
    }
    if let Some(card) = node
        .pointer("/text_post_app_info/link_preview_attachment/url")
        .and_then(Value::as_str)
        .and_then(super::threads_embed::instagram_attachment_requirements)
    {
        let linked = linked.get_or_insert(MetaMediaCollection {
            media: Vec::new(),
            required: MediaRequirements::default(),
            incomplete: true,
        });
        // The card describes this same linked attachment, including when its
        // inline object is only an empty stub. Merge rather than count twice.
        linked.required.merge(card);
    }
    if let Some(linked) = linked {
        media.media.extend(linked.media);
        media.required.total += linked.required.total;
        media.required.images += linked.required.images;
        media.required.videos += linked.required.videos;
    }
    // URL dedup applies to evidence as well as delivery. Preserve only the
    // unresolved deficit in addition to distinct usable attachments.
    let before_images = media
        .media
        .iter()
        .filter(|m| m.kind == super::MediaKind::Image)
        .count();
    let before_videos = media.media.len() - before_images;
    let before_total = media.media.len();
    media.media = dedup_media(media.media);
    let after_images = media
        .media
        .iter()
        .filter(|m| m.kind == super::MediaKind::Image)
        .count();
    let after_videos = media.media.len() - after_images;
    media.required.total = media
        .required
        .total
        .saturating_sub(before_total - media.media.len());
    media.required.images = media
        .required
        .images
        .saturating_sub(before_images - after_images);
    media.required.videos = media
        .required
        .videos
        .saturating_sub(before_videos - after_videos);
    media.incomplete = !media.required.is_satisfied_by(&media.media);
    media
}

/// Build a [`Post`] from a matched node, resolving reposts/quotes (Threads
/// design: a pure repost → mirror the original; a quote-post → outer content +
/// a link/attribution to the inner post).
fn build_post(node: &Value, original_url: &str) -> Result<Option<Post>, MediaRequirements> {
    let collected = collect_threads_media(node);
    if collected.incomplete {
        return Err(collected.required);
    }
    let media = collected.media;
    let mut caption = caption_of(node);
    let author = author_of(node);
    let has_own = !media.is_empty() || caption.as_deref().is_some_and(|c| !c.trim().is_empty());

    if has_own {
        // Quote-post: outer has its own content AND embeds another post — append
        // an attribution line to the inner post (don't fetch its media).
        if let Some(note) = inner_post(node).and_then(quote_note) {
            caption = Some(match caption {
                Some(c) if !c.trim().is_empty() => format!("{c}\n\n{note}"),
                _ => note,
            });
        }
        return Ok(Some(Post {
            author,
            caption,
            media: dedup_media(media),
            original_url: original_url.to_string(),
        }));
    }

    // Pure repost: the outer post is empty — mirror the inner (original) post.
    if let Some(inner) = inner_post(node) {
        let collected = collect_threads_media(inner);
        if collected.incomplete {
            return Err(collected.required);
        }
        let inner_media = collected.media;
        let inner_caption = caption_of(inner);
        if !inner_media.is_empty()
            || inner_caption
                .as_deref()
                .is_some_and(|c| !c.trim().is_empty())
        {
            return Ok(Some(Post {
                author: author_of(inner),
                caption: inner_caption,
                media: dedup_media(inner_media),
                original_url: original_url.to_string(),
            }));
        }
    }
    Ok(None)
}

fn caption_of(node: &Value) -> Option<String> {
    node.pointer("/caption/text")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn author_of(node: &Value) -> Option<String> {
    node.pointer("/user/username")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The embedded quoted/reposted post, if any. Key path is best-effort and
/// flagged for live validation; absence simply means no inner post.
fn inner_post(node: &Value) -> Option<&Value> {
    [
        "/text_post_app_info/share_info/quoted_post",
        "/text_post_app_info/share_info/reposted_post",
        "/quoted_post",
        "/reposted_post",
    ]
    .into_iter()
    .find_map(|ptr| node.pointer(ptr).filter(|n| n.is_object()))
}

/// Attribution line for a quote-post's inner post: `🔁 Quoting @user: <url>`.
fn quote_note(inner: &Value) -> Option<String> {
    let user = author_of(inner)?;
    match inner.get("code").and_then(Value::as_str) {
        Some(code) => Some(format!(
            "🔁 Quoting @{user}: {}",
            crate::urls::threads_post_url(&user, code)
        )),
        None => Some(format!("🔁 Quoting @{user}")),
    }
}

pub(super) fn looks_login_walled(html: &str) -> bool {
    let doc = Html::parse_document(html);
    // Threads renders this audience wall client-side. Recognize the active
    // route payload observed on DdWjmkgDnI3, not arbitrary bundle strings.
    if let Ok(selector) = Selector::parse(r#"script[type="application/json"]"#) {
        for script in doc.select(&selector) {
            let text = script.text().collect::<String>();
            if !text.contains("initialRouteInfo") {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                if has_restricted_route(&value) {
                    return true;
                }
            }
        }
    }
    // Also inspect visible page text, never JS string tables or metadata.
    let visible = doc
        .tree
        .nodes()
        .filter_map(|node| {
            let text = node.value().as_text()?;
            if node
                .ancestors()
                .filter_map(scraper::ElementRef::wrap)
                .any(|e| {
                    matches!(e.value().name(), "script" | "style" | "template" | "head")
                        || e.value().attr("hidden").is_some()
                        || e.value().attr("aria-hidden") == Some("true")
                })
            {
                return None;
            }
            Some(text.text.as_ref())
        })
        .collect::<Vec<&str>>()
        .join(" ");
    let text = visible
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    text.contains("log in to see")
        || text.contains("this account is private")
        || text.contains("this content isn't available to everyone")
        || text.contains("this content isn’t available to everyone")
        || text.contains("it can't be seen by certain audiences")
        || text.contains("it can’t be seen by certain audiences")
}

fn has_restricted_route(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            if let Some(route) = value.pointer("/initialRouteInfo/route") {
                if route.get("tracePolicy").and_then(Value::as_str)
                    == Some("barcelona.geoBlockPage")
                    && route
                        .pointer("/rootView/resource/__dr")
                        .and_then(Value::as_str)
                        == Some("BarcelonaGeoBlockedErrorRoot.react")
                {
                    return true;
                }
            }
            map.values().any(has_restricted_route)
        }
        Value::Array(values) => values.iter().any(has_restricted_route),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::MediaKind;

    fn post(html: &str, shortcode: &str) -> Option<Post> {
        match parse_threads_post(html, shortcode, "https://www.threads.com/@u/post/SC") {
            ParseOutcome::Post(p) => Some(p),
            _ => None,
        }
    }

    fn script(json: &str) -> String {
        format!(r#"<html><body><script type="application/json">{json}</script></body></html>"#)
    }

    fn data_media(node: Value) -> String {
        script(
            &serde_json::json!({"require":[{"__bbox":{"result":{"data":{"media":node}}}}]})
                .to_string(),
        )
    }

    #[test]
    fn captured_new_envelope_ignores_separate_recommendations() {
        let html = include_str!("../../tests/fixtures/threads_json_data_media.html");
        let p = post(html, "SC").unwrap();
        assert_eq!(p.author.as_deref(), Some("example.author"));
        assert_eq!(p.media.len(), 1);
        assert_eq!(p.media[0].kind, MediaKind::Video);
        assert_eq!(
            p.media[0].url,
            "https://scontent.cdninstagram.com/v/linked.mp4?efg=example&oe=EXAMPLE&x=1%2B2"
        );
    }

    #[test]
    fn new_envelope_native_and_linked_media_families() {
        let image = serde_json::json!({"media_type":1,"image_versions2":{"candidates":[{"url":"https://scontent.cdninstagram.com/v/photo.jpg?a=1&amp;b=2"}]}});
        let video = serde_json::json!({"media_type":2,"video_versions":[{"url":"https://scontent.cdninstagram.com/v/video.mp4"}]});
        let carousel =
            serde_json::json!({"media_type":8,"carousel_media":[image.clone(),video.clone()]});
        for (attachment, kinds) in [
            (image, vec![MediaKind::Image]),
            (video, vec![MediaKind::Video]),
            (carousel, vec![MediaKind::Image, MediaKind::Video]),
        ] {
            for linked in [false, true] {
                let mut node = serde_json::json!({"code":"SC","media_type":19,"caption":{"text":"outer text"},"user":{"username":"outer"}});
                if linked {
                    node["text_post_app_info"] =
                        serde_json::json!({"linked_inline_media":attachment});
                } else {
                    for (key, value) in attachment.as_object().unwrap() {
                        node[key] = value.clone();
                    }
                }
                let p = post(&data_media(node), "SC").unwrap();
                assert_eq!(p.media.iter().map(|m| m.kind).collect::<Vec<_>>(), kinds);
                assert_eq!(p.author.as_deref(), Some("outer"));
                assert_eq!(p.caption.as_deref(), Some("outer text"));
                if p.media[0].kind == MediaKind::Image {
                    assert!(p.media[0].url.ends_with("?a=1&b=2"));
                }
            }
        }
        let p = post(
            &data_media(
                serde_json::json!({"code":"SC","media_type":19,"caption":{"text":"text only"}}),
            ),
            "SC",
        )
        .unwrap();
        assert!(p.media.is_empty());
    }

    #[test]
    fn complete_target_beats_duplicate_stubs_across_and_within_scripts() {
        let stub =
            serde_json::json!({"code":"SC","caption":{"text":"stub"},"user":{"username":"u"}});
        let full = serde_json::json!({"code":"SC","media_type":2,"caption":{"text":"complete"},"video_versions":[{"url":"https://scontent.cdninstagram.com/v/full.mp4"}]});
        for html in [
            format!("{}{}", data_media(stub.clone()), data_media(full.clone())),
            script(&serde_json::json!({"records":[stub,full]}).to_string()),
        ] {
            let p = post(&html, "SC").unwrap();
            assert_eq!(p.caption.as_deref(), Some("complete"));
            assert_eq!(p.media[0].kind, MediaKind::Video);
        }
    }

    #[test]
    fn actual_attachment_evidence_beats_typed_text_and_smaller_projections() {
        let video = serde_json::json!({"code":"SC","video_versions":[{"url":"https://scontent.cdninstagram.com/v/real.mp4"}]});
        for first in [
            serde_json::json!({"code":"SC","media_type":19,"caption":{"text":"typed stub"}}),
            serde_json::json!({"code":"SC","media_type":2,"video_versions":[]}),
        ] {
            let html = format!("{}{}", data_media(first), data_media(video.clone()));
            assert_eq!(post(&html, "SC").unwrap().media[0].kind, MediaKind::Video);
        }
        let child = serde_json::json!({"image_versions2":{"candidates":[{"url":"https://scontent.cdninstagram.com/v/one.jpg"}]}});
        let small =
            serde_json::json!({"code":"SC","media_type":8,"carousel_media":[child.clone()]});
        let large = serde_json::json!({"code":"SC","media_type":8,"carousel_media":[child,{"video_versions":[{"url":"https://scontent.cdninstagram.com/v/two.mp4"}]}]});
        let html = format!("{}{}", data_media(small), data_media(large));
        assert_eq!(post(&html, "SC").unwrap().media.len(), 2);
    }

    #[test]
    fn instagram_preview_without_resolved_inline_node_is_incomplete() {
        for inline in [
            Value::Null,
            serde_json::json!({}),
            serde_json::json!({"code":"IG","caption":{"text":"not media"}}),
        ] {
            for path in ["reel/Clip", "p/Photo"] {
                let node = serde_json::json!({"code":"SC","media_type":19,"caption":{"text":"share"},"text_post_app_info":{"linked_inline_media":inline,"link_preview_attachment":{"url":format!("https://www.instagram.com/{path}/")}}});
                match parse_threads_post(&data_media(node), "SC", "u") {
                    ParseOutcome::Incomplete(required) => {
                        assert_eq!(required.total, 1);
                        assert_eq!(required.videos, usize::from(path.starts_with("reel")));
                    }
                    other => panic!("expected unresolved IG preview, got {other:?}"),
                }
            }
        }
        let external = serde_json::json!({"code":"SC","media_type":19,"caption":{"text":"article"},"text_post_app_info":{"link_preview_attachment":{"url":"https://example.com/article"}}});
        assert!(post(&data_media(external), "SC").unwrap().media.is_empty());
        let complete = serde_json::json!({"code":"SC","media_type":19,"text_post_app_info":{"link_preview_attachment":{"url":"https://www.instagram.com/reel/Clip/"},"linked_inline_media":{"media_type":2,"video_versions":[{"url":"https://scontent.cdninstagram.com/v/clip.mp4"}]}}});
        assert_eq!(post(&data_media(complete), "SC").unwrap().media.len(), 1);
    }

    #[test]
    fn partial_target_cannot_hide_behind_text_stub_or_wrong_kind() {
        let partial = serde_json::json!({"code":"SC","media_type":2,"caption":{"text":"video unavailable"},"video_versions":[]});
        for candidate in [
            serde_json::json!({"code":"SC","caption":{"text":"stub"}}),
            serde_json::json!({"code":"SC","media_type":1,"image_versions2":{"candidates":[{"url":"https://scontent.cdninstagram.com/v/photo.jpg"}]}}),
        ] {
            let html = format!("{}{}", data_media(candidate), data_media(partial.clone()));
            assert!(matches!(
                parse_threads_post(&html, "SC", "u"),
                ParseOutcome::Incomplete(_)
            ));
        }
    }

    #[test]
    fn malformed_missing_and_nested_target_data_never_claim_deletion() {
        for html in [
            script("{invalid thread_items"),
            data_media(serde_json::json!({"code":"SC"})),
            data_media(
                serde_json::json!({"code":"OTHER","text_post_app_info":{"share_info":{"quoted_post":{"code":"SC","caption":{"text":"inner"}}}}}),
            ),
        ] {
            assert!(matches!(
                parse_threads_post(&html, "SC", "u"),
                ParseOutcome::EmptyShell
            ));
        }
    }

    #[test]
    fn new_envelope_reposts_and_quotes_preserve_policy() {
        let inner = serde_json::json!({"code":"INNER","media_type":19,"caption":{"text":"original"},"user":{"username":"original"},"text_post_app_info":{"linked_inline_media":{"media_type":2,"video_versions":[{"url":"https://scontent.cdninstagram.com/v/original.mp4"}]}}});
        let repost = serde_json::json!({"code":"SC","user":{"username":"reposter"},"text_post_app_info":{"share_info":{"reposted_post":inner.clone()}}});
        let p = post(&data_media(repost), "SC").unwrap();
        assert_eq!(p.author.as_deref(), Some("original"));
        assert_eq!(p.media[0].kind, MediaKind::Video);
        let quote = serde_json::json!({"code":"SC","media_type":19,"caption":{"text":"opinion"},"user":{"username":"quoter"},"text_post_app_info":{"share_info":{"quoted_post":inner}}});
        let p = post(&data_media(quote), "SC").unwrap();
        assert!(p.media.is_empty());
        assert!(p.caption.unwrap().contains("Quoting @original"));
    }

    #[tokio::test]
    #[ignore = "LIVE: public Threads network probe; no cookies or Telegram sends"]
    async fn live_reported_share_and_reference_posts_use_production_chain() {
        let mut cfg = crate::config::Config::test_default();
        cfg.threads_user_agent = std::env::var("THREADS_USER_AGENT").unwrap_or_else(|_| "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36".into());
        cfg.threads_sec_ch_ua = std::env::var("THREADS_SEC_CH_UA").unwrap_or_else(|_| {
            r#""Not/A)Brand";v="8", "Chromium";v="126", "Google Chrome";v="126""#.into()
        });
        let (http, share_http) = crate::threads_http::clients(None).unwrap();
        let resolver = crate::threads_share::ThreadsShareResolver::with_client(
            share_http,
            cfg.threads_user_agent.clone(),
            cfg.threads_sec_ch_ua.clone(),
        );
        let target = resolver
            .resolve("https://www.threads.com/share/BBtSjKGHI4/")
            .await
            .unwrap();
        assert_eq!(target.shortcode, "DemLs3Xk0VO");
        let chain = super::super::build_threads_chain(&cfg, http);
        for (url, code, author, kind) in [
            (
                target.canonical_url.as_str(),
                target.shortcode.as_str(),
                "meow.meow.journey",
                MediaKind::Video,
            ),
            (
                "https://www.threads.com/@dr.kyle_modern_tcm/post/Daxu9_bnYC9",
                "Daxu9_bnYC9",
                "dr.kyle_modern_tcm",
                MediaKind::Video,
            ),
            (
                "https://www.threads.com/@feel_pale/post/DdVO0fGCX-j",
                "DdVO0fGCX-j",
                "feel_pale",
                MediaKind::Image,
            ),
        ] {
            let p = chain.extract(url, code).await.unwrap();
            assert_eq!(p.author.as_deref(), Some(author));
            assert_eq!(p.media.len(), 1, "exactly the post attachment, no icon or poster");
            assert_eq!(p.media[0].kind, kind);
            assert!(p.media.iter().all(|m| !m.url.contains("/rsrc.php/")));
            eprintln!(
                "LIVE target={code} author={author} attachments={} first_kind={kind:?}",
                p.media.len()
            );
        }
    }

    #[test]
    fn own_and_linked_duplicate_url_remains_one_attachment() {
        let node = serde_json::json!({"code":"SC","media_type":1,"image_versions2":{"candidates":[{"url":"https://scontent.cdninstagram.com/v/same.jpg"}]},"text_post_app_info":{"linked_inline_media":{"media_type":1,"image_versions2":{"candidates":[{"url":"https://scontent.cdninstagram.com/v/same.jpg"}]}}}});
        assert_eq!(post(&data_media(node), "SC").unwrap().media.len(), 1);
    }

    #[test]
    fn single_image_post() {
        let html = script(
            r#"{"require":[["ScheduledServerJS","handle",null,[{"__bbox":{"result":{"data":{"thread_items":[
              {"post":{"code":"SC","media_type":1,
                "image_versions2":{"candidates":[{"width":1080,"url":"https://scontent.cdninstagram.com/v/full.jpg"},{"width":320,"url":"https://scontent.cdninstagram.com/v/small.jpg"}]},
                "caption":{"text":"a thought with a pic"},"user":{"username":"zuck"}}}
            ]}}}}]]]}"#,
        );
        let p = post(&html, "SC").unwrap();
        assert_eq!(p.author.as_deref(), Some("zuck"));
        assert_eq!(p.caption.as_deref(), Some("a thought with a pic"));
        assert_eq!(p.media.len(), 1);
        assert_eq!(p.media[0].kind, MediaKind::Image);
        assert!(p.media[0].url.ends_with("full.jpg"), "largest candidate");
    }

    #[test]
    fn video_post_prefers_playable_video() {
        let html = script(
            r#"{"thread_items":[{"post":{"code":"SC","media_type":2,
              "image_versions2":{"candidates":[{"url":"https://scontent.cdninstagram.com/v/poster.jpg"}]},
              "video_versions":[{"type":101,"url":"https:\/\/instagram.fyvr1-1.fna.fbcdn.net\/o1\/v\/t16\/clip.mp4?efg=abc&oe=DEAD"}],
              "caption":{"text":"clip"},"user":{"username":"mosseri"}}}]}"#,
        );
        let p = post(&html, "SC").unwrap();
        assert_eq!(p.media.len(), 1, "video only, not its poster");
        assert_eq!(p.media[0].kind, MediaKind::Video);
        // Escaped slashes decoded; signed query string kept verbatim.
        assert!(
            p.media[0].url.contains("clip.mp4?efg=abc&oe=DEAD"),
            "url={}",
            p.media[0].url
        );
    }

    #[test]
    fn carousel_mixed_image_and_video() {
        let html = script(
            r#"{"thread_items":[{"post":{"code":"SC","media_type":8,"carousel_media":[
                {"image_versions2":{"candidates":[{"url":"https://scontent.cdninstagram.com/v/c1.jpg"}]}},
                {"media_type":2,
                 "image_versions2":{"candidates":[{"url":"https://scontent.cdninstagram.com/v/poster2.jpg"}]},
                 "video_versions":[{"type":101,"url":"https://scontent.cdninstagram.com/v/c2.mp4"}]}
            ],"caption":{"text":"carousel"},"user":{"username":"u"}}}]}"#,
        );
        let p = post(&html, "SC").unwrap();
        assert_eq!(p.media.len(), 2);
        assert_eq!(p.media[0].kind, MediaKind::Image);
        assert!(p.media[0].url.ends_with("c1.jpg"));
        assert_eq!(p.media[1].kind, MediaKind::Video, "video child, not poster");
        assert!(p.media[1].url.ends_with("c2.mp4"));
    }

    #[test]
    fn text_only_post_yields_caption_no_media() {
        let html = script(
            r#"{"thread_items":[{"post":{"code":"SC","media_type":19,
              "caption":{"text":"just text, no media here"},"user":{"username":"someone"}}}]}"#,
        );
        let p = post(&html, "SC").unwrap();
        assert!(p.media.is_empty());
        assert_eq!(p.caption.as_deref(), Some("just text, no media here"));
        assert_eq!(p.author.as_deref(), Some("someone"));
    }

    #[test]
    fn picks_post_by_code_ignoring_other_posts() {
        // The page embeds the target (SC) and an unrelated post (OTHER).
        let html = script(
            r#"{"thread_items":[
              {"post":{"code":"OTHER","media_type":1,"image_versions2":{"candidates":[{"url":"https://scontent.cdninstagram.com/v/other.jpg"}]},"caption":{"text":"nope"},"user":{"username":"x"}}},
              {"post":{"code":"SC","media_type":1,"image_versions2":{"candidates":[{"url":"https://scontent.cdninstagram.com/v/mine.jpg"}]},"caption":{"text":"mine"},"user":{"username":"y"}}}
            ]}"#,
        );
        let p = post(&html, "SC").unwrap();
        assert_eq!(p.caption.as_deref(), Some("mine"));
        assert_eq!(p.media.len(), 1);
        assert!(p.media[0].url.ends_with("mine.jpg"));
    }

    #[test]
    fn pure_repost_resolves_to_original() {
        // Outer post has no caption/media; it reposts an inner post.
        let html = script(
            r#"{"thread_items":[{"post":{"code":"SC","user":{"username":"reposter"},
              "text_post_app_info":{"share_info":{"reposted_post":{
                 "code":"INNER","media_type":1,
                 "image_versions2":{"candidates":[{"url":"https://scontent.cdninstagram.com/v/orig.jpg"}]},
                 "caption":{"text":"original post"},"user":{"username":"author"}}}}}}]}"#,
        );
        let p = post(&html, "SC").unwrap();
        assert_eq!(
            p.author.as_deref(),
            Some("author"),
            "mirror the original author"
        );
        assert_eq!(p.caption.as_deref(), Some("original post"));
        assert_eq!(p.media.len(), 1);
        assert!(p.media[0].url.ends_with("orig.jpg"));
    }

    #[test]
    fn quote_post_keeps_outer_and_links_inner() {
        // Outer post has its own text AND quotes an inner post.
        let html = script(
            r#"{"thread_items":[{"post":{"code":"SC","media_type":19,
              "caption":{"text":"hot take"},"user":{"username":"quoter"},
              "text_post_app_info":{"share_info":{"quoted_post":{
                 "code":"INNER","caption":{"text":"the original"},"user":{"username":"author"}}}}}}]}"#,
        );
        let p = post(&html, "SC").unwrap();
        assert_eq!(p.author.as_deref(), Some("quoter"));
        let cap = p.caption.unwrap();
        assert!(cap.contains("hot take"), "outer text kept: {cap}");
        assert!(
            cap.contains("🔁 Quoting @author"),
            "inner attribution: {cap}"
        );
        assert!(
            cap.contains("https://www.threads.com/@author/post/INNER"),
            "inner link: {cap}"
        );
    }

    #[test]
    fn text_post_with_linked_instagram_video_sends_the_video() {
        // Observed live (2026-07): a share-to-Threads text post (media_type 19,
        // no own media) carries the attached IG post as a full Polaris node
        // under text_post_app_info.linked_inline_media, while the share_info
        // quote/repost members stay null.
        let html = script(
            r#"{"thread_items":[{"post":{"code":"SC","media_type":19,"pk":"1",
              "caption":{"text":"看世界盃這麼貴啊😂"},"user":{"username":"dr.kyle"},
              "image_versions2":null,"video_versions":null,"carousel_media":null,
              "text_post_app_info":{
                "share_info":{"quoted_post":null,"reposted_post":null},
                "linked_inline_media":{
                  "code":"INNERIG","media_type":2,"caption":null,
                  "image_versions2":{"candidates":[{"url":"https://instagram.fyvr2-1.fna.fbcdn.net/v/poster.jpg?a=1"}]},
                  "video_versions":[{"type":101,"url":"https://instagram.fyvr2-1.fna.fbcdn.net/o1/v/clip.mp4?efg=abc&oe=AA"}],
                  "user":{"username":"dr.kyle"}}}}}]}"#,
        );
        let p = post(&html, "SC").unwrap();
        assert_eq!(p.caption.as_deref(), Some("看世界盃這麼貴啊😂"));
        assert_eq!(p.author.as_deref(), Some("dr.kyle"));
        assert_eq!(
            p.media.len(),
            1,
            "linked IG video collected, not its poster"
        );
        assert_eq!(p.media[0].kind, MediaKind::Video);
        assert!(
            p.media[0].url.contains("clip.mp4?efg=abc&oe=AA"),
            "url={}",
            p.media[0].url
        );
    }

    #[test]
    fn own_media_precedes_linked_media() {
        let html = script(
            r#"{"thread_items":[{"post":{"code":"SC","media_type":1,
              "image_versions2":{"candidates":[{"url":"https://scontent.cdninstagram.com/v/own.jpg"}]},
              "caption":{"text":"mine plus a share"},"user":{"username":"u"},
              "text_post_app_info":{"linked_inline_media":{
                "code":"L","media_type":2,
                "image_versions2":{"candidates":[{"url":"https://scontent.cdninstagram.com/v/lp.jpg"}]},
                "video_versions":[{"type":101,"url":"https://scontent.cdninstagram.com/v/linked.mp4"}]}}}}]}"#,
        );
        let p = post(&html, "SC").unwrap();
        assert_eq!(p.media.len(), 2);
        assert!(p.media[0].url.ends_with("own.jpg"), "own media first");
        assert_eq!(p.media[1].kind, MediaKind::Video);
        assert!(p.media[1].url.ends_with("linked.mp4"));
    }

    #[test]
    fn repost_of_post_with_linked_media_mirrors_it() {
        // A pure repost whose original carries an inline-shared IG video.
        let html = script(
            r#"{"thread_items":[{"post":{"code":"SC","user":{"username":"reposter"},
              "text_post_app_info":{"share_info":{"reposted_post":{
                "code":"INNER","media_type":19,"caption":{"text":"orig"},"user":{"username":"author"},
                "text_post_app_info":{"linked_inline_media":{
                  "code":"L","media_type":2,
                  "video_versions":[{"type":101,"url":"https://scontent.cdninstagram.com/v/in.mp4"}]}}}}}}}]}"#,
        );
        let p = post(&html, "SC").unwrap();
        assert_eq!(p.author.as_deref(), Some("author"));
        assert_eq!(p.caption.as_deref(), Some("orig"));
        assert_eq!(p.media.len(), 1);
        assert!(p.media[0].url.ends_with("in.mp4"));
    }

    #[test]
    fn rejects_non_cdn_media() {
        // A media URL on a non-Meta host must be dropped (no SSRF/exfil).
        let html = script(
            r#"{"thread_items":[{"post":{"code":"SC","media_type":1,
              "image_versions2":{"candidates":[{"url":"https://evil.example/x.jpg"}]},
              "caption":{"text":"text"},"user":{"username":"u"}}}]}"#,
        );
        assert!(matches!(
            parse_threads_post(&html, "SC", "u"),
            ParseOutcome::Incomplete(_)
        ));
    }

    #[test]
    fn empty_shell_is_classified_not_success() {
        let html =
            "<html><body><script type=\"application/json\">{\"config\":{}}</script></body></html>";
        assert!(matches!(
            parse_threads_post(html, "SC", "u"),
            ParseOutcome::EmptyShell
        ));
    }

    #[test]
    fn unrelated_data_is_not_proof_of_deletion() {
        let html = script(
            r#"{"thread_items":[{"post":{"code":"OTHER","caption":{"text":"x"},"user":{"username":"u"}}}]}"#,
        );
        assert!(matches!(
            parse_threads_post(&html, "SC", "u"),
            ParseOutcome::EmptyShell
        ));
    }

    #[test]
    fn captured_audience_route_is_restricted_without_visible_html() {
        // Reduced public payload from the reported BARWV10v9i share's target.
        let html = script(
            r#"{"require":[{"initialRouteInfo":{"route":{
            "rootView":{"resource":{"__dr":"BarcelonaGeoBlockedErrorRoot.react"},
            "props":{"title":"This content isn't available to everyone","description":"It can't be seen by certain audiences."}},
            "tracePolicy":"barcelona.geoBlockPage"}}}]}"#,
        );
        assert!(matches!(
            parse_threads_post(&html, "SC", "u"),
            ParseOutcome::LoginWalled
        ));
        let unused = script(
            r#"{"resources":["BarcelonaGeoBlockedErrorRoot.react"],"strings":["This content isn't available to everyone"]}"#,
        );
        assert!(matches!(
            parse_threads_post(&unused, "SC", "u"),
            ParseOutcome::EmptyShell
        ));
    }

    #[test]
    fn audience_wall_uses_visible_text_only() {
        let wall = "<h1>This content isn't available to everyone</h1><p>It can't be seen by certain audiences. See Why</p>";
        assert!(matches!(
            parse_threads_post(wall, "SC", "u"),
            ParseOutcome::LoginWalled
        ));
        let script_only =
            format!("<script>{wall}</script><template>{wall}</template><div hidden>{wall}</div>");
        assert!(matches!(
            parse_threads_post(&script_only, "SC", "u"),
            ParseOutcome::EmptyShell
        ));
        assert!(!looks_login_walled(
            "<p>Sorry, this page isn't available</p>"
        ));
    }

    #[test]
    fn available_post_takes_precedence_over_wall_phrases() {
        let html = script(
            r#"{"thread_items":[{"post":{"code":"SC","caption":{"text":"log in to see is just a phrase"},"user":{"username":"u"}}}]}"#,
        );
        let html = format!("{html}<p>Log in to see more posts</p>");
        assert!(post(&html, "SC").is_some());
    }

    #[test]
    fn login_wall_is_blocked() {
        let html = "<html><body><div>Log in to see this thread</div></body></html>";
        assert!(matches!(
            parse_threads_post(html, "SC", "u"),
            ParseOutcome::LoginWalled
        ));
    }
}
