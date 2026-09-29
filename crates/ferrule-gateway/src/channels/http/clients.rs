//! The HTTP API's clients (M39 §8): `<dir>/clients.json` (0600), one entry
//! per program allowed in. A key is `frk_` + 43 base64url characters (32
//! random bytes), shown once and kept only as its SHA-256; a webhook's
//! signing secret (`frw_…`) is kept as it is, since the gateway signs with
//! it. The CLI and the dashboard write this file; the gateway re-reads it
//! when it changes. When each client was last seen is the gateway's own
//! `state.json`, so the two never write the same file.

use crate::channels::hmac::{b64url, hex, same, sha256};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const FILE: &str = "clients.json";
const KEY_PREFIX: &str = "frk_";
const SECRET_PREFIX: &str = "frw_";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Client {
    pub name: String,
    /// Lower-case hex SHA-256 of the key.
    pub key_sha256: String,
    /// Unix seconds.
    pub created: i64,
    /// Where outbox messages are POSTed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<String>,
    /// The webhook's HMAC key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_secret: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Doc {
    #[serde(default)]
    clients: Vec<Client>,
}

/// What `add` made: shown once, never stored as it is (the key).
#[derive(Debug)]
pub struct Created {
    pub key: String,
    pub webhook_secret: Option<String>,
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// Every client; none when the file doesn't exist yet.
pub fn load(dir: &Path) -> Result<Vec<Client>, String> {
    match std::fs::read_to_string(path(dir)) {
        Ok(s) => serde_json::from_str::<Doc>(&s)
            .map(|d| d.clients)
            .map_err(|e| format!("{} can't be read: {e}", path(dir).display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(format!("{}: {e}", path(dir).display())),
    }
}

fn save(dir: &Path, clients: &[Client]) -> Result<(), String> {
    let doc = Doc {
        clients: clients.to_vec(),
    };
    let body = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    write_private(dir, FILE, body.as_bytes()).map_err(|e| format!("{}: {e}", path(dir).display()))
}

/// Writes `<dir>/<name>` through a temporary file, 0600 on Unix.
pub(super) fn write_private(dir: &Path, name: &str, body: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{name}.tmp"));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    opts.open(&tmp)?.write_all(body)?;
    std::fs::rename(&tmp, dir.join(name))
}

/// A client's name: 1–32 of `a-z 0-9 - _`, since it's also its chat id
/// (and `/` starts a conversation).
pub fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{name}` isn't a client name: 1–32 of a-z, 0-9, - and _"
        ))
    }
}

/// A webhook must be `https://`, or `http://` to this machine.
pub fn check_webhook(url: &str) -> Result<(), String> {
    let Ok(u) = reqwest::Url::parse(url) else {
        return Err(format!("`{url}` isn't a URL"));
    };
    let host = u.host_str().unwrap_or("");
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = bare.eq_ignore_ascii_case("localhost")
        || bare
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    match u.scheme() {
        "https" if !host.is_empty() => Ok(()),
        "http" if loopback => Ok(()),
        "http" => Err(format!(
            "a webhook must be https:// (plain http only to this machine), not `{url}`"
        )),
        _ => Err(format!("a webhook must be an https:// URL, not `{url}`")),
    }
}

fn token(prefix: &str) -> String {
    let bytes: [u8; 32] = ferrule_connections::seal::random();
    format!("{prefix}{}", b64url(&bytes))
}

/// The stored form of a key.
pub fn key_hash(key: &str) -> String {
    hex(&sha256(key.trim().as_bytes()))
}

/// A new client and its key (and a webhook secret with a webhook).
pub fn add(dir: &Path, name: &str, webhook: Option<&str>) -> Result<Created, String> {
    check_name(name)?;
    if let Some(w) = webhook {
        check_webhook(w)?;
    }
    let mut all = load(dir)?;
    if all.iter().any(|c| c.name == name) {
        return Err(format!(
            "there's already a client `{name}`: revoke it first, or pick another name"
        ));
    }
    let key = token(KEY_PREFIX);
    let webhook_secret = webhook.map(|_| token(SECRET_PREFIX));
    all.push(Client {
        name: name.to_string(),
        key_sha256: key_hash(&key),
        created: now(),
        webhook: webhook.map(str::to_string),
        webhook_secret: webhook_secret.clone(),
    });
    save(dir, &all)?;
    Ok(Created {
        key,
        webhook_secret,
    })
}

/// Sets or clears a client's webhook; a new one gets a new secret, returned.
pub fn set_webhook(
    dir: &Path,
    name: &str,
    webhook: Option<&str>,
) -> Result<Option<String>, String> {
    if let Some(w) = webhook {
        check_webhook(w)?;
    }
    let mut all = load(dir)?;
    let Some(c) = all.iter_mut().find(|c| c.name == name) else {
        return Err(format!("there's no client `{name}`"));
    };
    let secret = webhook.map(|_| token(SECRET_PREFIX));
    c.webhook = webhook.map(str::to_string);
    c.webhook_secret = secret.clone();
    save(dir, &all)?;
    Ok(secret)
}

/// Removes a client, so its key fails at once. `false`: there was none.
pub fn revoke(dir: &Path, name: &str) -> Result<bool, String> {
    let mut all = load(dir)?;
    let before = all.len();
    all.retain(|c| c.name != name);
    if all.len() == before {
        return Ok(false);
    }
    save(dir, &all)?;
    Ok(true)
}

/// The client a key belongs to, compared in constant time.
pub fn find<'a>(clients: &'a [Client], key: &str) -> Option<&'a Client> {
    let h = key_hash(key);
    let mut found = None;
    for c in clients {
        if same(&c.key_sha256, &h) {
            found = Some(c);
        }
    }
    found
}

/// When each client was last seen (unix seconds), from the gateway's
/// `state.json`.
pub fn last_used(dir: &Path) -> HashMap<String, i64> {
    std::fs::read_to_string(dir.join(super::STATE))
        .ok()
        .and_then(|s| serde_json::from_str::<super::State>(&s).ok())
        .map(|s| s.last_used)
        .unwrap_or_default()
}

pub(super) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_shown_once_and_kept_as_a_hash() {
        let dir = tempfile::tempdir().unwrap();
        let made = add(dir.path(), "shortcuts", None).unwrap();
        assert!(made.key.starts_with("frk_"));
        assert_eq!(made.key.len(), 4 + 43);
        assert!(made.webhook_secret.is_none());
        let raw = std::fs::read_to_string(path(dir.path())).unwrap();
        assert!(!raw.contains(&made.key));
        let all = load(dir.path()).unwrap();
        assert_eq!(find(&all, &made.key).unwrap().name, "shortcuts");
        assert!(find(&all, "frk_nope").is_none());
        assert!(add(dir.path(), "shortcuts", None).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path(dir.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(revoke(dir.path(), "shortcuts").unwrap());
        assert!(!revoke(dir.path(), "shortcuts").unwrap());
        assert!(find(&load(dir.path()).unwrap(), &made.key).is_none());
    }

    #[test]
    fn names_and_webhooks_are_checked() {
        assert!(check_name("ci-bot_2").is_ok());
        for bad in ["", "Ci", "a/b", "a b", &"x".repeat(33)] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
        assert!(check_webhook("https://example.com/hook").is_ok());
        assert!(check_webhook("http://127.0.0.1:9000/hook").is_ok());
        assert!(check_webhook("http://localhost/hook").is_ok());
        assert!(check_webhook("http://[::1]:9/h").is_ok());
        assert!(check_webhook("http://example.com/hook").is_err());
        assert!(check_webhook("ftp://example.com").is_err());
        assert!(check_webhook("nope").is_err());
        let dir = tempfile::tempdir().unwrap();
        let made = add(dir.path(), "ci", Some("https://example.com/h")).unwrap();
        assert!(made.webhook_secret.unwrap().starts_with("frw_"));
        let again = set_webhook(dir.path(), "ci", Some("https://example.com/2")).unwrap();
        assert!(again.is_some());
        assert_eq!(set_webhook(dir.path(), "ci", None).unwrap(), None);
        assert!(load(dir.path()).unwrap()[0].webhook.is_none());
    }
}
