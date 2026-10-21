//! M37 §4.4: the config from the page. A form per section and the raw
//! TOML, both saved the same way: checked with the real loader, refused
//! when they change a field that amounts to running a command on the
//! machine, written atomically with the file they replace kept as
//! `<config>.prev`. Secret-looking fields are hidden on the way out and
//! must come back unchanged. The last good copy is put back by
//! `config/restore` (§1.3).

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
];

/// A field the page can't change, by its own name: it names a command,
/// a path or an environment, loosens a gate, lets someone new in, or
/// moves where a secret is sent.
fn guarded_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k.contains("command")
        || k.contains("gate")
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

/// The first field the page may not change that `new` changes.
pub(crate) fn guarded_change(old: &DocumentMut, new: &DocumentMut) -> Option<String> {
    if let Some(twice) = twice_named(new.as_table(), "") {
        return Some(twice);
    }
    let (a, b) = (leaves(old), leaves(new));
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .find(|k| {
            let segs: Vec<&str> = k.split('.').collect();
            WHOLE.contains(&segs[0]) || segs.iter().any(|s| guarded_key(s))
        })
        .cloned()
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

fn place(ctx: &Ctx) -> Result<PathBuf, Answer> {
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
fn commit(
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
            return Some((
                403,
                json!({
                    "error": format!("`{field}` can't be changed from the chat or the Config page: it runs something on the machine, lets someone in, or decides where a secret goes. Channels, hooks and keys have their own pages on the dashboard."),
                    "field": field,
                }),
            ));
        }
    }
    let (sections, restart) = changed_sections(old, new);
    if let Err(e) = write_keeping_prev(path, &text) {
        return bad(500, format!("{e:#}"));
    }
    audit(
        ctx,
        "config_saved",
        json!({ "by": super::api::by(), "how": how, "sections": sections }),
    );
    ok(json!({ "ok": true, "sections": sections, "restart": restart && !sections.is_empty() }))
}

fn read_doc(path: &Path) -> Result<(String, Option<DocumentMut>), Answer> {
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
    Field {
        key: "gateway.telegram_stream",
        kind: "bool",
        choices: &[],
        help: "Stream in Telegram",
    },
    Field {
        key: "gateway.discord_stream",
        kind: "bool",
        choices: &[],
        help: "Stream in Discord",
    },
    Field {
        key: "gateway.slack_stream",
        kind: "bool",
        choices: &[],
        help: "Stream in Slack",
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
/// saved, without saving.
pub fn check(ctx: &Ctx, body: &Value) -> Answer {
    let (_, old, new) = need!(incoming(ctx, body));
    if let Err((line, why)) = check_text(&new.to_string()) {
        return ok(json!({ "ok": false, "line": line, "error": ctx.redactor.redact(&why) }));
    }
    let empty = DocumentMut::new();
    let old = old.as_ref().unwrap_or(&empty);
    if let Some(field) = guarded_change(old, &new) {
        return ok(
            json!({ "ok": false, "field": field, "error": format!("`{field}` can't be changed from the page") }),
        );
    }
    let (sections, restart) = changed_sections(old, &new);
    ok(json!({ "ok": true, "sections": sections, "restart": restart && !sections.is_empty() }))
}

/// `POST /api/config/save {text}`.
pub fn save(ctx: &Ctx, body: &Value) -> Answer {
    let (path, old, new) = need!(incoming(ctx, body));
    commit(ctx, &path, old.as_ref(), &new, "editor", true)
}

/// `POST /api/config/set {key, value}`: one form field.
pub fn set(ctx: &Ctx, body: &Value) -> Answer {
    let key = need!(arg(body, "key"));
    let Some(field) = FORM.iter().find(|f| f.key == key) else {
        return bad(400, format!("`{key}` isn't a field the page sets"));
    };
    let path = need!(place(ctx));
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
        _ => {
            return bad(
                400,
                format!(
                    "`{key}` takes {}",
                    match field.kind {
                        "bool" => "on or off".to_string(),
                        "int" => "a whole number".to_string(),
                        "float" => "a number".to_string(),
                        _ => format!("one of {}", field.choices.join(", ")),
                    }
                ),
            )
        }
    };
    let mut new = old.clone();
    let (section, name) = key.split_once('.').unwrap_or(("", key));
    let table = new
        .entry(section)
        .or_insert_with(toml_edit::table)
        .as_table_like_mut();
    let Some(table) = table else {
        return bad(422, format!("`[{section}]` isn't a table"));
    };
    match value {
        // Empty: back to the default.
        None => {
            table.remove(name);
        }
        Some(v) => {
            table.insert(name, Item::Value(v));
        }
    }
    commit(ctx, &path, Some(&old), &new, "form", true)
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

/// `POST /api/config/undo`: the file as it was before the page's last save.
pub fn undo(ctx: &Ctx) -> Answer {
    let path = need!(place(ctx));
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
    if let Ok(now) = std::fs::read_to_string(&path) {
        if let Err(e) = crate::secrets::write_private(&prev_of(&path), &now) {
            return bad(500, format!("{e:#}"));
        }
    }
    if let Err(e) = t.save() {
        return bad(500, format!("{e:#}"));
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
