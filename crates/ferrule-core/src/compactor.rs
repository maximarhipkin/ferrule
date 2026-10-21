//! The compaction strategy seam (M42 part 7, after Pi's pluggable
//! compaction). How a folded head becomes a summary is a replaceable
//! choice, not a constant of the loop: the default is the built-in
//! checklist pipeline on the agent's own model; a strategy can use
//! another prompt or another model. The transcript's fold record (M42
//! part 4) doesn't change either way, and neither does the call: the
//! agent sends a strategy's request through its own call path (M58.1).

use crate::message::Message;
use crate::profile::COMPACTION_TEMPLATE;
use crate::provider::{CompletionRequest, Provider};
use std::sync::Arc;

/// How the folded head of a session becomes its summary.
pub trait Compactor: Send + Sync {
    /// The request whose answer replaces the folded messages. The agent
    /// renders that text itself: no system prompt, each tool call by
    /// name with its arguments, each result under its tool's name, and
    /// the whole piece already cut to the summary model's budget. The
    /// agent sends the request like any other call
    /// (retried, a ledger row, charged to the budget, under the kill
    /// switch), rebuilds its context around the answer and logs it as the
    /// fold record.
    fn request(&self, transcript_text: &str) -> CompletionRequest;

    /// The model the request goes to; `None`: the agent's compaction
    /// model (`[agent] compaction_model`), else its own.
    fn provider(&self) -> Option<&dyn Provider> {
        None
    }
}

/// The built-in pipeline as a strategy: the checklist template on
/// whatever provider it's given. Routing and the fallback list belong to
/// that provider. `[agent] compaction_model` doesn't come through here:
/// the agent runs its own pipeline on that model
/// (`Agent::with_compaction_provider`).
pub struct TemplateCompactor {
    provider: Arc<dyn Provider>,
    max_output_tokens: Option<u32>,
}

impl TemplateCompactor {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self {
            provider,
            // Unset, as the main calls send it: a reasoning model refuses
            // `max_tokens`.
            max_output_tokens: None,
        }
    }

    pub fn with_max_output_tokens(mut self, tokens: Option<u32>) -> Self {
        self.max_output_tokens = tokens;
        self
    }
}

impl Compactor for TemplateCompactor {
    fn request(&self, transcript_text: &str) -> CompletionRequest {
        CompletionRequest {
            messages: vec![Message::user(format!(
                "{COMPACTION_TEMPLATE}{transcript_text}"
            ))],
            tools: vec![],
            max_output_tokens: self.max_output_tokens,
            // None, not 0: reasoning models refuse a temperature.
            temperature: None,
            stream: None,
        }
    }

    fn provider(&self) -> Option<&dyn Provider> {
        Some(self.provider.as_ref())
    }
}
