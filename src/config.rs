//! Runtime configuration, loaded from the environment (see `.env.example`).
//!
//! Cookieless-first: `IG_COOKIES_PATH` and `FALLBACK_PROVIDER` are unset by
//! default, so gallery-dl and the external fallback stay inactive (PLAN §0).

use crate::error::AppError;
use std::path::PathBuf;
use std::time::Duration;

/// A realistic desktop-Chrome UA; hot-config via `USER_AGENT` (PLAN §4.4).
/// Used for CDN media downloads.
const DEFAULT_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// A link-preview crawler UA; hot-config via `EMBED_USER_AGENT`. Current
/// Instagram serves browser UAs a JS-only shell with no media, but still serves
/// crawler UAs the Open Graph tags + the Polaris JSON blob (full media). The
/// embed scraper fetches the post page with THIS UA (PLAN §4.1).
const DEFAULT_EMBED_UA: &str = "facebookexternalhit/1.1 \
(+http://www.facebook.com/externalhit_uatext.php)";

/// `sec-ch-ua` client-hint matching `DEFAULT_UA`'s Chrome major. Threads serves
/// its server-rendered post JSON only to a coherent browser header set, so this
/// pairs with the desktop UA; a mismatched/crawler UA gets the empty SPA shell.
const DEFAULT_THREADS_SEC_CH_UA: &str =
    "\"Not/A)Brand\";v=\"8\", \"Chromium\";v=\"126\", \"Google Chrome\";v=\"126\"";

const FIFTY_MIB: u64 = 50 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Config {
    pub bot_token: String,
    /// Empty => allow any chat (logged as a warning at startup).
    pub allowed_chats: Vec<i64>,
    pub temp_dir: PathBuf,
    pub cache_ttl: Duration,
    pub yt_dlp_path: String,
    pub gallery_dl_path: String,
    pub ig_cookies_path: Option<String>,
    /// e.g. "embedez" | "jina"; `None` keeps the external fallback disabled.
    pub fallback_provider: Option<String>,
    pub jina_api_key: Option<String>,
    pub user_agent: String,
    /// UA for the in-process embed/post-page scrape (crawler UA; see above).
    pub embed_user_agent: String,
    /// Threads support kill-switch (default on). Gates link detection/enqueue.
    pub threads_enabled: bool,
    /// UA for the in-process Threads scrape — must be a real desktop-browser UA.
    /// Threads gates its server-rendered post JSON on a coherent browser header
    /// set; a crawler UA returns an empty shell. Hot-config `THREADS_USER_AGENT`.
    pub threads_user_agent: String,
    /// `sec-ch-ua` client-hint sent with the Threads scrape; keep coherent with
    /// `threads_user_agent`. Hot-config `THREADS_SEC_CH_UA`.
    pub threads_sec_ch_ua: String,
    pub max_upload_bytes: u64,
    pub queue_capacity: usize,
    pub job_timeout: Duration,
    pub request_pacing: Duration,
    /// Periodic liveness/metrics log interval. `None` disables it
    /// (`HEARTBEAT_SECS=0`).
    pub heartbeat: Option<Duration>,
}

impl Config {
    pub fn from_env() -> Result<Self, AppError> {
        Ok(Self {
            bot_token: req("TELEGRAM_BOT_TOKEN")?,
            allowed_chats: match opt("ALLOWED_CHAT_IDS") {
                Some(s) => parse_chat_ids(&s)?,
                None => Vec::new(),
            },
            temp_dir: opt("TEMP_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::temp_dir().join("igbot")),
            cache_ttl: Duration::from_secs(parse_u64("CACHE_TTL_SECS", 1200)?),
            yt_dlp_path: opt("YT_DLP_PATH").unwrap_or_else(|| "yt-dlp".into()),
            gallery_dl_path: opt("GALLERY_DL_PATH").unwrap_or_else(|| "gallery-dl".into()),
            ig_cookies_path: opt("IG_COOKIES_PATH"),
            fallback_provider: opt("FALLBACK_PROVIDER").map(|s| s.to_lowercase()),
            jina_api_key: opt("JINA_API_KEY"),
            user_agent: opt("USER_AGENT").unwrap_or_else(|| DEFAULT_UA.into()),
            embed_user_agent: opt("EMBED_USER_AGENT").unwrap_or_else(|| DEFAULT_EMBED_UA.into()),
            threads_enabled: parse_bool("THREADS_ENABLED", true),
            threads_user_agent: opt("THREADS_USER_AGENT").unwrap_or_else(|| DEFAULT_UA.into()),
            threads_sec_ch_ua: opt("THREADS_SEC_CH_UA")
                .unwrap_or_else(|| DEFAULT_THREADS_SEC_CH_UA.into()),
            max_upload_bytes: parse_u64("MAX_UPLOAD_BYTES", FIFTY_MIB)?,
            queue_capacity: parse_u64("QUEUE_CAPACITY", 16)? as usize,
            job_timeout: Duration::from_secs(parse_u64("JOB_TIMEOUT_SECS", 90)?),
            request_pacing: Duration::from_millis(parse_u64("REQUEST_PACING_MS", 1500)?),
            heartbeat: heartbeat_from_secs(parse_u64("HEARTBEAT_SECS", 3600)?),
        })
    }

    pub fn chat_allowed(&self, chat_id: i64) -> bool {
        self.allowed_chats.is_empty() || self.allowed_chats.contains(&chat_id)
    }

    /// A one-line, **secrets-free** snapshot of the runtime config for the
    /// startup log. Never emits the bot token, cookies path, or API keys — only
    /// booleans for those (see `summary_omits_secrets` test).
    pub fn summary(&self) -> String {
        format!(
            "allowed_chats={} queue_cap={} cache_ttl={}s job_timeout={}s pacing={}ms \
max_upload={}MB cookies={} fallback={} threads={} jina_key={} heartbeat={} embed_ua={:?}",
            self.allowed_chats.len(),
            self.queue_capacity,
            self.cache_ttl.as_secs(),
            self.job_timeout.as_secs(),
            self.request_pacing.as_millis(),
            self.max_upload_bytes / (1024 * 1024),
            self.ig_cookies_path.is_some(),
            self.fallback_provider.as_deref().unwrap_or("none"),
            self.threads_enabled,
            self.jina_api_key.is_some(),
            self.heartbeat.map_or_else(|| "off".to_string(), |d| format!("{}s", d.as_secs())),
            self.embed_user_agent,
        )
    }
}

fn req(var: &'static str) -> Result<String, AppError> {
    std::env::var(var)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or(AppError::MissingEnv(var))
}

fn opt(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|s| !s.trim().is_empty())
}

fn parse_u64(var: &'static str, default: u64) -> Result<u64, AppError> {
    match opt(var) {
        Some(s) => s
            .parse::<u64>()
            .map_err(|e| AppError::InvalidEnv { var, msg: e.to_string() }),
        None => Ok(default),
    }
}

/// Parse a boolean env flag. Unset → `default`; set → true unless it reads as a
/// negative (`0`/`false`/`no`/`off`). `opt` already drops empty values.
fn parse_bool(var: &str, default: bool) -> bool {
    match opt(var) {
        Some(s) => !matches!(s.to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
        None => default,
    }
}

/// Map `HEARTBEAT_SECS` to an interval; `0` disables the heartbeat.
fn heartbeat_from_secs(secs: u64) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(secs))
}

fn parse_chat_ids(s: &str) -> Result<Vec<i64>, AppError> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(|x| {
            x.parse::<i64>().map_err(|e| AppError::InvalidEnv {
                var: "ALLOWED_CHAT_IDS",
                msg: format!("'{x}': {e}"),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_allowlist_empty_allows_all() {
        let c = bare();
        assert!(c.chat_allowed(123));
        assert!(c.chat_allowed(-100));
    }

    #[test]
    fn chat_allowlist_restricts() {
        let mut c = bare();
        c.allowed_chats = vec![-1001, 42];
        assert!(c.chat_allowed(42));
        assert!(c.chat_allowed(-1001));
        assert!(!c.chat_allowed(7));
    }

    #[test]
    fn parse_chat_ids_handles_spaces_and_signs() {
        assert_eq!(parse_chat_ids(" -100123 , 42 ").unwrap(), vec![-100123, 42]);
        assert!(parse_chat_ids("abc").is_err());
        assert_eq!(parse_chat_ids("").unwrap(), Vec::<i64>::new());
    }

    #[test]
    fn summary_omits_secrets() {
        let mut c = bare();
        c.bot_token = "123456:SUPER-SECRET-TOKEN".into();
        c.ig_cookies_path = Some("/etc/igbot/cookies-SECRET.txt".into());
        c.jina_api_key = Some("jina-key-SECRET".into());
        let s = c.summary();
        assert!(!s.contains("SUPER-SECRET-TOKEN"), "token leaked: {s}");
        assert!(!s.contains("cookies-SECRET.txt"), "cookies path leaked: {s}");
        assert!(!s.contains("jina-key-SECRET"), "jina key leaked: {s}");
        // The non-secret booleans/values are still present.
        assert!(s.contains("cookies=true"));
        assert!(s.contains("jina_key=true"));
    }

    #[test]
    fn heartbeat_zero_disables() {
        assert_eq!(heartbeat_from_secs(0), None);
        assert_eq!(heartbeat_from_secs(3600), Some(Duration::from_secs(3600)));
    }

    fn bare() -> Config {
        Config {
            bot_token: "t".into(),
            allowed_chats: vec![],
            temp_dir: PathBuf::from("."),
            cache_ttl: Duration::from_secs(1),
            yt_dlp_path: "yt-dlp".into(),
            gallery_dl_path: "gallery-dl".into(),
            ig_cookies_path: None,
            fallback_provider: None,
            jina_api_key: None,
            user_agent: "ua".into(),
            embed_user_agent: "crawler-ua".into(),
            threads_enabled: true,
            threads_user_agent: "th-ua".into(),
            threads_sec_ch_ua: "th-ch".into(),
            max_upload_bytes: FIFTY_MIB,
            queue_capacity: 16,
            job_timeout: Duration::from_secs(90),
            request_pacing: Duration::from_millis(1500),
            heartbeat: Some(Duration::from_secs(3600)),
        }
    }
}
