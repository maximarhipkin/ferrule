//! A connection's credential, asked for by the MCP transport on every
//! request: read from the store, refreshed a minute before it expires (or
//! on a 401) under the store's lock, and never handed anywhere else.
//!
//! When the grant is gone, the connection is marked `needs_reconnect`, its
//! tools are suspended (see `Connections::servers`), and `broken` is called
//! once per break so the owner gets one notice.

use crate::oauth::{self, TokenError};
use crate::store::{now, Record, Secret, State, Store};
use async_trait::async_trait;
use ferrule_mcp::CredentialSource;
use std::sync::Arc;

/// Refresh this long before the token says it expires.
const EARLY: u64 = 60;

pub type Broken = Arc<dyn Fn(&str) + Send + Sync>;

pub struct StoreCredential {
    pub(crate) store: Arc<Store>,
    pub(crate) name: String,
    /// Changes when the owner reconnects, so a reconnected server restarts.
    pub(crate) connected_at: u64,
    pub(crate) write: bool,
    pub(crate) http: reqwest::Client,
    pub(crate) broken: Broken,
}

/// The header for `record`'s credential.
pub(crate) fn header_for(record: &Record, secret: &Secret) -> Option<(String, String)> {
    if let Some(key) = &secret.api_key {
        let name = record
            .service
            .header
            .clone()
            .unwrap_or_else(|| "Authorization".into());
        let value = record
            .service
            .header_value
            .as_deref()
            .unwrap_or("Bearer {key}")
            .replace("{key}", key);
        return Some((name, value));
    }
    secret
        .access_token
        .as_ref()
        .map(|t| ("Authorization".to_string(), format!("Bearer {t}")))
}

impl StoreCredential {
    fn gone(&self) -> String {
        format!(
            "the connection `{0}` needs reconnecting (the owner can send /connect {0})",
            self.name
        )
    }

    fn find(&self) -> Result<(Record, Secret), String> {
        let records = self
            .store
            .load()
            .map_err(|_| "the connections store can't be read".to_string())?;
        let record = records
            .into_iter()
            .find(|r| r.name == self.name)
            .ok_or_else(|| format!("the connection `{}` was removed", self.name))?;
        if record.state != State::Connected {
            return Err(self.gone());
        }
        let secret = self
            .store
            .open(&record)
            .map_err(|_| format!("the connection `{}`'s secret can't be opened", self.name))?;
        Ok((record, secret))
    }

    fn due(record: &Record) -> bool {
        record.expires_at.is_some_and(|at| at <= now() + EARLY)
    }

    /// The header now; refreshed first if it's due, or if it's `rejected`.
    async fn current(&self, rejected: Option<&str>) -> Result<(String, String), String> {
        let (record, secret) = self.find()?;
        let header = header_for(&record, &secret).ok_or_else(|| self.gone())?;
        let stale = rejected == Some(header.1.as_str());
        if secret.api_key.is_some() {
            // A key has nothing to refresh: a rejected one is a dead one.
            return if stale {
                self.mark_broken("the service refused the key").await;
                Err(self.gone())
            } else {
                Ok(header)
            };
        }
        if !stale && !Self::due(&record) {
            return Ok(header);
        }
        self.refresh(rejected).await
    }

    async fn refresh(&self, rejected: Option<&str>) -> Result<(String, String), String> {
        let guard = self
            .store
            .lock()
            .await
            .map_err(|_| "the connections store is busy; try again".to_string())?;
        // Another request or process may have refreshed while we waited.
        let (record, secret) = self.find()?;
        let header = header_for(&record, &secret).ok_or_else(|| self.gone())?;
        if rejected != Some(header.1.as_str()) && !Self::due(&record) {
            return Ok(header);
        }
        let (Some(meta), Some(refresh_token)) = (&record.oauth, &secret.refresh_token) else {
            drop(guard);
            self.mark_broken("the token expired and there's no refresh token")
                .await;
            return Err(self.gone());
        };
        let client = oauth::Client {
            id: meta.client_id.clone(),
            secret: secret.client_secret.clone(),
        };
        let got = oauth::refresh(
            &self.http,
            &meta.token_endpoint,
            &client,
            refresh_token,
            meta.resource.as_deref(),
        )
        .await;
        match got {
            Ok(tokens) => {
                let mut fresh = secret.clone();
                fresh.access_token = Some(tokens.access_token);
                if tokens.refresh_token.is_some() {
                    fresh.refresh_token = tokens.refresh_token;
                }
                let sealed = self
                    .store
                    .seal(&self.name, &fresh)
                    .map_err(|_| "the refreshed token can't be sealed".to_string())?;
                let mut all = self
                    .store
                    .load()
                    .map_err(|_| "the connections store can't be read".to_string())?;
                let mut header = None;
                if let Some(r) = all.iter_mut().find(|r| r.name == self.name) {
                    r.sealed = sealed;
                    r.expires_at = tokens.expires_in.map(|s| now() + s);
                    r.refreshed_at = Some(now());
                    r.last_error = None;
                    header = header_for(r, &fresh);
                }
                self.store
                    .save_locked(&guard, &all)
                    .map_err(|_| "the refreshed token can't be saved".to_string())?;
                tracing::info!(connection = %self.name, "token refreshed");
                header.ok_or_else(|| self.gone())
            }
            Err(TokenError::Refused(code)) => {
                drop(guard);
                self.mark_broken(&format!("the refresh was refused ({code})"))
                    .await;
                Err(self.gone())
            }
            Err(TokenError::Transient(_)) => Err(format!(
                "couldn't reach {} to refresh the token; try again shortly",
                record.service.title()
            )),
        }
    }

    /// `needs_reconnect`, and one notice per break.
    async fn mark_broken(&self, why: &str) {
        let why = why.to_string();
        let first = self
            .store
            .update(|all| {
                let Some(r) = all.iter_mut().find(|r| r.name == self.name) else {
                    return Ok(false);
                };
                let first = !r.notice_sent;
                r.state = State::NeedsReconnect;
                r.last_error = Some(why);
                r.notice_sent = true;
                Ok(first)
            })
            .await
            .unwrap_or(false);
        tracing::warn!(connection = %self.name, "connection needs reconnecting");
        if first {
            (self.broken)(&self.name);
        }
    }
}

#[async_trait]
impl CredentialSource for StoreCredential {
    fn id(&self) -> String {
        format!(
            "connection:{}:{}:{}",
            self.name,
            self.connected_at,
            if self.write { "write" } else { "read" }
        )
    }

    async fn header(&self) -> Result<(String, String), String> {
        self.current(None).await
    }

    async fn refreshed(&self, rejected: &str) -> Result<(String, String), String> {
        self.current(Some(rejected)).await
    }
}
