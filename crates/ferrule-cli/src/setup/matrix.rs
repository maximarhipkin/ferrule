//! M39 §4: `ferrule setup` → Matrix. The homeserver found from what was
//! typed (`matrix.org`, `@bot:matrix.org` or a URL), a token pasted or
//! made here from the bot's password (only the token is kept), checked
//! live, then pairing: the owner invites the bot and sends it a code.

use super::channels::{pairing_code, wait_for};
use super::{info, interruptible, ok, plural, put, table, warn, Target};
use crate::config;
use anyhow::{bail, Result};
use ferrule_gateway::channels::matrix::{self as mx, Login, MatrixChannel, MatrixConfig};
use inquire::validator::Validation;
use inquire::{Confirm, CustomUserError, MultiSelect, Password, PasswordDisplayMode, Select, Text};
use std::sync::Arc;

const TOKEN_ENV: &str = "MATRIX_ACCESS_TOKEN";

pub(super) fn summary(cfg: &config::Config) -> String {
    match &cfg.gateway.matrix {
        None => "off".into(),
        Some(m)
            if crate::channels::matrix::login(m, crate::config_follow::secret_value).is_err() =>
        {
            "login missing".into()
        }
        Some(m) => format!(
            "on · {} · {}",
            plural(m.allowed_users.len(), "user", "users"),
            plural(m.allowed_rooms.len(), "room", "rooms")
        ),
    }
}

pub(super) async fn step(t: &mut Target, guided: bool) -> Result<()> {
    let cfg = t.config()?;
    let Some(m) = cfg.gateway.matrix.clone().filter(|_| !guided) else {
        let ask =
            Confirm::new("Connect a Matrix bot account (any homeserver; unencrypted rooms only)?")
                .with_default(false)
                .prompt()?;
        return if ask { connect(t).await } else { Ok(()) };
    };
    match crate::channels::matrix::config(&m, None) {
        Ok(mc) => {
            let mc = MatrixConfig {
                state_dir: None,
                ..mc
            };
            match interruptible(mx::probe(mc)).await? {
                Ok(p) => ok(p.summary()),
                Err(e) => warn(e),
            }
        }
        Err(e) => warn(format!("{e:#}")),
    }
    let mut actions = vec!["Allow another user", "Allow a room"];
    if !m.allowed_users.is_empty() || !m.allowed_rooms.is_empty() {
        actions.push("Remove allowed users or rooms");
    }
    actions.extend(["Log in again", "Turn Matrix off"]);
    match Select::new("Matrix:", actions).prompt()? {
        "Allow another user" => allow(t).await?,
        "Allow a room" => allow_room(t)?,
        "Remove allowed users or rooms" => {
            let all: Vec<String> = m
                .allowed_users
                .iter()
                .chain(&m.allowed_rooms)
                .cloned()
                .collect();
            let drop = MultiSelect::new("Remove which? (space to mark, Enter to confirm)", all)
                .prompt()?;
            let keep = |v: &[String]| -> Vec<String> {
                v.iter().filter(|u| !drop.contains(u)).cloned().collect()
            };
            let (users, rooms) = (keep(&m.allowed_users), keep(&m.allowed_rooms));
            save_list(t, "allowed_users", &users)?;
            save_list(t, "allowed_rooms", &rooms)?;
            ok(format!(
                "{} and {} left",
                plural(users.len(), "user", "users"),
                plural(rooms.len(), "room", "rooms")
            ));
        }
        "Log in again" => connect(t).await?,
        _ => {
            if Confirm::new("Turn Matrix off?")
                .with_default(false)
                .prompt()?
            {
                let envs = crate::channels::secret_envs_of(&cfg, "matrix");
                table(t.root(), &["gateway"])?.remove("matrix");
                t.save()?;
                for env in envs {
                    t.forget_secret(&env)?;
                }
                if let Some(dir) = crate::channels::matrix::state_dir() {
                    let _ = std::fs::remove_file(dir.join("session.json"));
                }
                ok("Matrix is off");
            }
        }
    }
    Ok(())
}

fn shaped(
    first: char,
    what: &'static str,
) -> impl Fn(&str) -> Result<Validation, CustomUserError> + Clone {
    move |v: &str| {
        let v = v.trim();
        Ok(
            if v.is_empty() || (v.starts_with(first) && v.contains(':') && !v.contains(' ')) {
                Validation::Valid
            } else {
                Validation::Invalid(what.into())
            },
        )
    }
}

async fn connect(t: &mut Target) -> Result<()> {
    info("1. Register a separate account for the bot, e.g. https://app.element.io/#/register");
    info("   (your own account stays yours: the bot answers you from its own)");
    info("2. Encrypted rooms are refused: ferrule can't read end-to-end encrypted messages.");
    info("   DMs it opens are unencrypted; rooms you make for it need \"Enable end-to-end encryption\" off.");
    let cfg = t.config()?;
    let old = cfg.gateway.matrix.clone();
    let typed = Text::new("The bot's homeserver, or its user id")
        .with_help_message("matrix.org, @bot:matrix.org or https://matrix.example.org")
        .with_default(old.as_ref().map_or("matrix.org", |m| m.homeserver.as_str()))
        .prompt()?;
    let homeserver = interruptible(mx::discover(&typed)).await?;
    info(format!("homeserver: {homeserver}"));
    let paste = "Paste an access token (Element: Settings → Help & About → Access token)";
    let password = "Log in with the bot's user and password (only the token is kept)";
    let (token, user_id) = loop {
        let how = Select::new("How should it log in?", vec![password, paste]).prompt()?;
        let attempt = if how == paste {
            info("Close Element's tab without logging out: logging out ends that token.");
            let token = super::ask_secret("Access token (syt_…)", false, super::no_shape)?
                .unwrap_or_default();
            let p = interruptible(mx::probe(MatrixConfig {
                homeserver: homeserver.clone(),
                login: Login::Token(token.clone()),
                state_dir: None,
                inbox: None,
            }))
            .await?;
            p.map(|p| (token, p))
        } else {
            let default_user = typed
                .trim()
                .starts_with('@')
                .then(|| typed.trim().to_string());
            let mut ask = Text::new("The bot's user id")
                .with_validator(shaped('@', "a full user id like @bot:matrix.org"));
            if let Some(u) = default_user.as_deref() {
                ask = ask.with_default(u);
            }
            let user = ask.prompt()?.trim().to_string();
            let pw = Password::new("Its password")
                .without_confirmation()
                .with_display_mode(PasswordDisplayMode::Masked)
                .prompt()?;
            match interruptible(mx::login_for_token(&homeserver, &user, &pw)).await? {
                Ok((_, token)) => {
                    let p = interruptible(mx::probe(MatrixConfig {
                        homeserver: homeserver.clone(),
                        login: Login::Token(token.clone()),
                        state_dir: None,
                        inbox: None,
                    }))
                    .await?;
                    p.map(|p| (token, p))
                }
                Err(e) => Err(e),
            }
        };
        match attempt {
            Ok((token, p)) => {
                ok(format!("connected: {}", p.summary()));
                if !p.encrypted.is_empty() {
                    warn(format!(
                        "it won't answer in the encrypted room(s) it's in: {}",
                        p.encrypted.join(", ")
                    ));
                }
                break (token, p.user_id);
            }
            Err(e) => {
                warn(e);
                if Confirm::new("Try again?").with_default(true).prompt()? {
                    continue;
                }
                bail!("Matrix wasn't set up");
            }
        }
    };
    let env = old
        .as_ref()
        .and_then(|m| m.access_token_env.clone())
        .unwrap_or_else(|| TOKEN_ENV.to_string());
    let old_password = old.as_ref().and_then(|m| m.password_env.clone());
    t.set_secret(&env, &token)?;
    let tbl = table(t.root(), &["gateway", "matrix"])?;
    put(tbl, "homeserver", homeserver.as_str());
    put(tbl, "access_token_env", env.as_str());
    // Whose token it is: said by the doctor, compared across instances.
    put(tbl, "user", user_id.as_str());
    tbl.remove("password_env");
    t.save()?;
    if let Some(pw) = old_password {
        t.forget_secret(&pw)?;
    }
    // A session saved from an earlier password login is someone else's
    // now.
    if let Some(dir) = crate::channels::matrix::state_dir() {
        let _ = std::fs::remove_file(dir.join("session.json"));
    }
    ok("saved to [gateway.matrix]; the token to the secrets file");
    if Confirm::new("Pair your own Matrix account now?")
        .with_default(true)
        .prompt()?
    {
        allow(t).await?;
    } else {
        info("Later: `ferrule setup` → Matrix → Allow another user.");
    }
    Ok(())
}

async fn allow(t: &mut Target) -> Result<()> {
    let cfg = t.config()?;
    let Some(m) = cfg.gateway.matrix.clone() else {
        return Ok(());
    };
    let code = pairing_code();
    let paired = match crate::channels::matrix::config(&m, None) {
        Ok(mc) => {
            let bot = m.user.clone().unwrap_or_else(|| "the bot".into());
            info(format!(
                "From your own Matrix account, start a direct chat with {bot} (it joins by itself), then send it this code: {code}"
            ));
            // Setup keeps nothing: no files, no sync position.
            let mc = MatrixConfig {
                state_dir: None,
                inbox: None,
                ..mc
            };
            let ch = Arc::new(MatrixChannel::new(mc).with_pairing(&code));
            wait_for(ch.clone(), move || ch.paired()).await?
        }
        Err(e) => {
            warn(format!("{e:#}"));
            None
        }
    };
    let mut users = m.allowed_users.clone();
    if let Some((id, name)) = paired {
        if !users.contains(&id) {
            users.push(id.clone());
            save_list(t, "allowed_users", &users)?;
        }
        ok(format!("allowed {name} ({id})"));
        return Ok(());
    }
    let id = Text::new("Your Matrix user id to allow (Enter to skip)")
        .with_validator(shaped('@', "a full user id like @you:matrix.org"))
        .prompt()?;
    let id = id.trim().to_string();
    if id.is_empty() {
        info("Later: `ferrule setup` → Matrix → Allow another user.");
    } else {
        if !users.contains(&id) {
            users.push(id.clone());
            save_list(t, "allowed_users", &users)?;
        }
        ok(format!("allowed {id}"));
    }
    Ok(())
}

fn allow_room(t: &mut Target) -> Result<()> {
    let cfg = t.config()?;
    let Some(m) = cfg.gateway.matrix.clone() else {
        return Ok(());
    };
    info("In the room, a mention of the bot (or a reply to it) reaches the agent, from anyone in it.");
    info("Element: Room settings → Advanced → Internal room ID. The room must be unencrypted, and the bot invited.");
    let id = Text::new("Room id (Enter to skip)")
        .with_validator(shaped('!', "a room id like !abc123:matrix.org"))
        .prompt()?;
    let id = id.trim().to_string();
    if id.is_empty() {
        return Ok(());
    }
    let mut rooms = m.allowed_rooms.clone();
    if !rooms.contains(&id) {
        rooms.push(id.clone());
        save_list(t, "allowed_rooms", &rooms)?;
    }
    ok(format!("allowed {id}"));
    Ok(())
}

fn save_list(t: &mut Target, key: &str, ids: &[String]) -> Result<()> {
    let ids = toml_edit::Array::from_iter(ids.iter().map(String::as_str));
    put(table(t.root(), &["gateway", "matrix"])?, key, ids);
    t.save()
}
