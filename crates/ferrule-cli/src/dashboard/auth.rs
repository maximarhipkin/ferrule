//! Who may use the page (docs/m22-dashboard.md §2): one-time login links
//! in a file any ferrule process can add to, and sessions in the serving
//! process's memory, each with its CSRF token.

use crate::filewrite::Lock;
use anyhow::{Context, Result};
use ferrule_connections::seal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const COOKIE: &str = "ferrule_dash";
pub const CSRF_HEADER: &str = "x-ferrule-csrf";

fn now_secs() -> u64 {
    now_ms() / 1000
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LinkFile {
    #[serde(default)]
    links: Vec<Link>,
    /// Sessions opened before this (unix ms) are over: `/dashboard off`
    /// or `ferrule dashboard off`, from any process.
    #[serde(default)]
    revoked_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Link {
    /// sha256 of the token; the token itself is only ever in the URL.
    hash: String,
    /// The host the link was made for (a tunnel's), or none: any host the
    /// server accepts.
    #[serde(default)]
    host: Option<String>,
    expires: u64,
}

/// `<data>/private/dashboard/links.json`, 0600, changed under its lock.
pub struct Links {
    path: PathBuf,
}

/// What a used link was good for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Used {
    pub host: Option<String>,
}

impl Links {
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn default_path() -> Result<PathBuf> {
        Ok(crate::secrets::private_dir()?
            .join("dashboard")
            .join("links.json"))
    }

    #[cfg(test)]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn load(&self) -> Result<LinkFile> {
        match std::fs::read(&self.path) {
            Ok(b) => Ok(serde_json::from_slice(&b).unwrap_or_default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LinkFile::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    fn save(&self, file: &LinkFile) -> Result<()> {
        seal::write_private(&self.path, &serde_json::to_vec(file)?)
    }

    /// A new one-time token, good for `ttl`, for `host` (or any). Expired
    /// links are dropped on the way.
    pub fn mint(&self, host: Option<&str>, ttl: Duration) -> Result<String> {
        let _lock = Lock::take(&self.path)?;
        let mut file = self.load()?;
        let now = now_secs();
        file.links.retain(|l| l.expires > now);
        let token = seal::b64(&seal::random::<32>());
        file.links.push(Link {
            hash: seal::sha256_b64(token.as_bytes()),
            host: host.map(str::to_string),
            expires: now + ttl.as_secs().max(1),
        });
        self.save(&file)?;
        Ok(token)
    }

    /// Uses `token` up: `Some` once, for an unexpired link, and never again.
    pub fn consume(&self, token: &str) -> Result<Option<Used>> {
        if token.is_empty() || token.len() > 128 {
            return Ok(None);
        }
        let _lock = Lock::take(&self.path)?;
        let mut file = self.load()?;
        let now = now_secs();
        let hash = seal::sha256_b64(token.as_bytes());
        let before = file.links.len();
        let mut found = None;
        file.links.retain(|l| {
            if l.hash == hash {
                if l.expires > now {
                    found = Some(Used {
                        host: l.host.clone(),
                    });
                }
                return false;
            }
            l.expires > now
        });
        if file.links.len() != before {
            self.save(&file)?;
        }
        Ok(found)
    }

    /// Unused, unexpired links.
    pub fn pending(&self) -> usize {
        let now = now_secs();
        self.load()
            .map(|f| f.links.iter().filter(|l| l.expires > now).count())
            .unwrap_or(0)
    }

    /// The hosts unused links were made for.
    pub fn hosts(&self) -> Vec<String> {
        let now = now_secs();
        self.load()
            .map(|f| {
                f.links
                    .into_iter()
                    .filter(|l| l.expires > now)
                    .filter_map(|l| l.host)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every unused link and every session so far stops working.
    pub fn revoke(&self) -> Result<()> {
        let _lock = Lock::take(&self.path)?;
        self.save(&LinkFile {
            links: Vec::new(),
            revoked_ms: now_ms(),
        })
    }

    /// When sessions were last revoked (unix ms), 0 if never.
    pub fn revoked_ms(&self) -> u64 {
        self.load().map(|f| f.revoked_ms).unwrap_or(0)
    }
}

#[derive(Debug, Clone)]
struct Session {
    csrf: String,
    host: String,
    opened_ms: u64,
    created: Instant,
    last: Instant,
}

/// A logged-in browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Granted {
    pub csrf: String,
}

/// Sessions, keyed by the sha256 of their cookie.
pub struct Sessions {
    idle: Duration,
    absolute: Duration,
    map: Mutex<HashMap<String, Session>>,
}

impl Sessions {
    pub fn new(idle: Duration, absolute: Duration) -> Self {
        Self {
            idle,
            absolute,
            map: Mutex::new(HashMap::new()),
        }
    }

    /// A new session on `host`: (the cookie value, its CSRF token).
    pub fn open(&self, host: &str) -> (String, String) {
        let cookie = seal::b64(&seal::random::<32>());
        let csrf = seal::b64(&seal::random::<24>());
        let now = Instant::now();
        let mut map = self.map.lock().unwrap();
        self.prune(&mut map, now);
        map.insert(
            seal::sha256_b64(cookie.as_bytes()),
            Session {
                csrf: csrf.clone(),
                host: host.to_string(),
                opened_ms: now_ms(),
                created: now,
                last: now,
            },
        );
        (cookie, csrf)
    }

    fn prune(&self, map: &mut HashMap<String, Session>, now: Instant) {
        map.retain(|_, s| {
            now.duration_since(s.last) < self.idle && now.duration_since(s.created) < self.absolute
        });
    }

    /// The session behind `cookie`, if it's live, was opened on `host`
    /// and after `revoked_ms`; touching it keeps it from idling out.
    pub fn check(&self, cookie: Option<&str>, host: &str, revoked_ms: u64) -> Option<Granted> {
        let cookie = cookie.filter(|c| !c.is_empty() && c.len() <= 128)?;
        let now = Instant::now();
        let mut map = self.map.lock().unwrap();
        self.prune(&mut map, now);
        map.retain(|_, s| s.opened_ms > revoked_ms);
        let s = map.get_mut(&seal::sha256_b64(cookie.as_bytes()))?;
        if s.host != host {
            return None;
        }
        s.last = now;
        Some(Granted {
            csrf: s.csrf.clone(),
        })
    }

    pub fn close(&self, cookie: &str) {
        self.map
            .lock()
            .unwrap()
            .remove(&seal::sha256_b64(cookie.as_bytes()));
    }

    pub fn clear(&self) {
        self.map.lock().unwrap().clear();
    }

    pub fn live(&self) -> usize {
        let mut map = self.map.lock().unwrap();
        self.prune(&mut map, Instant::now());
        map.len()
    }

    /// Hosts live sessions were opened on.
    pub fn hosts(&self) -> Vec<String> {
        let mut map = self.map.lock().unwrap();
        self.prune(&mut map, Instant::now());
        map.values().map(|s| s.host.clone()).collect()
    }
}

/// Constant-time equality for tokens.
pub fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// The `Set-Cookie` for a new session; `Secure` over HTTPS (the tunnel).
pub fn set_cookie(value: &str, secure: bool, max_age: Duration) -> String {
    format!(
        "{COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
        max_age.as_secs(),
        if secure { "; Secure" } else { "" }
    )
}

pub fn clear_cookie(secure: bool) -> String {
    format!(
        "{COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}",
        if secure { "; Secure" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links() -> (tempfile::TempDir, Links) {
        let dir = tempfile::tempdir().unwrap();
        let l = Links::at(dir.path().join("private/dashboard/links.json"));
        (dir, l)
    }

    #[test]
    fn a_link_works_once() {
        let (_d, l) = links();
        let t = l
            .mint(Some("a.trycloudflare.com"), Duration::from_secs(60))
            .unwrap();
        assert_eq!(l.pending(), 1);
        assert_eq!(l.hosts(), vec!["a.trycloudflare.com".to_string()]);
        assert_eq!(
            l.consume(&t).unwrap(),
            Some(Used {
                host: Some("a.trycloudflare.com".into())
            })
        );
        assert_eq!(l.consume(&t).unwrap(), None);
        assert_eq!(l.pending(), 0);
        // The file holds hashes only.
        let text = std::fs::read_to_string(l.path()).unwrap();
        assert!(!text.contains(&t));
    }

    #[test]
    fn tampered_expired_and_cleared_links_fail() {
        let (_d, l) = links();
        let t = l.mint(None, Duration::from_secs(60)).unwrap();
        let mut bad = t.clone();
        bad.pop();
        bad.push(if t.ends_with('A') { 'B' } else { 'A' });
        assert_eq!(l.consume(&bad).unwrap(), None);
        assert_eq!(l.consume("").unwrap(), None);
        l.revoke().unwrap();
        assert_eq!(l.consume(&t).unwrap(), None);
        assert!(l.revoked_ms() > 0);

        // An expired one: rewrite its expiry into the past.
        let t = l.mint(None, Duration::from_secs(60)).unwrap();
        let mut f = l.load().unwrap();
        f.links[0].expires = now_secs() - 1;
        l.save(&f).unwrap();
        assert_eq!(l.consume(&t).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_link_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, l) = links();
        l.mint(None, Duration::from_secs(60)).unwrap();
        let mode = std::fs::metadata(l.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn sessions_are_bound_to_their_host_and_expire() {
        let s = Sessions::new(Duration::from_secs(60), Duration::from_secs(600));
        let (cookie, csrf) = s.open("127.0.0.1:9");
        assert_eq!(
            s.check(Some(&cookie), "127.0.0.1:9", 0),
            Some(Granted { csrf: csrf.clone() })
        );
        assert_eq!(s.check(Some(&cookie), "evil.example", 0), None);
        assert_eq!(s.check(Some("nope"), "127.0.0.1:9", 0), None);
        assert_eq!(s.check(None, "127.0.0.1:9", 0), None);
        assert_eq!(
            s.check(Some(&cookie), "127.0.0.1:9", now_ms() + 1),
            None,
            "revoked"
        );
        let (cookie, _) = s.open("127.0.0.1:9");
        s.close(&cookie);
        assert_eq!(s.check(Some(&cookie), "127.0.0.1:9", 0), None);

        let quick = Sessions::new(Duration::from_millis(1), Duration::from_secs(600));
        let (cookie, _) = quick.open("h");
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(quick.check(Some(&cookie), "h", 0), None);
        let short = Sessions::new(Duration::from_secs(60), Duration::from_millis(1));
        let (cookie, _) = short.open("h");
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(short.check(Some(&cookie), "h", 0), None);
    }

    #[test]
    fn cookies_are_strict_and_http_only() {
        let c = set_cookie("v", true, Duration::from_secs(10));
        assert!(c.contains("HttpOnly") && c.contains("SameSite=Strict") && c.contains("Secure"));
        assert!(!set_cookie("v", false, Duration::from_secs(10)).contains("Secure"));
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
    }
}
