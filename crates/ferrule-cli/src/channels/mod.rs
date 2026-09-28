//! M39: every chat channel in one table. Each place that used to list
//! Telegram, Discord and Slack by hand (the owners, the redactor, the
//! sandbox's secret names, streaming, setup's forgetting, the doctor and
//! the dashboard) asks here, so a new channel can't be half wired.

pub mod card;
pub mod email;
pub mod matrix;
pub mod settings;
pub mod whatsapp;

use crate::config::Config;

/// One chat channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Info {
    /// The adapter's name, and the session id's prefix.
    pub name: &'static str,
    pub title: &'static str,
    /// The config key holding who may DM it, as said in messages.
    pub users_key: &'static str,
    /// Its owner setting in `[trust]` (`owner_chat` for Telegram).
    pub owner_key: &'static str,
}

/// Every chat channel, in the owner's default primary order
/// ([`ferrule_trust::OWNER_CHANNELS`]).
pub const CHANNELS: &[Info] = &[
    Info {
        name: "telegram",
        title: "Telegram",
        users_key: "[gateway] telegram_allowed_chats",
        owner_key: "owner_chat",
    },
    Info {
        name: "discord",
        title: "Discord",
        users_key: "[gateway] discord_allowed_users",
        owner_key: "discord_owner",
    },
    Info {
        name: "slack",
        title: "Slack",
        users_key: "[gateway] slack_allowed_users",
        owner_key: "slack_owner",
    },
    Info {
        name: "whatsapp",
        title: "WhatsApp",
        users_key: "[gateway.whatsapp] allowed_users",
        owner_key: "whatsapp_owner",
    },
    Info {
        name: "matrix",
        title: "Matrix",
        users_key: "[gateway.matrix] allowed_users",
        owner_key: "matrix_owner",
    },
    Info {
        name: "mattermost",
        title: "Mattermost",
        users_key: "[gateway.mattermost] allowed_users",
        owner_key: "mattermost_owner",
    },
    Info {
        name: "signal",
        title: "Signal",
        users_key: "[gateway.signal] allowed_users",
        owner_key: "signal_owner",
    },
    Info {
        name: "email",
        title: "Email",
        users_key: "[gateway.email] allowed_senders",
        owner_key: "email_owner",
    },
    Info {
        name: "http",
        title: "HTTP API",
        users_key: "the HTTP API's client keys",
        owner_key: "http_owner",
    },
];

/// Whether the config switches `name` on (its token may still be missing:
/// building it says so).
pub fn configured(cfg: &Config, name: &str) -> bool {
    let g = &cfg.gateway;
    match name {
        "telegram" => g.telegram_token_env.is_some(),
        "discord" => g.discord_token_env.is_some(),
        "slack" => g.slack_bot_token_env.is_some() && g.slack_app_token_env.is_some(),
        "whatsapp" => g.whatsapp.is_some(),
        "matrix" => g.matrix.is_some(),
        "mattermost" => g.mattermost.is_some(),
        "signal" => g.signal.is_some(),
        "email" => g.email.is_some(),
        "http" => g.http.is_some(),
        _ => false,
    }
}

/// The names of every configured chat channel.
pub fn configured_names(cfg: &Config) -> Vec<&'static str> {
    CHANNELS
        .iter()
        .filter(|c| configured(cfg, c.name))
        .map(|c| c.name)
        .collect()
}

/// The env vars `name`'s config says hold its secrets.
pub fn secret_envs_of(cfg: &Config, name: &str) -> Vec<String> {
    let g = &cfg.gateway;
    let v: Vec<Option<&String>> = match name {
        "telegram" => vec![g.telegram_token_env.as_ref()],
        "discord" => vec![g.discord_token_env.as_ref()],
        "slack" => vec![
            g.slack_bot_token_env.as_ref(),
            g.slack_app_token_env.as_ref(),
        ],
        "whatsapp" => g.whatsapp.as_ref().map_or_else(Vec::new, |w| {
            vec![
                Some(&w.token_env),
                Some(&w.app_secret_env),
                Some(&w.verify_token_env),
            ]
        }),
        "matrix" => g.matrix.as_ref().map_or_else(Vec::new, |m| {
            vec![m.access_token_env.as_ref(), m.password_env.as_ref()]
        }),
        "mattermost" => g
            .mattermost
            .as_ref()
            .map_or_else(Vec::new, |m| vec![Some(&m.token_env)]),
        "email" => g
            .email
            .as_ref()
            .map_or_else(Vec::new, |e| vec![e.password_env.as_ref()]),
        _ => vec![],
    };
    v.into_iter().flatten().cloned().collect()
}

/// Every env var a configured channel holds a secret in: the sandbox
/// scrubs them from commands and the redactor hides their values.
pub fn secret_envs(cfg: &Config) -> Vec<String> {
    CHANNELS
        .iter()
        .flat_map(|c| secret_envs_of(cfg, c.name))
        .collect()
}

/// Whether some configured channel reads the env var `var`.
pub fn reads_env(cfg: &Config, var: &str) -> bool {
    secret_envs(cfg).iter().any(|v| v == var)
}

/// Who may DM `name`, as the config lists them (Telegram's chat ids as
/// text).
pub fn allowed_users(cfg: &Config, name: &str) -> Vec<String> {
    let g = &cfg.gateway;
    match name {
        "telegram" => g
            .telegram_allowed_chats
            .iter()
            .map(|c| c.to_string())
            .collect(),
        "discord" => g.discord_allowed_users.clone(),
        "slack" => g.slack_allowed_users.clone(),
        "whatsapp" => g
            .whatsapp
            .as_ref()
            .map(|w| w.allowed_users.clone())
            .unwrap_or_default(),
        "matrix" => g
            .matrix
            .as_ref()
            .map(|m| m.allowed_users.clone())
            .unwrap_or_default(),
        "mattermost" => g
            .mattermost
            .as_ref()
            .map(|m| m.allowed_users.clone())
            .unwrap_or_default(),
        "signal" => g
            .signal
            .as_ref()
            .map(|s| s.allowed_users.clone())
            .unwrap_or_default(),
        // A `@domain` entry is nobody in particular.
        "email" => g
            .email
            .as_ref()
            .map(|e| {
                e.allowed_senders
                    .iter()
                    .filter(|s| !s.starts_with('@'))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default(),
        _ => vec![],
    }
}

/// Whether `name`'s allowlist is empty (for email, `@domain` entries
/// count: they let people in).
pub fn lists_nobody(cfg: &Config, name: &str) -> bool {
    match (name, &cfg.gateway.email) {
        ("email", Some(e)) => e.allowed_senders.is_empty(),
        _ => allowed_users(cfg, name).is_empty(),
    }
}

/// Whether `name`'s replies stream (M27): its own `stream` setting, else
/// `[agent] stream`.
pub fn streams(cfg: &Config, name: &str) -> bool {
    let g = &cfg.gateway;
    let own = match name {
        "telegram" => g.telegram_stream,
        "discord" => g.discord_stream,
        "slack" => g.slack_stream,
        "matrix" => g.matrix.as_ref().and_then(|m| m.stream),
        "mattermost" => g.mattermost.as_ref().and_then(|m| m.stream),
        "http" => g.http.as_ref().and_then(|h| h.stream),
        // No edits: WhatsApp, Signal, email.
        _ => return false,
    };
    own.unwrap_or(cfg.agent.stream)
}

/// M39 (M38's collision check): the account `name` is on, when two
/// instances can't share it. Never a secret. `secret`: the instance's
/// secrets, by env name.
pub fn account(
    cfg: &Config,
    name: &str,
    secret: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let g = &cfg.gateway;
    match name {
        "whatsapp" => g.whatsapp.as_ref().map(|w| w.phone_number_id.clone()),
        // The bot's user id (setup writes it beside a token too), else a
        // fingerprint of the token: one token in two instances.
        "matrix" => g.matrix.as_ref().and_then(|m| {
            m.user.clone().or_else(|| {
                let token = secret(m.access_token_env.as_deref()?)?;
                let hash = ferrule_gateway::channels::hmac::sha256(token.trim().as_bytes());
                let short = ferrule_gateway::channels::hmac::hex(&hash[..4]);
                Some(format!(
                    "{}, token {short}…",
                    m.homeserver.trim_end_matches('/')
                ))
            })
        }),
        // The login and the IMAP server. A connection's address is sealed
        // in each instance's own store: a mailbox named only that way isn't
        // compared.
        "email" => g.email.as_ref().and_then(|e| {
            let user = e.username.as_ref().or(e.address.as_ref())?;
            let host = e
                .imap_host
                .clone()
                .or_else(|| email::provider(e.address.as_deref()?).map(|p| p.0 .0.to_string()))?;
            Some(format!(
                "{} on {}",
                user.to_ascii_lowercase(),
                host.to_ascii_lowercase()
            ))
        }),
        _ => None,
    }
}

/// Why two instances on one account is wrong, in words.
pub fn account_clash(channel: &str, id: &str, other: &str) -> String {
    match channel {
        "whatsapp" => format!("the same WhatsApp number (phone number id {id}) as the instance `{other}`: Meta sends its webhooks to one callback URL, so one of them hears nothing (or both take turns). Give one of them a number of its own"),
        "matrix" => format!("the same Matrix bot account ({id}) as the instance `{other}`: both would answer every message, and each moves the other's read position. Give each instance its own bot account"),
        "email" => format!("the same mailbox ({id}) as the instance `{other}`: whichever looks first takes a mail and marks it read, so each answers about half. Give each instance its own mailbox"),
        _ => format!("the same {channel} account ({id}) as the instance `{other}`: give one of them its own"),
    }
}

/// The session id prefix and title of a chat session (`whatsapp__…`).
pub fn of_session(session_id: &str) -> Option<(&'static Info, &str)> {
    CHANNELS.iter().find_map(|c| {
        session_id
            .strip_prefix(c.name)
            .and_then(|r| r.strip_prefix("__"))
            .map(|chat| (c, chat))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_the_trust_hubs_order_and_titles_agree() {
        let names: Vec<&str> = CHANNELS.iter().map(|c| c.name).collect();
        assert_eq!(names, ferrule_trust::OWNER_CHANNELS);
        for c in CHANNELS {
            assert_eq!(
                ferrule_trust::ChatRef::new(c.name, "1").channel_title(),
                c.title,
                "{}",
                c.name
            );
        }
    }

    #[test]
    fn a_sub_table_switches_its_channel_on_and_names_its_secrets() {
        let cfg: Config = toml::from_str(
            r#"
[gateway]
discord_token_env = "DISCORD_TOKEN"
[gateway.whatsapp]
phone_number_id = "1055"
allowed_users = ["972501234567"]
[gateway.matrix]
homeserver = "https://m.example.org"
user = "@bot:example.org"
password_env = "MATRIX_PASSWORD"
stream = false
"#,
        )
        .unwrap();
        assert_eq!(configured_names(&cfg), ["discord", "whatsapp", "matrix"]);
        assert_eq!(
            secret_envs(&cfg),
            [
                "DISCORD_TOKEN",
                "WHATSAPP_TOKEN",
                "WHATSAPP_APP_SECRET",
                "WHATSAPP_VERIFY_TOKEN",
                "MATRIX_PASSWORD"
            ]
        );
        assert!(reads_env(&cfg, "WHATSAPP_APP_SECRET"));
        assert_eq!(allowed_users(&cfg, "whatsapp"), ["972501234567"]);
        assert!(!streams(&cfg, "matrix"));
        assert!(!streams(&cfg, "whatsapp"), "no edits, no streaming");
        assert!(streams(&cfg, "discord"));
        let (c, chat) = of_session("whatsapp__972501234567").unwrap();
        assert_eq!((c.title, chat), ("WhatsApp", "972501234567"));
        assert!(of_session("dashboard__owner").is_none());
    }

    #[test]
    fn a_typo_in_a_sub_table_is_refused() {
        let e = toml::from_str::<Config>(
            "[gateway.whatsapp]\nphone_number_id = \"1\"\nallowed_user = [\"1\"]\n",
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("allowed_user"), "{e}");
    }
}
