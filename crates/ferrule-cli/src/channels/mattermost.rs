//! M39 §7: Mattermost on the CLI side: the adapter's settings from
//! `[gateway.mattermost]` and the dashboard card.

use super::card::{Field, Kind, Settings, Spec, Step};
use super::settings::Mattermost;
use crate::config_follow::secret_value;
use anyhow::{anyhow, Result};
use ferrule_gateway::channels::files::Inbox;
use ferrule_gateway::channels::mattermost::{self as mm, MattermostConfig};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

/// The bot token, from the env var the config names.
pub fn token(m: &Mattermost, secret: impl Fn(&str) -> Option<String>) -> Result<String> {
    secret(&m.token_env).ok_or_else(|| {
        anyhow!(
            "`{}` isn't set (needed by [gateway.mattermost] token_env) — run `ferrule setup` → Mattermost, or the dashboard's Mattermost card",
            m.token_env
        )
    })
}

/// The adapter's settings. `workspace`: where files people send are saved;
/// `None` (`ferrule tasks run-now`): nothing is taken in, only sent.
pub fn config(m: &Mattermost, workspace: Option<&Path>) -> Result<MattermostConfig> {
    Ok(MattermostConfig {
        server_url: m.server_url.trim_end_matches('/').to_string(),
        token: token(m, secret_value)?,
        inbox: workspace.map(|ws| Inbox::new(ws, m.max_file_mb)),
    })
}

fn probe(s: Settings) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
    Box::pin(async move {
        let m: Mattermost = s.read()?;
        let token = token(&m, |env| s.secrets.get(env).cloned()).map_err(|e| e.to_string())?;
        let p = mm::probe(MattermostConfig {
            server_url: m.server_url.trim_end_matches('/').to_string(),
            token,
            inbox: None,
        })
        .await?;
        let mut said = p.summary();
        if m.allowed_users.is_empty() {
            said.push_str(" · no one is allowed yet: `ferrule setup` → Mattermost pairs you, or list your username");
        }
        Ok(said)
    })
}

fn check(t: &toml::Table) -> Result<(), String> {
    let m: Mattermost = toml::Value::Table(t.clone())
        .try_into()
        .map_err(|e: toml::de::Error| e.message().to_string())?;
    if !(m.server_url.starts_with("https://") || m.server_url.starts_with("http://")) {
        return Err(format!(
            "the server is a URL like https://chat.example.com, not `{}`",
            m.server_url
        ));
    }
    for u in &m.allowed_users {
        let name = u.strip_prefix('@').unwrap_or(u);
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err(format!("`{u}` isn't a user id or a username like @max"));
        }
    }
    for c in &m.allowed_channels {
        if !mm::is_id(c) {
            return Err(format!(
                "`{c}` isn't a channel id (26 letters and digits: the channel's menu → View Info)"
            ));
        }
    }
    Ok(())
}

pub const SPEC: Spec = Spec {
    name: "mattermost",
    fields: &[
        Field {
            key: "server_url",
            label: "Server URL",
            hint: "the address you open Mattermost at: https://chat.example.com",
            kind: Kind::Text,
            optional: false,
        },
        Field {
            key: "token_env",
            label: "Bot token",
            hint: "the bot account's access token (Integrations → Bot Accounts)",
            kind: Kind::Secret {
                env: "MATTERMOST_TOKEN",
            },
            optional: false,
        },
        Field {
            key: "allowed_users",
            label: "Allowed users",
            hint: "@max or a user id — whose DMs reach the agent",
            kind: Kind::List,
            optional: true,
        },
        Field {
            key: "allowed_channels",
            label: "Allowed channels",
            hint: "channel ids where a mention reaches it (the channel's menu → View Info)",
            kind: Kind::List,
            optional: true,
        },
    ],
    guide: &[
        Step {
            text: "As an admin: System Console → Integrations → Bot Accounts → Enable Bot Account Creation",
            url: Some("https://docs.mattermost.com/configure/integrations-configuration-settings.html#bot-accounts"),
        },
        Step {
            text: "Integrations → Bot Accounts → Add Bot Account, then Create New Token and copy it (it's shown once)",
            url: Some("https://developers.mattermost.com/integrate/reference/bot-accounts/"),
        },
        Step {
            text: "Add the bot to your team, and to each channel it should answer in (/invite @bot)",
            url: None,
        },
        Step {
            text: "Save and Test here, then send the bot a direct message",
            url: None,
        },
    ],
    probe,
    check,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn table(pairs: &[(&str, toml::Value)]) -> toml::Table {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn the_card_checks_the_url_and_ids() {
        let ok = table(&[("server_url", "https://chat.example.com".into())]);
        assert!(check(&ok).is_ok());
        let bare = table(&[("server_url", "chat.example.com".into())]);
        assert!(check(&bare).unwrap_err().contains("a URL"));
        let mut who = ok.clone();
        who.insert("allowed_users".into(), vec!["@max", "max smith"].into());
        assert!(check(&who).unwrap_err().contains("max smith"));
        let mut ch = ok.clone();
        ch.insert("allowed_channels".into(), vec!["town-square"].into());
        assert!(check(&ch).unwrap_err().contains("channel id"));
        ch.insert(
            "allowed_channels".into(),
            vec!["c000000000000000000000town"].into(),
        );
        assert!(check(&ch).is_ok());
    }

    #[test]
    fn a_missing_token_is_named() {
        let m: Mattermost = toml::from_str("server_url = \"https://x\"").unwrap();
        assert_eq!(m.token_env, "MATTERMOST_TOKEN");
        let e = token(&m, |_| None).unwrap_err().to_string();
        assert!(e.contains("`MATTERMOST_TOKEN` isn't set"), "{e}");
        assert_eq!(token(&m, |_| Some("t".into())).unwrap(), "t");
    }
}
