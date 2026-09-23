use crate::channel::Channel;
use crate::error::GatewayError;
use crate::message::InboundMessage;
use crate::router::Router;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Ties one or more channel adapters to a `Router`. Every adapter pushes
/// onto the same inbound funnel and runs concurrently as its own tokio task;
/// the gateway's only job is fan-in + dispatch, mirroring NanoClaw's
/// "host router" rather than a heavier broker.
pub struct Gateway {
    channels: Vec<Arc<dyn Channel>>,
    router: Router,
}

impl Gateway {
    pub fn new(router: Router) -> Self {
        Self { channels: Vec::new(), router }
    }

    pub fn add_channel(&mut self, channel: Arc<dyn Channel>) -> &mut Self {
        self.channels.push(channel);
        self
    }

    /// Runs until every channel adapter's inbound loop returns. A channel
    /// that errors is logged and dropped; the others keep serving. Returns
    /// once all adapters have stopped (e.g. every source is exhausted), so a
    /// gateway with only file-backed or stdin-backed adapters terminates
    /// naturally instead of hanging forever.
    pub async fn run(self) -> Result<(), GatewayError> {
        let (tx, mut rx) = mpsc::channel::<InboundMessage>(256);
        let mut handles = Vec::with_capacity(self.channels.len());
        for channel in &self.channels {
            let channel = channel.clone();
            let tx = tx.clone();
            handles.push(tokio::spawn(async move {
                let name = channel.name().to_string();
                if let Err(e) = channel.run(tx).await {
                    tracing::error!(channel = %name, error = %e, "channel adapter stopped");
                }
            }));
        }
        // Drop our own sender so `rx` closes once every adapter task above
        // has dropped its clone — i.e. once all channels are done.
        drop(tx);

        while let Some(msg) = rx.recv().await {
            if let Err(e) = self.router.dispatch(msg).await {
                tracing::error!(error = %e, "failed to dispatch inbound message");
            }
        }
        for h in handles {
            let _ = h.await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::OutboundMessage;
    use async_trait::async_trait;
    use ferrule_core::provider::{CompletionRequest, CompletionResponse};
    use ferrule_core::tool::ToolContext;
    use ferrule_core::{Agent, AgentConfig, CoreError, HarnessProfile, Message, Provider, ToolRegistry, Usage};
    use std::collections::HashMap;

    struct EchoProvider;
    #[async_trait]
    impl Provider for EchoProvider {
        fn name(&self) -> &str {
            "echo"
        }
        async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            let last_user = req.messages.iter().rev().find(|m| m.role == ferrule_core::Role::User).and_then(|m| m.content.clone()).unwrap_or_default();
            Ok(CompletionResponse { message: Message::assistant(Some(format!("echo: {last_user}")), vec![], None), usage: Usage::default() })
        }
    }

    /// A channel with a fixed, canned inbound script — stands in for a real
    /// adapter (stdin, HTTP polling) so `Gateway::run` can be tested without
    /// any external process or network.
    struct ScriptedChannel {
        script: Vec<InboundMessage>,
        sent: std::sync::Mutex<Vec<OutboundMessage>>,
    }
    #[async_trait]
    impl Channel for ScriptedChannel {
        fn name(&self) -> &str {
            "scripted"
        }
        async fn run(&self, tx: mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
            for m in self.script.clone() {
                tx.send(m).await.map_err(|_| GatewayError::Channel("closed".into()))?;
            }
            Ok(())
        }
        async fn send(&self, msg: OutboundMessage) -> Result<(), GatewayError> {
            self.sent.lock().unwrap().push(msg);
            Ok(())
        }
    }

    fn msg(chat_id: &str, text: &str) -> InboundMessage {
        InboundMessage { channel: "scripted".into(), chat_id: chat_id.into(), sender: "u".into(), message_id: "1".into(), text: text.into(), attachments: vec![], reply_to: None, ts: 0 }
    }

    #[tokio::test]
    async fn run_terminates_once_all_channels_are_exhausted_and_replies_are_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let scripted = Arc::new(ScriptedChannel { script: vec![msg("c1", "hello")], sent: std::sync::Mutex::new(Vec::new()) });
        let mut channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        channels.insert("scripted".into(), scripted.clone());

        let factory: crate::router::AgentFactory = Arc::new(|_sid, transcript| {
            Ok(Agent::new(Arc::new(EchoProvider), ToolRegistry::new(), HarnessProfile::generic(), AgentConfig::default(), ToolContext::default(), Some(transcript))
                .with_system_prompt("test"))
        });
        let router = Router::new(dir.path(), factory, channels);
        let mut gateway = Gateway::new(router);
        gateway.add_channel(scripted.clone());

        // The channel's `run` finishes after sending one message, which
        // closes the funnel and lets `Gateway::run` return on its own —
        // no manual shutdown signal needed for a source that terminates.
        // Note: `run` returning only means every inbound message has been
        // *dispatched* onto its session lane, not that the lane's spawned
        // task has finished the agent turn and delivered the reply yet —
        // so the assertion below polls with a bounded timeout rather than
        // checking immediately.
        tokio::time::timeout(std::time::Duration::from_secs(2), gateway.run()).await.unwrap().unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while scripted.sent.lock().unwrap().is_empty() {
            assert!(tokio::time::Instant::now() < deadline, "reply was never delivered");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(scripted.sent.lock().unwrap().len(), 1);
        assert_eq!(scripted.sent.lock().unwrap()[0].text, "echo: hello");
    }
}
