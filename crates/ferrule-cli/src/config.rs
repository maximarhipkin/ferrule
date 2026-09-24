use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    pub base_url: String,
    /// Env var holding the API key (never the key itself in the file).
    pub api_key_env: String,
    pub model: String,
    /// Harness profile: kimi | openai | anthropic | generic
    #[serde(default = "default_profile")]
    pub profile: String,
    /// Optional USD-per-million-token prices, for the ledger's `cost_usd`
    /// column (`ferrule ledger`). Absent = cost stays null, never guessed.
    /// All three must be set for a call to get a cost — a partial price set
    /// would silently undercount.
    #[serde(default)]
    pub price_input_per_mtok: Option<f64>,
    #[serde(default)]
    pub price_cached_input_per_mtok: Option<f64>,
    #[serde(default)]
    pub price_output_per_mtok: Option<f64>,
}

fn default_profile() -> String {
    "generic".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentSettings {
    /// Check ferrule runs itself before a run that changed files may finish
    /// (e.g. "cargo test", "npm test"). A failure goes back to the agent to
    /// fix, a few rounds at most; then the run ends with a status.
    pub verify_command: Option<String>,
    /// How long the check may take before it counts as failed.
    #[serde(default = "default_verify_timeout_secs")]
    pub verify_timeout_secs: u64,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self { verify_command: None, verify_timeout_secs: default_verify_timeout_secs() }
    }
}

fn default_verify_timeout_secs() -> u64 {
    600
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GatewayConfig {
    /// Enable the stdin/stdout local channel (mostly for smoke-testing the
    /// gateway itself without any external service).
    #[serde(default)]
    pub local: bool,
    /// Env var holding the Telegram bot token. Unset/absent = Telegram
    /// channel disabled. Never the token itself in the file.
    pub telegram_token_env: Option<String>,
    #[serde(default = "default_telegram_base_url")]
    pub telegram_base_url: String,
    /// Telegram chat ids the bot answers. Empty = nobody: the bot replies
    /// once per chat with that chat's id, so it can be added here. A group's
    /// id lets every member of that group in.
    #[serde(default)]
    pub telegram_allowed_chats: Vec<i64>,
}

fn default_telegram_base_url() -> String {
    "https://api.telegram.org".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SchedulerConfig {
    /// How often the scheduler checks for due tasks.
    pub tick_interval_secs: u64,
    /// Gate script timeout before it's killed and the run is marked failed.
    pub gate_timeout_secs: u64,
    /// Working directory gate scripts run in. Defaults to the current
    /// directory so behavior is identical between the daemon and a one-off
    /// `ferrule tasks run-now`.
    pub gate_workspace: PathBuf,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self { tick_interval_secs: 30, gate_timeout_secs: 60, gate_workspace: PathBuf::from(".") }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpConfig {
    /// One entry per stdio MCP server to spawn at startup. A server that
    /// fails to start is logged and skipped — it never stops the agent.
    #[serde(default, rename = "servers")]
    pub servers: Vec<ferrule_mcp::McpServerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    /// Master switch: false = no catalog in the prompt, no skill tools.
    pub enabled: bool,
    /// Load skills from the workspace (`.ferrule/skills`, `.agents/skills`,
    /// `.claude/skills`). A cloned repo's skills end up in the system
    /// prompt, so turn this off when pointing the agent at untrusted repos.
    pub project: bool,
    /// Extra skill directories, searched before the default user ones.
    pub paths: Vec<PathBuf>,
    /// Skill names to ignore wherever they're found.
    pub disabled: Vec<String>,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self { enabled: true, project: true, paths: Vec::new(), disabled: Vec::new() }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub default_provider: Option<String>,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub agent: AgentSettings,
    #[serde(default)]
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub skills: SkillsConfig,
    #[serde(default)]
    pub sandbox: ferrule_sandbox::Policy,
    /// Env var name → where its value may go (credential gateway).
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretSpec>,
}

/// A `[secrets]` entry: the allowed hosts, or a table that also opts the
/// secret into URL substitution.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum SecretSpec {
    Hosts(Vec<String>),
    Table(SecretTable),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretTable {
    pub hosts: Vec<String>,
    #[serde(default)]
    pub in_url: bool,
}

impl From<&SecretSpec> for ferrule_proxy::SecretRule {
    fn from(spec: &SecretSpec) -> Self {
        match spec {
            SecretSpec::Hosts(hosts) => hosts.clone().into(),
            SecretSpec::Table(t) => Self { hosts: t.hosts.clone(), in_url: t.in_url },
        }
    }
}

pub const EXAMPLE_CONFIG: &str = r#"# ferrule configuration — `ferrule setup` writes and edits this for you.
# API keys never go in this file: they live in environment variables, or in
# the private secrets file `ferrule setup` keeps (a real env var wins).

default_provider = "kimi"

[providers.kimi]
base_url = "https://api.moonshot.ai/v1"
api_key_env = "MOONSHOT_API_KEY"
model = "kimi-k2.6"
profile = "kimi"
# price_input_per_mtok = 0.60         # USD / 1M input tokens — optional, for
# price_cached_input_per_mtok = 0.15  # `ferrule ledger`'s cost_usd column.
# price_output_per_mtok = 2.50        # Omit any of the three and cost stays null.

[providers.openai]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
model = "gpt-5.2"
profile = "openai"

# Any OpenAI-compatible endpoint works: OpenRouter, DeepSeek, Ollama, vLLM…
# [providers.local]
# base_url = "http://localhost:11434/v1"
# api_key_env = "OLLAMA_API_KEY"   # set to any non-empty value
# model = "qwen3-coder"
# profile = "generic"

# [agent]
# verify_command = "cargo test"   # ferrule runs it before a run that changed files ends
# verify_timeout_secs = 600

# [gateway]
# local = true                              # enable the stdin/stdout channel
# telegram_token_env = "TELEGRAM_BOT_TOKEN"  # unset = Telegram disabled
# telegram_allowed_chats = []               # chat ids the bot answers; empty =
#                                           # nobody (it replies with the chat id)
# telegram_base_url = "https://api.telegram.org"

# [scheduler]
# tick_interval_secs = 30   # how often to check for due tasks
# gate_timeout_secs = 60    # kill a gate script that runs longer than this
# gate_workspace = "."      # working directory for gate scripts

# [[mcp.servers]]           # each entry spawns one stdio MCP server; its
# name = "fs"                # tools register as mcp__fs__<tool>
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
# env = {}
# timeout_secs = 60          # per-call timeout, optional (default 60)

# [skills]                  # Agent Skills (SKILL.md folders, Claude-compatible).
# enabled = true             # Searched: <workspace>/.ferrule|.agents|.claude/skills,
# project = true             # then `paths`, ~/.config/ferrule/skills,
# paths = []                 # ~/.agents/skills, ~/.claude/skills. First name wins.
# disabled = []              # skill names to ignore. `ferrule skills` lists them all.

# [sandbox]                 # OS sandbox for the shell tool (Landlock / Seatbelt).
# mode = "workspace-write"   # or "read-only", or "off". `ferrule sandbox` shows
#                            # what applies here and tests it.
# require = false            # true: refuse to start if it can't be applied
# network = true             # false: shell commands can't open network sockets
# writable_roots = []        # extra writable dirs, e.g. ["~/.cargo", "~/.cache"]
#                            # for build caches; relative = inside the workspace
# tmp = true                 # /tmp and $TMPDIR stay writable
# scrub_secret_env = true    # drop *KEY*/*TOKEN*/*SECRET*… and api_key_env vars
# env_passthrough = []       # names to keep anyway, e.g. ["GITHUB_TOKEN"]

# [secrets]                 # Credential gateway: the shell tool's commands get a
#                            # same-shaped placeholder in $NAME, never the real
#                            # value. ferrule's local HTTPS proxy swaps the real
#                            # value in only on requests to the listed hosts, in
#                            # the Authorization header or a credential-named one
#                            # (x-api-key, PRIVATE-TOKEN…), and back out of their
#                            # responses. Anywhere else it stays a placeholder.
#                            # Name = env var ferrule reads; value = allowed hosts.
# GITHUB_TOKEN = ["api.github.com", "*.githubusercontent.com"]
#                            # APIs that take the key in the URL need an opt-in:
#                            # hosts often keep URLs where the model can read them.
# TELEGRAM_BOT_TOKEN = { hosts = ["api.telegram.org"], in_url = true }
"#;

/// `~/.config/ferrule/config.toml` (or the platform's equivalent).
pub fn global_config_path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .ok_or_else(|| anyhow!("no config dir"))?
        .join("ferrule")
        .join("config.toml"))
}

/// The file `Config::load` reads: `$FERRULE_CONFIG` (what `--config` sets),
/// else `./ferrule.toml`, else the global one. `None` when none exists;
/// `$FERRULE_CONFIG` is returned even if missing, so the error names it.
pub fn config_path() -> Result<Option<PathBuf>> {
    if let Some(path) = std::env::var_os("FERRULE_CONFIG").filter(|p| !p.is_empty()) {
        return Ok(Some(PathBuf::from(path)));
    }
    let local = PathBuf::from("ferrule.toml");
    if local.exists() {
        return Ok(Some(local));
    }
    let global = global_config_path()?;
    Ok(global.exists().then_some(global))
}

impl Config {
    pub fn load() -> Result<(Self, PathBuf)> {
        let Some(path) = config_path()? else {
            bail!("no config found. Run `ferrule setup` first.")
        };
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let cfg = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok((cfg, path))
    }

    pub fn resolve_provider(&self, name: Option<&str>) -> Result<(String, &ProviderConfig, String)> {
        let name = name
            .map(|s| s.to_string())
            .or_else(|| self.default_provider.clone())
            .ok_or_else(|| anyhow!("no provider selected and no default_provider set"))?;
        let cfg = self
            .providers
            .get(&name)
            .ok_or_else(|| anyhow!("provider `{name}` not in config"))?;
        let key = std::env::var(&cfg.api_key_env).with_context(|| {
            format!(
                "env var `{}` not set (needed by provider `{name}`) — run `ferrule setup`, or export it",
                cfg.api_key_env
            )
        })?;
        Ok((name, cfg, key))
    }
}

pub fn data_dir() -> Result<PathBuf> {
    // Windows: the local AppData, so saved keys don't roam with a profile.
    let dir = if cfg!(windows) { dirs::data_local_dir() } else { dirs::data_dir() }
        .ok_or_else(|| anyhow!("no data dir"))?
        .join("ferrule");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses_with_the_sandbox_and_secrets_blocks_uncommented() {
        let cfg: Config = toml::from_str(EXAMPLE_CONFIG).unwrap();
        assert_eq!(cfg.sandbox.mode, ferrule_sandbox::Mode::WorkspaceWrite);
        let block = &EXAMPLE_CONFIG[EXAMPLE_CONFIG.find("# [sandbox]").unwrap()..];
        let uncommented: String = block.lines().map(|l| format!("{}\n", l.strip_prefix("# ").unwrap_or(l))).collect();
        let cfg: Config = toml::from_str(&uncommented).unwrap();
        assert!(cfg.sandbox.network && cfg.sandbox.tmp && cfg.sandbox.scrub_secret_env && !cfg.sandbox.require);
        assert!(cfg.sandbox.writable_roots.is_empty() && cfg.sandbox.env_passthrough.is_empty());
        let rule = |name: &str| ferrule_proxy::SecretRule::from(&cfg.secrets[name]);
        assert_eq!(rule("GITHUB_TOKEN").hosts, ["api.github.com", "*.githubusercontent.com"]);
        assert!(!rule("GITHUB_TOKEN").in_url);
        assert_eq!(rule("TELEGRAM_BOT_TOKEN").hosts, ["api.telegram.org"]);
        assert!(rule("TELEGRAM_BOT_TOKEN").in_url);
    }

    #[test]
    fn telegram_allowed_chats_default_to_empty() {
        let cfg: Config = toml::from_str("[gateway]\ntelegram_token_env = \"T\"").unwrap();
        assert!(cfg.gateway.telegram_allowed_chats.is_empty());
        let cfg: Config = toml::from_str("[gateway]\ntelegram_allowed_chats = [42, -1001234]").unwrap();
        assert_eq!(cfg.gateway.telegram_allowed_chats, [42, -1001234]);
    }

    #[test]
    fn a_misspelt_secret_table_is_an_error() {
        assert!(toml::from_str::<Config>("[secrets]\nT = { hosts = [\"x.com\"], in_uri = true }").is_err());
        let cfg: Config = toml::from_str("[secrets]\nT = { hosts = [\"x.com\"] }").unwrap();
        assert!(!ferrule_proxy::SecretRule::from(&cfg.secrets["T"]).in_url);
    }
}
