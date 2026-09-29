//! M39 §2: `send_file {path, caption?}`, a workspace file to the chat the
//! session is talking to. There is no `to`: the session's own chat is the
//! only place it goes. Paths resolve the way the file tools resolve them,
//! so nothing outside the workspace or under the hidden paths leaves.

use crate::channels::files;
use crate::message::{Attachment, OutboundMessage};
use crate::router::Router;
use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};

/// The largest file `send_file` reads, whatever the channel allows.
const MAX_BYTES: u64 = 100 * 1024 * 1024;

/// Where `send_file` finds the session's chat: the router, bound once it
/// exists (the agent factory is built before it).
#[derive(Default)]
pub struct FileOut {
    router: OnceLock<Weak<Router>>,
}

impl FileOut {
    pub fn bind(&self, router: &Arc<Router>) {
        let _ = self.router.set(Arc::downgrade(router));
    }
}

pub struct SendFileTool {
    out: Arc<FileOut>,
    session_id: String,
    hidden: Vec<PathBuf>,
}

impl SendFileTool {
    pub fn new(out: Arc<FileOut>, session_id: impl Into<String>, hidden: Vec<PathBuf>) -> Self {
        Self {
            out,
            session_id: session_id.into(),
            hidden,
        }
    }
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
            description: "Send a file from the workspace to the person you're chatting with, in this chat (an image, a PDF, a report you wrote). It goes only to this chat.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path relative to the workspace (or absolute within it)" },
                    "caption": { "type": "string", "description": "Optional text sent with the file" }
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
        let msg = OutboundMessage {
            channel: channel_name.clone(),
            chat_id,
            text: args["caption"].as_str().unwrap_or("").to_string(),
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
        Ok(ToolOutput::ok(format!(
            "Sent {name} ({}) to this chat.",
            files::human(meta.len())
        )))
    }
}
