//! M27: a streamed Telegram reply against a mock Bot API — the first
//! message, throttled edits, a 429's `retry_after`, the rollover before
//! 4096, the final edit, and streaming off sending exactly what it did.

use async_trait::async_trait;
use ferrule_core::provider::{CompletionRequest, CompletionResponse};
use ferrule_core::tool::ToolContext;
use ferrule_core::{
    Agent, AgentConfig, CoreError, Delta, HarnessProfile, Message, Provider, ToolCall,
    ToolRegistry, Usage,
};
use ferrule_gateway::{
    AgentFactory, Channel, InboundMessage, Router, StreamPacing, TelegramChannel,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What the mock does to an `editMessageText`.
#[derive(Clone, Copy, PartialEq)]
enum Edits {
    Ok,
    /// The first edit gets a 429 with `retry_after: 1`.
    FirstIs429,
    /// Every edit is refused (not "not modified").
    Refused,
}

/// One Bot API call the mock saw.
#[derive(Debug, Clone)]
struct Call {
    method: String,
    body: Value,
    at: Instant,
    /// What it answered: a 429, an error, or ok.
    status: u16,
}

struct Bot {
    url: String,
    calls: Arc<Mutex<Vec<Call>>>,
}

impl Bot {
    fn start(edits: Edits) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let calls = Arc::new(Mutex::new(Vec::<Call>::new()));
        let seen = calls.clone();
        let next_id = AtomicUsize::new(100);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    head.push_str(&line);
                }
                let mut body = vec![0; length];
                let _ = reader.read_exact(&mut body);
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let method = head
                    .split_whitespace()
                    .nth(1)
                    .and_then(|path| path.rsplit('/').next())
                    .unwrap_or("")
                    .to_string();
                let edits_so_far = seen
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|c: &&Call| c.method == "editMessageText")
                    .count();
                let (status, reply) = match method.as_str() {
                    "sendMessage" => {
                        let id = next_id.fetch_add(1, Ordering::SeqCst);
                        (200, json!({"ok": true, "result": {"message_id": id}}))
                    }
                    "editMessageText" => match edits {
                        Edits::FirstIs429 if edits_so_far == 0 => (
                            429,
                            json!({"ok": false, "error_code": 429,
                                   "description": "Too Many Requests: retry after 1",
                                   "parameters": {"retry_after": 1}}),
                        ),
                        Edits::Refused => (
                            400,
                            json!({"ok": false, "error_code": 400,
                                   "description": "Bad Request: message can't be edited"}),
                        ),
                        _ => (200, json!({"ok": true, "result": true})),
                    },
                    _ => (200, json!({"ok": true, "result": []})),
                };
                seen.lock().unwrap().push(Call {
                    method,
                    body,
                    at: Instant::now(),
                    status,
                });
                let reply = reply.to_string();
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                        reply.len()
                    )
                    .as_bytes(),
                );
            }
        });
        Self { url, calls }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    /// What each message finally shows, by message id, in the order sent.
    fn shown(&self) -> Vec<String> {
        let mut ids = Vec::new();
        let mut text: HashMap<i64, String> = HashMap::new();
        let mut next = 100;
        for c in self.calls().iter().filter(|c| c.status == 200) {
            let t = c.body["text"].as_str().unwrap_or("").to_string();
            match c.method.as_str() {
                "sendMessage" => {
                    ids.push(next);
                    text.insert(next, t);
                    next += 1;
                }
                "editMessageText" => {
                    text.insert(c.body["message_id"].as_i64().unwrap(), t);
                }
                _ => {}
            }
        }
        ids.iter().map(|id| text[id].clone()).collect()
    }
}

/// Answers each call from `script`: the pieces it streams (with `gap`
/// between them) and whether it ends with a tool call.
struct Streamer {
    script: Vec<(Vec<String>, bool)>,
    gap: Duration,
    calls: AtomicUsize,
}

#[async_trait]
impl Provider for Streamer {
    fn name(&self) -> &str {
        "streamer"
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let (pieces, tool) = &self.script[n.min(self.script.len() - 1)];
        for p in pieces {
            if let Some(s) = &req.stream {
                s.send(Delta::Text(p.clone()));
            }
            tokio::time::sleep(self.gap).await;
        }
        let calls = if *tool {
            vec![ToolCall {
                id: format!("c{n}"),
                name: "missing".into(),
                arguments: json!({}),
            }]
        } else {
            vec![]
        };
        Ok(CompletionResponse {
            message: Message::assistant(Some(pieces.concat()), calls, None),
            usage: Usage::default(),
        })
    }
}

fn factory(script: Vec<(Vec<String>, bool)>, gap: Duration) -> AgentFactory {
    Arc::new(move |_sid, transcript| {
        Ok(Agent::new(
            Arc::new(Streamer {
                script: script.clone(),
                gap,
                calls: AtomicUsize::new(0),
            }),
            ToolRegistry::new(),
            HarnessProfile::generic(),
            AgentConfig::default(),
            ToolContext::default(),
            Some(transcript),
        )
        .with_system_prompt("test"))
    })
}

fn inbound() -> InboundMessage {
    InboundMessage {
        channel: "telegram".into(),
        chat_id: "9999".into(),
        sender: "max".into(),
        sender_id: None,
        message_id: "55".into(),
        text: "go".into(),
        attachments: vec![],
        reply_to: None,
        ts: 0,
    }
}

/// Runs one turn on a Telegram lane and returns the answer.
async fn turn(
    bot: &Bot,
    script: Vec<(Vec<String>, bool)>,
    gap: Duration,
    pacing: Option<StreamPacing>,
) -> String {
    let dir = tempfile::tempdir().unwrap();
    let telegram: Arc<dyn Channel> = Arc::new(TelegramChannel::with_base_url("T", &bot.url));
    let channels = HashMap::from([("telegram".to_string(), telegram)]);
    let mut router = Router::new(dir.path(), factory(script, gap), channels);
    if let Some(pacing) = pacing {
        router = router.with_streaming(["telegram".to_string()], pacing);
    }
    router.dispatch_and_wait(inbound()).await.unwrap().text
}

fn fast() -> StreamPacing {
    StreamPacing {
        first_after: Duration::from_millis(50),
        first_chars: 10_000,
        every: Duration::from_millis(150),
        limit: 4000,
    }
}

fn words(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("word{i} ")).collect()
}

fn delivering(calls: &[Call]) -> Vec<&Call> {
    calls
        .iter()
        .filter(|c| c.method == "sendMessage" || c.method == "editMessageText")
        .collect()
}

#[tokio::test]
async fn a_reply_is_sent_once_then_edited_no_faster_than_the_pace_and_ends_whole() {
    let bot = Bot::start(Edits::Ok);
    let answer = turn(
        &bot,
        vec![(words(20), false)],
        Duration::from_millis(40),
        Some(fast()),
    )
    .await;
    let calls = bot.calls();
    let calls = delivering(&calls);
    assert_eq!(calls[0].method, "sendMessage");
    assert_eq!(calls[0].body["reply_to_message_id"], 55);
    assert!(calls[1..].iter().all(|c| c.method == "editMessageText"
        && c.body["message_id"] == 100
        && c.body.get("reply_to_message_id").is_none()));
    // Throttled: fewer edits than deltas, and never two within the pace
    // (the final edit is exempt: it goes out when the answer is ready).
    assert!(calls.len() < 20, "{} calls", calls.len());
    for pair in calls[..calls.len() - 1].windows(2) {
        let gap = pair[1].at - pair[0].at;
        assert!(gap >= Duration::from_millis(140), "{gap:?}");
    }
    // Every preview is a prefix of the answer; the last call shows it all.
    for c in &calls {
        assert!(answer.starts_with(c.body["text"].as_str().unwrap().trim_end()));
    }
    assert_eq!(bot.shown(), [answer]);
}

#[tokio::test]
async fn a_429_holds_every_call_until_its_retry_after() {
    let bot = Bot::start(Edits::FirstIs429);
    let answer = turn(
        &bot,
        vec![(words(40), false)],
        Duration::from_millis(40),
        Some(fast()),
    )
    .await;
    let calls = bot.calls();
    let limited = calls.iter().position(|c| c.status == 429).expect("a 429");
    let after = &calls[limited + 1];
    assert!(
        after.at - calls[limited].at >= Duration::from_millis(990),
        "the next call came {:?} after the 429",
        after.at - calls[limited].at
    );
    assert_eq!(bot.shown(), [answer]);
}

#[tokio::test]
async fn a_long_reply_rolls_over_before_4096_and_the_final_edit_sets_every_part() {
    let bot = Bot::start(Edits::Ok);
    // 100 lines of 90 characters: 9000 in all, three messages.
    let lines: Vec<String> = (0..100)
        .map(|i| format!("{i:03} {}\n", "x".repeat(85)))
        .collect();
    let answer = turn(
        &bot,
        vec![(lines, false)],
        Duration::from_millis(5),
        Some(fast()),
    )
    .await;
    let shown = bot.shown();
    assert_eq!(
        shown.len(),
        3,
        "{:?}",
        shown.iter().map(|s| s.len()).collect::<Vec<_>>()
    );
    assert!(shown.iter().all(|s| s.encode_utf16().count() <= 4000));
    // Each break is at a line's end, and nothing is lost.
    assert!(shown[..2].iter().all(|s| s.ends_with('x')));
    assert_eq!(shown.join("\n"), answer);
    let sends: Vec<_> = bot
        .calls()
        .into_iter()
        .filter(|c| c.method == "sendMessage")
        .collect();
    assert_eq!(sends.len(), 3);
    assert_eq!(sends[0].body["reply_to_message_id"], 55);
    assert!(sends[1..]
        .iter()
        .all(|c| c.body.get("reply_to_message_id").is_none()));
}

#[tokio::test]
async fn a_preamble_is_replaced_by_the_answer_and_its_extra_parts_become_an_ellipsis() {
    let bot = Bot::start(Edits::Ok);
    // 80 lines of 90 characters: two messages, and the preview passes
    // 4000 characters about half a second before the preamble ends, so a
    // slow runner still gets an edit in after the rollover.
    let preamble: Vec<String> = (0..80)
        .map(|i| format!("{i:03} {}\n", "y".repeat(85)))
        .collect();
    let script = vec![(preamble, true), (vec!["All done.".into()], false)];
    // Slow enough that the preview rolls over into a second message.
    let answer = turn(&bot, script, Duration::from_millis(15), Some(fast())).await;
    assert_eq!(answer, "All done.");
    assert_eq!(bot.shown(), ["All done.", "…"]);
}

#[tokio::test]
async fn a_refused_edit_sends_the_answer_anew() {
    let bot = Bot::start(Edits::Refused);
    let answer = turn(
        &bot,
        vec![(words(20), false)],
        Duration::from_millis(40),
        Some(fast()),
    )
    .await;
    let calls = bot.calls();
    let last = calls.last().unwrap();
    assert_eq!(last.method, "sendMessage");
    assert_eq!(last.body["text"], answer.as_str());
    assert_eq!(last.body["reply_to_message_id"], 55);
}

#[tokio::test]
async fn a_quick_answer_and_streaming_off_both_send_exactly_todays_one_message() {
    let quick = Bot::start(Edits::Ok);
    let script = vec![(vec!["Hi there.".to_string()], false)];
    turn(
        &quick,
        script.clone(),
        Duration::ZERO,
        Some(StreamPacing::default()),
    )
    .await;
    let off = Bot::start(Edits::Ok);
    turn(&off, script, Duration::ZERO, None).await;
    let today = json!({"chat_id": "9999", "text": "Hi there.", "reply_to_message_id": 55});
    for bot in [quick, off] {
        let calls = bot.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].method, "sendMessage");
        assert_eq!(calls[0].body, today);
    }
}
