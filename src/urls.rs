//! Post-link detection + canonicalization for Instagram and Threads (PLAN §5).
//!
//! Named `urls` (not `url`) to avoid clashing with the `url` crate.
//!
//! Routing keys on [`Platform`] (parsed from the link host), never on the
//! shortcode — Instagram and Threads share the same base64url shortcode
//! alphabet, so a bare code is not self-describing and could collide.

use regex::Regex;
use std::sync::LazyLock;

/// Which platform a detected link belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Instagram,
    Threads,
}

impl Platform {
    /// Short namespace for the dedup key, so an identical shortcode on both
    /// platforms doesn't falsely dedup across them.
    pub fn dedup_prefix(self) -> &'static str {
        match self {
            Platform::Instagram => "ig",
            Platform::Threads => "th",
        }
    }

    /// Namespaced dedup key for a shortcode (claimed on enqueue, forgotten on
    /// failure — see [`crate::dedup`]).
    pub fn dedup_key(self, shortcode: &str) -> String {
        format!("{}:{}", self.dedup_prefix(), shortcode)
    }

    /// Caption-header emoji for this platform.
    pub fn emoji(self) -> &'static str {
        match self {
            Platform::Instagram => "📷",
            Platform::Threads => "🧵",
        }
    }

    /// Human label for user-facing failure copy ("Instagram may be …").
    pub fn label(self) -> &'static str {
        match self {
            Platform::Instagram => "Instagram",
            Platform::Threads => "Threads",
        }
    }
}

/// A detected post link: the platform, the canonical URL the backends fetch and
/// the caption links to, and the shortcode (dedup id + JSON match key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedLink {
    pub platform: Platform,
    pub canonical_url: String,
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

/// Matches threads.com / threads.net post links `/@username/post/<CODE>`,
/// capturing the username (1) and the shortcode (2). Trailing `/embed`, query
/// (`?xmt=…`), and fragment stop at the shortcode class, so the canonical URL is
/// rebuilt clean (tracking params dropped) from the two captures.
static THREADS_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)https?://(?:www\.)?threads\.(?:com|net)/@([A-Za-z0-9_.]+)/post/([A-Za-z0-9_-]+)",
    )
    .expect("valid Threads url regex")
});

/// Find all Instagram + Threads post links in a blob of text, de-duplicated by
/// (platform, shortcode) — first occurrence wins. The handler feeds this the
/// message text plus any entity URLs (PLAN §5).
pub fn find_links(text: &str) -> Vec<DetectedLink> {
    let mut out: Vec<DetectedLink> = Vec::new();
    let mut push = |link: DetectedLink| {
        if link.shortcode.is_empty() {
            return;
        }
        if out
            .iter()
            .any(|l| l.platform == link.platform && l.shortcode == link.shortcode)
        {
            return;
        }
        out.push(link);
    };

    for caps in IG_URL_RE.captures_iter(text) {
        let shortcode = caps.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
        push(DetectedLink {
            platform: Platform::Instagram,
            canonical_url: post_url(&shortcode),
            shortcode,
        });
    }
    for caps in THREADS_URL_RE.captures_iter(text) {
        let user = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        let shortcode = caps.get(2).map(|m| m.as_str().to_string()).unwrap_or_default();
        push(DetectedLink {
            platform: Platform::Threads,
            canonical_url: threads_post_url(user, &shortcode),
            shortcode,
        });
    }
    out
}

/// Canonical Instagram post URL — fetched by the embed scraper (with a crawler
/// UA), yt-dlp, fallbacks, and shown in captions.
pub fn post_url(shortcode: &str) -> String {
    format!("https://www.instagram.com/p/{shortcode}/")
}

/// Canonical Threads post URL. The primary domain is `threads.com` (since
/// 2025-04-24); the `@username` is required to fetch the post, so it is kept,
/// while tracking params and any `/embed` suffix are dropped by rebuilding.
pub fn threads_post_url(username: &str, shortcode: &str) -> String {
    format!("https://www.threads.com/@{username}/post/{shortcode}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(text: &str) -> Vec<String> {
        find_links(text).into_iter().map(|l| l.shortcode).collect()
    }

    fn pairs(text: &str) -> Vec<(Platform, String)> {
        find_links(text).into_iter().map(|l| (l.platform, l.shortcode)).collect()
    }

    // --- Instagram detection (unchanged behavior) ---

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

    // --- Threads detection (new) ---

    #[test]
    fn threads_com_post() {
        let links = find_links("see https://www.threads.com/@zuck/post/C7UV9BwJ8qq nice");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].platform, Platform::Threads);
        assert_eq!(links[0].shortcode, "C7UV9BwJ8qq");
        assert_eq!(links[0].canonical_url, "https://www.threads.com/@zuck/post/C7UV9BwJ8qq");
    }

    #[test]
    fn threads_net_legacy_domain() {
        let links = find_links("https://threads.net/@user.name/post/DAELxBhOoWc");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].platform, Platform::Threads);
        assert_eq!(links[0].shortcode, "DAELxBhOoWc");
        // Canonicalized to the current primary domain regardless of input host.
        assert_eq!(links[0].canonical_url, "https://www.threads.com/@user.name/post/DAELxBhOoWc");
    }

    #[test]
    fn threads_strips_tracking_and_embed_suffix() {
        // /embed suffix and the ?xmt= share tracker must not leak into the code.
        let links = find_links("https://www.threads.com/@a/post/AbC-1_d/embed?xmt=TOKEN&igshid=z");
        assert_eq!(links[0].shortcode, "AbC-1_d");
        assert_eq!(links[0].canonical_url, "https://www.threads.com/@a/post/AbC-1_d");
    }

    #[test]
    fn threads_ignores_profile_and_intent_urls() {
        assert!(find_links("https://www.threads.com/@someone").is_empty());
        assert!(find_links("https://www.threads.com/intent/post").is_empty());
    }

    #[test]
    fn threads_dedup_same_shortcode() {
        let t = "https://threads.com/@u/post/SAME and https://www.threads.net/@u/post/SAME?xmt=1";
        assert_eq!(codes(t), vec!["SAME"]);
    }

    #[test]
    fn threads_builds_canonical_url() {
        assert_eq!(threads_post_url("nasa", "ABC"), "https://www.threads.com/@nasa/post/ABC");
    }

    // --- Cross-platform: same shortcode on both must NOT collide ---

    #[test]
    fn ig_and_threads_same_shortcode_are_distinct() {
        let t = "https://instagram.com/p/SAME/ and https://threads.com/@u/post/SAME";
        let got = pairs(t);
        assert_eq!(got.len(), 2);
        assert!(got.contains(&(Platform::Instagram, "SAME".to_string())));
        assert!(got.contains(&(Platform::Threads, "SAME".to_string())));
    }

    #[test]
    fn dedup_keys_are_namespaced_per_platform() {
        assert_eq!(Platform::Instagram.dedup_key("SAME"), "ig:SAME");
        assert_eq!(Platform::Threads.dedup_key("SAME"), "th:SAME");
        assert_ne!(Platform::Instagram.dedup_key("SAME"), Platform::Threads.dedup_key("SAME"));
    }
}
