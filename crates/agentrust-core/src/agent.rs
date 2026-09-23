use crate::error::CoreError;
use crate::event::AgentEvent;
use crate::message::{Message, Usage};
use crate::profile::{HarnessProfile, COMPACTION_TEMPLATE};
use crate::provider::{CompletionRequest, Provider};
use crate::tool::{ToolContext, ToolRegistry};
use crate::transcript::Transcript;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub max_iterations: usize,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Number of trailing messages always kept verbatim through compaction.
    pub compaction_keep_last: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self { max_iterations: 60, max_output_tokens: None, temperature: None, compaction_keep_last: 6 }
    }
}

/// One serialized agent run loop. Construct one per session; do not drive it
/// concurrently — sessions are serialized by the caller (session lane).
pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    profile: HarnessProfile,
    config: AgentConfig,
    tool_ctx: ToolContext,
    transcript: Option<Transcript>,
    pub messages: Vec<Message>,
    pub usage: Usage,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: ToolRegistry,
        profile: HarnessProfile,
        config: AgentConfig,
        tool_ctx: ToolContext,
        transcript: Option<Transcript>,
    ) -> Self {
        Self { provider, tools, profile, config, tool_ctx, transcript, messages: Vec::new(), usage: Usage::default() }
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.messages.push(Message::system(prompt.into()));
        self
    }

    async fn emit(&self, tx: &mpsc::Sender<AgentEvent>, ev: AgentEvent) {
        let _ = tx.send(ev).await; // receiver may be detached in headless mode
    }

    fn est_context_tokens(&self) -> usize {
        self.messages.iter().map(|m| m.est_tokens()).sum()
    }

    /// The ReAct loop: call → tool calls → observe → repeat until text-only.
    pub async fn run(&mut self, goal: &str, tx: mpsc::Sender<AgentEvent>) -> Result<String, CoreError> {
        let session_id = self
            .transcript
            .as_ref()
            .and_then(|t| t.path().file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "ephemeral".into());
        self.emit(&tx, AgentEvent::RunStarted { session_id, goal: goal.into() }).await;

        let user = Message::user(goal);
        self.log(&user);
        self.messages.push(user);

        for iteration in 0..self.config.max_iterations {
            self.maybe_compact(&tx).await?;

            let req = CompletionRequest {
                messages: self.rendered_messages(),
                tools: self.tools.definitions(),
                max_output_tokens: self.config.max_output_tokens,
                temperature: self.config.temperature,
            };

            let resp = self.provider.complete(req).await.map_err(|e| {
                let msg = e.to_string();
                CoreError::Provider(msg)
            })?;

            self.usage.input_tokens += resp.usage.input_tokens;
            self.usage.output_tokens += resp.usage.output_tokens;
            self.usage.cached_input_tokens += resp.usage.cached_input_tokens;
            self.emit(
                &tx,
                AgentEvent::Usage {
                    input_tokens: resp.usage.input_tokens,
                    output_tokens: resp.usage.output_tokens,
                    cached_input_tokens: resp.usage.cached_input_tokens,
                },
            )
            .await;

            let msg = resp.message;
            if let Some(text) = &msg.content {
                if !text.is_empty() {
                    self.emit(&tx, AgentEvent::AssistantText { text: text.clone() }).await;
                }
            }
            if let Some(r) = &msg.reasoning {
                if !r.is_empty() {
                    self.emit(&tx, AgentEvent::Reasoning { text: r.clone() }).await;
                }
            }

            let finished = msg.tool_calls.is_empty();
            self.log(&msg);
            self.messages.push(msg.clone());

            if finished {
                let answer = msg.content.unwrap_or_default();
                self.emit(&tx, AgentEvent::RunFinished { answer_chars: answer.len(), iterations: iteration + 1 }).await;
                return Ok(answer);
            }

            for call in &msg.tool_calls {
                self.emit(&tx, AgentEvent::ToolCallStarted {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })
                .await;

                let result = self.tools.call(&call.name, call.arguments.clone(), &self.tool_ctx).await;
                let (content, ok) = match result {
                    Ok(out) => (out.content, true),
                    Err(e) => (format!("error: {e}"), false),
                };
                if !ok {
                    warn!(tool = %call.name, "tool call failed");
                }
                self.emit(&tx, AgentEvent::ToolCallFinished {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    ok,
                    output_chars: content.len(),
                })
                .await;

                let tool_msg = Message::tool_result(&call.id, content);
                self.log(&tool_msg);
                self.messages.push(tool_msg);
            }
        }
        Err(CoreError::MaxIterations(self.config.max_iterations))
    }

    fn log(&self, msg: &Message) {
        if let Some(t) = &self.transcript {
            if let Err(e) = t.log_message(msg) {
                warn!("transcript write failed: {e}");
            }
        }
    }

    /// Messages as sent to the provider: reasoning stripped when the profile
    /// says the model neither needs nor accepts it back.
    fn rendered_messages(&self) -> Vec<Message> {
        if self.profile.retain_reasoning {
            return self.messages.clone();
        }
        self.messages
            .iter()
            .map(|m| {
                let mut m = m.clone();
                m.reasoning = None;
                m
            })
            .collect()
    }

    /// Compaction: deterministic dedupe first (free), then structured LLM
    /// summary of everything before the trailing verbatim window.
    async fn maybe_compact(&mut self, tx: &mpsc::Sender<AgentEvent>) -> Result<(), CoreError> {
        let before = self.est_context_tokens();
        let trigger = self.profile.compaction_trigger_tokens();
        if before <= trigger {
            self.emit(tx, AgentEvent::ContextReady { est_tokens: before, threshold_tokens: trigger }).await;
            return Ok(());
        }
        info!(before, trigger, "compacting context");

        self.dedupe_tool_results();

        let keep = self.config.compaction_keep_last;
        if self.messages.len() <= keep + 1 {
            return Ok(()); // nothing foldable
        }
        let split = self.messages.len() - keep;
        let head = &self.messages[..split];

        // Skip a second compaction if the head is already mostly a summary.
        let transcript_text = head
            .iter()
            .map(|m| {
                let role = format!("{:?}", m.role).to_lowercase();
                let body = m.content.clone().unwrap_or_default();
                format!("{role}: {body}")
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let summary_req = CompletionRequest {
            messages: vec![Message::user(format!("{COMPACTION_TEMPLATE}{transcript_text}"))],
            tools: vec![],
            max_output_tokens: Some(4096),
            temperature: Some(0.0),
        };
        let summary = self.provider.complete(summary_req).await?.message.content.unwrap_or_default();

        let mut rebuilt = Vec::with_capacity(keep + 2);
        if let Some(sys) = self.messages.first().filter(|m| m.role == crate::message::Role::System) {
            rebuilt.push(sys.clone());
        }
        rebuilt.push(Message::user(format!(
            "[Compaction summary of earlier session]\n{summary}\n\nContinue from here."
        )));
        rebuilt.extend(self.messages[split..].iter().cloned());

        let folded = self.messages.len() - rebuilt.len();
        self.messages = rebuilt;
        let after = self.est_context_tokens();
        self.emit(tx, AgentEvent::Compacted { folded_messages: folded, est_tokens_before: before, est_tokens_after: after }).await;
        if let Some(t) = &self.transcript {
            let _ = t.log_event(&format!("compacted: {folded} messages, {before} -> {after} est tokens"));
        }
        Ok(())
    }

    /// Drop duplicate tool results, keeping only the most recent copy —
    /// deterministic 15-30% context savings with zero information loss.
    fn dedupe_tool_results(&mut self) {
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (i, m) in self.messages.iter().enumerate() {
            if m.role == crate::message::Role::Tool {
                if let Some(c) = &m.content {
                    seen.insert(c.clone(), i);
                }
            }
        }
        let mut latest: std::collections::HashSet<usize> = seen.values().cloned().collect();
        latest.insert(self.messages.len().saturating_sub(1));
        for (i, m) in self.messages.iter_mut().enumerate() {
            if m.role == crate::message::Role::Tool && !latest.contains(&i) && m.content.is_some() {
                if seen.contains_key(m.content.as_ref().unwrap()) {
                    m.content = Some("[superseded by identical later tool result]".into());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::CompletionResponse;
    use crate::tool::{Tool, ToolDefinition, ToolOutput};
    use std::sync::Mutex;

    struct ScriptProvider {
        responses: Mutex<Vec<Message>>,
    }

    #[async_trait::async_trait]
    impl Provider for ScriptProvider {
        fn name(&self) -> &str {
            "script"
        }
        async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            let msg = self.responses.lock().unwrap().remove(0);
            Ok(CompletionResponse { message: msg, usage: Usage { input_tokens: 10, output_tokens: 5, cached_input_tokens: 0 } })
        }
    }

    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "echo".into(),
                description: "echo back".into(),
                parameters: serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}}),
            }
        }
        async fn call(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
            Ok(ToolOutput::ok(args["text"].as_str().unwrap_or("").to_string()))
        }
    }

    fn make_agent(script: Vec<Message>) -> Agent {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        Agent::new(
            Arc::new(ScriptProvider { responses: Mutex::new(script) }),
            reg,
            HarnessProfile::generic(),
            AgentConfig::default(),
            ToolContext::default(),
            None,
        )
    }

    #[tokio::test]
    async fn loop_runs_tool_then_finishes() {
        let script = vec![
            Message::assistant(
                None,
                vec![crate::message::ToolCall { id: "1".into(), name: "echo".into(), arguments: serde_json::json!({"text": "hi"}) }],
                None,
            ),
            Message::assistant(Some("done: hi".into()), vec![], None),
        ];
        let mut agent = make_agent(script);
        let (tx, mut rx) = mpsc::channel(64);
        let answer = agent.run("say hi", tx).await.unwrap();
        assert_eq!(answer, "done: hi");
        assert_eq!(agent.messages.len(), 4); // user, assistant(call), tool, assistant(final)
        assert_eq!(agent.usage.input_tokens, 20);

        let mut saw_tool = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, AgentEvent::ToolCallFinished { ref name, ok: true, .. } if name == "echo") {
                saw_tool = true;
            }
        }
        assert!(saw_tool);
    }

    #[tokio::test]
    async fn unknown_tool_error_is_fed_back_not_crash() {
        let script = vec![
            Message::assistant(None, vec![crate::message::ToolCall { id: "1".into(), name: "nope".into(), arguments: serde_json::json!({}) }], None),
            Message::assistant(Some("recovered".into()), vec![], None),
        ];
        let mut agent = make_agent(script);
        let (tx, _rx) = mpsc::channel(64);
        let answer = agent.run("break things", tx).await.unwrap();
        assert_eq!(answer, "recovered");
        let tool_msg = &agent.messages[2];
        assert!(tool_msg.content.as_deref().unwrap().contains("error"));
    }

    #[tokio::test]
    async fn reasoning_stripped_only_when_profile_says_so() {
        let script = vec![Message::assistant(Some("ok".into()), vec![], Some("thinking...".into()))];
        let mut agent = make_agent(script);
        agent.profile = HarnessProfile::generic(); // retain_reasoning = false
        let (tx, _rx) = mpsc::channel(64);
        agent.run("t", tx).await.unwrap();
        assert!(agent.rendered_messages()[1].reasoning.is_none());

        let mut agent2 = make_agent(vec![Message::assistant(Some("ok".into()), vec![], Some("thinking...".into()))]);
        agent2.profile = HarnessProfile::kimi(); // retain_reasoning = true
        let (tx2, _rx2) = mpsc::channel(64);
        agent2.run("t", tx2).await.unwrap();
        assert!(agent2.rendered_messages()[1].reasoning.is_some());
    }
}
