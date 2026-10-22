//! ferrule-media: image, video and speech generation (M51,
//! `docs/m51-media.md`).
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
#[derive(Debug, Clone, PartialEq)]
pub struct ImageOut {
    pub bytes: Vec<u8>,
    /// `png`, `jpeg`, … — from the answer's own mime type, not the name.
    pub format: String,
    /// What the provider says it charged (OpenRouter's `usage.cost`), in
    /// dollars; `None` when the answer doesn't say.
    pub cost_usd: Option<f64>,
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
    /// M65 §3: workspace images of a person or thing to keep, sent as
    /// `input_references` (subject and identity), beside the first frame.
    pub references: Vec<std::path::PathBuf>,
    /// M65 §3: the resolution asked for (`720p`), when the price estimate
    /// depends on it (a model billed per video token).
    pub resolution: Option<String>,
}

/// M51.4: text to speak, for a voice reply.
#[derive(Debug, Clone, Default)]
pub struct SpeechReq {
    pub text: String,
    /// `None` = the config's default speech model.
    pub model: Option<String>,
    /// A voice the model knows; `None` = the model's own default.
    pub voice: Option<String>,
}

/// M65 §0: images for a vision model to look at, and what to say about
/// them: the vision helper for a chat model that can't see.
#[derive(Debug, Clone, Default)]
pub struct LookReq {
    pub model: String,
    pub task: String,
    /// Workspace images, in order: the one looked at, then any reference.
    pub images: Vec<std::path::PathBuf>,
}

/// What the vision model said, and what the provider says it charged.
#[derive(Debug, Clone, PartialEq)]
pub struct LookOut {
    pub text: String,
    pub cost_usd: Option<f64>,
}

/// Spoken audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioOut {
    pub bytes: Vec<u8>,
    /// `mp3`: the one compressed format the speech API returns.
    pub format: String,
}

/// A submitted video job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoJob {
    pub id: String,
}

/// Where a video job stands.
#[derive(Debug, Clone, PartialEq)]
pub enum VideoState {
    Pending,
    /// The result's download URL, and what the provider says it charged
    /// (`usage.cost`, dollars) when the answer says.
    Done {
        url: String,
        cost_usd: Option<f64>,
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

/// A video model's list price, for an estimate before a submit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoPrice {
    /// Dollars per second of video, the dearest of the model's per-second
    /// prices that name no resolution (so with audio, at its default
    /// size); a model that only has per-resolution prices, the dearest of
    /// those.
    pub usd_per_second: f64,
    /// The longest clip the model makes, for a request that names no
    /// length.
    pub longest_seconds: Option<u32>,
    /// M65 §3: the resolution the price is for, which the request must
    /// then ask for: set only for a model billed per video token.
    pub resolution: Option<&'static str>,
}

/// M65 §3: what a model billed per video token is asked for and priced
/// at: a 16:9 frame at 24 frames a second, `width × height × fps / 1024`
/// tokens a second (ByteDance's Seedance formula), at 720p when the model
/// lists it, else 480p.
const TOKEN_RESOLUTIONS: [(&str, f64); 2] = [("720p", 1280.0 * 720.0), ("480p", 854.0 * 480.0)];
const TOKEN_FPS: f64 = 24.0;

impl VideoPrice {
    /// From one entry of OpenRouter's `/videos/models` list: its
    /// `pricing_skus` (string values; a `cents_per_…` key is in cents) and
    /// `supported_durations`. `None` when no price is per second (a model
    /// billed per token, say).
    pub fn of(entry: &serde_json::Value) -> Option<Self> {
        let skus = entry["pricing_skus"].as_object()?;
        let mut plain: Option<f64> = None;
        let mut any: Option<f64> = None;
        for (key, value) in skus {
            let k = key.to_ascii_lowercase();
            if !k.contains("second") {
                continue;
            }
            let Some(mut usd) = value
                .as_str()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .or_else(|| value.as_f64())
            else {
                continue;
            };
            if k.starts_with("cents") {
                usd /= 100.0;
            }
            if !usd.is_finite() || usd < 0.0 {
                continue;
            }
            let sized = ["480p", "720p", "1080p", "4k", "1440p", "2160p"]
                .iter()
                .any(|r| k.contains(r));
            any = Some(any.map_or(usd, |a: f64| a.max(usd)));
            if !sized {
                plain = Some(plain.map_or(usd, |a: f64| a.max(usd)));
            }
        }
        let longest_seconds = entry["supported_durations"]
            .as_array()
            .and_then(|d| d.iter().filter_map(|s| s.as_u64()).max())
            .map(|s| s as u32);
        if let Some(usd_per_second) = plain.or(any) {
            return Some(Self {
                usd_per_second,
                longest_seconds,
                resolution: None,
            });
        }
        Self::per_token(entry, skus, longest_seconds)
    }

    /// M65 §3: a model billed per video token (no per-second price): the
    /// dearest token price that names no resolution, times the tokens a
    /// second of [`TOKEN_RESOLUTIONS`]' first listed size.
    fn per_token(
        entry: &serde_json::Value,
        skus: &serde_json::Map<String, serde_json::Value>,
        longest_seconds: Option<u32>,
    ) -> Option<Self> {
        let sized = ["480p", "720p", "1080p", "4k", "1440p", "2160p"];
        let per_token = skus
            .iter()
            .filter(|(k, _)| {
                let k = k.to_ascii_lowercase();
                k.starts_with("video_tokens") && !sized.iter().any(|r| k.contains(r))
            })
            .filter_map(|(_, v)| {
                v.as_str()
                    .and_then(|v| v.trim().parse::<f64>().ok())
                    .or_else(|| v.as_f64())
            })
            .filter(|p| p.is_finite() && *p >= 0.0)
            .fold(None, |a: Option<f64>, p| Some(a.map_or(p, |a| a.max(p))))?;
        let listed: Vec<&str> = entry["supported_resolutions"]
            .as_array()
            .map(|l| l.iter().filter_map(|r| r.as_str()).collect())
            .unwrap_or_default();
        let (resolution, pixels) = TOKEN_RESOLUTIONS
            .iter()
            .find(|(r, _)| listed.iter().any(|l| l.eq_ignore_ascii_case(r)))?;
        Some(Self {
            usd_per_second: per_token * pixels * TOKEN_FPS / 1024.0,
            longest_seconds,
            resolution: Some(resolution),
        })
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
    /// M51.4: text to speech, sync (seconds, like an image).
    async fn speak(&self, req: SpeechReq) -> Result<AudioOut, MediaError>;
    /// M65 §0: a vision model looks at images and answers in words.
    async fn look(&self, _req: LookReq) -> Result<LookOut, MediaError> {
        Err(MediaError::BadAnswer(
            "this media backend has no vision model".into(),
        ))
    }
    /// A video model's list price, from a cached price list. `None` when
    /// the backend has no list or the model isn't priced per second.
    async fn video_price(&self, _model: &str) -> Result<Option<VideoPrice>, MediaError> {
        Ok(None)
    }
    /// A speech model's list price per input unit (a character or a
    /// token), only for a model priced on its input alone; `None`
    /// otherwise.
    async fn speech_price(&self, _model: &str) -> Result<Option<f64>, MediaError> {
        Ok(None)
    }
}
