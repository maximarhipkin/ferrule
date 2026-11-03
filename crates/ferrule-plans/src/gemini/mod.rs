//! The Gemini / Antigravity plan:
//! - Native OAuth 2.0 PKCE sign-in (`auth`), sealed token store
//!   (`private/plans/gemini.json` in `store`), refresh under a cross-process
//!   lock, and the `PlanAuth` the native `CloudCodeProvider` asks before
//!   every request. The lock is held for the token refresh alone (one
//!   HTTP call, 20 s at most, under the lock's 30 s stale age); the
//!   project lookup and the quota read run outside it, and a lookup that
//!   finds no project waits out a backoff before the next.
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
use ferrule_core::failure::{Failure, Kind};
use ferrule_providers::codex::{PlanAuth, PlanCredentials, RateLimits};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use store::{Lookup, Meta, Record, Secrets, Store};

pub const NOT_SIGNED_IN: &str = "not signed in to the Gemini plan: run `ferrule login gemini` \
     or sign in on the dashboard's Models page";
pub const EXPIRED: &str = "your Google sign-in for the Gemini plan expired or was revoked: \
     run `ferrule login gemini` or sign in again on the dashboard's Models page";

/// A sign-in to do again, as the failure taxonomy names it.
fn sign_in_error(words: &str) -> CoreError {
    Failure::new(Kind::GeminiSignin, words).into()
}

/// The plan's name in the usage file and the ledger.
pub const PLAN: &str = "gemini-cli";

pub struct GeminiPlan {
    store: Store,
    issuer: Issuer,
    http: reqwest::Client,
    usage: Option<UsageFile>,
    /// A refresh was refused in this process (the owner is told once).
    refused: AtomicBool,
    /// A project lookup is running in this process: others don't start one.
    looking_up: AtomicBool,
}

/// Clears its flag when dropped.
struct Busy<'a>(&'a AtomicBool);

impl Busy<'_> {
    fn take(flag: &AtomicBool) -> Option<Busy<'_>> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Busy(flag))
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
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
            looking_up: AtomicBool::new(false),
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
    /// The sign-in is kept even when Google gave the account no project
    /// (onboarding may finish later); [`Meta::project_note`] then says so,
    /// and the next lookup waits out a backoff.
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
        if let Some(problem) = &info.problem {
            tracing::warn!("the Gemini sign-in found no Cloud Code project: {problem}");
        }
        let project_lookup = info
            .project_id
            .is_none()
            .then(|| Lookup::missed(now, None, info.problem.clone()));
        let record = Record {
            meta: Meta {
                email: claims.email,
                project_id: info.project_id,
                plan: info
                    .tier
                    .or(claims.plan)
                    .or_else(|| Some("antigravity".into())),
                expires_at,
                signed_in_at: now,
                last_refresh: now,
                signed_out: false,
                project_lookup,
            },
            secrets: Secrets {
                access_token: access,
                refresh_token: refresh,
                id_token: id,
            },
        };
        {
            let _lock = self.store.lock().await?;
            self.store.save(&record)?;
        }
        self.refused.store(false, Ordering::Relaxed);
        self.read_quota(&record).await;
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
        let was_signed_in = self.store.forget()?;
        Ok(LoggedOut {
            was_signed_in,
            revoked,
        })
    }

    /// Refresh now, whatever the expiry says (doctor's "refresh works"),
    /// and look the project and tier up again.
    pub async fn refresh_now(&self) -> Result<Meta, CoreError> {
        let record = self.refresh(Due::Always).await?;
        Ok(self.resolve_project(record).await.meta)
    }

    /// Under the lock: re-read (another process may have refreshed), and
    /// refresh only if it's still `due`. The lock covers the token call
    /// alone; the quota is read after it is let go.
    async fn refresh(&self, due: Due<'_>) -> Result<Record, CoreError> {
        let record = {
            let _lock = self.store.lock().await.map_err(provider)?;
            let mut record = self.load()?;
            let now = crate::now();
            let needed = match due {
                Due::Always => true,
                Due::Expiring => record.needs_refresh(now),
                Due::Refused(used) => record.secrets.access_token == used,
            };
            if !needed {
                return Ok(record);
            }
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
                    return Err(sign_in_error(EXPIRED));
                }
                Err(TokenError::Transient(why)) => {
                    return Err(CoreError::Transient {
                        message: format!("refreshing the Gemini sign-in: {why}"),
                        retry_after: None,
                    });
                }
            }
            self.store.save(&record).map_err(provider)?;
            record
        };
        self.read_quota(&record).await;
        Ok(record)
    }

    /// Looks the account's Cloud Code project and tier up (onboarding it
    /// when Google says to), outside the store lock, then merges what it
    /// found under the lock: another process may have written meanwhile.
    /// Finding none is remembered with a growing wait, so requests don't
    /// repeat the lookup. One lookup at a time in this process; a request
    /// that finds one running goes on with what the record has.
    async fn resolve_project(&self, record: Record) -> Record {
        let Some(_busy) = Busy::take(&self.looking_up) else {
            return record;
        };
        let info =
            auth::load_code_assist(&self.http, &self.issuer, &record.secrets.access_token).await;
        let merged = async {
            let _lock = self.store.lock().await.ok()?;
            let mut fresh = self.load().ok()?;
            if let Some(tier) = info.tier.clone() {
                fresh.meta.plan = Some(tier);
            }
            match info.project_id.clone() {
                Some(project) => {
                    fresh.meta.project_id = Some(project);
                    fresh.meta.project_lookup = None;
                }
                None if fresh.meta.project_id.is_none() => {
                    let problem = info.problem.clone();
                    tracing::warn!(
                        "no Cloud Code project for the Gemini plan yet: {}",
                        problem.as_deref().unwrap_or("none given")
                    );
                    fresh.meta.project_lookup = Some(Lookup::missed(
                        crate::now(),
                        fresh.meta.project_lookup.as_ref(),
                        problem,
                    ));
                }
                // Kept: the one already found (by this lookup's earlier
                // round, or by another process meanwhile).
                None => {}
            }
            self.store.save(&fresh).ok()?;
            Some(fresh)
        }
        .await;
        merged.unwrap_or(record)
    }

    /// The 5-hour and weekly windows, read after a sign-in or a refresh.
    async fn read_quota(&self, record: &Record) {
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
    }

    fn load(&self) -> Result<Record, CoreError> {
        let record = self
            .store
            .load()
            .map_err(provider)?
            .ok_or_else(|| sign_in_error(NOT_SIGNED_IN))?;
        if record.meta.signed_out {
            return Err(sign_in_error(EXPIRED));
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
        let mut record = self.load()?;
        let now = crate::now();
        if record.needs_refresh(now) {
            match self.refresh(Due::Expiring).await {
                Ok(fresh) => record = fresh,
                Err(CoreError::Transient { message, .. }) if record.usable(now) => {
                    tracing::warn!("{message}; using the current token until it expires");
                }
                Err(e) => return Err(e),
            }
        }
        if record.project_due(now) {
            record = self.resolve_project(record).await;
        }
        Ok(credentials(&record))
    }

    async fn refused(&self, used: &str) -> Result<(), CoreError> {
        self.refresh(Due::Refused(used)).await.map(|_| ())
    }

    fn observe(&self, limits: &RateLimits) {
        if let Some(usage) = &self.usage {
            let mut reading = Reading::from_codex(limits, crate::now());
            if reading.windows.is_empty() {
                // A usage-limit stop alone: the windows stay what the last
                // quota summary read.
                if let Some(last) = usage.get(PLAN) {
                    reading.windows = last.windows;
                }
            }
            usage.record(PLAN, reading);
        }
    }
}
