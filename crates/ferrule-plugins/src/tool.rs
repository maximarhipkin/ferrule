//! A plugin's tools as ordinary `Tool`s: the registry, the guards, the
//! parallel scheduler and sub-agent reach treat them like any other.

use crate::host::Host;
use crate::manifest::ToolSpec;
use crate::{schema, CallError, Plugin};
use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_sandbox::Sandbox;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// One tool of a loaded plugin.
pub struct PluginTool {
    plugin: Arc<Plugin>,
    spec: ToolSpec,
    sandbox: Sandbox,
}

/// Every tool `plugin` offers. `sandbox` supplies the read deny list, the
/// egress (credential proxy) and secret placeholders.
pub fn tools(plugin: Arc<Plugin>, sandbox: &Sandbox) -> Vec<Arc<dyn Tool>> {
    plugin
        .manifest()
        .tools
        .iter()
        .map(|spec| {
            Arc::new(PluginTool {
                plugin: plugin.clone(),
                spec: spec.clone(),
                sandbox: sandbox.clone(),
            }) as Arc<dyn Tool>
        })
        .collect()
}

impl PluginTool {
    fn name(&self) -> String {
        self.plugin.manifest().tool_name(&self.spec.name)
    }
    fn failed(&self, message: String) -> CoreError {
        CoreError::ToolFailed {
            tool: self.name(),
            message,
        }
    }
    /// Output a network response may have shaped is marked untrusted.
    fn untrusted(&self) -> bool {
        !self.plugin.manifest().capabilities.http.is_empty()
    }
}

/// Sets the flag when the call's future is dropped (the turn was
/// cancelled), so the blocking thread stops at the next fuel slice.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[async_trait::async_trait]
impl Tool for PluginTool {
    fn definition(&self) -> ToolDefinition {
        let m = self.plugin.manifest();
        ToolDefinition {
            name: self.name(),
            description: format!(
                "{} (plugin `{}` {})",
                self.spec.description.trim_end(),
                m.name,
                m.version
            ),
            parameters: self.spec.parameters.clone(),
        }
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let args = if args.is_null() {
            Value::Object(Default::default())
        } else {
            args
        };
        schema::validate(&self.spec.parameters, &args).map_err(|e| {
            self.failed(format!(
                "the arguments don't match the tool's schema, so the plugin didn't run: {e}"
            ))
        })?;
        let m = self.plugin.manifest();
        let host = Host {
            plugin: m.name.clone(),
            caps: m.capabilities.clone(),
            workspace: ctx.workspace.clone(),
            hidden: self.sandbox.read_deny_list(&ctx.workspace),
            sandbox: self.sandbox.clone(),
            runtime: tokio::runtime::Handle::current(),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let _guard = CancelOnDrop(cancel.clone());
        let plugin = self.plugin.clone();
        let tool = self.spec.name.clone();
        let result = tokio::task::spawn_blocking(move || plugin.call(&tool, &args, host, cancel))
            .await
            .map_err(|e| self.failed(format!("the plugin's thread failed: {e}")))?;
        let value = match result {
            Ok(v) => v,
            Err(CallError::Tool(e)) => return Err(self.failed(e)),
            Err(CallError::Failed(e)) => {
                return Err(self.failed(format!("plugin `{}` failed: {e}", m.name)))
            }
        };
        let text = match value {
            Value::String(s) => s,
            other => serde_json::to_string_pretty(&other).unwrap_or_default(),
        };
        let cap = m
            .limits
            .effective()
            .output_chars
            .map_or(ctx.max_output_chars, |c| c.min(ctx.max_output_chars));
        if !self.untrusted() {
            return Ok(ToolOutput::capped(text, cap));
        }
        // Cap first so the closing tag survives.
        let inner = ToolOutput::capped(escape(&text), cap.saturating_sub(200));
        Ok(ToolOutput {
            content: format!(
                "<plugin_output plugin=\"{}\" tool=\"{}\" untrusted=\"true\">\n{}\n</plugin_output>",
                escape_attr(&m.name),
                escape_attr(&self.spec.name),
                inner.content
            ),
            truncated: inner.truncated,
        })
    }

    fn changes_files(&self) -> bool {
        self.plugin.manifest().capabilities.writes()
    }

    fn read_only(&self) -> bool {
        self.spec.read_only && !self.changes_files()
    }
}

/// As web search escapes results: nothing inside can close the fence.
fn escape(s: &str) -> String {
    s.replace('<', "&lt;").replace('>', "&gt;")
}

fn escape_attr(s: &str) -> String {
    escape(s).replace('"', "&quot;")
}
