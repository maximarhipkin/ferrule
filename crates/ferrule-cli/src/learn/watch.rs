//! The learning page's two per-run duties (`docs/m16-learning-loop.md`,
//! "Failure notes" and "Measuring what helps"), as one observer on every
//! root agent:
//!
//! - **What helps.** Each run's end goes to `learn/usage.jsonl` with the
//!   lessons its prompt held, the playbook section's hash and whether it
//!   ended done; each `activate_skill` too. The page and the pass read the
//!   records off it ([`ferrule_learn::usage`]).
//! - **Failure notes.** A run that ends not done (the step limit, stuck, a
//!   check that kept failing, a token budget) leaves its own final status
//!   in memory, secrets scrubbed, at most [`NOTE_CHARS`] characters,
//!   tagged `failure-note` and its scope, so the normal recall brings it
//!   back on a later message it matches: in the user message after that
//!   message, never in the system prompt. No extra model call: the status
//!   is the one the agent already wrote. None is kept for a run the guard
//!   stopped (the kill switch, a cap, the owner's stop, the time limit), or
//!   one where the owner refused an approval: that is the owner's choice,
//!   a signal, not a failure. None for a read-only `/plan` run either, nor
//!   for one that read outside content (M54.5's taint): its status may
//!   carry that content, and a memory carries no taint into the later run
//!   that recalls it. On by default, even with learning off
//!   (`[learning] failure_notes`).
//! - **Memory upkeep.** At most once a day, after a run of the owner's,
//!   facts in the owner's memory that read the same are merged into one
//!   ([`MemoryStore::merge_duplicates`]): no model call, so it runs with
//!   learning off; each merge is journaled in `learn/log.jsonl` and is
//!   M15's reversible UPDATE. Facts that only resemble each other need a
//!   model's judgment, so they wait for the learning pass.

use crate::config::Config;
use ferrule_core::{RunEnd, RunObserver};
use ferrule_learn::usage::{Use, DONE, NOT_DONE};
use ferrule_learn::LearnDir;
use ferrule_memory::{MemoryStore, FAILURE_NOTE};
use ferrule_trust::Hub;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// The longest note kept, in characters.
pub const NOTE_CHARS: usize = 400;
/// Of it, the most the run's goal takes.
const GOAL_CHARS: usize = 120;

/// `[learning] failure_notes` as the page last set it in this process: 0
/// unknown (each agent keeps what its config said), 1 off, 2 on.
static NOTES: AtomicU8 = AtomicU8::new(0);

/// The page changed `[learning] failure_notes`: runs already under way
/// follow it at their end.
pub fn set_notes(on: bool) {
    NOTES.store(if on { 2 } else { 1 }, Ordering::Relaxed);
}

/// Scrubs secrets out of text.
pub type Scrub = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// The process's scrubbing: its known secret values and token shapes (the
/// health redactor), the credential proxy's bound ones (M54.3), and any
/// word that looks like an API key though no one knows it (M52.5's test).
pub fn scrubber(cfg: &Config) -> Scrub {
    let cfg = cfg.clone();
    let redactor: OnceLock<ferrule_gateway::Redactor> = OnceLock::new();
    Arc::new(move |text: &str| {
        let r = redactor.get_or_init(|| crate::health::redactor(&cfg));
        let known = ferrule_core::scrub::text(r.redact(text));
        mask_keys(&known)
    })
}

/// Each word of `text` that looks like a key, as `[redacted]`.
fn mask_keys(text: &str) -> String {
    let none = ferrule_gateway::Redactor::default();
    if !ferrule_gateway::paste_guard::holds_a_key(text, &none) {
        return text.to_string();
    }
    text.split(' ')
        .map(|w| {
            if ferrule_gateway::paste_guard::holds_a_key(w, &none) {
                "[redacted]"
            } else {
                w
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// One root agent's watch.
pub struct RunWatch {
    dir: LearnDir,
    session: String,
    instance: String,
    scope: String,
    tree: String,
    lessons: Vec<u32>,
    playbook: Option<String>,
    memory_db: PathBuf,
    notes: bool,
    /// False for a run that may write no note at all (`/plan`).
    may_note: AtomicBool,
    scrub: Scrub,
    hub: Option<Arc<Hub>>,
    started: Mutex<chrono::DateTime<chrono::Utc>>,
    /// The owner's session: its memory gets the daily upkeep.
    owner: bool,
}

/// How often the memory upkeep runs, at most.
const UPKEEP_EVERY: i64 = 86_400;

impl RunWatch {
    /// The watch of an agent of run tree `tree`, in session `session`,
    /// taught in `scope`, whose memory is `memory_db`. `guest`: the
    /// session isn't the owner's (its prompt holds its scope's lessons
    /// only).
    pub fn new(
        cfg: &Config,
        session: &str,
        scope: &str,
        tree: &str,
        memory_db: &Path,
        guest: bool,
    ) -> Option<Arc<Self>> {
        let data = crate::config::data_dir().ok()?;
        let block = super::prompt_block(&cfg.learning, Some(scope), guest);
        Some(Arc::new(Self {
            dir: LearnDir::new(&data),
            session: session.into(),
            instance: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
            scope: scope.into(),
            tree: tree.into(),
            lessons: block.as_ref().map(|b| b.ids.clone()).unwrap_or_default(),
            playbook: block
                .and_then(|b| b.text)
                .map(|t| format!("{:016x}", ferrule_core::fnv64(t.as_bytes()))),
            memory_db: memory_db.to_path_buf(),
            notes: cfg.learning.failure_notes,
            may_note: AtomicBool::new(true),
            scrub: scrubber(cfg),
            hub: crate::trust::hub(cfg).ok(),
            started: Mutex::new(chrono::Utc::now()),
            owner: !guest,
        }))
    }

    /// `false`: its runs are logged but leave no failure note (a
    /// read-only `/plan` run, whose memory is read-only too).
    pub fn with_notes(self: Arc<Self>, on: bool) -> Arc<Self> {
        self.may_note.store(on, Ordering::Relaxed);
        self
    }

    /// For `activate_skill`: each skill it loads is a line of the usage
    /// log.
    pub fn activation_hook(self: &Arc<Self>) -> ferrule_skills::OnActivate {
        let me = self.clone();
        Arc::new(move |skill: &str| {
            let u = Use::Skill {
                at: chrono::Utc::now().timestamp(),
                session: me.session.clone(),
                instance: me.instance.clone(),
                skill: skill.into(),
            };
            if let Err(e) = me.dir.record_use(&u) {
                tracing::warn!("learning: a skill's use wasn't recorded: {e}");
            }
        })
    }

    fn notes_on(&self) -> bool {
        self.may_note.load(Ordering::Relaxed)
            && notes_setting(NOTES.load(Ordering::Relaxed), self.notes)
    }

    /// Whether this run's note must not be kept: it read outside content
    /// (its status may carry it), the owner refused an approval in it, or
    /// the kill switch is on (the run's end is the owner's doing).
    fn kept_out(&self) -> bool {
        let Some(hub) = &self.hub else {
            return false;
        };
        if hub.untrusted(&self.tree).is_some() {
            return true;
        }
        if hub.stopped().is_some() || hub.escalated(&self.tree).is_some() {
            return true;
        }
        let since = *self.started.lock().unwrap_or_else(|e| e.into_inner());
        hub.audit()
            .read(Some(since))
            .unwrap_or_default()
            .iter()
            .any(|e| {
                e.event == "approval_answered"
                    && e.tree.as_deref() == Some(self.tree.as_str())
                    && e.detail["answer"] == "no"
            })
    }

    /// The daily memory upkeep, if it's due: `learn/upkeep.json` holds when
    /// it last ran. Returns the merges it made.
    async fn upkeep(&self) -> Vec<(i64, Vec<i64>)> {
        if !self.owner {
            return Vec::new();
        }
        let stamp = self.dir.root().join("upkeep.json");
        let now = chrono::Utc::now().timestamp();
        let last = std::fs::read_to_string(&stamp)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v["at"].as_i64())
            .unwrap_or(0);
        if now - last < UPKEEP_EVERY || !self.memory_db.exists() {
            return Vec::new();
        }
        // Stamped first: two runs ending at once don't both do it.
        let _ = ferrule_learn::files::write_atomic(
            &stamp,
            serde_json::json!({ "at": now }).to_string().as_bytes(),
        );
        let db = self.memory_db.clone();
        let merged = tokio::task::spawn_blocking(move || {
            MemoryStore::open(&db).and_then(|s| s.merge_duplicates())
        })
        .await;
        match merged {
            Ok(Ok(m)) => {
                for (kept, replaced) in &m {
                    self.dir.log(
                        "upkeep",
                        "merge",
                        serde_json::json!({ "kept": kept, "replaced": replaced,
                            "why": "the same fact, word for word" }),
                    );
                }
                m
            }
            Ok(Err(e)) => {
                tracing::warn!("memory upkeep: {e}");
                Vec::new()
            }
            Err(e) => {
                tracing::warn!("memory upkeep: {e}");
                Vec::new()
            }
        }
    }

    /// Writes `note` to memory, tagged with the scope.
    fn keep_note(&self, note: &str) -> Result<i64, String> {
        let scope_tag = format!("scope:{}", self.scope.replace(',', "_"));
        MemoryStore::open(&self.memory_db)
            .and_then(|s| s.insert_from(note, &[FAILURE_NOTE, &scope_tag], &[], Some(FAILURE_NOTE)))
            .map(|i| i.id)
            .map_err(|e| e.to_string())
    }
}

/// The page's setting (`page`, as [`NOTES`] holds it) over the agent's
/// own config's.
fn notes_setting(page: u8, own: bool) -> bool {
    match page {
        1 => false,
        2 => true,
        _ => own,
    }
}

/// The failure note for a run that asked `goal` and ended with `answer`,
/// stopped short because of `incomplete`, in `scope`; `None` when there's
/// nothing of the agent's own to keep: the run errored (no answer), was
/// done, or the guard ended it (its answer is the stop's own words).
pub fn note_for(
    goal: &str,
    answer: Option<&str>,
    incomplete: Option<&str>,
    scope: &str,
    day: &str,
    scrub: &dyn Fn(&str) -> String,
) -> Option<String> {
    let (answer, why) = (answer?.trim(), incomplete?.trim());
    if answer.is_empty() || answer == why {
        return None;
    }
    let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let clip = |s: String, n: usize| {
        if s.chars().count() <= n {
            s
        } else {
            format!(
                "{}…",
                s.chars().take(n.saturating_sub(1)).collect::<String>()
            )
        }
    };
    let goal = clip(scrub(&flat(goal)), GOAL_CHARS);
    let head = format!("Unfinished run ({day}, {scope}), asked: \"{goal}\". Its status: ");
    let left = NOTE_CHARS.saturating_sub(head.chars().count()).max(40);
    let status = clip(scrub(&flat(answer)), left);
    Some(clip(format!("{head}{status}"), NOTE_CHARS))
}

#[async_trait::async_trait]
impl RunObserver for RunWatch {
    fn name(&self) -> &str {
        "learning"
    }

    async fn begin(&self, _session_id: &str) {
        *self.started.lock().unwrap_or_else(|e| e.into_inner()) = chrono::Utc::now();
    }

    async fn end(&self, run: &RunEnd<'_>) -> Option<String> {
        let done = run.answer.is_some() && run.incomplete.is_none();
        let u = Use::Run {
            at: chrono::Utc::now().timestamp(),
            session: self.session.clone(),
            instance: self.instance.clone(),
            outcome: if done { DONE } else { NOT_DONE }.into(),
            lessons: self.lessons.clone(),
            playbook: self.playbook.clone(),
        };
        if let Err(e) = self.dir.record_use(&u) {
            tracing::warn!("learning: a run's use wasn't recorded: {e}");
        }
        self.upkeep().await;
        if done || !self.notes_on() {
            return None;
        }
        let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let note = note_for(
            run.goal,
            run.answer,
            run.incomplete,
            &self.scope,
            &day,
            &*self.scrub,
        )?;
        if self.kept_out() {
            return None;
        }
        match self.keep_note(&note) {
            Ok(id) => tracing::info!(memory = id, "learning: kept a failure note"),
            Err(e) => tracing::warn!("learning: the failure note wasn't kept: {e}"),
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-proj-AbC123dEf456GhI789jKl012";

    fn watch(d: &Path) -> Arc<RunWatch> {
        watch_with(d, None)
    }

    fn watch_with(d: &Path, hub: Option<Arc<Hub>>) -> Arc<RunWatch> {
        Arc::new(RunWatch {
            dir: LearnDir::new(d),
            session: "telegram__42".into(),
            instance: "i1".into(),
            scope: "chat:telegram__42".into(),
            tree: "telegram__42".into(),
            lessons: vec![3],
            playbook: Some("00ff".into()),
            memory_db: d.join("memory.db"),
            notes: true,
            may_note: AtomicBool::new(true),
            scrub: Arc::new(|t: &str| mask_keys(&t.replace("hunter2hunter2", "[redacted]"))),
            hub,
            started: Mutex::new(chrono::Utc::now()),
            owner: true,
        })
    }

    fn notes(d: &Path) -> Vec<ferrule_memory::Memory> {
        MemoryStore::open(d.join("memory.db"))
            .unwrap()
            .live_tagged(&[FAILURE_NOTE])
            .unwrap()
    }

    #[tokio::test]
    async fn a_failed_run_writes_one_scoped_note_and_a_done_run_none() {
        let d = tempfile::tempdir().unwrap();
        let w = watch(d.path());
        w.end(&RunEnd {
            session_id: "telegram__42",
            goal: "deploy the site to staging",
            answer: Some("Done: it's live."),
            incomplete: None,
        })
        .await;
        assert!(notes(d.path()).is_empty(), "a done run writes no note");
        let status = format!(
            "I stopped before finishing: `npm test` still fails. The token hunter2hunter2 and {KEY} were in the log.\n{}",
            "x".repeat(600)
        );
        w.end(&RunEnd {
            session_id: "telegram__42",
            goal: "deploy the site to staging",
            answer: Some(&status),
            incomplete: Some("`npm test` still fails after 3 rounds of fixes"),
        })
        .await;
        let all = notes(d.path());
        assert_eq!(all.len(), 1, "{all:?}");
        let n = &all[0];
        assert!(n.tags.contains(&"scope:chat:telegram__42".to_string()));
        assert!(
            n.content.contains("deploy the site to staging"),
            "{}",
            n.content
        );
        assert!(
            n.content.contains("`npm test` still fails"),
            "{}",
            n.content
        );
        assert!(!n.content.contains("hunter2hunter2"), "{}", n.content);
        assert!(!n.content.contains(KEY), "{}", n.content);
        assert!(n.content.chars().count() <= NOTE_CHARS);
        // Both runs are in the usage log, with the prompt's lessons.
        let uses = w.dir.uses();
        assert_eq!(uses.len(), 2);
        assert_eq!(w.dir.usage_stats().lessons[&3].failed, 1);
    }

    #[tokio::test]
    async fn a_guard_stop_or_an_error_writes_no_note() {
        let d = tempfile::tempdir().unwrap();
        let w = watch(d.path());
        let stop = "Ferrule is stopped (by telegram chat 42). Nothing was run.";
        w.end(&RunEnd {
            session_id: "telegram__42",
            goal: "go",
            answer: Some(stop),
            incomplete: Some(stop),
        })
        .await;
        w.end(&RunEnd {
            session_id: "telegram__42",
            goal: "go",
            answer: None,
            incomplete: None,
        })
        .await;
        assert!(notes(d.path()).is_empty());
        assert_eq!(w.dir.uses().len(), 2, "both still count as not done");
    }

    fn failed<'a>() -> RunEnd<'a> {
        RunEnd {
            session_id: "telegram__42",
            goal: "summarise the vendor's page",
            answer: Some("I stopped before finishing: the step limit."),
            incomplete: Some("it reached the limit of 30 steps"),
        }
    }

    #[tokio::test]
    async fn a_plan_run_logs_its_use_but_writes_no_note() {
        let d = tempfile::tempdir().unwrap();
        let w = watch(d.path()).with_notes(false);
        w.end(&failed()).await;
        assert!(notes(d.path()).is_empty());
        assert_eq!(w.dir.uses().len(), 1);
    }

    #[tokio::test]
    async fn a_run_that_read_outside_content_writes_no_note() {
        let d = tempfile::tempdir().unwrap();
        let hub = Arc::new(
            Hub::new(
                Default::default(),
                d.path(),
                &d.path().join("ledger.jsonl"),
                Arc::new(ferrule_trust::SystemClock),
                vec![],
            )
            .unwrap(),
        );
        let w = watch_with(d.path(), Some(hub.clone()));
        hub.begin_run("telegram__42");
        hub.saw_untrusted("telegram__42", "web_fetch");
        w.end(&failed()).await;
        assert!(notes(d.path()).is_empty(), "tainted: no note");
        // The same failure in a clean run is kept.
        hub.begin_run("telegram__42");
        w.end(&failed()).await;
        assert_eq!(notes(d.path()).len(), 1);
    }

    #[tokio::test]
    async fn the_daily_upkeep_merges_word_for_word_duplicates_once_a_day() {
        let d = tempfile::tempdir().unwrap();
        let w = watch(d.path());
        let store = MemoryStore::open(d.path().join("memory.db")).unwrap();
        let a = store.remember("The deploy target is fly.io", &[]).unwrap();
        let b = store
            .remember("the deploy target is  fly.io.", &[])
            .unwrap();
        let c = store.remember("The staging port is 5781", &[]).unwrap();
        store.set_pinned(a, true).unwrap();
        assert_eq!(w.upkeep().await, vec![(b, vec![a])]);
        let kept = store.get(b).unwrap().unwrap();
        assert!(kept.pinned, "the pin goes with the fact");
        assert_eq!(store.get(a).unwrap().unwrap().superseded_by, Some(b));
        assert!(store.get(c).unwrap().unwrap().superseded_by.is_none());
        // Not again the same day.
        store.remember("The staging port is 5781", &[]).unwrap();
        assert!(w.upkeep().await.is_empty());
        let log = std::fs::read_to_string(w.dir.root().join("log.jsonl")).unwrap();
        assert!(log.contains("\"merge\""), "{log}");
    }

    #[test]
    fn the_page_setting_wins_over_the_agents_own() {
        assert!(notes_setting(0, true) && !notes_setting(0, false));
        assert!(!notes_setting(1, true) && notes_setting(2, false));
    }

    #[tokio::test]
    async fn activations_are_logged() {
        let d = tempfile::tempdir().unwrap();
        let w = watch(d.path());
        (w.activation_hook())("deploy");
        assert!(matches!(&w.dir.uses()[0], Use::Skill { skill, .. } if skill == "deploy"));
    }
}
