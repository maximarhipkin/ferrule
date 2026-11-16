//! M69 phase 3: an edit list (EDL) the model writes as JSON, checked and
//! compiled into ffmpeg steps the gateway runs one by one. The model says
//! what goes where; the order of operations that keeps an edit clean is
//! fixed here, after browser-use/video-use's rules:
//!
//! 1. each segment cut on its own, brought to one size, frame rate and
//!    sample rate, its sound faded 30 ms at both ends (no clicks), silence
//!    added where it has none;
//! 2. the segments joined without re-encoding (concat demuxer);
//! 3. voice and music mixed in (music ducked under the voice, or under
//!    the clips' own sound when there's no voice), fades, captions burned
//!    last so their times are the output's;
//! 4. loudness measured, then mastered to -14 LUFS / -1 dBTP in a second
//!    pass, faststart;
//! 5. re-encoded to a bit rate that fits when the result is over the size
//!    it must fit.
//!
//! Every path handed in is already resolved inside the workspace by the
//! caller; paths that go into a filter string must be [`filter_safe`].

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::Path;

use crate::check::{MAX_PEAK, TARGET_LUFS};

/// The sound every segment is brought to.
const RATE: u32 = 48_000;
/// The fade at both ends of each segment's sound.
const CLICK: f64 = 0.03;
/// The audio bit rate of every step, kb/s.
pub const AUDIO_KBPS: u64 = 192;
const MAX_SEGMENTS: usize = 300;
const MAX_TOTAL: f64 = 3.0 * 3600.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edl {
    /// Where the video goes, a `.mp4` in the workspace.
    pub output: String,
    /// Width and height; 1080×1920 when omitted.
    #[serde(default = "default_size")]
    pub size: [u32; 2],
    #[serde(default = "default_fps")]
    pub fps: u32,
    pub segments: Vec<Segment>,
    /// An SRT or ASS file timed to the output, burned in last.
    #[serde(default)]
    pub captions: Option<String>,
    /// A folder of fonts for the captions (no fontconfig needed).
    #[serde(default)]
    pub fonts: Option<String>,
    #[serde(default)]
    pub voice: Option<Voice>,
    #[serde(default)]
    pub music: Option<Music>,
    /// The clips' own sound level, 1 when omitted (0 mutes them all).
    #[serde(default = "one")]
    pub source_volume: f64,
    /// Seconds of fade from black at the start and to black at the end.
    #[serde(default)]
    pub fade_in: f64,
    #[serde(default)]
    pub fade_out: f64,
    /// The loudness to master to, LUFS: -14 (online video) when omitted;
    /// lower is quieter overall (-18 for a soft music-only piece).
    #[serde(default)]
    pub loudness: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Segment {
    /// A video or an image in the workspace.
    pub source: String,
    /// Seconds into the source; 0 when omitted.
    #[serde(default)]
    pub start: f64,
    /// Seconds into the source; its end when omitted. For an image, how
    /// long it shows (from `start`, normally 0).
    #[serde(default)]
    pub end: Option<f64>,
    #[serde(default)]
    pub fit: Fit,
    /// A fixed zoom into the centre, 1 to 3 (1.1 hides a jump cut).
    #[serde(default)]
    pub zoom: Option<f64>,
    /// A slow push in over the segment, as the extra zoom reached at its
    /// end (0.05 is gentle), 0 to 0.5.
    #[serde(default)]
    pub push: Option<f64>,
    /// Leave this segment's own sound out.
    #[serde(default)]
    pub mute: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fit {
    /// Fill the frame, cropping what's outside (centred).
    #[default]
    Crop,
    /// The whole picture, black bars where it doesn't fill.
    Pad,
    /// The whole picture over a blurred copy of itself.
    Blur,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Voice {
    pub file: String,
    /// Where in the output it starts, seconds.
    #[serde(default)]
    pub at: f64,
    #[serde(default = "one")]
    pub volume: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Music {
    pub file: String,
    /// 0.3 when omitted: a bed, not the lead.
    #[serde(default = "music_volume")]
    pub volume: f64,
    /// Lower it under the voice (or the clips' sound); true when omitted.
    #[serde(default = "yes")]
    pub duck: bool,
}

fn default_size() -> [u32; 2] {
    [1080, 1920]
}
fn default_fps() -> u32 {
    30
}
fn one() -> f64 {
    1.0
}
fn music_volume() -> f64 {
    0.3
}
fn yes() -> bool {
    true
}

/// What the caller learned about a source with ffprobe.
#[derive(Debug, Clone, Copy)]
pub struct Source {
    pub seconds: f64,
    pub audio: bool,
    pub image: bool,
}

/// Whether `name` is a still image by its extension.
pub fn is_image(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".webp", ".bmp"]
        .iter()
        .any(|e| lower.ends_with(e))
}

/// Whether a relative path can go into a filter string as it is: letters,
/// digits and `._-/` only, no `..`.
pub fn filter_safe(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.contains("..")
        && !rel.starts_with('/')
        && rel
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

impl Edl {
    /// Each segment's length, or every problem found. `sources[i]` is
    /// segment `i`'s source.
    pub fn lengths(&self, sources: &[Source]) -> Result<Vec<f64>, Vec<String>> {
        let mut bad = Vec::new();
        let [w, h] = self.size;
        if !self.output.to_ascii_lowercase().ends_with(".mp4") {
            bad.push("`output` must be an .mp4 file".to_string());
        }
        if w % 2 == 1 || h % 2 == 1 || !(64..=4096).contains(&w) || !(64..=4096).contains(&h) {
            bad.push(format!(
                "`size` {w}×{h}: both even, 64 to 4096 (1080×1920 vertical, 1920×1080 landscape)"
            ));
        }
        if !(1..=60).contains(&self.fps) {
            bad.push(format!("`fps` {}: 1 to 60", self.fps));
        }
        if self.segments.is_empty() || self.segments.len() > MAX_SEGMENTS {
            bad.push(format!("1 to {MAX_SEGMENTS} segments"));
        }
        if self.loudness.is_some_and(|l| !(-30.0..=-9.0).contains(&l)) {
            bad.push("`loudness`: -30 to -9 LUFS (-14 when omitted)".into());
        }
        if !(0.0..=4.0).contains(&self.source_volume) {
            bad.push("`source_volume`: 0 to 4".into());
        }
        let mut lens = Vec::new();
        for (i, (s, src)) in self.segments.iter().zip(sources).enumerate() {
            let n = i + 1;
            let end = match (s.end, src.image) {
                (Some(e), _) => e,
                (None, true) => {
                    bad.push(format!(
                        "segment {n} ({}) is an image: give `end`, how long it shows",
                        s.source
                    ));
                    continue;
                }
                (None, false) => src.seconds,
            };
            if s.start < 0.0 || end <= s.start {
                bad.push(format!(
                    "segment {n}: start {} must be before end {end}",
                    s.start
                ));
                continue;
            }
            if !src.image && end > src.seconds + 0.05 {
                bad.push(format!(
                    "segment {n}: end {end} is past the end of {} ({:.2} s)",
                    s.source, src.seconds
                ));
            }
            if end - s.start < 0.1 {
                bad.push(format!("segment {n} is under 0.1 s"));
            }
            if s.zoom.is_some_and(|z| !(1.0..=3.0).contains(&z)) {
                bad.push(format!("segment {n}: `zoom` 1 to 3"));
            }
            if s.push.is_some_and(|p| !(0.0..=0.5).contains(&p)) {
                bad.push(format!("segment {n}: `push` 0 to 0.5"));
            }
            lens.push(end.min(if src.image { end } else { src.seconds }) - s.start);
        }
        let total: f64 = lens.iter().sum();
        if total > MAX_TOTAL {
            bad.push("the edit is over three hours".into());
        }
        if self.fade_in < 0.0 || self.fade_out < 0.0 || self.fade_in + self.fade_out > total {
            bad.push(
                "`fade_in` and `fade_out` are seconds, together no longer than the edit".into(),
            );
        }
        if let Some(v) = &self.voice {
            if v.at < 0.0 || !(0.0..=4.0).contains(&v.volume) {
                bad.push("`voice`: `at` 0 or more, `volume` 0 to 4".into());
            }
        }
        if let Some(m) = &self.music {
            if !(0.0..=2.0).contains(&m.volume) {
                bad.push("`music.volume`: 0 to 2".into());
            }
        }
        if bad.is_empty() {
            Ok(lens)
        } else {
            Err(bad)
        }
    }

    /// The loudness this edit is mastered to, LUFS.
    pub fn target(&self) -> f64 {
        self.loudness.unwrap_or(TARGET_LUFS)
    }

    fn encode(&self) -> Vec<OsString> {
        os(&[
            "-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p",
        ])
    }

    /// Segment `i`, `len` seconds of `src` (resolved), into `part`.
    pub fn segment(
        &self,
        i: usize,
        src: &Path,
        facts: Source,
        len: f64,
        part: &Path,
    ) -> Vec<OsString> {
        let s = &self.segments[i];
        let ([w, h], f) = (self.size, self.fps);
        let mut a = head();
        if facts.image {
            a.extend(os(&["-loop", "1", "-framerate"]));
            a.push(f.to_string().into());
            a.extend(os(&["-t"]));
            a.push(format!("{len:.3}").into());
        } else {
            a.extend(os(&["-ss"]));
            a.push(format!("{:.3}", s.start).into());
            a.extend(os(&["-t"]));
            a.push(format!("{len:.3}").into());
        }
        a.extend(os(&["-protocol_whitelist", "file", "-i"]));
        a.push(src.as_os_str().to_owned());
        let own_sound = facts.audio && !s.mute && !facts.image && self.source_volume > 0.0;
        if !own_sound {
            a.extend(os(&["-f", "lavfi", "-t"]));
            a.push(format!("{len:.3}").into());
            a.extend(os(&["-i"]));
            a.push(format!("anullsrc=r={RATE}:cl=stereo").into());
        }
        let fit = match s.fit {
            Fit::Crop => format!(
                "[0:v]scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h}"
            ),
            Fit::Pad => format!(
                "[0:v]scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black"
            ),
            Fit::Blur => format!(
                "[0:v]split[bg0][fg0];[bg0]scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h},boxblur=20:2[bg];[fg0]scale={w}:{h}:force_original_aspect_ratio=decrease[fg];[bg][fg]overlay=(W-w)/2:(H-h)/2"
            ),
        };
        let mut v = fit;
        if let Some(z) = s.zoom.filter(|z| *z > 1.0) {
            v.push_str(&format!(",scale=iw*{z}:ih*{z},crop={w}:{h}"));
        }
        if let Some(p) = s.push.filter(|p| *p > 0.0) {
            let frames = (len * f as f64).max(1.0).round();
            v.push_str(&format!(
                ",zoompan=z='1+{p}*on/{frames}':x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':d=1:s={w}x{h}:fps={f}"
            ));
        }
        v.push_str(&format!(",fps={f},format=yuv420p,setsar=1[v]"));
        let src_a = if own_sound { "[0:a:0]" } else { "[1:a]" };
        let vol = if own_sound && (self.source_volume - 1.0).abs() > 1e-6 {
            format!("volume={},", self.source_volume)
        } else {
            String::new()
        };
        let audio = format!(
            "{src_a}{vol}aresample={RATE},aformat=sample_fmts=fltp:channel_layouts=stereo,afade=t=in:d={CLICK},afade=t=out:st={:.3}:d={CLICK}[a]",
            (len - CLICK).max(0.0)
        );
        a.extend(os(&["-filter_complex"]));
        a.push(format!("{v};{audio}").into());
        a.extend(os(&["-map", "[v]", "-map", "[a]", "-t"]));
        a.push(format!("{len:.3}").into());
        a.extend(self.encode());
        a.extend(os(&["-r"]));
        a.push(f.to_string().into());
        a.extend(os(&["-c:a", "aac", "-b:a"]));
        a.push(format!("{AUDIO_KBPS}k").into());
        a.push(part.as_os_str().to_owned());
        a
    }

    /// Step 3: voice, music, fades and captions over the joined segments.
    /// `captions` and `fonts` are workspace-relative and [`filter_safe`].
    #[allow(clippy::too_many_arguments)]
    pub fn mix(
        &self,
        joined: &Path,
        voice: Option<&Path>,
        music: Option<&Path>,
        captions: Option<&str>,
        fonts: Option<&str>,
        total: f64,
        out: &Path,
    ) -> Vec<OsString> {
        let mut a = head();
        a.extend(os(&["-protocol_whitelist", "file", "-i"]));
        a.push(joined.as_os_str().to_owned());
        let mut next = 1;
        let voice_at = voice.map(|p| {
            a.extend(os(&["-protocol_whitelist", "file", "-i"]));
            a.push(p.as_os_str().to_owned());
            next += 1;
            next - 1
        });
        let music_at = music.map(|p| {
            a.extend(os(&[
                "-stream_loop",
                "-1",
                "-protocol_whitelist",
                "file",
                "-i",
            ]));
            a.push(p.as_os_str().to_owned());
            next += 1;
            next - 1
        });
        // The sound.
        let mut g = String::from("[0:a]anull[base]");
        let mut mix = vec!["[base]".to_string()];
        if let (Some(i), Some(v)) = (voice_at, &self.voice) {
            let ms = (v.at * 1000.0).round() as u64;
            g.push_str(&format!(
                ";[{i}:a]aresample={RATE},aformat=channel_layouts=stereo,volume={},adelay={ms}|{ms}[vo]",
                v.volume
            ));
        }
        if let (Some(i), Some(m)) = (music_at, &self.music) {
            let fade_at = (total - 1.5).max(0.0);
            g.push_str(&format!(
                ";[{i}:a]aresample={RATE},aformat=channel_layouts=stereo,volume={},atrim=0:{total:.3},afade=t=out:st={fade_at:.3}:d=1.5[mus]",
                m.volume
            ));
            if m.duck {
                // Keyed on the voice, else on the clips' own sound.
                let key = if voice_at.is_some() { "vo" } else { "base" };
                g.push_str(&format!(";[{key}]asplit[{key}x][key]"));
                if key == "vo" {
                    g.push_str(";[mus][key]sidechaincompress=threshold=0.03:ratio=8:attack=20:release=400[duck]");
                    mix = vec!["[base]".into(), "[vox]".into(), "[duck]".into()];
                } else {
                    g.push_str(";[mus][key]sidechaincompress=threshold=0.03:ratio=8:attack=20:release=400[duck]");
                    mix = vec!["[basex]".into(), "[duck]".into()];
                }
            } else {
                if voice_at.is_some() {
                    mix.push("[vo]".into());
                }
                mix.push("[mus]".into());
            }
        } else if voice_at.is_some() {
            mix.push("[vo]".into());
        }
        let mut tail = String::new();
        if self.fade_in > 0.0 {
            tail.push_str(&format!(",afade=t=in:d={}", self.fade_in));
        }
        if self.fade_out > 0.0 {
            tail.push_str(&format!(
                ",afade=t=out:st={:.3}:d={}",
                total - self.fade_out,
                self.fade_out
            ));
        }
        g.push_str(&format!(
            ";{}amix=inputs={}:duration=first:normalize=0{tail}[a]",
            mix.join(""),
            mix.len()
        ));
        // The picture: re-encoded only when it changes.
        let mut v = Vec::new();
        if self.fade_in > 0.0 {
            v.push(format!("fade=t=in:st=0:d={}", self.fade_in));
        }
        if self.fade_out > 0.0 {
            v.push(format!(
                "fade=t=out:st={:.3}:d={}",
                total - self.fade_out,
                self.fade_out
            ));
        }
        if let Some(c) = captions {
            let mut sub = format!("subtitles=filename={c}");
            if let Some(f) = fonts {
                sub.push_str(&format!(":fontsdir={f}"));
            }
            v.push(sub);
        }
        let picture = !v.is_empty();
        if picture {
            g.push_str(&format!(";[0:v]{}[v]", v.join(",")));
        }
        a.extend(os(&["-filter_complex"]));
        a.push(g.into());
        a.extend(os(&[
            "-map",
            if picture { "[v]" } else { "0:v" },
            "-map",
            "[a]",
            "-t",
        ]));
        a.push(format!("{total:.3}").into());
        if picture {
            a.extend(self.encode());
        } else {
            a.extend(os(&["-c:v", "copy"]));
        }
        a.extend(os(&["-c:a", "aac", "-b:a"]));
        a.push(format!("{AUDIO_KBPS}k").into());
        a.push(out.as_os_str().to_owned());
        a
    }
}

/// Step 2's list for the concat demuxer: the parts' file names, which sit
/// beside it.
pub fn concat_list(parts: &[String]) -> String {
    parts.iter().map(|p| format!("file '{p}'\n")).collect()
}

/// Step 2: the parts joined without re-encoding.
pub fn concat(list: &Path, out: &Path) -> Vec<OsString> {
    let mut a = head();
    a.extend(os(&["-f", "concat", "-protocol_whitelist", "file", "-i"]));
    a.push(list.as_os_str().to_owned());
    a.extend(os(&["-c", "copy"]));
    a.push(out.as_os_str().to_owned());
    a
}

/// Step 4a: loudness measured against `target` LUFS (prints JSON on
/// stderr at the info level).
pub fn measure(input: &Path, target: f64) -> Vec<OsString> {
    let mut a = os(&[
        "-hide_banner",
        "-nostats",
        "-nostdin",
        "-loglevel",
        "info",
        "-protocol_whitelist",
        "file",
        "-i",
    ]);
    a.push(input.as_os_str().to_owned());
    a.extend(os(&["-vn", "-af"]));
    a.push(format!("loudnorm=I={target}:TP={MAX_PEAK}:LRA=11:print_format=json").into());
    a.extend(os(&["-f", "null", "-"]));
    a
}

/// What loudnorm measured, for the second pass.
#[derive(Debug, Clone, PartialEq)]
pub struct Measured {
    pub i: String,
    pub tp: String,
    pub lra: String,
    pub thresh: String,
    pub offset: String,
}

/// The JSON loudnorm prints last.
pub fn measured(stderr: &str) -> Option<Measured> {
    let start = stderr.rfind('{')?;
    let end = stderr[start..].find('}')? + start;
    let v: serde_json::Value = serde_json::from_str(&stderr[start..=end]).ok()?;
    let get = |k: &str| {
        v[k].as_str()
            .filter(|s| s.parse::<f64>().is_ok_and(f64::is_finite))
            .map(str::to_string)
    };
    Some(Measured {
        i: get("input_i")?,
        tp: get("input_tp")?,
        lra: get("input_lra")?,
        thresh: get("input_thresh")?,
        offset: get("target_offset")?,
    })
}

/// Step 4b: mastered to `target` LUFS with the measurement, picture
/// copied, faststart.
pub fn master(input: &Path, m: &Measured, target: f64, out: &Path) -> Vec<OsString> {
    let mut a = head();
    a.extend(os(&["-protocol_whitelist", "file", "-i"]));
    a.push(input.as_os_str().to_owned());
    a.extend(os(&["-map", "0:v", "-map", "0:a", "-c:v", "copy", "-af"]));
    a.push(
        format!(
            "loudnorm=I={target}:TP={MAX_PEAK}:LRA=11:measured_I={}:measured_TP={}:measured_LRA={}:measured_thresh={}:offset={}:linear=true,aresample={RATE}",
            m.i, m.tp, m.lra, m.thresh, m.offset
        )
        .into(),
    );
    a.extend(os(&["-c:a", "aac", "-b:a"]));
    a.push(format!("{AUDIO_KBPS}k").into());
    a.extend(os(&["-movflags", "+faststart"]));
    a.push(out.as_os_str().to_owned());
    a
}

/// The video bit rate, kb/s, that makes `seconds` of video fit in
/// `bytes` with the audio and a margin.
pub fn fitting_kbps(bytes: u64, seconds: f64) -> u64 {
    let total = bytes as f64 * 8.0 * 0.92 / seconds.max(1.0) / 1000.0;
    (total as u64).saturating_sub(AUDIO_KBPS).max(150)
}

/// Step 5: the picture re-encoded at `kbps`, the sound copied.
pub fn shrink(input: &Path, kbps: u64, out: &Path) -> Vec<OsString> {
    let mut a = head();
    a.extend(os(&["-protocol_whitelist", "file", "-i"]));
    a.push(input.as_os_str().to_owned());
    a.extend(os(&["-c:v", "libx264", "-preset", "veryfast", "-b:v"]));
    a.push(format!("{kbps}k").into());
    a.extend(os(&["-maxrate"]));
    a.push(format!("{kbps}k").into());
    a.extend(os(&["-bufsize"]));
    a.push(format!("{}k", kbps * 2).into());
    a.extend(os(&[
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "copy",
        "-movflags",
        "+faststart",
    ]));
    a.push(out.as_os_str().to_owned());
    a
}

fn head() -> Vec<OsString> {
    os(&["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
}

fn os(s: &[&str]) -> Vec<OsString> {
    s.iter().map(OsString::from).collect()
}

/// When loudness couldn't be measured: the file as it is, faststart.
pub fn remux(input: &Path, out: &Path) -> Vec<OsString> {
    let mut a = head();
    a.extend(os(&["-protocol_whitelist", "file", "-i"]));
    a.push(input.as_os_str().to_owned());
    a.extend(os(&["-c", "copy", "-movflags", "+faststart"]));
    a.push(out.as_os_str().to_owned());
    a
}
