use serde::{Deserialize, Serialize};

/// A file/image/audio blob riding alongside a message. Adapters decide
/// whether `url` is a remote URL, a local path, or an opaque provider id —
/// the gateway itself never fetches or interprets it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub kind: String,
    pub url: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// A message normalized from any channel into one shape. `channel` + `chat_id`
/// together select the session lane (see `session::session_id`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub channel: String,
    pub chat_id: String,
    pub sender: String,
    pub message_id: String,
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Unix seconds, as reported by the channel (or capture time if unknown).
    pub ts: i64,
}

/// A reply to deliver back out through a channel adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub channel: String,
    pub chat_id: String,
    pub text: String,
    #[serde(default)]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}
