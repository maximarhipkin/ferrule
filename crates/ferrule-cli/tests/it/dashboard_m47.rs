//! M47, the dashboard redesign, through the real `ferrule` binary: the
//! first-run checklist follows the bot's real state. Later parts add their
//! own tests here (docs/m47-dashboard-redesign.md).

use super::dashboard::{gateway, home, sign_in, telegram, two, FakeTelegram, Server};
use serde_json::Value;

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
