//! PRIMARY Threads backend: in-process scrape of the public post page (Threads
//! design). Anonymous, no subprocess. Threads serves logged-out clients the full
//! private-API JSON server-side inside `<script type="application/json">`
//! (`data-sjs`) blocks — BUT only when sent a coherent desktop-browser header
//! set; a crawler/naive UA gets an empty SPA shell (HTTP 200, zero
//! `thread_items`), which we classify as a failure, never silent success.
//!
//! The JSON reuses Instagram's Polaris/Barcelona shape (`code`, `media_type`,
//! `image_versions2`, `video_versions`, `carousel_media`, `caption.text`,
//! `user.username`), so media collection is shared via [`collect_meta_media`].
//!
//! NB: the exact nesting for quote/repost (`text_post_app_info.share_info.*`) is
//! best-effort and flagged for live validation; it degrades to the outer post.

use super::{collect_meta_media, dedup_media, map_status, ExtractError, Extractor, Post};
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
        Self { http, user_agent, sec_ch_ua }
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
            tracing::debug!(status = status.as_u16(), bytes = body.len(), "threads fetch ok");
            Ok(body)
        } else {
            tracing::debug!(status = status.as_u16(), "threads fetch non-success");
            Err(map_status(status.as_u16()))
        }
    }
}

#[async_trait]
impl Extractor for ThreadsScraper {
    fn name(&self) -> &'static str {
        "threads-json"
    }

    async fn extract(&self, url: &str, shortcode: &str) -> Result<Post, ExtractError> {
        // Default to the most-actionable failure we can infer; an empty shell
        // (the header gate) is treated as Blocked, not silent success.
        let mut last = ExtractError::Transient("threads: no data".into());
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(300 * (1 << attempt))).await;
            }
            match self.fetch(url).await {
                Ok(html) => match parse_threads_post(&html, shortcode, url) {
                    ParseOutcome::Post(post) => return Ok(post),
                    ParseOutcome::LoginWalled => return Err(ExtractError::Blocked),
                    ParseOutcome::NotFound => return Err(ExtractError::NotFound),
                    ParseOutcome::EmptyShell => {
                        tracing::debug!(shortcode, attempt, "threads empty shell, retrying");
                        last = ExtractError::Blocked;
                    }
                },
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
    /// Post data was present but our shortcode's post wasn't in it.
    NotFound,
}

/// Scan the `application/json` blobs for `thread_items`, locate the post whose
/// `code` matches `shortcode`, and build a [`Post`].
fn parse_threads_post(html: &str, shortcode: &str, original_url: &str) -> ParseOutcome {
    let doc = Html::parse_document(html);
    let Ok(sel) = Selector::parse(r#"script[type="application/json"]"#) else {
        return ParseOutcome::EmptyShell;
    };

    let mut saw_thread_items = false;
    for script in doc.select(&sel) {
        let text = script.text().collect::<String>();
        if !text.contains("thread_items") {
            continue;
        }
        saw_thread_items = true;
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Some(node) = find_thread_post(&value, shortcode) {
            if let Some(post) = build_post(node, original_url) {
                return ParseOutcome::Post(post);
            }
        }
    }

    if saw_thread_items {
        ParseOutcome::NotFound
    } else if looks_login_walled(html) {
        ParseOutcome::LoginWalled
    } else {
        ParseOutcome::EmptyShell
    }
}

/// Recursively locate the post object whose `code` equals `shortcode`. Lenient
/// (unlike the IG finder): a text-only Threads post carries no media keys, so we
/// match on `code` plus any "looks like a post" key.
fn find_thread_post<'a>(v: &'a Value, shortcode: &str) -> Option<&'a Value> {
    match v {
        Value::Object(map) => {
            if map.get("code").and_then(Value::as_str) == Some(shortcode)
                && (map.contains_key("pk")
                    || map.contains_key("caption")
                    || map.contains_key("user")
                    || map.contains_key("media_type")
                    || map.contains_key("image_versions2")
                    || map.contains_key("carousel_media")
                    || map.contains_key("video_versions"))
            {
                return Some(v);
            }
            map.values().find_map(|child| find_thread_post(child, shortcode))
        }
        Value::Array(arr) => arr.iter().find_map(|child| find_thread_post(child, shortcode)),
        _ => None,
    }
}

/// Build a [`Post`] from a matched node, resolving reposts/quotes (Threads
/// design: a pure repost → mirror the original; a quote-post → outer content +
/// a link/attribution to the inner post).
fn build_post(node: &Value, original_url: &str) -> Option<Post> {
    let media = collect_meta_media(node);
    let mut caption = caption_of(node);
    let author = author_of(node);
    let has_own =
        !media.is_empty() || caption.as_deref().is_some_and(|c| !c.trim().is_empty());

    if has_own {
        // Quote-post: outer has its own content AND embeds another post — append
        // an attribution line to the inner post (don't fetch its media).
        if let Some(note) = inner_post(node).and_then(quote_note) {
            caption = Some(match caption {
                Some(c) if !c.trim().is_empty() => format!("{c}\n\n{note}"),
                _ => note,
            });
        }
        return Some(Post {
            author,
            caption,
            media: dedup_media(media),
            original_url: original_url.to_string(),
        });
    }

    // Pure repost: the outer post is empty — mirror the inner (original) post.
    if let Some(inner) = inner_post(node) {
        let inner_media = collect_meta_media(inner);
        let inner_caption = caption_of(inner);
        if !inner_media.is_empty()
            || inner_caption.as_deref().is_some_and(|c| !c.trim().is_empty())
        {
            return Some(Post {
                author: author_of(inner),
                caption: inner_caption,
                media: dedup_media(inner_media),
                original_url: original_url.to_string(),
            });
        }
    }
    None
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

fn looks_login_walled(html: &str) -> bool {
    let l = html.to_ascii_lowercase();
    l.contains("log in to see")
        || l.contains("this account is private")
        || l.contains("sorry, this page isn't available")
        || l.contains("isn't available")
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
        assert!(p.media[0].url.contains("clip.mp4?efg=abc&oe=DEAD"), "url={}", p.media[0].url);
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
        assert_eq!(p.author.as_deref(), Some("author"), "mirror the original author");
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
        assert!(cap.contains("🔁 Quoting @author"), "inner attribution: {cap}");
        assert!(cap.contains("https://www.threads.com/@author/post/INNER"), "inner link: {cap}");
    }

    #[test]
    fn rejects_non_cdn_media() {
        // A media URL on a non-Meta host must be dropped (no SSRF/exfil).
        let html = script(
            r#"{"thread_items":[{"post":{"code":"SC","media_type":1,
              "image_versions2":{"candidates":[{"url":"https://evil.example/x.jpg"}]},
              "caption":{"text":"text"},"user":{"username":"u"}}}]}"#,
        );
        let p = post(&html, "SC").unwrap();
        assert!(p.media.is_empty(), "non-CDN media dropped");
        assert_eq!(p.caption.as_deref(), Some("text"), "still delivered as text-only");
    }

    #[test]
    fn empty_shell_is_classified_not_success() {
        let html = "<html><body><script type=\"application/json\">{\"config\":{}}</script></body></html>";
        assert!(matches!(
            parse_threads_post(html, "SC", "u"),
            ParseOutcome::EmptyShell
        ));
    }

    #[test]
    fn data_present_but_post_missing_is_notfound() {
        let html = script(r#"{"thread_items":[{"post":{"code":"OTHER","caption":{"text":"x"},"user":{"username":"u"}}}]}"#);
        assert!(matches!(
            parse_threads_post(&html, "SC", "u"),
            ParseOutcome::NotFound
        ));
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
