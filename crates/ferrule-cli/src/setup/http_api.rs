//! M39 §8: `ferrule setup` → HTTP API. Turns `[gateway.http]` on, makes the
//! first key (printed once, with a curl to try it), and optionally opens
//! it to the internet through a quick tunnel (cloudflared fetched here, so
//! the gateway never downloads on start).

use super::{info, interruptible, ok, plural, put, table, warn, Target};
use crate::channels::http::{self as api, Listening};
use crate::config;
use anyhow::Result;
use ferrule_gateway::channels::http::clients;
use inquire::{Confirm, Select, Text};

pub(super) fn summary(cfg: &config::Config) -> String {
    let Some(h) = &cfg.gateway.http else {
        return "off".into();
    };
    let keys = match api::dir().map(|d| clients::load(&d)) {
        Ok(Ok(c)) => plural(c.len(), "key", "keys"),
        _ => "keys unreadable".into(),
    };
    let public = if h.public.is_some() { " · public" } else { "" };
    format!("on · port {} · {keys}{public}", h.port)
}

pub(super) async fn step(t: &mut Target, guided: bool) -> Result<()> {
    let cfg = t.config()?;
    let Some(h) = cfg.gateway.http.clone().filter(|_| !guided) else {
        let ask = Confirm::new(
            "Turn on the HTTP API, so programs (n8n, Zapier, scripts) can message the agent?",
        )
        .with_default(false)
        .prompt()?;
        return if ask { turn_on(t).await } else { Ok(()) };
    };
    match interruptible(api::listening(h.port)).await? {
        Listening::Ours => ok(format!("answering on 127.0.0.1:{}", h.port)),
        Listening::Free => info(format!(
            "127.0.0.1:{} opens when the gateway starts",
            h.port
        )),
        Listening::Other(why) => warn(format!("something else is on 127.0.0.1:{} ({why})", h.port)),
    }
    let dir = api::dir()?;
    let list = clients::load(&dir).unwrap_or_default();
    let mut actions = vec!["Create a key"];
    if !list.is_empty() {
        actions.push("Revoke a key");
    }
    actions.push(if h.public.is_some() {
        "Keep it on this machine only"
    } else {
        "Make it reachable from the internet (a quick tunnel)"
    });
    actions.extend(["Change the port", "Turn the HTTP API off"]);
    match Select::new("HTTP API:", actions).prompt()? {
        "Create a key" => new_key(h.port)?,
        "Revoke a key" => {
            let names: Vec<String> = list.iter().map(|c| c.name.clone()).collect();
            let name = Select::new("Revoke which? Its next request gets 401", names).prompt()?;
            match clients::revoke(&dir, &name) {
                Ok(_) => ok(format!("`{name}` revoked")),
                Err(e) => warn(e),
            }
        }
        "Keep it on this machine only" => {
            table(t.root(), &["gateway", "http"])?.remove("public");
            t.save()?;
            ok("127.0.0.1 only; restart the gateway");
        }
        "Make it reachable from the internet (a quick tunnel)" => public(t).await?,
        "Change the port" => {
            let port = ask_port(h.port)?;
            put(
                table(t.root(), &["gateway", "http"])?,
                "port",
                i64::from(port),
            );
            t.save()?;
            ok(format!("port {port}; restart the gateway"));
        }
        _ => {
            if Confirm::new("Turn the HTTP API off? The keys stay, for when it's back")
                .with_default(false)
                .prompt()?
            {
                table(t.root(), &["gateway"])?.remove("http");
                t.save()?;
                ok("the HTTP API is off");
            }
        }
    }
    Ok(())
}

fn ask_port(default: u16) -> Result<u16> {
    loop {
        let typed = Text::new("The port, on 127.0.0.1")
            .with_default(&default.to_string())
            .prompt()?;
        match typed.trim().parse::<u16>() {
            Ok(p) if p > 0 => return Ok(p),
            _ => warn("a number from 1 to 65535"),
        }
    }
}

async fn turn_on(t: &mut Target) -> Result<()> {
    info("Programs POST to http://127.0.0.1:<port>/v1/messages with a key of their own;");
    info("the guide: https://github.com/maximarhipkin/ferrule/blob/main/docs/channels.md#http");
    let mut port = ask_port(8788)?;
    loop {
        match interruptible(api::listening(port)).await? {
            Listening::Free | Listening::Ours => break,
            Listening::Other(why) => {
                warn(format!("something else is on 127.0.0.1:{port} ({why})"));
                port = ask_port(port.wrapping_add(1).max(1))?;
            }
        }
    }
    let tbl = table(t.root(), &["gateway", "http"])?;
    put(tbl, "port", i64::from(port));
    t.save()?;
    ok("saved to [gateway.http]; it opens when the gateway (re)starts");
    if Confirm::new("Create a key for your first program now?")
        .with_default(true)
        .prompt()?
    {
        new_key(port)?;
    } else {
        info("Later: `ferrule channels keys add <name>`, or the HTTP API card on the dashboard.");
    }
    if Confirm::new("Make it reachable from the internet too (a Cloudflare quick tunnel)?")
        .with_default(false)
        .prompt()?
    {
        public(t).await?;
    }
    Ok(())
}

fn new_key(port: u16) -> Result<()> {
    let dir = api::dir()?;
    loop {
        let name = Text::new("A name for the program (a-z, 0-9, - and _)")
            .with_help_message("also its chat's name, and what an owner setting names")
            .prompt()?;
        let hook = Text::new("A webhook for task results (Enter for none)")
            .with_help_message("https://…; results no request waits for are POSTed there, signed")
            .prompt()?;
        let hook = Some(hook.trim()).filter(|h| !h.is_empty());
        match clients::add(&dir, name.trim(), hook) {
            Ok(made) => {
                ok(format!("the key for `{}`, shown only now:", name.trim()));
                println!("\n    {}\n", made.key);
                if let Some(s) = made.webhook_secret {
                    ok(format!("the webhook's signing secret, shown only now: {s}"));
                }
                info("Try it:");
                info(format!(
                    "  curl -s http://127.0.0.1:{port}/v1/messages -H 'Authorization: Bearer {}' -H 'Content-Type: application/json' -d '{{\"text\": \"hello\"}}'",
                    made.key
                ));
                return Ok(());
            }
            Err(e) => {
                warn(e);
                if !Confirm::new("Try again?").with_default(true).prompt()? {
                    return Ok(());
                }
            }
        }
    }
}

async fn public(t: &mut Target) -> Result<()> {
    let cfg = t.config()?;
    let cf = crate::dashboard::cloudflared(&cfg);
    if !cf.possible() {
        warn("a quick tunnel needs cloudflared: install it, or unset [connections] cloudflared = \"off\"");
        return Ok(());
    }
    if cf.needs_fetch() {
        info("Fetching cloudflared (Cloudflare's release, into the data dir)…");
        if let Err(e) = interruptible(cf.path()).await? {
            warn(format!("{e:#}"));
            return Ok(());
        }
    }
    warn("Anyone with the tunnel's URL reaches the API; a key is still needed for everything.");
    put(table(t.root(), &["gateway", "http"])?, "public", "tunnel");
    t.save()?;
    ok("public through a quick tunnel: its URL shows in /status once the gateway restarts");
    Ok(())
}
