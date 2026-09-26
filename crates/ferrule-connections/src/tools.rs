//! The agent's side: it can ask for a service and see what's connected.
//! Neither tool connects anything or returns a secret.

use crate::service::{Chat, Connections};
use crate::store::State;
use async_trait::async_trait;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_core::CoreError;
use serde_json::{json, Value};
use std::sync::Arc;

pub const REQUEST: &str = "connection_request";
pub const LIST: &str = "connection_list";

/// Both tools, for a session in `chat`.
pub fn tools(conns: &Arc<Connections>, chat: Chat) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(RequestTool {
            conns: conns.clone(),
            chat,
        }),
        Arc::new(ListTool {
            conns: conns.clone(),
        }),
    ]
}

pub struct RequestTool {
    conns: Arc<Connections>,
    chat: Chat,
}

#[async_trait]
impl Tool for RequestTool {
    fn definition(&self) -> ToolDefinition {
        let names = self.conns.catalog().names().join(", ");
        ToolDefinition {
            name: REQUEST.into(),
            description: format!(
                "Ask the owner to connect an external service ({names}, or an https:// MCP \
                 server URL). This only sends them a button: nothing connects until they \
                 approve, and you never see a token. When they do, the service's tools \
                 (mcp__<service>__*) appear and you're told. Read-only unless write is true; \
                 ask for write only when the task needs changes."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "service": {"type": "string", "description": "A catalog name or an https:// MCP URL"},
                    "write": {"type": "boolean", "description": "Ask for write access (default false)"},
                    "reason": {"type": "string", "description": "One sentence for the owner: why it's needed"}
                },
                "required": ["service", "reason"]
            }),
        }
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let service = args["service"].as_str().unwrap_or("").trim();
        if service.is_empty() {
            return Err(CoreError::ToolFailed {
                tool: REQUEST.into(),
                message: "`service` is required".into(),
            });
        }
        let write = args["write"].as_bool().unwrap_or(false);
        let reason = args["reason"].as_str().unwrap_or("");
        match self.conns.request(&self.chat, service, write, reason).await {
            Ok(text) => Ok(ToolOutput::ok(text)),
            Err(e) => Err(CoreError::ToolFailed {
                tool: REQUEST.into(),
                message: format!("{e:#}"),
            }),
        }
    }

    fn changes_files(&self) -> bool {
        false
    }
}

pub struct ListTool {
    conns: Arc<Connections>,
}

#[async_trait]
impl Tool for ListTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: LIST.into(),
            description: "List connected external services and their state, and the ones that \
                          can be requested with connection_request."
                .into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    async fn call(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let snap = self.conns.snapshot().map_err(|e| CoreError::ToolFailed {
            tool: LIST.into(),
            message: format!("{e:#}"),
        })?;
        let mut lines = Vec::new();
        for c in &snap.connections {
            let state = match c.state {
                State::Connected => format!("connected, tools mcp__{}__*", c.name),
                State::NeedsReconnect => {
                    "needs reconnecting by the owner; its tools are suspended".into()
                }
            };
            lines.push(format!(
                "{}: {state} ({})",
                c.name,
                if c.write { "read-write" } else { "read-only" }
            ));
        }
        for p in &snap.pending {
            lines.push(format!("{p}: the owner is signing in"));
        }
        for a in &snap.asked {
            lines.push(format!("{a}: asked; waiting for the owner"));
        }
        if lines.is_empty() {
            lines.push("Nothing is connected.".into());
        }
        lines.push(format!(
            "Available: {}",
            self.conns.catalog().names().join(", ")
        ));
        Ok(ToolOutput::ok(lines.join("\n")))
    }

    fn changes_files(&self) -> bool {
        false
    }
    fn read_only(&self) -> bool {
        true
    }
}
