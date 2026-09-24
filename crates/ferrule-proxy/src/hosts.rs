//! Host patterns from `[secrets]`: `api.github.com` or `*.githubusercontent.com`.

use anyhow::{bail, Result};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPattern {
    Exact(String),
    /// `*.suffix`: any subdomain of `suffix`, but not `suffix` itself.
    Subdomains(String),
}

impl HostPattern {
    pub fn parse(raw: &str) -> Result<Self> {
        let s = raw.trim().trim_end_matches('.').to_ascii_lowercase();
        if s.contains("://") {
            bail!("`{raw}`: give a host name, not a URL");
        }
        if s.contains('/') {
            bail!("`{raw}`: paths aren't supported, only host names");
        }
        if s.contains(':') {
            bail!("`{raw}`: ports (and IPv6 literals) aren't supported, only host names");
        }
        let (wild, name) = match s.strip_prefix("*.") {
            Some(rest) => (true, rest),
            None => (false, s.as_str()),
        };
        let label_ok = |l: &str| {
            !l.is_empty()
                && l.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        };
        if !name.split('.').all(label_ok) {
            bail!("`{raw}` isn't a host name (a leading `*.` is the only wildcard)");
        }
        if wild && !name.contains('.') {
            bail!("`{raw}` is too broad: a wildcard needs at least two labels after `*.`");
        }
        Ok(if wild {
            Self::Subdomains(name.to_string())
        } else {
            Self::Exact(name.to_string())
        })
    }

    pub fn matches(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        match self {
            Self::Exact(h) => host == *h,
            Self::Subdomains(s) => {
                host.len() > s.len() + 1
                    && host.ends_with(s.as_str())
                    && host.as_bytes()[host.len() - s.len() - 1] == b'.'
            }
        }
    }
}

impl fmt::Display for HostPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(h) => f.write_str(h),
            Self::Subdomains(s) => write!(f, "*.{s}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_wildcard_patterns_match_what_they_say() {
        let exact = HostPattern::parse("API.GitHub.com.").unwrap();
        assert!(exact.matches("api.github.com"));
        assert!(exact.matches("API.GITHUB.COM."));
        assert!(!exact.matches("github.com"));
        assert!(!exact.matches("evil-api.github.com"));

        let wild = HostPattern::parse("*.githubusercontent.com").unwrap();
        assert!(wild.matches("raw.githubusercontent.com"));
        assert!(wild.matches("a.b.githubusercontent.com"));
        assert!(
            !wild.matches("githubusercontent.com"),
            "the bare suffix isn't a subdomain"
        );
        assert!(!wild.matches("evilgithubusercontent.com"));
        assert_eq!(wild.to_string(), "*.githubusercontent.com");
    }

    #[test]
    fn urls_ports_paths_and_broad_wildcards_are_rejected() {
        for bad in [
            "",
            "*",
            "*.com",
            "https://api.github.com",
            "api.github.com:443",
            "api.github.com/repos",
            "api.*.com",
            "a..b",
            "::1",
        ] {
            assert!(
                HostPattern::parse(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
        assert!(HostPattern::parse("localhost").is_ok());
        assert!(HostPattern::parse("127.0.0.1").is_ok());
    }
}
