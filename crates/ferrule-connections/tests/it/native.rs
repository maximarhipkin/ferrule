//! M37's key-based ways in, end to end against local mocks: Jira's REST
//! API with an email and token, Gmail over IMAP and SMTP with an app
//! password, Google's APIs with a service account's key. Plus the setup
//! checklist, the "needs attention" list and the buttons never offered.

use crate::common;

use base64::Engine;
use common::*;
use ferrule_connections::native::{net::Addr, Endpoints};
use ferrule_connections::{Connections, ConnectionsConfig};
use ferrule_mcp::ServerHost;
use ferrule_sandbox::Sandbox;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const JIRA_TOKEN: &str = "jira-token-for-tests-0123";
const APP_PASSWORD: &str = "abcdefghijklmnop";

fn fields(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

struct World {
    dir: tempfile::TempDir,
    conns: Arc<Connections>,
}

fn world(ep: Endpoints, secrets: &[(&'static str, &'static str)]) -> World {
    let dir = tempfile::tempdir().unwrap();
    let secrets: Vec<(&'static str, &'static str)> = secrets.to_vec();
    let host = ServerHost {
        sandbox: Arc::new(Sandbox::off()),
        workspace: dir.path().to_path_buf(),
        state_dir: dir.path().join("state"),
    };
    let conns = Connections::new(
        &dir.path().join("private"),
        ConnectionsConfig {
            cloudflared: Some("off".into()),
            ..Default::default()
        },
        Arc::new(move |name: &str| {
            secrets
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }),
        Arc::new(Recorder::default()),
        Some(host),
    )
    .unwrap()
    .with_native(ep);
    World { dir, conns }
}

fn loopback() -> Endpoints {
    Endpoints {
        loopback: true,
        ..Default::default()
    }
}

// ---- Jira -----------------------------------------------------------------

async fn jira_site() -> String {
    let want = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("max@example.com:{JIRA_TOKEN}"))
    );
    serve(Arc::new(move |r: Req| {
        if r.headers.get("authorization") != Some(&want) {
            return Resp::json(401, json!({"message": "Client must be authenticated"}));
        }
        match r.path.as_str() {
            "/rest/api/3/myself" => Resp::json(200, json!({"displayName": "Max"})),
            _ => Resp::json(404, json!({})),
        }
    }))
    .await
}

#[tokio::test]
async fn jira_with_email_and_token_is_tested_saved_sealed_and_bound_to_the_site() {
    let site = jira_site().await;
    let w = world(loopback(), &[]);
    let wrong = w
        .conns
        .connect_key(
            "jira",
            fields(&[
                ("site", &site),
                ("email", "max@example.com"),
                ("token", "nope"),
            ]),
            false,
            "dashboard",
        )
        .await
        .unwrap_err();
    assert!(
        wrong.contains("Atlassian refused the email and token"),
        "{wrong}"
    );
    assert!(!wrong.contains("nope"));
    assert!(
        w.conns.store().load().unwrap().is_empty(),
        "nothing saved on failure"
    );
    assert!(w.conns.last_attempt("jira").is_some());

    let ok = w
        .conns
        .connect_key(
            "jira",
            fields(&[
                ("site", &site),
                ("email", "max@example.com"),
                ("token", JIRA_TOKEN),
                ("unasked", "dropped"),
            ]),
            false,
            "dashboard",
        )
        .await
        .unwrap();
    assert!(!ok.is_empty());
    assert!(w.conns.last_attempt("jira").is_none(), "success clears it");
    let records = w.conns.store().load().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].hosts, vec!["127.0.0.1".to_string()]);
    assert_eq!(records[0].requested_by, "dashboard");
    let file = std::fs::read_to_string(w.conns.store().path()).unwrap();
    assert!(!file.contains(JIRA_TOKEN), "the token is sealed at rest");
    assert!(!file.contains("dropped"));

    // The tools run in-process, listed like any server's.
    let servers = w.conns.servers();
    let local = servers[0]
        .local
        .as_ref()
        .expect("a native server")
        .0
        .clone();
    let names: Vec<String> = local
        .tools()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.iter().any(|n| n.starts_with("jira_")), "{names:?}");

    let tested = w.conns.test(&records[0].name).await.unwrap();
    assert!(
        tested.starts_with("Works: signed in to Jira as Max"),
        "{tested}"
    );
}

#[tokio::test]
async fn a_bad_site_or_missing_field_is_named_before_anything_is_tried() {
    let w = world(Endpoints::default(), &[]);
    let e = w
        .conns
        .connect_key(
            "jira",
            fields(&[
                ("site", "not a site!"),
                ("email", "max@example.com"),
                ("token", "t"),
            ]),
            false,
            "dashboard",
        )
        .await
        .unwrap_err();
    assert!(e.contains("yourcompany.atlassian.net"), "{e}");
    let e = w
        .conns
        .connect_key("jira", fields(&[("site", "acme")]), false, "dashboard")
        .await
        .unwrap_err();
    assert!(e.contains("required"), "{e}");
    let e = w
        .conns
        .connect_key(
            "gmail",
            fields(&[
                ("email", "max@gmail.com"),
                ("app_password", "hunter2 hunter2"),
            ]),
            false,
            "dashboard",
        )
        .await
        .unwrap_err();
    assert!(
        e.contains("16 letters") && e.contains("apppasswords"),
        "{e}"
    );
}

// ---- Gmail: IMAP + SMTP mocks -----------------------------------------------

#[derive(Default)]
struct Mail {
    sent: Vec<String>,
    rcpts: Vec<String>,
}

async fn imap_mock() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let mut io = BufReader::new(s);
                io.get_mut()
                    .write_all(b"* OK Gimap ready\r\n")
                    .await
                    .unwrap();
                loop {
                    let mut line = String::new();
                    if io.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let (tag, rest) = line.trim_end().split_once(' ').unwrap_or(("*", ""));
                    let reply = if rest.starts_with("LOGIN") {
                        if rest.contains(APP_PASSWORD) {
                            format!("{tag} OK authenticated\r\n")
                        } else {
                            format!("{tag} NO [AUTHENTICATIONFAILED] Invalid credentials\r\n")
                        }
                    } else if rest.starts_with("LIST") {
                        format!(
                            "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
                             * LIST (\\All \\HasNoChildren) \"/\" \"[Gmail]/All Mail\"\r\n\
                             {tag} OK done\r\n"
                        )
                    } else if rest.starts_with("EXAMINE") {
                        assert!(rest.contains("[Gmail]/All Mail"), "{rest}");
                        format!("{tag} OK [READ-ONLY] examined\r\n")
                    } else if rest.starts_with("UID SEARCH") {
                        format!("* SEARCH 7 9\r\n{tag} OK done\r\n")
                    } else if rest.starts_with("UID FETCH") {
                        let head = "From: Dana <dana@example.com>\r\nSubject: Invoice\r\nDate: Mon, 1 Sep 2026 10:00:00 +0000\r\n\r\n";
                        let mut out = String::new();
                        for uid in [7, 9] {
                            out.push_str(&format!(
                                "* {uid} FETCH (UID {uid} BODY[HEADER.FIELDS (FROM TO SUBJECT DATE)] {{{}}}\r\n{head})\r\n",
                                head.len()
                            ));
                        }
                        out.push_str(&format!("{tag} OK done\r\n"));
                        out
                    } else if rest.starts_with("LOGOUT") {
                        let _ = io
                            .get_mut()
                            .write_all(format!("* BYE\r\n{tag} OK bye\r\n").as_bytes())
                            .await;
                        return;
                    } else {
                        format!("{tag} BAD unknown\r\n")
                    };
                    io.get_mut().write_all(reply.as_bytes()).await.unwrap();
                }
            });
        }
    });
    port
}

async fn w(io: &mut BufReader<tokio::net::TcpStream>, t: &str) {
    io.get_mut().write_all(t.as_bytes()).await.unwrap();
}

async fn smtp_mock(mail: Arc<Mutex<Mail>>) -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else {
                continue;
            };
            let mail = mail.clone();
            tokio::spawn(async move {
                let mut io = BufReader::new(s);
                w(&mut io, "220 smtp ready\r\n").await;
                loop {
                    let mut line = String::new();
                    if io.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let l = line.trim_end();
                    if l.starts_with("EHLO") {
                        w(&mut io, "250-hi\r\n250 AUTH PLAIN\r\n").await;
                    } else if let Some(t) = l.strip_prefix("AUTH PLAIN ") {
                        let raw = base64::engine::general_purpose::STANDARD.decode(t).unwrap();
                        if String::from_utf8_lossy(&raw).ends_with(APP_PASSWORD) {
                            w(&mut io, "235 ok\r\n").await;
                        } else {
                            w(&mut io, "535 5.7.8 Username and Password not accepted\r\n").await;
                        }
                    } else if l.starts_with("MAIL FROM") {
                        w(&mut io, "250 ok\r\n").await;
                    } else if let Some(r) = l.strip_prefix("RCPT TO:") {
                        mail.lock().unwrap().rcpts.push(r.to_string());
                        w(&mut io, "250 ok\r\n").await;
                    } else if l == "DATA" {
                        w(&mut io, "354 go\r\n").await;
                        let mut body = Vec::new();
                        loop {
                            let mut b = Vec::new();
                            io.read_until(b'\n', &mut b).await.unwrap();
                            if b == b".\r\n" {
                                break;
                            }
                            body.extend(b);
                        }
                        mail.lock()
                            .unwrap()
                            .sent
                            .push(String::from_utf8_lossy(&body).into());
                        w(&mut io, "250 queued\r\n").await;
                    } else if l == "QUIT" {
                        w(&mut io, "221 bye\r\n").await;
                        return;
                    } else {
                        w(&mut io, "502 no\r\n").await;
                    }
                }
            });
        }
    });
    port
}

#[tokio::test]
async fn gmail_with_an_app_password_searches_and_only_a_write_connection_sends() {
    let mail = Arc::new(Mutex::new(Mail::default()));
    let ep = Endpoints {
        imap: Addr::plain("127.0.0.1", imap_mock().await),
        smtp: Addr::plain("127.0.0.1", smtp_mock(mail.clone()).await),
        ..Default::default()
    };
    let w = world(ep, &[]);
    let wrong = w
        .conns
        .connect_key(
            "gmail",
            fields(&[
                ("email", "max@gmail.com"),
                ("app_password", "zzzz zzzz zzzz zzzz"),
            ]),
            false,
            "dashboard",
        )
        .await
        .unwrap_err();
    assert!(
        wrong.contains("apppasswords") && !wrong.contains("zzzz"),
        "{wrong}"
    );

    w.conns
        .connect_key(
            "gmail",
            fields(&[
                ("email", "max@gmail.com"),
                ("app_password", "abcd efgh ijkl mnop"),
            ]),
            false,
            "dashboard",
        )
        .await
        .unwrap();
    let local = w.conns.servers()[0].local.as_ref().unwrap().0.clone();
    let names: Vec<String> = local
        .tools()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        !names.contains(&"gmail_send".to_string()),
        "read-only: {names:?}"
    );
    let found = local
        .call("gmail_search", json!({"query": "subject:invoice"}))
        .await
        .unwrap();
    assert!(
        found.contains("2 match") && found.contains("Invoice"),
        "{found}"
    );
    assert!(local
        .call(
            "gmail_send",
            json!({"to": "a@b.c", "subject": "s", "body": "b"})
        )
        .await
        .is_err());

    // A write connection lists send, and it goes out over SMTP.
    w.conns.disconnect("gmail").await.unwrap();
    w.conns
        .connect_key(
            "gmail",
            fields(&[("email", "max@gmail.com"), ("app_password", APP_PASSWORD)]),
            true,
            "dashboard",
        )
        .await
        .unwrap();
    let local = w.conns.servers()[0].local.as_ref().unwrap().0.clone();
    let send = local
        .tools()
        .into_iter()
        .find(|t| t["name"] == "gmail_send")
        .expect("listed on a write connection");
    assert_eq!(
        send["annotations"]["readOnlyHint"], false,
        "gated as a write"
    );
    let sent = local
        .call(
            "gmail_send",
            json!({"to": "dana@example.com", "subject": "Hello", "body": "Hi Dana\n.dot line"}),
        )
        .await
        .unwrap();
    assert_eq!(sent, "sent to dana@example.com");
    let m = mail.lock().unwrap();
    assert_eq!(m.rcpts, vec!["<dana@example.com>".to_string()]);
    assert!(m.sent[0].contains("Subject: Hello"));
    let body = base64::engine::general_purpose::STANDARD.encode("Hi Dana\n.dot line");
    assert!(m.sent[0].contains(&body), "{}", m.sent[0]);
}

// ---- Google: a service account's key --------------------------------------

fn service_account_json() -> String {
    let pem = format!(
        "-----BEGIN {k}-----\n{}-----END {k}-----\n",
        include_str!("../../src/native/testdata/rsa-2048.b64"),
        k = "PRIVATE KEY"
    );
    let mut v = serde_json::Map::new();
    v.insert("type".into(), Value::from("service_account"));
    v.insert(
        "client_email".into(),
        Value::from("bot@proj.iam.gserviceaccount.com"),
    );
    v.insert("private_key".into(), Value::from(pem));
    Value::Object(v).to_string()
}

#[tokio::test]
async fn google_with_a_service_account_key_gets_a_token_and_says_what_to_share() {
    let api = serve(Arc::new(|r: Req| match (r.method.as_str(), r.path.as_str()) {
        ("POST", "/token") => {
            let f = r.form();
            assert_eq!(
                f.get("grant_type").map(String::as_str),
                Some("urn:ietf:params:oauth:grant-type:jwt-bearer")
            );
            Resp::json(200, json!({"access_token": "ya29.test", "expires_in": 3600}))
        }
        ("GET", "/drive/v3/files") if r.headers.get("authorization").map(String::as_str) == Some("Bearer ya29.test") => {
            Resp::json(200, json!({"files": [{"id": "f1", "name": "Plan", "mimeType": "application/vnd.google-apps.document"}]}))
        }
        _ => Resp::json(403, json!({"error": {"message": "nope"}})),
    }))
    .await;
    let ep = Endpoints {
        google_token: format!("{api}/token"),
        google_base: Some(api.clone()),
        ..Default::default()
    };
    let w = world(ep, &[]);
    let e = w
        .conns
        .connect_key(
            "google",
            fields(&[("key", "{\"type\":\"authorized_user\"}")]),
            false,
            "dashboard",
        )
        .await
        .unwrap_err();
    assert!(e.contains("isn't a service account key"), "{e}");
    w.conns
        .connect_key(
            "google",
            fields(&[("key", &service_account_json())]),
            false,
            "dashboard",
        )
        .await
        .unwrap();
    let r = &w.conns.store().load().unwrap()[0];
    assert_eq!(r.hosts, vec!["127.0.0.1".to_string()]);
    let tested = w.conns.test(&r.name).await.unwrap();
    assert!(
        tested.contains("bot@proj.iam.gserviceaccount.com"),
        "{tested}"
    );
    let file = std::fs::read_to_string(w.conns.store().path()).unwrap();
    assert!(!file.contains("PRIVATE KEY") && !file.contains("gserviceaccount"));
}

// ---- expiry, the checklist, and buttons bound to fail ---------------------

fn day_from_now(days: i64) -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + days * 86_400;
    // Civil date from days since the epoch (Hinnant).
    let z = t.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[tokio::test]
async fn a_key_with_an_end_date_is_named_before_it_stops() {
    let site = jira_site().await;
    let w = world(loopback(), &[]);
    let with = |day: String| {
        fields(&[
            ("site", &site),
            ("email", "max@example.com"),
            ("token", JIRA_TOKEN),
            ("expires", &day),
        ])
    };
    let past = w
        .conns
        .connect_key("jira", with(day_from_now(-3)), false, "dashboard")
        .await
        .unwrap_err();
    assert!(past.contains("passed"), "{past}");
    let bad = w
        .conns
        .connect_key("jira", with("next tuesday".into()), false, "dashboard")
        .await
        .unwrap_err();
    assert!(bad.contains("YYYY-MM-DD") || bad.contains("date"), "{bad}");
    w.conns
        .connect_key("jira", with(day_from_now(3)), false, "dashboard")
        .await
        .unwrap();
    assert!(w.conns.attention(1).is_empty());
    let soon = w.conns.attention(7);
    assert_eq!(soon.len(), 1);
    assert!(!soon[0].expired);
    assert!(soon[0].text.contains("expires in"), "{}", soon[0].text);
    assert_eq!(soon[0].tile, "atlassian");
}

#[tokio::test]
async fn the_checklist_says_what_is_missing_and_what_to_do_next() {
    let site = jira_site().await;
    let w = world(loopback(), &[]);
    let list = w.conns.checklist().await;
    let get = |l: &ferrule_connections::service::Checklist, id: &str| {
        l.checks.iter().find(|c| c.id == id).cloned().unwrap()
    };
    assert!(list.callback.is_none());
    let relay = get(&list, "relay");
    assert_eq!(relay.state, "missing");
    assert_eq!(relay.action.as_ref().unwrap().action, "relay_setup");
    assert_eq!(get(&list, "google_client").state, "missing");
    assert_eq!(get(&list, "mcp_preview").state, "unknown");
    let atl = get(&list, "atlassian");
    assert_eq!(atl.state, "not connected");
    assert_eq!(atl.action.as_ref().unwrap().service, Some("jira"));

    w.conns
        .connect_key(
            "jira",
            fields(&[
                ("site", &site),
                ("email", "max@example.com"),
                ("token", JIRA_TOKEN),
            ]),
            false,
            "dashboard",
        )
        .await
        .unwrap();
    assert_eq!(
        get(&w.conns.checklist().await, "atlassian").state,
        "connected"
    );

    // A relay address whose key isn't here; then one that doesn't answer.
    w.conns.set_relay_url(Some("http://127.0.0.1:1".into()));
    let relay = get(&w.conns.checklist().await, "relay");
    assert_eq!(relay.state, "missing");
    assert_eq!(relay.action.unwrap().action, "relay_use");
    let w2 = world(
        loopback(),
        &[
            ("FERRULE_RELAY_KEY", "k"),
            ("FERRULE_GOOGLE_CLIENT_ID", "id"),
            ("FERRULE_GOOGLE_CLIENT_SECRET", "s"),
        ],
    );
    w2.conns.set_relay_url(Some("http://127.0.0.1:1".into()));
    let l2 = w2.conns.checklist().await;
    assert_eq!(get(&l2, "relay").state, "unreachable");
    assert_eq!(l2.callback.as_deref(), Some("http://127.0.0.1:1/cb"));
    assert_eq!(get(&l2, "google_client").state, "ready");
    drop(w.dir);
}

#[tokio::test]
async fn no_button_is_offered_that_is_bound_to_fail() {
    let w = world(Endpoints::default(), &[]);
    let cat = w.conns.catalog();
    // Google with your own client: none saved.
    let r = w
        .conns
        .blocked(cat.get("google_oauth").unwrap(), false)
        .unwrap();
    assert!(
        r.text.contains("ferrule connections setup google"),
        "{}",
        r.text
    );
    assert!(r.text.contains("app password"));
    assert!(
        r.buttons.is_empty(),
        "the simple ways in take several fields: the page, not the chat"
    );
    // Atlassian's sign-in without a fixed callback.
    let atl = cat.get("atlassian").unwrap();
    let r = w.conns.blocked(atl, false).unwrap();
    assert!(
        r.text.contains("API token") && r.text.contains("relay"),
        "{}",
        r.text
    );
    assert!(r
        .buttons
        .iter()
        .all(|b| !format!("{:?}", b.action).contains("/connect atlassian")));
    assert!(
        w.conns.blocked(atl, true).is_none(),
        "with the relay it can work"
    );
    // A key-based service is never blocked here (it has no sign-in).
    assert!(w.conns.blocked(cat.get("jira").unwrap(), false).is_none());
}
