//! `ferrule plugins …` (M32): the owner's side of WASM tool plugins. The
//! install goes through the same manager as the agent's `plugin_add`, with
//! the owner at the terminal instead of the queue. Design:
//! `docs/m32-wasm-plugins.md`, use: `docs/plugins.md`.

use crate::self_extend::{confirm_at_terminal, owner_manager};
use anyhow::{bail, Result};
use clap::Subcommand;
use ferrule_extensions::{Outcome, PluginRequest};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum PluginsCmd {
    /// Install a plugin: check its SHA-256, load and scan it, show its
    /// tools and what it may touch, and install on yes. Needs a terminal
    Add {
        /// A local directory with plugin.json, `git:<url>[@rev]`, or the
        /// https URL of a plugin.json (`url:` optional)
        source: String,
        /// The plugin's directory inside a git repo
        #[arg(long)]
        path: Option<String>,
        /// The manifest's `sha256` must be this (required for a URL)
        #[arg(long)]
        sha256: Option<String>,
        /// Reinstall over an installed plugin of the same name
        #[arg(long)]
        replace: bool,
        /// Where the plugin's tools are tried while it is checked
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
    /// Installed plugins, their status, tools and capabilities
    List,
    /// Remove an installed plugin; running agents drop its tools within
    /// seconds
    Remove { name: String },
}

pub async fn run(op: PluginsCmd) -> Result<()> {
    match op {
        PluginsCmd::Add {
            source,
            path,
            sha256,
            replace,
            workspace,
        } => {
            if !std::io::stdin().is_terminal() {
                bail!("`ferrule plugins add` asks the owner at a terminal, and stdin isn't one");
            }
            let m = owner_manager(&workspace)?;
            let req = PluginRequest {
                source: source_of(&source)?,
                path,
                sha256,
                replace,
                capabilities: None,
            };
            println!("fetching, checking and loading {}…", req.source);
            match m.install_plugin_as_owner(req, confirm_at_terminal).await? {
                Outcome::Installed { name, tools, .. } => {
                    println!("installed `{name}`: {}", tools.join(", "));
                    println!("running agents pick it up within a few seconds");
                }
                Outcome::Pending { id } => println!("still pending: {id}"),
            }
        }
        PluginsCmd::List => list()?,
        PluginsCmd::Remove { name } => {
            let m = owner_manager(Path::new("."))?;
            m.remove_plugin(&name, true).await?;
            println!("removed `{name}`; running agents drop its tools within a few seconds");
        }
    }
    Ok(())
}

/// A bare `https://…/plugin.json` is a URL install, another `https://` a
/// git repo; a local directory is made absolute so the lock names it.
fn source_of(spec: &str) -> Result<String> {
    let spec = spec.trim();
    if spec.starts_with("git:") || spec.starts_with("url:") {
        return Ok(spec.to_string());
    }
    if spec.starts_with("https://") {
        let kind = if spec.ends_with(".json") {
            "url"
        } else {
            "git"
        };
        return Ok(format!("{kind}:{spec}"));
    }
    if spec.starts_with("http://") {
        bail!("plugins install over https only");
    }
    match dunce::canonicalize(spec) {
        Ok(dir) if dir.is_dir() => Ok(dir.to_string_lossy().into_owned()),
        _ => bail!("`{spec}` is not a directory, a git: source or an https URL"),
    }
}

fn list() -> Result<()> {
    let m = owner_manager(Path::new("."))?;
    let lock = m.lock()?;
    if lock.plugins.is_empty() {
        println!("no plugins installed (`ferrule plugins add <dir|git:url|url>`)");
    }
    for l in m.list()?.into_iter().filter(|l| l.kind == "plugin") {
        let Some(entry) = lock.plugins.get(&l.name) else {
            continue;
        };
        let mut line = format!(
            "{} {} [{}] {} — {}",
            l.name, entry.version, l.origin, l.source, l.status
        );
        if let Some(r) = &l.reason {
            line.push_str(&format!(" ({r})"));
        }
        println!("{line}");
        if !l.tools.is_empty() {
            println!("    tools: {}", l.tools.join(", "));
        }
        println!("    may: {}", entry.capabilities.describe().join("; "));
    }
    if !ferrule_plugins::AVAILABLE && !lock.plugins.is_empty() {
        println!("(this build has no plugin runtime: none of them load)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_is_classified_before_it_reaches_the_manager() {
        assert_eq!(
            source_of("https://example.com/p/plugin.json").unwrap(),
            "url:https://example.com/p/plugin.json"
        );
        assert_eq!(
            source_of("https://github.com/o/r").unwrap(),
            "git:https://github.com/o/r"
        );
        assert_eq!(
            source_of("git:https://x/y@v1").unwrap(),
            "git:https://x/y@v1"
        );
        assert!(source_of("http://example.com/plugin.json").is_err());
        assert!(source_of("/definitely/not/here").is_err());
        let dir = tempfile::tempdir().unwrap();
        let abs = source_of(dir.path().to_str().unwrap()).unwrap();
        assert!(Path::new(&abs).is_absolute());
    }
}
