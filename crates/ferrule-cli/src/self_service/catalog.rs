//! M63.4: one step from the catalog. `mcp_add` with `catalog` (and the
//! Extensions page's "Add") takes everything from the entry in
//! `ferrule-connections`' `catalog.toml`: where the server is or which
//! pinned package runs it, its arguments, which tools are writes, and the
//! key it needs with the hosts that key may go to. Nothing the model says
//! changes any of it. A missing key is asked for with `secret_request`
//! (the owner pastes it on the Safe, bound to the catalog's hosts), and the
//! server is added the moment it's saved. A sign-in entry sends the owner
//! its Sign in button; a bundled skill is turned on.
//!
//! What it adds is marked `catalog` in `[[mcp.servers]]`, so the trust gate
//! treats it as a connected service: its reads are outside content, its
//! changes ask the owner. A stdio server runs in the sandbox with its state
//! dir under `<data>/mcp-private`, hidden from commands and the file tools.

use super::{keys, Admin};
use crate::dashboard::api::{self, Answer};
use crate::dashboard::Ctx;
use ferrule_connections::{AuthKind, Catalog, Kind, SecretSpec, Service};
use ferrule_trust::ChatRef;
use serde_json::{json, Value};
use std::path::PathBuf;

/// The process's catalog: the built-in entries and the owner's own.
fn catalog(ctx: &Ctx) -> Catalog {
    ctx.connections
        .as_ref()
        .map(|c| c.catalog().clone())
        .unwrap_or_else(Catalog::built_in)
}

pub(crate) fn entry(ctx: &Ctx, id: &str) -> Result<Service, String> {
    let cat = catalog(ctx);
    cat.get(id).cloned().ok_or_else(|| {
        format!(
            "`{id}` isn't in the catalog. It has: {}",
            cat.services()
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

fn config(ctx: &Ctx) -> Result<(PathBuf, crate::config::Config), String> {
    let path = ctx
        .config_path
        .clone()
        .ok_or("the config file isn't available in this process")?;
    let cfg = crate::setup::Target::load(path.clone())
        .and_then(|t| t.config())
        .map_err(|e| format!("{e:#}"))?;
    Ok((path, cfg))
}

/// Where the entry's key stands.
enum Key {
    /// Saved and bound to the catalog's hosts (or none needed).
    Ready,
    /// Not saved yet: `secret_request` asks for it.
    Missing,
}

fn key_state(cfg: &crate::config::Config, sec: &SecretSpec) -> Result<Key, String> {
    let saved = crate::secrets::path()
        .and_then(|p| crate::secrets::read(&p))
        .unwrap_or_default()
        .iter()
        .any(|(n, v)| *n == sec.name && !v.is_empty());
    let value = saved || crate::secrets::non_empty(&sec.name).is_some();
    match cfg.secrets.get(&sec.name) {
        Some(spec) => {
            let bound = ferrule_proxy::SecretRule::from(spec).hosts;
            if let Some(h) = sec
                .hosts
                .iter()
                .find(|h| !bound.iter().any(|b| b.eq_ignore_ascii_case(h)))
            {
                return Err(format!(
                    "{} is saved already but goes to {}, not {h} where this server is. Change its \
                     sites on the dashboard's Safe, then add it again.",
                    sec.name,
                    bound.join(", ")
                ));
            }
            if value {
                Ok(Key::Ready)
            } else {
                Err(format!(
                    "{} is in the config without a value: paste it with the Safe's Rotate, then \
                     add it again.",
                    sec.name
                ))
            }
        }
        None if value => Err(format!(
            "{} is kept for something else here, so it isn't reused for this server.",
            sec.name
        )),
        None => Ok(Key::Missing),
    }
}

/// Refused before anything is asked: a name in use, or an entry that isn't
/// added this way.
fn refusal(ctx: &Ctx, cfg: &crate::config::Config, s: &Service) -> Option<String> {
    if s.kind == Kind::Skill {
        return None;
    }
    if cfg.mcp.servers.iter().any(|m| m.name == s.name) {
        return Some(format!(
            "{} is added already (mcp_off turns it off, mcp_remove takes it out).",
            s.name
        ));
    }
    if ctx.connections.as_ref().is_some_and(|c| {
        c.store()
            .load()
            .unwrap_or_default()
            .iter()
            .any(|r| r.name == s.name)
    }) {
        return Some(format!(
            "{} is connected already, on the Connections page.",
            s.name
        ));
    }
    None
}

/// What the owner's card says (and what the page's confirm asks).
pub(crate) fn card(ctx: &Ctx, id: &str, write: bool) -> Result<String, String> {
    let s = entry(ctx, id)?;
    let (_, cfg) = config(ctx)?;
    if let Some(why) = refusal(ctx, &cfg, &s) {
        return Err(why);
    }
    let covers = s.covers.as_deref().unwrap_or("").trim();
    let mode = if write {
        "with writes (each change still asks you)"
    } else {
        "read-only"
    };
    Ok(match (s.kind, s.auth) {
        (Kind::Skill, _) => format!(
            "Turn on the bundled skill {} ({covers}).",
            s.skill.as_deref().unwrap_or(&s.name)
        ),
        (Kind::Remote, AuthKind::Oauth) => format!(
            "Connect {} from the catalog, {mode}: you get its Sign in button next. {covers}",
            s.title()
        ),
        (Kind::Stdio, _) => format!(
            "Add {} from the catalog, {mode}: it runs {} in the sandbox, its sign-in kept in a \
             hidden folder, and its tools are scanned first. {covers}{}",
            s.title(),
            s.package
                .as_deref()
                .unwrap_or("")
                .trim_start_matches("npm:"),
            key_line(&cfg, s.secret.as_ref())
        ),
        (Kind::Remote, _) => format!(
            "Add {} ({}) from the catalog, {mode}; its tools are scanned first. {covers}{}",
            s.title(),
            s.url(write),
            key_line(&cfg, s.secret.as_ref())
        ),
    })
}

fn key_line(cfg: &crate::config::Config, sec: Option<&SecretSpec>) -> String {
    let Some(sec) = sec else {
        return String::new();
    };
    let hosts = sec.hosts.join(", ");
    match key_state(cfg, sec) {
        Ok(Key::Ready) => format!(" It uses the key {}, sent only to {hosts}.", sec.name),
        _ => format!(
            " It needs a key, {}, sent only to {hosts}: you get a link to paste it, and it's \
             added once it's saved.",
            sec.name
        ),
    }
}

/// `mcp_add` with `catalog`, after the owner's yes (or at once in owner
/// mode).
pub(super) async fn add(
    admin: &Admin,
    ctx: &Ctx,
    here: &ChatRef,
    session: &str,
    id: &str,
    write: bool,
    by: &str,
) -> Result<String, String> {
    let s = entry(ctx, id)?;
    let (path, cfg) = config(ctx)?;
    if let Some(why) = refusal(ctx, &cfg, &s) {
        return Err(why);
    }
    match (s.kind, s.auth) {
        (Kind::Skill, _) => skill_on(ctx, &s, by),
        (Kind::Remote, AuthKind::Oauth) => {
            let conns = ctx
                .connections
                .as_ref()
                .ok_or("connections aren't on in this process")?;
            let chat = ferrule_connections::Chat {
                channel: here.channel.clone(),
                id: here.chat.clone(),
            };
            conns
                .request(&chat, &s.name, write, "added from the catalog")
                .await
                .map_err(|e| format!("{e:#}"))
        }
        _ => {
            let place = Place {
                path,
                workspace: ctx.workspace.clone().unwrap_or_else(|| PathBuf::from(".")),
                service: s.clone(),
                write,
            };
            let Some(sec) = &s.secret else {
                return install(place).await;
            };
            match key_state(&cfg, sec)? {
                Key::Ready => install(place).await,
                Key::Missing => {
                    let why = format!(
                        "the {} server from ferrule's catalog needs it ({})",
                        s.title(),
                        s.registry.as_deref().unwrap_or(&s.name)
                    );
                    keys::request(admin, ctx, here, session, &sec.name, &sec.hosts, &why).await?;
                    later(&sec.name, &sec.hosts, place);
                    Ok(format!(
                        "{} needs the key {}: the owner was sent a link to paste it, sent only to \
                         {}. The server is added as soon as it's saved, and you're woken with \
                         the result.",
                        s.title(),
                        sec.name,
                        sec.hosts.join(", ")
                    ))
                }
            }
        }
    }
}

/// The add, once `name` is saved, if it was saved for the catalog's
/// hosts.
fn later(name: &str, wanted: &[String], place: Place) {
    let wanted = wanted.to_vec();
    let key = name.to_string();
    keys::when_saved(
        name,
        Box::new(move |hosts: Vec<String>| {
            Box::pin(async move {
                if let Some(h) = wanted
                    .iter()
                    .find(|w| !hosts.iter().any(|h| h.eq_ignore_ascii_case(w)))
                {
                    return format!(
                        "{key} was saved for {}, not {h} where {} is, so nothing was added; \
                         change its sites on the Safe and add it again.",
                        hosts.join(", "),
                        place.service.title()
                    );
                }
                match install(place).await {
                    Ok(said) => said,
                    Err(why) => format!("The server wasn't added: {why}"),
                }
            })
        }),
    );
}

fn skill_on(ctx: &Ctx, s: &Service, by: &str) -> Result<String, String> {
    let name = s.skill.clone().unwrap_or_else(|| s.name.clone());
    let (_, cfg) = config(ctx)?;
    if !cfg.skills.enabled {
        return Err("skills are off here ([skills] enabled = false)".into());
    }
    if !cfg.skills.bundled {
        return Err(format!(
            "{name} ships with ferrule, but the bundled skills are off ([skills] bundled = false)"
        ));
    }
    if !cfg.skills.disabled.contains(&name) {
        return Ok(format!(
            "{name} is a bundled skill and it's on already: activate it when a task needs it."
        ));
    }
    let path = ctx
        .config_path
        .clone()
        .ok_or("the config file isn't available in this process")?;
    crate::settings_admin::Settings::new(
        path,
        ctx.data.clone(),
        ctx.hub.clone(),
        ctx.workspace.clone(),
    )
    .skill_set_disabled(&name, false, by)
    .map(|d| d.said)
    .map_err(|e| format!("{e:#}"))
}

/// What an add needs once the turn that asked is over.
struct Place {
    path: PathBuf,
    workspace: PathBuf,
    service: Service,
    write: bool,
}

/// `ferrule mcp add` with the entry's settings: started in its sandbox,
/// its tools listed and scanned, then written to the config, where the
/// running gateway picks it up.
async fn install(place: Place) -> Result<String, String> {
    let s = &place.service;
    let mut args = crate::mcp_add::AddArgs {
        name: s.name.clone(),
        catalog: Some(s.name.clone()),
        write_tools: s.writes.clone(),
        no_sandbox: false,
        yes: true,
        no_doctor: true,
        workspace: place.workspace.clone(),
        ..Default::default()
    };
    match s.kind {
        Kind::Stdio => {
            let pkg = s
                .package
                .as_deref()
                .and_then(|p| p.strip_prefix("npm:"))
                .ok_or("the catalog entry has no npm package")?;
            args.command = vec!["npx".into(), "-y".into(), pkg.to_string()];
            args.command.extend(s.args.iter().cloned());
            if !place.write {
                args.command.extend(s.read_only_args.iter().cloned());
            }
            args.env = s.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
            args.private_state = true;
        }
        _ => {
            args.url = Some(s.url(place.write).to_string());
            if let Some((h, v)) = s.secret_header() {
                args.headers.push(format!("{h}={v}"));
            }
        }
    }
    if let Some(sec) = &s.secret {
        // Its hosts are the config's (checked above); the value is read
        // from where it's kept, never typed here.
        args.secrets.push(sec.name.clone());
    }
    let rt = tokio::runtime::Handle::current();
    let file = place.path.clone();
    let tools =
        tokio::task::spawn_blocking(move || rt.block_on(crate::mcp_add::add_to(file, args)))
            .await
            .map_err(|e| format!("the add stopped: {e}"))?
            .map_err(|e| format!("{e:#}"))?;
    let tools = if tools.is_empty() {
        "it offers no tools yet; ones it adds later are scanned then".to_string()
    } else {
        format!("its tools: {}", tools.join(", "))
    };
    let when = if crate::self_extend::allow_trusted(&place.path) {
        "They load within a few seconds, no restart."
    } else {
        "Running agents load them at their next start."
    };
    let mode = if place.write {
        "with writes"
    } else {
        "read-only"
    };
    Ok(format!(
        "Added {} from the catalog, {mode}; {tools}. What it reads counts as outside content, \
         and what it changes asks the owner first. {when}",
        s.title()
    ))
}

/// The Extensions page's list: each entry, what it needs, whether it's
/// added already.
pub(crate) fn listing(ctx: &Ctx) -> Vec<Value> {
    let Ok((_, cfg)) = config(ctx) else {
        return Vec::new();
    };
    let connected: Vec<String> = ctx
        .connections
        .as_ref()
        .and_then(|c| c.store().load().ok())
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.name)
        .collect();
    catalog(ctx)
        .services()
        .iter()
        // The sign-ins and the key forms of the Connections page stay
        // there; this lists what that page doesn't add.
        .filter(|s| s.kind != Kind::Remote || s.secret.is_some())
        .map(|s| {
            let added = match s.kind {
                Kind::Skill => {
                    let skill = s.skill.as_deref().unwrap_or(&s.name);
                    cfg.skills.enabled
                        && cfg.skills.bundled
                        && !cfg.skills.disabled.iter().any(|d| d == skill)
                }
                _ => {
                    cfg.mcp.servers.iter().any(|m| m.name == s.name) || connected.contains(&s.name)
                }
            };
            json!({
                "id": s.name,
                "title": s.title(),
                "kind": format!("{:?}", s.kind).to_lowercase(),
                "covers": s.covers,
                "docs": s.docs,
                "registry": s.registry,
                "package": s.package,
                "key": s.secret.as_ref().map(|k| json!({ "name": k.name, "hosts": k.hosts })),
                "added": added,
            })
        })
        .collect()
}

/// `POST mcp/catalog {id, write}`: the page's "Add". A missing key isn't
/// asked for in a chat here: the answer names it and its hosts, the page
/// opens the Safe with them filled in, and the server is added once it's
/// saved.
pub(crate) async fn from_page(ctx: &Ctx, body: &Value, by: &str) -> Answer {
    let id = match api::arg(body, "id") {
        Ok(id) => id.to_ascii_lowercase(),
        Err(answer) => return answer,
    };
    let write = body.get("write").and_then(Value::as_bool) == Some(true);
    let question = match card(ctx, &id, write) {
        Ok(q) => q,
        Err(why) => return api::bad(400, why),
    };
    if body.get("confirm").and_then(Value::as_bool) != Some(true) {
        return Some((409, json!({ "confirm": question })));
    }
    let (s, (path, cfg)) = match (entry(ctx, &id), config(ctx)) {
        (Ok(s), Ok(c)) => (s, c),
        (Err(e), _) | (_, Err(e)) => return api::bad(400, e),
    };
    let done = match (s.kind, s.auth) {
        (Kind::Skill, _) => skill_on(ctx, &s, by),
        (Kind::Remote, AuthKind::Oauth) => {
            return api::ok(json!({
                "ok": false,
                "said": format!("{} signs in: its tile is on the Connections page.", s.title()),
            }))
        }
        _ => {
            let place = Place {
                path,
                workspace: ctx.workspace.clone().unwrap_or_else(|| PathBuf::from(".")),
                service: s.clone(),
                write,
            };
            match &s.secret {
                None => install(place).await,
                Some(sec) => match key_state(&cfg, sec) {
                    Err(why) => Err(why),
                    Ok(Key::Ready) => install(place).await,
                    Ok(Key::Missing) => {
                        later(&sec.name, &sec.hosts, place);
                        return api::ok(json!({
                            "ok": false,
                            "needs_key": { "name": sec.name, "hosts": sec.hosts.join(", ") },
                            "said": format!(
                                "{} needs the key {}, sent only to {}: paste it in the Safe, and the server is added as soon as it's saved.",
                                s.title(), sec.name, sec.hosts.join(", ")
                            ),
                        }));
                    }
                },
            }
        }
    };
    match done {
        Ok(said) => api::ok(json!({ "ok": true, "said": ctx.redactor.redact(&said) })),
        Err(why) => api::bad(400, ctx.redactor.redact(&why)),
    }
}
