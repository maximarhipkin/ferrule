//! M39 §3: the WhatsApp adapter against a mock Graph API and relay
//! mailbox — the mailbox set up and drained in order, signatures checked
//! here too, redeliveries and strangers dropped, text, buttons and files
//! out, the 👀 as a read receipt, the 24-hour window (held, a template,
//! said plainly without one), rate limits, a refused token, and the local
//! listener's hub verification.

mod support;

use ferrule_gateway::channel::{Button, ButtonAction};
use ferrule_gateway::channels::files::Inbox;
use ferrule_gateway::channels::whatsapp::{Inbound, Template, WhatsAppChannel, WhatsAppConfig};
use ferrule_gateway::{Attachment, Channel, GatewayError, InboundMessage, OutboundMessage};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use support::until;
use support::whatsapp::{self as mock, Meta, APP_SECRET, MAX, PHONE_ID, RELAY_KEY, TOKEN, VERIFY};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const T: Duration = Duration::from_secs(10);

struct Running {
    ch: Arc<WhatsAppChannel>,
    rx: mpsc::Receiver<InboundMessage>,
    run: JoinHandle<Result<(), GatewayError>>,
}

fn config(meta: &Meta, inbound: Inbound, state: Option<&Path>) -> WhatsAppConfig {
    WhatsAppConfig {
        phone_number_id: PHONE_ID.into(),
        token: TOKEN.into(),
        app_secret: APP_SECRET.into(),
        verify_token: VERIFY.into(),
        api_url: meta.url.clone(),
        api_version: "v23.0".into(),
        inbound,
        template: None,
        state_dir: state.map(Path::to_path_buf),
        inbox: None,
    }
}

fn relay(meta: &Meta) -> Inbound {
    Inbound::Relay {
        url: meta.url.clone(),
        key: RELAY_KEY.into(),
    }
}

fn spawn(cfg: WhatsAppConfig) -> Running {
    let ch = Arc::new(
        WhatsAppChannel::new(cfg)
            .with_allowed(vec![MAX.into()])
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

fn out(text: &str) -> OutboundMessage {
    OutboundMessage {
        channel: "whatsapp".into(),
        chat_id: MAX.into(),
        text: text.into(),
        reply_to: None,
        attachments: vec![],
    }
}

#[tokio::test]
async fn the_mailbox_is_set_up_and_drained_in_order_once() {
    let meta = mock::start();
    let dir = tempfile::tempdir().unwrap();
    let mut r = spawn(config(&meta, relay(&meta), Some(dir.path())));
    until("configured", T, || meta.state().config.is_some()).await;
    assert_eq!(
        meta.state().config.clone().unwrap(),
        (VERIFY.to_string(), APP_SECRET.to_string())
    );
    meta.deliver(&mock::text("w1", MAX, "hello"), APP_SECRET);
    meta.deliver(&mock::text("w2", MAX, "second"), APP_SECRET);
    // Meta redelivers; a stranger writes; someone forges a body.
    meta.deliver(&mock::text("w1", MAX, "hello"), APP_SECRET);
    meta.deliver(&mock::text("w3", "15550999", "let me in"), APP_SECRET);
    meta.deliver(&mock::text("w4", MAX, "forged"), "not-the-secret");
    meta.deliver(&mock::text("w5", MAX, "third"), APP_SECRET);
    let a = recv(&mut r).await;
    assert_eq!(
        (
            a.chat_id.as_str(),
            a.sender.as_str(),
            a.text.as_str(),
            a.message_id.as_str()
        ),
        (MAX, "Max", "hello", "w1")
    );
    assert_eq!(a.sender_id.as_deref(), Some(MAX));
    assert!(a.ts > 1_700_000_000);
    assert_eq!(recv(&mut r).await.text, "second");
    assert_eq!(recv(&mut r).await.text, "third");
    assert!(r.ch.last_ok_poll().is_some());
    assert!(r.ch.polls());
    until("acked", T, || meta.state().events.is_empty()).await;
    // Nobody was answered: not the stranger, not the forger.
    assert!(meta.state().sent.is_empty(), "{:?}", meta.state().sent);
    r.run.abort();
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("mailbox.json")).unwrap())
            .unwrap();
    assert_eq!(saved["seq"], 6);

    // A restart asks for what comes after the last one taken.
    let r2 = spawn(config(&meta, relay(&meta), Some(dir.path())));
    let before = meta.state().takes;
    until("a take", T, || meta.state().takes > before).await;
    assert_eq!(*meta.state().afters.last().unwrap(), 6);
    r2.run.abort();
}

#[tokio::test]
async fn text_goes_as_whatsapp_markup_split_at_4096() {
    let meta = mock::start();
    let r = spawn(config(&meta, Inbound::None, None));
    r.ch.send(out("**bold** and [docs](https://example.com/a_b)"))
        .await
        .unwrap();
    let long = "word ".repeat(1200);
    r.ch.send(out(&long)).await.unwrap();
    let sent = meta.of_type("text");
    assert_eq!(sent[0]["to"], MAX);
    assert_eq!(sent[0]["messaging_product"], "whatsapp");
    assert_eq!(
        sent[0]["text"]["body"],
        "*bold* and docs (https://example.com/a_b)"
    );
    assert_eq!(sent.len(), 3, "the long one in two");
    assert!(sent[1..]
        .iter()
        .all(|m| m["text"]["body"].as_str().unwrap().chars().count() <= 4096));
}

#[tokio::test]
async fn approvals_are_reply_buttons_and_come_back_as_their_command() {
    let meta = mock::start();
    let mut r = spawn(config(&meta, relay(&meta), None));
    let b = |t: &str, c: &str| Button {
        text: t.into(),
        action: ButtonAction::Command(c.into()),
    };
    r.ch.send_buttons(
        out("Run `rm -rf build`?"),
        &[
            b("Approve", "/approve a1"),
            b("Deny this one, please thanks", "/deny a1"),
        ],
    )
    .await
    .unwrap();
    let i = &meta.of_type("interactive")[0]["interactive"];
    assert_eq!(i["type"], "button");
    assert_eq!(i["body"]["text"], "Run `rm -rf build`?");
    let buttons = i["action"]["buttons"].as_array().unwrap();
    assert_eq!(buttons[0]["reply"]["id"], "/approve a1");
    assert_eq!(buttons[1]["reply"]["title"], "Deny this one, plea…");

    // A tap is a message with the button's id.
    meta.deliver(
        &mock::message(
            "w9",
            MAX,
            "interactive",
            json!({"type": "button_reply", "button_reply": {"id": "/approve a1", "title": "Approve"}}),
        ),
        APP_SECRET,
    );
    assert_eq!(recv(&mut r).await.text, "/approve a1");

    // A link button can't be a reply button: all of it goes as text.
    r.ch.send_buttons(
        out("Read this"),
        &[Button {
            text: "Docs".into(),
            action: ButtonAction::Url("https://example.com".into()),
        }],
    )
    .await
    .unwrap();
    assert_eq!(meta.of_type("interactive").len(), 1);
    assert!(meta.texts().last().unwrap().contains("https://example.com"));
    r.run.abort();
}

#[tokio::test]
async fn the_eyes_are_a_read_receipt_and_a_reaction() {
    let meta = mock::start();
    let r = spawn(config(&meta, Inbound::None, None));
    r.ch.react(MAX, "wamid.in1", "👀").await.unwrap();
    let sent = meta.state().sent.clone();
    assert_eq!(sent[0]["status"], "read");
    assert_eq!(sent[0]["message_id"], "wamid.in1");
    assert_eq!(sent[1]["type"], "reaction");
    assert_eq!(sent[1]["reaction"]["emoji"], "👀");
    assert_eq!(sent[1]["reaction"]["message_id"], "wamid.in1");
}

#[tokio::test]
async fn a_closed_window_without_a_template_holds_and_says_so() {
    let meta = mock::start();
    let dir = tempfile::tempdir().unwrap();
    let mut r = spawn(config(&meta, relay(&meta), Some(dir.path())));
    meta.state().fail_next.push_back(131047);
    let e = r.ch.send(out("the nightly report")).await.unwrap_err();
    assert!(e.to_string().contains("24-hour window"), "{e}");
    assert!(e.to_string().contains("held"), "{e}");
    // Known closed now: the next one isn't even tried.
    let tried = meta.state().sent.len();
    assert!(r.ch.send(out("and another")).await.is_err());
    assert_eq!(meta.state().sent.len(), tried);
    assert_eq!(r.ch.held()[MAX], 2);
    let p = r.ch.problem().unwrap();
    assert!(p.contains("no template"), "{p}");
    let on_disk = ferrule_gateway::channels::whatsapp::window::held_on_disk(dir.path(), now());
    assert_eq!(on_disk[MAX], 2);

    // They write: what was held goes first, then their message is read.
    meta.deliver(&mock::text("w1", MAX, "hi"), APP_SECRET);
    assert_eq!(recv(&mut r).await.text, "hi");
    let texts = meta.texts();
    let n = texts.len();
    assert!(texts[n - 3].starts_with("Held while WhatsApp"), "{texts:?}");
    assert_eq!(texts[n - 2], "the nightly report");
    assert_eq!(texts[n - 1], "and another");
    assert!(r.ch.held().is_empty());
    assert!(r.ch.problem().is_none());
    r.run.abort();
}

#[tokio::test]
async fn with_a_template_the_person_is_told_once() {
    let meta = mock::start();
    let mut cfg = config(&meta, Inbound::None, None);
    cfg.template = Some(Template {
        name: "ferrule_waiting".into(),
        language: "en_US".into(),
    });
    let r = spawn(cfg);
    meta.state().fail_next.push_back(131047);
    r.ch.send(out("line one\nline two")).await.unwrap();
    r.ch.send(out("more")).await.unwrap();
    let t = meta.of_type("template");
    assert_eq!(t.len(), 1, "one template per closed window");
    assert_eq!(t[0]["template"]["name"], "ferrule_waiting");
    assert_eq!(t[0]["template"]["language"]["code"], "en_US");
    assert_eq!(
        t[0]["template"]["components"][0]["parameters"][0]["text"],
        "line one line two"
    );
    assert_eq!(r.ch.held()[MAX], 2);
    assert!(r.ch.problem().is_none());
}

#[tokio::test]
async fn a_failed_status_holds_the_message_and_is_a_problem() {
    let meta = mock::start();
    let r = spawn(config(&meta, relay(&meta), None));
    r.ch.send(out("sent but not delivered")).await.unwrap();
    let id = format!("wamid.out{}", meta.state().sent.len());
    meta.deliver(&mock::failed(&id, MAX, 131047), APP_SECRET);
    until("held", T, || !r.ch.held().is_empty()).await;
    // No template: the held message is what the owner hears about.
    assert!(r.ch.problem().unwrap().contains("held"));
    r.run.abort();
}

#[tokio::test]
async fn rate_limits_are_waited_out_and_a_refused_token_is_a_problem() {
    let meta = mock::start();
    let r = spawn(config(&meta, Inbound::None, None));
    meta.state().fail_next.extend([130429, 130429]);
    r.ch.send(out("eventually")).await.unwrap();
    assert_eq!(meta.texts().last().unwrap(), "eventually");
    meta.state().fail_next.extend([130429; 4]);
    let e = r.ch.send(out("never")).await.unwrap_err();
    assert!(matches!(e, GatewayError::RateLimited { .. }), "{e:?}");

    meta.state().fail_next.push_back(131030);
    let e = r.ch.send(out("x")).await.unwrap_err();
    assert!(e.to_string().contains("API Setup"), "{e}");

    meta.state().fail_next.push_back(190);
    assert!(r.ch.send(out("x")).await.is_err());
    let p = r.ch.problem().unwrap();
    assert!(p.contains("token was refused"), "{p}");
    assert!(!p.contains(TOKEN));
    r.ch.send(out("fixed")).await.unwrap();
    assert!(r.ch.problem().is_none());
}

#[tokio::test]
async fn files_come_in_to_the_inbox_and_go_out_as_media() {
    let meta = mock::start();
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = config(&meta, relay(&meta), None);
    cfg.inbox = Some(Inbox::new(ws.path(), 1));
    let mut r = spawn(cfg);
    meta.deliver(
        &mock::message(
            "wimg",
            MAX,
            "image",
            json!({"id": "media-1", "mime_type": "image/jpeg", "caption": "what is this"}),
        ),
        APP_SECRET,
    );
    let m = recv(&mut r).await;
    assert!(m.text.starts_with("what is this"), "{}", m.text);
    assert!(m.text.contains("inbox/whatsapp/"), "{}", m.text);
    assert_eq!(m.attachments.len(), 1);
    assert_eq!(std::fs::read(&m.attachments[0].url).unwrap(), b"JPEGBYTES");

    // Too big: refused, and the sender is told.
    meta.deliver(
        &mock::message(
            "wbig",
            MAX,
            "image",
            json!({"id": "media-big", "mime_type": "image/jpeg"}),
        ),
        APP_SECRET,
    );
    let m = recv(&mut r).await;
    assert!(m.text.contains("wasn't saved"), "{}", m.text);
    assert!(meta.texts().last().unwrap().starts_with("I couldn't take"));

    // Out: uploaded, then sent as an image with the caption.
    let file = ws.path().join("chart.png");
    std::fs::write(&file, b"\x89PNG fake").unwrap();
    let mut msg = out("the chart");
    msg.attachments = vec![Attachment {
        kind: "image/png".into(),
        url: file.to_string_lossy().into_owned(),
        name: Some("chart.png".into()),
    }];
    r.ch.send(msg).await.unwrap();
    assert_eq!(meta.state().uploads[0].0, "image/png");
    let img = &meta.of_type("image")[0];
    assert_eq!(img["image"]["id"], "media-up-1");
    assert_eq!(img["image"]["caption"], "the chart");
    r.run.abort();
}

#[tokio::test]
async fn the_listener_verifies_the_hub_and_every_signature() {
    let meta = mock::start();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut r = spawn(config(&meta, Inbound::Listen { port }, None));
    assert!(!r.ch.polls());
    let base = format!("http://127.0.0.1:{port}/webhook");
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut tries = 0;
    let verify = loop {
        match client
            .get(format!(
                "{base}?hub.mode=subscribe&hub.verify_token={VERIFY}&hub.challenge=12345"
            ))
            .send()
            .await
        {
            Ok(v) => break v,
            Err(_) if tries < 200 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("{e}"),
        }
    };
    assert_eq!(verify.status(), 200);
    assert_eq!(verify.text().await.unwrap(), "12345");
    let wrong = client
        .get(format!(
            "{base}?hub.mode=subscribe&hub.verify_token=nope&hub.challenge=1"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 403);

    let body = mock::text("wl1", MAX, "over the tunnel").to_string();
    let bad = client
        .post(&base)
        .header("x-hub-signature-256", "sha256=00")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 401);
    let big = client
        .post(&base)
        .body(vec![b'x'; 300 * 1024])
        .send()
        .await
        .unwrap();
    assert_eq!(big.status(), 413);
    let sig = ferrule_gateway::channels::hmac::sign(APP_SECRET.as_bytes(), body.as_bytes());
    let ok = client
        .post(&base)
        .header("x-hub-signature-256", sig)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    assert_eq!(recv(&mut r).await.text, "over the tunnel");
    r.run.abort();
}

#[tokio::test]
async fn setup_pairs_by_code_and_the_owner_is_answered() {
    let meta = mock::start();
    let ch = Arc::new(
        WhatsAppChannel::new(config(&meta, relay(&meta), None))
            .with_pairing("PAIR-1234")
            .with_fast_retries(),
    );
    let (tx, _rx) = mpsc::channel(8);
    let c = ch.clone();
    let run = tokio::spawn(async move { c.run(tx).await });
    meta.deliver(&mock::text("wp", MAX, "PAIR-1234"), APP_SECRET);
    until("paired", T, || ch.paired().is_some()).await;
    assert_eq!(ch.paired().unwrap(), (MAX.to_string(), "Max".to_string()));
    until("told", T, || !meta.texts().is_empty()).await;
    assert!(meta.texts()[0].starts_with("Paired."));
    run.abort();
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[tokio::test]
async fn the_probe_reads_the_number_and_says_what_is_wrong() {
    let meta = mock::start();
    let p = ferrule_gateway::channels::whatsapp::probe(&meta.url, "v23.0", PHONE_ID, TOKEN)
        .await
        .unwrap();
    assert_eq!(
        (p.number.as_str(), p.name.as_str()),
        ("+1 555 0100", "Ferrule Test")
    );
    let e = ferrule_gateway::channels::whatsapp::probe(&meta.url, "v23.0", PHONE_ID, "EAAwrong")
        .await
        .unwrap_err();
    assert!(
        e.contains("Meta refused the token") && e.contains("system user"),
        "{e}"
    );
    let e = ferrule_gateway::channels::whatsapp::probe(&meta.url, "v23.0", "+1 555", TOKEN)
        .await
        .unwrap_err();
    assert!(e.contains("should be digits"), "{e}");
}

// --- live --------------------------------------------------------------------

/// Against Meta's real Cloud API: reads the number, sends a message with
/// reply buttons to `FERRULE_LIVE_WHATSAPP_TO` (who must have written to
/// the number within 24 hours, or it is held and said so), and, with
/// `FERRULE_LIVE_WHATSAPP_RELAY_URL` + `FERRULE_LIVE_RELAY_KEY`, waits two
/// minutes for their reply through the relay. `FERRULE_LIVE_WHATSAPP_TOKEN`,
/// `FERRULE_LIVE_WHATSAPP_PHONE_ID`, `FERRULE_LIVE_WHATSAPP_APP_SECRET`,
/// `FERRULE_LIVE_WHATSAPP_VERIFY_TOKEN`; see docs/channels.md.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real WhatsApp Business number; see docs/channels.md"]
async fn whatsapp_live_round_trip() {
    let var = |n: &str| std::env::var(n).unwrap_or_else(|_| panic!("{n}"));
    let token = var("FERRULE_LIVE_WHATSAPP_TOKEN");
    let phone = var("FERRULE_LIVE_WHATSAPP_PHONE_ID");
    let to = var("FERRULE_LIVE_WHATSAPP_TO");
    let api = ferrule_gateway::channels::whatsapp::API_URL;
    let version = ferrule_gateway::channels::whatsapp::API_VERSION;
    let p = ferrule_gateway::channels::whatsapp::probe(api, version, &phone, &token)
        .await
        .unwrap();
    println!("number {} ({})", p.number, p.name);
    let relay = std::env::var("FERRULE_LIVE_WHATSAPP_RELAY_URL").ok();
    let inbound = match &relay {
        Some(url) => Inbound::Relay {
            url: url.clone(),
            key: var("FERRULE_LIVE_RELAY_KEY"),
        },
        None => Inbound::None,
    };
    let ch = Arc::new(
        WhatsAppChannel::new(WhatsAppConfig {
            phone_number_id: phone,
            token,
            app_secret: std::env::var("FERRULE_LIVE_WHATSAPP_APP_SECRET").unwrap_or_default(),
            verify_token: std::env::var("FERRULE_LIVE_WHATSAPP_VERIFY_TOKEN").unwrap_or_default(),
            api_url: api.into(),
            api_version: version.into(),
            inbound,
            template: None,
            state_dir: None,
            inbox: None,
        })
        .with_allowed(vec![to.clone()]),
    );
    let (tx, mut rx) = mpsc::channel(8);
    let c = ch.clone();
    tokio::spawn(async move { c.run(tx).await });
    let buttons = [Button {
        text: "Yes".into(),
        action: ButtonAction::Command("/yes".into()),
    }];
    let sent = ch
        .send_buttons(
            OutboundMessage {
                chat_id: to.clone(),
                ..out("ferrule live test: *bold*, then tap Yes")
            },
            &buttons,
        )
        .await;
    println!("send: {sent:?}");
    if relay.is_some() {
        let m = tokio::time::timeout(Duration::from_secs(120), rx.recv())
            .await
            .expect("a reply within two minutes")
            .unwrap();
        assert_eq!(m.chat_id, to);
        ch.react(&m.chat_id, &m.message_id, "👀").await.unwrap();
    }
}
