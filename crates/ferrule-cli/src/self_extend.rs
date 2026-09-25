//! Self-extension (M13) in the binary: the `[extensions]` config section,
//! the one `ExtensionManager` per process that owns every MCP server, the
//! inline approver for `ferrule chat`, and `ferrule extensions …`. The
//! design is `docs/m13-self-extension.md`.

use crate::config;
use anyhow::{anyhow, bail, Result};
use clap::Subcommand;
use ferrule_core::{Tool, ToolRegistry, ToolSource};
use ferrule_extensions::scan::report_for_owner;
use ferrule_extensions::{
    AllowList, Approver, ExtensionManager, Layout, ManagerConfig, Outcome, Pending, Review,
};
use ferrule_mcp::McpServerConfig;
use ferrule_sandbox::Sandbox;
use ferrule_skills::{LiveSkillTools, Scope, SkillRoot, SkillsHandle};
use serde::Deserialize;
use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ExtensionsConfig {
    /// Offer the model `mcp_add`, `skill_install`, `skill_keep` and the
    /// rest. Off by default: six more tool definitions on every request,
    /// and widening its own powers is something the owner turns on.
    pub enabled: bool,
    /// Sources installable without asking (`npm:@scope/*`,
    /// `git:https://host/org/*`, exact names, pinned versions). Honoured
    /// only from `--config`/`$FERRULE_CONFIG` or the global config: a
    /// `./ferrule.toml` may be in a workspace the agent can write.
    pub allow: Vec<String>,
}

/// Whether the file the config came from may carry the allow-list.
pub(crate) fn allow_trusted(path: &Path) -> bool {
    if std::env::var_os("FERRULE_CONFIG").is_some_and(|p| !p.is_empty()) {
        return true;
    }
    match config::global_config_path() {
        Ok(global) => same_file(path, &global),
        Err(_) => false,
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (dunce::canonicalize(a), dunce::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn allow_list(ext: &ExtensionsConfig, path: &Path) -> AllowList {
    if ext.allow.is_empty() {
        return AllowList::new::<String>(&[]);
    }
    if !allow_trusted(path) {
        tracing::warn!(
            "[extensions] allow in {} is ignored: the allow-list is read only from --config or the global config — every install will wait for approval",
            path.display()
        );
        return AllowList::new::<String>(&[]);
    }
    AllowList::new(&ext.allow)
}

/// Who an agent is, as far as extensions go (where M12 meets M13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// The top-level agent: every installed tool, plus the model's six
    /// extension tools when `[extensions] enabled`.
    Root,
    /// A sub-agent: installed tools only — just the ones that change
    /// nothing when `reading_only` (a verifier or read-only child) — and
    /// never the extension tools, so only the root can install, remove or
    /// keep anything.
    Child { reading_only: bool },
}

/// A source narrowed to the tools that don't change anything.
struct OnlyReading(Arc<dyn ToolSource>);

impl ToolSource for OnlyReading {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        crate::agents::only_reading(&self.0.tools())
    }
}

/// Every MCP server of this process, and the extension tools when on.
#[derive(Clone)]
pub struct Extensions {
    manager: Arc<ExtensionManager>,
    enabled: bool,
}

impl Extensions {
    /// The tools the servers offer right now.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.manager.tools()
    }

    /// Put the servers into `registry` as a live source, plus — for the
    /// root only — the model's extension tools when `[extensions] enabled`
    /// (the skill ones only when skills are on too).
    pub fn attach(&self, registry: &mut ToolRegistry, skills_on: bool, reach: Reach) {
        if reach == (Reach::Child { reading_only: true }) {
            registry.attach(Arc::new(OnlyReading(self.manager.clone())));
        } else {
            registry.attach(self.manager.clone());
        }
        if !self.enabled || reach != Reach::Root {
            return;
        }
        for tool in ferrule_extensions::tools(&self.manager) {
            if skills_on || !tool.definition().name.starts_with("skill_") {
                registry.register(tool);
            }
        }
    }

    /// The live skill set — project roots, then configured and default
    /// ones, then installed skills — freshly rediscovered, so a new agent
    /// sees what was added on disk since the last one.
    fn skills(&self) -> SkillsHandle {
        let handle = self
            .manager
            .skills()
            .cloned()
            .expect("the manager is always built with a skill set");
        handle.refresh();
        handle
    }

    /// Skills as tools over the live set: `activate_skill` knows a skill
    /// installed mid-session.
    pub fn skill_tools(&self) -> (SkillsHandle, Arc<dyn ToolSource>) {
        let handle = self.skills();
        (handle.clone(), Arc::new(LiveSkillTools::new(handle)))
    }

    /// `ferrule chat` at a terminal: ask there, instead of only queueing.
    pub fn ask_at_terminal(&self) {
        if std::io::stdin().is_terminal() {
            self.manager.set_approver(Arc::new(TtyApprover));
        }
    }
}

/// Build the process's manager: connect the configured servers (scanned,
/// and re-scanned on `list_changed`), load the lock's active entries, and
/// follow the lock and the config file from then on.
pub async fn start(
    servers: Vec<McpServerConfig>,
    sandbox: Arc<Sandbox>,
    workspace: &Path,
) -> Result<Extensions> {
    let (cfg, path) = config::Config::load()?;
    let layout = Layout::new(config::data_dir()?);
    let workspace = dunce::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let skills = skills_handle(&cfg.skills, &workspace, &layout);
    let _ = LIVE_SKILLS.set(skills.clone());
    let manager = ExtensionManager::new(ManagerConfig {
        allow: allow_list(&cfg.extensions, &path),
        layout,
        sandbox,
        workspace,
        skills: Some(skills),
    });
    manager.start(servers).await;
    manager.spawn_sync();
    crate::config_follow::spawn(&manager, &cfg, &path, allow_trusted(&path));
    crate::connections::follow(&manager, &cfg);
    Ok(Extensions {
        manager,
        enabled: cfg.extensions.enabled,
    })
}

/// The process's skills, once `start` has built them.
static LIVE_SKILLS: std::sync::OnceLock<SkillsHandle> = std::sync::OnceLock::new();

/// The running agents' skill set, to turn a skill off or on at once (M24).
pub(crate) fn live_skills() -> Option<&'static SkillsHandle> {
    LIVE_SKILLS.get()
}

fn skills_handle(cfg: &config::SkillsConfig, workspace: &Path, layout: &Layout) -> SkillsHandle {
    let paths: Vec<PathBuf> = cfg.paths.iter().map(|p| crate::expand_home(p)).collect();
    let mut roots = ferrule_skills::default_roots(workspace, cfg.project, &paths);
    roots.push(SkillRoot {
        dir: layout.skills_dir(),
        scope: Scope::User,
    });
    SkillsHandle::discovering(roots, cfg.disabled.clone())
}

/// Asks the owner in the chat terminal. Yes still means fetch-and-scan: a
/// flagged install stays queued for `ferrule extensions approve`, where the
/// owner sees the flagged text.
struct TtyApprover;

#[async_trait::async_trait]
impl Approver for TtyApprover {
    async fn decide(&self, pending: &Pending) -> Option<bool> {
        let question = format!(
            "Agent wants to install {} ({}) — allow? [y/N] ",
            pending.request.describe(),
            pending.reason
        );
        tokio::task::spawn_blocking(move || {
            eprint!("\n\x1b[1;33m{question}\x1b[0m");
            let _ = std::io::stderr().flush();
            let mut line = String::new();
            // End of input: leave it queued.
            if std::io::stdin().read_line(&mut line).ok()? == 0 {
                return None;
            }
            Some(yes(&line))
        })
        .await
        .ok()
        .flatten()
    }
}

fn yes(line: &str) -> bool {
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[derive(Subcommand)]
pub enum ExtCmd {
    /// Configured and installed MCP servers, installed skills, their status
    List,
    /// Install requests waiting for approval
    Pending,
    /// Fetch, start and scan a pending request here, show what it offers
    /// and what the scan found, and install it on yes. Needs a terminal
    Approve {
        id: String,
        /// Where the server runs while it is checked
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
    /// Drop a pending request (nothing of it was fetched)
    Deny { id: String },
    /// Remove a configured or installed server, or a skill; running agents
    /// drop it within seconds
    Remove {
        name: String,
        /// Delete the server's state dir too
        #[arg(long)]
        purge: bool,
    },
    /// Re-scan a suspended server or skill, show why it was suspended and
    /// what it offers now, and make it active again on yes. Needs a terminal
    Resume {
        name: String,
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
}

/// A manager for one owner command: no configured servers, no sync loop.
pub(crate) fn owner_manager(workspace: &Path) -> Result<Arc<ExtensionManager>> {
    let (cfg, path) = config::Config::load()?;
    let workspace = dunce::canonicalize(workspace)
        .map_err(|e| anyhow!("workspace {}: {e}", workspace.display()))?;
    Ok(ExtensionManager::new(ManagerConfig {
        layout: Layout::new(config::data_dir()?),
        allow: allow_list(&cfg.extensions, &path),
        sandbox: crate::shared_sandbox(&cfg)?,
        workspace,
        skills: None,
    }))
}

fn need_terminal(what: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("`ferrule extensions {what}` asks the owner at a terminal, and stdin isn't one");
    }
    Ok(())
}

/// The configured and installed servers (and skills, unless
/// `servers_only`), with pending requests.
pub fn list(servers_only: bool) -> Result<()> {
    let m = owner_manager(Path::new("."))?;
    let (cfg, _) = config::Config::load()?;
    for s in &cfg.mcp.servers {
        let what = s.url.as_deref().unwrap_or(&s.command);
        let off = if cfg.mcp.disabled.contains(&s.name) {
            " — disabled (`ferrule mcp enable`)"
        } else {
            ""
        };
        println!("server {} [configured] {what}{off}", s.name);
    }
    let listed = m.list()?;
    for l in listed
        .iter()
        .filter(|l| !servers_only || l.kind == "server")
    {
        let mut line = format!("{} {} [{}] {}", l.kind, l.name, l.origin, l.source);
        if let Some(pin) = &l.pin {
            line.push_str(&format!(" @ {}", &pin[..pin.len().min(12)]));
        }
        line.push_str(&format!(" — {}", l.status));
        if let Some(r) = &l.reason {
            line.push_str(&format!(" ({r})"));
        }
        if !l.tools.is_empty() {
            line.push_str(&format!("; tools: {}", l.tools.join(", ")));
        }
        println!("{line}");
    }
    if cfg.mcp.servers.is_empty() && listed.is_empty() {
        println!("no MCP servers configured, nothing installed");
    }
    let pending = m.queue().list()?.len();
    if pending > 0 {
        println!("{pending} request(s) pending — `ferrule extensions pending`");
    }
    if !cfg.extensions.enabled {
        println!("([extensions] enabled = false: agents can't install anything themselves)");
    }
    Ok(())
}

pub async fn run(op: ExtCmd) -> Result<()> {
    match op {
        ExtCmd::List => list(false)?,
        ExtCmd::Pending => {
            let m = owner_manager(Path::new("."))?;
            let all = m.queue().list()?;
            if all.is_empty() {
                println!("nothing pending");
            }
            for p in all {
                println!("{}  {}  {}", p.id, p.created_at, p.request.describe());
                println!("    why: {}", p.reason);
                if !p.findings.is_empty() {
                    println!("{}", report_for_owner(&p.findings));
                }
            }
        }
        ExtCmd::Approve { id, workspace } => {
            need_terminal("approve")?;
            let m = owner_manager(&workspace)?;
            println!("fetching, starting and scanning {id} in the sandbox…");
            match m.approve(&id, confirm_at_terminal).await? {
                Outcome::Installed { name, tools, .. } => {
                    println!("installed `{name}`: {}", tools.join(", "));
                    println!("running agents pick it up within a few seconds");
                }
                Outcome::Pending { id } => println!("still pending: {id}"),
            }
            m.shutdown_all().await;
        }
        ExtCmd::Deny { id } => {
            let m = owner_manager(Path::new("."))?;
            if m.deny(&id)? {
                println!("denied {id}; nothing was installed");
            } else {
                bail!("no pending request `{id}`");
            }
        }
        ExtCmd::Remove { name, purge } => {
            if let Some(removed) = crate::mcp_config::remove_configured(
                crate::mcp_config::config_file()?,
                &name,
                purge,
            )? {
                println!(
                    "removed `{name}` from {}; running agents stop it within a few seconds",
                    crate::setup::tilde(&removed.path)
                );
                for secret in removed.secrets_kept {
                    println!("  [secrets] {secret} is kept (nothing else here names it; `ferrule setup` → Tool credentials removes it)");
                }
                return Ok(());
            }
            let m = owner_manager(Path::new("."))?;
            if m.lock()?.servers.contains_key(&name) {
                m.remove_server(&name, true, purge).await?;
            } else {
                m.remove_skill(&name, true).await?;
            }
            println!("removed `{name}`");
        }
        ExtCmd::Resume { name, workspace } => {
            need_terminal("resume")?;
            let m = owner_manager(&workspace)?;
            m.resume(&name, confirm_at_terminal).await?;
            println!("resumed `{name}`; running agents pick it up within a few seconds");
            m.shutdown_all().await;
        }
    }
    Ok(())
}

/// Show the owner what they'd install and ask. A block hit needs the
/// word `waive`: that approves exactly the flagged text shown, and a
/// different text later suspends it again.
fn confirm_at_terminal(review: &Review) -> bool {
    println!("\n{}", review.what);
    println!("offers: {}", review.items.join(", "));
    if let Some(why) = &review.sandbox_degraded {
        println!("\x1b[1;31mnot fully sandboxed here: {why}\x1b[0m");
    }
    if review.findings.is_empty() {
        println!("scan: clean");
    } else {
        println!("scan:\n{}", report_for_owner(&review.findings));
    }
    let prompt = if review.blocked() {
        "The scan BLOCKED the text above. Type `waive` to install anyway: "
    } else {
        "Install? [y/N] "
    };
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    if review.blocked() {
        line.trim() == "waive"
    } else {
        yes(&line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_dirs_match_the_ones_configured_servers_had() {
        for name in ["github", "my-fs_2", "a.b", "../x", ""] {
            let want = crate::mcp_dir_name(name);
            assert_eq!(ferrule_extensions::layout::mcp_dir_name(name), want);
        }
    }

    struct Fixed(&'static str, bool);

    #[async_trait::async_trait]
    impl Tool for Fixed {
        fn definition(&self) -> ferrule_core::tool::ToolDefinition {
            ferrule_core::tool::ToolDefinition {
                name: self.0.into(),
                description: String::new(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }
        fn changes_files(&self) -> bool {
            self.1
        }
        async fn call(
            &self,
            _: serde_json::Value,
            _: &ferrule_core::ToolContext,
        ) -> Result<ferrule_core::tool::ToolOutput, ferrule_core::CoreError> {
            Ok(ferrule_core::tool::ToolOutput::ok(""))
        }
    }

    fn names(reg: &ToolRegistry) -> Vec<String> {
        let mut n: Vec<String> = reg.definitions().into_iter().map(|d| d.name).collect();
        n.sort();
        n
    }

    #[tokio::test]
    async fn a_child_never_gets_the_extension_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ExtensionManager::new(ManagerConfig {
            allow: AllowList::default(),
            layout: Layout::new(tmp.path().join("data")),
            sandbox: Arc::new(Sandbox::off()),
            workspace: tmp.path().to_path_buf(),
            skills: None,
        });
        let ext = Extensions {
            manager,
            enabled: true,
        };
        let six = [
            "extensions_list",
            "mcp_add",
            "mcp_remove",
            "skill_install",
            "skill_keep",
            "skill_remove",
        ];

        let mut root = ToolRegistry::new();
        ext.attach(&mut root, true, Reach::Root);
        assert_eq!(names(&root), six);

        for reading_only in [false, true] {
            let mut child = ToolRegistry::new();
            ext.attach(&mut child, true, Reach::Child { reading_only });
            let n = names(&child);
            assert!(six.iter().all(|t| !n.iter().any(|x| x == t)), "{n:?}");
            assert!(!child.contains("mcp_add") && !child.contains("skill_keep"));
        }
    }

    #[test]
    fn a_reading_child_sees_only_installed_tools_that_change_nothing() {
        struct Two;
        impl ToolSource for Two {
            fn tools(&self) -> Vec<Arc<dyn Tool>> {
                vec![
                    Arc::new(Fixed("mcp__s__look", false)),
                    Arc::new(Fixed("mcp__s__write", true)),
                ]
            }
        }
        let mut reg = ToolRegistry::new();
        reg.attach(Arc::new(OnlyReading(Arc::new(Two))));
        assert_eq!(names(&reg), ["mcp__s__look"]);
    }

    #[test]
    fn answers() {
        for y in ["y", "Y\n", " yes "] {
            assert!(yes(y), "{y}");
        }
        for n in ["", "n", "yess", "no"] {
            assert!(!yes(n), "{n}");
        }
    }

    #[test]
    fn the_allow_list_is_ignored_from_a_workspace_config() {
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path().join("ferrule.toml");
        std::fs::write(&local, "").unwrap();
        let ext = ExtensionsConfig {
            enabled: true,
            allow: vec!["npm:@modelcontextprotocol/*".into()],
        };
        // Neither $FERRULE_CONFIG (not set under `cargo test` unless the
        // caller set it) nor the global file.
        if std::env::var_os("FERRULE_CONFIG").is_none() {
            assert!(allow_list(&ext, &local).entries.is_empty());
        }
        assert!(!AllowList::new(&ext.allow).entries.is_empty());
    }
}
