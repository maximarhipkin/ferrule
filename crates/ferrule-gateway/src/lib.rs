//! ferrule-gateway: the long-running daemon process.
//!
//! This crate owns every external I/O boundary — channel adapters, session
//! routing, and (later) the task scheduler — so that `ferrule-core` and
//! `ferrule-cli` stay usable as a library and a plain one-shot CLI without
//! ever needing a daemon running. Mirrors OpenClaw's "Gateway as single
//! source of truth" pattern while keeping NanoClaw's minimalism: a queue and
//! a router, not a framework.

pub mod channel;
pub mod channels;
pub mod error;
pub mod gateway;
pub mod health;
pub mod menu;
pub mod message;
pub mod router;
pub mod scheduler;
pub mod sdnotify;
pub mod session;
pub mod stream;
pub mod tools;
pub mod transcribe;
pub mod typing;

pub use channel::{
    buttons_as_text, send_with_buttons, Button, ButtonAction, Channel, ChannelCapabilities,
};
pub use channels::{
    DiscordChannel, EmailChannel, HttpChannel, LocalChannel, MatrixChannel, MattermostChannel,
    SignalChannel, SlackChannel, TelegramChannel, WhatsAppChannel,
};
pub use error::GatewayError;
pub use gateway::{ChannelRestarts, Gateway, Interceptor, ACK_EMOJI};
pub use health::{
    Health, HealthSettings, Heartbeat, Leftover, Notice, RecentLog, Redactor, RunningMarker,
    StallHook,
};
pub use message::{Attachment, InboundMessage, OutboundMessage};
pub use router::{AgentFactory, Drained, LaneSnapshot, Reply, Reset, Router, NEW_HINT};
pub use scheduler::{
    ensure_builtin, BuiltinJob, BuiltinSpec, Ensured, Hold, JobReport, BUILTIN_CHANNEL,
};
pub use scheduler::{
    initial_next_run_at, NewTask, Run, RunOutcome, RunStatus, Scheduler, SchedulerError, Task,
    TaskKind, TaskStore, SCHEDULER_PSEUDO_CHANNEL,
};
pub use stream::StreamPacing;
pub use transcribe::{Transcriber, Transcription};
pub use typing::Typing;
