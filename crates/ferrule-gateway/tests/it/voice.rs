//! M41: a Telegram voice message, downloaded from a mock Bot API and
//! transcribed by a mock OpenAI-compatible service, reaches the agent as
//! text; with the service down, the agent gets the file and a note, and
//! the person the reason.

use async_trait::async_trait;
use ferrule_core::ledger::{LedgerRecord, LedgerSink};
use ferrule_core::provider::{CompletionRequest, CompletionResponse};
use ferrule_core::tool::ToolContext;
use ferrule_core::{
    Agent, AgentConfig, CoreError, HarnessProfile, Message, Provider, Role, ToolRegistry, Usage,
};
use ferrule_gateway::channels::files::Inbox;
use ferrule_gateway::transcribe::{Ledger, OpenAiTranscriber};
use ferrule_gateway::{
    AgentFactory, Channel, InboundMessage, Router, TelegramChannel, Transcription,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// The voice note's bytes: what Telegram serves and the service must get.
const OGG: &[u8] = b"OggS-not-really-a-voice-note";

/// One HTTP request a mock saw.
#[derive(Debug, Clone)]
struct Seen {
    path: String,
    head: String,
    body: Vec<u8>,
}

/// A one-thread HTTP server: `answer(path, body)` → (status, content type,
/// body).
fn serve(
    answer: impl Fn(&str, &[u8]) -> (u16, &'static str, Vec<u8>) + Send + 'static,
) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
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
            let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
            let (status, kind, reply) = answer(&path, &body);
            log.lock().unwrap().push(Seen { path, head, body });
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    reply.len()
                )
                .as_bytes(),
            );
            let _ = stream.write_all(&reply);
        }
    });
    (url, seen)
}

/// A Bot API with one voice note waiting, then nothing.
fn bot() -> (String, Arc<Mutex<Vec<Seen>>>) {
    let handed = Mutex::new(false);
    serve(move |path, _| {
        let ok = |result: Value| {
            (
                200,
                "application/json",
                json!({"ok": true, "result": result})
                    .to_string()
                    .into_bytes(),
            )
        };
        if path.starts_with("/file/botT/") {
            return (200, "audio/ogg", OGG.to_vec());
        }
        match path
            .split('?')
            .next()
            .unwrap_or("")
            .rsplit('/')
            .next()
            .unwrap_or("")
        {
            "getUpdates" => {
                let mut handed = handed.lock().unwrap();
                if *handed {
                    std::thread::sleep(Duration::from_millis(100));
                    return ok(json!([]));
                }
                *handed = true;
                ok(json!([{
                    "update_id": 1,
                    "message": {
                        "message_id": 41,
                        "date": 0,
                        "chat": {"id": 9999, "type": "private"},
                        "from": {"id": 9999, "first_name": "Max"},
                        "voice": {"file_id": "VOICE1", "duration": 14,
                                  "mime_type": "audio/ogg", "file_size": OGG.len()}
                    }
                }]))
            }
            "getFile" => ok(json!({"file_id": "VOICE1", "file_path": "voice/file_7.oga"})),
            "sendMessage" => ok(json!({"message_id": 500})),
            _ => ok(json!(true)),
        }
    })
}

/// Answers "echo: <what the agent was given>".
struct Echo;

#[async_trait]
impl Provider for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let last = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .and_then(|m| m.content.clone())
            .unwrap_or_default();
        Ok(CompletionResponse {
            message: Message::assistant(Some(format!("echo: {last}")), vec![], None),
            usage: Usage::default(),
        })
    }
}

fn factory() -> AgentFactory {
    Arc::new(|_sid, transcript| {
        Ok(Agent::new(
            Arc::new(Echo),
            ToolRegistry::new(),
            HarnessProfile::generic(),
            AgentConfig::default(),
            ToolContext::default(),
            Some(transcript),
        )
        .with_system_prompt("test"))
    })
}

#[derive(Default)]
struct Rows(Mutex<Vec<LedgerRecord>>);

impl LedgerSink for Rows {
    fn record(&self, record: LedgerRecord) {
        self.0.lock().unwrap().push(record);
    }
}

/// The voice update as the Telegram channel hands it on, then its turn.
/// Returns the agent's answer (what it was given, echoed).
async fn voice_turn(
    workspace: &std::path::Path,
    bot_url: &str,
    transcription: Transcription,
) -> String {
    let telegram = Arc::new(
        TelegramChannel::with_base_url("T", bot_url)
            .with_allowed_chats(vec![9999])
            .with_inbox(Some(Inbox::new(workspace, 20))),
    );
    let (tx, mut rx) = mpsc::channel::<InboundMessage>(8);
    let polling = {
        let telegram = telegram.clone();
        tokio::spawn(async move { telegram.run(tx).await })
    };
    let inbound = tokio::time::timeout(Duration::from_secs(20), rx.recv())
        .await
        .expect("the voice update arrives")
        .unwrap();
    polling.abort();
    assert_eq!(inbound.attachments.len(), 1, "{inbound:?}");
    assert_eq!(inbound.attachments[0].kind, "audio/ogg");

    let sessions = tempfile::tempdir().unwrap();
    let channels = HashMap::from([("telegram".to_string(), telegram as Arc<dyn Channel>)]);
    let router =
        Router::new(sessions.path(), factory(), channels).with_transcription(transcription);
    tokio::time::timeout(Duration::from_secs(30), router.dispatch_and_wait(inbound))
        .await
        .expect("the turn ends")
        .unwrap()
        .text
}

fn openai(base_url: String, key_env: Option<&str>, rows: &Arc<Rows>) -> Transcription {
    Transcription::On(Arc::new(OpenAiTranscriber {
        base_url,
        key_env: key_env.map(str::to_string),
        model: "whisper-1".into(),
        language: None,
        timeout: Duration::from_secs(10),
        ledger: Ledger {
            sink: Some(rows.clone() as Arc<dyn LedgerSink>),
            price_per_minute: Some(0.006),
        },
        client: reqwest::Client::new(),
    }))
}

#[tokio::test]
async fn a_telegram_voice_message_reaches_the_agent_as_its_transcript() {
    let workspace = tempfile::tempdir().unwrap();
    let (bot_url, bot_seen) = bot();
    let (stt_url, stt_seen) = serve(|path, _| {
        assert_eq!(path, "/v1/audio/transcriptions");
        (
            200,
            "application/json",
            json!({"text": "שלום, תזכיר לי לקנות חלב", "duration": 14.2, "language": "hebrew"})
                .to_string()
                .into_bytes(),
        )
    });
    std::env::set_var("FERRULE_IT_VOICE_KEY", "sk-voice-test");
    let rows = Arc::new(Rows::default());
    let answer = voice_turn(
        workspace.path(),
        &bot_url,
        openai(format!("{stt_url}/v1"), Some("FERRULE_IT_VOICE_KEY"), &rows),
    )
    .await;

    assert!(
        answer.starts_with("echo: [voice message, 0:14, transcribed]: שלום, תזכיר לי לקנות חלב"),
        "{answer}"
    );
    assert!(
        answer.contains("inbox/telegram/"),
        "the file's path stays in: {answer}"
    );

    let stt = stt_seen.lock().unwrap().clone();
    assert_eq!(stt.len(), 1, "{stt:?}");
    let head = stt[0].head.to_ascii_lowercase();
    assert!(
        head.contains("authorization: bearer sk-voice-test"),
        "{head}"
    );
    let body = String::from_utf8_lossy(&stt[0].body);
    assert!(body.contains("-voice.ogg\""), "{body}");
    assert!(body.contains("whisper-1"), "{body}");
    assert!(body.contains("verbose_json"), "{body}");
    assert!(
        !body.contains("name=\"language\""),
        "nothing pins a language unless the config does: {body}"
    );
    assert!(
        stt[0].body.windows(OGG.len()).any(|w| w == OGG),
        "the voice note goes up as it came"
    );

    let rows = rows.0.lock().unwrap().clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].call_kind, "transcription");
    assert_eq!(rows[0].outcome, "ok");
    let cost = rows[0].cost_usd.unwrap();
    assert!((cost - 14.2 / 60.0 * 0.006).abs() < 1e-9, "{cost}");

    let bot = bot_seen.lock().unwrap().clone();
    assert!(
        !bot.iter().any(|s| s.path.ends_with("/sendMessage")
            && !String::from_utf8_lossy(&s.body).contains("echo: ")),
        "nothing to tell the person when it worked: {bot:?}"
    );
}

#[tokio::test]
async fn with_the_service_down_the_agent_gets_the_file_and_the_person_the_reason() {
    let workspace = tempfile::tempdir().unwrap();
    let (bot_url, bot_seen) = bot();
    let down = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://127.0.0.1:{}/v1", l.local_addr().unwrap().port())
    };
    let rows = Arc::new(Rows::default());
    let answer = voice_turn(
        workspace.path(),
        &bot_url,
        openai(down.clone(), None, &rows),
    )
    .await;

    assert!(
        answer.starts_with(
            "echo: [voice message: not transcribed: couldn't reach the transcription service at"
        ),
        "{answer}"
    );
    assert!(answer.contains("inbox/telegram/"), "{answer}");
    let saved: Vec<_> = walk(workspace.path());
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert_eq!(std::fs::read(&saved[0]).unwrap(), OGG);

    let told: Vec<Value> = bot_seen
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.path.ends_with("/sendMessage"))
        .map(|s| serde_json::from_slice::<Value>(&s.body).unwrap())
        .filter(|m| !m["text"].as_str().unwrap().starts_with("echo: "))
        .collect();
    assert_eq!(told.len(), 1, "{told:?}");
    let text = told[0]["text"].as_str().unwrap();
    assert!(
        text.starts_with(
            "I couldn't transcribe your voice message: couldn't reach the transcription service at"
        ) && text.contains(&down)
            && text.contains("refused"),
        "{text}"
    );
    let rows = rows.0.lock().unwrap().clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, "error");
}

#[tokio::test]
async fn with_transcription_off_the_person_is_told_how_to_turn_it_on() {
    let workspace = tempfile::tempdir().unwrap();
    let (bot_url, bot_seen) = bot();
    let answer = voice_turn(
        workspace.path(),
        &bot_url,
        Transcription::Off {
            how: "voice transcription is off; set OPENAI_API_KEY".into(),
        },
    )
    .await;
    assert!(answer.contains("inbox/telegram/"), "{answer}");
    let told: Vec<Value> = bot_seen
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.path.ends_with("/sendMessage"))
        .map(|s| serde_json::from_slice::<Value>(&s.body).unwrap())
        .filter(|m| !m["text"].as_str().unwrap().starts_with("echo: "))
        .collect();
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(
        told[0]["text"],
        "voice transcription is off; set OPENAI_API_KEY"
    );
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}
