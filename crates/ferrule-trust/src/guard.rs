//! `TrustGuard`: the hub, seen from one agent in a run tree.

use crate::classify::{
    classify, classify_connected, classify_declared, grantable_command, only_to, recipients,
    untrusted_source, Gated, Kind,
};
use crate::grants::Spec;
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
    gates: bool,
    /// M54.5: where this run's gates are off, a send after outside content
    /// still asks.
    untrusted_gate: bool,
    /// M54.5: the run's own input is someone else's (a guest's message).
    guest_input: bool,
    /// M56.2: where this agent's commands run, which a standing shell
    /// grant is bound to. `None`: no shell grant applies.
    workspace: Option<String>,
}

/// What a guest's run starts having read.
const GUEST_INPUT: &str = "a message from someone who isn't the owner";

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
            gates: true,
            untrusted_gate: false,
            guest_input: false,
            workspace: None,
        }
    }

    /// M56.2: the workspace a standing shell grant must name.
    pub fn workspace(mut self, dir: Option<&std::path::Path>) -> Self {
        self.workspace = dir.map(|d| d.display().to_string());
        self
    }

    /// Plan mode: nothing that changes files, no gated command.
    pub fn planning(mut self, on: bool) -> Self {
        self.planning = on;
        self
    }

    /// Whether this run's gates ask at all. Off for the owner's own trees
    /// in owner mode, whatever `[trust] gates` says; plan mode's refusals
    /// don't depend on it.
    pub fn gates(mut self, on: bool) -> Self {
        self.gates = on;
        self
    }

    /// M54.5: with the gates off (owner mode), a connected service's
    /// change, a force push or branch deletion and a DELETE to a bound host
    /// still ask once the run has read content from outside.
    pub fn untrusted_gate(mut self, on: bool) -> Self {
        self.untrusted_gate = on;
        self
    }

    /// M54.5: the run answers someone who isn't the owner, so it has read
    /// outside content from its start.
    pub fn guest_input(mut self, on: bool) -> Self {
        self.guest_input = on;
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

    /// M56.2: what "Always here" would keep for `call`: a shell command
    /// that is exactly its one gated command, in this workspace, or a
    /// connected service's change to the addresses it names. Nothing
    /// for anything else, and nothing for a call that leaves the box once
    /// the run has read outside content: no standing grant covers that
    /// (M54.5), and none is made from it.
    fn standing(&self, call: GuardedCall<'_>, g: &Gated) -> Option<Spec> {
        if g.kind.leaves_the_box() && self.hub.untrusted(&self.tree).is_some() {
            return None;
        }
        match g.kind {
            Kind::ConnectedWrite => {
                let to = recipients(call.args);
                (!to.is_empty()).then(|| Spec::Send {
                    kind: g.kind.to_string(),
                    tool: call.tool.to_string(),
                    to,
                })
            }
            Kind::DeclaredApproval => None,
            _ if call.tool == "shell" => {
                let command = call.args.get("command")?.as_str()?;
                Some(Spec::Command {
                    kind: g.kind.to_string(),
                    argv: grantable_command(command, g)?,
                    cwd: self.workspace.clone()?,
                })
            }
            _ => None,
        }
    }

    fn refusal(g: &Gated, why: &str, after: Option<&str>) -> String {
        let after = after
            .map(|source| format!(" after this run read outside content ({source})"))
            .unwrap_or_default();
        format!(
            "`{}` is a {}, which needs the owner's approval{after}, and {why}. It was not run.",
            g.command, g.kind
        )
    }

    /// Asks whoever can say yes about `g`; `after` is the outside content
    /// this run read first, when that is why it asks (M54.5). A refusal
    /// goes back to the model to try another way, until too many stop the
    /// run (M56.2, `Hub::denied`).
    async fn ask(&self, call: GuardedCall<'_>, g: Gated, after: Option<&str>) -> Verdict {
        let subject = format!("`{}` ({})", g.command, g.kind);
        if self.hub.use_grant(&self.tree, &subject) {
            self.hub.let_through(&self.tree);
            return Verdict::Allow;
        }
        let standing = self.standing(call, &g);
        if let Some(spec) = &standing {
            if self.hub.use_standing(&self.tree, spec, &subject) {
                return Verdict::Allow;
            }
        }
        if self.hub.escalated(&self.tree).is_some() {
            return Verdict::Refuse(format!(
                "`{}` was not run: this run already stopped after too many refusals, and the owner has been told.",
                g.command
            ));
        }
        // Only a shell gate's subject is the command itself; a service's or
        // a plugin's names just the tool, whatever its arguments.
        let for_run = call.tool == "shell";
        let read = after
            .map(|source| format!(", after reading outside content ({source})"))
            .unwrap_or_default();
        let outcome = match &self.route {
            Route::Unattended(why) => {
                self.hub.refuse_unattended(&self.tree, &subject, why);
                Err(format!("{why}, so nobody can approve it"))
            }
            Route::Owner { chat_label } => {
                let run = self.hub.run_id(&self.tree).unwrap_or_default();
                let question = format!(
                    "ferrule wants to run, in {chat_label} (run {run}){read}:\n`{}` — {}.",
                    g.command, g.kind
                );
                let timeout = Duration::from_secs(self.hub.config().approval_timeout_secs);
                self.hub
                    .ask_gate(
                        &self.tree,
                        &subject,
                        &question,
                        timeout,
                        for_run,
                        standing.as_ref(),
                    )
                    .await
            }
            Route::Terminal(p) => {
                let question = format!("ferrule wants to run `{}` — {}{read}.", g.command, g.kind);
                self.hub
                    .ask_terminal(&self.tree, &subject, &question, p.clone())
                    .await
            }
        };
        match outcome {
            Ok(()) => {
                self.hub.let_through(&self.tree);
                Verdict::Allow
            }
            Err(why) => {
                let next = self.hub.denied(&self.tree, &subject);
                Verdict::Refuse(Self::refusal(&g, &why, after) + &next)
            }
        }
    }
}

#[async_trait]
impl Guard for TrustGuard {
    fn begin(&self) {
        if self.root {
            self.hub.begin_run(&self.tree);
        }
        if self.guest_input {
            self.hub.saw_untrusted(&self.tree, GUEST_INPUT);
        }
    }

    fn before_model_call(&self) -> Option<String> {
        // M56.2: too many refusals: the owner hears it, as the run's answer.
        if let Some(stop) = self.hub.escalated(&self.tree) {
            return Some(stop);
        }
        let unattended = matches!(self.route, Route::Unattended(_));
        self.hub.check(&self.tree, self.task.as_deref(), unattended)
    }

    async fn before_tool_call(&self, call: GuardedCall<'_>) -> Verdict {
        if untrusted_source(
            call.tool,
            call.args,
            call.changes_files,
            &self.hub.connected(),
        ) {
            self.hub.saw_untrusted(&self.tree, call.tool);
        }
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
        if !self.gates || !self.hub.config().gates {
            // M54.5: the one gate left. The confused deputy: the proxy
            // swaps in the owner's key for its own host, so an injected
            // page or email never needs to see a key to send mail as the
            // owner or delete a remote branch.
            if !self.untrusted_gate {
                return Verdict::Allow;
            }
            let Some(source) = self.hub.untrusted(&self.tree) else {
                return Verdict::Allow;
            };
            return match gated().filter(|g| g.kind.leaves_the_box()) {
                // Wave A: the owner's own scheduled task (the gates are
                // off only for the owner's trees, and nobody is there to
                // tap) may send to the owner alone: a morning briefing that
                // read the web or mail and emails or posts it to the owner.
                // An injection can't move where it goes: the addresses are
                // the config's and the connection store's, matched exactly.
                Some(g)
                    if g.kind == Kind::ConnectedWrite
                        && self.task.is_some()
                        && matches!(self.route, Route::Unattended(_))
                        && only_to(call.args, &self.hub.own_recipients()) =>
                {
                    self.hub.sent_to_owner(&self.tree, call.tool, &source);
                    Verdict::Allow
                }
                Some(g) => self.ask(call, g, Some(&source)).await,
                None => Verdict::Allow,
            };
        }
        match gated() {
            Some(g) => self.ask(call, g, None).await,
            None => Verdict::Allow,
        }
    }

    async fn halted(&self) -> String {
        self.hub.halted(&self.tree, self.task.as_deref()).await
    }
}
