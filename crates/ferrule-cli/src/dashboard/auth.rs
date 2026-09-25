//! Who may use the page (docs/m22-dashboard.md §2): one-time login links
//! in a file any ferrule process can add to, and sessions in another
//! beside it, so a restart logs nobody out (docs/m24-dashboard-2.md §1).

use crate::filewrite::Lock;
use anyhow::{Context, Result};
use ferrule_connections::seal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// One logged-in browser, as `sessions.json` holds it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Session {
    /// [`host_key`] of the host it was opened on.
    host: String,
    opened_ms: u64,
    last_ms: u64,
}

/// `<data>/private/dashboard/sessions.json` (M24): the sessions, keyed by
/// the sha256 of their cookie. Never the cookie, and no CSRF token: that
/// is derived from the cookie ([`csrf_of`]).
#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionFile {
    #[serde(default)]
    sessions: HashMap<String, Session>,
    /// When the last link was sent on its own after a restart (unix ms).
    #[serde(default)]
    auto_link_ms: u64,
    /// The port the page last listened on, tried first when `port = 0`.
    #[serde(default)]
    port: u16,
}

/// A logged-in browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Granted {
    pub csrf: String,
}

/// How often a session's last use is written back: the idle timer loses
/// at most this much in a crash, and a 3-second poll isn't a file write.
const TOUCH_EVERY_MS: u64 = 60_000;

/// What a session is bound to: "loopback" for this machine on any port
/// (a cookie ignores the port, and `port = 0` may pick another after a
/// restart), else the host itself.
pub fn host_key(host: &str) -> String {
    let h = host.to_ascii_lowercase();
    if h.starts_with("127.0.0.1:") || h.starts_with("localhost:") || h.starts_with("[::1]:") {
        "loopback".into()
    } else {
        h
    }
}

/// A session's CSRF token: only whoever holds the (HttpOnly) cookie can
/// work it out, and nothing needs storing.
pub fn csrf_of(cookie: &str) -> String {
    seal::sha256_b64(format!("csrf:{cookie}").as_bytes())
}

/// Sessions, on disk so a restart doesn't log anyone out (docs/m24 §1),
/// with the file's contents cached and re-read when it changes.
pub struct Sessions {
    idle_ms: u64,
    absolute_ms: u64,
    path: PathBuf,
    cache: Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    file: SessionFile,
    seen: Option<(SystemTime, u64)>,
}

fn stamp(path: &std::path::Path) -> Option<(SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

impl Sessions {
    pub fn new(idle: Duration, absolute: Duration, path: PathBuf) -> Self {
        Self {
            idle_ms: idle.as_millis() as u64,
            absolute_ms: absolute.as_millis() as u64,
            path,
            cache: Mutex::new(Cache::default()),
        }
    }

    /// `sessions.json` next to `links.json`.
    pub fn beside(links: &Links, idle: Duration, absolute: Duration) -> Self {
        Self::new(idle, absolute, links.path.with_file_name("sessions.json"))
    }

    #[cfg(test)]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn read(&self) -> SessionFile {
        match std::fs::read(&self.path) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                tracing::warn!(
                    "{} is unreadable, so every dashboard session is over: {e}",
                    self.path.display()
                );
                SessionFile::default()
            }),
            Err(_) => SessionFile::default(),
        }
    }

    /// The file as it is now: the cache, re-read if another process (or a
    /// restart) changed it.
    fn with<T>(&self, f: impl FnOnce(&SessionFile) -> T) -> T {
        let mut c = self.cache.lock().unwrap();
        let now = stamp(&self.path);
        if now != c.seen || now.is_none() {
            c.file = self.read();
            c.seen = now;
        }
        f(&c.file)
    }

    /// Changes the file under its lock, starting from what's on disk.
    fn change<T>(&self, f: impl FnOnce(&mut SessionFile) -> T) -> Result<T> {
        let _lock = Lock::take(&self.path)?;
        let mut file = self.read();
        let out = f(&mut file);
        seal::write_private(&self.path, &serde_json::to_vec(&file)?)?;
        let mut c = self.cache.lock().unwrap();
        c.seen = stamp(&self.path);
        c.file = file;
        Ok(out)
    }

    fn live_at(&self, s: &Session, now: u64, revoked_ms: u64) -> bool {
        s.opened_ms > revoked_ms
            && now.saturating_sub(s.last_ms) < self.idle_ms
            && now.saturating_sub(s.opened_ms) < self.absolute_ms
    }

    fn prune(&self, file: &mut SessionFile, now: u64, revoked_ms: u64) {
        file.sessions
            .retain(|_, s| self.live_at(s, now, revoked_ms));
    }

    /// A new session on `host`: (the cookie value, its CSRF token).
    pub fn open(&self, host: &str, revoked_ms: u64) -> Result<(String, String)> {
        let cookie = seal::b64(&seal::random::<32>());
        let now = now_ms();
        self.change(|f| {
            self.prune(f, now, revoked_ms);
            f.sessions.insert(
                seal::sha256_b64(cookie.as_bytes()),
                Session {
                    host: host_key(host),
                    opened_ms: now,
                    last_ms: now,
                },
            );
        })?;
        let csrf = csrf_of(&cookie);
        Ok((cookie, csrf))
    }

    /// The session behind `cookie`, if it's live, was opened on `host`
    /// and after `revoked_ms`; using it keeps it from idling out.
    pub fn check(&self, cookie: Option<&str>, host: &str, revoked_ms: u64) -> Option<Granted> {
        let cookie = cookie.filter(|c| !c.is_empty() && c.len() <= 128)?;
        let hash = seal::sha256_b64(cookie.as_bytes());
        let now = now_ms();
        let key = host_key(host);
        let last = self.with(|f| {
            let s = f.sessions.get(&hash)?;
            (self.live_at(s, now, revoked_ms) && s.host == key).then_some(s.last_ms)
        })?;
        if now.saturating_sub(last) >= TOUCH_EVERY_MS || now < last {
            let written = self.change(|f| {
                self.prune(f, now, revoked_ms);
                match f.sessions.get_mut(&hash) {
                    Some(s) => {
                        s.last_ms = now;
                        true
                    }
                    None => false,
                }
            });
            match written {
                Ok(true) => {}
                // Revoked (or pruned) between the read and the lock.
                Ok(false) => return None,
                Err(e) => tracing::warn!("dashboard sessions: {e:#}"),
            }
        }
        Some(Granted {
            csrf: csrf_of(cookie),
        })
    }

    pub fn close(&self, cookie: &str) {
        let hash = seal::sha256_b64(cookie.as_bytes());
        if let Err(e) = self.change(|f| f.sessions.remove(&hash)) {
            tracing::warn!("dashboard sessions: {e:#}");
        }
    }

    /// Every session ends, on disk too, so no later start loads one.
    pub fn clear(&self) -> Result<()> {
        self.change(|f| f.sessions.clear())
    }

    pub fn live(&self, revoked_ms: u64) -> usize {
        let now = now_ms();
        self.with(|f| {
            f.sessions
                .values()
                .filter(|s| self.live_at(s, now, revoked_ms))
                .count()
        })
    }

    /// Hosts live sessions were opened on ("loopback" for this machine).
    pub fn hosts(&self, revoked_ms: u64) -> Vec<String> {
        let now = now_ms();
        self.with(|f| {
            f.sessions
                .values()
                .filter(|s| self.live_at(s, now, revoked_ms))
                .map(|s| s.host.clone())
                .collect()
        })
    }

    /// After a restart: were any tunnel sessions live? They can never be
    /// used again (the tunnel's name changed and a cookie is scoped to
    /// its name), so they go. `Some(n)` when it's time to send a new link
    /// on its own: `n` sessions went and none was sent in the last
    /// `every`.
    pub fn retire_tunnel_sessions(
        &self,
        revoked_ms: u64,
        every: Duration,
    ) -> Result<Option<usize>> {
        let now = now_ms();
        self.change(|f| {
            self.prune(f, now, revoked_ms);
            let before = f.sessions.len();
            f.sessions.retain(|_, s| s.host == "loopback");
            let gone = before - f.sessions.len();
            if gone == 0 || now.saturating_sub(f.auto_link_ms) < every.as_millis() as u64 {
                return None;
            }
            f.auto_link_ms = now;
            Some(gone)
        })
    }

    /// The port the page last listened on, if any.
    pub fn last_port(&self) -> Option<u16> {
        self.with(|f| (f.port != 0).then_some(f.port))
    }

    pub fn remember_port(&self, port: u16) {
        if self.last_port() == Some(port) {
            return;
        }
        if let Err(e) = self.change(|f| f.port = port) {
            tracing::warn!("dashboard sessions: {e:#}");
        }
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

    fn sessions(dir: &tempfile::TempDir, idle: Duration, absolute: Duration) -> Sessions {
        Sessions::new(
            idle,
            absolute,
            dir.path().join("private/dashboard/sessions.json"),
        )
    }

    #[test]
    fn sessions_are_bound_to_their_host_and_expire() {
        let dir = tempfile::tempdir().unwrap();
        let s = sessions(&dir, Duration::from_secs(60), Duration::from_secs(600));
        let (cookie, csrf) = s.open("a.trycloudflare.com", 0).unwrap();
        assert_eq!(csrf, csrf_of(&cookie));
        assert_eq!(
            s.check(Some(&cookie), "a.trycloudflare.com", 0),
            Some(Granted { csrf: csrf.clone() })
        );
        assert_eq!(s.check(Some(&cookie), "evil.example", 0), None);
        assert_eq!(s.check(Some(&cookie), "127.0.0.1:9", 0), None);
        assert_eq!(s.check(Some("nope"), "a.trycloudflare.com", 0), None);
        assert_eq!(s.check(None, "a.trycloudflare.com", 0), None);
        assert_eq!(
            s.check(Some(&cookie), "a.trycloudflare.com", now_ms() + 1),
            None,
            "revoked"
        );
        let (cookie, _) = s.open("127.0.0.1:9", 0).unwrap();
        // This machine on any port: the port may change at a restart.
        assert!(s.check(Some(&cookie), "localhost:10", 0).is_some());
        s.close(&cookie);
        assert_eq!(s.check(Some(&cookie), "127.0.0.1:9", 0), None);

        let quick = sessions(&dir, Duration::from_millis(1), Duration::from_secs(600));
        let (cookie, _) = quick.open("h", 0).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(quick.check(Some(&cookie), "h", 0), None);
        let short = sessions(&dir, Duration::from_secs(60), Duration::from_millis(1));
        let (cookie, _) = short.open("h", 0).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(short.check(Some(&cookie), "h", 0), None);
    }

    /// A restart is a new process on the same data dir: a new `Sessions`
    /// and `Links` on the same files.
    #[test]
    fn sessions_survive_a_restart_and_so_do_revocation_and_used_links() {
        let dir = tempfile::tempdir().unwrap();
        let links = Links::at(dir.path().join("private/dashboard/links.json"));
        let before = Sessions::beside(&links, Duration::from_secs(60), Duration::from_secs(600));
        let token = links.mint(None, Duration::from_secs(60)).unwrap();
        assert!(links.consume(&token).unwrap().is_some());
        let (cookie, csrf) = before.open("127.0.0.1:9", links.revoked_ms()).unwrap();
        let (gone, _) = before.open("127.0.0.1:9", links.revoked_ms()).unwrap();
        before.close(&gone);
        drop(before);

        let links = Links::at(dir.path().join("private/dashboard/links.json"));
        let after = Sessions::beside(&links, Duration::from_secs(60), Duration::from_secs(600));
        assert_eq!(
            after.check(Some(&cookie), "127.0.0.1:4000", links.revoked_ms()),
            Some(Granted { csrf }),
            "the login survived, on a new port"
        );
        assert_eq!(
            after.check(Some(&gone), "127.0.0.1:9", 0),
            None,
            "logged out stays out"
        );
        assert_eq!(
            links.consume(&token).unwrap(),
            None,
            "a used link stays used"
        );
        // The file has hashes only: neither the cookie nor its CSRF token.
        let text = std::fs::read_to_string(after.path()).unwrap();
        assert!(!text.contains(&cookie) && !text.contains(&csrf_of(&cookie)));

        // Revoked from another process (`ferrule dashboard off`), then a
        // restart: still revoked, whichever file is read first.
        links.revoke().unwrap();
        let again = Sessions::beside(&links, Duration::from_secs(60), Duration::from_secs(600));
        assert_eq!(
            again.check(Some(&cookie), "127.0.0.1:9", links.revoked_ms()),
            None
        );
        let (cookie, _) = again.open("127.0.0.1:9", links.revoked_ms()).unwrap();
        again.clear().unwrap();
        let last = Sessions::beside(&links, Duration::from_secs(60), Duration::from_secs(600));
        assert_eq!(
            last.check(Some(&cookie), "127.0.0.1:9", 0),
            None,
            "cleared on disk"
        );
    }

    #[test]
    fn tunnel_sessions_are_retired_after_a_restart_with_one_link_per_window() {
        let dir = tempfile::tempdir().unwrap();
        let s = sessions(&dir, Duration::from_secs(60), Duration::from_secs(600));
        assert_eq!(
            s.retire_tunnel_sessions(0, Duration::from_secs(600))
                .unwrap(),
            None
        );
        let (local, _) = s.open("127.0.0.1:9", 0).unwrap();
        s.open("a.trycloudflare.com", 0).unwrap();
        assert_eq!(
            s.retire_tunnel_sessions(0, Duration::from_secs(600))
                .unwrap(),
            Some(1)
        );
        assert_eq!(s.hosts(0), vec!["loopback".to_string()]);
        assert!(s.check(Some(&local), "127.0.0.1:9", 0).is_some());
        // Another restart soon after: the sessions still go, no new link.
        s.open("b.trycloudflare.com", 0).unwrap();
        assert_eq!(
            s.retire_tunnel_sessions(0, Duration::from_secs(600))
                .unwrap(),
            None
        );
        assert_eq!(s.live(0), 1);
        // A revoked one never counts as live.
        s.open("c.trycloudflare.com", 0).unwrap();
        assert_eq!(
            s.retire_tunnel_sessions(now_ms() + 1, Duration::ZERO)
                .unwrap(),
            None
        );
        s.remember_port(4321);
        assert_eq!(
            sessions(&dir, Duration::from_secs(60), Duration::from_secs(600)).last_port(),
            Some(4321)
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_session_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let s = sessions(&dir, Duration::from_secs(60), Duration::from_secs(600));
        s.open("h", 0).unwrap();
        let mode = std::fs::metadata(s.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn cookies_are_strict_and_http_only() {
        let c = set_cookie("v", true, Duration::from_secs(10));
        assert!(c.contains("HttpOnly") && c.contains("SameSite=Strict") && c.contains("Secure"));
        assert!(!set_cookie("v", false, Duration::from_secs(10)).contains("Secure"));
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
    }
}
