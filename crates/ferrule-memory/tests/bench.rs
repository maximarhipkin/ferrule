//! The M30 recall benchmark (`tests/fixtures/recall_bench.json`): BM25
//! against the hybrid merges.
//!
//! The hermetic test runs the harness with the fake embedder (character
//! trigrams: it knows spelling, not meaning). The real-model run is
//! ignored; it needs the local model on disk:
//!
//! ```text
//! FERRULE_EMBED_MODEL_DIR=/path/to/models/potion-multilingual-128M@73908c3 \
//!   cargo test -p ferrule-memory --test bench -- --ignored --nocapture
//! ```

use ferrule_embed::{Embedder, FakeEmbedder};
use ferrule_memory::bench::{self, Fixture};
use ferrule_memory::{Hybrid, Merge};

fn fixture() -> Fixture {
    Fixture::from_json(include_str!("fixtures/recall_bench.json")).unwrap()
}

fn methods(floor: f32) -> Vec<(&'static str, Option<Hybrid>)> {
    vec![
        ("bm25", None),
        (
            "weighted 0.7",
            Some(Hybrid {
                merge: Merge::Weighted { vector_weight: 0.7 },
                min_similarity: floor,
            }),
        ),
        (
            "rrf k=60",
            Some(Hybrid {
                merge: Merge::Rrf { k: 60.0 },
                min_similarity: floor,
            }),
        ),
    ]
}

#[test]
fn the_fixture_is_the_size_the_design_says() {
    let f = fixture();
    assert_eq!(f.memories.len(), 80);
    assert_eq!(f.queries.len(), 48);
    let ids: std::collections::HashSet<_> = f.memories.iter().map(|m| &m.id).collect();
    assert_eq!(ids.len(), 80);
    for q in &f.queries {
        assert!(ids.contains(&q.expect), "{} expects {}", q.query, q.expect);
    }
    let hebrew = f
        .memories
        .iter()
        .filter(|m| m.text.chars().any(|c| ('\u{5d0}'..='\u{5ea}').contains(&c)))
        .count();
    assert_eq!(hebrew, 20);
}

#[test]
fn the_harness_scores_bm25_and_both_merges_with_the_fake_embedder() {
    let f = fixture();
    let fake = FakeEmbedder::new("trigram", 256);
    let model = fake.model().to_string();
    let mut embed = |texts: &[String]| texts.iter().map(|t| fake.vector(t)).collect();
    let report = bench::run(&f, &model, &mut embed, &methods(0.3)).unwrap();
    println!("{}", report.table());

    let bm25 = report.get("bm25", "all");
    assert_eq!(bm25.queries, 48);
    // BM25 gets every keyword query, no cross-lingual one, and a misspelt
    // query only through a word that is spelt right.
    assert_eq!(report.get("bm25", "keyword").recall_at_5, 1.0);
    assert_eq!(report.get("bm25", "crosslingual").recall_at_5, 0.0);
    let bm25_typo = report.get("bm25", "typo").recall_at_5;
    assert!(bm25_typo < 0.5);
    for m in ["weighted 0.7", "rrf k=60"] {
        let s = report.get(m, "all");
        assert_eq!(s.queries, 48);
        assert!((0.0..=1.0).contains(&s.mrr));
        assert!(s.recall_at_1 <= s.recall_at_5);
        // Trigrams know spelling: the hybrid finds misspelt words BM25 can't.
        assert!(
            report.get(m, "typo").recall_at_5 > bm25_typo,
            "{m}\n{}",
            report.table()
        );
        // And keyword queries don't get worse.
        assert_eq!(report.get(m, "keyword").recall_at_5, 1.0, "{m}");
    }
}

#[test]
fn a_floor_of_one_leaves_only_keyword_matches() {
    let f = fixture();
    let fake = FakeEmbedder::new("trigram", 256);
    let model = fake.model().to_string();
    let mut embed = |texts: &[String]| texts.iter().map(|t| fake.vector(t)).collect();
    let report = bench::run(&f, &model, &mut embed, &methods(1.01)).unwrap();
    for m in ["weighted 0.7", "rrf k=60"] {
        // No vector candidate passes: the same facts as BM25, in BM25 order.
        assert_eq!(report.get(m, "all"), report.get("bm25", "all"), "{m}");
    }
}

/// The benchmark with the real local model. Prints the table (and a sweep
/// of floors and weights) for the design doc.
#[test]
#[ignore = "needs the local model: set FERRULE_EMBED_MODEL_DIR"]
fn real_model() {
    let dir = std::env::var("FERRULE_EMBED_MODEL_DIR")
        .expect("set FERRULE_EMBED_MODEL_DIR to the downloaded model's directory");
    let spec = ferrule_embed::download::POTION_MULTILINGUAL;
    let local = ferrule_embed::local::StaticEmbedder::new(spec, std::path::PathBuf::from(dir));
    let model = local.model().to_string();
    let f = fixture();
    let started = std::time::Instant::now();
    let mut embed = |texts: &[String]| local.embed_sync(texts).expect("embed");
    let report = bench::run(&f, &model, &mut embed, &methods(0.3)).unwrap();
    println!("model {model}, {:.1?}", started.elapsed());
    println!("{}", report.table());
    for m in &report.methods {
        println!("{m}: {:.2} facts per query", report.get(m, "all").hits);
    }
    for (m, q) in &report.misses {
        println!("miss@5  {m:<13} {q}");
    }

    println!("\nsweep (all 48 queries: r@1 r@5 MRR hits/query; crosslingual+paraphrase+synonym r@5; keyword r@1)");
    for floor in [0.2f32, 0.25, 0.3, 0.35, 0.4, 0.5] {
        for (name, merge) in [
            ("weighted 0.5", Merge::Weighted { vector_weight: 0.5 }),
            ("weighted 0.7", Merge::Weighted { vector_weight: 0.7 }),
            ("weighted 0.9", Merge::Weighted { vector_weight: 0.9 }),
            ("rrf k=60", Merge::Rrf { k: 60.0 }),
        ] {
            let h = Hybrid {
                merge,
                min_similarity: floor,
            };
            let r = bench::run(&f, &model, &mut embed, &[(name, Some(h))]).unwrap();
            let all = r.get(name, "all");
            let sem: f64 = ["crosslingual", "paraphrase", "synonym"]
                .iter()
                .map(|c| r.get(name, c).recall_at_5 * r.get(name, c).queries as f64)
                .sum::<f64>()
                / 26.0;
            println!(
                "floor {floor:.2} {name:<13} {:.3} {:.3} {:.3} {:>5.2}   {sem:.3}   {:.3}",
                all.recall_at_1,
                all.recall_at_5,
                all.mrr,
                all.hits,
                r.get(name, "keyword").recall_at_1
            );
        }
    }
}
