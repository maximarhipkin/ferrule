//! M52.2: the Safe's inventory, Delete for every kind, and its audit view
//! (docs/m52-dashboard-safe.md, "M52.2"). Every credential ferrule holds,
//! grouped — model providers, channels, connected services, tool keys,
//! system — with where it's stored, where it may go, when it was saved and
//! changed, and when it was last used. Never a value, never a reveal.
//!
//! Where things live, as traced in the code: a provider's key and a
//! channel's tokens are values in `private/secrets.env` under the names
//! the config gives them (`api_key_env`, `*_token_env`; `models_page::
//! provider_save` and `channels::card::save` write them there); a `[secrets]`
//! tool key is a value in the same file plus its hosts in the config;
//! connections and plan sign-ins are sealed (AES-256-GCM) in their own
//! files under `private/`; a name may also come from the environment
//! ferrule was started with, which the page can't change.

use super::api::{arg, bad, config, confirmed, need, ok, setup_place, Answer};
use super::Ctx;
use crate::config::Config;
use crate::secret_meta::Entry;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// `GET secrets`: M50/M52's flat `keys` (what the chat's `secrets` op and
/// the page's add form read), plus `groups`, the whole inventory.
pub fn list(ctx: &Ctx) -> Answer {
    let (status, mut v) = super::tool_keys::list(ctx)?;
    if status != 200 {
        return Some((status, v));
    }
    if let (Some(cfg), Ok(place)) = (config(ctx), setup_place(ctx)) {
        v["groups"] = Value::Array(groups(ctx, &cfg, &place));
    }
    Some((status, v))
}

/// One credential, as the page shows it.
struct Item {
    /// What `secrets/delete` takes: `env:NAME`, `connection:NAME`,
    /// `plan:chatgpt`, `plan:claude`, `sign-ins`.
    id: String,
    name: String,
    what: String,
    stored: String,
    hosts: Vec<String>,
    /// Why the hosts are what they are, when it isn't the proxy's list.
    hosts_note: Option<String>,
    used_by: Vec<String>,
    created: Option<i64>,
    rotated: Option<i64>,
    last_used: Option<i64>,
    last_host: Option<String>,
    /// What "last used" means here, or why there's none.
    last_note: Option<String>,
    rotatable: bool,
    hosts_editable: bool,
    /// `None`: Delete is offered; `Some(why)`: it isn't, and why.
    no_delete: Option<String>,
}

impl Item {
    fn new(id: String, name: impl Into<String>, what: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            what: what.into(),
            stored: String::new(),
            hosts: Vec::new(),
            hosts_note: None,
            used_by: Vec::new(),
            created: None,
            rotated: None,
            last_used: None,
            last_host: None,
            last_note: None,
            rotatable: false,
            hosts_editable: false,
            no_delete: None,
        }
    }

    fn json(self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "what": self.what,
            "stored": self.stored,
            "hosts": self.hosts,
            "hosts_note": self.hosts_note,
            "used_by": self.used_by,
            "created": self.created,
            "rotated": self.rotated,
            "last_used": self.last_used,
            "last_host": self.last_host,
            "last_note": self.last_note,
            "rotatable": self.rotatable,
            "hosts_editable": self.hosts_editable,
            "deletable": self.no_delete.is_none(),
            "delete_note": self.no_delete,
        })
    }
}

/// Where a named value comes from, in words, and whether the page holds
/// it (the secrets file) or the machine does (the environment).
fn stored(name: &str, in_file: bool) -> (String, bool) {
    if in_file {
        return ("the secrets file (private/secrets.env)".into(), true);
    }
    match std::env::var_os(name).is_some_and(|v| !v.is_empty()) {
        true => (
            "the environment ferrule was started with (a service file or a shell profile)".into(),
            false,
        ),
        false => ("nowhere: it isn't set".into(), false),
    }
}

/// A named value's item: storage, dates and last use from the Safe's
/// notes, rotate and delete when the page holds it.
fn env_item(
    name: &str,
    what: String,
    in_file: bool,
    meta: &BTreeMap<String, Entry>,
    used_by: Vec<String>,
) -> Item {
    let mut it = Item::new(format!("env:{name}"), name, what);
    let (words, ours) = stored(name, in_file);
    it.stored = words;
    it.used_by = used_by;
    if let Some(m) = meta.get(name) {
        it.created = m.created;
        it.rotated = m.rotated;
        it.last_used = m.last_used;
        it.last_host = m.last_host.clone();
    }
    it.rotatable = ours;
    if !ours {
        it.no_delete = Some(if std::env::var_os(name).is_some() {
            "It's set in the environment ferrule was started with; remove it there.".into()
        } else {
            "There's nothing to delete: it isn't set.".into()
        });
    }
    it
}

fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .host_str()
        .map(|h| h.trim_start_matches('[').trim_end_matches(']').to_string())
}

fn groups(ctx: &Ctx, cfg: &Config, place: &crate::connections_setup::Place) -> Vec<Value> {
    let dir = place
        .secrets
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let meta = crate::secret_meta::read(&dir);
    let kept: BTreeSet<String> = crate::secrets::read(&place.secrets)
        .unwrap_or_default()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    let text = std::fs::read_to_string(&place.config).unwrap_or_default();
    let mut claimed: BTreeSet<String> = BTreeSet::new();
    let last_calls = ctx
        .data
        .as_ref()
        .map(|d| last_calls(&d.join("ledger.jsonl")))
        .unwrap_or_default();

    // Model providers: each keyed provider's key, and the plan sign-ins.
    let mut brain = Vec::new();
    let mut ids: Vec<&String> = cfg.providers.keys().collect();
    ids.sort();
    for id in ids {
        let p = &cfg.providers[id];
        let Some(var) = p.key_var() else { continue };
        if !claimed.insert(var.to_string()) {
            continue;
        }
        let mut it = env_item(
            var,
            format!("the {id} brain's API key"),
            kept.contains(var),
            &meta,
            super::tool_keys::used_by(&text, var),
        );
        it.hosts = host_of(&p.endpoint()).into_iter().collect();
        it.hosts_note =
            Some("ferrule sends it itself to the provider's address; commands never get it".into());
        // Model calls carry the key from ferrule's own process, not through
        // the proxy: the ledger's last call is its last use. The proxy's
        // (media, search) counts too when it's later.
        if let Some(t) = last_calls.get(id.as_str()) {
            if it.last_used.is_none_or(|u| *t > u) {
                it.last_used = Some(*t);
                it.last_host = it.hosts.first().cloned();
            }
        }
        it.last_note = Some("its last model call (the ledger) or proxied request".into());
        brain.push(it);
    }
    let private = &dir;
    if let Ok(Some(m)) = ferrule_plans::chatgpt::store::Store::new(private).meta() {
        let mut it = Item::new("plan:chatgpt".into(), "ChatGPT sign-in", "the ChatGPT plan");
        it.stored = "sealed (AES-256-GCM) in private/plans/chatgpt.json".into();
        it.created = Some(m.signed_in_at as i64);
        it.used_by = vec![match m.email {
            Some(e) => format!("the ChatGPT plan, as {e}"),
            None => "the ChatGPT plan".into(),
        }];
        it.hosts_note = Some("ferrule's own client sends it to OpenAI".into());
        it.last_note = Some("not tracked; Usage shows the plan's windows".into());
        brain.push(it);
    }
    if let Ok(Some(m)) = ferrule_plans::claude::token::TokenStore::new(private).meta() {
        let mut it = Item::new(
            "plan:claude".into(),
            "Claude setup-token",
            "the Claude plan, for Claude Code",
        );
        it.stored = "sealed (AES-256-GCM) in private/plans/claude-code.json".into();
        it.created = Some(m.pasted_at as i64);
        it.used_by = vec!["the Claude plan".into()];
        it.hosts_note = Some("Claude Code sends it to Anthropic".into());
        it.last_note = Some("not tracked".into());
        brain.push(it);
    }

    // Channels: the tokens each configured channel names.
    let mut channels = Vec::new();
    for name in crate::channels::configured_names(cfg) {
        let title = crate::channels::CHANNELS
            .iter()
            .find(|c| c.name == name)
            .map_or(name, |c| c.title);
        for var in crate::channels::secret_envs_of(cfg, name) {
            if !claimed.insert(var.clone()) {
                continue;
            }
            let mut it = env_item(
                &var,
                format!("{title}'s token"),
                kept.contains(&var),
                &meta,
                vec![title.to_string()],
            );
            it.hosts = match name {
                "telegram" => vec!["api.telegram.org".into()],
                "discord" => vec!["discord.com".into(), "gateway.discord.gg".into()],
                "slack" => vec!["slack.com".into()],
                "whatsapp" => vec!["graph.facebook.com".into()],
                _ => vec![],
            };
            it.hosts_note = Some(format!(
                "ferrule's {title} client sends it itself{}",
                if it.hosts.is_empty() {
                    ", to the server set on its card"
                } else {
                    ""
                }
            ));
            it.last_note = Some("in use whenever the channel runs; not tracked".into());
            channels.push(it);
        }
    }

    // Connected services: sealed records, deleted by disconnecting.
    let mut services = Vec::new();
    if let Some(c) = &ctx.connections {
        if let Ok(snap) = c.snapshot() {
            for s in snap.connections {
                let mut it = Item::new(
                    format!("connection:{}", s.name),
                    s.title.clone(),
                    format!(
                        "{}'s {}",
                        s.title,
                        if s.hosts.is_empty() { "sign-in" } else { "key" }
                    ),
                );
                it.stored = "sealed (AES-256-GCM) in private/connections/connections.json".into();
                it.hosts = s.hosts.clone();
                it.hosts_note = s
                    .hosts
                    .is_empty()
                    .then(|| "its tokens go to the service it signed in to".into());
                it.used_by = vec![format!("the {} tools", s.title)];
                it.created = Some(s.connected_at as i64);
                it.rotated = s.refreshed_at.map(|t| t as i64);
                it.last_note = Some("not tracked".into());
                services.push(it);
            }
        }
    }

    // Tool keys: `[secrets]`.
    let mut tools = Vec::new();
    for (name, spec) in &cfg.secrets {
        if !claimed.insert(name.clone()) {
            continue;
        }
        let mut it = env_item(
            name,
            "a key for commands, swapped in by the proxy".into(),
            kept.contains(name),
            &meta,
            super::tool_keys::used_by(&text, name),
        );
        it.hosts = ferrule_proxy::SecretRule::from(spec).hosts;
        it.hosts_editable = kept.contains(name) && !super::tool_keys::named_in(&text, name);
        it.last_note = Some("the last request the proxy swapped it into".into());
        tools.push(it);
    }

    // System: what ferrule makes for itself, and kept names no table claims.
    let mut system = Vec::new();
    for name in kept.iter().filter(|n| !claimed.contains(*n)) {
        let tables = naming_tables(&text, name);
        let what = if name == ferrule_connections::relay::RELAY_KEY_ENV {
            "the relay's key (sign-ins through your Cloudflare relay)".to_string()
        } else if tables.is_empty() {
            "kept, but nothing in the config names it".to_string()
        } else {
            format!("named in [{}]", tables.join("], ["))
        };
        let used_by = if tables.is_empty() {
            vec!["nothing in the config".into()]
        } else {
            tables.iter().map(|t| format!("[{t}]")).collect()
        };
        let mut it = env_item(name, what, true, &meta, used_by);
        it.last_note = Some("the last request the proxy swapped it into, if any".into());
        system.push(it);
    }
    {
        let mut it = Item::new(
            "sealing-key".into(),
            "Sealing key",
            "seals connections and plan sign-ins at rest",
        );
        let from_env = crate::secrets::var(ferrule_connections::seal::KEY_ENV).is_some();
        it.stored = if from_env {
            format!("${}", ferrule_connections::seal::KEY_ENV)
        } else {
            "private/connections.key".into()
        };
        it.hosts_note = Some("never leaves this machine".into());
        it.used_by = vec!["every connection and plan sign-in".into()];
        it.no_delete = Some(
            "Deleting it would make every sealed connection and sign-in unreadable; delete those instead."
                .into(),
        );
        system.push(it);
    }
    {
        let mut it = Item::new(
            "proxy-ca".into(),
            "Credential proxy CA",
            "the proxy's certificate authority and placeholder seed",
        );
        it.stored = "proxy/keys/ in the data directory".into();
        it.hosts_note = Some("never leaves this machine".into());
        it.used_by = vec!["every command's HTTPS through the proxy".into()];
        it.no_delete = Some(
            "Ferrule makes it at start and every running command trusts it; it holds no key of yours."
                .into(),
        );
        system.push(it);
    }
    if let Some(data) = &ctx.data {
        let links = super::auth::Links::at(data.join("private/dashboard/links.json"));
        let sessions = super::auth::Sessions::beside(
            &links,
            std::time::Duration::from_secs(cfg.dashboard.idle_minutes.max(1) * 60),
            std::time::Duration::from_secs(cfg.dashboard.session_hours.max(1) * 3600),
        );
        let mut it = Item::new(
            "sign-ins".into(),
            "Dashboard sign-ins",
            "one-time login links and this page's sessions",
        );
        it.stored = "hashes in private/dashboard/".into();
        it.hosts = sessions.hosts(links.revoked_ms());
        it.hosts_note = Some("the addresses with a live session".into());
        it.used_by = vec![format!(
            "{} open session(s)",
            sessions.live(links.revoked_ms())
        )];
        system.push(it);
    }

    [
        (
            "brain",
            "Model providers",
            "Keys and sign-ins the brain uses.",
            brain,
        ),
        (
            "channels",
            "Channels",
            "Tokens for the chat apps people reach the bot on.",
            channels,
        ),
        (
            "services",
            "Connected services",
            "Sign-ins and keys for the apps whose tools it uses.",
            services,
        ),
        (
            "tools",
            "Tool keys",
            "Keys for commands: commands see a stand-in; the proxy sends the real one only to its sites.",
            tools,
        ),
        (
            "system",
            "System",
            "What ferrule makes for itself, and kept keys no setting above claims.",
            system,
        ),
    ]
    .into_iter()
    .map(|(id, title, intro, items)| {
        json!({
            "id": id,
            "title": title,
            "intro": intro,
            "items": items.into_iter().map(Item::json).collect::<Vec<_>>(),
        })
    })
    .collect()
}

/// The top-level tables (outside `[secrets]`) that name `name`: as a
/// `*_env` value, in a `secrets` list, or as `${NAME}`.
fn naming_tables(config_text: &str, name: &str) -> Vec<String> {
    let Ok(doc) = config_text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let wanted = format!("${{{name}}}");
    fn walk(v: &toml::Value, key: &str, name: &str, wanted: &str) -> bool {
        match v {
            toml::Value::String(s) => {
                s.contains(wanted)
                    || (key == "secrets" && s == name)
                    || (key.ends_with("_env")
                        && (s == name
                            || name
                                .strip_prefix(s.as_str())
                                .is_some_and(|r| r.starts_with('_'))))
            }
            toml::Value::Array(a) => a.iter().any(|x| walk(x, key, name, wanted)),
            toml::Value::Table(t) => t.iter().any(|(k, x)| walk(x, k, name, wanted)),
            _ => false,
        }
    }
    doc.iter()
        .filter(|(k, _)| k.as_str() != "secrets")
        .filter(|(k, v)| walk(v, k, name, &wanted))
        .map(|(k, _)| k.clone())
        .collect()
}

/// Each provider's newest ledger row in the ledger's last 512 KB (unix
/// seconds), so the page doesn't read a year of rows. Rows of ferrule's
/// own kinds (media, transcription) name no provider id and don't match.
fn last_calls(ledger: &Path) -> BTreeMap<String, i64> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 512 * 1024;
    let mut out = BTreeMap::new();
    let Ok(mut f) = std::fs::File::open(ledger) else {
        return out;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return out;
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return out;
    }
    let text = String::from_utf8_lossy(&buf);
    // A cut first line is skipped.
    for line in text.lines().skip(usize::from(start > 0)) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let (Some(p), Some(t)) = (
            v["provider"].as_str(),
            v["timestamp"]
                .as_str()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()),
        ) else {
            continue;
        };
        let t = t.timestamp();
        let e = out.entry(p.to_string()).or_insert(t);
        *e = (*e).max(t);
    }
    out
}

/// `POST secrets/delete {id}` (M52.2): every kind. A kept value goes from
/// the secrets file, this process's environment and the proxy, and a
/// `[secrets]` entry nothing else names goes with it; a connection is
/// disconnected (its grant revoked where the service allows); a plan
/// sign-in is signed out; the dashboard's sign-ins all end. One confirm
/// card naming what stops working; audited by name.
pub async fn delete(ctx: &Ctx, body: &Value) -> Answer {
    if let Some(why) = super::tool_keys::locked() {
        return bad(403, why);
    }
    if ctx.live.is_none() {
        return bad(
            409,
            "Open this page from your running bot (send it /dashboard): only the bot holds the keys it uses.",
        );
    }
    let id = need!(arg(body, "id")).to_string();
    let Some(cfg) = config(ctx) else {
        return bad(503, "the config file isn't available in this process");
    };
    let place = need!(setup_place(ctx));
    if let Some(name) = id.strip_prefix("env:") {
        return delete_value(ctx, &cfg, &place, name, body);
    }
    if let Some(name) = id.strip_prefix("connection:") {
        let Some(c) = &ctx.connections else {
            return bad(503, "connections aren't available in this process");
        };
        need!(confirmed(
            body,
            format!("Delete {name}'s sign-in? Its tools go away and its grant is revoked where the service allows; connecting again takes a new sign-in.")
        ));
        return match c.disconnect(name).await {
            Ok(said) => {
                audit(
                    ctx,
                    "dashboard.secret.delete",
                    json!({ "name": name, "kind": "connection" }),
                );
                ok(json!({ "ok": true, "said": said }))
            }
            Err(e) => bad(400, format!("{e:#}")),
        };
    }
    let private = place
        .secrets
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    match id.as_str() {
        "plan:chatgpt" => {
            need!(confirmed(
                body,
                "Sign out of the ChatGPT plan? A brain on it stops until you sign in again from Models.".into()
            ));
            let store = ferrule_plans::chatgpt::store::Store::new(&private);
            match std::fs::remove_file(store.path()) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return bad(500, format!("the sign-in couldn't be deleted: {e}")),
            }
            audit(
                ctx,
                "dashboard.secret.delete",
                json!({ "name": "chatgpt", "kind": "plan" }),
            );
            ok(
                json!({ "ok": true, "said": "Deleted the ChatGPT sign-in from this machine. The running bot may keep using the plan until it restarts, and OpenAI keeps the session until it expires (`ferrule logout chatgpt` also revokes it)." }),
            )
        }
        "plan:claude" => {
            need!(confirmed(
                body,
                "Delete the Claude setup-token? A brain on the Claude plan stops until a token is pasted again on Models.".into()
            ));
            match ferrule_plans::claude::token::TokenStore::new(&private).delete() {
                Ok(_) => {
                    audit(
                        ctx,
                        "dashboard.secret.delete",
                        json!({ "name": "claude", "kind": "plan" }),
                    );
                    ok(
                        json!({ "ok": true, "said": "Deleted the Claude setup-token. A sign-in Claude Code made in its own config dir is ended with `ferrule logout claude` on the machine." }),
                    )
                }
                Err(e) => bad(500, format!("the token couldn't be deleted: {e:#}")),
            }
        }
        "sign-ins" => {
            let Some(data) = &ctx.data else {
                return bad(503, "the data directory isn't available in this process");
            };
            need!(confirmed(
                body,
                "End every dashboard session and unused login link, this page's too? You sign in again with /dashboard in your chat.".into()
            ));
            let links = super::auth::Links::at(data.join("private/dashboard/links.json"));
            if let Err(e) = links.revoke() {
                return bad(500, format!("the sign-ins couldn't be ended: {e:#}"));
            }
            audit(
                ctx,
                "dashboard.secret.delete",
                json!({ "name": "sign-ins", "kind": "system" }),
            );
            ok(
                json!({ "ok": true, "said": "Every session and login link is over. Send /dashboard in your chat for a new link." }),
            )
        }
        "sealing-key" | "proxy-ca" => bad(
            409,
            "This one isn't deleted from the page: the Safe says why next to it.",
        ),
        _ => bad(404, format!("there's nothing called {id} in the Safe")),
    }
}

fn delete_value(
    ctx: &Ctx,
    cfg: &Config,
    place: &crate::connections_setup::Place,
    name: &str,
    body: &Value,
) -> Answer {
    let stored = crate::secrets::read(&place.secrets).unwrap_or_default();
    if !stored.iter().any(|(n, _)| n == name) {
        return bad(
            409,
            if std::env::var_os(name).is_some() {
                format!("{name} is set in the environment ferrule was started with, not kept by the page; remove it there (the service file or your shell profile).")
            } else {
                format!("no key named {name} is kept here, so there's nothing to delete.")
            },
        );
    }
    let text = std::fs::read_to_string(&place.config).unwrap_or_default();
    let users = super::tool_keys::used_by(&text, name);
    let tool_only = cfg.secrets.contains_key(name) && !super::tool_keys::named_in(&text, name);
    need!(confirmed(
        body,
        format!(
            "Delete {name}? {} stops getting it until a key is saved again. It can't be shown or brought back.",
            capitalized(&users.join(", "))
        )
    ));
    let done = (|| -> anyhow::Result<()> {
        if tool_only {
            let mut t = crate::setup::Target::load(place.config.clone())?;
            crate::setup::table(t.root(), &["secrets"])?.remove(name);
            t.save()?;
        }
        crate::secrets::remove(&place.secrets, name)
    })();
    if let Err(e) = done {
        return bad(500, format!("{name} couldn't be deleted: {e:#}"));
    }
    // This process read it from the file at start (or a rotation put it
    // there): it goes now, and the proxy stops swapping it in.
    ferrule_sandbox::secret_env::forget(name);
    let unbound = crate::shared_broker(cfg)
        .ok()
        .flatten()
        .is_some_and(|b| b.unbind(name));
    audit(
        ctx,
        "dashboard.secret.delete",
        json!({ "name": name, "kind": "kept" }),
    );
    let mut said = format!("Deleted {name}: it's gone from the secrets file and from this process");
    said.push_str(if unbound {
        ", and the proxy no longer sends it."
    } else {
        "."
    });
    if !users.is_empty() && users.iter().any(|u| u != "nothing in the config yet") {
        said.push_str(" A channel or connection that read it at start stops at its next restart.");
    }
    ok(json!({ "ok": true, "said": said }))
}

fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => "Nothing".into(),
    }
}

/// `GET secrets/audit`: the Safe's history — key, config and approval
/// events, newest first, 200 at most, by name only (a key's name, the
/// config sections, an approval's op), never a value or a command.
pub fn audit_view(ctx: &Ctx) -> Answer {
    let Some(hub) = &ctx.hub else {
        return ok(json!({ "events": [], "available": false }));
    };
    let since = chrono::Utc::now() - chrono::Duration::days(90);
    let events = hub.audit().read(Some(since)).unwrap_or_default();
    let mut rows: Vec<Value> = events
        .into_iter()
        .filter(|e| {
            let n = e.event.as_str();
            n.starts_with("dashboard.secret.")
                || n.starts_with("self_service.secret")
                || n.starts_with("config_")
                || n.starts_with("approval_")
                || n.starts_with("channel.")
                || n == "dashboard.sandbox.mode"
        })
        .map(|e| {
            let d = &e.detail;
            let what = if let Some(name) = d.get("name").and_then(Value::as_str) {
                name.to_string()
            } else if let Some(s) = d.get("sections").and_then(Value::as_array) {
                s.iter()
                    .filter_map(Value::as_str)
                    .map(|s| format!("[{s}]"))
                    .collect::<Vec<_>>()
                    .join(" ")
            } else if let Some(op) = d.get("op").and_then(Value::as_str) {
                op.to_string()
            } else if let Some(s) = d.as_str() {
                s.to_string()
            } else {
                String::new()
            };
            json!({
                "at": e.at,
                "event": e.event,
                "what": what,
                "by": d.get("by").and_then(Value::as_str),
            })
        })
        .collect();
    rows.reverse();
    rows.truncate(200);
    ok(json!({ "events": rows, "available": true }))
}

fn audit(ctx: &Ctx, event: &str, detail: Value) {
    if let Some(hub) = &ctx.hub {
        hub.audit()
            .record(chrono::Utc::now(), event, None, None, detail);
    }
}
