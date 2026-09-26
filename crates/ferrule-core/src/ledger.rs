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
use std::time::{Duration, SystemTime};

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
    /// Input tokens written to the provider's prompt cache on this call
    /// (Anthropic `cache_creation_input_tokens`; M23). Also included in
    /// `input_tokens`, and priced at the cache-write rate. 0 on providers
    /// that don't report it, and on older rows.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_write_input_tokens: u64,
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
    /// The run tree the call belongs to: the root session's id, shared by
    /// its sub-agents. Stamped by M19's trust sink so day and task spend
    /// can be read back from the ledger. `None` on older rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    /// M25: the routing tier that answered, and on the first call after an
    /// escalation, why it moved. `None` when routing is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<crate::routing::RouteTag>,
    /// M27: how fast the turn felt — time to the first streamed token and
    /// to the first thing the user saw, and the tool batch that ran just
    /// before this call. `None` when nothing was measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<SpeedStats>,
}

/// M27 timings on a ledger row. Every part is optional: a row carries only
/// what was measured on it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SpeedStats {
    /// From sending the request to the first streamed text delta.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_token_ms: Option<u64>,
    /// From the start of the turn to the first text the user saw: the
    /// first streamed message, or the end of the answering call when the
    /// reply wasn't streamed. Set once per turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_visible_ms: Option<u64>,
    /// The tool calls answered between the previous call and this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_batch: Option<ToolBatch>,
}

impl SpeedStats {
    pub fn is_empty(&self) -> bool {
        self.first_token_ms.is_none()
            && self.first_visible_ms.is_none()
            && self.tool_batch.is_none()
    }
}

/// One response's tool calls: how many, how many ran side by side, and
/// wall time against the sum of the calls' own times (the saving is
/// `sum_ms - wall_ms`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolBatch {
    pub calls: usize,
    pub parallel: usize,
    pub wall_ms: u64,
    pub sum_ms: u64,
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

/// `call_kind` of the row the credential proxy's egress policy writes for a
/// refused request (M33). Not a provider call.
pub const EGRESS_DENIED_KIND: &str = "egress_denied";

impl LedgerRecord {
    pub fn is_error(&self) -> bool {
        self.outcome != "ok"
    }

    /// A row that records something other than a provider call (`ferrule
    /// eval`'s verdict, an egress refusal): cost and call counts skip it.
    pub fn is_bookkeeping(&self) -> bool {
        self.call_kind == "eval_result" || self.call_kind == EGRESS_DENIED_KIND
    }
}

/// Where per-call ledger rows go. Implementations must be cheap to clone
/// (wrap in `Arc`, as `Agent` stores it), safe to share across concurrent
/// sessions, and must never let a write failure propagate to the caller —
/// log it and drop the row instead.
pub trait LedgerSink: Send + Sync {
    fn record(&self, record: LedgerRecord);

    /// How much of the loop's work beyond ledger rows this sink wants as
    /// [`TraceEvent`]s (M33, OTel export). Asked once per run; the loop
    /// builds no events for a sink that says `Off`.
    fn trace_level(&self) -> TraceLevel {
        TraceLevel::Off
    }

    /// One turn boundary, tool call or reply (see [`TraceEvent`]). Must not
    /// block.
    fn trace(&self, _event: TraceEvent) {}
}

/// What a [`LedgerSink`] wants besides rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum TraceLevel {
    #[default]
    Off,
    /// Turn boundaries and tool calls: names, timings, outcomes.
    Spans,
    /// The same plus what was said: the goal, replies, tool arguments and
    /// results (the sink scrubs them).
    Content,
}

/// What a ledger row can't say: where turns start and end, and each tool
/// call. The `Option` fields are set only at [`TraceLevel::Content`].
#[derive(Debug, Clone, PartialEq)]
pub enum TraceEvent {
    TurnStarted {
        session_id: String,
        task_shape: String,
        origin: Option<String>,
        at: SystemTime,
        goal: Option<String>,
    },
    ToolCall {
        session_id: String,
        id: String,
        name: String,
        ok: bool,
        started: SystemTime,
        elapsed: Duration,
        arguments: Option<String>,
        result: Option<String>,
    },
    /// The model's reply on the call whose row comes next.
    CallContent { session_id: String, text: String },
    TurnFinished {
        session_id: String,
        at: SystemTime,
        ok: bool,
        incomplete: Option<String>,
    },
}

impl TraceEvent {
    pub fn session_id(&self) -> &str {
        match self {
            Self::TurnStarted { session_id, .. }
            | Self::ToolCall { session_id, .. }
            | Self::CallContent { session_id, .. }
            | Self::TurnFinished { session_id, .. } => session_id,
        }
    }
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

fn is_zero(n: &u64) -> bool {
    *n == 0
}
