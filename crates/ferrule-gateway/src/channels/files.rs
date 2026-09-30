//! M39 §2: files people send, saved where the agent's own tools can open
//! them, and the line the agent reads about each. An adapter downloads with
//! its own credentials (capped by [`read_capped`]) and hands the bytes to
//! [`Inbox::save`]; nothing lands outside `<workspace>/inbox/`.

use std::path::{Path, PathBuf};

/// The default cap on one file, in MB (`max_file_mb`).
pub const DEFAULT_MAX_MB: u64 = 20;

/// Where one gateway saves what arrives: `<workspace>/inbox/<channel>/<day>/`.
#[derive(Debug, Clone)]
pub struct Inbox {
    workspace: PathBuf,
    max_bytes: u64,
}

/// A file saved in the inbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    /// Relative to the workspace, with `/` on every OS.
    pub rel: String,
    pub path: PathBuf,
    pub mime: String,
    pub bytes: u64,
}

/// Why a file wasn't saved, in words for the sender and the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub name: String,
    pub why: String,
}

impl Inbox {
    pub fn new(workspace: impl Into<PathBuf>, max_mb: u64) -> Self {
        Self {
            workspace: workspace.into(),
            max_bytes: max_mb.max(1) * 1024 * 1024,
        }
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// "over the 20 MB limit", for a file of `bytes`.
    pub fn too_big(&self, name: &str, bytes: u64) -> Refused {
        Refused {
            name: name.to_string(),
            why: format!(
                "it is {}, over the {} limit (max_file_mb)",
                human(bytes),
                human(self.max_bytes)
            ),
        }
    }

    /// Saves `bytes` as `<id>-<name>` under today's folder for `channel`.
    pub fn save(
        &self,
        channel: &str,
        id: &str,
        name: &str,
        mime: Option<&str>,
        bytes: &[u8],
    ) -> std::io::Result<Saved> {
        let day = chrono::Local::now().format("%Y-%m-%d").to_string();
        let channel = safe_name(channel);
        let file = format!("{}-{}", clip(&safe_name(id), 40), safe_name(name));
        let dir = self.workspace.join("inbox").join(&channel).join(&day);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(&file);
        std::fs::write(&path, bytes)?;
        Ok(Saved {
            rel: format!("inbox/{channel}/{day}/{file}"),
            path,
            mime: mime
                .filter(|m| !m.is_empty())
                .map_or_else(|| mime_for(name).to_string(), str::to_string),
            bytes: bytes.len() as u64,
        })
    }
}

/// The line the agent reads for a saved file.
pub fn note(saved: &Saved) -> String {
    let kind = kind_of(&saved.mime);
    // A photo is handed to the model as pixels when it can see (M47); when
    // it can't, the provider layer adds that to its own note. So this line
    // doesn't claim either.
    let then = if kind == "photo" {
        ""
    } else {
        " Open it with your tools if you need it; you can't see images."
    };
    format!(
        "[The sender attached {} {kind}: {} ({}, {}).{then}]",
        article(kind),
        saved.rel,
        saved.mime,
        human(saved.bytes)
    )
}

/// The line the agent reads for a file that wasn't saved.
pub fn refused_note(r: &Refused) -> String {
    format!(
        "[The sender attached {}, which wasn't saved: {}.]",
        r.name, r.why
    )
}

/// What the sender is told about a file that wasn't saved.
pub fn refused_reply(r: &Refused) -> String {
    format!("I couldn't take {}: {}.", r.name, r.why)
}

/// `text` with a line per file appended; a message that was only files
/// reads as the notes alone.
pub fn with_notes(text: &str, saved: &[Saved], refused: &[Refused]) -> String {
    let notes: Vec<String> = saved
        .iter()
        .map(note)
        .chain(refused.iter().map(refused_note))
        .collect();
    match (text.trim().is_empty(), notes.is_empty()) {
        (_, true) => text.to_string(),
        (true, false) => notes.join("\n"),
        (false, false) => format!("{text}\n\n{}", notes.join("\n")),
    }
}

/// Reads a response's body, stopping at `max` bytes: `Err` names the size
/// in words. A `Content-Length` over the cap is refused before reading.
pub async fn read_capped(mut resp: reqwest::Response, max: u64) -> Result<Vec<u8>, String> {
    if let Some(len) = resp.content_length() {
        if len > max {
            return Err(format!(
                "it is {}, over the {} limit (max_file_mb)",
                human(len),
                human(max)
            ));
        }
    }
    let mut out = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(c)) => {
                out.extend_from_slice(&c);
                if out.len() as u64 > max {
                    return Err(format!("it is over the {} limit (max_file_mb)", human(max)));
                }
            }
            Ok(None) => return Ok(out),
            Err(e) => return Err(format!("the download failed: {}", e.without_url())),
        }
    }
}

/// A file name that is safe on every OS: letters, digits, `.-_`, nothing
/// that climbs out, at most 80 characters, never empty or dotted first.
pub fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut out: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    while out.starts_with('.') {
        out.remove(0);
    }
    let out = clip(&out, 80);
    if out.trim_matches(['_', '.']).is_empty() {
        "file".into()
    } else {
        out
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    // Keep the extension.
    let ext = Path::new(s)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| e.len() <= 10)
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let keep = max.saturating_sub(ext.chars().count());
    format!("{}{ext}", s.chars().take(keep).collect::<String>())
}

/// A MIME type from a file name, for the common kinds; else octet-stream.
pub fn mime_for(name: &str) -> &'static str {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "zip" => "application/zip",
        "mp3" => "audio/mpeg",
        "ogg" | "opus" => "audio/ogg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

/// "photo", "audio file", "video", "file".
pub fn kind_of(mime: &str) -> &'static str {
    match mime.split('/').next().unwrap_or("") {
        "image" => "photo",
        "audio" => "audio file",
        "video" => "video",
        _ => "file",
    }
}

fn article(kind: &str) -> &'static str {
    if kind.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    }
}

/// `184 KB`, `3.2 MB`.
pub fn human(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// A `multipart/form-data` body by hand (reqwest's `multipart` feature
/// would pull in another crate): `(boundary, body)`.
pub fn multipart(fields: &[(&str, &str)], file: (&str, &str, &str, &[u8])) -> (String, Vec<u8>) {
    let boundary = format!("ferrule-{}", uuid::Uuid::new_v4().simple());
    let mut body = Vec::new();
    for (k, v) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
    }
    let (field, name, mime, bytes) = file;
    let name = name.replace(['"', '\r', '\n'], "_");
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{name}\"\r\nContent-Type: {mime}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (boundary, body)
}

/// A local file the agent sends (`send_file`), read for upload: its name,
/// MIME type and bytes. `max` is the channel's own limit for that kind.
pub fn read_outgoing(
    att: &crate::message::Attachment,
) -> Result<(String, &'static str, Vec<u8>), crate::GatewayError> {
    let path = Path::new(&att.url);
    let name = att.name.clone().unwrap_or_else(|| {
        path.file_name()
            .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned())
    });
    let bytes = std::fs::read(path).map_err(|e| {
        crate::GatewayError::Channel(format!("couldn't read {name} to send it: {e}"))
    })?;
    Ok((name.clone(), mime_for(&name), bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_made_safe_and_stay_inside_the_inbox() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("..\\..\\boot.ini"), "boot.ini");
        assert_eq!(safe_name(".bashrc"), "bashrc");
        assert_eq!(safe_name("my photo (1).jpg"), "my_photo__1_.jpg");
        assert_eq!(safe_name(""), "file");
        assert_eq!(safe_name(".."), "file");
        let long = format!("{}.pdf", "a".repeat(200));
        let s = safe_name(&long);
        assert_eq!(s.chars().count(), 80);
        assert!(s.ends_with(".pdf"));
        let dir = tempfile::tempdir().unwrap();
        let inbox = Inbox::new(dir.path(), 1);
        let saved = inbox
            .save("whatsapp", "wamid/../x", "../../evil.sh", None, b"hi")
            .unwrap();
        assert!(saved
            .path
            .starts_with(dir.path().join("inbox").join("whatsapp")));
        assert!(saved.rel.starts_with("inbox/whatsapp/"), "{}", saved.rel);
        assert!(saved.rel.ends_with("-evil.sh"), "{}", saved.rel);
        assert_eq!(std::fs::read(&saved.path).unwrap(), b"hi");
    }

    #[test]
    fn the_note_names_the_path_type_and_size() {
        let s = Saved {
            rel: "inbox/whatsapp/2026-09-28/w1-IMG.jpg".into(),
            path: PathBuf::from("x"),
            mime: "image/jpeg".into(),
            bytes: 188_416,
        };
        assert_eq!(
            note(&s),
            "[The sender attached a photo: inbox/whatsapp/2026-09-28/w1-IMG.jpg (image/jpeg, 184 KB).]"
        );
        let r = Inbox::new("/w", 20).too_big("big.mov", 31 * 1024 * 1024);
        assert_eq!(
            refused_note(&r),
            "[The sender attached big.mov, which wasn't saved: it is 31.0 MB, over the 20.0 MB limit (max_file_mb).]"
        );
        assert_eq!(with_notes("", std::slice::from_ref(&s), &[]), note(&s));
        assert_eq!(with_notes("look", &[], &[]), "look");
        assert!(with_notes("look", &[s], &[r]).starts_with("look\n\n[The sender attached a photo"));
    }

    #[test]
    fn a_multipart_body_has_the_fields_and_the_file() {
        let (b, body) = multipart(
            &[("type", "image/png")],
            ("file", "a\"b.png", "image/png", b"PNG"),
        );
        let s = String::from_utf8_lossy(&body);
        assert!(s.starts_with(&format!("--{b}\r\n")));
        assert!(s.contains("name=\"type\"\r\n\r\nimage/png\r\n"));
        assert!(s.contains("filename=\"a_b.png\"\r\nContent-Type: image/png\r\n\r\nPNG\r\n"));
        assert!(s.ends_with(&format!("--{b}--\r\n")));
    }
}
