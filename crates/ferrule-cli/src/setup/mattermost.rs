//! M39 §7: `ferrule setup` → Mattermost. The server's URL, a bot account's
//! token checked live, then pairing (the owner DMs the bot a code) or a
//! typed username, and the channels it may answer a mention in.

use super::channels::{pairing_code, wait_for};
use super::{info, interruptible, ok, plural, put, table, warn, Target};
use crate::config;
use anyhow::{bail, Result};
use ferrule_gateway::channels::mattermost::{self as mm, MattermostChannel, MattermostConfig};
use inquire::{Confirm, MultiSelect, Select, Text};
use std::sync::Arc;

const TOKEN_ENV: &str = "MATTERMOST_TOKEN";

pub(super) fn summary(cfg: &config::Config) -> String {
    match &cfg.gateway.mattermost {
        None => "off".into(),
        Some(m)
            if crate::channels::mattermost::token(m, crate::config_follow::secret_value)
                .is_err() =>
        {
            "token missing".into()
        }
        Some(m) => format!(
            "on · {} · {}",
            plural(m.allowed_users.len(), "user", "users"),
            plural(m.allowed_channels.len(), "channel", "channels")
        ),
    }
}

pub(super) async fn step(t: &mut Target, guided: bool) -> Result<()> {
    let cfg = t.config()?;
    let Some(m) = cfg.gateway.mattermost.clone().filter(|_| !guided) else {
        let ask = Confirm::new("Connect a Mattermost bot account?")
            .with_default(false)
            .prompt()?;
        return if ask { connect(t).await } else { Ok(()) };
    };
    match crate::channels::mattermost::config(&m, None) {
        Ok(mc) => match interruptible(mm::probe(mc)).await? {
            Ok(p) => ok(p.summary()),
            Err(e) => warn(e),
        },
        Err(e) => warn(format!("{e:#}")),
    }
    let mut actions = vec!["Allow another user", "Allow a channel"];
    if !m.allowed_users.is_empty() || !m.allowed_channels.is_empty() {
        actions.push("Remove allowed users or channels");
    }
    actions.extend(["Change the token", "Turn Mattermost off"]);
    match Select::new("Mattermost:", actions).prompt()? {
        "Allow another user" => allow(t).await?,
        "Allow a channel" => allow_channel(t).await?,
        "Remove allowed users or channels" => {
            let all: Vec<String> = m
                .allowed_users
                .iter()
                .chain(&m.allowed_channels)
                .cloned()
                .collect();
            let drop = MultiSelect::new("Remove which? (space to mark, Enter to confirm)", all)
                .prompt()?;
            let keep = |v: &[String]| -> Vec<String> {
                v.iter().filter(|u| !drop.contains(u)).cloned().collect()
            };
            let (users, chans) = (keep(&m.allowed_users), keep(&m.allowed_channels));
            save_list(t, "allowed_users", &users)?;
            save_list(t, "allowed_channels", &chans)?;
            ok(format!(
                "{} and {} left",
                plural(users.len(), "user", "users"),
                plural(chans.len(), "channel", "channels")
            ));
        }
        "Change the token" => connect(t).await?,
        _ => {
            if Confirm::new("Turn Mattermost off?")
                .with_default(false)
                .prompt()?
            {
                let envs = crate::channels::secret_envs_of(&cfg, "mattermost");
                table(t.root(), &["gateway"])?.remove("mattermost");
                t.save()?;
                for env in envs {
                    t.forget_secret(&env)?;
                }
                ok("Mattermost is off");
            }
        }
    }
    Ok(())
}

/// `chat.example.com` → `https://chat.example.com`.
fn server_url(typed: &str) -> String {
    let t = typed.trim().trim_end_matches('/');
    if t.starts_with("https://") || t.starts_with("http://") {
        t.to_string()
    } else {
        format!("https://{t}")
    }
}

async fn connect(t: &mut Target) -> Result<()> {
    info("1. As an admin: System Console → Integrations → Bot Accounts → Enable Bot Account Creation");
    info("   https://developers.mattermost.com/integrate/reference/bot-accounts/");
    info("2. Integrations → Bot Accounts → Add Bot Account; then Create New Token and copy it");
    info("3. Add the bot to your team (and later to each channel it should answer in)");
    let cfg = t.config()?;
    let old = cfg.gateway.mattermost.clone();
    let mut ask = Text::new("The server's address")
        .with_help_message("what you open Mattermost at, e.g. https://chat.example.com");
    if let Some(m) = &old {
        ask = ask.with_default(&m.server_url);
    }
    let url = server_url(&ask.prompt()?);
    let token = loop {
        let token = super::ask_secret("The bot's access token", false, super::no_shape)?
            .unwrap_or_default();
        let p = interruptible(mm::probe(MattermostConfig {
            server_url: url.clone(),
            token: token.clone(),
            inbox: None,
        }))
        .await?;
        match p {
            Ok(p) => {
                ok(format!("connected: {}", p.summary()));
                break token;
            }
            Err(e) => {
                warn(e);
                if Confirm::new("Try again?").with_default(true).prompt()? {
                    continue;
                }
                bail!("Mattermost wasn't set up");
            }
        }
    };
    let env = old
        .as_ref()
        .map(|m| m.token_env.clone())
        .unwrap_or_else(|| TOKEN_ENV.to_string());
    t.set_secret(&env, &token)?;
    let tbl = table(t.root(), &["gateway", "mattermost"])?;
    put(tbl, "server_url", url.as_str());
    if env != TOKEN_ENV {
        put(tbl, "token_env", env.as_str());
    }
    t.save()?;
    ok("saved to [gateway.mattermost]; the token to the secrets file");
    if Confirm::new("Pair your own Mattermost account now?")
        .with_default(true)
        .prompt()?
    {
        allow(t).await?;
    } else {
        info("Later: `ferrule setup` → Mattermost → Allow another user.");
    }
    Ok(())
}

async fn allow(t: &mut Target) -> Result<()> {
    let cfg = t.config()?;
    let Some(m) = cfg.gateway.mattermost.clone() else {
        return Ok(());
    };
    let code = pairing_code();
    let mc = match crate::channels::mattermost::config(&m, None) {
        Ok(mc) => mc,
        Err(e) => {
            warn(format!("{e:#}"));
            return Ok(());
        }
    };
    let bot = match interruptible(mm::probe(mc.clone())).await? {
        Ok(p) => format!("@{}", p.username),
        Err(_) => "the bot".into(),
    };
    info(format!(
        "From your own Mattermost account, send {bot} a direct message with this code: {code}"
    ));
    let ch = Arc::new(MattermostChannel::new(mc.clone()).with_pairing(&code));
    let paired = wait_for(ch.clone(), move || ch.paired()).await?;
    let mut users = m.allowed_users.clone();
    if let Some((id, name)) = paired {
        if !users.contains(&id) {
            users.push(id.clone());
            save_list(t, "allowed_users", &users)?;
        }
        ok(format!("allowed @{name} ({id})"));
        return Ok(());
    }
    let name = Text::new("Your Mattermost username to allow (Enter to skip)").prompt()?;
    let name = name.trim().trim_start_matches('@').to_string();
    if name.is_empty() {
        info("Later: `ferrule setup` → Mattermost → Allow another user.");
        return Ok(());
    }
    match interruptible(mm::user_id(mc, &name)).await? {
        Ok(id) => {
            if !users.contains(&id) {
                users.push(id.clone());
                save_list(t, "allowed_users", &users)?;
            }
            ok(format!("allowed @{name} ({id})"));
        }
        Err(e) => warn(e),
    }
    Ok(())
}

async fn allow_channel(t: &mut Target) -> Result<()> {
    let cfg = t.config()?;
    let Some(m) = cfg.gateway.mattermost.clone() else {
        return Ok(());
    };
    info("In the channel, a mention of the bot reaches the agent, from anyone in it; it answers in a thread.");
    let mc = crate::channels::mattermost::config(&m, None)?;
    let list = match interruptible(mm::channels(mc)).await? {
        Ok(l) => l,
        Err(e) => {
            warn(e);
            return Ok(());
        }
    };
    let fresh: Vec<_> = list
        .into_iter()
        .filter(|c| !m.allowed_channels.contains(&c.id))
        .collect();
    if fresh.is_empty() {
        info("The bot isn't in any other channel: invite it first (/invite @bot in the channel).");
        return Ok(());
    }
    let labels: Vec<String> = fresh
        .iter()
        .map(|c| format!("~{} ({}, {})", c.name, c.team, c.id))
        .collect();
    let picked = MultiSelect::new(
        "Which channels? (space to mark, Enter to confirm)",
        labels.clone(),
    )
    .prompt()?;
    let mut chans = m.allowed_channels.clone();
    for p in &picked {
        let i = labels.iter().position(|l| l == p).unwrap_or(0);
        chans.push(fresh[i].id.clone());
    }
    if !picked.is_empty() {
        save_list(t, "allowed_channels", &chans)?;
        ok(format!(
            "allowed {}",
            plural(picked.len(), "channel", "channels")
        ));
    }
    Ok(())
}

fn save_list(t: &mut Target, key: &str, ids: &[String]) -> Result<()> {
    let ids = toml_edit::Array::from_iter(ids.iter().map(String::as_str));
    put(table(t.root(), &["gateway", "mattermost"])?, key, ids);
    t.save()
}

#[cfg(test)]
mod tests {
    use super::server_url;

    #[test]
    fn a_bare_host_becomes_https() {
        assert_eq!(server_url("chat.example.com/"), "https://chat.example.com");
        assert_eq!(
            server_url(" http://127.0.0.1:8065 "),
            "http://127.0.0.1:8065"
        );
    }
}
