use crate::error::GatewayError;
use crate::message::OutboundMessage;
use tokio::sync::mpsc;

/// What an adapter can do beyond plain send/receive. The router and any
/// future skills use this to decide what to attempt rather than probing at
/// call time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChannelCapabilities {
    pub reactions: bool,
    pub edits: bool,
    pub attachments: bool,
    /// Buttons under a message (M20: "Connect Notion"). Without them a
    /// button is sent as a line of text.
    pub buttons: bool,
}

/// A button under a message: a link to open, or a command sent back into
/// the chat as if typed (it's no more trusted than typed text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    pub text: String,
    pub action: ButtonAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ButtonAction {
    Url(String),
    Command(String),
}

/// Buttons as lines of text, for a channel without them.
pub fn buttons_as_text(text: &str, buttons: &[Button]) -> String {
    let mut out = text.to_string();
    for b in buttons {
        match &b.action {
            ButtonAction::Url(url) => out.push_str(&format!("\n• {}: {url}", b.text)),
            ButtonAction::Command(cmd) => out.push_str(&format!("\n• {}: send {cmd}", b.text)),
        }
    }
    out
}

/// Sends `msg` with `buttons` where the channel has them, as text where it
/// doesn't (or where they were refused).
pub async fn send_with_buttons(
    channel: &dyn Channel,
    mut msg: OutboundMessage,
    buttons: &[Button],
) -> Result<(), GatewayError> {
    if buttons.is_empty() {
        return channel.send(msg).await;
    }
    if channel.capabilities().buttons {
        match channel.send_buttons(msg.clone(), buttons).await {
            Ok(()) => return Ok(()),
            Err(e) => tracing::warn!(error = %e, "buttons were refused; sending them as text"),
        }
    }
    msg.text = buttons_as_text(&msg.text, buttons);
    channel.send(msg).await
}

/// A messaging surface: Telegram, a local stdin/loopback adapter, eventually
/// WhatsApp/Discord/Slack. Implementations must be cheap to clone via `Arc`
/// (the gateway shares one instance across every session that uses it).
#[async_trait::async_trait]
pub trait Channel: Send + Sync {
    /// Stable identifier used as the `channel` field of every message this
    /// adapter produces, and as the routing key back to it for replies.
    fn name(&self) -> &str;

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities::default()
    }

    /// Long-running inbound loop: push every inbound message onto `tx`.
    /// Must only return when the channel's own source is exhausted (stdin
    /// EOF) or a fatal transport error occurs — never as a matter of course,
    /// since returning here ends this channel's contribution to the gateway.
    async fn run(
        &self,
        tx: mpsc::Sender<crate::message::InboundMessage>,
    ) -> Result<(), GatewayError>;

    /// Deliver a reply. Called from the destination session's lane, so it
    /// may be invoked concurrently for different chats but never twice at
    /// once for the same chat.
    async fn send(&self, msg: OutboundMessage) -> Result<(), GatewayError>;

    /// Like `send`, returning the new message's id when the channel has
    /// one, so it can be edited later (M27: a streamed reply).
    async fn post(&self, msg: OutboundMessage) -> Result<Option<String>, GatewayError> {
        self.send(msg).await.map(|()| None)
    }

    /// Optional: send with buttons, one per row. Use [`send_with_buttons`],
    /// which falls back to text.
    async fn send_buttons(
        &self,
        _msg: OutboundMessage,
        _buttons: &[Button],
    ) -> Result<(), GatewayError> {
        Err(GatewayError::Unsupported("buttons"))
    }

    /// Optional: react to an inbound message (e.g. an emoji ack). Default
    /// is "not supported" rather than a silent no-op, so callers can tell
    /// the difference between "acked" and "can't ack here".
    async fn react(
        &self,
        _chat_id: &str,
        _message_id: &str,
        _emoji: &str,
    ) -> Result<(), GatewayError> {
        Err(GatewayError::Unsupported("reactions"))
    }

    /// Whether this adapter polls a remote service for messages (M19b: its
    /// health is how recently a poll last succeeded).
    fn polls(&self) -> bool {
        false
    }

    /// When a poll last succeeded; `None` before the first one.
    fn last_ok_poll(&self) -> Option<std::time::SystemTime> {
        None
    }

    /// What keeps this channel from hearing messages right now, in plain
    /// words (M19c: Telegram's 409 Conflict), for `/status`. `None` when
    /// nothing is known to be wrong.
    fn problem(&self) -> Option<String> {
        None
    }

    /// Optional: edit a previously sent message in place.
    async fn edit(
        &self,
        _chat_id: &str,
        _message_id: &str,
        _text: &str,
    ) -> Result<(), GatewayError> {
        Err(GatewayError::Unsupported("message edits"))
    }
}
