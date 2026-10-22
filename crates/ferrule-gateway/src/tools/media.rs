//! M51: `generate_image` and `generate_video`, chat-bound like
//! [`SendFileTool`], plus the poller that delivers finished videos
//! (`docs/m51-media.md`). Images are a sync call: generated, saved under
//! `media/` in the workspace, and (M65 §4) handed back to the model to
//! look at, not sent. Videos are an async job: submitted, one row in the
//! `media_jobs` table, an immediate answer; the [`MediaPoller`] built-in
//! task polls and downloads, then (M65 §4) wakes the session that asked to
//! watch the clip before it sends it. M51.4: `speak`, a sync call like an
//! image, sent as a voice note.
//!
//! Spend honesty: the daily cap (`[media] max_per_day`) is checked before
//! every call from the store — fail-closed — and every call and delivery
//! writes a ledger row with `call_kind = "media"`. The tools say plainly
//! that video costs dollars, not cents. Since wave A (2026-10-21) a row
//! carries the dollars: an image's and a finished video's `usage.cost`,
//! a video's estimate at submit (settled at delivery), a speech reply's
//! list-price estimate when there is one. A tool call's row is charged to
//! its run through [`MediaSpend`] (the trust hub), and the ledger is what
//! the day and task caps read. Models come from `[media]`'s allow-lists,
//! never from free text (M21).

use crate::channels::files;
use crate::message::{Attachment, OutboundMessage};
use crate::router::Router;
use crate::scheduler::{BuiltinJob, BuiltinSpec, JobReport, Task};
use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_core::{LedgerRecord, LedgerSink};
use ferrule_media::jobs::now_unix;
use ferrule_media::{ImageReq, JobKind, JobStore, MediaBackend, SpeechReq, VideoReq, VideoState};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};

/// The owner's dollar caps behind media spend: M19's trust hub in the
/// CLI. Without one (tests) only the counts hold.
pub trait MediaSpend: Send + Sync {
    /// Dollars `tree` may still spend under the run, day and task caps
    /// that are set; `None` when no dollar cap is set. `Err`: the caps
    /// can't be read, and the call is refused.
    fn usd_left(&self, tree: &str) -> Result<Option<f64>, String>;
    /// Charge one media row to `tree`'s run, as a model call's row is.
    fn charge(&self, tree: &str, row: &LedgerRecord);
}

/// `[media]`'s limits beyond the daily count.
#[derive(Debug, Clone, Default)]
pub struct MediaLimits {
    /// Image models a call may name besides the default; empty: only the
    /// default.
    pub image_models: Vec<String>,
    /// Video models a call may name besides the default.
    pub video_models: Vec<String>,
    /// Video submits per day; 0 = only `max_per_day` counts them.
    pub max_videos_per_day: u64,
    /// The most one video may cost by its estimate, in dollars; 0 = no
    /// per-video cap.
    pub max_usd_per_video: f64,
    /// M65 §3: the video models that take reference images
    /// (`input_references`); a reference for any other is refused.
    pub reference_models: Vec<String>,
}

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
    /// The allow-lists and dollar limits; unset, only the defaults and no
    /// dollar limit.
    limits: OnceLock<MediaLimits>,
    /// The trust hub's dollar caps; unset, none.
    spend: OnceLock<Arc<dyn MediaSpend>>,
    /// M65 §0: the vision helper's model (`[media] vision_model`); unset,
    /// no helper.
    vision: OnceLock<String>,
    /// M65 §0: what the helper said, by image, modification time and task,
    /// so the same file isn't paid for twice.
    pub(super) looks: std::sync::Mutex<std::collections::HashMap<String, String>>,
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
            limits: OnceLock::new(),
            spend: OnceLock::new(),
            vision: OnceLock::new(),
            looks: Default::default(),
        })
    }

    /// M65 §0: turn the vision helper on with `model`. The first call wins.
    pub fn set_vision(&self, model: String) {
        let _ = self.vision.set(model);
    }

    /// M65 §0: the vision helper for one session whose model can't see,
    /// charged to `tree`; `None` when `[media] vision_model` is off.
    pub fn looker(
        self: &Arc<Self>,
        session_id: &str,
        tree: &str,
    ) -> Option<Arc<dyn ferrule_core::vision::Looker>> {
        let model = self.vision.get()?.clone();
        Some(Arc::new(super::look::VisionHelper::new(
            self.clone(),
            model,
            session_id,
            tree,
        )))
    }

    /// The allow-lists and dollar limits (set once, before the tools are
    /// made: their definitions list the allowed models).
    pub fn set_limits(&self, limits: MediaLimits) {
        let _ = self.limits.set(limits);
    }

    /// The dollar caps media spend answers to, and is charged to.
    pub fn set_spend(&self, spend: Arc<dyn MediaSpend>) {
        let _ = self.spend.set(spend);
    }

    fn limits(&self) -> MediaLimits {
        self.limits.get().cloned().unwrap_or_default()
    }

    /// The models a `kind` call may name: the default first, then the
    /// allow-list's others.
    fn allowed(&self, kind: JobKind) -> Vec<String> {
        let limits = self.limits();
        let (default, listed) = match kind {
            JobKind::Video => (&self.video_model, limits.video_models),
            _ => (&self.image_model, limits.image_models),
        };
        let mut all = vec![default.clone()];
        for m in listed {
            if !all.contains(&m) {
                all.push(m);
            }
        }
        all
    }

    /// The model for a call: the default when it names none, else one
    /// of [`Self::allowed`]. A model never enters through a tool call's
    /// free text (M21): the owner lists it in the config first.
    fn pick(&self, kind: JobKind, asked: Option<&str>) -> Result<String, String> {
        let allowed = self.allowed(kind);
        let Some(asked) = asked.map(str::trim).filter(|m| !m.is_empty()) else {
            return Ok(allowed[0].clone());
        };
        if allowed.iter().any(|m| m == asked) {
            return Ok(asked.to_string());
        }
        let key = match kind {
            JobKind::Video => "video_models",
            _ => "image_models",
        };
        Err(format!(
            "`{asked}` isn't one of the models allowed here ({}); the owner adds others to [media] {key}",
            allowed.join(", ")
        ))
    }

    /// The `model` parameter of a tool's definition: only when there's a
    /// choice, and then only the allowed ones.
    fn model_param(&self, kind: JobKind) -> Option<Value> {
        let allowed = self.allowed(kind);
        (allowed.len() > 1).then(|| {
            json!({
                "type": "string",
                "enum": allowed,
                "description": format!("One of the allowed models; {} when omitted", allowed[0]),
            })
        })
    }

    /// Dollars `tree` may still spend; `None` when no dollar cap is set.
    fn usd_left(&self, tree: &str) -> Result<Option<f64>, String> {
        match self.spend.get() {
            Some(spend) => spend.usd_left(tree).map_err(|e| {
                format!("the [trust] dollar caps can't be checked ({e}), so no paid media call")
            }),
            None => Ok(None),
        }
    }

    /// Refuses a paid call once `tree`'s run, day or task has no dollars
    /// left (what it costs isn't known before an image or a voice reply).
    pub(super) fn check_budget(&self, tree: &str) -> Result<Option<f64>, String> {
        let left = self.usd_left(tree)?;
        if left.is_some_and(|l| l <= 0.0) {
            return Err(
                "this run, today or this task has no dollars left under the [trust] caps, so no paid media call".into(),
            );
        }
        Ok(left)
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
        let used = self
            .store
            .count_since(midnight())
            .map_err(|e| format!("the daily media cap can't be checked: {e}"))?;
        if used >= self.max_per_day {
            return Err(format!(
                "the daily media cap of {} is reached ([media] max_per_day); it resets at midnight UTC",
                self.max_per_day
            ));
        }
        Ok(())
    }

    /// The daily video cap, fail-closed like [`Self::check_cap`].
    fn check_video_cap(&self) -> Result<(), String> {
        let cap = self.limits().max_videos_per_day;
        if cap == 0 {
            return Ok(());
        }
        let used = self
            .store
            .count_kind_since(JobKind::Video, midnight())
            .map_err(|e| format!("the daily video cap can't be checked: {e}"))?;
        if used >= cap {
            return Err(format!(
                "the daily video cap of {cap} is reached ([media] max_videos_per_day); it resets at midnight UTC"
            ));
        }
        Ok(())
    }

    /// One ledger row per call or delivery, `call_kind = "media"`, with
    /// its dollars when they're known. `run`: the tree a tool call's row
    /// is charged to (the poller's settling rows charge no run; the day
    /// and task caps read them from the ledger).
    pub(super) fn ledger(&self, row: Row<'_>, run: Option<&str>) {
        let record = row.record();
        if let (Some(tree), Some(spend)) = (run, self.spend.get()) {
            spend.charge(tree, &record);
        }
        if let Some(sink) = &self.ledger {
            sink.record(record);
        }
    }
}

/// Midnight UTC today, as a unix time: where the daily counts start.
fn midnight() -> i64 {
    chrono::Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|t| t.and_utc().timestamp())
        .unwrap_or(0)
}

/// What one media ledger row says.
pub(super) struct Row<'a> {
    pub(super) session_id: &'a str,
    /// The run tree, so a scheduled task's media spend counts toward its
    /// own cap.
    pub(super) tree: Option<&'a str>,
    pub(super) model: &'a str,
    pub(super) latency_ms: u64,
    pub(super) err: Option<&'a str>,
    pub(super) cost_usd: Option<f64>,
}

impl Row<'_> {
    fn record(&self) -> LedgerRecord {
        LedgerRecord {
            timestamp: chrono::Utc::now().to_rfc3339(),
            session_id: self.session_id.into(),
            task_shape: "media".into(),
            origin: None,
            provider: "media".into(),
            model: self.model.into(),
            iteration: 0,
            call_kind: "media".into(),
            input_tokens: 0,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            cache_write_1h_input_tokens: 0,
            output_tokens: 0,
            tool_calls: 0,
            latency_ms: self.latency_ms,
            outcome: if self.err.is_some() { "error" } else { "ok" }.into(),
            error_kind: self.err.map(|_| "media".into()),
            error_message: self.err.map(str::to_string),
            cost_usd: self.cost_usd,
            eval: None,
            tree: self.tree.map(str::to_string),
            route: None,
            speed: None,
            plan: None,
            notional_usd: None,
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
    /// The run tree its spend is charged to; the session's own by default.
    tree: String,
    hidden: Vec<PathBuf>,
}

impl GenerateImageTool {
    pub fn new(
        shared: Arc<MediaShared>,
        session_id: impl Into<String>,
        hidden: Vec<PathBuf>,
    ) -> Self {
        let session_id = session_id.into();
        Self {
            shared,
            tree: session_id.clone(),
            session_id,
            hidden,
        }
    }

    /// A sub-agent's tool: its spend goes to its root's run.
    pub fn in_tree(mut self, tree: impl Into<String>) -> Self {
        self.tree = tree.into();
        self
    }
}

#[async_trait::async_trait]
impl Tool for GenerateImageTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "generate_image".into(),
            description: "Create an image with an image model and save it in the workspace under media/. It isn't sent: you get it back to look at against the request (and the reference); send it with send_file when it's right. With `reference` paths it edits those images instead. To keep a person's face and look (from a photo sent in the chat), pass that photo in `reference` and say in the prompt what must stay the same: the face, hair, build, skin tone, clothes. Each call spends money — say so before a big batch.".into(),
            parameters: {
                let mut p = json!({
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string", "description": "What to draw" },
                        "size": { "type": "string", "description": "e.g. 1024x1024; the model's default when omitted" },
                        "reference": { "type": "array", "items": { "type": "string" }, "description": "Workspace image paths to edit or take as reference" }
                    },
                    "required": ["prompt"]
                });
                if let Some(model) = self.shared.model_param(JobKind::Image) {
                    p["properties"]["model"] = model;
                }
                p
            },
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
        // M65 §4: the first reference is what the check holds it against.
        let compare = references.first().cloned();
        let model = self
            .shared
            .pick(JobKind::Image, args["model"].as_str())
            .map_err(|e| failed("generate_image", e))?;
        self.shared
            .check_cap()
            .map_err(|e| failed("generate_image", e.to_string()))?;
        self.shared
            .check_budget(&self.tree)
            .map_err(|e| failed("generate_image", e))?;
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
                    Row {
                        session_id: &self.session_id,
                        tree: Some(&self.tree),
                        model: &model,
                        latency_ms: started.elapsed().as_millis() as u64,
                        err: Some(&e.to_string()),
                        cost_usd: None,
                    },
                    Some(&self.tree),
                );
                return Err(failed("generate_image", e.to_string()));
            }
        };
        // Recorded before anything else can fail: the image is paid for.
        self.shared.ledger(
            Row {
                session_id: &self.session_id,
                tree: Some(&self.tree),
                model: &model,
                latency_ms: started.elapsed().as_millis() as u64,
                err: None,
                cost_usd: out.cost_usd,
            },
            Some(&self.tree),
        );
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
        job.tree = Some(self.tree.clone());
        // M65 §4: made, not sent; `send_file` makes it `done`.
        job.state = "checking".into();
        job.references = references_shown(&ctx.workspace, compare.iter());
        self.shared
            .store
            .record(&job)
            .map_err(|e| failed("generate_image", e.to_string()))?;
        // M60: the model sees what it made (a note if it can't see images);
        // M65 §0: for one that can't, the vision helper checks it against
        // the request and the reference instead.
        let (shown, unshown) = match ferrule_core::vision::image_file(&path) {
            Some(Ok(mut image)) => {
                image.check = Some(ferrule_core::message::ImageCheck {
                    task: check_task(&prompt, compare.is_some()),
                    reference: compare.as_ref().map(|p| p.to_string_lossy().into_owned()),
                });
                (vec![image], String::new())
            }
            Some(Err(why)) => (Vec::new(), format!(" It isn't shown to you: {why}.")),
            None => (Vec::new(), String::new()),
        };
        let against = if compare.is_some() {
            "the request and the reference (the same person?)"
        } else {
            "the request"
        };
        let next = if chat.is_some() {
            "If it's right, send it with send_file"
        } else {
            "This session has no chat: if it's right, report its path"
        };
        Ok(ToolOutput::ok(format!(
            "Saved {rel}. Not sent: look at it against {against} first.{unshown} {next}; if it isn't, try again (at most two retries in all) or say what's wrong."
        ))
        .with_images(shown))
    }
}

/// M65 §4: what the vision helper is asked about a generated image (or
/// video sheet), for a model that can't see it: a verdict against the
/// request and, with a reference, the person in it.
pub fn check_task(prompt: &str, with_reference: bool) -> String {
    let (reference, same) = if with_reference {
        (
            " The second image is the reference: the person or thing in it must be the same in the first.",
            " a different person from the reference (face, hair, skin tone, build, age);",
        )
    } else {
        ("", "")
    };
    format!(
        "The first image was just generated for this request: \"{prompt}\".{reference} Check it before it is sent. First line: OK or NOT OK. Then each problem you see, and where:{same} warped or extra faces, hands or fingers; garbled or misspelled text; wrong framing or cropping; anything the request asked for that is missing or different. Then one or two sentences on what it shows. Plain sentences, no preamble."
    )
}

/// The workspace-relative paths of `refs`, for the jobs table.
fn references_shown<'a>(
    workspace: &std::path::Path,
    refs: impl Iterator<Item = &'a PathBuf>,
) -> Vec<String> {
    refs.map(|p| {
        p.strip_prefix(workspace)
            .unwrap_or(p)
            .to_string_lossy()
            .into_owned()
    })
    .collect()
}

pub struct GenerateVideoTool {
    shared: Arc<MediaShared>,
    session_id: String,
    /// The run tree its spend is charged to; the session's own by default.
    tree: String,
    hidden: Vec<PathBuf>,
    /// M65 §3: `reference` (a person's photo) is offered: the owner's own
    /// chats and sub-agents only.
    references: bool,
}

impl GenerateVideoTool {
    pub fn new(
        shared: Arc<MediaShared>,
        session_id: impl Into<String>,
        hidden: Vec<PathBuf>,
    ) -> Self {
        let session_id = session_id.into();
        Self {
            shared,
            tree: session_id.clone(),
            session_id,
            hidden,
            references: false,
        }
    }

    /// A sub-agent's tool: its spend goes to its root's run.
    pub fn in_tree(mut self, tree: impl Into<String>) -> Self {
        self.tree = tree.into();
        self
    }

    /// M65 §3: offer `reference`, a person's photo carried into the clip.
    /// Only for the owner's own chats and sub-agents: a real person's face
    /// isn't recreated for anyone else.
    pub fn with_references(mut self, on: bool) -> Self {
        self.references = on;
        self
    }

    /// The models a reference may go to: the allowed ones that take them.
    fn reference_capable(&self) -> Vec<String> {
        let takes = self.shared.limits().reference_models;
        self.shared
            .allowed(JobKind::Video)
            .into_iter()
            .filter(|m| takes.contains(m))
            .collect()
    }

    /// What a video will cost, from the list price, before it's submitted;
    /// `Err` with why it can't be estimated. A request that names no
    /// length is priced at the model's longest (an upper bound).
    async fn estimate(&self, model: &str, seconds: Option<u32>) -> Result<Estimate, String> {
        match self.shared.backend.video_price(model).await {
            Ok(Some(price)) => {
                let seconds = seconds.or(price.longest_seconds).unwrap_or(8);
                Ok(Estimate {
                    usd: price.usd_per_second * f64::from(seconds),
                    per_second: price.usd_per_second,
                    seconds,
                    resolution: price.resolution,
                })
            }
            Ok(None) => Err(format!(
                "the provider's price list has no per-second price for {model}"
            )),
            Err(e) => Err(format!("the provider's price list can't be read: {e}")),
        }
    }
}

/// A video's cost before it's made: list price × length.
struct Estimate {
    usd: f64,
    per_second: f64,
    seconds: u32,
    /// The resolution the price is for, asked for in the request (M65 §3).
    resolution: Option<&'static str>,
}

#[async_trait::async_trait]
impl Tool for GenerateVideoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "generate_video".into(),
            description: "Start a video generation job (text-to-video, or image-to-video with `first_frame`). Answers right away with a job id — the render takes minutes; when it's done you're woken to watch it, and you send it once it's right. Give `seconds`: without it the price is reckoned at the model's longest. A video costs dollars, not cents: say the price class before you start one.".into(),
            parameters: {
                let mut p = json!({
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string", "description": "What happens in the clip" },
                        "seconds": { "type": "integer", "description": "Length in seconds (the model's range)" },
                        "aspect": { "type": "string", "description": "e.g. 16:9, 9:16, 1:1" },
                        "first_frame": { "type": "string", "description": "A workspace image path to start from" }
                    },
                    "required": ["prompt"]
                });
                if let Some(model) = self.shared.model_param(JobKind::Video) {
                    p["properties"]["model"] = model;
                }
                // M65 §3: the same person, in the owner's chats.
                let capable = self.reference_capable();
                if self.references && !capable.is_empty() {
                    p["properties"]["reference"] = json!({
                        "type": "array",
                        "items": { "type": "string" },
                        "description": format!(
                            "Workspace images of a person or thing that must look the same in the clip (a photo sent in the chat). From a video, take a clear, front-facing still at full size first (the video-editing skill says how). Only these models take references: {}",
                            capable.join(", ")
                        ),
                    });
                }
                p
            },
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
        let model = self
            .shared
            .pick(JobKind::Video, args["model"].as_str())
            .map_err(|e| failed("generate_video", e))?;
        // M65 §3: references, checked before anything is spent.
        let mut references = Vec::new();
        for r in args["reference"]
            .as_array()
            .map(|l| l.iter().filter_map(|r| r.as_str()).collect::<Vec<_>>())
            .unwrap_or_default()
        {
            if !self.references {
                return Err(failed(
                    "generate_video",
                    "nothing was submitted: a reference (a person's photo) is only taken in the owner's own chats",
                ));
            }
            let path = ferrule_tools::fs_tools::resolve(&ctx.workspace, &self.hidden, r)?;
            match ferrule_core::vision::image_file(&path) {
                Some(Ok(_)) => references.push(path),
                Some(Err(why)) => {
                    return Err(failed(
                        "generate_video",
                        format!("nothing was submitted: the reference {r} is {why}"),
                    ))
                }
                None => {
                    return Err(failed(
                        "generate_video",
                        format!("nothing was submitted: the reference {r} isn't a PNG, JPEG, GIF or WebP image (from a video, take a still frame first)"),
                    ))
                }
            }
        }
        if !references.is_empty() && !self.shared.limits().reference_models.contains(&model) {
            let capable = self.reference_capable();
            return Err(failed(
                "generate_video",
                if capable.is_empty() {
                    format!("nothing was submitted: {model} doesn't take reference images, and no allowed model does; the owner adds one to [media] video_models and reference_models")
                } else {
                    format!(
                        "nothing was submitted: {model} doesn't take reference images (it would make the clip without the face and still bill it). Models that do: {}",
                        capable.join(", ")
                    )
                },
            ));
        }
        self.shared
            .check_cap()
            .map_err(|e| failed("generate_video", e.to_string()))?;
        self.shared
            .check_video_cap()
            .map_err(|e| failed("generate_video", e))?;
        // The dollars before the submit: a video can cost more than a
        // whole day of chat, so it's checked against the per-video cap
        // and what the run, day and task have left. Without an estimate
        // it starts only when no dollar limit is set.
        let seconds = args["seconds"].as_u64().map(|n| n as u32);
        let per_video = self.shared.limits().max_usd_per_video;
        let left = self
            .shared
            .usd_left(&self.tree)
            .map_err(|e| failed("generate_video", e))?;
        let estimate = self.estimate(&model, seconds).await;
        if per_video > 0.0 || left.is_some() {
            let est = estimate.as_ref().map_err(|why| {
                failed(
                    "generate_video",
                    format!(
                        "nothing was submitted: what a video on {model} costs can't be estimated ({why}), and {} needs an estimate first",
                        if per_video > 0.0 {
                            "[media] max_usd_per_video"
                        } else {
                            "the [trust] dollar caps"
                        }
                    ),
                )
            })?;
            let priced = format!(
                "a {} s video on {model} comes to about ${:.2} at ${:.3}/s list price",
                est.seconds, est.usd, est.per_second
            );
            if per_video > 0.0 && est.usd > per_video {
                return Err(failed(
                    "generate_video",
                    format!(
                        "nothing was submitted: {priced}, over [media] max_usd_per_video (${per_video:.2}). A shorter clip or a cheaper model fits; the owner can raise the cap"
                    ),
                ));
            }
            if let Some(left) = left.filter(|l| est.usd > *l) {
                return Err(failed(
                    "generate_video",
                    format!(
                        "nothing was submitted: {priced}, and this run, today or this task has ${left:.2} left under the [trust] dollar caps"
                    ),
                ));
            }
        }
        let estimate = estimate.ok();
        let started = std::time::Instant::now();
        let submitted = self
            .shared
            .backend
            .submit_video(VideoReq {
                prompt: prompt.clone(),
                model: Some(model.clone()),
                seconds,
                aspect: args["aspect"].as_str().map(str::to_string),
                first_frame,
                references: references.clone(),
                resolution: estimate
                    .as_ref()
                    .and_then(|e| e.resolution)
                    .map(str::to_string),
            })
            .await;
        let job = match submitted {
            Ok(j) => j,
            Err(e) => {
                self.shared.ledger(
                    Row {
                        session_id: &self.session_id,
                        tree: Some(&self.tree),
                        model: &model,
                        latency_ms: started.elapsed().as_millis() as u64,
                        err: Some(&e.to_string()),
                        cost_usd: None,
                    },
                    Some(&self.tree),
                );
                return Err(failed("generate_video", e.to_string()));
            }
        };
        // The estimate is charged now, to the run and (through the
        // ledger) the day; the delivery row settles it to the bill.
        let estimate_usd = estimate.as_ref().map(|e| e.usd);
        self.shared.ledger(
            Row {
                session_id: &self.session_id,
                tree: Some(&self.tree),
                model: &model,
                latency_ms: started.elapsed().as_millis() as u64,
                err: None,
                cost_usd: estimate_usd,
            },
            Some(&self.tree),
        );
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
        row.tree = Some(self.tree.clone());
        row.estimate_usd = estimate_usd;
        row.references = references_shown(&ctx.workspace, references.iter());
        self.shared
            .store
            .record(&row)
            .map_err(|e| failed("generate_video", e.to_string()))?;
        let cost = match &estimate {
            Some(e) => format!(" (about ${:.2} for {} s at list price)", e.usd, e.seconds),
            None => " (its price isn't known in advance)".into(),
        };
        if !has_chat {
            // M51.3: a sub-agent (or a run with no chat) gets the file,
            // not a delivery.
            return Ok(ToolOutput::ok(format!(
                "Making it — job {} on {model}{cost}. A render takes minutes; the gateway saves it as {rel} when it's done. This session has no chat to send it to: report that path; whoever sends it watches it first.",
                job.id
            )));
        }
        Ok(ToolOutput::ok(format!(
            "Making it — job {} on {model}{cost}. A render takes minutes; when {rel} is done, this chat's session is woken to watch it before it's sent. Don't wait for it.",
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
    /// The run tree its spend is charged to; the session's own by default.
    tree: String,
}

impl SpeakTool {
    pub fn new(shared: Arc<MediaShared>, session_id: impl Into<String>) -> Self {
        let session_id = session_id.into();
        Self {
            shared,
            tree: session_id.clone(),
            session_id,
        }
    }

    /// A sub-agent's tool: its spend goes to its root's run.
    pub fn in_tree(mut self, tree: impl Into<String>) -> Self {
        self.tree = tree.into();
        self
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
                    "voice": { "type": "string", "description": "A voice the speech model knows; the configured one when omitted" },
                    "send": { "type": "boolean", "description": "false: only save the MP3, don't send it (a voice-over for a video you're editing); true when omitted" }
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
        self.shared
            .check_budget(&self.tree)
            .map_err(|e| failed("speak", e))?;
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
                    Row {
                        session_id: &self.session_id,
                        tree: Some(&self.tree),
                        model: &model,
                        latency_ms: started.elapsed().as_millis() as u64,
                        err: Some(&e.to_string()),
                        cost_usd: None,
                    },
                    Some(&self.tree),
                );
                return Err(failed("speak", e.to_string()));
            }
        };
        // The speech answer is raw audio with no cost in it. A model
        // priced on its input alone gets an estimate: the list price per
        // unit (a character or a token, no more units than characters)
        // times the characters, an upper bound. Any other is counted only.
        let latency_ms = started.elapsed().as_millis() as u64;
        let cost_usd = match self.shared.backend.speech_price(&model).await {
            Ok(Some(per_unit)) => Some(per_unit * chars as f64),
            _ => None,
        };
        self.shared.ledger(
            Row {
                session_id: &self.session_id,
                tree: Some(&self.tree),
                model: &model,
                latency_ms,
                err: None,
                cost_usd,
            },
            Some(&self.tree),
        );
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
        job.tree = Some(self.tree.clone());
        self.shared
            .store
            .record(&job)
            .map_err(|e| failed("speak", e.to_string()))?;
        // M65 §2: a voice-over for an edit is saved, not sent.
        if args["send"].as_bool() == Some(false) {
            return Ok(ToolOutput::ok(format!(
                "Saved {rel}, not sent (send: false)."
            )));
        }
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

/// The built-in task that polls pending video jobs and downloads finished
/// clips (`ferrule-media` on the scheduler, every minute). No model call
/// is made here, and (M65 §4) no clip is sent: the session that asked is
/// woken to watch it and send it. A clip still unchecked after
/// [`CHECK_FOR`], or one whose session can't be woken, gets a text note in
/// its chat instead: where it is, and that "send it" sends it as is.
pub struct MediaPoller {
    pub shared: Arc<MediaShared>,
}

/// M65 §4: how long a woken session has to check a finished video.
pub const CHECK_FOR: i64 = 30 * 60;

/// M65 §4: what the session that asked for a video is woken with.
fn wake_text(job: &ferrule_media::MediaJob) -> String {
    let refs = if job.references.is_empty() {
        String::new()
    } else {
        format!(" The reference: {}.", job.references.join(", "))
    };
    let against = if job.references.is_empty() {
        "the request"
    } else {
        "the request and the reference (is it the same person?)"
    };
    format!(
        "[ferrule] The video you started is ready and not sent yet: job {} on {}, saved as {}. It was asked for: \"{}\".{refs} Watch it with watch_video and compare it with {against}. If it's right, send it with send_file. If it isn't, try again (at most two retries in all) or tell the owner what's wrong and what you tried. If videos can't be watched here, send it with send_file and `unchecked` saying why.",
        job.id, job.model, job.out_path, job.prompt
    )
}

/// M65 §4: the note a chat gets for a video nobody checked.
fn unchecked_note(job: &ferrule_media::MediaJob) -> String {
    format!(
        "The video for \"{}\" finished but wasn't checked, so it wasn't sent. It's at {}. Say \"send it\" and I'll send it as it is.",
        job.prompt.chars().take(80).collect::<String>(),
        job.out_path
    )
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
                Ok(VideoState::Done { url, cost_usd }) => {
                    // The submit row charged the estimate; this one makes
                    // the ledger's sum what the provider billed.
                    let settle = cost_usd.map(|billed| billed - job.estimate_usd.unwrap_or(0.0));
                    let out = std::path::Path::new(&job.out_path);
                    match self.shared.backend.download(&url, out).await {
                        Ok(()) => {
                            // M65 §4: the session that asked watches it
                            // first; a sub-agent's clip has no chat, and its
                            // parent watches it before sending.
                            let note = match (&job.channel, &job.chat_id) {
                                (Some(channel), Some(chat_id)) => {
                                    let session = job.tree.clone().unwrap_or_default();
                                    let woke = self.shared.router().is_some_and(|r| {
                                        r.can_wake(&session) && r.wake(&session, wake_text(&job))
                                    });
                                    if woke {
                                        self.shared
                                            .store
                                            .mark_checking(&job.id, now_unix())
                                            .map_err(|e| e.to_string())?;
                                        format!(
                                            "{} is here; {session} is woken to check it.",
                                            job.out_path
                                        )
                                    } else {
                                        let _ = say(
                                            &self.shared,
                                            channel,
                                            chat_id,
                                            &unchecked_note(&job),
                                        )
                                        .await;
                                        self.shared
                                            .store
                                            .mark(&job.id, "noted", None)
                                            .map_err(|e| e.to_string())?;
                                        format!("{} is here; its session can't be woken, so {channel} was told where.", job.out_path)
                                    }
                                }
                                _ => {
                                    self.shared
                                        .store
                                        .mark(&job.id, "done", None)
                                        .map_err(|e| e.to_string())?;
                                    format!("{} is here.", job.out_path)
                                }
                            };
                            self.shared.ledger(
                                Row {
                                    session_id: &job.id,
                                    tree: job.tree.as_deref(),
                                    model: &job.model,
                                    latency_ms: 0,
                                    err: None,
                                    cost_usd: settle,
                                },
                                None,
                            );
                            done += 1;
                            tracing::info!(job = %job.id, "{note}");
                        }
                        Err(e) => {
                            self.shared
                                .store
                                .mark(&job.id, "failed", Some(&e.to_string()))
                                .map_err(|e| e.to_string())?;
                            // Rendered, so billed, though it never arrived.
                            self.shared.ledger(
                                Row {
                                    session_id: &job.id,
                                    tree: job.tree.as_deref(),
                                    model: &job.model,
                                    latency_ms: 0,
                                    err: Some(&e.to_string()),
                                    cost_usd: settle,
                                },
                                None,
                            );
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
                    // OpenRouter doesn't bill a failed render ("Zero
                    // Completion Insurance"), so the estimate comes back.
                    self.shared.ledger(
                        Row {
                            session_id: &job.id,
                            tree: job.tree.as_deref(),
                            model: &job.model,
                            latency_ms: 0,
                            err: Some(&reason),
                            cost_usd: job.estimate_usd.map(|e| -e),
                        },
                        None,
                    );
                    failed_n += 1;
                }
                Err(e) => {
                    // A poll error is transient: the row stays pending.
                    tracing::warn!(job = %job.id, "poll failed: {e}");
                }
            }
        }
        // M65 §4: a woken session that didn't check its video in time (a
        // failed turn, `/stop`, a restart) leaves the chat a note instead.
        let mut noted = 0usize;
        for job in self
            .shared
            .store
            .stale_checking(now_unix() - CHECK_FOR)
            .map_err(|e| e.to_string())?
        {
            if let (Some(channel), Some(chat_id)) = (&job.channel, &job.chat_id) {
                let _ = say(&self.shared, channel, chat_id, &unchecked_note(&job)).await;
            }
            self.shared
                .store
                .mark(&job.id, "noted", None)
                .map_err(|e| e.to_string())?;
            noted += 1;
        }
        Ok(JobReport {
            summary: format!(
                "media poller: {done} finished, {failed_n} failed, {noted} unchecked after {} minutes",
                CHECK_FOR / 60
            ),
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
