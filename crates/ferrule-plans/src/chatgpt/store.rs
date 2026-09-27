//! The ChatGPT sign-in at rest: `private/plans/chatgpt.json`, metadata in
//! the clear (status needs it without the key) and the three tokens sealed
//! with the M20 key under AAD `ferrule-plan:chatgpt`. Written by tmp and
//! rename, 0600 in a 0700 directory, which the sandbox hides from commands.

use crate::lock::FileLock;
use anyhow::{bail, Context, Result};
use ferrule_connections::seal::{write_private, Sealer};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const AAD: &str = "ferrule-plan:chatgpt";

/// The three tokens. Never printed.
#[derive(Clone, Serialize, Deserialize)]
pub struct Secrets {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub id_token: String,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secrets(…)")
    }
}

/// What the file says about the sign-in, readable without the key.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default)]
    pub fedramp: bool,
    /// The access token's `exp`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    pub signed_in_at: u64,
    pub last_refresh: u64,
    /// A refresh was refused for good: the tokens are kept for logout's
    /// revoke and never used again.
    #[serde(default)]
    pub signed_out: bool,
}

#[derive(Serialize, Deserialize)]
struct File {
    v: u32,
    #[serde(flatten)]
    meta: Meta,
    sealed: String,
}

#[derive(Debug, Clone)]
pub struct Record {
    pub meta: Meta,
    pub secrets: Secrets,
}

/// Refresh this long before the access token's `exp`.
pub const EARLY: u64 = 5 * 60;
/// With no `exp`, refresh after this long.
pub const STALE: u64 = 8 * 24 * 3600;

impl Record {
    pub fn needs_refresh(&self, now: u64) -> bool {
        match self.meta.expires_at {
            Some(exp) => exp <= now + EARLY,
            None => now >= self.meta.last_refresh + STALE,
        }
    }
    /// Still usable as it is, even if a refresh is due.
    pub fn usable(&self, now: u64) -> bool {
        self.meta.expires_at.is_none_or(|exp| exp > now)
    }
}

pub struct Store {
    dir: PathBuf,
    key_path: PathBuf,
}

impl Store {
    /// The store under ferrule's `private/` directory.
    pub fn new(private: &Path) -> Self {
        Self {
            dir: private.join("plans"),
            key_path: private.join("connections.key"),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join("chatgpt.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.join("chatgpt.lock")
    }

    pub(crate) async fn lock(&self) -> Result<FileLock> {
        FileLock::take(&self.lock_path()).await
    }

    fn file(&self) -> Result<Option<File>> {
        match std::fs::read(self.path()) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).with_context(|| {
                format!(
                    "{} doesn't parse; run `ferrule logout chatgpt` and sign in again",
                    self.path().display()
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path().display())),
        }
    }

    /// The metadata only: what status and doctor show. Never needs the key.
    pub fn meta(&self) -> Result<Option<Meta>> {
        Ok(self.file()?.map(|f| f.meta))
    }

    /// The whole record, tokens opened.
    pub fn load(&self) -> Result<Option<Record>> {
        let Some(file) = self.file()? else {
            return Ok(None);
        };
        let Some(sealer) = Sealer::load(&self.key_path)? else {
            bail!(
                "the key that seals the ChatGPT sign-in is missing ({}); run `ferrule login chatgpt` again",
                self.key_path.display()
            );
        };
        let plain = sealer
            .open(AAD, &file.sealed)
            .context("the ChatGPT sign-in can't be opened; run `ferrule login chatgpt` again")?;
        let secrets: Secrets =
            serde_json::from_slice(&plain).context("the sealed ChatGPT tokens don't parse")?;
        Ok(Some(Record {
            meta: file.meta,
            secrets,
        }))
    }

    pub fn save(&self, record: &Record) -> Result<()> {
        let sealer = Sealer::load_or_create(&self.key_path)?;
        let file = File {
            v: 1,
            meta: record.meta.clone(),
            sealed: sealer.seal(AAD, &serde_json::to_vec(&record.secrets)?),
        };
        write_private(&self.path(), &serde_json::to_vec_pretty(&file)?)
    }

    /// `true` when there was something to delete.
    pub fn delete(&self) -> Result<bool> {
        match std::fs::remove_file(self.path()) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("removing {}", self.path().display())),
        }
    }
}
