//! The secrets file's values, held in this process's memory instead of its
//! environment (M54.2).
//!
//! `ferrule-cli` loads `<data>/private/secrets.env` here at startup. Until
//! M54.2 it went into the process environment, where any process of the
//! same account could read it (`/proc/<pid>/environ`, `ps eww`) and every
//! child inherited it. Now consumers read a key through [`var`]: the value
//! saved while the process runs, else the real environment, else the
//! file's. A variable in the environment ferrule was started with still
//! wins over the file, as before.
//!
//! No child inherits a held value. A spawn passes one only by name: a
//! sandboxed command for an `env_passthrough` name (or every one with
//! `scrub_secret_env = false`, see [`crate::Sandbox::command`]), an owner's
//! hook for the names in its `env`.

use std::collections::BTreeMap;
use std::sync::RwLock;

static HELD: RwLock<BTreeMap<String, String>> = RwLock::new(BTreeMap::new());

fn read() -> std::sync::RwLockReadGuard<'static, BTreeMap<String, String>> {
    HELD.read().unwrap_or_else(|e| e.into_inner())
}

fn write() -> std::sync::RwLockWriteGuard<'static, BTreeMap<String, String>> {
    HELD.write().unwrap_or_else(|e| e.into_inner())
}

/// Holds the secrets file's entries, skipping every name the real
/// environment has (even empty): that one wins. Returns the names held.
pub fn load(entries: impl IntoIterator<Item = (String, String)>) -> Vec<String> {
    let mut held = write();
    let mut names = Vec::new();
    for (name, value) in entries {
        if std::env::var_os(&name).is_none() {
            held.insert(name.clone(), value);
            names.push(name);
        }
    }
    names
}

/// `name`'s value for this process: held (loaded from the file, or saved
/// since), else the real environment's.
pub fn var(name: &str) -> Option<String> {
    held(name).or_else(|| std::env::var(name).ok())
}

/// [`var`], without an empty value.
pub fn non_empty(name: &str) -> Option<String> {
    var(name).filter(|v| !v.is_empty())
}

/// The held value alone, never the environment's.
pub fn held(name: &str) -> Option<String> {
    read().get(name).cloned()
}

/// The held names, sorted.
pub fn names() -> Vec<String> {
    read().keys().cloned().collect()
}

/// The held names and values, sorted by name.
pub fn pairs() -> Vec<(String, String)> {
    read().iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// A key saved while the process runs (the dashboard's forms): later
/// reads get it, over the environment too, as `set_var` gave before.
pub fn set(name: &str, value: &str) {
    write().insert(name.to_string(), value.to_string());
}

/// Drops a held value (its key was deleted from the file).
pub fn forget(name: &str) {
    write().remove(name);
}
