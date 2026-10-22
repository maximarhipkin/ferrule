//! M39 §2: `send_file {path, caption?}`, a workspace file to the chat the
//! session is talking to. There is no `to`: the session's own chat is the
//! only place it goes. Paths resolve the way the file tools resolve them,
//! so nothing outside the workspace or under the hidden paths leaves.
//!
//! M65 §4: an image or video the agent made or changed goes only once this
//! session's model has looked at it since it last changed
//! ([`ferrule_core::vision::Looked`]): `watch_video`, `read_file` or
//! `generate_image` showed it the pixels, or the vision helper's words.
//! What the owner provided (`uploads/`, `inbox/`) and screen captures
//! (`media/screen/`) pass as they are. When looking can't be done here,
//! `unchecked` says why and the file goes marked as unchecked; a video the
//! owner was told about unchecked (the poller's note) goes when asked.

use crate::channels::files;
use crate::message::{Attachment, OutboundMessage};
use crate::router::Router;
use crate::tools::media::MediaShared;
use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_core::vision::Looked;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

/// The largest file `send_file` reads, whatever the channel allows.
const MAX_BYTES: u64 = 100 * 1024 * 1024;

/// Where `send_file` (and M56.6's chat tools) find the session's chat: the
/// router, bound once it exists (the agent factory is built before it).
#[derive(Default)]
pub struct FileOut {
    router: OnceLock<Weak<Router>>,
    /// M56.6: what `ask_user` waits on.
    questions: Arc<super::Questions>,
    /// M65 §4: the media jobs, so a sent file settles its row and a video
    /// the owner was told about unchecked can go when asked.
    media: OnceLock<Weak<MediaShared>>,
}

impl FileOut {
    pub fn bind(&self, router: &Arc<Router>) {
        let _ = self.router.set(Arc::downgrade(router));
    }

    /// M65 §4: the media jobs `send_file` settles.
    pub fn bind_media(&self, media: &Arc<MediaShared>) {
        let _ = self.media.set(Arc::downgrade(media));
    }

    /// The made-but-unsent media rows whose result is `path`.
    fn unsent_as(&self, workspace: &Path, path: &Path) -> Vec<ferrule_media::MediaJob> {
        let Some(media) = self.media.get().and_then(Weak::upgrade) else {
            return Vec::new();
        };
        let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let target = real(path);
        media
            .store
            .unsent()
            .unwrap_or_default()
            .into_iter()
            .filter(|j| real(&workspace.join(&j.out_path)) == target)
            .collect()
    }

    /// `path` was sent: its rows are `done`.
    fn settle(&self, workspace: &Path, path: &Path) {
        let Some(media) = self.media.get().and_then(Weak::upgrade) else {
            return;
        };
        for job in self.unsent_as(workspace, path) {
            if let Err(e) = media.store.mark(&job.id, "done", None) {
                tracing::warn!(job = %job.id, "couldn't mark a sent media job done: {e}");
            }
        }
    }

    pub(crate) fn router(&self) -> Option<Arc<Router>> {
        self.router.get().and_then(Weak::upgrade)
    }

    /// M56.6: `ask_user`'s questions; the gateway reads each chat message
    /// through them first (`Gateway::with_interceptor`).
    pub fn questions(&self) -> Arc<super::Questions> {
        self.questions.clone()
    }
}

pub struct SendFileTool {
    out: Arc<FileOut>,
    session_id: String,
    hidden: Vec<PathBuf>,
    /// M65 §4: the check before an image or video goes; `None`, no check.
    check: Option<SendCheck>,
}

/// M65 §4: what `send_file` asks before an image or video the agent made
/// goes out.
struct SendCheck {
    looked: Looked,
    /// This session's model takes images in: pixels, or the helper's words.
    sees_images: bool,
    /// `watch_video` is offered here.
    watches_video: bool,
}

impl SendFileTool {
    pub fn new(out: Arc<FileOut>, session_id: impl Into<String>, hidden: Vec<PathBuf>) -> Self {
        Self {
            out,
            session_id: session_id.into(),
            hidden,
            check: None,
        }
    }

    /// M65 §4: refuse an image or video the agent made that this session
    /// hasn't looked at since it last changed (see the module's doc).
    pub fn checking(mut self, looked: Looked, sees_images: bool, watches_video: bool) -> Self {
        self.check = Some(SendCheck {
            looked,
            sees_images,
            watches_video,
        });
        self
    }
}

/// Whether `path` is something the owner provided, or a capture of the
/// owner's screen: it goes as it is.
fn passes_as_is(workspace: &Path, path: &Path) -> bool {
    let ws = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let Ok(rel) = path
        .strip_prefix(&ws)
        .or_else(|_| path.strip_prefix(workspace))
    else {
        return false;
    };
    let mut parts = rel.components().map(|c| c.as_os_str().to_string_lossy());
    matches!(
        (parts.next().as_deref(), parts.next().as_deref()),
        (Some("uploads" | "inbox"), _) | (Some("media"), Some("screen"))
    )
}

fn failed(message: impl Into<String>) -> CoreError {
    CoreError::ToolFailed {
        tool: "send_file".into(),
        message: message.into(),
    }
}

#[async_trait::async_trait]
impl Tool for SendFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "send_file".into(),
            description: "Send a file from the workspace to the person you're chatting with, in this chat (an image, a PDF, a report you wrote). It goes only to this chat. An image or video you made or edited goes only after you've looked at it (watch_video, read_file, or generate_image's result) since it last changed.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path relative to the workspace (or absolute within it)" },
                    "caption": { "type": "string", "description": "Optional text sent with the file" },
                    "unchecked": { "type": "string", "description": "Only for an image or video you made that can't be looked at here (no way to watch videos, or the look failed): why. It goes marked as not checked." }
                },
                "required": ["path"]
            }),
        }
    }

    fn changes_files(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let path = ferrule_tools::fs_tools::resolve(
            &ctx.workspace,
            &self.hidden,
            args["path"].as_str().unwrap_or(""),
        )?;
        let meta =
            std::fs::metadata(&path).map_err(|e| failed(format!("{}: {e}", path.display())))?;
        if !meta.is_file() {
            return Err(failed(format!("{} isn't a file", path.display())));
        }
        if meta.len() > MAX_BYTES {
            return Err(failed(format!(
                "{} is {}, over the {} send_file takes",
                path.display(),
                files::human(meta.len()),
                files::human(MAX_BYTES)
            )));
        }
        let router = self
            .out
            .router
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| failed("there's no chat to send to (the gateway isn't running)"))?;
        let (channel_name, chat_id) = router.chat_of(&self.session_id).ok_or_else(|| {
            failed("this session isn't a chat, so there's nobody to send a file to")
        })?;
        let channel = router
            .channel(&channel_name)
            .ok_or_else(|| failed(format!("the {channel_name} channel isn't running")))?;
        if !channel.capabilities().attachments {
            return Err(failed(format!(
                "this channel ({channel_name}) can't send files; say where the file is instead"
            )));
        }
        let name = path
            .file_name()
            .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
        let mime = files::mime_for(&name);
        let mut caption = args["caption"].as_str().unwrap_or("").to_string();
        // M65 §4: nothing made goes out unseen.
        let video = mime.starts_with("video/");
        if let Some(check) = self
            .check
            .as_ref()
            .filter(|_| video || mime.starts_with("image/"))
            .filter(|_| !passes_as_is(&ctx.workspace, &path))
            .filter(|c| !c.looked.has_seen(&path))
        {
            let how = if video {
                "Watch it with watch_video"
            } else {
                "Look at it with read_file"
            };
            let unchecked = args["unchecked"]
                .as_str()
                .map(str::trim)
                .filter(|w| !w.is_empty());
            let can_look = if video {
                check.watches_video && check.sees_images
            } else {
                check.sees_images
            } && !check.looked.could_not(&path);
            let owner_asked = self
                .out
                .unsent_as(&ctx.workspace, &path)
                .iter()
                .any(|j| j.state == "noted");
            let why = match (owner_asked, unchecked) {
                (true, why) => why.unwrap_or("you asked for it as it is").to_string(),
                (false, Some(why)) if !can_look => why.to_string(),
                (false, Some(_)) => {
                    return Err(failed(format!(
                        "{name} can be looked at here: {} and compare it with what was asked, then send it. `unchecked` is only for when looking can't be done",
                        how.to_lowercase()
                    )))
                }
                (false, None) => {
                    return Err(failed(format!(
                        "{name} hasn't been looked at since it last changed. {how} and compare it with what was asked, then send it.{}",
                        if can_look {
                            ""
                        } else {
                            " Looking can't be done here: pass `unchecked` with why, and it goes marked as not checked"
                        }
                    )))
                }
            };
            caption = if caption.trim().is_empty() {
                format!("Not checked: {why}.")
            } else {
                format!("Not checked: {why}.\n{caption}")
            };
        }
        let msg = OutboundMessage {
            channel: channel_name.clone(),
            chat_id,
            text: caption,
            reply_to: None,
            attachments: vec![Attachment {
                kind: mime.to_string(),
                url: path.to_string_lossy().into_owned(),
                name: Some(name.clone()),
            }],
        };
        channel
            .send(msg)
            .await
            .map_err(|e| failed(format!("sending {name} failed: {e}")))?;
        self.out.settle(&ctx.workspace, &path);
        Ok(ToolOutput::ok(format!(
            "Sent {name} ({}) to this chat.",
            files::human(meta.len())
        )))
    }
}
