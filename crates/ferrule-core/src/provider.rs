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

/// A model backend. Implementations must be cheap to clone (Arc internally)
/// and safe to share across sessions.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    async fn complete(
        &self,
        req: CompletionRequest,
    ) -> Result<CompletionResponse, crate::error::CoreError>;
}
