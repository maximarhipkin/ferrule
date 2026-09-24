//! Where extensions may come from. A source is `<kind>:<locator>[@version]`;
//! an allow-list entry has the same shape, plus a trailing `/*` meaning
//! "anything under this prefix" — an npm scope, a git org, a URL prefix.
//! See `docs/m13-self-extension.md` §2 for why publisher granularity is the
//! default and why registry-wide entries are refused.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Npm,
    Pypi,
    Git,
    Url,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Npm => "npm",
            Kind::Pypi => "pypi",
            Kind::Git => "git",
            Kind::Url => "url",
        }
    }

    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "npm" => Ok(Kind::Npm),
            "pypi" => Ok(Kind::Pypi),
            "git" => Ok(Kind::Git),
            "url" => Ok(Kind::Url),
            other => Err(format!(
                "unknown source kind `{other}` (use npm:, pypi:, git: or url:)"
            )),
        }
    }
}

/// A parsed, normalised source: what the model asks to install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub kind: Kind,
    /// npm package name, PyPI project name (PEP 503-normalised), or URL.
    pub locator: String,
    /// npm/pypi: the exact version. git: the rev as asked (branch, tag or
    /// SHA), resolved to a SHA at install. url: always `None`.
    pub version: Option<String>,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind.as_str(), self.locator)?;
        match (&self.version, self.kind) {
            (Some(v), Kind::Pypi) => write!(f, "=={v}"),
            (Some(v), _) => write!(f, "@{v}"),
            (None, _) => Ok(()),
        }
    }
}

impl Source {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (kind, rest) = spec.trim().split_once(':').ok_or_else(|| {
            format!("`{spec}`: expected <kind>:<name>, e.g. npm:@scope/pkg@1.2.3")
        })?;
        let kind = Kind::parse(kind)?;
        let (locator, version) = split_version(kind, rest)?;
        let locator = normalise(kind, &locator)?;
        Ok(Self {
            kind,
            locator,
            version,
        })
    }

    /// npm and PyPI installs must name one exact version: no ranges, no
    /// `latest`, so nothing resolves differently later.
    pub fn require_exact_version(&self) -> Result<&str, String> {
        let v = self.version.as_deref().unwrap_or("");
        let exact = !v.is_empty()
            && v.chars().next().is_some_and(|c| c.is_ascii_digit())
            && v.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
        if exact {
            Ok(v)
        } else {
            Err(format!(
                "`{self}`: pin an exact version (e.g. {}:{}{}1.2.3) — ranges, tags and `latest` are refused",
                self.kind.as_str(),
                self.locator,
                if self.kind == Kind::Pypi { "==" } else { "@" }
            ))
        }
    }
}

fn split_version(kind: Kind, rest: &str) -> Result<(String, Option<String>), String> {
    let rest = rest.trim();
    let split = match kind {
        Kind::Pypi => rest
            .split_once("==")
            .map(|(a, b)| (a.to_string(), b.to_string())),
        // `@scope/pkg@1.0`: the scope's own `@` is at index 0.
        Kind::Npm => rest
            .rfind('@')
            .filter(|&i| i > 0)
            .map(|i| (rest[..i].to_string(), rest[i + 1..].to_string())),
        // A rev only after the last `/`, so `https://user@host/…` isn't one.
        Kind::Git => rest
            .rfind('@')
            .filter(|&i| rest.rfind('/').is_some_and(|slash| i > slash))
            .map(|i| (rest[..i].to_string(), rest[i + 1..].to_string())),
        Kind::Url => None,
    };
    Ok(match split {
        Some((loc, v)) if !v.is_empty() => (loc, Some(v)),
        Some((loc, _)) => (loc, None),
        None => (rest.to_string(), None),
    })
}

fn normalise(kind: Kind, locator: &str) -> Result<String, String> {
    if locator.is_empty() {
        return Err("empty source name".into());
    }
    match kind {
        Kind::Npm => {
            let l = locator.to_ascii_lowercase();
            let valid = l.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '@' | '/' | '-' | '_' | '.' | '*')
            });
            if !valid {
                return Err(format!("`{locator}` is not an npm package name"));
            }
            Ok(l)
        }
        // PEP 503: lowercase, runs of `-_.` become one `-`.
        Kind::Pypi => {
            let mut out = String::new();
            let mut sep = false;
            for c in locator.chars() {
                if matches!(c, '-' | '_' | '.') {
                    sep = true;
                    continue;
                }
                if !(c.is_ascii_alphanumeric() || c == '*') {
                    return Err(format!("`{locator}` is not a PyPI project name"));
                }
                if sep && !out.is_empty() {
                    out.push('-');
                }
                sep = false;
                out.push(c.to_ascii_lowercase());
            }
            Ok(out)
        }
        Kind::Git | Kind::Url => {
            let (scheme, rest) = locator
                .split_once("://")
                .ok_or_else(|| format!("`{locator}` is not a URL"))?;
            let scheme = scheme.to_ascii_lowercase();
            let ok = match kind {
                Kind::Git => scheme == "https" || scheme == "file",
                _ => scheme == "https",
            };
            if !ok {
                return Err(format!(
                    "`{locator}`: only {} URLs are accepted",
                    if kind == Kind::Git {
                        "https:// and file://"
                    } else {
                        "https://"
                    }
                ));
            }
            let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
            if host.contains('@') {
                return Err(format!("`{locator}`: credentials in the URL are refused"));
            }
            let mut path = path.trim_end_matches('/').to_string();
            if kind == Kind::Git {
                if let Some(p) = path.strip_suffix(".git") {
                    path = p.to_string();
                }
            }
            if path.split('/').any(|seg| seg == "..") {
                return Err(format!("`{locator}`: `..` in the path"));
            }
            let host = host.to_ascii_lowercase();
            Ok(if path.is_empty() {
                format!("{scheme}://{host}")
            } else {
                format!("{scheme}://{host}/{path}")
            })
        }
    }
}

/// One allow-list entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowEntry {
    pub kind: Kind,
    /// Normalised, without the `/*`.
    pub pattern: String,
    /// `/*`: anything under `pattern`.
    pub prefix: bool,
    /// Only this version (npm/pypi) or commit (git, a SHA prefix).
    pub version: Option<String>,
}

impl AllowEntry {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        let (kind, rest) = spec
            .split_once(':')
            .ok_or_else(|| format!("`{spec}`: expected <kind>:<pattern>"))?;
        let kind = Kind::parse(kind)?;
        let (loc, version) = split_version(kind, rest)?;
        let (loc, prefix) = match loc.strip_suffix("/*") {
            Some(l) => (l.to_string(), true),
            None if loc == "*" || loc.ends_with('*') => {
                return Err(format!(
                    "`{spec}`: registry-wide entries are refused — name a scope, org or prefix ending in /*"
                ))
            }
            None => (loc, false),
        };
        if prefix && version.is_some() {
            return Err(format!("`{spec}`: a /* entry can't pin a version"));
        }
        let pattern = normalise(kind, &loc)?;
        if pattern.contains('*') {
            return Err(format!("`{spec}`: `*` is only allowed as a trailing /*"));
        }
        if prefix {
            let too_wide = match kind {
                Kind::Npm => {
                    !(pattern.starts_with('@') && pattern.len() > 1 && !pattern.contains('/'))
                }
                Kind::Pypi => true,
                // At least one path segment under the host: an org, not all of GitHub.
                Kind::Git => url_path(&pattern).is_empty(),
                Kind::Url => false,
            };
            if too_wide {
                return Err(match kind {
                    Kind::Npm => format!("`{spec}`: an npm /* entry must be a scope: npm:@scope/*"),
                    Kind::Pypi => format!("`{spec}`: PyPI has no scopes — list whole project names"),
                    _ => format!(
                        "`{spec}`: registry-wide entries are refused — name an org: git:https://host/org/*"
                    ),
                });
            }
        }
        Ok(Self {
            kind,
            pattern,
            prefix,
            version: version.map(|v| v.to_ascii_lowercase()),
        })
    }

    /// Whether this entry covers `source`'s name. The version is checked
    /// separately by `permits_pin`, once the pin is known.
    pub fn covers(&self, source: &Source) -> bool {
        if self.kind != source.kind {
            return false;
        }
        if self.prefix {
            source
                .locator
                .strip_prefix(&self.pattern)
                .is_some_and(|rest| rest.starts_with('/') && rest.len() > 1)
        } else {
            source.locator == self.pattern
        }
    }

    pub fn permits_pin(&self, pin: &str) -> bool {
        match &self.version {
            None => true,
            Some(v) if self.kind == Kind::Git => {
                v.len() >= 7 && pin.to_ascii_lowercase().starts_with(v)
            }
            Some(v) => v == pin,
        }
    }
}

fn url_path(url: &str) -> &str {
    url.split_once("://")
        .and_then(|(_, rest)| rest.split_once('/'))
        .map(|(_, p)| p)
        .unwrap_or("")
}

/// The owner's allow-list. Bad entries are dropped when it's built, and
/// what was wrong with them is kept for a warning.
#[derive(Debug, Clone, Default)]
pub struct AllowList {
    pub entries: Vec<AllowEntry>,
    pub rejected: Vec<String>,
}

impl AllowList {
    pub fn new<S: AsRef<str>>(specs: &[S]) -> Self {
        let mut list = Self::default();
        for spec in specs {
            match AllowEntry::parse(spec.as_ref()) {
                Ok(e) => list.entries.push(e),
                Err(e) => list.rejected.push(e),
            }
        }
        list
    }

    /// Covered by some entry, before the pin is known. Nothing from a
    /// source that isn't covered is fetched before the owner approves.
    pub fn covers(&self, source: &Source) -> bool {
        self.entries.iter().any(|e| e.covers(source))
    }

    /// Covered by some entry whose version constraint `pin` meets.
    pub fn permits(&self, source: &Source, pin: &str) -> bool {
        self.entries
            .iter()
            .any(|e| e.covers(source) && e.permits_pin(pin))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(s: &str) -> Source {
        Source::parse(s).unwrap()
    }

    #[test]
    fn sources_parse_and_normalise() {
        let s = src("npm:@ModelContextProtocol/server-memory@2025.4.25");
        assert_eq!(s.kind, Kind::Npm);
        assert_eq!(s.locator, "@modelcontextprotocol/server-memory");
        assert_eq!(s.version.as_deref(), Some("2025.4.25"));
        assert_eq!(
            s.to_string(),
            "npm:@modelcontextprotocol/server-memory@2025.4.25"
        );

        let s = src("pypi:MCP_Server.Fetch==0.6.2");
        assert_eq!(s.locator, "mcp-server-fetch");
        assert_eq!(s.to_string(), "pypi:mcp-server-fetch==0.6.2");

        let s = src("git:HTTPS://GitHub.com/Org/Repo.git/@main");
        assert_eq!(s.locator, "https://github.com/Org/Repo");
        assert_eq!(s.version.as_deref(), Some("main"));
        let s = src("git:file:///tmp/x/repo");
        assert_eq!(s.locator, "file:///tmp/x/repo");
        assert_eq!(s.version, None);

        assert!(Source::parse("git:http://github.com/a/b").is_err());
        assert!(Source::parse("git:ssh://git@github.com/a/b").is_err());
        assert!(Source::parse("git:https://user:tok@github.com/a/b").is_err());
        assert!(Source::parse("url:http://host/mcp").is_err());
        assert!(Source::parse("cargo:foo").is_err());
        assert!(Source::parse("npm:foo bar").is_err());
    }

    #[test]
    fn npm_and_pypi_need_an_exact_version() {
        assert!(src("npm:pkg@1.2.3").require_exact_version().is_ok());
        assert!(src("pypi:pkg==0.6.2").require_exact_version().is_ok());
        for bad in [
            "npm:pkg",
            "npm:pkg@latest",
            "npm:pkg@^1.2.0",
            "npm:pkg@~1",
            "pypi:pkg",
            "npm:pkg@1.x || 2",
        ] {
            assert!(
                Source::parse(bad).map_or(true, |s| s.require_exact_version().is_err()),
                "{bad}"
            );
        }
    }

    #[test]
    fn publisher_entries_cover_what_they_publish_and_nothing_else() {
        let list = AllowList::new(&[
            "npm:@modelcontextprotocol/*",
            "npm:single-pkg@1.0.0",
            "pypi:mcp-server-fetch",
            "git:https://github.com/anthropics/*",
            "git:https://github.com/me/tools@3f2a9c1",
            "url:https://mcp.example.com/*",
        ]);
        assert!(list.rejected.is_empty(), "{:?}", list.rejected);

        assert!(list.covers(&src("npm:@modelcontextprotocol/server-memory@1.0.0")));
        assert!(!list.covers(&src("npm:@modelcontextprotocol-evil/x@1.0.0")));
        assert!(!list.covers(&src("npm:modelcontextprotocol@1.0.0")));

        let single = src("npm:single-pkg@1.0.0");
        assert!(list.permits(&single, "1.0.0"));
        assert!(
            !list.permits(&single, "1.0.1"),
            "a pinned entry holds its version"
        );

        assert!(list.covers(&src("pypi:mcp_server_fetch==1")));
        assert!(list.covers(&src("git:https://GITHUB.com/anthropics/skills.git")));
        assert!(!list.covers(&src("git:https://github.com/anthropics-fake/skills")));
        assert!(!list.covers(&src("git:https://github.com/anthropics")));

        let tools = src("git:https://github.com/me/tools@main");
        assert!(list.permits(&tools, "3f2a9c1aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(!list.permits(&tools, "0000000aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));

        assert!(list.covers(&src("url:https://mcp.example.com/v1/sse")));
        assert!(!list.covers(&src("url:https://mcp.example.com.evil.io/x")));
    }

    #[test]
    fn registry_wide_and_malformed_entries_are_refused() {
        for bad in [
            "npm:*",
            "npm:foo*",
            "npm:foo/*",
            "pypi:*",
            "pypi:mcp-*",
            "git:https://github.com/*",
            "url:*",
            "git:http://github.com/org/*",
            "npm:@scope/*@1.0.0",
            "docker:foo",
            "git:file:///*",
        ] {
            let list = AllowList::new(&[bad]);
            assert!(list.entries.is_empty(), "`{bad}` should be refused");
            assert_eq!(list.rejected.len(), 1);
        }
        assert!(AllowList::new(&["git:file:///srv/mirrors/*"])
            .rejected
            .is_empty());
    }
}
