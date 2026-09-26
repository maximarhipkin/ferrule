//! M31: the Slack adapter against a mock Socket Mode socket and Web API —
//! every envelope acked before anything else, redeliveries dropped, fresh
//! connections on request and after a drop, who gets in, threads, the
//! mrkdwn conversion, rate limits, Block Kit approvals, the slash command,
//! streaming, and the tokens staying out of logs.

mod support;

use ferrule_core::provider::{CompletionRequest, CompletionResponse};
use ferrule_core::tool::ToolContext;
use ferrule_core::{
    Agent, AgentConfig, CoreError, Delta, HarnessProfile, Message, Provider, ToolRegistry, Usage,
};
use ferrule_gateway::channel::{Button, ButtonAction};
use ferrule_gateway::channels::slack::{self as sl, SlackChannel};
use ferrule_gateway::{
    AgentFactory, Channel, GatewayError, InboundMessage, OutboundMessage, Router, StreamPacing,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::http::Response;
use support::slack::{self as mock, Slack, APP_TOKEN, BOT, BOT_TOKEN, TICKET};
use support::until;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const T: Duration = Duration::from_secs(10);

struct Running {
    ch: Arc<SlackChannel>,
    rx: mpsc::Receiver<InboundMessage>,
    run: JoinHandle<Result<(), GatewayError>>,
}

fn channel(s: &Slack, users: &[&str], channels: &[&str]) -> SlackChannel {
    SlackChannel::with_api(BOT_TOKEN, APP_TOKEN, &s.api).with_allowed(
        users.iter().map(|s| s.to_string()).collect(),
        channels.iter().map(|s| s.to_string()).collect(),
    )
}

fn spawn(ch: SlackChannel, capacity: usize) -> Running {
    let ch = Arc::new(ch.with_fast_retries());
    let (tx, rx) = mpsc::channel(capacity);
    let c = ch.clone();
    let run = tokio::spawn(async move { c.run(tx).await });
    Running { ch, rx, run }
}

async fn start(s: &Slack, users: &[&str], channels: &[&str]) -> Running {
    let r = spawn(channel(s, users, channels), 32);
    until("the socket", T, || s.state().connections >= 1).await;
    until("hello handled", T, || r.ch.last_ok_poll().is_some()).await;
    r
}

async fn recv(r: &mut Running) -> InboundMessage {
    tokio::time::timeout(T, r.rx.recv())
        .await
        .expect("a message in time")
        .expect("the channel open")
}

/// Nothing arrives for a moment.
async fn quiet(r: &mut Running) {
    if let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(300), r.rx.recv()).await {
        panic!("expected nothing, got {m:?}");
    }
}

fn out(chat: &str, text: &str) -> OutboundMessage {
    OutboundMessage {
        channel: "slack".into(),
        chat_id: chat.into(),
        text: text.into(),
        reply_to: None,
        attachments: vec![],
    }
}

fn filler() -> InboundMessage {
    InboundMessage {
        channel: "test".into(),
        chat_id: "x".into(),
        sender: "x".into(),
        sender_id: None,
        message_id: String::new(),
        text: "filler".into(),
        attachments: vec![],
        reply_to: None,
        ts: 0,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_envelope_is_acked_before_its_message_reaches_the_router() {
    let s = Slack::start();
    // A full queue: the adapter can't hand the message on until the test
    // drains it, so an ack seen meanwhile came first.
    let (tx, mut rx) = mpsc::channel(1);
    tx.send(filler()).await.unwrap();
    let ch = Arc::new(channel(&s, &["U1"], &[]).with_fast_retries());
    let c = ch.clone();
    tokio::spawn(async move { c.run(tx).await });
    until("the socket", T, || s.state().connections >= 1).await;
    let env = s.event("Ev1", mock::dm("U1", "1790000001.000100", "hi"));
    until("the ack", T, || s.state().acks().contains(&env)).await;
    assert_eq!(rx.recv().await.unwrap().text, "filler");
    let m = tokio::time::timeout(T, rx.recv()).await.unwrap().unwrap();
    assert_eq!(m.text, "hi");
    // A non-event envelope is acked too.
    let other = s.envelope("some_future_type", json!({}));
    until("its ack", T, || s.state().acks().contains(&other)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_allowed_dm_round_trips_unthreaded_and_gets_eyes() {
    let s = Slack::start();
    let mut r = start(&s, &["U1"], &[]).await;
    s.event(
        "Ev1",
        mock::dm(
            "U1",
            "1790000001.000100",
            "is 1 &lt; 2 &amp;&amp; 3 &gt; 2?",
        ),
    );
    let m = recv(&mut r).await;
    assert_eq!(m.channel, "slack");
    assert_eq!(m.chat_id, "U1", "a DM's chat is its user");
    assert_eq!(m.sender, "name-U1");
    assert_eq!(m.sender_id.as_deref(), Some("U1"));
    assert_eq!(m.message_id, "1790000001.000100");
    assert_eq!(m.text, "is 1 < 2 && 3 > 2?", "Slack's escapes undone");
    assert_eq!(m.ts, 1_790_000_001);
    r.ch.react("U1", &m.message_id, "👀").await.unwrap();
    r.ch.send(out("U1", "**yes**, see [docs](https://x.io)"))
        .await
        .unwrap();
    let st = s.state();
    assert!(
        st.calls("conversations.open").is_empty(),
        "the DM channel was learned from the message"
    );
    let posts = st.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["channel"], "DU1");
    assert!(posts[0].get("thread_ts").is_none(), "a DM isn't threaded");
    assert_eq!(posts[0]["text"], "*yes*, see <https://x.io|docs>");
    let react = st.calls("reactions.add");
    assert_eq!(
        react[0].json(),
        json!({ "channel": "DU1", "timestamp": "1790000001.000100", "name": "eyes" })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_to_an_owner_never_seen_opens_the_dm_first() {
    let s = Slack::start();
    let r = start(&s, &["U1"], &[]).await;
    r.ch.send(out("U1", "one")).await.unwrap();
    r.ch.send(out("U1", "two")).await.unwrap();
    let st = s.state();
    let opens = st.calls("conversations.open");
    assert_eq!(opens.len(), 1, "opened once, then remembered");
    assert_eq!(opens[0].json()["users"], "U1");
    assert!(st.posts().iter().all(|p| p["channel"] == "DU1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_redelivered_event_is_acked_again_but_passed_on_once() {
    let s = Slack::start();
    let mut r = start(&s, &["U1"], &[]).await;
    let first = s.event("Ev9", mock::dm("U1", "1790000001.000100", "once"));
    assert_eq!(recv(&mut r).await.text, "once");
    let again = s.event("Ev9", mock::dm("U1", "1790000001.000100", "once"));
    until("both acked", T, || {
        let acks = s.state().acks();
        acks.contains(&first) && acks.contains(&again)
    })
    .await;
    quiet(&mut r).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refresh_request_opens_a_new_connection_without_a_problem() {
    let s = Slack::start();
    let mut r = start(&s, &["U1"], &[]).await;
    s.send(json!({ "type": "disconnect", "reason": "refresh_requested" }));
    until("a second socket", T, || s.state().connections >= 2).await;
    assert_eq!(s.state().opens, 2, "a new apps.connections.open");
    s.event("Ev1", mock::dm("U1", "1790000001.000100", "still here"));
    assert_eq!(recv(&mut r).await.text, "still here");
    assert_eq!(r.ch.problem(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_socket_reconnects() {
    let s = Slack::start();
    let mut r = start(&s, &["U1"], &[]).await;
    s.close();
    until("a second socket", T, || s.state().connections >= 2).await;
    s.event("Ev1", mock::dm("U1", "1790000001.000100", "back"));
    assert_eq!(recv(&mut r).await.text, "back");
    until("the problem cleared by hello", T, || {
        r.ch.problem().is_none()
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_disabled_link_is_explained_and_retried() {
    let s = Slack::start();
    let r = start(&s, &["U1"], &[]).await;
    s.send(json!({ "type": "disconnect", "reason": "link_disabled" }));
    // On this one thread the problem stays up for the whole backoff, and
    // the poll comes round twice as often.
    until("the problem named", T, || {
        r.ch.problem()
            .is_some_and(|p| p.contains("Socket Mode is off for this app (link_disabled)"))
    })
    .await;
    until("a second socket", T, || s.state().connections >= 2).await;
    until("hello clears it", T, || r.ch.problem().is_none()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_silent_socket_is_closed_and_reopened() {
    let s = Slack::start();
    let _r = start(&s, &["U1"], &[]).await;
    // The mock answers pings (tungstenite does), so silence needs no
    // socket at all: an `apps.connections.open` address that never talks.
    let (dead, port) = support::bind();
    let listener = dead;
    // Blocking, and so its sockets: on macOS and Windows an accepted socket
    // inherits the listener's non-blocking mode, and the handshake fails.
    listener.set_nonblocking(false).unwrap();
    std::thread::spawn(move || {
        let mut held = vec![];
        for stream in listener.incoming().flatten() {
            let _ = stream.set_nonblocking(false);
            let mut ws = match tokio_tungstenite::tungstenite::accept(stream) {
                Ok(ws) => ws,
                Err(_) => continue,
            };
            // Hold it open, reading nothing: pings go unanswered.
            let _ = ws.get_mut().set_nonblocking(true);
            held.push(ws);
        }
    });
    s.once(
        "apps.connections.open",
        Response::json(json!({ "ok": true, "url": format!("ws://127.0.0.1:{port}/") })),
    );
    s.send(json!({ "type": "disconnect", "reason": "refresh_requested" }));
    let at = Instant::now();
    until("a new real socket after the silence", T, || {
        s.state().connections >= 2
    })
    .await;
    assert!(
        at.elapsed() >= Duration::from_millis(1500),
        "waited out the silence ({:?})",
        at.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mention_in_an_allowed_channel_is_a_thread_and_the_reply_goes_there() {
    let s = Slack::start();
    let mut r = start(&s, &[], &["C1"]).await;
    s.event(
        "Ev1",
        mock::mention("C1", "U7", "1790000002.000200", "/status please"),
    );
    let m = recv(&mut r).await;
    assert_eq!(m.chat_id, "C1/1790000002.000200");
    assert_eq!(m.text, "/status please", "the mention is stripped");
    // A mention inside a thread keeps that thread.
    let mut e = mock::mention("C1", "U7", "1790000009.000900", "more");
    e["thread_ts"] = json!("1790000002.000200");
    s.event("Ev2", e);
    assert_eq!(recv(&mut r).await.chat_id, "C1/1790000002.000200");

    r.ch.send(out(&m.chat_id, "done")).await.unwrap();
    let posts = s.state().posts();
    assert_eq!(posts[0]["channel"], "C1");
    assert_eq!(posts[0]["thread_ts"], "1790000002.000200");
    r.ch.react(&m.chat_id, &m.message_id, "👀").await.unwrap();
    assert_eq!(s.state().calls("reactions.add")[0].json()["channel"], "C1");

    // Another channel: ignored.
    s.event("Ev3", mock::mention("C2", "U7", "1790000003.000300", "hi"));
    quiet(&mut r).await;
    // A plain channel message (no mention) isn't for the bot.
    s.event(
        "Ev4",
        json!({ "type": "message", "channel_type": "channel", "channel": "C1", "user": "U7", "text": "chatter", "ts": "1790000004.0" }),
    );
    quiet(&mut r).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stranger_is_told_their_id_once_while_nobody_is_allowed_then_blocked() {
    let s = Slack::start();
    let mut r = start(&s, &[], &[]).await;
    s.event("Ev1", mock::dm("U666", "1790000001.000100", "hi"));
    s.event("Ev2", mock::dm("U666", "1790000001.000200", "hello?"));
    quiet(&mut r).await;
    until("the one answer", T, || s.state().posts().len() == 1).await;
    let told = s.state().posts()[0].clone();
    assert_eq!(told["channel"], "DU666");
    assert!(
        told["text"]
            .as_str()
            .unwrap()
            .contains("Slack user id is U666"),
        "{told}"
    );

    // With an allowlist in use, a stranger gets nothing at all.
    let s = Slack::start();
    let mut r = start(&s, &["U1"], &[]).await;
    s.event("Ev1", mock::dm("U666", "1790000001.000100", "hi"));
    quiet(&mut r).await;
    assert!(s.state().posts().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bots_edits_and_the_bots_own_messages_are_dropped() {
    let s = Slack::start();
    let mut r = start(&s, &["U1"], &[]).await;
    let mut bot = mock::dm("U1", "1.1", "from a bot");
    bot["bot_id"] = json!("B9");
    s.event("Ev1", bot);
    let mut edit = mock::dm("U1", "1.2", "edited");
    edit["subtype"] = json!("message_changed");
    s.event("Ev2", edit);
    s.event("Ev3", mock::dm(BOT, "1.3", "my own echo"));
    quiet(&mut r).await;
    s.event("Ev4", mock::dm("U1", "1.4", "real"));
    assert_eq!(recv(&mut r).await.text, "real");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_without_text_gets_plain_words_and_with_text_a_note() {
    let s = Slack::start();
    let mut r = start(&s, &["U1"], &["C1"]).await;
    let mut photo = mock::dm("U1", "1.1", "");
    photo["subtype"] = json!("file_share");
    photo["files"] = json!([{ "id": "F1", "mimetype": "image/png" }]);
    s.event("Ev1", photo);
    quiet(&mut r).await;
    until("the answer", T, || s.state().posts().len() == 1).await;
    let said = s.state().posts()[0].clone();
    assert_eq!(said["channel"], "DU1");
    assert!(said["text"]
        .as_str()
        .unwrap()
        .contains("I can only read text"));

    let mut both = mock::mention("C1", "U1", "1.2", "what's this?");
    both["files"] = json!([{ "id": "F2", "mimetype": "application/pdf" }]);
    s.event("Ev2", both);
    let m = recv(&mut r).await;
    assert!(m.text.starts_with("what's this?"));
    assert!(m
        .text
        .contains("The file attached to this message wasn't read"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_reply_is_split_under_4000_and_every_piece_is_mrkdwn() {
    let s = Slack::start();
    let r = start(&s, &["U1"], &[]).await;
    let long = "**word** ".repeat(1000);
    r.ch.send(out("U1", &long)).await.unwrap();
    let posts = s.state().posts();
    assert_eq!(posts.len(), 2, "7000 characters of mrkdwn in 2 pieces");
    let mut words = 0;
    for p in &posts {
        let t = p["text"].as_str().unwrap();
        assert!(t.encode_utf16().count() <= sl::MESSAGE_LIMIT);
        assert!(!t.contains("**"));
        words += t.split_whitespace().count();
    }
    assert_eq!(words, 1000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_429_on_post_is_waited_out_and_retried_and_an_edit_is_told_at_once() {
    let s = Slack::start();
    let r = start(&s, &["U1"], &[]).await;
    s.once(
        "chat.postMessage",
        Response::json(json!({ "ok": false, "error": "ratelimited" }))
            .with_status(429)
            .with_header("Retry-After", "1"),
    );
    let at = Instant::now();
    r.ch.send(out("U1", "patient")).await.unwrap();
    assert!(
        at.elapsed() >= Duration::from_millis(950),
        "{:?}",
        at.elapsed()
    );
    let posts = s.state().calls("chat.postMessage");
    assert_eq!(posts.len(), 2, "one 429, one retry");
    assert_eq!(posts[1].json()["text"], "patient");

    // `ok: false, error: ratelimited` with a 200 counts the same; an edit
    // hands the wait back instead of sleeping.
    s.once(
        "chat.update",
        Response::json(json!({ "ok": false, "error": "ratelimited" }))
            .with_header("Retry-After", "7"),
    );
    let e = r.ch.edit("U1", "1.1", "x").await.unwrap_err();
    assert!(
        matches!(e, GatewayError::RateLimited { retry_after } if retry_after >= Duration::from_secs(6)),
        "{e:?}"
    );
    let at = Instant::now();
    let e = r.ch.edit("U1", "1.1", "y").await.unwrap_err();
    assert!(matches!(e, GatewayError::RateLimited { .. }), "{e:?}");
    assert!(
        at.elapsed() < Duration::from_millis(500),
        "blocked, not sent"
    );
    assert_eq!(s.state().calls("chat.update").len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn posts_to_one_channel_are_spaced_a_second_apart() {
    let s = Slack::start();
    let ch = Arc::new(channel(&s, &["U1"], &[]));
    let at = Instant::now();
    let (a, b) = tokio::join!(ch.send(out("C9", "one")), ch.send(out("C9", "two")));
    a.unwrap();
    b.unwrap();
    ch.send(out("C8", "elsewhere")).await.unwrap();
    let times = s.state().times_of("chat.postMessage");
    assert_eq!(times.len(), 3);
    assert!(times[1].duration_since(times[0]) >= Duration::from_millis(950));
    assert!(
        at.elapsed() < Duration::from_millis(1900),
        "another channel doesn't wait"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_approval_goes_out_as_block_kit_and_a_tap_comes_back_as_its_command() {
    let s = Slack::start();
    let mut r = start(&s, &["U1"], &[]).await;
    let buttons = [
        Button {
            text: "Allow".into(),
            action: ButtonAction::Command("yes AB12".into()),
        },
        Button {
            text: "Refuse".into(),
            action: ButtonAction::Command("no AB12".into()),
        },
        Button {
            text: "Docs".into(),
            action: ButtonAction::Url("https://x.io".into()),
        },
    ];
    r.ch.send_buttons(out("U1", "Run `rm -rf target`?"), &buttons)
        .await
        .unwrap();
    let post = s.state().posts()[0].clone();
    assert_eq!(
        post["text"], "Run `rm -rf target`?",
        "the notification text"
    );
    let blocks = post["blocks"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "section");
    assert_eq!(blocks[0]["text"]["text"], "Run `rm -rf target`?");
    let elements = blocks[1]["elements"].as_array().unwrap();
    assert_eq!(blocks[1]["type"], "actions");
    assert_eq!(elements[0]["value"], "yes AB12");
    assert_eq!(elements[0]["action_id"], "cmd_0");
    assert_eq!(elements[1]["text"]["text"], "Refuse");
    assert_eq!(elements[2]["url"], "https://x.io");
    // The mock numbers its posts; this was the first.
    let ts = "1790000000.000001";

    let tap = |user: &str, value: &str, label: &str| {
        json!({
            "type": "block_actions",
            "user": { "id": user, "username": format!("u{user}"), "name": format!("u{user}") },
            "channel": { "id": "DU1" },
            "container": { "type": "message", "message_ts": ts, "channel_id": "DU1" },
            "message": { "ts": ts, "text": "Run `rm -rf target`?" },
            "actions": [{ "type": "button", "action_id": "cmd_0", "value": value,
                          "text": { "type": "plain_text", "text": label } }],
        })
    };
    let env = s.envelope("interactive", tap("U1", "yes AB12", "Allow"));
    let m = recv(&mut r).await;
    until("the ack", T, || s.state().acks().contains(&env)).await;
    assert_eq!(
        (m.chat_id.as_str(), m.text.as_str(), m.message_id.as_str()),
        ("U1", "yes AB12", "")
    );
    let update = s.state().calls("chat.update")[0].json();
    assert_eq!(update["ts"], json!(ts));
    assert_eq!(update["text"], "Run `rm -rf target`?\n→ Allow");
    assert_eq!(
        update["blocks"][0]["text"]["text"], "Run `rm -rf target`?\n→ Allow",
        "the buttons are gone"
    );

    // A stranger's tap (acked, as every envelope) does nothing.
    let env = s.envelope("interactive", tap("U666", "yes AB12", "Allow"));
    quiet(&mut r).await;
    until("the ack", T, || s.state().acks().contains(&env)).await;
    assert_eq!(s.state().calls("chat.update").len(), 1);
    // A link button's tap is only acked.
    let mut link = tap("U1", "", "Docs");
    link["actions"] = json!([{ "type": "button", "action_id": "url_2", "url": "https://x.io", "text": { "type": "plain_text", "text": "Docs" } }]);
    s.envelope("interactive", link);
    quiet(&mut r).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_slash_command_answers_in_an_allowed_channel_else_the_users_dm_else_nowhere() {
    let s = Slack::start();
    let mut r = start(&s, &["U1"], &["C1"]).await;
    let slash = |user: &str, channel: &str, text: &str| json!({ "command": "/ferrule", "text": text, "user_id": user, "user_name": format!("u{user}"), "channel_id": channel });
    let env = s.envelope("slash_commands", slash("U7", "C1", "model sonnet"));
    let m = recv(&mut r).await;
    until("the ack", T, || s.state().acks().contains(&env)).await;
    assert_eq!(
        (m.chat_id.as_str(), m.text.as_str()),
        ("C1", "/model sonnet"),
        "an allowed channel: answered there, not in a thread"
    );
    s.envelope("slash_commands", slash("U1", "C5", "/stop"));
    let m = recv(&mut r).await;
    assert_eq!((m.chat_id.as_str(), m.text.as_str()), ("U1", "/stop"));
    s.envelope("slash_commands", slash("U1", "DU1", ""));
    let m = recv(&mut r).await;
    assert_eq!((m.chat_id.as_str(), m.text.as_str()), ("U1", "/status"));
    s.envelope("slash_commands", slash("U666", "C5", "status"));
    quiet(&mut r).await;
    s.envelope("slash_commands", slash("U666", "DU666", "status"));
    quiet(&mut r).await;
    r.ch.send(out("C1", "answer")).await.unwrap();
    let post = s.state().posts()[0].clone();
    assert_eq!(post["channel"], "C1");
    assert!(post.get("thread_ts").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_token_ends_run_with_the_reason() {
    let s = Slack::start();
    let ch = SlackChannel::with_api("xoxb-wrong", APP_TOKEN, &s.api).with_fast_retries();
    let (tx, _rx) = mpsc::channel(4);
    let e = tokio::time::timeout(T, ch.run(tx))
        .await
        .unwrap()
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("bot token") && e.contains("OAuth & Permissions"),
        "{e}"
    );
    assert!(ch.problem().unwrap().starts_with("stopped:"));

    let ch = SlackChannel::with_api(BOT_TOKEN, "xapp-wrong", &s.api).with_fast_retries();
    let (tx, _rx) = mpsc::channel(4);
    let e = tokio::time::timeout(T, ch.run(tx))
        .await
        .unwrap()
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("app token") && e.contains("connections:write"),
        "{e}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctors_probe_reads_only() {
    let s = Slack::start();
    let p = sl::probe(&s.api, BOT_TOKEN, APP_TOKEN).await.unwrap();
    assert_eq!((p.bot_id.as_str(), p.bot_name.as_str()), (BOT, "ferrule"));
    assert_eq!(p.team, "Mock Team");
    assert!(p.socket.is_ok());
    let p = sl::probe(&s.api, BOT_TOKEN, "xapp-wrong").await.unwrap();
    assert!(p.socket.unwrap_err().contains("App-Level Token"));
    let e = sl::probe(&s.api, "xoxb-wrong", APP_TOKEN)
        .await
        .unwrap_err();
    assert!(e.contains("Bot User OAuth Token"), "{e}");
    let st = s.state();
    let methods: Vec<&str> = st.requests.iter().map(|r| r.path.as_str()).collect();
    assert!(
        methods
            .iter()
            .all(|m| *m == "/api/auth.test" || *m == "/api/apps.connections.open"),
        "{methods:?}"
    );
    assert_eq!(st.connections, 0, "no socket opened");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_probe_names_the_scopes_the_bot_token_lacks() {
    let s = Slack::start();
    let p = sl::probe(&s.api, BOT_TOKEN, APP_TOKEN).await.unwrap();
    assert!(p.missing_scopes().is_empty(), "no header, nothing claimed");
    for _ in 0..2 {
        s.once(
            "auth.test",
            Response::json(json!({
                "ok": true, "user_id": BOT, "user": "ferrule", "team": "Mock Team",
            }))
            .with_header("x-oauth-scopes", "chat:write,im:history, im:read,commands"),
        );
    }
    let p = sl::probe(&s.api, BOT_TOKEN, APP_TOKEN).await.unwrap();
    assert_eq!(
        p.missing_scopes(),
        ["app_mentions:read", "im:write", "reactions:write"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_pairs_the_first_dm_with_the_code_and_tells_nobody_else_anything() {
    let s = Slack::start();
    let ch = SlackChannel::with_api(BOT_TOKEN, APP_TOKEN, &s.api).with_pairing("ferrule-4827");
    let mut r = spawn(ch, 8);
    until("the socket", T, || s.state().connections >= 1).await;
    s.event("Ev1", mock::dm("U2", "1.1", "hello"));
    s.event("Ev2", mock::dm("U1", "1.2", "ferrule-4827"));
    until("paired", T, || r.ch.paired().is_some()).await;
    assert_eq!(r.ch.paired(), Some(("U1".into(), "name-U1".into())));
    until("the answer", T, || s.state().posts().len() == 1).await;
    let post = s.state().posts()[0].clone();
    assert_eq!(post["channel"], "DU1");
    assert!(post["text"].as_str().unwrap().starts_with("Paired."));
    s.event("Ev3", mock::dm("U1", "1.3", "now me"));
    assert_eq!(recv(&mut r).await.text, "now me");
}

// --- streaming --------------------------------------------------------------

struct Streamer;

#[async_trait::async_trait]
impl Provider for Streamer {
    fn name(&self) -> &str {
        "streamer"
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let mut all = String::new();
        for i in 0..40 {
            let p = format!("**word{i}** {}\n", "x".repeat(20));
            if let Some(s) = &req.stream {
                s.send(Delta::Text(p.clone()));
            }
            all.push_str(&p);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(CompletionResponse {
            message: Message::assistant(Some(all), vec![], None),
            usage: Usage::default(),
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_streamed_reply_edits_no_faster_than_every_1_5_seconds_in_its_thread() {
    let s = Slack::start();
    let r = start(&s, &[], &["C1"]).await;
    let dir = tempfile::tempdir().unwrap();
    let factory: AgentFactory = Arc::new(|_sid, transcript| {
        Ok(Agent::new(
            Arc::new(Streamer),
            ToolRegistry::new(),
            HarnessProfile::generic(),
            AgentConfig::default(),
            ToolContext::default(),
            Some(transcript),
        )
        .with_system_prompt("test"))
    });
    let ch: Arc<dyn Channel> = r.ch.clone();
    let router = Router::new(
        dir.path(),
        factory,
        HashMap::from([("slack".to_string(), ch)]),
    )
    .with_streaming(
        ["slack".to_string()],
        StreamPacing {
            first_after: Duration::from_millis(30),
            first_chars: 10_000,
            every: Duration::from_millis(80),
            limit: 4000,
        },
    );
    router
        .dispatch_and_wait(InboundMessage {
            channel: "slack".into(),
            chat_id: "C1/1790000002.000200".into(),
            sender: "max".into(),
            sender_id: Some("U1".into()),
            message_id: "1790000002.000200".into(),
            text: "go".into(),
            attachments: vec![],
            reply_to: None,
            ts: 0,
        })
        .await
        .unwrap();
    let st = s.state();
    let posts = st.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["thread_ts"], "1790000002.000200");
    let edits = st.calls("chat.update");
    assert!(edits.len() >= 2, "{} edits", edits.len());
    let mut times = st.times_of("chat.postMessage");
    times.extend(st.times_of("chat.update"));
    // The preview is paced; the final answer goes out when it's ready.
    times.pop();
    for w in times.windows(2) {
        assert!(
            w[1].duration_since(w[0]) >= Duration::from_millis(1450),
            "{:?}",
            w[1].duration_since(w[0])
        );
    }
    let last: Value = edits.last().unwrap().json();
    let text = last["text"].as_str().unwrap();
    assert!(text.contains("*word39*") && !text.contains("**"), "{text}");
}

// --- the tokens stay out of logs --------------------------------------------

#[derive(Clone, Default)]
struct Buf(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn no_log_line_or_error_carries_a_token_or_the_socket_ticket() {
    let buf = Buf::default();
    let b = buf.clone();
    let sub = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || b.clone())
        .finish();
    let _g = tracing::subscriber::set_default(sub);
    // Another test may have cached these callsites as off before this
    // subscriber existed.
    tracing::callsite::rebuild_interest_cache();
    let s = Slack::start();
    let mut r = start(&s, &["U1"], &[]).await;
    s.close();
    until("reconnected", T, || s.state().connections >= 2).await;
    s.once(
        "chat.postMessage",
        Response::json(json!({ "ok": false, "error": "channel_not_found" })),
    );
    let err = r.ch.send(out("U1", "x")).await.unwrap_err().to_string();
    assert!(err.contains("channel_not_found"), "{err}");
    s.event("Ev1", mock::dm("U666", "1.1", "stranger"));
    s.once(
        "apps.connections.open",
        Response::json(json!({ "ok": false, "error": "invalid_auth" })),
    );
    s.send(json!({ "type": "disconnect", "reason": "refresh_requested" }));
    let end = tokio::time::timeout(T, &mut r.run)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err()
        .to_string();
    let logs = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("slack"), "{logs}");
    for text in [&logs, &err, &end] {
        for secret in [BOT_TOKEN, APP_TOKEN, TICKET] {
            assert!(!text.contains(secret), "{text}");
        }
        assert!(!text.contains("127.0.0.1"), "no URL: {text}");
    }
}

// --- live --------------------------------------------------------------------

/// Against real Slack: connects, DMs the user, waits up to two minutes for
/// their reply. `FERRULE_LIVE_SLACK_BOT_TOKEN`, `FERRULE_LIVE_SLACK_APP_TOKEN`,
/// `FERRULE_LIVE_SLACK_USER`; see docs/slack.md.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Slack app; see docs/slack.md"]
async fn slack_live_round_trip() {
    let bot = std::env::var("FERRULE_LIVE_SLACK_BOT_TOKEN").expect("FERRULE_LIVE_SLACK_BOT_TOKEN");
    let app = std::env::var("FERRULE_LIVE_SLACK_APP_TOKEN").expect("FERRULE_LIVE_SLACK_APP_TOKEN");
    let user = std::env::var("FERRULE_LIVE_SLACK_USER").expect("FERRULE_LIVE_SLACK_USER");
    sl::check_tokens(&bot, &app).unwrap();
    let probe = sl::probe(sl::API_URL, &bot, &app).await.unwrap();
    println!(
        "bot {} ({}) in {}, socket {:?}",
        probe.bot_name, probe.bot_id, probe.team, probe.socket
    );
    let ch = Arc::new(SlackChannel::new(bot, app).with_allowed(vec![user.clone()], vec![]));
    let (tx, mut rx) = mpsc::channel(8);
    let c = ch.clone();
    tokio::spawn(async move { c.run(tx).await });
    ch.send(out(&user, "ferrule live test: reply to this DM"))
        .await
        .unwrap();
    let m = tokio::time::timeout(Duration::from_secs(120), rx.recv())
        .await
        .expect("a reply within two minutes")
        .unwrap();
    assert_eq!(m.chat_id, user);
    ch.react(&m.chat_id, &m.message_id, "👀").await.unwrap();
    let id = ch
        .post(out(&user, "got it: **streaming**…"))
        .await
        .unwrap()
        .unwrap();
    ch.edit(&user, &id, &format!("got it: {}", m.text))
        .await
        .unwrap();
}
