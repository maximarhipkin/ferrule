//! M52.2: what the Safe says about each kept key besides its name: when it
//! was first saved, when its value last changed, and when and where the
//! credential proxy last sent it. `secrets.meta.json` beside the secrets
//! file (so in the private dir the sandbox hides), names and times only —
//! never a value. A key saved before M52.2 has no dates until its next
//! write; the page says "not recorded" rather than guessing.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const FILE: &str = "secrets.meta.json";
/// A use is written at most this often per name: the proxy hears every
/// request.
const USE_EVERY_SECS: i64 = 60;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Unix seconds of the first write this file saw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<i64>,
    /// The last write that replaced a value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotated: Option<i64>,
    /// The last request the proxy swapped it into, and the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_host: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Meta {
    v: u32,
    #[serde(default)]
    keys: BTreeMap<String, Entry>,
}

fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Every entry in `dir` (the secrets file's directory). A missing or
/// unreadable file is an empty one: these are notes, not the keys.
pub fn read(dir: &Path) -> BTreeMap<String, Entry> {
    std::fs::read(path(dir))
        .ok()
        .and_then(|b| serde_json::from_slice::<Meta>(&b).ok())
        .map(|m| m.keys)
        .unwrap_or_default()
}

/// One lock for every read-modify-write in this process; another
/// process's write in between can lose a timestamp, never a key.
static WRITING: Mutex<()> = Mutex::new(());

fn change(dir: &Path, f: impl FnOnce(&mut BTreeMap<String, Entry>)) {
    let _held = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let mut keys = read(dir);
    f(&mut keys);
    let meta = Meta { v: 1, keys };
    let Ok(text) = serde_json::to_string_pretty(&meta) else {
        return;
    };
    if let Err(e) = crate::secrets::write_private(&path(dir), &text) {
        tracing::debug!("{}: {e:#}", path(dir).display());
    }
}

/// `secrets::set` wrote `name` into the file in `dir`; `replaced`: it had a
/// value before.
pub fn wrote(dir: &Path, name: &str, replaced: bool) {
    let t = now();
    change(dir, |keys| {
        let e = keys.entry(name.to_string()).or_default();
        if replaced {
            e.rotated = Some(t);
        } else {
            e.created = Some(t);
            e.rotated = None;
        }
    });
}

/// `secrets::remove` dropped `name`.
pub fn forgot(dir: &Path, name: &str) {
    change(dir, |keys| {
        keys.remove(name);
    });
}

/// The proxy swapped `name` into a request to `host` (`secret.used`):
/// written for this process's own secrets file, at most once a minute per
/// name.
pub fn used(name: &str, host: &str) {
    static LAST: Mutex<Option<HashMap<String, i64>>> = Mutex::new(None);
    let t = now();
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let seen = last.get_or_insert_with(HashMap::new);
        if seen.get(name).is_some_and(|at| t - at < USE_EVERY_SECS) {
            return;
        }
        seen.insert(name.to_string(), t);
    }
    let Ok(dir) = crate::secrets::private_dir() else {
        return;
    };
    change(&dir, |keys| {
        let e = keys.entry(name.to_string()).or_default();
        e.last_used = Some(t);
        e.last_host = Some(host.to_string());
    });
}
