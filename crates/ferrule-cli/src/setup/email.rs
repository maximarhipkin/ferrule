//! M39 §5: `ferrule setup` → Email. A mailbox with an app password (or
//! the Gmail connection M37 already holds), its servers filled in for the
//! big providers, checked live on IMAP and SMTP; then who may write, and a
//! test mail to them.

use super::{info, interruptible, ok, plural, put, table, warn, Target};
use crate::channels::email as ce;
use crate::channels::settings::Email;
use crate::config;
use anyhow::{bail, Result};
use ferrule_gateway::channels::email as em;
use ferrule_gateway::{Channel, OutboundMessage};
use inquire::validator::Validation;
use inquire::{Confirm, CustomUserError, MultiSelect, Password, PasswordDisplayMode, Select, Text};

const PASSWORD_ENV: &str = "EMAIL_PASSWORD";

pub(super) fn summary(cfg: &config::Config) -> String {
    match &cfg.gateway.email {
        None => "off".into(),
        Some(e) => match ce::resolve(e, crate::config_follow::secret_value, ce::connection) {
            Err(_) => "password missing".into(),
            Ok(c) => format!(
                "on · {} · {}",
                c.address,
                plural(e.allowed_senders.len(), "sender", "senders")
            ),
        },
    }
}

pub(super) async fn step(t: &mut Target, guided: bool) -> Result<()> {
    let cfg = t.config()?;
    let Some(e) = cfg.gateway.email.clone().filter(|_| !guided) else {
        let ask = Confirm::new(
            "Connect a mailbox (Gmail, iCloud, Fastmail or any IMAP/SMTP server) for mail to the agent?",
        )
        .with_default(false)
        .prompt()?;
        return if ask { connect(t).await } else { Ok(()) };
    };
    match ce::config(&e, None) {
        Ok(c) => match interruptible(em::probe(c)).await? {
            Ok(p) => ok(p.summary()),
            Err(why) => warn(why),
        },
        Err(err) => warn(format!("{err:#}")),
    }
    let mut actions = vec!["Allow another sender"];
    if !e.allowed_senders.is_empty() {
        actions.extend(["Remove senders", "Send a test mail"]);
    }
    actions.extend(["Change the mailbox", "Turn email off"]);
    match Select::new("Email:", actions).prompt()? {
        "Allow another sender" => allow(t, &e).await?,
        "Remove senders" => {
            let drop = MultiSelect::new(
                "Remove which? (space to mark, Enter to confirm)",
                e.allowed_senders.clone(),
            )
            .prompt()?;
            let keep: Vec<String> = e
                .allowed_senders
                .iter()
                .filter(|s| !drop.contains(s))
                .cloned()
                .collect();
            save_senders(t, &keep)?;
            ok(format!("{} left", plural(keep.len(), "sender", "senders")));
        }
        "Send a test mail" => {
            let to = Select::new(
                "To whom?",
                e.allowed_senders
                    .iter()
                    .filter(|s| !s.starts_with('@'))
                    .cloned()
                    .collect(),
            )
            .prompt()?;
            test_mail(&e, &to).await;
        }
        "Change the mailbox" => connect(t).await?,
        _ => {
            if Confirm::new("Turn email off?")
                .with_default(false)
                .prompt()?
            {
                let envs = crate::channels::secret_envs_of(&cfg, "email");
                table(t.root(), &["gateway"])?.remove("email");
                t.save()?;
                for env in envs {
                    t.forget_secret(&env)?;
                }
                if let Some(dir) = ce::state_dir() {
                    let _ = std::fs::remove_file(dir.join("state.json"));
                }
                ok("email is off");
            }
        }
    }
    Ok(())
}

fn an_address(v: &str) -> Result<Validation, CustomUserError> {
    let v = v.trim();
    Ok(match v.split_once('@') {
        Some((l, d)) if !l.is_empty() && d.contains('.') && !v.contains(' ') => Validation::Valid,
        _ => Validation::Invalid("an address like you@example.com".into()),
    })
}

/// The Gmail connections M37 holds, by name.
fn gmail_connections() -> Vec<String> {
    let Ok(dir) = crate::secrets::private_dir() else {
        return Vec::new();
    };
    ferrule_connections::Store::new(&dir)
        .load()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.service.native.as_deref() == Some("gmail"))
        .map(|r| r.name)
        .collect()
}

async fn connect(t: &mut Target) -> Result<()> {
    info("Best: a separate mailbox for the agent (yourname.agent@gmail.com), so it never reads your own mail.");
    info("It reads only new mail from the senders you allow, and leaves the rest unread.");
    let cfg = t.config()?;
    let old = cfg.gateway.email.clone().unwrap_or_default();
    let conns = gmail_connections();
    let mut how = Vec::new();
    for c in &conns {
        how.push(format!(
            "Use the Gmail connection `{c}` (its address and app password)"
        ));
    }
    how.push("Another mailbox, with an app password".to_string());
    let pick = if conns.is_empty() {
        how.len() - 1
    } else {
        Select::new("Which mailbox?", how.clone())
            .raw_prompt()?
            .index
    };
    let mut e = Email {
        allowed_senders: old.allowed_senders.clone(),
        max_file_mb: old.max_file_mb.max(1),
        poll_secs: old.poll_secs,
        require_auth_results: old.require_auth_results,
        ..Email::default()
    };
    let mut password = None;
    if let Some(name) = conns.get(pick) {
        e.use_connection = Some(name.clone());
    } else {
        info("Gmail: turn on 2-Step Verification, then make one at https://myaccount.google.com/apppasswords");
        info("iCloud: https://support.apple.com/en-us/102654 · Yahoo and Fastmail: their account security pages.");
        let mut ask = Text::new("The mailbox's address").with_validator(an_address);
        if let Some(a) = old.address.as_deref() {
            ask = ask.with_default(a);
        }
        let address = ask.prompt()?.trim().to_string();
        if ce::provider(&address).is_none() {
            info("Your provider's help pages name its IMAP and SMTP servers.");
            let imap = Text::new("IMAP server")
                .with_help_message(
                    "imap.example.com; port 993 (TLS) is assumed, 143 means STARTTLS",
                )
                .with_default(old.imap_host.as_deref().unwrap_or(""))
                .prompt()?;
            let smtp = Text::new("SMTP server")
                .with_help_message("smtp.example.com")
                .with_default(old.smtp_host.as_deref().unwrap_or(""))
                .prompt()?;
            let port = Select::new("SMTP port", vec!["465 (TLS)", "587 (STARTTLS)"]).prompt()?;
            e.imap_host = Some(imap.trim().to_string());
            e.smtp_host = Some(smtp.trim().to_string());
            e.smtp_port = Some(if port.starts_with("587") { 587 } else { 465 });
        }
        e.address = Some(address);
        e.password_env = Some(
            old.password_env
                .clone()
                .unwrap_or_else(|| PASSWORD_ENV.to_string()),
        );
        let pw = Password::new("App password")
            .with_help_message("not your normal password; Gmail's 16 letters, spaces are fine")
            .without_confirmation()
            .with_display_mode(PasswordDisplayMode::Masked)
            .prompt()?;
        password = Some(ce::app_password(&pw));
    }
    let resolved = ce::resolve(&e, |_| password.clone(), ce::connection);
    let checked = match resolved {
        Ok(c) => interruptible(em::probe(c))
            .await?
            .map_err(|w| w.to_string()),
        Err(err) => Err(format!("{err:#}")),
    };
    match checked {
        Ok(p) => ok(format!("connected: {}", p.summary())),
        Err(why) => {
            warn(why);
            bail!("email wasn't set up; nothing was saved");
        }
    }
    if let (Some(env), Some(pw)) = (&e.password_env, &password) {
        t.set_secret(env, pw)?;
    }
    let old_env = old
        .password_env
        .clone()
        .filter(|o| e.password_env.as_ref() != Some(o));
    let tbl = table(t.root(), &["gateway", "email"])?;
    for key in [
        "use_connection",
        "address",
        "imap_host",
        "smtp_host",
        "imap_port",
        "smtp_port",
        "username",
        "password_env",
    ] {
        tbl.remove(key);
    }
    if let Some(c) = &e.use_connection {
        put(tbl, "use_connection", c.as_str());
    }
    if let Some(a) = &e.address {
        put(tbl, "address", a.as_str());
    }
    if let Some(h) = &e.imap_host {
        put(tbl, "imap_host", h.as_str());
    }
    if let Some(h) = &e.smtp_host {
        put(tbl, "smtp_host", h.as_str());
    }
    if let Some(p) = e.smtp_port {
        put(tbl, "smtp_port", i64::from(p));
    }
    if let Some(env) = &e.password_env {
        put(tbl, "password_env", env.as_str());
    }
    t.save()?;
    if let Some(env) = old_env {
        t.forget_secret(&env)?;
    }
    // Another mailbox's UIDs mean nothing here.
    if let Some(dir) = ce::state_dir() {
        let _ = std::fs::remove_file(dir.join("state.json"));
    }
    ok("saved to [gateway.email]");
    let cfg = t.config()?;
    if let Some(e) = cfg.gateway.email.clone() {
        allow(t, &e).await?;
    }
    Ok(())
}

async fn allow(t: &mut Target, e: &Email) -> Result<()> {
    let who = Text::new("Your own address, to allow (Enter to skip)")
        .with_help_message("mail from it reaches the agent; @example.com allows a whole domain")
        .with_validator(|v: &str| {
            let v = v.trim();
            Ok(if v.is_empty() || ce::sender_ok(v) {
                Validation::Valid
            } else {
                Validation::Invalid("an address, or @example.com for a domain".into())
            })
        })
        .prompt()?;
    let who = who.trim().to_ascii_lowercase();
    if who.is_empty() {
        info("Later: `ferrule setup` → Email → Allow another sender.");
        return Ok(());
    }
    let mut senders = e.allowed_senders.clone();
    if !senders.iter().any(|s| s.eq_ignore_ascii_case(&who)) {
        senders.push(who.clone());
        save_senders(t, &senders)?;
    }
    ok(format!("allowed {who}"));
    if !who.starts_with('@')
        && Confirm::new(&format!("Send {who} a test mail?"))
            .with_default(true)
            .prompt()?
    {
        test_mail(e, &who).await;
    }
    Ok(())
}

async fn test_mail(e: &Email, to: &str) {
    let c = match ce::config(e, None) {
        Ok(c) => c,
        Err(err) => return warn(format!("{err:#}")),
    };
    let from = c.address.clone();
    // Setup keeps nothing: no threads, no UIDs.
    let ch = em::EmailChannel::new(em::EmailConfig {
        state_dir: None,
        ..c
    });
    let msg = OutboundMessage {
        channel: "email".into(),
        chat_id: to.to_string(),
        text: format!("This is ferrule's test mail from {from}. Reply to it and the agent answers in this thread."),
        reply_to: None,
        attachments: Vec::new(),
    };
    match interruptible(ch.send(msg)).await {
        Ok(Ok(_)) => ok(format!(
            "sent to {to}; check its spam folder if it isn't in the inbox"
        )),
        Ok(Err(err)) => warn(err),
        Err(_) => warn("stopped"),
    }
}

fn save_senders(t: &mut Target, senders: &[String]) -> Result<()> {
    let list = toml_edit::Array::from_iter(senders.iter().map(String::as_str));
    put(
        table(t.root(), &["gateway", "email"])?,
        "allowed_senders",
        list,
    );
    t.save()
}
