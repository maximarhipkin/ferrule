//! `ferrule doctor`: one pass over everything setup configures — the
//! config, the saved keys, each provider, Telegram, the sandbox, the
//! background service, the binary itself and the browser — with a line per check and what to do about each problem. Exits
//! non-zero if anything is broken.

use crate::setup::tilde;
use crate::{browser, config, probe, secrets, service};
use anyhow::Result;
use ferrule_mcp::McpServerConfig;
use ferrule_sandbox::{Backend, Mode, Sandbox};
use ferrule_tools::search::SearchProvider;
use std::fmt::Display;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Level {
    Ok,
    Note,
    Warn,
    Fail,
}

struct Report {
    color: bool,
    warnings: usize,
    failures: usize,
}

impl Report {
    fn new() -> Self {
        use std::io::IsTerminal;
        let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        Self {
            color,
            warnings: 0,
            failures: 0,
        }
    }

    fn line(&mut self, level: Level, what: &str, text: impl Display) {
        let (mark, ansi) = match level {
            Level::Ok => ("✓", "32"),
            Level::Note => ("·", "2"),
            Level::Warn => {
                self.warnings += 1;
                ("!", "33")
            }
            Level::Fail => {
                self.failures += 1;
                ("✗", "31")
            }
        };
        let mark = if self.color {
            format!("\x1b[{ansi}m{mark}\x1b[0m")
        } else {
            mark.to_string()
        };
        println!("  {mark} {what:<9} {text}");
    }

    fn ok(&mut self, what: &str, text: impl Display) {
        self.line(Level::Ok, what, text);
    }

    fn note(&mut self, what: &str, text: impl Display) {
        self.line(Level::Note, what, text);
    }

    fn warn(&mut self, what: &str, text: impl Display) {
        self.line(Level::Warn, what, text);
    }

    fn fail(&mut self, what: &str, text: impl Display) {
        self.line(Level::Fail, what, text);
    }

    /// A follow-up line under the last check: what to do about it.
    fn hint(&self, text: impl Display) {
        println!("  {:<11} → {text}", "");
    }

    fn finish(self) -> bool {
        println!();
        let n = |count: usize, one: &str, many: &str| {
            format!("{count} {}", if count == 1 { one } else { many })
        };
        match (self.warnings, self.failures) {
            (0, 0) => println!("All good."),
            (w, 0) => println!("{}, nothing broken.", n(w, "warning", "warnings")),
            (w, f) => println!(
                "{}, {}: fix the ✗ lines, then run `ferrule doctor` again.",
                n(f, "problem", "problems"),
                n(w, "warning", "warnings")
            ),
        }
        self.failures == 0
    }
}

pub async fn run(offline: bool, ping_models: bool) -> Result<bool> {
    let mut r = Report::new();
    println!(
        "ferrule doctor · v{}{}\n",
        env!("CARGO_PKG_VERSION"),
        if offline { " · offline" } else { "" }
    );
    let (cfg, path) = match config::Config::load() {
        Ok(loaded) => loaded,
        Err(e) => {
            r.fail("config", format!("{e:#}"));
            if config::config_path()?.is_none() {
                r.hint("run `ferrule setup`");
            }
            binary(&mut r);
            browser_check(&mut r, None);
            return Ok(r.finish());
        }
    };
    r.ok("config", tilde(&path));
    let secrets_path = secrets::path()?;
    keys(&mut r, &secrets_path);
    let http = probe::client();
    providers(&mut r, &cfg, &http, offline).await;
    models_check(&mut r, ping_models).await;
    let telegram_on = telegram(&mut r, &cfg, &http, offline).await;
    let backend = sandbox(&mut r, &cfg, &secrets_path);
    let confined = backend != Backend::None;
    mcp(&mut r, &cfg, backend);
    proxy(&mut r, &cfg);
    web_search_check(&mut r, &cfg, &http, offline).await;
    memory_check(&mut r, &cfg);
    agents_check(&mut r, &cfg, confined);
    hooks_check(&mut r, &cfg, &path);
    trust_check(&mut r, &cfg, telegram_on);
    service_check(&mut r, &path, telegram_on)?;
    health_check(&mut r, &cfg, telegram_on);
    connections_check(&mut r, &cfg);
    editing_check(&mut r, &cfg);
    binary(&mut r);
    browser_check(&mut r, Some(&cfg));
    Ok(r.finish())
}

/// M29: the edit tools, the repo map and the grammars this build has.
fn editing_check(r: &mut Report, cfg: &config::Config) {
    let tools = if cfg.agent.edit_file {
        "edit_file + write_file"
    } else {
        "write_file only (edit_file = false)"
    };
    let langs = ferrule_codemap::languages();
    let map = match cfg.agent.repo_map_tokens {
        0 => "repo map off".to_string(),
        n => format!("repo map {n} tokens in code repos"),
    };
    if langs.is_empty() {
        r.note(
            "editing",
            format!("{tools} · built without grammars: code_search is text search, no repo map"),
        );
    } else {
        r.ok("editing", format!("{tools} · {map} · {}", langs.join(", ")));
    }
    if cfg.agent.lint == config::LintMode::Off {
        r.note("lint", "off (lint = \"off\")");
        return;
    }
    let (found, missing): (Vec<_>, Vec<_>) = ferrule_hooks::lint::LINTERS
        .iter()
        .partition(|(name, _)| ferrule_hooks::lint::on_path(name));
    let names = |l: &[&(&str, &str)]| {
        l.iter()
            .map(|(n, ext)| format!("{n} ({ext})"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut line = format!(
        "after each edit, where the project configures it · found: {}",
        {
            let f = names(&found);
            if f.is_empty() {
                "none".to_string()
            } else {
                f
            }
        }
    );
    if !missing.is_empty() {
        line.push_str(&format!(" · not on PATH: {}", names(&missing)));
    }
    r.ok("lint", line);
    if missing.iter().any(|(n, _)| matches!(*n, "eslint" | "tsc")) {
        r.hint("eslint and tsc are also found in a project's node_modules/.bin");
    }
}

/// M20: what's connected and whether logins have a relay to come back by.
fn connections_check(r: &mut Report, cfg: &config::Config) {
    r.ok("connect", crate::connections::doctor_line(cfg));
}

/// M18: hooks run as the owner, outside the sandbox; say which will, and
/// which of this directory's won't.
fn hooks_check(r: &mut Report, cfg: &config::Config, path: &Path) {
    let settings = match crate::hooks_cli::settings(cfg, path) {
        Ok(s) => s,
        Err(e) => return r.fail("hooks", format!("{e:#}")),
    };
    let n = settings.entries().len();
    if n > 0 {
        r.ok(
            "hooks",
            format!(
                "{n} hook{} from your config · they run as you, outside the sandbox",
                if n == 1 { "" } else { "s" }
            ),
        );
    }
    let Ok(data) = config::data_dir() else {
        return;
    };
    let here = std::env::current_dir().unwrap_or_default();
    let state = ferrule_hooks::load_workspace(
        &here,
        settings.project,
        &ferrule_hooks::TrustStore::in_data_dir(&data),
    );
    match &state {
        ferrule_hooks::WorkspaceState::Trusted(ws) => r.ok(
            "hooks",
            format!("{} trusted hook(s) in {}", ws.entries().len(), tilde(&here)),
        ),
        ferrule_hooks::WorkspaceState::Untrusted { .. } => {
            r.warn("hooks", state.notice(&here).unwrap_or_default())
        }
        ferrule_hooks::WorkspaceState::Absent => {}
    }
}

fn keys(r: &mut Report, path: &Path) {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            r.note("keys", "no saved keys (setup keeps them in a private file)");
            return;
        }
        Err(e) => {
            r.fail("keys", format!("can't read {}: {e}", tilde(path)));
            return;
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let loose = |mode: u32| mode & 0o077 != 0;
        let dir_mode = path
            .parent()
            .and_then(|dir| std::fs::metadata(dir).ok())
            .map(|m| m.permissions().mode());
        if loose(meta.permissions().mode()) || dir_mode.is_some_and(loose) {
            r.warn(
                "keys",
                format!("{} can be read by other users", tilde(path)),
            );
            r.hint(format!(
                "chmod 700 {} && chmod 600 {}",
                tilde(path.parent().unwrap_or(path)),
                tilde(path)
            ));
            return;
        }
    }
    let _ = meta;
    r.ok("keys", format!("{} (private)", tilde(path)));
}

fn key(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

async fn providers(r: &mut Report, cfg: &config::Config, http: &reqwest::Client, offline: bool) {
    if cfg.providers.is_empty() {
        r.fail("provider", "none set up");
        r.hint("`ferrule setup` → Model provider");
        return;
    }
    let default = cfg.default_provider.as_deref();
    match default {
        None => {
            r.fail("provider", "no default_provider set");
            r.hint("`ferrule setup` → Model provider → Make it the default");
        }
        Some(name) if !cfg.providers.contains_key(name) => {
            r.fail(
                "provider",
                format!("default_provider is `{name}`, which isn't under [providers]"),
            );
            r.hint("`ferrule setup` → Model provider");
        }
        Some(_) => {}
    }
    let mut names: Vec<&String> = cfg.providers.keys().collect();
    names.sort_by_key(|name| (Some(name.as_str()) != default, name.as_str()));
    for name in names {
        let p = &cfg.providers[name];
        let is_default = Some(name.as_str()) == default;
        // M23: which driver talks to it, and whether the config says so.
        let label = format!(
            "{name}{} · {} · {} api ({})",
            if is_default { " (default)" } else { "" },
            p.model,
            p.api(),
            if p.api.is_some() { "set" } else { "inferred" }
        );
        let broken = |r: &mut Report, text: String| {
            if is_default {
                r.fail("provider", text)
            } else {
                r.warn("provider", text)
            }
        };
        let Some(value) = key(&p.api_key_env) else {
            broken(r, format!("{label}: no key (${} isn't set)", p.api_key_env));
            r.hint(format!(
                "`ferrule setup` → Model provider → {name} → Replace the API key"
            ));
            continue;
        };
        let from = match secrets::source(&p.api_key_env) {
            secrets::Source::Env => "key from your shell",
            _ => "saved key",
        };
        if offline {
            r.ok("provider", format!("{label} · {from}, not checked"));
            free_model(r, &label, &p.model);
            continue;
        }
        free_model(r, &label, &p.model);
        match probe::models(http, &p.base_url, &value, p.profile == "anthropic").await {
            Ok(models) if models.is_empty() || models.contains(&p.model) => {
                r.ok("provider", format!("{label} · {from} works"));
            }
            Ok(_) => {
                r.warn(
                    "provider",
                    format!(
                        "{label}: the key works, but the provider doesn't list `{}`",
                        p.model
                    ),
                );
                r.hint(format!(
                    "if calls fail, `ferrule setup` → Model provider → {name} → Change the model"
                ));
            }
            Err(probe::Check::Rejected(why)) => {
                broken(r, format!("{label}: {why} ({from}, ${})", p.api_key_env));
                r.hint(format!(
                    "`ferrule setup` → Model provider → {name} → Replace the API key"
                ));
            }
            Err(e) => r.warn("provider", format!("{label}: couldn't check the key: {e}")),
        }
    }
}

/// M19c: an OpenRouter `:free` model id is the usual reason a bot "runs
/// but doesn't answer".
fn free_model(r: &mut Report, label: &str, model: &str) {
    let Some(paid) = model.strip_suffix(":free") else {
        return;
    };
    r.warn(
        "provider",
        format!("{label}: a `:free` model draws on OpenRouter's shared free pool — it's rate-limited (HTTP 429) at busy times and often has no endpoint that supports tools (HTTP 404), which ferrule needs"),
    );
    r.hint(format!(
        "use the paid id `{paid}`, or set `[models] fallback` so another model answers when this one can't"
    ));
}

/// M21: the models beyond each provider's own (a key for each), the
/// default and fallback when set, anything they name that's gone, and with
/// `--ping-models` one real call to each.
async fn models_check(r: &mut Report, ping: bool) {
    let Ok(models) = crate::models::shared() else {
        return;
    };
    let view = models.view();
    let extra = view.models.iter().filter(|m| !m.primary).count();
    // A config with one model per provider and no [models]: the provider
    // lines above said it all.
    let in_use = extra > 0 || !view.fallback.is_empty() || !view.default_set.is_empty();
    if in_use {
        r.ok(
            "models",
            format!(
                "{} connected · default {} · fallback {}",
                view.models.len(),
                view.default.as_deref().unwrap_or("none"),
                if view.fallback.is_empty() {
                    "off".to_string()
                } else {
                    view.fallback.join(" → ")
                }
            ),
        );
    }
    for m in view.models.iter().filter(|m| !m.primary) {
        if let Some(paid) = m.reference.strip_suffix(":free") {
            r.warn(
                "models",
                format!(
                    "{}: a `:free` model is rate-limited at busy times and often can't use tools",
                    m.reference
                ),
            );
            r.hint(format!(
                "use `{paid}` instead, or keep it out of the default and fallback"
            ));
        }
    }
    for m in view.models.iter().filter(|m| !m.primary && !m.key_present) {
        let text = format!("{}: no key (${} isn't set)", m.reference, m.key_env);
        if m.default {
            r.fail("models", text)
        } else {
            r.warn("models", text)
        }
    }
    // A missing key was just said (or, for a provider's own model, above).
    for p in view
        .problems
        .iter()
        .filter(|p| in_use && !p.starts_with("no key:") && !view.routing.problems.contains(p))
    {
        r.fail("models", p);
        r.hint("`ferrule model list` shows what's connected; `ferrule model default <ref>` fixes the default");
    }
    routing_check(r, &view.routing);
    let unpriced = crate::models::catalog::unpriced(&models.catalog());
    for u in &unpriced {
        r.warn("models", u);
    }
    if !unpriced.is_empty() {
        r.hint("`ferrule model fill-prices` fills them from the provider catalogs");
    }
    if !ping {
        return;
    }
    for m in &view.models {
        let out = models.test(&m.reference).await;
        if out.ok {
            r.ok("models", format!("{} · {}", out.reference, out.said));
        } else {
            r.warn("models", format!("{}: {}", out.reference, out.said));
        }
    }
}

/// M25: routing on or off, and what would stop a turn from climbing.
fn routing_check(r: &mut Report, v: &crate::models::routing_admin::RoutingView) {
    if !v.enabled {
        return;
    }
    if v.on {
        let names: Vec<&str> = v.tiers.iter().map(|t| t.name.as_str()).collect();
        let cap = match v.strong_daily_usd {
            Some(cap) => format!(
                " · ${:.2} of ${cap:.2} above the cheap tier today",
                v.strong_spent_today
            ),
            None => String::new(),
        };
        r.ok("routing", format!("on · {}{cap}", names.join(" → ")));
    }
    for p in &v.problems {
        r.fail("routing", p);
    }
    if !v.problems.is_empty() {
        r.hint("`ferrule model route` shows the tiers; `ferrule model route set <cheap> <strong>` fixes them, `ferrule model route off` turns routing off");
    }
    for t in v.tiers.iter().filter(|t| !t.key_present) {
        r.warn(
            "routing",
            format!(
                "tier {} ({}): no key (${} isn't set), so a turn can't climb to it",
                t.name, t.reference, t.key_env
            ),
        );
    }
}

/// Returns whether Telegram is configured.
async fn telegram(
    r: &mut Report,
    cfg: &config::Config,
    http: &reqwest::Client,
    offline: bool,
) -> bool {
    let Some(env) = &cfg.gateway.telegram_token_env else {
        r.note("telegram", "off");
        return false;
    };
    let Some(token) = key(env) else {
        r.fail("telegram", format!("no bot token (${env} isn't set)"));
        r.hint("`ferrule setup` → Telegram → Replace the bot token");
        return true;
    };
    let chats = cfg.gateway.telegram_allowed_chats.len();
    let allowed = format!("{chats} chat{} allowed", if chats == 1 { "" } else { "s" });
    if offline {
        r.ok("telegram", format!("token set, not checked · {allowed}"));
    } else {
        let tg = probe::Telegram {
            http,
            base_url: &cfg.gateway.telegram_base_url,
            token: &token,
        };
        match tg.get_me().await {
            Ok(bot) => {
                r.ok("telegram", format!("@{bot} · {allowed}"));
                match tg.webhook().await {
                    Ok(Some(url)) => {
                        // Only the host: a webhook's path is often its secret.
                        let host = reqwest::Url::parse(&url)
                            .ok()
                            .and_then(|u| u.host_str().map(str::to_string))
                            .unwrap_or_else(|| "somewhere".into());
                        r.warn(
                            "telegram",
                            format!("the bot has a webhook set (to {host}), so Telegram sends its messages there, not to the gateway"),
                        );
                        r.hint("the gateway removes it when it starts, keeping waiting messages; or `ferrule setup` → Telegram removes it now");
                    }
                    Ok(None) => {}
                    Err(e) => r.warn("telegram", format!("couldn't check for a webhook: {e}")),
                }
            }
            Err(probe::Check::Rejected(why)) => {
                r.fail("telegram", why);
                r.hint("`ferrule setup` → Telegram → Replace the bot token");
            }
            Err(e) => r.warn("telegram", format!("couldn't reach Telegram: {e}")),
        }
    }
    if chats == 0 {
        r.warn(
            "telegram",
            "no chat is allowed, so the bot answers no one (it tells them their chat id)",
        );
        r.hint("`ferrule setup` → Telegram → Allow another chat");
    }
    true
}

/// The backend commands run confined by, `None` if they don't.
fn sandbox(r: &mut Report, cfg: &config::Config, secrets_path: &Path) -> Backend {
    let sandbox = match Sandbox::new(crate::sandbox_policy(cfg)) {
        Ok(sandbox) => sandbox,
        Err(e) => {
            r.fail("sandbox", e);
            r.hint("`ferrule setup` → Sandbox, or `ferrule sandbox` for details");
            return Backend::None;
        }
    };
    if !sandbox.is_active() {
        let why = match (cfg.sandbox.mode, sandbox.degraded()) {
            (Mode::Off, _) => "mode = off".to_string(),
            (_, Some(why)) => why.to_string(),
            (_, None) => "no sandbox on this system".to_string(),
        };
        r.warn("sandbox", format!("shell commands run unsandboxed: {why}"));
        if cfg!(windows) && cfg.sandbox.mode != Mode::Off {
            let shell = ferrule_sandbox::Shell::get();
            r.hint(match shell.kind {
                ferrule_sandbox::ShellKind::Posix => format!(
                    "{} can't run under the restricted token; set {}=powershell to run \
                     commands sandboxed in PowerShell (docs/windows-sandbox.md)",
                    shell.name,
                    ferrule_sandbox::SHELL_VAR
                ),
                ferrule_sandbox::ShellKind::PowerShell => format!(
                    "{} couldn't run under the restricted token; docs/windows-sandbox.md",
                    shell.name
                ),
            });
        }
        return Backend::None;
    }
    let mode = match cfg.sandbox.mode {
        Mode::Off => "off",
        Mode::ReadOnly => "read-only",
        Mode::WorkspaceWrite => "workspace-write",
    };
    let network = if cfg.sandbox.network {
        "network on"
    } else {
        "network off"
    };
    r.ok(
        "sandbox",
        format!("{} · {mode} · {network}", sandbox.backend()),
    );
    if sandbox.backend() == Backend::Windows {
        if !cfg.sandbox.network {
            r.warn(
                "sandbox",
                "network = false isn't enforced on Windows: commands are only told not to use it",
            );
        }
        if cfg.sandbox.deny_default_reads {
            r.note(
                "sandbox",
                "~/.ssh, cloud credential dirs and browser profiles: only the file tools refuse \
                 them on Windows; add a path to deny_read to close it to commands too",
            );
        }
    }
    if secrets_path.exists() {
        let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let (program, args): (&str, Vec<&std::ffi::OsStr>) = if cfg!(windows) {
            let args = ["/d".as_ref(), "/c".as_ref(), "type".as_ref()];
            ("cmd.exe", [&args[..], &[secrets_path.as_os_str()]].concat())
        } else {
            let args = ["-c".as_ref(), "cat \"$1\"".as_ref(), "sh".as_ref()];
            ("/bin/sh", [&args[..], &[secrets_path.as_os_str()]].concat())
        };
        let read = sandbox
            .command(program, args, &workspace)
            .and_then(|mut cmd| {
                cmd.stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
            });
        match read {
            Ok(status) if status.success() => {
                r.fail("sandbox", "commands the agent runs can read the saved keys");
                r.hint("`ferrule sandbox` shows what's wrong");
            }
            Ok(_) => r.ok("sandbox", "the saved keys are out of the agent's reach"),
            Err(e) => r.warn("sandbox", format!("couldn't run a sandboxed check: {e}")),
        }
    }
    sandbox.backend()
}

/// MCP servers that run outside the sandbox. Only the config is read: the
/// servers themselves aren't started. `backend` is what confines commands.
fn mcp(r: &mut Report, cfg: &config::Config, backend: Backend) {
    let confined = backend != Backend::None;
    let all = &cfg.mcp.servers;
    if all.is_empty() {
        return;
    }
    // A header's `${NAME}` with no `[secrets]` entry goes out as the real
    // value, unproxied, or as nothing.
    for s in all {
        for name in crate::mcp_config::secret_refs(s) {
            if ferrule_sandbox::looks_secret(&name) && !cfg.secrets.contains_key(&name) {
                r.warn(
                    "mcp",
                    format!("`{}` sends ${{{name}}}, which isn't in [secrets]", s.name),
                );
                r.hint(format!(
                    "bind it to the server's host: `ferrule mcp add {} --replace --secret {name} …`, or [secrets] in the config",
                    s.name
                ));
            }
        }
    }
    let names = |v: &[&McpServerConfig]| {
        v.iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    // Servers reached by URL aren't processes of ours: nothing to confine.
    let remote: Vec<_> = all.iter().filter(|s| s.url.is_some()).collect();
    if !remote.is_empty() {
        r.ok("mcp", format!("by URL: {}", names(&remote)));
    }
    let servers: Vec<_> = all.iter().filter(|s| s.url.is_none()).collect();
    let open: Vec<_> = servers.iter().copied().filter(|s| !s.sandbox).collect();
    for s in &open {
        r.warn(
            "mcp",
            format!(
                "`{}` has sandbox = false: {}",
                s.name,
                unconfined_gaps(backend)
            ),
        );
    }
    if !open.is_empty() {
        r.hint("remove `sandbox = false`, and give it `writable_roots` instead if it needs them");
    }
    let rest: Vec<_> = servers.iter().copied().filter(|s| s.sandbox).collect();
    match (rest.is_empty(), confined) {
        (true, _) => {}
        (false, true) => r.ok("mcp", format!("sandboxed: {}", names(&rest))),
        (false, false) => r.warn(
            "mcp",
            format!("unsandboxed, like shell commands: {}", names(&rest)),
        ),
    }
}

/// What a `sandbox = false` server can still do: with a backend it runs
/// hide-only (`Sandbox::unconfined`), without one it's fully open.
fn unconfined_gaps(backend: Backend) -> &'static str {
    match backend {
        Backend::None => "it can write anywhere you can and read the saved keys",
        Backend::Windows => {
            "it can write anywhere you can and use the network, and read ~/.ssh, cloud \
             credentials and browser profiles; the saved keys and deny_read stay shut"
        }
        _ => {
            "it can write anywhere you can (not beside a denied path) and use the network; \
             the saved keys and the read denies stay shut"
        }
    }
}

/// Sub-agents: on or off, the tree's limits, the role providers, and any
/// agents a crash left interrupted.
fn agents_check(r: &mut Report, cfg: &config::Config, confined: bool) {
    let a = &cfg.agents;
    if !a.enabled {
        r.note("agents", "off ([agents] enabled = false)");
        return;
    }
    if let Err(e) = crate::agents::check_roles(cfg) {
        r.fail("agents", format!("{e:#}"));
        return;
    }
    let mut roles: Vec<String> = a
        .roles
        .iter()
        .filter_map(|(role, rc)| rc.provider.as_ref().map(|p| format!("{role} → {p}")))
        .collect();
    roles.sort();
    let roles = if roles.is_empty() {
        String::new()
    } else {
        format!(" · {}", roles.join(", "))
    };
    r.ok(
        "agents",
        format!(
            "on · depth {} · {} at once per parent, {} per tree · {} tokens per {}h{roles}",
            a.max_depth, a.max_children, a.max_agents, a.max_tokens, a.budget_window_hours
        ),
    );
    if !confined {
        r.note(
            "agents",
            "a read-only verifier rests on its tool set and prompt: shell commands aren't confined",
        );
    }
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        r.warn(
            "agents",
            "git isn't on your PATH: children share their parent's workspace instead of a worktree",
        );
    }
    // Running in no live process counts too: the next ferrule to start
    // marks those interrupted.
    let interrupted = crate::agents::store()
        .and_then(|s| {
            let rows = s.all()?;
            Ok(rows
                .iter()
                .filter(|r| match r.status {
                    ferrule_agents::Status::Interrupted => true,
                    ferrule_agents::Status::Running => !s.running_elsewhere(&r.id).unwrap_or(true),
                    _ => false,
                })
                .count())
        })
        .unwrap_or(0);
    if interrupted > 0 {
        r.note(
            "agents",
            format!(
                "{interrupted} agent{} interrupted by a restart or a crash",
                if interrupted == 1 { " was" } else { "s were" }
            ),
        );
        r.hint("`ferrule agents list` shows them; `ferrule agents close <id>` cleans one up");
    }
}

/// Whether ferrule's own HTTPS — `web_fetch`, MCP servers by URL — and
/// commands' go through the credential proxy, or straight out.
/// M19: the kill switch, the caps and today's spend, and who approves.
fn trust_check(r: &mut Report, cfg: &config::Config, telegram_on: bool) {
    let hub = match crate::trust::hub(cfg) {
        Ok(h) => h,
        Err(e) => {
            r.fail("trust", format!("{e:#}"));
            return;
        }
    };
    if let Some(info) = hub.stopped() {
        r.warn("trust", ferrule_trust::kill::stop_message(&info));
    }
    let c = hub.config();
    let caps = [
        ("run", c.max_tokens_per_run, c.max_usd_per_run),
        ("day", c.max_tokens_per_day, c.max_usd_per_day),
        ("task", c.max_tokens_per_task, c.max_usd_per_task),
    ]
    .iter()
    .filter(|(_, t, u)| *t > 0 || *u > 0.0)
    .map(|(per, t, u)| {
        let mut parts = vec![];
        if *t > 0 {
            parts.push(format!("{} tokens", ferrule_trust::hub::thousands(*t)));
        }
        if *u > 0.0 {
            parts.push(format!("${u:.2}"));
        }
        format!("{} per {per}", parts.join(" / "))
    })
    .collect::<Vec<_>>();
    let today = match hub.today(None) {
        Ok((t, _)) => format!(
            "today {} tokens, ${:.2}",
            ferrule_trust::hub::thousands(t.tokens),
            t.usd
        ),
        Err(e) if c.needs_ledger() => {
            r.fail(
                "trust",
                format!("{e}: every run stops before its first call"),
            );
            return;
        }
        Err(_) => "today unknown".into(),
    };
    r.ok(
        "trust",
        format!(
            "{} · {today} · gates {}",
            if caps.is_empty() {
                "no caps".to_string()
            } else {
                caps.join(", ")
            },
            if c.gates { "on" } else { "off" }
        ),
    );
    if c.gates {
        match hub.owner() {
            Some(chat) if telegram_on => r.ok(
                "trust",
                format!("approvals go to Telegram chat {chat} (and the terminal)"),
            ),
            Some(chat) => r.note(
                "trust",
                format!("owner chat {chat}, but Telegram is off: only the terminal approves"),
            ),
            None => r.note(
                "trust",
                "no owner chat: only the terminal approves, and gated commands in unattended runs are refused",
            ),
        }
    }
}

fn proxy(r: &mut Report, cfg: &config::Config) {
    if cfg.secrets.is_empty() {
        r.note(
            "proxy",
            "off (no [secrets]): web_fetch and MCP servers by URL connect directly",
        );
        return;
    }
    let set = cfg
        .secrets
        .keys()
        .filter(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
        .count();
    if set == 0 {
        let names: Vec<&str> = cfg.secrets.keys().map(String::as_str).collect();
        r.warn(
            "proxy",
            format!(
                "off: none of {} is set, so web_fetch and MCP servers by URL connect directly",
                names.join(", ")
            ),
        );
        r.hint("`ferrule setup` → Tool credentials, or export them");
        return;
    }
    r.ok(
        "proxy",
        format!(
            "on for {set} secret{}: web_fetch, MCP servers and commands go through it",
            if set == 1 { "" } else { "s" }
        ),
    );
}

/// M28: `[web_search]` without a paid call: the key is set and bound to the
/// endpoint's host; a keyless SearXNG instance is asked for its `/config`.
/// M30: which recall the agent gets, and how much of the store has
/// vectors from the configured embedder. Nothing is called.
fn memory_check(r: &mut Report, cfg: &config::Config) {
    use crate::config::EmbedderChoice;
    // `Config::load` already refused a `[memory]` that doesn't validate.
    let Ok(choice) = cfg.memory.choice(&cfg.providers) else {
        return;
    };
    let model = match choice {
        EmbedderChoice::Off => {
            r.note("memory", "keyword recall ([memory] embedder is off)");
            return;
        }
        EmbedderChoice::Local => {
            if !cfg!(feature = "local-embed") {
                r.warn(
                    "memory",
                    "embedder = \"local\", but this build has no local embedder: keyword recall",
                );
                return;
            }
            use ferrule_embed::download::{presence, verify, Presence, POTION_MULTILINGUAL};
            let spec = POTION_MULTILINGUAL;
            let Ok(data) = config::data_dir() else {
                return;
            };
            let dir = spec.dir(&data);
            match presence(&spec, &dir) {
                Presence::Present => {}
                Presence::Missing => {
                    r.warn(
                        "memory",
                        "the local embedding model isn't downloaded: keyword recall",
                    );
                    r.hint("`ferrule memory model download`");
                    return;
                }
                Presence::Incomplete(files) => {
                    r.warn(
                        "memory",
                        format!(
                            "the local embedding model is incomplete ({}): keyword recall",
                            files.join(", ")
                        ),
                    );
                    r.hint("`ferrule memory model download` finishes it");
                    return;
                }
            }
            if let Err(e) = verify(&spec, &dir) {
                r.fail(
                    "memory",
                    format!("the local embedding model doesn't verify: {e}"),
                );
                r.hint(format!(
                    "delete {} and run `ferrule memory model download`",
                    tilde(&dir)
                ));
                return;
            }
            ferrule_embed::ModelId::new("local", &spec.tag(), spec.dim)
        }
        EmbedderChoice::Endpoint(e) => {
            if let Some(var) = &e.key_env {
                if key(var).is_none() {
                    r.warn(
                        "memory",
                        format!("{}: no key (${var} isn't set): keyword recall", e.model),
                    );
                    r.hint("export it, or `ferrule setup` → Memory recall");
                    return;
                }
            }
            ferrule_embed::ModelId::new("openai", &e.model, e.dim)
        }
    };
    let db = config::data_dir().map(|d| d.join("memory.db"));
    let counts = match db {
        Ok(db) if db.exists() => ferrule_memory::MemoryStore::open(&db)
            .and_then(|s| s.embedding_counts(model.as_str()))
            .ok(),
        _ => Some(Default::default()),
    };
    let Some(c) = counts else {
        r.warn(
            "memory",
            format!("{model} · the memory store can't be read"),
        );
        return;
    };
    r.ok(
        "memory",
        format!(
            "hybrid recall · {model} · {}/{} live memories embedded",
            c.live_embedded, c.live
        ),
    );
    if c.live_embedded < c.live {
        r.hint("the rest are found by keyword until `ferrule memory reindex` (or recall catches up on its own)");
    }
}

async fn web_search_check(
    r: &mut Report,
    cfg: &config::Config,
    http: &reqwest::Client,
    offline: bool,
) {
    // `Config::load` already refused a `[web_search]` that doesn't validate.
    let Ok(Some(s)) = cfg.web_search.settings() else {
        r.note("search", "off (no [web_search] provider)");
        return;
    };
    let name = s.provider.name();
    let host = cfg.web_search.host(s.provider).unwrap_or_default();
    let mut limits = Vec::new();
    if cfg.web_search.max_searches_per_day > 0 {
        limits.push(format!("{}/day", cfg.web_search.max_searches_per_day));
    }
    if let Some(p) = cfg.web_search.price_per_search_usd {
        limits.push(format!("${p}/search"));
    }
    let limits = if limits.is_empty() {
        String::new()
    } else {
        format!(" · {}", limits.join(", "))
    };
    if let Some(var) = &s.key_env {
        if key(var).is_none() {
            r.fail("search", format!("{name}: no key (${var} isn't set)"));
            r.hint("`ferrule setup` → Web search, or export it");
            return;
        }
        let bound = cfg.secrets.get(var).is_some_and(|spec| {
            ferrule_proxy::SecretRule::from(spec)
                .hosts
                .iter()
                .filter_map(|h| ferrule_proxy::HostPattern::parse(h).ok())
                .any(|p| p.matches(&host))
        });
        if !bound {
            r.fail(
                "search",
                format!("{name}: [secrets] {var} isn't bound to {host}, so the proxy won't put the key in"),
            );
            r.hint(format!(
                "add \"{host}\" to {var}'s hosts, or remove {var} from [secrets]"
            ));
            return;
        }
        if s.endpoint.starts_with("http://") && !is_loopback(&host) {
            r.fail(
                "search",
                format!("{name}: {} is plain http, and the proxy sends a key over it only to this machine", s.endpoint),
            );
            r.hint("use the https:// URL");
            return;
        }
        if offline {
            r.ok(
                "search",
                format!("{name} · key set, bound to {host}, not checked{limits}"),
            );
        } else {
            // Without the key: refused before it's billed, and any answer
            // at all says the endpoint is there.
            let url = s.provider.search_url(&s.endpoint);
            let req = match s.provider {
                SearchProvider::Tavily | SearchProvider::Exa => {
                    http.post(&url).json(&serde_json::json!({}))
                }
                _ => http.get(&url),
            };
            match req.send().await {
                Ok(_) => r.ok(
                    "search",
                    format!("{name} · key set, bound to {host}, endpoint answers (the key isn't tried: that's a paid search){limits}"),
                ),
                Err(e) => r.warn("search", format!("{name}: couldn't reach {}: {}", s.endpoint, e.without_url())),
            }
        }
    } else if offline {
        r.ok(
            "search",
            format!("{name} at {} · not checked{limits}", s.endpoint),
        );
    } else {
        let url = format!("{}/config", s.endpoint.trim_end_matches('/'));
        match http.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                r.ok(
                    "search",
                    format!("{name} at {} answers{limits}", s.endpoint),
                );
            }
            Ok(resp) => {
                r.warn(
                    "search",
                    format!("{name}: {url} answered {}", resp.status()),
                );
                r.hint("check `endpoint`, and that the instance allows format=json");
            }
            Err(e) => r.warn(
                "search",
                format!("{name}: couldn't reach {}: {}", s.endpoint, e.without_url()),
            ),
        }
    }
    let ignored = s.provider.ignores(&s);
    if !ignored.is_empty() {
        r.note("search", format!("{name} ignores {}", ignored.join(", ")));
    }
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn service_check(r: &mut Report, config_path: &Path, telegram_on: bool) -> Result<()> {
    match service::status() {
        service::Status::Unsupported(why) => {
            r.note("service", format!("not available here ({why})"));
            if telegram_on {
                r.hint("Telegram answers only while `ferrule gateway` runs");
            }
        }
        service::Status::NotInstalled if telegram_on => {
            r.warn(
                "service",
                "not installed, so Telegram answers only while `ferrule gateway` runs",
            );
            r.hint("`ferrule setup` → Background service");
        }
        service::Status::NotInstalled => {
            r.note("service", "not installed (only needed for Telegram)")
        }
        service::Status::Installed { running: true, .. } => {
            let workspace = service::installed().map_or("unknown".into(), |(_, ws)| tilde(&ws));
            r.ok("service", format!("running · workspace {workspace}"));
            if service::binary_changed_since_start() == Some(true) {
                r.warn(
                    "service",
                    "binary upgraded since it started; restart the service to run the new one",
                );
                r.hint(service::restart_hint());
            }
        }
        service::Status::Installed { running: false, .. } => {
            r.fail("service", "installed but not running");
            r.hint(format!("see why: {}", service::logs_hint()));
        }
    }
    if let service::Status::Installed { running: true, .. } = service::status() {
        r.note("service", format!("its logs: {}", service::logs_hint()));
    }
    if telegram_on {
        gateways_check(r);
    }
    let installed = matches!(service::status(), service::Status::Installed { .. });
    if installed && service::scope() == service::Scope::User && service::is_root() {
        r.warn(
            "service",
            "it runs as root, and so does every command the agent runs",
        );
        if cfg!(target_os = "linux") {
            r.hint(
                "`sudo ferrule setup --system` runs it as a dedicated user; then remove this one",
            );
        }
    }
    if service::scope() == service::Scope::User
        && !service::is_root()
        && Path::new("/etc/systemd/system/ferrule.service").exists()
    {
        r.note(
            "service",
            "a system service is installed too; `sudo ferrule doctor` checks it",
        );
    }
    if let Some((pinned, workspace)) = service::installed() {
        let here = std::path::absolute(config_path)?;
        if pinned != here {
            r.warn(
                "service",
                format!("it runs with {}, not {}", tilde(&pinned), tilde(&here)),
            );
            r.hint("`ferrule setup` → Background service → Reinstall it, to switch");
        }
        let data = config::data_dir()?;
        if data.starts_with(&workspace) {
            r.warn(
                "service",
                format!(
                    "its workspace {} holds ferrule's own data, which commands can then change",
                    tilde(&workspace)
                ),
            );
        }
    }
    Ok(())
}

/// M19c: two gateways polling one bot token take turns getting its
/// messages, and Telegram answers each with 409 Conflict.
fn gateways_check(r: &mut Report) {
    let pids = crate::health::running_gateways();
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    match pids.len() {
        0 => r.note("gateway", "none running on this machine"),
        1 => r.ok(
            "gateway",
            format!("one running on this machine (pid {list})"),
        ),
        n => {
            r.warn(
                "gateway",
                format!("{n} running on this machine (pids {list}): with one bot token they take turns getting its messages, and Telegram refuses each of them some (409 Conflict)"),
            );
            r.hint("keep one: stop the other (Ctrl-C in its terminal, or `kill <pid>`); `ferrule status` shows which one this data directory's is");
        }
    }
}

/// Is `ferrule` on PATH, and is it this binary?
fn binary(r: &mut Report) {
    let Ok(exe) = std::env::current_exe().and_then(|p| p.canonicalize()) else {
        return;
    };
    let name = if cfg!(windows) {
        "ferrule.exe"
    } else {
        "ferrule"
    };
    let on_path = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(name))
                .find(|p| p.is_file())
        })
        .unwrap_or_default();
    match on_path {
        None => {
            r.warn("binary", format!("{} isn't on your PATH", tilde(&exe)));
            if let Some(dir) = exe.parent() {
                r.hint(format!(
                    "add to your shell profile: export PATH=\"{}:$PATH\"",
                    dir.display()
                ));
            }
        }
        Some(found) if found.canonicalize().is_ok_and(|p| p == exe) => {
            r.ok("binary", tilde(&found))
        }
        Some(found) => {
            r.warn(
                "binary",
                format!(
                    "`ferrule` on your PATH is {}, not this one ({})",
                    tilde(&found),
                    tilde(&exe)
                ),
            );
        }
    }
}

/// Is there a Chrome or Chromium, does it start headless the way the
/// browser server would run it, and is `[browser]` ready? Problems only
/// count as failures when `[browser]` is on; otherwise they're notes.
fn browser_check(r: &mut Report, cfg: Option<&config::Config>) {
    let fallback = ferrule_mcp::BrowserConfig::default();
    let b = cfg.map_or(&fallback, |c| &c.browser);
    let on = b.enabled;
    let problem = |r: &mut Report, text: String| {
        if on {
            r.fail("browser", text)
        } else {
            r.note("browser", text)
        }
    };
    let Some(chrome) = browser::chrome(b) else {
        problem(
            r,
            "no Chrome or Chromium found (set CHROME_PATH, or `chrome` in [browser])".into(),
        );
        return;
    };
    let blocker = browser::sandbox_blocker(cfg);
    let no_sandbox = !b.chrome_sandbox || blocker.is_some();
    let started = match cfg {
        Some(cfg) => browser::launch_test(cfg, &chrome, no_sandbox),
        None => {
            ferrule_mcp::browser::launch_test(&chrome, no_sandbox, browser::LAUNCH_TIMEOUT, None)
        }
    };
    match started {
        Ok(()) if no_sandbox => r.ok(
            "browser",
            format!(
                "{} · starts headless, without its own sandbox",
                tilde(&chrome)
            ),
        ),
        Ok(()) => r.ok("browser", format!("{} · starts headless", tilde(&chrome))),
        Err(why) if !no_sandbox && ferrule_mcp::browser::is_sandbox_failure(&why) => {
            problem(
                r,
                format!(
                    "{}: Chrome's own sandbox can't start: {why}",
                    tilde(&chrome)
                ),
            );
            // Ubuntu 24.04 and later: it needs user namespaces, which
            // AppArmor only grants to a program with a profile saying so.
            let restricted =
                std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
                    .is_ok_and(|v| v.trim() == "1");
            if restricted {
                r.hint(format!(
                    "AppArmor restricts user namespaces here. Best: a profile allowing `userns` \
                     for {} (docs/browser.md). Else `sudo sysctl \
                     kernel.apparmor_restrict_unprivileged_userns=0` for the whole system",
                    chrome.display()
                ));
            }
            r.hint("last resort: `chrome_sandbox = false` in [browser] (docs/browser.md)");
        }
        Err(why) => problem(
            r,
            format!("{} didn't start headless: {why}", tilde(&chrome)),
        ),
    }
    let command = ferrule_mcp::browser::find_agent_browser(&b.command);
    if !on {
        let how = match &command {
            Ok(_) => "`ferrule setup` → Browser turns it on".to_string(),
            Err(why) => format!("{why}, then `ferrule setup` → Browser"),
        };
        r.note("browser", format!("off for the agent: {how}"));
        return;
    }
    match &command {
        Ok(path) => r.ok("browser", format!("driven by {}", tilde(path))),
        Err(why) => r.fail("browser", why.clone()),
    }
    if let Some(why) = blocker.filter(|_| b.chrome_sandbox) {
        r.fail(
            "browser",
            format!("{why}, so the agent won't get the browser"),
        );
        r.hint("`chrome_sandbox = false` in [browser] accepts that (docs/browser.md)");
    } else if !b.chrome_sandbox {
        r.warn(
            "browser",
            "Chrome runs without its own sandbox (chrome_sandbox = false); ferrule's still confines it",
        );
    }
    let route = match cfg {
        Some(cfg) if !cfg.secrets.is_empty() => "through the credential proxy",
        _ => "straight out (no [secrets], so no proxy)",
    };
    let domains = if b.allowed_domains.is_empty() {
        "any site".to_string()
    } else {
        b.allowed_domains.join(", ")
    };
    r.ok("browser", format!("on · {domains} · {route}"));
}

/// M19b: what watches the gateway — systemd's watchdog in the installed
/// unit, the heartbeat, the turn watchdog and deadline.
fn health_check(r: &mut Report, cfg: &config::Config, telegram_on: bool) {
    let unit = match service::status() {
        service::Status::Installed { unit, .. } if cfg!(target_os = "linux") => {
            Some(std::fs::read_to_string(unit).unwrap_or_default())
        }
        _ => None,
    };
    for (level, text, hint) in health_lines(&cfg.health, unit.as_deref(), telegram_on) {
        r.line(level, "health", text);
        if let Some(hint) = hint {
            r.hint(hint);
        }
    }
}

type Line = (Level, String, Option<&'static str>);

fn health_lines(h: &config::HealthConfig, unit: Option<&str>, telegram_on: bool) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    let turns = match (h.watchdog_after_secs, h.max_turn_minutes) {
        (0, 0) => "no turn watchdog, no turn deadline".to_string(),
        (0, m) => format!("no turn watchdog; turns end after {m} min"),
        (w, 0) => format!(
            "a stuck turn is reported after {}; no turn deadline",
            human(w)
        ),
        (w, m) => format!(
            "a stuck turn is reported after {}; turns end after {m} min",
            human(w)
        ),
    };
    out.push((Level::Ok, turns, None));
    if let Some(unit) = unit {
        if unit
            .lines()
            .any(|l| l.trim_start().starts_with("WatchdogSec="))
        {
            out.push((
                Level::Ok,
                "the unit has systemd's watchdog: a wedged gateway is restarted".into(),
                None,
            ));
        } else {
            out.push((
                Level::Warn,
                "the unit has no systemd watchdog (an older ferrule wrote it): a wedged gateway stays up".into(),
                Some("`ferrule setup` → Background service rewrites it"),
            ));
        }
    }
    let url = h.heartbeat_url.trim();
    if url.is_empty() {
        if telegram_on {
            out.push((
                Level::Note,
                "no heartbeat: nothing outside this machine notices if it goes down".into(),
                Some("[health] heartbeat_url, e.g. a healthchecks.io check"),
            ));
        }
    } else {
        // Only the host: the rest of the URL is often the check's secret.
        match reqwest::Url::parse(url) {
            Ok(u) if u.host_str().is_some() => out.push((
                Level::Ok,
                format!(
                    "heartbeat to {} every {}",
                    u.host_str().unwrap_or_default(),
                    human(h.heartbeat_secs.max(1))
                ),
                None,
            )),
            _ => out.push((
                Level::Fail,
                "heartbeat_url isn't a URL; no heartbeat is sent".into(),
                None,
            )),
        }
    }
    out
}

fn human(secs: u64) -> String {
    ferrule_gateway::health::human(std::time::Duration::from_secs(secs))
}

#[cfg(test)]
mod health_tests {
    use super::*;

    #[test]
    fn the_health_line_names_the_watchdogs_and_the_heartbeat_host_only() {
        let mut h = config::HealthConfig::default();
        let lines = health_lines(&h, Some("[Service]\nRestart=always\n"), true);
        assert_eq!(
            lines[0].1,
            "a stuck turn is reported after 10 min; turns end after 60 min"
        );
        assert_eq!(lines[1].0, Level::Warn);
        assert_eq!(lines[2].0, Level::Note);
        assert!(lines[2].1.starts_with("no heartbeat"));

        h.heartbeat_url = "https://hc-ping.com/5f1c-secret-uuid".into();
        h.watchdog_after_secs = 0;
        let lines = health_lines(&h, Some("Restart=always\nWatchdogSec=120\n"), true);
        assert_eq!(lines[0].1, "no turn watchdog; turns end after 60 min");
        assert_eq!(lines[1].0, Level::Ok);
        assert_eq!(lines[2].1, "heartbeat to hc-ping.com every 1 min");
        assert!(lines.iter().all(|l| !l.1.contains("secret")));

        h.heartbeat_url = "not a url".into();
        let lines = health_lines(&h, None, false);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].0, Level::Fail);
    }
}
