//! The Gemini / Antigravity plan:
//! - Native OAuth 2.0 PKCE sign-in (`auth`), sealed token store
//!   (`private/plans/gemini.json` in `store`), refresh under a cross-process
//!   lock, and the `PlanAuth` the native `CloudCodeProvider` asks before
//!   every request.
//! - Optional fallback subprocess engine (`cli`, `env`, `stream`, `engine`)
//!   when an explicit CLI binary is configured.

pub mod auth;
pub mod cli;
pub mod engine;
pub mod env;
pub mod store;
pub mod stream;

pub use engine::{EngineConfig, GeminiCli};

use crate::jwt;
use crate::usage::{Reading, UsageFile};
use anyhow::{Context, Result};
use auth::{Issuer, Tokens};
use ferrule_connections::oauth::TokenError;
use ferrule_core::error::CoreError;
use ferrule_providers::codex::{PlanAuth, PlanCredentials, RateLimits};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use store::{Meta, Record, Secrets, Store};

pub const NOT_SIGNED_IN: &str = "not signed in to the Gemini plan: run `ferrule login gemini`";
pub const EXPIRED: &str = "the Gemini sign-in expired or was revoked: run `ferrule login gemini`";

/// The plan's name in the usage file and the ledger.
pub const PLAN: &str = "gemini-cli";

pub struct GeminiPlan {
    store: Store,
    issuer: Issuer,
    http: reqwest::Client,
    usage: Option<UsageFile>,
    /// A refresh was refused in this process (the owner is told once).
    refused: AtomicBool,
}

/// What `ferrule logout gemini` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoggedOut {
    pub was_signed_in: bool,
    pub revoked: bool,
}

impl GeminiPlan {
    /// `private` is ferrule's `private/` directory; `data`, where the usage
    /// file goes (`None`: don't record usage).
    pub fn new(private: &Path, data: Option<&Path>, issuer: Issuer) -> Self {
        Self {
            store: Store::new(private),
            issuer,
            http: ferrule_connections::oauth::http_client(),
            usage: data.map(UsageFile::new),
            refused: AtomicBool::new(false),
        }
    }

    pub fn issuer(&self) -> &Issuer {
        &self.issuer
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn refresh_refused(&self) -> bool {
        self.refused.load(Ordering::Relaxed)
    }

    /// Keep what a sign-in returned and resolve the companion project + tier.
    pub async fn sign_in(&self, tokens: Tokens) -> Result<Meta> {
        let access = tokens
            .access_token
            .context("the sign-in had no access token")?;
        let refresh = tokens
            .refresh_token
            .context("the sign-in had no refresh token")?;
        let id = tokens.id_token.unwrap_or_default();
        let claims = jwt::merged(&id, &access);
        let now = crate::now();
        let expires_at = tokens
            .expires_in
            .map(|secs| now.saturating_add(secs))
            .or(claims.exp)
            .or(Some(now + 3600));
        let info = auth::load_code_assist(&self.http, &self.issuer, &access).await;
        let record = Record {
            meta: Meta {
                email: claims.email,
                project_id: info.as_ref().and_then(|i| i.project_id.clone()),
                plan: info
                    .and_then(|i| i.tier)
                    .or(claims.plan)
                    .or_else(|| Some("antigravity".into())),
                expires_at,
                signed_in_at: now,
                last_refresh: now,
                signed_out: false,
            },
            secrets: Secrets {
                access_token: access,
                refresh_token: refresh,
                id_token: id,
            },
        };
        let _lock = self.store.lock().await?;
        self.store.save(&record)?;
        self.refused.store(false, Ordering::Relaxed);
        if let Some(limits) = auth::fetch_quota(
            &self.http,
            &self.issuer,
            &record.secrets.access_token,
            record.meta.project_id.as_deref(),
        )
        .await
        {
            self.observe(&limits);
        }
        Ok(record.meta)
    }

    /// Revoke (best effort) and delete the sealed sign-in.
    pub async fn log_out(&self) -> Result<LoggedOut> {
        let _lock = self.store.lock().await?;
        let record = self.store.load().ok().flatten();
        let revoked = match &record {
            Some(r) => auth::revoke(&self.http, &self.issuer, &r.secrets.refresh_token).await,
            None => false,
        };
        let was_signed_in = self.store.delete()?;
        Ok(LoggedOut {
            was_signed_in,
            revoked,
        })
    }

    /// Refresh now, whatever the expiry says (doctor's "refresh works").
    pub async fn refresh_now(&self) -> Result<Meta, CoreError> {
        self.refresh(Due::Always).await.map(|r| r.meta)
    }

    /// Under the lock: re-read (another process may have refreshed), and
    /// refresh only if it's still `due`.
    async fn refresh(&self, due: Due<'_>) -> Result<Record, CoreError> {
        let _lock = self.store.lock().await.map_err(provider)?;
        let mut record = self.load()?;
        let now = crate::now();
        let needed = match due {
            Due::Always => true,
            Due::Expiring => record.needs_refresh(now),
            Due::Refused(used) => record.secrets.access_token == used,
        };
        if !needed && record.meta.project_id.is_some() {
            return Ok(record);
        }
        if needed {
            match auth::refresh(&self.http, &self.issuer, &record.secrets.refresh_token).await {
                Ok(tokens) => {
                    if let Some(access) = tokens.access_token {
                        record.secrets.access_token = access;
                    }
                    if let Some(id) = tokens.id_token {
                        record.secrets.id_token = id;
                    }
                    if let Some(refresh) = tokens.refresh_token {
                        record.secrets.refresh_token = refresh;
                    }
                    let claims =
                        jwt::merged(&record.secrets.id_token, &record.secrets.access_token);
                    record.meta.expires_at = tokens
                        .expires_in
                        .map(|secs| now.saturating_add(secs))
                        .or(claims.exp)
                        .or(Some(now + 3600));
                    record.meta.email = claims.email.or(record.meta.email);
                    record.meta.last_refresh = now;
                }
                Err(TokenError::Refused(code)) => {
                    tracing::warn!("the Gemini refresh was refused ({code})");
                    record.meta.signed_out = true;
                    self.store.save(&record).map_err(provider)?;
                    self.refused.store(true, Ordering::Relaxed);
                    return Err(CoreError::Provider(EXPIRED.into()));
                }
                Err(TokenError::Transient(why)) => {
                    return Err(CoreError::Transient {
                        message: format!("refreshing the Gemini sign-in: {why}"),
                        retry_after: None,
                    });
                }
            }
        }
        if record.meta.project_id.is_none() || matches!(due, Due::Always) {
            if let Some(info) =
                auth::load_code_assist(&self.http, &self.issuer, &record.secrets.access_token).await
            {
                record.meta.project_id = info.project_id.or(record.meta.project_id);
                record.meta.plan = info.tier.or(record.meta.plan);
            }
        }
        self.store.save(&record).map_err(provider)?;
        if let Some(limits) = auth::fetch_quota(
            &self.http,
            &self.issuer,
            &record.secrets.access_token,
            record.meta.project_id.as_deref(),
        )
        .await
        {
            self.observe(&limits);
        }
        Ok(record)
    }

    fn load(&self) -> Result<Record, CoreError> {
        let record = self
            .store
            .load()
            .map_err(provider)?
            .ok_or_else(|| CoreError::Provider(NOT_SIGNED_IN.into()))?;
        if record.meta.signed_out {
            return Err(CoreError::Provider(EXPIRED.into()));
        }
        Ok(record)
    }
}

enum Due<'a> {
    Always,
    Expiring,
    Refused(&'a str),
}

fn provider(e: anyhow::Error) -> CoreError {
    CoreError::Provider(format!("{e:#}"))
}

fn credentials(record: &Record) -> PlanCredentials {
    PlanCredentials {
        access_token: record.secrets.access_token.clone(),
        account_id: record.meta.project_id.clone(),
        fedramp: false,
    }
}

#[async_trait::async_trait]
impl PlanAuth for GeminiPlan {
    async fn credentials(&self) -> Result<PlanCredentials, CoreError> {
        let record = self.load()?;
        let now = crate::now();
        if !record.needs_refresh(now) && record.meta.project_id.is_some() {
            return Ok(credentials(&record));
        }
        match self.refresh(Due::Expiring).await {
            Ok(fresh) => Ok(credentials(&fresh)),
            Err(CoreError::Transient { message, .. }) if record.usable(now) => {
                tracing::warn!("{message}; using the current token until it expires");
                Ok(credentials(&record))
            }
            Err(e) => Err(e),
        }
    }

    async fn refused(&self, used: &str) -> Result<(), CoreError> {
        self.refresh(Due::Refused(used)).await.map(|_| ())
    }

    fn observe(&self, limits: &RateLimits) {
        if let Some(usage) = &self.usage {
            usage.record(PLAN, Reading::from_codex(limits, crate::now()));
        }
    }
}
