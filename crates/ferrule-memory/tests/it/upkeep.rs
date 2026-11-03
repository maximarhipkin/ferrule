//! The learning page's memory upkeep: use counts and the pin, added to an
//! older store in place; recalls counted; pinned facts never fade and come
//! first; the stale view; the owner's edit keeps history; word-for-word
//! duplicates merge without a model.

use ferrule_memory::{MemoryStore, FAILURE_NOTE, NOTE_DAYS, OWNER};
use rusqlite::Connection;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[test]
fn a_store_from_before_the_use_columns_gets_them_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    {
        // As M15/M30 left it: no recall_count, last_recalled_at, pinned, source.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE memories (id INTEGER PRIMARY KEY AUTOINCREMENT, content TEXT NOT NULL,
               tags TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL, embedding BLOB,
               superseded_by INTEGER, superseded_at INTEGER, embedding_model TEXT);
             INSERT INTO memories (content, tags, created_at) VALUES ('the deploy target is fly.io', '', 1);
             PRAGMA user_version = 1;",
        )
        .unwrap();
    }
    let m = MemoryStore::open(&path).unwrap();
    let old = m.get(1).unwrap().unwrap();
    assert_eq!(
        (old.recalls, old.pinned, old.source.clone()),
        (0, false, None)
    );
    assert_eq!(m.recent(5).unwrap().len(), 1);
    drop(m);
    // Reopening adds nothing twice, and the version an older build reads
    // stays.
    let m = MemoryStore::open(&path).unwrap();
    m.mark_recalled(&[1]).unwrap();
    let got = m.get(1).unwrap().unwrap();
    assert_eq!(got.recalls, 1);
    assert!(got.last_recalled.is_some());
    let version: i64 = Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 1);
}

#[test]
fn pins_stale_edits_and_sources() {
    let m = MemoryStore::in_memory().unwrap();
    let month = 31 * 86_400;
    let old = m
        .remember_at(
            "the backup bucket is s3://alpha-backups",
            &[],
            now() - 10 * month,
        )
        .unwrap();
    let pinned = m
        .remember_at(
            "the backup schedule is nightly at 02:00",
            &[],
            now() - 10 * month,
        )
        .unwrap();
    let fresh = m
        .insert_from("the backup tool is restic", &[], &[], Some(OWNER))
        .unwrap()
        .id;
    assert_eq!(
        m.get(fresh).unwrap().unwrap().source.as_deref(),
        Some(OWNER)
    );
    // A pin: it outranks the fresh fact, though ten months old.
    assert!(m.set_pinned(pinned, true).unwrap());
    let hits = m.recall("backup", 3).unwrap();
    assert_eq!(hits[0].id, pinned, "{hits:?}");
    // Stale: old, never recalled, not pinned.
    let stale: Vec<i64> = m.stale(30, 10).unwrap().iter().map(|x| x.id).collect();
    assert_eq!(stale, vec![old]);
    m.mark_recalled(&[old]).unwrap();
    assert!(m.stale(30, 10).unwrap().is_empty());
    // An edit replaces it, history kept, pin and counts carried.
    let e = m
        .edit(pinned, "the backup schedule is nightly at 03:00", OWNER)
        .unwrap();
    assert_ne!(e.id, pinned);
    let now_fact = m.get(e.id).unwrap().unwrap();
    assert!(now_fact.pinned);
    assert_eq!(m.get(pinned).unwrap().unwrap().superseded_by, Some(e.id));
    assert!(m.edit(pinned, "again", OWNER).is_err(), "a replaced fact");
    // A pinned failure note doesn't fade.
    let note = m
        .remember_at(
            "Unfinished run: the backup restore stalled",
            &[FAILURE_NOTE],
            now() - (NOTE_DAYS + 2) * 86_400,
        )
        .unwrap();
    assert!(m
        .recall("backup restore stalled", 5)
        .unwrap()
        .iter()
        .all(|x| x.id != note));
    m.set_pinned(note, true).unwrap();
    assert!(m
        .recall("backup restore stalled", 5)
        .unwrap()
        .iter()
        .any(|x| x.id == note));
}

#[test]
fn word_for_word_duplicates_merge_into_the_newest_without_a_model() {
    let m = MemoryStore::in_memory().unwrap();
    let a = m.remember("Port 5781 is staging", &[]).unwrap();
    let b = m.remember("port 5781  is staging.", &[]).unwrap();
    let c = m.remember("Port 5781 is staging", &["profile"]).unwrap();
    m.mark_recalled(&[a, a]).unwrap();
    assert_eq!(m.merge_duplicates().unwrap(), vec![(b, vec![a])]);
    assert_eq!(m.get(b).unwrap().unwrap().recalls, 2, "counts go along");
    assert!(
        m.get(c).unwrap().unwrap().superseded_by.is_none(),
        "other tags: kept apart"
    );
    // Reversible, as any M15 update.
    m.undo_update(b, &[a], false).unwrap();
    assert!(m.get(a).unwrap().unwrap().superseded_by.is_none());
    assert!(m.merge_duplicates().unwrap().len() == 1);
}
