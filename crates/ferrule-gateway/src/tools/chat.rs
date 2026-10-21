//! M56.6: the chat's own moves as agent tools, next to `send_file`:
//! `ask_user` (a question with 2–4 buttons whose answer comes back as the
//! tool's result), `react` (a reaction on the message the turn answers)
//! and `edit_message` (the bot's own previous reply, edited). Each acts on
//! the session's own chat only, and says "not supported here" where the
//! channel can't.

use crate::channel::{send_with_buttons, Button, ButtonAction};
use crate::gateway::Interceptor;
use crate::health::is_command;
use crate::message::{InboundMessage, OutboundMessage};
use crate::router::Router;
use crate::tools::FileOut;
use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

/// The command a choice button sends: `/answer <code> <n>`.
const ANSWER: &str = "/answer";
/// How long `ask_user` waits by default, and at most: under the watchdog's
/// 10 minutes, which would otherwise report the waiting turn as stuck.
const WAIT_DEFAULT: Duration = Duration::from_secs(300);
const WAIT_MAX: Duration = Duration::from_secs(540);
/// A button's label, at most (Telegram shows about this much).
const LABEL_MAX: usize = 60;

/// The tools a chat session gets, beside `send_file`.
pub fn chat_tools(out: &Arc<FileOut>, session_id: &str) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(AskUserTool {
            out: out.clone(),
            session_id: session_id.to_string(),
        }),
        Arc::new(ReactTool {
            out: out.clone(),
            session_id: session_id.to_string(),
        }),
        Arc::new(EditMessageTool {
            out: out.clone(),
            session_id: session_id.to_string(),
        }),
    ]
}

fn failed(tool: &str, message: impl Into<String>) -> CoreError {
    CoreError::ToolFailed {
        tool: tool.into(),
        message: message.into(),
    }
}

/// The router, or why there's no chat.
fn router(out: &FileOut, tool: &str) -> Result<Arc<Router>, CoreError> {
    out.router()
        .ok_or_else(|| failed(tool, "there's no chat (the gateway isn't running)"))
}

/// What the person said to a question.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Said {
    /// A choice, by its index.
    Choice(usize),
    /// Their own words instead.
    Words(String),
}

struct Asked {
    code: String,
    channel: String,
    chat: String,
    choices: Vec<String>,
    tx: oneshot::Sender<Said>,
}

/// Questions `ask_user` is waiting on, and the [`Interceptor`] that hands
/// a chat's answer to the right one: a tapped button (`/answer k7 2`), a
/// choice's number or label typed, or any other text from that chat while
/// the question waits, as the person's own words. Register it with the
/// gateway ([`FileOut::questions`]).
#[derive(Default)]
pub struct Questions {
    waiting: Mutex<Vec<Asked>>,
    /// Codes that were answered or expired, so a late tap is told so.
    gone: Mutex<std::collections::VecDeque<String>>,
}

impl Questions {
    fn open(
        &self,
        channel: &str,
        chat: &str,
        choices: Vec<String>,
    ) -> (String, oneshot::Receiver<Said>) {
        let (tx, rx) = oneshot::channel();
        let mut waiting = self.waiting.lock().unwrap();
        let code = loop {
            let n = uuid::Uuid::new_v4().as_u128();
            let code = format!("q{}{}", (b'a' + (n % 26) as u8) as char, 2 + (n / 26) % 8);
            let taken =
                waiting.iter().any(|a| a.code == code) || self.gone.lock().unwrap().contains(&code);
            if !taken {
                break code;
            }
        };
        waiting.push(Asked {
            code: code.clone(),
            channel: channel.into(),
            chat: chat.into(),
            choices,
            tx,
        });
        (code, rx)
    }

    /// Takes question `code` back (it timed out, or the turn stopped).
    fn withdraw(&self, code: &str) {
        let mut waiting = self.waiting.lock().unwrap();
        if let Some(i) = waiting.iter().position(|a| a.code == code) {
            waiting.remove(i);
            self.forget(code);
        }
    }

    fn forget(&self, code: &str) {
        let mut gone = self.gone.lock().unwrap();
        gone.push_back(code.to_string());
        while gone.len() > 256 {
            gone.pop_front();
        }
    }

    /// Answers the waiting question at `i` with `said`.
    fn settle(&self, waiting: &mut Vec<Asked>, i: usize, said: Said) {
        let asked = waiting.remove(i);
        self.forget(&asked.code);
        let _ = asked.tx.send(said);
    }

    /// `msg` read as an answer: `Some(reply)` takes it (an empty reply
    /// says nothing back), `None` lets it through.
    fn answer(&self, msg: &InboundMessage) -> Option<String> {
        let text = msg.text.trim();
        let mut waiting = self.waiting.lock().unwrap();
        if is_command(text, ANSWER) {
            let mut words = text.split_whitespace().skip(1);
            let code = words.next().unwrap_or("").to_ascii_lowercase();
            let n: Option<usize> = words.next().and_then(|w| w.parse().ok());
            let Some(i) = waiting
                .iter()
                .position(|a| a.code == code && a.channel == msg.channel && a.chat == msg.chat_id)
            else {
                let gone = self.gone.lock().unwrap().contains(&code);
                return Some(if gone {
                    "That question was already answered or ran out of time; I went on without this answer.".into()
                } else {
                    "No question with that code is waiting here.".into()
                });
            };
            return match n.filter(|n| (1..=waiting[i].choices.len()).contains(n)) {
                Some(n) => {
                    self.settle(&mut waiting, i, Said::Choice(n - 1));
                    Some(String::new())
                }
                None => Some(format!(
                    "Pick 1 to {}, or just write your answer.",
                    waiting[i].choices.len()
                )),
            };
        }
        // Another command goes its way; a photo alone isn't an answer.
        if text.starts_with('/') || text.is_empty() {
            return None;
        }
        let i = waiting
            .iter()
            .rposition(|a| a.channel == msg.channel && a.chat == msg.chat_id)?;
        let lower = text.to_lowercase();
        let said = text
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=waiting[i].choices.len()).contains(n))
            .map(|n| n - 1)
            .or_else(|| {
                waiting[i]
                    .choices
                    .iter()
                    .position(|c| c.to_lowercase() == lower)
            })
            .map_or_else(|| Said::Words(text.to_string()), Said::Choice);
        self.settle(&mut waiting, i, said);
        Some(String::new())
    }
}

#[async_trait::async_trait]
impl Interceptor for Questions {
    async fn intercept(&self, msg: &InboundMessage) -> Option<String> {
        self.answer(msg)
    }
}

/// Withdraws its question however the call ends: answered, timed out, or
/// dropped by a `/stop`.
struct Withdraw<'a>(&'a Questions, String);

impl Drop for Withdraw<'_> {
    fn drop(&mut self) {
        self.0.withdraw(&self.1);
    }
}

pub struct AskUserTool {
    out: Arc<FileOut>,
    session_id: String,
}

#[async_trait::async_trait]
impl Tool for AskUserTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ask_user".into(),
            description: "Ask the person you're chatting with a question with 2 to 4 answers to pick \
                          from, shown as buttons in this chat, and wait for the answer, which comes back \
                          as this tool's result. They may also write their own answer. Use it when you \
                          need their decision to go on; if nobody answers in time, you're told so."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The question, short and complete" },
                    "choices": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 2,
                        "maxItems": 4,
                        "description": "2 to 4 short answers, one per button"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "How long to wait, in seconds (default 300, at most 540)"
                    }
                },
                "required": ["question", "choices"]
            }),
        }
    }

    fn changes_files(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        const TOOL: &str = "ask_user";
        let question = args["question"].as_str().unwrap_or("").trim().to_string();
        if question.is_empty() {
            return Err(failed(TOOL, "the question is empty"));
        }
        let choices: Vec<String> = args["choices"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|c| c.as_str())
                    .map(|c| c.trim().to_string())
                    .filter(|c| !c.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if !(2..=4).contains(&choices.len()) {
            return Err(failed(TOOL, "give 2 to 4 non-empty choices"));
        }
        if let Some(c) = choices.iter().find(|c| c.chars().count() > LABEL_MAX) {
            return Err(failed(
                TOOL,
                format!("a choice is longer than {LABEL_MAX} characters: {c}"),
            ));
        }
        let wait = args["timeout_secs"]
            .as_u64()
            .map_or(WAIT_DEFAULT, Duration::from_secs)
            .clamp(Duration::from_secs(10), WAIT_MAX);
        let router = router(&self.out, TOOL)?;
        let (channel_name, chat_id) = router
            .chat_of(&self.session_id)
            .ok_or_else(|| failed(TOOL, "this session isn't a chat, so there's nobody to ask"))?;
        let channel = router
            .channel(&channel_name)
            .ok_or_else(|| failed(TOOL, format!("the {channel_name} channel isn't running")))?;
        let questions = self.out.questions();
        let (code, rx) = questions.open(&channel_name, &chat_id, choices.clone());
        let _withdraw = Withdraw(&questions, code.clone());
        let buttons: Vec<Button> = choices
            .iter()
            .enumerate()
            .map(|(i, c)| Button {
                text: c.clone(),
                action: ButtonAction::Command(format!("{ANSWER} {code} {}", i + 1)),
            })
            .collect();
        let msg = OutboundMessage {
            channel: channel_name.clone(),
            chat_id,
            text: question,
            reply_to: None,
            attachments: vec![],
        };
        send_with_buttons(channel.as_ref(), msg, &buttons)
            .await
            .map_err(|e| failed(TOOL, format!("the question couldn't be sent: {e}")))?;
        let text = match tokio::time::timeout(wait, rx).await {
            Ok(Ok(Said::Choice(i))) => format!("They picked: {}", choices[i]),
            Ok(Ok(Said::Words(w))) => {
                format!("They answered in their own words (not one of the choices): {w}")
            }
            Ok(Err(_)) => "The question was withdrawn before an answer came.".to_string(),
            Err(_) => format!(
                "No answer within {} s; the question was withdrawn. Go on without it and say what you \
                 assumed, or ask again later.",
                wait.as_secs()
            ),
        };
        Ok(ToolOutput::ok(text))
    }
}

pub struct ReactTool {
    out: Arc<FileOut>,
    session_id: String,
}

#[async_trait::async_trait]
impl Tool for ReactTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "react".into(),
            description: "Put an emoji reaction on the message you're answering in this chat (a quick \
                          👍 or 🎉 instead of a line of text). Telegram takes only its standard reaction \
                          emoji; a channel without reactions says so."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "emoji": { "type": "string", "description": "One emoji, e.g. 👍" }
                },
                "required": ["emoji"]
            }),
        }
    }

    fn changes_files(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        const TOOL: &str = "react";
        let emoji = args["emoji"].as_str().unwrap_or("").trim().to_string();
        if emoji.is_empty() || emoji.chars().count() > 8 {
            return Err(failed(TOOL, "give one emoji"));
        }
        let router = router(&self.out, TOOL)?;
        let (channel_name, chat_id, message_id) =
            router.turn_message(&self.session_id).ok_or_else(|| {
                failed(
                    TOOL,
                    "there's no message from the person to react to in this turn",
                )
            })?;
        let channel = router
            .channel(&channel_name)
            .ok_or_else(|| failed(TOOL, format!("the {channel_name} channel isn't running")))?;
        if !channel.capabilities().reactions {
            return Err(failed(
                TOOL,
                format!("reactions aren't supported here ({channel_name})"),
            ));
        }
        channel
            .react(&chat_id, &message_id, &emoji)
            .await
            .map_err(|e| failed(TOOL, format!("the reaction failed: {e}")))?;
        Ok(ToolOutput::ok(format!("Reacted {emoji} to their message.")))
    }
}

pub struct EditMessageTool {
    out: Arc<FileOut>,
    session_id: String,
}

#[async_trait::async_trait]
impl Tool for EditMessageTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit_message".into(),
            description: "Replace the text of your previous reply in this chat (the last answer you \
                          sent before this turn), e.g. to fix a mistake in it. The new text replaces it \
                          whole. A channel that can't edit messages says so."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "The reply's new text" }
                },
                "required": ["text"]
            }),
        }
    }

    fn changes_files(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        const TOOL: &str = "edit_message";
        let text = args["text"].as_str().unwrap_or("").to_string();
        if text.trim().is_empty() {
            return Err(failed(TOOL, "the new text is empty"));
        }
        let router = router(&self.out, TOOL)?;
        let (channel_name, chat_id) = router.chat_of(&self.session_id).ok_or_else(|| {
            failed(
                TOOL,
                "this session isn't a chat, so there's no message to edit",
            )
        })?;
        let channel = router
            .channel(&channel_name)
            .ok_or_else(|| failed(TOOL, format!("the {channel_name} channel isn't running")))?;
        if !channel.capabilities().edits {
            return Err(failed(
                TOOL,
                format!("editing messages isn't supported here ({channel_name})"),
            ));
        }
        let id = router.last_reply(&self.session_id).ok_or_else(|| {
            failed(
                TOOL,
                "I don't know which message my previous reply was (none since the gateway \
                 started, or the channel gave no id); send a correction instead",
            )
        })?;
        channel
            .edit(&chat_id, &id, &text)
            .await
            .map_err(|e| failed(TOOL, format!("the edit failed: {e}")))?;
        Ok(ToolOutput::ok("Edited your previous reply."))
    }
}
