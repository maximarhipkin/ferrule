//! M39 §4: the Matrix adapter against a mock homeserver — the catch-up
//! sync skipped, DMs by user id, rooms by mention or reply, strangers and
//! unlisted rooms ignored, encrypted rooms refused with one notice,
//! invites, approvals by reaction, files both ways (and the older media
//! endpoint), edits and the 👀, rate limits, a password session kept and
//! renewed, a revoked token reported, pairing, and the probe.

use crate::support;

use ferrule_gateway::channel::{Button, ButtonAction};
use ferrule_gateway::channels::files::Inbox;
use ferrule_gateway::channels::matrix::{self, Login, MatrixChannel, MatrixConfig};
use ferrule_gateway::{Attachment, Channel, GatewayError, InboundMessage, OutboundMessage};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use support::matrix::{
    self as mock, text, Homeserver, BOT, DM, MAX, PASSWORD, ROOM, STRANGER, TOKEN, USER,
};
use support::until;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const T: Duration = Duration::from_secs(10);

struct Running {
    ch: Arc<MatrixChannel>,
    rx: mpsc::Receiver<InboundMessage>,
    run: JoinHandle<Result<(), GatewayError>>,
}

fn config(hs: &Homeserver, state: Option<&Path>) -> MatrixConfig {
    MatrixConfig {
        homeserver: hs.url.clone(),
        login: Login::Token(TOKEN.into()),
        state_dir: state.map(Path::to_path_buf),
        inbox: None,
    }
}

fn start(ch: MatrixChannel) -> Running {
    let ch = Arc::new(ch.with_fast_retries());
    let (tx, rx) = mpsc::channel(32);
    let c = ch.clone();
    let run = tokio::spawn(async move { c.run(tx).await });
    Running { ch, rx, run }
}

fn spawn(cfg: MatrixConfig) -> Running {
    start(MatrixChannel::new(cfg).with_allowed(vec![MAX.into()], vec![ROOM.into()]))
}

async fn recv(r: &mut Running) -> InboundMessage {
    tokio::time::timeout(T, r.rx.recv())
        .await
        .expect("a message in time")
        .expect("the channel open")
}

/// Waits until the catch-up sync is done and a long-poll has begun.
async fn synced(hs: &Homeserver) {
    until("the first long-poll", T, || {
        hs.state().sinces.iter().any(Option::is_some)
    })
    .await;
}

fn out(chat: &str, text: &str) -> OutboundMessage {
    OutboundMessage {
        channel: "matrix".into(),
        chat_id: chat.into(),
        text: text.into(),
        reply_to: None,
        attachments: vec![],
    }
}

#[tokio::test]
async fn a_dm_arrives_by_user_id_and_the_backlog_is_skipped() {
    let hs = mock::start();
    hs.state().backlog = Some(json!({
        "rooms": { "join": { DM: { "timeline": { "events": [text("$old", MAX, "said last week")] } } } }
    }));
    let dir = tempfile::tempdir().unwrap();
    let mut r = spawn(config(&hs, Some(dir.path())));
    synced(&hs).await;
    hs.timeline(DM, vec![text("$1", MAX, "hello **there**")]);
    let m = recv(&mut r).await;
    assert_eq!(m.chat_id, MAX);
    assert_eq!(m.sender_id.as_deref(), Some(MAX));
    assert_eq!(m.message_id, "$1");
    assert_eq!(m.text, "hello **there**");
    assert_eq!(m.ts, 1_760_000_000);

    // The answer goes to the DM room, as Markdown with HTML beside it,
    // split at 16 000 characters.
    let mut reply = out(MAX, &format!("**done**\n{}", "x".repeat(17_000)));
    reply.reply_to = Some("$1".into());
    r.ch.send(reply).await.unwrap();
    let sent = hs.sent_to(DM, "m.room.message");
    assert_eq!(sent.len(), 2);
    assert!(sent[0]["formatted_body"]
        .as_str()
        .unwrap()
        .starts_with("<strong>done</strong>"));
    assert_eq!(sent[0]["format"], "org.matrix.custom.html");
    assert_eq!(sent[0]["m.relates_to"]["m.in_reply_to"]["event_id"], "$1");
    assert!(sent[1].get("m.relates_to").is_none());
    let txns = hs.state().txns.clone();
    assert_ne!(txns[0], txns[1]);

    // The sync position and the DM room are kept for the next start.
    r.run.abort();
    let state = std::fs::read_to_string(dir.path().join("state.json")).unwrap();
    assert!(state.contains("batch") && state.contains(DM), "{state}");
}

/// `ferrule tasks run-now` beside the gateway: its new DM is kept, and
/// it never moves the gateway's sync position back.
#[tokio::test]
async fn a_second_process_shares_the_state_file_without_rewinding_it() {
    let hs = mock::start();
    let dir = tempfile::tempdir().unwrap();
    let mut r = spawn(config(&hs, Some(dir.path())));
    synced(&hs).await;
    let batch = |d: &Path| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(d.join("state.json")).unwrap()).unwrap()
    };
    let before = batch(dir.path())["next_batch"].clone();
    assert!(before.is_string(), "{before}");

    let once = MatrixChannel::new(config(&hs, Some(dir.path())));
    once.send(out("@new:mock.org", "your report"))
        .await
        .unwrap();
    let s = batch(dir.path());
    assert_eq!(s["next_batch"], before);
    let room = s["dms"]["@new:mock.org"].as_str().unwrap().to_string();

    // The gateway's next save keeps that DM, and its send uses it.
    hs.timeline(DM, vec![text("$1", MAX, "hi")]);
    recv(&mut r).await;
    until("the new position saved", T, || {
        batch(dir.path())["next_batch"] != before
    })
    .await;
    assert_eq!(batch(dir.path())["dms"]["@new:mock.org"], room.as_str());
    r.ch.send(out("@new:mock.org", "again")).await.unwrap();
    assert_eq!(hs.sent_to(&room, "m.room.message").len(), 2);
    assert_eq!(hs.state().created.len(), 1);
}

#[tokio::test]
async fn rooms_need_a_mention_and_strangers_are_ignored() {
    let hs = mock::start();
    let mut r = spawn(config(&hs, None));
    synced(&hs).await;
    // A stranger's DM, an unlisted room, and a room message not for us.
    hs.state()
        .members
        .insert("!eve:mock.org".into(), vec![BOT.into(), STRANGER.into()]);
    hs.timeline("!eve:mock.org", vec![text("$s1", STRANGER, "hi bot")]);
    hs.state().members.insert(
        "!other:mock.org".into(),
        vec![BOT.into(), MAX.into(), STRANGER.into()],
    );
    hs.timeline("!other:mock.org", vec![text("$s2", MAX, "Ferrule: hi")]);
    hs.timeline(ROOM, vec![text("$s3", MAX, "just chatting")]);
    // By display name, from anyone in the room.
    hs.timeline(
        ROOM,
        vec![text("$m1", STRANGER, "Ferrule: what's the time?")],
    );
    let m = recv(&mut r).await;
    assert_eq!(m.message_id, "$m1");
    assert_eq!(m.chat_id, ROOM);
    assert_eq!(m.text, "what's the time?");

    // By `m.mentions`, with the pill in the body.
    let mut ev = text("$m2", MAX, "@ferrule:mock.org: status?");
    ev["content"]["m.mentions"] = json!({ "user_ids": [BOT] });
    hs.timeline(ROOM, vec![ev]);
    assert_eq!(recv(&mut r).await.text, "status?");

    // A reply to one of ours, without its quoted fallback.
    let id = r.ch.post(out(ROOM, "the answer")).await.unwrap().unwrap();
    let mut ev = text("$m3", MAX, "> <@ferrule:mock.org> the answer\n\nand then?");
    ev["content"]["m.relates_to"] = json!({ "m.in_reply_to": { "event_id": id } });
    hs.timeline(ROOM, vec![ev]);
    let m = recv(&mut r).await;
    assert_eq!(m.text, "and then?");
    assert_eq!(m.reply_to.as_deref(), Some(id.as_str()));

    // Our own events, notices and edits are never read.
    let mut notice = text("$n", MAX, "Ferrule: a notice");
    notice["content"]["msgtype"] = json!("m.notice");
    hs.timeline(
        ROOM,
        vec![
            notice,
            text("$own", BOT, "Ferrule: me"),
            text("$m4", MAX, "ferrule, last"),
        ],
    );
    assert_eq!(recv(&mut r).await.message_id, "$m4");
    // Nobody was answered along the way.
    assert!(hs.bodies("!eve:mock.org").is_empty());
    assert!(hs.bodies("!other:mock.org").is_empty());
    r.run.abort();
}

#[tokio::test]
async fn an_encrypted_room_is_refused_with_one_notice() {
    let hs = mock::start();
    hs.state().encrypted.insert(DM.into());
    let dir = tempfile::tempdir().unwrap();
    let mut r = spawn(config(&hs, Some(dir.path())));
    synced(&hs).await;
    let enc = |id: &str| {
        json!({ "type": "m.room.encrypted", "event_id": id, "sender": MAX,
                "content": { "algorithm": "m.megolm.v1.aes-sha2", "ciphertext": "…" } })
    };
    hs.timeline(DM, vec![enc("$e1"), enc("$e2")]);
    hs.timeline(ROOM, vec![text("$ok", MAX, "Ferrule: fine here")]);
    assert_eq!(recv(&mut r).await.message_id, "$ok");
    let notices = hs.bodies(DM);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("end-to-end encrypted"));
    assert_eq!(hs.sent_to(DM, "m.room.message")[0]["msgtype"], "m.notice");

    // Nothing is written there, and the refusal says why.
    let e = r.ch.send(out(DM, "hello")).await.unwrap_err();
    assert!(e.to_string().contains("encrypted"), "{e}");
    assert_eq!(r.ch.encrypted_rooms(), vec![DM.to_string()]);
    assert_eq!(matrix::encrypted_on_disk(dir.path()), vec![DM.to_string()]);
    r.run.abort();
}

#[tokio::test]
async fn invites_are_joined_only_from_allowed_users() {
    let hs = mock::start();
    let mut r = spawn(config(&hs, None));
    synced(&hs).await;
    hs.invite("!spam:mock.org", STRANGER, true, false);
    hs.invite("!secret:mock.org", MAX, false, true);
    hs.invite("!newdm:mock.org", MAX, true, false);
    until("the joins", T, || hs.state().joins.len() == 2).await;
    assert_eq!(
        hs.state().joins,
        vec![
            "!secret:mock.org".to_string(),
            "!newdm:mock.org".to_string()
        ]
    );
    // The encrypted room is told why; the DM is Max's from now on.
    until("the notice", T, || {
        !hs.bodies("!secret:mock.org").is_empty()
    })
    .await;
    hs.timeline("!newdm:mock.org", vec![text("$d", MAX, "hi")]);
    assert_eq!(recv(&mut r).await.chat_id, MAX);
    r.ch.send(out(MAX, "hello")).await.unwrap();
    assert_eq!(hs.bodies("!newdm:mock.org"), vec!["hello".to_string()]);
    r.run.abort();
}

#[tokio::test]
async fn approvals_are_reactions_and_come_back_as_their_command() {
    let hs = mock::start();
    let mut r = spawn(config(&hs, None));
    synced(&hs).await;
    // Nothing known about Max yet: a direct room is made for him.
    let buttons = [
        Button {
            text: "Allow".into(),
            action: ButtonAction::Command("yes a1b2".into()),
        },
        Button {
            text: "Refuse".into(),
            action: ButtonAction::Command("no a1b2".into()),
        },
    ];
    r.ch.send_buttons(out(MAX, "Run `rm -rf build`?"), &buttons)
        .await
        .unwrap();
    let created = hs.state().created.clone();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0]["is_direct"], true);
    assert_eq!(created[0]["invite"], json!([MAX]));
    let room = "!new1:mock.org";
    let body = &hs.bodies(room)[0];
    assert!(
        body.contains("👍 Allow: react 👍, or send `yes a1b2`"),
        "{body}"
    );
    assert!(body.contains("👎 Refuse"), "{body}");
    let reactions = hs.sent_to(room, "m.reaction");
    let keys: Vec<&str> = reactions
        .iter()
        .map(|c| c["m.relates_to"]["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["👍", "👎"]);
    let target = reactions[0]["m.relates_to"]["event_id"]
        .as_str()
        .unwrap()
        .to_string();

    let reaction = |id: &str, who: &str, key: &str| {
        json!({ "type": "m.reaction", "event_id": id, "sender": who,
                "content": { "m.relates_to": { "rel_type": "m.annotation", "event_id": target, "key": key } } })
    };
    // A stranger's tap does nothing; Max's (with a variation selector)
    // is the answer, once.
    hs.timeline(
        room,
        vec![
            reaction("$r0", STRANGER, "👍"),
            reaction("$r1", MAX, "👍\u{fe0f}"),
            reaction("$r2", MAX, "👎"),
        ],
    );
    hs.timeline(room, vec![text("$after", MAX, "next")]);
    let m = recv(&mut r).await;
    assert_eq!(m.text, "yes a1b2");
    assert_eq!(m.chat_id, MAX);
    assert_eq!(m.reply_to.as_deref(), Some(target.as_str()));
    assert_eq!(recv(&mut r).await.message_id, "$after");
    r.run.abort();
}

#[tokio::test]
async fn files_come_in_to_the_inbox_and_go_out_as_uploads() {
    let hs = mock::start();
    hs.state().v1_missing = true;
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = config(&hs, None);
    cfg.inbox = Some(Inbox::new(ws.path(), 1));
    let mut r = spawn(cfg);
    synced(&hs).await;
    let image = |id: &str, url: &str, size: u64| {
        json!({ "type": "m.room.message", "event_id": id, "sender": MAX, "origin_server_ts": 0,
                "content": { "msgtype": "m.image", "body": "what is this", "filename": "shot.png",
                             "url": url, "info": { "mimetype": "image/png", "size": size } } })
    };
    hs.timeline(DM, vec![image("$f1", "mxc://mock.org/img1", 8)]);
    let m = recv(&mut r).await;
    assert!(m.text.starts_with("what is this"), "{}", m.text);
    assert!(m.text.contains("inbox/matrix/"), "{}", m.text);
    assert_eq!(std::fs::read(&m.attachments[0].url).unwrap(), b"PNGBYTES");
    // The authenticated endpoint was missing: the older one served it.
    let downloads = hs.state().downloads.clone();
    assert!(downloads[0].starts_with("/_matrix/media/v3/download/mock.org/img1"));

    // Over the cap: refused before downloading, and the sender is told.
    hs.timeline(DM, vec![image("$f2", "mxc://mock.org/img1", 5 << 20)]);
    let m = recv(&mut r).await;
    assert!(m.text.contains("wasn't saved"), "{}", m.text);
    assert!(hs
        .bodies(DM)
        .last()
        .unwrap()
        .starts_with("I couldn't take shot.png"));
    assert_eq!(hs.state().downloads.len(), 1);

    // Out: uploaded, then posted as an image, then the text.
    let file = ws.path().join("chart.png");
    std::fs::write(&file, b"\x89PNG fake").unwrap();
    let mut msg = out(MAX, "the chart");
    msg.attachments = vec![Attachment {
        kind: "image/png".into(),
        url: file.to_string_lossy().into_owned(),
        name: Some("chart.png".into()),
    }];
    r.ch.send(msg).await.unwrap();
    let (name, mime, bytes) = hs.state().uploads[0].clone();
    assert_eq!((name.as_str(), mime.as_str()), ("chart.png", "image/png"));
    assert_eq!(bytes, b"\x89PNG fake");
    let sent = hs.sent_to(DM, "m.room.message");
    let img = &sent[sent.len() - 2];
    assert_eq!(img["msgtype"], "m.image");
    assert_eq!(img["url"], "mxc://mock.org/up1");
    assert_eq!(img["info"]["size"], 9);
    assert_eq!(sent.last().unwrap()["body"], "the chart");
    r.run.abort();
}

#[tokio::test]
async fn edits_replace_and_the_eyes_are_a_receipt_and_a_reaction() {
    let hs = mock::start();
    let ch = MatrixChannel::new(config(&hs, None)).with_fast_retries();
    let id = ch.post(out(ROOM, "draft")).await.unwrap().unwrap();
    ch.edit(ROOM, &id, "final *answer*").await.unwrap();
    let edit = hs.sent_to(ROOM, "m.room.message")[1].clone();
    assert_eq!(edit["body"], "* final *answer*");
    assert_eq!(edit["m.relates_to"]["rel_type"], "m.replace");
    assert_eq!(edit["m.relates_to"]["event_id"], id);
    assert_eq!(edit["m.new_content"]["body"], "final *answer*");
    assert_eq!(
        edit["m.new_content"]["formatted_body"],
        "final <em>answer</em>"
    );

    ch.react(ROOM, "$in1", "👀").await.unwrap();
    assert_eq!(
        hs.state().receipts,
        vec![(ROOM.to_string(), "$in1".to_string())]
    );
    let r = &hs.sent_to(ROOM, "m.reaction")[0];
    assert_eq!(r["m.relates_to"]["key"], "👀");

    // An alias is looked up.
    ch.send(out("#general:mock.org", "via the alias"))
        .await
        .unwrap();
    assert_eq!(hs.bodies(ROOM).last().unwrap(), "via the alias");
    assert_eq!(ch.stream_every(), Some(Duration::from_secs(3)));
    assert_eq!(ch.message_limit(), Some(matrix::MESSAGE_LIMIT));
}

#[tokio::test]
async fn rate_limits_are_waited_out_then_reported() {
    let hs = mock::start();
    let ch = MatrixChannel::new(config(&hs, None)).with_fast_retries();
    hs.state()
        .fail_next
        .push_back((429, "M_LIMIT_EXCEEDED".into(), 10));
    ch.send(out(ROOM, "after a wait")).await.unwrap();
    assert_eq!(hs.bodies(ROOM), vec!["after a wait".to_string()]);
    for _ in 0..4 {
        hs.state()
            .fail_next
            .push_back((429, "M_LIMIT_EXCEEDED".into(), 10));
    }
    let e = ch.send(out(ROOM, "never")).await.unwrap_err();
    assert!(matches!(e, GatewayError::RateLimited { .. }), "{e}");
    hs.state()
        .fail_next
        .push_back((403, "M_FORBIDDEN".into(), 0));
    let e = ch.send(out(ROOM, "no")).await.unwrap_err();
    assert!(e.to_string().contains("allowed to post"), "{e}");
}

#[tokio::test]
async fn a_password_session_is_kept_and_renewed_but_a_revoked_token_is_a_problem() {
    let hs = mock::start();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(&hs, Some(dir.path()));
    cfg.login = Login::Password {
        user: USER.into(),
        password: PASSWORD.into(),
    };
    let ch = MatrixChannel::new(cfg.clone()).with_fast_retries();
    ch.send(out(ROOM, "one")).await.unwrap();
    assert_eq!(hs.state().logins, 1);
    let session = dir.path().join("session.json");
    let kept = std::fs::read_to_string(&session).unwrap();
    assert!(kept.contains("syt_login_1") && !kept.contains(PASSWORD));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&session).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // A restart reuses the session.
    let ch = MatrixChannel::new(cfg.clone()).with_fast_retries();
    ch.send(out(ROOM, "two")).await.unwrap();
    assert_eq!(hs.state().logins, 1);
    // The server forgets it: logged in again, once, and the send goes.
    hs.revoke("syt_login_1");
    ch.send(out(ROOM, "three")).await.unwrap();
    assert_eq!(hs.state().logins, 2);
    assert_eq!(hs.bodies(ROOM), ["one", "two", "three"]);
    // A wrong password is said plainly.
    cfg.login = Login::Password {
        user: USER.into(),
        password: "wrong".into(),
    };
    cfg.state_dir = None;
    let ch = MatrixChannel::new(cfg).with_fast_retries();
    assert!(ch.send(out(ROOM, "x")).await.is_err());
    assert!(ch
        .problem()
        .unwrap()
        .contains("refused the user or password"));

    // A token login whose token is revoked: a problem naming setup, and
    // the token never shown.
    let r = spawn(config(&hs, None));
    synced(&hs).await;
    hs.revoke(TOKEN);
    until("the problem", T, || r.ch.problem().is_some()).await;
    let p = r.ch.problem().unwrap();
    assert!(p.contains("ferrule setup") && !p.contains(TOKEN), "{p}");
    r.run.abort();
}

#[tokio::test]
async fn setup_pairs_by_code_after_joining_the_invite() {
    let hs = mock::start();
    let mut r = start(MatrixChannel::new(config(&hs, None)).with_pairing("pair-4821"));
    synced(&hs).await;
    hs.invite("!pair:mock.org", STRANGER, true, false);
    until("the join", T, || !hs.state().joins.is_empty()).await;
    hs.state()
        .members
        .get_mut("!pair:mock.org")
        .unwrap()
        .dedup();
    hs.timeline("!pair:mock.org", vec![text("$p", STRANGER, "pair-4821")]);
    until("the pairing", T, || r.ch.paired().is_some()).await;
    assert_eq!(r.ch.paired().unwrap().0, STRANGER);
    until("the answer", T, || !hs.bodies("!pair:mock.org").is_empty()).await;
    assert!(hs.bodies("!pair:mock.org")[0].starts_with("Paired."));
    // From now on they are let in.
    hs.timeline("!pair:mock.org", vec![text("$q", STRANGER, "hello")]);
    assert_eq!(recv(&mut r).await.chat_id, STRANGER);
    r.run.abort();
}

#[tokio::test]
async fn the_probe_reads_the_account_and_says_what_is_wrong() {
    let hs = mock::start();
    hs.state().encrypted.insert(DM.into());
    let p = matrix::probe(config(&hs, None)).await.unwrap();
    assert_eq!(p.user_id, BOT);
    assert_eq!(p.rooms, 2);
    assert_eq!(p.encrypted, vec![DM.to_string()]);
    assert_eq!(
        p.summary(),
        "@ferrule:mock.org (Ferrule) · 2 rooms, 1 encrypted (refused)"
    );
    assert!(hs.state().sent.is_empty());

    // A password probe logs in and out again.
    let mut cfg = config(&hs, None);
    cfg.login = Login::Password {
        user: USER.into(),
        password: PASSWORD.into(),
    };
    matrix::probe(cfg).await.unwrap();
    let counts = {
        let s = hs.state();
        (s.logins, s.logouts)
    };
    assert_eq!(counts, (1, 1));

    let mut cfg = config(&hs, None);
    cfg.login = Login::Token("syt_nope".into());
    let e = matrix::probe(cfg).await.unwrap_err();
    assert!(
        e.contains("ferrule setup") && !e.contains("syt_nope"),
        "{e}"
    );
    let mut cfg = config(&hs, None);
    cfg.homeserver = format!("{}/not-matrix", hs.url);
    let e = matrix::probe(cfg).await.unwrap_err();
    assert!(e.contains("doesn't answer as a Matrix homeserver"), "{e}");
    let mut cfg = config(&hs, None);
    cfg.homeserver = "matrix.org".into();
    assert!(matrix::probe(cfg).await.unwrap_err().contains("a URL"));

    // Discovery follows `.well-known`; setup's login gives a token.
    assert_eq!(matrix::discover(&hs.url).await, hs.url);
    let (user, token) = matrix::login_for_token(&hs.url, USER, PASSWORD)
        .await
        .unwrap();
    assert_eq!(user, BOT);
    assert!(token.starts_with("syt_login_"));
    let e = matrix::login_for_token(&hs.url, USER, "nope")
        .await
        .unwrap_err();
    assert!(e.contains("refused the user or password"), "{e}");
}

/// A live round trip against a real homeserver: `FERRULE_LIVE_MATRIX_URL`,
/// `…_TOKEN`, and `…_TO` (a user id or an unencrypted room id).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn matrix_live_round_trip() {
    let (Ok(url), Ok(token), Ok(to)) = (
        std::env::var("FERRULE_LIVE_MATRIX_URL"),
        std::env::var("FERRULE_LIVE_MATRIX_TOKEN"),
        std::env::var("FERRULE_LIVE_MATRIX_TO"),
    ) else {
        eprintln!("set FERRULE_LIVE_MATRIX_URL, _TOKEN and _TO");
        return;
    };
    let cfg = MatrixConfig {
        homeserver: url,
        login: Login::Token(token),
        state_dir: None,
        inbox: None,
    };
    let p = matrix::probe(cfg.clone()).await.unwrap();
    eprintln!("probe: {}", p.summary());
    let ch = MatrixChannel::new(cfg);
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
