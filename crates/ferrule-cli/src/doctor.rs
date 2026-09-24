//! `ferrule doctor`: one pass over everything setup configures — the
//! config, the saved keys, each provider, Telegram, the sandbox, the
//! background service, the binary itself and the browser — with a line per check and what to do about each problem. Exits
//! non-zero if anything is broken.

use crate::setup::tilde;
use crate::{browser, config, probe, secrets, service};
use anyhow::Result;
use ferrule_sandbox::{Mode, Sandbox};
use std::fmt::Display;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
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

pub async fn run(offline: bool) -> Result<bool> {
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
    let telegram_on = telegram(&mut r, &cfg, &http, offline).await;
    let confined = sandbox(&mut r, &cfg, &secrets_path);
    mcp(&mut r, &cfg, confined);
    proxy(&mut r, &cfg);
    service_check(&mut r, &path, telegram_on)?;
    binary(&mut r);
    browser_check(&mut r, Some(&cfg));
    Ok(r.finish())
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
        let label = format!(
            "{name}{} · {}",
            if is_default { " (default)" } else { "" },
            p.model
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
            continue;
        }
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
                    Ok(Some(_)) => {
                        r.fail(
                            "telegram",
                            "the bot has a webhook set, so the gateway receives nothing",
                        );
                        r.hint("`ferrule setup` → Telegram offers to remove it");
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

/// Whether commands run confined.
fn sandbox(r: &mut Report, cfg: &config::Config, secrets_path: &Path) -> bool {
    let sandbox = match Sandbox::new(crate::sandbox_policy(cfg)) {
        Ok(sandbox) => sandbox,
        Err(e) => {
            r.fail("sandbox", e);
            r.hint("`ferrule setup` → Sandbox, or `ferrule sandbox` for details");
            return false;
        }
    };
    if !sandbox.is_active() {
        let why = match (cfg.sandbox.mode, sandbox.degraded()) {
            (Mode::Off, _) => "mode = off".to_string(),
            (_, Some(why)) => why.to_string(),
            (_, None) => "no sandbox on this system".to_string(),
        };
        r.warn("sandbox", format!("shell commands run unsandboxed: {why}"));
        return false;
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
    if cfg!(unix) && secrets_path.exists() {
        let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let read = sandbox
            .command(
                "/bin/sh",
                [
                    "-c".as_ref(),
                    "cat \"$1\"".as_ref(),
                    "sh".as_ref(),
                    secrets_path.as_os_str(),
                ],
                &workspace,
            )
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
    true
}

/// MCP servers that run outside the sandbox. Only the config is read: the
/// servers themselves aren't started.
fn mcp(r: &mut Report, cfg: &config::Config, confined: bool) {
    // Servers reached by URL aren't processes of ours: nothing to confine.
    let servers: Vec<_> = cfg.mcp.servers.iter().filter(|s| s.url.is_none()).collect();
    if servers.is_empty() {
        return;
    }
    let open: Vec<&str> = servers
        .iter()
        .filter(|s| !s.sandbox)
        .map(|s| s.name.as_str())
        .collect();
    for name in &open {
        r.warn(
            "mcp",
            format!(
                "`{name}` has sandbox = false: it can write anywhere you can and read the saved keys"
            ),
        );
    }
    if !open.is_empty() {
        r.hint("remove `sandbox = false`, and give it `writable_roots` instead if it needs them");
    }
    let rest = servers.len() - open.len();
    match (rest, confined) {
        (0, _) => {}
        (n, true) => r.ok(
            "mcp",
            format!("{n} server{} sandboxed", if n == 1 { "" } else { "s" }),
        ),
        (n, false) => r.warn(
            "mcp",
            format!(
                "{n} server{} unsandboxed, like shell commands",
                if n == 1 { " runs" } else { "s run" }
            ),
        ),
    }
}

/// Whether ferrule's own HTTPS — `web_fetch`, MCP servers by URL — and
/// commands' go through the credential proxy, or straight out.
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
