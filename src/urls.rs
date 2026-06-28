//! Instagram URL detection + canonicalization (PLAN §5).
//!
//! Named `urls` (not `url`) to avoid clashing with the `url` crate.

use regex::Regex;
use std::sync::LazyLock;

/// A detected Instagram link: what the user posted plus its canonical shortcode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstagramLink {
    pub original_url: String,
    pub shortcode: String,
}

/// Matches instagram.com / instagr.am post / reel / tv links, with an optional
/// `username/` segment, capturing the shortcode. Trailing query/path is ignored.
static IG_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)https?://(?:www\.|m\.)?(?:instagram\.com|instagr\.am)/(?:[A-Za-z0-9_.]+/)?(?:p|reel|reels|tv)/([A-Za-z0-9_-]+)",
    )
    .expect("valid IG url regex")
});

/// Find all Instagram post/reel links in a blob of text, de-duplicated by
/// shortcode (first occurrence wins). The handler feeds this the message text
/// plus any entity URLs (PLAN §5).
pub fn find_instagram_links(text: &str) -> Vec<InstagramLink> {
    let mut out: Vec<InstagramLink> = Vec::new();
    for caps in IG_URL_RE.captures_iter(text) {
        let original_url = caps.get(0).map(|m| m.as_str().to_string()).unwrap_or_default();
        let shortcode = caps.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
        if shortcode.is_empty() {
            continue;
        }
        if out.iter().any(|l| l.shortcode == shortcode) {
            continue;
        }
        out.push(InstagramLink { original_url, shortcode });
    }
    out
}

/// Canonical public post URL — fetched by the embed scraper (with a crawler
/// UA), yt-dlp, fallbacks, and shown in captions.
pub fn post_url(shortcode: &str) -> String {
    format!("https://www.instagram.com/p/{shortcode}/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(text: &str) -> Vec<String> {
        find_instagram_links(text).into_iter().map(|l| l.shortcode).collect()
    }

    #[test]
    fn plain_post() {
        assert_eq!(codes("look https://www.instagram.com/p/ABC123/ nice"), vec!["ABC123"]);
    }

    #[test]
    fn reel_with_query() {
        assert_eq!(
            codes("https://instagram.com/reel/DS5FFFBjQ3l/?igsh=abc%3D%3D"),
            vec!["DS5FFFBjQ3l"]
        );
    }

    #[test]
    fn username_segment() {
        assert_eq!(codes("https://www.instagram.com/nasa/p/C-a6bDVy7hz/"), vec!["C-a6bDVy7hz"]);
    }

    #[test]
    fn tv_and_reels_variants() {
        assert_eq!(codes("https://instagram.com/tv/AbC_dE/"), vec!["AbC_dE"]);
        assert_eq!(codes("https://instagram.com/reels/XyZ-9/"), vec!["XyZ-9"]);
    }

    #[test]
    fn short_host_and_mobile() {
        assert_eq!(codes("http://instagr.am/p/Short1/"), vec!["Short1"]);
        assert_eq!(codes("https://m.instagram.com/p/Mob2/"), vec!["Mob2"]);
    }

    #[test]
    fn dedup_same_shortcode() {
        let t = "https://instagram.com/p/SAME/ and https://www.instagram.com/p/SAME/?x=1";
        assert_eq!(codes(t), vec!["SAME"]);
    }

    #[test]
    fn multiple_distinct() {
        let t = "https://instagram.com/p/AAA/ https://instagram.com/reel/BBB/";
        assert_eq!(codes(t), vec!["AAA", "BBB"]);
    }

    #[test]
    fn ignores_non_post_paths_and_other_hosts() {
        assert!(codes("https://www.instagram.com/nasa/").is_empty());
        assert!(codes("https://example.com/p/ABC/").is_empty());
        assert!(codes("just text, no link").is_empty());
    }

    #[test]
    fn builds_canonical_urls() {
        assert_eq!(post_url("ABC"), "https://www.instagram.com/p/ABC/");
    }
}
