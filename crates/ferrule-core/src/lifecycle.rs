//! Lifecycle hooks (M18, `docs/m18-hooks.md`): commands the owner runs at
//! fixed points in an agent's life, with Claude Code's event names, stdin
//! payload and exit-code contract. This module is the part the loop needs:
//! the events, the payload, what a hook's exit code and output mean per
//! event, and the ordered set of hooks an agent fires. Spawning commands,
//! config files and the audit log's file live in `ferrule-hooks`.
//!
//! `verify_command` is a built-in instance: [`HookSet::add_check`] turns a
//! [`Verifier`] into a Stop hook the loop treats as a check (its own
//! events, wording and cap).

use crate::tool::ToolContext;
use crate::verify::Verifier;
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A point in an agent's life a hook can run at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HookEvent {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Stop,
    PreCompact,
    PostCompact,
    SubagentStart,
    SubagentStop,
}

impl HookEvent {
    pub const ALL: [HookEvent; 10] = [
        HookEvent::SessionStart,
        HookEvent::SessionEnd,
        HookEvent::UserPromptSubmit,
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::Stop,
        HookEvent::PreCompact,
        HookEvent::PostCompact,
        HookEvent::SubagentStart,
        HookEvent::SubagentStop,
    ];

    pub fn name(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::Stop => "Stop",
            HookEvent::PreCompact => "PreCompact",
            HookEvent::PostCompact => "PostCompact",
            HookEvent::SubagentStart => "SubagentStart",
            HookEvent::SubagentStop => "SubagentStop",
        }
    }

    pub fn parse(name: &str) -> Option<HookEvent> {
        HookEvent::ALL.into_iter().find(|e| e.name() == name)
    }

    /// Whether exit 2 blocks something here. Where it can't, it's a
    /// non-blocking error like any other failing exit code.
    pub fn can_block(self) -> bool {
        matches!(
            self,
            HookEvent::UserPromptSubmit
                | HookEvent::PreToolUse
                | HookEvent::PostToolUse
                | HookEvent::Stop
                | HookEvent::SubagentStart
                | HookEvent::SubagentStop
        )
    }

    /// Whether a matcher means anything here, and what it's matched on.
    pub fn takes_matcher(self) -> bool {
        !matches!(
            self,
            HookEvent::SessionEnd | HookEvent::UserPromptSubmit | HookEvent::Stop
        )
    }

    /// Plain stdout on exit 0 is a note for the model (Claude Code's rule).
    fn plain_stdout_is_context(self) -> bool {
        matches!(self, HookEvent::SessionStart | HookEvent::UserPromptSubmit)
    }

    /// Whether a note from this event reaches the model at all.
    pub fn takes_context(self) -> bool {
        !matches!(
            self,
            HookEvent::SessionEnd
                | HookEvent::Stop
                | HookEvent::SubagentStop
                | HookEvent::PreCompact
        )
    }
}

impl std::fmt::Display for HookEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// What a hook gets on stdin, as JSON. Keys that don't apply to the event
/// are left out. `hook_event_name` is set when it's fired.
#[derive(Debug, Clone, Default, Serialize)]
pub struct HookInput {
    pub hook_event_name: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<PathBuf>,
    pub cwd: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_response: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_hook_active: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_changed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_assistant_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
}

impl HookInput {
    /// What a matcher on `event` is matched against.
    fn subject(&self, event: HookEvent) -> Option<&str> {
        match event {
            HookEvent::PreToolUse | HookEvent::PostToolUse => self.tool_name.as_deref(),
            HookEvent::SessionStart => self.source.as_deref(),
            HookEvent::PreCompact | HookEvent::PostCompact => self.trigger.as_deref(),
            HookEvent::SubagentStart | HookEvent::SubagentStop => self.agent_type.as_deref(),
            _ => None,
        }
    }
}

/// `|`-separated alternatives, each an exact name or a glob (`*`, `?`).
/// Empty matches everything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Matcher(Vec<String>);

impl Matcher {
    pub fn parse(text: Option<&str>) -> Matcher {
        let alts: Vec<String> = text
            .unwrap_or_default()
            .split('|')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string)
            .collect();
        if alts.iter().any(|a| a == "*") {
            return Matcher::default();
        }
        Matcher(alts)
    }

    pub fn is_all(&self) -> bool {
        self.0.is_empty()
    }

    pub fn matches(&self, subject: Option<&str>) -> bool {
        if self.0.is_empty() {
            return true;
        }
        let Some(s) = subject else { return false };
        self.0.iter().any(|p| glob(p, s))
    }
}

impl std::fmt::Display for Matcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            f.write_str("*")
        } else {
            f.write_str(&self.0.join("|"))
        }
    }
}

/// `*` is any run of characters, `?` one character; the rest is literal.
fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Where a hook came from, in the order they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookSource {
    /// Ferrule's own (the `verify_command` check).
    Builtin,
    /// The trusted config's `[hooks]`.
    User,
    /// The workspace's `.ferrule/hooks.toml`, once trusted.
    Workspace,
}

impl HookSource {
    pub fn name(self) -> &'static str {
        match self {
            HookSource::Builtin => "builtin",
            HookSource::User => "user",
            HookSource::Workspace => "workspace",
        }
    }
}

/// What running a hook's command came to, before it's read for an event.
#[derive(Debug, Clone, Default)]
pub struct HookRun {
    /// `None`: it didn't exit on its own (killed, timed out, or never
    /// started).
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// It couldn't be started at all.
    pub spawn_error: Option<String>,
}

/// Runs a hook's command. `ferrule-hooks` has the one that spawns a
/// process; a check wraps a [`Verifier`].
#[async_trait::async_trait]
pub trait HookHandler: Send + Sync {
    /// The command, as shown to the owner and in the audit log.
    fn command(&self) -> String;
    async fn run(&self, input: &HookInput, ctx: &ToolContext) -> HookRun;
}

/// A [`Verifier`] as a Stop hook: exit 0 when it passes, exit 2 with its
/// output as the reason when it doesn't.
struct VerifierHook(Arc<dyn Verifier>);

#[async_trait::async_trait]
impl HookHandler for VerifierHook {
    fn command(&self) -> String {
        self.0.describe()
    }

    async fn run(&self, _input: &HookInput, ctx: &ToolContext) -> HookRun {
        match self.0.verify(ctx).await {
            Ok(()) => HookRun {
                exit_code: Some(0),
                ..Default::default()
            },
            Err(output) => HookRun {
                exit_code: Some(2),
                stderr: output,
                ..Default::default()
            },
        }
    }
}

/// One configured hook.
#[derive(Clone)]
pub struct Hook {
    pub event: HookEvent,
    pub matcher: Matcher,
    pub source: HookSource,
    pub handler: Arc<dyn HookHandler>,
    /// The built-in check: runs only when files changed, and the loop
    /// gives it the verify events, wording and cap.
    pub check: bool,
}

impl Hook {
    pub fn new(
        event: HookEvent,
        matcher: Matcher,
        source: HookSource,
        handler: Arc<dyn HookHandler>,
    ) -> Hook {
        Hook {
            event,
            matcher,
            source,
            handler,
            check: false,
        }
    }

    pub fn command(&self) -> String {
        self.handler.command()
    }

    fn same_as(&self, other: &Hook) -> bool {
        self.event == other.event
            && self.matcher == other.matcher
            && !self.check
            && !other.check
            && self.command() == other.command()
    }
}

impl std::fmt::Debug for Hook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hook")
            .field("event", &self.event)
            .field("matcher", &self.matcher)
            .field("source", &self.source)
            .field("command", &self.command())
            .field("check", &self.check)
            .finish()
    }
}

/// What one hook's run means for its event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Go on; maybe with a note for the model.
    Proceed { context: Option<String> },
    /// Stop what was about to happen, for this reason (the model's to read).
    Block { reason: String },
    /// It failed without blocking: the owner's to read, never the model's.
    Error { note: String },
}

/// Reads a hook's run for `event`: exit 0 proceeds (JSON on stdout may
/// still block or add a note), exit 2 blocks where the event can, anything
/// else is a non-blocking error. Reasons and notes are cut to `cap` chars.
pub fn interpret(event: HookEvent, command: &str, run: &HookRun, cap: usize) -> Verdict {
    if let Some(e) = &run.spawn_error {
        return Verdict::Error {
            note: format!("couldn't start: {e}"),
        };
    }
    if run.timed_out {
        return Verdict::Error {
            note: "timed out and was killed".into(),
        };
    }
    match run.exit_code {
        Some(0) => {}
        Some(2) if event.can_block() => {
            let reason = run.stderr.trim();
            let reason = if reason.is_empty() {
                format!("blocked by `{command}`")
            } else {
                cut(reason, cap)
            };
            return Verdict::Block { reason };
        }
        Some(code) => {
            return Verdict::Error {
                note: with_tail(format!("exit code {code}"), &run.stderr),
            }
        }
        None => {
            return Verdict::Error {
                note: with_tail("killed by a signal".into(), &run.stderr),
            }
        }
    }
    let out = run.stdout.trim();
    let json = out
        .starts_with('{')
        .then(|| serde_json::from_str::<Value>(out).ok())
        .flatten()
        .filter(Value::is_object);
    let Some(json) = json else {
        let context = (event.plain_stdout_is_context() && !out.is_empty()).then(|| cut(out, cap));
        return Verdict::Proceed { context };
    };
    let specific = &json["hookSpecificOutput"];
    fn text(v: &Value) -> Option<&str> {
        v.as_str().map(str::trim).filter(|s| !s.is_empty())
    }
    if event.can_block() {
        let reason = if json["decision"] == "block" {
            Some(text(&json["reason"]))
        } else if event == HookEvent::PreToolUse && specific["permissionDecision"] == "deny" {
            Some(text(&specific["permissionDecisionReason"]).or(text(&json["reason"])))
        } else {
            None
        };
        if let Some(reason) = reason {
            return Verdict::Block {
                reason: match reason {
                    Some(r) => cut(r, cap),
                    None => format!("blocked by `{command}`"),
                },
            };
        }
    }
    let context = text(&specific["additionalContext"])
        .or(text(&json["additionalContext"]))
        .map(|c| cut(c, cap));
    Verdict::Proceed { context }
}

fn with_tail(head: String, stderr: &str) -> String {
    let tail = stderr.trim();
    if tail.is_empty() {
        return head;
    }
    let chars: Vec<char> = tail.chars().collect();
    let start = chars.len().saturating_sub(AUDIT_NOTE_CHARS);
    format!("{head}: {}", chars[start..].iter().collect::<String>())
}

/// At most `cap` chars, marked when cut.
pub fn cut(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let mut out: String = text.chars().take(cap).collect();
    out.push_str("\n[… cut]");
    out
}

/// The audit note's size: a reason or an error's stderr tail.
pub const AUDIT_NOTE_CHARS: usize = 2_000;

/// One line of the audit log: a hook that ran, or was skipped because an
/// earlier one blocked.
#[derive(Debug, Clone, Serialize)]
pub struct HookRecord {
    /// Unix time, milliseconds.
    pub ts: i64,
    pub event: String,
    pub source: HookSource,
    pub command: String,
    pub matcher: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub blocked: bool,
    pub timed_out: bool,
    pub skipped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Where every hook run is recorded.
pub trait HookAudit: Send + Sync {
    fn record(&self, record: &HookRecord);
}

/// Caps on what hooks can do to a run.
#[derive(Debug, Clone, Copy)]
pub struct HookLimits {
    /// Per note or block reason, and per event point's joined notes.
    pub max_context_chars: usize,
    /// Times Stop (or SubagentStop) hooks may send one run back.
    pub max_stop_blocks: usize,
}

impl Default for HookLimits {
    fn default() -> Self {
        HookLimits {
            max_context_chars: 10_000,
            max_stop_blocks: 3,
        }
    }
}

/// One hook run, for the owner's screen.
#[derive(Debug, Clone)]
pub struct HookReport {
    pub event: HookEvent,
    pub source: HookSource,
    pub command: String,
    pub exit_code: Option<i32>,
    pub duration: Duration,
    pub blocked: bool,
    /// A non-blocking error: what went wrong.
    pub error: Option<String>,
}

/// What firing an event came to.
#[derive(Debug, Clone, Default)]
pub struct Fired {
    /// The first hook that blocked: its command and reason.
    pub block: Option<(String, String)>,
    /// The hooks' notes for the model, joined and capped.
    pub context: Option<String>,
    pub reports: Vec<HookReport>,
}

/// The hooks an agent fires, in the order they run: built-ins, then user
/// hooks, then workspace hooks, each in the order configured. Cheap to
/// clone; an empty set costs the loop nothing.
#[derive(Clone, Default)]
pub struct HookSet {
    hooks: Vec<Hook>,
    audit: Option<Arc<dyn HookAudit>>,
    pub limits: HookLimits,
    /// Set on a sub-agent's hooks: stamped into every payload.
    agent_id: Option<String>,
    parent_session_id: Option<String>,
}

impl std::fmt::Debug for HookSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookSet")
            .field("hooks", &self.hooks)
            .field("limits", &self.limits)
            .field("agent_id", &self.agent_id)
            .finish()
    }
}

impl HookSet {
    pub fn new() -> HookSet {
        HookSet::default()
    }

    pub fn with_audit(mut self, audit: Arc<dyn HookAudit>) -> HookSet {
        self.audit = Some(audit);
        self
    }

    pub fn with_limits(mut self, limits: HookLimits) -> HookSet {
        self.limits = limits;
        self
    }

    /// Adds `hook` in its source's place; the same command with the same
    /// matcher for the same event is kept once.
    pub fn add(&mut self, hook: Hook) {
        if self.hooks.iter().any(|h| h.same_as(&hook)) {
            return;
        }
        let at = self
            .hooks
            .iter()
            .position(|h| h.source > hook.source)
            .unwrap_or(self.hooks.len());
        self.hooks.insert(at, hook);
    }

    /// `verifier` as the built-in Stop check.
    pub fn add_check(&mut self, verifier: Arc<dyn Verifier>) {
        let mut hook = Hook::new(
            HookEvent::Stop,
            Matcher::default(),
            HookSource::Builtin,
            Arc::new(VerifierHook(verifier)),
        );
        hook.check = true;
        self.add(hook);
    }

    /// `other`'s hooks added to these; its audit sink and limits too when
    /// it has a sink and these don't.
    pub fn merge(&mut self, other: HookSet) {
        if self.audit.is_none() && other.audit.is_some() {
            self.audit = other.audit;
            self.limits = other.limits;
        }
        if self.agent_id.is_none() {
            self.agent_id = other.agent_id;
            self.parent_session_id = other.parent_session_id;
        }
        for hook in other.hooks {
            self.add(hook);
        }
    }

    /// What a sub-agent inherits: the PreToolUse and PostToolUse hooks,
    /// with its id in every payload.
    pub fn for_child(&self, agent_id: &str, parent_session_id: &str) -> HookSet {
        HookSet {
            hooks: self
                .hooks
                .iter()
                .filter(|h| {
                    !h.check && matches!(h.event, HookEvent::PreToolUse | HookEvent::PostToolUse)
                })
                .cloned()
                .collect(),
            audit: self.audit.clone(),
            limits: self.limits,
            agent_id: Some(agent_id.to_string()),
            parent_session_id: Some(parent_session_id.to_string()),
        }
    }

    /// Only the hooks for `events` (the supervisor keeps SubagentStart/Stop).
    pub fn only(&self, events: &[HookEvent]) -> HookSet {
        let mut set = self.clone();
        set.hooks.retain(|h| events.contains(&h.event));
        set
    }

    pub fn hooks(&self) -> &[Hook] {
        &self.hooks
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    pub fn has(&self, event: HookEvent) -> bool {
        self.hooks.iter().any(|h| h.event == event)
    }

    /// The payload's common part, stamped with this set's agent ids.
    pub fn input(
        &self,
        session_id: &str,
        transcript_path: Option<PathBuf>,
        cwd: PathBuf,
    ) -> HookInput {
        HookInput {
            session_id: session_id.to_string(),
            transcript_path,
            cwd,
            agent_id: self.agent_id.clone(),
            parent_session_id: self.parent_session_id.clone(),
            ..Default::default()
        }
    }

    /// The hooks for `event` whose matcher takes this payload, in order.
    pub fn matching(&self, event: HookEvent, input: &HookInput) -> Vec<Hook> {
        self.hooks
            .iter()
            .filter(|h| h.event == event && h.matcher.matches(input.subject(event)))
            .cloned()
            .collect()
    }

    /// Runs every matching hook in order until one blocks; the rest are
    /// recorded as skipped. Checks are left to the loop.
    pub async fn fire(&self, event: HookEvent, mut input: HookInput, ctx: &ToolContext) -> Fired {
        input.hook_event_name = event.name().to_string();
        let hooks: Vec<Hook> = self
            .matching(event, &input)
            .into_iter()
            .filter(|h| !h.check)
            .collect();
        let mut fired = Fired::default();
        let mut notes: Vec<String> = Vec::new();
        for (i, hook) in hooks.iter().enumerate() {
            let (verdict, report) = self.run_hook(hook, &input, ctx).await;
            fired.reports.push(report);
            match verdict {
                Verdict::Block { reason } => {
                    self.skip(&hooks[i + 1..], &input);
                    fired.block = Some((hook.command(), reason));
                    break;
                }
                Verdict::Proceed { context: Some(c) } => notes.push(c),
                _ => {}
            }
        }
        if !notes.is_empty() && event.takes_context() {
            fired.context = Some(cut(&notes.join("\n\n"), self.limits.max_context_chars));
        }
        fired
    }

    /// Runs one hook and records it. A check's reason isn't cut: the
    /// verifier already keeps the tail it wants the model to see.
    pub async fn run_hook(
        &self,
        hook: &Hook,
        input: &HookInput,
        ctx: &ToolContext,
    ) -> (Verdict, HookReport) {
        let mut input = input.clone();
        input.hook_event_name = hook.event.name().to_string();
        let command = hook.command();
        let started = Instant::now();
        let run = hook.handler.run(&input, ctx).await;
        let duration = started.elapsed();
        let cap = if hook.check {
            usize::MAX
        } else {
            self.limits.max_context_chars
        };
        let mut verdict = interpret(hook.event, &command, &run, cap);
        if let (true, Verdict::Block { reason }) = (hook.check, &mut verdict) {
            // A failing check's output goes to the model as it was.
            *reason = run.stderr.clone();
        }
        let (blocked, note, error) = match &verdict {
            Verdict::Block { reason } => (true, Some(reason.clone()), None),
            Verdict::Error { note } => (false, Some(note.clone()), Some(note.clone())),
            Verdict::Proceed { .. } => (false, None, None),
        };
        self.record(hook, &input, &run, duration, blocked, false, note);
        let report = HookReport {
            event: hook.event,
            source: hook.source,
            command,
            exit_code: run.exit_code,
            duration,
            blocked,
            error,
        };
        (verdict, report)
    }

    /// Records hooks that didn't run because an earlier one blocked.
    pub fn skip(&self, hooks: &[Hook], input: &HookInput) {
        for hook in hooks {
            self.record(
                hook,
                input,
                &HookRun::default(),
                Duration::ZERO,
                false,
                true,
                Some("skipped: an earlier hook blocked".into()),
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        hook: &Hook,
        input: &HookInput,
        run: &HookRun,
        duration: Duration,
        blocked: bool,
        skipped: bool,
        note: Option<String>,
    ) {
        let Some(audit) = &self.audit else { return };
        let note = note.map(|n| {
            let n: String = n.chars().take(AUDIT_NOTE_CHARS).collect();
            n
        });
        audit.record(&HookRecord {
            ts: chrono::Utc::now().timestamp_millis(),
            event: hook.event.name().to_string(),
            source: hook.source,
            command: hook.command(),
            matcher: hook.matcher.to_string(),
            tool: input.tool_name.clone(),
            session_id: input.session_id.clone(),
            agent_id: input.agent_id.clone(),
            exit_code: run.exit_code,
            duration_ms: duration.as_millis() as u64,
            blocked,
            timed_out: run.timed_out,
            skipped,
            note,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Answers with a fixed run and counts calls.
    struct Fixed {
        name: String,
        run: HookRun,
        calls: Mutex<Vec<HookInput>>,
    }

    fn fixed(name: &str, code: i32, stdout: &str, stderr: &str) -> Arc<Fixed> {
        Arc::new(Fixed {
            name: name.into(),
            run: HookRun {
                exit_code: Some(code),
                stdout: stdout.into(),
                stderr: stderr.into(),
                ..Default::default()
            },
            calls: Mutex::default(),
        })
    }

    #[async_trait::async_trait]
    impl HookHandler for Fixed {
        fn command(&self) -> String {
            self.name.clone()
        }
        async fn run(&self, input: &HookInput, _ctx: &ToolContext) -> HookRun {
            self.calls.lock().unwrap().push(input.clone());
            self.run.clone()
        }
    }

    #[derive(Default)]
    struct Log(Mutex<Vec<HookRecord>>);
    impl HookAudit for Log {
        fn record(&self, record: &HookRecord) {
            self.0.lock().unwrap().push(record.clone());
        }
    }

    fn ctx() -> ToolContext {
        ToolContext {
            workspace: PathBuf::from("/tmp"),
            max_output_chars: 1000,
        }
    }

    fn tool_input(name: &str) -> HookInput {
        HookInput {
            tool_name: Some(name.into()),
            ..Default::default()
        }
    }

    fn run(code: i32, stdout: &str, stderr: &str) -> HookRun {
        HookRun {
            exit_code: Some(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
            ..Default::default()
        }
    }

    #[test]
    fn globs_and_alternatives() {
        let m = Matcher::parse(Some("write_file | mcp__github__*"));
        assert!(m.matches(Some("write_file")));
        assert!(m.matches(Some("mcp__github__create_issue")));
        assert!(!m.matches(Some("shell")));
        assert!(!m.matches(None));
        assert!(Matcher::parse(Some("*")).matches(Some("x")));
        assert!(Matcher::parse(None).matches(None));
        assert!(Matcher::parse(Some("sh?ll")).matches(Some("shell")));
        assert!(!Matcher::parse(Some("shell")).matches(Some("shell2")));
        assert!(glob("a*b*c", "aXbYbZc"));
        assert!(!glob("a*b*c", "aXbYbZ"));
    }

    #[test]
    fn exit_codes_mean_what_claude_code_says() {
        let e = HookEvent::PreToolUse;
        assert_eq!(
            interpret(e, "g", &run(0, "", ""), 100),
            Verdict::Proceed { context: None }
        );
        assert_eq!(
            interpret(e, "g", &run(2, "", "no rm here\n"), 100),
            Verdict::Block {
                reason: "no rm here".into()
            }
        );
        assert_eq!(
            interpret(e, "g", &run(2, "", ""), 100),
            Verdict::Block {
                reason: "blocked by `g`".into()
            }
        );
        assert!(matches!(
            interpret(e, "g", &run(1, "", "boom"), 100),
            Verdict::Error { note } if note == "exit code 1: boom"
        ));
        // Exit 2 where nothing can be blocked is just an error.
        assert!(matches!(
            interpret(HookEvent::SessionEnd, "g", &run(2, "", "x"), 100),
            Verdict::Error { .. }
        ));
        let timed_out = HookRun {
            timed_out: true,
            ..Default::default()
        };
        assert!(matches!(
            interpret(e, "g", &timed_out, 100),
            Verdict::Error { .. }
        ));
    }

    #[test]
    fn json_on_stdout_blocks_or_adds_a_note() {
        let deny = r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"not in prod"}}"#;
        assert_eq!(
            interpret(HookEvent::PreToolUse, "g", &run(0, deny, ""), 100),
            Verdict::Block {
                reason: "not in prod".into()
            }
        );
        let block = r#"{"decision":"block","reason":"tests first"}"#;
        assert_eq!(
            interpret(HookEvent::Stop, "g", &run(0, block, ""), 100),
            Verdict::Block {
                reason: "tests first".into()
            }
        );
        let note = r#"{"hookSpecificOutput":{"additionalContext":"the build is at /out"}}"#;
        assert_eq!(
            interpret(HookEvent::PostToolUse, "g", &run(0, note, ""), 100),
            Verdict::Proceed {
                context: Some("the build is at /out".into())
            }
        );
        // Plain stdout is a note only where Claude Code makes it one.
        assert_eq!(
            interpret(
                HookEvent::UserPromptSubmit,
                "g",
                &run(0, "today is Friday", ""),
                100
            ),
            Verdict::Proceed {
                context: Some("today is Friday".into())
            }
        );
        assert_eq!(
            interpret(HookEvent::PreToolUse, "g", &run(0, "chatter", ""), 100),
            Verdict::Proceed { context: None }
        );
    }

    #[test]
    fn reasons_and_notes_are_capped() {
        let long = "x".repeat(50);
        match interpret(HookEvent::PreToolUse, "g", &run(2, "", &long), 10) {
            Verdict::Block { reason } => assert_eq!(reason, format!("{}\n[… cut]", "x".repeat(10))),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn hooks_run_in_order_and_the_first_block_wins() {
        let log = Arc::new(Log::default());
        let mut set = HookSet::new().with_audit(log.clone());
        let ws = fixed("ws", 0, "", "");
        let user_block = fixed("user-block", 2, "", "nope");
        let user_ok = fixed("user-ok", 0, r#"{"additionalContext":"note"}"#, "");
        set.add(Hook::new(
            HookEvent::PreToolUse,
            Matcher::default(),
            HookSource::Workspace,
            ws.clone(),
        ));
        set.add(Hook::new(
            HookEvent::PreToolUse,
            Matcher::parse(Some("shell")),
            HookSource::User,
            user_ok.clone(),
        ));
        set.add(Hook::new(
            HookEvent::PreToolUse,
            Matcher::parse(Some("shell")),
            HookSource::User,
            user_block.clone(),
        ));
        let fired = set
            .fire(HookEvent::PreToolUse, tool_input("shell"), &ctx())
            .await;
        assert_eq!(fired.block, Some(("user-block".into(), "nope".into())));
        assert_eq!(user_ok.calls.lock().unwrap().len(), 1);
        assert_eq!(
            user_ok.calls.lock().unwrap()[0].hook_event_name,
            "PreToolUse"
        );
        // The workspace hook comes after the user's and is skipped.
        assert!(ws.calls.lock().unwrap().is_empty());
        let summary: Vec<(String, bool, bool)> = log
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|r| (r.command.clone(), r.blocked, r.skipped))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("user-ok".to_string(), false, false),
                ("user-block".to_string(), true, false),
                ("ws".to_string(), false, true)
            ]
        );

        // Another tool: only the workspace hook matches.
        let fired = set
            .fire(HookEvent::PreToolUse, tool_input("read_file"), &ctx())
            .await;
        assert!(fired.block.is_none());
        assert_eq!(ws.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn notes_are_joined_and_capped_where_they_reach_the_model() {
        let mut set = HookSet::new().with_limits(HookLimits {
            max_context_chars: 12,
            max_stop_blocks: 3,
        });
        for name in ["a", "b"] {
            set.add(Hook::new(
                HookEvent::PostToolUse,
                Matcher::default(),
                HookSource::User,
                fixed(
                    name,
                    0,
                    &format!("{{\"additionalContext\":\"note {name}\"}}"),
                    "",
                ),
            ));
        }
        let fired = set
            .fire(HookEvent::PostToolUse, tool_input("shell"), &ctx())
            .await;
        assert_eq!(fired.context.as_deref(), Some("note a\n\nnote\n[… cut]"));
    }

    #[test]
    fn the_same_hook_twice_runs_once_and_a_child_gets_only_tool_hooks() {
        let mut set = HookSet::new();
        let h = fixed("guard", 0, "", "");
        for source in [HookSource::User, HookSource::Workspace] {
            set.add(Hook::new(
                HookEvent::PreToolUse,
                Matcher::parse(Some("shell")),
                source,
                h.clone(),
            ));
        }
        for event in [
            HookEvent::PostToolUse,
            HookEvent::Stop,
            HookEvent::SessionStart,
        ] {
            set.add(Hook::new(
                event,
                Matcher::default(),
                HookSource::User,
                fixed("x", 0, "", ""),
            ));
        }
        assert_eq!(set.hooks().len(), 4);
        let child = set.for_child("a-1", "root");
        let events: Vec<HookEvent> = child.hooks().iter().map(|h| h.event).collect();
        assert_eq!(events, vec![HookEvent::PreToolUse, HookEvent::PostToolUse]);
        let input = child.input("agent-a-1", None, PathBuf::from("/w"));
        assert_eq!(input.agent_id.as_deref(), Some("a-1"));
        assert_eq!(input.parent_session_id.as_deref(), Some("root"));
    }

    #[test]
    fn the_payload_leaves_out_what_doesnt_apply() {
        let input = HookInput {
            hook_event_name: "PreToolUse".into(),
            session_id: "s".into(),
            cwd: PathBuf::from("/w"),
            tool_name: Some("shell".into()),
            tool_input: Some(serde_json::json!({"command": "ls"})),
            ..Default::default()
        };
        let v = serde_json::to_value(&input).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "hook_event_name": "PreToolUse",
                "session_id": "s",
                "cwd": "/w",
                "tool_name": "shell",
                "tool_input": {"command": "ls"},
            })
        );
    }
}
