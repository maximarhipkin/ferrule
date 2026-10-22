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
            description: "Create an image with an image model and save it in the workspace under media/ (and send it to this chat when there is one). With `reference` paths it edits those images instead. Each call spends money — say so before a big batch.".into(),
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
        self.shared
            .store
            .record(&job)
            .map_err(|e| failed("generate_image", e.to_string()))?;
        // M60: the model sees what it made (a note if it can't see images).
        let (shown, unshown) = match ferrule_core::vision::image_file(&path) {
            Some(Ok(image)) => (vec![image], String::new()),
            Some(Err(why)) => (Vec::new(), format!(" It isn't shown to you: {why}.")),
            None => (Vec::new(), String::new()),
        };
        if let Some((channel_name, chat_id)) = chat {
            if let Err(e) = deliver(&self.shared, &channel_name, &chat_id, &path, &prompt).await {
                return Ok(ToolOutput::ok(format!(
                    "Saved {rel}, but sending it to the chat failed: {e}. The file is there.{unshown}"
                ))
                .with_images(shown));
            }
            return Ok(
                ToolOutput::ok(format!("Saved {rel} and sent it to this chat.{unshown}"))
                    .with_images(shown),
            );
        }
        Ok(ToolOutput::ok(format!(
            "Saved {rel}. This session has no chat to send it to; send_file can deliver it where needed.{unshown}"
        ))
        .with_images(shown))
    }
}

pub struct GenerateVideoTool {
    shared: Arc<MediaShared>,
    session_id: String,
    /// The run tree its spend is charged to; the session's own by default.
    tree: String,
    hidden: Vec<PathBuf>,
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
        }
    }

    /// A sub-agent's tool: its spend goes to its root's run.
    pub fn in_tree(mut self, tree: impl Into<String>) -> Self {
        self.tree = tree.into();
        self
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
}

#[async_trait::async_trait]
impl Tool for GenerateVideoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "generate_video".into(),
            description: "Start a video generation job (text-to-video, or image-to-video with `first_frame`). Answers right away with a job id — the render takes minutes and the file is sent to this chat when it's done. A video costs dollars, not cents: say the price class before you start one.".into(),
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
                "Making it — job {} on {model}{cost}. A render takes minutes; the gateway saves it as {rel} when it's done. This session has no chat to send it to: report that path, and send_file can deliver it once it's there.",
                job.id
            )));
        }
        Ok(ToolOutput::ok(format!(
            "Making it — job {} on {model}{cost}. A render takes minutes; I'll send {rel} here when it's done.",
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
                Ok(VideoState::Done { url, cost_usd }) => {
                    // The submit row charged the estimate; this one makes
                    // the ledger's sum what the provider billed.
                    let settle = cost_usd.map(|billed| billed - job.estimate_usd.unwrap_or(0.0));
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
