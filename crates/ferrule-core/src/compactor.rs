//! The compaction strategy seam (M42 part 7, after Pi's pluggable
//! compaction). How a folded head becomes a summary is a replaceable
//! choice, not a constant of the loop: the default is the built-in
//! checklist pipeline on the agent's own model; a strategy can use
//! another prompt, another model, or fold by topic instead. The
//! transcript's fold record (M42 part 4) doesn't change either way.

use crate::message::Message;
use crate::profile::COMPACTION_TEMPLATE;
use crate::provider::{CompletionRequest, Provider};
use crate::CoreError;
use std::sync::Arc;

/// How the folded head of a session becomes its summary.
#[async_trait::async_trait]
pub trait Compactor: Send + Sync {
    /// The summary replacing the folded messages, rendered "role: body"
    /// by the agent. The agent rebuilds its context around the answer and
    /// logs it as the fold record.
    async fn summarize(&self, transcript_text: &str) -> Result<String, CoreError>;
}

/// The built-in pipeline as a strategy: the checklist template on
/// whatever provider it's given — the agent's own model by default, a
/// cheaper one when `[agent] compaction_model` names it. One
/// deterministic call; retries and routing belong to the provider it's
/// handed (the CLI hands it a routed one).
pub struct TemplateCompactor {
    provider: Arc<dyn Provider>,
    max_output_tokens: Option<u32>,
}

impl TemplateCompactor {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self {
            provider,
            max_output_tokens: Some(4096),
        }
    }

    pub fn with_max_output_tokens(mut self, tokens: Option<u32>) -> Self {
        self.max_output_tokens = tokens;
        self
    }
}

#[async_trait::async_trait]
impl Compactor for TemplateCompactor {
    async fn summarize(&self, transcript_text: &str) -> Result<String, CoreError> {
        let req = CompletionRequest {
            messages: vec![Message::user(format!(
                "{COMPACTION_TEMPLATE}{transcript_text}"
            ))],
            tools: vec![],
            max_output_tokens: self.max_output_tokens,
            temperature: Some(0.0),
            stream: None,
        };
        Ok(self
            .provider
            .complete(req)
            .await?
            .message
            .content
            .unwrap_or_default())
    }
}
