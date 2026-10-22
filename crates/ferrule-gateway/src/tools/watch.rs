//! M65 §1: `watch_video {path, start?, end?, frames?}` (`docs/m65-video.md`).
//! The agent watches a video: the facts (`ffprobe`), one contact sheet of
//! evenly spread frames (ffmpeg's `tile` filter, one image, so one of the
//! four pixel slots), and what is said (the audio through the gateway's
//! [`Transcriber`]). ffmpeg and ffprobe run through the sandbox the way the
//! screen tools run their helpers: workspace paths only, the file
//! protocol only, a timeout, stderr's last line as the error. The sheet and
//! the audio are kept under `.ferrule/watch/` and pruned after a day; the
//! history holds the sheet's path, never its pixels (M60).

use crate::transcribe::{Audio, Transcriber};
use ferrule_core::error::CoreError;
use ferrule_core::message::ImageCheck;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_sandbox::Sandbox;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

/// Where the sheets and the audio go, under the workspace.
pub const DIR: &str = ".ferrule/watch";
/// The default number of frames on a sheet, and the most.
const FRAMES: u64 = 12;
const MAX_FRAMES: u64 = 16;
/// The longest side of a sheet, in pixels: well under the 3.5 MB image cap
/// as a JPEG, and as much as a model's image tiling makes use of.
const SHEET_SIDE: f64 = 2048.0;
/// The widest one tile gets.
const TILE_WIDTH: f64 = 512.0;
/// The most audio one call transcribes (transcription is paid per minute).
const MAX_AUDIO_SECS: f64 = 600.0;
/// How long the sheets and audio are kept.
const KEEP: Duration = Duration::from_secs(24 * 3600);

const PROBE_LIMIT: Duration = Duration::from_secs(30);
const SHEET_LIMIT: Duration = Duration::from_secs(120);
const AUDIO_LIMIT: Duration = Duration::from_secs(180);

/// ffmpeg and ffprobe, found once: on `PATH`, or in Homebrew's dirs. They
/// run in the sandbox, like ones the shell would run.
#[derive(Debug, Clone)]
pub struct Ffmpeg {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl Ffmpeg {
    /// Both programs, or `None` when either is missing.
    pub fn find() -> Option<Self> {
        Some(Self {
            ffmpeg: program("ffmpeg")?,
            ffprobe: program("ffprobe")?,
        })
    }
}

/// `name` on `PATH` or in Homebrew's dirs (`.exe` on Windows).
pub fn program(name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from))
        .map(|d| d.join(&file))
        .find(|p| p.is_file())
}

/// The sentence a video tool says when there's no ffmpeg.
pub const NO_FFMPEG: &str = "ffmpeg (with ffprobe) isn't installed on the machine ferrule runs on, so videos can't be watched or edited; the owner installs it with `brew install ffmpeg` or `apt install ffmpeg`";

pub struct WatchVideoTool {
    sandbox: Arc<Sandbox>,
    ffmpeg: Ffmpeg,
    transcriber: Option<Arc<dyn Transcriber>>,
    session_id: String,
    hidden: Vec<PathBuf>,
    /// M65 §4: the media jobs, so a generated video is checked against
    /// what it was asked to be.
    media: Option<Arc<super::media::MediaShared>>,
}

impl WatchVideoTool {
    pub fn new(
        sandbox: Arc<Sandbox>,
        ffmpeg: Ffmpeg,
        transcriber: Option<Arc<dyn Transcriber>>,
        session_id: impl Into<String>,
        hidden: Vec<PathBuf>,
    ) -> Self {
        Self {
            sandbox,
            ffmpeg,
            transcriber,
            session_id: session_id.into(),
            hidden,
            media: None,
        }
    }

    /// M65 §4: know the generated videos' requests and references.
    pub fn with_media(mut self, media: Option<Arc<super::media::MediaShared>>) -> Self {
        self.media = media;
        self
    }

    /// The job that made `path`, if one did: its request and references.
    fn made(&self, workspace: &Path, path: &Path) -> Option<ferrule_media::MediaJob> {
        let store = &self.media.as_ref()?.store;
        let rel = path.strip_prefix(workspace).unwrap_or(path);
        store
            .made_as(&path.to_string_lossy())
            .ok()
            .flatten()
            .or_else(|| store.made_as(&rel.to_string_lossy()).ok().flatten())
    }
}

fn failed(message: impl Into<String>) -> CoreError {
    CoreError::ToolFailed {
        tool: "watch_video".into(),
        message: message.into(),
    }
}

/// What ffprobe says about a file.
#[derive(Debug, Default)]
struct Facts {
    seconds: f64,
    size: u64,
    container: String,
    /// The picture as shown (a phone's rotation applied).
    width: u64,
    height: u64,
    fps: Option<f64>,
    video_codec: Option<String>,
    audio: Option<String>,
}

impl Facts {
    fn of(probe: &Value) -> Self {
        let num = |v: &Value| {
            v.as_str()
                .and_then(|s| s.trim().parse::<f64>().ok())
                .or_else(|| v.as_f64())
        };
        let streams = probe["streams"].as_array().cloned().unwrap_or_default();
        let video = streams
            .iter()
            .find(|s| s["codec_type"] == "video" && s["disposition"]["attached_pic"] != 1);
        let audio = streams.iter().find(|s| s["codec_type"] == "audio");
        let mut f = Facts {
            seconds: num(&probe["format"]["duration"])
                .or_else(|| video.and_then(|v| num(&v["duration"])))
                .unwrap_or(0.0),
            size: num(&probe["format"]["size"]).unwrap_or(0.0) as u64,
            container: probe["format"]["format_name"]
                .as_str()
                .unwrap_or_default()
                .split(',')
                .next()
                .unwrap_or_default()
                .to_string(),
            ..Facts::default()
        };
        if let Some(v) = video {
            f.width = v["width"].as_u64().unwrap_or(0);
            f.height = v["height"].as_u64().unwrap_or(0);
            let rotation = v["side_data_list"]
                .as_array()
                .and_then(|l| l.iter().find_map(|d| d["rotation"].as_f64()))
                .or_else(|| num(&v["tags"]["rotate"]))
                .unwrap_or(0.0);
            if (rotation.abs() - 90.0).abs() < 1.0 || (rotation.abs() - 270.0).abs() < 1.0 {
                std::mem::swap(&mut f.width, &mut f.height);
            }
            f.fps = v["avg_frame_rate"]
                .as_str()
                .and_then(rate)
                .or_else(|| v["r_frame_rate"].as_str().and_then(rate));
            f.video_codec = v["codec_name"].as_str().map(str::to_string);
        }
        if let Some(a) = audio {
            let mut said = a["codec_name"].as_str().unwrap_or("audio").to_string();
            if let Some(hz) = num(&a["sample_rate"]) {
                said.push_str(&format!(" {} kHz", trim_num(hz / 1000.0)));
            }
            match a["channels"].as_u64() {
                Some(1) => said.push_str(" mono"),
                Some(2) => said.push_str(" stereo"),
                Some(n) => said.push_str(&format!(" {n} channels")),
                None => {}
            }
            f.audio = Some(said);
        }
        f
    }

    fn has_video(&self) -> bool {
        self.width > 0 && self.height > 0
    }
}

/// `30000/1001` as frames a second; `None` for `0/0`.
fn rate(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/')?;
    let (n, d) = (n.parse::<f64>().ok()?, d.parse::<f64>().ok()?);
    (d > 0.0 && n > 0.0).then(|| n / d)
}

/// `29.97`, `30`, `44.1`: no trailing zeros.
fn trim_num(x: f64) -> String {
    let s = format!("{x:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// `1:05.3`, `0:00.0`, `1:02:03.0`.
pub fn clock(secs: f64) -> String {
    let secs = secs.max(0.0);
    let whole = secs.floor() as u64;
    let tenths = ((secs - whole as f64) * 10.0).floor() as u64;
    let (h, m, s) = (whole / 3600, whole / 60 % 60, whole % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}.{tenths}")
    } else {
        format!("{m}:{s:02}.{tenths}")
    }
}

/// A time argument: seconds (`12.5`) or a clock (`1:05`, `1:02:03.5`).
fn time_arg(v: &Value) -> Result<Option<f64>, String> {
    if v.is_null() {
        return Ok(None);
    }
    if let Some(x) = v.as_f64() {
        return Ok(Some(x));
    }
    let Some(s) = v.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let mut total = 0.0;
    for part in s.split(':') {
        let n: f64 = part
            .parse()
            .map_err(|_| format!("`{s}` isn't a time: give seconds (12.5) or m:ss (1:05)"))?;
        total = total * 60.0 + n;
    }
    Ok(Some(total))
}

/// The grid for `n` frames: columns, rows.
fn grid(n: u64) -> (u64, u64) {
    let cols = (n as f64).sqrt().ceil().max(1.0) as u64;
    (cols, n.div_ceil(cols))
}

/// One tile's size for a `cols`×`rows` sheet of `w`×`h` frames: the
/// sheet's longest side at most [`SHEET_SIDE`], even numbers (H.264's
/// scaler and the JPEG encoder like them).
fn tile(w: u64, h: u64, cols: u64, rows: u64) -> (u64, u64) {
    let aspect = w as f64 / h.max(1) as f64;
    let mut tw = TILE_WIDTH.min(SHEET_SIDE / cols as f64);
    let mut th = tw / aspect;
    if th * rows as f64 > SHEET_SIDE {
        th = SHEET_SIDE / rows as f64;
        tw = th * aspect;
    }
    let even = |x: f64| ((x / 2.0).floor() as u64 * 2).max(2);
    (even(tw), even(th))
}

/// `.ferrule/watch` under the workspace, made if missing, refused when it
/// leads outside it (a link a command made); files older than a day go.
fn watch_dir(workspace: &Path) -> Result<PathBuf, String> {
    let dir = workspace.join(DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't make {DIR}: {e}"))?;
    let (Ok(real), Ok(ws)) = (dunce_like(&dir), dunce_like(workspace)) else {
        return Err(format!("couldn't resolve {DIR}"));
    };
    if !real.starts_with(&ws) {
        return Err(format!("{DIR} leads outside the workspace"));
    }
    if let Ok(entries) = std::fs::read_dir(&real) {
        for e in entries.flatten() {
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > KEEP);
            if old {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(real)
}

/// The canonical path, without Windows' `\\?\` prefix where it isn't
/// needed (what `dunce` does; the gateway doesn't depend on it).
fn dunce_like(p: &Path) -> std::io::Result<PathBuf> {
    let real = std::fs::canonicalize(p)?;
    #[cfg(windows)]
    {
        let s = real.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            if !rest.starts_with("UNC") {
                return Ok(PathBuf::from(rest));
            }
        }
    }
    Ok(real)
}

/// A name for this call's files: the video's stem and the time.
fn stem(video: &Path) -> String {
    let base: String = video
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "video".into())
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{base}-{now}")
}

/// Runs `program args…` in the sandbox, up to `limit`; `Ok` is its stdout,
/// `Err` its last line of stderr or why it didn't run. M65 §5's posters
/// run the same way.
pub async fn run(
    sandbox: &Sandbox,
    program: &Path,
    args: &[OsString],
    workspace: &Path,
    limit: Duration,
) -> Result<Vec<u8>, String> {
    let cmd = sandbox
        .command(program, args, workspace)
        .map_err(|e| e.to_string())?;
    let mut cmd = tokio::process::Command::from(cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(limit, cmd.output())
        .await
        .map_err(|_| format!("it didn't finish in {} s", limit.as_secs()))?
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map_or_else(|| out.status.to_string(), str::to_string))
}

fn os(s: impl Into<OsString>) -> OsString {
    s.into()
}

#[async_trait::async_trait]
impl Tool for WatchVideoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "watch_video".into(),
            description: "Watch a video file in the workspace (one sent in this chat, or one you made or edited): its length, size and format, one contact sheet of frames spread evenly over it (an image you see, tiles in reading order), and what is said in it. To look closer at a part, call again with `start` and `end`. Watch a video before you answer about it, edit it or send it.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "The video, relative to the workspace (or absolute within it)" },
                    "start": { "type": ["number", "string"], "description": "Where to start, in seconds or m:ss; the beginning when omitted" },
                    "end": { "type": ["number", "string"], "description": "Where to stop, in seconds or m:ss; the end when omitted" },
                    "frames": { "type": "integer", "description": "Frames on the sheet, 1 to 16; 12 when omitted" }
                },
                "required": ["path"]
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
        let dir = watch_dir(&ctx.workspace).map_err(failed)?;

        // The facts.
        let probe = run(
            &self.sandbox,
            &self.ffmpeg.ffprobe,
            &[
                os("-v"),
                os("error"),
                os("-protocol_whitelist"),
                os("file"),
                os("-print_format"),
                os("json"),
                os("-show_format"),
                os("-show_streams"),
                os(&path),
            ],
            &ctx.workspace,
            PROBE_LIMIT,
        )
        .await
        .map_err(|e| failed(format!("ffprobe couldn't read {shown}: {e}")))?;
        let probe: Value = serde_json::from_slice(&probe)
            .map_err(|e| failed(format!("ffprobe's answer about {shown} isn't JSON: {e}")))?;
        let facts = Facts::of(&probe);
        if !facts.has_video() && facts.audio.is_none() {
            return Err(failed(format!(
                "{shown} has neither a picture nor sound that ffprobe can read"
            )));
        }
        let mut text = format!("{shown}: {}", clock(facts.seconds));
        if facts.has_video() {
            text.push_str(&format!(", {}×{}", facts.width, facts.height));
            if let Some(fps) = facts.fps {
                text.push_str(&format!(" at {} fps", trim_num(fps)));
            }
            if let Some(c) = &facts.video_codec {
                text.push_str(&format!(", {c}"));
            }
        } else {
            text.push_str(", no picture");
        }
        match &facts.audio {
            Some(a) => text.push_str(&format!(", sound: {a}")),
            None => text.push_str(", no sound"),
        }
        text.push_str(&format!(
            ", {} ({}).",
            crate::channels::files::human(facts.size),
            facts.container
        ));

        // The window.
        let start = time_arg(&args["start"]).map_err(failed)?.unwrap_or(0.0);
        let asked_end = time_arg(&args["end"]).map_err(failed)?;
        let length = facts.seconds;
        if length <= 0.0 {
            return Ok(ToolOutput::ok(format!(
                "{text}\nIts length can't be read, so no frames or sound were taken."
            )));
        }
        let end = asked_end.unwrap_or(length).min(length);
        if start < 0.0 || start >= end {
            return Err(failed(format!(
                "start {} and end {} don't make a part of a {} video",
                clock(start),
                clock(end),
                clock(length)
            )));
        }
        let span = end - start;

        // The contact sheet.
        let mut images = Vec::new();
        if facts.has_video() {
            let n = args["frames"]
                .as_u64()
                .unwrap_or(FRAMES)
                .clamp(1, MAX_FRAMES);
            let (cols, rows) = grid(n);
            let (tw, th) = tile(facts.width, facts.height, cols, rows);
            let times: Vec<f64> = (0..n)
                .map(|i| start + span * (i as f64 + 0.5) / n as f64)
                .collect();
            let sheet = dir.join(format!("{}-sheet.jpg", stem(&path)));
            let mut a: Vec<OsString> = vec![
                os("-hide_banner"),
                os("-loglevel"),
                os("error"),
                os("-nostdin"),
                os("-y"),
            ];
            for t in &times {
                a.extend([
                    os("-protocol_whitelist"),
                    os("file"),
                    os("-ss"),
                    os(format!("{t:.3}")),
                    os("-i"),
                    os(&path),
                ]);
            }
            let mut graph = String::new();
            for i in 0..n {
                graph.push_str(&format!(
                    "[{i}:v:0]trim=end_frame=1,scale={tw}:{th}:force_original_aspect_ratio=decrease,pad={tw}:{th}:(ow-iw)/2:(oh-ih)/2,setsar=1[f{i}];"
                ));
            }
            for i in 0..n {
                graph.push_str(&format!("[f{i}]"));
            }
            graph.push_str(&format!(
                "concat=n={n}:v=1:a=0,tile={cols}x{rows}:nb_frames={n}:padding=4:color=black[sheet]"
            ));
            a.extend([
                os("-filter_complex"),
                os(graph),
                os("-map"),
                os("[sheet]"),
                os("-frames:v"),
                os("1"),
                os("-q:v"),
                os("4"),
                os(&sheet),
            ]);
            match run(
                &self.sandbox,
                &self.ffmpeg.ffmpeg,
                &a,
                &ctx.workspace,
                SHEET_LIMIT,
            )
            .await
            {
                Err(e) => text.push_str(&format!("\nNo contact sheet: ffmpeg failed ({e}).")),
                Ok(_) => match ferrule_core::vision::image_file(&sheet) {
                    Some(Ok(mut image)) => {
                        let order: Vec<String> = times
                            .iter()
                            .enumerate()
                            .map(|(i, t)| format!("{}. {}", i + 1, clock(*t)))
                            .collect();
                        let lead = format!(
                            "a contact sheet of {n} frames from {}–{} of the video, {cols} across and {rows} down, in reading order at {}",
                            clock(start),
                            clock(end),
                            order.join(", ")
                        );
                        text.push_str(&format!("\nThe image is {lead}."));
                        // M65 §0: what the vision helper is asked, for a
                        // model that can't see the sheet itself; for a
                        // generated video (M65 §4), against its request and
                        // the person's reference.
                        let made = self.made(&ctx.workspace, &path);
                        let reference = made
                            .as_ref()
                            .and_then(|j| j.references.first())
                            .map(|r| ctx.workspace.join(r).to_string_lossy().into_owned());
                        let against = match &made {
                            Some(job) => {
                                text.push_str(&format!(
                                    "\nIt was generated for: \"{}\"{}. Compare it with that before you send it.",
                                    job.prompt,
                                    if job.references.is_empty() {
                                        String::new()
                                    } else {
                                        format!(", keeping the person in {}", job.references.join(", "))
                                    }
                                ));
                                format!(
                                    " It was generated for this request: \"{}\".{} Start with OK or NOT OK against the request{}, then each problem you see.",
                                    job.prompt,
                                    if reference.is_some() {
                                        " The second image is the reference: the person in it must be the same in the video (face, hair, skin tone, build, age)."
                                    } else {
                                        ""
                                    },
                                    if reference.is_some() { " and the reference" } else { "" }
                                )
                            }
                            None => String::new(),
                        };
                        image.check = Some(ImageCheck {
                            task: format!(
                                "This is {lead}.{against} Describe the video from it for an assistant that can't see images: tile by tile, what is on screen and what changes (people and how they look, never guessing who they are; actions; text on screen word for word; the setting), then what the clip as a whole shows, and anything that looks wrong (warped faces or hands, garbled text, glitches, black or frozen frames). Plain sentences, no preamble, at most 400 words."
                            ),
                            reference,
                        });
                        // M65 §4: looking at the sheet is looking at the
                        // video, for `send_file`.
                        image.shows = Some(path.to_string_lossy().into_owned());
                        images.push(image);
                    }
                    Some(Err(why)) => {
                        text.push_str(&format!("\nThe contact sheet is too big to show ({why}): call again with fewer frames."))
                    }
                    None => text.push_str("\nffmpeg made no contact sheet."),
                },
            }
        }

        // What is said.
        if facts.audio.is_some() {
            let heard_to = end.min(start + MAX_AUDIO_SECS);
            match &self.transcriber {
                None => text.push_str(
                    "\nWhat is said wasn't transcribed: transcription is off ([transcription] in the config).",
                ),
                Some(transcriber) => {
                    let audio = dir.join(format!("{}-audio.m4a", stem(&path)));
                    let a: Vec<OsString> = vec![
                        os("-hide_banner"),
                        os("-loglevel"),
                        os("error"),
                        os("-nostdin"),
                        os("-y"),
                        os("-protocol_whitelist"),
                        os("file"),
                        os("-ss"),
                        os(format!("{start:.3}")),
                        os("-to"),
                        os(format!("{heard_to:.3}")),
                        os("-i"),
                        os(&path),
                        os("-vn"),
                        os("-ac"),
                        os("1"),
                        os("-ar"),
                        os("16000"),
                        os("-c:a"),
                        os("aac"),
                        os("-b:a"),
                        os("48k"),
                        os(&audio),
                    ];
                    let heard = match run(
                        &self.sandbox,
                        &self.ffmpeg.ffmpeg,
                        &a,
                        &ctx.workspace,
                        AUDIO_LIMIT,
                    )
                    .await
                    {
                        Err(e) => Err(format!("ffmpeg couldn't take the sound out ({e})")),
                        Ok(_) => {
                            transcriber
                                .transcribe(&Audio {
                                    path: &audio,
                                    mime: "audio/mp4",
                                    session_id: &self.session_id,
                                    channel: self
                                        .session_id
                                        .split(':')
                                        .next()
                                        .unwrap_or("watch_video"),
                                })
                                .await
                        }
                    };
                    let _ = std::fs::remove_file(&audio);
                    let window = format!("{}–{}", clock(start), clock(heard_to));
                    match heard {
                        Ok(h) if h.text.trim().is_empty() => {
                            text.push_str(&format!("\nNothing is said in {window}."))
                        }
                        Ok(h) => {
                            text.push_str(&format!("\nWhat is said in {window}:\n{}", h.text.trim()))
                        }
                        Err(e) => text.push_str(&format!(
                            "\nWhat is said in {window} wasn't transcribed: {e}."
                        )),
                    }
                    if heard_to < end {
                        text.push_str(&format!(
                            "\nOnly {} of sound is transcribed a call: call again with start {} for the rest.",
                            clock(MAX_AUDIO_SECS),
                            clock(heard_to)
                        ));
                    }
                }
            }
        }
        if facts.has_video() {
            text.push_str("\nTo look closer at a part, call again with a narrower start and end.");
        }
        Ok(ToolOutput::ok(text).with_images(images))
    }
}
