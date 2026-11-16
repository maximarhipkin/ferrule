//! M69 phase 3: `render_video {edl, dry_run?}` and `render_video {status:
//! true}` (`docs/m69-video-craft.md`). The model writes an edit list
//! ([`ferrule_video::edl::Edl`]); this checks it against the sources
//! (ffprobe), compiles it into ffmpeg steps and runs them in the sandbox,
//! one render at a time in this process. In a chat it runs as a job that
//! outlives the turn (the Claude engine's own shell can't) and wakes the
//! session when it ends; for a sub-agent it runs inside the call. The
//! output must be a new file, so every version stays for the critic. Work
//! files go under `.ferrule/render/` and are removed at the end; a render
//! the process didn't finish (a restart) leaves no output, and `status`
//! says no render is known.

use super::check::probe;
use super::watch::{self, Ffmpeg};
use super::FileOut;
use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_sandbox::Sandbox;
use ferrule_video::check::{human, Probe};
use ferrule_video::edl::{self, Edl, Source};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DIR: &str = ".ferrule/render";

/// The render running or last finished in this process.
struct Job {
    session: String,
    output: String,
    started: Instant,
    step: String,
    result: Option<Result<String, String>>,
}

static JOB: Mutex<Option<Job>> = Mutex::new(None);

fn failed(message: impl Into<String>) -> CoreError {
    CoreError::ToolFailed {
        tool: "render_video".into(),
        message: message.into(),
    }
}

fn rel(workspace: &Path, p: &Path) -> String {
    p.strip_prefix(workspace)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned()
}

/// Everything a render needs, checked and resolved.
struct Plan {
    edl: Edl,
    sources: Vec<(PathBuf, Source)>,
    lens: Vec<f64>,
    output: PathBuf,
    work: PathBuf,
    captions: Option<String>,
    fonts: Option<String>,
    voice: Option<PathBuf>,
    music: Option<PathBuf>,
    cap: Option<u64>,
}

impl Plan {
    fn total(&self) -> f64 {
        self.lens.iter().sum()
    }
}

pub struct RenderVideoTool {
    sandbox: Arc<Sandbox>,
    ffmpeg: Ffmpeg,
    session_id: String,
    hidden: Vec<PathBuf>,
    /// The chat to wake when a render ends; `None` (a sub-agent): it runs
    /// inside the call.
    out: Option<Arc<FileOut>>,
}

impl RenderVideoTool {
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
        }
    }

    /// Run in the background and wake this chat's session at the end.
    pub fn for_chat(mut self, out: Arc<FileOut>) -> Self {
        self.out = Some(out);
        self
    }

    fn resolve(&self, ws: &Path, p: &str) -> Result<PathBuf, CoreError> {
        ferrule_tools::fs_tools::resolve(ws, &self.hidden, p)
    }

    async fn plan(&self, args: &Value, ws: &Path) -> Result<Plan, CoreError> {
        let raw = match &args["edl"] {
            Value::String(p) => {
                let file = self.resolve(ws, p)?;
                let text = std::fs::read_to_string(&file)
                    .map_err(|e| failed(format!("couldn't read {p}: {e}")))?;
                serde_json::from_str::<Value>(&text)
                    .map_err(|e| failed(format!("{p} isn't JSON: {e}")))?
            }
            Value::Object(_) => args["edl"].clone(),
            _ => return Err(failed("`edl` is the edit list: a JSON object, or the path of a .json file in the workspace")),
        };
        let edl: Edl = serde_json::from_value(raw)
            .map_err(|e| failed(format!("the edit list doesn't fit the schema: {e}")))?;
        // The sources, each probed once.
        let mut seen: HashMap<PathBuf, Source> = HashMap::new();
        let mut sources = Vec::new();
        for (i, s) in edl.segments.iter().enumerate() {
            let path = self.resolve(ws, &s.source)?;
            if !path.is_file() {
                return Err(failed(format!(
                    "segment {}: {} isn't a file",
                    i + 1,
                    s.source
                )));
            }
            let facts = match seen.get(&path) {
                Some(f) => *f,
                None => {
                    let f = if edl::is_image(&s.source) {
                        Source {
                            seconds: 0.0,
                            audio: false,
                            image: true,
                        }
                    } else {
                        let p = Probe::of(
                            &probe(&self.sandbox, &self.ffmpeg, &path, ws)
                                .await
                                .map_err(|e| {
                                    failed(format!("ffprobe couldn't read {}: {e}", s.source))
                                })?,
                        );
                        if !p.has_video() {
                            return Err(failed(format!(
                                "segment {}: {} has no picture",
                                i + 1,
                                s.source
                            )));
                        }
                        Source {
                            seconds: p.seconds,
                            audio: p.audio_codec.is_some(),
                            image: false,
                        }
                    };
                    seen.insert(path.clone(), f);
                    f
                }
            };
            sources.push((path, facts));
        }
        let facts: Vec<Source> = sources.iter().map(|(_, f)| *f).collect();
        let lens = edl.lengths(&facts).map_err(|bad| {
            failed(format!(
                "the edit list has problems:\n- {}",
                bad.join("\n- ")
            ))
        })?;
        let output = self.resolve(ws, &edl.output)?;
        if output.exists() {
            return Err(failed(format!(
                "{} exists: render each version to a new file (v2.mp4, v3.mp4…) so the versions can be compared",
                edl.output
            )));
        }
        if sources.iter().any(|(p, _)| *p == output) {
            return Err(failed("the output can't be one of the sources"));
        }
        let stem: String = output
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .take(40)
            .collect();
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let work = ws.join(DIR).join(format!("{stem}-{millis}"));
        let file = |name: &Option<String>, what: &str| -> Result<Option<PathBuf>, CoreError> {
            match name {
                None => Ok(None),
                Some(n) => {
                    let p = self.resolve(ws, n)?;
                    if p.is_file() {
                        Ok(Some(p))
                    } else {
                        Err(failed(format!("the {what} {n} isn't a file")))
                    }
                }
            }
        };
        let captions_src = file(&edl.captions, "captions")?;
        let captions = captions_src.as_ref().map(|p| {
            let ext = if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("ass")) {
                "ass"
            } else {
                "srt"
            };
            rel(ws, &work.join(format!("captions.{ext}")))
        });
        let fonts = match &edl.fonts {
            None => None,
            Some(f) => {
                let dir = self.resolve(ws, f)?;
                if !dir.is_dir() {
                    return Err(failed(format!("the fonts folder {f} isn't a folder")));
                }
                let r = rel(ws, &dir);
                if !edl::filter_safe(&r) {
                    return Err(failed(format!(
                        "the fonts folder {r} has characters ffmpeg's filter can't take: use letters, digits, . _ - and / only"
                    )));
                }
                Some(r)
            }
        };
        if captions.as_deref().is_some_and(|c| !edl::filter_safe(c)) {
            return Err(failed("the work folder's name can't go into a filter; rename the output with letters, digits, - and _"));
        }
        let voice = file(&edl.voice.as_ref().map(|v| v.file.clone()), "voice-over")?;
        let music = file(&edl.music.as_ref().map(|m| m.file.clone()), "music")?;
        let cap = self.out.as_ref().and_then(|o| o.send_cap(&self.session_id));
        if let Some(src) = &captions_src {
            std::fs::create_dir_all(&work)
                .map_err(|e| failed(format!("couldn't make {DIR}: {e}")))?;
            std::fs::copy(src, ws.join(captions.as_deref().unwrap_or_default()))
                .map_err(|e| failed(format!("couldn't copy the captions: {e}")))?;
        }
        Ok(Plan {
            edl,
            sources,
            lens,
            output,
            work,
            captions,
            fonts,
            voice,
            music,
            cap,
        })
    }
}

/// The steps, run in order; each one's name goes into the job's status.
async fn run_all(
    sandbox: Arc<Sandbox>,
    ffmpeg: Ffmpeg,
    ws: PathBuf,
    p: Plan,
) -> Result<String, String> {
    let started = Instant::now();
    std::fs::create_dir_all(&p.work).map_err(|e| format!("couldn't make {DIR}: {e}"))?;
    let result = steps(&sandbox, &ffmpeg, &ws, &p).await;
    let _ = std::fs::remove_dir_all(&p.work);
    let (size, extra) = result?;
    let name = rel(&ws, &p.output);
    let [w, h] = p.edl.size;
    Ok(format!(
        "Rendered {name}: {}, {w}×{h}, {} ({} segments) in {}.{extra} Check it with check_video, then watch_video it (overview and cuts) before you send it.",
        ferrule_video::clock(p.total()),
        human(size),
        p.lens.len(),
        ferrule_video::clock(started.elapsed().as_secs_f64())
    ))
}

fn step(name: String) {
    if let Some(j) = JOB.lock().unwrap().as_mut() {
        j.step = name;
    }
}

fn limit(secs: f64) -> Duration {
    Duration::from_secs((60.0 + secs).min(3600.0) as u64)
}

async fn steps(sb: &Sandbox, ff: &Ffmpeg, ws: &Path, p: &Plan) -> Result<(u64, String), String> {
    let n = p.sources.len();
    let total = p.total();
    let mut parts = Vec::new();
    for (i, ((src, facts), len)) in p.sources.iter().zip(&p.lens).enumerate() {
        step(format!("segment {} of {n}", i + 1));
        let name = format!("seg{i:04}.mp4");
        let part = p.work.join(&name);
        watch::run(
            sb,
            &ff.ffmpeg,
            &p.edl.segment(i, src, *facts, *len, &part),
            ws,
            limit(len * 4.0),
        )
        .await
        .map_err(|e| {
            format!(
                "segment {} ({}) failed: {e}",
                i + 1,
                p.edl.segments[i].source
            )
        })?;
        parts.push(name);
    }
    step("joining".into());
    let list = p.work.join("list.txt");
    std::fs::write(&list, edl::concat_list(&parts)).map_err(|e| e.to_string())?;
    let joined = p.work.join("joined.mp4");
    watch::run(
        sb,
        &ff.ffmpeg,
        &edl::concat(&list, &joined),
        ws,
        limit(total),
    )
    .await
    .map_err(|e| format!("joining failed: {e}"))?;
    step("mixing, captions".into());
    let pre = p.work.join("pre.mp4");
    let mix = p.edl.mix(
        &joined,
        p.voice.as_deref(),
        p.music.as_deref(),
        p.captions.as_deref(),
        p.fonts.as_deref(),
        total,
        &pre,
    );
    watch::run(sb, &ff.ffmpeg, &mix, ws, limit(total * 4.0))
        .await
        .map_err(|e| format!("mixing failed: {e}"))?;
    step("loudness".into());
    let mastered = p.work.join("mastered.mp4");
    let mut extra = String::new();
    let measured = watch::run_full(sb, &ff.ffmpeg, &edl::measure(&pre), ws, limit(total * 2.0))
        .await
        .ok()
        .and_then(|(_, err)| edl::measured(&String::from_utf8_lossy(&err)));
    let args = match &measured {
        Some(m) => edl::master(&pre, m, &mastered),
        None => {
            extra.push_str(" Its loudness couldn't be measured, so it isn't mastered to -14 LUFS.");
            edl::remux(&pre, &mastered)
        }
    };
    watch::run(sb, &ff.ffmpeg, &args, ws, limit(total * 2.0))
        .await
        .map_err(|e| format!("mastering failed: {e}"))?;
    let mut fin = mastered;
    let size = |f: &Path| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
    if let Some(cap) = p.cap.filter(|c| size(&fin) > *c) {
        step("fitting the send limit".into());
        let kbps = edl::fitting_kbps(cap, total);
        let small = p.work.join("small.mp4");
        watch::run(
            sb,
            &ff.ffmpeg,
            &edl::shrink(&fin, kbps, &small),
            ws,
            limit(total * 4.0),
        )
        .await
        .map_err(|e| format!("fitting the size failed: {e}"))?;
        extra.push_str(&format!(
            " It was re-encoded at {kbps} kb/s to fit this chat's {}.",
            human(cap)
        ));
        if size(&small) > cap {
            extra.push_str(" It is still too big: shorten it or lower the size.");
        }
        fin = small;
    }
    if let Some(parent) = p.output.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&fin, &p.output)
        .or_else(|_| std::fs::copy(&fin, &p.output).map(|_| ()))
        .map_err(|e| format!("couldn't move the result to {}: {e}", p.output.display()))?;
    Ok((size(&p.output), extra))
}

/// The steps as text, for `dry_run`.
fn shown(args: &[OsString]) -> String {
    args.iter()
        .map(|a| {
            let s = a.to_string_lossy();
            if s.contains([' ', ';', '[', '\'', '|']) {
                format!("\"{s}\"")
            } else {
                s.into_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[async_trait::async_trait]
impl Tool for RenderVideoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "render_video".into(),
            description: "Render a video from an edit list: segments of videos or images (each cut, fitted to one size and frame rate, its sound faded 30 ms at the ends), joined, then voice-over and music mixed in (music ducked), fades, captions burned in last, loudness mastered to -14 LUFS, faststart, and fitted to this chat's send limit. The output must be a new .mp4 (one per version). In a chat it runs in the background and you are woken when it's done; `status: true` says where it is. `dry_run: true` checks the list and shows the ffmpeg steps without running them. The edit list's fields are in the video-editing skill's references/edit-list.md.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "edl": {
                        "description": "The edit list as a JSON object, or the path of a .json file holding it: {output, size?: [w,h], fps?, segments: [{source, start?, end?, fit?: crop|pad|blur, zoom?, push?, mute?}], captions?, fonts?, voice?: {file, at?, volume?}, music?: {file, volume?, duck?}, source_volume?, fade_in?, fade_out?}",
                        "type": ["object", "string"]
                    },
                    "dry_run": { "type": "boolean", "description": "Check the list and show the steps, don't render" },
                    "status": { "type": "boolean", "description": "Say whether a render is running or how the last one ended" }
                }
            }),
        }
    }

    fn deferrable(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        if args["status"].as_bool() == Some(true) {
            let job = JOB.lock().unwrap();
            return Ok(ToolOutput::ok(match job.as_ref() {
                None => {
                    "No render is known in this process (none started since it began).".to_string()
                }
                Some(j) => match &j.result {
                    None => format!(
                        "Rendering {} for {}: {} so far, now {}.",
                        j.output,
                        if j.session == self.session_id {
                            "this chat"
                        } else {
                            "another chat"
                        },
                        ferrule_video::clock(j.started.elapsed().as_secs_f64()),
                        j.step
                    ),
                    Some(Ok(done)) => done.clone(),
                    Some(Err(e)) => format!("The last render, {}, failed: {e}", j.output),
                },
            }));
        }
        let ws = ctx.workspace.clone();
        let plan = self.plan(&args, &ws).await?;
        let total = plan.total();
        if args["dry_run"].as_bool() == Some(true) {
            let mut text = format!(
                "The edit list is fine: {} segments, {} in all, into {}. The steps:",
                plan.lens.len(),
                ferrule_video::clock(total),
                rel(&ws, &plan.output)
            );
            for (i, ((src, facts), len)) in plan.sources.iter().zip(&plan.lens).enumerate().take(3)
            {
                let part = plan.work.join(format!("seg{i:04}.mp4"));
                text.push_str(&format!(
                    "\n{}. ffmpeg {}",
                    i + 1,
                    shown(&plan.edl.segment(i, src, *facts, *len, &part))
                ));
            }
            if plan.lens.len() > 3 {
                text.push_str(&format!(
                    "\n… and {} more segments the same way.",
                    plan.lens.len() - 3
                ));
            }
            let joined = plan.work.join("joined.mp4");
            text.push_str(&format!(
                "\nJoin: ffmpeg {}\nMix: ffmpeg {}\nThen loudness measured and mastered to -14 LUFS, faststart{}.",
                shown(&edl::concat(&plan.work.join("list.txt"), &joined)),
                shown(&plan.edl.mix(&joined, plan.voice.as_deref(), plan.music.as_deref(), plan.captions.as_deref(), plan.fonts.as_deref(), total, &plan.work.join("pre.mp4"))),
                if plan.cap.is_some() { ", and fitted to this chat's send limit if bigger" } else { "" }
            ));
            let _ = std::fs::remove_dir_all(&plan.work);
            return Ok(ToolOutput::ok(text));
        }
        let output = rel(&ws, &plan.output);
        let count = plan.lens.len();
        {
            let mut job = JOB.lock().unwrap();
            if let Some(j) = job.as_ref().filter(|j| j.result.is_none()) {
                return Err(failed(format!(
                    "a render is already running ({}, {}); one at a time on this machine. Call with status: true, and try again when it's done",
                    j.output, j.step
                )));
            }
            *job = Some(Job {
                session: self.session_id.clone(),
                output: output.clone(),
                started: Instant::now(),
                step: "starting".into(),
                result: None,
            });
        }
        let (sb, ff) = (self.sandbox.clone(), self.ffmpeg.clone());
        let Some(out) = self.out.clone() else {
            // A sub-agent: inside the call.
            let r = run_all(sb, ff, ws, plan).await;
            if let Some(j) = JOB.lock().unwrap().as_mut() {
                j.result = Some(r.clone());
            }
            return r.map(ToolOutput::ok).map_err(failed);
        };
        let session = self.session_id.clone();
        let name = output.clone();
        tokio::spawn(async move {
            let r = run_all(sb, ff, ws, plan).await;
            let text = match &r {
                Ok(done) => format!("[render_video finished.] {done}"),
                Err(e) => format!("[render_video failed for {name}.] {e}. Fix the edit list and render again, or tell the person what blocks it."),
            };
            if let Some(j) = JOB.lock().unwrap().as_mut() {
                j.result = Some(r);
            }
            let woke = out
                .router()
                .is_some_and(|router| router.wake(&session, text));
            if !woke {
                tracing::info!(session = %session, "render_video ended; the session couldn't be woken (status has it)");
            }
        });
        Ok(ToolOutput::ok(format!(
            "Rendering {} in the background: {} segments, {} in all. You'll be woken with the result when it's done; until then you can answer other things. `render_video {{status: true}}` says where it is.",
            output,
            count,
            ferrule_video::clock(total)
        )))
    }
}
