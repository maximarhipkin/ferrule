//! M44: managed mode — one bot per container, driven and locked by a panel
//! (docs/m44-managed-mode.md §3). On with `[managed] enabled = true` or
//! `FERRULE_MANAGED=1`; `FERRULE_MANAGED=0` turns it off whatever the
//! config says, since the environment is the panel's.
//!
//! The policy file is the panel's too: read once, never written, never
//! merged into the config. What it locks is enforced where each feature is
//! used (the loaded config is narrowed in [`apply`], the sandbox refuses
//! commands, the dashboard answers 403), so no config edit lifts it.

use crate::config::{Config, Plan, ProviderConfig};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const ENV: &str = "FERRULE_MANAGED";
pub const POLICY_ENV: &str = "FERRULE_POLICY";
pub const BOT_ENV: &str = "FERRULE_BOT_ID";
/// `0` or `1`: Chrome's own sandbox, over `[browser] chrome_sandbox`. Read
/// in managed mode only: the `-browser` image sets it to 0, since Docker's
/// default seccomp profile has no user namespaces for it.
pub const CHROME_SANDBOX_ENV: &str = "FERRULE_BROWSER_CHROME_SANDBOX";
/// `1`: a first start's config turns the browser on (the `-browser` image).
pub const BROWSER_ENV: &str = "FERRULE_BROWSER";

pub const PANEL_SECRET_ENV: &str = "FERRULE_PANEL_SECRET";
pub const PUBLIC_URL_ENV: &str = "FERRULE_PUBLIC_URL";
pub const DASHBOARD_BIND_ENV: &str = "FERRULE_DASHBOARD_BIND";
pub const DASHBOARD_PORT_ENV: &str = "FERRULE_DASHBOARD_PORT";

/// Said wherever the Claude plan is refused in managed mode.
pub const NO_CLAUDE_PLAN: &str = "The Claude plan isn't available on a hosted bot. Anthropic's terms allow \
     a Claude subscription only for a person using the claude program themselves, not through a \
     service that runs it for them. Add an Anthropic API key instead, or sign in with the ChatGPT plan.";

/// `[managed]` in `ferrule.toml`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ManagedConfig {
    pub enabled: bool,
    /// The panel's policy file (or `FERRULE_POLICY`).
    pub policy: Option<PathBuf>,
    /// This bot's id; a panel sign-in token must name it (or
    /// `FERRULE_BOT_ID`).
    pub bot_id: Option<String>,
}

/// Where the managed switch was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Env,
    Config,
}

/// What the policy says about commands when ferrule's own OS sandbox
/// can't start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxNeed {
    /// No OS sandbox, no commands.
    #[default]
    Os,
    /// The container is enough: commands run in it without ferrule's.
    Container,
}

/// The panel's policy file. Every key is optional and a missing one
/// allows, so a policy lists only what it locks.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    /// Shown next to every lock.
    pub reason: Option<String>,
    /// The shell tool, hooks and the verify command.
    pub shell: bool,
    /// The browser tools.
    pub browser: bool,
    /// Installing MCP servers, plugins and skills, and self-extension.
    pub extensions: bool,
    pub sandbox: SandboxNeed,
    /// Allowed model providers, by kind (`openai`, `anthropic`,
    /// `openrouter`, `chatgpt`, … or an API host). Unset: any but the
    /// Claude plan, which managed mode never allows.
    pub providers: Option<Vec<String>>,
    pub max_usd_per_day: Option<f64>,
    pub max_usd_per_run: Option<f64>,
    pub max_tokens_per_day: Option<u64>,
    pub max_turn_minutes: Option<u64>,
    /// Channels that may be set up (`telegram`, `http`, …). Unset: any.
    pub channels: Option<Vec<String>>,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            reason: None,
            shell: true,
            browser: true,
            extensions: true,
            sandbox: SandboxNeed::Os,
            providers: None,
            max_usd_per_day: None,
            max_usd_per_run: None,
            max_tokens_per_day: None,
            max_turn_minutes: None,
            channels: None,
        }
    }
}

impl Policy {
    /// Everything off: what a policy that can't be read means.
    fn closed(why: &str) -> Self {
        Policy {
            reason: Some(why.to_string()),
            shell: false,
            browser: false,
            extensions: false,
            providers: Some(Vec::new()),
            channels: Some(Vec::new()),
            ..Policy::default()
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        let p: Policy = toml::from_str(text)?;
        for (name, v) in [
            ("max_usd_per_day", p.max_usd_per_day),
            ("max_usd_per_run", p.max_usd_per_run),
        ] {
            if v.is_some_and(|v| !(v.is_finite() && v > 0.0)) {
                bail!("{name} must be a number above 0");
            }
        }
        for (name, v) in [
            ("max_tokens_per_day", p.max_tokens_per_day),
            ("max_turn_minutes", p.max_turn_minutes),
        ] {
            if v == Some(0) {
                bail!("{name} must be above 0");
            }
        }
        Ok(p)
    }

    /// The words next to a lock.
    pub fn why(&self) -> String {
        self.reason
            .clone()
            .filter(|r| !r.trim().is_empty())
            .unwrap_or_else(|| "set by whoever runs this bot".into())
    }

    pub fn allows_provider(&self, kind: &str) -> bool {
        kind != "claude"
            && self
                .providers
                .as_ref()
                .is_none_or(|l| l.iter().any(|p| p.eq_ignore_ascii_case(kind)))
    }

    pub fn allows_channel(&self, name: &str) -> bool {
        self.channels
            .as_ref()
            .is_none_or(|l| l.iter().any(|c| c.eq_ignore_ascii_case(name)))
    }

    /// Each lock in words, for doctor and the dashboard.
    pub fn locks(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.shell {
            out.push("the shell is off (and hooks, verify_command, the linter, task gates, a transcription command)".to_string());
        }
        if !self.browser {
            out.push("the browser is off".into());
        }
        if !self.extensions {
            out.push("installing MCP servers, plugins and skills is off".into());
        }
        if self.sandbox == SandboxNeed::Container {
            out.push(
                "commands may run without ferrule's OS sandbox (the container is the boundary)"
                    .into(),
            );
        }
        if let Some(p) = &self.providers {
            out.push(format!("model providers: {}", list(p)));
        }
        if let Some(c) = &self.channels {
            out.push(format!("channels: {}", list(c)));
        }
        for (what, v) in [
            ("spend per day", self.max_usd_per_day),
            ("spend per run", self.max_usd_per_run),
        ] {
            if let Some(v) = v {
                out.push(format!("{what} at most ${v:.2}"));
            }
        }
        if let Some(t) = self.max_tokens_per_day {
            out.push(format!("at most {t} tokens per day"));
        }
        if let Some(m) = self.max_turn_minutes {
            out.push(format!("a turn ends after {m} minutes"));
        }
        out
    }
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        "none".into()
    } else {
        items.join(", ")
    }
}

/// Managed mode as this process found it at start.
#[derive(Debug)]
pub struct State {
    pub source: Option<Source>,
    pub policy_path: Option<PathBuf>,
    pub bot_id: Option<String>,
    /// The policy, or why it couldn't be read.
    policy: std::result::Result<Policy, String>,
}

impl State {
    pub fn on(&self) -> bool {
        self.source.is_some()
    }

    /// Why the policy couldn't be read (the gateway won't start).
    pub fn policy_error(&self) -> Option<&str> {
        self.policy.as_ref().err().map(String::as_str)
    }
}

/// The switch: the env when it says 0 or 1, else the config.
pub fn decide(env: Option<&str>, cfg: &ManagedConfig) -> Option<Source> {
    match env.map(str::trim) {
        Some("1" | "true" | "yes" | "on") => Some(Source::Env),
        Some("0" | "false" | "no" | "off") => None,
        _ => cfg.enabled.then_some(Source::Config),
    }
}

/// Reads `[managed]` from `text` (a config file) without the rest, which
/// may not parse yet; anything unreadable is "not there".
fn section(text: &str) -> ManagedConfig {
    toml::from_str::<toml::Table>(text)
        .ok()
        .and_then(|t| t.get("managed").cloned())
        .and_then(|v| v.try_into().ok())
        .unwrap_or_default()
}

/// Works `State` out from the env and the config's `[managed]`, reading
/// the policy file when one is named.
pub fn resolve(env: &dyn Fn(&str) -> Option<String>, config_text: Option<&str>) -> State {
    let cfg = config_text.map(section).unwrap_or_default();
    let source = decide(env(ENV).as_deref(), &cfg);
    let nonempty = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
    // Under the env switch the panel owns the policy and the bot id: the
    // config is the user's, and mustn't point the bot at another policy.
    let from_cfg = source != Some(Source::Env);
    let policy_path = nonempty(env(POLICY_ENV))
        .map(PathBuf::from)
        .or(if from_cfg { cfg.policy.clone() } else { None });
    let bot_id = nonempty(env(BOT_ENV)).or(if from_cfg { cfg.bot_id.clone() } else { None });
    let policy = match (&source, &policy_path) {
        (None, _) | (_, None) => Ok(Policy::default()),
        (Some(_), Some(path)) => read_policy(path).map_err(|e| format!("{e:#}")),
    };
    State {
        source,
        policy_path,
        bot_id,
        policy,
    }
}

pub fn read_policy(path: &Path) -> Result<Policy> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("the policy file {} can't be read", path.display()))?;
    Policy::parse(&text).with_context(|| format!("the policy file {}", path.display()))
}

/// This process's managed state, read once.
pub fn state() -> &'static State {
    static STATE: OnceLock<State> = OnceLock::new();
    STATE.get_or_init(|| {
        let text = crate::config::config_path()
            .ok()
            .flatten()
            .and_then(|p| std::fs::read_to_string(p).ok());
        resolve(&|k| std::env::var(k).ok(), text.as_deref())
    })
}

pub fn on() -> bool {
    state().on()
}

/// The policy in force: `None` outside managed mode, and everything off
/// when the file named couldn't be read (fail closed).
pub fn policy() -> Option<Policy> {
    let s = state();
    if !s.on() {
        return None;
    }
    Some(match &s.policy {
        Ok(p) => p.clone(),
        Err(e) => Policy::closed(&format!("the policy couldn't be read: {e}")),
    })
}

/// For the gateway's start: a policy path that is set must read.
pub fn check() -> Result<()> {
    match state().policy_error() {
        Some(e) if on() => {
            bail!("managed mode: {e}. Fix or remove the policy file; nothing starts without it.")
        }
        _ => Ok(()),
    }
}

/// An error for something managed mode doesn't do.
pub fn refused(what: &str, why: &str) -> anyhow::Error {
    anyhow!("{what} is off on a managed bot: {why}")
}

/// Refuses `what` in managed mode.
pub fn forbid(what: &str, why: &str) -> Result<()> {
    if on() {
        return Err(refused(what, why));
    }
    Ok(())
}

/// A provider's kind, as a policy names it: `chatgpt` or `claude` for a
/// plan, a preset's name for its API host (`openai`, `anthropic`, …),
/// else the host itself.
pub fn provider_kind(p: &ProviderConfig) -> String {
    kind_of(p.plan, &p.base_url)
}

/// [`provider_kind`] from the two fields that decide it.
pub fn kind_of(plan: Option<Plan>, base_url: &str) -> String {
    match plan {
        Some(Plan::Chatgpt) => return "chatgpt".into(),
        Some(Plan::ClaudeCode) => return "claude".into(),
        None => {}
    }
    let host = host_of(base_url);
    for preset in crate::setup::presets() {
        if host_of(preset.base_url) == host {
            return preset.name.to_string();
        }
    }
    host
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default()
}

/// Narrows a loaded config to the policy (from `Config::finish`): the
/// Claude plan and forbidden providers go, caps become the lower of the
/// two, and what's off is off. Each drop is logged once per load.
pub fn apply(cfg: &mut Config) {
    let Some(policy) = policy() else { return };
    apply_policy(
        cfg,
        &policy,
        std::env::var(CHROME_SANDBOX_ENV).ok().as_deref(),
    );
}

pub fn apply_policy(cfg: &mut Config, policy: &Policy, chrome_sandbox: Option<&str>) {
    let why = policy.why();
    // Only the Claude plan is dropped. A provider the policy doesn't allow
    // stays, so the catalog can show it with the reason, and it is refused
    // where it is used.
    let mut dropped = Vec::new();
    cfg.providers.retain(|name, p| {
        let kind = provider_kind(p);
        if kind == "claude" {
            tracing::warn!("[providers.{name}] isn't used: {NO_CLAUDE_PLAN}");
            dropped.push(name.clone());
            return false;
        }
        if !policy.allows_provider(&kind) {
            tracing::warn!("[providers.{name}] ({kind}) isn't allowed on this bot: {why}");
        }
        true
    });
    if cfg
        .default_provider
        .as_ref()
        .is_some_and(|d| dropped.contains(d))
    {
        cfg.default_provider = None;
    }
    let t = &mut cfg.trust;
    let lower_f = |mine: f64, cap: Option<f64>| match cap {
        Some(c) if mine <= 0.0 || mine > c => c,
        _ => mine,
    };
    let lower_u = |mine: u64, cap: Option<u64>| match cap {
        Some(c) if mine == 0 || mine > c => c,
        _ => mine,
    };
    t.max_usd_per_day = lower_f(t.max_usd_per_day, policy.max_usd_per_day);
    t.max_usd_per_run = lower_f(t.max_usd_per_run, policy.max_usd_per_run);
    t.max_tokens_per_day = lower_u(t.max_tokens_per_day, policy.max_tokens_per_day);
    cfg.health.max_turn_minutes = lower_u(cfg.health.max_turn_minutes, policy.max_turn_minutes);
    if !policy.browser {
        cfg.browser.enabled = false;
    }
    match chrome_sandbox.map(str::trim) {
        Some("0") => cfg.browser.chrome_sandbox = false,
        Some("1") => cfg.browser.chrome_sandbox = true,
        _ => {}
    }
    if !policy.extensions {
        cfg.extensions.enabled = false;
        cfg.extensions.allow.clear();
        for s in &cfg.mcp.servers {
            tracing::warn!(
                "[[mcp.servers]] `{}` doesn't run: extensions is off on a managed bot: {why}",
                s.name
            );
        }
        cfg.mcp.servers.clear();
    } else {
        for s in cfg.mcp.servers.iter_mut().filter(|s| !s.sandbox) {
            s.sandbox = true;
            tracing::warn!(
                "mcp.servers `{}`: sandbox = false is ignored on a managed bot",
                s.name
            );
        }
    }
    if let Some(h) = cfg.gateway.http.as_mut() {
        if h.public.as_deref() == Some("tunnel") {
            h.public = None;
            tracing::warn!(
                "[gateway.http] public = \"tunnel\" is off on a managed bot: the panel's proxy is the way in"
            );
        }
    }
    // No remote workspaces: keys to other machines leave the container.
    cfg.ssh.clear();
    if cfg
        .workspace
        .as_deref()
        .is_some_and(|w| w.starts_with("ssh:"))
    {
        cfg.workspace = None;
    }
    cfg.dashboard.enabled = true;
    if cfg.dashboard.remote == "tunnel" {
        cfg.dashboard.remote = "off".into();
    }
}

/// Whether commands may run under `sandbox` here: always outside managed
/// mode (as before M44), else only with the OS sandbox holding them or the
/// policy saying the container is enough. `Err` says why not, in words.
pub fn commands(sandbox_active: bool, degraded: Option<&str>) -> std::result::Result<(), String> {
    let Some(policy) = policy() else {
        return Ok(());
    };
    commands_under(&policy, sandbox_active, degraded)
}

pub fn commands_under(
    policy: &Policy,
    sandbox_active: bool,
    degraded: Option<&str>,
) -> std::result::Result<(), String> {
    if sandbox_active || policy.sandbox == SandboxNeed::Container {
        return Ok(());
    }
    Err(format!(
        "commands are off on this bot: ferrule's OS sandbox didn't start here ({}), and the policy doesn't allow running them without it",
        degraded.unwrap_or("no backend, or `[sandbox] mode = \"off\"` in the config")
    ))
}

/// What protects the agent's commands, in words (doctor, `/healthz`).
pub fn protection(sandbox: &ferrule_sandbox::Sandbox) -> String {
    let policy = policy();
    if policy.as_ref().is_some_and(|p| !p.shell) {
        return "no shell: the policy turns it off".into();
    }
    if sandbox.is_active() {
        return match sandbox.backend() {
            ferrule_sandbox::Backend::Landlock { abi } => {
                format!("ferrule's OS sandbox (Landlock ABI {abi} + seccomp)")
            }
            b => format!("ferrule's OS sandbox ({b:?})"),
        };
    }
    match policy {
        Some(p) if p.sandbox == SandboxNeed::Container => {
            "the container only (policy sandbox = \"container\"): ferrule's OS sandbox isn't running".into()
        }
        Some(_) => "nothing, so the shell, hooks and the browser are off".into(),
        None => "nothing: commands run unsandboxed".into(),
    }
}

/// A provider's kind under `policy`, or why it's refused: the Claude plan
/// always, and a kind the policy doesn't list.
pub fn kind_refusal_under(policy: &Policy, name: &str, kind: &str) -> Option<String> {
    if kind == "claude" {
        return Some(NO_CLAUDE_PLAN.into());
    }
    if !policy.allows_provider(kind) {
        return Some(format!(
            "provider `{name}` is not allowed on this bot: {}",
            policy.why()
        ));
    }
    None
}

/// [`kind_refusal_under`] with this process's policy; `None` outside
/// managed mode.
pub fn kind_refusal(name: &str, kind: &str) -> Option<String> {
    kind_refusal_under(&policy()?, name, kind)
}

/// Why the policy doesn't let `name` (a channel) run, or `None`.
pub fn channel_refusal(name: &str) -> Option<String> {
    let p = policy()?;
    (!p.allows_channel(name))
        .then(|| format!("channel `{name}` is not allowed on this bot: {}", p.why()))
}

pub fn provider_refusal(name: &str, p: &ProviderConfig) -> Option<String> {
    kind_refusal(name, &provider_kind(p))
}

/// Provider name → the policy's reason, for each one it doesn't allow.
/// Empty outside managed mode.
pub fn blocked_providers(cfg: &Config) -> BTreeMap<String, String> {
    let Some(policy) = policy() else {
        return BTreeMap::new();
    };
    cfg.providers
        .iter()
        .filter(|(_, p)| !policy.allows_provider(&provider_kind(p)))
        .map(|(name, _)| (name.clone(), policy.why()))
        .collect()
}

static PANEL: OnceLock<Vec<u8>> = OnceLock::new();

/// Takes `FERRULE_PANEL_SECRET` out of the environment (so no child
/// process inherits it) and keeps it in memory. Call before any thread
/// starts. A secret under 32 bytes is refused.
pub fn take_panel_secret() {
    let v = std::env::var(PANEL_SECRET_ENV).ok();
    // `main` calls this before the runtime or any thread starts.
    std::env::remove_var(PANEL_SECRET_ENV);
    match v.map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
        Some(v) if v.len() >= 32 => {
            let _ = PANEL.set(v.into_bytes());
        }
        Some(_) => eprintln!(
            "ferrule: FERRULE_PANEL_SECRET is shorter than 32 bytes, so panel sign-in is off"
        ),
        None => {}
    }
}

pub fn panel_secret() -> Option<&'static [u8]> {
    PANEL.get().map(Vec::as_slice)
}

pub fn shell_off_under(p: &Policy) -> Option<String> {
    (!p.shell).then(|| format!("the policy turns the shell off: {}", p.why()))
}

/// Why the shell is off, when it is.
pub fn shell_off() -> Option<String> {
    shell_off_under(&policy()?)
}

/// Why the user's commands (shell, verify, lint, gates, a transcription
/// command) don't run: the sandbox refuses them, or the policy has no shell.
pub fn user_commands_off(sandbox: &ferrule_sandbox::Sandbox) -> Option<String> {
    sandbox.refusal().map(str::to_string).or_else(shell_off)
}

/// Hooks always run outside the sandbox, so under `sandbox = "os"` they'd
/// be the one unsandboxed door.
pub fn hooks_off_under(p: &Policy) -> Option<String> {
    if let Some(why) = shell_off_under(p) {
        return Some(why);
    }
    (p.sandbox == SandboxNeed::Os).then(|| {
        format!(
            "hooks run outside ferrule's OS sandbox, and the policy keeps commands inside it (sandbox = \"os\"): {}",
            p.why()
        )
    })
}

pub fn hooks_off() -> Option<String> {
    hooks_off_under(&policy()?)
}

/// A sandbox that refuses commands when managed mode says they can't run
/// here (see [`commands`]).
pub fn guard(sandbox: ferrule_sandbox::Sandbox) -> ferrule_sandbox::Sandbox {
    match commands(sandbox.is_active(), sandbox.degraded()) {
        Ok(()) => sandbox,
        Err(why) => sandbox.refuse(why),
    }
}

/// The paths commands and the file tools may not read: the policy file.
pub fn hidden_for(s: &State) -> Vec<PathBuf> {
    match (&s.policy_path, s.on()) {
        (Some(p), true) => vec![p.clone()],
        _ => Vec::new(),
    }
}

/// Why a cap change isn't allowed under `p`: no raising, and no 0 (which
/// means "no cap") when the policy sets one.
pub fn cap_refusal_under(p: &Policy, key: &str, new: f64) -> Option<String> {
    let (cap, usd) = match key {
        "max_usd_per_day" => (p.max_usd_per_day?, true),
        "max_usd_per_run" => (p.max_usd_per_run?, true),
        "max_tokens_per_day" => (p.max_tokens_per_day? as f64, false),
        _ => return None,
    };
    let shown = if usd {
        format!("${cap:.2}")
    } else {
        format!("{}", cap as u64)
    };
    if new == 0.0 {
        return Some(format!(
            "{key} can't be 0 (no cap) on this bot; the most is {shown}: {}",
            p.why()
        ));
    }
    (new > cap).then(|| format!("{key} can't go above {shown} on this bot: {}", p.why()))
}

pub fn cap_refusal(key: &str, new: f64) -> Option<String> {
    cap_refusal_under(&policy()?, key, new)
}

/// A first start's config: `[dashboard]` from the env, the browser when
/// the image has one. Nothing else: the rest is the dashboard's.
pub fn starter_config(browser: bool) -> String {
    let mut out = String::from(
        "# Written by ferrule on a managed bot's first start. Set it up from the dashboard.\n\
         # The panel's policy (FERRULE_POLICY) decides what the bot may do; editing\n\
         # this file doesn't change that.\n",
    );
    if browser {
        out.push_str("\n[browser]\nenabled = true\n");
    }
    out
}

/// Writes the starter config when managed mode is on and `path` doesn't
/// exist yet. Whether it wrote one.
pub fn first_start(path: &Path) -> Result<bool> {
    if !on() || path.exists() {
        return Ok(false);
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let browser = std::env::var(BROWSER_ENV).is_ok_and(|v| v.trim() == "1");
    std::fs::write(path, starter_config(browser))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn the_env_decides_over_the_config() {
        let on = ManagedConfig {
            enabled: true,
            ..Default::default()
        };
        let off = ManagedConfig::default();
        assert_eq!(decide(Some("1"), &off), Some(Source::Env));
        assert_eq!(decide(Some("0"), &on), None);
        assert_eq!(decide(None, &on), Some(Source::Config));
        assert_eq!(decide(Some(""), &on), Some(Source::Config));
        assert_eq!(decide(None, &off), None);
    }

    #[test]
    fn the_policy_path_and_bot_come_from_the_env_first() {
        let dir = tempfile::tempdir().unwrap();
        let policy = dir.path().join("policy.toml");
        std::fs::write(&policy, "shell = false\nreason = \"beta\"\n").unwrap();
        let text = "[managed]\nenabled = true\nbot_id = \"b_cfg\"\npolicy = \"/nope\"\n";
        let s = resolve(
            &env_of(&[(POLICY_ENV, policy.to_str().unwrap()), (BOT_ENV, "b_env")]),
            Some(text),
        );
        assert_eq!(s.source, Some(Source::Config));
        assert_eq!(s.bot_id.as_deref(), Some("b_env"));
        let p = s.policy.unwrap();
        assert!(!p.shell && p.browser);
        assert_eq!(p.why(), "beta");
        // The config's path, missing: fail closed, with the reason.
        let s = resolve(&env_of(&[]), Some(text));
        assert!(s.policy_error().unwrap().contains("/nope"), "{s:?}");
    }

    #[test]
    fn a_misspelt_or_nonsense_policy_is_refused() {
        assert!(Policy::parse("shel = false").is_err());
        assert!(Policy::parse("max_usd_per_day = -1").is_err());
        assert!(Policy::parse("max_turn_minutes = 0").is_err());
        assert!(Policy::parse("sandbox = \"none\"").is_err());
        let p = Policy::parse("sandbox = \"container\"\nproviders = [\"openai\"]").unwrap();
        assert_eq!(p.sandbox, SandboxNeed::Container);
        assert!(p.allows_provider("OpenAI") && !p.allows_provider("openrouter"));
        assert!(!Policy::default().allows_provider("claude"));
        assert!(Policy::default().allows_channel("telegram"));
    }

    #[test]
    fn a_broken_config_still_has_its_managed_section_read() {
        let s = resolve(
            &env_of(&[]),
            Some("[managed]\nenabled = true\n[gateway]\nnot_a_key = 1\n"),
        );
        assert!(s.on());
        let s = resolve(&env_of(&[]), Some("this is not toml ["));
        assert!(!s.on());
    }

    fn cfg(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn the_config_is_narrowed_to_the_policy() {
        let mut c = cfg(r#"
default_provider = "cc"
[providers.oa]
base_url = "https://api.openai.com/v1"
api_key_env = "OA"
model = "gpt-5"
[providers.or]
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OR"
model = "x"
[providers.cc]
plan = "claude-code"
model = "sonnet"
[trust]
max_usd_per_day = 50.0
max_usd_per_run = 0.5
[browser]
enabled = true
[extensions]
enabled = true
[[mcp.servers]]
name = "x"
command = "x"
[ssh.app]
host = "h"
path = "/srv"
[dashboard]
remote = "tunnel"
[gateway.http]
public = "tunnel"
"#);
        let p = Policy::parse(
            "providers = [\"openai\", \"chatgpt\"]\nmax_usd_per_day = 5\nmax_usd_per_run = 1\n\
             max_turn_minutes = 10\nbrowser = false\nextensions = false\n",
        )
        .unwrap();
        apply_policy(&mut c, &p, Some("0"));
        let mut kept: Vec<_> = c.providers.keys().collect();
        kept.sort();
        assert_eq!(kept, ["oa", "or"]);
        assert_eq!(c.default_provider, None);
        assert_eq!(c.trust.max_usd_per_day, 5.0);
        assert_eq!(c.trust.max_usd_per_run, 0.5, "the lower one wins");
        assert_eq!(c.health.max_turn_minutes, 10);
        assert!(!c.browser.enabled && !c.browser.chrome_sandbox);
        assert!(!c.extensions.enabled && c.mcp.servers.is_empty());
        assert!(c.ssh.is_empty());
        assert_eq!(c.dashboard.remote, "off");
        assert_eq!(c.gateway.http.as_ref().unwrap().public, None);

        // With extensions on, an unsandboxed MCP server is sandboxed.
        let mut c = cfg("[[mcp.servers]]\nname = \"x\"\ncommand = \"x\"\nsandbox = false\n");
        apply_policy(&mut c, &Policy::default(), None);
        assert!(c.mcp.servers[0].sandbox);
    }

    #[test]
    fn under_the_env_the_configs_policy_and_bot_are_ignored() {
        let text = "[managed]\npolicy = \"/nope\"\nbot_id = \"b_cfg\"\n";
        let s = resolve(&env_of(&[(ENV, "1")]), Some(text));
        assert_eq!(s.source, Some(Source::Env));
        assert_eq!(s.policy_path, None);
        assert_eq!(s.bot_id, None);
        assert_eq!(s.policy, Ok(Policy::default()));
    }

    #[test]
    fn a_forbidden_provider_is_refused_with_the_policys_reason() {
        let p = Policy::parse("reason = \"beta\"\nproviders = [\"openai\"]").unwrap();
        let e = kind_refusal_under(&p, "or", "openrouter").unwrap();
        assert!(e.contains("beta") && e.contains("`or`"), "{e}");
        assert_eq!(
            kind_refusal_under(&p, "cc", "claude").as_deref(),
            Some(NO_CLAUDE_PLAN)
        );
        assert_eq!(kind_refusal_under(&p, "oa", "openai"), None);
    }

    #[test]
    fn hooks_need_the_container_word_and_the_shell() {
        assert!(hooks_off_under(&Policy::default()).is_some());
        let c = Policy::parse("sandbox = \"container\"").unwrap();
        assert_eq!(hooks_off_under(&c), None);
        let c = Policy::parse("sandbox = \"container\"\nshell = false").unwrap();
        assert!(hooks_off_under(&c).unwrap().contains("turns the shell off"));
    }

    #[test]
    fn caps_can_only_go_down() {
        let p = Policy::parse("max_usd_per_day = 5\nmax_tokens_per_day = 1000").unwrap();
        assert_eq!(cap_refusal_under(&p, "max_usd_per_day", 3.0), None);
        assert!(cap_refusal_under(&p, "max_usd_per_day", 6.0)
            .unwrap()
            .contains("can't go above $5.00"));
        assert!(cap_refusal_under(&p, "max_usd_per_day", 0.0)
            .unwrap()
            .contains("can't be 0 (no cap)"));
        assert!(cap_refusal_under(&p, "max_tokens_per_day", 2000.0)
            .unwrap()
            .contains("1000"));
        assert_eq!(cap_refusal_under(&p, "max_usd_per_run", 99.0), None);
    }

    #[test]
    fn the_policy_file_is_hidden_from_commands() {
        let dir = tempfile::tempdir().unwrap();
        let policy = dir.path().join("policy.toml");
        std::fs::write(&policy, "shell = false\n").unwrap();
        let path = policy.to_str().unwrap();
        let on = resolve(&env_of(&[(ENV, "1"), (POLICY_ENV, path)]), None);
        assert_eq!(hidden_for(&on), [PathBuf::from(path)]);
        let off = resolve(&env_of(&[(POLICY_ENV, path)]), None);
        assert!(hidden_for(&off).is_empty());
    }

    #[test]
    fn commands_need_the_os_sandbox_or_the_policys_word() {
        let os = Policy::default();
        let container = Policy {
            sandbox: SandboxNeed::Container,
            ..Policy::default()
        };
        assert!(commands_under(&os, true, None).is_ok());
        let e = commands_under(&os, false, Some("Landlock isn't available")).unwrap_err();
        assert!(e.contains("Landlock isn't available"), "{e}");
        assert!(commands_under(&container, false, None).is_ok());
    }

    #[test]
    fn a_closed_policy_allows_nothing() {
        let p = Policy::closed("unreadable");
        assert!(!p.shell && !p.browser && !p.extensions);
        assert!(!p.allows_provider("openai") && !p.allows_channel("http"));
        assert_eq!(p.why(), "unreadable");
    }

    #[test]
    fn the_starter_config_parses() {
        for b in [false, true] {
            let c: Config = toml::from_str(&starter_config(b)).unwrap();
            assert_eq!(c.browser.enabled, b);
        }
    }
}
