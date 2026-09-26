//! M31: the Discord adapter against a mock gateway and REST API — the
//! handshake and its intents, resume and re-identify on every kind of drop,
//! the close codes, rate limits, who gets in, the 2000-character split,
//! buttons, slash commands, streaming, and the token staying out of logs.

mod support;

use ferrule_core::provider::{CompletionRequest, CompletionResponse};
use ferrule_core::tool::ToolContext;
use ferrule_core::{
    Agent, AgentConfig, CoreError, Delta, HarnessProfile, Message, Provider, ToolRegistry, Usage,
};
use ferrule_gateway::channel::{Button, ButtonAction};
use ferrule_gateway::channels::discord::{self as dc, DiscordChannel};
use ferrule_gateway::{
    AgentFactory, Channel, GatewayError, InboundMessage, OutboundMessage, Router, StreamPacing,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::discord::{self as mock, Discord, Plan, BOT, TOKEN};
use support::http::Response;
use support::until;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const T: Duration = Duration::from_secs(10);

struct Running {
    ch: Arc<DiscordChannel>,
    rx: mpsc::Receiver<InboundMessage>,
    run: JoinHandle<Result<(), GatewayError>>,
}

fn start_with(d: &Discord, ch: DiscordChannel) -> Running {
    let ch = Arc::new(ch.with_fast_retries());
    let (tx, rx) = mpsc::channel(32);
    let c = ch.clone();
    let run = tokio::spawn(async move { c.run(tx).await });
    let _ = d;
    Running { ch, rx, run }
}

async fn start(d: &Discord, users: &[&str], channels: &[&str]) -> Running {
    let ch = DiscordChannel::with_api(TOKEN, &d.api).with_allowed(
        users.iter().map(|s| s.to_string()).collect(),
        channels.iter().map(|s| s.to_string()).collect(),
    );
    let r = start_with(d, ch);
    until("READY", T, || d.state().readies >= 1).await;
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
        channel: "discord".into(),
        chat_id: chat.into(),
        text: text.into(),
        reply_to: None,
        attachments: vec![],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identify_asks_for_the_right_intents_and_an_allowed_dm_round_trips() {
    let d = Discord::start();
    let mut r = start(&d, &["1001"], &[]).await;
    let identify = d.state().identifies()[0].clone();
    assert_eq!(identify["d"]["token"], TOKEN);
    assert_eq!(
        identify["d"]["intents"],
        1 | (1 << 9) | (1 << 12) | (1 << 15)
    );
    assert_eq!(identify["d"]["properties"]["browser"], "ferrule");
    d.dm("m1", "1001", "hello there");
    let m = recv(&mut r).await;
    assert_eq!(
        (m.channel.as_str(), m.chat_id.as_str()),
        ("discord", "1001")
    );
    assert_eq!(m.text, "hello there");
    assert_eq!(m.message_id, "m1");
    assert_eq!(m.sender_id.as_deref(), Some("1001"));
    assert_eq!(m.sender, "user1001");
    r.ch.react("1001", "m1", "👀").await.unwrap();
    r.ch.send(out("1001", "hi back")).await.unwrap();
    let s = d.state();
    assert_eq!(
        s.find("PUT", "/reactions/").first().map(|q| q.path.clone()),
        Some("/api/v10/channels/71001/messages/m1/reactions/%F0%9F%91%80/@me".into())
    );
    assert!(
        s.find("POST", "/users/@me/channels").is_empty(),
        "the DM channel came with the message"
    );
    let sent = s.messages();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "71001");
    assert_eq!(sent[0].1["content"], "hi back");
    assert_eq!(sent[0].1["allowed_mentions"], json!({ "parse": [] }));
    assert!(
        sent[0].1.get("message_reference").is_none(),
        "no reply-quote in a DM"
    );
    let auth = s.find("POST", "/channels/71001/messages")[0]
        .header("authorization")
        .map(str::to_string);
    assert_eq!(auth, Some(format!("Bot {TOKEN}")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_to_an_owner_never_seen_opens_the_dm_first() {
    let d = Discord::start();
    let r = start(&d, &["1001"], &[]).await;
    r.ch.send(out("1001", "the daemon started")).await.unwrap();
    r.ch.send(out("1001", "again")).await.unwrap();
    let s = d.state();
    let opened = s.find("POST", "/users/@me/channels");
    assert_eq!(opened.len(), 1, "opened once, then cached");
    assert_eq!(opened[0].json()["recipient_id"], "1001");
    assert_eq!(s.messages().iter().filter(|(c, _)| c == "71001").count(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stranger_is_told_their_id_once_while_nobody_is_allowed_and_never_reaches_the_agent() {
    let d = Discord::start();
    let mut r = start(&d, &[], &[]).await;
    d.dm("m1", "2002", "hi");
    d.dm("m2", "2002", "hello?");
    quiet(&mut r).await;
    let told = d.state().messages();
    assert_eq!(told.len(), 1, "{told:?}");
    let text = told[0].1["content"].as_str().unwrap().to_string();
    assert!(text.contains("Your Discord user id is 2002"), "{text}");
    assert!(text.contains("discord_allowed_users"), "{text}");

    // With a list in use: silence.
    let d = Discord::start();
    let mut r = start(&d, &["1001"], &[]).await;
    d.dm("m1", "2002", "hi");
    quiet(&mut r).await;
    assert!(d.state().messages().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bots_and_the_bots_own_messages_are_dropped() {
    let d = Discord::start();
    let mut r = start(&d, &["1001", BOT], &[]).await;
    let mut m = mock::dm("m1", "1001", "from a bot");
    m["author"]["bot"] = json!(true);
    d.dispatch("MESSAGE_CREATE", m);
    d.dispatch("MESSAGE_CREATE", mock::dm("m2", BOT, "my own"));
    let mut w = mock::dm("m3", "1001", "a webhook");
    w["webhook_id"] = json!("77");
    d.dispatch("MESSAGE_CREATE", w);
    quiet(&mut r).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_a_server_only_an_allowed_channel_that_addresses_the_bot_gets_in() {
    let d = Discord::start();
    d.state().channels.insert(
        "3333".into(),
        json!({ "id": "3333", "type": 11, "parent_id": "2222", "guild_id": "42" }),
    );
    d.state().channels.insert(
        "4444".into(),
        json!({ "id": "4444", "type": 11, "parent_id": "8888", "guild_id": "42" }),
    );
    let mut r = start(&d, &[], &["2222"]).await;
    // Allowed channel, not addressed: ignored.
    d.dispatch(
        "MESSAGE_CREATE",
        mock::guild("g1", "2222", "5", "chatting", false),
    );
    // Addressed, in a channel not on the list: ignored.
    d.dispatch(
        "MESSAGE_CREATE",
        mock::guild("g2", "8888", "5", "<@999> hi", true),
    );
    // Addressed in a thread under a channel not on the list: ignored.
    d.dispatch(
        "MESSAGE_CREATE",
        mock::guild("g3", "4444", "5", "<@999> hi", true),
    );
    quiet(&mut r).await;
    // Addressed in the allowed channel: in, mention stripped.
    d.dispatch(
        "MESSAGE_CREATE",
        mock::guild("g4", "2222", "5", "<@999> /status", true),
    );
    let m = recv(&mut r).await;
    assert_eq!((m.chat_id.as_str(), m.text.as_str()), ("2222", "/status"));
    // A reply to the bot counts as addressing it.
    let mut reply = mock::guild("g5", "2222", "5", "and this?", false);
    reply["referenced_message"] = json!({ "id": "5001", "author": { "id": BOT } });
    d.dispatch("MESSAGE_CREATE", reply);
    assert_eq!(recv(&mut r).await.text, "and this?");
    // A thread inherits its parent's place on the list.
    d.dispatch(
        "MESSAGE_CREATE",
        mock::guild("g6", "3333", "5", "<@!999> in a thread", true),
    );
    let m = recv(&mut r).await;
    assert_eq!(
        (m.chat_id.as_str(), m.text.as_str()),
        ("3333", "in a thread")
    );
    // Replies in a server quote the message that asked, and ping nobody.
    let mut o = out("2222", "done");
    o.reply_to = Some("g4".into());
    r.ch.send(o).await.unwrap();
    let sent = d.state().messages();
    let (c, body) = sent.last().unwrap();
    assert_eq!(c, "2222");
    assert_eq!(body["message_reference"]["message_id"], "g4");
    assert_eq!(body["allowed_mentions"], json!({ "parse": [] }));
    assert!(d.state().find("POST", "/users/@me/channels").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_socket_resumes_with_the_session_and_last_sequence() {
    let d = Discord::start();
    let mut r = start(&d, &["1001"], &[]).await;
    d.dm("m1", "1001", "one");
    recv(&mut r).await;
    d.close(4000);
    until("RESUMED", T, || d.state().resumed >= 1).await;
    let resume = d.state().resumes()[0].clone();
    assert_eq!(resume["d"]["session_id"], "sess1");
    assert_eq!(resume["d"]["seq"], 2, "READY was 1, the DM 2");
    assert_eq!(resume["d"]["token"], TOKEN);
    assert_eq!(d.state().identifies().len(), 1);
    assert!(r.ch.problem().is_none(), "{:?}", r.ch.problem());
    d.dm("m2", "1001", "two");
    assert_eq!(recv(&mut r).await.text, "two");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_resumes_and_an_invalid_session_resumes_or_identifies_as_told() {
    let d = Discord::start();
    let r = start(&d, &["1001"], &[]).await;
    d.op(json!({ "op": 7, "d": null }));
    until("resume after op 7", T, || d.state().resumed >= 1).await;
    d.op(json!({ "op": 9, "d": true }));
    until("resume after op 9 true", T, || d.state().resumed >= 2).await;
    assert_eq!(d.state().identifies().len(), 1);
    d.op(json!({ "op": 9, "d": false }));
    until("identify after op 9 false", T, || d.state().readies >= 2).await;
    assert_eq!(d.state().identifies().len(), 2);
    assert_eq!(d.state().resumes().len(), 2);
    assert!(!r.run.is_finished());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missed_heartbeat_ack_closes_the_zombie_and_resumes() {
    let d = Discord::start();
    {
        let mut s = d.state();
        s.heartbeat_ms = 80;
        s.ack = false;
    }
    let _r = start(&d, &["1001"], &[]).await;
    until("the resume", T, || !d.state().resumes().is_empty()).await;
    d.state().ack = true;
    let s = d.state();
    let beats: Vec<&Value> = s.frames.iter().filter(|f| f["op"] == 1).collect();
    assert!(!beats.is_empty());
    assert_eq!(beats[0]["d"], 1, "a heartbeat carries the last sequence");
    assert_eq!(s.identifies().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_token_ends_run_with_the_reason() {
    let d = Discord::start();
    d.state().plans.push_back(Plan::Close(4004));
    let ch = DiscordChannel::with_api(TOKEN, &d.api).with_allowed(vec!["1".into()], vec![]);
    let r = start_with(&d, ch);
    let err = tokio::time::timeout(T, r.run)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err()
        .to_string();
    assert!(err.contains("token is wrong (4004)"), "{err}");
    assert!(!err.contains(TOKEN));
    assert!(r.ch.problem().unwrap().contains("4004"));

    // A 401 on /gateway/bot is the same mistake, caught earlier.
    let d = Discord::start();
    let ch = DiscordChannel::with_api("wrong", &d.api);
    let r = start_with(&d, ch);
    let err = tokio::time::timeout(T, r.run)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err()
        .to_string();
    assert!(err.contains("401"), "{err}");
    assert_eq!(d.state().connections, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_message_content_intent_it_reconnects_without_it_and_says_so() {
    let d = Discord::start();
    d.state().plans.push_back(Plan::Close(4014));
    let ch = DiscordChannel::with_api(TOKEN, &d.api);
    let r = start_with(&d, ch);
    until("READY", T, || d.state().readies >= 1).await;
    let ids = d
        .state()
        .identifies()
        .iter()
        .map(|f| f["d"]["intents"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids, [4609 | (1 << 15), 4609]);
    let p = r.ch.problem().unwrap();
    assert!(
        p.contains("MESSAGE_CONTENT") && p.contains("Privileged Gateway Intents"),
        "{p}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_sequence_close_identifies_afresh() {
    let d = Discord::start();
    let _r = start(&d, &["1"], &[]).await;
    d.state().plans.push_back(Plan::Close(4007));
    d.close(4000);
    until("a second READY", T, || d.state().readies >= 2).await;
    assert_eq!(d.state().resumes().len(), 1, "tried to resume, was refused");
    assert_eq!(d.state().identifies().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_429_is_waited_out_and_retried_and_a_global_one_holds_every_route() {
    let d = Discord::start();
    let r = start(&d, &["1001"], &["2222", "3333"]).await;
    d.once(
        "POST",
        "/channels/2222/messages",
        Response::json(json!({ "message": "You are being rate limited.", "retry_after": 0.3, "global": false }))
            .with_status(429),
    );
    let at = Instant::now();
    r.ch.send(out("2222", "hello")).await.unwrap();
    assert!(
        at.elapsed() >= Duration::from_millis(290),
        "{:?}",
        at.elapsed()
    );
    assert_eq!(d.state().find("POST", "/channels/2222/messages").len(), 2);

    d.once(
        "POST",
        "/channels/2222/messages",
        Response::json(
            json!({ "message": "You are being rate limited.", "retry_after": 0.4, "global": true }),
        )
        .with_status(429),
    );
    let (a, b) = (r.ch.clone(), r.ch.clone());
    let at = Instant::now();
    let first = tokio::spawn(async move { a.send(out("2222", "one")).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    b.send(out("3333", "two")).await.unwrap();
    assert!(
        at.elapsed() >= Duration::from_millis(390),
        "the other channel waited too: {:?}",
        at.elapsed()
    );
    first.await.unwrap().unwrap();

    // An edit isn't retried: the streamer paces itself.
    d.once(
        "PATCH",
        "/channels/2222/messages/",
        Response::json(json!({ "message": "slow down", "retry_after": 1.5, "global": false }))
            .with_status(429),
    );
    let e = r.ch.edit("2222", "5001", "x").await.unwrap_err();
    assert!(
        matches!(e, GatewayError::RateLimited { retry_after } if retry_after >= Duration::from_millis(1400)),
        "{e}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_bucket_waits_for_its_reset_but_another_channel_does_not() {
    let d = Discord::start();
    let r = start(&d, &[], &["2222", "3333"]).await;
    d.once(
        "POST",
        "/channels/2222/messages",
        Response::json(json!({ "id": "1" }))
            .with_header("x-ratelimit-bucket", "msgbucket")
            .with_header("x-ratelimit-limit", "5")
            .with_header("x-ratelimit-remaining", "0")
            .with_header("x-ratelimit-reset-after", "0.4"),
    );
    r.ch.send(out("2222", "one")).await.unwrap();
    let at = Instant::now();
    r.ch.send(out("3333", "other channel")).await.unwrap();
    assert!(
        at.elapsed() < Duration::from_millis(300),
        "{:?}",
        at.elapsed()
    );
    r.ch.send(out("2222", "two")).await.unwrap();
    assert!(
        at.elapsed() >= Duration::from_millis(350),
        "{:?}",
        at.elapsed()
    );
    assert_eq!(
        d.state().find("POST", "/channels/2222/messages").len(),
        2,
        "no 429 needed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_reply_is_split_at_2000_and_buttons_ride_the_last_piece() {
    let d = Discord::start();
    let r = start(&d, &[], &["2222"]).await;
    let text = "line of words here\n".repeat(250); // 4750 characters
    let mut o = out("2222", &text);
    o.reply_to = Some("g1".into());
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
            action: ButtonAction::Url("https://example.com/d".into()),
        },
    ];
    r.ch.send_buttons(o, &buttons).await.unwrap();
    let sent = d.state().messages();
    assert_eq!(sent.len(), 3);
    let joined = sent
        .iter()
        .map(|(_, b)| b["content"].as_str().unwrap())
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        joined.split_whitespace().count(),
        text.split_whitespace().count()
    );
    for (_, b) in &sent {
        assert!(b["content"].as_str().unwrap().encode_utf16().count() <= 2000);
    }
    assert_eq!(sent[0].1["message_reference"]["message_id"], "g1");
    assert!(sent[1].1.get("message_reference").is_none());
    assert!(sent[0].1.get("components").is_none() && sent[1].1.get("components").is_none());
    let row = &sent[2].1["components"][0];
    assert_eq!(row["type"], 1);
    assert_eq!(
        row["components"][0],
        json!({ "type": 2, "style": 1, "label": "Allow", "custom_id": "cmd:yes AB12" })
    );
    assert_eq!(row["components"][2]["style"], 5);
    assert_eq!(row["components"][2]["url"], "https://example.com/d");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_button_tap_is_acked_at_once_and_arrives_as_its_command() {
    let d = Discord::start();
    let mut r = start(&d, &["1001"], &[]).await;
    let tap = |user: &str, id: &str| {
        json!({
            "id": id, "type": 3, "token": format!("tok-{id}"), "application_id": "555",
            "channel_id": format!("7{user}"),
            "user": { "id": user, "username": format!("user{user}") },
            "data": { "custom_id": "cmd:yes AB12", "component_type": 2 },
            "message": { "id": "5001", "content": "Run `rm -rf target`?", "components": [
                { "type": 1, "components": [
                    { "type": 2, "style": 1, "label": "Allow", "custom_id": "cmd:yes AB12" },
                    { "type": 2, "style": 1, "label": "Refuse", "custom_id": "cmd:no AB12" } ] } ] },
        })
    };
    let at = Instant::now();
    d.dispatch("INTERACTION_CREATE", tap("1001", "i1"));
    let m = recv(&mut r).await;
    assert_eq!(
        (m.chat_id.as_str(), m.text.as_str(), m.message_id.as_str()),
        ("1001", "yes AB12", "")
    );
    let acks = d.state().find("POST", "/interactions/i1/tok-i1/callback");
    assert_eq!(acks.len(), 1, "acknowledged before it was passed on");
    assert!(at.elapsed() < Duration::from_secs(3));
    let body = acks[0].json();
    assert_eq!(body["type"], 7);
    assert_eq!(body["data"]["components"], json!([]));
    assert_eq!(body["data"]["content"], "Run `rm -rf target`?\n→ Allow");
    assert!(
        acks[0].header("authorization").is_none(),
        "the interaction token is its own auth"
    );

    // A stranger's tap is acknowledged and does nothing.
    d.dispatch("INTERACTION_CREATE", tap("2002", "i2"));
    quiet(&mut r).await;
    let acks = d.state().find("POST", "/interactions/i2/");
    assert_eq!(acks[0].json(), json!({ "type": 6 }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slash_command_is_deferred_and_its_answer_edits_the_deferral() {
    let d = Discord::start();
    let mut r = start(&d, &["1001"], &[]).await;
    let slash = |user: &str, id: &str, args: Option<&str>| {
        let options = args.map_or(
            json!([]),
            |a| json!([{ "name": "args", "type": 3, "value": a }]),
        );
        json!({
            "id": id, "type": 2, "token": format!("tok-{id}"), "application_id": "555",
            "channel_id": format!("7{user}"),
            "user": { "id": user, "username": format!("user{user}") },
            "data": { "name": "model", "options": options },
        })
    };
    d.dispatch("INTERACTION_CREATE", slash("1001", "i1", Some(" opus ")));
    let m = recv(&mut r).await;
    assert_eq!(
        (m.chat_id.as_str(), m.text.as_str()),
        ("1001", "/model opus")
    );
    assert_eq!(
        d.state().find("POST", "/interactions/i1/tok-i1/callback")[0].json(),
        json!({ "type": 5 })
    );
    let id =
        r.ch.post(out("1001", "Switched to opus."))
            .await
            .unwrap()
            .unwrap();
    r.ch.edit("1001", &id, "Switched to opus. Done.")
        .await
        .unwrap();
    let hooks = d
        .state()
        .find("PATCH", "/webhooks/555/tok-i1/messages/@original");
    assert_eq!(
        hooks.len(),
        2,
        "the answer and its edit both went to the deferral"
    );
    assert_eq!(hooks[1].json()["content"], "Switched to opus. Done.");
    assert!(d.state().messages().is_empty());
    // The next reply is a plain message again.
    r.ch.send(out("1001", "later")).await.unwrap();
    assert_eq!(d.state().messages().len(), 1);

    // A stranger's slash command is told the bot is private, only to them.
    d.dispatch("INTERACTION_CREATE", slash("2002", "i2", None));
    quiet(&mut r).await;
    let reply = d.state().find("POST", "/interactions/i2/")[0].json();
    assert_eq!(reply["type"], 4);
    assert_eq!(reply["data"]["flags"], 64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_without_text_gets_plain_words_and_with_text_a_note() {
    let d = Discord::start();
    let mut r = start(&d, &["1001"], &[]).await;
    let mut m = mock::dm("m1", "1001", "");
    m["attachments"] = json!([{ "id": "a", "filename": "x.png", "content_type": "image/png" }]);
    d.dispatch("MESSAGE_CREATE", m);
    quiet(&mut r).await;
    let told = d.state().messages();
    assert!(
        told[0].1["content"]
            .as_str()
            .unwrap()
            .contains("I can only read text"),
        "{told:?}"
    );
    let mut m = mock::dm("m2", "1001", "what is this?");
    m["attachments"] =
        json!([{ "id": "a", "filename": "x.pdf", "content_type": "application/pdf" }]);
    d.dispatch("MESSAGE_CREATE", m);
    let got = recv(&mut r).await;
    assert!(
        got.text.starts_with("what is this?\n\n[The file attached"),
        "{}",
        got.text
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctors_probe_reads_only_and_setup_registers_the_commands() {
    let d = Discord::start();
    let p = dc::probe(&d.api, TOKEN).await.unwrap();
    assert_eq!(
        (p.bot_name.as_str(), p.bot_id.as_str()),
        ("ferrule-bot", BOT)
    );
    assert_eq!(p.app_id.as_deref(), Some("555"));
    assert_eq!(p.content_intent, Some(true));
    assert_eq!(p.identify, Some((990, 1000)));
    assert!(
        d.state().requests.iter().all(|q| q.method == "GET"),
        "doctor changes nothing"
    );
    let err = dc::probe(&d.api, "nope").await.unwrap_err();
    assert!(err.contains("Reset Token"), "{err}");
    assert_eq!(
        dc::register_commands(&d.api, TOKEN, "555").await.unwrap(),
        11
    );
    let put = d.state().find("PUT", "/applications/555/commands")[0].json();
    let names: Vec<&str> = put
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"status") && names.contains(&"undo"));
    assert!(dc::invite_url("555").contains("permissions=274877975552"));
    assert!(dc::invite_url("555").contains("scope=bot+applications.commands"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_pairs_the_first_dm_with_the_code_and_tells_nobody_else_anything() {
    let d = Discord::start();
    let ch = DiscordChannel::with_api(TOKEN, &d.api).with_pairing("ferrule-4827");
    let mut r = start_with(&d, ch);
    until("READY", T, || d.state().readies >= 1).await;
    d.dm("m1", "2002", "hello");
    d.dm("m2", "1001", "ferrule-4827");
    until("paired", T, || r.ch.paired().is_some()).await;
    assert_eq!(r.ch.paired(), Some(("1001".into(), "user1001".into())));
    quiet(&mut r).await;
    let told = d.state().messages();
    assert_eq!(told.len(), 1);
    assert_eq!(told[0].0, "71001");
    assert!(told[0].1["content"]
        .as_str()
        .unwrap()
        .starts_with("Paired."));
}

// --- streaming through the router ---------------------------------------

struct Streamer;

#[async_trait::async_trait]
impl Provider for Streamer {
    fn name(&self) -> &str {
        "streamer"
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let mut all = String::new();
        for i in 0..60 {
            let p = format!("word{i} {}\n", "x".repeat(50));
            if let Some(s) = &req.stream {
                s.send(Delta::Text(p.clone()));
            }
            all.push_str(&p);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(CompletionResponse {
            message: Message::assistant(Some(all), vec![], None),
            usage: Usage::default(),
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_streamed_reply_is_posted_edited_and_rolls_over_before_2000() {
    let d = Discord::start();
    let r = start(&d, &["1001"], &[]).await;
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
        HashMap::from([("discord".to_string(), ch)]),
    )
    .with_streaming(
        ["discord".to_string()],
        StreamPacing {
            first_after: Duration::from_millis(30),
            first_chars: 10_000,
            every: Duration::from_millis(80),
            limit: 4000,
        },
    );
    let answer = router
        .dispatch_and_wait(InboundMessage {
            channel: "discord".into(),
            chat_id: "1001".into(),
            sender: "max".into(),
            sender_id: Some("1001".into()),
            message_id: "m1".into(),
            text: "go".into(),
            attachments: vec![],
            reply_to: None,
            ts: 0,
        })
        .await
        .unwrap()
        .text;
    let s = d.state();
    let posts = s.messages();
    let edits = s.find("PATCH", "/channels/71001/messages/");
    assert!(posts.len() >= 2, "rolled over: {} posts", posts.len());
    assert!(!edits.is_empty());
    for q in &edits {
        assert!(q.json()["content"].as_str().unwrap().encode_utf16().count() <= 2000);
    }
    let words = answer.split_whitespace().count();
    assert!(words > 100, "{words}");
}

// --- the token stays out of logs ------------------------------------------

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
async fn no_log_line_or_error_carries_the_token() {
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
    let d = Discord::start();
    d.state().plans.push_back(Plan::Close(1011));
    let mut r = start(&d, &["1001"], &[]).await;
    assert!(d.state().connections >= 2, "reconnected after the 1011");
    d.once(
        "POST",
        "/channels/",
        Response::json(json!({ "message": "boom" })).with_status(500),
    );
    let err = r.ch.send(out("1001", "x")).await.unwrap_err().to_string();
    d.dm("m1", "2002", "stranger");
    d.close(4004);
    let end = tokio::time::timeout(T, &mut r.run)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err()
        .to_string();
    let logs = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("discord"), "{logs}");
    for text in [&logs, &err, &end] {
        assert!(!text.contains(TOKEN), "{text}");
        assert!(!text.contains("127.0.0.1"), "no URL: {text}");
    }
}

// --- live ------------------------------------------------------------------

/// Against real Discord: connects, DMs the user, waits up to two minutes
/// for their reply. `FERRULE_LIVE_DISCORD_TOKEN`, `FERRULE_LIVE_DISCORD_USER`;
/// see docs/discord.md.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Discord bot; see docs/discord.md"]
async fn discord_live_round_trip() {
    let token = std::env::var("FERRULE_LIVE_DISCORD_TOKEN").expect("FERRULE_LIVE_DISCORD_TOKEN");
    let user = std::env::var("FERRULE_LIVE_DISCORD_USER").expect("FERRULE_LIVE_DISCORD_USER");
    let probe = dc::probe(dc::API_URL, &token).await.unwrap();
    println!(
        "bot {} ({}), content intent {:?}",
        probe.bot_name, probe.bot_id, probe.content_intent
    );
    let ch = Arc::new(DiscordChannel::new(token).with_allowed(vec![user.clone()], vec![]));
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
        .post(out(&user, "got it: streaming…"))
        .await
        .unwrap()
        .unwrap();
    ch.edit(&user, &id, &format!("got it: {}", m.text))
        .await
        .unwrap();
}
