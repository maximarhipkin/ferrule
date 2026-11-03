//! The learning page through the real `ferrule` binary
//! (`docs/m16-learning-loop.md`, "The learning page"): its view, its two
//! switches written to the config (the nightly task following at once), a
//! dry run, the owner's 👎 on a page-chat reply kept as a signal and an
//! episode with learning off, and a proposed change kept, undone, and
//! another dropped.

use super::dashboard::{ferrule, gateway, home, sign_in, telegram, two, FakeTelegram, Server};
use serde_json::{json, Value};
use std::path::Path;

/// A finished pass with two proposals the gate couldn't prove.
fn seed_pass(data: &Path, id: &str) {
    let lesson = |k: usize, text: &str| {
        json!({
            "id": format!("p{k}"),
            "change": { "what": "lesson", "op": "add", "text": text },
            "reason": "it kept asking", "episode": "reaction:telegram__42:7",
            "gate": "no check to gate on", "status": "proposed",
        })
    };
    let pass = json!({
        "id": id, "status": "done", "trigger": "manual",
        "started_at": "2026-11-01T03:00:00Z", "finished_at": "2026-11-01T03:01:00Z",
        "caps": { "usd_per_pass": 0.5, "usd_per_day": 1.0, "tokens_per_pass": 1, "tokens_per_day": 1 },
        "spent": { "usd": 0.01, "tokens": 10, "calls": 1 },
        "day_spent_before": { "usd": 0.0, "tokens": 0, "calls": 0 },
        "cursor_before": 0, "cursor_after": 0,
        "proposed": [
            lesson(1, "Say which file you changed when you finish."),
            lesson(2, "Answer in the language the owner wrote in."),
        ],
    });
    let dir = data.join("learn/passes").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("pass.json"), pass.to_string()).unwrap();
}

fn change<'a>(view: &'a Value, id: &str) -> &'a Value {
    view["passes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["changes"].as_array().unwrap())
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no change {id}: {view:#}"))
}

#[test]
fn the_learning_page_reads_switches_keeps_feedback_and_lands_a_proposal() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let dir = home(&two(&a, &b, "", &telegram(&tg)));
    let data = dir.path().join("data");
    let pass = "20261101-030000-ab12";
    seed_pass(&data, pass);
    let _gw = gateway(dir.path(), &[]);
    let (_, page) = sign_in(&tg, 0);

    // Off by default, failure notes on; the seeded pass's proposals show.
    let v = page.read("learn");
    assert_eq!(v["enabled"], false, "{v:#}");
    assert_eq!(v["failure_notes"], true);
    assert_eq!(v["needs_restart"], false);
    assert_eq!(v["schedule"], "every night at 03:00 (UTC)");
    assert_eq!(v["caps"]["usd_per_pass"], 0.5);
    assert_eq!(v["last_pass"]["proposed"], 2, "{v:#}");
    let p1 = change(&v, &format!("{pass}:p1"));
    assert_eq!(
        (p1["kind"].as_str(), p1["outcome"].as_str()),
        (Some("lesson_add"), Some("proposed"))
    );

    // On: written to the file, the nightly task made at once; Telegram's
    // thumbs follow from the next start.
    let (s, r) = page.post("learn/settings", json!({ "enabled": true }));
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["ok"], true);
    assert_eq!(r["needs_restart"], true, "{r}");
    assert!(
        r["said"].as_str().unwrap().contains("Learning is on"),
        "{r}"
    );
    let file = std::fs::read_to_string(dir.path().join("ferrule.toml")).unwrap();
    assert!(
        file.contains("[learning]") && file.contains("enabled = true"),
        "{file}"
    );
    let tasks = ferrule(dir.path(), &["tasks", "list"]);
    assert!(
        String::from_utf8_lossy(&tasks.stdout).contains("ferrule-learn"),
        "{}",
        super::dashboard::describe(&tasks)
    );
    let (s, r) = page.post(
        "learn/settings",
        json!({ "enabled": false, "failure_notes": false }),
    );
    assert_eq!((s, &r["needs_restart"]), (200, &json!(false)), "{r}");
    assert_eq!(page.read("learn")["failure_notes"], false);
    let (s, r) = page.post("learn/settings", json!({ "enabled": "yes" }));
    assert_eq!(s, 400, "{r}");

    // A dry run spends nothing.
    let (s, r) = page.post("learn/run", json!({ "dry_run": true }));
    assert_eq!(s, 200, "{r}");
    assert!(r["said"].as_str().unwrap().starts_with("Dry run"), "{r}");
    assert!(r["id"].is_null());

    // A 👎 on the page chat's reply, with learning off: a signal and an
    // episode for the next pass.
    let (s, r) = page.post("chat/send", json!({ "text": "what's the plan for today?" }));
    assert_eq!(s, 200, "{r}");
    let chat = page.until("chat", |c| {
        c["entries"]
            .as_array()
            .is_some_and(|e| e.iter().any(|x| x["who"] == "agent"))
            && c["waiting"] == false
    });
    let reply = chat["entries"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|x| x["who"] == "agent")
        .unwrap()["id"]
        .clone();
    let (s, r) = page.post(
        "chat/react",
        json!({ "id": reply, "thumb": "down", "why": "too vague" }),
    );
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["said"], "Thanks — noted.");
    let (s, _) = page.post("chat/react", json!({ "id": 99999, "thumb": "up" }));
    assert_eq!(s, 404);
    // A "why" that holds a key is refused before anything is kept.
    let token = "ghp_a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8";
    let (s, r) = page.post(
        "chat/react",
        json!({ "id": reply, "thumb": "down", "why": format!("you printed {token}") }),
    );
    assert_eq!(s, 400, "{r}");
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .starts_with("That looked like an API key"),
        "{r}"
    );
    let signals = std::fs::read_to_string(data.join("learn/signals.jsonl")).unwrap();
    assert!(signals.contains("\"kind\":\"down\"") && signals.contains("too vague"));
    assert!(!signals.contains(token), "{signals}");
    assert_eq!(signals.lines().count(), 1, "{signals}");
    assert!(!signals.contains("A:a-one"), "no reply text: {signals}");
    let fb = &page.read("learn")["feedback"];
    assert_eq!(
        (fb["down_7d"].as_u64(), fb["up_7d"].as_u64()),
        (Some(1), Some(0))
    );
    let plan = ferrule(dir.path(), &["learn", "run", "--dry-run"]);
    let said = String::from_utf8_lossy(&plan.stdout);
    assert!(said.contains("dashboard__owner"), "{said}");

    // Keep lands a proposal as a numbered change of its pass; its Undo is
    // the usual one. Drop never lands one.
    let (s, r) = page.post("learn/keep", json!({ "id": format!("{pass}:p1") }));
    assert_eq!(s, 200, "{r}");
    assert_eq!(
        r["said"],
        "Learned: Say which file you changed when you finish."
    );
    assert_eq!(r["change"], format!("{pass}:1"));
    let v = page.read("learn");
    let lesson = &v["lessons"][0];
    assert_eq!(lesson["id"], "pb-1", "{v:#}");
    assert_eq!(lesson["uses"]["sessions"], 0);
    assert_eq!(change(&v, &format!("{pass}:1"))["outcome"], "kept");
    let (s, r) = page.post("learn/keep", json!({ "id": format!("{pass}:p1") }));
    assert_eq!(s, 409, "kept already: {r}");
    let (s, r) = page.post("learn/drop", json!({ "id": format!("{pass}:p2") }));
    assert_eq!(s, 200, "{r}");
    let (s, r) = page.post("learn/revert", json!({ "id": format!("{pass}:1") }));
    assert_eq!(s, 200, "{r}");
    let v = page.read("learn");
    assert_eq!(change(&v, &format!("{pass}:1"))["outcome"], "reverted");
    assert_eq!(change(&v, &format!("{pass}:p2"))["outcome"], "rejected");
    assert!(v["lessons"].as_array().unwrap().is_empty(), "{v:#}");
    let (s, _) = page.post("learn/revert", json!({ "id": format!("{pass}:p2") }));
    assert_eq!(s, 400);
}
