//! The learning loop's failure notes: recalled when a message matches
//! them, never as one of the newest facts, and only for a week.

use ferrule_memory::{MemoryStore, FAILURE_NOTE, NOTE_DAYS};

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[test]
fn a_failure_note_is_recalled_by_a_match_only_and_fades_after_a_week() {
    let m = MemoryStore::in_memory().unwrap();
    m.remember("the staging deploy script is deploy.sh", &[])
        .unwrap();
    let fresh = m
        .remember_at(
            "Unfinished run: asked to migrate the invoices table; `pytest` still failed",
            &[FAILURE_NOTE, "scope:chat:telegram__1"],
            now() - 3600,
        )
        .unwrap();
    m.remember_at(
        "Unfinished run: asked to migrate the invoices table last month; it timed out",
        &[FAILURE_NOTE],
        now() - (NOTE_DAYS + 1) * 86_400,
    )
    .unwrap();

    let hits = m.recall("migrate invoices", 10).unwrap();
    assert_eq!(
        hits.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![fresh],
        "{hits:?}"
    );
    let block = m
        .assemble_for_goal("migrate the invoices table", 2000)
        .unwrap();
    assert!(block.contains(&format!("#{fresh} ")), "{block}");
    // An unrelated goal: the newest facts fill in, the note doesn't.
    let other = m.assemble_for_goal("what's the weather", 2000).unwrap();
    assert!(other.contains("deploy.sh"), "{other}");
    assert!(!other.contains("Unfinished run"), "{other}");
}
