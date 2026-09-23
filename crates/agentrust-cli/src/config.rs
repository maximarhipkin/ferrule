use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
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
}

fn default_profile() -> String {
    "generic".into()
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentSettings {
    /// Validation command the agent must run green before finishing
    /// (e.g. "cargo test", "npm test"). "The build system is truth":
    /// IF verify == PASS THEN submit ELSE fix forward.
    pub verify_command: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub default_provider: Option<String>,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub agent: AgentSettings,
}

pub const EXAMPLE_CONFIG: &str = r#"# agentrust configuration
# API keys live in environment variables, never in this file.

default_provider = "kimi"

[providers.kimi]
base_url = "https://api.moonshot.ai/v1"
api_key_env = "MOONSHOT_API_KEY"
model = "kimi-k2.6"
profile = "kimi"

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
# verify_command = "cargo test"   # agent must make this pass before finishing
"#;

impl Config {
    pub fn load() -> Result<(Self, PathBuf)> {
        let local = PathBuf::from("agentrust.toml");
        if local.exists() {
            let text = std::fs::read_to_string(&local)?;
            return Ok((toml::from_str(&text)?, local));
        }
        let global = dirs::config_dir()
            .ok_or_else(|| anyhow!("no config dir"))?
            .join("agentrust")
            .join("config.toml");
        if global.exists() {
            let text = std::fs::read_to_string(&global)?;
            return Ok((toml::from_str(&text)?, global));
        }
        bail!("no config found. Run `agentrust config init` first.")
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
        let key = std::env::var(&cfg.api_key_env)
            .with_context(|| format!("env var `{}` not set (needed by provider `{name}`)", cfg.api_key_env))?;
        Ok((name, cfg, key))
    }
}

pub fn data_dir() -> Result<PathBuf> {
    let dir = dirs::data_dir()
        .ok_or_else(|| anyhow!("no data dir"))?
        .join("agentrust");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
