use serde::{Deserialize, Serialize};

/// Typed lifecycle/stream events — observability is structural, not bolted on.
/// Mirrors the run lifecycle used by production agent OSes:
/// accepted → context ready → streaming → tooling → persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    RunStarted {
        session_id: String,
        goal: String,
    },
    ContextReady {
        est_tokens: usize,
        threshold_tokens: usize,
    },
    AssistantText {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolCallStarted {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolCallFinished {
        id: String,
        name: String,
        ok: bool,
        output_chars: usize,
    },
    /// Context was compacted: how many messages were folded into a summary.
    Compacted {
        folded_messages: usize,
        est_tokens_before: usize,
        est_tokens_after: usize,
    },
    /// Old large tool results were shortened to a preview and a reference
    /// `search_history` can fetch back (the first, free stage of compaction).
    ToolResultsShortened {
        shortened: usize,
        est_tokens_before: usize,
        est_tokens_after: usize,
    },
    /// Context was truncated (the naive harness `ferrule eval` compares
    /// against): the oldest messages were dropped, nothing summarized.
    Truncated {
        dropped_messages: usize,
        est_tokens_before: usize,
        est_tokens_after: usize,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
        /// Input tokens written to the prompt cache (M23; 0 when the
        /// provider doesn't report it).
        cache_write_input_tokens: u64,
    },
    /// A provider call failed transiently and will be tried again.
    ProviderRetry {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error: String,
    },
    /// A model kept failing transiently after its retries; the turn goes
    /// on on the next model in the owner's fallback list (M21).
    ModelFallback {
        from: String,
        to: String,
        error: String,
    },
    /// Routing moved this turn up a tier (M25): `from` and `to` are tier
    /// names, `reason` is what the ledger records.
    Escalated {
        from: String,
        to: String,
        reason: String,
    },
    /// The run was going in circles; the model has been told to change course.
    Stuck {
        note: String,
    },
    VerifyStarted {
        check: String,
    },
    VerifyFinished {
        check: String,
        ok: bool,
    },
    /// A lifecycle hook ran (not the built-in check, which has the verify
    /// events). `error` is a non-blocking failure: the owner's to read.
    HookFinished {
        event: String,
        source: String,
        command: String,
        blocked: bool,
        exit_code: Option<i32>,
        duration_ms: u64,
        error: Option<String>,
    },
    /// M28: a skill loaded because the person's message named one of its
    /// triggers.
    SkillTriggered {
        name: String,
        matched: String,
    },
    RunFinished {
        answer_chars: usize,
        iterations: usize,
    },
    /// The run stopped before the model was done; its answer is a status.
    RunIncomplete {
        reason: String,
        iterations: usize,
    },
    Error {
        message: String,
    },
}
