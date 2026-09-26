//! Text embedders for memory recall (M30).
//!
//! One trait, [`Embedder`], and three implementations: [`FakeEmbedder`]
//! (deterministic, for tests), [`OpenAiEmbedder`] (any OpenAI-compatible
//! `/v1/embeddings` endpoint, reached through ferrule's credential proxy)
//! and, with the `local` feature, [`local::StaticEmbedder`] (a model2vec
//! static model read from disk, no key and no network).
//!
//! Every vector is L2-normalised, so cosine similarity is a dot product.
//! Every vector is tagged with its [`ModelId`], which includes the
//! dimension: vectors of different models are never compared.

pub mod download;
mod fake;
#[cfg(feature = "local")]
pub mod local;
mod openai;

pub use fake::FakeEmbedder;
pub use openai::{OpenAiEmbedder, OpenAiSettings};

use std::fmt;

/// A model's identity for storage: backend, model, revision where there
/// is one, and dimension (`local:potion-multilingual-128M@73908c3/256`).
/// Two ids that differ in anything are different models.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelId(String);

impl ModelId {
    pub fn new(backend: &str, model: &str, dim: usize) -> Self {
        Self(format!("{backend}:{model}/{dim}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The dimension, the part after the last `/`.
    pub fn dim(&self) -> usize {
        self.0
            .rsplit_once('/')
            .and_then(|(_, d)| d.parse().ok())
            .unwrap_or(0)
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a text is embedded for. Some models want a different prefix for a
/// query than for a stored document; the current backends ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Query,
    Document,
}

/// One call's vectors (one per input, in order) and the tokens the backend
/// billed for it (0 for a local model).
#[derive(Debug, Clone, Default)]
pub struct Embedded {
    pub vectors: Vec<Vec<f32>>,
    pub tokens: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    /// The endpoint refused the key (401/403).
    #[error("the embeddings endpoint at {host} refused the key (HTTP {status}); check {key_env}")]
    Unauthorized {
        host: String,
        status: u16,
        key_env: String,
    },
    /// 429: too many requests, or out of quota.
    #[error("the embeddings endpoint at {host} is rate limiting (HTTP 429){}", retry_note(.retry_after))]
    RateLimited {
        host: String,
        retry_after: Option<u64>,
    },
    #[error("the embeddings endpoint at {host} answered HTTP {status}: {body}")]
    Http {
        host: String,
        status: u16,
        body: String,
    },
    #[error("the embeddings endpoint at {host} can't be reached: {message}")]
    Transport { host: String, message: String },
    /// A well-formed answer that isn't what was asked for: the wrong
    /// number of vectors, or the wrong dimension.
    #[error("bad embeddings answer: {0}")]
    BadAnswer(String),
    /// The local model isn't downloaded, or doesn't verify.
    #[error("{0}")]
    Model(String),
}

fn retry_note(after: &Option<u64>) -> String {
    after
        .map(|s| format!("; retry after {s}s"))
        .unwrap_or_default()
}

impl EmbedError {
    /// A short machine tag, for the ledger's `error_kind`.
    pub fn kind(&self) -> &'static str {
        match self {
            EmbedError::Unauthorized { .. } => "unauthorized",
            EmbedError::RateLimited { .. } => "rate_limited",
            EmbedError::Http { .. } => "http",
            EmbedError::Transport { .. } => "transport",
            EmbedError::BadAnswer(_) => "bad_answer",
            EmbedError::Model(_) => "model",
        }
    }
}

#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    /// The id stored with every vector this embedder makes.
    fn model(&self) -> &ModelId;

    /// One vector per text, L2-normalised, of `model().dim()` floats.
    async fn embed(&self, texts: &[String], purpose: Purpose) -> Result<Embedded, EmbedError>;
}

/// Scales `v` to unit length in place (a zero vector stays zero).
pub fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Cosine similarity of two unit vectors: their dot product. Vectors of
/// different lengths score 0 (they can't be the same model).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// A vector as stored: little-endian f32s.
pub fn to_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// The reverse of [`to_bytes`]; `None` for a blob that isn't whole f32s.
pub fn from_bytes(b: &[u8]) -> Option<Vec<f32>> {
    let (words, rest) = b.as_chunks::<4>();
    rest.is_empty()
        .then(|| words.iter().map(|w| f32::from_le_bytes(*w)).collect())
}

/// Checks an answer has one vector of `dim` per input, and normalises it.
pub(crate) fn checked(
    mut vectors: Vec<Vec<f32>>,
    inputs: usize,
    dim: usize,
) -> Result<Vec<Vec<f32>>, EmbedError> {
    if vectors.len() != inputs {
        return Err(EmbedError::BadAnswer(format!(
            "{} vectors for {inputs} inputs",
            vectors.len()
        )));
    }
    for v in &mut vectors {
        if dim != 0 && v.len() != dim {
            return Err(EmbedError::BadAnswer(format!(
                "a {}-dimension vector from a {dim}-dimension model",
                v.len()
            )));
        }
        if v.iter().any(|x| !x.is_finite()) {
            return Err(EmbedError::BadAnswer("a vector with NaN or inf".into()));
        }
        normalize(v);
    }
    Ok(vectors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_id_carries_its_dimension() {
        let id = ModelId::new("openai", "text-embedding-3-small", 512);
        assert_eq!(id.as_str(), "openai:text-embedding-3-small/512");
        assert_eq!(id.dim(), 512);
        assert_ne!(id, ModelId::new("openai", "text-embedding-3-small", 1536));
    }

    #[test]
    fn bytes_round_trip_and_reject_ragged_blobs() {
        let v = vec![0.25f32, -1.5, 3.0];
        assert_eq!(from_bytes(&to_bytes(&v)).unwrap(), v);
        assert!(from_bytes(&[0, 1, 2]).is_none());
    }

    #[test]
    fn checked_refuses_the_wrong_count_dimension_or_nan() {
        assert!(checked(vec![vec![1.0, 0.0]], 2, 2).is_err());
        assert!(checked(vec![vec![1.0, 0.0, 0.0]], 1, 2).is_err());
        assert!(checked(vec![vec![f32::NAN, 0.0]], 1, 2).is_err());
        let v = checked(vec![vec![3.0, 4.0]], 1, 2).unwrap();
        assert!((v[0][0] - 0.6).abs() < 1e-6 && (v[0][1] - 0.8).abs() < 1e-6);
    }
}
