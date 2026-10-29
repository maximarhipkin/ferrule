use crate::message::{Message, Usage};
use crate::tool::ToolDefinition;

/// What the loop sends to a provider on each turn.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_output_tokens: Option<u32>,
    /// Harness-controlled: keep sampling deterministic-ish by default.
    pub temperature: Option<f32>,
    /// Stream the reply into this sink (M27). `None`: the driver sends and
    /// parses exactly as it did before streaming existed.
    pub stream: Option<DeltaSink>,
}

/// A piece of a reply as it streams in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delta {
    /// Visible answer text.
    Text(String),
    /// Bytes that aren't visible text (reasoning, tool arguments): the
    /// model is alive.
    Progress,
    /// Drop what was shown so far: a new call or a new attempt starts.
    /// Drivers never send it; the agent does.
    Reset,
}

/// Where a streaming driver sends its deltas.
#[derive(Clone)]
pub struct DeltaSink(pub std::sync::Arc<dyn Fn(Delta) + Send + Sync>);

impl DeltaSink {
    pub fn new(f: impl Fn(Delta) + Send + Sync + 'static) -> Self {
        DeltaSink(std::sync::Arc::new(f))
    }

    pub fn send(&self, delta: Delta) {
        (self.0)(delta)
    }
}

impl std::fmt::Debug for DeltaSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeltaSink")
    }
}

/// What a provider returns. `message.tool_calls` non-empty means the loop
/// must execute tools and call again; empty means the turn is finished.
#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub message: Message,
    pub usage: Usage,
    /// Why the model stopped, in its API's own word (`end_turn`,
    /// `max_tokens`, `length`, `max_output_tokens`, …); `None` when the
    /// driver isn't told.
    pub stop_reason: Option<String>,
}

impl CompletionResponse {
    /// The reply ran out of output tokens (or of context) mid-way: what it
    /// says is cut off, not finished.
    pub fn cut_off(&self) -> bool {
        matches!(
            self.stop_reason.as_deref(),
            Some("max_tokens" | "length" | "max_output_tokens" | "model_context_window_exceeded")
        )
    }
}

/// The model a call actually ran on, for a provider that picks one per
/// call (M21's routed provider). Ledger rows and prices use it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    pub provider: String,
    pub model: String,
}

impl Served {
    /// `provider/model`, the form the owner types.
    pub fn reference(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

/// A model backend. Implementations must be cheap to clone (Arc internally)
/// and safe to share across sessions.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    async fn complete(
        &self,
        req: CompletionRequest,
    ) -> Result<CompletionResponse, crate::error::CoreError>;

    /// [`Provider::complete`], and which model ran it (`None`: the one the
    /// agent was built with). Only a provider that routes overrides it.
    async fn complete_routed(
        &self,
        req: CompletionRequest,
    ) -> (
        Option<Served>,
        Result<CompletionResponse, crate::error::CoreError>,
    ) {
        (None, self.complete(req).await)
    }

    /// `served` still failed with `error`, a transient failure, after
    /// every retry. A provider with another model to try marks `served`
    /// down, so the next call goes elsewhere, and says what it did; the
    /// agent then tries again from the first attempt. `None`: nothing to
    /// fall back to, the error stands.
    fn fail_over(
        &self,
        _served: Option<&Served>,
        _error: &crate::error::CoreError,
    ) -> Option<FailOver> {
        None
    }

    /// Whether this provider routes between tiers (M25). The loop only
    /// keeps the books for [`Provider::escalate`] when it does, so a
    /// provider that doesn't runs exactly as before.
    fn routes(&self) -> bool {
        false
    }

    /// A new turn starts: a routing provider goes back to its floor.
    fn begin_turn(&self) {}

    /// The loop saw `signal`. A routing provider may move up a tier for
    /// the rest of the turn and says so.
    fn escalate(&self, _signal: &crate::routing::Signal) -> Option<crate::routing::Escalation> {
        None
    }

    /// Whether the model this provider serves can see images (M47). A
    /// driver that can't gets each photo as a one-line note instead of
    /// pixels; a routing provider answers for the tier a turn starts on.
    fn sees_images(&self) -> bool {
        false
    }

    /// Whether the model can open an image file with a tool of its own
    /// engine and see it (Claude Code's `Read`), though a tool's image
    /// reaches it only as a note. Such an engine records what it opened in
    /// [`CallContext::looked`].
    fn opens_images(&self) -> bool {
        false
    }

    /// The tier the last call went to, for its ledger row.
    fn route_tag(&self) -> Option<crate::routing::RouteTag> {
        None
    }

    /// This provider for a session that isn't the owner's (M54): the same
    /// state, with whatever it runs itself held to the stricter sandbox.
    /// `None`: it runs nothing itself, so it serves guests as it is.
    // ponytail: only the claude engine runs tools in its own process;
    // other providers return None.
    fn guest(&self) -> Option<std::sync::Arc<dyn Provider>> {
        None
    }
}

/// What a provider may know about the run calling it (M35). The agent
/// sets it around each model call; a provider that runs tools of its own
/// (the Claude Code engine) asks the run's guard before them and works in
/// the run's workspace. Read it at the start of the call: a task the
/// provider spawns doesn't inherit it.
#[derive(Clone, Default)]
pub struct CallContext {
    /// The run's guard (M19): approval gates, plan mode, the kill switch.
    pub guard: Option<std::sync::Arc<dyn crate::guard::Guard>>,
    /// The run's workspace (a sub-agent's worktree, say).
    pub workspace: Option<std::path::PathBuf>,
    /// M57.1: the run's deferred tools not loaded yet, whole. The request
    /// offers them only by name, through `tool_search`; a provider that
    /// offers tools on its own, with its own search, can offer them all.
    pub deferred_tools: Vec<crate::tool::ToolDefinition>,
    /// M57.2: the reasoning effort the run wants for every call (a
    /// sub-agent's `spawn_agent` effort), instead of its model's own.
    pub effort: Option<String>,
    /// M57.2: at least this effort for this call (a run's first turn, the
    /// turn after a failed check).
    pub effort_floor: Option<String>,
    /// M57.4: how long the provider should keep this call's prompt cache
    /// (`"1h"` on a gateway chat lane). `None`: the provider's default (5
    /// minutes on Anthropic). Only the Anthropic driver reads it.
    pub cache_ttl: Option<String>,
    /// M65 §4: what the run's model has looked at, for `send_file`. An
    /// engine whose own tools open images records those here.
    pub looked: Option<crate::vision::Looked>,
}

impl CallContext {
    /// M57.2: the effort a driver sends, given its model's configured one.
    /// No configured effort sends none: the model may not take the field,
    /// and only an owner who set one has said it does. `"none"` (reasoning
    /// off) stays. Otherwise the run's own level replaces it, and a floor
    /// raises it; a word [`effort_rank`] doesn't know is left as it is.
    pub fn effort_for(&self, configured: Option<&str>) -> Option<String> {
        let configured = configured?;
        if configured == "none" {
            return Some(configured.to_string());
        }
        let mut effort = self
            .effort
            .clone()
            .unwrap_or_else(|| configured.to_string());
        if let Some(floor) = &self.effort_floor {
            if matches!((effort_rank(floor), effort_rank(&effort)), (Some(f), Some(e)) if f > e) {
                effort = floor.clone();
            }
        }
        Some(effort)
    }
}

/// M57.2: where an effort word sits, lowest first, across the Anthropic
/// (`low`…`max`) and OpenAI (`minimal`…`xhigh`) scales; `None` for a word
/// neither uses (or `none`, which turns reasoning off).
pub fn effort_rank(word: &str) -> Option<usize> {
    ["minimal", "low", "medium", "high", "xhigh", "max"]
        .iter()
        .position(|w| *w == word)
}

impl std::fmt::Debug for CallContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallContext")
            .field("guard", &self.guard.is_some())
            .field("workspace", &self.workspace)
            .field("deferred_tools", &self.deferred_tools.len())
            .field("effort", &self.effort)
            .field("effort_floor", &self.effort_floor)
            .field("cache_ttl", &self.cache_ttl)
            .field("looked", &self.looked.is_some())
            .finish()
    }
}

tokio::task_local! {
    static CALL_CONTEXT: CallContext;
}

/// The calling run's context, or an empty one outside a run.
pub fn call_context() -> CallContext {
    CALL_CONTEXT.try_with(Clone::clone).unwrap_or_default()
}

/// Run `fut` with `ctx` as its [`call_context`].
pub async fn with_call_context<F: std::future::Future>(ctx: CallContext, fut: F) -> F::Output {
    CALL_CONTEXT.scope(ctx, fut).await
}

/// What [`Provider::fail_over`] switched to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailOver {
    /// The model that failed, `provider/model`.
    pub from: String,
    /// The model the next call goes to.
    pub to: String,
}
