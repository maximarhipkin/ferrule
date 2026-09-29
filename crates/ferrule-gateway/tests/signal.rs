//! M39 §6: the Signal adapter against a mock signal-cli daemon — DMs by
//! number or uuid, strangers dropped, groups by mention or reply only,
//! Note to Self, files both ways, Markdown as text styles, the 👀 with a
//! read receipt, delivery failures and rate limits, the stream lost and
//! resumed, a multi-account daemon, pairing, the probe, and (on Unix) the
//! daemon ferrule starts and restarts itself.

mod support;

use ferrule_gateway::channels::files::Inbox;
use ferrule_gateway::channels::signal::{self, SignalChannel, SignalConfig};
use ferrule_gateway::{Attachment, Channel, GatewayError, InboundMessage, OutboundMessage};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use support::signal::{
    self as mock, SignalDaemon, ACCOUNT, BOT_UUID, GROUP, MAX, MAX_UUID, NOBODY, OTHER_GROUP,
    STRANGER,
};
use support::until;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const T: Duration = Duration::from_secs(10);

struct Running {
    ch: Arc<SignalChannel>,
    rx: mpsc::Receiver<InboundMessage>,
    run: JoinHandle<Result<(), GatewayError>>,
}

fn config(d: &SignalDaemon) -> SignalConfig {
    SignalConfig {
        account: ACCOUNT.into(),
        url: d.url.clone(),
        daemon: None,
        inbox: None,
    }
}

fn start(ch: SignalChannel) -> Running {
    let ch = Arc::new(ch.with_fast_retries());
    let (tx, rx) = mpsc::channel(32);
    let c = ch.clone();
    let run = tokio::spawn(async move { c.run(tx).await });
    Running { ch, rx, run }
}

fn spawn(cfg: SignalConfig) -> Running {
    start(SignalChannel::new(cfg).with_allowed(vec![MAX.into()], vec![GROUP.into()]))
}

async fn listening(d: &SignalDaemon) {
    until("the event stream", T, || d.state().open > 0).await;
}

async fn recv(r: &mut Running) -> InboundMessage {
    tokio::time::timeout(T, r.rx.recv())
        .await
        .expect("a message in time")
        .expect("the channel open")
}

async fn nothing(r: &mut Running) {
    let got = tokio::time::timeout(Duration::from_millis(300), r.rx.recv()).await;
    assert!(got.is_err(), "expected nothing, got {got:?}");
}

fn out(chat: &str, text: &str) -> OutboundMessage {
    OutboundMessage {
        channel: "signal".into(),
        chat_id: chat.into(),
        text: text.into(),
        reply_to: None,
        attachments: vec![],
    }
}

#[tokio::test]
async fn a_dm_in_and_a_styled_quoted_reply_out() {
    let d = mock::start();
    let mut r = spawn(config(&d));
    listening(&d).await;
    d.push(mock::dm(MAX, "hello there", 1_760_000_000_001));
    let m = recv(&mut r).await;
    assert_eq!(m.channel, "signal");
    assert_eq!(m.chat_id, MAX);
    assert_eq!(m.sender, "Max");
    assert_eq!(m.sender_id.as_deref(), Some(MAX));
    assert_eq!(m.text, "hello there");
    assert_eq!(m.message_id, format!("1760000000001:{MAX}"));
    assert_eq!(m.ts, 1_760_000_000);

    let mut reply = out(MAX, "**done**, see `x`");
    reply.reply_to = Some(m.message_id.clone());
    let id = r.ch.post(reply).await.unwrap().unwrap();
    assert!(id.ends_with(&format!(":{ACCOUNT}")), "{id}");
    let sent = d.calls("send");
    assert_eq!(sent.len(), 1);
    let s = &sent[0];
    assert_eq!(s["recipient"], json!([MAX]));
    assert_eq!(s["message"], "done, see x");
    assert_eq!(s["textStyle"], json!(["0:4:BOLD", "10:1:MONOSPACE"]));
    assert_eq!(s["quoteTimestamp"], 1_760_000_000_001i64);
    assert_eq!(s["quoteAuthor"], MAX);
    assert!(r.ch.last_ok_poll().is_some());
    assert!(r.ch.problem().is_none());
    r.run.abort();
}

#[tokio::test]
async fn a_long_reply_is_cut_to_signals_limit() {
    let d = mock::start();
    let r = spawn(config(&d));
    let long = "word ".repeat(1000);
    r.ch.send(out(MAX, &long)).await.unwrap();
    let sent = d.calls("send");
    assert!(sent.len() >= 3, "{}", sent.len());
    for s in &sent {
        assert!(s["message"].as_str().unwrap().chars().count() <= signal::MESSAGE_LIMIT);
    }
    r.run.abort();
}

#[tokio::test]
async fn a_stranger_is_told_their_number_only_while_nobody_is_allowed() {
    let d = mock::start();
    let mut r = spawn(config(&d));
    listening(&d).await;
    d.push(mock::dm(STRANGER, "hi", 1_760_000_000_002));
    nothing(&mut r).await;
    assert!(d.calls("send").is_empty(), "a list in use: silence");
    r.run.abort();

    let d = mock::start();
    let mut r = start(SignalChannel::new(config(&d)));
    listening(&d).await;
    d.push(mock::dm(STRANGER, "hi", 1_760_000_000_003));
    nothing(&mut r).await;
    let sent = d.calls("send");
    assert_eq!(sent.len(), 1);
    let t = sent[0]["message"].as_str().unwrap();
    assert!(
        t.contains(STRANGER) && t.contains("[gateway.signal] allowed_users"),
        "{t}"
    );
    r.run.abort();
}

#[tokio::test]
async fn a_sender_allowed_by_uuid_chats_by_uuid() {
    let d = mock::start();
    let mut r = start(SignalChannel::new(config(&d)).with_allowed(vec![MAX_UUID.into()], vec![]));
    listening(&d).await;
    let mut ev = mock::dm(MAX, "by uuid", 1_760_000_000_004);
    // A sender who hides their number.
    ev["envelope"]["sourceNumber"] = json!(null);
    ev["envelope"]["source"] = json!(MAX_UUID);
    d.push(ev);
    let m = recv(&mut r).await;
    assert_eq!(m.chat_id, MAX_UUID);
    r.ch.send(out(&m.chat_id, "ok")).await.unwrap();
    assert_eq!(d.calls("send")[0]["recipient"], json!([MAX_UUID]));
    r.run.abort();
}

#[tokio::test]
async fn groups_need_the_list_and_a_mention_or_a_reply() {
    let d = mock::start();
    let mut r = spawn(config(&d));
    listening(&d).await;
    d.push(mock::group(
        STRANGER,
        GROUP,
        "chatting among ourselves",
        1_760_000_000_010,
        false,
    ));
    d.push(mock::group(
        MAX,
        OTHER_GROUP,
        "not this group",
        1_760_000_000_011,
        true,
    ));
    nothing(&mut r).await;

    d.push(mock::group(
        STRANGER,
        GROUP,
        "what's the plan?",
        1_760_000_000_012,
        true,
    ));
    let m = recv(&mut r).await;
    assert_eq!(m.chat_id, GROUP);
    assert_eq!(m.sender_id.as_deref(), Some(STRANGER));
    assert_eq!(m.text, "what's the plan?", "our mention taken out");

    // Someone else's mention stays readable; a reply to us counts.
    let mut ev = mock::group(MAX, GROUP, "\u{FFFC} knows", 1_760_000_000_013, false);
    ev["envelope"]["dataMessage"]["mentions"] =
        json!([{ "name": "Dana", "number": "+15550000077", "start": 0, "length": 1 }]);
    ev["envelope"]["dataMessage"]["quote"] = json!({ "id": 1_760_000_100_001i64, "author": ACCOUNT, "authorNumber": ACCOUNT, "authorUuid": BOT_UUID, "text": "earlier" });
    d.push(ev);
    let m = recv(&mut r).await;
    assert_eq!(m.text, "@Dana knows");
    assert_eq!(
        m.reply_to.as_deref(),
        Some(&*format!("1760000100001:{ACCOUNT}"))
    );

    r.ch.send(out(GROUP, "on it")).await.unwrap();
    let s = d.calls("send");
    assert_eq!(s.last().unwrap()["groupId"], GROUP);
    assert!(s.last().unwrap().get("recipient").is_none());
    r.run.abort();
}

#[tokio::test]
async fn a_mention_by_uuid_alone_is_ours_too() {
    let d = mock::start();
    let mut r = spawn(config(&d));
    listening(&d).await;
    until("our uuid learned", T, || {
        !d.calls("getUserStatus").is_empty()
    })
    .await;
    let mut ev = mock::group(MAX, GROUP, "status?", 1_760_000_000_014, true);
    ev["envelope"]["dataMessage"]["mentions"][0]["number"] = json!(null);
    d.push(ev);
    assert_eq!(recv(&mut r).await.text, "status?");
    r.run.abort();
}

#[tokio::test]
async fn note_to_self_reaches_the_agent_only_when_the_owner_number_is_allowed() {
    let note = |ts: i64| {
        json!({
            "account": ACCOUNT,
            "envelope": {
                "source": ACCOUNT, "sourceNumber": ACCOUNT, "sourceUuid": BOT_UUID,
                "sourceName": "Max", "timestamp": ts,
                "syncMessage": { "sentMessage": {
                    "destination": ACCOUNT, "destinationNumber": ACCOUNT, "destinationUuid": BOT_UUID,
                    "timestamp": ts, "message": "remind me at 5"
                } }
            }
        })
    };
    let to_someone = json!({
        "account": ACCOUNT,
        "envelope": {
            "source": ACCOUNT, "sourceNumber": ACCOUNT, "sourceName": "Max", "timestamp": 5,
            "syncMessage": { "sentMessage": { "destination": MAX, "destinationNumber": MAX, "timestamp": 5, "message": "my own chat" } }
        }
    });

    let d = mock::start();
    let mut r = spawn(config(&d));
    listening(&d).await;
    d.push(note(1_760_000_000_020));
    d.push(to_someone.clone());
    nothing(&mut r).await;
    r.run.abort();

    let d = mock::start();
    let mut r = start(
        SignalChannel::new(config(&d)).with_allowed(vec![ACCOUNT.into(), MAX.into()], vec![]),
    );
    listening(&d).await;
    d.push(to_someone);
    d.push(note(1_760_000_000_021));
    let m = recv(&mut r).await;
    assert_eq!(m.chat_id, ACCOUNT);
    assert_eq!(m.text, "remind me at 5");
    assert_eq!(m.message_id, format!("1760000000021:{ACCOUNT}"));
    // Our own message: the reaction, but no receipt to ourselves.
    r.ch.react(&m.chat_id, &m.message_id, "👀").await.unwrap();
    assert!(d.calls("sendReceipt").is_empty());
    assert_eq!(d.calls("sendReaction").len(), 1);
    r.run.abort();
}

#[tokio::test]
async fn reactions_updates_and_receipts_are_not_messages() {
    let d = mock::start();
    let mut r = spawn(config(&d));
    listening(&d).await;
    let mut react = mock::dm(MAX, "", 1_760_000_000_030);
    react["envelope"]["dataMessage"]["reaction"] = json!({ "emoji": "👍", "targetAuthor": ACCOUNT, "targetSentTimestamp": 1, "isRemove": false });
    d.push(react);
    d.push(json!({ "account": ACCOUNT, "envelope": { "source": MAX, "sourceNumber": MAX, "timestamp": 2,
        "receiptMessage": { "when": 2, "isRead": true, "timestamps": [1] } } }));
    d.push(json!({ "account": ACCOUNT, "envelope": { "source": MAX, "sourceNumber": MAX, "timestamp": 3,
        "typingMessage": { "action": "STARTED", "timestamp": 3 } } }));
    d.push(mock::dm(MAX, "", 1_760_000_000_031));
    // Another account's event, on a shared daemon.
    let mut other = mock::dm(MAX, "for someone else", 1_760_000_000_032);
    other["account"] = json!("+15559999999");
    d.push(other);
    nothing(&mut r).await;
    r.run.abort();
}

#[tokio::test]
async fn the_eyes_are_a_read_receipt_and_a_reaction() {
    let d = mock::start();
    let r = spawn(config(&d));
    let id = format!("1760000000040:{MAX}");
    r.ch.react(MAX, &id, "👀").await.unwrap();
    let receipt = &d.calls("sendReceipt")[0];
    assert_eq!(receipt["recipient"], MAX);
    assert_eq!(receipt["targetTimestamp"], json!([1_760_000_000_040i64]));
    assert_eq!(receipt["type"], "read");
    let reaction = &d.calls("sendReaction")[0];
    assert_eq!(reaction["recipient"], json!([MAX]));
    assert_eq!(reaction["emoji"], "👀");
    assert_eq!(reaction["targetAuthor"], MAX);
    assert_eq!(reaction["targetTimestamp"], 1_760_000_000_040i64);

    r.ch.react(GROUP, &format!("1760000000041:{STRANGER}"), "👀")
        .await
        .unwrap();
    assert_eq!(d.calls("sendReaction")[1]["groupId"], GROUP);
    r.run.abort();
}

#[tokio::test]
async fn files_in_are_saved_and_a_big_one_is_answered() {
    let d = mock::start();
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = config(&d);
    cfg.inbox = Some(Inbox::new(ws.path(), 1));
    let mut r = spawn(cfg);
    listening(&d).await;
    d.state()
        .attachments
        .insert("att-1".into(), b"%PDF-1.4 hello".to_vec());
    let mut ev = mock::dm(MAX, "the invoice", 1_760_000_000_050);
    ev["envelope"]["dataMessage"]["attachments"] = json!([
        { "contentType": "application/pdf", "filename": "invoice.pdf", "id": "att-1", "size": 14 },
        { "contentType": "video/mp4", "filename": "huge.mp4", "id": "att-2", "size": 5_000_000 }
    ]);
    d.push(ev);
    let m = recv(&mut r).await;
    assert_eq!(m.attachments.len(), 1);
    let path = std::path::Path::new(&m.attachments[0].url);
    assert_eq!(std::fs::read(path).unwrap(), b"%PDF-1.4 hello");
    assert!(path.starts_with(ws.path().join("inbox").join("signal")));
    assert!(m.text.starts_with("the invoice\n\n"), "{}", m.text);
    assert!(m.text.contains("invoice.pdf"), "{}", m.text);
    assert!(
        m.text.contains("huge.mp4, which wasn't saved"),
        "{}",
        m.text
    );
    let got = d.calls("getAttachment");
    assert_eq!(got.len(), 1, "the big one isn't fetched");
    assert_eq!(got[0]["recipient"], MAX);
    let told = d.calls("send");
    assert!(
        told[0]["message"]
            .as_str()
            .unwrap()
            .contains("couldn't take huge.mp4"),
        "{told:?}"
    );

    // A photo without a name, and nothing else.
    d.state()
        .attachments
        .insert("att-3".into(), vec![0xFF, 0xD8, 0xFF]);
    let mut ev = mock::dm(MAX, "", 1_760_000_000_051);
    ev["envelope"]["dataMessage"]["attachments"] =
        json!([{ "contentType": "image/jpeg", "id": "att-3", "size": 3 }]);
    d.push(ev);
    let m = recv(&mut r).await;
    assert!(
        m.attachments[0].url.ends_with("att-3.jpg"),
        "{:?}",
        m.attachments
    );
    assert!(
        m.text.starts_with("[The sender attached a photo"),
        "{}",
        m.text
    );
    r.run.abort();
}

#[tokio::test]
async fn without_an_inbox_a_file_alone_is_answered_not_read() {
    let d = mock::start();
    let mut r = spawn(config(&d));
    listening(&d).await;
    let mut ev = mock::dm(MAX, "", 1_760_000_000_052);
    ev["envelope"]["dataMessage"]["attachments"] =
        json!([{ "contentType": "audio/aac", "id": "v", "size": 10 }]);
    d.push(ev);
    nothing(&mut r).await;
    let told = d.calls("send");
    assert!(
        told[0]["message"].as_str().unwrap().contains("audio file"),
        "{told:?}"
    );
    assert!(d.calls("getAttachment").is_empty());
    r.run.abort();
}

#[tokio::test]
async fn files_out_ride_on_the_first_message_as_data_uris() {
    let d = mock::start();
    let r = spawn(config(&d));
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("report.csv");
    std::fs::write(&file, "a,b\n1,2\n").unwrap();
    let mut msg = out(MAX, "here it is");
    msg.attachments = vec![Attachment {
        kind: "file".into(),
        url: file.to_string_lossy().into_owned(),
        name: None,
    }];
    r.ch.send(msg).await.unwrap();
    let s = &d.calls("send")[0];
    let uri = s["attachments"][0].as_str().unwrap();
    assert!(
        uri.starts_with("data:text/csv;filename=report.csv;base64,"),
        "{uri}"
    );
    assert!(uri.ends_with("YSxiCjEsMgo="), "{uri}");
    assert_eq!(s["message"], "here it is");

    // A file with no text still goes.
    let mut msg = out(MAX, "");
    msg.attachments = vec![Attachment {
        kind: "file".into(),
        url: file.to_string_lossy().into_owned(),
        name: Some("data.csv".into()),
    }];
    r.ch.send(msg).await.unwrap();
    assert_eq!(d.calls("send").len(), 2);
    r.run.abort();
}

#[tokio::test]
async fn delivery_failures_and_rate_limits_are_said() {
    let d = mock::start();
    let r = spawn(config(&d));
    let e = r.ch.send(out(NOBODY, "hi")).await.unwrap_err().to_string();
    assert!(e.contains("isn't on Signal"), "{e}");
    d.fail_next("send", -5, "Failed to send message due to rate limiting");
    assert!(matches!(
        r.ch.send(out(MAX, "hi")).await,
        Err(GatewayError::RateLimited { .. })
    ));
    d.fail_next("send", -1, "Invalid group id");
    let e = r.ch.send(out(GROUP, "hi")).await.unwrap_err().to_string();
    assert!(
        e.contains("signal-cli send failed: Invalid group id"),
        "{e}"
    );
    r.run.abort();
}

#[tokio::test]
async fn a_lost_stream_is_reopened_and_what_came_meanwhile_arrives() {
    let d = mock::start();
    let mut r = spawn(config(&d));
    listening(&d).await;
    d.cut();
    d.push(mock::dm(MAX, "while you were away", 1_760_000_000_060));
    let m = recv(&mut r).await;
    assert_eq!(m.text, "while you were away");
    assert!(d.state().streams >= 2);
    r.run.abort();
}

#[tokio::test]
async fn nothing_answering_is_a_problem_in_words_until_it_does() {
    let (l, port) = support::bind();
    drop(l);
    let url = format!("http://127.0.0.1:{port}");
    let mut r = start(
        SignalChannel::new(SignalConfig {
            account: ACCOUNT.into(),
            url: url.clone(),
            daemon: None,
            inbox: None,
        })
        .with_allowed(vec![MAX.into()], vec![]),
    );
    until("a problem", T, || r.ch.problem().is_some()).await;
    let p = r.ch.problem().unwrap();
    assert!(p.contains("is the signal-cli daemon running?"), "{p}");

    let l = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    l.set_nonblocking(true).unwrap();
    let d = mock::start_on(l, port);
    listening(&d).await;
    until("the problem cleared", T, || r.ch.problem().is_none()).await;
    d.push(mock::dm(MAX, "back", 1_760_000_000_061));
    assert_eq!(recv(&mut r).await.text, "back");
    r.run.abort();
}

#[tokio::test]
async fn the_stream_is_checked_while_it_is_quiet() {
    let d = mock::start();
    let r = spawn(config(&d));
    listening(&d).await;
    let first = r.ch.last_ok_poll().unwrap();
    until("a version check", T, || !d.calls("version").is_empty()).await;
    until("a newer poll", T, || r.ch.last_ok_poll().unwrap() > first).await;
    r.run.abort();
}

#[tokio::test]
async fn a_daemon_with_several_accounts_is_told_which_is_ours() {
    let d = mock::start();
    d.state().multi = true;
    let mut r = spawn(config(&d));
    r.ch.send(out(MAX, "hi")).await.unwrap();
    let sent = d.calls("send");
    assert_eq!(sent.len(), 2, "one refused, then named");
    assert_eq!(sent[1]["account"], ACCOUNT);
    listening(&d).await;
    d.push(mock::dm(MAX, "multi", 1_760_000_000_070));
    assert_eq!(recv(&mut r).await.text, "multi");
    r.run.abort();
}

#[tokio::test]
async fn pairing_by_code_allows_its_sender() {
    let d = mock::start();
    let mut r = start(SignalChannel::new(config(&d)).with_pairing("ferrule-1234"));
    listening(&d).await;
    d.push(mock::dm(STRANGER, "hi", 1_760_000_000_080));
    d.push(mock::dm(MAX, "ferrule-1234", 1_760_000_000_081));
    until("paired", T, || r.ch.paired().is_some()).await;
    assert_eq!(r.ch.paired(), Some((MAX.to_string(), "Max".to_string())));
    until("the answer", T, || !d.calls("send").is_empty()).await;
    let told = d.calls("send");
    assert_eq!(told.len(), 1, "the stranger is told nothing during setup");
    assert_eq!(told[0]["recipient"], json!([MAX]));
    assert!(told[0]["message"].as_str().unwrap().starts_with("Paired."));
    d.push(mock::dm(MAX, "now", 1_760_000_000_082));
    assert_eq!(recv(&mut r).await.text, "now");
    r.run.abort();
}

#[tokio::test]
async fn the_probe_names_the_version_and_the_groups() {
    let d = mock::start();
    let p = signal::probe(&d.url, ACCOUNT).await.unwrap();
    assert_eq!(p.version, "0.13.9");
    assert_eq!(p.groups.len(), 2, "a group we left isn't offered");
    assert_eq!(p.groups[0], (GROUP.to_string(), "Team".to_string()));
    assert_eq!(
        p.summary(),
        format!("{ACCOUNT} · signal-cli 0.13.9 · 2 groups")
    );
    assert!(d.calls("send").is_empty());

    let (l, port) = support::bind();
    drop(l);
    let e = signal::probe(&format!("http://127.0.0.1:{port}"), ACCOUNT)
        .await
        .unwrap_err();
    assert!(e.contains("is the signal-cli daemon running?"), "{e}");
    let e = signal::probe("127.0.0.1:7583", ACCOUNT).await.unwrap_err();
    assert!(e.contains("a URL like"), "{e}");
}

/// The daemon ferrule starts: a script standing in for signal-cli.
#[cfg(unix)]
mod spawned {
    use super::*;
    use signal::Daemon;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    fn script(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("signal-cli");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn free_port() -> u16 {
        let (l, port) = support::bind();
        drop(l);
        port
    }

    fn spawned(program: PathBuf, port: u16, log: &Path) -> Running {
        start(
            SignalChannel::new(SignalConfig {
                account: ACCOUNT.into(),
                url: format!("http://127.0.0.1:{port}"),
                daemon: Some(Daemon {
                    program,
                    port,
                    log: Some(log.to_path_buf()),
                }),
                inbox: None,
            })
            .with_allowed(vec![MAX.into()], vec![]),
        )
    }

    #[tokio::test]
    async fn it_is_started_with_its_arguments_and_restarted_when_it_exits() {
        let dir = tempfile::tempdir().unwrap();
        let args = dir.path().join("args");
        let prog = script(
            dir.path(),
            &format!(
                "echo \"$@\" >> '{}'\necho 'java says hi' >&2\nexit 3",
                args.display()
            ),
        );
        let log = dir.path().join("gw").join("daemon.log");
        let port = free_port();
        let r = spawned(prog, port, &log);
        until("three runs", T, || {
            std::fs::read_to_string(&args).is_ok_and(|a| a.lines().count() >= 3)
        })
        .await;
        let a = std::fs::read_to_string(&args).unwrap();
        assert_eq!(
            a.lines().next().unwrap(),
            format!("-a {ACCOUNT} daemon --http 127.0.0.1:{port} --receive-mode on-connection --no-receive-stdout")
        );
        until("the problem", T, || {
            r.ch.problem().is_some_and(|p| p.contains("exited"))
        })
        .await;
        let p = r.ch.problem().unwrap();
        assert!(
            p.contains("times in a row") && p.contains("daemon.log"),
            "{p}"
        );
        let l = std::fs::read_to_string(&log).unwrap();
        assert!(
            l.contains("ferrule starts") && l.contains("java says hi"),
            "{l}"
        );
        r.run.abort();
    }

    #[tokio::test]
    async fn a_missing_program_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let r = spawned(
            dir.path().join("no-such-signal-cli"),
            free_port(),
            &dir.path().join("daemon.log"),
        );
        until("the problem", T, || r.ch.problem().is_some()).await;
        let p = r.ch.problem().unwrap();
        assert!(
            p.contains("couldn't start")
                && p.contains("no-such-signal-cli")
                && p.contains("signal_cli"),
            "{p}"
        );
        r.run.abort();
    }

    #[tokio::test]
    async fn a_daemon_already_answering_is_used_not_started_again() {
        let d = mock::start();
        let dir = tempfile::tempdir().unwrap();
        let ran = dir.path().join("ran");
        let prog = script(dir.path(), &format!("touch '{}'", ran.display()));
        let mut r = spawned(prog, d.port, &dir.path().join("daemon.log"));
        listening(&d).await;
        d.push(mock::dm(MAX, "adopted", 1_760_000_000_090));
        assert_eq!(recv(&mut r).await.text, "adopted");
        assert!(!ran.exists(), "no second daemon");
        assert!(r.ch.problem().is_none());
        r.run.abort();
    }
}

/// Against a real signal-cli daemon: `FERRULE_LIVE_SIGNAL_URL`
/// (`http://127.0.0.1:7583`), `_ACCOUNT` and `_TO` (a number to message).
#[tokio::test]
#[ignore]
async fn signal_live_round_trip() {
    let (Ok(url), Ok(account), Ok(to)) = (
        std::env::var("FERRULE_LIVE_SIGNAL_URL"),
        std::env::var("FERRULE_LIVE_SIGNAL_ACCOUNT"),
        std::env::var("FERRULE_LIVE_SIGNAL_TO"),
    ) else {
        eprintln!("set FERRULE_LIVE_SIGNAL_URL, _ACCOUNT and _TO");
        return;
    };
    let p = signal::probe(&url, &account).await.unwrap();
    eprintln!("probe: {}", p.summary());
    let ch = SignalChannel::new(SignalConfig {
        account,
        url,
        daemon: None,
        inbox: None,
    });
    let id = ch
        .post(out(&to, "ferrule live test: **hello** from `cargo test`"))
        .await
        .unwrap()
        .unwrap();
    ch.react(&to, &id, "👀").await.unwrap();
}
