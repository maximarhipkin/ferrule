//! M39 §5: the email adapter against a mock IMAP and SMTP server — only
//! new mail, the allowlist, loop and list guards, Authentication-Results,
//! approvals by the first line (and refused unvouched), replies threaded,
//! new threads, files both ways, the hourly budget, IDLE and polling, a
//! renumbered mailbox never replayed, a refused login, an oversized mail,
//! and the probe.

mod support;

use ferrule_connections::native::mime;
use ferrule_gateway::channels::email::{self, EmailChannel, EmailConfig, Server};
use ferrule_gateway::channels::files::Inbox;
use ferrule_gateway::{Attachment, Channel, GatewayError, InboundMessage, OutboundMessage};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use support::email::{mail, vouched, Provider, ADDRESS, MAX, PASSWORD};
use support::until;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const T: Duration = Duration::from_secs(10);

struct Running {
    ch: Arc<EmailChannel>,
    rx: mpsc::Receiver<InboundMessage>,
    run: JoinHandle<Result<(), GatewayError>>,
}

fn config(p: &Provider, state: Option<&Path>) -> EmailConfig {
    EmailConfig {
        address: format!("Ferrule <{ADDRESS}>"),
        username: ADDRESS.into(),
        password: PASSWORD.into(),
        imap: Server::new("127.0.0.1", p.imap_port),
        smtp: Server::new("127.0.0.1", p.smtp_port),
        require_auth: false,
        poll: Duration::from_millis(100),
        state_dir: state.map(Path::to_path_buf),
        inbox: None,
        instance: "default".into(),
    }
}

fn start(cfg: EmailConfig) -> Running {
    let ch = Arc::new(
        EmailChannel::new(cfg)
            .with_allowed(vec![MAX.into(), "@team.org".into()])
            .with_fast_retries(),
    );
    let (tx, rx) = mpsc::channel(32);
    let c = ch.clone();
    let run = tokio::spawn(async move { c.run(tx).await });
    Running { ch, rx, run }
}

async fn recv(r: &mut Running) -> InboundMessage {
    tokio::time::timeout(T, r.rx.recv())
        .await
        .expect("a message in time")
        .expect("the channel open")
}

async fn nothing(r: &mut Running) {
    let got = tokio::time::timeout(Duration::from_millis(400), r.rx.recv()).await;
    assert!(
        got.is_err(),
        "nothing should arrive: {:?}",
        got.ok().flatten().map(|m| m.text)
    );
}

/// Waits until the loop is watching (it has looked at least once).
async fn watching(r: &Running) {
    until("the first look", T, || r.ch.last_ok_poll().is_some()).await;
}

fn out(to: &str, text: &str) -> OutboundMessage {
    OutboundMessage {
        channel: "email".into(),
        chat_id: to.into(),
        text: text.into(),
        reply_to: None,
        attachments: vec![],
    }
}

fn header(data: &str, name: &str) -> Option<String> {
    let (h, _) = mime::headers(data.as_bytes());
    h.into_iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| mime::decode_words(&v))
}

#[tokio::test]
async fn new_mail_from_an_allowed_sender_arrives_and_nothing_older() {
    let p = Provider::start(true);
    let old = p.deliver(mail(MAX, "old", "from before ferrule", &[]));
    let mut r = start(config(&p, None));
    watching(&r).await;
    let uid = p.deliver(mail(
        &format!("Max A <{MAX}>"),
        "Plans",
        "Can you check the logs?\n\nOn Mon, 28 Sep 2026, bot wrote:\n> old stuff",
        &[],
    ));
    let m = recv(&mut r).await;
    assert_eq!(m.channel, "email");
    assert_eq!(m.chat_id, MAX);
    assert_eq!(m.sender, "Max A");
    assert_eq!(m.sender_id.as_deref(), Some(MAX));
    assert_eq!(m.text, "Subject: Plans\n\nCan you check the logs?");
    until("marked read", T, || p.seen(uid)).await;
    assert!(!p.seen(old), "mail from before start is left alone");
    // A domain entry lets a colleague in; a stranger is dropped unread.
    let eve = p.deliver(mail("eve@evil.com", "hi", "let me in", &[]));
    p.deliver(mail("dana@team.org", "Re: hi", "domain ok", &[]));
    let m = recv(&mut r).await;
    assert_eq!(
        (m.chat_id.as_str(), m.text.as_str()),
        ("dana@team.org", "domain ok")
    );
    assert!(!p.seen(eve));
    assert_eq!(p.sent(), 0, "strangers get no answer");
    assert!(p.state().idles > 0);
    r.run.abort();
}

#[tokio::test]
async fn lists_bounces_and_autoreplies_are_never_answered() {
    let p = Provider::start(true);
    let mut r = start(config(&p, None));
    watching(&r).await;
    let skipped = [
        p.deliver(mail(
            MAX,
            "digest",
            "x",
            &[("List-Id", "<dev.lists.example.com>")],
        )),
        p.deliver(mail(
            MAX,
            "away",
            "x",
            &[("Auto-Submitted", "auto-replied")],
        )),
        p.deliver(mail(MAX, "bulk", "x", &[("Precedence", "bulk")])),
        p.deliver(mail(MAX, "bounce", "x", &[("Return-Path", "<>")])),
        p.deliver(mail("MAILER-DAEMON@example.com", "Undelivered", "x", &[])),
        p.deliver(mail(MAX, "loop", "x", &[("X-Ferrule-Loop", "other")])),
    ];
    p.deliver(mail(MAX, "Re: real", "a person", &[]));
    let m = recv(&mut r).await;
    assert_eq!(m.text, "a person");
    for uid in skipped {
        assert!(!p.seen(uid), "{uid} left unread");
    }
    assert_eq!(p.sent(), 0);
    r.run.abort();
}

#[tokio::test]
async fn require_auth_drops_mail_the_server_didnt_vouch_for() {
    let p = Provider::start(true);
    let mut cfg = config(&p, None);
    cfg.require_auth = true;
    let mut r = start(cfg);
    watching(&r).await;
    let forged = p.deliver(mail(MAX, "Re: x", "forged", &[]));
    let wrong = p.deliver(mail(
        MAX,
        "Re: x",
        "other domain",
        &[("Authentication-Results", &vouched("evil.com"))],
    ));
    p.deliver(mail(
        MAX,
        "Re: x",
        "genuine",
        &[("Authentication-Results", &vouched("example.com"))],
    ));
    assert_eq!(recv(&mut r).await.text, "genuine");
    assert!(!p.seen(forged) && !p.seen(wrong));
    r.run.abort();
}

#[tokio::test]
async fn an_approval_is_its_first_line_and_needs_a_vouched_sender() {
    let p = Provider::start(true);
    let mut r = start(config(&p, None));
    watching(&r).await;
    p.deliver(mail(
        MAX,
        "Re: ferrule: May I run it?",
        "yes a1\n\nSent from my phone\n\nOn Mon, 28 Sep 2026 at 10:00, Ferrule <bot@mock.test> wrote:\n> May I run it?",
        &[("Authentication-Results", &vouched("example.com"))],
    ));
    assert_eq!(recv(&mut r).await.text, "yes a1");
    // Unvouched: refused, with a reply saying why; the agent sees nothing.
    let uid = p.deliver(mail(MAX, "Re: ferrule: May I?", "yes a2", &[]));
    until("the refusal", T, || p.sent() == 1).await;
    nothing(&mut r).await;
    assert!(p.seen(uid));
    let data = p.sent_data(0);
    assert!(data.contains("Auto-Submitted: auto-replied"), "{data}");
    let text = mime::parse(data.as_bytes()).text;
    assert!(text.contains("can't take an approval by email"), "{text}");
    r.run.abort();
}

#[tokio::test]
async fn replies_thread_under_the_last_mail_and_notices_start_a_thread() {
    let p = Provider::start(true);
    let mut r = start(config(&p, None));
    watching(&r).await;
    p.deliver(mail(
        MAX,
        "Re: Re: Quarterly",
        "numbers?",
        &[
            ("Message-ID", "<m2@example.com>"),
            ("References", "<m0@example.com> <m1@example.com>"),
        ],
    ));
    let m = recv(&mut r).await;
    assert_eq!(m.message_id, "<m2@example.com>");
    r.ch.send(out(MAX, "Here they are.")).await.unwrap();
    let data = p.sent_data(0);
    assert_eq!(header(&data, "Subject").unwrap(), "Re: Quarterly");
    assert_eq!(header(&data, "In-Reply-To").unwrap(), "<m2@example.com>");
    assert_eq!(
        header(&data, "References").unwrap(),
        "<m0@example.com> <m1@example.com> <m2@example.com>"
    );
    assert_eq!(header(&data, "X-Ferrule-Loop").unwrap(), "default");
    assert!(header(&data, "Message-ID")
        .unwrap()
        .contains(".ferrule@mock.test>"));
    assert_eq!(
        header(&data, "From").unwrap(),
        format!("Ferrule <{ADDRESS}>")
    );
    {
        let s = p.state();
        assert_eq!(s.sent[0].from, ADDRESS);
        assert_eq!(s.sent[0].to, vec![MAX.to_string()]);
    }
    assert_eq!(mime::parse(data.as_bytes()).text, "Here they are.");
    // Someone ferrule never heard from: a new thread, marked generated.
    r.ch.send(out("dana@team.org", "# Nightly report\nAll green."))
        .await
        .unwrap();
    let data = p.sent_data(1);
    assert_eq!(header(&data, "Subject").unwrap(), "ferrule: Nightly report");
    assert_eq!(header(&data, "Auto-Submitted").unwrap(), "auto-generated");
    assert!(header(&data, "In-Reply-To").is_none());
    // Mail can't be sent to something that isn't an address.
    assert!(r
        .ch
        .send(out("max@example.com\r\nBcc: eve@evil.com", "x"))
        .await
        .is_err());
    r.run.abort();
}

#[tokio::test]
async fn files_arrive_in_the_inbox_and_go_out_attached() {
    let p = Provider::start(true);
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = config(&p, None);
    cfg.inbox = Some(Inbox::new(ws.path(), 1));
    let mut r = start(cfg);
    watching(&r).await;
    let raw = format!(
        "From: {MAX}\r\nTo: {ADDRESS}\r\nSubject: Re: report\r\nMessage-ID: <f1@example.com>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b1\"\r\n\r\n--b1\r\nContent-Type: text/plain\r\n\r\nsee attached\r\n--b1\r\nContent-Type: text/csv; name=\"q3.csv\"\r\nContent-Disposition: attachment; filename=\"q3.csv\"\r\nContent-Transfer-Encoding: base64\r\n\r\nYSxiCjEsMgo=\r\n--b1--\r\n"
    );
    p.deliver(raw);
    let m = recv(&mut r).await;
    assert!(
        m.text.starts_with("see attached\n\n[The sender attached"),
        "{}",
        m.text
    );
    assert_eq!(m.attachments.len(), 1);
    let saved = &m.attachments[0];
    assert_eq!(std::fs::read(&saved.url).unwrap(), b"a,b\n1,2\n");
    assert!(saved.name.as_deref().unwrap().starts_with("inbox/email/"));

    let file = ws.path().join("chart.png");
    std::fs::write(&file, [0x89, b'P', b'N', b'G', 0, 1, 2]).unwrap();
    let mut msg = out(MAX, "The chart.");
    msg.attachments.push(Attachment {
        kind: "file".into(),
        url: file.to_string_lossy().into_owned(),
        name: None,
    });
    r.ch.send(msg).await.unwrap();
    let data = p.sent_data(0);
    let files = mime::files(data.as_bytes());
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].name, "chart.png");
    assert_eq!(files[0].content_type, "image/png");
    assert_eq!(files[0].bytes, [0x89, b'P', b'N', b'G', 0, 1, 2]);
    assert_eq!(mime::parse(data.as_bytes()).text, "The chart.");
    r.run.abort();
}

#[tokio::test]
async fn an_oversized_mail_is_answered_not_read() {
    let p = Provider::start(true);
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = config(&p, None);
    cfg.inbox = Some(Inbox::new(ws.path(), 1));
    let mut r = start(cfg);
    watching(&r).await;
    let big = "x".repeat(3 * 1024 * 1024 + 10);
    let uid = p.deliver(mail(MAX, "huge", &big, &[]));
    until("the answer", T, || p.sent() == 1).await;
    nothing(&mut r).await;
    assert!(p.seen(uid));
    assert!(!p.state().body_fetches.contains(&uid), "never downloaded");
    let text = mime::parse(p.sent_data(0).as_bytes()).text;
    assert!(text.contains("more than I can take"), "{text}");
    r.run.abort();
}

#[tokio::test]
async fn ten_mails_an_hour_to_one_address_then_it_stops() {
    let p = Provider::start(true);
    let ch = EmailChannel::new(config(&p, None)).with_fast_retries();
    for i in 0..email::BUDGET {
        ch.send(out(MAX, &format!("mail {i}"))).await.unwrap();
    }
    let err = ch.send(out(MAX, "one too many")).await.unwrap_err();
    assert!(err.to_string().contains("in the last hour"), "{err}");
    assert!(ch.problem().unwrap().contains("reply loop"));
    assert_eq!(p.sent(), email::BUDGET);
    ch.send(out("dana@team.org", "someone else")).await.unwrap();
}

#[tokio::test]
async fn without_idle_it_polls() {
    let p = Provider::start(false);
    let mut r = start(config(&p, None));
    watching(&r).await;
    p.deliver(mail(MAX, "Re: poll", "polled", &[]));
    assert_eq!(recv(&mut r).await.text, "polled");
    assert!(p.state().noops > 0);
    assert_eq!(p.state().idles, 0);
    r.run.abort();
}

#[tokio::test]
async fn a_restart_resumes_and_a_renumbered_mailbox_is_never_replayed() {
    let p = Provider::start(true);
    let dir = tempfile::tempdir().unwrap();
    let mut r = start(config(&p, Some(dir.path())));
    watching(&r).await;
    p.deliver(mail(MAX, "Re: a", "first", &[]));
    assert_eq!(recv(&mut r).await.text, "first");
    r.run.abort();
    let _ = r.run.await;
    // Mail while it was down is read after the restart.
    p.deliver(mail(MAX, "Re: b", "while down", &[]));
    let mut r = start(config(&p, Some(dir.path())));
    assert_eq!(recv(&mut r).await.text, "while down");
    r.run.abort();
    let _ = r.run.await;
    // The server renumbered the mailbox: everything there counts as old.
    {
        let mut s = p.state();
        s.uidvalidity += 1;
        for m in s.mailbox.iter_mut() {
            m.seen = false;
        }
    }
    p.deliver(mail(MAX, "Re: c", "renumbered, old", &[]));
    let mut r = start(config(&p, Some(dir.path())));
    watching(&r).await;
    nothing(&mut r).await;
    p.deliver(mail(MAX, "Re: d", "after", &[]));
    assert_eq!(recv(&mut r).await.text, "after");
    let state = std::fs::read_to_string(dir.path().join("state.json")).unwrap();
    assert!(state.contains("\"uidvalidity\": 8"), "{state}");
    assert!(!state.contains(PASSWORD));
    r.run.abort();
}

#[tokio::test]
async fn a_dropped_connection_reconnects() {
    let p = Provider::start(true);
    let mut r = start(config(&p, None));
    watching(&r).await;
    p.state().kill += 1;
    until("a second login", T, || p.state().logins >= 2).await;
    p.deliver(mail(MAX, "Re: back", "still here", &[]));
    assert_eq!(recv(&mut r).await.text, "still here");
    r.run.abort();
}

#[tokio::test]
async fn a_refused_login_says_app_password() {
    let p = Provider::start(true);
    let mut cfg = config(&p, None);
    cfg.password = "my real password".into();
    let r = start(cfg.clone());
    until("the refusal", T, || r.ch.problem().is_some()).await;
    let problem = r.ch.problem().unwrap();
    assert!(problem.contains("app password"), "{problem}");
    assert!(!problem.contains("my real password"));
    let err = email::probe(cfg).await.unwrap_err();
    assert!(err.contains("app password"), "{err}");
    r.run.abort();
}

#[tokio::test]
async fn the_probe_logs_in_to_both_and_sends_nothing() {
    let p = Provider::start(true);
    let probe = email::probe(config(&p, None)).await.unwrap();
    assert_eq!(probe.summary(), format!("Ferrule <{ADDRESS}> · IDLE"));
    {
        let s = p.state();
        assert_eq!((s.logins, s.smtp_logins, s.sent.len()), (1, 1, 0));
    }
    let p = Provider::start(false);
    let mut cfg = config(&p, None);
    cfg.address = ADDRESS.into();
    cfg.poll = Duration::from_secs(60);
    let probe = email::probe(cfg).await.unwrap();
    assert_eq!(
        probe.summary(),
        format!("{ADDRESS} · no IDLE, polling every 60 s")
    );
}

/// A real mailbox: `FERRULE_LIVE_EMAIL_ADDRESS`, `…_PASSWORD` (an app
/// password), `…_IMAP` and `…_SMTP` (`host:port`), `…_TO`. Logs in and sends
/// one mail to `…_TO`.
#[tokio::test]
#[ignore]
async fn live_probe_and_send() {
    let var = |k: &str| std::env::var(format!("FERRULE_LIVE_EMAIL_{k}")).unwrap();
    let server = |v: String| {
        let (h, p) = v.rsplit_once(':').unwrap();
        Server::new(h, p.parse().unwrap())
    };
    let cfg = EmailConfig {
        address: var("ADDRESS"),
        username: var("ADDRESS"),
        password: var("PASSWORD"),
        imap: server(var("IMAP")),
        smtp: server(var("SMTP")),
        require_auth: true,
        poll: Duration::from_secs(60),
        state_dir: None,
        inbox: None,
        instance: "live-test".into(),
    };
    println!("{}", email::probe(cfg.clone()).await.unwrap().summary());
    EmailChannel::new(cfg)
        .send(out(&var("TO"), "ferrule's live email test"))
        .await
        .unwrap();
}
