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
}

/// What [`Provider::fail_over`] switched to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailOver {
    /// The model that failed, `provider/model`.
    pub from: String,
    /// The model the next call goes to.
    pub to: String,
}
