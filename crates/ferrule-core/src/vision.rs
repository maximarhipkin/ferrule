//! Photos in a conversation (M47), and images tools return (M60).
//!
//! A user message carries [`ImageRef`]s: paths on disk, never bytes. So
//! does a tool result (M60: `read_file` on an image, an MCP screenshot, a
//! generated image). The driver that serves a call decides what each
//! becomes: pixels, when the model sees images and the image is one of the
//! newest few; a one-line note otherwise. This module is the part that
//! doesn't depend on a driver: which images are new enough, what the notes
//! say, and which files a tool may hand over as images.

use std::collections::HashSet;
use std::path::Path;

use crate::message::{ImageRef, Message, Role};

/// The most an image may weigh on the wire. The dashboard shrinks photos
/// to well under this; for a tool's image (M60) it's the cap, refused
/// above it, since nothing here can shrink one.
pub const MAX_IMAGE_BYTES: usize = 3_500_000;

/// How many photos go as pixels in one request: the newest. A long chat
/// doesn't resend every photo it ever saw.
pub const MAX_IMAGES: usize = 4;

/// What a photo's estimated cost in context is, in tokens (a typical
/// downscaled photo, whatever the provider's exact tiling).
pub const IMAGE_TOKENS: usize = 1_600;

/// Why a photo is a note and not pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteWhy {
    /// The model that serves the call can't see images.
    TextOnly,
    /// A photo from earlier in the chat, past the newest [`MAX_IMAGES`].
    Older,
    /// The file is gone, too big or not an image any more.
    Missing,
}

/// The `(message index, image index)` of the newest [`MAX_IMAGES`] photos.
pub fn pixel_set(messages: &[Message]) -> HashSet<(usize, usize)> {
    let mut all: Vec<(usize, usize)> = messages
        .iter()
        .enumerate()
        .flat_map(|(i, m)| (0..m.images.len()).map(move |j| (i, j)))
        .collect();
    let keep = all.len().saturating_sub(MAX_IMAGES);
    all.drain(..keep);
    all.into_iter().collect()
}

/// A message's words followed by a note for each of its photos: what a
/// text-only reader (a plan engine, a summary) gets of it.
pub fn text_with_notes(msg: &Message, why: NoteWhy) -> String {
    let mut text = msg.content.clone().unwrap_or_default();
    for img in &msg.images {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&note_for(msg, img, why));
    }
    text
}

/// The note for one of `msg`'s images: a photo someone sent, or (M60) an
/// image a tool returned.
pub fn note_for(msg: &Message, img: &ImageRef, why: NoteWhy) -> String {
    if msg.role == Role::Tool {
        tool_note(img, why)
    } else {
        note(img, why)
    }
}

/// M60: the sentence that stands in for a tool's image not sent as pixels.
pub fn tool_note(img: &ImageRef, why: NoteWhy) -> String {
    let name = name_of(img);
    match why {
        NoteWhy::TextOnly => {
            format!("[The tool returned an image, {name}, but this model can't see images.]")
        }
        NoteWhy::Older => format!("[image omitted: {name}, an earlier tool image]"),
        NoteWhy::Missing => format!(
            "[The tool's image {name} can't be sent any more: the file is gone, over the size cap, or not an image.]"
        ),
    }
}

/// M60: the file at `path` as an image a tool can hand the model. `None`
/// when its first bytes aren't a PNG, JPEG, GIF or WebP; an `Err` saying
/// why when it is one but over [`MAX_IMAGE_BYTES`]. The driver checks the
/// bytes again when it sends them.
pub fn image_file(path: &Path) -> Option<Result<ImageRef, String>> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let mut head = Vec::new();
    file.take(16).read_to_end(&mut head).ok()?;
    let mime = sniff(&head)?;
    if size > MAX_IMAGE_BYTES as u64 {
        let mb = |n: u64| format!("{:.1} MB", n as f64 / 1e6);
        return Some(Err(format!(
            "an image ({mime}) of {}, over the {} cap on an image sent to the model",
            mb(size),
            mb(MAX_IMAGE_BYTES as u64)
        )));
    }
    Some(Ok(ImageRef {
        path: path.to_string_lossy().into_owned(),
        mime: mime.to_string(),
        name: path.file_name().map(|n| n.to_string_lossy().into_owned()),
    }))
}

/// The type of an image by its first bytes: JPEG, PNG, GIF or WebP. SVG is
/// text that can carry script, and no model needs it, so it is not here.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// What to call an image in a note: its name, else its file's name.
fn name_of(img: &ImageRef) -> String {
    img.name
        .clone()
        .or_else(|| {
            std::path::Path::new(&img.path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "a file".to_string())
}

/// The sentence that stands in for a photo that isn't sent as pixels.
pub fn note(img: &ImageRef, why: NoteWhy) -> String {
    let name = name_of(img);
    match why {
        NoteWhy::TextOnly => format!(
            "[This model can't see images; the photo is saved as {name}. Open it with your tools if you need it.]"
        ),
        NoteWhy::Older => format!("[An earlier photo, saved as {name}.]"),
        NoteWhy::Missing => "[A photo was attached here but is no longer available.]".to_string(),
    }
}

/// Tokens the photos of `messages` add to the context estimate: only those
/// that would go as pixels count, the rest are a sentence.
pub fn est_tokens(messages: &[Message]) -> usize {
    pixel_set(messages).len() * IMAGE_TOKENS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(n: usize) -> ImageRef {
        ImageRef {
            path: format!("inbox/p{n}.jpg"),
            mime: "image/jpeg".into(),
            name: Some(format!("p{n}.jpg")),
        }
    }

    #[test]
    fn newest_four_images_go_as_pixels() {
        let msgs = vec![
            Message::user_with_images("one", vec![img(1), img(2)]),
            Message::user("no photo"),
            Message::user_with_images("two", vec![img(3), img(4), img(5)]),
        ];
        let set = pixel_set(&msgs);
        assert_eq!(set.len(), MAX_IMAGES);
        assert!(!set.contains(&(0, 0)), "the oldest photo is a note");
        assert!(set.contains(&(0, 1)) && set.contains(&(2, 0)) && set.contains(&(2, 2)));
        assert_eq!(est_tokens(&msgs), 4 * IMAGE_TOKENS);
        assert!(pixel_set(&[Message::user("hi")]).is_empty());
    }

    #[test]
    fn notes_say_what_happened_and_where_the_file_is() {
        let i = img(7);
        assert!(note(&i, NoteWhy::TextOnly).contains("can't see images"));
        assert!(note(&i, NoteWhy::TextOnly).contains("p7.jpg"));
        assert!(note(&i, NoteWhy::Older).contains("earlier photo"));
        assert!(note(&i, NoteWhy::Missing).contains("no longer available"));
        let bare = ImageRef {
            path: "inbox/x/y.png".into(),
            mime: "image/png".into(),
            name: None,
        };
        assert!(
            note(&bare, NoteWhy::Older).contains("y.png"),
            "falls back to the file name"
        );
    }
}
