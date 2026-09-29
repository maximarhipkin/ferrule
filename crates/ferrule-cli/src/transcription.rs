//! M41 §2: `[transcription]`, which backend turns voice messages into text,
//! and the gateway's [`Transcription`] built from it.

use crate::config::{Config, ProviderConfig};
use anyhow::{anyhow, bail, Result};
use ferrule_core::ledger::LedgerSink;
use ferrule_gateway::transcribe::{CommandTranscriber, Ledger, OpenAiTranscriber};
use ferrule_gateway::Transcription;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const OPENAI_BASE: &str = "https://api.openai.com/v1";
const OPENAI_KEY: &str = "OPENAI_API_KEY";

/// `[transcription]` (docs/channels.md, "Voice messages").
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TranscriptionConfig {
    /// `auto` (openai when an OpenAI key is set), `openai`, `command` or
    /// `off`.
    pub backend: String,
    /// `openai`: borrow a `[providers.X]`'s `base_url` and `api_key_env`.
    pub provider: Option<String>,
    pub base_url: Option<String>,
    /// Empty: no key is sent (a local server).
    pub api_key_env: Option<String>,
    pub model: String,
    /// ISO 639-1 (`he`); unset lets the backend detect the language.
    pub language: Option<String>,
    /// `command`: the program; `{file}` is the audio file's path.
    pub command: Option<String>,
    pub timeout_secs: u64,
    /// USD per minute of audio, for the ledger.
    pub price_per_minute: f64,
}

impl Default for TranscriptionConfig {
    fn default() -> Self {
        Self {
            backend: "auto".into(),
            provider: None,
            base_url: None,
            api_key_env: None,
            model: "whisper-1".into(),
            language: None,
            command: None,
            timeout_secs: 120,
            price_per_minute: 0.006,
        }
    }
}

/// What `[transcription]` comes to, and why.
#[derive(Debug, Clone, PartialEq)]
pub enum Choice {
    Off {
        /// Why, for `ferrule doctor`.
        why: String,
        /// The only model access is a subscription plan, which has no API
        /// key for this.
        plan_only: bool,
    },
    OpenAi {
        base_url: String,
        key_env: Option<String>,
        why: String,
    },
    Command {
        template: String,
    },
}

impl TranscriptionConfig {
    /// `lookup`: an env var's value (the process's environment in use).
    pub fn choice(
        &self,
        providers: &HashMap<String, ProviderConfig>,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Choice> {
        if self.timeout_secs == 0 {
            bail!("[transcription] timeout_secs must be at least 1");
        }
        if !(self.price_per_minute >= 0.0 && self.price_per_minute.is_finite()) {
            bail!("[transcription] price_per_minute can't be negative");
        }
        if let Some(lang) = self.language.as_deref().filter(|l| !l.is_empty()) {
            if !(2..=3).contains(&lang.len()) || !lang.chars().all(|c| c.is_ascii_lowercase()) {
                bail!("[transcription] language = \"{lang}\": expected an ISO 639-1 code such as \"he\" or \"en\", or leave it unset to detect it");
            }
        }
        let borrowed = match &self.provider {
            None => None,
            Some(name) => Some(providers.get(name).ok_or_else(|| {
                anyhow!("[transcription] provider = \"{name}\" isn't in [providers]")
            })?),
        };
        let set = |var: &str| lookup(var).is_some_and(|v| !v.is_empty());
        match self.backend.as_str() {
            "off" => Ok(Choice::Off {
                why: "[transcription] backend = \"off\"".into(),
                plan_only: false,
            }),
            "command" => {
                let template = self.command.clone().filter(|c| !c.trim().is_empty()).ok_or_else(|| {
                    anyhow!("[transcription] backend = \"command\" needs `command`, e.g. \"whisper-cli -m ~/models/ggml-small.bin -nt {{file}}\"")
                })?;
                let args = ferrule_gateway::transcribe::split_command(&template)
                    .map_err(|e| anyhow!(e))?;
                if !args.iter().any(|a| a.contains("{file}")) {
                    bail!(
                        "[transcription] command needs `{{file}}` where the audio file's path goes"
                    );
                }
                Ok(Choice::Command { template })
            }
            "openai" => {
                let base_url = self
                    .base_url
                    .clone()
                    .or_else(|| borrowed.map(|p| p.endpoint()).filter(|u| !u.is_empty()))
                    .unwrap_or_else(|| OPENAI_BASE.into());
                check_url(&base_url)?;
                let key_env = match &self.api_key_env {
                    Some(k) => Some(k.clone()),
                    None => borrowed
                        .and_then(|p| p.key_var().map(str::to_string))
                        .or_else(|| Some(OPENAI_KEY.into())),
                }
                .filter(|k| !k.is_empty());
                Ok(Choice::OpenAi {
                    base_url,
                    key_env,
                    why: "[transcription] backend = \"openai\"".into(),
                })
            }
            "auto" | "" => {
                if self.command.is_some() || self.base_url.is_some() || self.provider.is_some() {
                    bail!("[transcription] sets an endpoint or a command but backend = \"auto\": say backend = \"openai\" or \"command\"");
                }
                // An OpenAI provider with its key set, else OPENAI_API_KEY.
                let mut named: Vec<(&String, &ProviderConfig)> = providers
                    .iter()
                    .filter(|(_, p)| p.base_url.contains("api.openai.com"))
                    .filter(|(_, p)| p.key_var().is_some_and(set))
                    .collect();
                named.sort_by_key(|(n, _)| n.as_str());
                if let Some((name, p)) = named.first() {
                    let var = p.key_var().unwrap_or_default().to_string();
                    return Ok(Choice::OpenAi {
                        base_url: p.base_url.trim_end_matches('/').to_string(),
                        why: format!("auto: [providers.{name}]'s key ({var}) is set"),
                        key_env: Some(var),
                    });
                }
                if set(OPENAI_KEY) {
                    return Ok(Choice::OpenAi {
                        base_url: OPENAI_BASE.into(),
                        key_env: Some(OPENAI_KEY.into()),
                        why: format!("auto: {OPENAI_KEY} is set"),
                    });
                }
                let plan_only =
                    !providers.is_empty() && providers.values().all(|p| p.plan.is_some());
                Ok(Choice::Off {
                    why: format!("auto: no OpenAI key ({OPENAI_KEY} isn't set)"),
                    plan_only,
                })
            }
            other => bail!(
                "[transcription] backend = \"{other}\": expected auto, openai, command or off"
            ),
        }
    }
}

fn check_url(url: &str) -> Result<()> {
    match url::Url::parse(url) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => Ok(()),
        _ => bail!("[transcription] base_url = \"{url}\" isn't an http(s) URL"),
    }
}

/// What someone who sent a voice message hears while transcription is off.
pub fn how_to_turn_on(plan_only: bool) -> String {
    let mut s = String::from(
        "I got your voice message and saved it, but voice transcription is off, so I can't listen to it. \
         To turn it on, the owner can set OPENAI_API_KEY (an OpenAI API key), point [transcription] at \
         another OpenAI-compatible service such as Groq, or give it a local command such as whisper.cpp \
         (docs/channels.md, \"Voice messages\").",
    );
    if plan_only {
        s.push_str(
            " A ChatGPT or Claude subscription doesn't come with an API key for this; it needs a separate key or a local command.",
        );
    }
    s
}

/// The gateway's side of `[transcription]`.
pub fn build(cfg: &Config, sink: Option<Arc<dyn LedgerSink>>) -> Result<Transcription> {
    let t = &cfg.transcription;
    let timeout = Duration::from_secs(t.timeout_secs);
    let ledger = |price: f64| Ledger {
        sink: sink.clone(),
        price_per_minute: Some(price),
    };
    Ok(match t.choice(&cfg.providers, |v| std::env::var(v).ok())? {
        Choice::Off { plan_only, .. } => Transcription::Off {
            how: how_to_turn_on(plan_only),
        },
        Choice::Command { template } => Transcription::On(Arc::new(CommandTranscriber {
            template,
            timeout,
            ledger: ledger(0.0),
        })),
        Choice::OpenAi {
            base_url, key_env, ..
        } => Transcription::On(Arc::new(OpenAiTranscriber {
            base_url,
            key_env,
            model: t.model.clone(),
            language: t.language.clone().filter(|l| !l.is_empty()),
            timeout,
            ledger: ledger(t.price_per_minute),
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()?,
        })),
    })
}

/// `ferrule doctor`'s line: what's active and why, or how to turn it on.
pub fn doctor_line(cfg: &Config) -> (bool, String) {
    match cfg
        .transcription
        .choice(&cfg.providers, |v| std::env::var(v).ok())
    {
        Err(e) => (false, format!("voice transcription: {e:#}")),
        Ok(Choice::OpenAi {
            base_url,
            key_env,
            why,
        }) => {
            let key = match &key_env {
                Some(var) if std::env::var(var).is_ok_and(|v| !v.is_empty()) => {
                    format!("key {var}")
                }
                Some(var) => {
                    return (
                        false,
                        format!("voice transcription: {base_url}, but {var} isn't set"),
                    )
                }
                None => "no key".into(),
            };
            let lang = cfg
                .transcription
                .language
                .as_deref()
                .filter(|l| !l.is_empty())
                .map_or_else(|| "language detected".into(), |l| format!("language {l}"));
            (
                true,
                format!(
                    "voice transcription: {} at {base_url} ({key}, {lang}; {why})",
                    cfg.transcription.model
                ),
            )
        }
        Ok(Choice::Command { template }) => (
            true,
            format!("voice transcription: the local command `{template}`"),
        ),
        Ok(Choice::Off { why, plan_only }) => (
            true,
            format!(
                "voice transcription: off ({why}). Set OPENAI_API_KEY, or [transcription] backend = \"openai\" with another endpoint, or \"command\" with a local whisper{}",
                if plan_only {
                    "; a subscription plan has no API key for this"
                } else {
                    ""
                }
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn providers(toml_text: &str) -> HashMap<String, ProviderConfig> {
        toml::from_str(toml_text).unwrap()
    }

    #[test]
    fn auto_is_on_with_an_openai_key_and_off_without_one() {
        let t = TranscriptionConfig::default();
        let none = HashMap::new();
        let off = t.choice(&none, |_| None).unwrap();
        assert!(
            matches!(
                off,
                Choice::Off {
                    plan_only: false,
                    ..
                }
            ),
            "{off:?}"
        );
        let on = t
            .choice(&none, |v| (v == "OPENAI_API_KEY").then(|| "sk-x".into()))
            .unwrap();
        assert_eq!(
            on,
            Choice::OpenAi {
                base_url: OPENAI_BASE.into(),
                key_env: Some("OPENAI_API_KEY".into()),
                why: "auto: OPENAI_API_KEY is set".into(),
            }
        );
        let p = providers(
            "[oa]\nbase_url = \"https://api.openai.com/v1/\"\napi_key_env = \"MY_OA\"\nmodel = \"gpt-5\"\n",
        );
        let on = t
            .choice(&p, |v| (v == "MY_OA").then(|| "sk-x".into()))
            .unwrap();
        assert!(
            matches!(&on, Choice::OpenAi { key_env: Some(k), base_url, .. } if k == "MY_OA" && base_url == "https://api.openai.com/v1"),
            "{on:?}"
        );
    }

    #[test]
    fn a_plan_only_setup_is_told_its_plan_has_no_key() {
        let p = providers("[chatgpt]\nplan = \"chatgpt\"\nmodel = \"gpt-5\"\n");
        let off = TranscriptionConfig::default().choice(&p, |_| None).unwrap();
        assert!(
            matches!(
                off,
                Choice::Off {
                    plan_only: true,
                    ..
                }
            ),
            "{off:?}"
        );
        assert!(how_to_turn_on(true).contains("subscription"));
        assert!(!how_to_turn_on(false).contains("subscription"));
    }

    #[test]
    fn bad_settings_are_refused_in_words() {
        let bad =
            |t: TranscriptionConfig| t.choice(&HashMap::new(), |_| None).unwrap_err().to_string();
        assert!(bad(TranscriptionConfig {
            backend: "whisper".into(),
            ..Default::default()
        })
        .contains("expected auto, openai, command or off"));
        assert!(bad(TranscriptionConfig {
            backend: "command".into(),
            command: Some("whisper-cli -otxt".into()),
            ..Default::default()
        })
        .contains("{file}"));
        assert!(bad(TranscriptionConfig {
            language: Some("Hebrew".into()),
            ..Default::default()
        })
        .contains("ISO 639-1"));
        assert!(bad(TranscriptionConfig {
            command: Some("x {file}".into()),
            ..Default::default()
        })
        .contains("backend = \"auto\""));
    }
}
