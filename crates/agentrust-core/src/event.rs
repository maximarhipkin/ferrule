use serde::{Deserialize, Serialize};

/// Typed lifecycle/stream events — observability is structural, not bolted on.
/// Mirrors the run lifecycle used by production agent OSes:
/// accepted → context ready → streaming → tooling → persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    RunStarted { session_id: String, goal: String },
    ContextReady { est_tokens: usize, threshold_tokens: usize },
    AssistantText { text: String },
    Reasoning { text: String },
    ToolCallStarted { id: String, name: String, arguments: serde_json::Value },
    ToolCallFinished { id: String, name: String, ok: bool, output_chars: usize },
    /// Context was compacted: how many messages were folded into a summary.
    Compacted { folded_messages: usize, est_tokens_before: usize, est_tokens_after: usize },
    Usage { input_tokens: u64, output_tokens: u64, cached_input_tokens: u64 },
    RunFinished { answer_chars: usize, iterations: usize },
    Error { message: String },
}
