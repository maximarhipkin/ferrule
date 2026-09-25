use super::*;
use crate::gate::GateVerdict;
use ferrule_core::{CompletionResponse, CoreError, LedgerRecord};
use std::collections::VecDeque;
use std::sync::Mutex;

/// Answers from a queue; `None` is a provider error. 100 tokens a call.
struct Queue(Mutex<VecDeque<Option<String>>>);

#[async_trait::async_trait]
impl Provider for Queue {
    fn name(&self) -> &str {
        "queue"
    }
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let next = self.0.lock().unwrap().pop_front().flatten();
        match next {
            Some(text) => Ok(CompletionResponse {
                message: Message::assistant(Some(text), vec![], None),
                usage: Usage {
                    input_tokens: 90,
                    output_tokens: 10,
                    cached_input_tokens: 0,
                    cache_write_input_tokens: 0,
                },
            }),
            None => Err(CoreError::Provider("down".into())),
        }
    }
}

/// A gate with a fixed verdict that remembers the playbook it was shown.
struct FakeGate {
    passes: bool,
    seen: Mutex<Vec<Option<String>>>,
}

#[async_trait::async_trait]
impl Gate for FakeGate {
    fn check(&self) -> Option<String> {
        Some("true".into())
    }
    async fn run(&self, r: GateRun<'_>) -> GateVerdict {
        self.seen
            .lock()
            .unwrap()
            .push(r.playbook_block.map(str::to_string));
        GateVerdict {
            passed: self.passes,
            reason: if self.passes {
                "passed twice".into()
            } else {
                "`true` fails with the lesson: exit 1".into()
            },
        }
    }
}

struct Rows(Mutex<Vec<LedgerRecord>>);
impl LedgerSink for Rows {
    fn record(&self, r: LedgerRecord) {
        self.0.lock().unwrap().push(r);
    }
}

fn ep(n: i64) -> Episode {
    Episode {
        key: format!("task:t{n}:r{n}"),
        label: format!("t{n}"),
        goal: format!("goal {n}"),
        outcome: "failed".into(),
        detail: "exit 1".into(),
        fixed: true,
        transcript: None,
        at: n * 100,
    }
}

fn opts() -> Options {
    Options {
        caps: Caps {
            usd_per_pass: 1.0,
            usd_per_day: 2.0,
            tokens_per_pass: 100_000,
            tokens_per_day: 1_000_000,
        },
        max_episodes: 5,
        max_bullets: 40,
        max_clusters: 5,
        max_prompt_chars: 4000,
        trigger: "manual".into(),
        workspace: None,
    }
}

struct Setup {
    _d: tempfile::TempDir,
    env: Env,
    gate: Arc<FakeGate>,
    rows: Arc<Rows>,
}

fn setup(answers: &[Option<&str>], passes: bool, eps: Vec<Episode>) -> Setup {
    let d = tempfile::tempdir().unwrap();
    let gate = Arc::new(FakeGate {
        passes,
        seen: Mutex::default(),
    });
    let rows = Arc::new(Rows(Mutex::default()));
    let env = Env {
        dir: LearnDir::new(d.path()),
        provider: Arc::new(Queue(Mutex::new(
            answers.iter().map(|a| a.map(str::to_string)).collect(),
        ))),
        provider_name: "queue".into(),
        model: "m".into(),
        ledger: Some(rows.clone()),
        price: Arc::new(|_: &LedgerRecord| None),
        priced: false,
        day_before: Spent::default(),
        memory_db: Some(d.path().join("memory.db")),
        gate: gate.clone(),
        sessions_dir: None,
        task_episodes: eps,
    };
    Setup {
        _d: d,
        env,
        gate,
        rows,
    }
}

const ADD: &str = r#"{"op":"add","text":"Run cargo fmt before the tests.","reason":"fmt failed"}"#;

#[tokio::test]
async fn a_gated_lesson_is_kept_journaled_and_diffed() {
    let s = setup(&[Some(ADD)], true, vec![ep(1)]);
    let p = run_pass(&opts(), &s.env).await.unwrap();
    assert_eq!(p.status, DONE);
    assert_eq!(p.changes.len(), 1);
    let pb = s.env.dir.read_playbook().unwrap();
    assert!(
        pb.contains("- [pb-1] Run cargo fmt before the tests."),
        "{pb}"
    );
    let seen = s.gate.seen.lock().unwrap()[0].clone().unwrap();
    assert!(seen.contains("Run cargo fmt"));
    let diff = s.env.dir.read_pass_file(&p.id, "playbook.diff").unwrap();
    assert!(
        diff.contains("+- [pb-1] Run cargo fmt before the tests."),
        "{diff}"
    );
    let log = s.env.dir.read_pass_file(&p.id, "changelog.md").unwrap();
    assert!(log.contains("playbook add pb-1"), "{log}");
    let st = s.env.dir.state().unwrap();
    assert_eq!((st.cursor, st.last_id), (100, 1));
    let rows = s.rows.0.lock().unwrap().clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].call_kind, "learn");
    assert_eq!(rows[0].origin.as_deref(), Some("reflect"));
    // Nothing new next time: the cursor moved past the episode.
    let again = run_pass(&opts(), &s.env).await.unwrap();
    assert!(again.episodes.is_empty());
}

#[tokio::test]
async fn a_lesson_that_fails_the_gate_is_rejected_and_the_playbook_is_unchanged() {
    let s = setup(&[Some(ADD)], false, vec![ep(1)]);
    let p = run_pass(&opts(), &s.env).await.unwrap();
    assert_eq!(p.status, DONE);
    assert!(p.changes.is_empty());
    assert_eq!(p.rejected.len(), 1);
    assert!(p.rejected[0].reason.contains("fails with the lesson"));
    assert_eq!(s.env.dir.read_playbook().unwrap(), "");
    assert_eq!(
        s.env.dir.read_pass_file(&p.id, "playbook.diff").unwrap(),
        ""
    );
    let log = s.env.dir.read_pass_file(&p.id, "changelog.md").unwrap();
    assert!(log.contains("## Rejected"), "{log}");
}

#[tokio::test]
async fn guards_reject_duplicates_unknown_ids_and_garbage_without_a_gate_run() {
    let s = setup(
        &[
            Some(ADD),
            Some(ADD),
            Some(r#"{"op":"retire","id":"pb-9"}"#),
            Some("I think you should be careful"),
        ],
        true,
        vec![ep(1), ep(2), ep(3), ep(4)],
    );
    let p = run_pass(&opts(), &s.env).await.unwrap();
    assert_eq!(p.changes.len(), 1);
    let reasons: Vec<_> = p.rejected.iter().map(|r| r.reason.as_str()).collect();
    assert_eq!(reasons.len(), 3, "{reasons:?}");
    assert!(reasons[1].contains("pb-9"));
    assert!(reasons[2].starts_with("reflector answer was not a valid delta"));
    assert_eq!(s.gate.seen.lock().unwrap().len(), 1);
    assert_eq!(p.cursor_after, 400);
}

#[tokio::test]
async fn the_pass_stops_at_the_token_cap_and_leaves_the_rest() {
    let s = setup(
        &[Some(r#"{"op":"none","reason":"outage"}"#); 3],
        true,
        vec![ep(1), ep(2), ep(3)],
    );
    let mut o = opts();
    o.caps.tokens_per_pass = 150;
    let p = run_pass(&o, &s.env).await.unwrap();
    assert_eq!(p.status, STOPPED_BUDGET);
    assert_eq!(p.spent.calls, 2);
    assert_eq!(p.skipped.len(), 1);
    assert_eq!(p.cursor_after, 200, "the unreviewed episode stays ahead");
    assert_eq!(s.env.dir.state().unwrap().cursor, 200);
}

#[tokio::test]
async fn failing_calls_stop_the_pass() {
    let s = setup(&[None, None, None], true, vec![ep(1), ep(2), ep(3), ep(4)]);
    let p = run_pass(&opts(), &s.env).await.unwrap();
    assert_eq!(p.status, STOPPED_ERRORS);
    assert_eq!(p.skipped.len(), 1);
    assert_eq!(p.cursor_after, 0, "failed episodes are retried next pass");
}

#[tokio::test]
async fn a_failed_call_holds_the_cursor_before_its_episode() {
    let none = r#"{"op":"none","reason":"one-off"}"#;
    let s = setup(
        &[Some(none), None, Some(none)],
        true,
        vec![ep(1), ep(2), ep(3)],
    );
    let p = run_pass(&opts(), &s.env).await.unwrap();
    assert_eq!(p.status, DONE);
    assert_eq!(p.cursor_after, 199);
}

#[tokio::test]
async fn revert_restores_the_playbook_once() {
    let s = setup(&[Some(ADD)], true, vec![ep(1)]);
    s.env
        .dir
        .write_playbook("# mine\n- Always ask before deleting.\n")
        .unwrap();
    let p = run_pass(&opts(), &s.env).await.unwrap();
    assert!(s.env.dir.read_playbook().unwrap().contains("[pb-1]"));
    let r = revert(&s.env.dir, "last", None).unwrap();
    assert_eq!(r.id, p.id);
    assert_eq!(r.status, REVERTED);
    assert_eq!(
        s.env.dir.read_playbook().unwrap(),
        "# mine\n- Always ask before deleting.\n"
    );
    assert!(revert(&s.env.dir, &p.id, None).is_err());
    assert!(revert(&s.env.dir, "last", None).is_err());
}

#[tokio::test]
async fn revert_after_an_owner_edit_undoes_only_the_pass_lines() {
    let s = setup(&[Some(ADD)], true, vec![ep(1)]);
    let p = run_pass(&opts(), &s.env).await.unwrap();
    let edited = format!(
        "{}- Owner line added later.\n",
        s.env.dir.read_playbook().unwrap()
    );
    s.env.dir.write_playbook(&edited).unwrap();
    revert(&s.env.dir, &p.id, None).unwrap();
    let pb = s.env.dir.read_playbook().unwrap();
    assert!(!pb.contains("[pb-1]"), "{pb}");
    assert!(pb.contains("- Owner line added later."));
}

#[tokio::test]
async fn consolidation_merges_duplicates_and_revert_undoes_it() {
    let s = setup(
        &[Some(
            r#"{"action":"merge","content":"The database listens on port 5781 on localhost.","reason":"same fact"}"#,
        )],
        true,
        vec![],
    );
    let db = s.env.memory_db.clone().unwrap();
    {
        let m = MemoryStore::open(&db).unwrap();
        m.remember("the database listens on port 5781", &[])
            .unwrap();
        m.remember("database listens on port 5781 locally", &[])
            .unwrap();
        m.remember("Max prefers terse replies", &[]).unwrap();
    }
    let p = run_pass(&opts(), &s.env).await.unwrap();
    assert_eq!(p.changes.len(), 1, "{:?}", p.rejected);
    {
        let m = MemoryStore::open(&db).unwrap();
        let hits = m.recall("database port", 10).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(
            hits[0].content,
            "The database listens on port 5781 on localhost."
        );
    }
    revert(&s.env.dir, &p.id, Some(&db)).unwrap();
    let m = MemoryStore::open(&db).unwrap();
    assert_eq!(m.recall("database port", 10).unwrap().len(), 2);
}

#[tokio::test]
async fn the_plan_spends_and_writes_nothing() {
    let s = setup(&[], true, vec![ep(1), ep(2)]);
    let mut o = opts();
    o.max_episodes = 1;
    let plan = plan(&o, &s.env).unwrap();
    assert_eq!(plan.episodes.len(), 1);
    assert_eq!(plan.later, 1);
    assert_eq!(plan.check.as_deref(), Some("true"));
    assert!(s.rows.0.lock().unwrap().is_empty());
    assert!(!s.env.dir.root().exists());
}
