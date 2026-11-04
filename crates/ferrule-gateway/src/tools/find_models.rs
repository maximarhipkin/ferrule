//! M67: `find_models`, the OpenRouter models for a job, so the agent can
//! pick the one best suited to it when `[models] bot_picks` lets it.

use super::media::MediaShared;
use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_media::MediaBackend as _;
use serde_json::{json, Value};
use std::sync::Arc;

/// The most models one answer lists.
const SHOWN: usize = 40;

pub struct FindModelsTool {
    shared: Arc<MediaShared>,
}

impl FindModelsTool {
    pub fn new(shared: Arc<MediaShared>) -> Self {
        Self { shared }
    }
}

#[async_trait::async_trait]
impl Tool for FindModelsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "find_models".into(),
            description: "List OpenRouter's models for a job, newest first, with prices, so you can pick the one best suited to it: `text` for a sub-agent (spawn_agent's model; only ones that take tools), `vision` for a sub-agent that also reads images, `image` and `video` for generate_image's and generate_video's model, `speech` for speak's. Narrow it with `search` (a vendor or a name). Weigh the job against the price; the owner's dollar caps hold whatever you pick.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["text", "vision", "image", "video", "speech"] },
                    "search": { "type": "string", "description": "Only models whose id or name has this (any case)" }
                },
                "required": ["kind"]
            }),
        }
    }

    /// Rare, and loaded when wanted.
    fn deferrable(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let kind = args["kind"].as_str().unwrap_or("text");
        let search = args["search"].as_str().unwrap_or("").trim().to_lowercase();
        let list = self
            .shared
            .backend
            .models(kind)
            .await
            .map_err(|e| CoreError::ToolFailed {
                tool: "find_models".into(),
                message: format!("OpenRouter's model list couldn't be read: {e}"),
            })?;
        let mut rows: Vec<&Value> = list
            .iter()
            .filter(|m| fits(kind, m))
            .filter(|m| {
                search.is_empty()
                    || format!("{} {}", m["id"], m["name"])
                        .to_lowercase()
                        .contains(&search)
            })
            .collect();
        rows.sort_by_key(|m| std::cmp::Reverse(m["created"].as_i64().unwrap_or(0)));
        if rows.is_empty() {
            return Ok(ToolOutput::ok(format!("No {kind} models match.")));
        }
        let mut out: Vec<String> = rows.iter().take(SHOWN).map(|m| line(m)).collect();
        if rows.len() > SHOWN {
            out.push(format!(
                "… and {} more; narrow it with `search`.",
                rows.len() - SHOWN
            ));
        }
        Ok(ToolOutput::ok(out.join("\n")))
    }
}

/// A sub-agent's model must take tools; a `vision` one images too. The
/// other lists are already only that kind.
fn fits(kind: &str, m: &Value) -> bool {
    let has = |list: &Value, word: &str| {
        list.as_array()
            .is_some_and(|l| l.iter().any(|x| x.as_str() == Some(word)))
    };
    match kind {
        "text" => has(&m["supported_parameters"], "tools"),
        "vision" => {
            has(&m["supported_parameters"], "tools")
                && has(&m["architecture"]["input_modalities"], "image")
        }
        _ => true,
    }
}

/// `id · $in/$out per million tokens · context`, or a video's per second.
fn line(m: &Value) -> String {
    let per_m = |v: &Value| {
        v.as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|p| *p > 0.0)
            .map(|p| p * 1e6)
    };
    let mut parts = vec![m["id"].as_str().unwrap_or("?").to_string()];
    if let Some(v) = ferrule_media::VideoPrice::of(m) {
        parts.push(format!("${:.2}/s", v.usd_per_second));
    }
    match (
        per_m(&m["pricing"]["prompt"]),
        per_m(&m["pricing"]["completion"]),
    ) {
        (Some(i), Some(o)) => parts.push(format!("${i:.2} in / ${o:.2} out per M tokens")),
        (Some(i), None) => parts.push(format!("${i:.2} per M tokens in")),
        _ => {}
    }
    if let Some(c) = m["context_length"].as_u64() {
        parts.push(format!("{}k context", c / 1000));
    }
    parts.join(" · ")
}
