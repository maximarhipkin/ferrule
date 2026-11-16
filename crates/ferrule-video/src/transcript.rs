//! When each line was said. Transcribers answer in different shapes: the
//! OpenAI API's `verbose_json` (`segments`), whisper.cpp's `-oj`
//! (`transcription[].offsets` in milliseconds), a bare JSON list of
//! `{start, end, text}`, or subtitle files (SRT, WebVTT). [`parse`] reads
//! any of them into [`Segment`]s; anything else is plain text, which the
//! caller keeps as it was.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One stretch of speech, in seconds from the start of what was heard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// The segments in `s`, when it is one of the shapes above; `None` for
/// plain text (or a shape with no timings).
pub fn parse(s: &str) -> Option<Vec<Segment>> {
    let t = s.trim_start_matches('\u{feff}').trim();
    let found = if t.starts_with('{') || t.starts_with('[') {
        serde_json::from_str::<Value>(t).ok().and_then(|v| json(&v))
    } else if t.starts_with("WEBVTT") || t.contains("-->") {
        Some(cues(t))
    } else {
        None
    };
    found.filter(|segs| !segs.is_empty())
}

/// The text alone, segments joined with spaces.
pub fn text(segs: &[Segment]) -> String {
    segs.iter()
        .map(|s| s.text.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// One line per segment, `[0:01.2–0:03.4] words`, each shifted by
/// `offset` seconds (the start of the part that was heard).
pub fn lines(segs: &[Segment], offset: f64) -> String {
    segs.iter()
        .filter(|s| !s.text.trim().is_empty())
        .map(|s| {
            format!(
                "[{}–{}] {}",
                crate::clock(s.start + offset),
                crate::clock(s.end + offset),
                s.text.trim()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn json(v: &Value) -> Option<Vec<Segment>> {
    let num = |v: &Value| v.as_f64().or_else(|| v.as_str()?.trim().parse().ok());
    let list = v
        .get("segments")
        .or_else(|| v.get("transcription"))
        .or_else(|| v.get("chunks"))
        .unwrap_or(v)
        .as_array()?;
    let mut out = Vec::new();
    for seg in list {
        let text = seg["text"].as_str().unwrap_or_default().to_string();
        let (start, end) = if let (Some(a), Some(b)) = (num(&seg["start"]), num(&seg["end"])) {
            (a, b)
        } else if let (Some(a), Some(b)) =
            (num(&seg["offsets"]["from"]), num(&seg["offsets"]["to"]))
        {
            (a / 1000.0, b / 1000.0)
        } else if let Some([a, b]) = seg["timestamp"].as_array().map(Vec::as_slice) {
            match (num(a), num(b)) {
                (Some(a), Some(b)) => (a, b),
                _ => continue,
            }
        } else {
            continue;
        };
        out.push(Segment { start, end, text });
    }
    Some(out)
}

/// SRT or WebVTT cues.
fn cues(s: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut lines = s.lines().peekable();
    while let Some(line) = lines.next() {
        let Some((a, b)) = line.split_once("-->") else {
            continue;
        };
        let (Some(start), Some(end)) = (stamp(a), stamp(b.split_whitespace().next().unwrap_or("")))
        else {
            continue;
        };
        let mut text = Vec::new();
        while let Some(l) = lines.peek() {
            if l.trim().is_empty() {
                break;
            }
            text.push(lines.next().unwrap_or_default().trim().to_string());
        }
        out.push(Segment {
            start,
            end,
            text: text.join(" "),
        });
    }
    out
}

/// `00:01:02,500`, `01:02.500` or `1:02:03.5` as seconds.
pub fn stamp(s: &str) -> Option<f64> {
    let s = s.trim().replace(',', ".");
    if s.is_empty() {
        return None;
    }
    let mut total = 0.0;
    for part in s.split(':') {
        total = total * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(total)
}
