//! The extension manager: one per host process. It owns every MCP server
//! the agent can reach — the owner's configured ones and the installed ones
//! from the lock — and serves their tools as a [`ToolSource`], so a server
//! installed, suspended or removed mid-session is in or out of the very
//! next provider request.
//!
//! Every surface it exposes has been scanned: at install, at every load,
//! and on every `notifications/tools/list_changed`. An installed server
//! whose approved surface changes, or which grows a flagged tool, is
//! suspended as a whole; a configured server only loses the changed or
//! flagged tools (the owner wrote that config by hand). See
//! `docs/m13-self-extension.md` §4–§9.

use crate::allowlist::{AllowList, Kind, Source};
use crate::error::{refused, ExtError, Result};
use crate::layout::Layout;
use crate::lock::{self, LockFile, LockStore, Origin, ServerEntry, SkillEntry, Status, Waiver};
use crate::pending::{Pending, PendingQueue, Request};
use crate::scan::{self, Finding};
use crate::skill::{self, SkillCandidate};
use crate::source::{self, McpRequest, SkillRequest};
use ferrule_core::tool::{Tool, ToolContext, ToolSource};
use ferrule_core::verify::Verifier;
use ferrule_mcp::{build_tools, McpClient, McpServerConfig, McpToolInfo, ServerHost};
use ferrule_sandbox::Sandbox;
use ferrule_skills::SkillsHandle;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

/// How often a daemon re-reads the lock, to pick up what the owner approved
/// or removed from the CLI.
mod plugin;

pub const SYNC_EVERY: Duration = Duration::from_secs(2);
/// A self-written skill's check gets this long.
const CHECK_TIMEOUT: Duration = Duration::from_secs(120);

/// Asked when the agent requests a source that isn't allow-listed.
#[async_trait::async_trait]
pub trait Approver: Send + Sync {
    /// `Some(true)`: the owner approved it just now (it is then fetched and
    /// scanned, and a block hit still refuses it). `Some(false)`: denied.
    /// `None`: leave it queued for `ferrule extensions approve`.
    async fn decide(&self, pending: &Pending) -> Option<bool>;
}

/// The default: everything waits in the queue.
pub struct QueueApprover;

#[async_trait::async_trait]
impl Approver for QueueApprover {
    async fn decide(&self, _pending: &Pending) -> Option<bool> {
        None
    }
}

pub struct ManagerConfig {
    pub layout: Layout,
    pub allow: AllowList,
    /// The host's shared sandbox; every server runs in its helper form.
    pub sandbox: Arc<Sandbox>,
    /// The agent's workspace: every server's working directory.
    pub workspace: PathBuf,
    /// The live skill set to refresh when a skill comes or goes. Its roots
    /// should include `layout.skills_dir()`.
    pub skills: Option<SkillsHandle>,
}

/// What an install request came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Installed and live: the tool (or skill) names now offered.
    Installed {
        name: String,
        tools: Vec<String>,
        warnings: usize,
    },
    /// Waiting for the owner.
    Pending { id: String },
}

/// What the owner is shown before confirming an approval or a resume.
#[derive(Debug, Clone)]
pub struct Review {
    pub what: String,
    /// Tools (MCP) or the skill name.
    pub items: Vec<String>,
    pub findings: Vec<Finding>,
    pub sandbox_degraded: Option<String>,
    /// M32: what a plugin may do, one line each (with what is new since
    /// the last grant called out). Empty for servers and skills.
    pub capabilities: Vec<String>,
}

impl Review {
    pub fn blocked(&self) -> bool {
        scan::blocks(&self.findings).next().is_some()
    }
}

/// One line per extension, for `extensions_list` and `ferrule extensions list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    /// `server`, `skill` or `plugin`.
    pub kind: &'static str,
    /// `configured`, or the lock origin.
    pub origin: String,
    pub source: String,
    pub pin: Option<String>,
    /// `active`, `suspended` or `not loaded`.
    pub status: String,
    pub reason: Option<String>,
    pub tools: Vec<String>,
}

/// What [`ExtensionManager::probe`] found.
#[derive(Debug, Clone)]
pub struct Probe {
    /// The tools the server offers, after `enabled_tools`.
    pub tools: Vec<McpToolInfo>,
    pub findings: Vec<Finding>,
    /// Tools with a block hit.
    pub blocked: Vec<String>,
    pub sandbox_degraded: Option<String>,
}

impl Probe {
    pub fn blocked(&self) -> bool {
        !self.blocked.is_empty()
    }
}

struct Live {
    /// Tells a stale watcher (of a server since replaced) from the current one.
    id: u64,
    client: Arc<McpClient>,
    tools: Vec<Arc<dyn Tool>>,
    /// In the lock (installed); false for an owner-configured server.
    managed: bool,
    /// Tool name → digest of the surface accepted so far.
    approved: BTreeMap<String, String>,
    waivers: Vec<Waiver>,
    /// The lock entry it was loaded from, to notice the owner changing it.
    entry: Option<ServerEntry>,
}

/// A server started and scanned but not yet committed.
struct Prepared {
    req: McpRequest,
    source: Source,
    pin: Option<String>,
    cfg: McpServerConfig,
    client: Arc<McpClient>,
    infos: Vec<McpToolInfo>,
    surface: Surface,
    /// A git checkout, and whether this install created it.
    checkout: Option<(PathBuf, bool)>,
}

/// A skill fetched and inspected but not yet committed.
struct PreparedSkill {
    source: String,
    pin: Option<String>,
    candidate: SkillCandidate,
    /// The clone to delete afterwards, if any.
    clone: Option<PathBuf>,
}

#[derive(Default)]
struct Surface {
    digests: BTreeMap<String, String>,
    findings: Vec<Finding>,
    /// Tools with an unwaived block hit.
    blocked: BTreeSet<String>,
    /// Approved tools whose surface is not what was approved.
    changed: BTreeSet<String>,
}

impl Surface {
    fn of(
        infos: &[McpToolInfo],
        approved: Option<&BTreeMap<String, String>>,
        waivers: &[Waiver],
    ) -> Self {
        let mut s = Surface::default();
        for info in infos {
            let digest = scan::tool_digest(&info.name, &info.description, &info.input_schema);
            let findings = scan::scan_tool(&info.name, &info.description, &info.input_schema);
            for f in scan::blocks(&findings) {
                if !lock::waived(waivers, f, &digest) {
                    s.blocked.insert(info.name.clone());
                }
            }
            if let Some(old) = approved.and_then(|a| a.get(&info.name)) {
                if *old != digest {
                    s.changed.insert(info.name.clone());
                }
            }
            s.findings.extend(findings);
            s.digests.insert(info.name.clone(), digest);
        }
        s
    }

    fn unwaived_blocks(&self) -> Vec<Finding> {
        scan::blocks(&self.findings)
            .filter(|f| self.blocked.contains(&f.item))
            .cloned()
            .collect()
    }

    fn warnings(&self) -> usize {
        self.findings.len() - scan::blocks(&self.findings).count()
    }
}

pub struct ExtensionManager {
    cfg: ManagerConfig,
    store: LockStore,
    queue: PendingQueue,
    approver: RwLock<Arc<dyn Approver>>,
    live: RwLock<BTreeMap<String, Live>>,
    /// Serialises installs, removals, syncs and re-scans.
    ops: tokio::sync::Mutex<()>,
    next_id: AtomicU64,
    /// The lock as last synced, to refresh skills only when it changed.
    last_lock: Mutex<Option<LockFile>>,
    /// The sandbox servers started from now on run in: `cfg.sandbox` until
    /// [`Self::set_sandbox`] swaps it (M17: a secret bound mid-run).
    sandbox: RwLock<Arc<Sandbox>>,
    /// The configured servers as last applied by `start`/`set_configured`,
    /// running or not.
    configured: tokio::sync::Mutex<Vec<McpServerConfig>>,
    /// M32: loaded plugins, by name.
    plugins: RwLock<BTreeMap<String, plugin::LivePlugin>>,
    /// Warned once that installed plugins can't run in this build.
    no_plugin_runtime: AtomicBool,
}

impl ToolSource for ExtensionManager {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.live
            .read()
            .unwrap()
            .values()
            .flat_map(|l| l.tools.iter().cloned())
            .chain(self.plugin_tools())
            .collect()
    }
}

impl ExtensionManager {
    pub fn new(cfg: ManagerConfig) -> Arc<Self> {
        for r in &cfg.allow.rejected {
            tracing::warn!("extensions.allow: {r}");
        }
        Arc::new(Self {
            sandbox: RwLock::new(cfg.sandbox.clone()),
            configured: tokio::sync::Mutex::new(Vec::new()),
            store: LockStore::new(cfg.layout.lock_path()),
            queue: PendingQueue::new(cfg.layout.pending_dir()),
            cfg,
            approver: RwLock::new(Arc::new(QueueApprover)),
            live: RwLock::new(BTreeMap::new()),
            ops: tokio::sync::Mutex::new(()),
            next_id: AtomicU64::new(1),
            last_lock: Mutex::new(None),
            plugins: RwLock::new(BTreeMap::new()),
            no_plugin_runtime: AtomicBool::new(false),
        })
    }

    pub fn set_approver(&self, approver: Arc<dyn Approver>) {
        *self.approver.write().unwrap() = approver;
    }

    pub fn layout(&self) -> &Layout {
        &self.cfg.layout
    }

    /// The live skill set the manager refreshes, if it was given one.
    pub fn skills(&self) -> Option<&SkillsHandle> {
        self.cfg.skills.as_ref()
    }

    pub fn queue(&self) -> &PendingQueue {
        &self.queue
    }

    pub fn lock(&self) -> Result<LockFile> {
        self.store.load()
    }

    /// Connect the owner's configured servers, then load every active
    /// lock entry. A server that fails is logged and skipped.
    pub async fn start(self: &Arc<Self>, configured: Vec<McpServerConfig>) {
        *self.configured.lock().await = configured.clone();
        for cfg in configured {
            let name = cfg.name.clone();
            if let Err(e) = self.add_server(cfg).await {
                tracing::warn!(server = %name, "mcp server `{name}` failed to start, continuing without it: {e}");
            }
        }
        if let Err(e) = self.sync().await {
            tracing::error!("extensions not loaded: {e}");
        }
    }

    /// Re-read the lock every [`SYNC_EVERY`] for as long as the manager
    /// lives: what the owner approves, resumes or removes from the CLI
    /// reaches this process's sessions.
    pub fn spawn_sync(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut last_err = String::new();
            loop {
                tokio::time::sleep(SYNC_EVERY).await;
                let Some(me) = weak.upgrade() else { return };
                match me.sync().await {
                    Ok(()) => last_err.clear(),
                    Err(e) => {
                        let e = e.to_string();
                        if e != last_err {
                            tracing::error!("extensions sync: {e}");
                            last_err = e;
                        }
                    }
                }
            }
        })
    }

    /// Stop every server, for a clean exit.
    pub async fn shutdown_all(&self) {
        let all: Vec<_> = std::mem::take(&mut *self.live.write().unwrap())
            .into_values()
            .collect();
        for l in all {
            l.client.shutdown().await;
        }
    }

    // ---- the M17 hook -------------------------------------------------

    /// Connect an owner-configured server mid-run (M17's MCP hot-add). It
    /// is scanned: a block hit is a loud warning, not a refusal, since the
    /// owner chose it; a later `list_changed` that brings a flagged or
    /// changed tool drops that tool. Returns the tool names now offered.
    pub async fn add_server(self: &Arc<Self>, cfg: McpServerConfig) -> Result<Vec<String>> {
        let _ops = self.ops.lock().await;
        let name = cfg.name.clone();
        if self.live.read().unwrap().contains_key(&name) {
            return Err(refused(format!(
                "an mcp server named `{name}` is already running"
            )));
        }
        let (client, infos) = self.start_client(cfg).await?;
        let surface = Surface::of(&infos, None, &[]);
        if !surface.findings.is_empty() {
            tracing::warn!(
                server = %name,
                "configured mcp server `{name}`: the description scan flagged its tools (loaded anyway, you configured it):\n{}",
                scan::report_for_owner(&surface.findings)
            );
        }
        Ok(self.register(&name, client, infos, surface.digests, vec![], false, None))
    }

    /// Bring the configured servers in line with `servers`, the config as
    /// just re-read: start the new ones, stop the ones gone, restart the
    /// ones whose entry changed. Diffed against the list last applied, not
    /// against what runs, so a server that failed to start is retried only
    /// once its entry changes. What changed, for the log.
    pub async fn set_configured(self: &Arc<Self>, servers: Vec<McpServerConfig>) -> Vec<String> {
        let mut applied = self.configured.lock().await;
        let mut changes = Vec::new();
        for old in applied.iter() {
            let now = servers.iter().find(|s| s.name == old.name);
            if now == Some(old) {
                continue;
            }
            let _ops = self.ops.lock().await;
            let configured = self
                .live
                .read()
                .unwrap()
                .get(&old.name)
                .is_some_and(|l| !l.managed);
            if configured {
                self.unload(&old.name).await;
            }
            if now.is_none() {
                changes.push(format!("stopped `{}`", old.name));
            }
        }
        for cfg in &servers {
            let before = applied.iter().find(|s| s.name == cfg.name);
            if before == Some(cfg) {
                continue;
            }
            let name = cfg.name.clone();
            let verb = if before.is_some() {
                "restarted"
            } else {
                "started"
            };
            match self.add_server(cfg.clone()).await {
                Ok(tools) => changes.push(format!("{verb} `{name}` ({})", tools.join(", "))),
                Err(e) => {
                    tracing::warn!(server = %name, "mcp server `{name}` failed to start, continuing without it: {e}");
                    changes.push(format!("`{name}` failed to start: {e}"));
                }
            }
        }
        *applied = servers;
        changes
    }

    /// The sandbox new servers get from now on. Running ones keep theirs;
    /// plugins, which run in-process, switch at once.
    pub fn set_sandbox(&self, sandbox: Arc<Sandbox>) {
        *self.sandbox.write().unwrap() = sandbox;
        self.rebuild_plugin_tools();
    }

    pub fn sandbox(&self) -> Arc<Sandbox> {
        self.sandbox.read().unwrap().clone()
    }

    /// Start `cfg`'s server the way it would run, list its tools (less the
    /// ones `enabled_tools` leaves out), scan them, and stop it again.
    /// Nothing is registered or recorded: `ferrule mcp add`'s smoke test.
    pub async fn probe(&self, cfg: McpServerConfig) -> Result<Probe> {
        let (client, infos) = self.start_client(cfg).await?;
        let surface = Surface::of(&infos, None, &[]);
        let probe = Probe {
            blocked: surface.blocked.iter().cloned().collect(),
            findings: surface.findings,
            sandbox_degraded: client.sandbox_degraded().map(str::to_string),
            tools: infos,
        };
        client.shutdown().await;
        Ok(probe)
    }

    // ---- installs -------------------------------------------------------

    /// The model's `mcp_add`.
    pub async fn install_mcp(self: &Arc<Self>, req: McpRequest) -> Result<Outcome> {
        let _ops = self.ops.lock().await;
        let source = self.check_mcp_request(&req, true)?;
        if !self.cfg.allow.covers(&source) {
            let reason = format!("{source} is not on the allow-list");
            return self.ask_owner(Request::Mcp(req), &reason).await;
        }
        let prepared = self.prepare_mcp(req.clone(), Some(&self.cfg.allow)).await?;
        if !prepared.surface.blocked.is_empty() {
            let findings = prepared.surface.findings.clone();
            let summary = scan::summary_for_model(&prepared.surface.unwaived_blocks());
            self.abort(prepared).await;
            let p = self.queue.add(
                Request::Mcp(req),
                "the description scan flagged it",
                findings,
            )?;
            return Err(refused(format!(
                "the scan flagged {summary}; nothing was installed. The owner can review it as {}",
                p.id
            )));
        }
        self.commit(prepared, Origin::Agent, vec![]).await
    }

    /// The model's `skill_install`.
    pub async fn install_skill(self: &Arc<Self>, req: SkillRequest) -> Result<Outcome> {
        let _ops = self.ops.lock().await;
        let source = parse_skill_source(&req.source)?;
        if !self.cfg.allow.covers(&source) {
            let reason = format!("{source} is not on the allow-list");
            return self.ask_owner(Request::Skill(req), &reason).await;
        }
        let prepared = self.prepare_skill(&req, Some(&self.cfg.allow))?;
        let blocks: Vec<Finding> = scan::blocks(&prepared.candidate.findings)
            .cloned()
            .collect();
        if !blocks.is_empty() {
            let findings = prepared.candidate.findings.clone();
            prepared.cleanup();
            let p = self.queue.add(
                Request::Skill(req),
                "the description scan flagged it",
                findings,
            )?;
            return Err(refused(format!(
                "the scan flagged {}; nothing was installed. The owner can review it as {}",
                scan::summary_for_model(&blocks),
                p.id
            )));
        }
        self.commit_skill(prepared, Origin::Agent, vec![], req.replace)
    }

    /// The model's `skill_keep`: a draft the agent wrote in the workspace,
    /// kept once its check passes and it scans clean.
    pub async fn keep_skill(
        self: &Arc<Self>,
        name: &str,
        check: &str,
        workspace: &Path,
        replace: bool,
    ) -> Result<Outcome> {
        source::validate_name(name)?;
        let draft = workspace.join(".ferrule").join("skill-drafts").join(name);
        if !draft.join("SKILL.md").is_file() {
            return Err(refused(format!(
                "write the skill to .ferrule/skill-drafts/{name}/SKILL.md first"
            )));
        }
        let candidate = skill::inspect(&draft)?;
        if candidate.name != name {
            return Err(refused(format!(
                "the draft's SKILL.md names itself `{}`, not `{name}`",
                candidate.name
            )));
        }
        if check.trim().is_empty() {
            return Err(refused(
                "a skill is kept only with a check that exercises it",
            ));
        }
        // Run in the sandbox with the draft as its workspace: it can write
        // there and nowhere else it couldn't already.
        let verifier = ferrule_tools::CommandVerifier::new(check, self.sandbox(), CHECK_TIMEOUT);
        let ctx = ToolContext {
            workspace: dunce::canonicalize(&draft)?,
            max_output_chars: 4_000,
        };
        if let Err(out) = verifier.verify(&ctx).await {
            return Err(refused(format!(
                "the check failed, the skill was not kept:\n{out}"
            )));
        }
        // The check may have written into the draft: inspect what is kept.
        let candidate = skill::inspect(&draft)?;
        let blocks: Vec<Finding> = scan::blocks(&candidate.findings).cloned().collect();
        if !blocks.is_empty() {
            return Err(refused(format!(
                "the scan flagged {}; the skill was not kept",
                scan::summary_for_model(&blocks)
            )));
        }
        let _ops = self.ops.lock().await;
        self.commit_skill(
            PreparedSkill {
                source: format!("self:{name}"),
                pin: None,
                candidate,
                clone: None,
            },
            Origin::SelfWritten,
            vec![],
            replace,
        )
    }

    // ---- removal --------------------------------------------------------

    /// Remove an installed server. The model may remove only what the agent
    /// installed; the owner (`by_owner`) anything in the lock. Configured
    /// servers are the config file's business.
    pub async fn remove_server(&self, name: &str, by_owner: bool, purge: bool) -> Result<()> {
        let _ops = self.ops.lock().await;
        let lock = self.store.load()?;
        let Some(entry) = lock.servers.get(name) else {
            return Err(refused(if self.live.read().unwrap().contains_key(name) {
                format!(
                    "`{name}` is configured by the owner, not installed; it can't be removed here"
                )
            } else {
                format!("no installed mcp server named `{name}`")
            }));
        };
        if !by_owner && entry.origin == Origin::Owner {
            return Err(refused(format!(
                "`{name}` was installed by the owner; only they can remove it"
            )));
        }
        self.store.update(|l| {
            l.servers.remove(name);
            Ok(())
        })?;
        self.unload(name).await;
        if let Some(co) = &entry.checkout {
            remove_checkout(&self.cfg.layout, co);
        }
        if purge {
            let _ = fs::remove_dir_all(self.cfg.layout.mcp_state(name));
        }
        Ok(())
    }

    pub async fn remove_skill(&self, name: &str, by_owner: bool) -> Result<()> {
        let _ops = self.ops.lock().await;
        let lock = self.store.load()?;
        let Some(entry) = lock.skills.get(name) else {
            return Err(refused(format!("no installed skill named `{name}`")));
        };
        if !by_owner && entry.origin == Origin::Owner {
            return Err(refused(format!(
                "`{name}` was installed by the owner; only they can remove it"
            )));
        }
        self.store.update(|l| {
            l.skills.remove(name);
            let _ = fs::remove_dir_all(self.cfg.layout.skills_dir().join(name));
            let _ = fs::remove_dir_all(self.cfg.layout.suspended_skills_dir().join(name));
            Ok(())
        })?;
        self.refresh_skills();
        Ok(())
    }

    // ---- the owner's side ------------------------------------------------

    /// Fetch, start and scan a pending request in this process, show it to
    /// the owner through `confirm`, and install it on yes. Block hits the
    /// owner confirms become waivers, bound to the surface they saw.
    pub async fn approve(
        self: &Arc<Self>,
        id: &str,
        confirm: impl FnOnce(&Review) -> bool,
    ) -> Result<Outcome> {
        let _ops = self.ops.lock().await;
        self.approve_locked(id, confirm).await
    }

    async fn approve_locked(
        self: &Arc<Self>,
        id: &str,
        confirm: impl FnOnce(&Review) -> bool,
    ) -> Result<Outcome> {
        let pending = self
            .queue
            .get(id)?
            .ok_or_else(|| refused(format!("no pending request `{id}`")))?;
        let out = match pending.request.clone() {
            Request::Mcp(req) => {
                self.check_mcp_request(&req, false)?;
                let prepared = self.prepare_mcp(req, None).await?;
                let review = Review {
                    what: pending.request.describe(),
                    items: prepared.infos.iter().map(|i| i.name.clone()).collect(),
                    findings: prepared.surface.findings.clone(),
                    sandbox_degraded: prepared.client.sandbox_degraded().map(str::to_string),
                    capabilities: vec![],
                };
                if !confirm(&review) {
                    self.abort(prepared).await;
                    return Err(refused("not approved; nothing was installed"));
                }
                let waivers = waivers_for(
                    &prepared.surface.unwaived_blocks(),
                    &prepared.surface.digests,
                );
                self.commit(prepared, Origin::Agent, waivers).await?
            }
            Request::Skill(req) => {
                let prepared = self.prepare_skill(&req, None)?;
                let review = Review {
                    what: pending.request.describe(),
                    items: vec![prepared.candidate.name.clone()],
                    findings: prepared.candidate.findings.clone(),
                    sandbox_degraded: None,
                    capabilities: vec![],
                };
                if !confirm(&review) {
                    prepared.cleanup();
                    return Err(refused("not approved; nothing was installed"));
                }
                let waivers = skill_waivers(&prepared.candidate);
                self.commit_skill(prepared, Origin::Agent, waivers, req.replace)?
            }
            Request::Plugin(req) => self.approve_plugin(req, confirm).await?,
        };
        self.queue.remove(id)?;
        Ok(out)
    }

    /// Drop a pending request. Nothing of it was ever fetched.
    pub fn deny(&self, id: &str) -> Result<bool> {
        self.queue.remove(id)
    }

    /// Re-scan a suspended server or skill, show it to the owner, and make
    /// it active again (with its current surface approved) on yes.
    pub async fn resume(
        self: &Arc<Self>,
        name: &str,
        confirm: impl FnOnce(&Review) -> bool,
    ) -> Result<()> {
        let _ops = self.ops.lock().await;
        let lock = self.store.load()?;
        if let Some(entry) = lock.servers.get(name).cloned() {
            if entry.status != Status::Suspended {
                return Err(refused(format!("`{name}` is not suspended")));
            }
            if let (Some(co), Some(pin)) = (&entry.checkout, &entry.pin) {
                crate::git::verify(co, pin).map_err(|e| {
                    refused(format!(
                        "{e}; the checkout can't be trusted — remove and reinstall it"
                    ))
                })?;
            }
            let (client, infos) = self.start_client(server_config(name, &entry)).await?;
            let surface = Surface::of(&infos, None, &entry.waivers);
            let review = Review {
                what: format!("resume MCP server `{name}` from {}", entry.source),
                items: infos.iter().map(|i| i.name.clone()).collect(),
                findings: surface.findings.clone(),
                sandbox_degraded: client.sandbox_degraded().map(str::to_string),
                capabilities: vec![],
            };
            if !confirm(&review) {
                client.shutdown().await;
                return Err(refused("not resumed"));
            }
            let mut waivers = entry.waivers.clone();
            waivers.extend(waivers_for(&surface.unwaived_blocks(), &surface.digests));
            let updated = self.store.update(|l| {
                let e = l
                    .servers
                    .get_mut(name)
                    .ok_or_else(|| refused(format!("`{name}` was removed meanwhile")))?;
                e.status = Status::Active;
                e.reason = None;
                e.tools = surface.digests.clone();
                e.waivers = waivers.clone();
                Ok(e.clone())
            })?;
            self.register(
                name,
                client,
                infos,
                surface.digests,
                waivers,
                true,
                Some(updated),
            );
            return Ok(());
        }
        if let Some(entry) = lock.skills.get(name).cloned() {
            if entry.status != Status::Suspended {
                return Err(refused(format!("`{name}` is not suspended")));
            }
            let dir = self.cfg.layout.suspended_skills_dir().join(name);
            let candidate = skill::inspect(&dir)?;
            let review = Review {
                what: format!("resume skill `{name}` from {}", entry.source),
                items: vec![name.to_string()],
                findings: candidate.findings.clone(),
                sandbox_degraded: None,
                capabilities: vec![],
            };
            if !confirm(&review) {
                return Err(refused("not resumed"));
            }
            let waivers = skill_waivers(&candidate);
            self.store.update(|l| {
                let e = l
                    .skills
                    .get_mut(name)
                    .ok_or_else(|| refused(format!("`{name}` was removed meanwhile")))?;
                skill::replace_dir(&dir, &self.cfg.layout.skills_dir().join(name))?;
                e.status = Status::Active;
                e.reason = None;
                e.digest = candidate.digest.clone();
                e.waivers = waivers;
                Ok(())
            })?;
            self.refresh_skills();
            return Ok(());
        }
        if let Some(entry) = lock.plugins.get(name).cloned() {
            return self.resume_plugin(name, entry, confirm);
        }
        Err(refused(format!("nothing installed is named `{name}`")))
    }

    /// Everything the manager knows of: live servers, the lock, the queue
    /// size is separate ([`PendingQueue::list`]).
    pub fn list(&self) -> Result<Vec<Listed>> {
        let lock = self.store.load()?;
        let live = self.live.read().unwrap();
        let mut out = Vec::new();
        for (name, l) in live.iter().filter(|(_, l)| !l.managed) {
            out.push(Listed {
                name: name.clone(),
                kind: "server",
                origin: "configured".into(),
                source: l.client.config().command.clone(),
                pin: None,
                status: "active".into(),
                reason: None,
                tools: l.approved.keys().cloned().collect(),
            });
        }
        for (name, e) in &lock.servers {
            let loaded = live.get(name).filter(|l| l.managed);
            out.push(Listed {
                name: name.clone(),
                kind: "server",
                origin: e.origin.as_str().into(),
                source: e.source.clone(),
                pin: e.pin.clone(),
                status: match (e.status, loaded) {
                    (Status::Suspended, _) => "suspended",
                    (Status::Active, Some(_)) => "active",
                    (Status::Active, None) => "not loaded",
                }
                .into(),
                reason: e.reason.clone(),
                tools: e.tools.keys().cloned().collect(),
            });
        }
        for (name, e) in &lock.skills {
            out.push(Listed {
                name: name.clone(),
                kind: "skill",
                origin: e.origin.as_str().into(),
                source: e.source.clone(),
                pin: e.pin.clone(),
                status: match e.status {
                    Status::Active => "active",
                    Status::Suspended => "suspended",
                }
                .into(),
                reason: e.reason.clone(),
                tools: vec![],
            });
        }
        drop(live);
        self.list_plugins(&lock, &mut out);
        Ok(out)
    }

    // ---- the lock, re-read ------------------------------------------------

    /// Bring this process in line with the lock: load active entries that
    /// aren't live, unload ones gone or suspended, reload ones the owner
    /// changed, and check installed skills against their digests.
    pub async fn sync(self: &Arc<Self>) -> Result<()> {
        let _ops = self.ops.lock().await;
        let lock = self.store.load()?;
        for (name, entry) in &lock.servers {
            let current = self
                .live
                .read()
                .unwrap()
                .get(name)
                .map(|l| (l.managed, l.entry.clone()));
            match current {
                Some((false, _)) => {
                    tracing::warn!(server = %name, "installed mcp server `{name}` has the name of a configured one; not loaded");
                    continue;
                }
                Some((true, Some(old))) if same_launch(&old, entry) => {
                    if entry.status == Status::Active {
                        if let Some(l) = self.live.write().unwrap().get_mut(name) {
                            l.waivers = entry.waivers.clone();
                        }
                        continue;
                    }
                    self.unload(name).await;
                }
                Some(_) => self.unload(name).await,
                None => {}
            }
            if entry.status == Status::Active {
                self.load_entry(name, entry).await;
            }
        }
        let gone: Vec<String> = self
            .live
            .read()
            .unwrap()
            .iter()
            .filter(|(n, l)| l.managed && !lock.servers.contains_key(*n))
            .map(|(n, _)| n.clone())
            .collect();
        for name in gone {
            self.unload(&name).await;
        }
        self.sync_plugins(&lock);

        let skills_changed = {
            let mut last = self.last_lock.lock().unwrap();
            let changed = last.as_ref().map(|l| &l.skills) != Some(&lock.skills);
            *last = Some(lock.clone());
            changed
        };
        if skills_changed || self.skill_anomaly(&lock) {
            self.verify_skills()?;
            self.refresh_skills();
        }
        Ok(())
    }

    /// Load one active lock entry: verify the checkout, start, re-list and
    /// compare with the approved surface. Anything off suspends it.
    async fn load_entry(self: &Arc<Self>, name: &str, entry: &ServerEntry) {
        if let (Some(co), Some(pin)) = (&entry.checkout, &entry.pin) {
            if let Err(e) = crate::git::verify(co, pin) {
                self.mark_suspended(name, &format!("its checkout doesn't match the pin: {e}"));
                return;
            }
        }
        let (client, infos) = match self.start_client(server_config(name, entry)).await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(server = %name, "installed mcp server `{name}` failed to start, continuing without it: {e}");
                return;
            }
        };
        let surface = Surface::of(&infos, Some(&entry.tools), &entry.waivers);
        if let Some(reason) = suspend_reason(&surface) {
            client.shutdown().await;
            self.mark_suspended(name, &reason);
            return;
        }
        let mut entry = entry.clone();
        if surface.digests != entry.tools {
            // Only additions or removals got here: accept them.
            match self.store.update(|l| {
                if let Some(e) = l.servers.get_mut(name) {
                    e.tools = surface.digests.clone();
                }
                Ok(())
            }) {
                Ok(()) => entry.tools = surface.digests.clone(),
                Err(e) => {
                    tracing::warn!(server = %name, "couldn't record `{name}`'s new tools: {e}")
                }
            }
        }
        let waivers = entry.waivers.clone();
        self.register(
            name,
            client,
            infos,
            surface.digests,
            waivers,
            true,
            Some(entry),
        );
    }

    /// Cheap pre-check, no lock taken: an active skill whose directory is
    /// missing or whose digest is off, or a directory with no lock entry.
    fn skill_anomaly(&self, lock: &LockFile) -> bool {
        let dir = self.cfg.layout.skills_dir();
        let on_disk = dir_names(&dir);
        on_disk.iter().any(|n| {
            lock.skills
                .get(n)
                .is_none_or(|e| e.status != Status::Active)
        }) || lock.skills.iter().any(|(n, e)| {
            e.status == Status::Active
                && skill::skill_md_digest(&dir.join(n)).ok().as_ref() != Some(&e.digest)
        })
    }

    /// Under the lock's guard, so a concurrent install (which moves its dir
    /// in under the same guard) is never mistaken for a stray: suspend
    /// active skills that changed or vanished, and move out directories no
    /// entry accounts for.
    fn verify_skills(&self) -> Result<()> {
        let layout = &self.cfg.layout;
        let (skills, suspended) = (layout.skills_dir(), layout.suspended_skills_dir());
        let stray = self.store.update(|l| {
            let mut stray = Vec::new();
            for (name, e) in l.skills.iter_mut() {
                if e.status != Status::Active {
                    continue;
                }
                let dir = skills.join(name);
                let digest = skill::skill_md_digest(&dir).ok();
                if digest.as_ref() == Some(&e.digest) {
                    continue;
                }
                let reason = if digest.is_none() {
                    "its directory is missing"
                } else {
                    "its SKILL.md changed after it was installed"
                };
                tracing::warn!(skill = %name, "installed skill `{name}` suspended: {reason}");
                if dir.exists() {
                    skill::replace_dir(&dir, &suspended.join(name))?;
                }
                e.status = Status::Suspended;
                e.reason = Some(reason.into());
            }
            for name in dir_names(&skills) {
                if l.skills
                    .get(&name)
                    .is_none_or(|e| e.status != Status::Active)
                {
                    stray.push(name);
                }
            }
            for name in &stray {
                let to = suspended.join(format!("unlisted-{name}"));
                skill::replace_dir(&skills.join(name), &to)?;
            }
            Ok(stray)
        })?;
        for name in stray {
            tracing::warn!(skill = %name, "a skill directory `{name}` with no lock entry was moved out of the installed skills");
        }
        Ok(())
    }

    // ---- internals ---------------------------------------------------------

    /// Name, source and collisions, before anything is fetched.
    fn check_mcp_request(&self, req: &McpRequest, from_model: bool) -> Result<Source> {
        source::validate_name(&req.name)?;
        let src = Source::parse(&req.source).map_err(refused)?;
        if matches!(src.kind, Kind::Npm | Kind::Pypi) {
            src.require_exact_version().map_err(refused)?;
        }
        if src.kind != Kind::Git && req.command.is_some() {
            return Err(refused("`command` is only for git sources"));
        }
        if src.kind == Kind::Git && req.command.is_none() {
            return Err(refused("a git source needs `command`: the server file (or interpreter and script) inside the repo"));
        }
        let live = self.live.read().unwrap();
        if live.get(&req.name).is_some_and(|l| !l.managed) {
            return Err(refused(format!(
                "`{}` is the name of a configured mcp server",
                req.name
            )));
        }
        drop(live);
        if let Some(e) = self.store.load()?.servers.get(&req.name) {
            if !req.replace {
                return Err(refused(format!(
                    "`{}` is already installed; pass replace=true to reinstall it",
                    req.name
                )));
            }
            if from_model && e.origin == Origin::Owner {
                return Err(refused(format!(
                    "`{}` was installed by the owner",
                    req.name
                )));
            }
        }
        Ok(src)
    }

    /// Queue the request and ask the approver. A yes at the prompt installs
    /// it now, provided the scan is clean; a flagged one stays queued for
    /// the owner's full review (`ferrule extensions approve`).
    async fn ask_owner(self: &Arc<Self>, request: Request, reason: &str) -> Result<Outcome> {
        let p = self.queue.add(request, reason, vec![])?;
        let approver = self.approver.read().unwrap().clone();
        match approver.decide(&p).await {
            None => Ok(Outcome::Pending { id: p.id }),
            Some(false) => {
                self.queue.remove(&p.id)?;
                Err(refused("the owner denied it"))
            }
            Some(true) => {
                let mut flagged = None;
                let out = self
                    .approve_locked(&p.id, |r| {
                        let blocks: Vec<Finding> = scan::blocks(&r.findings).cloned().collect();
                        if !blocks.is_empty() {
                            flagged = Some(scan::summary_for_model(&blocks));
                        }
                        blocks.is_empty()
                    })
                    .await;
                match (out, flagged) {
                    (Ok(o), _) => Ok(o),
                    (Err(_), Some(summary)) => Err(refused(format!(
                        "the owner allowed the source, but the scan flagged {summary}; nothing was installed. It waits as {} for the owner's review",
                        p.id
                    ))),
                    (Err(e), None) => {
                        // Fetching or starting failed: nothing to keep queued.
                        let _ = self.queue.remove(&p.id);
                        Err(e)
                    }
                }
            }
        }
    }

    /// Materialise, start and scan. `allow` given: the pin must satisfy it.
    async fn prepare_mcp(&self, req: McpRequest, allow: Option<&AllowList>) -> Result<Prepared> {
        let src = Source::parse(&req.source).map_err(refused)?;
        let not_allowed = |pin: &str| {
            refused(format!(
                "{} at {pin} is not allowed by the allow-list's version pin",
                src
            ))
        };
        let mut cfg = McpServerConfig {
            name: req.name.clone(),
            sandbox: true,
            ..Default::default()
        };
        let mut checkout = None;
        let pin = match src.kind {
            Kind::Npm | Kind::Pypi => {
                let v = src.require_exact_version().map_err(refused)?.to_string();
                if allow.is_some_and(|a| !a.permits(&src, &v)) {
                    return Err(not_allowed(&v));
                }
                let (c, a) = if src.kind == Kind::Npm {
                    source::npm_launch(&src.locator, &v, &req.args)
                } else {
                    source::pypi_launch(&src.locator, &v, &req.args)
                };
                (cfg.command, cfg.args) = (c, a);
                Some(v)
            }
            Kind::Git => {
                let (dir, created, sha) = self.fetch_checkout(&req.name, &src, allow)?;
                let command = req.command.as_deref().unwrap_or("");
                match source::git_launch(&dir, command, &req.args) {
                    Ok((c, a)) => (cfg.command, cfg.args) = (c, a),
                    Err(e) => {
                        if created {
                            remove_checkout(&self.cfg.layout, &dir);
                        }
                        return Err(e);
                    }
                }
                checkout = Some((dir, created));
                Some(sha)
            }
            Kind::Url => {
                if allow.is_some_and(|a| !a.permits(&src, "")) {
                    return Err(not_allowed("-"));
                }
                cfg.url = Some(src.locator.clone());
                None
            }
        };
        let (client, infos) = match self.start_client(cfg.clone()).await {
            Ok(x) => x,
            Err(e) => {
                if let Some((dir, true)) = &checkout {
                    remove_checkout(&self.cfg.layout, dir);
                }
                return Err(e);
            }
        };
        let surface = Surface::of(&infos, None, &[]);
        Ok(Prepared {
            req,
            source: src,
            pin,
            cfg,
            client,
            infos,
            surface,
            checkout,
        })
    }

    /// Clone into staging, pin, check the allow-list's commit pin, and move
    /// it to its checkout dir. Returns (dir, created by this call, sha).
    fn fetch_checkout(
        &self,
        name: &str,
        src: &Source,
        allow: Option<&AllowList>,
    ) -> Result<(PathBuf, bool, String)> {
        let staging = self.cfg.layout.staging();
        fs::create_dir_all(&staging)?;
        let tmp = staging.join(uuid::Uuid::new_v4().simple().to_string());
        let sha = match crate::git::fetch_pinned(&src.locator, src.version.as_deref(), &tmp) {
            Ok(sha) => sha,
            Err(e) => {
                let _ = fs::remove_dir_all(&tmp);
                return Err(e);
            }
        };
        if allow.is_some_and(|a| !a.permits(src, &sha)) {
            let _ = fs::remove_dir_all(&tmp);
            return Err(refused(format!(
                "{src} resolved to {sha}, which the allow-list's commit pin doesn't allow"
            )));
        }
        let dest = self.cfg.layout.checkout(name, &sha);
        if dest.exists() {
            if crate::git::verify(&dest, &sha).is_ok() {
                let _ = fs::remove_dir_all(&tmp);
                return Ok((dest, false, sha));
            }
            fs::remove_dir_all(&dest)?;
        }
        if let Some(p) = dest.parent() {
            fs::create_dir_all(p)?;
        }
        fs::rename(&tmp, &dest)?;
        Ok((dest, true, sha))
    }

    async fn start_client(
        &self,
        cfg: McpServerConfig,
    ) -> Result<(Arc<McpClient>, Vec<McpToolInfo>)> {
        let name = cfg.name.clone();
        let state_dir = self.cfg.layout.mcp_state(&name);
        fs::create_dir_all(&state_dir)?;
        let host = ServerHost {
            sandbox: self.sandbox(),
            workspace: self.cfg.workspace.clone(),
            state_dir,
        };
        let client = Arc::new(McpClient::new(cfg, host)?);
        if let Some(reason) = client.sandbox_degraded() {
            tracing::warn!(server = %name, "mcp server `{name}` runs UNSANDBOXED: {reason}");
        }
        match list_enabled(&client).await {
            Ok(infos) => Ok((client, infos)),
            Err(e) => {
                client.shutdown().await;
                Err(e.into())
            }
        }
    }

    async fn abort(&self, p: Prepared) {
        p.client.shutdown().await;
        if let Some((dir, true)) = &p.checkout {
            remove_checkout(&self.cfg.layout, dir);
        }
    }

    async fn commit(
        self: &Arc<Self>,
        p: Prepared,
        origin: Origin,
        waivers: Vec<Waiver>,
    ) -> Result<Outcome> {
        let name = p.req.name.clone();
        let entry = ServerEntry {
            source: p.source.to_string(),
            pin: p.pin.clone(),
            command: p.cfg.command.clone(),
            args: p.cfg.args.clone(),
            url: p.cfg.url.clone(),
            checkout: p.checkout.as_ref().map(|(d, _)| d.clone()),
            origin,
            installed_at: lock::now(),
            status: Status::Active,
            reason: None,
            tools: p.surface.digests.clone(),
            waivers: waivers.clone(),
        };
        let replace = p.req.replace;
        let old = self.store.update(|l| {
            let old = l.servers.get(&name).cloned();
            if old.is_some() && !replace {
                return Err(refused(format!("`{name}` is already installed")));
            }
            l.servers.insert(name.clone(), entry.clone());
            Ok(old)
        });
        let old = match old {
            Ok(old) => old,
            Err(e) => {
                self.abort(p).await;
                return Err(e);
            }
        };
        self.unload(&name).await;
        if let Some(old_co) = old.and_then(|o| o.checkout) {
            if Some(&old_co) != entry.checkout.as_ref() {
                remove_checkout(&self.cfg.layout, &old_co);
            }
        }
        let warnings = p.surface.warnings();
        if warnings > 0 {
            tracing::warn!(server = %name, "mcp server `{name}` installed with scan warnings:\n{}", scan::report_for_owner(&p.surface.findings));
        }
        let tools = self.register(
            &name,
            p.client,
            p.infos,
            p.surface.digests,
            waivers,
            true,
            Some(entry),
        );
        Ok(Outcome::Installed {
            name,
            tools,
            warnings,
        })
    }

    /// Make a started server's tools live and watch it for `list_changed`.
    #[allow(clippy::too_many_arguments)]
    fn register(
        self: &Arc<Self>,
        name: &str,
        client: Arc<McpClient>,
        infos: Vec<McpToolInfo>,
        approved: BTreeMap<String, String>,
        waivers: Vec<Waiver>,
        managed: bool,
        entry: Option<ServerEntry>,
    ) -> Vec<String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let tools = build_tools(&client, infos);
        let names: Vec<String> = tools.iter().map(|t| t.definition().name).collect();
        let mut rx = client.subscribe_list_changed();
        self.live.write().unwrap().insert(
            name.to_string(),
            Live {
                id,
                client,
                tools,
                managed,
                approved,
                waivers,
                entry,
            },
        );
        // Holds only a weak manager and the receiver: once the server is
        // unloaded and its client dropped, `changed()` ends and so does this.
        let weak: Weak<Self> = Arc::downgrade(self);
        let name = name.to_string();
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let Some(me) = weak.upgrade() else { return };
                if !me.on_list_changed(&name, id).await {
                    return;
                }
            }
        });
        names
    }

    /// Re-list, diff and re-scan after `notifications/tools/list_changed`.
    /// False once this server (generation `id`) is no longer live.
    async fn on_list_changed(&self, name: &str, id: u64) -> bool {
        let _ops = self.ops.lock().await;
        let Some((client, approved, waivers, managed)) = self
            .live
            .read()
            .unwrap()
            .get(name)
            .filter(|l| l.id == id)
            .map(|l| {
                (
                    l.client.clone(),
                    l.approved.clone(),
                    l.waivers.clone(),
                    l.managed,
                )
            })
        else {
            return false;
        };
        let infos = match list_enabled(&client).await {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(server = %name, "re-listing `{name}` after list_changed failed: {e}");
                return true;
            }
        };
        let surface = Surface::of(&infos, Some(&approved), &waivers);
        if managed {
            if let Some(reason) = suspend_reason(&surface) {
                tracing::warn!(server = %name, "installed mcp server `{name}` suspended mid-session: {reason}\n{}", scan::report_for_owner(&surface.findings));
                // Record the suspension before tearing the server down: the
                // client's shutdown can take a while, and a crash in between
                // must not let a restart bring the flagged server back.
                self.mark_suspended(name, &reason);
                self.unload(name).await;
                return false;
            }
            if let Err(e) = self.store.update(|l| {
                if let Some(e) = l.servers.get_mut(name) {
                    e.tools = surface.digests.clone();
                }
                Ok(())
            }) {
                tracing::warn!(server = %name, "couldn't record `{name}`'s new tools, not loading them: {e}");
                return true;
            }
            self.replace_tools(name, id, &client, infos, surface.digests);
        } else {
            let (keep, dropped): (Vec<_>, Vec<_>) = infos.into_iter().partition(|i| {
                !surface.blocked.contains(&i.name) && !surface.changed.contains(&i.name)
            });
            if !dropped.is_empty() {
                let names: Vec<_> = dropped.iter().map(|i| i.name.as_str()).collect();
                tracing::warn!(server = %name, "configured mcp server `{name}` changed its tools mid-session; dropped {names:?} (flagged or changed since start):\n{}", scan::report_for_owner(&surface.findings));
            }
            let mut digests = approved;
            for i in &keep {
                digests.insert(i.name.clone(), surface.digests[&i.name].clone());
            }
            self.replace_tools(name, id, &client, keep, digests);
        }
        true
    }

    fn replace_tools(
        &self,
        name: &str,
        id: u64,
        client: &Arc<McpClient>,
        infos: Vec<McpToolInfo>,
        approved: BTreeMap<String, String>,
    ) {
        let tools = build_tools(client, infos);
        if let Some(l) = self
            .live
            .write()
            .unwrap()
            .get_mut(name)
            .filter(|l| l.id == id)
        {
            l.tools = tools;
            l.approved = approved;
        }
    }

    async fn unload(&self, name: &str) {
        let gone = self.live.write().unwrap().remove(name);
        if let Some(l) = gone {
            l.client.shutdown().await;
        }
    }

    fn mark_suspended(&self, name: &str, reason: &str) {
        tracing::warn!(server = %name, "installed mcp server `{name}` suspended: {reason}");
        if let Err(e) = self.store.update(|l| {
            if let Some(e) = l.servers.get_mut(name) {
                e.status = Status::Suspended;
                e.reason = Some(reason.into());
            }
            Ok(())
        }) {
            tracing::error!(server = %name, "couldn't record `{name}` as suspended: {e}");
        }
    }

    fn prepare_skill(
        &self,
        req: &SkillRequest,
        allow: Option<&AllowList>,
    ) -> Result<PreparedSkill> {
        let src = parse_skill_source(&req.source)?;
        let staging = self.cfg.layout.staging();
        fs::create_dir_all(&staging)?;
        let clone = staging.join(uuid::Uuid::new_v4().simple().to_string());
        let cleanup = |e: ExtError| {
            let _ = fs::remove_dir_all(&clone);
            e
        };
        let sha = crate::git::fetch_pinned(&src.locator, src.version.as_deref(), &clone)
            .map_err(cleanup)?;
        if allow.is_some_and(|a| !a.permits(&src, &sha)) {
            return Err(cleanup(refused(format!(
                "{src} resolved to {sha}, which the allow-list's commit pin doesn't allow"
            ))));
        }
        let dir = skill::skill_dir_in(&clone, req.path.as_deref()).map_err(cleanup)?;
        let candidate = skill::inspect(&dir).map_err(cleanup)?;
        Ok(PreparedSkill {
            source: src.to_string(),
            pin: Some(sha),
            candidate,
            clone: Some(clone),
        })
    }

    fn commit_skill(
        &self,
        p: PreparedSkill,
        origin: Origin,
        waivers: Vec<Waiver>,
        replace: bool,
    ) -> Result<Outcome> {
        let layout = &self.cfg.layout;
        let name = p.candidate.name.clone();
        let copy = layout
            .staging()
            .join(format!("{}-skill", uuid::Uuid::new_v4().simple()));
        let result = skill::copy_skill(&p.candidate.dir, &copy).and_then(|()| {
            self.store.update(|l| {
                if let Some(old) = l.skills.get(&name) {
                    if !replace {
                        return Err(refused(format!(
                            "a skill `{name}` is already installed; pass replace=true to reinstall it"
                        )));
                    }
                    if origin != Origin::Owner && old.origin == Origin::Owner {
                        return Err(refused(format!("`{name}` was installed by the owner")));
                    }
                }
                // The digest of what is kept, not of the draft/clone.
                let digest = skill::skill_md_digest(&copy)?;
                skill::replace_dir(&copy, &layout.skills_dir().join(&name))?;
                let _ = fs::remove_dir_all(layout.suspended_skills_dir().join(&name));
                l.skills.insert(
                    name.clone(),
                    SkillEntry {
                        source: p.source.clone(),
                        pin: p.pin.clone(),
                        origin,
                        installed_at: lock::now(),
                        status: Status::Active,
                        reason: None,
                        digest,
                        waivers: waivers.clone(),
                    },
                );
                Ok(())
            })
        });
        let _ = fs::remove_dir_all(&copy);
        let warnings = p.candidate.findings.len() - scan::blocks(&p.candidate.findings).count();
        p.cleanup();
        result?;
        self.refresh_skills();
        Ok(Outcome::Installed {
            name: name.clone(),
            tools: vec![name],
            warnings,
        })
    }

    fn refresh_skills(&self) {
        if let Some(h) = &self.cfg.skills {
            h.refresh();
        }
    }
}

impl PreparedSkill {
    fn cleanup(&self) {
        if let Some(c) = &self.clone {
            let _ = fs::remove_dir_all(c);
        }
    }
}

fn parse_skill_source(spec: &str) -> Result<Source> {
    let src = Source::parse(spec).map_err(refused)?;
    if src.kind != Kind::Git {
        return Err(refused(
            "skills install from git only: git:https://host/org/repo[@rev]",
        ));
    }
    Ok(src)
}

/// The server's tools, less those its `enabled_tools` leaves out: those are
/// never scanned, offered or recorded.
async fn list_enabled(
    client: &McpClient,
) -> std::result::Result<Vec<McpToolInfo>, ferrule_mcp::McpError> {
    let mut infos = client.list_tools().await?;
    infos.retain(|i| client.config().tool_enabled(&i.name));
    Ok(infos)
}

fn server_config(name: &str, e: &ServerEntry) -> McpServerConfig {
    McpServerConfig {
        name: name.into(),
        command: if e.url.is_some() {
            String::new()
        } else {
            e.command.clone()
        },
        args: e.args.clone(),
        url: e.url.clone(),
        sandbox: true,
        ..Default::default()
    }
}

/// Whether two lock entries start the same process the same way.
fn same_launch(a: &ServerEntry, b: &ServerEntry) -> bool {
    (
        &a.source,
        &a.pin,
        &a.command,
        &a.args,
        &a.url,
        &a.checkout,
        &a.installed_at,
    ) == (
        &b.source,
        &b.pin,
        &b.command,
        &b.args,
        &b.url,
        &b.checkout,
        &b.installed_at,
    )
}

/// Why an installed server must not stay live with this surface, if so.
/// Only rule names and tool names: the model may read it.
fn suspend_reason(s: &Surface) -> Option<String> {
    if !s.blocked.is_empty() {
        return Some(format!(
            "the description scan flagged {}",
            scan::summary_for_model(&s.unwaived_blocks())
        ));
    }
    if !s.changed.is_empty() {
        let names: Vec<_> = s.changed.iter().map(String::as_str).collect();
        return Some(format!(
            "the approved description of {} changed",
            names.join(", ")
        ));
    }
    None
}

fn waivers_for(blocks: &[Finding], digests: &BTreeMap<String, String>) -> Vec<Waiver> {
    let mut out: Vec<Waiver> = Vec::new();
    for f in blocks {
        let w = Waiver {
            item: f.item.clone(),
            rule: f.rule.clone(),
            digest: digests.get(&f.item).cloned().unwrap_or_default(),
        };
        if !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

fn skill_waivers(c: &SkillCandidate) -> Vec<Waiver> {
    let digests = BTreeMap::from([(c.name.clone(), c.digest.clone())]);
    let blocks: Vec<Finding> = scan::blocks(&c.findings).cloned().collect();
    waivers_for(&blocks, &digests)
}

/// Delete a checkout, but only one under the extensions' own `src/`.
fn remove_checkout(layout: &Layout, dir: &Path) {
    if dir.starts_with(layout.root().join("src")) {
        let _ = fs::remove_dir_all(dir);
    }
}

fn dir_names(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}
