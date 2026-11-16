//! M69 §5: `check_video {path, profile?, captions?}` (`docs/m69-video-craft.md`).
//! Measures a render instead of trusting a look at it: ffprobe's facts, one
//! analysis pass (loudness and true peak, silence, black and frozen
//! picture, cuts; [`ferrule_video::detect`]), the file's layout
//! (faststart), the size against what this chat can send, and, given a
//! captions file, its timing. [`ferrule_video::check`] turns the numbers
//! into FAIL and WARN findings for the kind of video (`profile`), each with
//! a time and a fix. The result is recorded in the session's
//! [`Looked`], which `send_file` reads: a video the agent made with a FAIL
//! goes only with `known_issues`. ffmpeg runs in the sandbox like
//! `watch_video`'s; the analysis is kept per file and modification time, so
//! `watch_video {focus: "cuts"}` reuses it.

use super::watch::{self, Ffmpeg};
use super::FileOut;
use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_core::vision::Looked;
use ferrule_sandbox::Sandbox;
use ferrule_video::check::{self, Level, Probe, Profile};
use ferrule_video::detect::{self, Analysis};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// After this many failing versions in one folder the fix loop stops: the
/// agent sends with `known_issues` or says what is still wrong.
pub const MAX_FAILING: u32 = 3;

const PROBE_LIMIT: Duration = Duration::from_secs(30);
/// The analyses kept, newest last.
const KEPT: usize = 16;

type Kept = (PathBuf, Option<SystemTime>, Analysis);
static ANALYSES: Mutex<Vec<Kept>> = Mutex::new(Vec::new());

fn failed(message: impl Into<String>) -> CoreError {
    CoreError::ToolFailed {
        tool: "check_video".into(),
        message: message.into(),
    }
}

/// ffprobe's facts about `path`, as JSON.
pub(crate) async fn probe(
    sandbox: &Sandbox,
    ffmpeg: &Ffmpeg,
    path: &Path,
    workspace: &Path,
) -> Result<Value, String> {
    let args: Vec<OsString> = [
        "-v",
        "error",
        "-protocol_whitelist",
        "file",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
    ]
    .iter()
    .map(OsString::from)
    .chain([path.as_os_str().to_owned()])
    .collect();
    let out = watch::run(sandbox, &ffmpeg.ffprobe, &args, workspace, PROBE_LIMIT).await?;
    serde_json::from_slice(&out).map_err(|e| format!("ffprobe's answer isn't JSON: {e}"))
}

/// The analysis pass over `path` ([`detect::args`]), or the one kept for
/// it as it is now. Its time limit grows with the video's length.
pub(crate) async fn analyse(
    sandbox: &Sandbox,
    ffmpeg: &Ffmpeg,
    path: &Path,
    workspace: &Path,
    p: &Probe,
) -> Result<Analysis, String> {
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let at = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    if let Some((_, _, a)) = ANALYSES
        .lock()
        .unwrap()
        .iter()
        .find(|(k, t, _)| *k == key && *t == at)
    {
        return Ok(a.clone());
    }
    let (video, audio) = (p.has_video(), p.audio_codec.is_some());
    let limit = Duration::from_secs((60.0 + p.seconds * 1.5).min(900.0) as u64);
    let (_, stderr) = watch::run_full(
        sandbox,
        &ffmpeg.ffmpeg,
        &detect::args(path, video, audio),
        workspace,
        limit,
    )
    .await?;
    let a = detect::parse(&String::from_utf8_lossy(&stderr), p.seconds, video, audio);
    let mut kept = ANALYSES.lock().unwrap();
    kept.retain(|(k, _, _)| *k != key);
    kept.push((key, at, a.clone()));
    if kept.len() > KEPT {
        kept.remove(0);
    }
    Ok(a)
}

pub struct CheckVideoTool {
    sandbox: Arc<Sandbox>,
    ffmpeg: Ffmpeg,
    session_id: String,
    hidden: Vec<PathBuf>,
    /// The chat's send limit comes from here; `None` (a sub-agent), none.
    out: Option<Arc<FileOut>>,
    /// Where the verdict goes for `send_file`; `None` (a sub-agent), nowhere.
    looked: Option<Looked>,
}

impl CheckVideoTool {
    pub fn new(
        sandbox: Arc<Sandbox>,
        ffmpeg: Ffmpeg,
        session_id: impl Into<String>,
        hidden: Vec<PathBuf>,
    ) -> Self {
        Self {
            sandbox,
            ffmpeg,
            session_id: session_id.into(),
            hidden,
            out: None,
            looked: None,
        }
    }

    /// The chat this session sends to (its size limit) and the record
    /// `send_file` reads.
    pub fn for_chat(mut self, out: Arc<FileOut>, looked: Looked) -> Self {
        self.out = Some(out);
        self.looked = Some(looked);
        self
    }
}

#[async_trait::async_trait]
impl Tool for CheckVideoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "check_video".into(),
            description: "Measure a video instead of guessing: loudness and true peak, silence, black and frozen picture, cuts and shot lengths, codec and pixel format, faststart, sound and picture lengths, its size against what this chat can send, and (with `captions`) whether the captions can be read. Returns FAIL findings (broken: fix before sending) and WARN findings (usually wrong for this kind of video), each with a time and a fix, then the numbers. Check every video you made or edited before you send it, and again after each fix.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "The video, relative to the workspace (or absolute within it)" },
                    "profile": { "type": "string", "enum": ["short", "slides", "promo", "long"], "description": "The kind of video, which decides what counts as a problem: short (vertical clip for Reels/TikTok/Shorts), slides (slides or an explainer), promo (an ad, often from generated clips), long (a landscape edit). Guessed from its shape and length when omitted" },
                    "captions": { "type": "string", "description": "An SRT, WebVTT or ASS file of the captions burned into or shipped with it, to check their timing" }
                },
                "required": ["path"]
            }),
        }
    }

    fn changes_files(&self) -> bool {
        false
    }

    fn deferrable(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let path = ferrule_tools::fs_tools::resolve(
            &ctx.workspace,
            &self.hidden,
            args["path"].as_str().unwrap_or(""),
        )?;
        if !path.is_file() {
            return Err(failed(format!("{} isn't a file", path.display())));
        }
        let shown = path
            .strip_prefix(&ctx.workspace)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let probed = probe(&self.sandbox, &self.ffmpeg, &path, &ctx.workspace)
            .await
            .map_err(|e| failed(format!("ffprobe couldn't read {shown}: {e}")))?;
        let p = Probe::of(&probed);
        if !p.has_video() && p.audio_codec.is_none() {
            return Err(failed(format!(
                "{shown} has neither a picture nor sound that ffprobe can read"
            )));
        }
        let profile = match args["profile"].as_str().filter(|s| !s.trim().is_empty()) {
            Some(s) => Profile::parse(s).ok_or_else(|| {
                failed(format!(
                    "`{s}` isn't a profile: short, slides, promo or long"
                ))
            })?,
            None => Profile::guess(&p),
        };
        let analysis = analyse(&self.sandbox, &self.ffmpeg, &path, &ctx.workspace, &p).await;
        let faststart = matches!(p.container.as_str(), "mov" | "mp4")
            .then(|| check::faststart(&path))
            .flatten();
        let send_cap = self.out.as_ref().and_then(|o| o.send_cap(&self.session_id));
        let input = check::Input {
            profile,
            probe: &p,
            analysis: analysis.as_ref().map_err(Clone::clone),
            faststart,
            send_cap,
        };
        let mut found = check::findings(&input);
        let mut caption_note = None;
        if let Some(c) = args["captions"].as_str().filter(|s| !s.trim().is_empty()) {
            let file = ferrule_tools::fs_tools::resolve(&ctx.workspace, &self.hidden, c)?;
            caption_note = Some(match std::fs::read_to_string(&file) {
                Err(e) => format!("not measured: couldn't read {c} ({e})."),
                Ok(text) => match check::captions(&text, profile, p.seconds) {
                    Err(why) => format!("not measured: {c} has {why}."),
                    Ok(more) => {
                        let n = more.len();
                        found.extend(more);
                        if n == 0 {
                            format!("{c}: every caption can be read in time.")
                        } else {
                            format!("{c}: {n} finding(s) above.")
                        }
                    }
                },
            });
        }
        let mut text = check::report(&shown, &input, &found, caption_note.as_deref());
        let fails: Vec<String> = found
            .iter()
            .filter(|f| f.level == Level::Fail)
            .map(|f| f.what.clone())
            .collect();
        if let Some(looked) = &self.looked {
            let version = looked.checked(&path, fails.clone());
            if !fails.is_empty() {
                text.push_str(&if version >= MAX_FAILING {
                    format!(
                        "\nThis is failing version {version} of {MAX_FAILING} in this folder: stop re-rendering. Send it with `known_issues` saying what is still wrong, or tell the person what blocks it."
                    )
                } else {
                    format!(
                        "\nFailing version {version} of {MAX_FAILING}: fix the FAIL findings and check again; send_file won't send it as it is without `known_issues`."
                    )
                });
            }
        }
        Ok(ToolOutput::ok(text))
    }
}
