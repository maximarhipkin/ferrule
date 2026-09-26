//! `ferrule setup`: the interactive installer. The first run walks through
//! a model provider and its key, Telegram, Discord, Slack, tool credentials, web search, the sandbox,
//! the browser and the background service; later runs open a menu to change any one
//! part. Answers are checked live where they can be (the key opens the
//! model list, the bot token answers `getMe`) and saved the moment they're
//! confirmed, so Ctrl-C never loses what's done. Keys go to the private
//! secrets file, never into the config, and config edits keep the file's
//! comments and layout.

mod channels;
mod local;
mod remote;

use crate::{browser, config, probe, secrets, service};
use anyhow::{anyhow, bail, Context, Result};
use ferrule_sandbox::{Mode, Sandbox};
use ferrule_tools::search::SearchProvider;
use inquire::validator::Validation;
use inquire::{
    Confirm, CustomUserError, InquireError, MultiSelect, Password, PasswordDisplayMode, Select,
    Text,
};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use toml_edit::{DocumentMut, Item, TableLike, Value};

const HEADER: &str = "\
# ferrule configuration, written by `ferrule setup`. Re-run it to change any
# part, or edit this file by hand (`ferrule config edit`). API keys aren't
# in here: setup keeps them in a private file of their own.
# Every option, commented: `ferrule config example`.
";

pub async fn run() -> Result<()> {
    if !has_terminal() {
        bail!("`ferrule setup` asks questions, so it needs a terminal (from a script: `ferrule setup < /dev/tty`)");
    }
    let path = match config::config_path()? {
        Some(path) => path,
        None => config::global_config_path()?,
    };
    let mut t = Target::load(path)?;
    let http = probe::client();
    let first = t.config()?.providers.is_empty();
    println!("ferrule setup · config: {}", tilde(&t.path));
    println!("Esc skips a question, Ctrl-C stops. Each answer is saved once you confirm it.");
    let finished = if first {
        guided(&mut t, &http).await?
    } else {
        menu(&mut t, &http).await?
    };
    if !finished {
        println!(
            "\nSetup stopped. What you confirmed is saved; `ferrule setup` picks up from there."
        );
        return Ok(());
    }
    let cfg = t.config()?;
    println!("\nAll set. Config: {}", tilde(&t.path));
    if !cfg.providers.is_empty() {
        println!("  ferrule chat      talk to the agent in this terminal");
    }
    println!("  ferrule doctor    check that everything works");
    println!("  ferrule setup     change any of this later");
    Ok(())
}

/// Is there a terminal to ask on? inquire reads `/dev/tty` when stdin
/// isn't one, so `curl … | sh` installers work with `< /dev/tty` or even
/// without it.
pub(crate) fn has_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() || (cfg!(unix) && std::fs::File::open("/dev/tty").is_ok())
}

/// The first run: every part in order.
async fn guided(t: &mut Target, http: &reqwest::Client) -> Result<bool> {
    if !crate::import::detected().is_empty() {
        heading("Import");
        if settle(crate::import::setup_step(t, true).await)?.quit() {
            return Ok(false);
        }
    }
    heading("Model provider");
    if settle(provider_step(t, http, true).await)?.quit() {
        return Ok(false);
    }
    heading("Telegram");
    if settle(telegram_step(t, http, true).await)?.quit() {
        return Ok(false);
    }
    heading("Discord");
    if settle(channels::discord_step(t, true).await)?.quit() {
        return Ok(false);
    }
    heading("Slack");
    if settle(channels::slack_step(t, true).await)?.quit() {
        return Ok(false);
    }
    heading("Tool credentials");
    if settle(credentials_step(t, http, true).await)?.quit() {
        return Ok(false);
    }
    heading("Web search");
    if settle(web_search_step(t, true))?.quit() {
        return Ok(false);
    }
    heading("Memory recall");
    if settle(memory_step(t, true).await)?.quit() {
        return Ok(false);
    }
    heading("Sandbox");
    if settle(sandbox_step(t, true))?.quit() {
        return Ok(false);
    }
    heading("Network policy");
    if settle(network_step(t, true))?.quit() {
        return Ok(false);
    }
    heading("Browser");
    if settle(browser_step(t))?.quit() {
        return Ok(false);
    }
    heading("MCP servers");
    if settle(crate::mcp_add::setup_step(t, true).await)?.quit() {
        return Ok(false);
    }
    let gw = t.config()?.gateway;
    if gw.telegram_token_env.is_some()
        || gw.discord_token_env.is_some()
        || gw.slack_bot_token_env.is_some()
    {
        heading("Background service");
        if settle(service_step(t, true))?.quit() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Later runs: pick a part, change it, back to the menu.
async fn menu(t: &mut Target, http: &reqwest::Client) -> Result<bool> {
    loop {
        let cfg = t.config()?;
        let service = service::status();
        let items = vec![
            format!("Model provider       {}", provider_summary(&cfg)),
            format!("Telegram             {}", telegram_summary(&cfg)),
            format!("Discord              {}", channels::discord_summary(&cfg)),
            format!("Slack                {}", channels::slack_summary(&cfg)),
            format!("Tool credentials     {}", credentials_summary(&cfg)),
            format!("Web search           {}", web_search_summary(&cfg)),
            format!("Memory recall        {}", memory_summary(&cfg)),
            format!("Sandbox              {}", sandbox_summary(&cfg)),
            format!("Network policy       {}", network_summary(&cfg)),
            format!("Browser              {}", browser_summary(&cfg)),
            format!("MCP servers          {}", mcp_summary(&cfg)),
            format!("Import               {}", crate::import::setup_summary()),
            format!("Remote workspace     {}", remote::summary(&cfg)),
            format!("Background service   {}", service_summary(&service)),
            "Done".to_string(),
        ];
        let done = items.len() - 1;
        let pick = match Select::new("What do you want to change?", items)
            .with_page_size(done + 1)
            .raw_prompt()
        {
            Ok(choice) => choice.index,
            Err(InquireError::OperationCanceled) => done,
            Err(InquireError::OperationInterrupted) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let result = match pick {
            0 => provider_step(t, http, false).await,
            1 => telegram_step(t, http, false).await,
            2 => channels::discord_step(t, false).await,
            3 => channels::slack_step(t, false).await,
            4 => credentials_step(t, http, false).await,
            5 => web_search_step(t, false),
            6 => memory_step(t, false).await,
            7 => sandbox_step(t, false),
            8 => network_step(t, false),
            9 => browser_step(t),
            10 => crate::mcp_add::setup_step(t, false).await,
            11 => crate::import::setup_step(t, false).await,
            12 => remote::step(t).await,
            13 => service_step(t, false),
            _ => break,
        };
        if settle(result)?.quit() {
            return Ok(false);
        }
    }
    if t.changed
        && matches!(
            service::status(),
            service::Status::Installed { running: true, .. }
        )
    {
        let restart = Confirm::new(
            "The background service is running. Restart it so it picks up the changes?",
        )
        .with_default(true)
        .prompt();
        if let Ok(true) = restart {
            service::restart()?;
            ok("service restarted");
        }
    }
    Ok(true)
}

enum Flow {
    Continue,
    Quit,
}

impl Flow {
    fn quit(&self) -> bool {
        matches!(self, Flow::Quit)
    }
}

/// Esc skips the rest of a step, Ctrl-C ends setup, and any other error is
/// shown and setup goes on — whatever was saved before it stays saved.
fn settle(result: Result<()>) -> Result<Flow> {
    let Err(e) = result else {
        return Ok(Flow::Continue);
    };
    match e.downcast_ref::<InquireError>() {
        Some(InquireError::OperationCanceled) => {
            println!("  (skipped)");
            Ok(Flow::Continue)
        }
        Some(InquireError::OperationInterrupted) => Ok(Flow::Quit),
        Some(InquireError::NotTTY) => bail!("`ferrule setup` needs a terminal to ask on"),
        _ => {
            println!("  ✗ {e:#}");
            Ok(Flow::Continue)
        }
    }
}

/// Ctrl-C while waiting on the network ends setup, the way it does at a
/// prompt.
async fn interruptible<T>(fut: impl Future<Output = T>) -> Result<T> {
    tokio::select! {
        out = fut => Ok(out),
        _ = tokio::signal::ctrl_c() => Err(InquireError::OperationInterrupted.into()),
    }
}

pub(crate) fn heading(title: &str) {
    println!("\n── {title}");
}

pub(crate) fn ok(text: impl std::fmt::Display) {
    println!("  ✓ {text}");
}

pub(crate) fn warn(text: impl std::fmt::Display) {
    println!("  ! {text}");
}

pub(crate) fn info(text: impl std::fmt::Display) {
    println!("  {text}");
}

/// `~/…` for paths under the home directory.
pub fn tilde(path: &Path) -> String {
    let rest =
        dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf));
    match rest {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

// ── The config file ────────────────────────────────────────────────────

/// The config file being edited, as a document that keeps comments.
pub(crate) struct Target {
    pub(crate) path: PathBuf,
    doc: DocumentMut,
    /// Something was saved this run (config or a key): worth a restart.
    changed: bool,
}

impl Target {
    pub(crate) fn load(path: PathBuf) -> Result<Self> {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let broken = || {
            format!(
                "{} doesn't parse; fix it by hand, or move it away and run setup again",
                path.display()
            )
        };
        let doc: DocumentMut = text.parse().with_context(broken)?;
        let t = Self {
            path: path.clone(),
            doc,
            changed: false,
        };
        t.config().with_context(broken)?;
        Ok(t)
    }

    /// Read the file again after something else wrote it, as a change.
    pub(crate) fn reload(&mut self) -> Result<()> {
        *self = Self {
            changed: true,
            ..Self::load(self.path.clone())?
        };
        Ok(())
    }

    pub(crate) fn config(&self) -> Result<config::Config> {
        let cfg: config::Config =
            toml::from_str(&self.doc.to_string()).map_err(|e| anyhow!("{e}"))?;
        cfg.finish()
    }

    pub(crate) fn root(&mut self) -> &mut dyn TableLike {
        self.doc.as_table_mut()
    }

    /// Check the edit still makes a valid config, then write it in one go.
    pub(crate) fn save(&mut self) -> Result<()> {
        self.save_then(|| Ok(()))
    }

    /// [`Self::save`], running `before` once the new text is checked and
    /// written next to the config, just before it replaces it. If `before`
    /// fails the config is left as it was.
    pub(crate) fn save_then(&mut self, before: impl FnOnce() -> Result<()>) -> Result<()> {
        self.config()
            .context("that change would break the config, so it wasn't saved")?;
        let mut text = self.doc.to_string();
        if !text.trim_start().starts_with('#') {
            text = format!("{HEADER}\n{}", text.trim_start());
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let tmp = self
            .path
            .with_extension(format!("toml.tmp-{}", std::process::id()));
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        let done = before().and_then(|()| {
            crate::filewrite::replace(&tmp, &self.path)
                .with_context(|| format!("writing {}", self.path.display()))
        });
        if done.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        done?;
        self.changed = true;
        Ok(())
    }

    fn set_secret(&mut self, name: &str, value: &str) -> Result<()> {
        secrets::set(&secrets::path()?, name, value)?;
        self.changed = true;
        if secrets::source(name) == secrets::Source::Env
            && std::env::var(name).is_ok_and(|v| v != value)
        {
            warn(format!(
                "${name} is also set in your shell, and that value wins over the saved one. \
                 Unset it (and drop it from your shell profile) to use this one."
            ));
        }
        Ok(())
    }

    /// Delete a saved value unless something else in the config still
    /// reads that variable.
    fn forget_secret(&mut self, name: &str) -> Result<()> {
        let cfg = self.config()?;
        let used = cfg.providers.values().any(|p| p.api_key_env == name)
            || cfg.gateway.telegram_token_env.as_deref() == Some(name)
            || cfg.gateway.discord_token_env.as_deref() == Some(name)
            || cfg.gateway.slack_bot_token_env.as_deref() == Some(name)
            || cfg.gateway.slack_app_token_env.as_deref() == Some(name)
            || cfg.secrets.contains_key(name);
        if !used {
            secrets::remove(&secrets::path()?, name)?;
            self.changed = true;
        }
        Ok(())
    }
}

/// The table at `path`, created (as implicit, so an empty one isn't
/// written) where missing.
pub(crate) fn table<'a>(
    root: &'a mut dyn TableLike,
    path: &[&str],
) -> Result<&'a mut dyn TableLike> {
    let mut tbl = root;
    for key in path {
        let item = tbl.entry(key).or_insert_with(|| {
            let mut new = toml_edit::Table::new();
            new.set_implicit(true);
            Item::Table(new)
        });
        tbl = item
            .as_table_like_mut()
            .ok_or_else(|| anyhow!("`{key}` in the config isn't a table"))?;
    }
    Ok(tbl)
}

/// Set `key`, keeping the old value's comment and spacing.
pub(crate) fn put(tbl: &mut dyn TableLike, key: &str, new: impl Into<Value>) {
    let mut new = new.into();
    if let Some(old) = tbl.get(key).and_then(Item::as_value) {
        *new.decor_mut() = old.decor().clone();
    }
    match tbl.get_mut(key) {
        Some(item) => *item = Item::Value(new),
        None => {
            tbl.insert(key, Item::Value(new));
        }
    }
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Off => "off",
        Mode::ReadOnly => "read-only",
        Mode::WorkspaceWrite => "workspace-write",
    }
}

fn key_is_set(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.is_empty())
}

// ── Menu labels ────────────────────────────────────────────────────────

fn provider_summary(cfg: &config::Config) -> String {
    let cat = crate::models::Catalog::from_config(cfg);
    if cfg.models.default.is_some() {
        if let Ok((e, _)) = cat.default_entry() {
            let mut text = e.reference();
            if !key_is_set(&e.key_env) {
                text.push_str(" · key missing");
            }
            if cat.entries.len() > 1 {
                text.push_str(&format!(" (+{} more)", cat.entries.len() - 1));
            }
            return text;
        }
    }
    let Some(name) = cfg
        .default_provider
        .as_ref()
        .filter(|n| cfg.providers.contains_key(*n))
    else {
        return if cfg.providers.is_empty() {
            "none yet".into()
        } else {
            "no default set".into()
        };
    };
    let p = &cfg.providers[name];
    let mut text = format!("{name} · {}", p.model);
    if !key_is_set(&p.api_key_env) {
        text.push_str(" · key missing");
    }
    if cat.entries.len() > 1 {
        text.push_str(&format!(" (+{} more)", cat.entries.len() - 1));
    }
    text
}

fn telegram_summary(cfg: &config::Config) -> String {
    match &cfg.gateway.telegram_token_env {
        None => "off".into(),
        Some(env) if !key_is_set(env) => "token missing".into(),
        Some(_) => format!(
            "on · {} allowed",
            plural(cfg.gateway.telegram_allowed_chats.len(), "chat", "chats")
        ),
    }
}

fn credentials_summary(cfg: &config::Config) -> String {
    if cfg.secrets.is_empty() {
        "none".into()
    } else {
        cfg.secrets.keys().cloned().collect::<Vec<_>>().join(", ")
    }
}

fn web_search_summary(cfg: &config::Config) -> String {
    match cfg.web_search.settings() {
        Ok(Some(s)) => match &s.key_env {
            Some(var) if !key_is_set(var) => format!("{} · key missing", s.provider.name()),
            _ => s.provider.name().to_string(),
        },
        _ => "off".into(),
    }
}

fn memory_summary(cfg: &config::Config) -> String {
    match cfg.memory.embedder.as_str() {
        "local" => "keywords + local model".into(),
        "openai" => format!(
            "keywords + {}",
            cfg.memory.model.as_deref().unwrap_or("endpoint")
        ),
        _ => "keywords".into(),
    }
}

fn sandbox_summary(cfg: &config::Config) -> String {
    let network = if cfg.sandbox.network {
        "network on"
    } else {
        "network off"
    };
    format!("{} · {network}", mode_name(cfg.sandbox.mode))
}

fn network_summary(cfg: &config::Config) -> String {
    let e = &cfg.egress;
    let open = if e.default == "deny" {
        format!(
            "{} allowed, the rest refused",
            plural(e.allow.len(), "host", "hosts")
        )
    } else if e.deny.is_empty() {
        "public hosts".to_string()
    } else {
        format!("public hosts but {}", plural(e.deny.len(), "rule", "rules"))
    };
    let private = if e.private == "allow" {
        "private ranges open"
    } else {
        "private ranges blocked"
    };
    format!("{open} · {private}")
}

fn browser_summary(cfg: &config::Config) -> String {
    match (cfg.browser.enabled, cfg.browser.chrome_sandbox) {
        (false, _) => "off".into(),
        (true, true) => "on".into(),
        (true, false) => "on · without Chrome's own sandbox".into(),
    }
}

fn mcp_summary(cfg: &config::Config) -> String {
    match cfg.mcp.servers.len() {
        0 => "none".into(),
        n if n <= 3 => cfg
            .mcp
            .servers
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        n => plural(n, "server", "servers"),
    }
}

fn service_summary(status: &service::Status) -> String {
    match status {
        service::Status::Unsupported(_) => "not available here".into(),
        service::Status::NotInstalled => "not installed".into(),
        service::Status::Installed { running: true, .. } => "running".into(),
        service::Status::Installed { running: false, .. } => "installed, not running".into(),
    }
}

// ── Prompts ────────────────────────────────────────────────────────────

/// A masked prompt. With `keep`, Enter keeps the saved value (`None`).
/// `shape` returns why a value can't be right, if it can tell.
pub(crate) fn ask_secret(
    prompt: &str,
    keep: bool,
    shape: fn(&str) -> Option<&'static str>,
) -> Result<Option<String>> {
    let validator = move |input: &str| -> Result<Validation, CustomUserError> {
        let value = input.trim();
        Ok(if value.is_empty() {
            if keep {
                Validation::Valid
            } else {
                Validation::Invalid("required (Esc skips)".into())
            }
        } else if value.contains(char::is_whitespace) {
            Validation::Invalid("it can't contain spaces".into())
        } else if let Some(why) = shape(value) {
            Validation::Invalid(why.into())
        } else {
            Validation::Valid
        })
    };
    let mut prompt = Password::new(prompt)
        .without_confirmation()
        .with_display_mode(PasswordDisplayMode::Masked)
        .with_validator(validator);
    if keep {
        prompt = prompt.with_help_message("Enter keeps the saved one");
    }
    let value = prompt.prompt()?;
    let value = value.trim();
    Ok((!value.is_empty()).then(|| value.to_string()))
}

pub(crate) fn no_shape(_: &str) -> Option<&'static str> {
    None
}

// ── Model provider ─────────────────────────────────────────────────────

pub(crate) struct Preset {
    label: &'static str,
    pub(crate) name: &'static str,
    pub(crate) base_url: &'static str,
    pub(crate) key_env: &'static str,
    pub(crate) profile: &'static str,
    /// Empty: pick from the provider's list.
    pub(crate) model: &'static str,
    /// Where to get a key. Empty: none needed (a local server).
    key_url: &'static str,
}

const PRESETS: &[Preset] = &[
    Preset {
        label: "OpenAI",
        name: "openai",
        base_url: "https://api.openai.com/v1",
        key_env: "OPENAI_API_KEY",
        profile: "openai",
        model: "gpt-5.2",
        key_url: "https://platform.openai.com/api-keys",
    },
    Preset {
        label: "Moonshot (Kimi)",
        name: "kimi",
        base_url: "https://api.moonshot.ai/v1",
        key_env: "MOONSHOT_API_KEY",
        profile: "kimi",
        model: "kimi-k2.6",
        key_url: "https://platform.moonshot.ai/console/api-keys",
    },
    Preset {
        label: "OpenRouter (hundreds of models, one key)",
        name: "openrouter",
        base_url: "https://openrouter.ai/api/v1",
        key_env: "OPENROUTER_API_KEY",
        profile: "generic",
        model: "",
        key_url: "https://openrouter.ai/keys",
    },
    Preset {
        label: "DeepSeek",
        name: "deepseek",
        base_url: "https://api.deepseek.com/v1",
        key_env: "DEEPSEEK_API_KEY",
        profile: "generic",
        model: "deepseek-chat",
        key_url: "https://platform.deepseek.com/api_keys",
    },
    Preset {
        label: "Google Gemini",
        name: "gemini",
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        key_env: "GEMINI_API_KEY",
        profile: "generic",
        model: "gemini-2.5-flash",
        key_url: "https://aistudio.google.com/apikey",
    },
    Preset {
        label: "Groq",
        name: "groq",
        base_url: "https://api.groq.com/openai/v1",
        key_env: "GROQ_API_KEY",
        profile: "generic",
        model: "llama-3.3-70b-versatile",
        key_url: "https://console.groq.com/keys",
    },
    Preset {
        label: "Anthropic (Claude, native Messages API)",
        name: "anthropic",
        base_url: "https://api.anthropic.com/v1",
        key_env: "ANTHROPIC_API_KEY",
        profile: "anthropic",
        model: "claude-sonnet-5",
        key_url: "https://console.anthropic.com/settings/keys",
    },
    Preset {
        label: "Ollama (models running on this machine)",
        name: "ollama",
        base_url: "http://localhost:11434/v1",
        key_env: "OLLAMA_API_KEY",
        profile: "generic",
        model: "qwen3-coder",
        key_url: "",
    },
];

/// The preset called `name`.
pub(crate) fn preset(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name == name)
}

/// A provider being added or changed.
struct NewProvider {
    label: String,
    name: String,
    base_url: String,
    key_env: String,
    profile: String,
    model: String,
    key_url: String,
    needs_key: bool,
}

impl NewProvider {
    fn from_preset(p: &Preset) -> Self {
        Self {
            label: p.label.into(),
            name: p.name.into(),
            base_url: p.base_url.into(),
            key_env: p.key_env.into(),
            profile: p.profile.into(),
            model: p.model.into(),
            key_url: p.key_url.into(),
            needs_key: !p.key_url.is_empty(),
        }
    }

    fn from_config(name: &str, p: &config::ProviderConfig) -> Self {
        let preset = PRESETS.iter().find(|preset| preset.base_url == p.base_url);
        Self {
            label: name.into(),
            name: name.into(),
            base_url: p.base_url.clone(),
            key_env: p.api_key_env.clone(),
            profile: p.profile.clone(),
            model: p.model.clone(),
            key_url: preset
                .map(|preset| preset.key_url.to_string())
                .unwrap_or_default(),
            needs_key: preset.is_none_or(|preset| !preset.key_url.is_empty()),
        }
    }
}

fn valid_provider_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

async fn provider_step(t: &mut Target, http: &reqwest::Client, guided: bool) -> Result<()> {
    let cfg = t.config()?;
    if guided || cfg.providers.is_empty() {
        return add_provider(t, http).await;
    }
    let mut names: Vec<String> = cfg.providers.keys().cloned().collect();
    names.sort_by_key(|n| (cfg.default_provider.as_ref() != Some(n), n.clone()));
    let mut labels: Vec<String> = names
        .iter()
        .map(|name| {
            let p = &cfg.providers[name];
            let default = if cfg.default_provider.as_ref() == Some(name) {
                " (default)"
            } else {
                ""
            };
            let missing = if key_is_set(&p.api_key_env) {
                ""
            } else {
                " · key missing"
            };
            format!("{name} · {}{default}{missing}", p.model)
        })
        .collect();
    labels.push("Add a provider".into());
    let several = crate::models::Catalog::from_config(&cfg).entries.len() > 1;
    if several {
        labels.push("Default model".into());
        labels.push("Routing: start cheap, escalate when needed".into());
    }
    let pick = Select::new("Which provider?", labels).raw_prompt()?.index;
    match names.get(pick) {
        Some(name) => edit_provider(t, http, name).await,
        None if pick == names.len() => add_provider(t, http).await,
        None if pick == names.len() + 1 => default_model_step(t),
        None => routing_step(t),
    }
}

async fn add_provider(t: &mut Target, http: &reqwest::Client) -> Result<()> {
    let cfg = t.config()?;
    // M34: local servers running here come first, with what they serve.
    let servers = interruptible(local::found(http, &cfg)).await?;
    let mut labels: Vec<String> = servers.iter().map(local::label).collect();
    labels.extend(PRESETS.iter().map(|p| p.label.to_string()));
    labels.push("Another OpenAI-compatible server (vLLM, LM Studio, a gateway…)".into());
    let pick = Select::new("Which model provider?", labels)
        .with_page_size(servers.len() + PRESETS.len() + 1)
        .raw_prompt()?
        .index;
    let mut np = match (servers.get(pick), pick.checked_sub(servers.len())) {
        (Some(server), _) => local::provider(server),
        (None, Some(i)) if i < PRESETS.len() => NewProvider::from_preset(&PRESETS[i]),
        _ => ask_custom_provider(&cfg)?,
    };
    if cfg.providers.contains_key(&np.name) {
        info(format!("You have `{}` already; this updates it.", np.name));
    }
    if np.profile == "anthropic" {
        info(ANTHROPIC_NOTE);
    }
    let (key, models) = ask_provider_key(http, &np).await?;
    if !np.needs_key {
        local::describe_models(http, &np.base_url).await;
    }
    np.model = pick_model(&models, &np.model)?;
    if let Some(key) = &key {
        t.set_secret(&np.key_env, key)?;
    }
    write_provider(t.root(), &np)?;
    if !np.needs_key {
        let probe_key = key
            .clone()
            .or_else(|| std::env::var(&np.key_env).ok())
            .unwrap_or_else(|| "none".into());
        local::fit(t, http, &mut np, &probe_key).await?;
    }
    let default = crate::models::Catalog::from_config(&cfg)
        .default_entry()
        .ok()
        .map(|(e, _)| e.reference());
    let make_default = match default {
        None => true,
        Some(d) if d.split('/').next() == Some(np.name.as_str()) => false,
        Some(d) => Confirm::new(&format!(
            "Use `{}/{}` instead of `{d}` by default?",
            np.name, np.model
        ))
        .with_default(false)
        .prompt()?,
    };
    if make_default {
        make_provider_default(t.root(), &np.name)?;
    }
    t.save()?;
    ok(format!("saved `{}` · {}", np.name, np.model));
    test_saved(t, &format!("{}/{}", np.name, np.model), key).await
}

const ANTHROPIC_NOTE: &str =
    "Ferrule talks to Claude through Anthropic's own Messages API (`api = \"anthropic\"`): \
     prompt caching is on, so a long conversation's repeated prefix is billed at the cached rate. \
     Extended thinking is off unless you set `thinking` on the provider or model.";

/// `name` as the default: `default_provider`, and a `[models] default`
/// that would override it goes.
fn make_provider_default(root: &mut dyn TableLike, name: &str) -> Result<()> {
    put(root, "default_provider", name);
    if let Some(models) = root.get_mut("models").and_then(Item::as_table_like_mut) {
        models.remove("default");
    }
    Ok(())
}

/// One real call to a model just saved, said plainly; a failure is only
/// a warning, the config is kept.
async fn test_saved(t: &mut Target, reference: &str, key: Option<String>) -> Result<()> {
    let cat = crate::models::Catalog::from_config(&t.config()?);
    let Ok(entry) = cat.resolve(reference).cloned() else {
        return Ok(());
    };
    let key = key.or_else(|| entry.key());
    let out = interruptible(crate::models::test_entry_with(&entry, key)).await?;
    if out.ok {
        ok(format!("{}: {}", out.reference, out.said));
    } else {
        warn(format!("{}: {}", out.reference, out.said));
    }
    Ok(())
}

/// Every connected model, to pick the default from; `[models] default`
/// is set to the pick.
fn default_model_step(t: &mut Target) -> Result<()> {
    let cat = crate::models::Catalog::from_config(&t.config()?);
    let current = cat.default_entry().ok().map(|(e, _)| e.reference());
    let refs: Vec<String> = cat.entries.iter().map(|e| e.reference()).collect();
    let cursor = current
        .as_ref()
        .and_then(|c| refs.iter().position(|r| r == c))
        .unwrap_or(0);
    let pick = Select::new(
        "Default model (chats, tasks and agents without their own)",
        refs,
    )
    .with_starting_cursor(cursor)
    .prompt()?;
    if current.as_deref() == Some(pick.as_str()) {
        return Ok(());
    }
    put(table(t.root(), &["models"])?, "default", pick.as_str());
    t.save()?;
    ok(format!("the default is {pick} now"));
    Ok(())
}

/// M25: a cheap tier every turn starts on and a strong one it moves up to
/// when it fails; the suggested pair is where the cursor starts.
fn routing_step(t: &mut Target) -> Result<()> {
    let cat = crate::models::Catalog::from_config(&t.config()?);
    if cat.routing.enabled {
        info(format!(
            "Routing is on: {}.",
            cat.routing.names().join(" → ")
        ));
        let pick = Select::new(
            "Routing",
            vec![
                "Pick the cheap and strong models again",
                "Turn routing off",
                "Leave it",
            ],
        )
        .raw_prompt()?
        .index;
        match pick {
            0 => {}
            1 => {
                put(table(t.root(), &["routing"])?, "enabled", false);
                t.save()?;
                ok("routing is off: turns run on the default model");
                return Ok(());
            }
            _ => return Ok(()),
        }
    } else {
        info(
            "Every turn starts on a cheap model and moves up to a strong one only when it fails: \
             a call error, invalid tool calls, a failed check or Stop hook, no progress. \
             The next turn starts cheap again.",
        );
    }
    let sg = crate::models::routing_admin::suggest(&cat, &[], None);
    let refs: Vec<String> = cat.entries.iter().map(|e| e.reference()).collect();
    let labels: Vec<String> = cat
        .entries
        .iter()
        .map(|e| match e.pricing {
            Some(p) => format!(
                "{} · ${}/${} per M in/out",
                e.reference(),
                p.input,
                p.output
            ),
            None => format!("{} · price unknown", e.reference()),
        })
        .collect();
    let at = |p: &Option<crate::models::routing_admin::Pick>, among: &[String]| {
        p.as_ref()
            .and_then(|p| among.iter().position(|r| *r == p.reference))
    };
    let cheap = Select::new("Cheap tier: every turn starts here", labels.clone())
        .with_starting_cursor(at(&sg.cheap, &refs).unwrap_or(0))
        .raw_prompt()?
        .index;
    let rest: Vec<usize> = (0..refs.len()).filter(|&i| i != cheap).collect();
    let rest_refs: Vec<String> = rest.iter().map(|&i| refs[i].clone()).collect();
    let strong = Select::new(
        "Strong tier: a turn moves here when the cheap one fails",
        rest.iter().map(|&i| labels[i].clone()).collect(),
    )
    .with_starting_cursor(at(&sg.strong, &rest_refs).unwrap_or(rest.len() - 1))
    .raw_prompt()?
    .index;
    let cap = Text::new("Daily cap on spend above the cheap tier, in dollars (empty: none)")
        .with_validator(|v: &str| -> Result<Validation, CustomUserError> {
            let v = v.trim();
            Ok(match v.parse::<f64>() {
                _ if v.is_empty() => Validation::Valid,
                Ok(c) if c.is_finite() && c > 0.0 => Validation::Valid,
                _ => Validation::Invalid("dollars a day, like 2.5".into()),
            })
        })
        .prompt()?;
    let cap = cap.trim().parse::<f64>().ok();
    let tiers = [refs[cheap].clone(), rest_refs[strong].clone()];
    write_routing(t.root(), &tiers, cap)?;
    t.save()?;
    ok(format!(
        "routing is on: {} → {}{}",
        tiers[0],
        tiers[1],
        cap.map(|c| format!(" · at most ${c:.2} a day above the cheap tier"))
            .unwrap_or_default()
    ));
    Ok(())
}

/// `[routing]` on over `tiers`, cheap first; the cap is set or removed.
fn write_routing(root: &mut dyn TableLike, tiers: &[String], cap: Option<f64>) -> Result<()> {
    let r = table(root, &["routing"])?;
    put(r, "enabled", true);
    put(
        r,
        "tiers",
        toml_edit::Array::from_iter(tiers.iter().map(String::as_str)),
    );
    match cap {
        Some(c) => put(r, "strong_daily_usd", c),
        None => {
            r.remove("strong_daily_usd");
        }
    }
    Ok(())
}

/// `[providers.<name>.models."<model>"]`, and the alias if one's given.
fn write_extra_model(
    root: &mut dyn TableLike,
    name: &str,
    model: &str,
    alias: Option<&str>,
) -> Result<()> {
    let mut own = toml_edit::Table::new();
    own.set_implicit(false);
    table(root, &["providers", name, "models"])?.insert(model, Item::Table(own));
    if let Some(a) = alias {
        put(
            table(root, &["models", "aliases"])?,
            a,
            format!("{name}/{model}").as_str(),
        );
    }
    Ok(())
}

/// Connect another model on provider `name` (`[providers.name.models.m]`).
async fn add_model_step(t: &mut Target, http: &reqwest::Client, name: &str) -> Result<()> {
    let cfg = t.config()?;
    let p = cfg.providers[name].clone();
    let models = match std::env::var(&p.api_key_env) {
        Ok(key) => {
            match interruptible(probe::models(
                http,
                &p.base_url,
                &key,
                p.profile == "anthropic",
            ))
            .await?
            {
                Ok(models) => models,
                Err(e) => {
                    warn(format!("couldn't fetch the model list: {e}"));
                    Vec::new()
                }
            }
        }
        Err(_) => Vec::new(),
    };
    let have: Vec<&String> = std::iter::once(&p.model).chain(p.models.keys()).collect();
    let offer: Vec<String> = models.into_iter().filter(|m| !have.contains(&m)).collect();
    let model = pick_model(&offer, "")?;
    if have.contains(&&model) {
        info(format!("`{name}/{model}` is connected already"));
        return Ok(());
    }
    let alias = Text::new("A short name for it (optional)")
        .with_placeholder("fast")
        .with_help_message("then `/model use fast` in Telegram; Esc to skip")
        .prompt_skippable()?
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty());
    let alias = alias.filter(|a| {
        let taken = cfg.providers.contains_key(a) || cfg.models.aliases.contains_key(a);
        if taken {
            warn(format!("`{a}` is taken; no alias set"));
        }
        !taken
    });
    write_extra_model(t.root(), name, &model, alias.as_deref())?;
    t.save()?;
    ok(format!("connected `{name}/{model}`"));
    test_saved(t, &format!("{name}/{model}"), None).await
}

fn ask_custom_provider(cfg: &config::Config) -> Result<NewProvider> {
    let taken: Vec<String> = cfg.providers.keys().cloned().collect();
    let name = Text::new("A short name for it")
        .with_placeholder("local")
        .with_validator(move |v: &str| -> Result<Validation, CustomUserError> {
            let v = v.trim();
            Ok(if !valid_provider_name(v) {
                Validation::Invalid("lowercase letters, digits, - and _".into())
            } else if taken.iter().any(|n| n == v) {
                Validation::Invalid("that name is taken; pick it from the menu to change it".into())
            } else {
                Validation::Valid
            })
        })
        .prompt()?;
    let name = name.trim().to_string();
    let base_url = Text::new("Base URL (the part before /chat/completions)")
        .with_placeholder("http://localhost:8000/v1")
        .with_validator(|v: &str| -> Result<Validation, CustomUserError> {
            let v = v.trim();
            Ok(
                if (v.starts_with("http://") || v.starts_with("https://"))
                    && !v.contains(char::is_whitespace)
                {
                    Validation::Valid
                } else {
                    Validation::Invalid("a URL starting with http:// or https://".into())
                },
            )
        })
        .prompt()?;
    let suggested = format!("{}_API_KEY", name.to_uppercase().replace('-', "_"));
    let key_env = Text::new("Name to save its key under")
        .with_initial_value(&suggested)
        .with_help_message("an environment variable name; Esc if it needs no key")
        .with_validator(|v: &str| -> Result<Validation, CustomUserError> {
            Ok(if secrets::valid_name(v.trim()) {
                Validation::Valid
            } else {
                Validation::Invalid("letters, digits and _, not starting with a digit".into())
            })
        })
        .prompt_skippable()?;
    Ok(NewProvider {
        label: name.clone(),
        needs_key: key_env.is_some(),
        key_env: key_env.map_or(suggested, |k| k.trim().to_string()),
        name,
        base_url: base_url.trim().trim_end_matches('/').to_string(),
        profile: "generic".into(),
        model: String::new(),
        key_url: String::new(),
    })
}

/// Ask for the key and check it against the model list until it works or
/// the user settles for it. Returns the key to save (`None`: keep the one
/// already set) and the models it can see.
async fn ask_provider_key(
    http: &reqwest::Client,
    np: &NewProvider,
) -> Result<(Option<String>, Vec<String>)> {
    let current = std::env::var(&np.key_env).ok().filter(|v| !v.is_empty());
    let anthropic = np.profile == "anthropic";
    if !np.needs_key {
        // Local servers take any key; the client still sends one.
        let key = current.clone().unwrap_or_else(|| "none".into());
        let models = match interruptible(probe::models(http, &np.base_url, &key, anthropic)).await?
        {
            Ok(models) => {
                ok(format!(
                    "reached {} · {}",
                    np.base_url,
                    plural(models.len(), "model", "models")
                ));
                models
            }
            Err(e) => {
                warn(format!(
                    "couldn't reach {}: {e}. Is it running?",
                    np.base_url
                ));
                Vec::new()
            }
        };
        return Ok((current.is_none().then(|| "none".into()), models));
    }
    if !np.key_url.is_empty() {
        info(format!("Get a key at {}", np.key_url));
    }
    loop {
        let entered = ask_secret(
            &format!("{} API key", np.label),
            current.is_some(),
            no_shape,
        )?;
        let key = entered
            .clone()
            .or_else(|| current.clone())
            .unwrap_or_default();
        match interruptible(probe::models(http, &np.base_url, &key, anthropic)).await? {
            Ok(models) => {
                ok(format!(
                    "the key works · {} available",
                    plural(models.len(), "model", "models")
                ));
                return Ok((entered, models));
            }
            Err(probe::Check::Rejected(why)) => {
                warn(why);
                if Confirm::new("Try another key?")
                    .with_default(true)
                    .prompt()?
                {
                    continue;
                }
                if !Confirm::new("Save this key anyway?")
                    .with_default(false)
                    .prompt()?
                {
                    bail!("`{}` wasn't saved", np.name);
                }
                return Ok((entered, Vec::new()));
            }
            Err(e) => {
                warn(format!("couldn't check the key: {e}"));
                if !Confirm::new("Save it anyway?")
                    .with_default(true)
                    .prompt()?
                {
                    bail!("`{}` wasn't saved", np.name);
                }
                return Ok((entered, Vec::new()));
            }
        }
    }
}

/// Pick from the provider's list, or type a name when there's no list or
/// the model isn't on it.
fn pick_model(models: &[String], current: &str) -> Result<String> {
    const TYPE: &str = "Type a model name…";
    if !models.is_empty() {
        let mut options: Vec<String> = models.to_vec();
        options.push(TYPE.into());
        let cursor = models.iter().position(|m| m == current).unwrap_or(0);
        let choice = Select::new("Model", options)
            .with_starting_cursor(cursor)
            .with_page_size(12)
            .with_help_message("↑↓ to move, type to filter, Enter to pick")
            .prompt()?;
        if choice != TYPE {
            return Ok(choice);
        }
    }
    let mut prompt =
        Text::new("Model").with_validator(|v: &str| -> Result<Validation, CustomUserError> {
            let v = v.trim();
            Ok(if v.is_empty() || v.contains(char::is_whitespace) {
                Validation::Invalid("a model id, like gpt-5.2".into())
            } else {
                Validation::Valid
            })
        });
    if !current.is_empty() {
        prompt = prompt.with_initial_value(current);
    }
    Ok(prompt.prompt()?.trim().to_string())
}

fn write_provider(root: &mut dyn TableLike, np: &NewProvider) -> Result<()> {
    let p = table(root, &["providers", &np.name])?;
    put(p, "base_url", np.base_url.as_str());
    put(p, "api_key_env", np.key_env.as_str());
    put(p, "model", np.model.as_str());
    put(p, "profile", np.profile.as_str());
    // M23: a native driver is written down, so the file says what runs.
    let api = ferrule_providers::infer_api(&np.base_url);
    if api != ferrule_providers::Api::Chat {
        put(p, "api", api.to_string().as_str());
    }
    Ok(())
}

async fn edit_provider(t: &mut Target, http: &reqwest::Client, name: &str) -> Result<()> {
    let cfg = t.config()?;
    let p = cfg.providers[name].clone();
    let is_default = cfg.default_provider.as_deref() == Some(name);
    let mut actions = vec![
        "Change the model",
        "Add another model on it",
        "Test it",
        "Replace the API key",
    ];
    if !is_default {
        actions.push("Make it the default");
    }
    actions.push("Remove it");
    match Select::new(&format!("{name}:"), actions).prompt()? {
        "Change the model" => {
            let models = if key_is_set(&p.api_key_env) {
                let key = std::env::var(&p.api_key_env)?;
                match interruptible(probe::models(
                    http,
                    &p.base_url,
                    &key,
                    p.profile == "anthropic",
                ))
                .await?
                {
                    Ok(models) => models,
                    Err(e) => {
                        warn(format!("couldn't fetch the model list: {e}"));
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            let model = pick_model(&models, &p.model)?;
            put(
                table(t.root(), &["providers", name])?,
                "model",
                model.as_str(),
            );
            t.save()?;
            ok(format!("`{name}` now uses {model}"));
            test_saved(t, &format!("{name}/{model}"), None).await?;
        }
        "Add another model on it" => add_model_step(t, http, name).await?,
        "Test it" => test_saved(t, name, None).await?,
        "Replace the API key" => {
            let (key, _) = ask_provider_key(http, &NewProvider::from_config(name, &p)).await?;
            if let Some(key) = key {
                t.set_secret(&p.api_key_env, &key)?;
                ok("key saved");
            }
        }
        "Make it the default" => {
            make_provider_default(t.root(), name)?;
            t.save()?;
            ok(format!("`{name}` is the default now"));
        }
        _ => {
            if !Confirm::new(&format!("Remove `{name}`?"))
                .with_default(false)
                .prompt()?
            {
                return Ok(());
            }
            table(t.root(), &["providers"])?.remove(name);
            if is_default {
                let mut rest: Vec<&String> = cfg.providers.keys().filter(|n| *n != name).collect();
                rest.sort();
                match rest.first() {
                    Some(next) => {
                        put(t.root(), "default_provider", next.as_str());
                        info(format!("`{next}` is the default now"));
                    }
                    None => {
                        t.root().remove("default_provider");
                    }
                }
            }
            t.save()?;
            t.forget_secret(&p.api_key_env)?;
            ok(format!("removed `{name}`"));
        }
    }
    Ok(())
}

// ── Telegram ───────────────────────────────────────────────────────────

async fn telegram_step(t: &mut Target, http: &reqwest::Client, guided: bool) -> Result<()> {
    let cfg = t.config()?;
    let token = cfg
        .gateway
        .telegram_token_env
        .as_deref()
        .and_then(|env| std::env::var(env).ok())
        .filter(|v| !v.is_empty());
    let Some(token) = token.filter(|_| !guided) else {
        let ask =
            Confirm::new("Connect a Telegram bot, so you can talk to the agent from your phone?")
                .with_default(true)
                .prompt()?;
        return if ask {
            telegram_connect(t, http).await
        } else {
            Ok(())
        };
    };
    let base = cfg.gateway.telegram_base_url.clone();
    let tg = probe::Telegram {
        http,
        base_url: &base,
        token: &token,
    };
    let bot = match interruptible(tg.get_me()).await? {
        Ok(name) => {
            ok(format!("@{name}"));
            Some(name)
        }
        Err(e) => {
            warn(e);
            None
        }
    };
    if bot.is_some() {
        check_webhook(&tg).await?;
    }
    let allowed = cfg.gateway.telegram_allowed_chats.clone();
    let mut actions = vec!["Allow another chat"];
    if !allowed.is_empty() {
        actions.push("Remove allowed chats");
    }
    actions.extend(["Replace the bot token", "Turn Telegram off"]);
    match Select::new("Telegram:", actions).prompt()? {
        "Allow another chat" => allow_chats(t, &tg, bot.as_deref().unwrap_or("your bot")).await?,
        "Remove allowed chats" => {
            let ids: Vec<String> = allowed.iter().map(i64::to_string).collect();
            let drop = MultiSelect::new("Remove which? (space to mark, Enter to confirm)", ids)
                .raw_prompt()?;
            let drop: Vec<i64> = drop.iter().map(|choice| allowed[choice.index]).collect();
            let kept: Vec<i64> = allowed
                .iter()
                .copied()
                .filter(|id| !drop.contains(id))
                .collect();
            save_allowed(t, &kept)?;
            ok(format!("{} left", plural(kept.len(), "chat", "chats")));
        }
        "Replace the bot token" => telegram_connect(t, http).await?,
        _ => {
            if !Confirm::new("Turn Telegram off?")
                .with_default(false)
                .prompt()?
            {
                return Ok(());
            }
            let env = cfg.gateway.telegram_token_env.clone().unwrap_or_default();
            table(t.root(), &["gateway"])?.remove("telegram_token_env");
            t.save()?;
            t.forget_secret(&env)?;
            ok("Telegram is off");
        }
    }
    Ok(())
}

async fn telegram_connect(t: &mut Target, http: &reqwest::Client) -> Result<()> {
    info("1. In Telegram, open @BotFather and send /newbot");
    info("2. Give it a name, then a username ending in \"bot\"");
    info("3. BotFather answers with a token like 123456789:AAH…");
    let cfg = t.config()?;
    let base = cfg.gateway.telegram_base_url.clone();
    let shape = |v: &str| {
        (!probe::plausible_bot_token(v)).then_some("that isn't a bot token (digits:letters)")
    };
    let (token, bot) = loop {
        let token = ask_secret("Bot token", false, shape)?.unwrap_or_default();
        let tg = probe::Telegram {
            http,
            base_url: &base,
            token: &token,
        };
        match interruptible(tg.get_me()).await? {
            Ok(name) => {
                ok(format!("connected to @{name}"));
                break (token, Some(name));
            }
            Err(probe::Check::Rejected(why)) => {
                warn(why);
                if Confirm::new("Try another token?")
                    .with_default(true)
                    .prompt()?
                {
                    continue;
                }
                bail!("Telegram wasn't set up");
            }
            Err(e) => {
                warn(format!("couldn't reach Telegram: {e}"));
                if Confirm::new("Save the token anyway?")
                    .with_default(true)
                    .prompt()?
                {
                    break (token, None);
                }
                bail!("Telegram wasn't set up");
            }
        }
    };
    let env = cfg
        .gateway
        .telegram_token_env
        .clone()
        .unwrap_or_else(|| "TELEGRAM_BOT_TOKEN".into());
    t.set_secret(&env, &token)?;
    put(
        table(t.root(), &["gateway"])?,
        "telegram_token_env",
        env.as_str(),
    );
    t.save()?;
    if let Some(bot) = bot {
        let tg = probe::Telegram {
            http,
            base_url: &base,
            token: &token,
        };
        check_webhook(&tg).await?;
        allow_chats(t, &tg, &bot).await?;
    }
    Ok(())
}

async fn check_webhook(tg: &probe::Telegram<'_>) -> Result<()> {
    match interruptible(tg.webhook()).await? {
        Ok(Some(url)) => {
            warn(format!(
                "this bot has a webhook ({url}), so ferrule can't receive its messages"
            ));
            if Confirm::new("Remove the webhook?")
                .with_default(true)
                .prompt()?
            {
                interruptible(tg.delete_webhook()).await??;
                ok("webhook removed");
            }
        }
        Ok(None) => {}
        Err(e) => warn(format!("couldn't check for a webhook: {e}")),
    }
    Ok(())
}

/// Watch the bot's messages and allow the chats the user confirms. If
/// something else is reading them (a running gateway), fall back to typing
/// the id: the gateway answers strangers with their chat id.
async fn allow_chats(t: &mut Target, tg: &probe::Telegram<'_>, bot: &str) -> Result<()> {
    let mut allowed = t.config()?.gateway.telegram_allowed_chats;
    let before = allowed.len();
    info(format!(
        "Now send any message to @{bot} from the Telegram account that should use it."
    ));
    info(format!(
        "For a group: add the bot, then mention it there (\"@{bot} hi\")."
    ));
    info("Waiting up to 2 minutes…");
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut offset = None;
    let mut declined = Vec::new();
    let mut busy = false;
    'wait: while Instant::now() < deadline {
        let left = deadline
            .saturating_duration_since(Instant::now())
            .as_secs()
            .clamp(1, 25);
        let updates = match interruptible(tg.updates(offset, left)).await? {
            Ok(updates) => updates,
            Err(probe::Check::Conflict(_)) => {
                busy = true;
                break;
            }
            Err(e) => {
                warn(format!("couldn't read the bot's messages: {e}"));
                break;
            }
        };
        let (next, chats) = probe::seen_chats(&updates);
        offset = next.or(offset);
        for chat in chats {
            if allowed.contains(&chat.id) || declined.contains(&chat.id) {
                continue;
            }
            let kind = if chat.kind == "private" {
                String::new()
            } else {
                format!(", a {}", chat.kind)
            };
            let question = format!(
                "Message from {}{kind} (chat {}). Let this chat use the bot?",
                chat.name, chat.id
            );
            if !Confirm::new(&question).with_default(true).prompt()? {
                declined.push(chat.id);
                continue;
            }
            allowed.push(chat.id);
            save_allowed(t, &allowed)?;
            let _ = interruptible(tg.send(
                chat.id,
                "✅ Connected: this chat can talk to your ferrule agent.",
            ))
            .await?;
            ok(format!("allowed {}", chat.name));
            if !Confirm::new("Wait for another chat?")
                .with_default(false)
                .prompt()?
            {
                break 'wait;
            }
        }
    }
    // Mark what setup read as seen, so the gateway doesn't answer it later.
    if offset.is_some() {
        let _ = interruptible(tg.updates(offset, 0)).await?;
    }
    if allowed.len() > before {
        return Ok(());
    }
    if busy {
        warn("something else is reading this bot's messages, probably a ferrule gateway that's running already");
        if allowed.is_empty() {
            info("Message the bot anyway: while no chat is allowed, it answers with the chat id to type here.");
        }
    } else {
        info("No chat was added.");
    }
    let id = Text::new("Chat id to allow (Enter to skip)")
        .with_validator(|v: &str| -> Result<Validation, CustomUserError> {
            let v = v.trim();
            Ok(if v.is_empty() || v.parse::<i64>().is_ok() {
                Validation::Valid
            } else {
                Validation::Invalid("a number, like 123456789 (groups are negative)".into())
            })
        })
        .prompt()?;
    if let Ok(id) = id.trim().parse::<i64>() {
        allowed.push(id);
        save_allowed(t, &allowed)?;
        ok(format!("allowed chat {id}"));
    } else {
        info("Later: `ferrule setup` → Telegram → Allow another chat.");
    }
    Ok(())
}

fn save_allowed(t: &mut Target, ids: &[i64]) -> Result<()> {
    let ids = toml_edit::Array::from_iter(ids.iter().copied());
    put(
        table(t.root(), &["gateway"])?,
        "telegram_allowed_chats",
        ids,
    );
    t.save()
}

// ── Tool credentials ───────────────────────────────────────────────────

struct TokenPreset {
    label: &'static str,
    name: &'static str,
    hosts: &'static [&'static str],
    /// Where to create one.
    url: &'static str,
    /// An endpoint that answers 2xx for a good bearer token.
    check: Option<&'static str>,
}

const TOKENS: &[TokenPreset] = &[
    TokenPreset {
        label: "GitHub (git push, gh, the API)",
        name: "GITHUB_TOKEN",
        hosts: &["api.github.com", "github.com", "*.githubusercontent.com"],
        url: "https://github.com/settings/personal-access-tokens",
        check: Some("https://api.github.com/user"),
    },
    TokenPreset {
        label: "GitLab",
        name: "GITLAB_TOKEN",
        hosts: &["gitlab.com"],
        url: "https://gitlab.com/-/user_settings/personal_access_tokens",
        check: Some("https://gitlab.com/api/v4/user"),
    },
    TokenPreset {
        label: "Hugging Face",
        name: "HF_TOKEN",
        hosts: &["huggingface.co"],
        url: "https://huggingface.co/settings/tokens",
        check: Some("https://huggingface.co/api/whoami-v2"),
    },
    TokenPreset {
        label: "npm",
        name: "NPM_TOKEN",
        hosts: &["registry.npmjs.org"],
        url: "",
        check: None,
    },
    TokenPreset {
        label: "Cloudflare",
        name: "CLOUDFLARE_API_TOKEN",
        hosts: &["api.cloudflare.com"],
        url: "https://dash.cloudflare.com/profile/api-tokens",
        check: Some("https://api.cloudflare.com/client/v4/user/tokens/verify"),
    },
    TokenPreset {
        label: "Vercel",
        name: "VERCEL_TOKEN",
        hosts: &["api.vercel.com"],
        url: "https://vercel.com/account/tokens",
        check: Some("https://api.vercel.com/v2/user"),
    },
];

async fn credentials_step(t: &mut Target, http: &reqwest::Client, guided: bool) -> Result<()> {
    if guided {
        info(
            "Commands the agent runs can use tokens (git push, gh, API calls) without ever seeing",
        );
        info(
            "them: they get a stand-in, and ferrule swaps the real one in only on requests to the",
        );
        info("hosts you allow.");
        if !Confirm::new("Add a token now, e.g. for GitHub?")
            .with_default(false)
            .prompt()?
        {
            return Ok(());
        }
        loop {
            add_token(t, http).await?;
            if !Confirm::new("Add another?").with_default(false).prompt()? {
                return Ok(());
            }
        }
    }
    loop {
        let cfg = t.config()?;
        let names: Vec<String> = cfg.secrets.keys().cloned().collect();
        let mut labels: Vec<String> = names
            .iter()
            .map(|name| {
                let hosts = ferrule_proxy::SecretRule::from(&cfg.secrets[name])
                    .hosts
                    .join(", ");
                let note = match secrets::source(name) {
                    _ if !key_is_set(name) => " · value missing",
                    secrets::Source::Env => " · from your shell",
                    _ => "",
                };
                format!("{name} → {hosts}{note}")
            })
            .collect();
        labels.push("Add a token".into());
        labels.push("Done".into());
        let pick = Select::new("Tool credentials", labels).raw_prompt()?.index;
        match names.get(pick) {
            Some(name) => edit_token(t, http, name).await?,
            None if pick == names.len() => add_token(t, http).await?,
            None => return Ok(()),
        }
    }
}

async fn add_token(t: &mut Target, http: &reqwest::Client) -> Result<()> {
    let cfg = t.config()?;
    let mut labels: Vec<&str> = TOKENS.iter().map(|p| p.label).collect();
    labels.push("Something else");
    let pick = Select::new("A token for", labels).raw_prompt()?.index;
    let (name, hosts, check) = match TOKENS.get(pick) {
        Some(preset) => {
            if !preset.url.is_empty() {
                info(format!("Create one at {}", preset.url));
            }
            let hosts = preset.hosts.iter().map(|h| h.to_string()).collect();
            (preset.name.to_string(), hosts, preset.check)
        }
        None => {
            let name = Text::new("Variable name commands will use")
                .with_placeholder("MY_SERVICE_TOKEN")
                .with_validator(|v: &str| -> Result<Validation, CustomUserError> {
                    Ok(if secrets::valid_name(v.trim()) {
                        Validation::Valid
                    } else {
                        Validation::Invalid(
                            "letters, digits and _, not starting with a digit".into(),
                        )
                    })
                })
                .prompt()?;
            (name.trim().to_string(), ask_hosts(&[])?, None)
        }
    };
    if cfg.secrets.contains_key(&name) {
        info(format!("You have {name} already; this updates it."));
    }
    let value = ask_token_value(http, &name, check).await?;
    if let Some(value) = value {
        t.set_secret(&name, &value)?;
    }
    write_secret_hosts(t.root(), &name, &hosts)?;
    t.save()?;
    ok(format!("{name} → {}", hosts.join(", ")));
    Ok(())
}

/// Ask for a token's value, checking it where there's a way to. `None`:
/// keep the saved one.
async fn ask_token_value(
    http: &reqwest::Client,
    name: &str,
    check: Option<&str>,
) -> Result<Option<String>> {
    let keep = key_is_set(name);
    loop {
        let entered = ask_secret(&format!("{name} value"), keep, no_shape)?;
        let (Some(url), Some(value)) = (check, &entered) else {
            return Ok(entered);
        };
        match interruptible(probe::token_accepted(http, url, value)).await? {
            Ok(()) => {
                ok("the token works");
                return Ok(entered);
            }
            Err(probe::Check::Rejected(why)) => {
                warn(why);
                if Confirm::new("Try another?").with_default(true).prompt()? {
                    continue;
                }
                if !Confirm::new("Save it anyway?")
                    .with_default(false)
                    .prompt()?
                {
                    bail!("{name} wasn't saved");
                }
                return Ok(entered);
            }
            Err(e) => {
                warn(format!("couldn't check it: {e}"));
                return Ok(entered);
            }
        }
    }
}

pub(crate) fn ask_hosts(current: &[String]) -> Result<Vec<String>> {
    let current = current.join(", ");
    let answer = Text::new("Hosts it may be sent to, comma-separated")
        .with_initial_value(&current)
        .with_help_message("like api.example.com, *.example.com")
        .with_validator(|v: &str| -> Result<Validation, CustomUserError> {
            let hosts = split_hosts(v);
            let bad = hosts
                .iter()
                .find(|h| h.contains(['/', ':', ' ']) || h.starts_with('.'));
            Ok(match bad {
                _ if hosts.is_empty() => Validation::Invalid("at least one host".into()),
                Some(bad) => Validation::Invalid(
                    format!("`{bad}`: just the host name, no scheme or path").into(),
                ),
                None => Validation::Valid,
            })
        })
        .prompt()?;
    Ok(split_hosts(&answer))
}

pub(crate) fn split_hosts(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Point `[secrets] NAME` at `hosts`; a table-form entry keeps its other
/// keys (`in_url`).
pub(crate) fn write_secret_hosts(
    root: &mut dyn TableLike,
    name: &str,
    hosts: &[String],
) -> Result<()> {
    let secrets = table(root, &["secrets"])?;
    let hosts = toml_edit::Array::from_iter(hosts.iter().map(String::as_str));
    match secrets.get_mut(name).and_then(Item::as_table_like_mut) {
        Some(entry) => put(entry, "hosts", hosts),
        None => put(secrets, name, hosts),
    }
    Ok(())
}

async fn edit_token(t: &mut Target, http: &reqwest::Client, name: &str) -> Result<()> {
    let cfg = t.config()?;
    let hosts = ferrule_proxy::SecretRule::from(&cfg.secrets[name]).hosts;
    match Select::new(
        &format!("{name}:"),
        vec!["Replace the value", "Change the hosts", "Remove it"],
    )
    .prompt()?
    {
        "Replace the value" => {
            let check = TOKENS.iter().find(|p| p.name == name).and_then(|p| p.check);
            if let Some(value) = ask_token_value(http, name, check).await? {
                t.set_secret(name, &value)?;
                ok(format!("{name} saved"));
            }
        }
        "Change the hosts" => {
            let hosts = ask_hosts(&hosts)?;
            write_secret_hosts(t.root(), name, &hosts)?;
            t.save()?;
            ok(format!("{name} → {}", hosts.join(", ")));
        }
        _ => {
            if !Confirm::new(&format!("Remove {name}?"))
                .with_default(false)
                .prompt()?
            {
                return Ok(());
            }
            let secrets = table(t.root(), &["secrets"])?;
            secrets.remove(name);
            if secrets.is_empty() {
                t.root().remove("secrets");
            }
            t.save()?;
            t.forget_secret(name)?;
            ok(format!("removed {name}"));
        }
    }
    Ok(())
}

// ── Memory recall ──────────────────────────────────────────────────────

/// M30: keyword recall only, a local model (downloaded here, never
/// silently), or an OpenAI-compatible embeddings endpoint of a configured
/// provider.
async fn memory_step(t: &mut Target, guided: bool) -> Result<()> {
    use ferrule_embed::download::POTION_MULTILINGUAL;
    let cfg = t.config()?;
    let spec = POTION_MULTILINGUAL;
    info("Long-term memory is recalled by keyword. An embedding model also finds facts by meaning: a paraphrase, a synonym, the same fact in Hebrew or English.");
    if guided
        && !Confirm::new("Recall memories by meaning too?")
            .with_default(false)
            .prompt()?
    {
        return Ok(());
    }
    let local = format!(
        "A local model: no key, nothing leaves this machine ({} download)",
        crate::embedding::mb(spec.total_size())
    );
    let mut labels = vec!["Keywords only".to_string(), local];
    let providers: Vec<String> = cfg.providers.keys().cloned().collect();
    for p in &providers {
        labels.push(format!(
            "The embeddings endpoint of provider `{p}` (paid per token)"
        ));
    }
    let pick = Select::new("Recall with", labels).raw_prompt()?.index;
    match pick {
        0 => {
            put(table(t.root(), &["memory"])?, "embedder", "off");
            t.save()?;
            ok("memory recall: keywords only");
        }
        1 => {
            if !cfg!(feature = "local-embed") {
                bail!("this ferrule was built without the local embedder");
            }
            let dir = spec.dir(&config::data_dir()?);
            if ferrule_embed::download::presence(&spec, &dir)
                != ferrule_embed::download::Presence::Present
                && !Confirm::new(&format!(
                    "Download {} ({}, pinned revision {}) to {}?",
                    spec.repo,
                    crate::embedding::mb(spec.total_size()),
                    &spec.revision[..7],
                    dir.display()
                ))
                .with_default(true)
                .prompt()?
            {
                return Ok(());
            }
            interruptible(crate::embedding::fetch_model(&cfg)).await??;
            put(table(t.root(), &["memory"])?, "embedder", "local");
            t.save()?;
            ok("memory recall: keywords + the local model (checksums match)");
            info("Memories saved before now are found by keyword until `ferrule memory reindex`.");
        }
        n => {
            let provider = &providers[n - 2];
            let model = Text::new("Embedding model")
                .with_initial_value(
                    cfg.memory
                        .model
                        .as_deref()
                        .unwrap_or("text-embedding-3-small"),
                )
                .prompt()?;
            let model = model.trim().to_string();
            let known = matches!(
                model.as_str(),
                "text-embedding-3-small" | "text-embedding-3-large" | "text-embedding-ada-002"
            );
            let dims = if known {
                None
            } else {
                Some(inquire::CustomType::<u64>::new("Its vector size (dimensions)").prompt()?)
            };
            let tbl = table(t.root(), &["memory"])?;
            put(tbl, "embedder", "openai");
            put(tbl, "provider", provider.as_str());
            put(tbl, "model", model.as_str());
            match dims {
                Some(d) => put(tbl, "dimensions", d as i64),
                None => {
                    tbl.remove("dimensions");
                }
            }
            t.save()?;
            ok(format!(
                "memory recall: keywords + {model} via `{provider}`"
            ));
            info("Set price_input_per_mtok under [memory] to see its cost in the ledger. `ferrule memory reindex` embeds what's already saved.");
        }
    }
    Ok(())
}

// ── Web search ─────────────────────────────────────────────────────────

/// A search provider setup offers: its key's usual variable and where to
/// get one. `None` for SearXNG, which takes the owner's own instance.
type SearchPreset = (
    SearchProvider,
    &'static str,
    Option<(&'static str, &'static str)>,
);

const SEARCH_PROVIDERS: [SearchPreset; 4] = [
    (
        SearchProvider::Brave,
        "Brave Search API",
        Some(("BRAVE_API_KEY", "https://api-dashboard.search.brave.com")),
    ),
    (
        SearchProvider::Tavily,
        "Tavily",
        Some(("TAVILY_API_KEY", "https://app.tavily.com")),
    ),
    (
        SearchProvider::Exa,
        "Exa",
        Some(("EXA_API_KEY", "https://dashboard.exa.ai")),
    ),
    (
        SearchProvider::Searxng,
        "SearXNG (your own instance, no key)",
        None,
    ),
];

fn web_search_step(t: &mut Target, guided: bool) -> Result<()> {
    let cfg = t.config()?;
    let current = cfg.web_search.settings()?;
    info("The agent can search the web through a search API. Its key goes through ferrule's proxy: the agent and its commands only ever see a stand-in.");
    if guided
        && !Confirm::new("Give the agent web search?")
            .with_default(false)
            .prompt()?
    {
        return Ok(());
    }
    let mut labels: Vec<&str> = SEARCH_PROVIDERS.iter().map(|p| p.1).collect();
    if current.is_some() {
        labels.push("Turn it off");
    }
    let pick = Select::new("Search with", labels).raw_prompt()?.index;
    let old_key = current.as_ref().and_then(|s| s.key_env.clone());
    let Some(&(provider, _, key)) = SEARCH_PROVIDERS.get(pick) else {
        t.root().remove("web_search");
        t.save()?;
        if let Some(var) = old_key {
            t.forget_secret(&var)?;
        }
        ok("web search off");
        return Ok(());
    };
    let same = current.as_ref().is_some_and(|s| s.provider == provider);
    let mut value = None;
    let (key_env, endpoint) = match key {
        Some((var, url)) => {
            let var = old_key
                .clone()
                .filter(|_| same)
                .unwrap_or_else(|| var.into());
            if !key_is_set(&var) {
                info(format!("Get a key at {url}"));
            }
            // No check: every call these APIs answer is a paid search.
            value = ask_secret(&format!("{var} value"), key_is_set(&var), no_shape)?;
            (Some(var), None)
        }
        None => {
            let initial = current
                .as_ref()
                .filter(|_| same)
                .map(|s| s.endpoint.clone())
                .unwrap_or_default();
            let url = Text::new("Your SearXNG instance's URL")
                .with_initial_value(&initial)
                .with_help_message("it must allow format=json (search.formats in its settings.yml)")
                .with_validator(|v: &str| -> Result<Validation, CustomUserError> {
                    Ok(match url::Url::parse(v.trim()) {
                        Ok(u)
                            if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() =>
                        {
                            Validation::Valid
                        }
                        _ => Validation::Invalid("an http(s) URL".into()),
                    })
                })
                .prompt()?;
            (None, Some(url.trim().trim_end_matches('/').to_string()))
        }
    };
    let cap = inquire::CustomType::<u64>::new("Most searches a day (0: no cap)")
        .with_default(cfg.web_search.max_searches_per_day)
        .prompt()?;
    if let (Some(var), Some(value)) = (&key_env, &value) {
        t.set_secret(var, value)?;
    }
    let tbl = table(t.root(), &["web_search"])?;
    put(tbl, "provider", provider.name());
    match &key_env {
        Some(var) => put(tbl, "api_key_env", var.as_str()),
        None => {
            tbl.remove("api_key_env");
        }
    }
    match &endpoint {
        Some(url) => put(tbl, "endpoint", url.as_str()),
        None if !same => {
            tbl.remove("endpoint");
        }
        None => {}
    }
    if cap > 0 {
        put(tbl, "max_searches_per_day", cap as i64);
    } else {
        tbl.remove("max_searches_per_day");
    }
    t.save()?;
    if let Some(var) = old_key.filter(|v| key_env.as_ref() != Some(v)) {
        t.forget_secret(&var)?;
    }
    ok(format!(
        "web search with {}: the agent gets it the next time it starts",
        provider.name()
    ));
    Ok(())
}

// ── Sandbox ────────────────────────────────────────────────────────────

fn sandbox_step(t: &mut Target, guided: bool) -> Result<()> {
    if cfg!(windows) {
        warn("Windows has no OS sandbox in ferrule yet: shell commands the agent runs have your own permissions.");
        info("Saved keys stay out of its file tools and out of the commands' environment either way.");
        info("For full isolation, run the Linux build under WSL2 instead.");
        return Ok(());
    }
    let cfg = t.config()?;
    info("Shell commands the agent runs go through an OS sandbox (Landlock on Linux, Seatbelt on macOS).");
    let recommended = guided
        && Confirm::new("Use the recommended one? Commands can change the workspace but not the rest of the system, and can use the network")
            .with_default(true)
            .prompt()?;
    let (mode, network) = if recommended {
        (Mode::WorkspaceWrite, true)
    } else {
        let modes = [
            (
                Mode::WorkspaceWrite,
                "workspace-write   commands can change the workspace, nothing else (recommended)",
            ),
            (
                Mode::ReadOnly,
                "read-only         commands can look, not change anything",
            ),
            (Mode::Off, "off               no OS sandbox"),
        ];
        let cursor = modes
            .iter()
            .position(|(m, _)| *m == cfg.sandbox.mode)
            .unwrap_or(0);
        let pick = Select::new("Sandbox", modes.iter().map(|(_, label)| *label).collect())
            .with_starting_cursor(cursor)
            .raw_prompt()?
            .index;
        let network = Confirm::new("Let commands use the network (package installs, git, APIs)?")
            .with_default(cfg.sandbox.network)
            .prompt()?;
        (modes[pick].0, network)
    };
    let sandbox = table(t.root(), &["sandbox"])?;
    put(sandbox, "mode", mode_name(mode));
    put(sandbox, "network", network);
    t.save()?;
    match Sandbox::new(crate::sandbox_policy(&t.config()?)) {
        Ok(sandbox) if sandbox.is_active() => {
            ok(format!("sandbox works here · {}", sandbox.backend()))
        }
        Ok(sandbox) => warn(format!(
            "commands will run unsandboxed: {}",
            sandbox.degraded().unwrap_or("the sandbox is off")
        )),
        Err(e) => warn(e),
    }
    Ok(())
}

// ── Network policy ─────────────────────────────────────────────────────

/// Hosts a coding agent reaches to fetch code and packages: the "package
/// hosts only" starting policy (docs/egress.md).
pub(crate) const PACKAGE_HOSTS: &[&str] = &[
    "github.com",
    "*.github.com",
    "*.githubusercontent.com",
    "gitlab.com",
    "registry.npmjs.org",
    "pypi.org",
    "files.pythonhosted.org",
    "crates.io",
    "*.crates.io",
    "proxy.golang.org",
    "sum.golang.org",
    "rubygems.org",
    "repo.maven.apache.org",
];

/// M33: `[egress]`, the hosts ferrule's tools and (when proxied) commands
/// may reach. Model servers, MCP servers and the search backend named in
/// the config are let through either way.
fn network_step(t: &mut Target, guided: bool) -> Result<()> {
    let cfg = t.config()?;
    info("web_fetch, web_search, MCP servers and plugins reach the network through ferrule's proxy; so do shell commands once there are rules.");
    info("Private addresses (your LAN, loopback, the cloud metadata address) are blocked by default; the servers your config names stay reachable.");
    let presets = [
        "open                public hosts, private ranges blocked (recommended)",
        "package hosts only  GitHub, GitLab and the package registries; the rest refused",
        "keep                leave [egress] as it is (edit it by hand, docs/egress.md)",
    ];
    let cursor = if cfg.egress.default == "deny" { 1 } else { 0 };
    if guided
        && Confirm::new("Use the recommended one? Public hosts, nothing on your LAN")
            .with_default(true)
            .prompt()?
    {
        write_egress(t.root(), false, &cfg.egress.private_allow)?;
        return t.save();
    }
    let pick = Select::new("Network policy", presets.to_vec())
        .with_starting_cursor(cursor)
        .raw_prompt()?
        .index;
    if pick == 2 {
        return Ok(());
    }
    let lan = Confirm::new(
        "Should tools reach anything on your LAN or this machine (a NAS, a local service)?",
    )
    .with_default(!cfg.egress.private_allow.is_empty())
    .prompt()?;
    let private_allow = if lan {
        ask_lan_hosts(&cfg.egress.private_allow)?
    } else {
        Vec::new()
    };
    write_egress(t.root(), pick == 1, &private_allow)?;
    t.save()?;
    ok(format!("network policy: {}", network_summary(&t.config()?)));
    Ok(())
}

fn ask_lan_hosts(current: &[String]) -> Result<Vec<String>> {
    let answer = Text::new("Private hosts or ranges tools may reach, comma-separated")
        .with_initial_value(&current.join(", "))
        .with_help_message("like nas.local, 192.168.1.0/24")
        .prompt()?;
    Ok(split_hosts(&answer))
}

/// `[egress]` for a starting policy: open (public hosts) or package hosts
/// only, with `private_allow`. Deny rules the owner wrote stay.
fn write_egress(
    root: &mut dyn TableLike,
    packages_only: bool,
    private_allow: &[String],
) -> Result<()> {
    let egress = table(root, &["egress"])?;
    if packages_only {
        put(egress, "default", "deny");
        put(
            egress,
            "allow",
            toml_edit::Array::from_iter(PACKAGE_HOSTS.iter().copied()),
        );
    } else {
        egress.remove("default");
        egress.remove("allow");
    }
    egress.remove("private");
    if private_allow.is_empty() {
        egress.remove("private_allow");
    } else {
        put(
            egress,
            "private_allow",
            toml_edit::Array::from_iter(private_allow.iter().map(String::as_str)),
        );
    }
    Ok(())
}

// ── Browser ────────────────────────────────────────────────────────────

/// Offer the browser when there's a Chrome and agent-browser to drive it,
/// and turn it on only once Chrome has started the way the agent would run
/// it. Nothing is downloaded.
fn browser_step(t: &mut Target) -> Result<()> {
    let cfg = t.config()?;
    let b = &cfg.browser;
    info("The agent can use a real headless Chrome for pages that need JavaScript, a login or clicks.");
    let Some(chrome) = browser::chrome(b) else {
        info("No Chrome or Chromium found, so there's nothing to turn on. ferrule never downloads one: install it, then come back here.");
        return Ok(());
    };
    if let Err(why) = ferrule_mcp::browser::find_agent_browser(&b.command) {
        info(format!(
            "Found {}, but agent-browser, which drives it, isn't ready: {why}. Then come back here.",
            tilde(&chrome)
        ));
        return Ok(());
    }
    let want = Confirm::new(&format!("Let the agent use {}?", tilde(&chrome)))
        .with_default(b.enabled)
        .prompt()?;
    let mut chrome_sandbox = b.chrome_sandbox;
    let mut on = want;
    if want {
        if let Some(why) = browser::sandbox_blocker(Some(&cfg)) {
            warn(format!("{why}."));
            info("ferrule's sandbox still confines it, but a page that breaks out of Chrome's renderer would get everything the agent's commands can reach. docs/browser.md has the details.");
            chrome_sandbox = !Confirm::new("Run Chrome without its own sandbox?")
                .with_default(!b.chrome_sandbox)
                .prompt()?;
            on = !chrome_sandbox;
        }
    }
    if on {
        match browser::launch_test(&cfg, &chrome, !chrome_sandbox) {
            Ok(()) => ok("Chrome starts headless inside the sandbox"),
            Err(why) => {
                warn(format!("Chrome didn't start: {why}"));
                if chrome_sandbox && ferrule_mcp::browser::is_sandbox_failure(&why) {
                    info("Its own sandbox can't start here. `ferrule doctor` and docs/browser.md say how to fix that.");
                }
                on = false;
            }
        }
    }
    if on == b.enabled && chrome_sandbox == b.chrome_sandbox {
        if want && !on {
            info("The browser stays off.");
        }
        return Ok(());
    }
    let tbl = table(t.root(), &["browser"])?;
    put(tbl, "enabled", on);
    if on && !chrome_sandbox {
        put(tbl, "chrome_sandbox", false);
    } else if chrome_sandbox && !b.chrome_sandbox {
        tbl.remove("chrome_sandbox");
    }
    t.save()?;
    if on {
        ok("browser on: the agent gets it the next time it starts");
    } else {
        info("The browser is off.");
    }
    Ok(())
}

// ── Background service ─────────────────────────────────────────────────

fn service_step(t: &mut Target, guided: bool) -> Result<()> {
    match service::status() {
        service::Status::Unsupported(why) => {
            info(format!("No background service here: {why}."));
            info(if cfg!(windows) {
                r"Run the gateway yourself instead, in a window you leave open: ferrule gateway --workspace $HOME\ferrule-workspace"
            } else {
                "Run the gateway yourself instead, e.g. in tmux: ferrule gateway --workspace ~/ferrule-workspace"
            });
        }
        service::Status::NotInstalled => {
            info("The gateway is what answers on Telegram. As a background service it starts at");
            info("login and comes back if it stops.");
            if service::scope() == service::Scope::System {
                info(format!(
                    "As root, it's a system service run as a user of its own, `{}`: no login, no sudo,",
                    service::SYSTEM_USER
                ));
                info("and it can write only its data dir and workspace.");
            }
            let question = if guided {
                "Run it in the background now?"
            } else {
                "Install it?"
            };
            if !Confirm::new(question).with_default(true).prompt()? {
                info("Later: `ferrule setup` → Background service, or run `ferrule gateway` yourself.");
                return Ok(());
            }
            install_service(t, None)?;
        }
        service::Status::Installed { running, .. } => {
            let pinned = service::installed();
            let workspace = pinned.as_ref().map(|(_, ws)| ws.clone());
            info(format!(
                "{} · workspace {}",
                if running {
                    "running"
                } else {
                    "installed, not running"
                },
                workspace.as_deref().map_or("unknown".into(), tilde)
            ));
            let this = std::path::absolute(&t.path)?;
            if let Some((config, _)) = &pinned {
                if *config != this {
                    warn(format!("it uses {}, not {}", tilde(config), tilde(&this)));
                }
            }
            let actions = vec![
                "Restart it",
                "Reinstall it (another workspace, or this config)",
                "Stop and remove it",
            ];
            match Select::new("Background service:", actions).prompt()? {
                "Restart it" => {
                    service::restart()?;
                    t.changed = false;
                    ok(format!("restarted · logs: {}", service::logs_hint()));
                }
                "Stop and remove it" => {
                    if Confirm::new("Stop and remove the service?")
                        .with_default(false)
                        .prompt()?
                    {
                        service::uninstall()?;
                        ok("removed");
                    }
                }
                _ => install_service(t, workspace.as_deref())?,
            }
        }
    }
    Ok(())
}

fn install_service(t: &mut Target, workspace: Option<&Path>) -> Result<()> {
    if !t.path.exists() {
        bail!("there's no config to run it with yet; set up a model provider first");
    }
    let system = service::scope() == service::Scope::System;
    if !system && service::is_root() {
        warn("You're root: this service would run as root, and so would every command the agent");
        warn("runs — the sandbox limits writes, not what root may read or change through its");
        warn(if cfg!(target_os = "linux") {
            "privileges. `sudo ferrule setup --system` runs it as a dedicated user instead."
        } else {
            "privileges. Run setup as an ordinary user instead."
        });
        if !Confirm::new("Run the service as root anyway?")
            .with_default(false)
            .prompt()?
        {
            bail!("the service wasn't installed");
        }
    }
    let default = workspace.map_or_else(
        || {
            if system {
                service::SYSTEM_WORKSPACE.to_string()
            } else {
                "~/ferrule-workspace".to_string()
            }
        },
        tilde,
    );
    let answer = Text::new("Workspace: the folder the agent works in")
        .with_default(&default)
        .with_help_message("created if it doesn't exist")
        .prompt()?;
    let workspace = std::path::absolute(crate::expand_home(Path::new(answer.trim())))?;
    std::fs::create_dir_all(&workspace)
        .with_context(|| format!("creating {}", workspace.display()))?;
    let workspace = dunce::canonicalize(workspace)?;
    let data = config::data_dir()?;
    let data = dunce::canonicalize(&data).unwrap_or(data);
    if data.starts_with(&workspace) {
        let linux = if cfg!(target_os = "linux") {
            ", and can't create files at its top level"
        } else {
            ""
        };
        warn(format!(
            "{} holds ferrule's own data ({}): commands could change its memory and sessions{linux}. Your keys stay hidden either way.",
            tilde(&workspace),
            tilde(&data)
        ));
        if !Confirm::new("Use it anyway?")
            .with_default(false)
            .prompt()?
        {
            bail!("the service wasn't installed");
        }
    }
    let exe = dunce::canonicalize(std::env::current_exe()?)?;
    let exe_text = exe.to_string_lossy();
    if exe_text.contains("/target/debug/") || exe_text.contains("/target/release/") {
        warn(format!(
            "{} is a build-tree binary: a rebuild or `cargo clean` breaks the service. \
             Install ferrule (install.sh, or `cargo install --path crates/ferrule-cli`) and run setup from there.",
            tilde(&exe)
        ));
        if !Confirm::new("Use this binary anyway?")
            .with_default(false)
            .prompt()?
        {
            bail!("the service wasn't installed");
        }
    }
    let mut path_env = std::env::var("PATH").unwrap_or_default();
    if let Some(dir) = exe.parent() {
        if !std::env::split_paths(&path_env).any(|p| p == dir) {
            path_env = format!("{}:{path_env}", dir.display());
        }
    }
    let spec = service::Spec {
        exe,
        workspace,
        config: std::path::absolute(&t.path)?,
        path_env,
    };
    let notes = service::install(&spec)?;
    t.changed = false;
    ok(if system {
        "the gateway runs in the background now, as `ferrule`, and starts at boot"
    } else {
        "the gateway runs in the background now, and starts at login"
    });
    for note in notes {
        if system {
            info(note);
        } else {
            warn(note);
        }
    }
    info(format!("Logs: {}", service::logs_hint()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrule_providers::Api;

    fn target(dir: &Path, text: &str) -> Target {
        let path = dir.join("sub/config.toml");
        if !text.is_empty() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
        }
        Target::load(path).unwrap()
    }

    #[test]
    fn the_anthropic_preset_writes_the_native_driver_down() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = target(dir.path(), "");
        for preset in ["anthropic", "openai"] {
            let p = PRESETS.iter().find(|p| p.name == preset).unwrap();
            write_provider(t.root(), &NewProvider::from_preset(p)).unwrap();
        }
        t.save().unwrap();
        let cfg = t.config().unwrap();
        assert_eq!(cfg.providers["anthropic"].api, Some(Api::Anthropic));
        assert_eq!(cfg.providers["openai"].api, None);
        assert_eq!(cfg.providers["openai"].api(), Api::Chat);
    }

    #[test]
    fn another_model_gets_its_own_table_and_a_new_default_provider_clears_models_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = target(dir.path(), "");
        for preset in ["openai", "groq"] {
            let p = PRESETS.iter().find(|p| p.name == preset).unwrap();
            write_provider(t.root(), &NewProvider::from_preset(p)).unwrap();
        }
        put(t.root(), "default_provider", "openai");
        write_extra_model(t.root(), "openai", "gpt-5-mini", Some("fast")).unwrap();
        put(table(t.root(), &["models"]).unwrap(), "default", "fast");
        t.save().unwrap();
        let cat = crate::models::Catalog::from_config(&t.config().unwrap());
        assert_eq!(cat.entries.len(), 3);
        assert_eq!(
            cat.default_entry().unwrap().0.reference(),
            "openai/gpt-5-mini"
        );
        make_provider_default(t.root(), "groq").unwrap();
        t.save().unwrap();
        let cfg = t.config().unwrap();
        assert_eq!(cfg.models.default, None);
        let cat = crate::models::Catalog::from_config(&cfg);
        assert_eq!(
            cat.default_entry().unwrap().0.reference(),
            "groq/llama-3.3-70b-versatile"
        );
        // The alias stays, pointing where it did.
        assert_eq!(
            cat.resolve("fast").unwrap().reference(),
            "openai/gpt-5-mini"
        );
        let mut names: Vec<&str> = PRESETS.iter().map(|p| p.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), PRESETS.len());
        assert!(names.contains(&"gemini"));
    }

    #[test]
    fn routing_is_written_on_and_back_off() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = target(dir.path(), "");
        for preset in ["openai", "groq"] {
            let p = PRESETS.iter().find(|p| p.name == preset).unwrap();
            write_provider(t.root(), &NewProvider::from_preset(p)).unwrap();
        }
        put(t.root(), "default_provider", "openai");
        let tiers = [
            "groq/llama-3.3-70b-versatile".to_string(),
            "openai/gpt-5.2".to_string(),
        ];
        write_routing(t.root(), &tiers, Some(2.5)).unwrap();
        t.save().unwrap();
        let cfg = t.config().unwrap();
        assert!(cfg.routing.enabled);
        assert_eq!(cfg.routing.tiers, tiers.to_vec());
        assert_eq!(cfg.routing.strong_daily_usd, Some(2.5));
        write_routing(t.root(), &tiers, None).unwrap();
        put(table(t.root(), &["routing"]).unwrap(), "enabled", false);
        t.save().unwrap();
        let cfg = t.config().unwrap();
        assert!(!cfg.routing.enabled);
        assert_eq!(cfg.routing.strong_daily_usd, None);
        assert_eq!(cfg.routing.tiers.len(), 2);
    }

    #[test]
    fn a_new_config_gets_a_header_and_every_part_parses() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = target(dir.path(), "");
        let mut np = NewProvider::from_preset(&PRESETS[0]);
        np.model = "gpt-5.2".into();
        write_provider(t.root(), &np).unwrap();
        put(t.root(), "default_provider", "openai");
        put(
            table(t.root(), &["gateway"]).unwrap(),
            "telegram_token_env",
            "TELEGRAM_BOT_TOKEN",
        );
        save_allowed(&mut t, &[42, -1001]).unwrap();
        let hosts = ["api.github.com".to_string(), "github.com".to_string()];
        write_secret_hosts(t.root(), "GITHUB_TOKEN", &hosts).unwrap();
        let sandbox = table(t.root(), &["sandbox"]).unwrap();
        put(sandbox, "mode", "read-only");
        put(sandbox, "network", false);
        t.save().unwrap();

        let text = std::fs::read_to_string(&t.path).unwrap();
        assert!(text.starts_with(HEADER), "{text}");
        assert!(!text.contains("[providers]\n"), "{text}");
        let cfg: config::Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.default_provider.as_deref(), Some("openai"));
        assert_eq!(cfg.providers["openai"].api_key_env, "OPENAI_API_KEY");
        assert_eq!(cfg.providers["openai"].profile, "openai");
        assert_eq!(cfg.gateway.telegram_allowed_chats, [42, -1001]);
        assert_eq!(
            ferrule_proxy::SecretRule::from(&cfg.secrets["GITHUB_TOKEN"]).hosts,
            hosts
        );
        assert_eq!(cfg.sandbox.mode, Mode::ReadOnly);
        assert!(!cfg.sandbox.network);

        // Saving again doesn't stack headers.
        t.save().unwrap();
        let again = std::fs::read_to_string(&t.path).unwrap();
        assert_eq!(again.matches("# ferrule configuration").count(), 1);
    }

    #[test]
    fn edits_keep_comments_prices_and_table_form_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let text = "# mine\ndefault_provider = \"a\"  # keep me\n\n[providers.a]\nbase_url = \"http://x\"\n\
                    api_key_env = \"A_KEY\"\nmodel = \"m1\"   # the model\nprice_input_per_mtok = 1.5\n\n\
                    [secrets]\nT = { hosts = [\"api.telegram.org\"], in_url = true }\n";
        let mut t = target(dir.path(), text);
        put(table(t.root(), &["providers", "a"]).unwrap(), "model", "m2");
        write_secret_hosts(t.root(), "T", &["x.org".to_string()]).unwrap();
        write_secret_hosts(t.root(), "GITHUB_TOKEN", &["github.com".to_string()]).unwrap();
        t.save().unwrap();

        let text = std::fs::read_to_string(&t.path).unwrap();
        assert!(text.starts_with("# mine\n"), "{text}");
        assert!(
            text.contains("default_provider = \"a\"  # keep me"),
            "{text}"
        );
        assert!(text.contains("model = \"m2\"   # the model"), "{text}");
        let cfg: config::Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.providers["a"].price_input_per_mtok, Some(1.5));
        let rule = ferrule_proxy::SecretRule::from(&cfg.secrets["T"]);
        assert_eq!(rule.hosts, ["x.org"]);
        assert!(rule.in_url);
        assert!(cfg.secrets.contains_key("GITHUB_TOKEN"));
    }

    #[test]
    fn a_save_that_would_break_the_config_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = target(dir.path(), "[gateway]\nlocal = true\n");
        put(table(t.root(), &["sandbox"]).unwrap(), "mode", "sideways");
        assert!(t.save().is_err());
        assert_eq!(
            std::fs::read_to_string(&t.path).unwrap(),
            "[gateway]\nlocal = true\n"
        );
        // A file that isn't a config isn't edited at all...
        std::fs::write(&t.path, "gateway = 3\n").unwrap();
        assert!(Target::load(t.path.clone()).is_err());
        // ...and a non-table in the way is an error, not a panic.
        let mut doc: DocumentMut = "gateway = 3\n".parse().unwrap();
        assert!(table(doc.as_table_mut(), &["gateway", "x"]).is_err());
    }

    #[test]
    fn the_network_presets_write_a_policy_that_loads() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = target(dir.path(), "[egress]\ndeny = [\"*.example.net\"]\n");
        write_egress(
            t.root(),
            true,
            &["nas.local".into(), "192.168.1.0/24".into()],
        )
        .unwrap();
        t.save().unwrap();
        let cfg = t.config().unwrap();
        assert_eq!(cfg.egress.default, "deny");
        assert_eq!(cfg.egress.allow.len(), PACKAGE_HOSTS.len());
        assert_eq!(cfg.egress.deny, ["*.example.net"]);
        let policy = cfg.egress.policy().unwrap();
        assert!(policy.has_rules());
        assert!(
            network_summary(&cfg).starts_with("13 hosts allowed"),
            "{}",
            network_summary(&cfg)
        );

        write_egress(t.root(), false, &[]).unwrap();
        t.save().unwrap();
        let cfg = t.config().unwrap();
        assert_eq!(cfg.egress.default, "allow");
        assert!(cfg.egress.allow.is_empty() && cfg.egress.private_allow.is_empty());
        assert_eq!(
            network_summary(&cfg),
            "public hosts but 1 rule · private ranges blocked"
        );
    }

    #[test]
    fn host_lists_are_split_and_trimmed() {
        assert_eq!(
            split_hosts(" API.x.com, *.y.org ,,"),
            ["api.x.com", "*.y.org"]
        );
        assert!(valid_provider_name("my-local_2"));
        assert!(!valid_provider_name("Bad Name"));
    }
}
