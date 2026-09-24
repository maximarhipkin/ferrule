//! M16's learning loop in the CLI (`docs/m16-learning-loop.md`): the
//! `[learning]` config, `ferrule learn run|show|diff|revert`, the built-in
//! `ferrule-learn` scheduler job, the `[Playbook]` prompt section, and the
//! episodes only the CLI can see (scheduled-task runs in `tasks.db`).

use crate::config::{self, Config};
use crate::ledger;
use anyhow::{Context as _, Result};
use clap::Subcommand;
use ferrule_core::{HarnessProfile, LedgerRecord};
use ferrule_gateway::{
    ensure_builtin, BuiltinJob, BuiltinSpec, Ensured, JobReport, RunStatus, Scheduler, Task,
    TaskStore, BUILTIN_CHANNEL, SCHEDULER_PSEUDO_CHANNEL,
};
use ferrule_learn::files::{Change, DONE};
use ferrule_learn::{Caps, Episode, LearnDir, Options, PassRecord, Spent, WorkspaceGate};
use ferrule_providers::OpenAiCompatProvider;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The built-in task's name in `ferrule tasks list`.
pub const TASK_NAME: &str = "ferrule-learn";
/// The most characters of lessons a system prompt gets.
pub const PROMPT_CHARS: usize = 4000;
/// Runs of one task looked at when finding its latest failure.
const RUNS_PER_TASK: usize = 50;

/// `[learning]`. Off by default: the pass spends money unattended and
/// changes every future agent's system prompt, so the owner turns it on
/// (§10). A playbook written by hand is injected either way.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LearningConfig {
    /// Register the nightly pass on the scheduler.
    pub enabled: bool,
    pub schedule: String,
    pub timezone: String,
    /// Put the playbook's lessons in system prompts.
    pub playbook: bool,
    /// A `[providers]` name for the pass; unset = the gateway's.
    pub provider: Option<String>,
    /// What the gate re-runs; unset = `[agent] verify_command`.
    pub check: Option<String>,
    pub max_usd_per_pass: f64,
    pub max_usd_per_day: f64,
    pub max_tokens_per_pass: u64,
    pub max_tokens_per_day: u64,
    pub max_episodes: usize,
    pub max_bullets: usize,
    pub max_clusters: usize,
    pub gate_max_iterations: usize,
    pub gate_timeout_secs: u64,
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            schedule: "0 3 * * *".into(),
            timezone: "UTC".into(),
            playbook: true,
            provider: None,
            check: None,
            max_usd_per_pass: 0.5,
            max_usd_per_day: 1.0,
            max_tokens_per_pass: 300_000,
            max_tokens_per_day: 1_000_000,
            max_episodes: 5,
            max_bullets: 40,
            max_clusters: 5,
            gate_max_iterations: 20,
            gate_timeout_secs: 900,
        }
    }
}

impl LearningConfig {
    fn caps(&self) -> Caps {
        Caps {
            usd_per_pass: self.max_usd_per_pass,
            usd_per_day: self.max_usd_per_day,
            tokens_per_pass: self.max_tokens_per_pass,
            tokens_per_day: self.max_tokens_per_day,
        }
    }
}

#[derive(Subcommand)]
pub enum LearnCmd {
    /// Run one learning pass now (works while [learning] is off)
    Run {
        /// Show what the pass would review and its caps; no calls, no writes
        #[arg(long)]
        dry_run: bool,
        /// The workspace the gate copies and re-runs failed goals in
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        #[arg(long)]
        provider: Option<String>,
    },
    /// The playbook as prompts see it, the last passes, and whether learning is on
    Show,
    /// A pass's playbook diff and memory merges (default: the latest that changed something)
    Diff { pass: Option<String> },
    /// Undo a pass's playbook changes and memory merges
    Revert {
        /// A pass id, or `last`
        pass: String,
    },
}

/// The `[Playbook]` section for a system prompt, if there are lessons and
/// `[learning] playbook` is on.
pub fn prompt_section(cfg: &LearningConfig) -> Option<String> {
    if !cfg.playbook {
        return None;
    }
    let dir = LearnDir::new(&config::data_dir().ok()?);
    let text = dir.read_playbook().ok()?;
    ferrule_learn::prompt_block(&text, cfg.max_bullets, PROMPT_CHARS).text
}

fn spec(cfg: &LearningConfig) -> BuiltinSpec {
    BuiltinSpec {
        name: TASK_NAME.into(),
        schedule: cfg.schedule.clone(),
        timezone: cfg.timezone.clone(),
        description: "[built-in] the learning pass (`ferrule learn show`)".into(),
    }
}

/// Registers the built-in job on `scheduler`. With `ensure`, first makes
/// `tasks.db` match `[learning]`: the task is added, moved or removed. A bad
/// schedule is reported and learning stays off; the gateway still starts.
pub fn register(
    cfg: &Config,
    scheduler: Scheduler,
    workspace: &Path,
    provider: Option<String>,
    ensure: bool,
) -> Scheduler {
    let learning = &cfg.learning;
    if ensure {
        let want = learning.enabled.then(|| spec(learning));
        match ensure_builtin(
            scheduler.store(),
            TASK_NAME,
            want.as_ref(),
            chrono::Utc::now(),
        ) {
            Ok(Ensured::Added(id)) => tracing::info!(task = %id, "learning pass scheduled"),
            Ok(Ensured::Updated(id)) => tracing::info!(task = %id, "learning pass rescheduled"),
            Ok(Ensured::Removed(_)) => tracing::info!("learning is off; its task was removed"),
            Ok(_) => {}
            Err(e) => {
                eprintln!("ferrule: [learning] schedule: {e}; the learning pass is off");
                return scheduler;
            }
        }
    }
    if !learning.enabled {
        return scheduler;
    }
    let workspace = workspace.canonicalize().unwrap_or(workspace.to_path_buf());
    scheduler.with_builtin(
        TASK_NAME,
        Arc::new(Job {
            workspace,
            provider: learning.provider.clone().or(provider),
        }),
    )
}

struct Job {
    workspace: PathBuf,
    provider: Option<String>,
}

#[async_trait::async_trait]
impl BuiltinJob for Job {
    async fn run(&self, _task: &Task) -> Result<JobReport, String> {
        let (cfg, _) = Config::load().map_err(|e| format!("{e:#}"))?;
        let p = pass(&cfg, &self.workspace, self.provider.clone(), "scheduled")
            .await
            .map_err(|e| format!("{e:#}"))?;
        Ok(JobReport {
            summary: summary(&p),
            incomplete: (p.status != DONE).then(|| p.status.clone()),
        })
    }
}

fn summary(p: &PassRecord) -> String {
    format!(
        "pass {}: {}, {} change(s), {} rejected, {} skipped, ${:.4} / {} tokens",
        p.id,
        p.status,
        p.changes.len(),
        p.rejected.len(),
        p.skipped.len(),
        p.spent.usd,
        p.spent.tokens
    )
}

fn options(cfg: &Config, workspace: &Path, trigger: &str) -> Options {
    let l = &cfg.learning;
    Options {
        caps: l.caps(),
        max_episodes: l.max_episodes,
        max_bullets: l.max_bullets,
        max_clusters: l.max_clusters,
        max_prompt_chars: PROMPT_CHARS,
        trigger: trigger.into(),
        workspace: Some(workspace.to_path_buf()),
    }
}

/// Everything a pass needs from this machine. Reads the ledger's last 24
/// hours first: a pass that can't know today's spend doesn't start.
fn env(cfg: &Config, workspace: &Path, provider: Option<String>) -> Result<ferrule_learn::Env> {
    let data = config::data_dir()?;
    let day_before = day_spent()?;
    let wanted = cfg.learning.provider.clone().or(provider);
    let (name, pcfg, key) = cfg.resolve_provider(wanted.as_deref())?;
    let model = Arc::new(OpenAiCompatProvider::new(
        name.clone(),
        &pcfg.base_url,
        key,
        &pcfg.model,
    ));
    let pricing = ledger::ProviderPricing::from_config(pcfg);
    let price: ferrule_learn::PriceFn =
        Arc::new(move |r: &LedgerRecord| pricing.map(|p| p.cost_usd(r)));
    let dir = LearnDir::new(&data);
    let cursor = dir.state()?.cursor;
    let gate = WorkspaceGate {
        provider: model.clone(),
        model: pcfg.model.clone(),
        profile: HarnessProfile::by_name(&pcfg.profile),
        sandbox: crate::shared_sandbox(cfg)?,
        workspace: workspace.to_path_buf(),
        check: cfg
            .learning
            .check
            .clone()
            .or_else(|| cfg.agent.verify_command.clone()),
        max_iterations: cfg.learning.gate_max_iterations,
        scratch_root: std::env::temp_dir(),
        skip: vec![data.clone()],
        run_timeout: Duration::from_secs(cfg.learning.gate_timeout_secs),
        check_timeout: Duration::from_secs(cfg.agent.verify_timeout_secs),
    };
    Ok(ferrule_learn::Env {
        dir,
        provider: model,
        provider_name: name,
        model: pcfg.model.clone(),
        ledger: ledger::build_sink(cfg),
        price,
        priced: pricing.is_some(),
        day_before,
        memory_db: Some(data.join("memory.db")),
        gate: Arc::new(gate),
        sessions_dir: Some(data.join("sessions")),
        task_episodes: task_episodes(&data, cursor)?,
    })
}

/// What passes spent in the rolling last 24 hours.
fn day_spent() -> Result<Spent> {
    let since = chrono::Utc::now() - chrono::Duration::hours(24);
    let (rows, _) = ledger::read_records(&ledger::ledger_path()?, Some(since))
        .context("the ledger can't be read, so today's learning spend is unknown; no pass")?;
    Ok(Spent::from_rows(&rows))
}

/// Scheduled-task runs that ended failed or incomplete after `cursor`: the
/// latest per task, `fixed` when a later run succeeded. Built-in tasks
/// (the pass itself) are never episodes.
pub fn task_episodes(data: &Path, cursor: i64) -> Result<Vec<Episode>> {
    let db = data.join("tasks.db");
    if !db.exists() {
        return Ok(Vec::new());
    }
    let store = TaskStore::open(&db)?;
    let mut out = Vec::new();
    for t in store.list()? {
        if t.channel == BUILTIN_CHANNEL {
            continue;
        }
        let runs = store.runs_for(&t.id, RUNS_PER_TASK)?;
        let at = |r: &ferrule_gateway::Run| r.finished_at.unwrap_or(r.started_at);
        let Some(i) = runs.iter().position(|r| {
            matches!(r.status, RunStatus::Failed | RunStatus::Incomplete) && at(r) > cursor
        }) else {
            continue;
        };
        let run = &runs[i];
        let session = ferrule_gateway::session::session_id(SCHEDULER_PSEUDO_CHANNEL, &t.id);
        let transcript = data.join("sessions").join(format!("{session}.jsonl"));
        out.push(Episode {
            key: format!("task:{}:{}", t.id, run.id),
            label: t.name.clone(),
            goal: t.prompt.clone(),
            outcome: run.status.as_str().into(),
            detail: run.detail.clone().unwrap_or_default(),
            fixed: runs[..i].iter().any(|r| r.status == RunStatus::Succeeded),
            transcript: transcript.exists().then_some(transcript),
            at: at(run),
        });
    }
    Ok(out)
}

async fn pass(
    cfg: &Config,
    workspace: &Path,
    provider: Option<String>,
    trigger: &str,
) -> Result<PassRecord> {
    let env = env(cfg, workspace, provider)?;
    ferrule_learn::run_pass(&options(cfg, workspace, trigger), &env).await
}

pub async fn cmd(op: LearnCmd) -> Result<()> {
    let data = config::data_dir()?;
    let dir = LearnDir::new(&data);
    match op {
        LearnCmd::Run {
            dry_run,
            workspace,
            provider,
        } => {
            let (cfg, _) = Config::load()?;
            let workspace = workspace.canonicalize().unwrap_or(workspace);
            if dry_run {
                let env = env(&cfg, &workspace, provider)?;
                print_plan(&ferrule_learn::plan(
                    &options(&cfg, &workspace, "manual"),
                    &env,
                )?);
                return Ok(());
            }
            let p = pass(&cfg, &workspace, provider, "manual").await?;
            println!("{}", summary(&p));
            for c in &p.changes {
                println!("  {}", change_line(c));
            }
            for r in &p.rejected {
                println!("  rejected {}: {}", r.op, r.reason);
            }
            println!(
                "details: {}",
                dir.pass_dir(&p.id).join("changelog.md").display()
            );
        }
        LearnCmd::Show => show(&dir)?,
        LearnCmd::Diff { pass } => diff(&dir, pass)?,
        LearnCmd::Revert { pass } => {
            let p = ferrule_learn::revert(&dir, &pass, Some(&data.join("memory.db")))?;
            println!("reverted pass {}", p.id);
            if let Some(r) = &p.reverted {
                for u in &r.undone {
                    println!("  undone: {u}");
                }
                for u in &r.not_undone {
                    println!("  not undone: {u}");
                }
            }
        }
    }
    Ok(())
}

fn print_plan(p: &ferrule_learn::Plan) {
    println!("dry run: no model calls, nothing written");
    println!(
        "episodes ({} now, {} left for later):",
        p.episodes.len(),
        p.later
    );
    for e in &p.episodes {
        let fixed = if e.fixed { ", fixed later" } else { "" };
        println!("  {} [{}{fixed}] {}", e.label, e.outcome, e.key);
    }
    println!(
        "memory clusters ({} now, {} left for later):",
        p.clusters.len(),
        p.later_clusters
    );
    for c in &p.clusters {
        let ids: Vec<String> = c.iter().map(|m| format!("#{}", m.id)).collect();
        println!("  {}", ids.join(", "));
    }
    println!(
        "check: {}",
        p.check
            .as_deref()
            .map_or("none — every add and edit is rejected".into(), |c| format!(
                "`{c}`"
            ))
    );
    println!(
        "caps: ${:.2}/pass, ${:.2}/day, {} tokens/pass, {} tokens/day; spent in the last 24h: ${:.4}, {} tokens",
        p.caps.usd_per_pass,
        p.caps.usd_per_day,
        p.caps.tokens_per_pass,
        p.caps.tokens_per_day,
        p.day_before.usd,
        p.day_before.tokens
    );
    if !p.priced {
        println!("the provider has no prices in config: only the token caps hold");
    }
}

fn change_line(c: &Change) -> String {
    match c {
        Change::Playbook {
            applied, reason, ..
        } => {
            let text = applied
                .new
                .as_deref()
                .or(applied.old.as_deref())
                .unwrap_or("");
            format!(
                "playbook {} pb-{}: {text} — {reason}",
                applied.op, applied.id
            )
        }
        Change::Memory {
            new_id,
            replaced,
            content,
            ..
        } => {
            let ids: Vec<String> = replaced.iter().map(|i| format!("#{i}")).collect();
            format!("memory {} → #{new_id}: {content}", ids.join(", "))
        }
    }
}

fn show(dir: &LearnDir) -> Result<()> {
    let (cfg, _) = Config::load()?;
    let l = &cfg.learning;
    if l.enabled {
        println!(
            "learning: on, `{}` ({}) as the `{TASK_NAME}` task",
            l.schedule, l.timezone
        );
    } else {
        println!("learning: off ([learning] enabled = false); `ferrule learn run` still works");
    }
    println!("playbook: {}", dir.playbook_path().display());
    let text = dir.read_playbook()?;
    if !l.playbook {
        println!("in prompts: no ([learning] playbook = false)");
    } else {
        let block = ferrule_learn::prompt_block(&text, l.max_bullets, PROMPT_CHARS);
        match &block.text {
            Some(t) => println!("in prompts:\n{t}"),
            None => println!("in prompts: nothing yet (no lessons)"),
        }
        if block.omitted > 0 {
            println!(
                "{} lesson(s) left out by the caps ({} lessons, {PROMPT_CHARS} characters)",
                block.omitted, l.max_bullets
            );
        }
    }
    let passes = dir.passes();
    if passes.is_empty() {
        println!("passes: none yet");
    } else {
        println!("passes (newest first):");
        for p in passes.iter().rev().take(10) {
            println!("  {}", summary(p));
        }
    }
    Ok(())
}

fn diff(dir: &LearnDir, pass: Option<String>) -> Result<()> {
    let id = match pass {
        Some(id) => id,
        None => dir
            .passes()
            .into_iter()
            .rev()
            .find(|p| !p.changes.is_empty())
            .map(|p| p.id)
            .ok_or_else(|| anyhow::anyhow!("no learning pass has changed anything yet"))?,
    };
    let p = dir.load_pass(&id)?;
    println!("pass {} ({})", p.id, p.status);
    let d = dir.read_pass_file(&id, "playbook.diff").unwrap_or_default();
    if d.is_empty() {
        println!("playbook: unchanged");
    } else {
        print!("{d}");
    }
    for c in p
        .changes
        .iter()
        .filter(|c| matches!(c, Change::Memory { .. }))
    {
        println!("{}", change_line(c));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_block_parses_and_matches_the_defaults() {
        let ex = config::EXAMPLE_CONFIG;
        let start = ex.find("# [learning]").unwrap();
        let block = &ex[start..start + ex[start..].find("\n\n").unwrap()];
        let toml_text: String = block
            .lines()
            .map(|l| format!("{}\n", l.strip_prefix("# ").unwrap_or(l)))
            .collect();
        let cfg: Config = toml::from_str(&toml_text).unwrap();
        let d = LearningConfig::default();
        let l = &cfg.learning;
        assert!(l.enabled, "the example shows how to turn it on");
        assert!(!d.enabled, "off by default");
        assert_eq!(
            (l.schedule.as_str(), l.max_usd_per_pass, l.max_usd_per_day),
            (d.schedule.as_str(), d.max_usd_per_pass, d.max_usd_per_day)
        );
        assert_eq!(l.playbook, d.playbook);
        let none: Config = toml::from_str("").unwrap();
        assert!(!none.learning.enabled && none.learning.playbook);
    }

    #[test]
    fn task_episodes_take_the_latest_failure_per_task_and_skip_the_builtin() {
        let d = tempfile::tempdir().unwrap();
        let store = TaskStore::open(d.path().join("tasks.db")).unwrap();
        let add = |name: &str, channel: &str| {
            let t = ferrule_gateway::NewTask {
                name: name.into(),
                kind: ferrule_gateway::TaskKind::Cron,
                schedule: "0 9 * * *".into(),
                timezone: "UTC".into(),
                channel: channel.into(),
                chat_id: "c".into(),
                prompt: format!("do {name}"),
                gate: None,
            };
            store.add(t, name.into(), 0, Some(0)).unwrap()
        };
        add("report", "local");
        add(TASK_NAME, BUILTIN_CHANNEL);
        add("quiet", "local");
        let run = |task: &str, id: &str, at: i64, status: RunStatus| {
            assert!(store.start_run(task, id, at).unwrap());
            store.finish_run(id, status, Some("why"), at + 1).unwrap();
        };
        run("report", "r1", 100, RunStatus::Failed);
        run("report", "r2", 200, RunStatus::Incomplete);
        run("report", "r3", 300, RunStatus::Succeeded);
        run(TASK_NAME, "l1", 100, RunStatus::Failed);
        run("quiet", "q1", 100, RunStatus::Succeeded);
        drop(store);

        let eps = task_episodes(d.path(), 0).unwrap();
        assert_eq!(eps.len(), 1, "{eps:?}");
        let e = &eps[0];
        assert_eq!(e.key, "task:report:r2");
        assert_eq!(
            (e.outcome.as_str(), e.fixed, e.at),
            ("incomplete", true, 201)
        );
        assert_eq!(e.goal, "do report");
        assert!(task_episodes(d.path(), 201).unwrap().is_empty());
    }
}
