//! The HTTP API (M39 §8), hermetically: the channel on 127.0.0.1:0, a
//! stand-in for the router answering what it hears, and a webhook
//! receiver of our own.

use ferrule_gateway::channels::files::Inbox;
use ferrule_gateway::channels::hmac;
use ferrule_gateway::channels::http::{clients, HttpChannel, HttpConfig};
use ferrule_gateway::{Attachment, Button, ButtonAction, Channel, InboundMessage, OutboundMessage};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

struct Api {
    ch: Arc<HttpChannel>,
    rx: mpsc::Receiver<InboundMessage>,
    base: String,
    key: String,
    dir: tempfile::TempDir,
    _task: tokio::task::JoinHandle<()>,
}

fn config(dir: &std::path::Path, rate: u32) -> HttpConfig {
    HttpConfig {
        dir: dir.join("http"),
        bind: [127, 0, 0, 1].into(),
        port: 0,
        requests_per_minute: rate,
        tunnel: None,
        inbox: Some(Inbox::new(dir.join("ws"), 1)),
    }
}

async fn start_with(dir: tempfile::TempDir, rate: u32, webhook: Option<&str>) -> Api {
    let made = clients::add(&dir.path().join("http"), "ci", webhook).unwrap();
    let ch = Arc::new(
        HttpChannel::new(config(dir.path(), rate))
            .with_timing(vec![Duration::ZERO; 3], Duration::from_millis(100)),
    );
    let (tx, rx) = mpsc::channel(16);
    let run = ch.clone();
    let task = tokio::spawn(async move {
        run.run(tx).await.unwrap();
    });
    while ch.port() == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Api {
        base: format!("http://127.0.0.1:{}", ch.port()),
        ch,
        rx,
        key: made.key,
        dir,
        _task: task,
    }
}

async fn start() -> Api {
    start_with(tempfile::tempdir().unwrap(), 30, None).await
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

impl Api {
    fn post(&self, body: Value) -> reqwest::RequestBuilder {
        http()
            .post(format!("{}/v1/messages", self.base))
            .bearer_auth(&self.key)
            .json(&body)
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        http()
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.key)
    }

    async fn next(&mut self) -> InboundMessage {
        tokio::time::timeout(Duration::from_secs(5), self.rx.recv())
            .await
            .expect("a message in")
            .unwrap()
    }

    fn reply(&self, to: &InboundMessage, text: &str) -> OutboundMessage {
        OutboundMessage {
            channel: "http".into(),
            chat_id: to.chat_id.clone(),
            text: text.into(),
            reply_to: Some(to.message_id.clone()),
            attachments: vec![],
        }
    }

    fn notice(&self, chat: &str, text: &str) -> OutboundMessage {
        OutboundMessage {
            channel: "http".into(),
            chat_id: chat.into(),
            text: text.into(),
            reply_to: None,
            attachments: vec![],
        }
    }
}

#[tokio::test]
async fn a_request_waits_for_its_answer() {
    let mut api = start().await;
    let req = tokio::spawn(
        api.post(json!({"text": "hi", "conversation": "build-7"}))
            .send(),
    );
    let got = api.next().await;
    assert_eq!(got.channel, "http");
    assert_eq!(got.chat_id, "ci/build-7");
    assert_eq!(got.sender_id.as_deref(), Some("ci"));
    assert_eq!(got.text, "hi");
    api.ch.send(api.reply(&got, "hello")).await.unwrap();
    api.ch.answered(&got.chat_id, &got.message_id).await;
    let resp = req.await.unwrap().unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["id"], got.message_id.as_str());
    assert_eq!(v["conversation"], "build-7");
    assert_eq!(v["text"], "hello");
    assert_eq!(v["files"], json!([]));

    // No conversation: the client's own chat.
    let req = tokio::spawn(api.post(json!({"text": "again"})).send());
    let got = api.next().await;
    assert_eq!(got.chat_id, "ci");
    api.ch.answered(&got.chat_id, &got.message_id).await;
    let v: Value = req.await.unwrap().unwrap().json().await.unwrap();
    assert_eq!(v["text"], "");
    assert_eq!(v["conversation"], Value::Null);
    assert!(!api.ch.busy_notices());
    assert!(!api.ch.polls());
    assert!(api.ch.note().unwrap().contains("127.0.0.1:"));
}

#[tokio::test]
async fn a_key_that_is_unknown_or_revoked_gets_a_401_and_nothing_more() {
    let api = start().await;
    let resp = http()
        .post(format!("{}/v1/messages", api.base))
        .bearer_auth("frk_not-a-key")
        .json(&json!({"text": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    assert_eq!(
        resp.headers()["www-authenticate"],
        "Bearer realm=\"ferrule\""
    );
    let body = resp.text().await.unwrap();
    assert_eq!(body, r#"{"error":"unknown or revoked key"}"#);
    let none = http()
        .get(format!("{}/v1/events", api.base))
        .send()
        .await
        .unwrap();
    assert_eq!(none.status(), 401);
    assert_eq!(api.get("/v1/events").send().await.unwrap().status(), 200);
    // Revoked: refused at once, without a restart.
    assert!(clients::revoke(&api.dir.path().join("http"), "ci").unwrap());
    assert_eq!(api.get("/v1/events").send().await.unwrap().status(), 401);
}

#[tokio::test]
async fn bad_requests_are_said_plainly() {
    let api = start().await;
    let status = |r: reqwest::Response| r.status().as_u16();
    let big = "x".repeat(65 * 1024);
    assert_eq!(
        status(api.post(json!({"text": big})).send().await.unwrap()),
        413
    );
    let r = api.get("/v1/messages").send().await.unwrap();
    assert_eq!(status(r), 405);
    let r = http()
        .post(format!("{}/v1/messages", api.base))
        .bearer_auth(&api.key)
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("must be JSON"));
    assert_eq!(
        status(api.post(json!({"text": "  "})).send().await.unwrap()),
        400
    );
    let r = api
        .post(json!({"text": "x", "conversation": "a/b"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(status(api.get("/v2/else").send().await.unwrap()), 404);
    assert_eq!(status(api.get("/v1/files/nope").send().await.unwrap()), 404);
}

/// The SSE stream: `accepted`, a `delta` per post or edit (the whole text
/// so far), an approval ask sent meanwhile as `message`, then `done`.
#[tokio::test]
async fn sse_streams_the_answer_as_it_grows() {
    let mut api = start().await;
    let resp = api
        .post(json!({"text": "count"}))
        .header("accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let got = api.next().await;
    let posted = api
        .ch
        .post(api.reply(&got, "one"))
        .await
        .unwrap()
        .expect("an id to edit");
    api.ch.edit("ci", &posted, "one two").await.unwrap();
    let ask = OutboundMessage {
        text: "Allow `rm`?".into(),
        ..api.notice("ci", "")
    };
    api.ch
        .send_buttons(
            ask,
            &[
                Button {
                    text: "Allow".into(),
                    action: ButtonAction::Command("yes a1b2".into()),
                },
                Button {
                    text: "Refuse".into(),
                    action: ButtonAction::Command("no a1b2".into()),
                },
            ],
        )
        .await
        .unwrap();
    api.ch.edit("ci", &posted, "one two three").await.unwrap();
    api.ch.answered("ci", &got.message_id).await;
    let body = tokio::time::timeout(Duration::from_secs(5), resp.text())
        .await
        .unwrap()
        .unwrap();
    let events: Vec<(String, Value)> = body
        .split("\n\n")
        .filter_map(|block| {
            let ev = block.lines().find_map(|l| l.strip_prefix("event: "))?;
            let data = block.lines().find_map(|l| l.strip_prefix("data: "))?;
            Some((ev.to_string(), serde_json::from_str(data).unwrap()))
        })
        .collect();
    let names: Vec<&str> = events.iter().map(|(e, _)| e.as_str()).collect();
    assert_eq!(
        names,
        ["accepted", "delta", "delta", "message", "delta", "done"],
        "{body}"
    );
    assert_eq!(events[0].1["id"], got.message_id.as_str());
    assert_eq!(events[1].1["text"], "one");
    assert_eq!(events[2].1["text"], "one two");
    assert_eq!(events[3].1["text"], "Allow `rm`?");
    assert_eq!(
        events[3].1["choices"],
        json!([{"label":"Allow","reply":"yes a1b2"},{"label":"Refuse","reply":"no a1b2"}])
    );
    assert_eq!(events[5].1["text"], "one two three");
    // The ask is in the outbox too, for a client that isn't streaming.
    let v: Value = api
        .get("/v1/events")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["events"][0]["choices"][0]["reply"], "yes a1b2");
    assert_eq!(v["last"], 1);
}

#[tokio::test]
async fn the_outbox_holds_what_no_request_waits_for_and_long_polls() {
    let mut api = start().await;
    api.ch
        .send(api.notice("ci", "task done: 3 files"))
        .await
        .unwrap();
    api.ch
        .send(api.notice("ci/nightly", "nightly: green"))
        .await
        .unwrap();
    let v: Value = api
        .get("/v1/events?after=0")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["last"], 2);
    assert_eq!(v["events"][0]["n"], 1);
    assert_eq!(v["events"][0]["text"], "task done: 3 files");
    assert_eq!(v["events"][1]["conversation"], "nightly");
    let v: Value = api
        .get("/v1/events?after=2")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["events"], json!([]));

    // A long-poll returns when something arrives.
    let poll = tokio::spawn(api.get("/v1/events?after=2&wait=10").send());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!poll.is_finished());
    api.ch.send(api.notice("ci", "later")).await.unwrap();
    let v: Value = tokio::time::timeout(Duration::from_secs(5), poll)
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["events"][0]["text"], "later");
    assert_eq!(v["events"][0]["n"], 3);

    // A request whose client hung up: its answer lands in the outbox.
    let req = tokio::spawn(api.post(json!({"text": "slow"})).send());
    let got = api.next().await;
    req.abort();
    tokio::time::sleep(Duration::from_millis(200)).await;
    api.ch.send(api.reply(&got, "finally")).await.unwrap();
    api.ch.answered("ci", &got.message_id).await;
    let v: Value = api
        .get("/v1/events?after=3")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["events"][0]["text"], "finally");
    assert_eq!(v["events"][0]["reply_to"], got.message_id.as_str());

    // It lasts across a restart, and the numbers go on.
    let dir = api.dir.path().to_path_buf();
    let again = HttpChannel::new(config(&dir, 30));
    again.send(api.notice("ci", "after restart")).await.unwrap();
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("http/state.json")).unwrap())
            .unwrap();
    let ci = state["outbox"]["ci"].as_array().unwrap();
    assert_eq!(ci.last().unwrap()["n"], 5);
    assert!(state["last_used"]["ci"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn files_go_out_as_links_for_their_client_and_come_in_to_the_inbox() {
    let mut api = start().await;
    let file = api.dir.path().join("report.csv");
    std::fs::write(&file, "a,b\n1,2\n").unwrap();
    let req = tokio::spawn(api.post(json!({"text": "the report?"})).send());
    let got = api.next().await;
    let mut out = api.reply(&got, "here");
    out.attachments = vec![Attachment {
        kind: "document".into(),
        url: file.to_string_lossy().into_owned(),
        name: None,
    }];
    api.ch.send(out).await.unwrap();
    api.ch.answered("ci", &got.message_id).await;
    let v: Value = req.await.unwrap().unwrap().json().await.unwrap();
    assert_eq!(v["files"][0]["name"], "report.csv");
    let url = v["files"][0]["url"].as_str().unwrap().to_string();
    assert!(url.starts_with("/v1/files/"));
    let r = api.get(&url).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "text/csv");
    assert_eq!(r.text().await.unwrap(), "a,b\n1,2\n");
    // Another client's key doesn't open it.
    let other = clients::add(&api.dir.path().join("http"), "other", None).unwrap();
    let r = http()
        .get(format!("{}{url}", api.base))
        .bearer_auth(&other.key)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // In: base64 to the inbox, with a note for the agent.
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG....");
    let big = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 50 * 1024]);
    let req = tokio::spawn(
        api.post(json!({"text": "look", "files": [
            {"name": "shot.png", "data": data},
        ]}))
        .send(),
    );
    let got = api.next().await;
    assert!(got.text.contains("look"), "{}", got.text);
    assert!(got.text.contains("inbox/http/"), "{}", got.text);
    assert_eq!(got.attachments.len(), 1);
    assert_eq!(got.attachments[0].kind, "image/png");
    assert!(std::path::Path::new(&got.attachments[0].url).exists());
    api.ch.answered("ci", &got.message_id).await;
    req.await.unwrap().unwrap();
    // A body over 64 KiB is refused whole.
    let r = api
        .post(json!({"text": "x", "files": [{"name": "b.bin", "data": big}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 413);
}

#[tokio::test]
async fn over_the_rate_is_a_429_with_retry_after() {
    let api = start_with(tempfile::tempdir().unwrap(), 2, None).await;
    for _ in 0..2 {
        assert_eq!(api.get("/v1/events").send().await.unwrap().status(), 200);
    }
    let r = api.get("/v1/events").send().await.unwrap();
    assert_eq!(r.status(), 429);
    let wait: u64 = r.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&wait));
}

/// A webhook receiver: the bodies and signatures it got; `fail` answers
/// 500 to everything.
async fn receiver(fail: bool) -> (String, Arc<Mutex<Vec<(Vec<u8>, String)>>>) {
    let got = Arc::new(Mutex::new(vec![]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://127.0.0.1:{}/hook",
        listener.local_addr().unwrap().port()
    );
    let seen = got.clone();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![];
            let mut chunk = [0u8; 4096];
            let (head, body) = loop {
                let n = sock.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .map_or(0, |v| v.trim().parse().unwrap());
                    while buf.len() < i + 4 + len {
                        let n = sock.read(&mut chunk).await.unwrap();
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    break (head, buf[i + 4..i + 4 + len].to_vec());
                }
            };
            let sig = head
                .lines()
                .find_map(|l| l.strip_prefix("x-ferrule-signature:"))
                .unwrap_or("")
                .trim()
                .to_string();
            seen.lock().unwrap().push((body, sig));
            let status = if fail { "500 Oops" } else { "204 No Content" };
            let _ = sock
                .write_all(format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\n\r\n").as_bytes())
                .await;
        }
    });
    (url, got)
}

#[tokio::test]
async fn outbox_messages_go_to_the_webhook_signed() {
    let (url, got) = receiver(false).await;
    let dir = tempfile::tempdir().unwrap();
    let api = start_with(dir, 30, Some(&url)).await;
    let secret = clients::load(&api.dir.path().join("http")).unwrap()[0]
        .webhook_secret
        .clone()
        .unwrap();
    api.ch
        .send(api.notice("ci/nightly", "done: 2 passed"))
        .await
        .unwrap();
    for _ in 0..200 {
        if !got.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (body, sig) = got.lock().unwrap()[0].clone();
    assert!(hmac::verify(secret.as_bytes(), &body, &sig), "{sig}");
    assert!(!hmac::verify(b"frk_the-api-key-is-not-it", &body, &sig));
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["client"], "ci");
    assert_eq!(v["conversation"], "nightly");
    assert_eq!(v["text"], "done: 2 passed");
    assert_eq!(v["n"], 1);
    assert!(api.ch.problem().is_none());
}

#[tokio::test]
async fn a_webhook_that_keeps_failing_is_retried_then_a_problem() {
    let (url, got) = receiver(true).await;
    let api = start_with(tempfile::tempdir().unwrap(), 30, Some(&url)).await;
    api.ch.send(api.notice("ci", "result")).await.unwrap();
    for _ in 0..300 {
        if api.ch.problem().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let p = api.ch.problem().expect("a problem");
    assert!(
        p.contains("webhook of `ci` failed (it answered 500)"),
        "{p}"
    );
    assert!(p.contains("stays in its outbox"), "{p}");
    assert_eq!(got.lock().unwrap().len(), 3);
    // Still there to fetch.
    let v: Value = api
        .get("/v1/events")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["events"][0]["text"], "result");
}

#[tokio::test]
async fn a_port_in_use_is_said_plainly() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), 30);
    cfg.port = taken.local_addr().unwrap().port();
    let (tx, _rx) = mpsc::channel(1);
    let err = HttpChannel::new(cfg).run(tx).await.unwrap_err().to_string();
    assert!(err.contains("couldn't listen on 127.0.0.1:"), "{err}");
    assert!(err.contains("[gateway.http] port"), "{err}");
}

#[tokio::test]
async fn an_unreadable_clients_file_lets_no_one_in_and_says_so() {
    let api = start().await;
    std::fs::write(api.dir.path().join("http/clients.json"), "{ nope").unwrap();
    assert_eq!(api.get("/v1/events").send().await.unwrap().status(), 401);
    assert!(api
        .ch
        .problem()
        .unwrap()
        .contains("clients.json can't be read"));
}
