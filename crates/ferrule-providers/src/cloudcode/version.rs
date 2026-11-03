//! The Antigravity / Cloud Code client version ferrule speaks as.
//! Starts at [`CLIENT_VERSION`] (`1.4.12`), caches learned versions in
//! `<data>/gemini-client-version.json`, and honors
//! `FERRULE_GEMINI_CLIENT_VERSION` when set.

use super::CLIENT_VERSION;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub const ENV_OVERRIDE: &str = "FERRULE_GEMINI_CLIENT_VERSION";
const CACHE_FILE: &str = "gemini-client-version.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Learned {
    pub version: String,
    pub checked_at: u64,
    pub source: String,
}

pub struct ClientIdentity {
    cache: Option<PathBuf>,
    overridden: Option<String>,
    learned: RwLock<Option<Learned>>,
}

impl ClientIdentity {
    pub fn new(data_dir: Option<&Path>) -> Self {
        let cache = data_dir.map(|d| d.join(CACHE_FILE));
        let learned = cache.as_deref().and_then(read_cache);
        let overridden = std::env::var(ENV_OVERRIDE)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        Self {
            cache,
            overridden,
            learned: RwLock::new(learned),
        }
    }

    pub fn with_override(mut self, version: Option<String>) -> Self {
        self.overridden = version
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        self
    }

    pub fn current(&self) -> String {
        if let Some(v) = &self.overridden {
            return v.clone();
        }
        self.learned
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|l| l.version.clone())
            .filter(|v| newer(v, CLIENT_VERSION) || v == CLIENT_VERSION)
            .unwrap_or_else(|| CLIENT_VERSION.to_string())
    }

    pub fn record(&self, version: &str, source: &str) {
        if !newer(version, &self.current()) {
            return;
        }
        let entry = Learned {
            version: version.to_string(),
            checked_at: now(),
            source: source.to_string(),
        };
        if let Some(path) = &self.cache {
            if let Ok(bytes) = serde_json::to_vec_pretty(&entry) {
                let _ = std::fs::write(path, bytes);
            }
        }
        *self.learned.write().unwrap_or_else(|e| e.into_inner()) = Some(entry);
    }
}

static GLOBAL: OnceLock<Arc<ClientIdentity>> = OnceLock::new();

pub fn global() -> Arc<ClientIdentity> {
    GLOBAL
        .get_or_init(|| Arc::new(ClientIdentity::new(None)))
        .clone()
}

pub fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().strip_prefix('v').unwrap_or(v.trim());
    let core = v.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

pub fn newer(a: &str, b: &str) -> bool {
    match (parse(a), parse(b)) {
        (Some(pa), Some(pb)) => pa > pb,
        _ => false,
    }
}

fn read_cache(path: &Path) -> Option<Learned> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
