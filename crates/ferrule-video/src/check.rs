//! What the numbers mean. A [`Profile`] (the kind of video) turns ffprobe's
//! facts, an [`Analysis`] and caption timings into [`Finding`]s: **FAIL**
//! only for output that is objectively broken (it clips, it can't be sent,
//! phones can't play it, it is mostly black), **WARN** for what is usually
//! wrong for that kind of video and may be meant. Each finding has a time
//! where there is one and a fix. Anything that wasn't measured is said so,
//! never passed.
//!
//! The numbers and where they come from are in `docs/m69-video-craft.md`.
//! Marked heuristics are this milestone's own, not a published rule.

use crate::detect::{Analysis, Span};
use crate::transcript;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Read;
use std::path::Path;

/// The loudness online video is mastered to (YouTube turns louder down),
/// LUFS integrated.
pub const TARGET_LUFS: f64 = -14.0;
/// The highest true peak a master should reach, dBTP.
pub const MAX_PEAK: f64 = -1.0;

/// The kind of video, which decides what is a problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// A vertical clip for Reels, TikTok, Shorts: fast, captioned.
    Short,
    /// Slides or an explainer: holds are expected.
    Slides,
    /// An ad or promo, often from generated clips.
    Promo,
    /// A longer landscape edit.
    Long,
}

impl Profile {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "short" | "reel" | "shorts" | "tiktok" | "vertical" => Self::Short,
            "slides" | "explainer" | "slideshow" => Self::Slides,
            "promo" | "ad" | "advert" => Self::Promo,
            "long" | "landscape" | "youtube" => Self::Long,
            _ => return None,
        })
    }

    /// A guess when none is given: tall and up to three minutes is a short,
    /// longer than three minutes is a long edit, the rest a promo.
    pub fn guess(p: &Probe) -> Self {
        if p.seconds > 180.0 {
            Self::Long
        } else if p.height > p.width {
            Self::Short
        } else {
            Self::Promo
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Short => "short",
            Self::Slides => "slides",
            Self::Promo => "promo",
            Self::Long => "long",
        }
    }

    /// The shortest silence that counts as dead air.
    fn dead_air(&self) -> f64 {
        match self {
            Self::Short | Self::Promo => 1.5,
            Self::Slides => 3.0,
            Self::Long => 5.0,
        }
    }

    /// The longest a picture may stay still before it's worth a look;
    /// `None`: holds are the point (slides).
    fn longest_still(&self) -> Option<f64> {
        match self {
            Self::Short | Self::Promo => Some(3.0),
            Self::Slides => None,
            Self::Long => Some(10.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Level {
    Fail,
    Warn,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub level: Level,
    /// Seconds into the video, where there's one place.
    pub at: Option<f64>,
    pub what: String,
    pub fix: String,
}

impl Finding {
    fn new(level: Level, at: Option<f64>, what: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            level,
            at,
            what: what.into(),
            fix: fix.into(),
        }
    }
}

/// What ffprobe says, the parts the checks use.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Probe {
    pub seconds: f64,
    pub size: u64,
    pub container: String,
    /// As shown (a phone's rotation applied).
    pub width: u64,
    pub height: u64,
    pub fps: Option<f64>,
    pub video_codec: Option<String>,
    pub pix_fmt: Option<String>,
    pub video_seconds: Option<f64>,
    pub audio_codec: Option<String>,
    pub audio_seconds: Option<f64>,
}

impl Probe {
    /// From `ffprobe -print_format json -show_format -show_streams`.
    pub fn of(probe: &Value) -> Self {
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
        let mut p = Probe {
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
            ..Probe::default()
        };
        if let Some(v) = video {
            p.width = v["width"].as_u64().unwrap_or(0);
            p.height = v["height"].as_u64().unwrap_or(0);
            let rotation = v["side_data_list"]
                .as_array()
                .and_then(|l| l.iter().find_map(|d| d["rotation"].as_f64()))
                .or_else(|| num(&v["tags"]["rotate"]))
                .unwrap_or(0.0);
            if ((rotation.abs() - 90.0).abs() < 1.0) || ((rotation.abs() - 270.0).abs() < 1.0) {
                std::mem::swap(&mut p.width, &mut p.height);
            }
            p.fps = ["avg_frame_rate", "r_frame_rate"]
                .iter()
                .find_map(|k| v[*k].as_str().and_then(rate));
            p.video_codec = v["codec_name"].as_str().map(str::to_string);
            p.pix_fmt = v["pix_fmt"].as_str().map(str::to_string);
            p.video_seconds = num(&v["duration"]);
        }
        if let Some(a) = audio {
            p.audio_codec = a["codec_name"].as_str().map(str::to_string);
            p.audio_seconds = num(&a["duration"]);
        }
        p
    }

    pub fn has_video(&self) -> bool {
        self.width > 0 && self.height > 0
    }
}

/// `30000/1001` as frames a second; `None` for `0/0`.
fn rate(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/')?;
    let (n, d) = (n.parse::<f64>().ok()?, d.parse::<f64>().ok()?);
    (d > 0.0 && n > 0.0).then(|| n / d)
}

/// Whether an MP4/MOV file has its index (`moov`) before its media
/// (`mdat`), so it starts playing before it has all arrived. `None` when
/// the file isn't one or can't be read.
pub fn faststart(path: &Path) -> Option<bool> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut pos = 0u64;
    let mut head = [0u8; 16];
    // Top-level boxes only; a sane file has a handful.
    for _ in 0..64 {
        if pos + 8 > len {
            return None;
        }
        use std::io::Seek;
        f.seek(std::io::SeekFrom::Start(pos)).ok()?;
        f.read_exact(&mut head[..8]).ok()?;
        let size32 = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as u64;
        let kind: [u8; 4] = [head[4], head[5], head[6], head[7]];
        let size = match size32 {
            0 => len - pos,
            1 => {
                f.read_exact(&mut head[8..16]).ok()?;
                u64::from_be_bytes(head[8..16].try_into().ok()?)
            }
            n => n,
        };
        match &kind {
            b"moov" => return Some(true),
            b"mdat" => return Some(false),
            _ => {}
        }
        if size < 8 {
            return None;
        }
        pos += size;
    }
    None
}

/// Everything a render is checked against, measured first.
pub struct Input<'a> {
    pub profile: Profile,
    pub probe: &'a Probe,
    /// The analysis pass; `None` when it didn't run (with why).
    pub analysis: Result<&'a Analysis, String>,
    pub faststart: Option<bool>,
    /// The most bytes the chat this goes to can send, when known.
    pub send_cap: Option<u64>,
}

/// The findings for a render, most serious first, in time order within.
pub fn findings(i: &Input) -> Vec<Finding> {
    let p = i.probe;
    let mut out = Vec::new();
    let fail = Level::Fail;
    let warn = Level::Warn;

    // Delivery.
    if let Some(cap) = i.send_cap {
        if p.size > cap {
            let kbps = (cap as f64 * 8.0 * 0.9 / p.seconds.max(1.0) / 1000.0).floor();
            out.push(Finding::new(
                fail,
                None,
                format!(
                    "{} is larger than the {} this chat can send",
                    human(p.size),
                    human(cap)
                ),
                format!("re-encode to about {kbps} kb/s in total (video plus audio), or a smaller frame size"),
            ));
        }
    }
    if p.has_video() {
        let codec = p.video_codec.as_deref().unwrap_or("?");
        let pix = p.pix_fmt.as_deref().unwrap_or("?");
        if codec == "h264" && !matches!(pix, "yuv420p" | "yuvj420p") {
            out.push(Finding::new(
                fail,
                None,
                format!("H.264 in {pix}: many phones play only yuv420p"),
                "add -pix_fmt yuv420p to the encode",
            ));
        } else if !matches!(codec, "h264" | "hevc" | "vp9" | "av1") {
            out.push(Finding::new(
                warn,
                None,
                format!("the picture is {codec}, which chats and phones may not play"),
                "encode with -c:v libx264 -pix_fmt yuv420p",
            ));
        }
        if p.width % 2 == 1 || p.height % 2 == 1 {
            out.push(Finding::new(
                warn,
                None,
                format!("odd frame size {}×{}", p.width, p.height),
                "scale to even numbers (scale=-2:<height>)",
            ));
        }
        match i.profile {
            Profile::Short if p.width > p.height => out.push(Finding::new(
                warn,
                None,
                format!(
                    "{}×{} is landscape, and a short is watched upright",
                    p.width, p.height
                ),
                "crop or pad to 1080×1920 (9:16)",
            )),
            Profile::Long if p.height > p.width => out.push(Finding::new(
                warn,
                None,
                format!(
                    "{}×{} is upright, and a long edit is watched landscape",
                    p.width, p.height
                ),
                "make it 1920×1080 (16:9)",
            )),
            _ => {}
        }
        if p.width.min(p.height) < 720 {
            out.push(Finding::new(
                warn,
                None,
                format!("{}×{} is below 720p and will look soft", p.width, p.height),
                "render at 1080 on the short side when the sources allow",
            ));
        }
    }
    if i.faststart == Some(false) {
        out.push(Finding::new(
            warn,
            None,
            "the index is at the end of the file, so it can't start playing until it has all arrived",
            "add -movflags +faststart to the encode",
        ));
    }
    if let Some(a) = &p.audio_codec {
        if p.container == "mov" && !matches!(a.as_str(), "aac" | "mp3" | "alac" | "opus") {
            out.push(Finding::new(
                warn,
                None,
                format!("the sound is {a}, which some players won't play in an MP4"),
                "encode the sound with -c:a aac -b:a 160k",
            ));
        }
    }
    if let (Some(v), Some(a)) = (p.video_seconds, p.audio_seconds) {
        if (v - a).abs() > 0.1 {
            let (longer, by) = if v > a {
                ("picture", v - a)
            } else {
                ("sound", a - v)
            };
            out.push(Finding::new(
                warn,
                Some(v.min(a)),
                format!(
                    "the {longer} runs {:.1} s longer than the {}",
                    by,
                    if v > a { "sound" } else { "picture" }
                ),
                "trim both to the same length (-shortest), or pad the shorter one on purpose",
            ));
        }
    }
    if p.has_video() && p.audio_codec.is_none() && i.profile != Profile::Slides {
        out.push(Finding::new(
            warn,
            None,
            "there is no sound track",
            "add music or voice, or say in the caption that it is silent on purpose",
        ));
    }

    // What the pass measured.
    let a = match &i.analysis {
        Ok(a) => *a,
        Err(_) => {
            sort(&mut out);
            return out;
        }
    };
    let length = p.seconds.max(0.0);
    if a.measured_audio {
        if let Some(peak) = a.true_peak {
            if peak > 0.0 {
                out.push(Finding::new(
                    fail,
                    None,
                    format!("true peak {peak:+.1} dBTP: the sound clips"),
                    format!(
                        "normalise with loudnorm=I={TARGET_LUFS}:TP={MAX_PEAK}:LRA=11 (two passes)"
                    ),
                ));
            } else if peak > MAX_PEAK {
                out.push(Finding::new(
                    warn,
                    None,
                    format!("true peak {peak:+.1} dBTP, above {MAX_PEAK} dBTP: it may clip after the platform re-encodes it"),
                    format!("normalise with loudnorm=I={TARGET_LUFS}:TP={MAX_PEAK}:LRA=11"),
                ));
            }
        }
        if let Some(lufs) = a.integrated {
            if lufs <= -60.0 {
                out.push(Finding::new(
                    warn,
                    None,
                    "the sound track is silent",
                    "check the mix: a muted track, or the wrong stream mapped",
                ));
            } else if (lufs - TARGET_LUFS).abs() > 2.0 {
                out.push(Finding::new(
                    warn,
                    None,
                    format!(
                        "loudness {lufs:.1} LUFS, {} than the {TARGET_LUFS} LUFS online video is mastered to",
                        if lufs < TARGET_LUFS { "quieter" } else { "louder" }
                    ),
                    format!("normalise with loudnorm=I={TARGET_LUFS}:TP={MAX_PEAK}:LRA=11 (two passes: measure, then apply)"),
                ));
            }
        }
        for s in &a.silent {
            let inner = s.start > 0.3 && s.end < length - 0.3;
            if inner && s.len() >= i.profile.dead_air() && a.integrated.is_some_and(|l| l > -60.0) {
                out.push(Finding::new(
                    warn,
                    Some(s.start),
                    format!(
                        "{:.1} s of silence ({}–{})",
                        s.len(),
                        crate::clock(s.start),
                        crate::clock(s.end)
                    ),
                    "cut the gap, or lay music or room tone under it",
                ));
            }
        }
    }
    if a.measured_video && p.has_video() {
        let black: f64 = a.black.iter().map(Span::len).sum();
        if length > 0.0 && black > length * 0.5 {
            out.push(Finding::new(
                fail,
                None,
                format!("{:.0}% of the video is black", 100.0 * black / length),
                "check the sources and the overlay order: a layer is covering the picture",
            ));
        } else {
            for s in &a.black {
                let edge = (s.start < 0.1 || s.end > length - 0.1) && s.len() <= 1.0;
                if !edge {
                    out.push(Finding::new(
                        warn,
                        Some(s.start),
                        format!("{:.1} s of black at {}", s.len(), crate::clock(s.start)),
                        "a gap between clips or a transition into nothing: close it, or make it a deliberate fade",
                    ));
                }
            }
        }
        if let Some(longest) = i.profile.longest_still() {
            for s in &a.frozen {
                if s.len() >= longest && s.len() < length - 0.5 {
                    out.push(Finding::new(
                        warn,
                        Some(s.start),
                        format!(
                            "the picture doesn't move for {:.1} s ({}–{})",
                            s.len(),
                            crate::clock(s.start),
                            crate::clock(s.end)
                        ),
                        "a stuck frame or a long hold: cut sooner, or add movement (a slow zoom, a cutaway)",
                    ));
                }
            }
        }
        // Heuristic (M69): a short whose picture holds 8 s or more without a cut.
        if i.profile == Profile::Short {
            if let Some((start, len)) = shots(&a.cuts, length)
                .into_iter()
                .max_by(|x, y| x.1.total_cmp(&y.1))
            {
                if len >= 8.0 && length > len + 0.5 {
                    out.push(Finding::new(
                        warn,
                        Some(start),
                        format!("one shot holds {len:.1} s from {}", crate::clock(start)),
                        "short clips usually change picture every few seconds: cut in a closer shot or B-roll",
                    ));
                }
            }
        }
    }
    sort(&mut out);
    out
}

/// Each shot's start and length, from the cut times.
pub fn shots(cuts: &[f64], length: f64) -> Vec<(f64, f64)> {
    let mut edges = vec![0.0];
    edges.extend(
        cuts.iter()
            .copied()
            .filter(|t| *t > 0.05 && *t < length - 0.05),
    );
    edges.push(length);
    edges.windows(2).map(|w| (w[0], w[1] - w[0])).collect()
}

fn sort(out: &mut [Finding]) {
    out.sort_by(|a, b| {
        (a.level == Level::Warn)
            .cmp(&(b.level == Level::Warn))
            .then(a.at.unwrap_or(-1.0).total_cmp(&b.at.unwrap_or(-1.0)))
    });
}

/// Caption timing: each cue on screen long enough to read, not faster than
/// can be read, lines short enough, no two at once, none past the end.
/// `text` is SRT, WebVTT or ASS.
pub fn captions(text: &str, profile: Profile, length: f64) -> Result<Vec<Finding>, String> {
    let cues = if text.contains("[Events]") {
        ass(text)
    } else {
        transcript::parse(text).unwrap_or_default()
    };
    if cues.is_empty() {
        return Err("no timed captions found in it (SRT, WebVTT or ASS)".into());
    }
    let social = matches!(profile, Profile::Short | Profile::Promo);
    // Netflix's subtitle rules for slides and long edits; word-by-word
    // social captions are allowed shorter cues (0.35 s).
    let min_secs = if social { 0.35 } else { 5.0 / 6.0 };
    let mut out = Vec::new();
    let warn = Level::Warn;
    for (k, c) in cues.iter().enumerate() {
        let words = c.text.split_whitespace().count();
        if words == 0 {
            continue;
        }
        let secs = c.end - c.start;
        let chars = c.text.chars().filter(|ch| !ch.is_whitespace()).count() as f64;
        let quote = c.text.chars().take(40).collect::<String>();
        if secs < min_secs {
            out.push(Finding::new(
                warn,
                Some(c.start),
                format!("\"{quote}\" is on screen {secs:.2} s, under {min_secs:.2} s"),
                "hold it longer, or merge it with the next caption",
            ));
        } else if (!social || words > 3) && chars / secs > 20.0 {
            out.push(Finding::new(
                warn,
                Some(c.start),
                format!(
                    "\"{quote}\" needs {:.0} characters a second to read; 20 is the most",
                    chars / secs
                ),
                "shorten the line or keep it up longer",
            ));
        }
        for line in c.text.split(['\n', '|']) {
            if line.chars().count() > 42 {
                out.push(Finding::new(
                    warn,
                    Some(c.start),
                    format!(
                        "a caption line is {} characters; 42 is the most",
                        line.chars().count()
                    ),
                    "break it into two lines or two captions",
                ));
            }
        }
        if let Some(next) = cues.get(k + 1) {
            if next.start < c.end - 0.01 {
                out.push(Finding::new(
                    warn,
                    Some(next.start),
                    format!("two captions overlap at {}", crate::clock(next.start)),
                    "end each caption before the next one starts",
                ));
            }
        }
        if length > 0.0 && c.start >= length {
            out.push(Finding::new(
                warn,
                Some(c.start),
                format!(
                    "a caption starts at {}, after the video ends",
                    crate::clock(c.start)
                ),
                "retime the captions to the edited video",
            ));
        }
    }
    sort(&mut out);
    Ok(out)
}

/// ASS `Dialogue:` lines as cues (start, end and text; style tags out).
fn ass(text: &str) -> Vec<transcript::Segment> {
    let mut out: Vec<transcript::Segment> = text
        .lines()
        .filter_map(|l| l.strip_prefix("Dialogue:"))
        .filter_map(|rest| {
            let f: Vec<&str> = rest.splitn(10, ',').collect();
            let (start, end) = (transcript::stamp(f.get(1)?)?, transcript::stamp(f.get(2)?)?);
            let mut t = String::new();
            let mut tag = false;
            for ch in f.get(9)?.chars() {
                match ch {
                    '{' => tag = true,
                    '}' => tag = false,
                    _ if !tag => t.push(ch),
                    _ => {}
                }
            }
            Some(transcript::Segment {
                start,
                end,
                text: t.replace("\\N", "\n").replace("\\n", "\n"),
            })
        })
        .collect();
    out.sort_by(|a, b| a.start.total_cmp(&b.start));
    out
}

/// The report the tool returns: the verdict line, each finding, then what
/// was measured and what wasn't.
pub fn report(name: &str, i: &Input, found: &[Finding], caption_note: Option<&str>) -> String {
    let fails = found.iter().filter(|f| f.level == Level::Fail).count();
    let warns = found.len() - fails;
    let mut s = format!(
        "check_video {name} (profile {}): {}.",
        i.profile.name(),
        match (fails, warns) {
            (0, 0) => "nothing found".to_string(),
            (f, w) => format!("{f} FAIL, {w} WARN"),
        }
    );
    for f in found {
        let level = if f.level == Level::Fail {
            "FAIL"
        } else {
            "WARN"
        };
        let at =
            f.at.map(|t| format!(" {}", crate::clock(t)))
                .unwrap_or_default();
        s.push_str(&format!("\n{level}{at}: {}. Fix: {}.", f.what, f.fix));
    }
    let p = i.probe;
    let mut m = vec![crate::clock(p.seconds)];
    if p.has_video() {
        m.push(format!(
            "{}×{}{}",
            p.width,
            p.height,
            p.fps
                .map(|f| format!(" at {} fps", (f * 100.0).round() / 100.0))
                .unwrap_or_default()
        ));
        m.push(format!(
            "{} {}",
            p.video_codec.as_deref().unwrap_or("?"),
            p.pix_fmt.as_deref().unwrap_or("")
        ));
    }
    m.push(p.audio_codec.clone().unwrap_or_else(|| "no sound".into()));
    m.push(match i.send_cap {
        Some(cap) => format!("{} (this chat sends up to {})", human(p.size), human(cap)),
        None => human(p.size),
    });
    if let Some(fs) = i.faststart {
        m.push(format!("faststart {}", if fs { "yes" } else { "no" }));
    }
    let mut not = Vec::new();
    match &i.analysis {
        Ok(a) => {
            if a.measured_audio {
                let num = |x: Option<f64>, unit: &str| {
                    x.map_or_else(|| "?".to_string(), |v| format!("{v:.1} {unit}"))
                };
                m.push(format!(
                    "loudness {}, true peak {}, range {}",
                    num(a.integrated, "LUFS"),
                    num(a.true_peak, "dBTP"),
                    num(a.lra, "LU")
                ));
                if a.integrated.is_none() {
                    not.push("loudness (ebur128 printed no summary)");
                }
            }
            if a.measured_video && p.has_video() {
                let shots = shots(&a.cuts, p.seconds);
                let mut lens: Vec<f64> = shots.iter().map(|s| s.1).collect();
                lens.sort_by(f64::total_cmp);
                let median = lens.get(lens.len() / 2).copied().unwrap_or(0.0);
                let longest = shots.iter().max_by(|x, y| x.1.total_cmp(&y.1));
                m.push(format!(
                    "{} cuts ({:.0} a minute), shots median {median:.1} s{}",
                    a.cuts.len(),
                    a.cuts.len() as f64 * 60.0 / p.seconds.max(1.0),
                    longest
                        .map(|(t, l)| format!(", longest {l:.1} s at {}", crate::clock(*t)))
                        .unwrap_or_default()
                ));
                if !a.cuts.is_empty() {
                    let at: Vec<String> =
                        a.cuts.iter().take(40).map(|t| crate::clock(*t)).collect();
                    m.push(format!("cuts at {}", at.join(", ")));
                }
                let spans = |l: &[Span]| {
                    if l.is_empty() {
                        "none".to_string()
                    } else {
                        l.iter()
                            .take(10)
                            .map(|s| format!("{}–{}", crate::clock(s.start), crate::clock(s.end)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                };
                m.push(format!("black: {}", spans(&a.black)));
                m.push(format!("frozen 1 s or more: {}", spans(&a.frozen)));
            }
            if a.measured_audio {
                let quiet: Vec<Span> = a
                    .silent
                    .iter()
                    .filter(|s| s.len() >= 0.5)
                    .copied()
                    .collect();
                m.push(format!(
                    "silence 0.5 s or more: {}",
                    if quiet.is_empty() {
                        "none".to_string()
                    } else {
                        quiet
                            .iter()
                            .take(10)
                            .map(|s| format!("{}–{}", crate::clock(s.start), crate::clock(s.end)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                ));
            }
        }
        Err(why) => {
            s.push_str(&format!(
                "\nNot measured: loudness, peaks, silence, black and frozen picture, cuts ({why})."
            ));
        }
    }
    if i.faststart.is_none() && p.container.contains("mp4") {
        not.push("faststart (the file's boxes couldn't be read)");
    }
    if i.send_cap.is_none() {
        not.push("whether it fits a chat (no send limit known here)");
    }
    s.push_str(&format!("\nMeasured: {}.", m.join("; ")));
    if let Some(c) = caption_note {
        s.push_str(&format!("\nCaptions: {c}"));
    }
    if !not.is_empty() {
        s.push_str(&format!("\nNot measured: {}.", not.join("; ")));
    }
    s
}

/// `4.2 MB`, `830 KB`.
pub fn human(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} KB", b / 1e3)
    } else {
        format!("{bytes} B")
    }
}
