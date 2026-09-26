//! The model's side: `mcp_add`, `mcp_remove`, `skill_install`,
//! `skill_remove`, `skill_keep`, `plugin_add`, `plugin_remove` (M32) and
//! `extensions_list`. Every refusal is a
//! tool error with policy wording only — never the flagged text itself.

use crate::error::ExtError;
use crate::manager::{ExtensionManager, Outcome};
use crate::source::{McpRequest, PluginRequest, SkillRequest};
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_core::CoreError;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

/// The eight tools, over one manager.
pub fn tools(manager: &Arc<ExtensionManager>) -> Vec<Arc<dyn Tool>> {
    [
        Op::McpAdd,
        Op::McpRemove,
        Op::SkillInstall,
        Op::SkillRemove,
        Op::SkillKeep,
        Op::PluginAdd,
        Op::PluginRemove,
        Op::List,
    ]
    .into_iter()
    .map(|op| {
        Arc::new(ExtTool {
            op,
            manager: manager.clone(),
        }) as Arc<dyn Tool>
    })
    .collect()
}

#[derive(Clone, Copy)]
enum Op {
    McpAdd,
    McpRemove,
    SkillInstall,
    SkillRemove,
    SkillKeep,
    PluginAdd,
    PluginRemove,
    List,
}

struct ExtTool {
    op: Op,
    manager: Arc<ExtensionManager>,
}

#[derive(Deserialize)]
struct RemoveArgs {
    name: String,
    #[serde(default)]
    purge: bool,
}

#[derive(Deserialize)]
struct KeepArgs {
    name: String,
    check: String,
    #[serde(default)]
    replace: bool,
}

impl ExtTool {
    fn name(&self) -> &'static str {
        match self.op {
            Op::McpAdd => "mcp_add",
            Op::McpRemove => "mcp_remove",
            Op::SkillInstall => "skill_install",
            Op::SkillRemove => "skill_remove",
            Op::SkillKeep => "skill_keep",
            Op::PluginAdd => "plugin_add",
            Op::PluginRemove => "plugin_remove",
            Op::List => "extensions_list",
        }
    }

    fn fail(&self, message: impl Into<String>) -> CoreError {
        CoreError::ToolFailed {
            tool: self.name().into(),
            message: message.into(),
        }
    }

    fn args<T: for<'de> Deserialize<'de>>(&self, v: Value) -> Result<T, CoreError> {
        serde_json::from_value(v).map_err(|e| self.fail(format!("bad arguments: {e}")))
    }

    fn ext(&self, e: ExtError) -> CoreError {
        self.fail(e.to_string())
    }
}

fn outcome_text(o: Outcome, kind: &str) -> String {
    match o {
        Outcome::Installed {
            name,
            tools,
            warnings,
        } => {
            let mut s = if kind == "skill" {
                format!("skill `{name}` installed; activate it with activate_skill")
            } else if kind == "plugin" {
                format!(
                    "plugin `{name}` installed; its tools are available now: {}",
                    tools.join(", ")
                )
            } else {
                format!(
                    "mcp server `{name}` installed; its tools are available now: {}",
                    tools.join(", ")
                )
            };
            if warnings > 0 {
                s.push_str(&format!(
                    " ({warnings} scan warning(s) were logged for the owner)"
                ));
            }
            s
        }
        Outcome::Pending { id } => format!(
            "pending approval: {id} — ask the owner to run 'ferrule extensions approve {id}'. Nothing is installed until then."
        ),
    }
}

#[async_trait::async_trait]
impl Tool for ExtTool {
    fn definition(&self) -> ToolDefinition {
        let (description, parameters) = match self.op {
            Op::McpAdd => (
                "Install an MCP server and use its tools in this session. Sources: \
                 npm:<pkg>@<exact version>, pypi:<pkg>==<exact version>, \
                 git:<https url>[@<branch|tag|commit>] (with `command`: a file in the repo, \
                 or node/python3/python/deno/bun and a script in the repo), url:<https url>. \
                 Allow-listed sources install at once; others wait for the owner's approval. \
                 Every tool description is scanned before it is offered.",
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Short name; tools become mcp__<name>__<tool>."},
                        "source": {"type": "string"},
                        "command": {"type": "string", "description": "git only: what to run, relative to the repo."},
                        "args": {"type": "array", "items": {"type": "string"}},
                        "replace": {"type": "boolean", "description": "Reinstall over an existing server of this name."}
                    },
                    "required": ["name", "source"]
                }),
            ),
            Op::McpRemove => (
                "Remove an MCP server this agent installed. `purge` also deletes its state.",
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "purge": {"type": "boolean"}
                    },
                    "required": ["name"]
                }),
            ),
            Op::SkillInstall => (
                "Install a skill (a directory with SKILL.md) from a git repo, pinned to one commit, \
                 usable at once with activate_skill. Source: git:<https url>[@<rev>]; `path` is the \
                 skill's directory in the repo (the root when omitted).",
                json!({
                    "type": "object",
                    "properties": {
                        "source": {"type": "string"},
                        "path": {"type": "string"},
                        "replace": {"type": "boolean"}
                    },
                    "required": ["source"]
                }),
            ),
            Op::SkillRemove => (
                "Remove a skill this agent installed or kept.",
                json!({
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                }),
            ),
            Op::SkillKeep => (
                "Keep a skill you wrote: put it in .ferrule/skill-drafts/<name>/SKILL.md (with any \
                 scripts beside it), and give a `check` command that exercises it. The check runs in \
                 the sandbox inside the draft directory; the skill is kept only if it passes and the \
                 scan is clean.",
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "check": {"type": "string"},
                        "replace": {"type": "boolean"}
                    },
                    "required": ["name", "check"]
                }),
            ),
            Op::PluginAdd => (
                "Install a WASM tool plugin (a plugin.json manifest and its .wasm) and use its tools \
                 in this session. Sources: git:<https url>[@<rev>] (with `path`: the plugin's \
                 directory in the repo), url:<https url of plugin.json> (with `sha256`: the .wasm's \
                 SHA-256), or a directory in the workspace. The plugin runs sandboxed with only the \
                 capabilities its manifest declares; files, network or secrets always need the \
                 owner's approval, as does any local directory.",
                json!({
                    "type": "object",
                    "properties": {
                        "source": {"type": "string"},
                        "path": {"type": "string", "description": "git only: the plugin's directory in the repo."},
                        "sha256": {"type": "string", "description": "The .wasm's SHA-256; required for url sources."},
                        "replace": {"type": "boolean", "description": "Reinstall over an existing plugin of this name."}
                    },
                    "required": ["source"]
                }),
            ),
            Op::PluginRemove => (
                "Remove a WASM plugin this agent installed.",
                json!({
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                }),
            ),
            Op::List => (
                "List MCP servers, skills and plugins: configured, installed, suspended, and pending requests.",
                json!({"type": "object", "properties": {}}),
            ),
        };
        ToolDefinition {
            name: self.name().into(),
            description: description.into(),
            parameters,
        }
    }

    fn changes_files(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let m = &self.manager;
        let text = match self.op {
            Op::McpAdd => {
                let req: McpRequest = self.args(args)?;
                outcome_text(m.install_mcp(req).await.map_err(|e| self.ext(e))?, "server")
            }
            Op::McpRemove => {
                let a: RemoveArgs = self.args(args)?;
                m.remove_server(&a.name, false, a.purge)
                    .await
                    .map_err(|e| self.ext(e))?;
                format!("mcp server `{}` removed", a.name)
            }
            Op::SkillInstall => {
                let req: SkillRequest = self.args(args)?;
                outcome_text(
                    m.install_skill(req).await.map_err(|e| self.ext(e))?,
                    "skill",
                )
            }
            Op::SkillRemove => {
                let a: RemoveArgs = self.args(args)?;
                m.remove_skill(&a.name, false)
                    .await
                    .map_err(|e| self.ext(e))?;
                format!("skill `{}` removed", a.name)
            }
            Op::SkillKeep => {
                let a: KeepArgs = self.args(args)?;
                let o = m
                    .keep_skill(&a.name, &a.check, &ctx.workspace, a.replace)
                    .await
                    .map_err(|e| self.ext(e))?;
                outcome_text(o, "skill")
            }
            Op::PluginAdd => {
                let req: PluginRequest = self.args(args)?;
                outcome_text(
                    m.install_plugin(req).await.map_err(|e| self.ext(e))?,
                    "plugin",
                )
            }
            Op::PluginRemove => {
                let a: RemoveArgs = self.args(args)?;
                m.remove_plugin(&a.name, false)
                    .await
                    .map_err(|e| self.ext(e))?;
                format!("plugin `{}` removed", a.name)
            }
            Op::List => {
                let mut lines = Vec::new();
                for l in m.list().map_err(|e| self.ext(e))? {
                    let mut s = format!(
                        "{} {} [{}] {} — {}",
                        l.kind, l.name, l.origin, l.source, l.status
                    );
                    if let Some(r) = &l.reason {
                        s.push_str(&format!(" ({r})"));
                    }
                    if !l.tools.is_empty() {
                        s.push_str(&format!("; tools: {}", l.tools.join(", ")));
                    }
                    lines.push(s);
                }
                for p in m.queue().list().map_err(|e| self.ext(e))? {
                    lines.push(format!("pending {}: {}", p.id, p.request.describe()));
                }
                if lines.is_empty() {
                    "nothing installed or pending".into()
                } else {
                    lines.join("\n")
                }
            }
        };
        Ok(ToolOutput::ok(text))
    }
}
