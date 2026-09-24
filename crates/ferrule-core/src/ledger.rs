//! Phase 0 — the per-call ledger.
//!
//! One [`LedgerRecord`] per provider `complete()` call. This module only
//! defines the record shape and the sink trait — it is deliberately
//! IO-free, so `ferrule-core` stays usable as a library with no filesystem
//! side effects. A concrete sink (file-backed JSONL today) lives in
//! `ferrule-cli`; `Agent` holds an optional `Arc<dyn LedgerSink>` and calls
//! it around every `Provider::complete()` call, so a failed call still
//! produces a row (outcome = "error") — that failure visibility is the
//! whole point, see `docs/research-routing-and-local-models.md` Phase 0.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// One row per provider `complete()` call.
///
/// `cost_usd` is always `None` when `Agent` builds the record — core has no
/// notion of pricing. A sink that knows per-provider prices (e.g. the
/// file-backed CLI sink, from `[providers.*].price_*_per_mtok` config) may
/// fill it in before persisting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerRecord {
    /// RFC 3339, UTC.
    pub timestamp: String,
    pub session_id: String,
    /// Cheap static tag set by the entry point: `"run"` | `"chat"` |
    /// `"gateway"` | `"scheduler"`.
    pub task_shape: String,
    /// Origin detail, where cheap: channel name (gateway sessions) or task
    /// id (scheduler sessions). `None` for `run`/`chat`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    pub provider: String,
    pub model: String,
    /// 0-based index of this provider call within the turn (the
    /// `Agent::run` invocation it happened in) — the loop iteration for the
    /// main ReAct call, or the enclosing iteration's index for a
    /// compaction-summary call.
    pub iteration: usize,
    /// `"turn"` for the main ReAct call, `"compaction"` for the summary call
    /// `maybe_compact` makes, `"status"` for the closing status answer when a
    /// run stops early — all can share one `iteration` index.
    #[serde(default = "default_call_kind")]
    pub call_kind: String,
    /// Includes `cached_input_tokens` (OpenAI `prompt_tokens` convention).
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    /// Number of tool calls in the response (0 for a final answer or a
    /// compaction summary).
    pub tool_calls: usize,
    pub latency_ms: u64,
    /// `"ok"` | `"error"` | `"retried"` (a transient error that was tried
    /// again; every attempt gets its own row).
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Set on rows `ferrule eval` writes: which suite run, task and variant
    /// the call belongs to. `None` everywhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval: Option<EvalTag>,
}

/// Where an eval row belongs. The per-call rows carry it with `result`
/// empty; the one `call_kind = "eval_result"` row per task run carries the
/// verdict in `result` (its shape is `ferrule-eval`'s, kept as JSON here so
/// core doesn't depend on it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalTag {
    pub run_id: String,
    pub suite: String,
    /// `"capability"` | `"regression"`.
    pub kind: String,
    pub task: String,
    /// `"engineered"` | `"naive"`.
    pub variant: String,
    #[serde(default)]
    pub repeat: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
}

fn default_call_kind() -> String {
    "turn".into()
}

impl LedgerRecord {
    pub fn is_error(&self) -> bool {
        self.outcome != "ok"
    }
}

/// Where per-call ledger rows go. Implementations must be cheap to clone
/// (wrap in `Arc`, as `Agent` stores it), safe to share across concurrent
/// sessions, and must never let a write failure propagate to the caller —
/// log it and drop the row instead.
pub trait LedgerSink: Send + Sync {
    fn record(&self, record: LedgerRecord);
}

/// Static per-session context `Agent` carries so every ledger row it emits
/// knows who's asking. `model` is here (rather than on `Provider`) because
/// the `Provider` trait exposes only `name()`; the caller already resolved
/// the model from config when it built the provider, so it hands it in here
/// instead of the trait signature growing a method for one observability
/// feature.
#[derive(Clone)]
pub struct LedgerContext {
    pub sink: Arc<dyn LedgerSink>,
    pub task_shape: String,
    pub origin: Option<String>,
    pub model: String,
}
