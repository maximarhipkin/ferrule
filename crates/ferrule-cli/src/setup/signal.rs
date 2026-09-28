//! M39 §6: `ferrule setup` → Signal. signal-cli (and Java for its JVM
//! build) found or explained, never downloaded; the account picked from
//! the ones signal-cli holds, or linked here like Signal Desktop (the link
//! shown as a QR code when `qrencode` is installed); the daemon ferrule
//! starts, or one you run; then pairing and groups.

use super::channels::{pairing_code, wait_for};
use super::{info, interruptible, ok, plural, put, table, warn, Target};
use crate::channels::signal as sg;
use crate::config;
use anyhow::{bail, Result};
use ferrule_gateway::channels::signal::{self as gs, SignalConfig};
use ferrule_gateway::SignalChannel;
use inquire::validator::Validation;
use inquire::{Confirm, CustomUserError, MultiSelect, Select, Text};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

/// How long a link code waits to be scanned.
const LINK_WAIT: Duration = Duration::from_secs(180);

pub(super) fn summary(cfg: &config::Config) -> String {
    match &cfg.gateway.signal {
        None => "off".into(),
        Some(s) => format!(
            "on · {} · {} · {}",
            s.account.trim(),
            plural(s.allowed_users.len(), "user", "users"),
            plural(s.allowed_groups.len(), "group", "groups")
        ),
    }
}

pub(super) async fn step(t: &mut Target, guided: bool) -> Result<()> {
    let cfg = t.config()?;
    let Some(s) = cfg.gateway.signal.clone().filter(|_| !guided) else {
        let ask = Confirm::new("Connect Signal (through signal-cli, which you install)?")
            .with_default(false)
            .prompt()?;
        return if ask { connect(t).await } else { Ok(()) };
    };
    match interruptible(sg::test(&s)).await? {
        Ok(said) => ok(said),
        Err(e) => warn(e),
    }
    let mut actions = vec!["Allow another number", "Allow a group"];
    if !s.allowed_users.is_empty() || !s.allowed_groups.is_empty() {
        actions.push("Remove allowed numbers or groups");
    }
    actions.extend(["Change the account or daemon", "Turn Signal off"]);
    match Select::new("Signal:", actions).prompt()? {
        "Allow another number" => allow(t).await?,
        "Allow a group" => allow_group(t).await?,
        "Remove allowed numbers or groups" => {
            let all: Vec<String> = s
                .allowed_users
                .iter()
                .chain(&s.allowed_groups)
                .cloned()
                .collect();
            let drop = MultiSelect::new("Remove which? (space to mark, Enter to confirm)", all)
                .prompt()?;
            let keep = |v: &[String]| -> Vec<String> {
                v.iter().filter(|u| !drop.contains(u)).cloned().collect()
            };
            let (users, groups) = (keep(&s.allowed_users), keep(&s.allowed_groups));
            save_list(t, "allowed_users", &users)?;
            save_list(t, "allowed_groups", &groups)?;
            ok(format!(
                "{} and {} left",
                plural(users.len(), "number", "numbers"),
                plural(groups.len(), "group", "groups")
            ));
        }
        "Change the account or daemon" => connect(t).await?,
        _ => {
            if Confirm::new("Turn Signal off? (signal-cli keeps its account; `signal-cli -a <number> unregister` or removing the linked device ends it)")
                .with_default(false)
                .prompt()?
            {
                table(t.root(), &["gateway"])?.remove("signal");
                t.save()?;
                ok("Signal is off");
            }
        }
    }
    Ok(())
}

fn number_shape(v: &str) -> Result<Validation, CustomUserError> {
    let v = v.trim();
    Ok(if v.is_empty() || sg::number(v).is_ok() {
        Validation::Valid
    } else {
        Validation::Invalid("a number in international form, like +972501234567".into())
    })
}

/// Explains what's missing and how to get it.
fn install_help(found: &sg::Found) {
    warn(found.summary());
    info("Install signal-cli (ferrule doesn't download or bundle it):");
    info("  https://github.com/AsamK/signal-cli/releases — the native Linux build needs nothing else;");
    info("  the JVM build (macOS, Windows) needs Java 21 or newer: https://adoptium.net/temurin/releases/");
    info("  macOS: `brew install signal-cli` · Linux: unpack the release and put bin/ on PATH");
    match found.java {
        Some(v) if v < 21 => warn(format!(
            "Java {v} is installed; the JVM build needs 21 or newer"
        )),
        Some(v) => info(format!("Java {v} is installed")),
        None => {}
    }
}

async fn connect(t: &mut Target) -> Result<()> {
    let cfg = t.config()?;
    let old = cfg.gateway.signal.clone();
    let run_own = "A daemon I run myself (signal-cli daemon --http, or its container)";
    let spawn = "ferrule starts signal-cli's daemon on 127.0.0.1 (recommended)";
    let how = Select::new("How should ferrule reach signal-cli?", vec![spawn, run_own]).prompt()?;
    let mut url = None;
    let mut program: Option<String> = old.as_ref().and_then(|s| s.signal_cli.clone());
    let mut known: Vec<String> = Vec::new();
    if how == run_own {
        let u = Text::new("The daemon's URL")
            .with_default(
                old.as_ref()
                    .and_then(|s| s.url.as_deref())
                    .unwrap_or("http://127.0.0.1:8080"),
            )
            .with_validator(|v: &str| {
                Ok(if v.starts_with("http://") || v.starts_with("https://") {
                    Validation::Valid
                } else {
                    Validation::Invalid("a URL like http://127.0.0.1:8080".into())
                })
            })
            .prompt()?;
        url = Some(u.trim().trim_end_matches('/').to_string());
    } else {
        let probe_cfg = crate::channels::settings::Signal {
            account: String::new(),
            url: None,
            signal_cli: program.clone(),
            port: 0,
            allowed_users: vec![],
            allowed_groups: vec![],
            max_file_mb: 0,
        };
        let mut found = interruptible(sg::detect(Some(&probe_cfg))).await?;
        while found.signal_cli.is_none() {
            install_help(&found);
            let path = Text::new(
                "signal-cli's path, once installed (Enter to check PATH again, Esc to stop)",
            )
            .prompt()?;
            let path = path.trim();
            program = (!path.is_empty()).then(|| path.to_string());
            let probe_cfg = crate::channels::settings::Signal {
                signal_cli: program.clone(),
                ..probe_cfg.clone()
            };
            found = interruptible(sg::detect(Some(&probe_cfg))).await?;
        }
        let (prog, v) = found.signal_cli.clone().unwrap_or_default();
        ok(format!("signal-cli {v} ({})", prog.display()));
        match interruptible(sg::accounts(&prog)).await? {
            Ok(a) => known = a,
            Err(e) => warn(e),
        }
        if known.is_empty() {
            info("signal-cli holds no account yet.");
            info("Best: register a separate number for the agent (https://github.com/AsamK/signal-cli/wiki/Registration-with-captcha),");
            info("then come back here. Or link it to your own phone now, like Signal Desktop:");
            if Confirm::new("Link signal-cli to your phone's Signal now?")
                .with_default(true)
                .prompt()?
            {
                if let Some(n) = link(&prog).await? {
                    known.push(n);
                }
            }
        } else {
            let link_one = "Link another one to my phone (like Signal Desktop)";
            let mut choices: Vec<String> = known.clone();
            choices.push(link_one.into());
            let pick = Select::new("Which account should the agent use?", choices).prompt()?;
            if pick == link_one {
                known = link(&prog).await?.into_iter().collect();
            } else {
                known = vec![pick];
            }
        }
    }
    let account = match known.first() {
        Some(a) => a.clone(),
        None => Text::new("The account's number")
            .with_validator(number_shape)
            .with_default(old.as_ref().map_or("", |s| s.account.as_str()))
            .prompt()?
            .trim()
            .to_string(),
    };
    if sg::number(&account).is_err() {
        bail!("Signal wasn't set up: no account");
    }
    let tbl = table(t.root(), &["gateway", "signal"])?;
    put(tbl, "account", account.as_str());
    match &url {
        Some(u) => put(tbl, "url", u.as_str()),
        None => {
            tbl.remove("url");
        }
    }
    match &program {
        Some(p) if url.is_none() => put(tbl, "signal_cli", p.as_str()),
        _ => {
            tbl.remove("signal_cli");
        }
    }
    t.save()?;
    ok("saved to [gateway.signal]");
    if let Some(s) = t.config()?.gateway.signal {
        if s.url.is_some() {
            match interruptible(sg::test(&s)).await? {
                Ok(said) => ok(said),
                Err(e) => warn(e),
            }
        }
    }
    if Confirm::new("Allow your own Signal number now?")
        .with_default(true)
        .prompt()?
    {
        allow(t).await?;
    } else {
        info("Later: `ferrule setup` → Signal → Allow another number.");
    }
    Ok(())
}

/// `signal-cli link`: shows the link (a QR code when `qrencode` is on
/// PATH) and waits for the phone to scan it. The number it linked.
async fn link(prog: &std::path::Path) -> Result<Option<String>> {
    let mut child = tokio::process::Command::new(prog)
        .args(["link", "-n", "ferrule"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let Some(out) = child.stdout.take() else {
        bail!("signal-cli link printed nothing");
    };
    let mut lines = BufReader::new(out).lines();
    let first = interruptible(tokio::time::timeout(
        Duration::from_secs(60),
        lines.next_line(),
    ))
    .await?;
    let Ok(Ok(Some(uri))) = first else {
        warn("signal-cli link gave no link; its own message may say why:");
        if let Ok(o) = child.wait_with_output().await {
            warn(String::from_utf8_lossy(&o.stderr).trim().to_string());
        }
        return Ok(None);
    };
    let uri = uri.trim().to_string();
    info("On your phone: Signal → Settings → Linked devices → + (Link new device), and scan:");
    let shown = match sg::on_path("qrencode") {
        Some(q) => std::process::Command::new(q)
            .args(["-t", "ansiutf8", &uri])
            .status()
            .is_ok_and(|s| s.success()),
        None => false,
    };
    if !shown {
        info("(install `qrencode` to see it as a QR code here; or make one from this link on a machine you trust)");
    }
    info(format!("  {uri}"));
    info("Waiting up to 3 minutes…");
    let mut number = None;
    let waited = interruptible(tokio::time::timeout(LINK_WAIT, async {
        while let Ok(Some(l)) = lines.next_line().await {
            // "Associated with: +972…"
            if let Some(n) = l.split_whitespace().find(|w| sg::number(w).is_ok()) {
                number = Some(n.to_string());
            }
        }
        child.wait().await
    }))
    .await?;
    match (waited, number) {
        (Ok(Ok(s)), Some(n)) if s.success() => {
            ok(format!(
                "linked {n}; the phone's history isn't copied, only new messages arrive"
            ));
            Ok(Some(n))
        }
        (Err(_), _) => {
            warn("the link wasn't scanned in time");
            Ok(None)
        }
        _ => {
            warn("signal-cli couldn't finish linking");
            Ok(None)
        }
    }
}

/// A channel to talk to the daemon from setup: ferrule's own daemon is
/// started for the time it takes (or the gateway's is used), and nothing
/// people send is saved.
fn setup_channel(s: &crate::channels::settings::Signal) -> Result<SignalConfig> {
    let ws = std::env::temp_dir();
    let mut c = sg::config(s, Some(&ws))?;
    c.inbox = None;
    if let Some(d) = c.daemon.as_mut() {
        d.log = None;
    }
    Ok(c)
}

async fn allow(t: &mut Target) -> Result<()> {
    let cfg = t.config()?;
    let Some(s) = cfg.gateway.signal.clone() else {
        return Ok(());
    };
    let mut users = s.allowed_users.clone();
    let note = "My number is the agent's (a linked phone): allow Note to Self";
    let code = "Pair with a code: I send one to the agent's number";
    let typed = "Type my number";
    let how = Select::new("How should it know you?", vec![code, typed, note]).prompt()?;
    let id = if how == note {
        info("Messages you write in Note to Self reach the agent; it answers there.");
        Some((s.account.trim().to_string(), "Note to Self".to_string()))
    } else if how == code {
        let code = pairing_code();
        info(format!(
            "From your phone, send {} this code in Signal: {code}",
            s.account.trim()
        ));
        let ch = Arc::new(SignalChannel::new(setup_channel(&s)?).with_pairing(&code));
        wait_for(ch.clone(), move || ch.paired()).await?
    } else {
        None
    };
    let (id, name) = match id {
        Some(found) => found,
        None => {
            let n = Text::new("Your Signal number to allow (Enter to skip)")
                .with_validator(number_shape)
                .prompt()?;
            let n = n.trim().to_string();
            if n.is_empty() {
                info("Later: `ferrule setup` → Signal → Allow another number.");
                return Ok(());
            }
            (n.clone(), n)
        }
    };
    if !users.contains(&id) {
        users.push(id.clone());
        save_list(t, "allowed_users", &users)?;
    }
    ok(format!("allowed {name} ({id})"));
    Ok(())
}

async fn allow_group(t: &mut Target) -> Result<()> {
    let cfg = t.config()?;
    let Some(s) = cfg.gateway.signal.clone() else {
        return Ok(());
    };
    info("In a group, a mention of the agent (or a reply to it) reaches it, from anyone in the group.");
    let url = sg::url(&s);
    let groups = match interruptible(gs::probe(&url, &s.account)).await? {
        Ok(p) => p.groups,
        Err(e) => {
            warn(e);
            if s.url.is_none() {
                info("The daemon runs while the gateway does: start it (`ferrule gateway`, or the service), then try again.");
            }
            vec![]
        }
    };
    let left: Vec<(String, String)> = groups
        .into_iter()
        .filter(|(id, _)| !s.allowed_groups.contains(id))
        .collect();
    let id = if left.is_empty() {
        let v = Text::new("Group id (Enter to skip)").prompt()?;
        v.trim().to_string()
    } else {
        let items: Vec<String> = left.iter().map(|(id, n)| format!("{n}  ({id})")).collect();
        let pick = Select::new("Which group?", items.clone()).prompt()?;
        let i = items.iter().position(|x| *x == pick).unwrap_or(0);
        left[i].0.clone()
    };
    if id.is_empty() {
        return Ok(());
    }
    let mut all = s.allowed_groups.clone();
    all.push(id.clone());
    save_list(t, "allowed_groups", &all)?;
    ok(format!("allowed group {id}"));
    Ok(())
}

fn save_list(t: &mut Target, key: &str, ids: &[String]) -> Result<()> {
    let ids = toml_edit::Array::from_iter(ids.iter().map(String::as_str));
    put(table(t.root(), &["gateway", "signal"])?, key, ids);
    t.save()
}
