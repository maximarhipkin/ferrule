//! `[trust]` in ferrule.toml.

use serde::Deserialize;

/// The owner's caps and gates. Every cap is off at 0.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TrustConfig {
    pub max_tokens_per_run: u64,
    pub max_usd_per_run: f64,
    pub max_tokens_per_day: u64,
    pub max_usd_per_day: f64,
    /// One scheduled task's calendar day: all its runs and their sub-agents.
    pub max_tokens_per_task: u64,
    pub max_usd_per_task: f64,
    /// The share of a cap at which the owner is warned, once per window.
    pub warn_at: f64,
    /// What "a day" is: a calendar day in this IANA zone.
    pub timezone: String,
    /// The Telegram chat for approvals and warnings. Unset: the first
    /// private chat in `[gateway] telegram_allowed_chats`.
    pub owner_chat: Option<i64>,
    pub approval_timeout_secs: u64,
    pub plan_timeout_secs: u64,
    /// The approval gates on destructive shell commands.
    pub gates: bool,
}

impl Default for TrustConfig {
    fn default() -> Self {
        Self {
            max_tokens_per_run: 5_000_000,
            max_usd_per_run: 5.0,
            max_tokens_per_day: 50_000_000,
            max_usd_per_day: 20.0,
            max_tokens_per_task: 0,
            max_usd_per_task: 0.0,
            warn_at: 0.8,
            timezone: "UTC".into(),
            owner_chat: None,
            approval_timeout_secs: 600,
            plan_timeout_secs: 3600,
            gates: true,
        }
    }
}

impl TrustConfig {
    /// Everything off: no caps, no gates. What `ferrule eval` runs under
    /// unless a suite opts in, and a starting point for tests.
    pub fn off() -> Self {
        Self {
            max_tokens_per_run: 0,
            max_usd_per_run: 0.0,
            max_tokens_per_day: 0,
            max_usd_per_day: 0.0,
            gates: false,
            ..Self::default()
        }
    }

    pub fn tz(&self) -> Result<chrono_tz::Tz, String> {
        self.timezone
            .parse()
            .map_err(|_| format!("[trust] timezone `{}` isn't an IANA zone", self.timezone))
    }

    /// Caps that need the ledger on disk (the day and the task).
    pub fn needs_ledger(&self) -> bool {
        self.max_tokens_per_day > 0
            || self.max_usd_per_day > 0.0
            || self.max_tokens_per_task > 0
            || self.max_usd_per_task > 0.0
    }

    pub fn validate(&self) -> Result<(), String> {
        self.tz()?;
        if !(self.warn_at > 0.0 && self.warn_at <= 1.0) {
            return Err(format!(
                "[trust] warn_at = {} must be in (0, 1]",
                self.warn_at
            ));
        }
        let money = [
            ("max_usd_per_run", self.max_usd_per_run),
            ("max_usd_per_day", self.max_usd_per_day),
            ("max_usd_per_task", self.max_usd_per_task),
        ];
        for (key, v) in money {
            if !v.is_finite() || v < 0.0 {
                return Err(format!("[trust] {key} = {v} must be 0 (off) or more"));
            }
        }
        if self.approval_timeout_secs == 0 || self.plan_timeout_secs == 0 {
            return Err(
                "[trust] approval_timeout_secs and plan_timeout_secs must be more than 0".into(),
            );
        }
        Ok(())
    }
}
