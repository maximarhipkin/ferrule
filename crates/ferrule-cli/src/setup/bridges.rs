//! M59: `ferrule setup` → WhatsApp (personal) and → iMessage. Both talk to
//! a bridge the owner runs (WAHA, BlueBubbles Server): its address and key
//! checked live, then who may write, typed. Pairing the phone (WAHA's QR
//! code) or signing in to Messages is the bridge's job, not this one's.

use super::{info, interruptible, ok, plural, put, table, warn, Target};
use crate::channels::bridges;
use crate::config;
use anyhow::{bail, Result};
use ferrule_gateway::channels::bluebubbles as bb;
use ferrule_gateway::channels::waha;
use inquire::{Confirm, MultiSelect, Select, Text};

const WAHA_KEY_ENV: &str = "WAHA_API_KEY";
const BB_PASSWORD_ENV: &str = "BLUEBUBBLES_PASSWORD";

/// `127.0.0.1:3000` → `http://127.0.0.1:3000`.
fn http_url(typed: &str) -> String {
    let t = typed.trim().trim_end_matches('/');
    if t.starts_with("https://") || t.starts_with("http://") {
        t.to_string()
    } else {
        format!("http://{t}")
    }
}

fn split(text: &str) -> Vec<String> {
    text.split([',', ' ', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

// ── WhatsApp (personal) ───────────────────────────────────────────────

pub(super) fn waha_summary(cfg: &config::Config) -> String {
    match &cfg.gateway.whatsapp_personal {
        None => "off".into(),
        Some(w) if bridges::waha_key(w, crate::config_follow::secret_value).is_err() => {
            "API key missing".into()
        }
        Some(w) => format!(
            "on · WAHA {} · {} · {}",
            w.url,
            plural(w.allowed_users.len(), "number", "numbers"),
            plural(w.allowed_groups.len(), "group", "groups")
        ),
    }
}

pub(super) async fn waha_step(t: &mut Target, guided: bool) -> Result<()> {
    let cfg = t.config()?;
    let Some(w) = cfg.gateway.whatsapp_personal.clone().filter(|_| !guided) else {
        let ask = Confirm::new(
            "Connect a personal WhatsApp number (through a WAHA bridge you run; unofficial)?",
        )
        .with_default(false)
        .prompt()?;
        return if ask { waha_connect(t).await } else { Ok(()) };
    };
    match bridges::waha_config(&w, None) {
        Ok(wc) => match interruptible(waha::probe(wc)).await? {
            Ok(p) if p.working() => ok(p.summary()),
            Ok(p) => warn(p.explain()),
            Err(e) => warn(e),
        },
        Err(e) => warn(format!("{e:#}")),
    }
    let mut actions = vec!["Allow numbers", "Allow a group"];
    if !w.allowed_users.is_empty() || !w.allowed_groups.is_empty() {
        actions.push("Remove allowed numbers or groups");
    }
    actions.extend(["Change the address or key", "Turn WhatsApp (personal) off"]);
    match Select::new("WhatsApp (personal):", actions).prompt()? {
        "Allow numbers" => waha_allow(t)?,
        "Allow a group" => {
            let g =
                Text::new("The group's id (it ends in @g.us; WAHA's dashboard → Chats lists them)")
                    .prompt()?;
            let g = g.trim().to_string();
            if !waha::is_group(&g) {
                warn("that isn't a group id; nothing changed");
            } else if !w.allowed_groups.contains(&g) {
                let mut groups = w.allowed_groups.clone();
                groups.push(g);
                save_list(t, "whatsapp_personal", "allowed_groups", &groups)?;
                ok("allowed; a mention of your number there reaches the agent");
            }
        }
        "Remove allowed numbers or groups" => {
            let all: Vec<String> = w
                .allowed_users
                .iter()
                .chain(&w.allowed_groups)
                .cloned()
                .collect();
            let drop = MultiSelect::new("Remove which? (space to mark, Enter to confirm)", all)
                .prompt()?;
            let keep = |v: &[String]| -> Vec<String> {
                v.iter().filter(|u| !drop.contains(u)).cloned().collect()
            };
            save_list(
                t,
                "whatsapp_personal",
                "allowed_users",
                &keep(&w.allowed_users),
            )?;
            save_list(
                t,
                "whatsapp_personal",
                "allowed_groups",
                &keep(&w.allowed_groups),
            )?;
            ok("removed");
        }
        "Change the address or key" => waha_connect(t).await?,
        _ => {
            if Confirm::new("Turn WhatsApp (personal) off?")
                .with_default(false)
                .prompt()?
            {
                let envs = crate::channels::secret_envs_of(&cfg, "whatsapp_personal");
                table(t.root(), &["gateway"])?.remove("whatsapp_personal");
                t.save()?;
                for env in envs {
                    t.forget_secret(&env)?;
                }
                ok("WhatsApp (personal) is off; unlink the device in WhatsApp → Linked devices too");
            }
        }
    }
    Ok(())
}

async fn waha_connect(t: &mut Target) -> Result<()> {
    warn("WAHA automates WhatsApp Web. WhatsApp's terms don't allow unofficial clients, and a number can be banned for it. Use a number you can lose.");
    info("1. Run WAHA with an API key, e.g. docker run -d -p 127.0.0.1:3000:3000 -e WAHA_API_KEY=<a long random key> devlikeapro/waha");
    info("   https://waha.devlike.pro/docs/overview/quick-start/");
    info("2. In WAHA's dashboard (its address + /dashboard), start the session and scan its QR code with WhatsApp → Linked devices");
    let cfg = t.config()?;
    let old = cfg.gateway.whatsapp_personal.clone();
    let url = http_url(
        &Text::new("WAHA's address")
            .with_default(
                old.as_ref()
                    .map_or("http://127.0.0.1:3000", |w| w.url.as_str()),
            )
            .prompt()?,
    );
    let session = Text::new("The session")
        .with_default(old.as_ref().map_or("default", |w| w.session.as_str()))
        .prompt()?
        .trim()
        .to_string();
    let key = loop {
        let key = super::ask_secret("WAHA's API key", false, super::no_shape)?.unwrap_or_default();
        let p = interruptible(waha::probe(waha::WahaConfig {
            url: url.clone(),
            session: session.clone(),
            api_key: Some(key.clone()),
            inbox: None,
        }))
        .await?;
        match p {
            Ok(p) if p.working() => {
                ok(format!("connected: {}", p.summary()));
                break key;
            }
            Ok(p) => {
                warn(p.explain());
                if Confirm::new("Scanned it? Check again")
                    .with_default(true)
                    .prompt()?
                {
                    continue;
                }
                info("Saving it anyway: the gateway waits for the session to work.");
                break key;
            }
            Err(e) => {
                warn(e);
                if Confirm::new("Try again?").with_default(true).prompt()? {
                    continue;
                }
                bail!("WhatsApp (personal) wasn't set up");
            }
        }
    };
    let env = old
        .as_ref()
        .map(|w| w.api_key_env.clone())
        .unwrap_or_else(|| WAHA_KEY_ENV.to_string());
    t.set_secret(&env, &key)?;
    let tbl = table(t.root(), &["gateway", "whatsapp_personal"])?;
    put(tbl, "url", url.as_str());
    if session != "default" {
        put(tbl, "session", session.as_str());
    }
    if env != WAHA_KEY_ENV {
        put(tbl, "api_key_env", env.as_str());
    }
    t.save()?;
    ok("saved to [gateway.whatsapp_personal]; the key to the secrets file");
    waha_allow(t)
}

fn waha_allow(t: &mut Target) -> Result<()> {
    let cfg = t.config()?;
    let Some(w) = cfg.gateway.whatsapp_personal.clone() else {
        return Ok(());
    };
    info("Only these numbers reach the agent; anyone else is ignored, and never answered. Your own number allows your \"Message yourself\" chat.");
    let typed = Text::new("Numbers to allow, with the country code (Enter to skip)")
        .with_help_message("972501234567, 15551234567")
        .prompt()?;
    let mut users = w.allowed_users.clone();
    let mut added = 0;
    for n in split(&typed) {
        let n = waha::normalize(&n);
        if n.is_empty() {
            continue;
        }
        if !users.contains(&n) {
            users.push(n);
            added += 1;
        }
    }
    if added > 0 {
        save_list(t, "whatsapp_personal", "allowed_users", &users)?;
        ok(format!("allowed {}", plural(added, "number", "numbers")));
    } else if users.is_empty() {
        info("Later: `ferrule setup` → WhatsApp (personal) → Allow numbers.");
    }
    Ok(())
}

// ── iMessage ──────────────────────────────────────────────────────────

pub(super) fn bb_summary(cfg: &config::Config) -> String {
    match &cfg.gateway.imessage {
        None => "off".into(),
        Some(m) if bridges::bb_password(m, crate::config_follow::secret_value).is_err() => {
            "password missing".into()
        }
        Some(m) => format!(
            "on · {} · {}",
            m.url,
            plural(m.allowed_users.len(), "address", "addresses")
        ),
    }
}

pub(super) async fn bb_step(t: &mut Target, guided: bool) -> Result<()> {
    let cfg = t.config()?;
    let Some(m) = cfg.gateway.imessage.clone().filter(|_| !guided) else {
        let ask = Confirm::new("Connect iMessage (through BlueBubbles Server on a Mac)?")
            .with_default(false)
            .prompt()?;
        return if ask { bb_connect(t).await } else { Ok(()) };
    };
    match bridges::bb_config(&m, None) {
        Ok(bc) => match interruptible(bb::probe(bc)).await? {
            Ok(p) => ok(p.summary()),
            Err(e) => warn(e),
        },
        Err(e) => warn(format!("{e:#}")),
    }
    let mut actions = vec!["Allow people"];
    if !m.allowed_users.is_empty() {
        actions.push("Remove allowed people");
    }
    actions.extend(["Change the address or password", "Turn iMessage off"]);
    match Select::new("iMessage:", actions).prompt()? {
        "Allow people" => bb_allow(t)?,
        "Remove allowed people" => {
            let drop = MultiSelect::new(
                "Remove which? (space to mark, Enter to confirm)",
                m.allowed_users.clone(),
            )
            .prompt()?;
            let keep: Vec<String> = m
                .allowed_users
                .iter()
                .filter(|u| !drop.contains(u))
                .cloned()
                .collect();
            save_list(t, "imessage", "allowed_users", &keep)?;
            ok("removed");
        }
        "Change the address or password" => bb_connect(t).await?,
        _ => {
            if Confirm::new("Turn iMessage off?")
                .with_default(false)
                .prompt()?
            {
                let envs = crate::channels::secret_envs_of(&cfg, "imessage");
                table(t.root(), &["gateway"])?.remove("imessage");
                t.save()?;
                for env in envs {
                    t.forget_secret(&env)?;
                }
                ok("iMessage is off");
            }
        }
    }
    Ok(())
}

async fn bb_connect(t: &mut Target) -> Result<()> {
    info("iMessage needs a Mac that stays on, signed in to Messages (ideally with an Apple ID of its own), running BlueBubbles Server:");
    info("   https://bluebubbles.app/install/ (give it Full Disk Access and Accessibility, and set a server password)");
    let cfg = t.config()?;
    let old = cfg.gateway.imessage.clone();
    let mut ask = Text::new("The server's address").with_help_message(
        "the Mac's address and BlueBubbles' port, e.g. http://192.168.1.20:1234",
    );
    if let Some(m) = &old {
        ask = ask.with_default(&m.url);
    }
    let url = http_url(&ask.prompt()?);
    let password = loop {
        let pw =
            super::ask_secret("The server password", false, super::no_shape)?.unwrap_or_default();
        let p = interruptible(bb::probe(bb::BlueBubblesConfig {
            url: url.clone(),
            password: pw.clone(),
            private_api: false,
            poll: std::time::Duration::from_secs(3),
            inbox: None,
            cursor: None,
        }))
        .await?;
        match p {
            Ok(p) => {
                ok(format!("connected: {}", p.summary()));
                if p.private_api {
                    info("Its Private API is on: `private_api = true` in [gateway.imessage] adds typing and quoted replies.");
                }
                break pw;
            }
            Err(e) => {
                warn(e);
                if Confirm::new("Try again?").with_default(true).prompt()? {
                    continue;
                }
                bail!("iMessage wasn't set up");
            }
        }
    };
    let env = old
        .as_ref()
        .map(|m| m.password_env.clone())
        .unwrap_or_else(|| BB_PASSWORD_ENV.to_string());
    t.set_secret(&env, &password)?;
    let tbl = table(t.root(), &["gateway", "imessage"])?;
    put(tbl, "url", url.as_str());
    if env != BB_PASSWORD_ENV {
        put(tbl, "password_env", env.as_str());
    }
    t.save()?;
    ok("saved to [gateway.imessage]; the password to the secrets file");
    bb_allow(t)
}

fn bb_allow(t: &mut Target) -> Result<()> {
    let cfg = t.config()?;
    let Some(m) = cfg.gateway.imessage.clone() else {
        return Ok(());
    };
    info("Only these people reach the agent, in a one-to-one conversation; anyone else, and group chats, are ignored.");
    let typed = Text::new("Phone numbers or Apple ID emails to allow (Enter to skip)")
        .with_help_message("+15551234567, max@icloud.com")
        .prompt()?;
    let mut users = m.allowed_users.clone();
    let mut added = 0;
    for a in split(&typed) {
        let a = bb::normalize(&a);
        if !users.contains(&a) {
            users.push(a);
            added += 1;
        }
    }
    if added > 0 {
        save_list(t, "imessage", "allowed_users", &users)?;
        ok(format!("allowed {}", plural(added, "address", "addresses")));
    } else if users.is_empty() {
        info("Later: `ferrule setup` → iMessage → Allow people.");
    }
    Ok(())
}

fn save_list(t: &mut Target, channel: &str, key: &str, ids: &[String]) -> Result<()> {
    let ids = toml_edit::Array::from_iter(ids.iter().map(String::as_str));
    put(table(t.root(), &["gateway", channel])?, key, ids);
    t.save()
}
