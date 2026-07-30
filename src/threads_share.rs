use crate::extract::{map_status, ExtractError};
use crate::urls::threads_post_url;
use reqwest::{redirect::Policy, StatusCode};
use std::collections::HashSet;
use std::time::Duration;
use url::Url;

const MAX_REDIRECTS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedThreadsPost {
    pub canonical_url: String,
    pub shortcode: String,
}

/// Resolve Threads `/share/<token>` aliases without allowing reqwest to follow
/// redirects automatically. Each Location is validated before the next request.
pub struct ThreadsShareResolver {
    http: reqwest::Client,
    sec_ch_ua: String,
}

impl ThreadsShareResolver {
    pub fn new(user_agent: String, sec_ch_ua: String) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(Policy::none())
            .user_agent(user_agent)
            .build()?;
        Ok(Self { http, sec_ch_ua })
    }

    pub async fn resolve(&self, share_url: &str) -> Result<ResolvedThreadsPost, ExtractError> {
        let mut state = RedirectState::new(share_url)?;
        loop {
            let response = self
                .http
                .get(state.request_url())
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
                .map_err(|e| ExtractError::Transient(format!("Threads share redirect: {e}")))?;
            let status = response.status();
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            if let Some(resolved) = state.advance(status, location.as_deref())? {
                return Ok(resolved);
            }
        }
    }
}

struct RedirectState {
    current: Url,
    redirects: usize,
    seen: HashSet<String>,
}

impl RedirectState {
    fn new(share_url: &str) -> Result<Self, ExtractError> {
        let current = Url::parse(share_url)
            .map_err(|_| ExtractError::Unavailable("invalid Threads share URL".into()))?;
        if !is_share_target(&current) {
            return Err(ExtractError::Unavailable(
                "invalid Threads share URL".into(),
            ));
        }
        let seen = HashSet::from([current.as_str().to_string()]);
        Ok(Self {
            current,
            redirects: 0,
            seen,
        })
    }

    fn request_url(&self) -> &str {
        self.current.as_str()
    }

    fn advance(
        &mut self,
        status: StatusCode,
        location: Option<&str>,
    ) -> Result<Option<ResolvedThreadsPost>, ExtractError> {
        if !status.is_redirection() {
            return Err(map_status(status.as_u16()));
        }
        let location = location.ok_or_else(|| {
            ExtractError::Transient("Threads share redirect missing Location".into())
        })?;
        let target = self
            .current
            .join(location)
            .map_err(|_| ExtractError::Transient("invalid Threads share redirect".into()))?;
        if self.redirects >= MAX_REDIRECTS {
            return Err(ExtractError::Transient(
                "Threads share exceeded redirect limit".into(),
            ));
        }
        self.redirects += 1;
        if !self.seen.insert(target.as_str().to_string()) {
            return Err(ExtractError::Transient(
                "Threads share redirect loop".into(),
            ));
        }
        if let Some(post) = parse_post_target(&target) {
            return Ok(Some(post));
        }
        if is_share_target(&target) {
            self.current = target;
            return Ok(None);
        }
        Err(ExtractError::Transient(
            "Threads share did not redirect to a post".into(),
        ))
    }
}

fn parse_post_target(target: &Url) -> Option<ResolvedThreadsPost> {
    if !is_safe_threads_url(target) {
        return None;
    }
    let segments: Vec<_> = target
        .path_segments()?
        .filter(|part| !part.is_empty())
        .collect();
    if segments.len() != 3 || !segments[0].starts_with('@') || segments[1] != "post" {
        return None;
    }
    let username = &segments[0][1..];
    let shortcode = segments[2];
    if username.is_empty()
        || !username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'))
        || shortcode.is_empty()
        || !shortcode
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        return None;
    }
    Some(ResolvedThreadsPost {
        canonical_url: threads_post_url(username, shortcode),
        shortcode: shortcode.to_string(),
    })
}

fn is_threads_host(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "threads.com" | "www.threads.com" | "threads.net" | "www.threads.net"
    )
}

fn is_safe_threads_url(target: &Url) -> bool {
    target.scheme() == "https"
        && target.host_str().is_some_and(is_threads_host)
        && target.port_or_known_default() == Some(443)
        && target.username().is_empty()
        && target.password().is_none()
}

fn is_share_target(target: &Url) -> bool {
    if !is_safe_threads_url(target) {
        return false;
    }
    let Some(segments) = target.path_segments() else {
        return false;
    };
    let segments: Vec<_> = segments.filter(|part| !part.is_empty()).collect();
    segments.len() == 2
        && segments[0] == "share"
        && !segments[1].is_empty()
        && segments[1]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    const TEST_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
    const TEST_SEC_CH_UA: &str =
        "\"Not/A)Brand\";v=\"8\", \"Chromium\";v=\"126\", \"Google Chrome\";v=\"126\"";

    #[test]
    fn absolute_permalink_redirect_resolves_to_clean_post() {
        let mut state = RedirectState::new("https://www.threads.com/share/BAaEtHuRGL/")
            .expect("valid test share URL");
        let resolved = state
            .advance(
                StatusCode::FOUND,
                Some("https://www.threads.com/@ganbaruby2025/post/DbSFeVnAZ79?xmt=tracking&slof=1"),
            )
            .expect("valid redirect")
            .expect("permalink should finish resolution");

        assert_eq!(resolved.shortcode, "DbSFeVnAZ79");
        assert_eq!(
            resolved.canonical_url,
            "https://www.threads.com/@ganbaruby2025/post/DbSFeVnAZ79"
        );
    }

    #[test]
    fn legacy_domain_can_follow_an_intermediate_share_redirect() {
        let mut state = RedirectState::new("https://www.threads.net/share/BAaEtHuRGL/")
            .expect("valid test share URL");
        assert_eq!(
            state.request_url(),
            "https://www.threads.net/share/BAaEtHuRGL/"
        );
        assert_eq!(
            state
                .advance(
                    StatusCode::MOVED_PERMANENTLY,
                    Some("https://www.threads.com/share/BAaEtHuRGL/"),
                )
                .expect("allowed intermediate redirect"),
            None
        );
        assert_eq!(
            state.request_url(),
            "https://www.threads.com/share/BAaEtHuRGL/"
        );
        let resolved = state
            .advance(
                StatusCode::FOUND,
                Some("/@ganbaruby2025/post/DbSFeVnAZ79?xmt=tracking"),
            )
            .expect("relative permalink redirect")
            .expect("permalink should finish resolution");
        assert_eq!(resolved.shortcode, "DbSFeVnAZ79");
    }

    #[test]
    fn redirect_loop_is_rejected() {
        let mut state = RedirectState::new("https://www.threads.com/share/LOOP/")
            .expect("valid test share URL");
        let error = state
            .advance(
                StatusCode::FOUND,
                Some("https://www.threads.com/share/LOOP/"),
            )
            .expect_err("same URL must be rejected as a loop");
        assert!(matches!(error, ExtractError::Transient(_)));
    }

    #[test]
    fn relative_permalink_redirect_is_supported() {
        let mut state = RedirectState::new("https://www.threads.com/share/TOKEN/")
            .expect("valid test share URL");
        let resolved = state
            .advance(
                StatusCode::TEMPORARY_REDIRECT,
                Some("/@user.name/post/AbC_1-2/"),
            )
            .expect("relative redirect")
            .expect("post target");
        assert_eq!(
            resolved.canonical_url,
            "https://www.threads.com/@user.name/post/AbC_1-2"
        );
    }

    #[test]
    fn redirect_without_location_is_transient() {
        let mut state = RedirectState::new("https://www.threads.com/share/TOKEN/")
            .expect("valid test share URL");
        assert!(matches!(
            state.advance(StatusCode::FOUND, None),
            Err(ExtractError::Transient(_))
        ));
    }

    #[test]
    fn off_domain_and_http_redirects_are_rejected() {
        for target in [
            "https://evil.example/@user/post/CODE",
            "https://threads.com.evil.example/@user/post/CODE",
            "http://www.threads.com/@user/post/CODE",
            "https://www.threads.com:8443/@user/post/CODE",
            "https://attacker@www.threads.com/@user/post/CODE",
        ] {
            let mut state = RedirectState::new("https://www.threads.com/share/TOKEN/")
                .expect("valid test share URL");
            assert!(
                state.advance(StatusCode::FOUND, Some(target)).is_err(),
                "target={target}"
            );
        }
    }

    #[test]
    fn malformed_final_post_path_is_rejected() {
        for target in [
            "https://www.threads.com/@user/profile/CODE",
            "https://www.threads.com/@user/post/",
            "https://www.threads.com/@bad!user/post/CODE",
            "https://www.threads.com/@user/post/BAD.CODE",
        ] {
            let mut state = RedirectState::new("https://www.threads.com/share/TOKEN/")
                .expect("valid test share URL");
            assert!(
                state.advance(StatusCode::FOUND, Some(target)).is_err(),
                "target={target}"
            );
        }
    }

    #[test]
    fn not_found_status_keeps_existing_error_classification() {
        let mut state = RedirectState::new("https://www.threads.com/share/MISSING/")
            .expect("valid test share URL");
        assert!(matches!(
            state.advance(StatusCode::NOT_FOUND, None),
            Err(ExtractError::NotFound)
        ));
    }

    #[test]
    fn more_than_three_redirects_is_rejected() {
        let mut state =
            RedirectState::new("https://www.threads.com/share/ONE/").expect("valid test share URL");
        for token in ["TWO", "THREE", "FOUR"] {
            let next = format!("https://www.threads.com/share/{token}/");
            assert_eq!(
                state
                    .advance(StatusCode::FOUND, Some(&next))
                    .expect("hop allowed"),
                None
            );
        }
        assert!(matches!(
            state.advance(
                StatusCode::FOUND,
                Some("https://www.threads.com/@user/post/CODE"),
            ),
            Err(ExtractError::Transient(_))
        ));
    }

    #[tokio::test]
    #[ignore = "live Threads endpoint probe"]
    async fn live_sample_share_resolves_with_production_client() {
        let resolver = ThreadsShareResolver::new(TEST_UA.into(), TEST_SEC_CH_UA.into())
            .expect("build live test client");
        let resolved = resolver
            .resolve("https://www.threads.com/share/BAaEtHuRGL/")
            .await
            .expect("resolve supplied live share URL");
        assert_eq!(resolved.shortcode, "DbSFeVnAZ79");
        assert_eq!(
            resolved.canonical_url,
            "https://www.threads.com/@ganbaruby2025/post/DbSFeVnAZ79"
        );
    }
}
