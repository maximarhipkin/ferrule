//! `ferrule setup`: the interactive installer. The first run walks through
//! a model provider and its key, Telegram, tool credentials, the sandbox
//! and the background service; later runs open a menu to change any one
//! part. Answers are checked live where they can be (the key opens the
//! model list, the bot token answers `getMe`) and saved the moment they're
//! confirmed, so Ctrl-C never loses what's done. Keys go to the private
//! secrets file, never into the config, and config edits keep the file's
//! comments and layout.

use crate::{config, probe, secrets, service};
use anyhow::{anyhow, bail, Context, Result};
use ferrule_sandbox::{Mode, Sandbox};
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
fn has_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() || (cfg!(unix) && std::fs::File::open("/dev/tty").is_ok())
}

/// The first run: every part in order.
async fn guided(t: &mut Target, http: &reqwest::Client) -> Result<bool> {
    heading("Model provider");
    if settle(provider_step(t, http, true).await)?.quit() {
        return Ok(false);
    }
    heading("Telegram");
    if settle(telegram_step(t, http, true).await)?.quit() {
        return Ok(false);
    }
    heading("Tool credentials");
    if settle(credentials_step(t, http, true).await)?.quit() {
        return Ok(false);
    }
    heading("Sandbox");
    if settle(sandbox_step(t, true))?.quit() {
        return Ok(false);
    }
    if t.config()?.gateway.telegram_token_env.is_some() {
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
            format!("Tool credentials     {}", credentials_summary(&cfg)),
            format!("Sandbox              {}", sandbox_summary(&cfg)),
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
            2 => credentials_step(t, http, false).await,
            3 => sandbox_step(t, false),
            4 => service_step(t, false),
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

fn heading(title: &str) {
    println!("\n── {title}");
}

fn ok(text: impl std::fmt::Display) {
    println!("  ✓ {text}");
}

fn warn(text: impl std::fmt::Display) {
    println!("  ! {text}");
}

fn info(text: impl std::fmt::Display) {
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
struct Target {
    path: PathBuf,
    doc: DocumentMut,
    /// Something was saved this run (config or a key): worth a restart.
    changed: bool,
}

impl Target {
    fn load(path: PathBuf) -> Result<Self> {
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

    fn config(&self) -> Result<config::Config> {
        toml::from_str(&self.doc.to_string()).map_err(|e| anyhow!("{e}"))
    }

    fn root(&mut self) -> &mut dyn TableLike {
        self.doc.as_table_mut()
    }

    /// Check the edit still makes a valid config, then write it in one go.
    fn save(&mut self) -> Result<()> {
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
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("writing {}", self.path.display()))?;
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
fn table<'a>(root: &'a mut dyn TableLike, path: &[&str]) -> Result<&'a mut dyn TableLike> {
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
fn put(tbl: &mut dyn TableLike, key: &str, new: impl Into<Value>) {
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
    if cfg.providers.len() > 1 {
        text.push_str(&format!(" (+{} more)", cfg.providers.len() - 1));
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

fn sandbox_summary(cfg: &config::Config) -> String {
    let network = if cfg.sandbox.network {
        "network on"
    } else {
        "network off"
    };
    format!("{} · {network}", mode_name(cfg.sandbox.mode))
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
fn ask_secret(
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

fn no_shape(_: &str) -> Option<&'static str> {
    None
}

// ── Model provider ─────────────────────────────────────────────────────

struct Preset {
    label: &'static str,
    name: &'static str,
    base_url: &'static str,
    key_env: &'static str,
    profile: &'static str,
    /// Empty: pick from the provider's list.
    model: &'static str,
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
        label: "Anthropic (Claude)",
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
    let pick = Select::new("Which provider?", labels).raw_prompt()?.index;
    match names.get(pick) {
        Some(name) => edit_provider(t, http, name).await,
        None => add_provider(t, http).await,
    }
}

async fn add_provider(t: &mut Target, http: &reqwest::Client) -> Result<()> {
    let cfg = t.config()?;
    let mut labels: Vec<&str> = PRESETS.iter().map(|p| p.label).collect();
    labels.push("Another OpenAI-compatible server (vLLM, LM Studio, a gateway…)");
    let pick = Select::new("Which model provider?", labels)
        .with_page_size(PRESETS.len() + 1)
        .raw_prompt()?
        .index;
    let mut np = match PRESETS.get(pick) {
        Some(preset) => NewProvider::from_preset(preset),
        None => ask_custom_provider(&cfg)?,
    };
    if cfg.providers.contains_key(&np.name) {
        info(format!("You have `{}` already; this updates it.", np.name));
    }
    let (key, models) = ask_provider_key(http, &np).await?;
    np.model = pick_model(&models, &np.model)?;
    if let Some(key) = key {
        t.set_secret(&np.key_env, &key)?;
    }
    write_provider(t.root(), &np)?;
    let default = cfg
        .default_provider
        .as_ref()
        .filter(|d| cfg.providers.contains_key(*d));
    let make_default = match default {
        None => true,
        Some(d) if *d == np.name => false,
        Some(d) => Confirm::new(&format!("Use `{}` instead of `{d}` by default?", np.name))
            .with_default(false)
            .prompt()?,
    };
    if make_default {
        put(t.root(), "default_provider", np.name.as_str());
    }
    t.save()?;
    ok(format!("saved `{}` · {}", np.name, np.model));
    Ok(())
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
    Ok(())
}

async fn edit_provider(t: &mut Target, http: &reqwest::Client, name: &str) -> Result<()> {
    let cfg = t.config()?;
    let p = cfg.providers[name].clone();
    let is_default = cfg.default_provider.as_deref() == Some(name);
    let mut actions = vec!["Change the model", "Replace the API key"];
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
        }
        "Replace the API key" => {
            let (key, _) = ask_provider_key(http, &NewProvider::from_config(name, &p)).await?;
            if let Some(key) = key {
                t.set_secret(&p.api_key_env, &key)?;
                ok("key saved");
            }
        }
        "Make it the default" => {
            put(t.root(), "default_provider", name);
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

fn ask_hosts(current: &[String]) -> Result<Vec<String>> {
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

fn split_hosts(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Point `[secrets] NAME` at `hosts`; a table-form entry keeps its other
/// keys (`in_url`).
fn write_secret_hosts(root: &mut dyn TableLike, name: &str, hosts: &[String]) -> Result<()> {
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
    let default = workspace.map_or_else(|| "~/ferrule-workspace".to_string(), tilde);
    let answer = Text::new("Workspace: the folder the agent works in")
        .with_default(&default)
        .with_help_message("created if it doesn't exist")
        .prompt()?;
    let workspace = std::path::absolute(crate::expand_home(Path::new(answer.trim())))?;
    std::fs::create_dir_all(&workspace)
        .with_context(|| format!("creating {}", workspace.display()))?;
    let workspace = workspace.canonicalize()?;
    let data = config::data_dir()?;
    let data = data.canonicalize().unwrap_or(data);
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
    let exe = std::env::current_exe()?.canonicalize()?;
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
    ok("the gateway runs in the background now, and starts at login");
    for note in notes {
        warn(note);
    }
    info(format!("Logs: {}", service::logs_hint()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(dir: &Path, text: &str) -> Target {
        let path = dir.join("sub/config.toml");
        if !text.is_empty() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
        }
        Target::load(path).unwrap()
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
    fn host_lists_are_split_and_trimmed() {
        assert_eq!(
            split_hosts(" API.x.com, *.y.org ,,"),
            ["api.x.com", "*.y.org"]
        );
        assert!(valid_provider_name("my-local_2"));
        assert!(!valid_provider_name("Bad Name"));
    }
}
