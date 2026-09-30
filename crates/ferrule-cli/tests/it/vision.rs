//! M47 part 5b: a photo reaches a model that can see it (as pixels) or one
//! that can't (as a saved file and a plain notice), from Telegram and from
//! the dashboard page, through the real `ferrule` binary. The scripted
//! provider records what each request carried.

use super::dashboard::{gateway, home, http, sign_in, telegram, two, FakeTelegram, Page, Server};
use base64::Engine;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// The first bytes of a JPEG, then filler: enough for the bytes to be
/// sniffed as one.
fn jpeg(len: usize) -> Vec<u8> {
    let mut b = vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0,
    ];
    b.resize(len, 7);
    b
}

fn png() -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n".to_vec();
    b.resize(64, 1);
    b
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn sizes(id: &str) -> Value {
    json!([
        {"file_id": "tiny", "width": 90, "height": 60, "file_size": 900},
        {"file_id": id, "width": 1280, "height": 720, "file_size": 40_000},
    ])
}

/// Provider `a` with `default` as its model (and `other` as its second),
/// `extra` a table appended to it.
fn config(a: &Server, b: &Server, tg: &FakeTelegram, default: &str, other: &str) -> String {
    two(a, b, "", &telegram(tg))
        .replace("model = \"a-one\"", &format!("model = \"{default}\""))
        .replace("\"a-two\"", &format!("\"{other}\""))
}

/// Every image the model was sent, over all the requests.
fn pixels(server: &Server) -> Vec<String> {
    let mut out = Vec::new();
    for body in server.bodies() {
        for m in body["messages"].as_array().into_iter().flatten() {
            for part in m["content"].as_array().into_iter().flatten() {
                if part["type"] == "image_url" {
                    out.push(part["image_url"]["url"].as_str().unwrap_or("").to_string());
                }
            }
        }
    }
    out
}

/// The text of the last user message sent to the model.
fn last_user(server: &Server) -> String {
    let bodies = server.bodies();
    let last = bodies.last().expect("the model was asked");
    let m = last["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .unwrap();
    match &m["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

fn no_requests_yet(server: &Server) {
    assert!(server.bodies().is_empty(), "{:#?}", server.bodies());
}

#[test]
fn a_telegram_photo_reaches_a_vision_model_as_pixels() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let dir = home(&config(&a, &b, &tg, "gpt-4o", "deepseek-chat"));
    let _gw = gateway(dir.path(), &[]);
    let bytes = jpeg(40_000);
    tg.add_file("big", &bytes);
    tg.add_file("tiny", &jpeg(900));
    no_requests_yet(&a);

    tg.say_photo(42, sizes("big"), "what is on this label?", None);
    let (_, said) = tg.wait_for(42, "A:gpt-4o", 0);
    assert_eq!(said, "A:gpt-4o");

    // The 1280 px size came as pixels, byte for byte, with its caption and
    // the saved path as a note; the model wasn't told it can't see.
    let seen = pixels(&a);
    assert_eq!(seen.len(), 1, "{:#?}", a.bodies());
    assert_eq!(
        seen[0],
        format!("data:image/jpeg;base64,{}", b64(&bytes)),
        "the biggest size that fits"
    );
    let words = last_user(&a);
    assert!(words.contains("what is on this label?"), "{words}");
    assert!(words.contains("inbox/telegram/"), "{words}");
    assert!(!words.contains("can't see"), "{words}");
    // The user wasn't told the model is blind.
    assert!(!tg.all().contains("can't see images"), "{}", tg.all());
    // The file is on disk where the note says.
    let saved: Vec<_> = walk(&dir.path().join("work").join("inbox"));
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert_eq!(std::fs::read(&saved[0]).unwrap(), bytes);
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in read.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[test]
fn a_text_only_model_gets_the_saved_file_and_the_user_a_notice() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let dir = home(&config(&a, &b, &tg, "deepseek-chat", "gpt-4o"));
    let _gw = gateway(dir.path(), &[]);
    tg.add_file("big", &jpeg(40_000));
    tg.add_file("tiny", &jpeg(900));

    tg.say_photo(42, sizes("big"), "", None);
    let (n, _) = tg.wait_for(42, "A:deepseek-chat", 0);
    let (_, notice) = tg.wait_for(42, "can't see images", 0);
    assert!(notice.contains("sees photos"), "{notice}");
    assert!(pixels(&a).is_empty(), "{:#?}", a.bodies());
    let words = last_user(&a);
    assert!(
        words.contains("inbox/telegram/"),
        "the file's path: {words}"
    );

    // A second photo in the same chat: still no pixels, and no second notice.
    tg.say_photo(42, sizes("big"), "and this one?", None);
    tg.wait_for(42, "A:deepseek-chat", n);
    let notices = tg
        .sent
        .lock()
        .unwrap()
        .iter()
        .filter(|m| {
            m["text"]
                .as_str()
                .unwrap_or("")
                .contains("can't see images")
        })
        .count();
    assert_eq!(notices, 1, "{}", tg.all());
    assert!(pixels(&a).is_empty());
}

#[test]
fn vision_false_in_config_wins_over_the_name() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let mut config = config(&a, &b, &tg, "gpt-4o", "gpt-4o-mini");
    config = config.replace(
        "[providers.a.models.\"gpt-4o-mini\"]",
        "[providers.a.models.\"gpt-4o\"]\nvision = false\n[providers.a.models.\"gpt-4o-mini\"]",
    );
    let dir = home(&config);
    let _gw = gateway(dir.path(), &[]);
    tg.add_file("big", &jpeg(40_000));
    tg.add_file("tiny", &jpeg(900));

    tg.say_photo(42, sizes("big"), "", None);
    tg.wait_for(42, "A:gpt-4o", 0);
    tg.wait_for(42, "can't see images", 0);
    assert!(
        pixels(&a).is_empty(),
        "the config said no: {:#?}",
        a.bodies()
    );
}

#[test]
fn a_photo_over_the_cap_is_refused_with_a_reason() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let dir = home(&config(&a, &b, &tg, "gpt-4o", "deepseek-chat"));
    let _gw = gateway(dir.path(), &[]);

    let huge = json!([
        {"file_id": "h", "width": 4000, "height": 3000, "file_size": 30_000_000},
    ]);
    tg.say_photo(42, huge, "is this ok?", None);
    tg.wait_for(42, "A:gpt-4o", 0);
    // The model is told what happened, in words, next to the caption.
    let words = last_user(&a);
    assert!(words.contains("is this ok?"), "{words}");
    assert!(words.contains("over the"), "{words}");
    assert!(words.contains("max_file_mb"), "{words}");
    assert!(pixels(&a).is_empty());
    assert!(walk(&dir.path().join("work").join("inbox")).is_empty());
}

#[test]
fn an_album_is_read_once() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let dir = home(&config(&a, &b, &tg, "gpt-4o", "deepseek-chat"));
    let _gw = gateway(dir.path(), &[]);
    tg.add_file("big", &jpeg(40_000));
    tg.add_file("tiny", &jpeg(900));

    for _ in 0..3 {
        tg.say_photo(42, sizes("big"), "", Some("album-1"));
    }
    tg.wait_for(42, "A:gpt-4o", 0);
    tg.wait_for(42, "only looked at the first photo", 0);
    // Let any stray second turn show itself before counting.
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(a.calls().len(), 1, "one photo, one turn: {:?}", a.calls());
    assert_eq!(pixels(&a).len(), 1);
    let said = tg
        .sent
        .lock()
        .unwrap()
        .iter()
        .filter(|m| {
            m["text"]
                .as_str()
                .unwrap_or("")
                .contains("only looked at the first photo")
        })
        .count();
    assert_eq!(said, 1, "one reply per album: {}", tg.all());
}

// ---- From the page -------------------------------------------------------

fn photo_body(mime: &str, bytes: &[u8], text: &str) -> String {
    json!({"text": text, "mime": mime, "data": b64(bytes)}).to_string()
}

fn post_photo(page: &Page, prefix: &str, body: &str, csrf: bool) -> (u16, String) {
    let o = super::dashboard::origin(page.port);
    let path = format!("{prefix}/api/chat/photo");
    let mut headers = vec![
        ("cookie", page.cookie.as_str()),
        ("origin", o.as_str()),
        ("content-type", "application/json"),
    ];
    if csrf {
        headers.push(("x-ferrule-csrf", page.csrf.as_str()));
    }
    let (s, _, b) = http(page.port, "POST", &path, &headers, body);
    (s, b)
}

fn listening(page: &Page) {
    page.until("chat", |v| v["listening"] == true);
}

/// The answer to the chat once an agent bubble containing `needle` shows.
fn agent_says(page: &Page, needle: &str) -> Value {
    page.until("chat", |v| {
        v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["who"] == "agent" && e["text"].as_str().unwrap_or("").contains(needle))
    })
}

fn signed_in(
    config: &str,
    tg: &FakeTelegram,
) -> (tempfile::TempDir, super::dashboard::Running, Page) {
    let dir = home(config);
    let gw = gateway(dir.path(), &[]);
    let (_, page) = sign_in(tg, 0);
    listening(&page);
    (dir, gw, page)
}

#[test]
fn a_page_photo_reaches_the_model() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let (dir, _gw, page) = signed_in(&config(&a, &b, &tg, "gpt-4o", "deepseek-chat"), &tg);
    let bytes = jpeg(30_000);

    let (s, body) = post_photo(
        &page,
        "",
        &photo_body("image/jpeg", &bytes, "read this"),
        true,
    );
    assert_eq!(s, 200, "{body}");
    let chat = agent_says(&page, "A:gpt-4o");
    // The owner's bubble carries the photo's name and never its bytes.
    let mine = chat["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["who"] == "you")
        .unwrap();
    assert!(
        mine["photo"]["name"].as_str().unwrap().ends_with(".jpg"),
        "{mine}"
    );
    assert!(
        !chat.to_string().contains(&b64(&bytes[..64])),
        "no pixels in the log"
    );
    assert_eq!(
        pixels(&a),
        vec![format!("data:image/jpeg;base64,{}", b64(&bytes))]
    );
    assert!(last_user(&a).contains("read this"));
    assert_eq!(walk(&dir.path().join("work").join("inbox")).len(), 1);
}

#[test]
fn a_page_photo_to_a_text_only_model_says_so() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let (_dir, _gw, page) = signed_in(&config(&a, &b, &tg, "deepseek-chat", "gpt-4o"), &tg);

    let (s, body) = post_photo(&page, "", &photo_body("image/png", &png(), ""), true);
    assert_eq!(s, 200, "{body}");
    agent_says(&page, "can't see images");
    agent_says(&page, "A:deepseek-chat");
    assert!(pixels(&a).is_empty());
}

#[test]
fn a_page_photo_is_checked() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let (_dir, _gw, page) = signed_in(&config(&a, &b, &tg, "gpt-4o", "deepseek-chat"), &tg);

    // SVG can carry script: never a photo, whatever it is labelled.
    let svg = b"<svg xmlns='http://www.w3.org/2000/svg'><script>1</script></svg>";
    let (s, _) = post_photo(&page, "", &photo_body("image/svg+xml", svg, ""), true);
    assert_eq!(s, 415);
    let (s, _) = post_photo(&page, "", &photo_body("image/png", svg, ""), true);
    assert_eq!(s, 400, "an SVG labelled png");
    // A PNG labelled jpeg is refused by its bytes.
    let (s, _) = post_photo(&page, "", &photo_body("image/jpeg", &png(), ""), true);
    assert_eq!(s, 400);
    // Not base64 at all.
    let (s, _) = post_photo(
        &page,
        "",
        &json!({"mime": "image/jpeg", "data": "%%%"}).to_string(),
        true,
    );
    assert_eq!(s, 400);
    // Just over the page's 3.5 MB: read whole, then refused for its size.
    let (s, _) = post_photo(
        &page,
        "",
        &photo_body("image/jpeg", &jpeg(3_505_000), ""),
        true,
    );
    assert_eq!(s, 413);
    // No CSRF header: no photo, whatever else is right.
    let (s, _) = post_photo(&page, "", &photo_body("image/jpeg", &jpeg(500), ""), false);
    assert_eq!(s, 403);
    // A GET isn't a way in.
    let (s, _, _) = http(
        page.port,
        "GET",
        "/api/chat/photo",
        &[("cookie", &page.cookie)],
        "",
    );
    assert!(s == 404 || s == 405, "{s}");
    // Nothing above reached the model.
    std::thread::sleep(Duration::from_millis(300));
    no_requests_yet(&a);
}

#[test]
fn a_big_body_without_a_session_is_refused_unread() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let (_dir, _gw, page) = signed_in(&config(&a, &b, &tg, "gpt-4o", "deepseek-chat"), &tg);

    // A 5 MB Content-Length from someone with no session: 413 at once, off
    // the header, without waiting for a body that will never come.
    for (headers, what) in [
        (
            vec![("content-type", "application/json")],
            "no session at all",
        ),
        (
            vec![
                ("cookie", "ferrule_session=nope"),
                ("origin", "http://127.0.0.1"),
                ("content-type", "application/json"),
                ("x-ferrule-csrf", "nope"),
            ],
            "a made-up session",
        ),
    ] {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", page.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut req = format!(
            "POST /api/chat/photo HTTP/1.1\r\nhost: 127.0.0.1:{}\r\nconnection: close\r\ncontent-length: 5000000\r\n",
            page.port
        );
        for (k, v) in &headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        let start = Instant::now();
        s.write_all(req.as_bytes()).unwrap();
        let mut raw = String::new();
        let _ = s.read_to_string(&mut raw);
        assert!(raw.starts_with("HTTP/1.1 413"), "{what}: {raw:?}");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{what}: waited for a body"
        );
    }
    // The page still works.
    assert_eq!(page.get("chat").0, 200);
}
