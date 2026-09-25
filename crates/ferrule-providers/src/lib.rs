//! Provider drivers, one per wire API, all behind the same `Provider` trait:
//! - `openai_compat` (`api = "chat"`): Chat Completions. Kimi (Moonshot),
//!   OpenAI, DeepSeek, OpenRouter, Groq, Ollama, llama.cpp, vLLM;
//! - `anthropic` (`api = "anthropic"`, M23): the native Messages API, with
//!   prompt caching and thinking carried through a tool loop;
//! - `responses` (`api = "responses"`, M23): OpenAI's Responses API,
//!   stateless, with encrypted reasoning carried through a tool loop.
//!
//! [`build`] is the one place a caller turns config into a driver.

pub mod anthropic;
mod common;
pub mod openai_compat;
pub mod responses;

pub use anthropic::AnthropicProvider;
pub use openai_compat::OpenAiCompatProvider;
pub use responses::ResponsesProvider;

use ferrule_core::provider::Provider;
use std::sync::Arc;

/// Which wire API a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Api {
    Chat,
    Anthropic,
    Responses,
}

impl Api {
    pub const NAMES: &'static str = "\"chat\", \"anthropic\" or \"responses\"";

    pub fn as_str(self) -> &'static str {
        match self {
            Api::Chat => "chat",
            Api::Anthropic => "anthropic",
            Api::Responses => "responses",
        }
    }

    pub fn parse(s: &str) -> Result<Api, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "chat" => Ok(Api::Chat),
            "anthropic" => Ok(Api::Anthropic),
            "responses" => Ok(Api::Responses),
            other => Err(format!("unknown api {other:?}: use {}", Api::NAMES)),
        }
    }
}

impl std::fmt::Display for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The api a provider with no `api` set speaks: Anthropic's own host gets
/// the native driver, everything else Chat Completions. Responses is never
/// guessed; it is always written.
pub fn infer_api(base_url: &str) -> Api {
    let rest = base_url
        .split_once("://")
        .map(|(_, r)| r)
        .unwrap_or(base_url);
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    let host = host.split(':').next().unwrap_or("").to_ascii_lowercase();
    if host == "api.anthropic.com" {
        Api::Anthropic
    } else {
        Api::Chat
    }
}

/// Anthropic's `thinking` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Thinking {
    /// `{type: "adaptive"}` (Claude 4.6 and later).
    Adaptive,
    /// `{type: "disabled"}`. A 400 on models that always think (Opus 5.5).
    Disabled,
    /// `{type: "enabled", budget_tokens: N}` (older models only; a 400 on
    /// Sonnet 5, Opus 4.7+ and Fable).
    Budget(u32),
}

impl Thinking {
    /// `"adaptive"`, `"disabled"` or a number of tokens.
    pub fn parse(s: &str) -> Result<Thinking, String> {
        let t = s.trim().to_ascii_lowercase();
        match t.as_str() {
            "adaptive" => Ok(Thinking::Adaptive),
            "disabled" | "off" => Ok(Thinking::Disabled),
            _ => match t.parse::<u32>() {
                Ok(n) if n >= 1024 => Ok(Thinking::Budget(n)),
                Ok(_) => Err("a thinking budget is at least 1024 tokens".into()),
                Err(_) => Err(format!(
                    "unknown thinking {s:?}: use \"adaptive\", \"disabled\" or a budget in tokens"
                )),
            },
        }
    }
}

impl std::fmt::Display for Thinking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Thinking::Adaptive => f.write_str("adaptive"),
            Thinking::Disabled => f.write_str("disabled"),
            Thinking::Budget(n) => write!(f, "{n}"),
        }
    }
}

/// Per-provider (or per-model) settings only the native drivers read.
/// All optional: unset means the model's own default. The chat driver
/// ignores them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct DriverOptions {
    /// Anthropic only.
    pub thinking: Option<Thinking>,
    /// Anthropic `output_config.effort`, Responses `reasoning.effort`.
    pub effort: Option<String>,
    /// The output cap when a request doesn't set one.
    pub max_tokens: Option<u32>,
}

/// The driver for `api`. `name` is the provider's name in config.
pub fn build(
    api: Api,
    name: impl Into<String>,
    base_url: impl Into<String>,
    api_key: impl Into<String>,
    model: impl Into<String>,
    options: DriverOptions,
) -> Arc<dyn Provider> {
    match api {
        Api::Chat => Arc::new(OpenAiCompatProvider::new(name, base_url, api_key, model)),
        Api::Anthropic => Arc::new(AnthropicProvider::new(
            name, base_url, api_key, model, options,
        )),
        Api::Responses => Arc::new(ResponsesProvider::new(
            name, base_url, api_key, model, options,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_anthropics_own_host_is_inferred_native() {
        assert_eq!(infer_api("https://api.anthropic.com/v1"), Api::Anthropic);
        assert_eq!(
            infer_api("https://API.anthropic.com:443/v1/"),
            Api::Anthropic
        );
        assert_eq!(infer_api("https://api.anthropic.com"), Api::Anthropic);
        assert_eq!(infer_api("https://api.openai.com/v1"), Api::Chat);
        assert_eq!(infer_api("https://openrouter.ai/api/v1"), Api::Chat);
        assert_eq!(infer_api("http://localhost:11434/v1"), Api::Chat);
        // Not a lookalike host, and not a path that mentions it.
        assert_eq!(infer_api("https://api.anthropic.com.evil.io/v1"), Api::Chat);
        assert_eq!(
            infer_api("https://proxy.io/api.anthropic.com/v1"),
            Api::Chat
        );
    }

    #[test]
    fn api_and_thinking_parse_their_words() {
        assert_eq!(Api::parse("Anthropic"), Ok(Api::Anthropic));
        assert_eq!(Api::parse("responses"), Ok(Api::Responses));
        assert!(Api::parse("messages").unwrap_err().contains("\"chat\""));
        assert_eq!(Thinking::parse("adaptive"), Ok(Thinking::Adaptive));
        assert_eq!(Thinking::parse("disabled"), Ok(Thinking::Disabled));
        assert_eq!(Thinking::parse("8000"), Ok(Thinking::Budget(8000)));
        assert!(Thinking::parse("100").is_err());
        assert!(Thinking::parse("lots").is_err());
    }
}
