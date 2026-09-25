//! Sub-agents in the CLI: the supervisor every entry point shares, the
//! factory that builds a child like any other agent (narrowed to what its
//! role may do), the wakers that run an idle root when its children report,
//! and `ferrule agents`.

use crate::config::{self, AgentsConfig, Config};
use crate::ledger;
use crate::models::Scope;
use anyhow::{bail, Result};
use ferrule_agents::{
    AgentRow, AgentStore, ChildFactory, ChildSpec, Role, Status, Supervisor, Waker,
};
use ferrule_core::tool::Tool;
use ferrule_core::LedgerSink;
use ferrule_gateway::Router;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};
use tokio::sync::mpsc;

/// Builds one agent: the CLI's `build_agent_from`, handed in so this module
/// doesn't need to know how.
pub type Build = Arc<
    dyn Fn(Scope, &ChildSpec, Option<ledger::LedgerTag>) -> Result<ferrule_core::Agent>
        + Send
        + Sync,
>;

/// The supervisor for this process, or None when `[agents] enabled =
/// false`. `provider` is the process's one-off (`--provider`/`--model`);
/// a role in `[agents.roles]` can name another model. Otherwise a child
/// runs on what its root's task or chat runs on (docs/m21-models.md §3).
pub fn supervisor(
    cfg: &Config,
    provider: Option<String>,
    sink: Option<Arc<dyn LedgerSink>>,
    build: Build,
) -> Result<Option<Arc<Supervisor>>> {
    if !cfg.agents.enabled {
        return Ok(None);
    }
    check_roles(cfg)?;
    let roles: HashMap<String, Option<String>> = cfg
        .agents
        .roles
        .iter()
        .map(|(r, c)| (r.clone(), c.model.clone().or_else(|| c.provider.clone())))
        .collect();
    let me: Arc<OnceLock<Weak<Supervisor>>> = Arc::default();
    let factory: ChildFactory = {
        let me = me.clone();
        Arc::new(move |spec: &ChildSpec| {
            let sup = me.get().and_then(Weak::upgrade);
            let mut scope = Scope::for_session(&spec.tree);
            scope.session = spec.id.clone();
            // A model spawn_agent named (checked as connected when it
            // spawned) goes ahead of its role's.
            let scope = match (&spec.model, role_provider(&roles, sup.as_deref(), spec)) {
                (Some(m), _) => scope.fixed(Some(m.clone()), "spawn_agent's model"),
                (None, Some((role, word))) => scope.fixed(Some(word), &format!("role {role}")),
                (None, None) => scope.fixed(provider.clone(), "the root's model"),
            };
            let tag =
                ledger::LedgerTag::new(&sink, "agent", Some(format!("agent:{}", spec.parent)));
            build(scope, spec, tag).map_err(|e| format!("{e:#}"))
        })
    };
    let data = config::data_dir()?;
    let store = AgentStore::open(data.join("agents.db"))?;
    let sup = Supervisor::new(store, data.join("sessions"), cfg.agents.limits(), factory)?;
    let _ = me.set(Arc::downgrade(&sup));
    let models = crate::models::shared()?;
    sup.set_model_check(Arc::new(move |word| {
        models.resolve(word).map(|e| e.reference())
    }));
    sup.set_worktrees_dir(data.join("worktrees"));
    Ok(Some(sup))
}

pub fn check_roles(cfg: &Config) -> Result<()> {
    for (role, rc) in &cfg.agents.roles {
        if Role::parse(role).is_none() {
            bail!(
                "[agents.roles.{role}]: no such role; the roles are worker, planner and verifier"
            );
        }
        if let Some(p) = &rc.provider {
            if !cfg.providers.contains_key(p) {
                bail!("[agents.roles.{role}] provider = \"{p}\": there is no [providers.{p}]");
            }
        }
        if let Some(m) = &rc.model {
            if rc.provider.is_some() {
                bail!("[agents.roles.{role}]: set `model` or `provider`, not both");
            }
            if let Err(e) = crate::models::Catalog::from_config(cfg).resolve(m) {
                bail!("[agents.roles.{role}] model = \"{m}\": {e}");
            }
        }
    }
    Ok(())
}

/// The model for `spec` (a ref, or a provider's name) and the role that
/// names it: its own role's, else the nearest ancestor's whose role has
/// one, so a verifier's helpers stay on the verifier's model. None: the
/// root's.
fn role_provider(
    roles: &HashMap<String, Option<String>>,
    sup: Option<&Supervisor>,
    spec: &ChildSpec,
) -> Option<(String, String)> {
    let mut chain = vec![spec.role.as_str().to_string()];
    if let Some(sup) = sup {
        let mut next = Some(spec.parent.clone());
        while let Some(id) = next {
            let Ok(Some(row)) = sup.store().get(&id) else {
                break;
            };
            chain.push(row.role);
            next = row.parent;
        }
    }
    chain
        .iter()
        .find_map(|r| Some((r.clone(), roles.get(r).cloned().flatten()?)))
}

/// What a child may use of the tools the root has: a read-only child
/// can't write files or memories, and a verifier or read-only child only
/// gets the MCP tools that don't change anything (MCP servers run in the
/// root's workspace, not the child's copy).
pub fn narrows(spec: Option<&ChildSpec>) -> (bool, bool) {
    match spec {
        None => (false, false),
        Some(s) => (s.read_only, s.read_only || s.role == Role::Verifier),
    }
}

pub fn only_reading(tools: &[Arc<dyn Tool>]) -> Vec<Arc<dyn Tool>> {
    tools
        .iter()
        .filter(|t| !t.changes_files())
        .cloned()
        .collect()
}

/// The gateway's: a chat's root is run by its lane, answering in the chat.
pub struct RouterWaker(pub Weak<Router>);

impl Waker for RouterWaker {
    fn can_wake(&self, root: &str) -> bool {
        self.0.upgrade().is_some_and(|r| r.can_wake(root))
    }
    fn wake(&self, root: &str, text: String) -> bool {
        self.0.upgrade().is_some_and(|r| r.wake(root, text))
    }
}

/// `ferrule run`'s: the loop that waits for the tree runs the root.
pub struct ChannelWaker {
    pub root: String,
    pub tx: mpsc::UnboundedSender<String>,
}

impl Waker for ChannelWaker {
    fn can_wake(&self, root: &str) -> bool {
        root == self.root
    }
    fn wake(&self, root: &str, text: String) -> bool {
        root == self.root && self.tx.send(text).is_ok()
    }
}

pub fn store() -> Result<AgentStore> {
    Ok(AgentStore::open(config::data_dir()?.join("agents.db"))?)
}

/// `ferrule agents list`: every tree, children indented under parents.
pub fn list(all: bool) -> Result<()> {
    let store = store()?;
    let rows: Vec<AgentRow> = store
        .all()?
        .into_iter()
        .filter(|r| all || r.status != Status::Closed || r.parent.is_none())
        .collect();
    let mut shown = 0;
    for root in rows.iter().filter(|r| r.parent.is_none()) {
        let mut lines = Vec::new();
        below(&rows, &root.id, 1, &store, &mut lines);
        if lines.is_empty() {
            continue;
        }
        println!("{} (root)", root.id);
        for line in &lines {
            println!("{line}");
        }
        shown += lines.len();
    }
    if shown == 0 {
        println!(
            "No agents{}.",
            if all {
                ""
            } else {
                " open (--all shows closed ones)"
            }
        );
    }
    Ok(())
}

/// A line per agent below `parent`, each followed by its own children.
fn below(
    rows: &[AgentRow],
    parent: &str,
    depth: usize,
    store: &AgentStore,
    lines: &mut Vec<String>,
) {
    for r in rows.iter().filter(|r| r.parent.as_deref() == Some(parent)) {
        let name = r
            .name
            .as_deref()
            .map(|n| format!(" \"{n}\""))
            .unwrap_or_default();
        let elsewhere =
            if r.status == Status::Running && store.running_elsewhere(&r.id).unwrap_or(false) {
                " (in another ferrule process)"
            } else {
                ""
            };
        let branch = r
            .branch
            .as_deref()
            .map(|b| format!(", branch {b}"))
            .unwrap_or_default();
        let model = r
            .model
            .as_deref()
            .map(|m| format!(", on {m}"))
            .unwrap_or_default();
        lines.push(format!(
            "{}{}{name} [{}] {}{elsewhere}, {} tokens{branch}{model}",
            "  ".repeat(depth),
            r.id,
            r.role,
            r.status,
            r.tokens
        ));
        below(rows, &r.id, depth + 1, store, lines);
    }
}

/// `ferrule agents close`: `id` and everything it started.
pub async fn close(cfg: &AgentsConfig, id: &str) -> Result<()> {
    let store = store()?;
    let Some(row) = store.get(id)? else {
        bail!("there is no agent {id}; `ferrule agents list` shows them");
    };
    let mut stack = vec![row.id.clone()];
    while let Some(next) = stack.pop() {
        if store.running_elsewhere(&next)? {
            bail!(
                "agent {next} is running in another ferrule process (the gateway, or a `ferrule run`); \
                 only that process can stop it"
            );
        }
        stack.extend(store.children(&next)?.into_iter().map(|c| c.id));
    }
    let data = config::data_dir()?;
    let refuse: ChildFactory = Arc::new(|_| Err("`ferrule agents close` starts no agents".into()));
    let sup = Supervisor::new(store, data.join("sessions"), cfg.limits(), refuse)?;
    sup.set_worktrees_dir(data.join("worktrees"));
    let closed = sup.close_tree(id).await?;
    if closed.ids.is_empty() {
        println!("{id} was already closed.");
    } else {
        println!("Closed {}.", closed.ids.join(", "));
    }
    for note in closed.notes {
        println!("{note}");
    }
    Ok(())
}
