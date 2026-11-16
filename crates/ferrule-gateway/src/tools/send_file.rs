//! M39 §2: `send_file {path, caption?}`, a workspace file to the chat the
//! session is talking to. There is no `to`: the session's own chat is the
//! only place it goes. Paths resolve the way the file tools resolve them,
//! so nothing outside the workspace or under the hidden paths leaves.
//!
//! M65 §4: an image or video the agent made or changed goes only once this
//! session's model has looked at it since it last changed
//! ([`ferrule_core::vision::Looked`]): `watch_video`, `read_file` or
//! `generate_image` showed it the pixels, or the vision helper's words, or
//! (the Claude Code engine) claude's own `Read` opened the image, or the
//! video's contact sheet. What the owner provided (`uploads/`, `inbox/`)
//! and screen captures (`media/screen/`) pass as they are. When this
//! session has no way to look (no images reach its model, no
//! `watch_video`), `unchecked` says why and the file goes marked as
//! unchecked; a video the owner was told about unchecked (the poller's
//! note) goes when asked.

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
    /// M69: the videos sent since the gateway started, newest last, so the
    /// owner's 👍/👎 on one reaches the session that made it.
    videos: std::sync::Mutex<std::collections::VecDeque<SentVideo>>,
}

/// M69: one video `send_file` sent, and the message it went in.
struct SentVideo {
    channel: String,
    chat: String,
    id: String,
    session: String,
    path: String,
}

/// The videos remembered for reactions.
const VIDEOS_KEPT: usize = 100;

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

    /// `path` was sent: its rows are `done`, or (M65 §5) `unchecked` when
    /// it went marked as not checked.
    fn settle(&self, workspace: &Path, path: &Path, unchecked: bool) {
        let Some(media) = self.media.get().and_then(Weak::upgrade) else {
            return;
        };
        let state = if unchecked { "unchecked" } else { "done" };
        for job in self.unsent_as(workspace, path) {
            if let Err(e) = media.store.mark(&job.id, state, None) {
                tracing::warn!(job = %job.id, "couldn't mark a sent media job done: {e}");
            }
        }
    }

    /// M69: the most bytes a video sent to `session_id`'s chat may be, as
    /// far as ferrule knows: Telegram's and WhatsApp's own upload limits,
    /// else what `send_file` reads. `None` when it isn't a chat.
    pub fn send_cap(&self, session_id: &str) -> Option<u64> {
        let (channel, _) = self.router()?.chat_of(session_id)?;
        Some(match channel.as_str() {
            "telegram" => crate::channels::telegram::FILE_MAX,
            "whatsapp" => crate::channels::whatsapp::AV_MAX,
            _ => MAX_BYTES,
        })
    }

    /// M69: the owner reacted to message `id` in `chat`. When it carried a
    /// video `send_file` sent, the session that sent it is woken to learn
    /// from it: a 👎 asks once what was off, a 👍 notes what worked,
    /// silently. True when it was such a video and the session was woken.
    pub fn thumbed(&self, channel: &str, chat: &str, id: &str, up: bool) -> bool {
        let found = self
            .videos
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .rev()
            .find(|v| v.channel == channel && v.chat == chat && v.id == id)
            .map(|v| (v.session.clone(), v.path.clone()));
        let (Some((session, path)), Some(router)) = (found, self.router()) else {
            return false;
        };
        let text = if up {
            format!(
                "[The owner reacted 👍 to the video you sent, {path}.] Note what worked in media/style as the video-editing skill's taste reference says (its measured numbers are a liked example), then end with NO_REPLY: don't answer this."
            )
        } else {
            format!(
                "[The owner reacted 👎 to the video you sent, {path}.] Ask them once, in one short message, what was off (pace, captions, sound, the look, or something else). When they answer, record it in media/style as the video-editing skill's taste reference says, and offer a fixed version."
            )
        };
        router.wake(&session, text)
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
    /// It opens image files with its engine's own tool (Claude Code's
    /// `Read`), whatever a tool's image reaches it as.
    opens_images: bool,
    /// `watch_video` is offered here.
    watches_video: bool,
    /// M69: `check_video` is offered here, so a made video goes only once
    /// it was measured as it is, and one with a FAIL only with
    /// `known_issues`.
    checks_video: bool,
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
    pub fn checking(
        mut self,
        looked: Looked,
        sees_images: bool,
        opens_images: bool,
        watches_video: bool,
    ) -> Self {
        self.check = Some(SendCheck {
            looked,
            sees_images,
            opens_images,
            watches_video,
            checks_video: false,
        });
        self
    }

    /// M69: also refuse a video the agent made that `check_video` hasn't
    /// measured since it last changed, or that failed it (unless
    /// `known_issues` says so in the caption).
    pub fn checking_video_if(mut self, on: bool) -> Self {
        if let Some(c) = &mut self.check {
            c.checks_video = on;
        }
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
            description: format!(
                "Send a file from the workspace to the person you're chatting with, in this chat (an image, a PDF, a report you wrote). It goes only to this chat. An image or video you made or edited goes only after you've looked at it since it last changed: {}.",
                if self.check.as_ref().is_some_and(|c| c.opens_images) {
                    "open the image with Read; for a video, watch_video, then open the contact sheet it saved with Read"
                } else {
                    "watch_video, read_file, or generate_image's result"
                }
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path relative to the workspace (or absolute within it)" },
                    "caption": { "type": "string", "description": "Optional text sent with the file" },
                    "unchecked": { "type": "string", "description": "Only for an image or video you made that can't be looked at here (no way to watch videos, or the look failed): why. It goes marked as not checked." },
                    "known_issues": { "type": "string", "description": "Only for a video you made that check_video still fails after your fixes: what is still wrong, in a sentence. It goes in the caption." }
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
        let mut unseen = false;
        // M65 §4: nothing made goes out unseen.
        let video = mime.starts_with("video/");
        if let Some(check) = self
            .check
            .as_ref()
            .filter(|_| video || mime.starts_with("image/"))
            .filter(|_| !passes_as_is(&ctx.workspace, &path))
            .filter(|c| !c.looked.has_seen(&path))
        {
            let how = match (video, check.opens_images) {
                (true, true) => {
                    "Watch it with watch_video, then open the contact sheet it saved with Read"
                }
                (true, false) => "Watch it with watch_video",
                (false, true) => "Open it with Read",
                (false, false) => "Look at it with read_file",
            };
            let unchecked = args["unchecked"]
                .as_str()
                .map(str::trim)
                .filter(|w| !w.is_empty());
            // Why this session can't look, if it can't. claude's `Read`
            // can always be tried again, whatever a tool's image became.
            let sees = check.sees_images || check.opens_images;
            let cannot = if video && !check.watches_video {
                Some("watch_video isn't available in this session")
            } else if !sees {
                Some("no images reach this session's model and there's no vision helper")
            } else if !check.opens_images && check.looked.could_not(&path) {
                Some("the last try to look at it failed")
            } else {
                None
            };
            let can_look = cannot.is_none();
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
                        "{name} can be looked at here: {} and compare it with what was asked, then send it. `unchecked` is only for when this session has no way to look",
                        how.to_lowercase()
                    )))
                }
                (false, None) => {
                    return Err(failed(format!(
                        "{name} hasn't been looked at since it last changed. {how} and compare it with what was asked, then send it.{}",
                        match cannot {
                            None => String::new(),
                            Some(why) => format!(
                                " If you can't ({why}), pass `unchecked` with why, and it goes marked as not checked"
                            ),
                        }
                    )))
                }
            };
            unseen = true;
            caption = if caption.trim().is_empty() {
                format!("Not checked: {why}.")
            } else {
                format!("Not checked: {why}.\n{caption}")
            };
        }
        // M69: a made video goes measured, and a broken one only said so.
        if let Some(check) = self
            .check
            .as_ref()
            .filter(|c| c.checks_video && video)
            .filter(|_| !passes_as_is(&ctx.workspace, &path))
            .filter(|_| {
                !self
                    .out
                    .unsent_as(&ctx.workspace, &path)
                    .iter()
                    .any(|j| j.state == "noted")
            })
        {
            let known = args["known_issues"]
                .as_str()
                .map(str::trim)
                .filter(|w| !w.is_empty());
            match (check.looked.check_of(&path), known) {
                (None, _) => {
                    return Err(failed(format!(
                        "{name} hasn't been measured since it last changed: run check_video on it, fix any FAIL, then send it"
                    )))
                }
                (Some(fails), None) if !fails.is_empty() => {
                    return Err(failed(format!(
                        "{name} fails check_video: {}. Fix that and check again; if it can't be fixed, send it with `known_issues` saying what is still wrong",
                        fails.join("; ")
                    )))
                }
                (Some(fails), Some(known)) if !fails.is_empty() => {
                    caption = if caption.trim().is_empty() {
                        format!("Known issues: {known}")
                    } else {
                        format!("Known issues: {known}\n{caption}")
                    };
                }
                _ => {}
            }
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
        let id = channel
            .send_with_id(msg)
            .await
            .map_err(|e| failed(format!("sending {name} failed: {e}")))?;
        // M69: a video's message, for the owner's 👍/👎 on it.
        if let (true, Some(id)) = (video, id) {
            let mut videos = self.out.videos.lock().unwrap_or_else(|e| e.into_inner());
            if videos.len() >= VIDEOS_KEPT {
                videos.pop_front();
            }
            videos.push_back(SentVideo {
                channel: channel_name.clone(),
                chat: router
                    .chat_of(&self.session_id)
                    .map(|(_, c)| c)
                    .unwrap_or_default(),
                id,
                session: self.session_id.clone(),
                path: path
                    .strip_prefix(&ctx.workspace)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned(),
            });
        }
        self.out.settle(&ctx.workspace, &path, unseen);
        Ok(ToolOutput::ok(format!(
            "Sent {name} ({}) to this chat.",
            files::human(meta.len())
        )))
    }
}
