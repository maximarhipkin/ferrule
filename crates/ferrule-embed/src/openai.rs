//! Any OpenAI-compatible `POST {base_url}/embeddings`: OpenAI itself,
//! Ollama, llama.cpp's server, vLLM, LM Studio, most gateways.
//!
//! The embedder never holds a real key. The caller hands it the credential
//! proxy's placeholder and a client whose traffic goes through the proxy,
//! which swaps the real key in for the endpoint's host only.

use crate::{checked, EmbedError, Embedded, Embedder, ModelId, Purpose};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct OpenAiSettings {
    /// Up to and including `/v1`: the request goes to `{base_url}/embeddings`.
    pub base_url: String,
    pub model: String,
    /// The length of the vectors the model returns. Part of the model id,
    /// and an answer of any other length is refused.
    pub dim: usize,
    /// Send `dimensions` in the request (OpenAI's text-embedding-3 models
    /// can shorten their vectors; most other servers reject the field).
    pub send_dimensions: bool,
    /// The proxy's placeholder for the key, sent as `Authorization: Bearer`.
    /// `None` for a server that wants no key (a local Ollama).
    pub key: Option<String>,
    /// The key's env var, named in a 401's message.
    pub key_env: Option<String>,
    pub timeout: Duration,
}

pub struct OpenAiEmbedder {
    settings: OpenAiSettings,
    client: reqwest::Client,
    id: ModelId,
    host: String,
}

impl OpenAiEmbedder {
    /// `client` should come from `ferrule_tools::egress::client_builder`,
    /// so it goes through the credential proxy.
    pub fn new(settings: OpenAiSettings, client: reqwest::Client) -> Self {
        let id = ModelId::new("openai", &settings.model, settings.dim);
        let host = reqwest::Url::parse(&settings.base_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| settings.base_url.clone());
        Self {
            settings,
            client,
            id,
            host,
        }
    }

    pub fn host(&self) -> &str {
        &self.host
    }
}

#[derive(Deserialize)]
struct Answer {
    data: Vec<Item>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Item {
    embedding: Vec<f32>,
    #[serde(default)]
    index: usize,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

#[async_trait::async_trait]
impl Embedder for OpenAiEmbedder {
    fn model(&self) -> &ModelId {
        &self.id
    }

    async fn embed(&self, texts: &[String], _purpose: Purpose) -> Result<Embedded, EmbedError> {
        if texts.is_empty() {
            return Ok(Embedded::default());
        }
        let s = &self.settings;
        let mut body = json!({"model": s.model, "input": texts, "encoding_format": "float"});
        if s.send_dimensions {
            body["dimensions"] = json!(s.dim);
        }
        let url = format!("{}/embeddings", s.base_url.trim_end_matches('/'));
        let mut req = self.client.post(url).timeout(s.timeout).json(&body);
        if let Some(key) = &s.key {
            req = req.bearer_auth(key);
        }
        let transport = |e: reqwest::Error| EmbedError::Transport {
            host: self.host.clone(),
            message: if e.is_timeout() {
                format!("no answer within {}s", s.timeout.as_secs())
            } else {
                e.without_url().to_string()
            },
        };
        let resp = req.send().await.map_err(transport)?;
        let status = resp.status();
        if status == 401 || status == 403 {
            return Err(EmbedError::Unauthorized {
                host: self.host.clone(),
                status: status.as_u16(),
                key_env: s.key_env.clone().unwrap_or_else(|| "the key".into()),
            });
        }
        if status == 429 {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<f64>().ok())
                .map(|s| s.ceil().max(0.0) as u64);
            return Err(EmbedError::RateLimited {
                host: self.host.clone(),
                retry_after,
            });
        }
        let bytes = resp.bytes().await.map_err(transport)?;
        if !status.is_success() {
            let body: String = String::from_utf8_lossy(&bytes).chars().take(300).collect();
            return Err(EmbedError::Http {
                host: self.host.clone(),
                status: status.as_u16(),
                body,
            });
        }
        let mut answer: Answer = serde_json::from_slice(&bytes)
            .map_err(|e| EmbedError::BadAnswer(format!("not an embeddings answer: {e}")))?;
        answer.data.sort_by_key(|i| i.index);
        let vectors = checked(
            answer.data.into_iter().map(|i| i.embedding).collect(),
            texts.len(),
            s.dim,
        )?;
        let tokens = answer
            .usage
            .map(|u| u.prompt_tokens.max(u.total_tokens))
            .unwrap_or(0);
        Ok(Embedded { vectors, tokens })
    }
}
