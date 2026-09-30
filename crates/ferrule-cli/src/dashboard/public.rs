//! The address the dashboard is opened at behind a proxy (M44): its host,
//! its origin and the path prefix the proxy serves it under.

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Public {
    /// `host` or `host:port`, lowercase: what a request's `Host` says.
    pub host: String,
    /// `scheme://host[:port]`, what a browser sends as `Origin`.
    pub origin: String,
    /// The path prefix without a trailing slash; `""` when there is none.
    pub base: String,
    /// `https`: cookies carry `Secure`.
    pub secure: bool,
}

impl Public {
    pub fn parse(raw: &str) -> Result<Public> {
        let raw = raw.trim();
        // Before any URL parsing, which would fold `..` and `%2e` away.
        let path_part = raw.split_once("://").map_or(raw, |(_, rest)| rest);
        if raw.contains(['?', '#', '@', '%'])
            || raw.chars().any(char::is_whitespace)
            || path_part.split('/').any(|s| s == "..")
        {
            bail!("no query, fragment, user name or `..` in it");
        }
        let url = url::Url::parse(raw).context("not a URL")?;
        let scheme = url.scheme();
        if scheme != "http" && scheme != "https" {
            bail!("it must start with http:// or https://");
        }
        let Some(host) = url.host_str() else {
            bail!("it has no host");
        };
        let host = match url.port() {
            Some(p) => format!("{}:{p}", host.to_ascii_lowercase()),
            None => host.to_ascii_lowercase(),
        };
        let base = url.path().trim_end_matches('/').to_string();
        for seg in base.split('/').filter(|s| !s.is_empty()) {
            if !seg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '-'))
            {
                bail!("the path may hold only letters, digits and . _ ~ -");
            }
        }
        Ok(Public {
            origin: format!("{scheme}://{host}"),
            host,
            base,
            secure: scheme == "https",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_urls_parse_and_bad_ones_are_refused() {
        let p = Public::parse("https://Bots.Example.com/b/b_4f2a/").unwrap();
        assert_eq!(p.host, "bots.example.com");
        assert_eq!(p.origin, "https://bots.example.com");
        assert_eq!(p.base, "/b/b_4f2a");
        assert!(p.secure);
        let p = Public::parse("http://localhost:8080").unwrap();
        assert_eq!((p.host.as_str(), p.base.as_str()), ("localhost:8080", ""));
        assert!(!p.secure);
        assert_eq!(Public::parse("https://x.io:443/").unwrap().host, "x.io");
        for bad in [
            "ftp://x/",
            "https://x/a?b=1",
            "https://x/#a",
            "https://u@x/",
            "https://x/a/../b",
            "https://x/a b",
            "https://x/%2e/",
            "not a url",
        ] {
            assert!(Public::parse(bad).is_err(), "{bad} should be refused");
        }
    }
}
