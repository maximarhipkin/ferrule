//! M43: `/goal` in any chat. The owner sends what "done" looks like; the
//! loop runs on its own router lane (a pseudo-channel, so the chat's lane
//! isn't held and nothing double-delivers — the scheduler's pattern), the
//! judge's verdicts land in the loop's state file (M42 part 5), and the
//! ending is reported back into the chat it started from.

use crate::goal;
use anyhow::Result;
use ferrule_gateway::{Channel, InboundMessage, OutboundMessage, Router};
use ferrule_trust::Hub;
use std::collections::HashMap;
use std::sync::Arc;

/// The pseudo-channel goal loops run on: never registered, so a lane's
/// own reply path no-ops and the door delivers to the real chat itself.
pub const GOAL_CHANNEL: &str = "goal";

/// The `/goal` door: starts and lists goal loops from a chat.
pub struct GoalDoor {
    pub hub: Arc<Hub>,
    pub router: Arc<Router>,
    /// The chat channels, for the completion report.
    pub channels: HashMap<String, Arc<dyn Channel>>,
}

impl GoalDoor {
    fn start(&self, msg: &InboundMessage, text: &str) -> Option<String> {
        let lane_chat = uuid::Uuid::new_v4().to_string();
        let session = ferrule_gateway::session::session_id(GOAL_CHANNEL, &lane_chat);
        let prepared =
            match goal::prepare_as(Some(text.to_string()), Vec::new(), None, Some(session)) {
                Ok(p) => p,
                Err(e) => return Some(format!("The goal didn't start: {e:#}")),
            };
        let judge = prepared
            .verify
            .iter()
            .map(|c| format!("`{c}`"))
            .collect::<Vec<_>>()
            .join(", then ");
        let router = self.router.clone();
        let channels = self.channels.clone();
        let back = (msg.channel.clone(), msg.chat_id.clone());
        let goal_text = prepared.prompt.clone();
        let sid = prepared.session_id.clone();
        let ack = format!(
            "Goal loop started: {}\nThe judge is {judge} — it decides, and I'll report here when it's met or stuck.\n\nSession `{}` — resume from the terminal with `ferrule run --resume {}`.",
            prepared.prompt, prepared.session_id, prepared.session_id
        );
        tokio::spawn(async move {
            let verdict = run_loop(&router, &lane_chat, &goal_text).await;
            let state = goal::load(
                &crate::config::data_dir()
                    .map(|d| d.join("sessions"))
                    .unwrap_or_default(),
                &sid,
            );
            let text = report(&goal_text, &sid, verdict, state.ok().flatten().as_ref());
            send(&channels, &back.0, &back.1, text).await;
        });
        Some(ack)
    }

    fn list(&self) -> String {
        let loops = goal::open_loops();
        if loops.is_empty() {
            return "No open goal loops. `/goal <what done looks like>` starts one — the judge comes from `[agent] verify_command`.".into();
        }
        let mut out = format!(
            "{} open goal loop{}:",
            loops.len(),
            if loops.len() == 1 { "" } else { "s" }
        );
        for (sid, state) in loops {
            let last = state
                .last_failure
                .as_deref()
                .and_then(|f| f.lines().next())
                .map(|l| {
                    let cut: String = l.chars().take(80).collect();
                    format!(" — last judge word: {cut}")
                })
                .unwrap_or_default();
            out.push_str(&format!(
                "\n• {} ({} judge run{}){last}\n  `{}`",
                state.goal,
                state.attempts,
                if state.attempts == 1 { "" } else { "s" },
                sid
            ));
        }
        out
    }
}

/// Runs the loop's first round on its lane and returns what came of it.
async fn run_loop(router: &Arc<Router>, lane_chat: &str, prompt: &str) -> Result<String> {
    let msg = InboundMessage {
        channel: GOAL_CHANNEL.into(),
        chat_id: lane_chat.into(),
        sender: "the owner".into(),
        sender_id: None,
        message_id: uuid::Uuid::new_v4().to_string(),
        text: prompt.to_string(),
        attachments: vec![],
        reply_to: None,
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    };
    router
        .dispatch_and_wait(msg)
        .await
        .map(|reply| reply.text)
        .map_err(|e| anyhow::anyhow!(e.to_string()))
}

/// The completion report for the chat the loop started from.
fn report(
    goal_text: &str,
    sid: &str,
    verdict: Result<String>,
    state: Option<&goal::LoopState>,
) -> String {
    let attempts = state.map(|s| s.attempts).unwrap_or(0);
    let satisfied = state.is_some_and(|s| s.judge_satisfied());
    match (satisfied, verdict) {
        (true, _) => format!(
            "✅ Goal met: {goal_text}\nThe judge passed after {attempts} run{}. (session `{sid}`)",
            if attempts == 1 { "" } else { "s" }
        ),
        (false, Ok(answer)) => {
            let tail = answer.lines().next().unwrap_or("").chars().take(200).collect::<String>();
            format!(
                "⏸ Goal pending: {goal_text}\nThe judge hasn't passed ({attempts} run{}). Last word: {tail}\nResume from the terminal: `ferrule run --resume {sid}`",
                if attempts == 1 { "" } else { "s" }
            )
        }
        (false, Err(e)) => format!(
            "⚠️ The goal loop errored: {goal_text}\n{e}\nResume from the terminal: `ferrule run --resume {sid}`"
        ),
    }
}

/// Delivers a message to a chat, for the doors' completion reports.
pub(crate) async fn send_to_chat(
    channels: &HashMap<String, Arc<dyn Channel>>,
    channel: &str,
    chat_id: &str,
    text: String,
) {
    let Some(channel) = channels.get(channel) else {
        return;
    };
    let out = OutboundMessage {
        channel: channel.name().to_string(),
        chat_id: chat_id.to_string(),
        text,
        reply_to: None,
        attachments: vec![],
    };
    if let Err(e) = channel.send(out).await {
        tracing::warn!(error = %e, "a door couldn't report to the chat");
    }
}

/// Delivers the loop's ending to the chat it started in.
async fn send(
    channels: &HashMap<String, Arc<dyn Channel>>,
    channel: &str,
    chat_id: &str,
    text: String,
) {
    send_to_chat(channels, channel, chat_id, text).await
}

#[async_trait::async_trait]
impl ferrule_gateway::Interceptor for GoalDoor {
    async fn intercept(&self, msg: &InboundMessage) -> Option<String> {
        if !crate::trust::is_chat_channel(&msg.channel) {
            return None;
        }
        let mut words = msg.text.split_whitespace();
        let cmd = words.next()?;
        let cmd = cmd.split('@').next().unwrap_or(cmd).to_lowercase();
        if cmd != "/goal" {
            return None;
        }
        if crate::trust::owner_in(&self.hub, msg) != Some(true) {
            return Some("Only the owner can start a goal loop.".into());
        }
        let text = words.collect::<Vec<_>>().join(" ");
        if text.trim().is_empty() {
            return Some(self.list());
        }
        self.start(msg, text.trim())
    }
}
