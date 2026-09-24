//! A memory store written by ferrule before M15 (main at 2f1045f, three
//! `ferrule memory add` calls) is migrated in place and still recalls.

use ferrule_memory::{Decision, MemoryStore, SCHEMA_VERSION};
use rusqlite::Connection;

fn fixture_copy() -> (tempfile::TempDir, std::path::PathBuf) {
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
    (dir, path)
}

#[test]
fn the_fixture_really_is_pre_m15() {
    let (_dir, path) = fixture_copy();
    let conn = Connection::open(&path).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 0);
    let cols: Vec<String> = conn
        .prepare("PRAGMA table_info(memories)")
        .unwrap()
        .query_map([], |r| r.get(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!cols.iter().any(|c| c == "superseded_by"));
}

#[test]
fn a_pre_m15_store_is_migrated_and_still_recalls() {
    let (_dir, path) = fixture_copy();
    let store = MemoryStore::open(&path).unwrap();

    // Old rows are live and recall exactly as before.
    let hits = store.recall("deploy", 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, 1);
    assert_eq!(hits[0].content, "The deploy target is fly.io, region ams");
    assert_eq!(hits[0].tags, vec!["infra".to_string()]);
    assert_eq!(hits[0].superseded_by, None);
    assert_eq!(store.recent(10).unwrap().len(), 3);
    let hits = store.recall("staging port", 5).unwrap();
    assert_eq!(hits[0].id, 3);

    // The new pipeline works on the migrated rows.
    let upd = store
        .supersede(1, "The deploy target is render.com, region frankfurt", &[])
        .unwrap();
    assert_eq!(upd.decision, Decision::Updated);
    let hits = store.recall("fly.io", 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, upd.id);
    assert_eq!(hits[0].tags, vec!["infra".to_string()]);
    drop(store);

    // Schema is at the current version, and reopening is a no-op.
    let conn = Connection::open(&path).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let cols: Vec<String> = conn
        .prepare("PRAGMA table_info(memories)")
        .unwrap()
        .query_map([], |r| r.get(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(cols.iter().any(|c| c == "superseded_by"));
    assert!(cols.iter().any(|c| c == "superseded_at"));
    drop(conn);
    let store = MemoryStore::open(&path).unwrap();
    assert_eq!(store.recent(10).unwrap().len(), 3);
}

#[test]
fn a_half_migrated_store_finishes_migrating() {
    // A column already present (an interrupted or hand-made migration) must
    // not make the ALTER fail.
    let (_dir, path) = fixture_copy();
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("ALTER TABLE memories ADD COLUMN superseded_by INTEGER;")
        .unwrap();
    drop(conn);
    let store = MemoryStore::open(&path).unwrap();
    assert_eq!(store.recall("Hebrew", 5).unwrap()[0].id, 2);
}
