//! The Codex client version ferrule speaks as (M36, docs/m36-self-update.md
//! §1). The ChatGPT backend refuses a model newer than the client asking
//! for it, and a new model needs a new Codex release, so the version is
//! learned instead of frozen at build time: from npm, then GitHub, then the
//! compiled-in [`CLIENT_VERSION`]. `FERRULE_CODEX_CLIENT_VERSION` wins over
//! all of them and turns the learning off.
//!
//! The learned version is cached in `<data>/codex-client-version.json` and
//! refreshed in the background once it's a day old; a turn never waits on
//! npm, except for the one refresh a "requires a newer version" answer
//! asks for ([`ClientIdentity::refresh_past`]).

use super::CLIENT_VERSION;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, info};

pub const NPM_URL: &str = "https://registry.npmjs.org/-/package/@openai/codex/dist-tags";
pub const GITHUB_URL: &str = "https://api.github.com/repos/openai/codex/releases/latest";
/// The override, as before M36.
pub const ENV_OVERRIDE: &str = "FERRULE_CODEX_CLIENT_VERSION";
/// `npm_url,github_url`, for tests of the binary: where the version is
/// learned from ("" turns the learning off).
pub const ENV_SOURCES: &str = "FERRULE_CODEX_VERSION_SOURCES";
const CACHE_FILE: &str = "codex-client-version.json";
/// How old a learned version may be before it's checked again.
pub const TTL: Duration = Duration::from_secs(24 * 3600);
/// After a failed check, how long until the next background one.
const RETRY_FAILED: Duration = Duration::from_secs(3600);
/// Two forced refreshes (a burst of "too old" answers) share one fetch.
const FORCED_GAP: Duration = Duration::from_secs(60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// The minimum client version of the models ferrule knows without asking,
/// at the Codex pin (docs/m36-self-update.md §1.1).
pub const BUILTIN_MINIMUMS: &[(&str, &str)] = &[
    ("gpt-5.5", "0.124.0"),
    ("gpt-5.6-sol", "0.144.0"),
    ("gpt-5.6-terra", "0.144.0"),
    ("gpt-5.6-luna", "0.144.0"),
    ("gpt-6-astra", "0.153.0"),
    ("gpt-6-sol", "0.155.0"),
    ("gpt-6-luna", "0.155.0"),
];

/// What the cache file holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Learned {
    pub version: String,
    /// Unix seconds.
    pub checked_at: u64,
    /// "npm", "github" or "builtin".
    pub source: String,
}

/// Where the version is learned from.
#[derive(Debug, Clone)]
pub struct Sources {
    pub npm: String,
    pub github: String,
}

impl Default for Sources {
    fn default() -> Self {
        Self {
            npm: NPM_URL.into(),
            github: GITHUB_URL.into(),
        }
    }
}

/// The learned client version, shared by every Codex driver in a process.
pub struct ClientIdentity {
    cache: Option<PathBuf>,
    /// `None`: never fetch (the default until [`configure`] says otherwise).
    sources: Option<Sources>,
    overridden: Option<String>,
    learned: RwLock<Option<Learned>>,
    refreshing: AtomicBool,
    last_attempt: Mutex<Option<Instant>>,
    fetch_lock: tokio::sync::Mutex<()>,
    client: reqwest::Client,
}

impl ClientIdentity {
    /// An identity that learns from `sources` (none: never fetches) and
    /// caches in `data_dir`.
    pub fn new(data_dir: Option<&Path>, sources: Option<Sources>) -> Self {
        let cache = data_dir.map(|d| d.join(CACHE_FILE));
        let learned = cache.as_deref().and_then(read_cache);
        Self {
            cache,
            sources,
            overridden: None,
            learned: RwLock::new(learned),
            refreshing: AtomicBool::new(false),
            last_attempt: Mutex::new(None),
            fetch_lock: tokio::sync::Mutex::new(()),
            client: crate::common::client(),
        }
    }

    /// Fetch with `client` (a test's, trusting another CA).
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Pin the version, as `FERRULE_CODEX_CLIENT_VERSION` does.
    pub fn with_override(mut self, version: Option<String>) -> Self {
        self.overridden = version
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        self
    }

    /// The version to send now. A stale one is refreshed in the
    /// background; this never waits on the network.
    pub fn current(self: &Arc<Self>) -> String {
        if let Some(v) = &self.overridden {
            return v.clone();
        }
        let learned = self.learned();
        let fresh = learned
            .as_ref()
            .is_some_and(|l| now().saturating_sub(l.checked_at) < TTL.as_secs());
        if !fresh {
            self.refresh_in_background();
        }
        learned
            .map(|l| l.version)
            .filter(|v| acceptable(v))
            .unwrap_or_else(|| CLIENT_VERSION.to_string())
    }

    /// What was learned, if anything.
    pub fn learned(&self) -> Option<Learned> {
        self.learned
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Check now, ignoring the cache's age: a request sent as `sent` was
    /// refused as too old. `Some(version)` when there's a newer one to
    /// retry with; `None` when nothing newer could be learned (or the
    /// version is pinned).
    pub async fn refresh_past(self: &Arc<Self>, sent: &str) -> Option<String> {
        if self.overridden.is_some() || self.sources.is_none() {
            return None;
        }
        {
            let _one = self.fetch_lock.lock().await;
            // A burst of refusals shares one fetch.
            let recent = self
                .last_attempt
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some_and(|at| at.elapsed() < FORCED_GAP);
            if !recent {
                self.fetch_and_store().await;
            }
        }
        let now = self.current_quiet();
        newer(&now, sent).then_some(now)
    }

    /// [`Self::current`] without starting a refresh.
    fn current_quiet(&self) -> String {
        if let Some(v) = &self.overridden {
            return v.clone();
        }
        self.learned()
            .map(|l| l.version)
            .filter(|v| acceptable(v))
            .unwrap_or_else(|| CLIENT_VERSION.to_string())
    }

    fn refresh_in_background(self: &Arc<Self>) {
        if self.sources.is_none() {
            return;
        }
        let failed_lately = self
            .last_attempt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|at| at.elapsed() < RETRY_FAILED);
        if failed_lately {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.clone();
        rt.spawn(async move {
            {
                let _one = me.fetch_lock.lock().await;
                me.fetch_and_store().await;
            }
            me.refreshing.store(false, Ordering::Release);
        });
    }

    /// One check: npm, then GitHub. Stores what it learns.
    async fn fetch_and_store(&self) {
        let Some(sources) = &self.sources else {
            return;
        };
        *self.last_attempt.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        let found = match fetch(&self.client, &sources.npm, npm_version).await {
            Some(v) => Some((v, "npm")),
            None => fetch(&self.client, &sources.github, github_version)
                .await
                .map(|v| (v, "github")),
        };
        let Some((version, source)) = found else {
            debug!("the Codex client version couldn't be learned; keeping what we have");
            return;
        };
        let learned = Learned {
            version,
            checked_at: now(),
            source: source.into(),
        };
        let old = self.current_quiet();
        if newer(&learned.version, &old) {
            info!(from = %old, to = %learned.version, source, "Codex client version learned");
        }
        if let Some(path) = &self.cache {
            write_cache(path, &learned);
        }
        *self.learned.write().unwrap_or_else(|e| e.into_inner()) = Some(learned);
    }
}

static GLOBAL: OnceLock<RwLock<Arc<ClientIdentity>>> = OnceLock::new();

fn global_cell() -> &'static RwLock<Arc<ClientIdentity>> {
    GLOBAL.get_or_init(|| RwLock::new(Arc::new(from_env(None, false))))
}

/// The process's identity. Until [`configure`] runs it never fetches, so
/// a library user or a test isn't surprised by network calls.
pub fn global() -> Arc<ClientIdentity> {
    global_cell()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Learn the version (unless `learn` is false) and cache it in
/// `data_dir`. The CLI calls it once at start.
pub fn configure(data_dir: Option<&Path>, learn: bool) {
    *global_cell().write().unwrap_or_else(|e| e.into_inner()) = Arc::new(from_env(data_dir, learn));
}

fn from_env(data_dir: Option<&Path>, learn: bool) -> ClientIdentity {
    let sources = match std::env::var(ENV_SOURCES) {
        Ok(s) if s.trim().is_empty() => None,
        Ok(s) => {
            let mut it = s.split(',').map(|u| u.trim().to_string());
            Some(Sources {
                npm: it.next().unwrap_or_default(),
                github: it.next().unwrap_or_default(),
            })
        }
        Err(_) => learn.then(Sources::default),
    };
    ClientIdentity::new(data_dir, sources).with_override(std::env::var(ENV_OVERRIDE).ok())
}

/// Whether `model` is known to need a newer client than `version`. A model
/// ferrule doesn't know is assumed usable: the backend has the last word.
pub fn builtin_usable(model: &str, version: &str) -> bool {
    BUILTIN_MINIMUMS
        .iter()
        .find(|(m, _)| *m == model)
        .is_none_or(|(_, min)| !newer(min, version))
}

/// `MAJOR.MINOR.PATCH`, cut from anything longer (`0.158.0-alpha.1` →
/// `0.158.0`): what `/models?client_version=` takes.
pub fn triple(version: &str) -> String {
    parse(version)
        .map(|(a, b, c)| format!("{a}.{b}.{c}"))
        .unwrap_or_else(|| version.to_string())
}

/// `a > b`, as versions; anything that doesn't parse is never newer.
pub fn newer(a: &str, b: &str) -> bool {
    match (parse(a), parse(b)) {
        (Some(a), Some(b)) => a > b,
        (Some(_), None) => true,
        _ => false,
    }
}

fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    let t = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(t)
}

/// A release (no pre-release part) not below the compiled-in version.
fn acceptable(v: &str) -> bool {
    let v = v.trim();
    !v.contains(['-', '+']) && parse(v).is_some() && !newer(CLIENT_VERSION, v)
}

fn npm_version(body: &Value) -> Option<String> {
    body["latest"].as_str().map(str::to_string)
}

fn github_version(body: &Value) -> Option<String> {
    if body["prerelease"].as_bool() == Some(true) || body["draft"].as_bool() == Some(true) {
        return None;
    }
    let tag = body["tag_name"].as_str()?;
    Some(tag.strip_prefix("rust-v").unwrap_or(tag).to_string())
}

async fn fetch(
    client: &reqwest::Client,
    url: &str,
    read: fn(&Value) -> Option<String>,
) -> Option<String> {
    if url.is_empty() {
        return None;
    }
    let resp = client
        .get(url)
        .header(reqwest::header::USER_AGENT, "ferrule")
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| debug!(error = %e.without_url(), "Codex version check failed"))
        .ok()?;
    if !resp.status().is_success() {
        debug!(status = %resp.status(), "Codex version check refused");
        return None;
    }
    let body: Value = resp.json().await.ok()?;
    read(&body).filter(|v| acceptable(v))
}

fn read_cache(path: &Path) -> Option<Learned> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Learned>(&text)
        .ok()
        .filter(|l| acceptable(&l.version))
}

fn write_cache(path: &Path, learned: &Learned) {
    let Ok(text) = serde_json::to_string_pretty(learned) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    let done = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, path));
    if let Err(e) = done {
        debug!(error = %e, path = %path.display(), "couldn't cache the Codex client version");
        let _ = std::fs::remove_file(&tmp);
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::mock::{ok, serve, status};

    fn sources(npm: &str, github: &str) -> Option<Sources> {
        Some(Sources {
            npm: npm.into(),
            github: github.into(),
        })
    }

    /// The mock's base ends in `/v1`; any path under it answers.
    fn at(base: &str, path: &str) -> String {
        format!("{base}/{path}")
    }

    async fn settled(id: &Arc<ClientIdentity>) {
        for _ in 0..200 {
            if !id.refreshing.load(Ordering::Acquire) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the background refresh never finished");
    }

    #[test]
    fn versions_compare_and_cut() {
        assert!(newer("0.158.0", "0.157.1"));
        assert!(newer("0.157.10", "0.157.9"));
        assert!(!newer("0.157.1", "0.157.1"));
        assert!(!newer("junk", "0.1.0"));
        assert_eq!(triple("0.158.0-alpha.1"), "0.158.0");
        assert!(!acceptable("0.200.0-alpha.2"));
        assert!(!acceptable("0.100.0"), "below the compiled-in version");
        assert!(acceptable(CLIENT_VERSION));
        assert!(builtin_usable("gpt-6-sol", "0.155.0"));
        assert!(!builtin_usable("gpt-6-sol", "0.154.9"));
        assert!(
            builtin_usable("gpt-9-new", "0.1.0"),
            "unknown: ask the server"
        );
    }

    #[tokio::test]
    async fn it_learns_from_npm_and_caches_it() {
        let dir = tempfile::tempdir().unwrap();
        let (base, served) = serve(vec![ok(
            r#"{"latest":"0.170.2","alpha":"0.171.0-alpha.1"}"#,
        )]);
        let id = Arc::new(ClientIdentity::new(
            Some(dir.path()),
            sources(&at(&base, "npm"), &at(&base, "gh")),
        ));
        // The first call doesn't wait: it answers with what it has.
        assert_eq!(id.current(), CLIENT_VERSION);
        settled(&id).await;
        assert_eq!(id.current(), "0.170.2");
        assert_eq!(served.join().unwrap().len(), 1, "GitHub wasn't needed");
        let cached = read_cache(&dir.path().join(CACHE_FILE)).unwrap();
        assert_eq!(
            (cached.version.as_str(), cached.source.as_str()),
            ("0.170.2", "npm")
        );

        // A new process reads the cache and, within the TTL, fetches nothing.
        let again = Arc::new(ClientIdentity::new(
            Some(dir.path()),
            sources("http://127.0.0.1:9/never", ""),
        ));
        assert_eq!(again.current(), "0.170.2");
        assert!(!again.refreshing.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn a_stale_cache_is_refreshed_and_github_backs_npm_up() {
        let dir = tempfile::tempdir().unwrap();
        write_cache(
            &dir.path().join(CACHE_FILE),
            &Learned {
                version: "0.160.0".into(),
                checked_at: now() - TTL.as_secs() - 5,
                source: "npm".into(),
            },
        );
        let (base, served) = serve(vec![
            status("503 Service Unavailable", "", "{}"),
            ok(r#"{"tag_name":"rust-v0.165.0","prerelease":false}"#),
        ]);
        let id = Arc::new(ClientIdentity::new(
            Some(dir.path()),
            sources(&at(&base, "npm"), &at(&base, "gh")),
        ));
        assert_eq!(id.current(), "0.160.0", "the stale one is used meanwhile");
        settled(&id).await;
        assert_eq!(id.current(), "0.165.0");
        assert_eq!(id.learned().unwrap().source, "github");
        assert_eq!(served.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn offline_keeps_the_constant_and_a_backwards_source_is_ignored() {
        let id = Arc::new(ClientIdentity::new(
            None,
            sources("http://127.0.0.1:9/npm", "http://127.0.0.1:9/gh"),
        ));
        assert_eq!(id.current(), CLIENT_VERSION);
        settled(&id).await;
        assert_eq!(id.current(), CLIENT_VERSION);

        let (base, _served) = serve(vec![
            ok(r#"{"latest":"0.100.0"}"#),
            ok(r#"{"tag_name":"rust-v0.101.0"}"#),
        ]);
        let id = Arc::new(ClientIdentity::new(
            None,
            sources(&at(&base, "npm"), &at(&base, "gh")),
        ));
        assert_eq!(id.refresh_past(CLIENT_VERSION).await, None);
        assert_eq!(id.current(), CLIENT_VERSION);
    }

    #[tokio::test]
    async fn the_override_wins_and_fetches_nothing() {
        let id = Arc::new(
            ClientIdentity::new(None, sources("http://127.0.0.1:9/npm", ""))
                .with_override(Some(" 0.150.0 ".into())),
        );
        assert_eq!(id.current(), "0.150.0");
        assert!(!id.refreshing.load(Ordering::Acquire));
        assert_eq!(id.refresh_past("0.150.0").await, None);
    }

    #[tokio::test]
    async fn a_forced_refresh_says_whether_the_version_moved() {
        let (base, served) = serve(vec![ok(r#"{"latest":"0.180.0"}"#)]);
        let id = Arc::new(ClientIdentity::new(None, sources(&at(&base, "npm"), "")));
        assert_eq!(
            id.refresh_past(CLIENT_VERSION).await.as_deref(),
            Some("0.180.0")
        );
        // A second one within a minute shares the first's answer: newer
        // than what that request sent, not newer than itself.
        assert_eq!(
            id.refresh_past(CLIENT_VERSION).await.as_deref(),
            Some("0.180.0")
        );
        assert_eq!(id.refresh_past("0.180.0").await, None);
        assert_eq!(served.join().unwrap().len(), 1);
    }

    #[test]
    fn without_configure_nothing_is_fetched() {
        assert!(global().sources.is_none() || std::env::var(ENV_SOURCES).is_ok());
    }
}
