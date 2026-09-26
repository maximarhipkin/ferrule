//! The Discord and Slack steps of `ferrule setup` (M31). Same shape as
//! Telegram's: a token checked live, then who may talk to the bot. The
//! user is paired by DMing the bot a one-time code, or typed by id.

use super::{info, interruptible, ok, plural, put, table, warn, Target};
use crate::config;
use anyhow::{bail, Result};
use ferrule_gateway::channel::Channel;
use ferrule_gateway::channels::{discord, slack};
use inquire::validator::Validation;
use inquire::{Confirm, CustomUserError, MultiSelect, Select, Text};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long setup waits for the pairing DM.
const PAIR_WAIT: Duration = Duration::from_secs(120);

// ── Menu labels ────────────────────────────────────────────────────────

pub(super) fn discord_summary(cfg: &config::Config) -> String {
    let g = &cfg.gateway;
    summary(
        g.discord_token_env.as_deref().map(|e| (e, None)),
        g.discord_allowed_users.len(),
    )
}

pub(super) fn slack_summary(cfg: &config::Config) -> String {
    let g = &cfg.gateway;
    summary(
        g.slack_bot_token_env
            .as_deref()
            .map(|e| (e, g.slack_app_token_env.as_deref())),
        g.slack_allowed_users.len(),
    )
}

/// `envs`: the token variable, and a second one when the channel needs two.
fn summary(envs: Option<(&str, Option<&str>)>, users: usize) -> String {
    match envs {
        None => "off".into(),
        Some((env, second)) if !set(env) || second.is_some_and(|e| !set(e)) => {
            "token missing".into()
        }
        Some(_) => format!("on · {} allowed", plural(users, "user", "users")),
    }
}

fn set(env: &str) -> bool {
    std::env::var(env).is_ok_and(|v| !v.is_empty())
}

fn token(env: Option<&str>) -> Option<String> {
    env.and_then(|e| std::env::var(e).ok())
        .filter(|v| !v.is_empty())
}

// ── Discord ────────────────────────────────────────────────────────────

pub(super) async fn discord_step(t: &mut Target, guided: bool) -> Result<()> {
    let cfg = t.config()?;
    let api = cfg.gateway.discord_api_url.clone();
    let Some(tok) = token(cfg.gateway.discord_token_env.as_deref()).filter(|_| !guided) else {
        let ask = Confirm::new("Connect a Discord bot, so you can talk to the agent from Discord?")
            .with_default(false)
            .prompt()?;
        return if ask {
            discord_connect(t).await
        } else {
            Ok(())
        };
    };
    match interruptible(discord::probe(&api, &tok)).await? {
        Ok(p) => {
            ok(&p.bot_name);
            if p.content_intent == Some(false) {
                info("Message Content Intent is off: DMs work, server channels don't.");
            }
        }
        Err(e) => warn(e),
    }
    let allowed = cfg.gateway.discord_allowed_users.clone();
    match manage("Discord", &allowed)? {
        Manage::Allow => allow_discord(t, &api, &tok).await?,
        Manage::Keep(kept) => save_users(t, "discord_allowed_users", &kept)?,
        Manage::Replace => discord_connect(t).await?,
        Manage::Off => {
            let env = cfg.gateway.discord_token_env.clone().unwrap_or_default();
            table(t.root(), &["gateway"])?.remove("discord_token_env");
            t.save()?;
            t.forget_secret(&env)?;
            ok("Discord is off");
        }
        Manage::Nothing => {}
    }
    Ok(())
}

async fn discord_connect(t: &mut Target) -> Result<()> {
    info("1. Open https://discord.com/developers/applications → New Application");
    info("2. Bot → Reset Token, and copy it");
    info("3. For server channels, also turn on Bot → Message Content Intent (DMs don't need it)");
    info("docs/discord.md has the details.");
    let cfg = t.config()?;
    let api = cfg.gateway.discord_api_url.clone();
    let shape = |v: &str| {
        (v.split('.').count() != 3).then_some("that isn't a Discord bot token (three parts, dots)")
    };
    let (tok, probe) = loop {
        let tok = super::ask_secret("Bot token", false, shape)?.unwrap_or_default();
        match interruptible(discord::probe(&api, &tok)).await? {
            Ok(p) => {
                ok(format!("connected to {}", p.bot_name));
                break (tok, Some(p));
            }
            Err(e) if e.contains("rejected") => {
                warn(e);
                if Confirm::new("Try another token?")
                    .with_default(true)
                    .prompt()?
                {
                    continue;
                }
                bail!("Discord wasn't set up");
            }
            Err(e) => {
                warn(format!("couldn't reach Discord: {e}"));
                if Confirm::new("Save the token anyway?")
                    .with_default(true)
                    .prompt()?
                {
                    break (tok, None);
                }
                bail!("Discord wasn't set up");
            }
        }
    };
    let env = cfg
        .gateway
        .discord_token_env
        .clone()
        .unwrap_or_else(|| "DISCORD_BOT_TOKEN".into());
    t.set_secret(&env, &tok)?;
    put(
        table(t.root(), &["gateway"])?,
        "discord_token_env",
        env.as_str(),
    );
    t.save()?;
    let Some(p) = probe else {
        return Ok(());
    };
    if let Some(app_id) = &p.app_id {
        match interruptible(discord::register_commands(&api, &tok, app_id)).await? {
            Ok(n) => ok(format!(
                "registered {}",
                plural(n, "slash command", "slash commands")
            )),
            Err(e) => warn(format!(
                "couldn't register the slash commands ({e}); typed commands (/new, /status…) still work"
            )),
        }
    }
    if p.content_intent == Some(false) {
        info(
            "Message Content Intent is off: DMs work, server channels don't until you turn it on.",
        );
    }
    if let Some(app_id) = &p.app_id {
        info(format!(
            "To use it in a server too, invite it: {}",
            discord::invite_url(app_id)
        ));
    }
    allow_discord(t, &api, &tok).await
}

async fn allow_discord(t: &mut Target, api: &str, tok: &str) -> Result<()> {
    let code = pairing_code();
    info(format!(
        "Now DM the bot this code from the Discord account that should use it: {code}"
    ));
    info("(Discord → the bot's profile → Message. Share a server with it first.)");
    let ch = Arc::new(discord::DiscordChannel::with_api(tok, api).with_pairing(&code));
    let paired = wait_for(ch.clone(), move || ch.paired()).await?;
    let id_ok = |v: &str| v.len() >= 15 && v.chars().all(|c| c.is_ascii_digit());
    add_user(
        t,
        "Discord",
        "discord_allowed_users",
        paired,
        ("Discord user id", "digits, like 123456789012345678 (Settings → Advanced → Developer Mode, then right-click yourself → Copy User ID)"),
        id_ok,
    )
}

// ── Slack ──────────────────────────────────────────────────────────────

pub(super) async fn slack_step(t: &mut Target, guided: bool) -> Result<()> {
    let cfg = t.config()?;
    let g = &cfg.gateway;
    let api = g.slack_api_url.clone();
    let tokens =
        token(g.slack_bot_token_env.as_deref()).zip(token(g.slack_app_token_env.as_deref()));
    let Some((bot, app)) = tokens.filter(|_| !guided) else {
        let ask = Confirm::new("Connect a Slack app, so you can talk to the agent from Slack?")
            .with_default(false)
            .prompt()?;
        return if ask { slack_connect(t).await } else { Ok(()) };
    };
    match interruptible(slack::probe(&api, &bot, &app)).await? {
        Ok(p) => {
            ok(format!("{} in {}", p.bot_name, p.team));
            if let Err(e) = p.socket {
                warn(e);
            }
        }
        Err(e) => warn(e),
    }
    let allowed = g.slack_allowed_users.clone();
    match manage("Slack", &allowed)? {
        Manage::Allow => allow_slack(t, &api, &bot, &app).await?,
        Manage::Keep(kept) => save_users(t, "slack_allowed_users", &kept)?,
        Manage::Replace => slack_connect(t).await?,
        Manage::Off => {
            let envs = [g.slack_bot_token_env.clone(), g.slack_app_token_env.clone()];
            let gw = table(t.root(), &["gateway"])?;
            gw.remove("slack_bot_token_env");
            gw.remove("slack_app_token_env");
            t.save()?;
            for env in envs.into_iter().flatten() {
                t.forget_secret(&env)?;
            }
            ok("Slack is off");
        }
        Manage::Nothing => {}
    }
    Ok(())
}

async fn slack_connect(t: &mut Target) -> Result<()> {
    info("1. Open https://api.slack.com/apps → Create New App → From a manifest,");
    info("   and paste the manifest in docs/slack.md (Socket Mode, the bot scopes, the events)");
    info("2. Install to Workspace, then copy OAuth & Permissions → Bot User OAuth Token (xoxb-…)");
    info("3. Basic Information → App-Level Tokens → Generate, scope connections:write (xapp-…)");
    info(format!("The bot scopes: {}", slack::BOT_SCOPES.join(", ")));
    let cfg = t.config()?;
    let api = cfg.gateway.slack_api_url.clone();
    let (bot, app, reached) = loop {
        let bot =
            super::ask_secret("Bot token (xoxb-…)", false, super::no_shape)?.unwrap_or_default();
        let app = super::ask_secret("App-level token (xapp-…)", false, super::no_shape)?
            .unwrap_or_default();
        if let Err(e) = slack::check_tokens(&bot, &app) {
            warn(e);
            if Confirm::new("Try again?").with_default(true).prompt()? {
                continue;
            }
            bail!("Slack wasn't set up");
        }
        match interruptible(slack::probe(&api, &bot, &app)).await? {
            Ok(p) => {
                ok(format!("connected to {} in {}", p.bot_name, p.team));
                match p.socket {
                    Ok(()) => break (bot, app, true),
                    Err(e) => {
                        warn(e);
                        if Confirm::new("Try another app-level token?")
                            .with_default(true)
                            .prompt()?
                        {
                            continue;
                        }
                        break (bot, app, false);
                    }
                }
            }
            Err(e) if e.contains("rejected") || e.contains("invalid_auth") => {
                warn(e);
                if Confirm::new("Try other tokens?")
                    .with_default(true)
                    .prompt()?
                {
                    continue;
                }
                bail!("Slack wasn't set up");
            }
            Err(e) => {
                warn(format!("couldn't reach Slack: {e}"));
                if Confirm::new("Save the tokens anyway?")
                    .with_default(true)
                    .prompt()?
                {
                    break (bot, app, false);
                }
                bail!("Slack wasn't set up");
            }
        }
    };
    let g = &cfg.gateway;
    let bot_env = g
        .slack_bot_token_env
        .clone()
        .unwrap_or_else(|| "SLACK_BOT_TOKEN".into());
    let app_env = g
        .slack_app_token_env
        .clone()
        .unwrap_or_else(|| "SLACK_APP_TOKEN".into());
    t.set_secret(&bot_env, &bot)?;
    t.set_secret(&app_env, &app)?;
    let gw = table(t.root(), &["gateway"])?;
    put(gw, "slack_bot_token_env", bot_env.as_str());
    put(gw, "slack_app_token_env", app_env.as_str());
    t.save()?;
    if reached {
        allow_slack(t, &api, &bot, &app).await?;
    }
    Ok(())
}

async fn allow_slack(t: &mut Target, api: &str, bot: &str, app: &str) -> Result<()> {
    let code = pairing_code();
    info(format!(
        "Now DM the app this code from the Slack account that should use it: {code}"
    ));
    info("(Slack → Apps → your app → Messages. If there's no box to type in, turn on App Home → Messages Tab.)");
    let ch = Arc::new(slack::SlackChannel::with_api(bot, app, api).with_pairing(&code));
    let paired = wait_for(ch.clone(), move || ch.paired()).await?;
    let id_ok = |v: &str| {
        v.len() >= 9
            && (v.starts_with('U') || v.starts_with('W'))
            && v.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    };
    add_user(
        t,
        "Slack",
        "slack_allowed_users",
        paired,
        (
            "Slack member id",
            "like U0123ABCDEF (your profile → ⋮ → Copy member ID)",
        ),
        id_ok,
    )
}

// ── Shared ─────────────────────────────────────────────────────────────

enum Manage {
    Allow,
    Keep(Vec<String>),
    Replace,
    Off,
    Nothing,
}

fn manage(title: &str, allowed: &[String]) -> Result<Manage> {
    let mut actions = vec!["Allow another user"];
    if !allowed.is_empty() {
        actions.push("Remove allowed users");
    }
    let replace = if title == "Slack" {
        "Replace the tokens"
    } else {
        "Replace the bot token"
    };
    let off = format!("Turn {title} off");
    actions.extend([replace, off.as_str()]);
    Ok(match Select::new(&format!("{title}:"), actions).prompt()? {
        "Allow another user" => Manage::Allow,
        "Remove allowed users" => {
            let drop = MultiSelect::new(
                "Remove which? (space to mark, Enter to confirm)",
                allowed.to_vec(),
            )
            .raw_prompt()?;
            let drop: Vec<&String> = drop.iter().map(|c| &allowed[c.index]).collect();
            let kept: Vec<String> = allowed
                .iter()
                .filter(|id| !drop.contains(id))
                .cloned()
                .collect();
            ok(format!("{} left", plural(kept.len(), "user", "users")));
            Manage::Keep(kept)
        }
        a if a == replace => Manage::Replace,
        _ => {
            if Confirm::new(&format!("Turn {title} off?"))
                .with_default(false)
                .prompt()?
            {
                Manage::Off
            } else {
                Manage::Nothing
            }
        }
    })
}

/// Six digits for the user to DM the bot.
fn pairing_code() -> String {
    let n = u32::from_le_bytes(ferrule_connections::seal::random::<4>());
    format!("{:06}", n % 1_000_000)
}

/// Run the channel until someone pairs or [`PAIR_WAIT`] runs out.
async fn wait_for<C: Channel + 'static>(
    ch: Arc<C>,
    paired: impl Fn() -> Option<(String, String)>,
) -> Result<Option<(String, String)>> {
    info("Waiting up to 2 minutes…");
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let run = tokio::spawn(async move { ch.run(tx).await });
    // The admitted user's messages aren't for setup.
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let deadline = Instant::now() + PAIR_WAIT;
    let found = interruptible(async {
        while Instant::now() < deadline && !run.is_finished() {
            if let Some(who) = paired() {
                // Let the "Paired" reply go out before the socket closes.
                tokio::time::sleep(Duration::from_millis(1500)).await;
                return Some(who);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        paired()
    })
    .await;
    if run.is_finished() {
        if let Ok(Ok(Err(e))) = tokio::time::timeout(Duration::ZERO, run).await {
            warn(format!("the bot's connection closed: {e}"));
        }
    } else {
        run.abort();
    }
    drain.abort();
    found
}

fn add_user(
    t: &mut Target,
    title: &str,
    key: &str,
    paired: Option<(String, String)>,
    (label, help): (&str, &'static str),
    id_ok: fn(&str) -> bool,
) -> Result<()> {
    let cfg = t.config()?;
    let mut users = if key.starts_with("slack") {
        cfg.gateway.slack_allowed_users
    } else {
        cfg.gateway.discord_allowed_users
    };
    if let Some((id, name)) = paired {
        if !users.contains(&id) {
            users.push(id.clone());
            save_users(t, key, &users)?;
        }
        ok(format!("allowed {name} ({id})"));
        return Ok(());
    }
    info("Nobody paired. A running gateway with nobody allowed answers a DM with the id to type here.");
    let id = Text::new(&format!("{label} to allow (Enter to skip)"))
        .with_validator(move |v: &str| -> Result<Validation, CustomUserError> {
            let v = v.trim();
            Ok(if v.is_empty() || id_ok(v) {
                Validation::Valid
            } else {
                Validation::Invalid(help.into())
            })
        })
        .prompt()?;
    let id = id.trim();
    if id.is_empty() {
        info(format!(
            "Later: `ferrule setup` → {title} → Allow another user."
        ));
    } else {
        if !users.iter().any(|u| u == id) {
            users.push(id.to_string());
            save_users(t, key, &users)?;
        }
        ok(format!("allowed {id}"));
    }
    Ok(())
}

fn save_users(t: &mut Target, key: &str, ids: &[String]) -> Result<()> {
    let ids = toml_edit::Array::from_iter(ids.iter().map(String::as_str));
    put(table(t.root(), &["gateway"])?, key, ids);
    t.save()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pairing_code_is_six_digits() {
        for _ in 0..50 {
            let code = pairing_code();
            assert_eq!(code.len(), 6);
            assert!(code.chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn the_summary_names_the_missing_token() {
        assert_eq!(summary(None, 0), "off");
        assert_eq!(
            summary(Some(("FERRULE_TEST_M31_UNSET_ENV", None)), 1),
            "token missing"
        );
        assert_eq!(summary(Some(("PATH", None)), 2), "on · 2 users allowed");
        assert_eq!(
            summary(Some(("PATH", Some("FERRULE_TEST_M31_UNSET_ENV"))), 2),
            "token missing"
        );
    }
}
