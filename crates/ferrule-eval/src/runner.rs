//! Runs a suite: for each selected task, each variant and each repeat, a
//! fresh fixture, an agent, the graders, and one verdict row in the ledger.

use crate::fixture::Fixture;
use crate::grade::{self, GraderResult};
use crate::rubric::{self, Judge};
use crate::sink::{Caps, EvalSink, Pricing, Totals};
use crate::suite::{Suite, Task};
use crate::variant::{self, MemoryTools, Variant};
use anyhow::{Context as _, Result};
use ferrule_core::{
    AgentEvent, CoreError, EvalTag, HarnessProfile, LedgerRecord, LedgerSink, Provider, StopFlag,
    Transcript,
};
use ferrule_sandbox::Sandbox;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// The ledger's `call_kind` for a verdict row.
pub const RESULT_KIND: &str = "eval_result";

/// What the runs use: one provider and model for every variant.
pub struct Env {
    pub provider: Arc<dyn Provider>,
    /// The `[providers.*]` name, as the ledger records it.
    pub provider_name: String,
    pub model: String,
    pub profile: HarnessProfile,
    pub sandbox: Arc<Sandbox>,
    pub memory_tools: Option<MemoryTools>,
    /// Where rows end up (the CLI's ledger file). `None` keeps them in
    /// memory only: the returned [`SuiteRun`] still has every verdict.
    pub ledger: Option<Arc<dyn LedgerSink>>,
    pub pricing: Option<Pricing>,
    /// Transcripts go to `<dir>/<run_id>/<task>--<variant>.jsonl`.
    pub transcripts: Option<PathBuf>,
    /// Grades rubrics. `None`: the run's own provider and model, and the
    /// report says the run was self-judged.
    pub judge: Option<Judge>,
}

impl Env {
    fn judge(&self) -> (Judge, bool) {
        match &self.judge {
            Some(j) => (j.clone(), false),
            None => (
                Judge {
                    provider: self.provider.clone(),
                    provider_name: self.provider_name.clone(),
                    model: self.model.clone(),
                    pricing: self.pricing,
                },
                true,
            ),
        }
    }
}

pub type Progress = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone)]
pub struct Options {
    pub variants: Vec<Variant>,
    pub tags: Vec<String>,
    pub tasks: Vec<String>,
    pub repeat: u32,
    /// Overrides the suite's `context_window`.
    pub context_window: Option<usize>,
    pub caps: Caps,
    pub keep: bool,
    /// Parent of the per-run fixture directories (default: the OS temp
    /// dir).
    pub work_root: Option<PathBuf>,
    pub progress: Option<Progress>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            variants: vec![Variant::Engineered],
            tags: vec![],
            tasks: vec![],
            repeat: 1,
            context_window: None,
            caps: Caps::default(),
            keep: false,
            work_root: None,
            progress: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Pass,
    Fail,
    /// Setup, provider or grader trouble: no verdict on the agent.
    Error,
    /// The budget ended it; not counted in pass rates.
    Stopped,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Pass => "pass",
            Outcome::Fail => "fail",
            Outcome::Error => "error",
            Outcome::Stopped => "stopped",
        }
    }
}

/// One (task, variant, repeat): the payload of its verdict row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub task: String,
    pub variant: Variant,
    #[serde(default)]
    pub repeat: u32,
    pub outcome: Outcome,
    #[serde(default)]
    pub graders: Vec<GraderResult>,
    #[serde(default)]
    pub totals: Totals,
    #[serde(default)]
    pub iterations: usize,
    #[serde(default)]
    pub wall_ms: u64,
    #[serde(default)]
    pub truncations: u32,
    #[serde(default)]
    pub compactions: u32,
    /// Failed checks the engineered variant was sent back to fix.
    #[serde(default)]
    pub verify_failures: u32,
    /// Why the agent stopped before it was done, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_early: Option<String>,
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default)]
    pub context_window: usize,
    #[serde(default)]
    pub ferrule_version: String,
}

/// A whole suite run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteRun {
    pub run_id: String,
    pub suite: String,
    pub kind: String,
    pub provider: String,
    pub model: String,
    pub context_window: usize,
    pub results: Vec<TaskResult>,
    pub totals: Totals,
    /// Set when the budget ended the suite early.
    pub budget_stop: Option<String>,
    /// (task, variant, repeat) runs the budget stop left unstarted.
    pub not_run: usize,
    /// Who judged the rubrics (`<model> via <provider>`), if any task has
    /// one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge: Option<String>,
    /// The judge is the model under test.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub self_judged: bool,
}

/// `<UTC time to the millisecond>-<random>`: ids sort in the order the runs
/// started, which is how the diff finds the run before. (Ids from before
/// the milliseconds were added, `…T153312-a8ae1c`, still sort first within
/// their second: `-` sorts before `.`.)
pub fn new_run_id() -> String {
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3f");
    let short = uuid::Uuid::new_v4().simple().to_string();
    format!("{now}-{}", &short[..6])
}

pub async fn run_suite(suite: &Suite, env: &Env, opts: &Options) -> Result<SuiteRun> {
    let tasks = suite.select(&opts.tags, &opts.tasks)?;
    let run_id = new_run_id();
    let window = opts.context_window.or(suite.context_window);
    let profile = variant::windowed(&env.profile, window);
    let sink = Arc::new(EvalSink::new(env.ledger.clone(), env.pricing, opts.caps));
    let work = opts
        .work_root
        .clone()
        .unwrap_or_else(std::env::temp_dir)
        .join("ferrule-eval")
        .join(&run_id);
    std::fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
    let transcripts = match &env.transcripts {
        Some(dir) => {
            let dir = dir.join(&run_id);
            std::fs::create_dir_all(&dir)?;
            Some(dir)
        }
        None => None,
    };
    let say = |line: &str| {
        if let Some(p) = &opts.progress {
            p(line)
        }
    };

    let repeat = opts.repeat.max(1);
    let planned = tasks.len() * opts.variants.len() * repeat as usize;
    let mut results = Vec::with_capacity(planned);
    'outer: for task in &tasks {
        for r in 0..repeat {
            // Variants back to back per task, so a budget stop leaves a
            // fair comparison of the tasks that did run.
            for &v in &opts.variants {
                if sink.exceeded().is_some() {
                    break 'outer;
                }
                let label = run_label(&task.id, v, r, repeat);
                say(&format!("▶ {label}"));
                let res = run_one(RunOne {
                    suite,
                    task,
                    variant: v,
                    repeat: r,
                    env,
                    profile: &profile,
                    sink: &sink,
                    run_id: &run_id,
                    work: &work,
                    label: &label,
                    transcripts: transcripts.as_deref(),
                    keep: opts.keep,
                })
                .await;
                say(&format!(
                    "  {} {label}: {} ({} calls, {} tokens){}",
                    match res.outcome {
                        Outcome::Pass => "✓",
                        Outcome::Fail => "✗",
                        Outcome::Error => "!",
                        Outcome::Stopped => "■",
                    },
                    res.outcome.as_str(),
                    res.totals.calls,
                    res.totals.tokens(),
                    res.stopped_early
                        .as_deref()
                        .map(|w| format!(" — {w}"))
                        .unwrap_or_default()
                ));
                results.push(res);
            }
        }
    }
    if !opts.keep {
        let _ = std::fs::remove_dir(&work);
        if let Some(parent) = work.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
    let not_run = planned - results.len();
    let (judge, self_judged) = env.judge();
    let rubrics = tasks.iter().any(|t| t.grade.rubric.is_some());
    Ok(SuiteRun {
        judge: rubrics.then(|| judge.label()),
        self_judged: rubrics && self_judged,
        run_id,
        suite: suite.name.clone(),
        kind: suite.kind.as_str().into(),
        provider: env.provider_name.clone(),
        model: env.model.clone(),
        context_window: profile.context_window,
        totals: sink.suite_totals(),
        budget_stop: sink.exceeded(),
        not_run,
        results,
    })
}

fn run_label(task: &str, v: Variant, r: u32, repeat: u32) -> String {
    if repeat > 1 {
        format!("{task}--{v}--{r}")
    } else {
        format!("{task}--{v}")
    }
}

struct RunOne<'a> {
    suite: &'a Suite,
    task: &'a Task,
    variant: Variant,
    repeat: u32,
    env: &'a Env,
    profile: &'a HarnessProfile,
    sink: &'a Arc<EvalSink>,
    run_id: &'a str,
    work: &'a std::path::Path,
    label: &'a str,
    transcripts: Option<&'a std::path::Path>,
    keep: bool,
}

#[derive(Default)]
struct Seen {
    iterations: usize,
    truncations: u32,
    compactions: u32,
    verify_failures: u32,
}

async fn run_one(p: RunOne<'_>) -> TaskResult {
    let started = Instant::now();
    let tag = EvalTag {
        run_id: p.run_id.into(),
        suite: p.suite.name.clone(),
        kind: p.suite.kind.as_str().into(),
        task: p.task.id.clone(),
        variant: p.variant.as_str().into(),
        repeat: p.repeat,
        result: None,
    };
    let mut result = TaskResult {
        task: p.task.id.clone(),
        variant: p.variant,
        repeat: p.repeat,
        outcome: Outcome::Error,
        graders: vec![],
        totals: Totals::default(),
        iterations: 0,
        wall_ms: 0,
        truncations: 0,
        compactions: 0,
        verify_failures: 0,
        stopped_early: None,
        fingerprint: p.task.fingerprint.clone(),
        context_window: p.profile.context_window,
        ferrule_version: env!("CARGO_PKG_VERSION").into(),
    };

    let fixture = match Fixture::prepare(p.work, p.label, p.task, &p.env.sandbox, p.keep).await {
        Ok(f) => f,
        Err(e) => {
            result.stopped_early = Some(format!("fixture: {e:#}"));
            return finish(p, tag, result, started);
        }
    };
    let before = p
        .task
        .grade
        .rubric
        .is_some()
        .then(|| rubric::Snapshot::take(&fixture.workspace));
    let transcript = p
        .transcripts
        .and_then(|dir| Transcript::create(dir, p.label).ok());
    let stop = StopFlag::new();
    let agent = variant::build(variant::Build {
        variant: p.variant,
        provider: p.env.provider.clone(),
        profile: p.profile,
        sandbox: &p.env.sandbox,
        memory_tools: p.env.memory_tools.as_ref(),
        workspace: &fixture.workspace,
        state: &fixture.state,
        max_iterations: p.task.max_iterations(p.suite),
        check: p.task.check.as_deref(),
        transcript,
    });
    let mut agent = agent
        .with_ledger(
            p.sink.clone() as Arc<dyn LedgerSink>,
            "eval",
            None,
            p.env.model.clone(),
        )
        .with_stop_flag(stop.clone());
    p.sink.begin(tag.clone(), stop);

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let watcher = tokio::spawn(async move {
        let mut seen = Seen::default();
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::Truncated { .. } => seen.truncations += 1,
                AgentEvent::Compacted { .. } => seen.compactions += 1,
                AgentEvent::VerifyFinished { ok: false, .. } => seen.verify_failures += 1,
                AgentEvent::RunFinished { iterations, .. }
                | AgentEvent::RunIncomplete { iterations, .. } => seen.iterations = iterations,
                _ => {}
            }
        }
        seen
    });
    let timeout = Duration::from_secs(p.task.timeout_secs(p.suite));
    let ran = tokio::time::timeout(timeout, agent.run(&p.task.prompt, tx)).await;
    let incomplete = agent.incomplete.clone();
    drop(agent);
    let seen = watcher.await.unwrap_or_default();
    result.truncations = seen.truncations;
    result.compactions = seen.compactions;
    result.verify_failures = seen.verify_failures;
    // Totals are taken after grading, so they include the judge's call.
    let end = |result: &mut TaskResult| {
        result.totals = p.sink.end();
        result.iterations = seen.iterations.max(result.totals.turns as usize);
    };

    let answer = match ran {
        Err(_) => {
            result.stopped_early = Some(format!("timed out after {}s", timeout.as_secs()));
            None
        }
        Ok(Err(CoreError::Aborted(_))) if p.sink.exceeded().is_some() => {
            end(&mut result);
            result.outcome = Outcome::Stopped;
            result.stopped_early = p.sink.exceeded();
            return finish(p, tag, result, started);
        }
        Ok(Err(e)) => {
            // A provider that's down says nothing about the harness.
            end(&mut result);
            result.stopped_early = Some(format!("agent error: {e}"));
            return finish(p, tag, result, started);
        }
        Ok(Ok(answer)) => {
            result.stopped_early = incomplete;
            Some(answer)
        }
    };

    // Graded even after a timeout or an early stop: the work is what it is.
    let command = grade::command(p.task, &fixture.workspace, &p.env.sandbox).await;
    if let (Some(rubric), Some(before)) = (&p.task.grade.rubric, &before) {
        if let Some(why) = p.sink.exceeded() {
            // No budget left to ask the judge: no verdict either way.
            end(&mut result);
            result.outcome = Outcome::Stopped;
            result.stopped_early = Some(format!("not judged: {why}"));
            return finish(p, tag, result, started);
        }
        let bundle = rubric::bundle(
            before,
            &fixture.workspace,
            command.as_ref(),
            answer.as_deref(),
        );
        let (judge, _) = p.env.judge();
        let g = rubric::grade(
            &judge,
            p.sink,
            rubric::Ask {
                session_id: format!("{}/{}", p.run_id, p.label),
                iteration: seen.iterations,
                prompt: &p.task.prompt,
                rubric,
            },
            &bundle,
        )
        .await;
        result.graders.extend(command);
        result.graders.push(g);
    } else {
        result.graders.extend(command);
    }
    end(&mut result);
    result.outcome = if result.graders.iter().any(|g| g.error) {
        Outcome::Error
    } else if result.graders.iter().all(|g| g.passed) {
        Outcome::Pass
    } else {
        Outcome::Fail
    };
    drop(fixture);
    finish(p, tag, result, started)
}

fn finish(p: RunOne<'_>, mut tag: EvalTag, mut result: TaskResult, started: Instant) -> TaskResult {
    result.wall_ms = started.elapsed().as_millis() as u64;
    tag.result = serde_json::to_value(&result).ok();
    p.sink.write_uncounted(LedgerRecord {
        timestamp: chrono::Utc::now().to_rfc3339(),
        session_id: format!("{}/{}", p.run_id, p.label),
        task_shape: "eval".into(),
        origin: Some(format!("{}/{}", p.suite.name, p.task.id)),
        provider: p.env.provider_name.clone(),
        model: p.env.model.clone(),
        iteration: result.iterations,
        call_kind: RESULT_KIND.into(),
        input_tokens: 0,
        cached_input_tokens: 0,
        output_tokens: 0,
        tool_calls: 0,
        latency_ms: result.wall_ms,
        outcome: result.outcome.as_str().into(),
        error_kind: None,
        error_message: result.stopped_early.clone(),
        cost_usd: None,
        eval: Some(tag),
    });
    result
}
