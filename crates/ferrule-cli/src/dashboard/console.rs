//! M37 §4.1/§4.2: the command console, a line and not a terminal. The
//! server splits the line itself (no shell: `$`, globs, pipes, `;`, `&&`
//! and redirections are refused), parses it with clap on the real command
//! tree, classes the command, and runs an allowed one as a child of this
//! binary with stdin closed. [`PARITY`] is where every subcommand lives.

use super::api::{bad, ok, Answer};
use super::http::Request;
use super::Ctx;
use clap::parser::ValueSource;
use clap::{ArgMatches, CommandFactory};
use serde_json::{json, Value};
use std::ffi::OsString;

/// What running a command from the page takes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Class {
    /// Runs at once.
    Read,
    /// The page confirms first (`409` until `confirm: true`).
    Change,
    /// Confirms, with the red button.
    Destructive,
    /// Not from the page; the reason says what does it instead.
    Refused(&'static str),
}

/// One leaf of the command tree: its class as typed, where it lives on the
/// page, and a note. Flags can change the class ([`classify`]).
pub struct Leaf {
    pub path: &'static str,
    pub class: Class,
    pub page: &'static str,
    pub note: &'static str,
}

const fn leaf(path: &'static str, class: Class, page: &'static str, note: &'static str) -> Leaf {
    Leaf {
        path,
        class,
        page,
        note,
    }
}

use Class::{Change, Destructive, Read, Refused};

const PROMPTS: &str = "it asks at the terminal";

/// Every `ferrule` subcommand, and where it lives on the page. A unit test
/// walks `Cli::command()` and fails on a leaf that isn't here.
pub const PARITY: &[Leaf] = &[
    leaf(
        "setup",
        Refused("the wizard asks at the terminal; `setup --refresh-service` runs here"),
        "terminal only",
        "`--refresh-service` from the console",
    ),
    leaf("doctor", Read, "Home (Run doctor) + console", ""),
    leaf(
        "update",
        Change,
        "fix buttons + console",
        "needs `--yes` (the page's confirm is the yes) or `--check`",
    ),
    leaf("run", Change, "console", "one agent turn, with its tools"),
    leaf(
        "graph run",
        Change,
        "console",
        "an agent graph, end to end; approval nodes need `--yes` here",
    ),
    leaf(
        "chat",
        Refused("a REPL isn't a console command: use the Chat page"),
        "Chat",
        "",
    ),
    leaf("memory add", Change, "console", ""),
    leaf("memory search", Read, "console", ""),
    leaf("memory reindex", Change, "console", ""),
    leaf("memory model download", Change, "console", "needs `--yes`"),
    leaf("memory recent", Read, "console", ""),
    leaf("memory forget", Destructive, "console", ""),
    leaf("config path", Read, "Config + console", ""),
    leaf(
        "config edit",
        Refused("an editor isn't a web thing: use the Config page's editor"),
        "Config (the raw editor)",
        "",
    ),
    leaf("config example", Read, "console", ""),
    leaf(
        "config init",
        Refused("the service already has its config: change it on the Config page"),
        "terminal only",
        "",
    ),
    leaf(
        "gateway",
        Refused("it is the running service"),
        "terminal only",
        "",
    ),
    leaf("status", Read, "Home + console", ""),
    leaf("health", Read, "console", ""),
    leaf(
        "backup",
        Change,
        "console",
        "`--include-secrets` is terminal only",
    ),
    leaf(
        "restore",
        Refused("it refuses while the gateway runs, and the page is the gateway: stop it and restore at the terminal"),
        "terminal only",
        "`--dry-run` checks a backup from the console",
    ),
    leaf(
        "dashboard link",
        Change,
        "console",
        "`--remote` is terminal only (it holds a terminal open)",
    ),
    leaf(
        "dashboard off",
        Destructive,
        "session menu + console",
        "logs out every browser, this one too",
    ),
    leaf("model list", Read, "Models + console", ""),
    leaf("model default", Change, "Models + console", ""),
    leaf("model test", Read, "Models + console", ""),
    leaf("model add", Change, "Models + console", ""),
    leaf("model remove", Destructive, "Models + console", ""),
    leaf("model alias", Change, "Models + console", ""),
    leaf("model pin", Change, "Models + console", ""),
    leaf("model unpin", Change, "Models + console", ""),
    leaf("model fallback", Change, "Models + console", ""),
    leaf("model catalog", Read, "Models + console", ""),
    leaf("model recommend", Read, "console", ""),
    leaf("model fill-prices", Change, "console", ""),
    leaf(
        "model eval",
        Change,
        "Eval + console",
        "needs `--yes` (it spends money)",
    ),
    leaf("model route set", Change, "Routing + console", ""),
    leaf("model route off", Change, "Routing + console", ""),
    leaf(
        "tasks add",
        Change,
        "Tasks + console",
        "`--gate` is terminal only (a gate is a shell command)",
    ),
    leaf("tasks list", Read, "Tasks + console", ""),
    leaf("tasks model", Change, "Tasks + console", ""),
    leaf("tasks schedule", Change, "Tasks + console", ""),
    leaf("tasks pause", Change, "Tasks + console", ""),
    leaf("tasks resume", Change, "Tasks + console", ""),
    leaf("tasks delete", Destructive, "Tasks + console", ""),
    leaf("tasks runs", Read, "Tasks + console", ""),
    leaf("tasks run-now", Change, "Tasks + console", ""),
    leaf("learn run", Change, "console", "`--dry-run` only reads"),
    leaf("learn show", Read, "console", ""),
    leaf("learn diff", Read, "console", ""),
    leaf("learn revert", Destructive, "console", ""),
    leaf("ledger", Read, "Usage + console", ""),
    leaf("agents list", Read, "Agents + console", ""),
    leaf("agents close", Destructive, "Agents + console", ""),
    leaf("skills disable", Change, "Extensions + console", ""),
    leaf("skills enable", Change, "Extensions + console", ""),
    leaf("hooks list", Read, "console", ""),
    leaf(
        "hooks trust",
        Refused("trusting hooks lets their commands run as you later, and it asks at the terminal"),
        "terminal only",
        "no flag skips its question, on purpose",
    ),
    leaf("hooks untrust", Change, "console", ""),
    leaf("extensions list", Read, "Extensions + console", ""),
    leaf("extensions pending", Read, "Extensions + console", ""),
    leaf("extensions approve", Change, "Extensions + console", ""),
    leaf("extensions deny", Change, "Extensions + console", ""),
    leaf("extensions remove", Destructive, "Extensions + console", ""),
    leaf("extensions resume", Change, "Extensions + console", ""),
    leaf("ssh list", Read, "console", ""),
    leaf("ssh trust", Change, "console", "needs `--fingerprint`"),
    leaf("ssh test", Read, "console", ""),
    leaf("import openclaw", Read, "console", "`--apply` changes"),
    leaf("import hermes", Read, "console", "`--apply` changes"),
    leaf("plugins add", Change, "console", ""),
    leaf("plugins list", Read, "console", ""),
    leaf("plugins remove", Destructive, "console", ""),
    leaf(
        "mcp add",
        Change,
        "console",
        "needs `-y`; `--no-sandbox` is terminal only",
    ),
    leaf("mcp list", Read, "Extensions + console", ""),
    leaf("mcp remove", Destructive, "Extensions + console", ""),
    leaf("mcp disable", Change, "Extensions + console", ""),
    leaf("mcp enable", Change, "Extensions + console", ""),
    leaf("instances list", Read, "console", ""),
    leaf(
        "instances new",
        Refused("it runs another instance's setup wizard, which asks at the terminal"),
        "terminal only",
        "",
    ),
    leaf(
        "instances remove",
        Refused("it removes another instance: run it at the terminal"),
        "terminal only",
        "",
    ),
    leaf("channels keys list", Read, "Channels (HTTP API card) + console", ""),
    leaf(
        "channels keys add",
        Refused("it prints a key once: make it on the HTTP API card, where it isn't kept in the console's output"),
        "Channels (HTTP API card)",
        "",
    ),
    leaf(
        "channels keys webhook",
        Refused("it prints a signing secret once: run it at the terminal"),
        "terminal only",
        "",
    ),
    leaf(
        "channels keys revoke",
        Destructive,
        "Channels (HTTP API card) + console",
        "",
    ),
    leaf("connections list", Read, "Connections + console", ""),
    leaf(
        "connections add",
        Refused(
            "keys and sign-ins go in on the Connections page, where a key never lands in a log",
        ),
        "Connections",
        "",
    ),
    leaf(
        "connections remove",
        Destructive,
        "Connections + console",
        "",
    ),
    leaf("connections catalog", Read, "Connections + console", ""),
    leaf(
        "connections relay deploy",
        Refused("it asks for a Cloudflare token: use the callback address card on Connections"),
        "Connections (callback address card)",
        "",
    ),
    leaf("connections relay check", Read, "Connections + console", ""),
    leaf(
        "connections setup",
        Refused(PROMPTS),
        "Connections (the checklist)",
        "the page's checklist is the same list",
    ),
    leaf(
        "eval run",
        Change,
        "Eval + console",
        "it spends money; `--dry-run` only reads",
    ),
    leaf("eval report", Read, "Eval + console", ""),
    leaf("stop", Change, "Home + console", "`--status` only reads"),
    leaf("undo", Change, "console", ""),
    leaf("trust status", Read, "console", ""),
    leaf("trust audit", Read, "console", ""),
    leaf(
        "trust caps",
        Read,
        "console",
        "`--set` needs `--yes` and confirms",
    ),
    leaf(
        "login",
        Refused("sign in from the Models page (Sign in)"),
        "Models (§2.4)",
        "",
    ),
    leaf("logout", Destructive, "Models + console", ""),
    leaf("plan list", Read, "console", ""),
    leaf("plan approve", Change, "console", ""),
    leaf("plan reject", Change, "console", ""),
    leaf(
        "sandbox",
        Refused("it runs any command: a shell by another name"),
        "terminal only",
        "",
    ),
    leaf(
        "claude-mcp",
        Refused("internal: claude speaks to it over stdio"),
        "terminal only",
        "",
    ),
];

/// Splits a line into words: single quotes are literal, double quotes and
/// bare words take backslash escapes. Anything a shell would act on is
/// refused, never passed on.
pub fn split(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("a quote (') isn't closed".into()),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c) => word.push(c),
                            None => return Err("a quote (\") isn't closed".into()),
                        },
                        Some('$') | Some('`') => return Err(refused_char('$')),
                        Some(c) => word.push(c),
                        None => return Err("a quote (\") isn't closed".into()),
                    }
                }
            }
            '\\' => {
                started = true;
                match chars.next() {
                    Some(c) => word.push(c),
                    None => return Err("the line ends in a backslash".into()),
                }
            }
            '$' | '`' | '*' | '?' | '[' | '{' | '~' | '|' | ';' | '&' | '<' | '>' | '(' | ')'
            | '\n' | '\r' => return Err(refused_char(c)),
            c => {
                started = true;
                word.push(c);
            }
        }
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

fn refused_char(c: char) -> String {
    let what = match c {
        '$' | '`' => "variables and command substitution",
        '*' | '?' | '[' | '{' | '~' => "globs and expansions",
        '|' => "pipes",
        ';' | '&' | '\n' | '\r' => "several commands on one line",
        '<' | '>' => "redirections",
        _ => "shell syntax",
    };
    format!(
        "`{}`: this is a ferrule command line, not a shell, so {what} aren't run. Quote it \
         ('…') if it's part of a value",
        c.escape_default()
    )
}

/// The words after an optional leading `ferrule`.
fn words_of(line: &str) -> Result<Vec<String>, String> {
    let mut words = split(line)?;
    if words.first().is_some_and(|w| w == "ferrule") {
        words.remove(0);
    }
    Ok(words)
}

fn given(m: &ArgMatches, id: &str) -> bool {
    m.ids().any(|i| i.as_str() == id) && m.value_source(id) == Some(ValueSource::CommandLine)
}

fn flag(m: &ArgMatches, id: &str) -> bool {
    given(m, id) && m.try_get_one::<bool>(id).ok().flatten() == Some(&true)
}

/// The leaf's path (`model route set`) and its matches.
fn leaf_of(m: &ArgMatches) -> (String, &ArgMatches) {
    let mut path = Vec::new();
    let mut at = m;
    while let Some((name, sub)) = at.subcommand() {
        path.push(name.to_string());
        at = sub;
    }
    (path.join(" "), at)
}

/// The class of a parsed line: the leaf's, then what its flags make it.
pub fn classify(path: &str, m: &ArgMatches) -> Class {
    let Some(entry) = PARITY.iter().find(|l| l.path == path) else {
        return Refused("this command isn't in the page's list yet");
    };
    match path {
        "setup" if flag(m, "refresh_service") => Change,
        "update" if flag(m, "check") => Read,
        "update" if !flag(m, "yes") => Refused(
            "it asks before installing: add --yes (the page's confirm stands for it), or --check",
        ),
        "update" => Destructive,
        "memory model download" if !flag(m, "yes") => {
            Refused("it asks before downloading: add --yes (the page's confirm stands for it)")
        }
        "model eval" if !flag(m, "yes") => {
            Refused("it asks before spending money: add --yes (the page's confirm stands for it)")
        }
        "dashboard link" if flag(m, "remote") => {
            Refused("--remote holds a terminal open: run it at the terminal")
        }
        "tasks add" if given(m, "gate") => {
            Refused("--gate is a shell command, which the page doesn't run: add the task without it, or at the terminal")
        }
        "learn run" | "eval run" if flag(m, "dry_run") => Read,
        "ssh trust" if !given(m, "fingerprint") => Refused(
            "it asks you to compare the key: add --fingerprint SHA256:… (from `ssh test`)",
        ),
        "import openclaw" | "import hermes" if flag(m, "apply") => Change,
        "mcp add" if flag(m, "no_sandbox") => Refused(
            "--no-sandbox runs the server outside the sandbox, which is a shell: terminal only",
        ),
        "mcp add" if !flag(m, "yes") => {
            Refused("it asks questions: add -y (the page's confirm stands for it)")
        }
        "stop" if flag(m, "status") => Read,
        "backup" if flag(m, "include_secrets") => Refused(
            "a file of every key shouldn't be made from a browser: run it at the terminal",
        ),
        "restore" if flag(m, "dry_run") => Read,
        "trust caps" if given(m, "set") && !flag(m, "yes") => Refused(
            "raising a cap asks first: add --yes (the page's confirm stands for it)",
        ),
        "trust caps" if given(m, "set") => Change,
        _ => entry.class,
    }
}

fn class_name(c: Class) -> &'static str {
    match c {
        Read => "read",
        Change => "change",
        Destructive => "destructive",
        Refused(_) => "refused",
    }
}

/// What a line would do, without running it: `Ok((words, path, class))`,
/// or the text to show (help, or clap's error) and its exit code.
pub enum Parsed {
    Command {
        words: Vec<String>,
        path: String,
        class: Class,
    },
    Shown {
        text: String,
        code: i32,
    },
}

pub fn parse(line: &str) -> Result<Parsed, String> {
    let words = words_of(line)?;
    if words.is_empty() {
        return Err("type a ferrule command, like `doctor` or `model list`".into());
    }
    for w in &words {
        if w == "--config" || w.starts_with("--config=") {
            return Err("--config: the page runs commands on the service's own config only".into());
        }
        if w == "--instance" || w.starts_with("--instance=") {
            return Err("--instance: the page runs commands on its own instance only".into());
        }
        if w == "--workspace" || w.starts_with("--workspace=") {
            return Err(
                "--workspace: the page runs commands in the service's own workspace".into(),
            );
        }
    }
    let cmd = crate::Cli::command().color(clap::ColorChoice::Never);
    let argv = std::iter::once("ferrule".to_string()).chain(words.iter().cloned());
    match cmd.try_get_matches_from(argv) {
        Ok(m) => {
            let (path, leaf) = leaf_of(&m);
            let class = classify(&path, leaf);
            Ok(Parsed::Command { words, path, class })
        }
        Err(e) => {
            use clap::error::ErrorKind::*;
            let code = match e.kind() {
                DisplayHelp | DisplayVersion | DisplayHelpOnMissingArgumentOrSubcommand => 0,
                _ => 2,
            };
            Ok(Parsed::Shown {
                text: e.render().to_string(),
                code,
            })
        }
    }
}

fn audit(ctx: &Ctx, event: &str, detail: Value) {
    if let Some(hub) = &ctx.hub {
        hub.audit()
            .record(chrono::Utc::now(), event, None, None, detail);
    }
}

/// `POST /api/console/run {line, confirm?}`.
pub fn run(ctx: &Ctx, body: &Value) -> Answer {
    let line = body["line"].as_str().unwrap_or("").trim().to_string();
    let parsed = match parse(&line) {
        Ok(p) => p,
        Err(e) => {
            audit(
                ctx,
                "console_refused",
                json!({ "by": "dashboard", "line": line, "why": e }),
            );
            return bad(400, e);
        }
    };
    let (words, path, class) = match parsed {
        Parsed::Shown { text, code } => {
            let run = ctx.runs.finished(format!("ferrule {line}"), &text, code);
            return ok(json!({ "ok": true, "class": "read", "job": run.view(0) }));
        }
        Parsed::Command { words, path, class } => (words, path, class),
    };
    if let Refused(why) = class {
        audit(
            ctx,
            "console_refused",
            json!({ "by": "dashboard", "line": line, "why": why }),
        );
        let hint = PARITY
            .iter()
            .find(|l| l.path == path)
            .map(|l| l.page)
            .unwrap_or("");
        return Some((
            403,
            json!({ "error": format!("`{path}` doesn't run from the page: {why}"), "class": "refused", "page": hint }),
        ));
    }
    if matches!(class, Change | Destructive) && body["confirm"].as_bool() != Some(true) {
        return Some((
            409,
            json!({
                "confirm": format!("Run `ferrule {}`?", words.join(" ")),
                "class": class_name(class),
            }),
        ));
    }
    if ctx.runs.busy() {
        return bad(409, "a command is still running: wait for it, or stop it");
    }
    let args: Vec<OsString> = words.iter().map(OsString::from).collect();
    let mut env = super::api::child_env(ctx);
    if ctx.config_path.is_none() {
        // No config was loaded: the child must not go looking for one of
        // the caller's.
        env.retain(|(k, _)| k != "FERRULE_CONFIG");
    }
    let label = format!("ferrule {}", words.join(" "));
    audit(
        ctx,
        "console_run",
        json!({ "by": "dashboard", "line": label, "class": class_name(class) }),
    );
    let hub = ctx.hub.clone();
    let done_label = label.clone();
    let class_s = class_name(class);
    match ctx.runs.start_then(label, args, env, move |run| {
        if let Some(hub) = hub {
            hub.audit().record(
                chrono::Utc::now(),
                "console_done",
                None,
                None,
                json!({
                    "by": "dashboard",
                    "line": done_label,
                    "class": class_s,
                    "code": run.code(),
                    "secs": run.started.elapsed().map_or(0, |d| d.as_secs()),
                }),
            );
        }
    }) {
        Ok(run) => ok(json!({ "ok": true, "class": class_s, "job": run.view(0) })),
        Err(e) => bad(500, format!("couldn't start ferrule: {e}")),
    }
}

/// `GET /api/console/job?id=&from=N`.
pub fn job(ctx: &Ctx, req: &Request) -> Answer {
    let id = req.query.get("id").map(String::as_str).unwrap_or("");
    let from = req
        .query
        .get("from")
        .and_then(|f| f.parse().ok())
        .unwrap_or(0usize);
    match ctx.runs.get(id) {
        Some(run) => ok(run.view(from)),
        None => bad(404, "no such job: the page keeps the last 20"),
    }
}

/// `POST /api/console/cancel {id}`.
pub fn cancel(ctx: &Ctx, body: &Value) -> Answer {
    let id = body["id"].as_str().unwrap_or("");
    if ctx.runs.cancel(id) {
        ok(json!({ "ok": true }))
    } else {
        bad(400, "that job isn't running")
    }
}

/// `GET /api/console/complete?line=`: what can follow, from the clap tree,
/// and model references or services where an argument takes one.
pub fn complete(ctx: &Ctx, req: &Request) -> Answer {
    let line = req.query.get("line").cloned().unwrap_or_default();
    let ends_in_space = line.ends_with(' ') || line.is_empty();
    let mut words = match words_of(&line) {
        Ok(w) => w,
        Err(_) => return ok(json!({ "ok": true, "items": [] })),
    };
    let partial = if ends_in_space {
        String::new()
    } else {
        words.pop().unwrap_or_default()
    };
    let root = crate::Cli::command();
    let mut at = &root;
    let mut path = Vec::new();
    for w in &words {
        match at.find_subcommand(w) {
            Some(sub) => {
                path.push(w.clone());
                at = sub;
            }
            None => break,
        }
    }
    let mut items = Vec::new();
    for sub in at.get_subcommands() {
        let name = sub.get_name();
        if sub.is_hide_set() || name == "help" || !name.starts_with(&partial) {
            continue;
        }
        items.push(json!({
            "word": name,
            "help": sub.get_about().map(|a| a.to_string()).unwrap_or_default(),
            "kind": "command",
        }));
    }
    if partial.starts_with('-') || (at.get_subcommands().next().is_none() && partial.is_empty()) {
        for a in at.get_arguments() {
            let Some(long) = a.get_long() else { continue };
            let word = format!("--{long}");
            if a.is_hide_set() || long == "config" || long == "workspace" {
                continue;
            }
            if word.starts_with(&partial) {
                items.push(json!({
                    "word": word,
                    "help": a.get_help().map(|h| h.to_string()).unwrap_or_default(),
                    "kind": "flag",
                }));
            }
        }
    }
    let path = path.join(" ");
    let values: Vec<String> = match path.as_str() {
        "model default" | "model test" | "model remove" | "model pin" | "model unpin"
        | "model fallback" | "tasks model" | "model route set" => ctx
            .models
            .as_ref()
            .map(|m| m.view().models.into_iter().map(|r| r.reference).collect())
            .unwrap_or_default(),
        "connections remove" | "connections setup" => ctx
            .connections
            .as_ref()
            .map(|c| c.catalog().names())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    if !partial.starts_with('-') {
        for v in values
            .into_iter()
            .filter(|v| v.starts_with(&partial))
            .take(50)
        {
            items.push(json!({ "word": v, "help": "", "kind": "value" }));
        }
    }
    let class = PARITY
        .iter()
        .find(|l| l.path == path)
        .map(|l| class_name(l.class));
    ok(json!({ "ok": true, "path": path, "class": class, "items": items }))
}

/// `GET /api/console/parity`: the matrix, for the page's help.
pub fn parity() -> Answer {
    let rows: Vec<Value> = PARITY
        .iter()
        .map(|l| {
            json!({
                "command": l.path,
                "class": class_name(l.class),
                "page": l.page,
                "note": match l.class { Refused(why) if l.note.is_empty() => why, _ => l.note },
            })
        })
        .collect();
    ok(json!({ "ok": true, "rows": rows }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(cmd: &clap::Command, prefix: &str, out: &mut Vec<String>) {
        let subs: Vec<_> = cmd
            .get_subcommands()
            .filter(|s| s.get_name() != "help")
            .collect();
        if subs.is_empty() && !prefix.is_empty() {
            out.push(prefix.to_string());
        }
        for s in subs {
            let path = if prefix.is_empty() {
                s.get_name().to_string()
            } else {
                format!("{prefix} {}", s.get_name())
            };
            leaves(s, &path, out);
        }
    }

    #[test]
    fn every_subcommand_has_a_place_on_the_page_or_a_reason() {
        let mut all = Vec::new();
        leaves(&crate::Cli::command(), "", &mut all);
        let missing: Vec<_> = all
            .iter()
            .filter(|p| !PARITY.iter().any(|l| l.path == p.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "give these a row in dashboard/console.rs PARITY (and docs/m37-control-room.md §4.1): {missing:?}"
        );
        let stale: Vec<_> = PARITY
            .iter()
            .filter(|l| !all.iter().any(|p| p == l.path))
            .map(|l| l.path)
            .collect();
        assert!(stale.is_empty(), "no such subcommand any more: {stale:?}");
        let doc = include_str!("../../../../docs/m37-control-room.md");
        for p in &all {
            assert!(
                doc.contains(&format!("| `{p}` |")),
                "docs/m37-control-room.md §4.1 has no row for `{p}`"
            );
        }
    }

    #[test]
    fn the_line_is_split_without_a_shell() {
        assert_eq!(
            split(r#"memory add "it's a \"note\"" --tags 'a b' x\ y"#).unwrap(),
            vec!["memory", "add", r#"it's a "note""#, "--tags", "a b", "x y"]
        );
        assert_eq!(split("  doctor   ").unwrap(), vec!["doctor"]);
        assert_eq!(split("run ''").unwrap(), vec!["run", ""]);
        for bad in [
            "doctor; rm -rf /",
            "doctor && x",
            "doctor | tee f",
            "doctor > f",
            "doctor < f",
            "run $HOME",
            "run \"$(id)\"",
            "run `id`",
            "memory search *",
            "run ~/x",
            "doctor &",
            "run 'open",
            "run \"open",
            "doctor\nstatus",
        ] {
            assert!(split(bad).is_err(), "{bad:?} should be refused");
        }
        // Quoted, a glob is a value.
        assert_eq!(
            split("memory search '*'").unwrap(),
            vec!["memory", "search", "*"]
        );
    }

    fn class(line: &str) -> Class {
        match parse(line).unwrap() {
            Parsed::Command { class, .. } => class,
            Parsed::Shown { text, .. } => panic!("{line}: {text}"),
        }
    }

    #[test]
    fn commands_are_classed_and_prompting_ones_need_their_flag() {
        assert_eq!(class("doctor"), Read);
        assert_eq!(class("ferrule model list"), Read);
        assert_eq!(class("update --check"), Read);
        assert!(matches!(class("update"), Refused(w) if w.contains("--yes")));
        assert_eq!(class("update --yes"), Destructive);
        assert_eq!(class("model default a/b"), Change);
        assert_eq!(class("tasks delete 7"), Destructive);
        assert!(matches!(class("setup"), Refused(_)));
        assert_eq!(class("setup --refresh-service"), Change);
        assert!(matches!(class("sandbox -- sh"), Refused(_)));
        assert!(matches!(class("gateway"), Refused(_)));
        assert!(matches!(class("chat"), Refused(_)));
        assert!(matches!(class("config edit"), Refused(_)));
        assert_eq!(class("backup"), Change);
        assert!(matches!(class("backup --include-secrets"), Refused(_)));
        assert!(matches!(class("restore b.tar.gz"), Refused(_)));
        assert_eq!(class("restore --dry-run b.tar.gz"), Read);
        assert!(
            matches!(class("mcp add x --no-sandbox -y -- node s.js"), Refused(w) if w.contains("sandbox"))
        );
        assert!(matches!(class("mcp add x -- node s.js"), Refused(w) if w.contains("-y")));
        assert_eq!(class("mcp add x -y -- node s.js"), Change);
        assert!(matches!(class("ssh trust host"), Refused(w) if w.contains("--fingerprint")));
        assert!(matches!(
            class("tasks add n --kind cron --schedule '0 9 * * *' --channel local --chat-id x --prompt p --gate true"),
            Refused(w) if w.contains("--gate")
        ));
        assert_eq!(class("trust caps"), Read);
        assert!(matches!(
            class("trust caps --set max_usd_per_day=10"),
            Refused(_)
        ));
        assert_eq!(class("trust caps --set max_usd_per_day=10 --yes"), Change);
        assert_eq!(class("stop --status"), Read);
        assert_eq!(class("stop"), Change);
        assert!(matches!(class("dashboard link --remote"), Refused(_)));
        assert!(parse("doctor --config /etc/x.toml").is_err());
        assert!(parse("doctor --config=/etc/x.toml").is_err());
        assert!(parse("doctor --instance work").is_err());
        assert!(parse("--instance=work doctor").is_err());
        assert!(parse("undo --workspace /").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn help_and_mistakes_are_answered_in_process() {
        match parse("model --help").unwrap() {
            Parsed::Shown { text, code } => {
                assert_eq!(code, 0);
                assert!(text.contains("Usage"), "{text}");
                assert!(!text.contains('\x1b'));
            }
            _ => panic!("help should be shown"),
        }
        match parse("modle list").unwrap() {
            Parsed::Shown { code, .. } => assert_eq!(code, 2),
            _ => panic!("a typo is clap's to explain"),
        }
    }
}
