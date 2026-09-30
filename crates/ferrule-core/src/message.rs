use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Reasoning/thinking content for models trained with interleaved thinking
    /// (Kimi K2 Thinking, DeepSeek, OpenAI reasoning items). Harness profiles
    /// decide whether this is preserved across turns — stripping it is the
    /// classic "generic harness" mistake that destroys agent performance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// The assistant turn exactly as a native driver received it (M23):
    /// Anthropic's content blocks with signed thinking, or the Responses
    /// output items with encrypted reasoning. Only the driver that made it
    /// reads it back, for the same model, inside the tool loop in progress;
    /// everything else reads the neutral fields above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<NativeBlocks>,
    /// Photos the sender attached (M47). History holds the files' paths,
    /// never the bytes; the driver that serves a call reads and encodes
    /// them, or writes a note in their place (see [`crate::vision`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageRef>,
}

/// A photo saved on disk that a user message carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageRef {
    /// Where the file is (inside the workspace's inbox).
    pub path: String,
    /// `image/jpeg`, `image/png`, `image/webp` or `image/gif`.
    pub mime: String,
    /// What to call it in a note: the file name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Opaque provider blocks carried with an assistant message (see
/// [`Message::native`]). They can hold thinking and signatures, so `Debug`
/// prints only how many there are.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeBlocks {
    /// The driver's api: `anthropic` or `responses`.
    pub api: String,
    /// The model that produced them, as the provider reported it.
    pub model: String,
    pub items: Vec<serde_json::Value>,
}

impl std::fmt::Debug for NativeBlocks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeBlocks")
            .field("api", &self.api)
            .field("model", &self.model)
            .field("items", &format_args!("<{} items>", self.items.len()))
            .finish()
    }
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(content.into()),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning: None,
            native: None,
            images: vec![],
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning: None,
            native: None,
            images: vec![],
        }
    }
    pub fn assistant(
        content: Option<String>,
        tool_calls: Vec<ToolCall>,
        reasoning: Option<String>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content,
            tool_calls,
            tool_call_id: None,
            reasoning,
            native: None,
            images: vec![],
        }
    }
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: vec![],
            tool_call_id: Some(tool_call_id.into()),
            reasoning: None,
            native: None,
            images: vec![],
        }
    }

    /// A user message with photos attached.
    pub fn user_with_images(content: impl Into<String>, images: Vec<ImageRef>) -> Self {
        Self {
            images,
            ..Self::user(content)
        }
    }

    /// This message with its native blocks attached.
    pub fn with_native(mut self, native: NativeBlocks) -> Self {
        self.native = Some(native);
        self
    }

    /// Rough token estimate (~4 chars/token) for compaction triggering.
    pub fn est_tokens(&self) -> usize {
        let content = self.content.as_deref().unwrap_or("").len();
        let reasoning = self.reasoning.as_deref().unwrap_or("").len();
        let calls: usize = self
            .tool_calls
            .iter()
            .map(|c| c.arguments.to_string().len() + c.name.len())
            .sum();
        (content + reasoning + calls) / 4 + 4
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    /// Input tokens written to the provider's prompt cache (Anthropic's
    /// `cache_creation_input_tokens`), part of `input_tokens` like
    /// `cached_input_tokens`. 0 where the provider doesn't report writes.
    #[serde(default)]
    pub cache_write_input_tokens: u64,
    /// What the call would have cost at API prices, when the provider
    /// knows better than the price table: the Claude Code engine's own
    /// figure for a plan turn (M35). Ledger rows carry it as
    /// `notional_usd`; it is never money spent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notional_usd: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn est_tokens_scales_with_content() {
        let m = Message::user("a".repeat(400));
        assert!(m.est_tokens() >= 100);
        let empty = Message::user("");
        assert!(empty.est_tokens() < 10);
    }

    #[test]
    fn native_blocks_never_show_in_debug_and_old_rows_still_parse() {
        let m = Message::assistant(Some("hi".into()), vec![], None).with_native(NativeBlocks {
            api: "anthropic".into(),
            model: "claude-sonnet-5".into(),
            items: vec![serde_json::json!({"type": "thinking", "thinking": "secret plan", "signature": "sig-abc"})],
        });
        let dbg = format!("{m:?}");
        assert!(
            !dbg.contains("secret plan") && !dbg.contains("sig-abc"),
            "{dbg}"
        );
        assert!(dbg.contains("<1 items>"), "{dbg}");

        let back: Message = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back.native, m.native);
        let old: Message = serde_json::from_str(r#"{"role":"assistant","content":"x"}"#).unwrap();
        assert!(old.native.is_none());
        assert!(!serde_json::to_string(&old).unwrap().contains("native"));

        let u: Usage = serde_json::from_str(r#"{"input_tokens":5,"output_tokens":1}"#).unwrap();
        assert_eq!((u.cached_input_tokens, u.cache_write_input_tokens), (0, 0));
    }

    #[test]
    fn old_transcripts_without_images_still_load() {
        // An M46 user message: no `images` key.
        let old: Message =
            serde_json::from_str(r#"{"role":"user","content":"hello","tool_calls":[]}"#).unwrap();
        assert!(old.images.is_empty());
        assert!(!serde_json::to_string(&old).unwrap().contains("images"));
        // With photos it round-trips, and a plain message stays plain.
        let m = Message::user_with_images(
            "look",
            vec![ImageRef {
                path: "inbox/a.png".into(),
                mime: "image/png".into(),
                name: None,
            }],
        );
        let back: Message = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back.images, m.images);
    }
}
