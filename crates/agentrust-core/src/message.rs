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
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: Role::System, content: Some(content.into()), tool_calls: vec![], tool_call_id: None, reasoning: None }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: Role::User, content: Some(content.into()), tool_calls: vec![], tool_call_id: None, reasoning: None }
    }
    pub fn assistant(content: Option<String>, tool_calls: Vec<ToolCall>, reasoning: Option<String>) -> Self {
        Self { role: Role::Assistant, content, tool_calls, tool_call_id: None, reasoning }
    }
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self { role: Role::Tool, content: Some(content.into()), tool_calls: vec![], tool_call_id: Some(tool_call_id.into()), reasoning: None }
    }

    /// Rough token estimate (~4 chars/token) for compaction triggering.
    pub fn est_tokens(&self) -> usize {
        let content = self.content.as_deref().unwrap_or("").len();
        let reasoning = self.reasoning.as_deref().unwrap_or("").len();
        let calls: usize = self.tool_calls.iter().map(|c| c.arguments.to_string().len() + c.name.len()).sum();
        (content + reasoning + calls) / 4 + 4
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
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
}
