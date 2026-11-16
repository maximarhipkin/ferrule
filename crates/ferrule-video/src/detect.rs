//! One analysis pass over a video: the arguments for ffmpeg and a parser
//! for what it prints. The picture is scaled down first (detection doesn't
//! need detail), then `blackdetect`, `freezedetect` and a scene-change
//! `select` + `metadata=print` run in one chain; the sound goes through
//! `silencedetect` and `ebur128` (loudness, loudness range, true peak).
//! All of it is printed to stderr at the info level, so the caller keeps
//! stderr, not stdout.

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::Path;

/// The filters this pass and the edit recipes use; `ferrule doctor` names
/// the ones a build lacks.
pub const FILTERS: &[&str] = &[
    "blackdetect",
    "freezedetect",
    "silencedetect",
    "ebur128",
    "select",
    "metadata",
    "subtitles",
    "loudnorm",
    "sidechaincompress",
    "xfade",
    "overlay",
    "tile",
];

/// The scene-change score above which a frame counts as a cut. 0.3 is
/// ffmpeg's documented example; slow dissolves score lower and are missed.
pub const SCENE: f64 = 0.3;

/// A stretch of the video, in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub start: f64,
    pub end: f64,
}

impl Span {
    pub fn len(&self) -> f64 {
        (self.end - self.start).max(0.0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() <= 0.0
    }
}

/// What the pass measured. A `None` or an empty list from a pass that
/// didn't run means "not measured"; [`Analysis::measured_video`] and
/// [`Analysis::measured_audio`] say which halves ran.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Analysis {
    pub measured_video: bool,
    pub measured_audio: bool,
    /// Integrated loudness, LUFS.
    pub integrated: Option<f64>,
    /// Loudness range, LU.
    pub lra: Option<f64>,
    /// True peak, dBTP (ebur128 prints it as dBFS of the oversampled peak).
    pub true_peak: Option<f64>,
    pub black: Vec<Span>,
    pub frozen: Vec<Span>,
    pub silent: Vec<Span>,
    /// Where the picture changes (scene score above [`SCENE`]).
    pub cuts: Vec<f64>,
}

/// ffmpeg's arguments for the pass over `input`. `video` and `audio` say
/// which streams it has (from ffprobe), so a silent clip or a sound file
/// still runs.
pub fn args(input: &Path, video: bool, audio: bool) -> Vec<OsString> {
    let mut a: Vec<OsString> = [
        "-hide_banner",
        "-nostats",
        "-nostdin",
        "-loglevel",
        "info",
        "-protocol_whitelist",
        "file",
        "-i",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    a.push(input.as_os_str().to_owned());
    if video {
        a.push("-vf".into());
        a.push(
            format!(
                "scale=-2:360,blackdetect=d=0.1:pix_th=0.10,freezedetect=n=0.003:d=1,select='gt(scene\\,{SCENE})',metadata=print:key=lavfi.scene_score"
            )
            .into(),
        );
    } else {
        a.push("-vn".into());
    }
    if audio {
        a.push("-af".into());
        a.push("silencedetect=n=-45dB:d=0.5,ebur128=peak=true".into());
    } else {
        a.push("-an".into());
    }
    a.extend(["-f", "null", "-"].map(OsString::from));
    a
}

/// The number after `key` in `line` (`black_start:1.5`, `silence_end: 3`).
fn after(line: &str, key: &str) -> Option<f64> {
    let rest = &line[line.find(key)? + key.len()..];
    let rest = rest.trim_start_matches([':', '=', ' ']);
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e')))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// What the pass printed (`stderr`), for a video `length` seconds long
/// (stretches still open at the end close there).
pub fn parse(stderr: &str, length: f64, video: bool, audio: bool) -> Analysis {
    let mut a = Analysis {
        measured_video: video,
        measured_audio: audio,
        ..Analysis::default()
    };
    let (mut freeze, mut silence) = (None::<f64>, None::<f64>);
    let mut summary = false;
    for line in stderr.lines() {
        if line.contains("black_start") {
            if let (Some(s), Some(e)) = (after(line, "black_start"), after(line, "black_end")) {
                a.black.push(Span { start: s, end: e });
            }
        } else if line.contains("freeze_start") {
            freeze = after(line, "freeze_start");
        } else if line.contains("freeze_end") {
            if let (Some(s), Some(e)) = (freeze.take(), after(line, "freeze_end")) {
                a.frozen.push(Span { start: s, end: e });
            }
        } else if line.contains("silence_start") {
            silence = after(line, "silence_start");
        } else if line.contains("silence_end") {
            if let (Some(s), Some(e)) = (silence.take(), after(line, "silence_end")) {
                a.silent.push(Span {
                    start: s.max(0.0),
                    end: e,
                });
            }
        } else if line.contains("Parsed_metadata") && line.contains("pts_time") {
            if let Some(t) = after(line, "pts_time") {
                a.cuts.push(t);
            }
        } else if line.contains("Summary:") && line.contains("ebur128") {
            summary = true;
        } else if summary {
            let t = line.trim_start();
            if t.starts_with("I:") {
                a.integrated = after(t, "I:");
            } else if t.starts_with("LRA:") {
                a.lra = after(t, "LRA:");
            } else if t.starts_with("Peak:") {
                a.true_peak = after(t, "Peak:");
            }
        }
    }
    if let Some(s) = freeze {
        a.frozen.push(Span {
            start: s,
            end: length,
        });
    }
    if let Some(s) = silence {
        a.silent.push(Span {
            start: s.max(0.0),
            end: length,
        });
    }
    a
}
