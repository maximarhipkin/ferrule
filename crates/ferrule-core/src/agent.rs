use crate::error::CoreError;
use crate::event::AgentEvent;
use crate::ledger::{LedgerContext, LedgerRecord, LedgerSink};
use crate::message::{Message, Usage};
use crate::profile::{HarnessProfile, COMPACTION_TEMPLATE};
use crate::provider::{CompletionRequest, CompletionResponse, Provider};
use crate::stuck::{Step, Stuck};
use crate::tool::{ToolContext, ToolRegistry};
use crate::transcript::Transcript;
use crate::verify::Verifier;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Wrapper the skills system puts around an activated skill's instructions
/// (`<skill_content name="x">…</skill_content>`). Compaction carries these
/// blocks forward verbatim instead of summarising them: a summary of "how
/// to do X" loses exactly the detail the skill exists to provide.
pub const SKILL_CONTENT_OPEN: &str = "<skill_content name=\"";
pub const SKILL_CONTENT_CLOSE: &str = "</skill_content>";

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub max_iterations: usize,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Number of trailing messages always kept verbatim through compaction.
    pub compaction_keep_last: usize,
    pub retry: RetryPolicy,
    /// Failed checks a run gets to fix before it stops (see [`Verifier`]).
    pub max_verify_rounds: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 60,
            max_output_tokens: None,
            temperature: None,
            compaction_keep_last: 6,
            retry: RetryPolicy::default(),
            max_verify_rounds: 3,
        }
    }
}

/// How a provider call that failed transiently (`CoreError::Transient`) is
/// retried: exponential backoff with jitter, or the server's `Retry-After`
/// when it gives one, all within one time budget.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Tries per call, the first included. 1 turns retrying off.
    pub max_attempts: u32,
    pub base_delay: Duration,
    /// Cap on the computed backoff. A server's `Retry-After` is only held
    /// to the budget.
    pub max_delay: Duration,
    /// No retry that would start later than this after the first try.
    pub budget: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 4, base_delay: Duration::from_secs(2), max_delay: Duration::from_secs(30), budget: Duration::from_secs(120) }
    }
}

impl RetryPolicy {
    /// The wait before trying again after `attempt` (1-based) failed with
    /// `err`, `elapsed` after the first try started; `None` to give up.
    pub fn delay(&self, err: &CoreError, attempt: u32, elapsed: Duration) -> Option<Duration> {
        let CoreError::Transient { retry_after, .. } = err else {
            return None;
        };
        if attempt >= self.max_attempts {
            return None;
        }
        let delay = match retry_after {
            Some(d) => *d,
            None => jitter(self.base_delay.saturating_mul(1 << (attempt - 1).min(16)).min(self.max_delay)),
        };
        (elapsed + delay <= self.budget).then_some(delay)
    }
}

/// Somewhere in the upper half of `d`, so sessions that failed together
/// don't all come back at the same instant.
fn jitter(d: Duration) -> Duration {
    use std::hash::BuildHasher;
    // Each `RandomState` is freshly keyed: a random number without a crate.
    let r = std::collections::hash_map::RandomState::new().hash_one(0u8);
    let half = d / 2;
    half + Duration::from_nanos(r % (half.as_nanos() as u64 + 1))
}

/// Why a run ended before the model said it was done.
#[derive(Debug, Clone)]
enum StopReason {
    MaxIterations(usize),
    Stuck(Stuck),
    VerifyFailing { check: String, rounds: usize },
}

impl std::fmt::Display for StopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StopReason::MaxIterations(n) => write!(f, "it reached the limit of {n} step{}", if *n == 1 { "" } else { "s" }),
            StopReason::Stuck(stuck) => f.write_str(&stuck.reason()),
            StopReason::VerifyFailing { check, rounds } => {
                let rounds = if *rounds == 1 { "one round".to_string() } else { format!("{rounds} rounds") };
                write!(f, "`{check}` still fails after {rounds} of fixes")
            }
        }
    }
}

impl StopReason {
    fn into_error(self) -> CoreError {
        match self {
            StopReason::MaxIterations(n) => CoreError::MaxIterations(n),
            other => CoreError::Stopped(other.to_string()),
        }
    }
}

/// One serialized agent run loop. Construct one per session; do not drive it
/// concurrently — sessions are serialized by the caller (session lane).
pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    profile: HarnessProfile,
    config: AgentConfig,
    tool_ctx: ToolContext,
    transcript: Option<Transcript>,
    ledger: Option<LedgerContext>,
    verifier: Option<Arc<dyn Verifier>>,
    /// What the current run was asked to do: kept verbatim through
    /// compaction, since it's what says when the work is done.
    goal: Option<String>,
    pub messages: Vec<Message>,
    pub usage: Usage,
    /// Set when the last run stopped before finishing (the step limit, a
    /// loop, a check that kept failing): why. Its answer is then a status,
    /// not a result.
    pub incomplete: Option<String>,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: ToolRegistry,
        profile: HarnessProfile,
        config: AgentConfig,
        tool_ctx: ToolContext,
        transcript: Option<Transcript>,
    ) -> Self {
        Self {
            provider,
            tools,
            profile,
            config,
            tool_ctx,
            transcript,
            ledger: None,
            verifier: None,
            goal: None,
            messages: Vec::new(),
            usage: Usage::default(),
            incomplete: None,
        }
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.messages.push(Message::system(prompt.into()));
        self
    }

    /// Attach a per-call ledger sink (Phase 0, see `crate::ledger`). Default
    /// is no sink — existing callers/tests are unaffected. `task_shape` and
    /// `origin` identify *why* this session exists (`"run"`, `"chat"`,
    /// `"gateway"` + channel, `"scheduler"` + task id); `model` is supplied
    /// by the caller because `Provider` exposes only `name()`, not a model.
    pub fn with_ledger(mut self, sink: Arc<dyn LedgerSink>, task_shape: impl Into<String>, origin: Option<String>, model: impl Into<String>) -> Self {
        self.ledger = Some(LedgerContext { sink, task_shape: task_shape.into(), origin, model: model.into() });
        self
    }

    /// Check the work before a run that changed files may finish; see
    /// [`Verifier`].
    pub fn with_verifier(mut self, verifier: Arc<dyn Verifier>) -> Self {
        self.verifier = Some(verifier);
        self
    }

    async fn emit(&self, tx: &mpsc::Sender<AgentEvent>, ev: AgentEvent) {
        let _ = tx.send(ev).await; // receiver may be detached in headless mode
    }

    fn est_context_tokens(&self) -> usize {
        self.messages.iter().map(|m| m.est_tokens()).sum()
    }

    fn session_id(&self) -> String {
        self.transcript
            .as_ref()
            .and_then(|t| t.path().file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "ephemeral".into())
    }

    /// Time a `provider.complete()` call and, if a ledger sink is attached,
    /// emit one row for it — success or failure. This is the only place
    /// that writes to the ledger; both the main loop and the compaction
    /// summary call go through it.
    ///
    /// A transient failure is tried again per `config.retry`; every attempt
    /// gets its own row, the ones that led to a retry marked `"retried"`.
    async fn call_provider(
        &self,
        tx: &mpsc::Sender<AgentEvent>,
        req: CompletionRequest,
        iteration: usize,
        call_kind: &str,
    ) -> Result<CompletionResponse, CoreError> {
        let first = Instant::now();
        let mut attempt = 1;
        loop {
            let start = Instant::now();
            let result = self.provider.complete(req.clone()).await;
            let latency_ms = start.elapsed().as_millis() as u64;
            let retry_in = match &result {
                Err(e) => self.config.retry.delay(e, attempt, first.elapsed()),
                Ok(_) => None,
            };
            self.record_completion(iteration, call_kind, latency_ms, &result, retry_in.is_some());
            let (Some(delay), Err(e)) = (retry_in, &result) else {
                return result;
            };
            warn!(attempt, delay_ms = delay.as_millis() as u64, "provider call failed, retrying: {e}");
            self.emit(
                tx,
                AgentEvent::ProviderRetry {
                    attempt,
                    max_attempts: self.config.retry.max_attempts,
                    delay_ms: delay.as_millis() as u64,
                    error: e.to_string(),
                },
            )
            .await;
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    fn record_completion(
        &self,
        iteration: usize,
        call_kind: &str,
        latency_ms: u64,
        result: &Result<CompletionResponse, CoreError>,
        retried: bool,
    ) {
        let Some(ledger) = &self.ledger else { return };
        let (input_tokens, cached_input_tokens, output_tokens, tool_calls, outcome, error_kind, error_message) = match result {
            Ok(resp) => (
                resp.usage.input_tokens,
                resp.usage.cached_input_tokens,
                resp.usage.output_tokens,
                resp.message.tool_calls.len(),
                "ok".to_string(),
                None,
                None,
            ),
            Err(e) => {
                let outcome = if retried { "retried" } else { "error" };
                (0, 0, 0, 0, outcome.to_string(), Some(error_kind_of(e)), Some(truncate_error(&e.to_string())))
            }
        };
        let record = LedgerRecord {
            timestamp: chrono::Utc::now().to_rfc3339(),
            session_id: self.session_id(),
            task_shape: ledger.task_shape.clone(),
            origin: ledger.origin.clone(),
            provider: self.provider.name().to_string(),
            model: ledger.model.clone(),
            iteration,
            call_kind: call_kind.to_string(),
            input_tokens,
            cached_input_tokens,
            output_tokens,
            tool_calls,
            latency_ms,
            outcome,
            error_kind,
            error_message,
            cost_usd: None,
        };
        ledger.sink.record(record);
    }

    /// The ReAct loop: call → tool calls → observe → repeat until text-only.
    ///
    /// A run that can't finish still answers: at the step limit, in a loop
    /// it was already warned about, or with a check that keeps failing, it
    /// stops with a status of where things stand ([`Agent::incomplete`]
    /// says why) instead of an error.
    pub async fn run(&mut self, goal: &str, tx: mpsc::Sender<AgentEvent>) -> Result<String, CoreError> {
        let session_id = self.session_id();
        self.emit(&tx, AgentEvent::RunStarted { session_id, goal: goal.into() }).await;
        self.goal = Some(goal.to_string());
        self.incomplete = None;
        self.push(Message::user(goal));

        let mut steps: Vec<Step> = Vec::new();
        let mut nudged = false;
        // A tool that changes files succeeded, so the check has to pass
        // before the run may finish.
        let mut unverified = false;
        let mut failed_checks = 0;

        for iteration in 0..self.config.max_iterations {
            self.maybe_compact(&tx, iteration).await?;
            let resp = self.call_provider(&tx, self.request(), iteration, "turn").await?;
            self.add_usage(&tx, &resp.usage).await;

            let msg = resp.message;
            if let Some(text) = &msg.content {
                if !text.is_empty() {
                    self.emit(&tx, AgentEvent::AssistantText { text: text.clone() }).await;
                }
            }
            if let Some(r) = &msg.reasoning {
                if !r.is_empty() {
                    self.emit(&tx, AgentEvent::Reasoning { text: r.clone() }).await;
                }
            }

            let finished = msg.tool_calls.is_empty();
            self.push(msg.clone());

            if finished {
                if let Some(verifier) = self.verifier.clone().filter(|_| unverified) {
                    let check = verifier.describe();
                    self.emit(&tx, AgentEvent::VerifyStarted { check: check.clone() }).await;
                    let result = verifier.verify(&self.tool_ctx).await;
                    self.emit(&tx, AgentEvent::VerifyFinished { check: check.clone(), ok: result.is_ok() }).await;
                    if let Err(output) = result {
                        failed_checks += 1;
                        if failed_checks > self.config.max_verify_rounds {
                            let reason = StopReason::VerifyFailing { check, rounds: self.config.max_verify_rounds };
                            return self.wrap_up(&tx, iteration + 1, reason).await;
                        }
                        self.push(Message::user(format!(
                            "[ferrule] `{check}` fails, so this isn't done yet. Fix what it reports, then finish again; \
                             it runs again when you do.\n\n{output}"
                        )));
                        continue;
                    }
                }
                let answer = msg.content.unwrap_or_default();
                self.emit(&tx, AgentEvent::RunFinished { answer_chars: answer.len(), iterations: iteration + 1 }).await;
                return Ok(answer);
            }

            for call in &msg.tool_calls {
                self.emit(&tx, AgentEvent::ToolCallStarted {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })
                .await;

                let result = self.tools.call(&call.name, call.arguments.clone(), &self.tool_ctx).await;
                let (content, ok) = match result {
                    Ok(out) => (out.content, true),
                    Err(e) => (format!("error: {e}"), false),
                };
                if !ok {
                    warn!(tool = %call.name, "tool call failed");
                }
                if ok && self.tools.changes_files(&call.name) {
                    unverified = true;
                }
                self.emit(&tx, AgentEvent::ToolCallFinished {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    ok,
                    output_chars: content.len(),
                })
                .await;

                steps.push(Step::new(&call.name, &call.arguments, &content, ok));
                self.push(Message::tool_result(&call.id, content));
            }

            if let Some(stuck) = Stuck::detect(&steps) {
                if nudged {
                    return self.wrap_up(&tx, iteration + 1, StopReason::Stuck(stuck)).await;
                }
                // One warning first: told what it's repeating, a model
                // usually changes course.
                nudged = true;
                steps.clear();
                let note = stuck.nudge();
                warn!(?stuck, "the run is going in circles");
                self.emit(&tx, AgentEvent::Stuck { note: note.clone() }).await;
                self.push(Message::user(note));
            }
        }
        let limit = self.config.max_iterations;
        self.wrap_up(&tx, limit, StopReason::MaxIterations(limit)).await
    }

    /// Ends a run that can't finish with one more call, for a status the
    /// person can act on: what got done, what's blocking, what's next. An
    /// error would throw all of that away.
    async fn wrap_up(&mut self, tx: &mpsc::Sender<AgentEvent>, iterations: usize, reason: StopReason) -> Result<String, CoreError> {
        let why = reason.to_string();
        warn!(%why, "stopping the run before it finished");
        self.incomplete = Some(why.clone());
        self.push(Message::user(format!(
            "[ferrule] Stopping here: {why}. Don't call any tools. Write a short status for the person you are \
             working for, in their language: say first that you stopped before finishing and why, then what \
             you got done, what is blocking, and the next step or the question they need to answer."
        )));
        if let Err(e) = self.maybe_compact(tx, iterations).await {
            warn!("compaction before the status answer failed: {e}");
        }
        // The tools stay declared: some APIs refuse a history that holds
        // tool calls when the request has no tools.
        let resp = match self.call_provider(tx, self.request(), iterations, "status").await {
            Ok(resp) => resp,
            Err(e) => {
                warn!("the status answer failed: {e}");
                return Err(reason.into_error());
            }
        };
        self.add_usage(tx, &resp.usage).await;
        let mut msg = resp.message;
        msg.tool_calls.clear();
        let answer = match msg.content.as_deref().map(str::trim) {
            Some(text) if !text.is_empty() => text.to_string(),
            _ => format!("Stopped before finishing: {why}."),
        };
        msg.content = Some(answer.clone());
        self.emit(tx, AgentEvent::AssistantText { text: answer.clone() }).await;
        self.push(msg);
        self.emit(tx, AgentEvent::RunIncomplete { reason: why, iterations }).await;
        Ok(answer)
    }

    fn request(&self) -> CompletionRequest {
        CompletionRequest {
            messages: self.rendered_messages(),
            tools: self.tools.definitions(),
            max_output_tokens: self.config.max_output_tokens,
            temperature: self.config.temperature,
        }
    }

    async fn add_usage(&mut self, tx: &mpsc::Sender<AgentEvent>, usage: &Usage) {
        self.usage.input_tokens += usage.input_tokens;
        self.usage.output_tokens += usage.output_tokens;
        self.usage.cached_input_tokens += usage.cached_input_tokens;
        self.emit(
            tx,
            AgentEvent::Usage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cached_input_tokens: usage.cached_input_tokens,
            },
        )
        .await;
    }

    /// Adds to the history and the transcript.
    fn push(&mut self, msg: Message) {
        self.log(&msg);
        self.messages.push(msg);
    }

    fn log(&self, msg: &Message) {
        if let Some(t) = &self.transcript {
            if let Err(e) = t.log_message(msg) {
                warn!("transcript write failed: {e}");
            }
        }
    }

    /// Messages as sent to the provider: reasoning stripped when the profile
    /// says the model neither needs nor accepts it back.
    fn rendered_messages(&self) -> Vec<Message> {
        if self.profile.retain_reasoning {
            return self.messages.clone();
        }
        self.messages
            .iter()
            .map(|m| {
                let mut m = m.clone();
                m.reasoning = None;
                m
            })
            .collect()
    }

    /// Compaction: deterministic dedupe first (free), then structured LLM
    /// summary of everything before the trailing verbatim window.
    async fn maybe_compact(&mut self, tx: &mpsc::Sender<AgentEvent>, iteration: usize) -> Result<(), CoreError> {
        let before = self.est_context_tokens();
        let trigger = self.profile.compaction_trigger_tokens();
        if before <= trigger {
            self.emit(tx, AgentEvent::ContextReady { est_tokens: before, threshold_tokens: trigger }).await;
            return Ok(());
        }
        info!(before, trigger, "compacting context");

        self.dedupe_tool_results();

        let keep = self.config.compaction_keep_last;
        if self.messages.len() <= keep + 1 {
            return Ok(()); // nothing foldable
        }
        let split = self.messages.len() - keep;
        let head = &self.messages[..split];

        // Skills activated in the folded part stay in force: their blocks
        // ride along verbatim (unless the verbatim tail already holds them)
        // and are kept out of the summarizer's input.
        let in_tail = skill_blocks(&self.messages[split..]);
        let carried: Vec<String> = skill_blocks(head)
            .into_iter()
            .filter(|(name, _)| !in_tail.iter().any(|(n, _)| n == name))
            .map(|(_, block)| block)
            .collect();

        // Skip a second compaction if the head is already mostly a summary.
        let transcript_text = head
            .iter()
            .map(|m| {
                let role = format!("{:?}", m.role).to_lowercase();
                let body = m.content.clone().unwrap_or_default();
                let body = if may_hold_skill(m) { elide_skill_blocks(&body) } else { body };
                format!("{role}: {body}")
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let summary_req = CompletionRequest {
            messages: vec![Message::user(format!("{COMPACTION_TEMPLATE}{transcript_text}"))],
            tools: vec![],
            max_output_tokens: Some(4096),
            temperature: Some(0.0),
        };
        let summary = self.call_provider(tx, summary_req, iteration, "compaction").await?.message.content.unwrap_or_default();

        let mut rebuilt = Vec::with_capacity(keep + 2);
        if let Some(sys) = self.messages.first().filter(|m| m.role == crate::message::Role::System) {
            rebuilt.push(sys.clone());
        }
        let mut summary_msg = format!("[Compaction summary of earlier session]\n{summary}");
        if !carried.is_empty() {
            summary_msg.push_str("\n\n[Skill instructions activated earlier in this session — still in force]\n");
            summary_msg.push_str(&carried.join("\n\n"));
        }
        // The request itself, word for word: it's what says when the work
        // is done, and a summary tends to blur exactly that.
        let goal_in_tail = |goal: &str| {
            self.messages[split..].iter().any(|m| m.role == crate::message::Role::User && m.content.as_deref() == Some(goal))
        };
        if let Some(goal) = self.goal.as_deref().filter(|g| !goal_in_tail(g)) {
            summary_msg.push_str("\n\n[The request being worked on, verbatim]\n");
            summary_msg.push_str(goal);
        }
        summary_msg.push_str("\n\nContinue from here.");
        rebuilt.push(Message::user(summary_msg));
        rebuilt.extend(self.messages[split..].iter().cloned());

        let folded = self.messages.len() - rebuilt.len();
        self.messages = rebuilt;
        let after = self.est_context_tokens();
        self.emit(tx, AgentEvent::Compacted { folded_messages: folded, est_tokens_before: before, est_tokens_after: after }).await;
        if let Some(t) = &self.transcript {
            let _ = t.log_event(&format!("compacted: {folded} messages, {before} -> {after} est tokens"));
        }
        Ok(())
    }

    /// Drop duplicate tool results, keeping only the most recent copy —
    /// deterministic 15-30% context savings with zero information loss.
    fn dedupe_tool_results(&mut self) {
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (i, m) in self.messages.iter().enumerate() {
            if m.role == crate::message::Role::Tool {
                if let Some(c) = &m.content {
                    seen.insert(c.clone(), i);
                }
            }
        }
        let mut latest: std::collections::HashSet<usize> = seen.values().cloned().collect();
        latest.insert(self.messages.len().saturating_sub(1));
        for (i, m) in self.messages.iter_mut().enumerate() {
            if m.role == crate::message::Role::Tool
                && !latest.contains(&i)
                && m.content.is_some()
                && seen.contains_key(m.content.as_ref().unwrap())
            {
                m.content = Some("[superseded by identical later tool result]".into());
            }
        }
    }
}

/// Skill blocks arrive as tool results and, after a compaction, inside the
/// summary (a user message). Anything else quoting the tag is left alone.
fn may_hold_skill(m: &Message) -> bool {
    matches!(m.role, crate::message::Role::Tool | crate::message::Role::User)
}

/// Byte ranges and names of the `<skill_content>` blocks in `text`. A block
/// whose close tag was cut off runs to the end of the text.
fn skill_spans(text: &str) -> Vec<(std::ops::Range<usize>, &str)> {
    let mut spans = Vec::new();
    let mut pos = 0;
    while let Some(i) = text[pos..].find(SKILL_CONTENT_OPEN) {
        let start = pos + i;
        let name_start = start + SKILL_CONTENT_OPEN.len();
        let Some(name_len) = text[name_start..].find('"') else {
            break;
        };
        let end = text[name_start..]
            .find(SKILL_CONTENT_CLOSE)
            .map(|j| name_start + j + SKILL_CONTENT_CLOSE.len())
            .unwrap_or(text.len());
        spans.push((start..end, &text[name_start..name_start + name_len]));
        pos = end;
    }
    spans
}

/// `(name, block)` for every skill activated in `messages`, first-seen
/// order, the latest copy of each name winning.
fn skill_blocks(messages: &[Message]) -> Vec<(String, String)> {
    let mut blocks: Vec<(String, String)> = Vec::new();
    for text in messages.iter().filter(|m| may_hold_skill(m)).filter_map(|m| m.content.as_deref()) {
        for (range, name) in skill_spans(text) {
            let block = text[range].to_string();
            match blocks.iter_mut().find(|(n, _)| n == name) {
                Some(slot) => slot.1 = block,
                None => blocks.push((name.to_string(), block)),
            }
        }
    }
    blocks
}

fn elide_skill_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    for (range, name) in skill_spans(text) {
        out.push_str(&text[pos..range.start]);
        out.push_str(&format!("[skill `{name}` instructions — carried forward verbatim, not part of this summary]"));
        pos = range.end;
    }
    out.push_str(&text[pos..]);
    out
}

fn error_kind_of(e: &CoreError) -> String {
    match e {
        CoreError::Provider(_) => "provider",
        CoreError::Transient { .. } => "transient",
        CoreError::MalformedResponse(_) => "malformed_response",
        CoreError::ToolNotFound(_) => "tool_not_found",
        CoreError::ToolFailed { .. } => "tool_failed",
        CoreError::Io(_) => "io",
        CoreError::Serde(_) => "serde",
        CoreError::MaxIterations(_) => "max_iterations",
        CoreError::Stopped(_) => "stopped",
        CoreError::Aborted(_) => "aborted",
    }
    .to_string()
}

/// Keep ledger rows small: the JSONL file is meant to be grepped/aggregated,
/// not to hold full error dumps.
fn truncate_error(s: &str) -> String {
    const MAX_CHARS: usize = 500;
    if s.chars().count() <= MAX_CHARS {
        return s.to_string();
    }
    let mut truncated: String = s.chars().take(MAX_CHARS).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::CompletionResponse;
    use crate::tool::{Tool, ToolDefinition, ToolOutput};
    use std::sync::Mutex;

    struct ScriptProvider {
        responses: Mutex<Vec<Message>>,
    }

    #[async_trait::async_trait]
    impl Provider for ScriptProvider {
        fn name(&self) -> &str {
            "script"
        }
        async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            let msg = self.responses.lock().unwrap().remove(0);
            Ok(CompletionResponse { message: msg, usage: Usage { input_tokens: 10, output_tokens: 5, cached_input_tokens: 0 } })
        }
    }

    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "echo".into(),
                description: "echo back".into(),
                parameters: serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}}),
            }
        }
        async fn call(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
            Ok(ToolOutput::ok(args["text"].as_str().unwrap_or("").to_string()))
        }
    }

    fn make_agent(script: Vec<Message>) -> Agent {
        agent_with(script, AgentConfig::default())
    }

    fn agent_with(script: Vec<Message>, config: AgentConfig) -> Agent {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        Agent::new(
            Arc::new(ScriptProvider { responses: Mutex::new(script) }),
            reg,
            HarnessProfile::generic(),
            config,
            ToolContext::default(),
            None,
        )
    }

    fn echo(text: &str) -> Message {
        let call = crate::message::ToolCall { id: "1".into(), name: "echo".into(), arguments: serde_json::json!({ "text": text }) };
        Message::assistant(None, vec![call], None)
    }

    fn say(text: &str) -> Message {
        Message::assistant(Some(text.into()), vec![], None)
    }

    /// Room for every event of a long test run: `emit` waits on a full
    /// channel, and these tests only read after the run.
    fn events() -> (mpsc::Sender<AgentEvent>, mpsc::Receiver<AgentEvent>) {
        mpsc::channel(4096)
    }

    fn drain(rx: &mut mpsc::Receiver<AgentEvent>) -> Vec<AgentEvent> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[tokio::test]
    async fn loop_runs_tool_then_finishes() {
        let script = vec![
            Message::assistant(
                None,
                vec![crate::message::ToolCall { id: "1".into(), name: "echo".into(), arguments: serde_json::json!({"text": "hi"}) }],
                None,
            ),
            Message::assistant(Some("done: hi".into()), vec![], None),
        ];
        let mut agent = make_agent(script);
        let (tx, mut rx) = mpsc::channel(64);
        let answer = agent.run("say hi", tx).await.unwrap();
        assert_eq!(answer, "done: hi");
        assert_eq!(agent.messages.len(), 4); // user, assistant(call), tool, assistant(final)
        assert_eq!(agent.usage.input_tokens, 20);

        let mut saw_tool = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, AgentEvent::ToolCallFinished { ref name, ok: true, .. } if name == "echo") {
                saw_tool = true;
            }
        }
        assert!(saw_tool);
    }

    #[tokio::test]
    async fn unknown_tool_error_is_fed_back_not_crash() {
        let script = vec![
            Message::assistant(None, vec![crate::message::ToolCall { id: "1".into(), name: "nope".into(), arguments: serde_json::json!({}) }], None),
            Message::assistant(Some("recovered".into()), vec![], None),
        ];
        let mut agent = make_agent(script);
        let (tx, _rx) = mpsc::channel(64);
        let answer = agent.run("break things", tx).await.unwrap();
        assert_eq!(answer, "recovered");
        let tool_msg = &agent.messages[2];
        assert!(tool_msg.content.as_deref().unwrap().contains("error"));
    }

    #[tokio::test]
    async fn reasoning_stripped_only_when_profile_says_so() {
        let script = vec![Message::assistant(Some("ok".into()), vec![], Some("thinking...".into()))];
        let mut agent = make_agent(script);
        agent.profile = HarnessProfile::generic(); // retain_reasoning = false
        let (tx, _rx) = mpsc::channel(64);
        agent.run("t", tx).await.unwrap();
        assert!(agent.rendered_messages()[1].reasoning.is_none());

        let mut agent2 = make_agent(vec![Message::assistant(Some("ok".into()), vec![], Some("thinking...".into()))]);
        agent2.profile = HarnessProfile::kimi(); // retain_reasoning = true
        let (tx2, _rx2) = mpsc::channel(64);
        agent2.run("t", tx2).await.unwrap();
        assert!(agent2.rendered_messages()[1].reasoning.is_some());
    }

    struct RecordingSink {
        records: Mutex<Vec<LedgerRecord>>,
    }

    impl LedgerSink for RecordingSink {
        fn record(&self, record: LedgerRecord) {
            self.records.lock().unwrap().push(record);
        }
    }

    struct FailingProvider;

    #[async_trait::async_trait]
    impl Provider for FailingProvider {
        fn name(&self) -> &str {
            "failing"
        }
        async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            Err(CoreError::Provider("boom".into()))
        }
    }

    #[tokio::test]
    async fn failing_provider_call_still_writes_an_error_row() {
        let sink = Arc::new(RecordingSink { records: Mutex::new(Vec::new()) });
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        let mut agent = Agent::new(
            Arc::new(FailingProvider),
            reg,
            HarnessProfile::generic(),
            AgentConfig::default(),
            ToolContext::default(),
            None,
        )
        .with_ledger(sink.clone(), "run", None, "test-model");

        let (tx, _rx) = mpsc::channel(64);
        let result = agent.run("hi", tx).await;
        assert!(result.is_err());

        let records = sink.records.lock().unwrap();
        assert_eq!(records.len(), 1, "a failed call must still produce a row");
        let r = &records[0];
        assert_eq!(r.outcome, "error");
        assert_eq!(r.error_kind.as_deref(), Some("provider"));
        assert!(r.error_message.as_deref().unwrap().contains("boom"));
        assert_eq!(r.task_shape, "run");
        assert_eq!(r.model, "test-model");
        assert_eq!(r.provider, "failing");
        assert_eq!(r.iteration, 0);
        assert_eq!(r.input_tokens, 0);
    }

    /// Like `ScriptProvider` but with per-response usage, so a test can
    /// assert exact token numbers per ledger row instead of a fixed value.
    struct ScriptProviderWithUsage {
        responses: Mutex<Vec<(Message, Usage)>>,
    }

    #[async_trait::async_trait]
    impl Provider for ScriptProviderWithUsage {
        fn name(&self) -> &str {
            "script-usage"
        }
        async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            let (message, usage) = self.responses.lock().unwrap().remove(0);
            Ok(CompletionResponse { message, usage })
        }
    }

    #[tokio::test]
    async fn ledger_records_one_row_per_call_with_iteration_and_tokens() {
        let script = vec![
            (
                Message::assistant(
                    None,
                    vec![crate::message::ToolCall { id: "1".into(), name: "echo".into(), arguments: serde_json::json!({"text": "hi"}) }],
                    None,
                ),
                Usage { input_tokens: 100, output_tokens: 10, cached_input_tokens: 20 },
            ),
            (Message::assistant(Some("done".into()), vec![], None), Usage { input_tokens: 150, output_tokens: 8, cached_input_tokens: 30 }),
        ];
        let sink = Arc::new(RecordingSink { records: Mutex::new(Vec::new()) });
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        let mut agent = Agent::new(
            Arc::new(ScriptProviderWithUsage { responses: Mutex::new(script) }),
            reg,
            HarnessProfile::generic(),
            AgentConfig::default(),
            ToolContext::default(),
            None,
        )
        .with_ledger(sink.clone(), "chat", Some("telegram".into()), "test-model");

        let (tx, _rx) = mpsc::channel(64);
        let answer = agent.run("say hi", tx).await.unwrap();
        assert_eq!(answer, "done");

        let records = sink.records.lock().unwrap();
        assert_eq!(records.len(), 2, "one ledger row per provider call, including tool-call iterations");

        assert_eq!(records[0].iteration, 0);
        assert_eq!(records[0].input_tokens, 100);
        assert_eq!(records[0].cached_input_tokens, 20);
        assert_eq!(records[0].output_tokens, 10);
        assert_eq!(records[0].tool_calls, 1);
        assert_eq!(records[0].call_kind, "turn");
        assert_eq!(records[0].outcome, "ok");
        assert_eq!(records[0].task_shape, "chat");
        assert_eq!(records[0].origin.as_deref(), Some("telegram"));

        assert_eq!(records[1].iteration, 1);
        assert_eq!(records[1].input_tokens, 150);
        assert_eq!(records[1].cached_input_tokens, 30);
        assert_eq!(records[1].output_tokens, 8);
        assert_eq!(records[1].tool_calls, 0);
        assert_eq!(records[1].outcome, "ok");
    }

    /// Records every prompt it is sent and answers with a fixed summary.
    struct CapturingProvider {
        prompts: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Provider for CapturingProvider {
        fn name(&self) -> &str {
            "capture"
        }
        async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            let prompt = req.messages.iter().filter_map(|m| m.content.clone()).collect::<Vec<_>>().join("\n");
            self.prompts.lock().unwrap().push(prompt);
            Ok(CompletionResponse { message: Message::assistant(Some("SUMMARY".into()), vec![], None), usage: Usage::default() })
        }
    }

    #[tokio::test]
    async fn compaction_carries_skill_instructions_forward_verbatim() {
        let provider = Arc::new(CapturingProvider { prompts: Mutex::new(Vec::new()) });
        let mut profile = HarnessProfile::generic();
        profile.context_window = 1_000;
        profile.output_reserve = 0;
        profile.compaction_threshold = 0.1; // compact above ~100 tokens
        let config = AgentConfig { compaction_keep_last: 2, ..Default::default() };
        let mut agent = Agent::new(provider.clone(), ToolRegistry::new(), profile, config, ToolContext::default(), None)
            .with_system_prompt("sys");

        let block = format!("{SKILL_CONTENT_OPEN}pdf\">\nALWAYS-RUN-EXTRACT-FIRST\n{SKILL_CONTENT_CLOSE}");
        let call = crate::message::ToolCall { id: "1".into(), name: "activate_skill".into(), arguments: serde_json::json!({"name": "pdf"}) };
        agent.messages.push(Message::user("convert the pdf"));
        agent.messages.push(Message::assistant(None, vec![call], None));
        agent.messages.push(Message::tool_result("1", block.clone()));
        let filler = |i: usize| Message::user(format!("{}{i}", "filler ".repeat(50)));
        agent.messages.extend((0..4).map(filler));

        let summary_text = |agent: &Agent| {
            agent.messages.iter().filter_map(|m| m.content.clone()).find(|c| c.starts_with("[Compaction summary")).unwrap()
        };
        let (tx, _rx) = mpsc::channel(64);
        agent.maybe_compact(&tx, 0).await.unwrap();
        let first = summary_text(&agent);
        assert!(first.contains(&block), "skill block must survive compaction verbatim:\n{first}");
        assert!(first.ends_with("Continue from here."));
        {
            let prompts = provider.prompts.lock().unwrap();
            assert_eq!(prompts.len(), 1);
            assert!(!prompts[0].contains("ALWAYS-RUN-EXTRACT-FIRST"), "the summarizer must not see (and paraphrase) the skill body");
            assert!(prompts[0].contains("[skill `pdf` instructions"));
        }

        // A second compaction folds the first summary; the block rides along
        // once more, not twice.
        agent.messages.extend((4..6).map(filler));
        agent.maybe_compact(&tx, 1).await.unwrap();
        let second = summary_text(&agent);
        assert_eq!(second.matches(&block).count(), 1, "{second}");
        assert_eq!(provider.prompts.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn skill_block_still_in_the_verbatim_tail_is_not_duplicated() {
        let provider = Arc::new(CapturingProvider { prompts: Mutex::new(Vec::new()) });
        let mut profile = HarnessProfile::generic();
        profile.context_window = 1_000;
        profile.output_reserve = 0;
        profile.compaction_threshold = 0.1;
        let config = AgentConfig { compaction_keep_last: 2, ..Default::default() };
        let mut agent = Agent::new(provider, ToolRegistry::new(), profile, config, ToolContext::default(), None);

        let block = format!("{SKILL_CONTENT_OPEN}pdf\">\nbody\n{SKILL_CONTENT_CLOSE}");
        agent.messages.extend((0..4).map(|i| Message::user(format!("{}{i}", "filler ".repeat(50)))));
        agent.messages.push(Message::tool_result("1", block.clone()));
        agent.messages.push(Message::tool_result("2", format!("{block} again")));

        let (tx, _rx) = mpsc::channel(64);
        agent.maybe_compact(&tx, 0).await.unwrap();
        let summary = agent.messages[0].content.clone().unwrap();
        assert!(summary.starts_with("[Compaction summary"));
        assert!(!summary.contains(&block), "the tail already holds it");
    }

    fn fast_retry(max_attempts: u32) -> RetryPolicy {
        RetryPolicy { max_attempts, base_delay: Duration::from_millis(1), max_delay: Duration::from_millis(2), budget: Duration::from_secs(5) }
    }

    /// Fails transiently `failures` times, then answers.
    struct FlakyProvider {
        failures: Mutex<u32>,
    }

    #[async_trait::async_trait]
    impl Provider for FlakyProvider {
        fn name(&self) -> &str {
            "flaky"
        }
        async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            let mut left = self.failures.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                return Err(CoreError::Transient { message: "HTTP 503".into(), retry_after: None });
            }
            Ok(CompletionResponse { message: say("ok"), usage: Usage::default() })
        }
    }

    fn flaky_agent(failures: u32, max_attempts: u32, sink: Arc<RecordingSink>) -> Agent {
        let config = AgentConfig { retry: fast_retry(max_attempts), ..Default::default() };
        Agent::new(Arc::new(FlakyProvider { failures: Mutex::new(failures) }), ToolRegistry::new(), HarnessProfile::generic(), config, ToolContext::default(), None)
            .with_ledger(sink, "run", None, "m")
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_with_a_row_per_attempt() {
        let sink = Arc::new(RecordingSink { records: Mutex::new(Vec::new()) });
        let mut agent = flaky_agent(2, 4, sink.clone());
        let (tx, mut rx) = events();
        assert_eq!(agent.run("hi", tx).await.unwrap(), "ok");

        let outcomes: Vec<String> = sink.records.lock().unwrap().iter().map(|r| r.outcome.clone()).collect();
        assert_eq!(outcomes, ["retried", "retried", "ok"]);
        assert_eq!(sink.records.lock().unwrap()[0].error_kind.as_deref(), Some("transient"));
        let retries: Vec<u32> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::ProviderRetry { attempt, max_attempts: 4, .. } => Some(attempt),
                _ => None,
            })
            .collect();
        assert_eq!(retries, [1, 2]);
    }

    #[tokio::test]
    async fn retries_stop_at_max_attempts() {
        let sink = Arc::new(RecordingSink { records: Mutex::new(Vec::new()) });
        let mut agent = flaky_agent(10, 3, sink.clone());
        let (tx, _rx) = events();
        let err = agent.run("hi", tx).await.unwrap_err();
        assert!(err.is_transient(), "{err}");
        let outcomes: Vec<String> = sink.records.lock().unwrap().iter().map(|r| r.outcome.clone()).collect();
        assert_eq!(outcomes, ["retried", "retried", "error"]);
    }

    #[test]
    fn retry_delay_backs_off_within_bounds() {
        let policy = RetryPolicy::default();
        let transient = CoreError::Transient { message: "x".into(), retry_after: None };
        for attempt in 1..=3 {
            let full = Duration::from_secs(2 << (attempt - 1));
            let d = policy.delay(&transient, attempt, Duration::ZERO).unwrap();
            assert!(d >= full / 2 && d <= full, "attempt {attempt}: {d:?}");
        }
        assert_eq!(policy.delay(&transient, 4, Duration::ZERO), None, "4 attempts in all");
        assert_eq!(policy.delay(&transient, 1, Duration::from_secs(119)), None, "past the budget");
        assert_eq!(policy.delay(&CoreError::Provider("401".into()), 1, Duration::ZERO), None);

        // The server's own wait wins over the backoff cap, not over the budget.
        let told = CoreError::Transient { message: "429".into(), retry_after: Some(Duration::from_secs(45)) };
        assert_eq!(policy.delay(&told, 1, Duration::ZERO), Some(Duration::from_secs(45)));
        assert_eq!(policy.delay(&told, 1, Duration::from_secs(80)), None);
    }

    #[tokio::test]
    async fn the_step_limit_ends_with_a_status_not_an_error() {
        let config = AgentConfig { max_iterations: 2, ..Default::default() };
        let mut agent = agent_with(vec![echo("a"), echo("b"), say("Stopped: got a and b, c is next.")], config);
        let (tx, mut rx) = events();
        let answer = agent.run("do a, b and c", tx).await.unwrap();
        assert_eq!(answer, "Stopped: got a and b, c is next.");
        assert_eq!(agent.incomplete.as_deref(), Some("it reached the limit of 2 steps"));
        let asked = agent.messages[agent.messages.len() - 2].content.clone().unwrap();
        assert!(asked.starts_with("[ferrule] Stopping here: it reached the limit of 2 steps."), "{asked}");
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AgentEvent::RunIncomplete { iterations: 2, .. })));

        // The next run starts clean.
        agent.messages.clear();
        let mut agent = agent_with(vec![say("fine")], AgentConfig::default());
        let (tx, _rx) = events();
        agent.run("again", tx).await.unwrap();
        assert_eq!(agent.incomplete, None);
    }

    #[tokio::test]
    async fn a_status_answer_that_calls_tools_anyway_falls_back() {
        let config = AgentConfig { max_iterations: 1, ..Default::default() };
        let mut agent = agent_with(vec![echo("a"), echo("b")], config);
        let (tx, _rx) = events();
        let answer = agent.run("go", tx).await.unwrap();
        assert_eq!(answer, "Stopped before finishing: it reached the limit of 1 step.");
        assert!(agent.messages.last().unwrap().tool_calls.is_empty(), "no dangling tool call in the history");
    }

    #[tokio::test]
    async fn a_loop_gets_one_warning_then_the_run_stops() {
        let mut script = vec![echo("same"); 8];
        script.push(say("I'm stuck on the same result."));
        let mut agent = make_agent(script);
        let (tx, mut rx) = events();
        let answer = agent.run("loop", tx).await.unwrap();
        assert_eq!(answer, "I'm stuck on the same result.");
        assert_eq!(agent.incomplete.as_deref(), Some("it kept repeating the same `echo` call after being warned"));

        let events = drain(&mut rx);
        assert_eq!(events.iter().filter(|e| matches!(e, AgentEvent::Stuck { .. })).count(), 1);
        assert_eq!(events.iter().filter(|e| matches!(e, AgentEvent::ToolCallFinished { .. })).count(), 8);
        let warned = agent.messages.iter().filter_map(|m| m.content.as_deref()).filter(|c| c.contains("Doing it again won't change")).count();
        assert_eq!(warned, 1);
    }

    #[tokio::test]
    async fn a_warned_loop_that_changes_course_finishes_normally() {
        let mut script = vec![echo("same"); 4];
        script.extend([echo("different"), say("done")]);
        let mut agent = make_agent(script);
        let (tx, _rx) = events();
        assert_eq!(agent.run("loop", tx).await.unwrap(), "done");
        assert_eq!(agent.incomplete, None);
    }

    /// Answers from a script and counts its runs.
    struct ScriptedCheck {
        results: Mutex<Vec<Result<(), String>>>,
        runs: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl Verifier for ScriptedCheck {
        fn describe(&self) -> String {
            "cargo test".into()
        }
        async fn verify(&self, _ctx: &ToolContext) -> Result<(), String> {
            *self.runs.lock().unwrap() += 1;
            self.results.lock().unwrap().remove(0)
        }
    }

    fn check(results: Vec<Result<(), String>>) -> Arc<ScriptedCheck> {
        Arc::new(ScriptedCheck { results: Mutex::new(results), runs: Mutex::new(0) })
    }

    #[tokio::test]
    async fn a_failing_check_sends_the_run_back_to_work() {
        let verifier = check(vec![Err("test foo ... FAILED".into()), Ok(())]);
        let script = vec![echo("edit"), say("done"), echo("fix"), say("done, tests pass")];
        let mut agent = make_agent(script).with_verifier(verifier.clone());
        let (tx, mut rx) = events();
        assert_eq!(agent.run("fix the bug", tx).await.unwrap(), "done, tests pass");
        assert_eq!(*verifier.runs.lock().unwrap(), 2);
        assert_eq!(agent.incomplete, None);

        let told = agent.messages.iter().filter_map(|m| m.content.as_deref()).find(|c| c.starts_with("[ferrule] `cargo test` fails")).unwrap();
        assert!(told.ends_with("test foo ... FAILED"), "the check's output reaches the model: {told}");
        let results: Vec<bool> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::VerifyFinished { ok, .. } => Some(ok),
                _ => None,
            })
            .collect();
        assert_eq!(results, [false, true]);
    }

    #[tokio::test]
    async fn nothing_changed_nothing_to_check() {
        let verifier = check(vec![]);
        let mut agent = make_agent(vec![say("the answer is 4")]).with_verifier(verifier.clone());
        let (tx, _rx) = events();
        agent.run("what is 2+2", tx).await.unwrap();
        assert_eq!(*verifier.runs.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn a_check_that_keeps_failing_ends_with_a_status() {
        let verifier = check(vec![Err("1 failed".into()), Err("still 1 failed".into())]);
        let config = AgentConfig { max_verify_rounds: 1, ..Default::default() };
        let script = vec![echo("edit"), say("done"), say("done now"), say("One test still fails; I couldn't find why.")];
        let mut agent = agent_with(script, config).with_verifier(verifier.clone());
        let (tx, _rx) = events();
        let answer = agent.run("fix it", tx).await.unwrap();
        assert_eq!(answer, "One test still fails; I couldn't find why.");
        assert_eq!(agent.incomplete.as_deref(), Some("`cargo test` still fails after one round of fixes"));
        assert_eq!(*verifier.runs.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn compaction_keeps_the_request_verbatim() {
        let provider = Arc::new(CapturingProvider { prompts: Mutex::new(Vec::new()) });
        let mut profile = HarnessProfile::generic();
        profile.context_window = 1_000;
        profile.output_reserve = 0;
        profile.compaction_threshold = 0.1;
        let config = AgentConfig { compaction_keep_last: 2, ..Default::default() };
        let mut agent = Agent::new(provider, ToolRegistry::new(), profile, config, ToolContext::default(), None);

        let goal = "Rename every `Foo` to `Bar`, but not in tests/.";
        agent.goal = Some(goal.into());
        agent.messages.push(Message::user(goal));
        agent.messages.extend((0..4).map(|i| Message::user(format!("{}{i}", "filler ".repeat(50)))));
        let (tx, _rx) = events();
        agent.maybe_compact(&tx, 0).await.unwrap();
        let summary = agent.messages[0].content.clone().unwrap();
        assert!(summary.contains(&format!("[The request being worked on, verbatim]\n{goal}")), "{summary}");
        assert!(summary.ends_with("Continue from here."));

        // Still in the verbatim tail: not repeated.
        agent.messages.push(Message::user(goal));
        agent.messages.push(Message::user("last"));
        agent.maybe_compact(&tx, 1).await.unwrap();
        let summary = agent.messages[0].content.clone().unwrap();
        assert!(!summary.contains(goal), "{summary}");
    }
}
