//! Photos on the wire (M47): which models see them, and how a driver turns
//! a photo's path into pixels or a note. M60: a tool result's images go the
//! same way, inside the result where the API allows it (Anthropic), else in
//! a user message right after the results, marked as tool output.
//!
//! History holds paths. The driver that serves a call loads each photo that
//! is one of the newest few ([`ferrule_core::vision::pixel_set`]), checks it
//! is a real image by its first bytes (not by the name or the claimed type),
//! and caps its size; anything it can't or shouldn't send becomes a note.

use base64::Engine;
use ferrule_core::error::{CoreError, FailureClass};
use ferrule_core::message::{ImageRef, Message};
pub use ferrule_core::vision::sniff;
use ferrule_core::vision::{note_for, NoteWhy};
use std::collections::HashSet;

/// The most a photo may weigh on the wire. The page shrinks photos to well
/// under this before they get here; it's the backstop.
pub const MAX_BYTES: usize = ferrule_core::vision::MAX_IMAGE_BYTES;

/// One photo, as a driver will send it.
#[derive(Debug, PartialEq)]
pub(crate) enum Part {
    Pixels { mime: String, data: String },
    Note(String),
}

/// What the parts of `msg` (the `idx`-th of its request) are. `sees` says
/// the model can take pixels; `pixels` is [`ferrule_core::vision::pixel_set`].
pub(crate) fn parts(
    idx: usize,
    msg: &Message,
    pixels: &HashSet<(usize, usize)>,
    sees: bool,
) -> Vec<Part> {
    msg.images
        .iter()
        .enumerate()
        .map(|(j, img)| {
            if !sees {
                Part::Note(note_for(msg, img, NoteWhy::TextOnly))
            } else if !pixels.contains(&(idx, j)) {
                Part::Note(note_for(msg, img, NoteWhy::Older))
            } else {
                match load(img) {
                    Ok((mime, data)) => Part::Pixels { mime, data },
                    Err(why) => Part::Note(note_for(msg, img, why)),
                }
            }
        })
        .collect()
}

/// M60: a tool result's words with a note for each of its images that
/// isn't going as pixels, and the (mime, base64) of those that are.
pub(crate) fn tool_parts(
    idx: usize,
    msg: &Message,
    pixels: &HashSet<(usize, usize)>,
    sees: bool,
) -> (String, Vec<(String, String)>) {
    let mut text = msg.content.clone().unwrap_or_default();
    let mut pics = Vec::new();
    for part in parts(idx, msg, pixels, sees) {
        match part {
            Part::Pixels { mime, data } => pics.push((mime, data)),
            Part::Note(n) => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&n);
            }
        }
    }
    (text, pics)
}

/// M60: what heads the user message that carries tool results' images to
/// an API whose tool results take only text (Chat Completions,
/// Responses): it says they are the tools' output, not the person's words.
pub(crate) fn tool_images_lead(calls: &[String]) -> String {
    format!(
        "[Tool output, not a message from the user: the images the tool call(s) {} returned, in order.]",
        calls.join(", ")
    )
}

/// Whether a request would carry any photo as pixels (a file that has gone
/// missing counts: the retry it might cause is harmless).
pub(crate) fn has_pixels(msgs: &[Message], sees: bool) -> bool {
    sees && !ferrule_core::vision::pixel_set(msgs).is_empty()
}

/// The photo's (mime, base64), or why it can't go: gone, too big, or not an
/// image after all.
pub(crate) fn load(img: &ImageRef) -> Result<(String, String), NoteWhy> {
    use std::io::Read;
    let file = std::fs::File::open(&img.path).map_err(|_| NoteWhy::Missing)?;
    let mut bytes = Vec::new();
    // One byte past the cap says "too big" without reading a huge file.
    file.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| NoteWhy::Missing)?;
    if bytes.len() > MAX_BYTES {
        return Err(NoteWhy::Missing);
    }
    let mime = sniff(&bytes).ok_or(NoteWhy::Missing)?;
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok((mime.to_string(), data))
}

/// A 400 whose message is about the photo: a server that lists `image_url`
/// as unknown, a model that "does not support vision". The request is then
/// tried once more with notes.
pub(crate) fn image_rejected(error: &CoreError) -> bool {
    if error.class() != FailureClass::BadRequest {
        return false;
    }
    let text = error.to_string().to_ascii_lowercase();
    ["image", "vision", "multimodal", "multi-modal", "modalit"]
        .iter()
        .any(|w| text.contains(w))
}

/// Whether a model of this name can see images. The config's `vision` key
/// overrides the answer either way; this is the guess when it isn't set.
/// Judged on the part after the last `/` (`openrouter/anthropic/claude-…`).
pub fn by_name(model: &str) -> bool {
    let full = model.to_ascii_lowercase();
    let m = full.rsplit('/').next().unwrap_or(&full);
    let starts = |ps: &[&str]| ps.iter().any(|p| m.starts_with(p));
    // The exceptions to a family that mostly sees.
    if starts(&[
        "o1-mini",
        "o3-mini",
        "gpt-3.5",
        "deepseek",
        "kimi-k2",
        "gemma3:1b",
        "gemma3-1b",
    ]) {
        return false;
    }
    if m.contains("-vl") || m.contains("vision") || m.ends_with("-v") || m.contains("-vision") {
        return true;
    }
    if m == "o1" || m == "o3" || starts(&["o1-", "o3-", "o4-mini"]) {
        return true;
    }
    if starts(&[
        "claude-3",
        "claude-opus",
        "claude-sonnet",
        "claude-haiku",
        "claude-fable",
        "claude-4",
        "gpt-4o",
        "gpt-4.1",
        "gpt-4-turbo",
        "gpt-5",
        "gemini",
        "grok-4",
        "llava",
        "bakllava",
        "moondream",
        "minicpm-v",
        "llama3.2-vision",
        "llama-4",
        "pixtral",
        "mistral-medium-3",
        "gemma3",
    ]) {
        return true;
    }
    // claude-<n>-<size>: claude-4-…, claude-opus-4-…, all covered above;
    // mistral-small-3.1 and newer.
    if let Some(rest) = m.strip_prefix("mistral-small-3.") {
        return rest
            .chars()
            .next()
            .is_some_and(|c| ('1'..='9').contains(&c));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_that_see_and_names_that_dont() {
        for yes in [
            "claude-sonnet-4-5",
            "claude-3-5-haiku-latest",
            "claude-opus-5-5",
            "claude-fable-5-1",
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-4.1-nano",
            "gpt-5.2-codex",
            "o1",
            "o3",
            "o4-mini",
            "gemini-2.5-flash",
            "grok-4-fast",
            "grok-2-vision-1212",
            "llava:13b",
            "moondream",
            "minicpm-v:8b",
            "llama3.2-vision:11b",
            "llama-4-scout",
            "qwen2.5-vl-72b",
            "kimi-vl-a3b",
            "pixtral-large",
            "mistral-small-3.2",
            "mistral-medium-3",
            "gemma3:12b",
            "openrouter/anthropic/claude-sonnet-4-5",
        ] {
            assert!(by_name(yes), "{yes} should see images");
        }
        for no in [
            "kimi-k2.6",
            "kimi-k2-thinking",
            "deepseek-chat",
            "deepseek-reasoner",
            "gpt-3.5-turbo",
            "o1-mini",
            "o3-mini",
            "gemma3:1b",
            "llama3.1:8b",
            "qwen3-coder",
            "mistral-small-2409",
            "mock",
            "",
        ] {
            assert!(!by_name(no), "{no} should not");
        }
    }

    #[test]
    fn a_photo_is_what_its_bytes_say_and_never_svg() {
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0, 0]), Some("image/jpeg"));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(sniff(b"GIF89a.."), Some("image/gif"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"<svg xmlns='http://www.w3.org/2000/svg'/>"), None);
        assert_eq!(sniff(b"<?xml version='1.0'?><svg/>"), None);
        assert_eq!(sniff(b"MZ\x90\0"), None);
        assert_eq!(sniff(b""), None);
    }

    fn write(dir: &std::path::Path, name: &str, bytes: &[u8]) -> ImageRef {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        ImageRef {
            path: p.to_string_lossy().into_owned(),
            mime: "image/jpeg".into(),
            name: Some(name.into()),
        }
    }

    #[test]
    fn load_checks_bytes_size_and_existence() {
        let dir = tempfile::tempdir().unwrap();
        let ok = write(dir.path(), "a.jpg", &[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3]);
        let (mime, data) = load(&ok).unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(data, "/9j/4AECAw==");
        // Named .jpg and claiming jpeg, but it is a script: refused.
        let fake = write(dir.path(), "b.jpg", b"#!/bin/sh\necho hi\n");
        assert_eq!(load(&fake), Err(NoteWhy::Missing));
        let mut big = vec![0xFF, 0xD8, 0xFF];
        big.resize(MAX_BYTES + 1, 0);
        assert_eq!(
            load(&write(dir.path(), "c.jpg", &big)),
            Err(NoteWhy::Missing)
        );
        let mut edge = vec![0xFF, 0xD8, 0xFF];
        edge.resize(MAX_BYTES, 0);
        assert!(load(&write(dir.path(), "d.jpg", &edge)).is_ok());
        let gone = ImageRef {
            path: dir.path().join("nope.jpg").to_string_lossy().into_owned(),
            mime: "image/jpeg".into(),
            name: None,
        };
        assert_eq!(load(&gone), Err(NoteWhy::Missing));
    }

    #[test]
    fn parts_send_pixels_only_to_a_model_that_sees_and_only_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let imgs: Vec<ImageRef> = (0..5)
            .map(|i| write(dir.path(), &format!("{i}.jpg"), &[0xFF, 0xD8, 0xFF, i]))
            .collect();
        let msgs = vec![
            Message::user_with_images("old", imgs[..3].to_vec()),
            Message::user_with_images("new", imgs[3..].to_vec()),
        ];
        let set = ferrule_core::vision::pixel_set(&msgs);
        let old = parts(0, &msgs[0], &set, true);
        assert!(
            matches!(&old[0], Part::Note(n) if n.contains("earlier photo")),
            "{old:?}"
        );
        assert!(matches!(old[1], Part::Pixels { .. }));
        let new = parts(1, &msgs[1], &set, true);
        assert!(new.iter().all(|p| matches!(p, Part::Pixels { .. })));
        let blind = parts(1, &msgs[1], &set, false);
        assert!(blind
            .iter()
            .all(|p| matches!(p, Part::Note(n) if n.contains("can't see images"))));
        std::fs::remove_file(dir.path().join("4.jpg")).unwrap();
        let gone = parts(1, &msgs[1], &set, true);
        assert!(matches!(&gone[1], Part::Note(n) if n.contains("no longer available")));
    }

    #[test]
    fn only_a_400_about_images_is_retried_without_them() {
        let e = |t: &str| CoreError::Provider(format!("HTTP 400: {t}"));
        assert!(image_rejected(&e(
            "Invalid content type. image_url is only supported by certain models."
        )));
        assert!(image_rejected(&e("this model does not support vision")));
        assert!(!image_rejected(&e("max_tokens is too large")));
        assert!(!image_rejected(&CoreError::Transient {
            message: "HTTP 503: image service down".into(),
            retry_after: None
        }));
    }
}
