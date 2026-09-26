//! The M30 recall benchmark: a fixture of facts and queries, each query
//! expecting one fact, scored as recall@1, recall@5 and MRR (over the top
//! 10) per query category, for plain BM25 and for each hybrid merge.
//!
//! The harness doesn't embed anything itself: the caller passes a function
//! from texts to vectors (a fake embedder in the hermetic test, the real
//! model in the ignored one).

use crate::{Hybrid, MemoryError, MemoryStore, QueryVector};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize)]
pub struct Fixture {
    pub memories: Vec<FixtureMemory>,
    pub queries: Vec<FixtureQuery>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FixtureMemory {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FixtureQuery {
    pub category: String,
    pub query: String,
    /// The `id` of the fact that answers it.
    pub expect: String,
}

impl Fixture {
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

/// One method's numbers over a set of queries.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Scores {
    pub queries: usize,
    pub recall_at_1: f64,
    pub recall_at_5: f64,
    pub mrr: f64,
    /// Facts returned per query (at most 10): what a floor keeps out.
    pub hits: f64,
}

/// Scores by method, then by category (`"all"` for the whole set).
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub methods: Vec<String>,
    pub categories: Vec<String>,
    pub scores: BTreeMap<(String, String), Scores>,
    /// The queries a method missed in its top 5: `(method, query)`.
    pub misses: Vec<(String, String)>,
}

impl Report {
    pub fn get(&self, method: &str, category: &str) -> Scores {
        self.scores
            .get(&(method.to_string(), category.to_string()))
            .copied()
            .unwrap_or_default()
    }

    /// A plain-text table: one row per category, `r@1 / r@5 / MRR` per
    /// method.
    pub fn table(&self) -> String {
        let mut out = format!("{:<18}", "category");
        for m in &self.methods {
            out.push_str(&format!(" | {m:^22}"));
        }
        out.push('\n');
        out.push_str(&format!("{:<18}", ""));
        for _ in &self.methods {
            out.push_str(&format!(" | {:>6} {:>6} {:>7}", "r@1", "r@5", "MRR"));
        }
        out.push('\n');
        for c in &self.categories {
            let n = self.get(&self.methods[0], c).queries;
            out.push_str(&format!("{:<18}", format!("{c} ({n})")));
            for m in &self.methods {
                let s = self.get(m, c);
                out.push_str(&format!(
                    " | {:>6.3} {:>6.3} {:>7.3}",
                    s.recall_at_1, s.recall_at_5, s.mrr
                ));
            }
            out.push('\n');
        }
        out
    }
}

/// Loads the fixture into a fresh in-memory store, embeds every fact and
/// query with `embed` (tagged `model`), and scores each method: `None` is
/// plain BM25 ([`MemoryStore::recall`]), `Some` a hybrid merge.
pub fn run(
    fixture: &Fixture,
    model: &str,
    embed: &mut dyn FnMut(&[String]) -> Vec<Vec<f32>>,
    methods: &[(&str, Option<Hybrid>)],
) -> Result<Report, MemoryError> {
    let store = MemoryStore::in_memory()?;
    let texts: Vec<String> = fixture.memories.iter().map(|m| m.text.clone()).collect();
    let vectors = embed(&texts);
    let mut by_row = BTreeMap::new();
    for (m, v) in fixture.memories.iter().zip(&vectors) {
        let row = store.remember(&m.text, &[])?;
        store.set_embedding(row, &m.text, model, v)?;
        by_row.insert(row, m.id.clone());
    }
    let queries: Vec<String> = fixture.queries.iter().map(|q| q.query.clone()).collect();
    let qvecs = embed(&queries);

    let mut categories: Vec<String> = Vec::new();
    for q in &fixture.queries {
        if !categories.contains(&q.category) {
            categories.push(q.category.clone());
        }
    }
    categories.push("all".into());

    let mut report = Report {
        methods: methods.iter().map(|m| m.0.to_string()).collect(),
        categories,
        ..Report::default()
    };
    for (name, hybrid) in methods {
        let mut sums: BTreeMap<String, (usize, f64, f64, f64, f64)> = BTreeMap::new();
        for (q, qv) in fixture.queries.iter().zip(&qvecs) {
            let hits = match hybrid {
                None => store.recall(&q.query, 10)?,
                Some(h) => {
                    store.recall_hybrid(&q.query, Some(QueryVector { model, vector: qv }), 10, h)?
                }
            };
            let rank = hits
                .iter()
                .position(|m| by_row.get(&m.id) == Some(&q.expect))
                .map(|p| p + 1);
            if !matches!(rank, Some(r) if r <= 5) {
                report.misses.push((name.to_string(), q.query.clone()));
            }
            for key in [q.category.as_str(), "all"] {
                let e = sums.entry(key.to_string()).or_default();
                e.0 += 1;
                e.1 += f64::from(u8::from(rank == Some(1)));
                e.2 += f64::from(u8::from(matches!(rank, Some(r) if r <= 5)));
                e.3 += rank.map(|r| 1.0 / r as f64).unwrap_or(0.0);
                e.4 += hits.len() as f64;
            }
        }
        for (cat, (n, r1, r5, rr, hits)) in sums {
            let n_f = n as f64;
            report.scores.insert(
                (name.to_string(), cat),
                Scores {
                    queries: n,
                    recall_at_1: r1 / n_f,
                    recall_at_5: r5 / n_f,
                    mrr: rr / n_f,
                    hits: hits / n_f,
                },
            );
        }
    }
    Ok(report)
}
