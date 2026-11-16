//! M69, video craft (`docs/m69-video-craft.md`). The parts of making video
//! well that are code rather than taste: what a render measures, which of
//! those numbers are broken (FAIL) or doubtful (WARN) for the kind of video
//! it is, whether its captions can be read, when each line was said, and
//! timecodes burned onto frames without a font. Nothing here runs a
//! process; the gateway's tools run ffmpeg in the sandbox with the
//! arguments built here and pass back what it printed. Kept apart from the
//! gateway so the same checks can serve other agents later (M69 phase 5).

pub mod check;
pub mod detect;
pub mod labels;
pub mod transcript;

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
