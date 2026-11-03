//! The Gemini / Antigravity sign-in at rest: `private/plans/gemini.json`,
//! metadata in the clear (status needs it without the key) and the tokens
//! sealed with the M20 key under AAD `ferrule-plan:gemini`. Written by tmp
//! and rename, 0600 in a 0700 directory, which the sandbox hides from commands.

use crate::lock::FileLock;
use anyhow::{bail, Context, Result};
use ferrule_connections::seal::{write_private, Sealer};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

const AAD: &str = "ferrule-plan:gemini";

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
    /// `cloudaicompanionProject` from `v1internal:loadCodeAssist`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Tier display name or ID (e.g. `Antigravity`, `Google AI Pro`, `free-tier`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// The access token's `exp` (unix seconds).
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
pub const STALE: u64 = 50 * 60;

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
        self.dir.join("gemini.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.join("gemini.lock")
    }

    /// Present once the owner signed out here: the CLI's token is never
    /// imported again (only an explicit sign-in writes a record).
    fn no_import_path(&self) -> PathBuf {
        self.dir.join("gemini.no-cli-import")
    }

    pub(crate) async fn lock(&self) -> Result<FileLock> {
        FileLock::take(&self.lock_path()).await
    }

    fn file(&self) -> Result<Option<File>> {
        match std::fs::read(self.path()) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).with_context(|| {
                format!(
                    "{} doesn't parse; run `ferrule logout gemini` and sign in again",
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
                "the key that seals the Gemini sign-in is missing ({}); run `ferrule login gemini` again",
                self.key_path.display()
            );
        };
        let plain = sealer
            .open(AAD, &file.sealed)
            .context("the Gemini sign-in can't be opened; run `ferrule login gemini` again")?;
        let secrets: Secrets =
            serde_json::from_slice(&plain).context("the sealed Gemini tokens don't parse")?;
        Ok(Some(Record {
            meta: file.meta,
            secrets,
        }))
    }

    pub fn save(&self, record: &Record) -> Result<()> {
        // An explicit sign-in (or import) lifts a past logout's opt-out.
        let _ = std::fs::remove_file(self.no_import_path());
        let sealer = Sealer::load_or_create(&self.key_path)?;
        let file = File {
            v: 1,
            meta: record.meta.clone(),
            sealed: sealer.seal(AAD, &serde_json::to_vec(&record.secrets)?),
        };
        write_private(&self.path(), &serde_json::to_vec_pretty(&file)?)
    }

    /// Sign-out: deletes the record and remembers not to import the CLI's
    /// token again, so a logout sticks. `true` when there was something
    /// to delete.
    pub fn forget(&self) -> Result<bool> {
        let had = self.delete()?;
        write_private(
            &self.no_import_path(),
            b"signed out; `ferrule login gemini` signs in again\n",
        )?;
        Ok(had)
    }

    /// `true` when there was something to delete.
    pub fn delete(&self) -> Result<bool> {
        match std::fs::remove_file(self.path()) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("removing {}", self.path().display())),
        }
    }

    /// Imports an existing Antigravity CLI (`antigravity-oauth-token`) token
    /// from `config_dir` (or `~/.gemini/antigravity-cli`) into the sealed
    /// store once: only when there has never been a record here. A record
    /// that is signed out (a refused refresh) stays signed out, and after a
    /// logout ([`Store::forget`]) nothing is imported again.
    pub fn import_from_cli_if_empty(&self, config_dir: &Path) -> Result<Option<Meta>> {
        if let Some(meta) = self.meta()? {
            return Ok(Some(meta));
        }
        if self.no_import_path().exists() {
            return Ok(None);
        }
        for dir in super::cli::candidate_dirs(config_dir) {
            if let Some(record) = read_antigravity_token(&dir) {
                self.save(&record)?;
                return Ok(Some(record.meta));
            }
        }
        Ok(None)
    }
}

fn read_antigravity_token(dir: &Path) -> Option<Record> {
    let now = crate::now();
    let agy_token = dir.join("antigravity-oauth-token");
    let bytes = std::fs::read(&agy_token).ok()?;
    let doc: Value = serde_json::from_slice(&bytes).ok()?;
    let tok = &doc["token"];
    let access = tok["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())?
        .to_string();
    let refresh = tok["refresh_token"]
        .as_str()
        .filter(|s| !s.is_empty())?
        .to_string();
    let id_token = tok["id_token"].as_str().unwrap_or_default().to_string();
    let email = std::fs::read(dir.join("settings.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|s| {
            s["user_email"]
                .as_str()
                .filter(|e| !e.is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            let dot = super::env::dot_gemini(dir);
            std::fs::read(dot.join("google_accounts.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|a| {
                    a["active"]
                        .as_str()
                        .filter(|e| !e.is_empty())
                        .map(str::to_string)
                })
        })
        .or_else(|| crate::jwt::claims(&id_token).email);
    Some(Record {
        meta: Meta {
            email,
            project_id: None,
            plan: Some("antigravity".into()),
            // Force a fresh token + project lookup on first request.
            expires_at: Some(0),
            signed_in_at: now,
            last_refresh: 0,
            signed_out: false,
        },
        secrets: Secrets {
            access_token: access,
            refresh_token: refresh,
            id_token,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli_token(dir: &Path, refresh: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let doc = serde_json::json!({
            "token": { "access_token": "ya29.cli", "refresh_token": refresh }
        });
        std::fs::write(
            dir.join("antigravity-oauth-token"),
            serde_json::to_vec(&doc).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn the_cli_token_is_imported_once_and_a_logout_sticks() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("private"));
        let cli = tmp.path().join("cli");
        cli_token(&cli, "1//cli-refresh");

        // First run: nothing here yet, the CLI's sign-in comes over.
        assert!(store.import_from_cli_if_empty(&cli).unwrap().is_some());
        assert_eq!(
            store.load().unwrap().unwrap().secrets.refresh_token,
            "1//cli-refresh"
        );

        // Logout: gone, and the CLI's token isn't pulled back in.
        assert!(store.forget().unwrap());
        assert!(store.import_from_cli_if_empty(&cli).unwrap().is_none());
        assert!(store.meta().unwrap().is_none());

        // An explicit sign-in lifts the opt-out for good.
        let mut rec = read_antigravity_token(&cli).unwrap();
        rec.secrets.refresh_token = "1//native".into();
        store.save(&rec).unwrap();
        assert!(!store.no_import_path().exists());
    }

    #[test]
    fn a_refused_refresh_stays_signed_out() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("private"));
        let cli = tmp.path().join("cli");
        cli_token(&cli, "1//dead");
        store.import_from_cli_if_empty(&cli).unwrap();
        let mut rec = store.load().unwrap().unwrap();
        rec.meta.signed_out = true;
        store.save(&rec).unwrap();

        // The same dead CLI token isn't imported over the signed-out record.
        let meta = store.import_from_cli_if_empty(&cli).unwrap().unwrap();
        assert!(meta.signed_out);
    }
}
