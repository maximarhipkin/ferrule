//! Where a remote workspace is: `ssh:<name>` (a `[ssh.<name>]` block) or
//! `ssh://[user@]host[:port]/path` (docs/m34-ssh-local.md §2).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// One `[ssh.<name>]` block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    /// A host name, an address, or a `Host` alias from the ssh config.
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// The remote workspace: absolute, or `~/…` under the remote home.
    pub path: String,
    /// A private key to use (and only that one). Else ssh-agent and the
    /// ssh config decide, as for a plain `ssh`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<PathBuf>,
    /// An ssh config file used instead of `~/.ssh/config` (`ssh -F`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_config: Option<PathBuf>,
    /// The `ssh` program, when it isn't the one on `PATH`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<PathBuf>,
}

/// A parsed, checked remote workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// `app` for `ssh:app`; for a URL, made from the host and port. Names
    /// the local anchor dir.
    pub name: String,
    /// As the owner wrote it: `ssh:app` or the URL.
    pub label: String,
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub path: String,
    pub identity_file: Option<PathBuf>,
    pub ssh_config: Option<PathBuf>,
    pub ssh: Option<PathBuf>,
}

/// Whether a workspace setting means a remote one.
pub fn is_remote(spec: &str) -> bool {
    spec.starts_with("ssh:")
}

impl Target {
    /// `ssh:<name>` against `hosts`, or an `ssh://` URL.
    pub fn parse(spec: &str, hosts: &BTreeMap<String, HostConfig>) -> Result<Target, String> {
        if let Some(rest) = spec.strip_prefix("ssh://") {
            return Self::from_url(spec, rest);
        }
        let Some(name) = spec.strip_prefix("ssh:") else {
            return Err(format!(
                "`{spec}` isn't an ssh workspace (ssh:<name> or ssh://host/path)"
            ));
        };
        let Some(cfg) = hosts.get(name) else {
            let known: Vec<&str> = hosts.keys().map(String::as_str).collect();
            return Err(if known.is_empty() {
                format!("`{spec}`: there is no [ssh.{name}] block in the config (add one with `ferrule setup`)")
            } else {
                format!(
                    "`{spec}`: there is no [ssh.{name}] block in the config; configured: {}",
                    known.join(", ")
                )
            });
        };
        Self::from_config(name, cfg)
    }

    pub fn from_config(name: &str, cfg: &HostConfig) -> Result<Target, String> {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        {
            return Err(format!(
                "[ssh.{name}]: the name may only use letters, digits, `-`, `_` and `.`"
            ));
        }
        let target = Target {
            name: name.to_string(),
            label: format!("ssh:{name}"),
            host: cfg.host.clone(),
            user: cfg.user.clone(),
            port: cfg.port,
            path: cfg.path.clone(),
            identity_file: cfg.identity_file.as_deref().map(expand_home),
            ssh_config: cfg.ssh_config.as_deref().map(expand_home),
            ssh: cfg.ssh.clone(),
        };
        target.check().map_err(|e| format!("[ssh.{name}]: {e}"))?;
        Ok(target)
    }

    fn from_url(spec: &str, rest: &str) -> Result<Target, String> {
        let bad = |why: &str| format!("`{spec}`: {why} (ssh://[user@]host[:port]/path)");
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => return Err(bad("no remote path")),
        };
        let (user, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u.to_string()), h),
            None => (None, authority),
        };
        let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
            let (h, after) = v6.split_once(']').ok_or_else(|| bad("an unclosed `[`"))?;
            let port = match after.strip_prefix(':') {
                Some(p) => Some(p.parse::<u16>().map_err(|_| bad("a bad port"))?),
                None if after.is_empty() => None,
                None => return Err(bad("junk after `]`")),
            };
            (h.to_string(), port)
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (
                    h.to_string(),
                    Some(p.parse::<u16>().map_err(|_| bad("a bad port"))?),
                ),
                None => (hostport.to_string(), None),
            }
        };
        // `ssh://host/~/app` is `~/app` on the remote.
        let path = match path.strip_prefix("/~") {
            Some(home) if home.is_empty() || home.starts_with('/') => format!("~{home}"),
            _ => path.to_string(),
        };
        let mut name: String = host
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        if let Some(p) = port {
            name.push_str(&format!("-{p}"));
        }
        let target = Target {
            name,
            label: spec.to_string(),
            host,
            user,
            port,
            path,
            identity_file: None,
            ssh_config: None,
            ssh: None,
        };
        target.check().map_err(|e| bad(&e))?;
        Ok(target)
    }

    /// Nothing here may reach ssh as an option: a host or user starting
    /// with `-` would be one (`-oProxyCommand=…`).
    fn check(&self) -> Result<(), String> {
        let word = |what: &str, v: &str| -> Result<(), String> {
            if v.is_empty() {
                return Err(format!("an empty {what}"));
            }
            if v.starts_with('-') || v.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(format!("{what} `{v}` can't start with `-` or hold spaces"));
            }
            Ok(())
        };
        word("host", &self.host)?;
        if let Some(user) = &self.user {
            word("user", user)?;
            if user.contains('@') {
                return Err(format!("user `{user}` can't hold `@`"));
            }
        }
        if self.port == Some(0) {
            return Err("port 0".into());
        }
        let p = &self.path;
        if !(p.starts_with('/') || p == "~" || p.starts_with("~/")) {
            return Err(format!(
                "path `{p}` must be absolute (or `~/…`, under the remote home)"
            ));
        }
        if p.contains('\0') || p.contains('\n') {
            return Err("the path holds a control character".into());
        }
        Ok(())
    }

    /// `user@host:port:/path`, for people.
    pub fn describe(&self) -> String {
        let mut s = String::new();
        if let Some(u) = &self.user {
            s.push_str(u);
            s.push('@');
        }
        if self.host.contains(':') {
            s.push_str(&format!("[{}]", self.host));
        } else {
            s.push_str(&self.host);
        }
        if let Some(p) = self.port {
            s.push_str(&format!(":{p}"));
        }
        s.push(':');
        s.push_str(&self.path);
        s
    }

    /// Whether the target is this machine: then ferrule's local sandbox is
    /// bypassed, not extended.
    pub fn is_loopback(&self) -> bool {
        let h = self.host.to_ascii_lowercase();
        if matches!(h.as_str(), "localhost" | "::1" | "0.0.0.0" | "[::1]") || h.starts_with("127.")
        {
            return true;
        }
        local_hostname().is_some_and(|l| l.eq_ignore_ascii_case(&h))
    }
}

fn local_hostname() -> Option<String> {
    for var in ["HOSTNAME", "COMPUTERNAME"] {
        if let Some(v) = std::env::var_os(var).and_then(|v| v.into_string().ok()) {
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `~/x` against the local home.
pub fn expand_home(p: &std::path::Path) -> PathBuf {
    match (p.strip_prefix("~"), ferrule_sandbox::home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => p.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts() -> BTreeMap<String, HostConfig> {
        let mut m = BTreeMap::new();
        m.insert(
            "app".into(),
            HostConfig {
                host: "app.example.com".into(),
                user: Some("ferrule".into()),
                port: Some(2222),
                path: "/srv/app".into(),
                ..Default::default()
            },
        );
        m
    }

    #[test]
    fn named_targets_come_from_the_config() {
        let t = Target::parse("ssh:app", &hosts()).unwrap();
        assert_eq!(t.label, "ssh:app");
        assert_eq!(t.describe(), "ferrule@app.example.com:2222:/srv/app");
        let err = Target::parse("ssh:web", &hosts()).unwrap_err();
        assert!(err.contains("[ssh.web]") && err.contains("app"), "{err}");
    }

    #[test]
    fn urls_parse_with_and_without_user_port_and_v6() {
        let t = Target::parse("ssh://deploy@build.lan:22/srv/app", &BTreeMap::new()).unwrap();
        assert_eq!(
            (t.user.as_deref(), t.host.as_str(), t.port, t.path.as_str()),
            (Some("deploy"), "build.lan", Some(22), "/srv/app")
        );
        assert_eq!(t.name, "build.lan-22");
        let t = Target::parse("ssh://box/~/work", &BTreeMap::new()).unwrap();
        assert_eq!((t.user, t.port, t.path.as_str()), (None, None, "~/work"));
        let t = Target::parse("ssh://[::1]:2299/tmp/w", &BTreeMap::new()).unwrap();
        assert_eq!((t.host.as_str(), t.port), ("::1", Some(2299)));
        assert!(t.is_loopback());
        assert_eq!(t.describe(), "[::1]:2299:/tmp/w");
    }

    #[test]
    fn nothing_can_smuggle_an_ssh_option() {
        for spec in [
            "ssh://-oProxyCommand=x/srv",
            "ssh://-l@host/srv",
            "ssh://host",
            "ssh://host:99999/srv",
            "ssh://a b/srv",
            "ssh://[::1/srv",
        ] {
            assert!(
                Target::parse(spec, &BTreeMap::new()).is_err(),
                "{spec} parsed"
            );
        }
        let cfg = HostConfig {
            host: "h".into(),
            path: "srv".into(),
            ..Default::default()
        };
        assert!(Target::from_config("x", &cfg)
            .unwrap_err()
            .contains("absolute"));
        assert!(Target::from_config(
            "a/b",
            &HostConfig {
                path: "/s".into(),
                ..cfg
            }
        )
        .is_err());
    }
}
