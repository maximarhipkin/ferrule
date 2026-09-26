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

    /// The tier the last call went to, for its ledger row.
    fn route_tag(&self) -> Option<crate::routing::RouteTag> {
        None
    }
}

/// What [`Provider::fail_over`] switched to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailOver {
    /// The model that failed, `provider/model`.
    pub from: String,
    /// The model the next call goes to.
    pub to: String,
}
