//! `<data>/private/connections/connections.json`: what's connected, with
//! every token, key and client secret sealed (see `seal`). Written through
//! a temp file and a rename, changed only under a lock file, and never
//! overwritten while it doesn't parse.

use crate::catalog::Service;
use crate::seal::{self, Sealer};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Connected,
    /// The grant is gone (revoked, expired): tools suspended until the
    /// owner reconnects.
    NeedsReconnect,
}

/// OAuth details that aren't secret: where to refresh and revoke.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct OauthMeta {
    pub token_endpoint: String,
    #[serde(default)]
    pub revocation_endpoint: Option<String>,
    pub client_id: String,
    /// `resource` for refreshes, when the service takes it.
    #[serde(default)]
    pub resource: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Record {
    pub name: String,
    pub service: Service,
    pub write: bool,
    /// Asked for; `granted` is what the token response said, if it did.
    pub scopes: Vec<String>,
    #[serde(default)]
    pub granted: Option<String>,
    pub state: State,
    pub connected_at: u64,
    #[serde(default)]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub refreshed_at: Option<u64>,
    /// A fixed phrase, never a server's text.
    #[serde(default)]
    pub last_error: Option<String>,
    /// The reconnect notice for the current break was sent.
    #[serde(default)]
    pub notice_sent: bool,
    #[serde(default)]
    pub tools: Option<usize>,
    /// "relay", "tunnel", "paste" or "terminal".
    pub via: String,
    /// Who asked: "owner", "terminal" or "agent".
    pub requested_by: String,
    #[serde(default)]
    pub oauth: Option<OauthMeta>,
    /// Sealed [`Secret`] JSON, with `name` as associated data.
    pub sealed: String,
}

/// What's sealed.
#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Secret {
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

#[derive(Default, Deserialize, Serialize)]
struct File {
    v: u32,
    #[serde(default)]
    connections: Vec<Record>,
}

pub struct Store {
    dir: PathBuf,
    key_path: PathBuf,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Store {
    /// `private` is ferrule's private dir (`<data>/private`).
    pub fn new(private: &Path) -> Self {
        Self {
            dir: private.join("connections"),
            key_path: private.join("connections.key"),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join("connections.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.join("connections.lock")
    }

    /// Every connection. A missing store is an empty one; one that doesn't
    /// parse is an error (and `save` won't replace it).
    pub fn load(&self) -> Result<Vec<Record>> {
        match std::fs::read(self.path()) {
            Ok(bytes) => {
                let file: File = serde_json::from_slice(&bytes).with_context(|| {
                    format!(
                        "{} doesn't parse; fix or remove it (nothing overwrites it until then)",
                        self.path().display()
                    )
                })?;
                Ok(file.connections)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path().display())),
        }
    }

    /// A cheap fingerprint of the file, to notice another process's write.
    pub fn stamp(&self) -> Option<String> {
        std::fs::read(self.path())
            .ok()
            .map(|b| seal::sha256_b64(&b))
    }

    fn save(&self, records: &[Record]) -> Result<()> {
        if self.path().exists() {
            // Refuses to replace a file that doesn't parse.
            self.load()?;
        }
        let file = File {
            v: 1,
            connections: records.to_vec(),
        };
        seal::write_private(&self.path(), &serde_json::to_vec_pretty(&file)?)
    }

    /// Change the store under the lock: `f` gets what's on disk now.
    pub async fn update<T>(&self, f: impl FnOnce(&mut Vec<Record>) -> Result<T>) -> Result<T> {
        let _lock = self.lock().await?;
        let mut records = self.load()?;
        let out = f(&mut records)?;
        self.save(&records)?;
        Ok(out)
    }

    /// Save while already holding the lock (a change that awaits between
    /// reading and writing, like a token refresh).
    pub fn save_locked(&self, _held: &LockGuard, records: &[Record]) -> Result<()> {
        self.save(records)
    }

    pub fn sealer(&self) -> Result<Sealer> {
        Sealer::load_or_create(&self.key_path)
    }

    /// The key if there is one; never made. A store whose key is gone
    /// can't open its secrets.
    pub fn existing_sealer(&self) -> Result<Option<Sealer>> {
        Sealer::load(&self.key_path)
    }

    pub fn seal(&self, name: &str, secret: &Secret) -> Result<String> {
        Ok(self.sealer()?.seal(name, &serde_json::to_vec(secret)?))
    }

    pub fn open(&self, record: &Record) -> Result<Secret> {
        let Some(sealer) = self.existing_sealer()? else {
            bail!("the connections key is missing");
        };
        let plain = sealer.open(&record.name, &record.sealed)?;
        Ok(serde_json::from_slice(&plain)?)
    }

    /// Serializes changes across processes: a lock file made with
    /// `create_new`. One older than 30 s belonged to a process that died
    /// holding it and is taken over. Gives up after 20 s.
    pub async fn lock(&self) -> Result<LockGuard> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let path = self.lock_path();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(LockGuard { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > Duration::from_secs(30));
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if std::time::Instant::now() > deadline {
                        bail!("the connections store is locked ({})", path.display());
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => return Err(e).with_context(|| format!("locking {}", path.display())),
            }
        }
    }
}

pub struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;

    fn record(store: &Store, name: &str, token: &str) -> Record {
        let secret = Secret {
            access_token: Some(token.into()),
            ..Default::default()
        };
        Record {
            name: name.into(),
            service: Catalog::built_in().get("notion").unwrap().clone(),
            write: false,
            scopes: vec![],
            granted: None,
            state: State::Connected,
            connected_at: now(),
            expires_at: None,
            refreshed_at: None,
            last_error: None,
            notice_sent: false,
            tools: None,
            via: "relay".into(),
            requested_by: "owner".into(),
            oauth: None,
            sealed: store.seal(name, &secret).unwrap(),
        }
    }

    #[tokio::test]
    async fn tokens_are_sealed_on_disk_and_open_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let r = record(&store, "notion", "ntn_SECRET_TOKEN");
        store
            .update(|all| {
                all.push(r.clone());
                Ok(())
            })
            .await
            .unwrap();
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(!text.contains("ntn_SECRET_TOKEN"));
        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            store.open(&loaded[0]).unwrap().access_token.as_deref(),
            Some("ntn_SECRET_TOKEN")
        );
        assert!(!store.lock_path().exists(), "the lock is released");
        assert!(!format!("{:?}", store.open(&loaded[0]).unwrap()).contains("ntn_"));
    }

    #[tokio::test]
    async fn a_corrupt_store_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        std::fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        std::fs::write(store.path(), "{ hand edited").unwrap();
        assert!(store.load().is_err());
        assert!(store.update(|_| Ok(())).await.is_err());
        assert_eq!(
            std::fs::read_to_string(store.path()).unwrap(),
            "{ hand edited"
        );
    }

    #[tokio::test]
    async fn a_lost_key_means_secrets_cant_open_but_nothing_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let r = record(&store, "notion", "t");
        store
            .update(|all| {
                all.push(r);
                Ok(())
            })
            .await
            .unwrap();
        std::fs::remove_file(dir.path().join("connections.key")).unwrap();
        let loaded = store.load().unwrap();
        assert!(store.open(&loaded[0]).is_err());
        assert_eq!(store.load().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_lock_serializes_and_a_stale_one_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let held = store.lock().await.unwrap();
        let started = std::time::Instant::now();
        let waiter = {
            let store = Store::new(dir.path());
            tokio::spawn(async move {
                let _g = store.lock().await.unwrap();
            })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!waiter.is_finished());
        drop(held);
        waiter.await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(200));

        // A lock left by a dead process.
        std::fs::write(store.lock_path(), "").unwrap();
        let old = SystemTime::now() - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(store.lock_path())
            .unwrap()
            .set_modified(old)
            .unwrap();
        let _g = store.lock().await.unwrap();
    }
}
