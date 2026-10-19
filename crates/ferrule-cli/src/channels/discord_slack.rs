//! M50: Discord's and Slack's dashboard cards. Their settings sit in
//! `[gateway]` itself (`discord_token_env`, `slack_allowed_users`, …), not
//! in a table of their own, so the card keys are the full key names and
//! the card's save writes them there (`Spec::flat`). Test calls the same
//! probes as `ferrule setup` → Discord / Slack.

use super::card::{Field, Kind, Settings, Spec, Step};
use crate::config::GatewayConfig;
use ferrule_gateway::channels::{discord, slack};
use std::future::Future;
use std::pin::Pin;

/// The table read as `[gateway]`, for the types and the keys it knows.
fn check(t: &toml::Table) -> Result<(), String> {
    let _: GatewayConfig = toml::Value::Table(t.clone())
        .try_into()
        .map_err(|e: toml::de::Error| e.message().to_string())?;
    Ok(())
}

fn discord_probe(s: Settings) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
    Box::pin(async move {
        let token = s
            .secret("discord_token_env")
            .ok_or("paste the bot token.")?
            .to_string();
        let p = discord::probe(discord::API_URL, &token).await?;
        let mut said = format!("Works: {}.", p.bot_name);
        if let Some(app) = &p.app_id {
            said.push_str(&format!(
                " Invite it to a server of yours (Discord only lets you DM a bot you share one with): https://discord.com/oauth2/authorize?client_id={app}&scope=bot+applications.commands&permissions=274877975552"
            ));
        }
        if p.content_intent == Some(false) {
            said.push_str(" Message Content Intent is off: DMs and mentions work, other server messages don't.");
        }
        Ok(said)
    })
}

fn slack_probe(s: Settings) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
    Box::pin(async move {
        let bot = s
            .secret("slack_bot_token_env")
            .ok_or("paste the bot token (xoxb-…).")?
            .to_string();
        let app = s
            .secret("slack_app_token_env")
            .ok_or("paste the app token (xapp-…).")?
            .to_string();
        let p = slack::probe(slack::API_URL, &bot, &app).await?;
        p.socket.clone()?;
        Ok(format!("Works: {} in {}.", p.bot_name, p.team))
    })
}

const USERS_HINT: &str =
    "who may DM it; empty: nobody yet (a DM then answers with the id to put here)";

pub const DISCORD: Spec = Spec {
    name: "discord",
    fields: &[
        Field {
            key: "discord_token_env",
            label: "Bot token",
            hint: "three parts with dots, from the Bot tab (Reset Token)",
            kind: Kind::Secret {
                env: "DISCORD_BOT_TOKEN",
            },
            optional: false,
        },
        Field {
            key: "discord_allowed_users",
            label: "Allowed users",
            hint: USERS_HINT,
            kind: Kind::List,
            optional: true,
        },
        Field {
            key: "discord_allowed_channels",
            label: "Allowed channels",
            hint: "server channel ids where a mention reaches it",
            kind: Kind::List,
            optional: true,
        },
    ],
    guide: &[
        Step {
            text: "Open the Developer Portal → New Application",
            url: Some("https://discord.com/developers/applications"),
        },
        Step {
            text: "Bot → Reset Token, copy it and paste it below; for server channels also turn on Message Content Intent",
            url: None,
        },
        Step {
            text: "Test: it gives you the link that invites the bot to a server of yours",
            url: None,
        },
        Step {
            text: "Your user id: Settings → Advanced → Developer Mode, then right-click your name → Copy User ID",
            url: None,
        },
    ],
    probe: discord_probe,
    check,
};

pub const SLACK: Spec = Spec {
    name: "slack",
    fields: &[
        Field {
            key: "slack_bot_token_env",
            label: "Bot token",
            hint: "xoxb-…, from OAuth & Permissions",
            kind: Kind::Secret {
                env: "SLACK_BOT_TOKEN",
            },
            optional: false,
        },
        Field {
            key: "slack_app_token_env",
            label: "App token",
            hint: "xapp-…, from Basic Information → App-Level Tokens (scope connections:write)",
            kind: Kind::Secret {
                env: "SLACK_APP_TOKEN",
            },
            optional: false,
        },
        Field {
            key: "slack_allowed_users",
            label: "Allowed users",
            hint: USERS_HINT,
            kind: Kind::List,
            optional: true,
        },
        Field {
            key: "slack_allowed_channels",
            label: "Allowed channels",
            hint: "channel ids (C…) where a mention reaches it",
            kind: Kind::List,
            optional: true,
        },
    ],
    guide: &[
        Step {
            text: "Create New App → From a manifest, and paste the manifest from the Slack guide",
            url: Some("https://api.slack.com/apps"),
        },
        Step {
            text: "The manifest (Socket Mode, the bot's scopes and events)",
            url: Some("https://github.com/maximarhipkin/ferrule/blob/main/docs/slack.md"),
        },
        Step {
            text: "Install to Workspace, then copy the Bot User OAuth Token (xoxb-…)",
            url: None,
        },
        Step {
            text:
                "Basic Information → App-Level Tokens → Generate, scope connections:write (xapp-…)",
            url: None,
        },
    ],
    probe: slack_probe,
    check,
};
