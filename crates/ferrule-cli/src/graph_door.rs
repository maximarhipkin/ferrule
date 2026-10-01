//! M43: `/graph <file>` in a chat. The graph runs on the gateway's shared
//! supervisor, its approval nodes ask the owner's chat (the trust hub's
//! ask_owner, buttons and all), and the ending is reported where it
//! started. The runner stays deterministic Rust — the chat only starts it
//! and answers its gates.

use ferrule_agents::Supervisor;
use ferrule_gateway::{Channel, InboundMessage};
use ferrule_trust::Hub;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// The `/graph` door: runs a declarative agent graph from a chat.
pub struct GraphDoor {
    pub hub: Arc<Hub>,
    pub channels: HashMap<String, Arc<dyn Channel>>,
    /// The gateway's shared supervisor; None (agents off) refuses.
    pub supervisor: Option<Arc<Supervisor>>,
    pub workspace: PathBuf,
    pub max_iterations: usize,
}

impl GraphDoor {
    #[cfg(test)]
    pub const HANDLES: &'static [&'static str] = &["graph"];

    fn start(&self, msg: &InboundMessage, file: &str, yes: bool) -> Option<String> {
        let Some(sup) = self.supervisor.clone() else {
            return Some("A graph needs sub-agents: set `[agents] enabled = true`.".into());
        };
        let path = {
            let p = PathBuf::from(file);
            if p.is_absolute() {
                p
            } else {
                self.workspace.join(p)
            }
        };
        if !path.exists() {
            return Some(format!(
                "No graph file at {} (relative paths start at the workspace).",
                path.display()
            ));
        }
        let hub = self.hub.clone();
        let channels = self.channels.clone();
        let back = (msg.channel.clone(), msg.chat_id.clone());
        let name = path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| "graph".into());
        let ack_name = name.clone();
        let workspace = self.workspace.clone();
        let max_iterations = self.max_iterations;
        tokio::spawn(async move {
            let outcome = crate::graph::run(
                &path,
                crate::graph::RunOpts {
                    goal: None,
                    model: None,
                    workspace,
                    max_iterations,
                    max_steps: None,
                    auto_approve: yes,
                    supervisor: Some(sup),
                    hub: Some(hub),
                    quiet: true,
                },
            )
            .await;
            let text = match outcome {
                Ok(report) => {
                    let mark = if report.code == 0 { "✅" } else { "⏸" };
                    let last = report.lines.last().cloned().unwrap_or_default();
                    format!("{mark} {name}: {last}")
                }
                Err(e) => format!("⚠️ {name} errored: {e:#}"),
            };
            crate::goal_door::send_to_chat(&channels, &back.0, &back.1, text).await;
        });
        Some(format!(
            "Graph {ack_name} started{} — I'll report here when it ends. Approval nodes ask in your chat.",
            if yes { " (approving every gate)" } else { "" }
        ))
    }
}

#[async_trait::async_trait]
impl ferrule_gateway::Interceptor for GraphDoor {
    async fn intercept(&self, msg: &InboundMessage) -> Option<String> {
        if !crate::trust::is_chat_channel(&msg.channel) {
            return None;
        }
        let mut words = msg.text.split_whitespace();
        let cmd = words.next()?;
        let cmd = cmd.split('@').next().unwrap_or(cmd).to_lowercase();
        if cmd != "/graph" {
            return None;
        }
        if crate::trust::owner_in(&self.hub, msg) != Some(true) {
            return Some("Only the owner can run a graph.".into());
        }
        let mut yes = false;
        let mut file = None;
        for w in words {
            if w == "--yes" || w == "-y" {
                yes = true;
            } else if file.is_none() {
                file = Some(w);
            }
        }
        let Some(file) = file else {
            return Some(
                "Usage: /graph <file> [--yes] — the file is relative to the workspace.".into(),
            );
        };
        self.start(msg, file, yes)
    }
}
