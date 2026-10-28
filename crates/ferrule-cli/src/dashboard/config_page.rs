//! M37 §4.4: the config from the page. A form per section and the raw
//! TOML, both saved the same way: checked with the real loader, refused
//! when they change a field that amounts to running a command on the
//! machine, written atomically with the file they replace kept as
//! `<config>.prev`. Secret-looking fields are hidden on the way out and
//! must come back unchanged. The last good copy is put back by
//! `config/restore` (§1.3).
//!
//! M52.4: a save shows its diff of non-secret values first (`config/check`
//! returns it); every write carries the `base_hash` of the file the page
//! loaded, and a file changed since is refused with a 409 rather than
//! silently undone; and the last [`KEEP`] versions are kept in
//! `<data>/config-history/` with their secret fields hidden, each one
//! previewed as a diff and restored through the same guard as a save.

use super::api::{arg, bad, missing, need, ok, Answer};
use super::Ctx;
use crate::config::Config;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item};

/// What a hidden field reads as on the page.
pub const HIDDEN: &str = "(hidden: change it on the Connections or Models page)";

/// Sections the page can't change at all: each runs something, reaches
/// the machine or decides where a secret may go.
const WHOLE: &[&str] = &[
    "sandbox",
    "hooks",
    "extensions",
    "ssh",
    "secrets",
    "egress",
    "browser",
    "plans",
    "plugins",
    // M61: seeing the owner's screen is turned on in the file alone.
    "screen",
];

/// M56.3: fields inside a [`WHOLE`] section the page and the chat may
/// change all the same: a switch that runs nothing new and moves no key.
const OPEN: &[&str] = &["browser.enabled", "plans.claude_code.turn_timeout_minutes"];

/// Whole-number fields the page and the chat keep inside a range: the
/// key, the least and the most.
const BOUNDS: &[(&str, i64, i64)] = &[
    ("plans.claude_code.turn_timeout_minutes", 5, 240),
    ("health.watchdog_after_secs", 0, 86_400),
];

/// A field the page can't change, by its own name: it names a command,
/// a path or an environment, loosens a gate, lets someone new in, or
/// moves where a secret is sent.
fn guarded_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k.contains("command")
        // A gate (`[trust] gates`, `gate_writes`), not the gateway.
        || k.replace("gateway", "").contains("gate")
        || k.contains("hook")
        || k.contains("path")
        || k.contains("allowed")
        || k.contains("owner")
        || k.contains("workspace")
        || k.contains("relay")
        || k.contains("roots")
        || matches!(
            k.as_str(),
            "args"
                | "env"
                | "binary"
                | "program"
                | "cloudflared"
                | "endpoint"
                | "issuer"
                | "sandbox"
                | "signal_cli"
                | "tailscale_users"
        )
        || k.ends_with("_env")
        || k.ends_with("url")
}

/// A field whose value is a secret, not the name of one.
fn secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    if k.ends_with("_env") || k.ends_with("_var") {
        return false;
    }
    k == "key"
        || k.ends_with("_key")
        || k.contains("token")
        || k.contains("secret")
        || k.contains("password")
}

/// Every leaf value, by its dotted path; a table in an array is named by
/// its `name` when it has one, so reordering servers isn't a change.
fn walk(
    item: &mut Item,
    path: &mut Vec<String>,
    f: &mut dyn FnMut(&[String], &mut toml_edit::Value),
) {
    match item {
        Item::Value(v) => walk_value(v, path, f),
        Item::Table(t) => {
            for (k, it) in t.iter_mut() {
                path.push(k.get().to_string());
                walk(it, path, f);
                path.pop();
            }
        }
        Item::ArrayOfTables(a) => {
            for (i, t) in a.iter_mut().enumerate() {
                let seg = t
                    .get("name")
                    .and_then(Item::as_str)
                    .map_or_else(|| i.to_string(), |n| format!("[{n}]"));
                path.push(seg);
                for (k, it) in t.iter_mut() {
                    path.push(k.get().to_string());
                    walk(it, path, f);
                    path.pop();
                }
                path.pop();
            }
        }
        Item::None => {}
    }
}

fn walk_value(
    v: &mut toml_edit::Value,
    path: &mut Vec<String>,
    f: &mut dyn FnMut(&[String], &mut toml_edit::Value),
) {
    match v {
        toml_edit::Value::InlineTable(t) => {
            for (k, v) in t.iter_mut() {
                path.push(k.get().to_string());
                walk_value(v, path, f);
                path.pop();
            }
        }
        toml_edit::Value::Array(a) => {
            for (i, v) in a.iter_mut().enumerate() {
                path.push(i.to_string());
                walk_value(v, path, f);
                path.pop();
            }
        }
        leaf => f(path, leaf),
    }
}

/// The key a leaf belongs to: its last segment that isn't an index.
fn key_of(path: &[String]) -> &str {
    path.iter()
        .rev()
        .find(|s| s.parse::<usize>().is_err())
        .map_or("", String::as_str)
}

fn plain(v: &toml_edit::Value) -> String {
    match v {
        toml_edit::Value::String(s) => format!("{:?}", s.value()),
        other => other.clone().decorated("", "").to_string(),
    }
}

fn leaves(doc: &DocumentMut) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut item = Item::Table(doc.as_table().clone());
    walk(&mut item, &mut Vec::new(), &mut |p, v| {
        out.insert(p.join("."), plain(v));
    });
    out
}

/// A name two tables of one array share (`[[mcp.servers]]`), as a path.
/// [`walk`] keys those tables by name, so the second would hide what the
/// first changes from [`guarded_change`].
fn twice_named(table: &toml_edit::Table, path: &str) -> Option<String> {
    table.iter().find_map(|(k, item)| {
        let here = if path.is_empty() {
            k.to_string()
        } else {
            format!("{path}.{k}")
        };
        match item {
            Item::Table(t) => twice_named(t, &here),
            Item::ArrayOfTables(a) => {
                let mut seen = std::collections::HashSet::new();
                a.iter()
                    .filter_map(|t| t.get("name").and_then(Item::as_str))
                    .find(|n| !seen.insert(*n))
                    .map(|n| format!("{here}.[{n}]"))
                    .or_else(|| a.iter().find_map(|t| twice_named(t, &here)))
            }
            _ => None,
        }
    })
}

/// A channel's own table in `[gateway]` (`[gateway.matrix]`, `[gateway.email]`
/// and the rest: hosts, accounts, ports, who gets in) or the local channel,
/// which the Channels page sets; `[gateway]`'s own switches the page sets.
fn channel_key(segs: &[&str]) -> bool {
    segs[0] == "gateway"
        && (segs.get(1) == Some(&"local")
            || segs.get(2).is_some_and(|s| s.parse::<usize>().is_err()))
}

/// Why `field` can't be changed here, and where it can.
fn refusal(field: &str) -> String {
    let telegram = if field.starts_with("gateway.telegram_allowed_chats") {
        " To let a Telegram group or person in, add the bot to the group or have them write to it: the bot asks you in Telegram."
    } else {
        ""
    };
    format!(
        "`{field}` can't be changed from the chat or the Config page: it runs something on the machine, lets someone in, or decides where a secret goes. Channels, hooks and keys have their own pages on the dashboard.{telegram}"
    )
}

/// The first field the page may not change that `new` changes.
pub(crate) fn guarded_change(old: &DocumentMut, new: &DocumentMut) -> Option<String> {
    if let Some(twice) = twice_named(new.as_table(), "") {
        return Some(twice);
    }
    let (a, b) = (leaves(old), leaves(new));
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();
    // M56.3: a new search provider takes its key's name along and drops a
    // custom endpoint; `key_moved` decides whether that key may follow.
    let provider = "web_search.provider";
    let follows = |k: &str| {
        a.get(provider) != b.get(provider)
            && (k == "web_search.api_key_env" || (k == "web_search.endpoint" && !b.contains_key(k)))
    };
    keys.into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .find(|k| {
            let segs: Vec<&str> = k.split('.').collect();
            !OPEN.contains(&k.as_str())
                && !follows(k)
                && (WHOLE.contains(&segs[0])
                    || channel_key(&segs)
                    || segs.iter().any(|s| guarded_key(s)))
        })
        .cloned()
}

/// M56.3: the first key `new` would send to a host `old` doesn't send it
/// to, said as a refusal. `[secrets]` itself is guarded whole; this is
/// for the keys the loader binds by itself (web search's to its
/// endpoint's host, memory's, telemetry's), which a provider switch
/// would otherwise carry to the new provider's host. The one new binding
/// let through is web search's provider's own variable on that
/// provider's own host ([`crate::config::WebSearchConfig::own_key`]).
pub(crate) fn key_moved(old: &DocumentMut, new: &DocumentMut) -> Option<String> {
    let load = |d: &DocumentMut| toml::from_str::<Config>(&d.to_string()).ok();
    let written = load(new)?;
    let now = written.clone().finish().ok()?;
    // A file that doesn't load binds what its `[secrets]` say, and those
    // can't change here.
    let was = load(old).and_then(|c| c.finish().ok()).unwrap_or(written);
    let own = now.web_search.own_key();
    let hosts = |cfg: &Config, name: &str| {
        cfg.secrets
            .get(name)
            .map_or_else(Vec::new, |s| ferrule_proxy::SecretRule::from(s).hosts)
    };
    now.secrets.keys().find_map(|name| {
        let before = hosts(&was, name);
        let host = hosts(&now, name).into_iter().find(|h| {
            !before.contains(h) && own.as_ref() != Some(&(name.clone(), h.clone()))
        })?;
        Some(format!(
            "That would send the key {name} to {host}, where it isn't sent now. Where a key goes is set on the dashboard's Keys panel or with `ferrule setup`; for another search provider, save its key with secret_request first."
        ))
    })
}

/// The text with its secret fields hidden.
fn hide(doc: &DocumentMut) -> String {
    let mut item = Item::Table(doc.as_table().clone());
    walk(&mut item, &mut Vec::new(), &mut |p, v| {
        if secret_key(key_of(p)) && v.is_str() {
            let decor = v.decor().clone();
            *v = toml_edit::Value::from(HIDDEN);
            *v.decor_mut() = decor;
        }
    });
    let mut doc = doc.clone();
    if let Item::Table(t) = item {
        *doc.as_table_mut() = t;
    }
    doc.to_string()
}

/// Puts the hidden fields back from `old`; refuses a secret changed or
/// added on the page. `redact` is what the page was sent through: a value
/// that comes back exactly as it went out is the file's value.
fn unhide(
    old: &DocumentMut,
    new: &mut DocumentMut,
    redact: &dyn Fn(&str) -> String,
) -> Result<(), String> {
    let was: BTreeMap<String, String> = {
        let mut out = BTreeMap::new();
        let mut item = Item::Table(old.as_table().clone());
        walk(&mut item, &mut Vec::new(), &mut |p, v| {
            if let Some(s) = v.as_str() {
                out.insert(p.join("."), s.to_string());
            }
        });
        out
    };
    let mut refused = None;
    let mut item = Item::Table(new.as_table().clone());
    walk(&mut item, &mut Vec::new(), &mut |p, v| {
        let at = p.join(".");
        if !secret_key(key_of(p)) {
            if let (Some(now), Some(old)) = (v.as_str(), was.get(&at)) {
                if now != old && now == redact(old) {
                    let decor = v.decor().clone();
                    *v = toml_edit::Value::from(old.as_str());
                    *v.decor_mut() = decor;
                }
            }
            return;
        }
        if !v.is_str() {
            return;
        }
        match (v.as_str(), was.get(&at)) {
            (Some(HIDDEN), Some(old)) => {
                let decor = v.decor().clone();
                *v = toml_edit::Value::from(old.as_str());
                *v.decor_mut() = decor;
            }
            (Some(now), Some(old)) if now == old => {}
            _ => {
                refused.get_or_insert(at);
            }
        }
    });
    if let Some(at) = refused {
        return Err(format!(
            "`{at}` holds a secret, which the page doesn't take here: keys go in on the Connections or Models page, into secrets.env"
        ));
    }
    if let Item::Table(t) = item {
        *new.as_table_mut() = t;
    }
    Ok(())
}

/// The loader's verdict on `text`, with the line when it has one.
fn check_text(text: &str) -> Result<(), (Option<usize>, String)> {
    match toml::from_str::<Config>(text) {
        Err(e) => {
            let line = e
                .span()
                .map(|s| text[..s.start.min(text.len())].matches('\n').count() + 1);
            Err((line, e.message().to_string()))
        }
        Ok(cfg) => cfg
            .finish()
            .map(|_| ())
            .map_err(|e| (None, format!("{e:#}"))),
    }
}

/// The versions `<data>/config-history/` keeps (M52.4).
const KEEP: usize = 20;

/// A file's text as the page's `base_hash`: SHA-256, base64.
pub(super) fn hash_of(text: &str) -> String {
    ferrule_connections::seal::sha256_b64(text.as_bytes())
}

/// M52.4: refuses a write made from a page that loaded another version
/// of the file than the one on disk now. No `base_hash` (the chat's ops,
/// an older page) is let through as before.
pub(super) fn fresh(path: &Path, body: &Value) -> Result<(), Answer> {
    let Some(base) = body.get("base_hash").and_then(Value::as_str) else {
        return Ok(());
    };
    let now = std::fs::read_to_string(path).unwrap_or_default();
    if hash_of(&now) == base {
        return Ok(());
    }
    Err(Some((
        409,
        json!({
            "error": "The config changed since this page loaded it (another tab, the chat, or the machine). Nothing was saved: reload to see the file as it is now, then make your change again.",
            "stale": true,
        }),
    )))
}

/// What `new` changes against `old`, leaf by leaf: a secret field says
/// "(hidden)", everything goes through the redactor, 200 rows at most.
fn diff(ctx: &Ctx, old: &DocumentMut, new: &DocumentMut) -> Vec<Value> {
    let (a, b) = (leaves(old), leaves(new));
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .take(200)
        .map(|k| {
            let segs: Vec<String> = k.split('.').map(str::to_string).collect();
            let secret = secret_key(key_of(&segs));
            let shown = |v: Option<&String>| {
                v.map(|v| {
                    if secret {
                        "(hidden)".to_string()
                    } else {
                        ctx.redactor.redact(v)
                    }
                })
            };
            json!({ "key": k, "before": shown(a.get(k)), "after": shown(b.get(k)) })
        })
        .collect()
}

fn history_dir(ctx: &Ctx) -> Option<PathBuf> {
    ctx.data.as_ref().map(|d| d.join("config-history"))
}

/// The kept versions, newest first: (id, its text, its note).
fn versions(dir: &Path) -> Vec<(String, String, Value)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_suffix(".toml")
                .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
                .map(str::to_string)
        })
        .collect();
    ids.sort_by(|a, b| b.len().cmp(&a.len()).then(b.cmp(a)));
    ids.into_iter()
        .filter_map(|id| {
            let text = std::fs::read_to_string(dir.join(format!("{id}.toml"))).ok()?;
            let note = std::fs::read(dir.join(format!("{id}.json")))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or(Value::Null);
            Some((id, text, note))
        })
        .collect()
}

/// Keeps `doc` as a version (its secret fields hidden, so the history
/// never holds a value), unless it's the newest one already; the oldest
/// beyond [`KEEP`] go.
fn keep_version(ctx: &Ctx, doc: &DocumentMut, how: &str, sections: &[String]) {
    let Some(dir) = history_dir(ctx) else {
        return;
    };
    let text = hide(doc);
    if text.trim().is_empty() {
        return;
    }
    let all = versions(&dir);
    if all.first().is_some_and(|(_, t, _)| *t == text) {
        return;
    }
    let mut id = chrono::Utc::now().timestamp_millis();
    if let Some(newest) = all.first().and_then(|(i, _, _)| i.parse::<i64>().ok()) {
        id = id.max(newest + 1);
    }
    let note = json!({
        "at": chrono::Utc::now().to_rfc3339(),
        "how": how,
        "by": super::api::by(),
        "sections": sections,
    });
    let written = std::fs::create_dir_all(&dir)
        .map_err(anyhow::Error::from)
        .and_then(|()| crate::secrets::write_private(&dir.join(format!("{id}.toml")), &text))
        .and_then(|()| {
            crate::secrets::write_private(&dir.join(format!("{id}.json")), &note.to_string())
        });
    if let Err(e) = written {
        tracing::warn!("config history: {e:#}");
        return;
    }
    for (old, _, _) in versions(&dir).into_iter().skip(KEEP) {
        let _ = std::fs::remove_file(dir.join(format!("{old}.toml")));
        let _ = std::fs::remove_file(dir.join(format!("{old}.json")));
    }
}

/// Keeps the file as it was before a write and as it is after (M52.4),
/// for writers outside [`commit`]: the last good copy, the autonomy switch.
pub(super) fn remember(ctx: &Ctx, before: &str, after: &str, how: &str) {
    let (Ok(old), Ok(new)) = (before.parse::<DocumentMut>(), after.parse::<DocumentMut>()) else {
        return;
    };
    let (sections, _) = changed_sections(&old, &new);
    keep_version(ctx, &old, "found", &[]);
    keep_version(ctx, &new, how, &sections);
}

pub(super) fn place(ctx: &Ctx) -> Result<PathBuf, Answer> {
    ctx.config_path
        .clone()
        .ok_or_else(|| missing("the config file"))
}

fn prev_of(path: &Path) -> PathBuf {
    super::config_prev(path)
}

/// Writes `text` over `path` atomically (temp, fsync, rename), keeping
/// what was there as `<config>.prev`.
pub fn write_keeping_prev(path: &Path, text: &str) -> anyhow::Result<()> {
    if let Ok(now) = std::fs::read_to_string(path) {
        crate::secrets::write_private(&prev_of(path), &now)?;
    }
    crate::secrets::write_private(path, text)
}

/// The sections `new` changes, and whether the running gateway follows
/// them by itself (`[[mcp.servers]]` and `[secrets]`, M17).
fn changed_sections(old: &DocumentMut, new: &DocumentMut) -> (Vec<String>, bool) {
    let (a, b) = (leaves(old), leaves(new));
    let mut out: Vec<String> = a
        .keys()
        .chain(b.keys())
        .filter(|k| a.get(*k) != b.get(*k))
        .map(|k| k.split('.').next().unwrap_or_default().to_string())
        .collect();
    out.sort();
    out.dedup();
    let restart = out.iter().any(|s| s != "mcp" && s != "secrets");
    (out, restart)
}

fn audit(ctx: &Ctx, event: &str, detail: Value) {
    if let Some(hub) = &ctx.hub {
        hub.audit()
            .record(chrono::Utc::now(), event, None, None, detail);
    }
}

/// Checks `new` (with its secrets already put back) against the file, and
/// writes it. `guard`: refuse a command-bearing change.
pub(super) fn commit(
    ctx: &Ctx,
    path: &Path,
    old: Option<&DocumentMut>,
    new: &DocumentMut,
    how: &str,
    guard: bool,
) -> Answer {
    let text = new.to_string();
    if let Err((line, why)) = check_text(&text) {
        return Some((
            422,
            json!({ "error": ctx.redactor.redact(&why), "line": line }),
        ));
    }
    let empty = DocumentMut::new();
    let old = old.unwrap_or(&empty);
    if guard {
        if let Some(field) = guarded_change(old, new) {
            return Some((403, json!({ "error": refusal(&field), "field": field })));
        }
        if let Some(why) = key_moved(old, new) {
            return bad(403, why);
        }
    }
    let (sections, restart) = changed_sections(old, new);
    // M52.4: the file as it was (written here or elsewhere), then this.
    keep_version(ctx, old, "found", &[]);
    if let Err(e) = write_keeping_prev(path, &text) {
        return bad(500, format!("{e:#}"));
    }
    keep_version(ctx, new, how, &sections);
    audit(
        ctx,
        "config_saved",
        json!({ "by": super::api::by(), "how": how, "sections": sections }),
    );
    ok(json!({ "ok": true, "sections": sections, "restart": restart && !sections.is_empty() }))
}

pub(super) fn read_doc(path: &Path) -> Result<(String, Option<DocumentMut>), Answer> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let doc = text.parse::<DocumentMut>().ok();
            Ok((text, doc))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok((String::new(), Some(DocumentMut::new())))
        }
        Err(e) => Err(bad(500, format!("couldn't read {}: {e}", path.display()))),
    }
}

/// A field a form can set: its dotted key, its kind and its choices.
struct Field {
    key: &'static str,
    kind: &'static str,
    choices: &'static [&'static str],
    help: &'static str,
}

const FORM: &[Field] = &[
    Field {
        key: "agent.stream",
        kind: "bool",
        choices: &[],
        help: "Stream replies as they're written",
    },
    Field {
        key: "agent.parallel_tools",
        kind: "int",
        choices: &[],
        help: "Tool calls run at once",
    },
    Field {
        key: "agent.edit_file",
        kind: "bool",
        choices: &[],
        help: "The edit_file tool",
    },
    Field {
        key: "agent.repo_map_tokens",
        kind: "int",
        choices: &[],
        help: "Repository map budget (0: off)",
    },
    Field {
        key: "agent.lint",
        kind: "choice",
        choices: &["auto", "off"],
        help: "Lint after an edit",
    },
    Field {
        key: "agent.auto_commit",
        kind: "bool",
        choices: &[],
        help: "Commit each change, for /undo",
    },
    Field {
        key: "agent.verify_timeout_secs",
        kind: "int",
        choices: &[],
        help: "How long the verify command may run",
    },
    // The sub-agents' budget. config_set asks the owner on a card like every
    // other change here, so an agent still can't raise its own budget alone.
    Field {
        key: "agents.max_tokens",
        kind: "int",
        choices: &[],
        help: "Tokens all of a chat's sub-agents may use per window",
    },
    Field {
        key: "agents.budget_window_hours",
        kind: "int",
        choices: &[],
        help: "The sub-agents' budget window, in hours",
    },
    // In `[plans]`, which the page can't change otherwise: a longer limit
    // runs nothing new. The engine reads it when its driver is built, so
    // a running gateway keeps the old limit until it restarts.
    Field {
        key: "plans.claude_code.turn_timeout_minutes",
        kind: "int",
        choices: &[],
        help: "How long a Claude Code turn may run, in minutes (5–240; after a restart)",
    },
    // `[gateway]`'s own switches; the channels read them when they're
    // built, so a running gateway keeps the old ones until it restarts.
    Field {
        key: "gateway.telegram_stream",
        kind: "bool",
        choices: &[],
        help: "Stream in Telegram (after a restart)",
    },
    Field {
        key: "gateway.telegram_group_mention_only",
        kind: "bool",
        choices: &[],
        help: "Telegram groups: answer only an @mention or a reply (after a restart)",
    },
    Field {
        key: "gateway.discord_stream",
        kind: "bool",
        choices: &[],
        help: "Stream in Discord (after a restart)",
    },
    Field {
        key: "gateway.slack_stream",
        kind: "bool",
        choices: &[],
        help: "Stream in Slack (after a restart)",
    },
    Field {
        key: "gateway.busy_notices",
        kind: "bool",
        choices: &[],
        help: "Tell a message sent during a long turn that I'm busy (after a restart)",
    },
    // The chat notice only: systemd's watchdog and the heartbeat don't
    // read it (a lane is stuck after 10 min for them when it's 0).
    Field {
        key: "health.watchdog_after_secs",
        kind: "int",
        choices: &[],
        help: "A turn with no progress this long: one \"Stuck on …\" message, in seconds (0 = off; after a restart)",
    },
    Field {
        key: "scheduler.tick_interval_secs",
        kind: "int",
        choices: &[],
        help: "How often due tasks are looked for",
    },
    Field {
        key: "dashboard.idle_minutes",
        kind: "int",
        choices: &[],
        help: "The tunnel closes after this long unused",
    },
    Field {
        key: "dashboard.session_hours",
        kind: "int",
        choices: &[],
        help: "A login lasts this long",
    },
    Field {
        key: "dashboard.link_minutes",
        kind: "int",
        choices: &[],
        help: "A login link works this long",
    },
    Field {
        key: "update.auto",
        kind: "bool",
        choices: &[],
        help: "Install updates by itself",
    },
    Field {
        key: "update.channel",
        kind: "choice",
        choices: &["stable", "prerelease"],
        help: "Which releases count",
    },
    Field {
        key: "update.claude",
        kind: "bool",
        choices: &[],
        help: "Keep Claude Code up to date too",
    },
    Field {
        key: "memory.vector_weight",
        kind: "float",
        choices: &[],
        help: "Meaning vs words in memory search (0–1)",
    },
    Field {
        key: "memory.min_similarity",
        kind: "float",
        choices: &[],
        help: "Weakest match memory search returns",
    },
    // M56.3: web search and the browser, from the chat too. The key's
    // variable and the endpoint stay guarded: they move where a key goes.
    Field {
        key: "web_search.provider",
        kind: "choice",
        choices: &["duckduckgo", "brave", "tavily", "exa", "off"],
        help: "Web search service (duckduckgo needs no key; off: no web search)",
    },
    Field {
        key: "web_search.max_results",
        kind: "int",
        choices: &[],
        help: "Results per search (1–20)",
    },
    Field {
        key: "web_search.safe_search",
        kind: "choice",
        choices: &["off", "moderate", "strict"],
        help: "Safe search",
    },
    Field {
        key: "web_search.region",
        kind: "text",
        choices: &[],
        help: "Search region, like us or de",
    },
    Field {
        key: "web_search.language",
        kind: "text",
        choices: &[],
        help: "Search language, like en or he",
    },
    Field {
        key: "web_search.max_output_tokens",
        kind: "int",
        choices: &[],
        help: "Most of a search's results the model reads",
    },
    Field {
        key: "web_search.timeout_secs",
        kind: "int",
        choices: &[],
        help: "How long a search may take",
    },
    Field {
        key: "web_search.price_per_search_usd",
        kind: "float",
        choices: &[],
        help: "What a search costs, toward the dollar caps",
    },
    Field {
        key: "web_search.max_searches_per_day",
        kind: "int",
        choices: &[],
        help: "Searches a day (0: no cap)",
    },
    Field {
        key: "browser.enabled",
        kind: "bool",
        choices: &[],
        help: "The browser tools",
    },
];

fn field_value(doc: &DocumentMut, key: &str) -> Value {
    let mut item = doc.as_item();
    for seg in key.split('.') {
        match item.get(seg) {
            Some(i) => item = i,
            None => return Value::Null,
        }
    }
    match item.as_value() {
        Some(toml_edit::Value::Boolean(b)) => json!(*b.value()),
        Some(toml_edit::Value::Integer(i)) => json!(*i.value()),
        Some(toml_edit::Value::Float(f)) => json!(*f.value()),
        Some(toml_edit::Value::String(s)) => json!(s.value()),
        _ => Value::Null,
    }
}

/// `GET /api/config`: the file with its secrets hidden, the form's fields
/// and what can be undone.
pub fn get(ctx: &Ctx) -> Answer {
    let path = need!(place(ctx));
    let (text, doc) = need!(read_doc(&path));
    let shown = match &doc {
        Some(d) => hide(d),
        // Not TOML at all: the page shows nothing it can't hide.
        None => String::new(),
    };
    let fields: Vec<Value> = FORM
        .iter()
        .map(|f| {
            json!({
                "key": f.key, "kind": f.kind, "choices": f.choices, "help": f.help,
                "value": doc.as_ref().map_or(Value::Null, |d| field_value(d, f.key)),
            })
        })
        .collect();
    let reads = check_text(&text)
        .err()
        .map(|(line, why)| json!({ "line": line, "error": ctx.redactor.redact(&why) }));
    ok(json!({
        "path": path.display().to_string(),
        // M52.4: sent back with every write, so a stale page is refused.
        "hash": hash_of(&text),
        "text": ctx.redactor.redact(&shown),
        "readable": doc.is_some(),
        "problem": reads,
        "fields": fields,
        "prev": prev_of(&path).exists(),
        "last_good": ctx.data.as_ref().is_some_and(|d| crate::last_good::path(d).exists()),
    }))
}

/// The page's text, parsed, with the file's secrets put back.
fn incoming(
    ctx: &Ctx,
    body: &Value,
) -> Result<(PathBuf, Option<DocumentMut>, DocumentMut), Answer> {
    let path = place(ctx)?;
    let text = body
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| bad(400, "`text` is missing"))?;
    if text.len() > 512 * 1024 {
        return Err(bad(413, "that file is over 512 KB"));
    }
    let mut new = text.parse::<DocumentMut>().map_err(|e| {
        let line = e
            .span()
            .map(|s| text[..s.start.min(text.len())].matches('\n').count() + 1);
        Some((422, json!({ "error": e.message(), "line": line })))
    })?;
    let (_, old) = read_doc(&path)?;
    let empty = DocumentMut::new();
    let redact = |s: &str| ctx.redactor.redact(s);
    unhide(old.as_ref().unwrap_or(&empty), &mut new, &redact).map_err(|why| bad(403, why))?;
    Ok((path, old, new))
}

/// `POST /api/config/check {text}`: whether it reads and whether it may be
/// saved, without saving, and (M52.4) what it changes, value by value,
/// secrets hidden.
pub fn check(ctx: &Ctx, body: &Value) -> Answer {
    let (path, old, new) = need!(incoming(ctx, body));
    need!(fresh(&path, body));
    if let Err((line, why)) = check_text(&new.to_string()) {
        return ok(json!({ "ok": false, "line": line, "error": ctx.redactor.redact(&why) }));
    }
    let empty = DocumentMut::new();
    let old = old.as_ref().unwrap_or(&empty);
    if let Some(field) = guarded_change(old, &new) {
        return ok(json!({ "ok": false, "error": refusal(&field), "field": field }));
    }
    if let Some(why) = key_moved(old, &new) {
        return ok(json!({ "ok": false, "error": why }));
    }
    let (sections, restart) = changed_sections(old, &new);
    ok(json!({
        "ok": true,
        "sections": sections,
        "restart": restart && !sections.is_empty(),
        "diff": diff(ctx, old, &new),
    }))
}

/// `POST /api/config/save {text, base_hash?}`.
pub fn save(ctx: &Ctx, body: &Value) -> Answer {
    let (path, old, new) = need!(incoming(ctx, body));
    need!(fresh(&path, body));
    commit(ctx, &path, old.as_ref(), &new, "editor", true)
}

/// `GET /api/config/versions` (M52.4): the kept versions, newest first,
/// with when, how and by whom each was written and which sections it
/// changed; whether each is the file as it is now.
pub fn versions_view(ctx: &Ctx) -> Answer {
    let path = need!(place(ctx));
    let Some(dir) = history_dir(ctx) else {
        return ok(json!({ "versions": [] }));
    };
    let now = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| t.parse::<DocumentMut>().ok())
        .map(|d| hide(&d));
    let list: Vec<Value> = versions(&dir)
        .into_iter()
        .map(|(id, text, note)| {
            json!({
                "id": id,
                "at": note["at"],
                "how": note["how"],
                "by": note["by"],
                "sections": note["sections"],
                "current": now.as_deref() == Some(text.as_str()),
            })
        })
        .collect();
    ok(json!({ "versions": list, "keep": KEEP }))
}

/// A kept version by its id, as a document.
fn version(ctx: &Ctx, id: &str) -> Result<DocumentMut, Answer> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad(400, "`id` is a version's number"));
    }
    let dir = history_dir(ctx).ok_or_else(|| missing("the data directory"))?;
    let text = std::fs::read_to_string(dir.join(format!("{id}.toml")))
        .map_err(|_| bad(404, "that version isn't kept any more"))?;
    text.parse::<DocumentMut>()
        .map_err(|_| bad(422, "that version doesn't read"))
}

/// `GET /api/config/version?id=`: what restoring it would change in the
/// file as it is now, as a diff of non-secret values.
pub fn version_diff(ctx: &Ctx, req: &super::http::Request) -> Answer {
    let id = req.query.get("id").map(String::as_str).unwrap_or_default();
    let path = need!(place(ctx));
    let target = need!(version(ctx, id));
    let (_, old) = need!(read_doc(&path));
    let empty = DocumentMut::new();
    let old = old.as_ref().unwrap_or(&empty);
    ok(json!({ "id": id, "diff": diff(ctx, old, &target) }))
}

/// `POST /api/config/version/restore {id, base_hash?, confirm}`: the file
/// back to a kept version, through the same checks and guard as a save
/// (its hidden secrets come back from the file as it is; a secret field
/// that differs is refused, like the editor's).
pub fn version_restore(ctx: &Ctx, body: &Value) -> Answer {
    let id = need!(arg(body, "id")).to_string();
    let path = need!(place(ctx));
    need!(fresh(&path, body));
    let mut new = need!(version(ctx, &id));
    let (_, old) = need!(read_doc(&path));
    let empty = DocumentMut::new();
    let redact = |s: &str| ctx.redactor.redact(s);
    if let Err(why) = unhide(old.as_ref().unwrap_or(&empty), &mut new, &redact) {
        return bad(403, why);
    }
    let changes = diff(ctx, old.as_ref().unwrap_or(&empty), &new).len();
    if changes == 0 {
        return ok(json!({ "ok": true, "said": "The file is that version already." }));
    }
    need!(super::api::confirmed(
        body,
        format!(
            "Put the config back to this version? It changes {changes} value(s); the file as it is now stays in the history."
        )
    ));
    // As undo: a file that doesn't read is the way out, so not guarded.
    let guard = old.is_some();
    commit(ctx, &path, old.as_ref(), &new, "restore", guard)
}

/// `POST /api/config/set {key, value, base_hash?}`: one form field.
pub fn set(ctx: &Ctx, body: &Value) -> Answer {
    let key = need!(arg(body, "key"));
    let Some(field) = FORM.iter().find(|f| f.key == key) else {
        return bad(400, format!("`{key}` isn't a field the page sets"));
    };
    let path = need!(place(ctx));
    need!(fresh(&path, body));
    let (_, old) = need!(read_doc(&path));
    let Some(old) = old else {
        return bad(422, "the config file doesn't read as TOML; fix it in the editor or restore the last good copy");
    };
    let raw = body.get("value").unwrap_or(&Value::Null);
    let value: Option<toml_edit::Value> = match (field.kind, raw) {
        (_, Value::Null) => None,
        ("bool", Value::Bool(b)) => Some((*b).into()),
        ("int", Value::Number(n)) if n.as_i64().is_some_and(|i| i >= 0) => {
            n.as_i64().map(Into::into)
        }
        ("float", Value::Number(n)) => n.as_f64().map(Into::into),
        ("choice", Value::String(s)) if field.choices.contains(&s.as_str()) => {
            Some(s.as_str().into())
        }
        // A short code (a region, a language), nothing more.
        ("text", Value::String(s))
            if (1..=16).contains(&s.len())
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') =>
        {
            Some(s.as_str().into())
        }
        _ => {
            return bad(
                400,
                format!(
                    "`{key}` takes {}",
                    match field.kind {
                        "bool" => "on or off".to_string(),
                        "int" => "a whole number".to_string(),
                        "float" => "a number".to_string(),
                        "text" => "a short code: letters, digits, - or _".to_string(),
                        _ => format!("one of {}", field.choices.join(", ")),
                    }
                ),
            )
        }
    };
    if let (Some((_, least, most)), Some(v)) = (
        BOUNDS.iter().find(|(k, ..)| *k == key),
        value.as_ref().and_then(toml_edit::Value::as_integer),
    ) {
        if !(*least..=*most).contains(&v) {
            return bad(400, format!("`{key}` takes {least} to {most}"));
        }
    }
    let mut new = old.clone();
    // `plans.claude_code.turn_timeout_minutes` is two tables deep.
    let (section, name) = key.rsplit_once('.').unwrap_or(("", key));
    let mut table: &mut dyn toml_edit::TableLike = new.as_table_mut();
    for seg in section.split('.') {
        let item = table.entry(seg).or_insert_with(|| {
            let mut t = toml_edit::Table::new();
            t.set_implicit(true);
            Item::Table(t)
        });
        let Some(t) = item.as_table_like_mut() else {
            return bad(422, format!("`[{section}]` isn't a table"));
        };
        table = t;
    }
    match value {
        // Empty: back to the default.
        None => {
            table.remove(name);
        }
        Some(v) => {
            table.insert(name, Item::Value(v));
        }
    }
    let mut said = None;
    if key == "web_search.provider" && field_value(&old, key) != field_value(&new, key) {
        let (var, words) = match search_key(ctx, &old, field_value(&new, key).as_str()) {
            Ok(found) => found,
            Err(why) => return bad(409, why),
        };
        if let Some(t) = new.get_mut("web_search").and_then(Item::as_table_like_mut) {
            t.remove("endpoint");
            match var {
                Some(var) => {
                    t.insert("api_key_env", Item::Value(var.into()));
                }
                None => {
                    t.remove("api_key_env");
                }
            }
        }
        said = Some(words);
    }
    let mut answer = commit(ctx, &path, Some(&old), &new, "form", true);
    if let (Some(words), Some((200, v))) = (said, answer.as_mut()) {
        v["said"] = json!(words);
    }
    answer
}

/// M56.3: the key a new search provider gets from the form or the chat,
/// and what to say. Only a key that goes to that provider's host already:
/// a `[secrets]` entry bound there (the key in use now, the provider's own
/// variable, then any other), else the provider's own variable
/// (`TAVILY_API_KEY`) when it's saved and `[secrets]` doesn't name it,
/// which the loader binds to that host alone. Nothing fits: refused, with
/// the `secret_request` that would make one. A provider without a key,
/// and "off", drop the key's name; every switch drops a custom endpoint.
fn search_key(
    ctx: &Ctx,
    old: &DocumentMut,
    provider: Option<&str>,
) -> Result<(Option<String>, String), String> {
    use ferrule_tools::search::SearchProvider;
    let p = match provider {
        None => Some(SearchProvider::Duckduckgo),
        Some(name) => SearchProvider::parse(name),
    };
    let (Some(p), Some(own)) = (p, p.and_then(SearchProvider::key_env)) else {
        return Ok((
            None,
            match p {
                Some(p) => format!("Web search uses {} now, with no key.", p.name()),
                None => "Web search is off.".into(),
            },
        ));
    };
    let host = crate::config::WebSearchConfig::default()
        .host(p)
        .ok_or_else(|| format!("{} has no address to send its key to", p.name()))?;
    let cfg = toml::from_str::<Config>(&old.to_string()).ok();
    let secrets = cfg.as_ref().map(|c| &c.secrets);
    let bound_here = |name: &str| {
        let hosts = ferrule_proxy::SecretRule::from(secrets?.get(name)?).hosts;
        hosts.contains(&host).then_some(hosts)
    };
    let current = cfg.as_ref().and_then(|c| c.web_search.api_key_env.clone());
    let found = current
        .iter()
        .map(String::as_str)
        .chain([own])
        .chain(
            secrets
                .into_iter()
                .flat_map(|s| s.keys().map(String::as_str)),
        )
        .find_map(|name| Some((name.to_string(), bound_here(name)?)));
    if let Some((var, hosts)) = found {
        let to = hosts.join(", ");
        let words = format!(
            "Web search uses {} now, with the key {var}, which goes only to {to}.",
            p.name()
        );
        return Ok((Some(var), words));
    }
    if secrets.is_none_or(|s| !s.contains_key(own)) && saved(ctx, own) {
        let words = format!(
            "Web search uses {} now, with the key {own}, which goes only to {host}.",
            p.name()
        );
        return Ok((Some(own.into()), words));
    }
    Err(format!(
        "{} needs a key, and none saved here goes to {host}. Ask the owner for one with secret_request (name {own}, or another if that one is taken; host {host}), then set the provider again. Web search is unchanged.",
        p.name()
    ))
}

/// Whether the key `name` has a value: in this process, or in the secrets
/// file the page writes.
fn saved(ctx: &Ctx, name: &str) -> bool {
    crate::secrets::non_empty(name).is_some()
        || super::api::setup_place(ctx)
            .ok()
            .and_then(|p| crate::secrets::read(&p.secrets).ok())
            .is_some_and(|kept| kept.iter().any(|(n, v)| n == name && !v.is_empty()))
}

/// Replaces the file with `from`'s text after checking it reads.
fn put_back(ctx: &Ctx, from: &Path, how: &str, what: &str) -> Answer {
    let path = need!(place(ctx));
    let text = match std::fs::read_to_string(from) {
        Ok(t) => t,
        Err(_) => return bad(404, format!("there's no {what} to go back to")),
    };
    let Ok(new) = text.parse::<DocumentMut>() else {
        return bad(422, format!("the {what} doesn't read either"));
    };
    let (_, old) = need!(read_doc(&path));
    // A file that doesn't read at all came from a terminal, and going
    // back is the way out of it; otherwise the same fields are guarded.
    let guard = old.is_some();
    commit(ctx, &path, old.as_ref(), &new, how, guard)
}

/// `POST /api/config/undo {base_hash?}`: the file as it was before the
/// page's last save.
pub fn undo(ctx: &Ctx, body: &Value) -> Answer {
    let path = need!(place(ctx));
    need!(fresh(&path, body));
    put_back(ctx, &prev_of(&path), "undo", "earlier copy")
}

/// What the Settings page's Autonomy row shows (M54 §8), from the config.
/// `owner` and `off` are themselves; `workspace-write` is "Ask me first",
/// and so is `read-only`, which asks first too (it just can't write at
/// all). A config that doesn't read shows the default, "ask".
pub fn autonomy(ctx: &Ctx) -> &'static str {
    use ferrule_sandbox::Mode;
    match super::api::config(ctx).map_or(Mode::default(), |c| c.sandbox.mode) {
        Mode::Owner => "owner",
        Mode::Off => "off",
        Mode::WorkspaceWrite | Mode::ReadOnly => "ask",
    }
}

/// `POST /api/sandbox/mode {mode: "owner" | "ask", confirm}`: the owner's
/// switch (M54 §8). Page-only: no chat op maps to it (`route_of`), so
/// nothing in the chat or the bot's shell can change `[sandbox]`. "ask"
/// writes `workspace-write` and drops `[trust] gates = false`; either
/// choice restarts the gateway.
pub fn sandbox_mode(ctx: &Ctx, body: &Value) -> Answer {
    if crate::managed::on() {
        return bad(
            403,
            crate::managed::refused("The autonomy switch", "the sandbox is the panel's"),
        );
    }
    let (value, label) = match need!(arg(body, "mode")) {
        "owner" => ("owner", "Protected autonomous"),
        "ask" => ("workspace-write", "Ask me first"),
        other => return bad(400, format!("`mode` is `owner` or `ask`, not `{other}`")),
    };
    let path = need!(place(ctx));
    need!(fresh(&path, body));
    let mut t = match crate::setup::Target::load(path.clone()) {
        Ok(t) => t,
        Err(e) => return bad(422, format!("{e:#}")),
    };
    let old = t
        .root()
        .get("sandbox")
        .and_then(Item::as_table_like)
        .and_then(|s| s.get("mode"))
        .and_then(Item::as_str)
        .unwrap_or("workspace-write")
        .to_string();
    // `gates = true` or none is left alone; only the off switch goes.
    let gates_off = value == "workspace-write"
        && t.root()
            .get("trust")
            .and_then(Item::as_table_like)
            .and_then(|s| s.get("gates"))
            .and_then(Item::as_bool)
            == Some(false);
    if old == value && !gates_off {
        return ok(json!({ "ok": true, "said": format!("Already in {label} mode.") }));
    }
    need!(super::api::restart_refusal(ctx));
    need!(super::api::confirmed(
        body,
        if value == "owner" && !cfg!(target_os = "linux") {
            format!("Restart in {label} mode? Owner mode needs Linux; this host would fall back to Ask me first.")
        } else {
            format!("Restart in {label} mode?")
        }
    ));
    let sandbox = match crate::setup::table(t.root(), &["sandbox"]) {
        Ok(s) => s,
        Err(e) => return bad(422, format!("{e:#}")),
    };
    crate::setup::put(sandbox, "mode", value);
    if gates_off {
        if let Some(trust) = t.root().get_mut("trust").and_then(Item::as_table_like_mut) {
            trust.remove("gates");
        }
    }
    // Same as the page's other writes: what was there stays as `.prev`.
    let before = std::fs::read_to_string(&path).ok();
    if let Some(now) = &before {
        if let Err(e) = crate::secrets::write_private(&prev_of(&path), now) {
            return bad(500, format!("{e:#}"));
        }
    }
    if let Err(e) = t.save() {
        return bad(500, format!("{e:#}"));
    }
    if let (Some(before), Ok(after)) = (&before, std::fs::read_to_string(&path)) {
        remember(ctx, before, &after, "autonomy");
    }
    audit(
        ctx,
        "dashboard.sandbox.mode",
        json!({ "by": super::api::by(), "from": old, "to": value, "gates_removed": gates_off }),
    );
    super::api::gateway_restart(ctx, &json!({ "confirm": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(s: &str) -> DocumentMut {
        s.parse().unwrap()
    }

    #[test]
    fn command_bearing_fields_are_found_whatever_else_changes() {
        let old = doc("[agent]\nstream = true\n\n[[mcp.servers]]\nname = \"fs\"\ncommand = \"npx\"\nargs = [\"a\"]\n");
        let ok = doc("[agent]\nstream = false\n\n[[mcp.servers]]\nname = \"fs\"\ncommand = \"npx\"\nargs = [\"a\"]\n");
        assert_eq!(guarded_change(&old, &ok), None);
        let args = doc("[agent]\nstream = true\n\n[[mcp.servers]]\nname = \"fs\"\ncommand = \"npx\"\nargs = [\"a\", \"b\"]\n");
        assert_eq!(
            guarded_change(&old, &args).as_deref(),
            Some("mcp.servers.[fs].args.1")
        );
        for (text, field) in [
            (
                "[agent]\nverify_command = \"make\"\n",
                "agent.verify_command",
            ),
            ("[sandbox]\nmode = \"off\"\n", "sandbox.mode"),
            ("[[hooks.pre_tool]]\ncommand = \"x\"\n", "hooks"),
            (
                "[gateway]\ntelegram_allowed_chats = [1]\n",
                "gateway.telegram_allowed_chats.0",
            ),
            (
                "[gateway]\ntelegram_base_url = \"https://x\"\n",
                "gateway.telegram_base_url",
            ),
            ("[trust]\ngates = false\n", "trust.gates"),
            (
                "[connections]\ncloudflared = \"/tmp/x\"\n",
                "connections.cloudflared",
            ),
            (
                "[plans.claude_code]\nbinary = \"/tmp/x\"\n",
                "plans.claude_code.binary",
            ),
        ] {
            let got = guarded_change(&DocumentMut::new(), &doc(text)).unwrap_or_default();
            assert!(got.starts_with(field), "{text} -> {got}");
        }
        // `[gateway]`'s channel tables stay with the Channels page.
        for (text, field) in [
            (
                "[gateway.email]\nimap_host = \"x\"\n",
                "gateway.email.imap_host",
            ),
            ("[gateway.http]\nport = 1\n", "gateway.http.port"),
            ("[gateway]\nlocal = true\n", "gateway.local"),
            (
                "[connections]\ngate_writes = false\n",
                "connections.gate_writes",
            ),
        ] {
            let got = guarded_change(&DocumentMut::new(), &doc(text)).unwrap_or_default();
            assert_eq!(got, field, "{text}");
        }
        // The gateway isn't a gate: its own switches are the page's.
        let switches = doc("[gateway]\ntelegram_group_mention_only = true\ntelegram_stream = false\ndiscord_stream = false\nslack_stream = false\nbusy_notices = false\n");
        assert_eq!(guarded_change(&DocumentMut::new(), &switches), None);
        // The stuck-turn message is a notice, not a gate.
        let watchdog = doc("[health]\nwatchdog_after_secs = 0\n");
        assert_eq!(guarded_change(&DocumentMut::new(), &watchdog), None);
        // The one `[plans]` field the page sets.
        let timeout = doc("[plans.claude_code]\nturn_timeout_minutes = 60\n");
        assert_eq!(guarded_change(&DocumentMut::new(), &timeout), None);
    }

    #[test]
    fn secrets_go_out_hidden_and_come_back_only_unchanged() {
        let old =
            doc("[x]\napi_key = \"abc123\" # mine\nkey_env = \"K\"\ntelegram_token_env = \"T\"\n");
        let same = |s: &str| s.to_string();
        let shown = hide(&old);
        assert!(!shown.contains("abc123"), "{shown}");
        assert!(
            shown.contains("# mine") && shown.contains("\"T\""),
            "{shown}"
        );
        let mut back = doc(&shown);
        unhide(&old, &mut back, &same).unwrap();
        assert!(back.to_string().contains("\"abc123\""));
        let mut changed = doc(&shown.replace(HIDDEN, "other"));
        assert!(unhide(&old, &mut changed, &same)
            .unwrap_err()
            .contains("x.api_key"));
        let mut added = doc("[y]\ntoken = \"new\"\n");
        assert!(unhide(&old, &mut added, &same).is_err());
    }
}
