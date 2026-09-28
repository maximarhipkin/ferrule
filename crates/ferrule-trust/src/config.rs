//! `[trust]` in ferrule.toml.

use serde::Deserialize;

/// The channels an owner chat can be on, in the default primary order.
pub const OWNER_CHANNELS: &[&str] = &[
    "telegram",
    "discord",
    "slack",
    "whatsapp",
    "matrix",
    "mattermost",
    "signal",
    "email",
    "http",
];

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
    /// M31: the owner's Discord user id (their DMs with the bot). Unset:
    /// the first of `[gateway] discord_allowed_users`.
    pub discord_owner: Option<String>,
    /// M31: the owner's Slack member id (`U…`). Unset: the first of
    /// `[gateway] slack_allowed_users`.
    pub slack_owner: Option<String>,
    /// M39: the owner's WhatsApp number, `wa_id` digits. Unset: the first
    /// of `[gateway.whatsapp] allowed_users`.
    pub whatsapp_owner: Option<String>,
    /// M39: the owner's Matrix user id (`@max:example.org`).
    pub matrix_owner: Option<String>,
    /// M39: the owner's Mattermost user id.
    pub mattermost_owner: Option<String>,
    /// M39: the owner's Signal number (`+972…`) or ACI uuid.
    pub signal_owner: Option<String>,
    /// M39: the owner's email address.
    pub email_owner: Option<String>,
    /// M39: the HTTP API client (by name) that is the owner.
    pub http_owner: Option<String>,
    /// M31: which owner chat gets approvals and warnings, one of
    /// [`OWNER_CHANNELS`]. Unset: the first in that order among the
    /// channels that run and have an owner.
    pub owner_channel: Option<String>,
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
            discord_owner: None,
            slack_owner: None,
            whatsapp_owner: None,
            matrix_owner: None,
            mattermost_owner: None,
            signal_owner: None,
            email_owner: None,
            http_owner: None,
            owner_channel: None,
            approval_timeout_secs: 600,
            plan_timeout_secs: 3600,
            gates: true,
        }
    }
}

impl TrustConfig {
    /// The owner setting for a channel other than Telegram (whose owner is
    /// a number, `owner_chat`): `discord_owner`, `whatsapp_owner`, ….
    pub fn owner_on(&self, channel: &str) -> Option<&String> {
        match channel {
            "discord" => self.discord_owner.as_ref(),
            "slack" => self.slack_owner.as_ref(),
            "whatsapp" => self.whatsapp_owner.as_ref(),
            "matrix" => self.matrix_owner.as_ref(),
            "mattermost" => self.mattermost_owner.as_ref(),
            "signal" => self.signal_owner.as_ref(),
            "email" => self.email_owner.as_ref(),
            "http" => self.http_owner.as_ref(),
            _ => None,
        }
    }

    /// Every owner set in `[trust]`, Telegram's first.
    pub fn named_owners(&self) -> Vec<crate::chat::ChatRef> {
        use crate::chat::ChatRef;
        self.owner_chat
            .map(ChatRef::from)
            .into_iter()
            .chain(
                OWNER_CHANNELS
                    .iter()
                    .filter_map(|c| self.owner_on(c).map(|u| ChatRef::new(*c, u.clone()))),
            )
            .collect()
    }

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
        if let Some(c) = &self.owner_channel {
            if !OWNER_CHANNELS.contains(&c.as_str()) {
                return Err(format!(
                    "[trust] owner_channel = \"{c}\" must be one of {}",
                    OWNER_CHANNELS.join(", ")
                ));
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

/// The owner chats, primary first: `owner_channel`'s, then the rest in
/// [`OWNER_CHANNELS`] order (M31).
pub fn order_owners(
    chats: Vec<crate::chat::ChatRef>,
    primary: Option<&str>,
) -> Vec<crate::chat::ChatRef> {
    let rank = |c: &crate::chat::ChatRef| {
        let at = OWNER_CHANNELS
            .iter()
            .position(|n| *n == c.channel)
            .unwrap_or(OWNER_CHANNELS.len());
        (Some(c.channel.as_str()) != primary, at)
    };
    let mut chats = chats;
    chats.sort_by_key(|c| rank(c));
    chats
}
