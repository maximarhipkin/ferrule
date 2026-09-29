//! M39 §7: the Mattermost adapter against a mock server — the socket's
//! challenge, DMs by user id, usernames resolved, channels by mention or
//! a thread the bot answered in, strangers and unlisted channels ignored,
//! approvals by reaction, files both ways, edits and the 👀, rate limits,
//! a revoked token reported, reconnects, pairing, and the probe.

mod support;

use ferrule_gateway::channel::{Button, ButtonAction};
use ferrule_gateway::channels::files::Inbox;
use ferrule_gateway::channels::mattermost::{self, MattermostChannel, MattermostConfig};
use ferrule_gateway::{Attachment, Channel, GatewayError, InboundMessage, OutboundMessage};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use support::http::Response;
use support::mattermost::{
    self as mock, dm, reaction, said, Mattermost, BOT, CHANNEL, DM, MAX, OTHER, STRANGER, TOKEN,
};
use support::until;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const T: Duration = Duration::from_secs(10);

struct Running {
    ch: Arc<MattermostChannel>,
    rx: mpsc::Receiver<InboundMessage>,
    run: JoinHandle<Result<(), GatewayError>>,
}

fn config(mm: &Mattermost) -> MattermostConfig {
    MattermostConfig {
        server_url: mm.url.clone(),
        token: TOKEN.into(),
        inbox: None,
    }
}

fn start(ch: MattermostChannel) -> Running {
    let ch = Arc::new(ch.with_fast_retries());
    let (tx, rx) = mpsc::channel(32);
    let c = ch.clone();
    let run = tokio::spawn(async move { c.run(tx).await });
    Running { ch, rx, run }
}

fn spawn(cfg: MattermostConfig) -> Running {
    start(MattermostChannel::new(cfg).with_allowed(vec![MAX.into()], vec![CHANNEL.into()]))
}

async fn recv(r: &mut Running) -> InboundMessage {
    tokio::time::timeout(T, r.rx.recv())
        .await
        .expect("a message in time")
        .expect("the channel open")
}

/// Waits until the socket took the token.
async fn connected(mm: &Mattermost, n: usize) {
    until("the socket's challenge", T, || {
        mm.state()
            .frames
            .iter()
            .filter(|f| f["action"] == "authentication_challenge")
            .count()
            >= n
    })
    .await;
}

fn out(chat: &str, text: &str) -> OutboundMessage {
    OutboundMessage {
        channel: "mattermost".into(),
        chat_id: chat.into(),
        text: text.into(),
        reply_to: None,
        attachments: vec![],
    }
}

fn pid(n: u32) -> String {
    format!("q{n:025}")
}

#[tokio::test]
async fn a_dm_arrives_by_user_id_and_the_answer_goes_to_its_channel() {
    let mm = mock::start();
    let mut r = spawn(config(&mm));
    connected(&mm, 1).await;
    let challenge = mm.state().frames[0].clone();
    assert_eq!(challenge["data"]["token"], TOKEN);
    assert_eq!(challenge["seq"], 1);

    mm.send(dm(&pid(1), MAX, "hello **there**"));
    let m = recv(&mut r).await;
    assert_eq!(m.channel, "mattermost");
    assert_eq!(m.chat_id, MAX);
    assert_eq!(m.sender, "max");
    assert_eq!(m.sender_id.as_deref(), Some(MAX));
    assert_eq!(m.message_id, pid(1));
    assert_eq!(m.text, "hello **there**");
    assert_eq!(m.ts, 1_760_000_000);

    // Markdown as is, split at 16 383 characters, in the DM channel.
    r.ch.send(out(MAX, &format!("**done**\n{}", "x".repeat(17_000))))
        .await
        .unwrap();
    let posts = mm.state().posts_in(DM);
    assert_eq!(posts.len(), 2);
    assert!(posts[0]["message"]
        .as_str()
        .unwrap()
        .starts_with("**done**"));
    assert!(posts[0].get("root_id").is_none());
    assert!(mm.state().calls("POST /api/v4/channels/direct").is_empty());
    for req in mm.state().requests.iter() {
        assert_eq!(
            req.header("authorization"),
            Some(&*format!("Bearer {TOKEN}"))
        );
    }

    // Pings keep the socket alive and nothing reconnects.
    until("a ping", T, || {
        mm.state().frames.iter().any(|f| f["action"] == "ping")
    })
    .await;
    assert_eq!(mm.state().connections, 1);
    assert!(r.ch.last_ok_poll().is_some());
    assert!(r.ch.problem().is_none());
    r.run.abort();
}

/// `ferrule tasks run-now`: an owner's result opens the DM channel.
#[tokio::test]
async fn a_send_to_a_user_opens_the_dm_and_a_username_is_looked_up() {
    let mm = mock::start();
    let once = MattermostChannel::new(config(&mm)).with_allowed(vec![MAX.into()], vec![]);
    let id = once.post(out(MAX, "your report")).await.unwrap().unwrap();
    assert_eq!(id.len(), 26);
    let direct = mm.state().calls("POST /api/v4/channels/direct");
    assert_eq!(direct.len(), 1);
    assert_eq!(direct[0].json(), json!([BOT, MAX]));
    assert_eq!(mm.state().posts_in(DM).len(), 1);

    let fresh = MattermostChannel::new(config(&mm));
    fresh.send(out("@max", "by name")).await.unwrap();
    assert_eq!(mm.state().posts_in(DM).len(), 2);
    let e = fresh
        .send(out("nobody", "x"))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("neither a channel id nor a username"), "{e}");

    // An id that isn't an allowed user is a channel.
    fresh.send(out(OTHER, "to a channel")).await.unwrap();
    assert_eq!(mm.state().posts_in(OTHER).len(), 1);
}

#[tokio::test]
async fn usernames_in_the_allow_list_are_resolved_at_start() {
    let mm = mock::start();
    let mut r = start(
        MattermostChannel::new(config(&mm))
            .with_allowed(vec!["@Max".into(), "@ghost".into()], vec![]),
    );
    connected(&mm, 1).await;
    let asked = mm.state().calls("POST /api/v4/users/usernames");
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].json(), json!(["max", "ghost"]));
    mm.send(dm(&pid(2), MAX, "hi"));
    assert_eq!(recv(&mut r).await.chat_id, MAX);
    r.run.abort();
}

#[tokio::test]
async fn channels_need_a_mention_or_our_thread_and_strangers_are_ignored() {
    let mm = mock::start();
    let mut r = spawn(config(&mm));
    connected(&mm, 1).await;

    // Unaddressed, an unlisted channel, a stranger's DM, our own post and
    // another bot's: nothing reaches the agent.
    mm.send(said(
        &pid(1),
        CHANNEL,
        STRANGER,
        "just chatting",
        None,
        false,
    ));
    mm.send(said(&pid(2), OTHER, MAX, "help", None, true));
    mm.send(dm(&pid(3), STRANGER, "let me in"));
    mm.send(mock::posted(
        DM,
        "D",
        mock::post(&pid(4), DM, BOT, "me", None),
        &[],
    ));
    let mut bot = mock::post(&pid(5), DM, MAX, "a webhook", None);
    bot["props"] = json!({ "from_bot": "true" });
    mm.send(mock::posted(DM, "D", bot, &[]));
    let mut system = mock::post(&pid(6), DM, MAX, "joined", None);
    system["type"] = json!("system_join_channel");
    mm.send(mock::posted(DM, "D", system, &[]));

    // Mentioned in the allowed channel, by anyone there: the thread is
    // the chat, the mention is gone.
    mm.send(said(&pid(7), CHANNEL, STRANGER, "what's up?", None, true));
    let m = recv(&mut r).await;
    assert_eq!(m.message_id, pid(7));
    assert_eq!(m.chat_id, format!("{CHANNEL}/{}", pid(7)));
    assert_eq!(m.text, "what's up?");
    assert_eq!(m.sender, "eve");
    // A stranger isn't told anything while someone is allowed.
    assert!(mm.state().posts().is_empty());

    // The answer stays in the thread; a follow-up there needs no mention.
    r.ch.send(out(&m.chat_id, "all good")).await.unwrap();
    let p = mm.state().posts_in(CHANNEL);
    assert_eq!(p[0]["root_id"], pid(7));
    mm.send(said(
        &pid(8),
        CHANNEL,
        MAX,
        "and now?",
        Some(&pid(7)),
        false,
    ));
    let m = recv(&mut r).await;
    assert_eq!(m.chat_id, format!("{CHANNEL}/{}", pid(7)));
    assert_eq!(m.text, "and now?");

    // Named in the text without the mentions list (an older server).
    mm.send(mock::posted(
        CHANNEL,
        "O",
        mock::post(&pid(9), CHANNEL, MAX, "thanks @Ferrule.", None),
        &[],
    ));
    let m = recv(&mut r).await;
    assert_eq!(m.message_id, pid(9));
    // `@ferrule-dev` is someone else.
    mm.send(mock::posted(
        CHANNEL,
        "O",
        mock::post(&pid(10), CHANNEL, MAX, "@ferrule-dev ping", None),
        &[],
    ));
    mm.send(dm(&pid(11), MAX, "last"));
    assert_eq!(recv(&mut r).await.message_id, pid(11));
    r.run.abort();
}

#[tokio::test]
async fn a_stranger_is_told_their_id_once_while_nobody_is_allowed() {
    let mm = mock::start();
    let r = start(MattermostChannel::new(config(&mm)));
    connected(&mm, 1).await;
    mm.send(dm(&pid(1), STRANGER, "hi"));
    mm.send(dm(&pid(2), STRANGER, "hello?"));
    until("the stranger's answer", T, || {
        !mm.state().posts().is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let posts = mm.state().posts();
    assert_eq!(posts.len(), 1);
    let text = posts[0]["message"].as_str().unwrap();
    assert!(
        text.contains(STRANGER) && text.contains("[gateway.mattermost] allowed_users"),
        "{text}"
    );
    r.run.abort();
}

#[tokio::test]
async fn approvals_are_reactions_and_come_back_as_their_command() {
    let mm = mock::start();
    let mut r = spawn(config(&mm));
    connected(&mm, 1).await;
    let buttons = [
        Button {
            text: "Approve".into(),
            action: ButtonAction::Command("yes ab12".into()),
        },
        Button {
            text: "Deny".into(),
            action: ButtonAction::Command("no ab12".into()),
        },
        Button {
            text: "Always".into(),
            action: ButtonAction::Command("always ab12".into()),
        },
        Button {
            text: "Docs".into(),
            action: ButtonAction::Url("https://example.com/docs".into()),
        },
    ];
    r.ch.send_buttons(out(MAX, "Run `rm -rf build`?"), &buttons)
        .await
        .unwrap();
    let post = mm.state().posts_in(DM)[0].clone();
    let text = post["message"].as_str().unwrap();
    assert!(
        text.contains("• :+1: Approve: react :+1:, or send `yes ab12`"),
        "{text}"
    );
    assert!(
        text.contains("• :-1: Deny: react :-1:, or send `no ab12`"),
        "{text}"
    );
    assert!(text.contains(":one: Always"), "{text}");
    assert!(text.contains("https://example.com/docs"), "{text}");
    let id = format!("p{:025}", 1);
    let reactions = mm.state().reactions();
    assert_eq!(
        reactions,
        vec![
            (id.clone(), "+1".to_string()),
            (id.clone(), "-1".to_string()),
            (id.clone(), "one".to_string())
        ]
    );

    // A stranger's reaction, our own, and one on another post: nothing.
    mm.send(reaction(STRANGER, &id, "+1"));
    mm.send(reaction(BOT, &id, "+1"));
    mm.send(reaction(MAX, &pid(9), "+1"));
    mm.send(reaction(MAX, &id, "-1"));
    let m = recv(&mut r).await;
    assert_eq!(m.text, "no ab12");
    assert_eq!(m.chat_id, MAX);
    assert_eq!(m.sender_id.as_deref(), Some(MAX));
    // Used once.
    mm.send(reaction(MAX, &id, "+1"));
    mm.send(dm(&pid(3), MAX, "next"));
    assert_eq!(recv(&mut r).await.text, "next");
    r.run.abort();
}

#[tokio::test]
async fn files_come_in_to_the_inbox_and_go_out_as_uploads() {
    let mm = mock::start();
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = config(&mm);
    cfg.inbox = Some(Inbox::new(ws.path(), 1));
    let mut r = spawn(cfg);
    connected(&mm, 1).await;

    mm.file(
        "f1aaaaaaaaaaaaaaaaaaaaaaaa",
        "notes.txt",
        "text/plain",
        "remember the milk",
    );
    mm.file(
        "f2aaaaaaaaaaaaaaaaaaaaaaaa",
        "big.txt",
        "text/plain",
        &"x".repeat(2 * 1024 * 1024),
    );
    let mut p = mock::post(&pid(1), DM, MAX, "see these", None);
    p["file_ids"] = json!(["f1aaaaaaaaaaaaaaaaaaaaaaaa", "f2aaaaaaaaaaaaaaaaaaaaaaaa"]);
    // Only the first comes with its metadata: the other's is asked for.
    p["metadata"] = json!({ "files": [{ "id": "f1aaaaaaaaaaaaaaaaaaaaaaaa", "name": "notes.txt", "mime_type": "text/plain", "size": 17 }] });
    mm.send(mock::posted(DM, "D", p, &[]));
    let m = recv(&mut r).await;
    assert!(m.text.starts_with("see these"), "{}", m.text);
    assert!(m.text.contains("inbox/mattermost/"), "{}", m.text);
    assert!(m.text.contains("big.txt"), "{}", m.text);
    assert_eq!(m.attachments.len(), 1);
    let saved = std::fs::read_to_string(&m.attachments[0].url).unwrap();
    assert_eq!(saved, "remember the milk");
    assert_eq!(
        mm.state()
            .calls("GET /api/v4/files/f2aaaaaaaaaaaaaaaaaaaaaaaa/info")
            .len(),
        1
    );
    // The big one was never downloaded, and its sender was told.
    assert!(mm
        .state()
        .calls("GET /api/v4/files/f2aaaaaaaaaaaaaaaaaaaaaaaa ")
        .is_empty());
    assert_eq!(
        mm.state()
            .calls("GET /api/v4/files/f2aaaaaaaaaaaaaaaaaaaaaaaa")
            .len(),
        1
    );
    let told = mm.state().posts_in(DM);
    assert!(
        told[0]["message"].as_str().unwrap().contains("big.txt"),
        "{told:?}"
    );

    // Out: uploaded to the channel, then on the post.
    let file = ws.path().join("report.csv");
    std::fs::write(&file, "a,b\n1,2\n").unwrap();
    let mut msg = out(MAX, "here it is");
    msg.attachments = vec![Attachment {
        kind: "text/csv".into(),
        url: file.to_string_lossy().into_owned(),
        name: Some("report.csv".into()),
    }];
    r.ch.send(msg).await.unwrap();
    let up = mm.state().calls("POST /api/v4/files");
    assert_eq!(up.len(), 1);
    let body = String::from_utf8_lossy(&up[0].body).to_string();
    assert!(
        body.contains("name=\"channel_id\"\r\n\r\nd00000000000000000000dmmax"),
        "{body}"
    );
    assert!(
        body.contains("filename=\"report.csv\"") && body.contains("a,b\n1,2"),
        "{body}"
    );
    let last = mm.state().posts_in(DM).last().cloned().unwrap();
    assert_eq!(last["message"], "here it is");
    assert_eq!(last["file_ids"].as_array().unwrap().len(), 1);

    // An upload the server refuses as too big says so.
    mm.once(
        "POST /api/v4/files",
        Response::json(json!({ "message": "too large" })).with_status(413),
    );
    let mut msg = out(MAX, "");
    msg.attachments = vec![Attachment {
        kind: "text/csv".into(),
        url: file.to_string_lossy().into_owned(),
        name: None,
    }];
    let e = r.ch.send(msg).await.unwrap_err().to_string();
    assert!(e.contains("Maximum File Size"), "{e}");
    r.run.abort();
}

#[tokio::test]
async fn without_an_inbox_a_file_alone_is_answered_not_read() {
    let mm = mock::start();
    let mut r = spawn(config(&mm));
    connected(&mm, 1).await;
    mm.file(
        "f1aaaaaaaaaaaaaaaaaaaaaaaa",
        "photo.jpg",
        "image/jpeg",
        "jpeg",
    );
    let mut p = mock::post(&pid(1), DM, MAX, "", None);
    p["file_ids"] = json!(["f1aaaaaaaaaaaaaaaaaaaaaaaa"]);
    mm.send(mock::posted(DM, "D", p, &[]));
    until("the answer", T, || !mm.state().posts().is_empty()).await;
    let told = mm.state().posts()[0]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(told.contains("I got your image"), "{told}");
    mm.send(dm(&pid(2), MAX, "ok"));
    assert_eq!(recv(&mut r).await.text, "ok");
    r.run.abort();
}

#[tokio::test]
async fn edits_patch_the_post_and_the_eyes_mark_it_read() {
    let mm = mock::start();
    let ch =
        MattermostChannel::new(config(&mm)).with_allowed(vec![MAX.into()], vec![CHANNEL.into()]);
    let id = ch.post(out(MAX, "thinking…")).await.unwrap().unwrap();
    ch.edit(MAX, &id, "done").await.unwrap();
    let patch = mm.state().calls(&format!("PUT /api/v4/posts/{id}/patch"));
    assert_eq!(patch[0].json(), json!({ "message": "done" }));
    assert_eq!(ch.stream_every(), Some(Duration::from_secs(2)));
    assert_eq!(ch.message_limit(), Some(16_383));

    let chat = format!("{CHANNEL}/{}", pid(4));
    ch.react(&chat, &pid(4), "👀").await.unwrap();
    let view = mm.state().calls("POST /api/v4/channels/members/me/view");
    assert_eq!(view[0].json(), json!({ "channel_id": CHANNEL }));
    assert_eq!(mm.state().reactions(), vec![(pid(4), "eyes".to_string())]);
    // An emoji Mattermost has no name for is skipped, not an error.
    ch.react(&chat, &pid(4), "🦀").await.unwrap();
    assert_eq!(mm.state().reactions().len(), 1);
}

#[tokio::test]
async fn rate_limits_are_waited_out_then_reported() {
    let mm = mock::start();
    let ch = MattermostChannel::new(config(&mm))
        .with_allowed(vec![], vec![CHANNEL.into()])
        .with_fast_retries();
    let limited = || {
        Response::json(json!({ "message": "too many requests" }))
            .with_status(429)
            .with_header("X-Ratelimit-Reset", "0")
    };
    mm.once("POST /api/v4/posts", limited());
    ch.send(out(CHANNEL, "once")).await.unwrap();
    assert_eq!(mm.state().calls("POST /api/v4/posts").len(), 2);

    for _ in 0..4 {
        mm.once("POST /api/v4/posts", limited());
    }
    let e = ch.send(out(CHANNEL, "never")).await.unwrap_err();
    assert!(matches!(e, GatewayError::RateLimited { .. }), "{e}");

    mm.once(
        "POST /api/v4/posts",
        Response::json(json!({ "message": "no permission" })).with_status(403),
    );
    let e = ch.send(out(CHANNEL, "x")).await.unwrap_err().to_string();
    assert!(e.contains("is the bot a member"), "{e}");
}

#[tokio::test]
async fn a_revoked_token_stops_the_channel_with_a_problem_to_act_on() {
    let mm = mock::start();
    mm.state().revoked = true;
    let r = spawn(config(&mm));
    let res = tokio::time::timeout(T, r.run).await.unwrap().unwrap();
    let e = res.unwrap_err().to_string();
    assert!(e.contains("token was refused"), "{e}");
    let p = r.ch.problem().unwrap();
    assert!(p.contains("Bot Accounts") && !p.contains(TOKEN), "{p}");
}

#[tokio::test]
async fn a_closed_socket_is_reopened() {
    let mm = mock::start();
    let mut r = spawn(config(&mm));
    connected(&mm, 1).await;
    mm.close();
    connected(&mm, 2).await;
    until("the second connection", T, || mm.state().connections == 2).await;
    mm.send(dm(&pid(1), MAX, "still there?"));
    assert_eq!(recv(&mut r).await.text, "still there?");
    // The bot's own account was asked once.
    assert_eq!(mm.state().calls("GET /api/v4/users/me").len(), 1);
    r.run.abort();
}

#[tokio::test]
async fn setup_pairs_by_code() {
    let mm = mock::start();
    let r = start(MattermostChannel::new(config(&mm)).with_pairing("482913"));
    connected(&mm, 1).await;
    mm.send(dm(&pid(1), STRANGER, "wrong"));
    mm.send(dm(&pid(2), STRANGER, " 482913 "));
    until("the pairing", T, || r.ch.paired().is_some()).await;
    assert_eq!(
        r.ch.paired(),
        Some((STRANGER.to_string(), "eve".to_string()))
    );
    until("the answer", T, || !mm.state().posts().is_empty()).await;
    let posts = mm.state().posts();
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert!(posts[0]["message"].as_str().unwrap().starts_with("Paired."));
    r.run.abort();
}

#[tokio::test]
async fn the_probe_reads_the_account_and_says_what_is_wrong() {
    let mm = mock::start();
    let p = mattermost::probe(config(&mm)).await.unwrap();
    assert_eq!(p.user_id, BOT);
    assert!(
        p.summary().starts_with("@ferrule on 127.0.0.1:"),
        "{}",
        p.summary()
    );
    assert!(mm.state().posts().is_empty());

    let list = mattermost::channels(config(&mm)).await.unwrap();
    let names: Vec<(&str, &str)> = list
        .iter()
        .map(|c| (c.id.as_str(), c.name.as_str()))
        .collect();
    assert_eq!(names, vec![(CHANNEL, "Town Square"), (OTHER, "ops")]);
    assert_eq!(list[0].team, "Acme");
    assert_eq!(mattermost::user_id(config(&mm), "@Max").await.unwrap(), MAX);
    let e = mattermost::user_id(config(&mm), "ghost").await.unwrap_err();
    assert!(e.contains("no user @ghost"), "{e}");

    let mut bad = config(&mm);
    bad.token = "wrong".into();
    let e = mattermost::probe(bad).await.unwrap_err();
    assert!(e.contains("token was refused"), "{e}");

    let mut not = config(&mm);
    not.server_url = format!("{}/not-here", mm.url);
    let e = mattermost::probe(not).await.unwrap_err();
    assert!(e.contains("doesn't answer as a Mattermost server"), "{e}");

    let mut plain = config(&mm);
    plain.server_url = "chat.example.com".into();
    let e = mattermost::probe(plain).await.unwrap_err();
    assert!(e.contains("https://chat.example.com"), "{e}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn mattermost_live_round_trip() {
    let (Ok(url), Ok(token), Ok(to)) = (
        std::env::var("FERRULE_LIVE_MATTERMOST_URL"),
        std::env::var("FERRULE_LIVE_MATTERMOST_TOKEN"),
        std::env::var("FERRULE_LIVE_MATTERMOST_TO"),
    ) else {
        eprintln!("set FERRULE_LIVE_MATTERMOST_URL, _TOKEN and _TO (a user id or @username)");
        return;
    };
    let cfg = MattermostConfig {
        server_url: url,
        token,
        inbox: None,
    };
    let p = mattermost::probe(cfg.clone()).await.unwrap();
    eprintln!("probe: {}", p.summary());
    let ch = MattermostChannel::new(cfg).with_allowed(vec![to.clone()], vec![]);
    let id = ch
        .post(out(&to, "ferrule live test: **hello** from `cargo test`"))
        .await
        .unwrap()
        .unwrap();
    ch.edit(&to, &id, "ferrule live test: **edited**")
        .await
        .unwrap();
    ch.react(&to, &id, "👀").await.unwrap();
}
