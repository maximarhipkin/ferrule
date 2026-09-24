//! `ferrule eval`: run a task suite against a provider, as ferrule's full
//! harness, as a deliberately naive one, or both, and report the difference
//! (see `docs/eval.md`).

use crate::config;
use crate::ledger;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use ferrule_core::HarnessProfile;
use ferrule_eval::{plan, report, Caps, Env, Options, Pricing, Suite, SuiteKind, Variant};
use ferrule_providers::OpenAiCompatProvider;
use ferrule_sandbox::Sandbox;
use std::path::PathBuf;
use std::sync::Arc;

/// Exit status when the budget stopped the suite.
const EXIT_BUDGET: i32 = 3;

#[derive(Subcommand)]
pub enum EvalCmd {
    /// Run a suite: `--variant ab` runs every task under both harnesses
    Run {
        /// Suite directory (or its suite.toml)
        suite: PathBuf,
        /// engineered (ferrule's harness), naive (the baseline) or ab (both)
        #[arg(long, default_value = "engineered")]
        variant: String,
        /// A `[providers.*]` entry; default_provider if omitted
        #[arg(long)]
        provider: Option<String>,
        /// Model to use instead of the provider's configured one (both
        /// variants use the same)
        #[arg(long)]
        model: Option<String>,
        /// Shrink the context window, for both variants, to put the harness
        /// under pressure (default: the suite's, else the profile's)
        #[arg(long)]
        context_window: Option<usize>,
        /// Only tasks with this tag (repeatable)
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Only this task (repeatable)
        #[arg(long = "task")]
        tasks: Vec<String>,
        /// Run each task this many times per variant
        #[arg(long, default_value_t = 1)]
        repeat: u32,
        /// Stop the suite once this much is spent (needs the provider's
        /// prices in the config); 0 = no cost cap
        #[arg(long, default_value_t = 5.0)]
        max_usd: f64,
        /// Stop the suite past this many input + output tokens; 0 = no cap
        #[arg(long, default_value_t = 20_000_000)]
        max_tokens: u64,
        /// Show the runs and the worst-case spend, call nothing
        #[arg(long)]
        dry_run: bool,
        /// Keep each task's workspace (their paths are printed)
        #[arg(long)]
        keep: bool,
    },
}

pub async fn cmd(op: EvalCmd) -> Result<()> {
    match op {
        EvalCmd::Run {
            suite,
            variant,
            provider,
            model,
            context_window,
            tags,
            tasks,
            repeat,
            max_usd,
            max_tokens,
            dry_run,
            keep,
        } => {
            let variants = Variant::parse(&variant)
                .ok_or_else(|| anyhow!("--variant: `{variant}` isn't engineered, naive or ab"))?;
            let suite = Suite::load(&suite)?;
            let caps = Caps {
                max_usd: (max_usd > 0.0).then_some(max_usd),
                max_tokens: (max_tokens > 0).then_some(max_tokens),
            };
            let (cfg, _) = config::Config::load()?;
            let name = provider
                .or_else(|| cfg.default_provider.clone())
                .ok_or_else(|| anyhow!("no --provider and no default_provider in the config"))?;
            let pcfg = cfg
                .providers
                .get(&name)
                .ok_or_else(|| anyhow!("provider `{name}` not in config"))?;
            let model = model.unwrap_or_else(|| pcfg.model.clone());
            let pricing = ledger::ProviderPricing::from_config(pcfg).map(|p| Pricing {
                input: p.input,
                cached_input: p.cached_input,
                output: p.output,
            });
            let base = HarnessProfile::by_name(&pcfg.profile);
            let window = context_window.or(suite.context_window);

            if dry_run {
                let profile = ferrule_eval::variant::windowed(&base, window);
                print!(
                    "{}",
                    plan::render(&plan::PlanInput {
                        suite: &suite,
                        tags: &tags,
                        tasks: &tasks,
                        variants: &variants,
                        repeat,
                        profile: &profile,
                        provider: &name,
                        model: &model,
                        pricing,
                        caps,
                    })?
                );
                return Ok(());
            }

            let (_, _, key) = cfg.resolve_provider(Some(&name))?;
            let provider = Arc::new(OpenAiCompatProvider::new(
                name.clone(),
                &pcfg.base_url,
                key,
                &model,
            ));
            // The config's sandbox without the credential broker: a task's
            // result mustn't depend on the keys this machine holds.
            let sandbox =
                Arc::new(Sandbox::new(crate::sandbox_policy(&cfg)).map_err(|e| anyhow!(e))?);
            if caps.max_usd.is_some() && pricing.is_none() {
                eprintln!(
                    "ferrule eval: `{name}` has no prices in the config, so only the token cap ({}) applies",
                    caps.max_tokens
                        .map(|t| format!("{t} tokens"))
                        .unwrap_or_else(|| "none".into())
                );
            }
            let data = config::data_dir()?;
            let env = Env {
                provider,
                provider_name: name,
                model,
                profile: base,
                sandbox,
                memory_tools: Some(Arc::new(crate::memory_tools::tools)),
                ledger: ledger::build_sink(&cfg),
                pricing,
                transcripts: Some(data.join("eval")),
            };
            let opts = Options {
                variants,
                tags,
                tasks,
                repeat,
                context_window,
                caps,
                keep,
                work_root: None,
                progress: Some(Arc::new(|line: &str| eprintln!("{line}"))),
            };
            let run = ferrule_eval::run_suite(&suite, &env, &opts).await?;
            let text = report::render(&run);
            print!("\n{text}");
            let dir = data.join("eval").join(&run.run_id);
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("report.txt"), &text)?;
            std::fs::write(dir.join("run.json"), serde_json::to_string_pretty(&run)?)?;
            println!("transcripts and this report: {}", dir.display());
            if keep {
                println!(
                    "workspaces kept under {}",
                    std::env::temp_dir()
                        .join("ferrule-eval")
                        .join(&run.run_id)
                        .display()
                );
            }
            std::process::exit(exit_code(&suite, &run));
        }
    }
}

/// 3 when the budget stopped the suite; 1 when a grader couldn't decide,
/// or a regression suite has a failure; else 0.
fn exit_code(suite: &Suite, run: &ferrule_eval::SuiteRun) -> i32 {
    use ferrule_eval::Outcome;
    if run.budget_stop.is_some() {
        return EXIT_BUDGET;
    }
    let error = run.results.iter().any(|r| r.outcome == Outcome::Error);
    let failed = run.results.iter().any(|r| r.outcome == Outcome::Fail);
    if error || (suite.kind == SuiteKind::Regression && failed) {
        1
    } else {
        0
    }
}
