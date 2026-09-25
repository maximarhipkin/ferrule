//! M18: lifecycle hooks in the CLI (docs/m18-hooks.md): which `[hooks]`
//! count, an agent's hook set, and `ferrule hooks list|trust|untrust`.
//!
//! Hooks run as the owner, outside the sandbox, so nothing the model can
//! write turns one on: `[hooks]` is read only from `--config` or the global
//! config (like the extensions allow-list), a workspace's hooks file runs
//! only once the owner trusted it as it is, the trust record is under
//! `<data dir>/private/`, which the tools can't reach, and `trust` needs a
//! terminal.

use crate::config;
use anyhow::{anyhow, bail, Result};
use clap::Subcommand;
use ferrule_core::HookSet;
use ferrule_hooks::{HooksConfig, TrustStore};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Once;

#[derive(Subcommand)]
pub enum HooksCmd {
    /// The hooks an agent in this workspace runs, by event, and the last runs
    List {
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        /// How many recent runs to show from the audit log
        #[arg(long, default_value_t = 20)]
        runs: usize,
    },
    /// Trust the workspace's .ferrule/hooks.toml as it is now (asks at a terminal)
    Trust {
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
    /// Stop trusting the workspace's .ferrule/hooks.toml
    Untrust {
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
}

/// The `[hooks]` that count: the config's when it came from a trusted
/// file, else none (said once per process). A bad section is an error.
pub fn settings(cfg: &config::Config, path: &Path) -> Result<HooksConfig> {
    cfg.hooks
        .validate()
        .map_err(|e| anyhow!("[hooks] in {}: {e}", path.display()))?;
    if crate::self_extend::allow_trusted(path) {
        return Ok(cfg.hooks.clone());
    }
    if cfg.hooks != HooksConfig::default() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            eprintln!(
                "ferrule: [hooks] in {} is ignored: hooks are read only from --config or the global config",
                path.display()
            );
        });
    }
    Ok(HooksConfig::default())
}

/// The hooks for a top-level agent in `workspace`. Workspace hooks that
/// won't run are said once per process. (A sub-agent gets its root's, from
/// the supervisor.)
pub fn for_agent(cfg: &config::Config, path: &Path, workspace: &Path) -> Result<HookSet> {
    let settings = settings(cfg, path)?;
    let loaded = ferrule_hooks::build(&settings, workspace, &config::data_dir()?);
    if let Some(notice) = loaded.notice {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| eprintln!("{notice}"));
    }
    Ok(loaded.set)
}

pub fn run(op: HooksCmd) -> Result<()> {
    let data = config::data_dir()?;
    match op {
        HooksCmd::List { workspace, runs } => {
            // Listing works without a config file: then there are no user
            // hooks and workspace hooks are off.
            let (settings, verify) = match config::Config::load() {
                Ok((cfg, path)) => (settings(&cfg, &path)?, cfg.agent.verify_command),
                Err(e) => {
                    eprintln!("({e} — showing default [hooks] settings)");
                    (HooksConfig::default(), None)
                }
            };
            let workspace = dunce::canonicalize(&workspace).unwrap_or(workspace);
            print!(
                "{}",
                ferrule_hooks::render_list(&settings, verify.as_deref(), &workspace, &data, runs)
            );
        }
        HooksCmd::Trust { workspace } => {
            let workspace = dunce::canonicalize(workspace)?;
            let file = ferrule_hooks::trust::workspace_file(&workspace);
            let text = std::fs::read_to_string(&file)
                .map_err(|e| anyhow!("can't read {}: {e}", file.display()))?;
            let hooks = ferrule_hooks::WorkspaceHooks::parse(&text)
                .map_err(|e| anyhow!("{}: {e}", file.display()))?;
            if !std::io::stdin().is_terminal() {
                bail!("`ferrule hooks trust` asks the owner at a terminal, and stdin isn't one");
            }
            println!("{}:", file.display());
            for (event, entry) in hooks.entries() {
                match &entry.matcher {
                    Some(m) => println!("  {event} [{m}] {}", entry.command),
                    None => println!("  {event} {}", entry.command),
                }
            }
            print!(
                "These run as you, outside the sandbox, whenever an agent works here. Trust them? [y/N] "
            );
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            if !matches!(line.trim(), "y" | "Y" | "yes") {
                bail!("not trusted");
            }
            let store = TrustStore::in_data_dir(&data);
            let n = store.trust(&workspace).map_err(|e| anyhow!(e))?;
            // What was trusted must be what was shown.
            if store.trusted(&workspace) != Some(ferrule_hooks::trust::fingerprint(text.as_bytes()))
            {
                store.untrust(&workspace).map_err(|e| anyhow!(e))?;
                bail!(
                    "{} changed while you were reading it; not trusted",
                    file.display()
                );
            }
            println!(
                "trusted {n} hook{} in {}; editing the file needs trusting it again",
                if n == 1 { "" } else { "s" },
                file.display()
            );
            if let Ok((cfg, path)) = config::Config::load() {
                if !settings(&cfg, &path)?.project {
                    println!("they won't run until `[hooks] project = true` is in your config");
                }
            }
        }
        HooksCmd::Untrust { workspace } => {
            let workspace = dunce::canonicalize(&workspace).unwrap_or(workspace);
            if TrustStore::in_data_dir(&data)
                .untrust(&workspace)
                .map_err(|e| anyhow!(e))?
            {
                println!("{}'s hooks won't run any more", workspace.display());
            } else {
                println!("{} wasn't trusted", workspace.display());
            }
        }
    }
    Ok(())
}
