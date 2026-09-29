//! M39 §3: `ferrule setup` → WhatsApp. The business number's id and token
//! checked live, the app secret, a verify token made here, where webhooks
//! come in (the relay's mailbox or a local listener behind a tunnel), then
//! what to paste into Meta's Webhooks form, and pairing by a code sent
//! from the owner's own WhatsApp.

use super::channels::{pairing_code, wait_for};
use super::{info, interruptible, ok, plural, put, table, warn, Target};
use crate::channels::settings::WhatsAppInbound;
use crate::channels::whatsapp as cw;
use crate::config;
use anyhow::{bail, Result};
use ferrule_gateway::channels::whatsapp::{self as wa, Inbound, WhatsAppChannel, WhatsAppConfig};
use inquire::validator::Validation;
use inquire::{Confirm, CustomUserError, MultiSelect, Select, Text};
use std::sync::Arc;

pub(super) fn summary(cfg: &config::Config) -> String {
    match &cfg.gateway.whatsapp {
        None => "off".into(),
        Some(w) if crate::config_follow::secret_value(&w.token_env).is_none() => {
            "token missing".into()
        }
        Some(w) => format!(
            "on · {} allowed",
            plural(w.allowed_users.len(), "number", "numbers")
        ),
    }
}

pub(super) async fn step(t: &mut Target, guided: bool) -> Result<()> {
    let cfg = t.config()?;
    let Some(w) = cfg.gateway.whatsapp.clone().filter(|_| !guided) else {
        let ask = Confirm::new(
            "Connect a WhatsApp Business number (Meta's Cloud API — not your personal WhatsApp)?",
        )
        .with_default(false)
        .prompt()?;
        return if ask { connect(t).await } else { Ok(()) };
    };
    let token = crate::config_follow::secret_value(&w.token_env).unwrap_or_default();
    match interruptible(wa::probe(
        &w.api_url,
        &w.api_version,
        &w.phone_number_id,
        &token,
    ))
    .await?
    {
        Ok(p) => ok(format!("{} {}", p.number, p.name)),
        Err(e) => warn(e),
    }
    let mut actions = vec!["Allow another number"];
    if !w.allowed_users.is_empty() {
        actions.push("Remove allowed numbers");
    }
    actions.extend([
        "Show the webhook settings for Meta",
        "Replace the credentials",
        "Turn WhatsApp off",
    ]);
    match Select::new("WhatsApp:", actions).prompt()? {
        "Allow another number" => allow(t).await?,
        "Remove allowed numbers" => {
            let drop = MultiSelect::new(
                "Remove which? (space to mark, Enter to confirm)",
                w.allowed_users.clone(),
            )
            .prompt()?;
            let kept: Vec<String> = w
                .allowed_users
                .iter()
                .filter(|u| !drop.contains(u))
                .cloned()
                .collect();
            save_users(t, &kept)?;
            ok(format!("{} left", plural(kept.len(), "number", "numbers")));
        }
        "Show the webhook settings for Meta" => webhook(t).await?,
        "Replace the credentials" => connect(t).await?,
        _ => {
            if Confirm::new("Turn WhatsApp off?")
                .with_default(false)
                .prompt()?
            {
                let envs = crate::channels::secret_envs_of(&cfg, "whatsapp");
                table(t.root(), &["gateway"])?.remove("whatsapp");
                t.save()?;
                for env in envs {
                    t.forget_secret(&env)?;
                }
                ok("WhatsApp is off");
            }
        }
    }
    Ok(())
}

fn digits(what: &'static str) -> impl Fn(&str) -> Result<Validation, CustomUserError> + Clone {
    move |v: &str| {
        let v = v.trim();
        Ok(if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) {
            Validation::Valid
        } else {
            Validation::Invalid(what.into())
        })
    }
}

async fn connect(t: &mut Target) -> Result<()> {
    info("1. https://developers.facebook.com/apps → Create app → Business, and add the WhatsApp product");
    info("2. WhatsApp → API Setup: copy the Phone number ID (while testing, add your own number under To)");
    info(
        "3. https://business.facebook.com/settings/system-users → a system user → Generate token,",
    );
    info("   with whatsapp_business_messaging and whatsapp_business_management (a permanent one; the 24-hour one expires)");
    info("4. App settings → Basic → App secret");
    let cfg = t.config()?;
    let old = cfg.gateway.whatsapp.clone();
    let api = old
        .as_ref()
        .map_or(wa::API_URL.to_string(), |w| w.api_url.clone());
    let version = old
        .as_ref()
        .map_or(wa::API_VERSION.to_string(), |w| w.api_version.clone());
    let (phone, token) = loop {
        let mut ask = Text::new("Phone number ID").with_validator(digits(
            "digits, from API Setup → From → Phone number ID (not the phone number)",
        ));
        if let Some(w) = &old {
            ask = ask.with_default(&w.phone_number_id);
        }
        let phone = ask.prompt()?.trim().to_string();
        let token =
            super::ask_secret("Access token (EAA…)", false, super::no_shape)?.unwrap_or_default();
        match interruptible(wa::probe(&api, &version, &phone, &token)).await? {
            Ok(p) => {
                ok(format!("connected to {} {}", p.number, p.name));
                break (phone, token);
            }
            Err(e) => {
                warn(e);
                if Confirm::new("Try again?").with_default(true).prompt()? {
                    continue;
                }
                bail!("WhatsApp wasn't set up");
            }
        }
    };
    let app_secret = super::ask_secret("App secret", false, super::no_shape)?.unwrap_or_default();
    let names = old.as_ref().map_or(
        (
            "WHATSAPP_TOKEN".to_string(),
            "WHATSAPP_APP_SECRET".to_string(),
            "WHATSAPP_VERIFY_TOKEN".to_string(),
        ),
        |w| {
            (
                w.token_env.clone(),
                w.app_secret_env.clone(),
                w.verify_token_env.clone(),
            )
        },
    );
    // The verify token only proves to Meta that the callback URL is ours:
    // made here, kept across a re-run.
    let verify = crate::config_follow::secret_value(&names.2).unwrap_or_else(|| {
        ferrule_gateway::channels::hmac::hex(&ferrule_connections::seal::random::<12>())
    });
    let relay_there = cfg.connections.relay_url.is_some()
        && crate::config_follow::secret_value(ferrule_connections::relay::RELAY_KEY_ENV).is_some();
    let through = if relay_there {
        "Through my relay (nothing to run on this machine)"
    } else {
        "Through a relay (`ferrule connections relay deploy` first)"
    };
    let listen = "Listen on 127.0.0.1 behind my own tunnel (cloudflared, ngrok…)";
    let inbound = match Select::new(
        "Where should Meta's webhooks come in?",
        vec![through, listen],
    )
    .prompt()?
    {
        l if l == listen => "listen",
        _ if !relay_there => {
            info("Run `ferrule connections relay deploy`, then `ferrule setup` → WhatsApp again.");
            bail!("WhatsApp wasn't set up: no relay yet");
        }
        _ => "relay",
    };
    let template = Text::new("Template for a closed 24-hour window (Enter for none)")
        .with_help_message("an approved utility template with one {{1}}; without one, a reply after a day of silence waits until they write")
        .with_default(old.as_ref().and_then(|w| w.template.as_deref()).unwrap_or(""))
        .prompt()?;
    t.set_secret(&names.0, &token)?;
    t.set_secret(&names.1, &app_secret)?;
    t.set_secret(&names.2, &verify)?;
    let tbl = table(t.root(), &["gateway", "whatsapp"])?;
    put(tbl, "phone_number_id", phone.as_str());
    put(tbl, "token_env", names.0.as_str());
    put(tbl, "app_secret_env", names.1.as_str());
    put(tbl, "verify_token_env", names.2.as_str());
    put(tbl, "inbound", inbound);
    match template.trim() {
        "" => {
            tbl.remove("template");
        }
        name => put(tbl, "template", name),
    }
    t.save()?;
    ok("saved to [gateway.whatsapp]; the token and secrets to the secrets file");
    webhook(t).await?;
    if Confirm::new("Saved the webhook in Meta and subscribed to messages? Pair your number now")
        .with_default(true)
        .prompt()?
    {
        allow(t).await?;
    } else {
        info("Later: `ferrule setup` → WhatsApp → Allow another number.");
    }
    Ok(())
}

/// What goes into Meta's Webhooks form. With the relay, the mailbox is
/// configured first, so Meta's check is answered.
async fn webhook(t: &mut Target) -> Result<()> {
    let cfg = t.config()?;
    let Some(w) = cfg.gateway.whatsapp.clone() else {
        return Ok(());
    };
    let verify = crate::config_follow::secret_value(&w.verify_token_env).unwrap_or_default();
    let app_secret = crate::config_follow::secret_value(&w.app_secret_env).unwrap_or_default();
    let url = match w.inbound {
        WhatsAppInbound::Listen => {
            info(format!(
                "Point a tunnel at http://127.0.0.1:{} (e.g. `cloudflared tunnel --url http://127.0.0.1:{}`).",
                w.listen_port, w.listen_port
            ));
            "your tunnel's https address".to_string()
        }
        WhatsAppInbound::Relay => {
            let Some((relay, key)) = cw::relay(&cfg, &w) else {
                bail!("no relay is deployed: `ferrule connections relay deploy`");
            };
            interruptible(wa::configure_mailbox(&relay, &key, &verify, &app_secret))
                .await?
                .map_err(anyhow::Error::msg)?
        }
    };
    info("In Meta: WhatsApp → Configuration → Webhook → Edit");
    info(format!("  Callback URL:  {url}"));
    info(format!("  Verify token:  {verify}"));
    info("then Verify and save, and under Webhook fields subscribe to `messages`.");
    Ok(())
}

async fn allow(t: &mut Target) -> Result<()> {
    let cfg = t.config()?;
    let Some(w) = cfg.gateway.whatsapp.clone() else {
        return Ok(());
    };
    let code = pairing_code();
    let paired = match cw::config(&cfg, &w, Some(&std::env::temp_dir())) {
        Ok(wc) => {
            info(format!(
                "Now send this code to the business number from the WhatsApp that should use it: {code}"
            ));
            let wc = WhatsAppConfig {
                // Setup keeps nothing: no files, no window state.
                inbox: None,
                state_dir: None,
                ..wc
            };
            if let Inbound::Listen { port } = wc.inbound {
                info(format!(
                    "(Your tunnel must be pointing at 127.0.0.1:{port}.)"
                ));
            }
            let ch = Arc::new(WhatsAppChannel::new(wc).with_pairing(&code));
            wait_for(ch.clone(), move || ch.paired()).await?
        }
        Err(e) => {
            warn(format!("{e:#}"));
            None
        }
    };
    let mut users = w.allowed_users.clone();
    if let Some((id, name)) = paired {
        if !users.contains(&id) {
            users.push(id.clone());
            save_users(t, &users)?;
        }
        ok(format!("allowed {name} (+{id})"));
        return Ok(());
    }
    let id = Text::new("Your WhatsApp number to allow, with the country code (Enter to skip)")
        .with_validator(|v: &str| -> Result<Validation, CustomUserError> {
            let v = v.trim().trim_start_matches('+').replace([' ', '-'], "");
            Ok(
                if v.is_empty() || (v.len() >= 8 && v.bytes().all(|b| b.is_ascii_digit())) {
                    Validation::Valid
                } else {
                    Validation::Invalid("digits with the country code, like 972501234567".into())
                },
            )
        })
        .prompt()?;
    let id = id.trim().trim_start_matches('+').replace([' ', '-'], "");
    if id.is_empty() {
        info("Later: `ferrule setup` → WhatsApp → Allow another number.");
    } else {
        if !users.contains(&id) {
            users.push(id.clone());
            save_users(t, &users)?;
        }
        ok(format!("allowed +{id}"));
    }
    Ok(())
}

fn save_users(t: &mut Target, ids: &[String]) -> Result<()> {
    let ids = toml_edit::Array::from_iter(ids.iter().map(String::as_str));
    put(
        table(t.root(), &["gateway", "whatsapp"])?,
        "allowed_users",
        ids,
    );
    t.save()
}
