//! A deterministic embedder for tests: each text is a bag of hashed
//! character trigrams, so texts that share spelling are close (a typo
//! stays near its word) and nothing else is. It knows no synonyms and no
//! languages, which keeps tests about the plumbing, not about a model.

use crate::{EmbedError, Embedded, Embedder, ModelId, Purpose};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Clone)]
pub struct FakeEmbedder {
    id: ModelId,
    dim: usize,
    fail: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
}

impl FakeEmbedder {
    /// `name` goes into the model id, so two fakes with different names
    /// are different models.
    pub fn new(name: &str, dim: usize) -> Self {
        Self {
            id: ModelId::new("fake", name, dim),
            dim,
            fail: Arc::default(),
            calls: Arc::default(),
        }
    }

    /// Makes every later call fail (or succeed again), like an endpoint
    /// that went down. Clones share the switch.
    pub fn set_failing(&self, fail: bool) {
        self.fail.store(fail, Ordering::SeqCst);
    }

    /// How many `embed` calls were made, failed ones included.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// The vector for one text, without the async wrapper.
    pub fn vector(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        for word in text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
        {
            let padded: Vec<char> = format!(" {} ", word.to_lowercase()).chars().collect();
            for gram in padded.windows(3) {
                let h = fnv(gram);
                let slot = (h % self.dim as u64) as usize;
                v[slot] += if h >> 63 == 0 { 1.0 } else { -1.0 };
            }
        }
        crate::normalize(&mut v);
        v
    }
}

fn fnv(chars: &[char]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for c in chars {
        for b in (*c as u32).to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    h
}

#[async_trait::async_trait]
impl Embedder for FakeEmbedder {
    fn model(&self) -> &ModelId {
        &self.id
    }

    async fn embed(&self, texts: &[String], _purpose: Purpose) -> Result<Embedded, EmbedError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(EmbedError::Transport {
                host: "fake".into(),
                message: "the fake embedder is set to fail".into(),
            });
        }
        Ok(Embedded {
            vectors: texts.iter().map(|t| self.vector(t)).collect(),
            tokens: texts
                .iter()
                .map(|t| t.split_whitespace().count() as u64)
                .sum(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cosine;

    #[test]
    fn deterministic_normalised_and_typo_tolerant() {
        let e = FakeEmbedder::new("t", 128);
        let a = e.vector("postgres port 5781");
        assert_eq!(a, e.vector("postgres port 5781"));
        let norm: f32 = a.iter().map(|x| x * x).sum();
        assert!((norm - 1.0).abs() < 1e-5);
        let typo = cosine(&a, &e.vector("postgress prot 5781"));
        let other = cosine(&a, &e.vector("the cat sleeps on the sofa"));
        assert!(typo > 0.4, "{typo}");
        assert!(typo > other + 0.3, "{typo} vs {other}");
    }

    #[tokio::test]
    async fn a_failing_fake_fails_and_counts() {
        let e = FakeEmbedder::new("t", 16);
        let texts = vec!["a b".to_string()];
        assert_eq!(
            e.embed(&texts, Purpose::Query).await.unwrap().vectors.len(),
            1
        );
        e.clone().set_failing(true);
        assert!(e.embed(&texts, Purpose::Query).await.is_err());
        assert_eq!(e.calls(), 2);
        assert_eq!(e.model().as_str(), "fake:t/16");
    }
}
