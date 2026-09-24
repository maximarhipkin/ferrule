use crate::client::{McpClient, ServerHost};
use crate::config::McpServerConfig;
use crate::error::McpError;
use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// One remote MCP tool, wrapped as an ordinary ferrule `Tool`. Multiple
/// `McpRemoteTool`s for the same server share one `McpClient`/connection.
pub struct McpRemoteTool {
    client: Arc<McpClient>,
    full_name: String,
    remote_name: String,
    description: String,
    input_schema: Value,
    read_only: bool,
    timeout: Duration,
}

#[async_trait::async_trait]
impl Tool for McpRemoteTool {
    fn changes_files(&self) -> bool {
        !self.read_only
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.full_name.clone(),
            description: self.description.clone(),
            parameters: self.input_schema.clone(),
        }
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        match self
            .client
            .call_tool(&self.remote_name, args, self.timeout)
            .await
        {
            Ok(result) if result.is_error => {
                let text = result.text();
                Err(CoreError::ToolFailed {
                    tool: self.full_name.clone(),
                    message: if text.is_empty() {
                        "mcp tool reported an error".into()
                    } else {
                        text
                    },
                })
            }
            Ok(result) => Ok(ToolOutput::capped(result.text(), ctx.max_output_chars)),
            Err(e) => Err(CoreError::ToolFailed {
                tool: self.full_name.clone(),
                message: e.to_string(),
            }),
        }
    }
}

/// Spawn `cfg`'s server, run the handshake, list its tools, and wrap each one
/// as a `mcp__<server>__<tool>` `Tool`. On any failure the caller should log
/// a warning and continue without this server — never let one bad MCP server
/// stop the agent from starting.
///
/// When `cfg.sandbox` is `false`, or this OS has no sandbox backend, the
/// server runs unconfined — still with secrets scrubbed and the proxy env
/// set — and that is logged loudly, once, here.
pub async fn connect_and_build_tools(
    cfg: McpServerConfig,
    host: ServerHost,
) -> Result<Vec<Arc<dyn Tool>>, McpError> {
    let server_name = cfg.name.clone();
    let timeout = cfg.timeout();
    let client = Arc::new(McpClient::new(cfg, host));
    if let Some(reason) = client.sandbox_degraded() {
        tracing::warn!(
            server = %server_name,
            "mcp server `{server_name}` runs UNSANDBOXED: {reason} — it can read ferrule's \
             environment (secrets aside) and write anywhere its own OS user can"
        );
    }
    let infos = client.list_tools().await?;
    Ok(infos
        .into_iter()
        .map(|info| {
            Arc::new(McpRemoteTool {
                client: client.clone(),
                full_name: format!("mcp__{server_name}__{}", info.name),
                remote_name: info.name,
                description: info.description,
                input_schema: info.input_schema,
                read_only: info.annotations.read_only,
                timeout,
            }) as Arc<dyn Tool>
        })
        .collect())
}
