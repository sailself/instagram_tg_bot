//! Dedicated Threads HTTP clients and read-only Netscape cookie import.
use reqwest::{cookie::CookieStore, header::HeaderValue, redirect::Policy, Client};
use std::{
    io::Read,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use url::Url;

const MAX_COOKIE_BYTES: u64 = 1024 * 1024;

pub(crate) fn is_threads_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some("threads.com" | "www.threads.com" | "threads.net" | "www.threads.net")
        )
}

pub fn clients(path: Option<&str>) -> anyhow::Result<(Client, Client)> {
    let cookies = path.map(ThreadsCookies::load).transpose()?.map(Arc::new);
    let build = |redirect| {
        let mut builder = Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(redirect);
        if let Some(store) = &cookies {
            builder = builder.cookie_provider(store.clone());
        }
        builder.build()
    };
    // A separate client keeps login cookies away from IG, CDN and Telegram traffic.
    let post = build(Policy::custom(|attempt| {
        if attempt.previous().len() >= 5 || !is_threads_url(attempt.url()) {
            attempt.stop()
        } else {
            attempt.follow()
        }
    }))?;
    let share = build(Policy::none())?;
    Ok((post, share))
}

struct ThreadsCookie {
    domain: String,
    subdomains: bool,
    path: String,
    expires: u64,
    pair: String,
}

struct ThreadsCookies(Vec<ThreadsCookie>);

impl ThreadsCookies {
    fn load(path: &str) -> anyhow::Result<Self> {
        let file = std::fs::File::open(path)
            .map_err(|_| anyhow::anyhow!("cannot read THREADS_COOKIES_PATH"))?;
        let mut text = String::new();
        file.take(MAX_COOKIE_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(|_| {
                anyhow::anyhow!("cannot read THREADS_COOKIES_PATH as UTF-8 Netscape cookies")
            })?;
        anyhow::ensure!(
            text.len() as u64 <= MAX_COOKIE_BYTES,
            "Threads cookie file exceeds 1 MiB"
        );
        Self::parse(&text).map_err(anyhow::Error::msg)
    }

    fn parse(text: &str) -> Result<Self, &'static str> {
        let mut cookies = Vec::new();
        for raw in text.trim_start_matches('\u{feff}').lines() {
            let line = raw.strip_prefix("#HttpOnly_").unwrap_or(raw);
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 7 {
                return Err("invalid Netscape Threads cookie record");
            }
            let domain = fields[0].trim_start_matches('.').to_ascii_lowercase();
            // Multi-domain browser exports are allowed, but only exact Threads
            // domains are imported. Never reinterpret Instagram cookies as Threads.
            if !matches!(
                domain.as_str(),
                "threads.com" | "www.threads.com" | "threads.net" | "www.threads.net"
            ) {
                continue;
            }
            let subdomains = match fields[1] {
                "TRUE" => true,
                "FALSE" => false,
                _ => return Err("invalid Threads cookie domain flag"),
            };
            if !matches!(fields[3], "TRUE" | "FALSE") || !fields[2].starts_with('/') {
                return Err("invalid Threads cookie scope");
            }
            let expires = fields[4]
                .parse()
                .map_err(|_| "invalid Threads cookie expiry")?;
            let name = fields[5];
            let value = fields[6];
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
                || !value
                    .bytes()
                    .all(|b| (0x21..=0x7e).contains(&b) && b != b';' && b != b',')
            {
                return Err("invalid Threads cookie name or value");
            }
            cookies.push(ThreadsCookie {
                domain,
                subdomains,
                path: fields[2].into(),
                expires,
                pair: format!("{name}={value}"),
            });
        }
        let now = now_secs();
        cookies.retain(|c| c.expires == 0 || c.expires > now);
        if cookies.is_empty() {
            return Err("THREADS_COOKIES_PATH contains no unexpired Threads cookies");
        }
        // More specific paths precede general paths, as in browser Cookie headers.
        cookies.sort_by_key(|c| std::cmp::Reverse(c.path.len()));
        Ok(Self(cookies))
    }

    fn header(&self, url: &Url, now: u64) -> Option<HeaderValue> {
        if !is_threads_url(url) {
            return None;
        }
        let host = url.host_str()?;
        let pairs: Vec<_> = self
            .0
            .iter()
            .filter(|c| {
                let domain_matches =
                    host == c.domain || (c.subdomains && host.ends_with(&format!(".{}", c.domain)));
                let path_matches = url.path() == c.path
                    || (url.path().starts_with(&c.path)
                        && (c.path.ends_with('/')
                            || url.path().as_bytes().get(c.path.len()) == Some(&b'/')));
                domain_matches && path_matches && (c.expires == 0 || c.expires > now)
            })
            .map(|c| c.pair.as_str())
            .collect();
        if pairs.is_empty() {
            return None;
        }
        let mut header = HeaderValue::from_str(&pairs.join("; ")).ok()?;
        header.set_sensitive(true);
        Some(header)
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(u64::MAX, |d| d.as_secs())
}

impl CookieStore for ThreadsCookies {
    // Read-only snapshot: never persist or import response cookies.
    fn set_cookies(&self, _: &mut dyn Iterator<Item = &HeaderValue>, _: &Url) {}
    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        self.header(url, now_secs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn jar() -> ThreadsCookies {
        ThreadsCookies::parse("# Netscape HTTP Cookie File\n#HttpOnly_.threads.com\tTRUE\t/\tTRUE\t0\tsessionid\tfake\nwww.threads.com\tFALSE\t/post\tTRUE\t0\tspecific\tx\n.instagram.com\tTRUE\t/\tTRUE\t0\tignore\tsecret\n.threads.net\tTRUE\t/\tTRUE\t0\tlegacy\ty\n").unwrap()
    }
    #[test]
    fn cookie_domain_path_and_https_scope() {
        let jar = jar();
        for url in [
            "https://threads.com.evil.test/",
            "https://evilthreads.com/",
            "https://cdninstagram.com/",
            "https://www.instagram.com/",
            "http://www.threads.com/",
            "https://www.threads.com:444/",
            "https://evil@www.threads.com/",
        ] {
            assert!(jar.cookies(&Url::parse(url).unwrap()).is_none(), "{url}");
        }
        let h = jar
            .cookies(&Url::parse("https://www.threads.com/post/a").unwrap())
            .unwrap();
        assert_eq!(h.to_str().unwrap(), "specific=x; sessionid=fake");
        assert!(h.is_sensitive());
        for url in [
            "https://www.threads.com/poster",
            "https://threads.com/post/a",
        ] {
            assert_eq!(
                jar.cookies(&Url::parse(url).unwrap()).unwrap(),
                "sessionid=fake"
            );
        }
        assert_eq!(
            jar.cookies(&Url::parse("https://www.threads.net/").unwrap())
                .unwrap(),
            "legacy=y"
        );
    }
    #[test]
    fn quoted_values_are_preserved_and_response_cookies_ignored() {
        let jar = ThreadsCookies::parse(".threads.com\tTRUE\t/\tTRUE\t0\trur\t\"REG\\054123\"\n")
            .unwrap();
        let url = Url::parse("https://www.threads.com/").unwrap();
        let before = jar.cookies(&url).unwrap();
        assert_eq!(before, "rur=\"REG\\054123\"");
        let response_cookie = HeaderValue::from_static("sessionid=changed; Path=/");
        jar.set_cookies(&mut std::iter::once(&response_cookie), &url);
        assert_eq!(jar.cookies(&url).unwrap(), before);
    }

    #[test]
    fn expiry_and_invalid_records() {
        for text in [
            "bad",
            ".threads.com\tTRUE\t/\tTRUE\t1\ta\tb",
            ".threads.com\tTRUE\t/\tTRUE\t0\ta\tb; injected=yes",
            ".threads.com\tTRUE\t/\tTRUE\tbad\ta\tb",
        ] {
            assert!(ThreadsCookies::parse(text).is_err());
        }
        let mut jar = jar();
        for c in &mut jar.0 {
            c.expires = 100;
        }
        assert!(jar
            .header(&Url::parse("https://www.threads.com/").unwrap(), 100)
            .is_none());
    }
    #[test]
    fn configured_file_is_read_only_and_errors_are_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret-name.txt");
        std::fs::write(&path, ".threads.com\tTRUE\t/\tTRUE\t0\tsessionid\tfake\n").unwrap();
        assert!(clients(path.to_str()).is_ok());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            ".threads.com\tTRUE\t/\tTRUE\t0\tsessionid\tfake\n"
        );
        std::fs::remove_file(&path).unwrap();
        let error = clients(path.to_str()).err().unwrap().to_string();
        assert!(!error.contains("secret-name"));
        assert!(clients(None).is_ok());
    }
}
