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
    AgentEvent, CoreError, EvalTag, Guard, HarnessProfile, LedgerRecord, LedgerSink, Policy,
    Provider, Served, StopFlag, Tier, Tiered, Transcript,
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
    /// M16: the owner's rendered playbook. Only a suite with
    /// `owner_playbook = true` passes it to the engineered variant.
    pub playbook: Option<String>,
    /// M19: the owner's trust, for a suite with `owner_trust = true`. Given
    /// the tree (`eval:<run id>`) and a task run's sink, it returns the
    /// sink to write through and the guard to run under. `None`, or a
    /// suite that doesn't opt in: no guard, and rows without a tree, which
    /// the owner's meter skips.
    pub owner_trust: Option<OwnerTrust>,
    /// M25: the two models `--variant routing` compares. Without it the
    /// routing variants end in an error, not a verdict.
    pub routing: Option<Routing>,
}

/// One model a routing arm runs on.
#[derive(Clone)]
pub struct Arm {
    pub provider: Arc<dyn Provider>,
    pub provider_name: String,
    pub model: String,
    pub pricing: Option<Pricing>,
}

impl Arm {
    /// `provider/model`.
    pub fn reference(&self) -> String {
        format!("{}/{}", self.provider_name, self.model)
    }

    fn served(&self) -> Served {
        Served {
            provider: self.provider_name.clone(),
            model: self.model.clone(),
        }
    }
}

/// `--variant routing`: `cheap` runs on the cheap arm, `strong` on the
/// strong one, and `routed` starts each task on the cheap one and moves up
/// on a failure signal, under `policy` (every trigger, by default).
#[derive(Clone)]
pub struct Routing {
    pub cheap: Arm,
    pub strong: Arm,
    pub policy: Policy,
}

/// The routing pair a run compared, for the report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingPair {
    pub cheap: String,
    pub strong: String,
}

/// What one variant's agent talks to, and the model its rows default to.
struct Serving {
    provider: Arc<dyn Provider>,
    model: String,
}

impl Env {
    /// The provider and model a variant's verdict row names: for `routed`,
    /// the tier it starts on.
    fn names(&self, v: Variant) -> (String, String) {
        match (v, &self.routing) {
            (Variant::Cheap | Variant::Routed, Some(r)) => {
                (r.cheap.provider_name.clone(), r.cheap.model.clone())
            }
            (Variant::Strong, Some(r)) => (r.strong.provider_name.clone(), r.strong.model.clone()),
            _ => (self.provider_name.clone(), self.model.clone()),
        }
    }

    fn serving(&self, v: Variant) -> Option<Serving> {
        let arm = |a: &Arm| Serving {
            provider: a.provider.clone(),
            model: a.model.clone(),
        };
        match (v, &self.routing) {
            (Variant::Engineered | Variant::Naive, _) => Some(Serving {
                provider: self.provider.clone(),
                model: self.model.clone(),
            }),
            (_, None) => None,
            (Variant::Cheap, Some(r)) => Some(arm(&r.cheap)),
            (Variant::Strong, Some(r)) => Some(arm(&r.strong)),
            (Variant::Routed, Some(r)) => {
                // A fresh ladder per task run: nothing carries over.
                let tier = |a: &Arm| Tier {
                    name: a.reference(),
                    provider: a.provider.clone(),
                    served: a.served(),
                };
                Some(Serving {
                    provider: Arc::new(Tiered::new(
                        "routed",
                        vec![tier(&r.cheap), tier(&r.strong)],
                        r.policy.clone(),
                    )),
                    model: r.cheap.model.clone(),
                })
            }
        }
    }
}

pub type OwnerTrust =
    Arc<dyn Fn(&str, Arc<dyn LedgerSink>) -> (Arc<dyn LedgerSink>, Arc<dyn Guard>) + Send + Sync>;

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
    /// M25: why the routed variant moved up a tier, once per move.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub escalations: Vec<String>,
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
    /// M25: the models a `--variant routing` run compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<RoutingPair>,
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
    let routes = opts.variants.iter().any(|v| v.routing());
    let sink = match (&env.routing, routes) {
        // Each row at the price of the model that served it, and a model
        // without prices stays unpriced.
        (Some(r), true) => EvalSink::new(env.ledger.clone(), None, opts.caps)
            .price_model(&r.cheap.provider_name, &r.cheap.model, r.cheap.pricing)
            .price_model(&r.strong.provider_name, &r.strong.model, r.strong.pricing)
            .price_model(&env.provider_name, &env.model, env.pricing),
        _ => EvalSink::new(env.ledger.clone(), env.pricing, opts.caps),
    };
    let sink = Arc::new(sink);
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
        routing: env
            .routing
            .as_ref()
            .filter(|_| routes)
            .map(|r| RoutingPair {
                cheap: r.cheap.reference(),
                strong: r.strong.reference(),
            }),
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
    escalations: Vec<String>,
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
        escalations: vec![],
        fingerprint: p.task.fingerprint.clone(),
        context_window: p.profile.context_window,
        ferrule_version: env!("CARGO_PKG_VERSION").into(),
    };

    let Some(serving) = p.env.serving(p.variant) else {
        result.stopped_early = Some(format!(
            "the {} variant needs a cheap and a strong model (--cheap, --strong)",
            p.variant
        ));
        return finish(p, tag, result, started);
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
        provider: serving.provider,
        profile: p.profile,
        sandbox: &p.env.sandbox,
        memory_tools: p.env.memory_tools.as_ref(),
        workspace: &fixture.workspace,
        state: &fixture.state,
        max_iterations: p.task.max_iterations(p.suite),
        check: p.task.check.as_deref(),
        transcript,
        playbook: p
            .suite
            .owner_playbook
            .then_some(p.env.playbook.as_deref())
            .flatten(),
    });
    let mut sink = p.sink.clone() as Arc<dyn LedgerSink>;
    let mut owner_stop = None;
    if let (true, Some(owner)) = (p.suite.owner_trust, &p.env.owner_trust) {
        let (s, g) = owner(&format!("eval:{}", p.run_id), sink);
        sink = s;
        owner_stop = Some(Arc::new(OwnerStop {
            inner: g,
            why: std::sync::Mutex::new(None),
        }));
    }
    let mut agent = agent
        .with_ledger(sink, "eval", None, serving.model.clone())
        .with_stop_flag(stop.clone());
    if let Some(g) = &owner_stop {
        agent = agent.with_guard(g.clone() as Arc<dyn Guard>);
    }
    p.sink.begin(tag.clone(), stop);

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let watcher = tokio::spawn(async move {
        let mut seen = Seen::default();
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::Truncated { .. } => seen.truncations += 1,
                AgentEvent::Compacted { .. } => seen.compactions += 1,
                AgentEvent::VerifyFinished { ok: false, .. } => seen.verify_failures += 1,
                AgentEvent::Escalated { reason, .. } => seen.escalations.push(reason),
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
    result.escalations = seen.escalations.clone();
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
        Ok(Ok(_)) if owner_stop.as_ref().is_some_and(|g| g.why().is_some()) => {
            // The owner's cap or kill switch, not the task: no verdict.
            end(&mut result);
            result.outcome = Outcome::Stopped;
            result.stopped_early = owner_stop.and_then(|g| g.why());
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

/// The owner's guard over an eval task, remembering why it stopped the
/// run, if it did.
struct OwnerStop {
    inner: Arc<dyn Guard>,
    why: std::sync::Mutex<Option<String>>,
}

impl OwnerStop {
    fn why(&self) -> Option<String> {
        self.why.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn note(&self, why: &str) {
        self.why
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(|| why.to_string());
    }
}

#[async_trait::async_trait]
impl Guard for OwnerStop {
    fn begin(&self) {
        self.inner.begin()
    }

    fn before_model_call(&self) -> Option<String> {
        let why = self.inner.before_model_call();
        if let Some(w) = &why {
            self.note(w);
        }
        why
    }

    async fn before_tool_call(&self, call: ferrule_core::GuardedCall<'_>) -> ferrule_core::Verdict {
        self.inner.before_tool_call(call).await
    }

    async fn halted(&self) -> String {
        let why = self.inner.halted().await;
        self.note(&why);
        why
    }
}

fn finish(p: RunOne<'_>, mut tag: EvalTag, mut result: TaskResult, started: Instant) -> TaskResult {
    result.wall_ms = started.elapsed().as_millis() as u64;
    tag.result = serde_json::to_value(&result).ok();
    let (provider, model) = p.env.names(p.variant);
    p.sink.write_uncounted(LedgerRecord {
        timestamp: chrono::Utc::now().to_rfc3339(),
        session_id: format!("{}/{}", p.run_id, p.label),
        task_shape: "eval".into(),
        origin: Some(format!("{}/{}", p.suite.name, p.task.id)),
        provider,
        model,
        iteration: result.iterations,
        call_kind: RESULT_KIND.into(),
        input_tokens: 0,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
        output_tokens: 0,
        tool_calls: 0,
        latency_ms: result.wall_ms,
        outcome: result.outcome.as_str().into(),
        error_kind: None,
        error_message: result.stopped_early.clone(),
        cost_usd: None,
        eval: Some(tag),
        tree: None,
        route: None,
        speed: None,
    });
    result
}
