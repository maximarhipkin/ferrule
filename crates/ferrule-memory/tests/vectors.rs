//! M30: vectors in the memory store — hybrid ordering, time decay, never
//! mixing models, the reindex queue, forget, and the schema staying
//! readable by older builds.

use ferrule_memory::{Hybrid, MemoryStore, Merge, QueryVector, SCHEMA_VERSION};
use rusqlite::Connection;

const M: &str = "fake:test/3";

fn unit(v: [f32; 3]) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter().map(|x| x / n).collect()
}

fn embed(store: &MemoryStore, id: i64, model: &str, v: [f32; 3]) {
    let content = store.get(id).unwrap().unwrap().content;
    assert!(store.set_embedding(id, &content, model, &unit(v)).unwrap());
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn ids(hits: &[ferrule_memory::Memory]) -> Vec<i64> {
    hits.iter().map(|m| m.id).collect()
}

const WEIGHTED: Hybrid = Hybrid {
    merge: Merge::Weighted { vector_weight: 0.7 },
    min_similarity: 0.5,
};
const RRF: Hybrid = Hybrid {
    merge: Merge::Rrf { k: 60.0 },
    min_similarity: 0.5,
};

#[test]
fn a_paraphrase_is_found_by_vector_and_keywords_still_count() {
    let s = MemoryStore::in_memory().unwrap();
    let a = s
        .remember("Omer's partner is allergic to peanuts", &[])
        .unwrap();
    let b = s
        .remember("The office wifi password rotates monthly", &[])
        .unwrap();
    let c = s
        .remember("Peanuts are sold at the corner shop", &[])
        .unwrap();
    embed(&s, a, M, [1.0, 0.0, 0.0]);
    embed(&s, b, M, [0.0, 1.0, 0.0]);
    embed(&s, c, M, [0.2, 0.0, 1.0]);

    // No shared word with `a`: BM25 finds nothing.
    let q = "food intolerance of his spouse";
    assert!(s.recall(q, 5).unwrap().is_empty());
    let qv = unit([0.95, 0.05, 0.1]);
    let v = QueryVector {
        model: M,
        vector: &qv,
    };
    for h in [WEIGHTED, RRF] {
        let hits = s.recall_hybrid(q, Some(v), 5, &h).unwrap();
        // `b` and `c` are below the floor: no noise.
        assert_eq!(ids(&hits), vec![a], "{h:?}");
    }

    // "peanuts" matches `a` and `c` by keyword; the vector favours `a`.
    for h in [WEIGHTED, RRF] {
        let hits = s.recall_hybrid("peanuts", Some(v), 5, &h).unwrap();
        assert_eq!(ids(&hits), vec![a, c], "{h:?}");
        assert!(hits[0].score > hits[1].score);
    }
}

#[test]
fn without_a_vector_hybrid_is_exactly_bm25() {
    let s = MemoryStore::in_memory().unwrap();
    for t in [
        "deploy target is fly.io",
        "deploy with care on fridays",
        "staging deploy port 8080",
    ] {
        let id = s.remember(t, &[]).unwrap();
        embed(&s, id, M, [1.0, 0.0, 0.0]);
    }
    let plain = s.recall("deploy port", 10).unwrap();
    let hybrid = s.recall_hybrid("deploy port", None, 10, &WEIGHTED).unwrap();
    assert_eq!(ids(&plain), ids(&hybrid));
    let scores = |h: &[ferrule_memory::Memory]| h.iter().map(|m| m.score).collect::<Vec<_>>();
    assert_eq!(scores(&plain), scores(&hybrid));
    assert_eq!(
        s.assemble_for_goal("fix the deploy port", 2_000).unwrap(),
        s.assemble_for_goal_hybrid("fix the deploy port", None, 2_000, &RRF)
            .unwrap()
    );
}

#[test]
fn time_decay_applies_after_the_merge() {
    let s = MemoryStore::in_memory().unwrap();
    let week = 7 * 24 * 3600;
    let old = s
        .remember_at("The build cache lives on the NAS", &[], now() - 4 * week)
        .unwrap();
    let new = s
        .remember_at("The build cache lives on the SSD", &[], now())
        .unwrap();
    embed(&s, old, M, [1.0, 0.0, 0.0]);
    embed(&s, new, M, [0.8, 0.6, 0.0]);
    let qv = unit([1.0, 0.0, 0.0]);
    let v = QueryVector {
        model: M,
        vector: &qv,
    };
    for h in [WEIGHTED, RRF] {
        // `old` is the better match on both sides, but four half-lives old.
        let hits = s.recall_hybrid("build cache NAS", Some(v), 5, &h).unwrap();
        assert_eq!(ids(&hits), vec![new, old], "{h:?}");
        assert!(hits[1].score < hits[0].score / 4.0, "{h:?}");
    }
}

#[test]
fn a_hit_on_a_replaced_fact_returns_its_correction() {
    let s = MemoryStore::in_memory().unwrap();
    let a = s.remember("Staging runs on port 8443", &[]).unwrap();
    embed(&s, a, M, [1.0, 0.0, 0.0]);
    let upd = s.supersede(a, "Staging moved to port 5782", &[]).unwrap();
    // The correction has no vector yet (stale): the old row's vector still
    // leads to it.
    let qv = unit([1.0, 0.0, 0.0]);
    let v = QueryVector {
        model: M,
        vector: &qv,
    };
    let hits = s
        .recall_hybrid("where is staging", Some(v), 5, &WEIGHTED)
        .unwrap();
    assert_eq!(ids(&hits), vec![upd.id]);
}

#[test]
fn another_models_vectors_are_never_compared() {
    let s = MemoryStore::in_memory().unwrap();
    let a = s.remember("alpha fact", &[]).unwrap();
    let b = s.remember("beta fact", &[]).unwrap();
    embed(&s, a, M, [1.0, 0.0, 0.0]);
    // Same length, other model: identical direction, still not a candidate.
    embed(&s, b, "fake:other/3", [1.0, 0.0, 0.0]);
    let qv = unit([1.0, 0.0, 0.0]);
    let hits = s
        .recall_hybrid(
            "nothing shared",
            Some(QueryVector {
                model: M,
                vector: &qv,
            }),
            5,
            &RRF,
        )
        .unwrap();
    assert_eq!(ids(&hits), vec![a]);

    // Same model name at another dimension is another id; a blob of the
    // wrong length under the right id (a hand edit) is skipped, not a crash.
    let four = [0.5f32; 4];
    assert!(s.set_embedding(b, "beta fact", M, &four).unwrap());
    let hits = s
        .recall_hybrid(
            "nothing shared",
            Some(QueryVector {
                model: M,
                vector: &qv,
            }),
            5,
            &RRF,
        )
        .unwrap();
    assert_eq!(ids(&hits), vec![a]);

    let c = s.embedding_counts(M).unwrap();
    assert_eq!((c.live, c.live_embedded, c.stale), (2, 2, 0));
    let c = s.embedding_counts("fake:other/3").unwrap();
    assert_eq!((c.live, c.live_embedded, c.stale), (2, 0, 2));
}

#[test]
fn the_reindex_queue_resumes_where_it_stopped() {
    let s = MemoryStore::in_memory().unwrap();
    let all: Vec<i64> = (0..7)
        .map(|i| s.remember(&format!("fact number {i}"), &[]).unwrap())
        .collect();
    // One row already done by this model, one by another.
    embed(&s, all[1], M, [1.0, 0.0, 0.0]);
    embed(&s, all[4], "fake:old/3", [1.0, 0.0, 0.0]);

    // A first run does one batch of 3, then is "interrupted".
    let batch = s.stale_rows(M, 0, 3).unwrap();
    assert_eq!(
        batch.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![all[0], all[2], all[3]]
    );
    for (id, content) in &batch {
        assert!(s
            .set_embedding(*id, content, M, &unit([0.0, 1.0, 0.0]))
            .unwrap());
    }
    // A fresh run starts from the rows still stale; nothing is redone.
    let mut rest = Vec::new();
    let mut after = 0;
    loop {
        let batch = s.stale_rows(M, after, 2).unwrap();
        if batch.is_empty() {
            break;
        }
        for (id, content) in &batch {
            s.set_embedding(*id, content, M, &unit([0.0, 1.0, 0.0]))
                .unwrap();
            rest.push(*id);
            after = *id;
        }
    }
    assert_eq!(rest, vec![all[4], all[5], all[6]]);
    assert_eq!(s.embedding_counts(M).unwrap().stale, 0);

    // The lazy pass takes the newest live stale rows.
    let n = s.remember("fact number 7", &[]).unwrap();
    assert_eq!(
        s.stale_live(M, 32).unwrap(),
        vec![(n, "fact number 7".into())]
    );
}

#[test]
fn a_vector_never_outlives_its_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let s = MemoryStore::open(&path).unwrap();
    let a = s
        .remember("The VPN endpoint is vpn.example.org", &[])
        .unwrap();
    embed(&s, a, M, [1.0, 0.0, 0.0]);
    // A vector computed for other text is not stored.
    assert!(!s
        .set_embedding(a, "some older text", M, &unit([1.0, 0.0, 0.0]))
        .unwrap());

    // Any edit of the text (an older build, a hand edit) drops the vector.
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE memories SET content = 'The VPN endpoint is vpn2.example.org' WHERE id = ?1",
        [a],
    )
    .unwrap();
    let (blob, model): (Option<Vec<u8>>, Option<String>) = conn
        .query_row(
            "SELECT embedding, embedding_model FROM memories WHERE id = ?1",
            [a],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((blob, model), (None, None));
    // A tag edit keeps it.
    embed(&s, a, M, [1.0, 0.0, 0.0]);
    conn.execute("UPDATE memories SET tags = 'net' WHERE id = ?1", [a])
        .unwrap();
    assert_eq!(s.embedding_counts(M).unwrap().live_embedded, 1);
}

#[test]
fn forget_removes_the_vector_with_the_fact() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let s = MemoryStore::open(&path).unwrap();
    let a = s.remember("The door code is 4711", &[]).unwrap();
    embed(&s, a, M, [0.0, 0.0, 1.0]);
    let b = s.supersede(a, "The door code is 4712", &[]).unwrap().id;
    embed(&s, b, M, [0.0, 0.1, 1.0]);
    assert_eq!(s.forget(b).unwrap(), vec![a, b]);
    let qv = unit([0.0, 0.0, 1.0]);
    let hits = s
        .recall_hybrid(
            "code",
            Some(QueryVector {
                model: M,
                vector: &qv,
            }),
            5,
            &WEIGHTED,
        )
        .unwrap();
    assert!(hits.is_empty());
    let conn = Connection::open(&path).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE embedding IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn near_duplicates_by_vector_skip_replaced_and_excluded_facts() {
    let s = MemoryStore::in_memory().unwrap();
    let a = s.remember("Omer prefers tea", &[]).unwrap();
    let b = s.remember("עומר מעדיף תה", &[]).unwrap();
    let c = s.remember("Unrelated", &[]).unwrap();
    embed(&s, a, M, [1.0, 0.0, 0.0]);
    embed(&s, b, M, [0.98, 0.2, 0.0]);
    embed(&s, c, M, [0.0, 1.0, 0.0]);
    let qv = unit([1.0, 0.05, 0.0]);
    let v = QueryVector {
        model: M,
        vector: &qv,
    };
    let near = s.similar_by_vector(v, 0.9, &[], 5).unwrap();
    assert_eq!(ids(&near), vec![a, b]);
    assert_eq!(ids(&s.similar_by_vector(v, 0.9, &[a], 5).unwrap()), vec![b]);
    s.supersede(a, "Omer prefers green tea", &[]).unwrap();
    assert_eq!(ids(&s.similar_by_vector(v, 0.9, &[], 5).unwrap()), vec![b]);
}

#[test]
fn the_schema_version_stays_so_older_builds_keep_working() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/pre-m15-memory.db"
        ),
        &path,
    )
    .unwrap();
    {
        let s = MemoryStore::open(&path).unwrap();
        assert_eq!(s.embedding_counts(M).unwrap().stale, 3);
        embed(&s, 1, M, [1.0, 0.0, 0.0]);
    }
    // Opening twice is a no-op.
    drop(MemoryStore::open(&path).unwrap());
    let conn = Connection::open(&path).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(SCHEMA_VERSION, 1);
    // What an M15 build does with the file: insert and update without
    // naming the new column, and search FTS.
    conn.execute(
        "INSERT INTO memories (content, tags, created_at) VALUES ('written by an old build', '', 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE memories SET superseded_by = 4, superseded_at = 0 WHERE id = 2",
        [],
    )
    .unwrap();
    let hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH 'old'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1);
    let s = MemoryStore::open(&path).unwrap();
    assert_eq!(s.stale_live(M, 10).unwrap().len(), 2);
}
