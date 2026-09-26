//! `TrustGuard`: the hub, seen from one agent in a run tree.

use crate::classify::{classify, classify_connected, classify_declared, Gated};
use crate::hub::{Hub, Prompter};
use async_trait::async_trait;
use ferrule_core::{Guard, GuardedCall, Verdict};
use std::sync::Arc;
use std::time::Duration;

/// Who can say yes to a gated call in this run.
#[derive(Clone)]
pub enum Route {
    /// The owner chat on Telegram (a gateway turn).
    Owner { chat_label: String },
    /// The terminal the run was started from.
    Terminal(Arc<dyn Prompter>),
    /// Nobody: refused. The reason says which kind of run this is.
    Unattended(String),
}

#[derive(Clone)]
pub struct TrustGuard {
    hub: Arc<Hub>,
    tree: String,
    task: Option<String>,
    root: bool,
    route: Route,
    planning: bool,
}

impl TrustGuard {
    /// The root agent of `tree` (a session id). A scheduled task's tree is
    /// `scheduler__<task>`; its task caps follow from that.
    pub fn root(hub: Arc<Hub>, tree: impl Into<String>, route: Route) -> Self {
        let tree = tree.into();
        let task = tree
            .strip_prefix(crate::meter::TASK_PREFIX)
            .map(str::to_string);
        Self {
            hub,
            tree,
            task,
            root: true,
            route,
            planning: false,
        }
    }

    /// Plan mode: nothing that changes files, no gated command.
    pub fn planning(mut self, on: bool) -> Self {
        self.planning = on;
        self
    }

    /// The same guard for a sub-agent of this tree: same run, caps, switch,
    /// route and phase. It doesn't start a run of its own.
    pub fn child(&self) -> Self {
        Self {
            root: false,
            ..self.clone()
        }
    }

    pub fn tree(&self) -> &str {
        &self.tree
    }

    pub fn is_planning(&self) -> bool {
        self.planning
    }

    pub fn hub(&self) -> &Arc<Hub> {
        &self.hub
    }

    fn refusal(g: &Gated, why: &str) -> String {
        format!(
            "`{}` is a {}, which needs the owner's approval, and {why}. It was not run.",
            g.command, g.kind
        )
    }
}

#[async_trait]
impl Guard for TrustGuard {
    fn begin(&self) {
        if self.root {
            self.hub.begin_run(&self.tree);
        }
    }

    fn before_model_call(&self) -> Option<String> {
        let unattended = matches!(self.route, Route::Unattended(_));
        self.hub.check(&self.tree, self.task.as_deref(), unattended)
    }

    async fn before_tool_call(&self, call: GuardedCall<'_>) -> Verdict {
        let gated = || {
            classify(call.tool, call.args, self.hub.bound_hosts())
                .or_else(|| {
                    classify_connected(call.tool, call.changes_files, &self.hub.connected())
                })
                .or_else(|| classify_declared(call.tool, call.needs_approval))
        };
        if self.planning {
            if call.changes_files && call.tool != "shell" {
                return Verdict::Refuse(format!(
                    "this is plan mode: `{}` can change files, and nothing changes before the plan is approved. Explore read-only and answer with the plan.",
                    call.tool
                ));
            }
            // A sub-agent is read-only in plan mode too, but its own
            // worktree (the default) is a new branch and checkout.
            if call.tool == "spawn_agent"
                && call.args.get("worktree").and_then(|w| w.as_bool()) != Some(false)
            {
                return Verdict::Refuse(
                    "this is plan mode: a sub-agent in its own git worktree creates a branch. Start it with `\"worktree\": false`; it explores read-only like you.".into(),
                );
            }
            if let Some(g) = gated() {
                return Verdict::Refuse(format!(
                    "this is plan mode: `{}` is a {}. Put it in the plan instead; it will still need the owner's approval when the plan runs.",
                    g.command, g.kind
                ));
            }
            return Verdict::Allow;
        }
        if !self.hub.config().gates {
            return Verdict::Allow;
        }
        let Some(g) = gated() else {
            return Verdict::Allow;
        };
        let subject = format!("`{}` ({})", g.command, g.kind);
        let outcome = match &self.route {
            Route::Unattended(why) => {
                self.hub.refuse_unattended(&self.tree, &subject, why);
                Err(format!("{why}, so nobody can approve it"))
            }
            Route::Owner { chat_label } => {
                let run = self.hub.run_id(&self.tree).unwrap_or_default();
                let question = format!(
                    "ferrule wants to run, in {chat_label} (run {run}):\n`{}` — {}.",
                    g.command, g.kind
                );
                let timeout = Duration::from_secs(self.hub.config().approval_timeout_secs);
                self.hub
                    .ask_owner(&self.tree, &subject, &question, timeout)
                    .await
            }
            Route::Terminal(p) => {
                let question = format!("ferrule wants to run `{}` — {}.", g.command, g.kind);
                self.hub
                    .ask_terminal(&self.tree, &subject, &question, p.clone())
                    .await
            }
        };
        match outcome {
            Ok(()) => Verdict::Allow,
            Err(why) => Verdict::Refuse(Self::refusal(&g, &why)),
        }
    }

    async fn halted(&self) -> String {
        self.hub.halted(&self.tree, self.task.as_deref()).await
    }
}
