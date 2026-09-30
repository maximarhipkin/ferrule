//! Photos in a conversation (M47).
//!
//! A user message carries [`ImageRef`]s: paths on disk, never bytes. The
//! driver that serves a call decides what each becomes: pixels, when the
//! model sees images and the photo is one of the newest few; a one-line
//! note otherwise. This module is the part that doesn't depend on a
//! driver: which photos are new enough, and what the notes say.

use std::collections::HashSet;

use crate::message::{ImageRef, Message};

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
        text.push_str(&note(img, why));
    }
    text
}

/// The sentence that stands in for a photo that isn't sent as pixels.
pub fn note(img: &ImageRef, why: NoteWhy) -> String {
    let name = img
        .name
        .clone()
        .or_else(|| {
            std::path::Path::new(&img.path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "a file".to_string());
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
