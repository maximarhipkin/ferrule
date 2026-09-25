//! `ferrule eval`: run a task suite against a provider, as ferrule's full
//! harness, as a deliberately naive one, or both, and report the difference
//! (see `docs/eval.md`).

use crate::config;
use crate::ledger;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use ferrule_core::HarnessProfile;
use ferrule_eval::{
    history, plan, report, Caps, Env, Judge, Options, Pricing, Suite, SuiteKind, Variant,
};
use ferrule_providers::OpenAiCompatProvider;
use ferrule_sandbox::Sandbox;
use std::path::PathBuf;
use std::sync::Arc;

/// Exit status when the budget (or the owner's trust) stopped the suite.
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
        /// variants use the same). Without --provider it can be any
        /// connected model's ref: an alias or provider/model. The eval never
        /// follows `[models] default`, a chat's pin or the fallback list
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
        /// A `[providers.*]` entry (with its configured model) to grade
        /// rubrics; default: the provider under test, and the report says
        /// the run was self-judged
        #[arg(long)]
        judge_provider: Option<String>,
    },
    /// Print a saved run's report and its diff against the run before it
    Report {
        /// Only runs of this suite (by its name); default: any
        suite: Option<String>,
        /// This run (its id, or the start of it) instead of the latest
        #[arg(long)]
        run: Option<String>,
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
            judge_provider,
        } => {
            let variants = Variant::parse(&variant)
                .ok_or_else(|| anyhow!("--variant: `{variant}` isn't engineered, naive or ab"))?;
            let suite = Suite::load(&suite)?;
            let caps = Caps {
                max_usd: (max_usd > 0.0).then_some(max_usd),
                max_tokens: (max_tokens > 0).then_some(max_tokens),
            };
            let (cfg, _) = config::Config::load()?;
            // Only what's named here, or default_provider: a run is
            // comparable with the last one whatever the owner has since made
            // the default, pinned or listed as a fallback.
            let cat = crate::models::Catalog::from_config(&cfg);
            let (provider, model) = match (provider, model) {
                (None, Some(w)) => match cat.resolve(&w) {
                    Ok(e) => (Some(e.provider.clone()), Some(e.model.clone())),
                    Err(_) => (None, Some(w)),
                },
                named => named,
            };
            let name = provider
                .or_else(|| cfg.default_provider.clone())
                .ok_or_else(|| anyhow!("no --provider and no default_provider in the config"))?;
            let pcfg = cfg
                .providers
                .get(&name)
                .ok_or_else(|| anyhow!("provider `{name}` not in config"))?;
            let model = model.unwrap_or_else(|| pcfg.model.clone());
            let prices = |pcfg| {
                ledger::ProviderPricing::from_config(pcfg).map(|p| Pricing {
                    input: p.input,
                    cached_input: p.cached_input,
                    output: p.output,
                })
            };
            let own = cat
                .entries
                .iter()
                .find(|e| e.provider == name && e.model == model);
            let pricing = cat
                .price(&name, &model)
                .map(|p| Pricing {
                    input: p.input,
                    cached_input: p.cached_input,
                    output: p.output,
                })
                .or_else(|| prices(pcfg));
            let base = HarnessProfile::by_name(own.map_or(&pcfg.profile, |e| &e.profile));
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
            let judge = match &judge_provider {
                Some(jname) => {
                    let jcfg = cfg
                        .providers
                        .get(jname)
                        .ok_or_else(|| anyhow!("--judge-provider: `{jname}` not in config"))?;
                    let (_, _, jkey) = cfg.resolve_provider(Some(jname))?;
                    Some(Judge {
                        provider: Arc::new(OpenAiCompatProvider::new(
                            jname.clone(),
                            &jcfg.base_url,
                            jkey,
                            &jcfg.model,
                        )),
                        provider_name: jname.clone(),
                        model: jcfg.model.clone(),
                        pricing: prices(jcfg),
                    })
                }
                None => None,
            };
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
                judge,
                // Read only for a suite that opts in (M16 §9).
                playbook: suite
                    .owner_playbook
                    .then(|| crate::learn::prompt_section(&cfg.learning))
                    .flatten(),
                // M19 §10: the owner's hub is only built for a suite that
                // opts in, so a default run doesn't read its state at all.
                owner_trust: if suite.owner_trust {
                    owner_trust(&cfg)?
                } else {
                    None
                },
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
            let earlier = history::load_runs(&data.join("eval"));
            let text = format!(
                "{}{}",
                report::render(&run),
                history::render(&history::diffs(&earlier, &run))
            );
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
        EvalCmd::Report { suite, run } => {
            let root = config::data_dir()?.join("eval");
            let runs = history::load_runs(&root);
            let found = runs.iter().rev().find(|r| {
                suite.as_ref().is_none_or(|s| &r.suite == s)
                    && run
                        .as_ref()
                        .is_none_or(|id| r.run_id.starts_with(id.as_str()))
            });
            let Some(found) = found else {
                return Err(anyhow!(
                    "no saved eval run{}{} under {}",
                    suite
                        .map(|s| format!(" of suite `{s}`"))
                        .unwrap_or_default(),
                    run.map(|id| format!(" with id `{id}`")).unwrap_or_default(),
                    root.display()
                ));
            };
            print!(
                "{}{}",
                report::render(found),
                history::render(&history::diffs(&runs, found))
            );
            Ok(())
        }
    }
}

/// Both variants of a suite with `owner_trust = true` run under the
/// owner's guard, unattended, and charge the owner's day under the tree
/// `eval:<run id>`.
fn owner_trust(cfg: &config::Config) -> Result<Option<ferrule_eval::OwnerTrust>> {
    crate::trust::hub(cfg)?;
    let cfg = cfg.clone();
    Ok(Some(Arc::new(move |tree: &str, sink| {
        crate::trust::seat(
            tree,
            ferrule_trust::Route::Unattended("this is an eval run, which runs unattended".into()),
        );
        let tag = ledger::LedgerTag {
            sink,
            task_shape: "eval".into(),
            origin: None,
        };
        let (tag, guard) = crate::trust::equip(&cfg, tree, false, Some(tag))
            .expect("the hub was built before the suite started");
        (tag.sink, guard as Arc<dyn ferrule_core::Guard>)
    })))
}

/// 3 when the budget stopped the suite; 1 when a grader couldn't decide,
/// or a regression suite has a failure in the variant it gates (ferrule's
/// own: the naive baseline is expected to fail); else 0.
fn exit_code(suite: &Suite, run: &ferrule_eval::SuiteRun) -> i32 {
    use ferrule_eval::Outcome;
    // Only the budget, or (for `owner_trust`) the owner's caps and kill
    // switch, stop a task.
    if run.budget_stop.is_some() || run.results.iter().any(|r| r.outcome == Outcome::Stopped) {
        return EXIT_BUDGET;
    }
    let error = run.results.iter().any(|r| r.outcome == Outcome::Error);
    let failed = run
        .results
        .iter()
        .any(|r| r.outcome == Outcome::Fail && r.variant == ferrule_eval::Variant::Engineered);
    if error || (suite.kind == SuiteKind::Regression && failed) {
        1
    } else {
        0
    }
}
