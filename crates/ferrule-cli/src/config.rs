use anyhow::{anyhow, bail, Context, Result};
use ferrule_providers::{Api, DriverOptions, Thinking};
use ferrule_tools::search::{SafeSearch, SearchProvider, SearchSettings};
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
    /// M23: what a cache write costs. Unset: 1.25 × input on an
    /// `anthropic` provider (the 5-minute write price), input elsewhere.
    #[serde(default)]
    pub price_cache_write_per_mtok: Option<f64>,
    /// M23: the wire API, `chat` | `anthropic` | `responses`. Unset:
    /// `anthropic` for api.anthropic.com, `chat` for everything else.
    #[serde(default)]
    pub api: Option<Api>,
    /// M23, `anthropic` only: `"adaptive"`, `"disabled"` or a budget in
    /// tokens. Unset: the model's default, and no field is sent.
    #[serde(default)]
    pub thinking: Option<Thinking>,
    /// M23: `output_config.effort` (anthropic) or `reasoning.effort`
    /// (responses). Unset: the model's default.
    #[serde(default)]
    pub effort: Option<String>,
    /// M23: the output cap when a call doesn't set one (native drivers).
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// More models on the same endpoint and key (M21), besides `model`:
    /// `[providers.X.models."id"]`, each field falling back to the
    /// provider's.
    #[serde(default)]
    pub models: BTreeMap<String, ModelConfig>,
}

/// `[providers.X.models."id"]`: where a model differs from its provider.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    pub profile: Option<String>,
    /// Tokens; overrides the profile's window.
    pub context_window: Option<usize>,
    pub price_input_per_mtok: Option<f64>,
    pub price_cached_input_per_mtok: Option<f64>,
    pub price_output_per_mtok: Option<f64>,
    pub price_cache_write_per_mtok: Option<f64>,
    /// M23: the driver settings, over the provider's.
    pub thinking: Option<Thinking>,
    pub effort: Option<String>,
    pub max_tokens: Option<u32>,
    /// Where the prices came from when ferrule wrote them (M22): "openrouter
    /// catalog 2026-09-25". Unset: set by hand, and never overwritten.
    pub price_source: Option<String>,
}

/// `[models]` (docs/m21-models.md): the default, the fallback list and
/// aliases. Every entry is a reference: `provider/model`, a provider (its
/// own `model`), an alias or a model id only one provider has.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelsConfig {
    /// Unset: `default_provider`'s model.
    pub default: Option<String>,
    /// Tried in order when a model stays down after its retries. Empty:
    /// no fallback.
    pub fallback: Vec<String>,
    pub aliases: BTreeMap<String, String>,
    /// M22: a public model list (OpenRouter's shape) the dashboard and
    /// `ferrule model catalog` read prices from when no connected provider
    /// is OpenRouter. Unset: OpenRouter's; "": none.
    pub catalog_url: Option<String>,
}

/// `[routing]` (docs/m25-routing.md): start every turn on the cheap tier
/// and move up only on a failure. Off unless `enabled`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingConfig {
    pub enabled: bool,
    /// Model references, cheap first; two or more.
    pub tiers: Vec<String>,
    /// Back to the floor at the next turn; off: the level holds for the
    /// session.
    pub de_escalate: bool,
    /// A cap on today's (UTC) spend above tier 0; unset: none.
    pub strong_daily_usd: Option<f64>,
    pub triggers: TriggersConfig,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tiers: Vec::new(),
            de_escalate: true,
            strong_daily_usd: None,
            triggers: TriggersConfig::default(),
        }
    }
}

/// `[routing.triggers]`: which signals escalate. All on by default.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TriggersConfig {
    pub call_failed: bool,
    /// Invalid tool calls in a row; 0 = off.
    pub tool_errors: u32,
    pub checks: bool,
    pub stop_hooks: bool,
    /// The same tool call this many times in a row; 0 = off.
    pub no_progress: u32,
    pub watchdog: bool,
}

impl Default for TriggersConfig {
    fn default() -> Self {
        let p = ferrule_core::Policy::default();
        Self {
            call_failed: p.call_failed,
            tool_errors: p.tool_errors,
            checks: p.checks,
            stop_hooks: p.stop_hooks,
            no_progress: p.no_progress,
            watchdog: p.watchdog,
        }
    }
}

impl RoutingConfig {
    pub fn policy(&self) -> ferrule_core::Policy {
        let t = &self.triggers;
        ferrule_core::Policy {
            de_escalate: self.de_escalate,
            call_failed: t.call_failed,
            tool_errors: t.tool_errors,
            checks: t.checks,
            stop_hooks: t.stop_hooks,
            no_progress: t.no_progress,
            watchdog: t.watchdog,
        }
    }
}

impl ProviderConfig {
    /// The wire API: as written, else inferred from `base_url`.
    pub fn api(&self) -> Api {
        self.api
            .unwrap_or_else(|| ferrule_providers::infer_api(&self.base_url))
    }

    /// The driver settings for `model`: its own, else the provider's.
    pub fn driver_options(&self, model: &str) -> DriverOptions {
        let mc = self.models.get(model);
        DriverOptions {
            thinking: mc.and_then(|m| m.thinking).or(self.thinking),
            effort: mc
                .and_then(|m| m.effort.clone())
                .or_else(|| self.effort.clone()),
            max_tokens: mc.and_then(|m| m.max_tokens).or(self.max_tokens),
        }
    }

    /// A driver for `model` on this provider.
    pub fn client(
        &self,
        name: &str,
        key: impl Into<String>,
        model: &str,
    ) -> std::sync::Arc<dyn ferrule_core::provider::Provider> {
        ferrule_providers::build(
            self.api(),
            name,
            &self.base_url,
            key,
            model,
            self.driver_options(model),
        )
    }
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
    /// M27: how many read-only tool calls from one response may run at
    /// once. 1 runs them one after another.
    #[serde(default = "default_parallel_tools")]
    pub parallel_tools: usize,
    /// M27: stream replies as the model writes them, where the channel can
    /// edit a sent message (Telegram) and in `ferrule chat`.
    #[serde(default = "default_stream")]
    pub stream: bool,
    /// M29: offer `edit_file` (SEARCH/REPLACE). `write_file` stays either
    /// way; false is for a provider that misbehaves with the tool.
    #[serde(default = "default_true")]
    pub edit_file: bool,
    /// M29: the repo map's budget in tokens (chars / 4), in a workspace
    /// that looks like a code repo. 0 turns the map off; `code_search`
    /// stays.
    #[serde(default = "default_repo_map_tokens")]
    pub repo_map_tokens: usize,
    /// M29: after `edit_file`/`write_file`, run the project's own linter
    /// on the file and append what it reports. `auto`: when it's
    /// installed and the project has its config file; `off`: never.
    #[serde(default)]
    pub lint: LintMode,
    /// How long one linter run may take.
    #[serde(default = "default_lint_timeout_secs")]
    pub lint_timeout_secs: u64,
    /// M29: end each run that changed files in one git commit of exactly
    /// the files the agent changed (never the owner's dirty work). Off by
    /// default; `ferrule undo` / `/undo` take the latest one back.
    #[serde(default)]
    pub auto_commit: bool,
    /// `new`: commit on a `ferrule/auto-…` branch made at the first commit;
    /// `current`: on the branch HEAD is on.
    #[serde(default)]
    pub auto_commit_branch: crate::autocommit::BranchMode,
    /// `Name <email>`, the author and committer of agent commits.
    #[serde(default = "default_auto_commit_author")]
    pub auto_commit_author: String,
}

fn default_auto_commit_author() -> String {
    crate::autocommit::DEFAULT_AUTHOR.into()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LintMode {
    #[default]
    Auto,
    Off,
}

fn default_lint_timeout_secs() -> u64 {
    10
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            verify_command: None,
            verify_timeout_secs: default_verify_timeout_secs(),
            parallel_tools: default_parallel_tools(),
            stream: default_stream(),
            edit_file: true,
            repo_map_tokens: default_repo_map_tokens(),
            lint: LintMode::Auto,
            lint_timeout_secs: default_lint_timeout_secs(),
            auto_commit: false,
            auto_commit_branch: Default::default(),
            auto_commit_author: default_auto_commit_author(),
        }
    }
}

fn default_repo_map_tokens() -> usize {
    ferrule_codemap::DEFAULT_MAP_TOKENS
}

fn default_true() -> bool {
    true
}

fn default_stream() -> bool {
    true
}

fn default_parallel_tools() -> usize {
    4
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
    /// M27: whether Telegram replies stream; unset follows `[agent] stream`.
    #[serde(default)]
    pub telegram_stream: Option<bool>,
    /// M31: env var holding the Discord bot token. Unset = Discord off.
    pub discord_token_env: Option<String>,
    /// Discord's REST API, for tests; the Gateway URL comes from it.
    #[serde(default = "default_discord_api_url")]
    pub discord_api_url: String,
    /// Discord user ids whose DMs reach the agent. Empty = nobody: a DM is
    /// told its sender's id once, so it can be added here.
    #[serde(default)]
    pub discord_allowed_users: Vec<String>,
    /// Discord channel ids (and their threads) where anyone who mentions
    /// the bot, or replies to it, reaches the agent.
    #[serde(default)]
    pub discord_allowed_channels: Vec<String>,
    #[serde(default)]
    pub discord_stream: Option<bool>,
    /// M31: env var holding the Slack bot token (`xoxb-`). Slack runs when
    /// both it and the app token are set.
    pub slack_bot_token_env: Option<String>,
    /// Env var holding the Slack app-level token (`xapp-`), for Socket Mode.
    pub slack_app_token_env: Option<String>,
    /// Slack's Web API, for tests.
    #[serde(default = "default_slack_api_url")]
    pub slack_api_url: String,
    /// Slack member ids (`U…`) whose DMs reach the agent.
    #[serde(default)]
    pub slack_allowed_users: Vec<String>,
    /// Slack channel ids (`C…`) where an @mention reaches the agent; it
    /// answers in a thread.
    #[serde(default)]
    pub slack_allowed_channels: Vec<String>,
    #[serde(default)]
    pub slack_stream: Option<bool>,
}

fn default_telegram_base_url() -> String {
    "https://api.telegram.org".into()
}

fn default_discord_api_url() -> String {
    ferrule_gateway::channels::discord::API_URL.into()
}

fn default_slack_api_url() -> String {
    ferrule_gateway::channels::slack::API_URL.into()
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
        Self {
            tick_interval_secs: 30,
            gate_timeout_secs: 60,
            gate_workspace: PathBuf::from("."),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpConfig {
    /// One entry per stdio MCP server to spawn at startup. A server that
    /// fails to start is logged and skipped — it never stops the agent.
    #[serde(default, rename = "servers")]
    pub servers: Vec<ferrule_mcp::McpServerConfig>,
    /// Configured servers the owner turned off (M24): they stay in the
    /// config but aren't started.
    #[serde(default)]
    pub disabled: Vec<String>,
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
    /// M28: skills load when a person's message names one of their
    /// `triggers:` (docs/skills.md).
    pub triggers: bool,
    /// Most skills one message loads that way.
    pub max_triggered: usize,
    /// Their combined size, in tokens (4 characters each).
    pub trigger_budget_tokens: usize,
    /// Whether the workspace's own skills may trigger. Off: a cloned repo
    /// mustn't make its skills load on common words.
    pub project_triggers: bool,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            project: true,
            paths: Vec::new(),
            disabled: Vec::new(),
            triggers: true,
            max_triggered: 2,
            trigger_budget_tokens: 4000,
            project_triggers: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub default_provider: Option<String>,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    /// M21: the default model, the fallback list and aliases.
    #[serde(default)]
    pub models: ModelsConfig,
    /// M25: tiers, cheap first, and when to move up.
    #[serde(default)]
    pub routing: RoutingConfig,
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
    pub extensions: crate::self_extend::ExtensionsConfig,
    #[serde(default)]
    pub sandbox: ferrule_sandbox::Policy,
    /// Env var name → where its value may go (credential gateway).
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretSpec>,
    /// A real browser for the agent, off unless turned on.
    #[serde(default)]
    pub browser: ferrule_mcp::BrowserConfig,
    /// Sub-agents: whether an agent may start them, and their limits.
    #[serde(default)]
    pub agents: AgentsConfig,
    /// Lifecycle hooks (M18); read only from a trusted config file.
    #[serde(default)]
    pub hooks: ferrule_hooks::HooksConfig,
    /// M16's learning pass and the playbook in prompts.
    #[serde(default)]
    pub learning: crate::learn::LearningConfig,
    /// M19: spending caps, the kill switch and the approval gates.
    #[serde(default)]
    pub trust: ferrule_trust::TrustConfig,
    /// M19b: the gateway's watchdogs, restart notice and heartbeat.
    #[serde(default)]
    pub health: HealthConfig,
    /// M20: connected services and the relay their logins come back through.
    #[serde(default)]
    pub connections: ferrule_connections::ConnectionsConfig,
    #[serde(default)]
    pub dashboard: DashboardConfig,
    /// M24: where `ferrule model eval` and the dashboard find the suite.
    #[serde(default)]
    pub eval: EvalConfig,
    /// M28: the `web_search` tool, off unless a provider is set.
    #[serde(default)]
    pub web_search: WebSearchConfig,
    /// M30: how recall finds facts; keyword only unless an embedder is set.
    #[serde(default)]
    pub memory: MemoryConfig,
    /// M33: where ferrule's requests, and commands' through the proxy, may
    /// go (docs/egress.md).
    #[serde(default)]
    pub egress: EgressConfig,
    /// M33: the agent's work as OpenTelemetry traces, off unless an
    /// endpoint is set (docs/otel.md).
    #[serde(default)]
    pub telemetry: TelemetryConfig,
}

/// `[telemetry]` (docs/otel.md).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetryConfig {
    /// The collector's OTLP/HTTP base URL; `/v1/traces` is appended.
    /// Unset: nothing is exported.
    pub endpoint: Option<String>,
    /// Sent with every export. A value names its secret as `${VAR}`, never
    /// holds it.
    pub headers: BTreeMap<String, String>,
    /// Put prompts, replies and tool arguments and results on spans,
    /// scrubbed of every known secret.
    pub content: bool,
    pub service_name: Option<String>,
}

impl TelemetryConfig {
    /// Where spans are POSTed, `None` when export is off.
    pub fn traces_url(&self) -> Result<Option<String>> {
        let Some(raw) = self
            .endpoint
            .as_deref()
            .map(str::trim)
            .filter(|e| !e.is_empty())
        else {
            return Ok(None);
        };
        let mut url =
            url::Url::parse(raw).map_err(|e| anyhow!("[telemetry] endpoint = \"{raw}\": {e}"))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            bail!("[telemetry] endpoint = \"{raw}\": an http(s) URL is needed (OTLP/HTTP)");
        }
        if !url.username().is_empty() || url.password().is_some() {
            bail!(
                "[telemetry] endpoint: no credentials in the URL; send them as a header \
                 naming a [secrets] variable (docs/otel.md)"
            );
        }
        if !url.path().ends_with("/v1/traces") {
            let path = format!("{}/v1/traces", url.path().trim_end_matches('/'));
            url.set_path(&path);
        }
        Ok(Some(url.to_string()))
    }

    /// The host the collector is on, for binding header secrets to.
    pub fn host(&self) -> Option<String> {
        let url = url::Url::parse(&self.traces_url().ok()??).ok()?;
        url.host_str()
            .map(|h| h.trim_start_matches('[').trim_end_matches(']').to_string())
    }

    /// The `${VAR}`s the headers use.
    pub fn header_vars(&self) -> Vec<String> {
        let mut out = Vec::new();
        for value in self.headers.values() {
            let mut rest = value.as_str();
            while let Some(start) = rest.find("${") {
                let after = &rest[start + 2..];
                let Some(end) = after.find('}') else { break };
                out.push(after[..end].to_string());
                rest = &after[end + 1..];
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Refuses a header that holds a key rather than naming one.
    pub fn check_headers(&self) -> Result<()> {
        for (name, value) in &self.headers {
            let literal = strip_vars(value);
            let secret_name =
                ferrule_sandbox::looks_secret(name) || name.eq_ignore_ascii_case("authorization");
            let literal_key = literal
                .split(|c: char| c.is_whitespace() || c == ',' || c == ';')
                .any(looks_like_key);
            if literal_key || (secret_name && !value.contains("${") && !value.trim().is_empty()) {
                bail!(
                    "[telemetry] headers.{name} holds a key: put it in the environment, add the \
                     variable to [secrets], and write `${{VAR}}` here instead (docs/otel.md)"
                );
            }
        }
        Ok(())
    }
}

/// `value` with each `${VAR}` taken out.
fn strip_vars(value: &str) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        match rest[start..].find('}') {
            Some(end) => rest = &rest[start + end + 1..],
            None => {
                rest = "";
            }
        }
        out.push(' ');
    }
    out.push_str(rest);
    out
}

/// Whether a word is shaped like an API key: a known prefix, or 20+ token
/// characters mixing letters and digits.
fn looks_like_key(word: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "sk-",
        "sk_",
        "pk_",
        "rk_",
        "xox",
        "xapp-",
        "ghp_",
        "gho_",
        "ghs_",
        "github_pat_",
        "glpat-",
        "AKIA",
        "AIza",
        "hf_",
    ];
    let word = word.trim_matches(|c| c == '"' || c == '\'');
    if word.len() >= 12 && PREFIXES.iter().any(|p| word.starts_with(p)) {
        return true;
    }
    word.len() >= 20
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.+/=".contains(c))
        && word.chars().any(|c| c.is_ascii_digit())
        && word.chars().any(|c| c.is_ascii_alphabetic())
}

/// `[egress]` (docs/egress.md).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct EgressConfig {
    /// `allow` (public hosts go) or `deny` (only `allow` goes).
    pub default: String,
    /// Host patterns (`*.example.com`), IPs or CIDRs, `:port` optional.
    pub allow: Vec<String>,
    /// Always wins, over `allow` and configured endpoints alike.
    pub deny: Vec<String>,
    /// `block` (loopback for ferrule's own requests, the LAN, link-local,
    /// cloud metadata) or `allow`.
    pub private: String,
    /// Private hosts, IPs or CIDRs reachable anyway. The metadata address
    /// needs its exact IP here.
    pub private_allow: Vec<String>,
}

impl Default for EgressConfig {
    fn default() -> Self {
        Self {
            default: "allow".into(),
            allow: Vec::new(),
            deny: Vec::new(),
            private: "block".into(),
            private_allow: Vec::new(),
        }
    }
}

impl EgressConfig {
    /// The policy as written, before ferrule adds its configured endpoints.
    pub fn policy(&self) -> Result<ferrule_proxy::EgressPolicy> {
        let default_deny = match self.default.as_str() {
            "allow" => false,
            "deny" => true,
            other => bail!("[egress] default = \"{other}\": use \"allow\" or \"deny\""),
        };
        let block_private = match self.private.as_str() {
            "block" => true,
            "allow" => false,
            other => bail!("[egress] private = \"{other}\": use \"block\" or \"allow\""),
        };
        ferrule_proxy::EgressPolicy::new(
            default_deny,
            &self.allow,
            &self.deny,
            block_private,
            &self.private_allow,
        )
    }
}

/// `[memory]` (docs/memory.md).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryConfig {
    /// `off` (keyword recall, as before M30), `local` (the downloaded
    /// model2vec model) or `openai` (any `/v1/embeddings` endpoint).
    pub embedder: String,
    /// `weighted` or `rrf`.
    pub merge: String,
    /// The cosine's share of a weighted merge, 0 to 1.
    pub vector_weight: f64,
    /// Rows less similar than this to the query aren't vector matches.
    pub min_similarity: f32,
    /// `openai`: borrow a `[providers.X]`'s `base_url` and `api_key_env`.
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub model: Option<String>,
    /// The vectors' length; known for OpenAI's own models.
    pub dimensions: Option<usize>,
    pub price_input_per_mtok: Option<f64>,
    /// `ferrule memory reindex`'s pace against the endpoint; 0: no limit.
    pub max_requests_per_minute: u32,
    pub timeout_secs: u64,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            embedder: "off".into(),
            merge: "weighted".into(),
            vector_weight: 0.7,
            min_similarity: 0.3,
            provider: None,
            base_url: None,
            api_key_env: None,
            model: None,
            dimensions: None,
            price_input_per_mtok: None,
            max_requests_per_minute: 60,
            timeout_secs: 5,
        }
    }
}

/// `[memory]`'s embedder, checked.
#[derive(Debug, Clone, PartialEq)]
pub enum EmbedderChoice {
    Off,
    Local,
    Endpoint(EndpointSettings),
}

/// `embedder = "openai"`: everything but the key's placeholder.
#[derive(Debug, Clone, PartialEq)]
pub struct EndpointSettings {
    pub base_url: String,
    pub host: String,
    pub model: String,
    pub dim: usize,
    pub send_dimensions: bool,
    pub key_env: Option<String>,
    pub price_input_per_mtok: Option<f64>,
    pub max_requests_per_minute: u32,
    pub timeout: std::time::Duration,
}

/// The vector length of OpenAI's own embedding models.
fn known_dim(model: &str) -> Option<usize> {
    match model {
        "text-embedding-3-small" | "text-embedding-ada-002" => Some(1536),
        "text-embedding-3-large" => Some(3072),
        _ => None,
    }
}

impl MemoryConfig {
    pub fn hybrid(&self) -> Result<ferrule_memory::Hybrid> {
        if !(0.0..=1.0).contains(&self.vector_weight) {
            bail!("[memory] vector_weight must be 0 to 1");
        }
        if !(0.0..=1.0).contains(&self.min_similarity) {
            bail!("[memory] min_similarity must be 0 to 1");
        }
        let merge = match self.merge.as_str() {
            "weighted" => ferrule_memory::Merge::Weighted {
                vector_weight: self.vector_weight,
            },
            "rrf" => ferrule_memory::Merge::Rrf { k: 60.0 },
            other => bail!("[memory] merge = \"{other}\": expected weighted or rrf"),
        };
        Ok(ferrule_memory::Hybrid {
            merge,
            min_similarity: self.min_similarity,
        })
    }

    /// The checked embedder; `providers` is for `provider = "X"`.
    pub fn choice(&self, providers: &HashMap<String, ProviderConfig>) -> Result<EmbedderChoice> {
        self.hybrid()?;
        match self.embedder.as_str() {
            "off" | "" => return Ok(EmbedderChoice::Off),
            "local" => return Ok(EmbedderChoice::Local),
            "openai" => {}
            other => bail!("[memory] embedder = \"{other}\": expected off, local or openai"),
        }
        let borrowed =
            match &self.provider {
                None => None,
                Some(name) => Some(providers.get(name).ok_or_else(|| {
                    anyhow!("[memory] provider = \"{name}\" isn't in [providers]")
                })?),
            };
        let Some(base_url) = self
            .base_url
            .clone()
            .or_else(|| borrowed.map(|p| p.base_url.clone()))
        else {
            bail!("[memory] embedder = \"openai\" needs `base_url` (or `provider`)");
        };
        let host = match url::Url::parse(&base_url) {
            Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => u
                .host_str()
                .unwrap_or_default()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string(),
            _ => bail!("[memory] base_url = \"{base_url}\" isn't an http(s) URL"),
        };
        let Some(model) = self.model.clone().filter(|m| !m.is_empty()) else {
            bail!("[memory] embedder = \"openai\" needs `model`, e.g. \"text-embedding-3-small\"");
        };
        let Some(dim) = self.dimensions.or_else(|| known_dim(&model)) else {
            bail!("[memory] model = \"{model}\" needs `dimensions`, the length of its vectors");
        };
        if dim == 0 {
            bail!("[memory] dimensions must be at least 1");
        }
        if self
            .price_input_per_mtok
            .is_some_and(|p| !(p >= 0.0 && p.is_finite()))
        {
            bail!("[memory] price_input_per_mtok can't be negative");
        }
        let key_env = self
            .api_key_env
            .clone()
            .or_else(|| borrowed.map(|p| p.api_key_env.clone()))
            .filter(|k| !k.is_empty());
        Ok(EmbedderChoice::Endpoint(EndpointSettings {
            base_url: base_url.trim_end_matches('/').to_string(),
            host,
            // Only OpenAI's text-embedding-3 models take `dimensions`, and
            // only when it's asked for; other servers reject the field.
            send_dimensions: self.dimensions.is_some() && model.starts_with("text-embedding-3"),
            model,
            dim,
            key_env,
            price_input_per_mtok: self.price_input_per_mtok,
            max_requests_per_minute: self.max_requests_per_minute,
            timeout: std::time::Duration::from_secs(self.timeout_secs.max(1)),
        }))
    }
}

/// `[web_search]` (docs/web-search.md).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebSearchConfig {
    /// `brave`, `tavily`, `searxng` or `exa`; unset: no `web_search` tool.
    pub provider: Option<String>,
    /// The env var holding the API key. Bound to the endpoint's host in
    /// `[secrets]` unless it's there already.
    pub api_key_env: Option<String>,
    /// The API's base URL: the provider's own by default, required for
    /// SearXNG.
    pub endpoint: Option<String>,
    pub max_results: usize,
    pub safe_search: String,
    pub region: Option<String>,
    pub language: Option<String>,
    pub max_output_tokens: usize,
    pub timeout_secs: u64,
    /// Charged per search toward `[trust]`'s dollar caps.
    pub price_per_search_usd: Option<f64>,
    /// 0: no cap.
    pub max_searches_per_day: u64,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            provider: None,
            api_key_env: None,
            endpoint: None,
            max_results: 5,
            safe_search: "moderate".into(),
            region: None,
            language: None,
            max_output_tokens: 1500,
            timeout_secs: 20,
            price_per_search_usd: None,
            max_searches_per_day: 0,
        }
    }
}

impl WebSearchConfig {
    /// The checked provider, or `None` when search is off.
    pub fn provider(&self) -> Result<Option<SearchProvider>> {
        let Some(name) = self.provider.as_deref() else {
            return Ok(None);
        };
        let p = SearchProvider::parse(name).ok_or_else(|| {
            anyhow!("[web_search] provider = \"{name}\": expected brave, tavily, searxng or exa")
        })?;
        Ok(Some(p))
    }

    /// The base URL searches go to.
    pub fn endpoint(&self, p: SearchProvider) -> Option<String> {
        self.endpoint
            .clone()
            .or_else(|| p.default_endpoint().map(str::to_string))
    }

    /// The endpoint's host, which the key is bound to.
    pub fn host(&self, p: SearchProvider) -> Option<String> {
        let endpoint = self.endpoint(p)?;
        url::Url::parse(&endpoint)
            .ok()?
            .host_str()
            .map(|h| h.trim_start_matches('[').trim_end_matches(']').to_string())
    }

    /// Everything the tool needs but the key's placeholder.
    pub fn settings(&self) -> Result<Option<SearchSettings>> {
        let Some(p) = self.provider()? else {
            return Ok(None);
        };
        let Some(endpoint) = self.endpoint(p) else {
            bail!("[web_search] provider = \"searxng\" needs `endpoint`, your instance's URL");
        };
        match url::Url::parse(&endpoint) {
            Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => {}
            _ => bail!("[web_search] endpoint = \"{endpoint}\" isn't an http(s) URL"),
        }
        if p.needs_key() && self.api_key_env.as_deref().unwrap_or("").is_empty() {
            bail!(
                "[web_search] provider = \"{}\" needs `api_key_env`, the env var holding its key",
                p.name()
            );
        }
        if !(1..=ferrule_tools::search::MAX_RESULTS).contains(&self.max_results) {
            bail!(
                "[web_search] max_results must be 1 to {}",
                ferrule_tools::search::MAX_RESULTS
            );
        }
        let safe_search = SafeSearch::parse(&self.safe_search).ok_or_else(|| {
            anyhow!(
                "[web_search] safe_search = \"{}\": expected off, moderate or strict",
                self.safe_search
            )
        })?;
        if self
            .price_per_search_usd
            .is_some_and(|p| !(p >= 0.0 && p.is_finite()))
        {
            bail!("[web_search] price_per_search_usd can't be negative");
        }
        if self.max_output_tokens < 100 {
            bail!("[web_search] max_output_tokens must be at least 100");
        }
        let mut s = SearchSettings::new(p, endpoint);
        s.key_env = self.api_key_env.clone().filter(|e| !e.is_empty());
        s.max_results = self.max_results;
        s.safe_search = safe_search;
        s.region = self.region.clone().filter(|r| !r.is_empty());
        s.language = self.language.clone().filter(|l| !l.is_empty());
        s.max_output_tokens = self.max_output_tokens;
        s.timeout = std::time::Duration::from_secs(self.timeout_secs.max(1));
        Ok(Some(s))
    }
}

/// `[eval]` (docs/m24-dashboard-2.md §2).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EvalConfig {
    /// The starter suite's directory; unset: `$FERRULE_EVAL_SUITE`, then
    /// `./evals/starter`, then the checkout the binary was built from.
    pub suite: Option<PathBuf>,
}

/// `[dashboard]` (docs/m22-dashboard.md).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DashboardConfig {
    /// The gateway serves the page on 127.0.0.1.
    pub enabled: bool,
    /// 0: any free port (written to `<data>/gateway/dashboard.json`).
    pub port: u16,
    /// `tunnel`: `/dashboard` opens a cloudflared quick tunnel for the
    /// phone; `off`: local links only.
    pub remote: String,
    /// A session, and the tunnel, close after this long without a request.
    pub idle_minutes: u64,
    /// A session ends this long after its login regardless.
    pub session_hours: u64,
    /// An unused login link expires after this.
    pub link_minutes: u64,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 0,
            remote: "tunnel".into(),
            idle_minutes: 30,
            session_hours: 12,
            link_minutes: 10,
        }
    }
}

/// `[health]` (docs/m19b-reliability.md).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HealthConfig {
    /// A polling channel (Telegram) with no successful poll for this long
    /// is stale: `/status` says so.
    pub poll_stale_secs: u64,
    /// A turn with no progress (no model call or tool call started or
    /// finished) for this long gets one message to the owner. 0 = off.
    pub watchdog_after_secs: u64,
    /// A turn running this long is ended the way `/stop` ends it, and its
    /// chat is free again. 0 = no limit.
    pub max_turn_minutes: u64,
    /// Tell the owner every time the gateway starts, not only after an
    /// unclean exit.
    pub notify_on_start: bool,
    /// A URL that gets `{status, reason, version, uptime_secs}` POSTed
    /// every `heartbeat_secs`, for a dead man's switch outside the
    /// machine. Empty = no heartbeat.
    pub heartbeat_url: String,
    pub heartbeat_secs: u64,
    /// Telegram refusing `getUpdates` with 409 Conflict (another program
    /// polling with the token, or a webhook) this long gets one message to
    /// the owner, and as long without one after gets "recovered" (M19c).
    pub telegram_conflict_secs: u64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            poll_stale_secs: 300,
            watchdog_after_secs: 600,
            max_turn_minutes: 60,
            notify_on_start: false,
            heartbeat_url: String::new(),
            heartbeat_secs: 60,
            telegram_conflict_secs: 60,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentsConfig {
    /// false: no agent tools, nobody starts sub-agents.
    pub enabled: bool,
    pub max_depth: u32,
    pub max_children: usize,
    pub max_agents: usize,
    /// Tokens all of one tree's sub-agents may use per window.
    pub max_tokens: u64,
    pub budget_window_hours: u32,
    /// Per role (`worker`, `planner`, `verifier`): a provider other than
    /// the one the root runs on.
    pub roles: HashMap<String, RoleConfig>,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        let l = ferrule_agents::Limits::default();
        Self {
            enabled: true,
            max_depth: l.max_depth,
            max_children: l.max_children,
            max_agents: l.max_agents,
            max_tokens: l.max_tokens,
            budget_window_hours: (l.budget_window_secs / 3600) as u32,
            roles: HashMap::new(),
        }
    }
}

impl AgentsConfig {
    pub fn limits(&self) -> ferrule_agents::Limits {
        ferrule_agents::Limits {
            max_depth: self.max_depth,
            max_children: self.max_children,
            max_agents: self.max_agents,
            max_tokens: self.max_tokens,
            budget_window_secs: i64::from(self.budget_window_hours.max(1)) * 3600,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleConfig {
    /// A name from `[providers]`: its own model.
    pub provider: Option<String>,
    /// A model reference (M21); not with `provider`.
    pub model: Option<String>,
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
            SecretSpec::Table(t) => Self {
                hosts: t.hosts.clone(),
                in_url: t.in_url,
            },
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

# More models on the same key; each field is optional (the provider's if unset).
# [providers.openai.models."gpt-5.2-mini"]
# context_window = 400000
# price_input_per_mtok = 0.25
# price_cached_input_per_mtok = 0.025
# price_output_per_mtok = 2.0

# Which model answers, and what takes over in an outage. A model is named
# `provider/model`, a provider (its `model` above), or an alias.
# `ferrule model default <ref>` and Telegram's `/model default <ref>` edit this.
# [models]
# default = "openai/gpt-5.2"
# fallback = []                   # e.g. ["kimi"]; empty = no fallback
# [models.aliases]
# mini = "openai/gpt-5.2-mini"

# Any OpenAI-compatible endpoint works: OpenRouter, DeepSeek, Ollama, vLLM…
# [providers.local]
# base_url = "http://localhost:11434/v1"
# api_key_env = "OLLAMA_API_KEY"   # set to any non-empty value
# model = "qwen3-coder"
# profile = "generic"

# [agent]
# verify_command = "cargo test"   # ferrule runs it before a run that changed files ends
# verify_timeout_secs = 600
# parallel_tools = 4              # read-only tool calls from one response run at once; 1 = one by one
# stream = true                   # replies grow as the model writes (Telegram, `ferrule chat`)
# edit_file = true                # offer edit_file (SEARCH/REPLACE); write_file stays either way
# repo_map_tokens = 1024          # repo map budget in a code repo (tokens); 0 = no map
# lint = "auto"                   # after an edit, run the project's linter (rustfmt/ruff/gofmt/eslint/tsc) if installed and configured; "off"
# lint_timeout_secs = 10
# auto_commit = false            # commit each run's own files (never yours) to git; `ferrule undo` reverts
# auto_commit_branch = "new"      # "new": a ferrule/auto-* branch; "current": the branch you're on
# auto_commit_author = "ferrule <ferrule@localhost>"

# [gateway]
# local = true                              # enable the stdin/stdout channel
# telegram_token_env = "TELEGRAM_BOT_TOKEN"  # unset = Telegram disabled
# telegram_allowed_chats = []               # chat ids the bot answers; empty =
#                                           # nobody (it replies with the chat id)
# telegram_base_url = "https://api.telegram.org"
# telegram_stream = true                    # unset = follow [agent] stream
# discord_token_env = "DISCORD_BOT_TOKEN"    # unset = Discord disabled (docs/discord.md)
# discord_allowed_users = []                # user ids whose DMs it answers
# discord_allowed_channels = []             # channels where an @mention reaches it
# discord_stream = true
# slack_bot_token_env = "SLACK_BOT_TOKEN"    # xoxb-; Slack needs both tokens
# slack_app_token_env = "SLACK_APP_TOKEN"    # xapp-, for Socket Mode (docs/slack.md)
# slack_allowed_users = []                  # member ids (U…) whose DMs it answers
# slack_allowed_channels = []               # channels where an @mention reaches it
# slack_stream = true

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
# writable_roots = []        # extra writable dirs for this server only; it
#                            # always gets the workspace, tmp and its own state
#                            # dir (HOME, caches) under the data dir, plus network
# sandbox = true             # false: no OS sandbox for it (doctor warns)
# enabled_tools = []         # offer only these tools ("prefix*" ok); empty: all
# max_output_chars = 20000   # cap on one tool result; output_caps = { tool = N }
#                            # caps one tool. `ferrule mcp add` writes entries
#                            # like these after starting and scanning the server;
#                            # running gateways pick up changes here, no restart
#
# [[mcp.servers]]           # or a remote one, over Streamable HTTP: `url`
# name = "remote"            # instead of `command`. With [secrets] it goes
# url = "https://mcp.example.com/mcp"  # through the credential proxy, so a
# headers = { Authorization = "Bearer ${EXAMPLE_TOKEN}" }  # secret's
#                            # placeholder is swapped for the real value only
#                            # on its hosts; ${VAR} is read from ferrule's env.

# [skills]                  # Agent Skills (SKILL.md folders, Claude-compatible).
# enabled = true             # Searched: <workspace>/.ferrule|.agents|.claude/skills,
# project = true             # then `paths`, ~/.config/ferrule/skills,
# paths = []                 # ~/.agents/skills, ~/.claude/skills. First name wins.
# disabled = []              # skill names to ignore. `ferrule skills` lists them all.
# triggers = true            # load a skill when your message names its `triggers:`
# max_triggered = 2          # skills one message loads that way
# trigger_budget_tokens = 4000   # their combined size
# project_triggers = false   # let the workspace's skills trigger too

# [web_search]              # The web_search tool (docs/web-search.md). Off until a
# provider = "brave"         # provider is set: brave, tavily, exa or searxng.
# api_key_env = "BRAVE_API_KEY"  # Bound to the endpoint's host through the proxy;
#                            # the model and the sandbox only ever see a placeholder.
# endpoint = "https://api.search.brave.com"  # required for searxng
# max_results = 5            # 1..20; the model may ask for fewer.
# safe_search = "moderate"   # off, moderate or strict
# max_output_tokens = 1500   # results are trimmed to fit
# timeout_secs = 20
# price_per_search_usd = 0.005  # counts toward [trust]'s dollar caps
# max_searches_per_day = 0   # 0: no cap. Every search is a ledger row.

# [memory]                  # Long-term memory recall (docs/memory.md). Keyword
# embedder = "local"         # search only until an embedder is set: "local" (a
#                            # 531 MB multilingual model, `ferrule memory model
#                            # download`; no key, nothing leaves the machine) or
#                            # "openai" (any /v1/embeddings endpoint, below).
# merge = "weighted"         # or "rrf"; weighted won the benchmark
# vector_weight = 0.7        # the similarity's share of a weighted merge
# min_similarity = 0.3       # less similar facts match by keyword only
# provider = "openai"        # embedder = "openai": borrow a [providers.X]'s
# base_url = "https://api.openai.com/v1"  # base_url and api_key_env, or set
# api_key_env = "OPENAI_API_KEY"  # them. The key goes through the proxy.
# model = "text-embedding-3-small"
# dimensions = 512           # required for models other than OpenAI's own
# price_input_per_mtok = 0.02  # every request is a ledger row
# max_requests_per_minute = 60  # `ferrule memory reindex`'s pace

# [telemetry]               # OpenTelemetry traces of the agent's work over OTLP/HTTP
# endpoint = "http://127.0.0.1:4318"  # (docs/otel.md). Off until set; /v1/traces
#                            # is appended. Sessions, turns, model and tool calls.
# headers = { "x-honeycomb-team" = "${HONEYCOMB_API_KEY}" }  # name a variable, never
#                            # a key; a secret-looking one is bound to the endpoint.
# content = false            # true: prompts, replies and tool I/O too, scrubbed.
# service_name = "ferrule"

# [extensions]              # Self-extension: the agent installs MCP servers and
# enabled = false            # skills mid-run (mcp_add, skill_install, skill_keep…).
# allow = []                 # Installable without asking, exact pins only, e.g.
#                            # ["npm:@modelcontextprotocol/*", "git:https://github.com/me/*"].
#                            # Anything else waits for `ferrule extensions approve`.
#                            # Read only from --config or the global config, never
#                            # from ./ferrule.toml. Every tool text is scanned.

# [learning]                # The learning pass: reviews failed or retried runs,
# enabled = true             # keeps a lesson in data/learn/playbook.md only when the
# schedule = "0 3 * * *"     # task's check passes with it, merges duplicate memories.
# timezone = "UTC"           # Off by default: it spends unattended and changes every
# playbook = true            # prompt. playbook = false keeps lessons out of prompts.
# max_usd_per_pass = 0.5     # Caps come from the ledger (call_kind "learn"); the pass
# max_usd_per_day = 1.0      # stops cleanly at either. `ferrule learn show|diff|revert`.

# [sandbox]                 # OS sandbox for the shell tool (Landlock / Seatbelt /
#                            # a restricted token on Windows). docs/sandbox.md
# mode = "workspace-write"   # or "read-only", or "off". `ferrule sandbox` shows
#                            # what applies here and tests it.
# require = false            # true: refuse to start if it can't be applied
# network = true             # false: shell commands can't open network sockets
# writable_roots = []        # extra writable dirs, e.g. ["~/.cargo", "~/.cache"]
#                            # for build caches; relative = inside the workspace
# tmp = true                 # /tmp and $TMPDIR stay writable
# scrub_secret_env = true    # drop *KEY*/*TOKEN*/*SECRET*… and api_key_env vars
# env_passthrough = []       # names to keep anyway, e.g. ["GITHUB_TOKEN"]
# deny_read = []             # extra paths commands and the file tools can't
#                            # read, e.g. ["~/work/.env.production"]
# deny_default_reads = true  # ~/.ssh, cloud credential dirs, browser profiles
# allow_read = []            # re-open one of those, e.g. ["~/.kube"]
# process_limit = 256        # Windows: most processes per command tree (0 = none);
#                            # memory_mb = 4096 caps the tree's memory there

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
# DISCORD_BOT_TOKEN = ["discord.com"]
# SLACK_BOT_TOKEN = ["slack.com"]

# [browser]                 # A real headless Chrome for pages that need
# enabled = false            # JavaScript, a login or clicks, driven by
#                            # agent-browser 0.38+ (npm i -g agent-browser).
#                            # Only an installed Chrome is used; `ferrule
#                            # setup` → Browser turns it on. It runs in the
#                            # sandbox, with its profile under the data dir,
#                            # and through the credential proxy with [secrets].
# chrome = "/usr/bin/google-chrome"   # unset = the first one found
# allowed_domains = []       # e.g. ["example.com", "*.example.org"]; empty = any
# chrome_sandbox = true      # false: Chrome without its own sandbox, for root
#                            # and containers. See docs/browser.md first.

# [agents]                  # Sub-agents: an agent can start others in the
# enabled = true             # background, wait for their reports, resume and
#                            # close them. See docs/agents.md; they cost tokens.
# max_depth = 2              # levels below the agent you talk to
# max_children = 4           # running at once, per parent
# max_agents = 12            # open (not closed) per tree
# max_tokens = 2000000       # all of a tree's sub-agents together, per window;
# budget_window_hours = 24   # the agent you talk to isn't counted
# [agents.roles.verifier]   # a role on another provider, from [providers]
# provider = "local"
#
# [trust]                    # M19 (docs/m19-trust-cost.md). Caps are 0 = off;
# max_tokens_per_run = 5000000  # a run is one prompt and its sub-agents.
# max_usd_per_run = 5.0      # Dollars need prices in [providers.*].
# max_tokens_per_day = 50000000 # Day and task caps are read from the ledger,
# max_usd_per_day = 20.0     # so they count every ferrule process.
# max_tokens_per_task = 0    # One scheduled task's day, sub-agents included.
# max_usd_per_task = 0.0
# warn_at = 0.8              # Telegram warning, once per cap and window.
# timezone = "UTC"           # Where a day starts (an IANA zone).
# owner_chat = 123456789     # Approvals and warnings; unset: the first private
#                            # chat in [gateway] telegram_allowed_chats.
#                            # discord_owner = "<user id>", slack_owner = "U…":
#                            # the owner on those; unset: the first allowed user.
#                            # owner_channel = "discord": whose chat gets the
#                            # approvals and warnings; unset: Telegram, else
#                            # Discord, else Slack.
# approval_timeout_secs = 600 # No answer refuses the command.
# plan_timeout_secs = 3600
# gates = true               # Ask before rm -rf, force pushes, DELETE to a bound host.
#
# [health]                   # M19b (docs/m19b-reliability.md): the gateway is
#                            # never silently deaf. /status answers from any chat.
# poll_stale_secs = 300      # a chat channel with no ok poll (Telegram) or
#                            # socket frame (Discord, Slack) this long is stale.
# watchdog_after_secs = 600  # a turn with no progress this long: one message to
#                            # the owner ("stuck on … — /stop to cancel"). 0 = off.
# max_turn_minutes = 60      # a turn this long is ended like /stop. 0 = no limit.
# notify_on_start = false    # "back up" on every start; after a crash or a kill
#                            # the owner hears it anyway, with the interrupted turn.
# heartbeat_url = ""         # POSTed {status: ok|degraded, reason, version,
#                            # uptime_secs} every heartbeat_secs, e.g. a
#                            # healthchecks.io check that alerts when it stops.
#                            # The reason never holds messages or secrets.
# heartbeat_secs = 60
# telegram_conflict_secs = 60 # Telegram's 409 Conflict (another program polling
#                            # with the token, or a webhook) this long: one
#                            # message to the owner, and one when it clears.
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
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let cfg = cfg
            .finish()
            .with_context(|| format!("in {}", path.display()))?;
        Ok((cfg, path))
    }

    /// Checks what parsing can't, and fills in what other settings imply:
    /// `[web_search]`'s and `[memory]`'s keys, bound to their endpoint's
    /// host in `[secrets]` unless the owner bound them there already.
    pub fn finish(mut self) -> Result<Self> {
        self.memory.hybrid()?;
        self.egress.policy()?;
        if let EmbedderChoice::Endpoint(e) = self.memory.choice(&self.providers)? {
            if let Some(var) = e.key_env {
                self.secrets
                    .entry(var)
                    .or_insert_with(|| SecretSpec::Hosts(vec![e.host]));
            }
        }
        if let Some(url) = self.telemetry.traces_url()? {
            self.telemetry.check_headers()?;
            // A secret-looking header variable is bound to the collector,
            // so the export carries its placeholder and the proxy swaps it.
            if let Some(host) = self.telemetry.host() {
                for var in self.telemetry.header_vars() {
                    if ferrule_sandbox::looks_secret(&var) {
                        self.secrets
                            .entry(var)
                            .or_insert_with(|| SecretSpec::Hosts(vec![host.clone()]));
                    }
                }
            }
            tracing::debug!("telemetry: exporting to {url}");
        }
        if let Some(settings) = self.web_search.settings()? {
            if let (Some(var), Some(host)) = (
                settings.key_env.clone(),
                self.web_search.host(settings.provider),
            ) {
                self.secrets
                    .entry(var)
                    .or_insert_with(|| SecretSpec::Hosts(vec![host]));
            }
        }
        Ok(self)
    }

    pub fn resolve_provider(
        &self,
        name: Option<&str>,
    ) -> Result<(String, &ProviderConfig, String)> {
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

/// `$FERRULE_DATA_DIR` if set (the system service's), else `ferrule` in
/// the platform's data dir.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("FERRULE_DATA_DIR").filter(|d| !d.is_empty()) {
        let dir = PathBuf::from(dir);
        std::fs::create_dir_all(&dir)?;
        return Ok(dir);
    }
    // Windows: the local AppData, so saved keys don't roam with a profile.
    let dir = if cfg!(windows) {
        dirs::data_local_dir()
    } else {
        dirs::data_dir()
    }
    .ok_or_else(|| anyhow!("no data dir"))?
    .join("ferrule");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_web_search_block_parses_uncommented_and_binds_its_key() {
        let start = EXAMPLE_CONFIG.find("# [web_search]").unwrap();
        let end = start + EXAMPLE_CONFIG[start..].find("\n\n").unwrap();
        let uncommented: String = EXAMPLE_CONFIG[start..end]
            .lines()
            .map(|l| format!("{}\n", l.strip_prefix("# ").unwrap_or(l)))
            .collect();
        let cfg: Config = toml::from_str(&uncommented).unwrap();
        let cfg = cfg.finish().unwrap();
        let s = cfg.web_search.settings().unwrap().unwrap();
        assert_eq!(s.provider, SearchProvider::Brave);
        assert_eq!(s.key_env.as_deref(), Some("BRAVE_API_KEY"));
        let rule = ferrule_proxy::SecretRule::from(&cfg.secrets["BRAVE_API_KEY"]);
        assert_eq!(rule.hosts, ["api.search.brave.com"]);
    }

    #[test]
    fn example_memory_block_parses_uncommented() {
        let start = EXAMPLE_CONFIG.find("# [memory]").unwrap();
        let end = start + EXAMPLE_CONFIG[start..].find("\n\n").unwrap();
        let uncommented: String = EXAMPLE_CONFIG[start..end]
            .lines()
            .map(|l| format!("{}\n", l.strip_prefix("# ").unwrap_or(l)))
            .collect();
        let providers = "[providers.openai]\nbase_url = \"https://api.openai.com/v1\"\n\
                         api_key_env = \"OPENAI_API_KEY\"\nmodel = \"gpt-5\"\n";
        let cfg: Config = toml::from_str(&format!("{providers}{uncommented}")).unwrap();
        let cfg = cfg.finish().unwrap();
        assert!(matches!(
            cfg.memory.choice(&cfg.providers).unwrap(),
            EmbedderChoice::Local
        ));
        // The local model binds no key.
        assert!(cfg.secrets.is_empty());
        assert_eq!(
            cfg.memory.hybrid().unwrap(),
            ferrule_memory::Hybrid::default()
        );

        let endpoint = uncommented.replace("embedder = \"local\"", "embedder = \"openai\"");
        let cfg: Config = toml::from_str(&format!("{providers}{endpoint}")).unwrap();
        let cfg = cfg.finish().unwrap();
        let EmbedderChoice::Endpoint(e) = cfg.memory.choice(&cfg.providers).unwrap() else {
            panic!("not an endpoint");
        };
        assert_eq!(e.base_url, "https://api.openai.com/v1");
        assert_eq!(
            (e.model.as_str(), e.dim, e.send_dimensions),
            ("text-embedding-3-small", 512, true)
        );
        assert_eq!(e.key_env.as_deref(), Some("OPENAI_API_KEY"));
        assert_eq!(e.price_input_per_mtok, Some(0.02));
        let rule = ferrule_proxy::SecretRule::from(&cfg.secrets["OPENAI_API_KEY"]);
        assert_eq!(rule.hosts, ["api.openai.com"]);
    }

    #[test]
    fn memory_is_keyword_only_by_default_and_checked_when_on() {
        let off: Config = toml::from_str("").unwrap();
        let off = off.finish().unwrap();
        assert!(matches!(
            off.memory.choice(&off.providers).unwrap(),
            EmbedderChoice::Off
        ));
        let bad = |t: &str| {
            let c: Config = toml::from_str(t).unwrap();
            c.finish().unwrap_err().to_string()
        };
        let endpoint = "[memory]\nembedder = \"openai\"\n";
        assert!(bad("[memory]\nembedder = \"bert\"").contains("expected off, local or openai"));
        assert!(bad("[memory]\nmerge = \"max\"").contains("merge"));
        assert!(bad("[memory]\nvector_weight = 1.5").contains("vector_weight"));
        assert!(bad("[memory]\nmin_similarity = -0.1").contains("min_similarity"));
        assert!(bad(endpoint).contains("base_url"));
        assert!(bad(&format!("{endpoint}provider = \"nope\"")).contains("isn't in [providers]"));
        assert!(bad(&format!("{endpoint}base_url = \"ftp://x\"")).contains("http(s)"));
        assert!(bad(&format!("{endpoint}base_url = \"http://127.0.0.1:1\"")).contains("model"));
        assert!(bad(&format!(
            "{endpoint}base_url = \"http://127.0.0.1:1\"\nmodel = \"nomic-embed-text\""
        ))
        .contains("dimensions"));
        assert!(bad(&format!(
            "{endpoint}base_url = \"http://127.0.0.1:1\"\nmodel = \"m\"\ndimensions = 0"
        ))
        .contains("at least 1"));
        assert!(bad(&format!(
            "{endpoint}base_url = \"http://h\"\nmodel = \"text-embedding-3-small\"\nprice_input_per_mtok = -1.0"
        ))
        .contains("negative"));

        // A local server with no key, a model that isn't OpenAI's: no
        // `dimensions` sent, nothing bound.
        let c: Config = toml::from_str(&format!(
            "{endpoint}base_url = \"http://127.0.0.1:11434/v1/\"\nmodel = \"nomic-embed-text\"\ndimensions = 768"
        ))
        .unwrap();
        let c = c.finish().unwrap();
        let EmbedderChoice::Endpoint(e) = c.memory.choice(&c.providers).unwrap() else {
            panic!("not an endpoint");
        };
        assert_eq!(e.base_url, "http://127.0.0.1:11434/v1");
        assert_eq!((e.dim, e.send_dimensions, e.key_env), (768, false, None));
        assert!(c.secrets.is_empty());
        // An owner's own binding wins.
        let c: Config = toml::from_str(&format!(
            "[secrets]\nK = [\"proxy.example\"]\n{endpoint}base_url = \"https://api.openai.com/v1\"\n\
             model = \"text-embedding-3-large\"\napi_key_env = \"K\""
        ))
        .unwrap();
        let c = c.finish().unwrap();
        let rule = ferrule_proxy::SecretRule::from(&c.secrets["K"]);
        assert_eq!(rule.hosts, ["proxy.example"]);
        let EmbedderChoice::Endpoint(e) = c.memory.choice(&c.providers).unwrap() else {
            panic!("not an endpoint");
        };
        assert_eq!((e.dim, e.send_dimensions), (3072, false));
        assert!(toml::from_str::<Config>("[memory]\nembeder = \"local\"").is_err());
    }

    #[test]
    fn web_search_is_off_by_default_and_checked_when_on() {
        let off: Config = toml::from_str("").unwrap();
        assert!(off.web_search.settings().unwrap().is_none());
        assert!(off.finish().unwrap().secrets.is_empty());
        let bad = |t: &str| {
            let c: Config = toml::from_str(t).unwrap();
            c.finish().unwrap_err().to_string()
        };
        assert!(bad("[web_search]\nprovider = \"bing\"").contains("expected brave"));
        assert!(bad("[web_search]\nprovider = \"brave\"").contains("api_key_env"));
        assert!(bad("[web_search]\nprovider = \"searxng\"").contains("endpoint"));
        assert!(
            bad("[web_search]\nprovider = \"tavily\"\napi_key_env = \"K\"\nmax_results = 50")
                .contains("max_results")
        );
        assert!(
            bad("[web_search]\nprovider = \"exa\"\napi_key_env = \"K\"\nsafe_search = \"on\"")
                .contains("safe_search")
        );
        // Keyless SearXNG binds nothing; an owner's own binding wins.
        let c: Config = toml::from_str(
            "[web_search]\nprovider = \"searxng\"\nendpoint = \"http://127.0.0.1:8888\"",
        )
        .unwrap();
        assert!(c.finish().unwrap().secrets.is_empty());
        let c: Config = toml::from_str(
            "[secrets]\nK = [\"proxy.example\"]\n[web_search]\nprovider = \"exa\"\napi_key_env = \"K\"",
        )
        .unwrap();
        let c = c.finish().unwrap();
        assert_eq!(
            ferrule_proxy::SecretRule::from(&c.secrets["K"]).hosts,
            ["proxy.example"]
        );
    }

    #[test]
    fn example_config_parses_with_the_sandbox_secrets_and_browser_blocks_uncommented() {
        let cfg: Config = toml::from_str(EXAMPLE_CONFIG).unwrap();
        assert_eq!(cfg.sandbox.mode, ferrule_sandbox::Mode::WorkspaceWrite);
        let block = &EXAMPLE_CONFIG[EXAMPLE_CONFIG.find("# [sandbox]").unwrap()..];
        let uncommented: String = block
            .lines()
            .map(|l| format!("{}\n", l.strip_prefix("# ").unwrap_or(l)))
            .collect();
        let cfg: Config = toml::from_str(&uncommented).unwrap();
        assert!(
            cfg.sandbox.network
                && cfg.sandbox.tmp
                && cfg.sandbox.scrub_secret_env
                && !cfg.sandbox.require
        );
        assert!(cfg.sandbox.writable_roots.is_empty() && cfg.sandbox.env_passthrough.is_empty());
        assert!(
            cfg.sandbox.deny_default_reads
                && cfg.sandbox.deny_read.is_empty()
                && cfg.sandbox.allow_read.is_empty()
                && cfg.sandbox.process_limit == 256
        );
        let rule = |name: &str| ferrule_proxy::SecretRule::from(&cfg.secrets[name]);
        assert_eq!(
            rule("GITHUB_TOKEN").hosts,
            ["api.github.com", "*.githubusercontent.com"]
        );
        assert!(!rule("GITHUB_TOKEN").in_url);
        assert_eq!(rule("TELEGRAM_BOT_TOKEN").hosts, ["api.telegram.org"]);
        assert!(rule("TELEGRAM_BOT_TOKEN").in_url);
        assert!(!cfg.browser.enabled && cfg.browser.chrome_sandbox);
        assert!(cfg.browser.chrome.is_some() && cfg.browser.allowed_domains.is_empty());
        assert!(cfg.agents.enabled);
        assert_eq!(cfg.agents.limits(), ferrule_agents::Limits::default());
        assert_eq!(
            cfg.trust,
            ferrule_trust::TrustConfig {
                owner_chat: Some(123456789),
                ..Default::default()
            }
        );
        assert_eq!(
            cfg.agents.roles["verifier"].provider.as_deref(),
            Some("local")
        );
    }

    #[test]
    fn telegram_allowed_chats_default_to_empty() {
        let cfg: Config = toml::from_str("[gateway]\ntelegram_token_env = \"T\"").unwrap();
        assert!(cfg.gateway.telegram_allowed_chats.is_empty());
        let cfg: Config =
            toml::from_str("[gateway]\ntelegram_allowed_chats = [42, -1001234]").unwrap();
        assert_eq!(cfg.gateway.telegram_allowed_chats, [42, -1001234]);
    }

    #[test]
    fn a_misspelt_secret_table_is_an_error() {
        assert!(
            toml::from_str::<Config>("[secrets]\nT = { hosts = [\"x.com\"], in_uri = true }")
                .is_err()
        );
        let cfg: Config = toml::from_str("[secrets]\nT = { hosts = [\"x.com\"] }").unwrap();
        assert!(!ferrule_proxy::SecretRule::from(&cfg.secrets["T"]).in_url);
    }

    #[test]
    fn example_telemetry_block_parses_uncommented_and_binds_its_header_key() {
        let start = EXAMPLE_CONFIG.find("# [telemetry]").unwrap();
        let end = start + EXAMPLE_CONFIG[start..].find("\n\n").unwrap();
        let uncommented: String = EXAMPLE_CONFIG[start..end]
            .lines()
            .map(|l| format!("{}\n", l.strip_prefix("# ").unwrap_or(l)))
            .collect();
        let cfg: Config = toml::from_str(&uncommented).unwrap();
        let cfg = cfg.finish().unwrap();
        assert_eq!(
            cfg.telemetry.traces_url().unwrap().as_deref(),
            Some("http://127.0.0.1:4318/v1/traces")
        );
        let rule = ferrule_proxy::SecretRule::from(&cfg.secrets["HONEYCOMB_API_KEY"]);
        assert_eq!(rule.hosts, ["127.0.0.1"]);
        assert!(crate::egress::endpoints(&cfg).contains(&("127.0.0.1".into(), 4318)));
    }

    #[test]
    fn telemetry_is_off_by_default_and_its_url_is_checked() {
        let t = |toml: &str| toml::from_str::<Config>(toml).unwrap().telemetry;
        assert_eq!(t("").traces_url().unwrap(), None);
        assert_eq!(
            t("[telemetry]\nendpoint = \"https://otel.example.com/v1/traces\"")
                .traces_url()
                .unwrap()
                .as_deref(),
            Some("https://otel.example.com/v1/traces")
        );
        assert_eq!(
            t("[telemetry]\nendpoint = \"https://api.example.com/otlp/\"")
                .traces_url()
                .unwrap()
                .as_deref(),
            Some("https://api.example.com/otlp/v1/traces")
        );
        for bad in [
            "grpc://h:4317",
            "https://user:pw@h.example.com",
            "not a url",
        ] {
            let e = t(&format!("[telemetry]\nendpoint = \"{bad}\"")).traces_url();
            assert!(e.is_err(), "{bad}");
        }
        assert!(toml::from_str::<Config>("[telemetry]\nendpont = \"x\"").is_err());
    }

    #[test]
    fn a_telemetry_header_holding_a_key_is_refused() {
        let load = |headers: &str| {
            toml::from_str::<Config>(&format!(
                "[telemetry]\nendpoint = \"https://api.honeycomb.io\"\nheaders = {{ {headers} }}"
            ))
            .unwrap()
            .finish()
        };
        for raw in [
            r#"authorization = "Bearer abc""#,
            r#"x-honeycomb-team = "hcaik_01j9zq8x7c6v5b4n3m2l1k0j9h8g""#,
            r#"x-custom = "sk-ant-api03-abcdefghijklmnop""#,
            r#"x-api-key = "plain""#,
        ] {
            let e = load(raw).unwrap_err().to_string();
            assert!(e.contains("[secrets]"), "{raw}: {e}");
        }
        for fine in [
            r#"authorization = "Bearer ${OTEL_TOKEN}""#,
            r#"x-scope-orgid = "tenant-1""#,
            r#"x-honeycomb-dataset = "ferrule""#,
        ] {
            assert!(load(fine).is_ok(), "{fine}");
        }
        let cfg = load(r#"authorization = "Bearer ${OTEL_TOKEN}""#).unwrap();
        let rule = ferrule_proxy::SecretRule::from(&cfg.secrets["OTEL_TOKEN"]);
        assert_eq!(rule.hosts, ["api.honeycomb.io"]);
    }
}
