//! M35: the Claude plan in the binary (docs/m35-subscriptions.md §7–8).
//! Every model request goes through the unmodified `claude` binary; ferrule
//! never calls the Anthropic API with a plan's token. Signing in happens in
//! Anthropic's own flow (`claude auth login`) or `claude setup-token`, at a
//! terminal, never in a chat.

use super::SignIn;
use crate::config::{self, ClaudeCodePlanConfig, Config, Plan};
use anyhow::{bail, Context, Result};
use ferrule_core::provider::Provider;
use ferrule_plans::claude::cli::{self, AuthStatus};
use ferrule_plans::claude::token::{self, Credential, TokenStore};
use ferrule_plans::claude::{ClaudeCode, EngineConfig};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The hosts claude talks to, let through the egress policy when a
/// `claude-code` provider is configured.
pub const ANTHROPIC_HOSTS: &[&str] = &[
    "api.anthropic.com",
    "claude.ai",
    "console.anthropic.com",
    "statsig.anthropic.com",
];

/// What works under the engine, for setup and the guide.
pub const FEATURES: &str = "\
  ferrule feature            under the Claude plan (Claude Code engine)
  tools and approvals        claude's own tools, each gated by ferrule's approvals
  memory, tasks, messaging   yes, bridged into claude as ferrule's tools
  compaction                 claude's own
  repo map, per-edit lint    no (claude explores and edits itself)
  routing inside a turn      no; fallback to another model between turns: yes
  streaming, sub-agents      yes
  sandbox                    claude runs inside it; its Bash shares claude's sandbox
  cost                       $0 on the ledger, the notional price shown";

/// The plan's model a new `[providers.claude-code]` starts on.
pub const MODEL: &str = "sonnet";

/// `[plans.claude_code]`, the defaults when there's no config.
pub fn settings() -> ClaudeCodePlanConfig {
    Config::load()
        .map(|(c, _)| c.plans.claude_code)
        .unwrap_or_default()
}

pub fn config_dir(s: &ClaudeCodePlanConfig) -> Result<PathBuf> {
    Ok(s.config_dir(&config::data_dir()?))
}

/// The engine's settings from the config: the binary and config dir, the
/// limits, the bridge (this binary's hidden `claude-mcp`) and the sandbox.
pub fn engine_config(cfg: &Config) -> Result<EngineConfig> {
    let s = &cfg.plans.claude_code;
    let data = config::data_dir()?;
    let dir = s.config_dir(&data);
    let workspace = std::env::current_dir().unwrap_or_else(|_| data.clone());
    let mut e = EngineConfig::new(s.binary(), dir.clone(), workspace);
    e.private_dir = Some(crate::secrets::private_dir()?);
    e.repair = crate::update::claude::Claude::from_config(cfg, &data).map(|c| {
        Arc::new(crate::update::claude::Fixer::new(data.clone(), c))
            as Arc<dyn ferrule_plans::claude::Repairer>
    });
    e.data_dir = Some(data);
    e.turn_timeout = Duration::from_secs(s.turn_timeout_minutes.max(1) * 60);
    e.max_output_bytes = s.max_output_mb.max(1) * 1024 * 1024;
    e.scrub_subprocess_env = s.scrub_subprocess_env;
    e.bridge_command = std::env::current_exe()
        .ok()
        .map(|exe| (exe, vec!["claude-mcp".to_string()]));
    e.sandbox = Some(crate::engine_sandbox(cfg, &dir)?);
    Ok(e)
}

pub fn client(name: &str, model: &str) -> Arc<dyn Provider> {
    match Config::load().and_then(|(c, _)| engine_config(&c)) {
        Ok(e) => Arc::new(ClaudeCode::new(name, model, e)),
        Err(e) => super::unavailable(name, format!("the Claude plan: {e:#}")),
    }
}

/// The credential a turn would run on.
pub fn credential() -> Result<Credential> {
    TokenStore::new(&crate::secrets::private_dir()?).credential()
}

/// `claude auth status`, asked at most every 30 s per config dir: setup and
/// the model list ask often, and it starts a node process.
type Cached = Option<(Instant, PathBuf, Result<AuthStatus, String>)>;
static LAST: Mutex<Cached> = Mutex::new(None);

fn auth_status(binary: &Path, dir: &Path) -> Result<AuthStatus, String> {
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, d, r)) = &*last {
        if d == dir && at.elapsed() < Duration::from_secs(30) {
            return r.clone();
        }
    }
    let r = cli::auth_status(binary, dir, None).map_err(|e| format!("{e:#}"));
    *last = Some((Instant::now(), dir.to_path_buf(), r.clone()));
    r
}

/// The next [`state`] asks claude again.
fn forget_status() {
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Where the Claude plan's sign-in stands. A token ferrule holds counts as
/// signed in (claude is only asked when there's none: its own login).
pub fn state() -> SignIn {
    let s = settings();
    let binary = s.binary();
    if cli::find(&binary).is_none() {
        return SignIn::Unreadable(format!(
            "Claude Code isn't installed ({}): {}",
            binary.display(),
            cli::INSTALL
        ));
    }
    let credential = match credential() {
        Ok(c) => c,
        Err(e) => return SignIn::Unreadable(format!("{e:#}")),
    };
    match credential {
        Credential::Exported(_) => SignIn::In {
            email: None,
            plan: Some("CLAUDE_CODE_OAUTH_TOKEN".into()),
        },
        Credential::Pasted { at, .. } => match token::age(at, super::now()) {
            token::Age::Expired => SignIn::Expired,
            _ => SignIn::In {
                email: None,
                plan: Some("setup-token".into()),
            },
        },
        Credential::ClaudeLogin => {
            let dir = match config_dir(&s) {
                Ok(d) => d,
                Err(e) => return SignIn::Unreadable(format!("{e:#}")),
            };
            match auth_status(&binary, &dir) {
                Ok(a) if a.is_plan() => SignIn::In {
                    email: a.email,
                    plan: a.subscription_type,
                },
                Ok(a) if a.logged_in => SignIn::Unreadable(format!(
                    "claude is signed in with {}, not a Claude plan",
                    a.auth_method.as_deref().unwrap_or("something else")
                )),
                Ok(_) => SignIn::Out,
                Err(e) => SignIn::Unreadable(e),
            }
        }
    }
}

/// `ferrule login claude [--token]`, at a terminal.
pub async fn login(paste_token: bool) -> Result<()> {
    let s = settings();
    let binary = s.binary();
    let dir = config_dir(&s)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    if paste_token {
        let token = read_token()?;
        save_setup_token(&crate::secrets::private_dir()?, &token)?;
        println!(
            "Saved the setup-token, sealed. Only the claude process gets it, as \
             CLAUDE_CODE_OAUTH_TOKEN; ferrule itself never uses it. It lasts a year: \
             `ferrule doctor` warns 30 days before."
        );
        println!(
            "This stores a Claude credential on this machine. For none at all, export \
             CLAUDE_CODE_OAUTH_TOKEN yourself or use `ferrule login claude` without --token."
        );
    } else {
        if cli::find(&binary).is_none() {
            bail!(
                "Claude Code isn't installed ({} not found): {}",
                binary.display(),
                cli::INSTALL
            );
        }
        if !std::io::stdin().is_terminal() {
            bail!("`ferrule login claude` opens Anthropic's own sign-in and needs a terminal; `--token` takes a setup-token from `claude setup-token` instead");
        }
        println!("Signing in through Claude Code (Anthropic's own sign-in)…");
        let status = tokio::task::spawn_blocking({
            let (binary, dir) = (binary.clone(), dir.clone());
            move || {
                cli::command(&binary, &dir, None, &["auth", "login", "--claudeai"])
                    .status()
                    .context("running claude auth login")
            }
        })
        .await??;
        if !status.success() {
            bail!("claude auth login didn't finish ({status})");
        }
    }
    forget_status();
    match state() {
        s @ SignIn::In { .. } => println!("The Claude plan: {}.", s.word()),
        other => bail!("the Claude plan still isn't signed in: {}", other.word()),
    }
    if !paste_token {
        if let Ok(Credential::Exported(_)) = credential() {
            println!("Note: CLAUDE_CODE_OAUTH_TOKEN is exported here and goes first.");
        }
    }
    if let Some(line) = super::login::add_provider(Plan::ClaudeCode, MODEL)? {
        println!("{line}");
    }
    Ok(())
}

/// Checks a setup-token's shape and stores it, sealed, in `private` (the
/// terminal's `--token` and the dashboard's form). Errors never echo it.
pub fn save_setup_token(private: &Path, token: &str) -> Result<()> {
    let t = token.trim();
    token::check_setup_token(t)?;
    TokenStore::new(private).save(t, super::now())?;
    forget_status();
    Ok(())
}

/// The setup-token: hidden at a terminal, one line from a pipe
/// (`claude setup-token | … --token` never puts it in the shell history).
fn read_token() -> Result<String> {
    let raw = if std::io::stdin().is_terminal() {
        println!("Run `claude setup-token` (Anthropic's sign-in) and paste what it prints.");
        inquire::Password::new("Setup-token:")
            .without_confirmation()
            .with_display_mode(inquire::PasswordDisplayMode::Masked)
            .prompt()?
    } else {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        line
    };
    let t = raw.trim().to_string();
    token::check_setup_token(&t)?;
    Ok(t)
}

/// `ferrule logout claude`: the pasted token deleted, and claude's own
/// login in ferrule's config dir ended. claude's login in `~/.claude` is
/// the user's and stays.
pub async fn logout() -> Result<()> {
    let s = settings();
    let dir = config_dir(&s)?;
    if TokenStore::new(&crate::secrets::private_dir()?).delete()? {
        println!("Deleted the saved setup-token.");
    }
    let own = dir.starts_with(config::data_dir()?);
    let binary = s.binary();
    if own && cli::find(&binary).is_some() && dir.exists() {
        let out = tokio::task::spawn_blocking(move || {
            cli::command(&binary, &dir, None, &["auth", "logout"])
                .stdin(std::process::Stdio::null())
                .output()
        })
        .await??;
        if out.status.success() {
            println!("Signed Claude Code out of ferrule's config dir.");
        }
    } else if !own {
        println!(
            "claude's own login in {} is yours and stays; `claude auth logout` ends it.",
            dir.display()
        );
    }
    if token::exported_token().is_some() {
        println!("CLAUDE_CODE_OAUTH_TOKEN is still exported in this environment; unset it too.");
    }
    forget_status();
    Ok(())
}

/// Setup's Claude plan choice: the feature table, then a sign-in if
/// there's none, then the model. `false`: nothing was set up — the claude
/// CLI isn't there, or no sign-in was wanted — so the caller can offer
/// another way instead of going on with no provider.
pub async fn setup_step(t: &mut crate::setup::Target) -> Result<bool> {
    use crate::setup::{info, ok, warn};
    info("The Claude plan runs through Claude Code, Anthropic's own CLI. Some ferrule features work differently:");
    println!("{FEATURES}");
    let s = t.config()?.plans.claude_code;
    let binary = s.binary();
    match cli::find(&binary) {
        None => {
            warn(format!("Claude Code isn't installed: {}", cli::INSTALL));
            info("Back to the model choices — once it's installed, this way works.");
            return Ok(false);
        }
        Some(path) => {
            let dir = config_dir(&s)?;
            match cli::version(&path, &dir) {
                Ok(v) => ok(format!("Claude Code {v} at {}", path.display())),
                Err(e) => warn(format!("{e:#}")),
            }
        }
    }
    match state() {
        s @ SignIn::In { .. } => ok(format!("the Claude plan: {}", s.word())),
        _ => {
            const WAYS: [&str; 3] = [
                "Sign in through Claude Code (Anthropic's own sign-in, nothing stored by ferrule)",
                "Paste a setup-token from `claude setup-token` (sealed; for a service without your shell)",
                "Not now",
            ];
            let way = inquire::Select::new("How should Claude Code sign in?", WAYS.to_vec())
                .raw_prompt()?
                .index;
            match way {
                0 | 1 => crate::setup::interruptible(login(way == 1)).await??,
                _ => return Ok(false),
            }
        }
    }
    let cfg = t.config()?;
    let existing = cfg
        .providers
        .iter()
        .find(|(_, p)| p.plan == Some(Plan::ClaudeCode))
        .map(|(n, p)| (n.clone(), p.model.clone()));
    let name = existing
        .as_ref()
        .map(|(n, _)| n.clone())
        .unwrap_or_else(|| "claude-code".into());
    let current = existing.map(|(_, m)| m).unwrap_or_else(|| MODEL.into());
    let models: Vec<String> = super::CLAUDE_CODE_MODELS
        .iter()
        .map(|m| m.to_string())
        .collect();
    let model = crate::setup::pick_model(&models, &current)?;
    let tbl = crate::setup::table(t.root(), &["providers", &name])?;
    crate::setup::put(tbl, "plan", "claude-code");
    crate::setup::put(tbl, "model", model.as_str());
    crate::setup::ask_default(t, &cfg, &name, &model)?;
    t.save()?;
    ok(format!("saved `{name}` · {model} on the Claude plan"));
    Ok(true)
}

/// Doctor's lines for the Claude plan: the binary and its version, which
/// credential is active, a pasted token's age, variables that would bill
/// the API for a `claude` run by hand, and settings that route around
/// ferrule's gates.
pub struct Check {
    pub ok: Vec<String>,
    pub warn: Vec<(String, Option<String>)>,
    pub fail: Vec<(String, Option<String>)>,
}

pub fn check(s: &ClaudeCodePlanConfig, now: u64) -> Check {
    let mut c = Check {
        ok: vec![],
        warn: vec![],
        fail: vec![],
    };
    let binary = s.binary();
    let dir = match config_dir(s) {
        Ok(d) => d,
        Err(e) => {
            c.fail.push((format!("{e:#}"), None));
            return c;
        }
    };
    match cli::find(&binary) {
        None => {
            c.fail.push((
                format!(
                    "Claude Code isn't installed ({} not found)",
                    binary.display()
                ),
                Some(cli::INSTALL.into()),
            ));
            return c;
        }
        Some(path) => match cli::version(&path, &dir) {
            Ok(v) => c.ok.push(format!("Claude Code {v} ({})", path.display())),
            Err(e) => c.warn.push((format!("{e:#}"), None)),
        },
    }
    match credential() {
        Err(e) => c.fail.push((
            format!("the setup-token can't be read: {e:#}"),
            Some("`ferrule login claude --token`".into()),
        )),
        Ok(cred) => {
            c.ok.push(format!("credential: {}", cred.describe()));
            if let Credential::Pasted { at, .. } = cred {
                match token::age(at, now) {
                    token::Age::Fine { days_left } => {
                        c.ok.push(format!("the setup-token has {days_left} days left"))
                    }
                    token::Age::Soon { days_left } => c.warn.push((
                        format!("the setup-token runs out in {days_left} days"),
                        Some("`claude setup-token`, then `ferrule login claude --token`".into()),
                    )),
                    token::Age::Expired => c.fail.push((
                        "the setup-token is more than a year old and has run out".into(),
                        Some("`claude setup-token`, then `ferrule login claude --token`".into()),
                    )),
                }
            }
            if matches!(cred, Credential::ClaudeLogin) {
                match cli::auth_status(&binary, &dir, None) {
                    Ok(a) if a.is_plan() => c.ok.push(format!(
                        "claude is signed in{}{}",
                        a.email.map(|e| format!(" as {e}")).unwrap_or_default(),
                        a.subscription_type
                            .map(|p| format!(" ({p})"))
                            .unwrap_or_default()
                    )),
                    Ok(a) if a.logged_in => c.fail.push((
                        format!(
                            "claude is signed in with {}, not a Claude plan",
                            a.auth_method.as_deref().unwrap_or("something else")
                        ),
                        Some("`ferrule login claude`".into()),
                    )),
                    Ok(_) => c.fail.push((
                        "Claude Code isn't signed in".into(),
                        Some("`ferrule login claude`".into()),
                    )),
                    Err(e) => c.warn.push((format!("{e:#}"), None)),
                }
            }
        }
    }
    let outranking: Vec<&str> = ferrule_plans::claude::env::OUTRANKING
        .iter()
        .copied()
        .filter(|v| std::env::var_os(v).is_some_and(|x| !x.is_empty()))
        .collect();
    if !outranking.is_empty() {
        c.warn.push((
            format!(
                "{} is set: ferrule's engine strips it, but a `claude` you run by hand would bill the API instead of the plan",
                outranking.join(", ")
            ),
            None,
        ));
    }
    let risky = ferrule_plans::claude::env::risky_settings(&dir);
    if !risky.is_empty() {
        c.warn.push((
            format!(
                "{} sets {}, which can route around ferrule's approvals",
                dir.join("settings.json").display(),
                risky.join(", ")
            ),
            Some(
                "remove them, or leave [plans.claude_code] config_dir empty for ferrule's own"
                    .into(),
            ),
        ));
    }
    c
}
