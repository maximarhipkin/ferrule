//! Evaluating a candidate model (docs/m24-dashboard-2.md §2): the starter
//! suite, or its smoke subset, run on a model the owner is considering,
//! with the cost estimated first, under the owner's M19 caps, stored as
//! `ferrule eval` stores a run, and compared with the default's last result
//! on the same tasks. The harness is M14's, unchanged: everything here is
//! the `Env` and `Options` it's handed. Nothing here touches the dashboard.

use crate::config::Config;
use crate::ledger::{FileLedgerSink, Prices, ProviderPricing};
use crate::models::{catalog, Catalog};
use anyhow::{anyhow, bail, Result};
use chrono::Utc;
use ferrule_core::{Guard, GuardedCall, LedgerRecord, LedgerSink, Verdict};
use ferrule_eval::{history, report, Caps, Env, Options, Outcome, Pricing, Suite, SuiteRun};
use ferrule_trust::{Hub, Route, TrustConfig, TrustGuard, TrustSink};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `ferrule eval`'s own defaults, the budget when the owner has no cap on.
const DEFAULT_USD: f64 = 5.0;
const DEFAULT_TOKENS: u64 = 20_000_000;

const TYPICAL: &str = include_str!("typical.json");

/// One task's token use on the mock model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Use {
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
    pub calls: u64,
}

impl Use {
    fn add(&mut self, o: &Use) {
        self.input += o.input;
        self.cached_input += o.cached_input;
        self.output += o.output;
        self.calls += o.calls;
    }

    pub fn tokens(&self) -> u64 {
        self.input + self.output
    }
}

#[derive(Deserialize)]
struct Table {
    tasks: BTreeMap<String, Use>,
}

/// The starter suite's typical use per task (`typical.json`).
pub fn typical() -> BTreeMap<String, Use> {
    serde_json::from_str::<Table>(TYPICAL)
        .expect("typical.json is checked by its test")
        .tasks
}

/// Which of the starter suite's tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Subset {
    /// The `smoke` tag: 4 quick tasks, one of each kind.
    Smoke,
    /// All 20.
    Starter,
}

impl Subset {
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim() {
            "smoke" | "" => Ok(Self::Smoke),
            "starter" | "all" => Ok(Self::Starter),
            other => bail!("--suite: `{other}` isn't smoke or starter"),
        }
    }

    fn tags(self) -> Vec<String> {
        match self {
            Self::Smoke => vec!["smoke".into()],
            Self::Starter => vec![],
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Smoke => "the starter suite's smoke subset",
            Self::Starter => "the whole starter suite",
        }
    }
}

/// The starter suite's directory: `[eval] suite`, `$FERRULE_EVAL_SUITE`,
/// `./evals/starter`, then the checkout the binary was built from.
pub fn find_suite(cfg: &Config) -> Result<PathBuf> {
    let has = |d: &Path| d.join("suite.toml").is_file();
    if let Some(d) = &cfg.eval.suite {
        if has(d) {
            return Ok(d.clone());
        }
        bail!(
            "[eval] suite = {:?} has no suite.toml",
            d.display().to_string()
        );
    }
    if let Some(d) = std::env::var_os("FERRULE_EVAL_SUITE").map(PathBuf::from) {
        if has(&d) {
            return Ok(d);
        }
        bail!("$FERRULE_EVAL_SUITE ({}) has no suite.toml", d.display());
    }
    let here = PathBuf::from("evals").join("starter");
    if has(&here) {
        return Ok(here);
    }
    let built = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/starter");
    if has(&built) {
        return Ok(built);
    }
    bail!(
        "the starter suite isn't here: set [eval] suite = \"<dir>\" to the evals/starter directory of a ferrule checkout"
    )
}

/// The model under evaluation.
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    /// `provider/model`.
    pub reference: String,
    pub provider: String,
    pub model: String,
    /// Connected in the config; otherwise a catalog model run through its
    /// provider for the eval only.
    pub connected: bool,
    pub pricing: Option<ProviderPricing>,
    /// "config", "catalog" or "none".
    pub price_from: &'static str,
    #[serde(skip)]
    profile: String,
    #[serde(skip)]
    context_window: Option<usize>,
}

/// `word` as a connected model (an alias, a provider, `provider/model`),
/// else `provider/<any id>` on a configured provider, else a catalog id on
/// the connected OpenRouter. `listings` prices a model the config has no
/// prices for.
pub fn candidate(
    cfg: &Config,
    word: &str,
    listings: Option<&[catalog::Listing]>,
) -> Result<Candidate> {
    let cat = Catalog::from_config(cfg);
    let mut c = match cat.resolve(word) {
        Ok(e) => Candidate {
            reference: e.reference(),
            provider: e.provider.clone(),
            model: e.model.clone(),
            connected: true,
            pricing: e.pricing,
            price_from: if e.pricing.is_some() {
                "config"
            } else {
                "none"
            },
            profile: e.profile.clone(),
            context_window: e.context_window,
        },
        Err(why) => {
            let word = word.trim();
            // `provider/model` on a configured provider, else a catalog id
            // (`qwen/qwen3-coder`) on the connected OpenRouter.
            let (p, m) = match word.split_once('/') {
                Some((p, m)) if cfg.providers.contains_key(p) => (p.to_string(), m),
                _ => match catalog::sources(cfg)
                    .into_iter()
                    .find(|s| s.provider.is_some() && s.is_openrouter())
                    .and_then(|s| s.provider)
                {
                    Some(p) if word.contains('/') => (p, word),
                    _ => bail!("{why}"),
                },
            };
            let pcfg = &cfg.providers[&p];
            if m.is_empty() {
                bail!("no model was named after `{p}/`");
            }
            let p = p.as_str();
            Candidate {
                reference: format!("{p}/{m}"),
                provider: p.to_string(),
                model: m.to_string(),
                connected: false,
                pricing: None,
                price_from: "none",
                profile: pcfg.profile.clone(),
                context_window: None,
            }
        }
    };
    if c.pricing.is_none() {
        if let Some(listings) = listings {
            let hit = listings
                .iter()
                .filter(|l| l.provider.as_deref() == Some(c.provider.as_str()))
                .chain(listings.iter().filter(|l| l.provider.is_none()))
                .find_map(|l| l.find(&c.model).and_then(|m| m.pricing));
            if let Some(p) = hit {
                c.pricing = Some(p);
                c.price_from = "catalog";
            }
        }
    }
    Ok(c)
}

/// A finished run in a line: the view's and the comparison's unit.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub run_id: String,
    pub reference: String,
    pub passed: usize,
    pub ran: usize,
    pub planned: usize,
    pub usd: Option<f64>,
    pub tokens: u64,
    /// Why the run stopped early, if it did.
    pub stopped: Option<String>,
    pub tasks: Vec<TaskLine>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskLine {
    pub task: String,
    pub outcome: String,
}

pub fn summary(run: &SuiteRun, planned: usize) -> Summary {
    let rows: Vec<_> = run
        .results
        .iter()
        .filter(|r| r.variant == ferrule_eval::Variant::Engineered)
        .collect();
    let stopped = run.budget_stop.clone().or_else(|| {
        rows.iter()
            .find(|r| r.outcome == Outcome::Stopped)
            .map(|r| r.stopped_early.clone().unwrap_or_else(|| "stopped".into()))
    });
    Summary {
        run_id: run.run_id.clone(),
        reference: format!("{}/{}", run.provider, run.model),
        passed: rows.iter().filter(|r| r.outcome == Outcome::Pass).count(),
        ran: rows.len(),
        planned,
        usd: run.totals.cost_usd,
        tokens: run.totals.tokens(),
        stopped,
        tasks: rows
            .iter()
            .map(|r| TaskLine {
                task: r.task.clone(),
                outcome: r.outcome.as_str().to_string(),
            })
            .collect(),
    }
}

/// The latest saved run of `suite` on `provider`/`model` whose engineered
/// tasks are exactly `tasks`.
pub fn baseline(
    data: &Path,
    suite: &str,
    tasks: &[String],
    provider: &str,
    model: &str,
) -> Option<Summary> {
    let want: BTreeSet<&str> = tasks.iter().map(String::as_str).collect();
    history::load_runs(&data.join("eval"))
        .iter()
        .rev()
        .find(|r| {
            let got: BTreeSet<&str> = r
                .results
                .iter()
                .filter(|t| t.variant == ferrule_eval::Variant::Engineered)
                .map(|t| t.task.as_str())
                .collect();
            r.suite == suite && r.provider == provider && r.model == model && got == want
        })
        .map(|r| summary(r, tasks.len()))
}

/// The eval's budget: the lesser of the per-run caps and what's left of
/// the day's; `ferrule eval`'s defaults when no cap is on.
#[derive(Debug, Clone, Serialize)]
pub struct Budget {
    pub max_usd: Option<f64>,
    pub max_tokens: Option<u64>,
    pub why: String,
}

impl Budget {
    pub fn caps(&self) -> Caps {
        Caps {
            max_usd: self.max_usd,
            max_tokens: self.max_tokens,
        }
    }
}

/// The budget, or why the eval can't start at all (the kill switch, a day
/// cap used up).
pub fn budget(trust: &TrustConfig, hub: &Hub) -> std::result::Result<Budget, String> {
    if let Some(s) = hub.stopped() {
        return Err(format!(
            "The kill switch is on (by {}, {}): nothing is sent to a model until it's off.",
            s.by, s.at
        ));
    }
    let today = match hub.today(None) {
        Ok((day, _)) => Some(day),
        Err(e) if trust.needs_ledger() => {
            return Err(format!("{e}, so the day caps can't be checked"))
        }
        Err(_) => None,
    };
    let mut why = Vec::new();
    let mut usd = None::<f64>;
    let mut tokens = None::<u64>;
    if trust.max_usd_per_run > 0.0 {
        usd = Some(trust.max_usd_per_run);
        why.push("the per-run cap");
    }
    if trust.max_tokens_per_run > 0 {
        tokens = Some(trust.max_tokens_per_run);
    }
    if let Some(day) = today {
        if trust.max_usd_per_day > 0.0 {
            let left = trust.max_usd_per_day - day.usd;
            if left <= 0.0 {
                return Err(format!(
                    "Today's ${:.2} cap is used up (${:.2} spent).",
                    trust.max_usd_per_day, day.usd
                ));
            }
            if usd.is_none_or(|u| left < u) {
                usd = Some(left);
                why.push("what's left of today's cap");
            }
        }
        if trust.max_tokens_per_day > 0 {
            let left = trust.max_tokens_per_day.saturating_sub(day.tokens);
            if left == 0 {
                return Err(format!(
                    "Today's {} token cap is used up.",
                    ferrule_trust::hub::thousands(trust.max_tokens_per_day)
                ));
            }
            if tokens.is_none_or(|t| left < t) {
                tokens = Some(left);
                if !why.contains(&"what's left of today's cap") {
                    why.push("what's left of today's cap");
                }
            }
        }
    }
    if usd.is_none() && tokens.is_none() {
        usd = Some(DEFAULT_USD);
        tokens = Some(DEFAULT_TOKENS);
        why.push("`ferrule eval`'s defaults, since no cap is on");
    }
    why.dedup();
    Ok(Budget {
        max_usd: usd,
        max_tokens: tokens,
        why: why.join(" and "),
    })
}

/// What running it would take, shown before the confirm.
#[derive(Debug, Clone, Serialize)]
pub struct Estimate {
    pub candidate: Candidate,
    pub subset: Subset,
    pub suite: String,
    #[serde(skip)]
    pub suite_dir: PathBuf,
    pub tasks: Vec<String>,
    pub typical: Use,
    pub tokens: u64,
    pub usd: Option<f64>,
    pub budget: Option<Budget>,
    /// Why it can't run now.
    pub refused: Option<String>,
    /// The default model, and its last result on these tasks.
    pub default: Option<String>,
    pub baseline: Option<Summary>,
    pub text: String,
}

/// The estimate for `c` on `subset`. `hub`: the owner's caps and kill
/// switch; `data`: where earlier runs are.
pub fn estimate(
    cfg: &Config,
    hub: &Hub,
    data: &Path,
    c: Candidate,
    subset: Subset,
) -> Result<Estimate> {
    let dir = find_suite(cfg)?;
    let suite = Suite::load(&dir)?;
    let tasks: Vec<String> = suite
        .select(&subset.tags(), &[])?
        .iter()
        .map(|t| t.id.clone())
        .collect();
    let table = typical();
    let mut total = Use::default();
    for t in &tasks {
        if let Some(u) = table.get(t) {
            total.add(u);
        }
    }
    let usd = c
        .pricing
        // The mock writes no cache, so neither does the typical use.
        .map(|p| pricing(&p).cost(total.input, total.cached_input, 0, total.output));
    let (budget, mut refused) = match budget(&hub.config().clone(), hub) {
        Ok(b) => (Some(b), None),
        Err(e) => (None, Some(e)),
    };
    if refused.is_none() && cfg.resolve_provider(Some(&c.provider)).is_err() {
        refused = Some(format!(
            "`{}` has no key here: run `ferrule setup`, or export it",
            c.provider
        ));
    }
    let cat = Catalog::from_config(cfg);
    let default = cat.default_entry().ok().map(|(e, _)| e.clone());
    let baseline = default
        .as_ref()
        .and_then(|d| baseline(data, &suite.name, &tasks, &d.provider, &d.model));
    let mut e = Estimate {
        candidate: c,
        subset,
        suite: suite.name.clone(),
        suite_dir: dir,
        tasks,
        typical: total,
        tokens: total.tokens(),
        usd,
        budget,
        refused,
        default: default.map(|d| d.reference()),
        baseline,
        text: String::new(),
    };
    e.text = render_estimate(&e);
    Ok(e)
}

fn pricing(p: &ProviderPricing) -> Pricing {
    Pricing {
        input: p.input,
        cached_input: p.cached_input,
        cache_write: p.cache_write,
        output: p.output,
    }
}

fn k(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

fn dollars(x: f64) -> String {
    if x < 0.01 && x > 0.0 {
        format!("${x:.4}")
    } else {
        format!("${x:.2}")
    }
}

/// The estimate as the page's question and the CLI's preamble.
pub fn render_estimate(e: &Estimate) -> String {
    let c = &e.candidate;
    let mut out = format!(
        "Evaluate {} on {} ({} task{}, ferrule's harness).\n",
        c.reference,
        e.subset.label(),
        e.tasks.len(),
        if e.tasks.len() == 1 { "" } else { "s" }
    );
    if !c.connected {
        out.push_str(&format!(
            "It isn't connected: the eval runs it through `{}` without adding it.\n",
            c.provider
        ));
    }
    let tokens = format!(
        "about {} tokens ({} in, {} out)",
        k(e.tokens),
        k(e.typical.input),
        k(e.typical.output)
    );
    match (e.usd, &c.pricing) {
        (Some(usd), Some(p)) => out.push_str(&format!(
            "Estimate: {tokens} ≈ {} at the {} prices (${}/${} per 1M in/out).\n",
            dollars(usd),
            if c.price_from == "catalog" { "catalog's" } else { "configured" },
            p.input,
            p.output
        )),
        _ => out.push_str(&format!(
            "Estimate: {tokens}. This model has no prices, so the dollar cap can't see its spend; only the token cap applies.\n"
        )),
    }
    out.push_str(
        "That's the mock model's use on these tasks: a real model takes more turns, often several times more.\n",
    );
    if let Some(b) = &e.budget {
        let mut parts = Vec::new();
        if let Some(u) = b.max_usd {
            parts.push(dollars(u));
        }
        if let Some(t) = b.max_tokens {
            parts.push(format!("{} tokens", k(t)));
        }
        out.push_str(&format!(
            "Budget: it stops at {} ({}).\n",
            parts.join(" or "),
            b.why
        ));
    }
    match (&e.default, &e.baseline) {
        (Some(d), Some(b)) => out.push_str(&format!(
            "The default, {d}, passed {}/{} on these tasks last time (run {}{}).\n",
            b.passed,
            b.planned,
            b.run_id,
            b.usd
                .map(|u| format!(", {}", dollars(u)))
                .unwrap_or_default()
        )),
        (Some(d), None) => out.push_str(&format!(
            "The default, {d}, has no saved run on these tasks to compare with.\n"
        )),
        _ => {}
    }
    if let Some(r) = &e.refused {
        out.push_str(&format!("It can't run now: {r}\n"));
    }
    out
}

/// A finished eval.
#[derive(Debug, Clone, Serialize)]
pub struct Finished {
    pub summary: Summary,
    pub baseline: Option<Summary>,
    /// The saved `report.txt`.
    pub report: String,
    #[serde(skip)]
    pub dir: PathBuf,
}

/// What a run needs: the config, where data goes, the owner's hub.
pub struct Setup {
    pub cfg: Config,
    pub data: PathBuf,
    pub hub: Arc<Hub>,
    pub estimate: Estimate,
    /// Who asked, for the audit log.
    pub by: String,
}

/// Runs the estimate's tasks on the candidate, engineered variant, and
/// saves the run where `ferrule eval run` saves one.
pub async fn run(
    s: Setup,
    progress: Arc<dyn Fn(&str) + Send + Sync>,
    cancel: Arc<AtomicBool>,
) -> Result<Finished> {
    let e = &s.estimate;
    if let Some(r) = &e.refused {
        bail!("{r}");
    }
    let budget = e
        .budget
        .clone()
        .ok_or_else(|| anyhow!("no budget was worked out"))?;
    let c = &e.candidate;
    let mut suite = Suite::load(&e.suite_dir)?;
    // The owner's spend (M19): every task runs under the owner's guard and
    // its rows count toward the day, under the tree `eval:<run id>`.
    suite.owner_trust = true;
    let (_, pcfg, key) = s.cfg.resolve_provider(Some(&c.provider))?;
    let provider = Arc::new(ferrule_providers::OpenAiCompatProvider::new(
        c.provider.clone(),
        &pcfg.base_url,
        key,
        &c.model,
    ));
    let sandbox = Arc::new(
        ferrule_sandbox::Sandbox::new(crate::sandbox_policy(&s.cfg)).map_err(|e| anyhow!(e))?,
    );
    let prices = prices_with(&s.cfg, c);
    let ledger: Arc<dyn LedgerSink> = Arc::new(FileLedgerSink::new(
        s.data.join("ledger.jsonl"),
        prices.clone(),
    ));
    let mut profile = ferrule_core::HarnessProfile::by_name(&c.profile);
    if let Some(w) = c.context_window {
        profile.context_window = w;
    }
    let hub = s.hub.clone();
    let stop = cancel.clone();
    let owner: ferrule_eval::OwnerTrust = Arc::new(move |tree: &str, sink| {
        let p = prices.clone();
        let price: ferrule_trust::Pricer =
            Arc::new(move |r: &LedgerRecord| p(&r.provider, &r.model).map(|p| p.cost_usd(r)));
        let sink = Arc::new(TrustSink::new(sink, hub.clone(), tree, Some(price)));
        let guard = TrustGuard::root(
            hub.clone(),
            tree,
            Route::Unattended("this is a model eval, which runs unattended".into()),
        );
        (
            sink as Arc<dyn LedgerSink>,
            Arc::new(Cancellable {
                inner: Arc::new(guard),
                cancel: stop.clone(),
            }) as Arc<dyn Guard>,
        )
    });
    let env = Env {
        provider,
        provider_name: c.provider.clone(),
        model: c.model.clone(),
        profile,
        sandbox,
        memory_tools: Some(Arc::new(crate::memory_tools::tools)),
        ledger: Some(ledger),
        pricing: c.pricing.as_ref().map(pricing),
        transcripts: Some(s.data.join("eval")),
        judge: None,
        playbook: None,
        owner_trust: Some(owner),
        routing: None,
    };
    let opts = Options {
        variants: vec![ferrule_eval::Variant::Engineered],
        tags: e.subset.tags(),
        tasks: vec![],
        repeat: 1,
        context_window: None,
        caps: budget.caps(),
        keep: false,
        work_root: None,
        progress: Some(progress),
        edit_tools: Default::default(),
    };
    s.hub.audit().record(
        Utc::now(),
        "model.eval",
        None,
        None,
        json!({"model": c.reference, "suite": e.suite, "subset": e.subset,
               "estimate_usd": e.usd, "estimate_tokens": e.tokens,
               "max_usd": budget.max_usd, "max_tokens": budget.max_tokens,
               "by": s.by, "phase": "start"}),
    );
    let earlier = history::load_runs(&s.data.join("eval"));
    let run = ferrule_eval::run_suite(&suite, &env, &opts).await?;
    let text = format!(
        "{}{}",
        report::render(&run),
        history::render(&history::diffs(&earlier, &run))
    );
    let dir = s.data.join("eval").join(&run.run_id);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("report.txt"), &text)?;
    std::fs::write(dir.join("run.json"), serde_json::to_string_pretty(&run)?)?;
    let sum = summary(&run, e.tasks.len());
    s.hub.audit().record(
        Utc::now(),
        "model.eval",
        None,
        Some(&run.run_id),
        json!({"model": c.reference, "suite": e.suite, "subset": e.subset,
               "passed": sum.passed, "planned": sum.planned, "usd": sum.usd,
               "tokens": sum.tokens, "stopped": sum.stopped, "by": s.by, "phase": "done"}),
    );
    let baseline = e.baseline.clone();
    Ok(Finished {
        summary: sum,
        baseline,
        report: text,
        dir,
    })
}

/// The config's prices, with the candidate's own for its calls (a catalog
/// model isn't in the config, and its provider's would be wrong).
fn prices_with(cfg: &Config, c: &Candidate) -> Prices {
    let cat = Catalog::from_config(cfg);
    let (p, m, own) = (c.provider.clone(), c.model.clone(), c.pricing);
    Arc::new(move |provider: &str, model: &str| {
        if provider == p && model == m {
            own
        } else {
            cat.price(provider, model)
        }
    })
}

/// The owner's guard, plus the page's Cancel.
struct Cancellable {
    inner: Arc<dyn Guard>,
    cancel: Arc<AtomicBool>,
}

const CANCELLED: &str = "Cancelled by the owner.";

#[async_trait::async_trait]
impl Guard for Cancellable {
    fn begin(&self) {
        self.inner.begin()
    }

    fn before_model_call(&self) -> Option<String> {
        if self.cancel.load(Ordering::SeqCst) {
            return Some(CANCELLED.into());
        }
        self.inner.before_model_call()
    }

    async fn before_tool_call(&self, call: GuardedCall<'_>) -> Verdict {
        self.inner.before_tool_call(call).await
    }

    async fn halted(&self) -> String {
        let cancel = self.cancel.clone();
        let cancelled = async move {
            while !cancel.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            CANCELLED.to_string()
        };
        tokio::select! {
            why = self.inner.halted() => why,
            why = cancelled => why,
        }
    }
}

// ---- One eval at a time, in the background -------------------------------

/// This process's candidate eval, if one ran: the dashboard's.
#[derive(Default)]
pub struct Jobs {
    current: Mutex<Option<Arc<Job>>>,
}

pub struct Job {
    cancel: Arc<AtomicBool>,
    state: Mutex<JobState>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct JobState {
    model: String,
    subset: Option<Subset>,
    started_ms: i64,
    planned: usize,
    done: usize,
    running: bool,
    /// The task running now.
    current: Option<String>,
    /// The harness's progress lines, the last 40.
    lines: Vec<String>,
    cancelled: bool,
    finished: Option<Finished>,
    error: Option<String>,
}

impl Jobs {
    /// The running or last eval.
    pub fn view(&self) -> Value {
        match self.current.lock().unwrap().as_ref() {
            Some(j) => json!({ "job": j.state.lock().unwrap().clone() }),
            None => json!({ "job": null }),
        }
    }

    pub fn running(&self) -> bool {
        self.current
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|j| j.state.lock().unwrap().running)
    }

    /// Starts `s` on its own thread with its own runtime, so the graders
    /// and the agent loop can't stall the gateway's. `Err`: one is running.
    pub fn start(&self, s: Setup) -> std::result::Result<(), Value> {
        let mut slot = self.current.lock().unwrap();
        if let Some(j) = slot.as_ref() {
            if j.state.lock().unwrap().running {
                return Err(json!({ "job": j.state.lock().unwrap().clone() }));
            }
        }
        let job = Arc::new(Job {
            cancel: Arc::new(AtomicBool::new(false)),
            state: Mutex::new(JobState {
                model: s.estimate.candidate.reference.clone(),
                subset: Some(s.estimate.subset),
                started_ms: Utc::now().timestamp_millis(),
                planned: s.estimate.tasks.len(),
                running: true,
                ..Default::default()
            }),
        });
        *slot = Some(job.clone());
        drop(slot);
        let j = job.clone();
        let progress: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |line: &str| {
            let mut st = j.state.lock().unwrap();
            if let Some(task) = line.strip_prefix("▶ ") {
                st.current = Some(task.to_string());
            } else if line.trim_start().starts_with(['✓', '✗', '!', '■']) {
                st.done += 1;
                st.current = None;
            }
            st.lines.push(line.to_string());
            let over = st.lines.len().saturating_sub(40);
            st.lines.drain(..over);
        });
        let spawned = std::thread::Builder::new()
            .name("model-eval".into())
            .spawn(move || {
                let out = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .map_err(anyhow::Error::from)
                    .and_then(|rt| rt.block_on(run(s, progress, job.cancel.clone())));
                let mut st = job.state.lock().unwrap();
                st.running = false;
                st.current = None;
                match out {
                    Ok(f) => st.finished = Some(f),
                    Err(e) => st.error = Some(format!("{e:#}")),
                }
            });
        if let Err(e) = spawned {
            let slot = self.current.lock().unwrap();
            if let Some(j) = slot.as_ref() {
                let mut st = j.state.lock().unwrap();
                st.running = false;
                st.error = Some(format!("couldn't start the eval's thread: {e}"));
            }
        }
        Ok(())
    }

    /// Asks the running eval to stop; false when none runs.
    pub fn cancel(&self) -> bool {
        let slot = self.current.lock().unwrap();
        match slot.as_ref() {
            Some(j) if j.state.lock().unwrap().running => {
                j.cancel.store(true, Ordering::SeqCst);
                j.state.lock().unwrap().cancelled = true;
                true
            }
            _ => false,
        }
    }
}

/// `ferrule model eval`: the estimate, the confirm, the run in the
/// foreground with its progress on stderr, the report and the comparison.
pub async fn cli(word: &str, subset: &str, yes: bool) -> Result<()> {
    use std::io::{IsTerminal, Write};
    let subset = Subset::parse(subset)?;
    let (cfg, _) = Config::load()?;
    let hub = crate::trust::hub(&cfg)?;
    let data = crate::config::data_dir()?;
    let mut c = candidate(&cfg, word, None)?;
    if c.pricing.is_none() {
        if let Ok((_, listings)) = catalog::listings(false).await {
            c = candidate(&cfg, word, Some(&listings))?;
        }
    }
    let e = estimate(&cfg, &hub, &data, c, subset)?;
    eprint!("{}", e.text);
    if let Some(r) = &e.refused {
        bail!("{r}");
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("it spends money: add --yes to run it without a terminal");
        }
        eprint!("Run it? [y/N] ");
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            eprintln!("Not run.");
            return Ok(());
        }
    }
    let f = run(
        Setup {
            cfg,
            data,
            hub,
            estimate: e,
            by: "cli".into(),
        },
        Arc::new(|line: &str| eprintln!("{line}")),
        Arc::new(AtomicBool::new(false)),
    )
    .await?;
    print!("\n{}", f.report);
    println!("{}", compare(&f));
    println!("transcripts and this report: {}", f.dir.display());
    if f.summary.stopped.is_some() {
        std::process::exit(3);
    }
    Ok(())
}

/// The candidate's result next to the default's last one.
pub fn compare(f: &Finished) -> String {
    let s = &f.summary;
    let line = |x: &Summary| {
        format!(
            "{}/{} pass, {} tokens{}",
            x.passed,
            x.planned,
            k(x.tokens),
            x.usd
                .map(|u| format!(", {}", dollars(u)))
                .unwrap_or_default()
        )
    };
    let mut out = format!("{}: {}", s.reference, line(s));
    if let Some(w) = &s.stopped {
        out.push_str(&format!(" (stopped: {w})"));
    }
    match &f.baseline {
        Some(b) => out.push_str(&format!(
            "\nthe default, {}: {} (run {})",
            b.reference,
            line(b),
            b.run_id
        )),
        None => out.push_str("\nthe default has no saved run on these tasks to compare with"),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_typical_table_covers_every_starter_task() {
        let t = typical();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/starter");
        let suite = Suite::load(&dir).unwrap();
        for task in &suite.tasks {
            let u = t
                .get(&task.id)
                .unwrap_or_else(|| panic!("{} missing", task.id));
            assert!(u.calls > 0 && u.input > 0, "{}: {u:?}", task.id);
        }
        assert_eq!(t.len(), suite.tasks.len());
        assert_eq!(suite.select(&["smoke".into()], &[]).unwrap().len(), 4);
    }

    fn hub(dir: &Path, trust: TrustConfig) -> Hub {
        Hub::new(
            trust,
            dir,
            &dir.join("ledger.jsonl"),
            Arc::new(ferrule_trust::SystemClock),
            vec![],
        )
        .unwrap()
    }

    #[test]
    fn the_budget_is_the_lesser_of_the_run_cap_and_what_is_left_today() {
        let dir = tempfile::tempdir().unwrap();
        let trust = TrustConfig {
            max_usd_per_run: 2.0,
            max_usd_per_day: 3.0,
            max_tokens_per_run: 0,
            max_tokens_per_day: 0,
            ..TrustConfig::off()
        };
        let h = hub(dir.path(), trust.clone());
        let b = budget(&trust, &h).unwrap();
        assert_eq!(b.max_usd, Some(2.0));
        // $2.50 spent today by the owner: $0.50 left.
        let row = json!({"timestamp": Utc::now().to_rfc3339(), "session_id": "s",
            "task_shape": "chat", "provider": "p", "model": "m", "iteration": 0,
            "input_tokens": 10, "cached_input_tokens": 0, "output_tokens": 1, "tool_calls": 0, "latency_ms": 1,
            "outcome": "ok", "cost_usd": 2.5});
        std::fs::write(dir.path().join("ledger.jsonl"), format!("{row}\n")).unwrap();
        let b = budget(&trust, &h).unwrap();
        assert!((b.max_usd.unwrap() - 0.5).abs() < 1e-9, "{b:?}");
        assert!(b.why.contains("today"), "{b:?}");
        let row2 = json!({"timestamp": Utc::now().to_rfc3339(), "session_id": "s",
            "task_shape": "chat", "provider": "p", "model": "m", "iteration": 0,
            "input_tokens": 10, "cached_input_tokens": 0, "output_tokens": 1, "tool_calls": 0, "latency_ms": 1,
            "outcome": "ok", "cost_usd": 1.0});
        std::fs::write(dir.path().join("ledger.jsonl"), format!("{row}\n{row2}\n")).unwrap();
        let e = budget(&trust, &h).unwrap_err();
        assert!(e.contains("used up"), "{e}");
        // No cap at all: `ferrule eval`'s defaults.
        let off = TrustConfig::off();
        let b = budget(&off, &hub(dir.path(), off.clone())).unwrap();
        assert_eq!((b.max_usd, b.max_tokens), (Some(5.0), Some(20_000_000)));
    }

    #[test]
    fn the_kill_switch_refuses_an_eval() {
        let dir = tempfile::tempdir().unwrap();
        let off = TrustConfig::off();
        let h = hub(dir.path(), off.clone());
        h.engage("test", None).unwrap();
        let e = budget(&off, &h).unwrap_err();
        assert!(e.contains("kill switch"), "{e}");
    }
    fn cfg() -> Config {
        toml::from_str(
            r#"default_provider = "a"

[providers.a]
base_url = "http://127.0.0.1:9/v1"
api_key_env = "A_KEY"
model = "a-one"
price_input_per_mtok = 1.0
price_cached_input_per_mtok = 0.1
price_output_per_mtok = 4.0

[providers.or]
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OR_KEY"
model = "openai/gpt-5.2-mini"
"#,
        )
        .unwrap()
    }

    fn listing(provider: Option<&str>, id: &str, input: f64) -> catalog::Listing {
        catalog::Listing {
            source: "t".into(),
            provider: provider.map(str::to_string),
            from: "cache",
            fetched_at: None,
            error: None,
            models: vec![catalog::Listed {
                id: id.into(),
                name: None,
                context: None,
                pricing: Some(ProviderPricing {
                    input,
                    cached_input: input / 10.0,
                    cache_write: None,
                    output: input * 4.0,
                }),
                tools: Some(true),
                free: false,
            }],
        }
    }

    #[test]
    fn a_candidate_is_a_connected_model_a_model_on_a_provider_or_a_catalog_id() {
        let cfg = cfg();
        let c = candidate(&cfg, "a", None).unwrap();
        assert_eq!((c.reference.as_str(), c.connected), ("a/a-one", true));
        assert_eq!((c.price_from, c.pricing.unwrap().output), ("config", 4.0));
        // Not connected: on its provider, for the eval only, unpriced...
        let c = candidate(&cfg, "a/a-new", None).unwrap();
        assert_eq!((c.provider.as_str(), c.model.as_str()), ("a", "a-new"));
        assert!(!c.connected);
        // ...unless a catalog prices it; the provider's own list first.
        let lists = [
            listing(None, "a-new", 9.0),
            listing(Some("a"), "a-new", 2.0),
        ];
        let c = candidate(&cfg, "a/a-new", Some(&lists)).unwrap();
        assert_eq!((c.price_from, c.pricing.unwrap().input), ("catalog", 2.0));
        // A catalog id with its own slash goes to the connected OpenRouter.
        let lists = [listing(None, "qwen/qwen3-coder", 0.3)];
        let c = candidate(&cfg, "qwen/qwen3-coder", Some(&lists)).unwrap();
        assert_eq!(c.reference, "or/qwen/qwen3-coder");
        assert_eq!(c.pricing.unwrap().input, 0.3);
        assert!(candidate(&cfg, "nothing-like-it", None).is_err());
    }

    struct Open;

    #[async_trait::async_trait]
    impl Guard for Open {
        fn before_model_call(&self) -> Option<String> {
            None
        }
        async fn before_tool_call(&self, _: GuardedCall<'_>) -> Verdict {
            Verdict::Allow
        }
        async fn halted(&self) -> String {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn cancel_stops_the_next_call_and_halts_the_one_running() {
        let cancel = Arc::new(AtomicBool::new(false));
        let g = Arc::new(Cancellable {
            inner: Arc::new(Open),
            cancel: cancel.clone(),
        });
        assert_eq!(g.before_model_call(), None);
        let running = tokio::spawn({
            let g = g.clone();
            async move { g.halted().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!running.is_finished());
        cancel.store(true, Ordering::SeqCst);
        assert_eq!(g.before_model_call().as_deref(), Some(CANCELLED));
        let why = tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(why, CANCELLED);
    }
}
