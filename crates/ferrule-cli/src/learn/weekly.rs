//! The weekly playbook eval (`docs/m16-learning-loop.md`, "The weekly
//! playbook eval"; `docs/eval.md`): is the playbook still worth having?
//! Each kept lesson whose pass had a check, learned from a scheduled task,
//! becomes an eval task (the task's prompt, a copy of the pass's
//! workspace, the check as grader); with none, the starter suite's smoke
//! tasks stand in. Each runs twice, `--ablate playbook`: with the owner's
//! playbook and without it, on the learning pass's model, inside
//! `[learning] eval_usd_cap` and the owner's caps and kill switch. One line
//! a run goes to `learn/evals.jsonl`; the run itself is saved like any
//! eval run (`ferrule eval report` lists it). Off by default
//! (`[learning] weekly_eval`): it spends money unattended.

use crate::config::{self, Config};
use crate::ledger;
use anyhow::{anyhow, bail, Context as _, Result};
use ferrule_eval::{Caps, Env, Options, Outcome, Part, Pricing, Suite, SuiteRun, Variant};
use ferrule_gateway::{BuiltinJob, BuiltinSpec, JobReport, Task, TaskStore};
use ferrule_learn::files::{Change, REVERTED};
use ferrule_learn::LearnDir;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The built-in task's name in `ferrule tasks list`.
pub const TASK_NAME: &str = "ferrule-learn-eval";
/// The starter suite's tasks used when no kept lesson has a check.
const STARTER_TAGS: &[&str] = &["smoke"];
/// Runs the trend shows.
const TREND: usize = 12;
/// A token cap on top of the dollar one, for a provider without prices.
const MAX_TOKENS: u64 = 2_000_000;
/// The suite file a run writes, under `learn/`.
const SUITE_DIR: &str = "weekly-eval";

/// An eval in this process, one at a time.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// One run, a line of `learn/evals.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measured {
    /// RFC 3339.
    pub at: String,
    /// The share of tasks passed with the playbook; `None` when none
    /// finished with a verdict.
    pub with_pass: Option<f64>,
    pub without_pass: Option<f64>,
    pub tasks: usize,
    pub spent_usd: f64,
    /// `kept lessons` or `starter`.
    pub suite: String,
    pub run_id: String,
    /// Why it stopped short (a cap), if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
}

impl Measured {
    /// With minus without, when both have a rate.
    pub fn delta(&self) -> Option<f64> {
        Some(self.with_pass? - self.without_pass?)
    }
}

fn log_path(dir: &LearnDir) -> PathBuf {
    dir.root().join("evals.jsonl")
}

/// Every run kept, oldest first.
pub fn history(dir: &LearnDir) -> Vec<Measured> {
    std::fs::read_to_string(log_path(dir))
        .map(|t| {
            t.lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The learning page's `eval`.
pub fn view(cfg: &Config, dir: &LearnDir) -> Value {
    let all = history(dir);
    let last = all.last().map(|m| {
        json!({
            "at": m.at, "with_pass": m.with_pass, "without_pass": m.without_pass,
            "tasks": m.tasks, "spent_usd": m.spent_usd,
        })
    });
    let trend: Vec<Value> = all
        .iter()
        .rev()
        .take(TREND)
        .rev()
        .map(|m| json!({ "at": m.at, "delta": m.delta() }))
        .collect();
    json!({
        "enabled": cfg.learning.weekly_eval,
        "usd_cap": cfg.learning.eval_usd_cap,
        "last": last,
        "trend": trend,
    })
}

pub fn spec(cfg: &Config) -> BuiltinSpec {
    BuiltinSpec {
        name: TASK_NAME.into(),
        schedule: cfg.learning.eval_schedule.clone(),
        timezone: cfg.learning.timezone.clone(),
        description: "[built-in] the weekly playbook eval (the Learning page)".into(),
    }
}

/// The tasks a run would use.
pub struct Built {
    pub suite: Suite,
    pub tags: Vec<String>,
    /// `kept lessons` or `starter`.
    pub source: &'static str,
}

#[derive(Serialize)]
struct FileOut {
    suite: HeaderOut,
    task: Vec<TaskOut>,
}

#[derive(Serialize)]
struct HeaderOut {
    name: String,
    description: String,
    owner_playbook: bool,
    max_iterations: usize,
    timeout_secs: u64,
}

#[derive(Serialize)]
struct TaskOut {
    id: String,
    prompt: String,
    fixture: PathBuf,
    check: String,
    grade: GradeOut,
}

#[derive(Serialize)]
struct GradeOut {
    command: String,
}

/// Whether `dir` is small enough to copy for a task, by the gate's caps.
fn copyable(dir: &Path) -> bool {
    let (mut files, mut bytes) = (0usize, 0u64);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            return false;
        };
        for e in rd.flatten() {
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                stack.push(e.path());
            } else if t.is_file() {
                files += 1;
                bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
                if files > ferrule_learn::gate::MAX_FILES || bytes > ferrule_learn::gate::MAX_BYTES
                {
                    return false;
                }
            }
        }
    }
    true
}

/// The kept lessons with a check, learned from a scheduled task that is
/// still there, as eval tasks in `<learn>/weekly-eval/suite.toml`; with
/// none, the starter suite's smoke tasks (`starter`).
pub fn build_suite(dir: &LearnDir, data: &Path, starter: Option<PathBuf>) -> Result<Built> {
    let db = data.join("tasks.db");
    let tasks = if db.exists() {
        TaskStore::open(&db)
            .and_then(|s| s.list())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut out: Vec<TaskOut> = Vec::new();
    for p in dir.passes().iter().filter(|p| p.status != REVERTED) {
        let (Some(check), Some(ws)) = (&p.check, &p.workspace) else {
            continue;
        };
        let ws = PathBuf::from(ws);
        for (i, c) in p.changes.iter().enumerate() {
            let Change::Playbook {
                applied, episode, ..
            } = c
            else {
                continue;
            };
            if applied.op == "retire" || p.is_undone(i + 1) {
                continue;
            }
            let mut parts = episode.splitn(3, ':');
            let (Some("task"), Some(task_id)) = (parts.next(), parts.next()) else {
                continue;
            };
            let Some(task) = tasks.iter().find(|t| t.id == task_id) else {
                continue;
            };
            let id: String = format!("pb-{}-{task_id}", applied.id)
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            let seen = out
                .iter()
                .any(|t| t.prompt == task.prompt && &t.check == check);
            if seen || !ws.is_dir() || !copyable(&ws) {
                continue;
            }
            out.push(TaskOut {
                id,
                prompt: task.prompt.clone(),
                fixture: ws.clone(),
                check: check.clone(),
                grade: GradeOut {
                    command: check.clone(),
                },
            });
        }
    }
    if out.is_empty() {
        let starter = starter.ok_or_else(|| {
            anyhow!("no kept lesson has a check to measure it by, and the starter suite isn't here")
        })?;
        let mut suite = Suite::load(&starter)?;
        suite.owner_playbook = true;
        return Ok(Built {
            suite,
            tags: STARTER_TAGS.iter().map(|t| t.to_string()).collect(),
            source: "starter",
        });
    }
    let file = FileOut {
        suite: HeaderOut {
            name: "playbook-weekly".into(),
            description: "The kept lessons' tasks, with and without the playbook".into(),
            owner_playbook: true,
            max_iterations: 30,
            timeout_secs: 900,
        },
        task: out,
    };
    let root = dir.root().join(SUITE_DIR);
    std::fs::create_dir_all(&root)?;
    std::fs::write(root.join("suite.toml"), toml::to_string(&file)?)?;
    Ok(Built {
        suite: Suite::load(&root)?,
        tags: Vec::new(),
        source: "kept lessons",
    })
}

/// The share of `variant`'s runs that passed, of those with a verdict.
fn rate(run: &SuiteRun, variant: Variant) -> Option<f64> {
    let judged: Vec<_> = run
        .results
        .iter()
        .filter(|r| r.variant == variant && matches!(r.outcome, Outcome::Pass | Outcome::Fail))
        .collect();
    let passed = judged.iter().filter(|r| r.outcome == Outcome::Pass).count();
    (!judged.is_empty()).then(|| passed as f64 / judged.len() as f64)
}

/// Runs `built` with and without the playbook (`env.playbook`), inside
/// `usd_cap`, and keeps the run's line in `dir`.
pub async fn measure(
    dir: &LearnDir,
    built: &Built,
    env: &Env,
    usd_cap: f64,
    work_root: Option<PathBuf>,
    allow_grader_reads: bool,
) -> Result<(SuiteRun, Measured)> {
    if env.playbook.is_none() {
        bail!("the playbook has no lessons to measure");
    }
    let opts = Options {
        variants: vec![Variant::Engineered, Variant::Ablated],
        tags: built.tags.clone(),
        ablate: vec![Part::Playbook],
        repeat: 1,
        caps: Caps {
            max_usd: (usd_cap > 0.0).then_some(usd_cap),
            max_tokens: Some(MAX_TOKENS),
        },
        work_root,
        allow_grader_reads,
        ..Default::default()
    };
    let run = ferrule_eval::run_suite(&built.suite, env, &opts).await?;
    let m = Measured {
        at: chrono::Utc::now().to_rfc3339(),
        with_pass: rate(&run, Variant::Engineered),
        without_pass: rate(&run, Variant::Ablated),
        tasks: built.suite.select(&built.tags, &[])?.len(),
        spent_usd: run.totals.cost_usd.unwrap_or(0.0),
        suite: built.source.into(),
        run_id: run.run_id.clone(),
        stopped: run.budget_stop.clone(),
    };
    let line = serde_json::to_string(&m)?;
    ferrule_learn::files::append_log(&log_path(dir), &line).map_err(|e| anyhow!(e))?;
    Ok((run, m))
}

/// The eval's environment from the config: the learning pass's model, the
/// config's sandbox without the credential broker, the owner's playbook,
/// and the owner's guard (caps, kill switch) over every run.
fn env(cfg: &Config, data: &Path) -> Result<Env> {
    let cat = crate::models::Catalog::from_config(cfg);
    let entry = match cfg.learning.provider.as_deref() {
        Some(w) => cat.resolve(w).map_err(|e| anyhow!("{e}"))?,
        None => cat.default_entry().map_err(|e| anyhow!("{e}"))?.0,
    };
    let key = entry
        .key()
        .filter(|k| !k.is_empty() || entry.plan.is_some())
        .ok_or_else(|| anyhow!(entry.no_key()))?;
    let sandbox = Arc::new(crate::managed::guard(
        ferrule_sandbox::Sandbox::new(crate::sandbox_policy(cfg)).map_err(|e| anyhow!(e))?,
    ));
    crate::trust::hub(cfg)?;
    let owner = cfg.clone();
    let owner_trust: ferrule_eval::OwnerTrust = Arc::new(move |tree: &str, sink| {
        crate::trust::seat(
            tree,
            ferrule_trust::Route::Unattended("this is the weekly playbook eval".into()),
        );
        let tag = ledger::LedgerTag {
            sink,
            task_shape: "eval".into(),
            origin: Some("learn-eval".into()),
        };
        let (tag, guard) = crate::trust::equip(&owner, tree, false, Some(tag), None)
            .expect("the hub was built before the eval started");
        (tag.sink, guard as Arc<dyn ferrule_core::Guard>)
    });
    Ok(Env {
        provider: entry.client(key),
        provider_name: entry.provider.clone(),
        model: entry.model.clone(),
        profile: entry.harness(),
        sandbox,
        memory_tools: Some(Arc::new(crate::memory_tools::tools)),
        ledger: ledger::build_sink(cfg),
        pricing: entry.pricing.map(|p| Pricing {
            input: p.input,
            cached_input: p.cached_input,
            output: p.output,
            cache_write: p.cache_write,
        }),
        transcripts: Some(data.join("eval")),
        judge: None,
        playbook: super::prompt_section(&cfg.learning, None, false),
        owner_trust: Some(owner_trust),
        routing: None,
    })
}

/// Why no eval may start now, in words.
fn refusal(cfg: &Config) -> Result<Option<String>> {
    if cfg.learning.eval_usd_cap <= 0.0 {
        return Ok(Some(
            "No eval: [learning] eval_usd_cap is 0, and the eval runs only under a cap.".into(),
        ));
    }
    if let Some(why) = crate::trust::hub(cfg)?.check("learn-eval", None, false) {
        return Ok(Some(format!("No eval: {why}")));
    }
    if RUNNING.load(Ordering::SeqCst) {
        return Ok(Some("An eval is already running.".into()));
    }
    if super::prompt_section(&cfg.learning, None, false).is_none() {
        return Ok(Some(
            "No eval: the playbook has no lessons to measure yet.".into(),
        ));
    }
    Ok(None)
}

/// One whole run from the config; the run is saved under `<data>/eval`.
async fn run(cfg: &Config) -> Result<Measured> {
    let data = config::data_dir()?;
    let dir = LearnDir::new(&data);
    let starter = crate::model_eval::find_suite(cfg).ok();
    let built = build_suite(&dir, &data, starter)?;
    let env = env(cfg, &data)?;
    let (run, m) = measure(&dir, &built, &env, cfg.learning.eval_usd_cap, None, false).await?;
    let out = data.join("eval").join(&run.run_id);
    std::fs::create_dir_all(&out)?;
    std::fs::write(out.join("run.json"), serde_json::to_string_pretty(&run)?)
        .context("saving the eval run")?;
    Ok(m)
}

fn words(m: &Measured) -> String {
    let pct = |r: Option<f64>| r.map_or("no verdict".into(), |r| format!("{:.0}%", r * 100.0));
    let mut s = format!(
        "playbook eval ({}, {} task(s)): {} with the playbook, {} without, ${:.4}",
        m.suite,
        m.tasks,
        pct(m.with_pass),
        pct(m.without_pass),
        m.spent_usd
    );
    if let Some(why) = &m.stopped {
        s.push_str(&format!("; stopped: {why}"));
    }
    s
}

/// The page's "Run now": one eval in the background. `Err` says why
/// none starts: the cap, the owner's caps or kill switch, one running, an
/// empty playbook, no tasks, no model.
pub async fn start(cfg: &Config) -> Result<String> {
    if let Some(why) = refusal(cfg)? {
        bail!(why);
    }
    let data = config::data_dir()?;
    let built = build_suite(
        &LearnDir::new(&data),
        &data,
        crate::model_eval::find_suite(cfg).ok(),
    )?;
    env(cfg, &data)?;
    let n = built.suite.select(&built.tags, &[])?.len();
    if RUNNING.swap(true, Ordering::SeqCst) {
        bail!("An eval is already running.");
    }
    let cap = cfg.learning.eval_usd_cap;
    let cfg = cfg.clone();
    tokio::spawn(async move {
        match run(&cfg).await {
            Ok(m) => tracing::info!("learning: {}", words(&m)),
            Err(e) => tracing::warn!("learning: the playbook eval failed: {e:#}"),
        }
        RUNNING.store(false, Ordering::SeqCst);
    });
    Ok(format!(
        "The playbook eval started: {n} task(s) from the {}, each with and without the playbook, within ${:.2}. Its result shows here when it's done.",
        built.source, cap
    ))
}

pub struct Job;

#[async_trait::async_trait]
impl BuiltinJob for Job {
    async fn run(&self, _task: &Task) -> Result<JobReport, String> {
        let (cfg, _) = Config::load().map_err(|e| format!("{e:#}"))?;
        if !cfg.learning.weekly_eval {
            let said = super::apply(&cfg).unwrap_or_else(|e| format!("{e:#}"));
            return Ok(JobReport {
                summary: format!("no eval: [learning] weekly_eval = false; {said}"),
                incomplete: None,
            });
        }
        if let Some(why) = refusal(&cfg).map_err(|e| format!("{e:#}"))? {
            return Ok(JobReport {
                summary: why.clone(),
                incomplete: Some(why),
            });
        }
        RUNNING.store(true, Ordering::SeqCst);
        let done = run(&cfg).await;
        RUNNING.store(false, Ordering::SeqCst);
        let m = done.map_err(|e| format!("{e:#}"))?;
        Ok(JobReport {
            summary: words(&m),
            incomplete: m.stopped.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrule_core::{
        CompletionRequest, CompletionResponse, CoreError, HarnessProfile, Message, Provider, Role,
        ToolCall, Usage,
    };

    const LESSON: &str = "Write result.txt containing DONE before you answer.";

    /// Writes `result.txt` only when its prompt holds the lesson.
    struct Follows;

    #[async_trait::async_trait]
    impl Provider for Follows {
        fn name(&self) -> &str {
            "mock"
        }
        async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            let sys = req
                .messages
                .iter()
                .find(|m| m.role == Role::System)
                .and_then(|m| m.content.clone())
                .unwrap_or_default();
            let wrote = req.messages.iter().any(|m| m.role == Role::Tool);
            let message = if sys.contains(LESSON) && !wrote {
                Message::assistant(
                    None,
                    vec![ToolCall {
                        id: "c1".into(),
                        name: "write_file".into(),
                        arguments: json!({ "path": "result.txt", "content": "DONE\n" }),
                    }],
                    None,
                )
            } else {
                Message::assistant(Some("done".into()), vec![], None)
            };
            Ok(CompletionResponse {
                message,
                usage: Usage::default(),
                stop_reason: None,
            })
        }
    }

    fn mock_env(playbook: Option<String>) -> Env {
        Env {
            provider: Arc::new(Follows),
            provider_name: "mock".into(),
            model: "mock-1".into(),
            profile: HarnessProfile::generic(),
            sandbox: Arc::new(ferrule_sandbox::Sandbox::off()),
            memory_tools: None,
            ledger: None,
            pricing: None,
            transcripts: None,
            judge: None,
            playbook,
            owner_trust: None,
            routing: None,
        }
    }

    /// A kept lesson learned from scheduled task `report`, whose pass had
    /// a check, in a data dir with the task in `tasks.db`.
    fn seeded() -> (tempfile::TempDir, LearnDir) {
        let d = tempfile::tempdir().unwrap();
        let data = d.path().join("data");
        let ws = d.path().join("work");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("README"), "a workspace").unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let store = TaskStore::open(data.join("tasks.db")).unwrap();
        store
            .add(
                ferrule_gateway::NewTask {
                    name: "report".into(),
                    kind: ferrule_gateway::TaskKind::Cron,
                    schedule: "0 9 * * *".into(),
                    timezone: "UTC".into(),
                    channel: "local".into(),
                    chat_id: "c".into(),
                    prompt: "Produce the daily report.".into(),
                    gate: None,
                    model: None,
                },
                "report".into(),
                0,
                Some(0),
            )
            .unwrap();
        let dir = LearnDir::new(&data);
        let pass = json!({
            "id": "20261101-030000-ab12", "status": "done", "trigger": "manual",
            "started_at": "2026-11-01T03:00:00Z",
            "caps": { "usd_per_pass": 0.5, "usd_per_day": 1.0, "tokens_per_pass": 1, "tokens_per_day": 1 },
            "spent": { "usd": 0.0, "tokens": 0, "calls": 0 },
            "day_spent_before": { "usd": 0.0, "tokens": 0, "calls": 0 },
            "check": "grep -q DONE result.txt",
            "workspace": ws.display().to_string(),
            "cursor_before": 0, "cursor_after": 0,
            "changes": [{
                "kind": "playbook", "reason": "r", "episode": "task:report:r1", "gate": "g",
                "applied": { "op": "add", "id": 1, "new": LESSON, "line": 3 },
            }],
        });
        let p = dir.pass_dir("20261101-030000-ab12");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("pass.json"), pass.to_string()).unwrap();
        (d, dir)
    }

    #[test]
    fn a_kept_lesson_with_a_check_becomes_an_eval_task() {
        let (d, dir) = seeded();
        let built = build_suite(&dir, &d.path().join("data"), None).unwrap();
        assert_eq!(built.source, "kept lessons");
        assert!(built.suite.owner_playbook);
        let t = &built.suite.tasks[0];
        assert_eq!(
            (t.id.as_str(), t.prompt.as_str()),
            ("pb-1-report", "Produce the daily report.")
        );
        assert_eq!(t.grade.command.as_deref(), Some("grep -q DONE result.txt"));
        // Without one, and without the starter suite: said, not guessed.
        let empty = tempfile::tempdir().unwrap();
        let e = build_suite(&LearnDir::new(empty.path()), empty.path(), None)
            .err()
            .unwrap();
        assert!(e.to_string().contains("starter suite"), "{e}");
    }

    #[tokio::test]
    async fn the_eval_measures_the_playbook_with_and_without_it() {
        let (d, dir) = seeded();
        let built = build_suite(&dir, &d.path().join("data"), None).unwrap();
        let block = format!("[Playbook]\n- {LESSON}");
        let work = d.path().join("runs");
        let (_, m) = measure(&dir, &built, &mock_env(Some(block)), 1.0, Some(work), true)
            .await
            .unwrap();
        assert_eq!(
            (m.with_pass, m.without_pass),
            (Some(1.0), Some(0.0)),
            "{m:?}"
        );
        assert_eq!(m.delta(), Some(1.0));
        assert_eq!(m.tasks, 1);
        assert_eq!(history(&dir), vec![m.clone()]);
        let cfg: Config =
            toml::from_str("[learning]\nweekly_eval = true\neval_usd_cap = 2.5\n").unwrap();
        let v = view(&cfg, &dir);
        assert_eq!(v["enabled"], true);
        assert_eq!(v["usd_cap"], 2.5);
        assert_eq!(v["last"]["with_pass"], 1.0);
        assert_eq!(v["trend"][0]["delta"], 1.0);
        // No lessons, nothing to measure.
        let e = measure(&dir, &built, &mock_env(None), 1.0, None, true)
            .await
            .err()
            .unwrap();
        assert!(e.to_string().contains("no lessons"), "{e}");
    }
}
