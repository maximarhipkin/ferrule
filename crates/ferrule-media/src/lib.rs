//! ferrule-media: image and video generation (M51, `docs/m51-media.md`).
//!
//! One trait, [`MediaBackend`]; an OpenRouter implementation
//! ([`openrouter::OpenRouterMedia`]) reached through ferrule's credential
//! proxy (the caller hands it the proxy's placeholder and an egress client,
//! exactly like `ferrule-embed`); a deterministic [`fake::FakeMedia`] for
//! tests; and [`jobs::JobStore`], the SQLite table an async video job lives
//! in between "submitted" and "delivered".

pub mod fake;
pub mod jobs;
pub mod openrouter;

use std::path::Path;

pub use jobs::{JobKind, JobStore, MediaJob};

/// A generated image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageOut {
    pub bytes: Vec<u8>,
    /// `png`, `jpeg`, … — from the answer's own mime type, not the name.
    pub format: String,
}

#[derive(Debug, Clone, Default)]
pub struct ImageReq {
    pub prompt: String,
    /// `None` = the config's default image model.
    pub model: Option<String>,
    /// OpenAI-style size, e.g. `1024x1024`; `None` = the model's default.
    pub size: Option<String>,
    /// Workspace paths of reference images: with them, this is an edit.
    pub references: Vec<std::path::PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct VideoReq {
    pub prompt: String,
    pub model: Option<String>,
    pub seconds: Option<u32>,
    pub aspect: Option<String>,
    /// First frame: image-to-video.
    pub first_frame: Option<std::path::PathBuf>,
}

/// A submitted video job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoJob {
    pub id: String,
}

/// Where a video job stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoState {
    Pending,
    /// The result's download URL.
    Done {
        url: String,
    },
    Failed {
        reason: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("the media endpoint at {host} refused the key (HTTP {status}); check {key_env}")]
    Unauthorized {
        host: String,
        status: u16,
        key_env: String,
    },
    #[error("the media endpoint at {host} is rate limiting (HTTP 429){}", retry_note(.retry_after))]
    RateLimited {
        host: String,
        retry_after: Option<u64>,
    },
    #[error("the media endpoint at {host} answered HTTP {status}: {body}")]
    Http {
        host: String,
        status: u16,
        body: String,
    },
    #[error("the media endpoint at {host} can't be reached: {message}")]
    Transport { host: String, message: String },
    #[error("bad media answer: {0}")]
    BadAnswer(String),
    #[error("{0}")]
    Store(String),
}

fn retry_note(retry_after: &Option<u64>) -> String {
    match retry_after {
        Some(s) => format!("; try again in {s}s"),
        None => String::new(),
    }
}

#[async_trait::async_trait]
pub trait MediaBackend: Send + Sync {
    /// One image, sync (they take seconds; the tool's timeout bounds it).
    async fn generate_image(&self, req: ImageReq) -> Result<ImageOut, MediaError>;
    /// Submit a video job; the answer comes back immediately with an id.
    async fn submit_video(&self, req: VideoReq) -> Result<VideoJob, MediaError>;
    async fn poll_video(&self, job: &str) -> Result<VideoState, MediaError>;
    /// Download a finished job's file to `dest` (confined by the caller).
    async fn download(&self, url: &str, dest: &Path) -> Result<(), MediaError>;
}
