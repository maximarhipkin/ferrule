//! M66: the owner's commands in the dashboard's own chat, through the
//! real `ferrule` binary: `/plan` and `/goal` sent with `chat/send` reach
//! their doors (never the agent as words), a plan's question is asked in
//! the page's chat and answered there, and each one's report comes back.

use super::dashboard::{gateway, home, sign_in, telegram, two, FakeTelegram, Page, Server};
use serde_json::{json, Value};

/// The page chat's agent entries, in order.
fn said(chat: &Value) -> Vec<String> {
    chat["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["who"] == "agent")
        .map(|e| e["text"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Waits for an agent entry containing `needle`, and returns it whole.
fn wait_for(page: &Page, needle: &str) -> Value {
    let chat = page.until("chat", |c| said(c).iter().any(|t| t.contains(needle)));
    chat["entries"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|e| e["who"] == "agent" && e["text"].as_str().unwrap_or("").contains(needle))
        .unwrap()
        .clone()
}

#[test]
fn plan_and_goal_from_the_page_chat_reach_their_doors_and_answer_there() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let extra = format!("{}\n[agent]\nverify_command = [\"true\"]\n", telegram(&tg));
    let dir = home(&two(&a, &b, "", &extra));
    let _gw = gateway(dir.path(), &[]);
    let (_, page) = sign_in(&tg, 0);

    // The commands the page's chat answers, from the one menu.
    let cmds: Vec<String> = page.read("commands")["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["cmd"].as_str().unwrap().to_string())
        .collect();
    for c in [
        "/plan", "/goal", "/model", "/undo", "/caps", "/stop", "/new",
    ] {
        assert!(cmds.iter().any(|x| x == c), "{c}: {cmds:?}");
    }
    assert!(!cmds.iter().any(|x| x == "/dashboard" || x == "/connect"));

    // `/plan`: the door acknowledges it, the question is asked here, the
    // page's Allow (its button sends `yes <code>`) runs it, and the plan's
    // answer comes back here.
    let (s, r) = page.post("chat/send", json!({ "text": "/plan tidy the docs folder" }));
    assert_eq!(s, 200, "{r}");
    wait_for(&page, "Planning (read-only");
    let asked = wait_for(&page, "tidy the docs folder");
    let button = asked["buttons"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["text"] == "Allow")
        .unwrap_or_else(|| panic!("an Allow button: {asked:#}"))["send"]
        .as_str()
        .unwrap()
        .to_string();
    let inbox = page.read("approvals");
    assert_eq!(
        inbox["approvals"][0]["chat"], "dashboard:owner",
        "{inbox:#}"
    );
    let plans = page.read("plans");
    assert_eq!(plans["plans"][0]["waiting"], true, "{plans:#}");
    let (s, r) = page.post("chat/send", json!({ "text": button }));
    assert_eq!(s, 200, "{r}");
    wait_for(&page, "approved; running it");
    let done = page.until("plans", |p| p["plans"][0]["status"] == "executed");
    assert_eq!(done["plans"][0]["waiting"], false);

    // `/goal`: the door starts the loop, and its report comes back here.
    let (s, r) = page.post("chat/send", json!({ "text": "/goal the docs are tidy" }));
    assert_eq!(s, 200, "{r}");
    wait_for(&page, "Goal loop started: the docs are tidy");
    wait_for(&page, "Goal met: the docs are tidy");

    // The model never read either command as words.
    for body in a.bodies().iter().chain(b.bodies().iter()) {
        let text = body.to_string();
        assert!(
            !text.contains("/plan tidy") && !text.contains("/goal the docs"),
            "a command reached the model: {text}"
        );
    }
}
