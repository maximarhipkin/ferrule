//! Owner-private-chat-only, admitted-destination-only Telegram text sending.
//! No credentials, admission changes, arbitrary endpoints or channel selection.

use super::FileOut;
use crate::message::OutboundMessage;
use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use serde_json::{json, Value};
use std::sync::Arc;

pub struct SendMessageTool {
    out: Arc<FileOut>,
    session_id: String,
    owner_chat: i64,
}

impl SendMessageTool {
    /// Only the configured owner's exact private Telegram session is offered
    /// this tool. A group, scheduled task or sub-agent never inherits it.
    pub fn for_owner(out: Arc<FileOut>, session_id: &str, owner_chat: i64) -> Option<Self> {
        (owner_chat > 0 && session_id == format!("telegram__{owner_chat}")).then(|| Self {
            out,
            session_id: session_id.into(),
            owner_chat,
        })
    }
}

fn failed(message: impl Into<String>) -> CoreError {
    CoreError::ToolFailed {
        tool: "send_message".into(),
        message: message.into(),
    }
}

#[async_trait::async_trait]
impl Tool for SendMessageTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "send_message".into(),
            description: "Send a text message to an already admitted Telegram chat, only on the owner's explicit request. Use the known numeric chat id, never guess a destination. This posts immediately under the existing trust approval policy. No new chats are admitted. Do not retry an ambiguous delivery failure automatically (it may already have posted).".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "chat_id": {"type": "string", "description": "Known admitted numeric Telegram chat id"},
                    "text": {"type": "string", "description": "Exact text to post"}
                },
                "required": ["chat_id", "text"],
                "additionalProperties": false
            }),
        }
    }

    fn changes_files(&self) -> bool {
        false
    }
    fn needs_approval(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let object = args
            .as_object()
            .ok_or_else(|| failed("give chat_id and text"))?;
        if object.keys().any(|k| k != "chat_id" && k != "text") {
            return Err(failed("only chat_id and text are supported"));
        }
        let chat_id = args["chat_id"]
            .as_str()
            .ok_or_else(|| failed("chat_id must be a numeric string"))?;
        let id = chat_id
            .parse::<i64>()
            .map_err(|_| failed("chat_id must be numeric"))?;
        if id == 0 || id.to_string() != chat_id {
            return Err(failed("chat_id must be a canonical nonzero numeric id"));
        }
        let text = args["text"]
            .as_str()
            .ok_or_else(|| failed("text must be a string"))?;
        if text.trim().is_empty() || text.encode_utf16().count() > 4096 {
            return Err(failed(
                "text must be nonempty and at most 4096 UTF-16 units",
            ));
        }
        let router = self
            .out
            .router()
            .ok_or_else(|| failed("the gateway isn't running"))?;
        let expected = ("telegram".to_string(), self.owner_chat.to_string());
        if router.chat_of(&self.session_id).as_ref() != Some(&expected) {
            return Err(failed(
                "sending is only supported from the owner's private Telegram chat",
            ));
        }
        let channel = router
            .channel("telegram")
            .ok_or_else(|| failed("Telegram isn't running"))?;
        if !channel.allows_cross_chat_send(chat_id) {
            return Err(failed(
                "destination is not an admitted Telegram chat; no message sent",
            ));
        }
        let message_id = channel
            .send_with_id(OutboundMessage {
                channel: "telegram".into(),
                chat_id: chat_id.into(),
                text: text.into(),
                reply_to: None,
                attachments: vec![],
            })
            .await
            .map_err(|e| {
                failed(format!(
                    "delivery failed (may be ambiguous; do not retry automatically): {e}"
                ))
            })?;
        Ok(ToolOutput::ok(json!({"sent": true, "channel": "telegram", "chat_id": chat_id, "message_id": message_id}).to_string()))
    }
}
