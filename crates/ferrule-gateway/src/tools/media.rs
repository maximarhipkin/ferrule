//! M51: `generate_image` and `generate_video`, chat-bound like
//! [`SendFileTool`], plus the poller that delivers finished videos
//! (`docs/m51-media.md`). Images are a sync call: generated, saved under
//! `media/` in the workspace, sent to the session's chat. Videos are an
//! async job: submitted, one row in the `media_jobs` table, an immediate
//! answer; the [`MediaPoller`] built-in task polls, downloads and delivers.
//! M51.4: `speak`, a sync call like an image, sent as a voice note.
//!
//! Spend honesty: the daily cap (`[media] max_per_day`) is checked before
//! every call from the store — fail-closed — and every call and delivery
//! writes a ledger row with `call_kind = "media"`. The tools say plainly
//! that video costs dollars, not cents.

use crate::channels::files;
use crate::message::{Attachment, OutboundMessage};
use crate::router::Router;
use crate::scheduler::{BuiltinJob, BuiltinSpec, JobReport, Task};
use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_core::{LedgerRecord, LedgerSink};
use ferrule_media::jobs::now_unix;
use ferrule_media::{ImageReq, JobStore, MediaBackend, SpeechReq, VideoReq, VideoState};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};

/// The media backend, the jobs table, and the way back to the chat — one
/// handle shared by the tools and the poller; the router is bound once it
/// exists (the agent factory is built before it).
pub struct MediaShared {
    pub backend: Arc<dyn MediaBackend>,
    pub store: Arc<JobStore>,
    pub image_model: String,
    pub video_model: String,
    /// 0 = no cap.
    pub max_per_day: u64,
    pub ledger: Option<Arc<dyn LedgerSink>>,
    router: OnceLock<Weak<Router>>,
    /// M51.4: `speak`'s default model and voice; unset, no `speak`.
    speech: OnceLock<(String, Option<String>)>,
}

impl MediaShared {
    pub fn new(
        backend: Arc<dyn MediaBackend>,
        store: Arc<JobStore>,
        image_model: String,
        video_model: String,
        max_per_day: u64,
        ledger: Option<Arc<dyn LedgerSink>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            backend,
            store,
            image_model,
            video_model,
            max_per_day,
            ledger,
            router: OnceLock::new(),
            speech: OnceLock::new(),
        })
    }

    pub fn bind(&self, router: &Arc<Router>) {
        let _ = self.router.set(Arc::downgrade(router));
    }

    /// M51.4: turn `speak` on with its default model (and voice; `None` is
    /// the model's own). The first call wins.
    pub fn set_speech(&self, model: String, voice: Option<String>) {
        let _ = self.speech.set((model, voice));
    }

    /// Whether `speak` is on.
    pub fn speaks(&self) -> bool {
        self.speech.get().is_some()
    }

    fn router(&self) -> Option<Arc<Router>> {
        self.router.get().and_then(Weak::upgrade)
    }

    /// The daily cap, fail-closed: a store that can't be counted refuses.
    fn check_cap(&self) -> Result<(), String> {
        if self.max_per_day == 0 {
            return Ok(());
        }
        let midnight = chrono::Utc::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .map(|t| t.and_utc().timestamp())
            .unwrap_or(0);
        let used = self
            .store
            .count_since(midnight)
            .map_err(|e| format!("the daily media cap can't be checked: {e}"))?;
        if used >= self.max_per_day {
            return Err(format!(
                "the daily media cap of {} is reached ([media] max_per_day); it resets at midnight UTC",
                self.max_per_day
            ));
        }
        Ok(())
    }

    /// One ledger row per call or delivery, `call_kind = "media"`.
    fn ledger(&self, session_id: &str, model: &str, latency_ms: u64, err: Option<&str>) {
        if let Some(sink) = &self.ledger {
            sink.record(LedgerRecord {
                timestamp: chrono::Utc::now().to_rfc3339(),
                session_id: session_id.into(),
                task_shape: "media".into(),
                origin: None,
                provider: "media".into(),
                model: model.into(),
                iteration: 0,
                call_kind: "media".into(),
                input_tokens: 0,
                cached_input_tokens: 0,
                cache_write_input_tokens: 0,
                cache_write_1h_input_tokens: 0,
                output_tokens: 0,
                tool_calls: 0,
                latency_ms,
                outcome: if err.is_some() { "error" } else { "ok" }.into(),
                error_kind: err.map(|_| "media".into()),
                error_message: err.map(str::to_string),
                cost_usd: None,
                eval: None,
                tree: None,
                route: None,
                speed: None,
                plan: None,
                notional_usd: None,
            });
        }
    }
}

fn failed(tool: &str, message: impl Into<String>) -> CoreError {
    CoreError::ToolFailed {
        tool: tool.into(),
        message: message.into(),
    }
}

/// `media/<unix>-<n>.<ext>` under the workspace, never overwriting.
fn fresh_path(workspace: &std::path::Path, ext: &str) -> PathBuf {
    let dir = workspace.join("media");
    let _ = std::fs::create_dir_all(&dir);
    let now = now_unix();
    for n in 0..100u32 {
        let p = dir.join(format!("{now}-{n}.{ext}"));
        if !p.exists() {
            return p;
        }
    }
    dir.join(format!("{now}-{}.{}", std::process::id(), ext))
}

pub struct GenerateImageTool {
    shared: Arc<MediaShared>,
    session_id: String,
    hidden: Vec<PathBuf>,
}

impl GenerateImageTool {
    pub fn new(
        shared: Arc<MediaShared>,
        session_id: impl Into<String>,
        hidden: Vec<PathBuf>,
    ) -> Self {
        Self {
            shared,
            session_id: session_id.into(),
            hidden,
        }
    }
}

#[async_trait::async_trait]
impl Tool for GenerateImageTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "generate_image".into(),
            description: "Create an image with an image model and save it in the workspace under media/ (and send it to this chat when there is one). With `reference` paths it edits those images instead. Each call spends money — say so before a big batch.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "What to draw" },
                    "model": { "type": "string", "description": "An image model id; the configured default when omitted" },
                    "size": { "type": "string", "description": "e.g. 1024x1024; the model's default when omitted" },
                    "reference": { "type": "array", "items": { "type": "string" }, "description": "Workspace image paths to edit or take as reference" }
                },
                "required": ["prompt"]
            }),
        }
    }

    fn changes_files(&self) -> bool {
        true
    }

    /// M57.1: rare, and loaded when wanted.
    fn deferrable(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let prompt = args["prompt"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        if prompt.is_empty() {
            return Err(failed("generate_image", "say what to draw"));
        }
        let mut references = Vec::new();
        if let Some(list) = args["reference"].as_array() {
            for r in list.iter().filter_map(|r| r.as_str()) {
                references.push(ferrule_tools::fs_tools::resolve(
                    &ctx.workspace,
                    &self.hidden,
                    r,
                )?);
            }
        }
        self.shared
            .check_cap()
            .map_err(|e| failed("generate_image", e.to_string()))?;
        let model = args["model"]
            .as_str()
            .unwrap_or(&self.shared.image_model)
            .to_string();
        let started = std::time::Instant::now();
        let out = self
            .shared
            .backend
            .generate_image(ImageReq {
                prompt: prompt.clone(),
                model: Some(model.clone()),
                size: args["size"].as_str().map(str::to_string),
                references,
            })
            .await;
        let out = match out {
            Ok(o) => o,
            Err(e) => {
                self.shared.ledger(
                    &self.session_id,
                    &model,
                    started.elapsed().as_millis() as u64,
                    Some(&e.to_string()),
                );
                return Err(failed("generate_image", e.to_string()));
            }
        };
        let path = fresh_path(&ctx.workspace, &out.format);
        std::fs::write(&path, &out.bytes)
            .map_err(|e| failed("generate_image", format!("{}: {e}", path.display())))?;
        let rel = path
            .strip_prefix(&ctx.workspace)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let id = rel.clone();
        let mut job = ferrule_media::JobStore::image(&id, &model, &prompt, &rel);
        let chat = self
            .shared
            .router()
            .and_then(|r| r.chat_of(&self.session_id));
        if let Some((channel, chat_id)) = &chat {
            job.channel = Some(channel.clone());
            job.chat_id = Some(chat_id.clone());
        }
        self.shared
            .store
            .record(&job)
            .map_err(|e| failed("generate_image", e.to_string()))?;
        self.shared.ledger(
            &self.session_id,
            &model,
            started.elapsed().as_millis() as u64,
            None,
        );
        if let Some((channel_name, chat_id)) = chat {
            if let Err(e) = deliver(&self.shared, &channel_name, &chat_id, &path, &prompt).await {
                return Ok(ToolOutput::ok(format!(
                    "Saved {rel}, but sending it to the chat failed: {e}. The file is there."
                )));
            }
            return Ok(ToolOutput::ok(format!(
                "Saved {rel} and sent it to this chat."
            )));
        }
        Ok(ToolOutput::ok(format!(
            "Saved {rel}. This session has no chat to send it to; send_file can deliver it where needed."
        )))
    }
}

pub struct GenerateVideoTool {
    shared: Arc<MediaShared>,
    session_id: String,
    hidden: Vec<PathBuf>,
}

impl GenerateVideoTool {
    pub fn new(
        shared: Arc<MediaShared>,
        session_id: impl Into<String>,
        hidden: Vec<PathBuf>,
    ) -> Self {
        Self {
            shared,
            session_id: session_id.into(),
            hidden,
        }
    }
}

#[async_trait::async_trait]
impl Tool for GenerateVideoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "generate_video".into(),
            description: "Start a video generation job (text-to-video, or image-to-video with `first_frame`). Answers right away with a job id — the render takes minutes and the file is sent to this chat when it's done. A video costs dollars, not cents: say the price class before you start one.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "What happens in the clip" },
                    "model": { "type": "string", "description": "A video model id; the configured default when omitted" },
                    "seconds": { "type": "integer", "description": "Length in seconds (the model's range)" },
                    "aspect": { "type": "string", "description": "e.g. 16:9, 9:16, 1:1" },
                    "first_frame": { "type": "string", "description": "A workspace image path to start from" }
                },
                "required": ["prompt"]
            }),
        }
    }

    fn changes_files(&self) -> bool {
        false
    }

    /// M57.1: rare, and loaded when wanted.
    fn deferrable(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let prompt = args["prompt"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        if prompt.is_empty() {
            return Err(failed("generate_video", "say what happens in the clip"));
        }
        let first_frame = match args["first_frame"].as_str() {
            Some(f) => Some(ferrule_tools::fs_tools::resolve(
                &ctx.workspace,
                &self.hidden,
                f,
            )?),
            None => None,
        };
        self.shared
            .check_cap()
            .map_err(|e| failed("generate_video", e.to_string()))?;
        let model = args["model"]
            .as_str()
            .unwrap_or(&self.shared.video_model)
            .to_string();
        let started = std::time::Instant::now();
        let submitted = self
            .shared
            .backend
            .submit_video(VideoReq {
                prompt: prompt.clone(),
                model: Some(model.clone()),
                seconds: args["seconds"].as_u64().map(|n| n as u32),
                aspect: args["aspect"].as_str().map(str::to_string),
                first_frame,
            })
            .await;
        let job = match submitted {
            Ok(j) => j,
            Err(e) => {
                self.shared.ledger(
                    &self.session_id,
                    &model,
                    started.elapsed().as_millis() as u64,
                    Some(&e.to_string()),
                );
                return Err(failed("generate_video", e.to_string()));
            }
        };
        let out = fresh_path(&ctx.workspace, "mp4");
        let rel = out
            .strip_prefix(&ctx.workspace)
            .unwrap_or(&out)
            .to_string_lossy()
            .into_owned();
        let mut row =
            ferrule_media::JobStore::video(&job.id, &model, &prompt, &out.to_string_lossy());
        let chat = self
            .shared
            .router()
            .and_then(|r| r.chat_of(&self.session_id));
        let has_chat = chat.is_some();
        if let Some((channel, chat_id)) = chat {
            row.channel = Some(channel);
            row.chat_id = Some(chat_id);
        }
        self.shared
            .store
            .record(&row)
            .map_err(|e| failed("generate_video", e.to_string()))?;
        self.shared.ledger(
            &self.session_id,
            &model,
            started.elapsed().as_millis() as u64,
            None,
        );
        if !has_chat {
            // M51.3: a sub-agent (or a run with no chat) gets the file,
            // not a delivery.
            return Ok(ToolOutput::ok(format!(
                "Making it — job {} on {model}. A render takes minutes; the gateway saves it as {rel} when it's done. This session has no chat to send it to: report that path, and send_file can deliver it once it's there.",
                job.id
            )));
        }
        Ok(ToolOutput::ok(format!(
            "Making it — job {} on {model}. A render takes minutes; I'll send {rel} here when it's done.",
            job.id
        )))
    }
}

/// M51.4: the longest text `speak` reads out (a voice reply, not a
/// document; the speech models' own limits are a few thousand characters).
const SPEAK_MAX_CHARS: usize = 4000;

/// M51.4: `speak {text, voice?}`, a voice reply: OpenRouter's
/// `/audio/speech`, saved under `media/`, sent to the chat as a voice note.
pub struct SpeakTool {
    shared: Arc<MediaShared>,
    session_id: String,
}

impl SpeakTool {
    pub fn new(shared: Arc<MediaShared>, session_id: impl Into<String>) -> Self {
        Self {
            shared,
            session_id: session_id.into(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for SpeakTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "speak".into(),
            description: "Say something out loud: turn `text` into speech, save it in the workspace under media/ and send it to this chat as a voice message. Use it when the person asks for a voice reply or sent a voice message and wants one back; write the text as it should be spoken. Each call spends a little money.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "What to say, as it should be spoken (at most 4,000 characters)" },
                    "voice": { "type": "string", "description": "A voice the speech model knows; the configured one when omitted" }
                },
                "required": ["text"]
            }),
        }
    }

    fn changes_files(&self) -> bool {
        true
    }

    /// M57.1: rare, and loaded when wanted.
    fn deferrable(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let Some((model, default_voice)) = self.shared.speech.get().cloned() else {
            return Err(failed(
                "speak",
                "no speech model is set ([media] speech_model)",
            ));
        };
        let text = args["text"].as_str().unwrap_or_default().trim().to_string();
        if text.is_empty() {
            return Err(failed("speak", "say what to speak"));
        }
        let chars = text.chars().count();
        if chars > SPEAK_MAX_CHARS {
            return Err(failed(
                "speak",
                format!("that's {chars} characters; a voice reply takes at most {SPEAK_MAX_CHARS}. Shorten it, or send it as text"),
            ));
        }
        self.shared
            .check_cap()
            .map_err(|e| failed("speak", e.to_string()))?;
        let voice = args["voice"]
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .or(default_voice);
        let started = std::time::Instant::now();
        let out = match self
            .shared
            .backend
            .speak(SpeechReq {
                text: text.clone(),
                model: Some(model.clone()),
                voice,
            })
            .await
        {
            Ok(o) => o,
            Err(e) => {
                self.shared.ledger(
                    &self.session_id,
                    &model,
                    started.elapsed().as_millis() as u64,
                    Some(&e.to_string()),
                );
                return Err(failed("speak", e.to_string()));
            }
        };
        let path = fresh_path(&ctx.workspace, &out.format);
        std::fs::write(&path, &out.bytes)
            .map_err(|e| failed("speak", format!("{}: {e}", path.display())))?;
        let rel = path
            .strip_prefix(&ctx.workspace)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let mut job = ferrule_media::JobStore::speech(&rel, &model, &text, &rel);
        let chat = self
            .shared
            .router()
            .and_then(|r| r.chat_of(&self.session_id));
        if let Some((channel, chat_id)) = &chat {
            job.channel = Some(channel.clone());
            job.chat_id = Some(chat_id.clone());
        }
        self.shared
            .store
            .record(&job)
            .map_err(|e| failed("speak", e.to_string()))?;
        self.shared.ledger(
            &self.session_id,
            &model,
            started.elapsed().as_millis() as u64,
            None,
        );
        if let Some((channel_name, chat_id)) = chat {
            if let Err(e) = send_out(&self.shared, &channel_name, &chat_id, &path, "", true).await {
                return Ok(ToolOutput::ok(format!(
                    "Saved {rel}, but sending it to the chat failed: {e}. The file is there."
                )));
            }
            return Ok(ToolOutput::ok(format!(
                "Said it: {rel} is sent to this chat as a voice message."
            )));
        }
        Ok(ToolOutput::ok(format!(
            "Saved {rel}. This session has no chat to send it to; send_file can deliver it where needed."
        )))
    }
}

/// Send a finished file to a chat, the way `send_file` does.
async fn deliver(
    shared: &MediaShared,
    channel_name: &str,
    chat_id: &str,
    path: &std::path::Path,
    caption: &str,
) -> Result<(), String> {
    send_out(shared, channel_name, chat_id, path, caption, false).await
}

/// `voice`: the attachment's kind is `voice`, which Telegram sends with
/// `sendVoice` (M51.4); other channels send it by its file type.
async fn send_out(
    shared: &MediaShared,
    channel_name: &str,
    chat_id: &str,
    path: &std::path::Path,
    caption: &str,
    voice: bool,
) -> Result<(), String> {
    let router = shared.router().ok_or("the gateway isn't running")?;
    let channel = router
        .channel(channel_name)
        .ok_or_else(|| format!("the {channel_name} channel isn't running"))?;
    let name = path
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    let msg = OutboundMessage {
        channel: channel_name.to_string(),
        chat_id: chat_id.to_string(),
        text: caption.chars().take(900).collect(),
        reply_to: None,
        attachments: vec![Attachment {
            kind: if voice {
                "voice".into()
            } else {
                files::mime_for(&name).to_string()
            },
            url: path.to_string_lossy().into_owned(),
            name: Some(name),
        }],
    };
    channel.send(msg).await.map_err(|e| format!("{e}"))
}

/// The built-in task that polls pending video jobs, downloads finished
/// clips and delivers them to their chats (`ferrule-media` on the
/// scheduler, every minute). No model call is ever made here.
pub struct MediaPoller {
    pub shared: Arc<MediaShared>,
}

#[async_trait::async_trait]
impl BuiltinJob for MediaPoller {
    async fn run(&self, _task: &Task) -> Result<JobReport, String> {
        let pending = self
            .shared
            .store
            .pending_videos()
            .map_err(|e| e.to_string())?;
        let mut done = 0usize;
        let mut failed_n = 0usize;
        for job in pending {
            match self.shared.backend.poll_video(&job.id).await {
                Ok(VideoState::Pending) => {}
                Ok(VideoState::Done { url }) => {
                    let out = std::path::Path::new(&job.out_path);
                    match self.shared.backend.download(&url, out).await {
                        Ok(()) => {
                            let mut note = format!("{} is here.", job.out_path);
                            if let (Some(channel), Some(chat_id)) = (&job.channel, &job.chat_id) {
                                match deliver(&self.shared, channel, chat_id, out, &job.prompt)
                                    .await
                                {
                                    Ok(()) => {
                                        note = format!("{} delivered to {channel}.", job.out_path)
                                    }
                                    Err(e) => {
                                        note = format!(
                                            "{} is saved but couldn't be sent: {e}",
                                            job.out_path
                                        )
                                    }
                                }
                            }
                            self.shared
                                .store
                                .mark(&job.id, "done", None)
                                .map_err(|e| e.to_string())?;
                            self.shared.ledger(&job.id, &job.model, 0, None);
                            done += 1;
                            tracing::info!(job = %job.id, "{note}");
                        }
                        Err(e) => {
                            self.shared
                                .store
                                .mark(&job.id, "failed", Some(&e.to_string()))
                                .map_err(|e| e.to_string())?;
                            failed_n += 1;
                        }
                    }
                }
                Ok(VideoState::Failed { reason }) => {
                    self.shared
                        .store
                        .mark(&job.id, "failed", Some(&reason))
                        .map_err(|e| e.to_string())?;
                    if let (Some(channel), Some(chat_id)) = (&job.channel, &job.chat_id) {
                        let _ = say(&self.shared, channel, chat_id, &format!(
                            "The video \"{}\" didn't render: {reason}. Nothing else is running for it.",
                            job.prompt.chars().take(80).collect::<String>()
                        ))
                        .await;
                    }
                    self.shared.ledger(&job.id, &job.model, 0, Some(&reason));
                    failed_n += 1;
                }
                Err(e) => {
                    // A poll error is transient: the row stays pending.
                    tracing::warn!(job = %job.id, "poll failed: {e}");
                }
            }
        }
        Ok(JobReport {
            summary: format!("media poller: {done} delivered, {failed_n} failed"),
            incomplete: None,
        })
    }
}

/// A plain text note to a chat (a failure worth saying).
async fn say(
    shared: &MediaShared,
    channel_name: &str,
    chat_id: &str,
    text: &str,
) -> Result<(), String> {
    let router = shared.router().ok_or("the gateway isn't running")?;
    let channel = router
        .channel(channel_name)
        .ok_or_else(|| format!("the {channel_name} channel isn't running"))?;
    channel
        .send(OutboundMessage {
            channel: channel_name.to_string(),
            chat_id: chat_id.to_string(),
            text: text.to_string(),
            reply_to: None,
            attachments: vec![],
        })
        .await
        .map_err(|e| e.to_string())
}

/// The poller's built-in task spec.
pub fn spec() -> BuiltinSpec {
    crate::scheduler::BuiltinSpec {
        name: "ferrule-media".into(),
        schedule: "* * * * *".into(),
        timezone: "UTC".into(),
        description: "[built-in] deliver finished video renders".into(),
    }
}
