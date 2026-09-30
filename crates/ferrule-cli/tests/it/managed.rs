//! Managed mode: the commands it refuses, what doctor says, and a first
//! start on an empty home.

use std::path::Path;

use serde_json::json;

use super::dashboard::{command, describe, gateway, home, link_in, login, plain};

fn policy(dir: &Path, text: &str) -> String {
    let path = dir.join("policy.toml");
    std::fs::write(&path, text).unwrap();
    path.display().to_string()
}

#[test]
fn managed_mode_refuses_update_setup_ssh_instances_and_the_claude_login() {
    let dir = home("");
    let pol = policy(dir.path(), "reason = \"beta\"\nsandbox = \"container\"\n");
    let env = [
        ("FERRULE_MANAGED", "1"),
        ("FERRULE_POLICY", pol.as_str()),
        ("FERRULE_BOT_ID", "b_test"),
    ];
    let cases: [(&[&str], &str); 5] = [
        (&["setup"], "is off on a managed bot"),
        (&["update", "--check"], "is off on a managed bot"),
        (&["ssh", "list"], "is off on a managed bot"),
        (&["instances", "list"], "is off on a managed bot"),
        (
            &["login", "claude"],
            "The Claude plan isn't available on a hosted bot",
        ),
    ];
    for (args, want) in cases {
        let out = command(dir.path(), args, &env).output().unwrap();
        let text = describe(&out);
        assert!(!out.status.success(), "{args:?}: {text}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(want),
            "{args:?}: {text}"
        );
    }

    // Without the switch the same command is fine.
    let out = command(dir.path(), &["ssh", "list"], &[]).output().unwrap();
    assert!(out.status.success(), "{}", describe(&out));
}

#[test]
fn doctor_shows_managed_mode_and_the_policy() {
    let dir = home("");
    let pol = policy(dir.path(), "reason = \"beta\"\nshell = false\n");
    let env = [("FERRULE_MANAGED", "1"), ("FERRULE_POLICY", pol.as_str())];
    let out = command(dir.path(), &["doctor"], &env).output().unwrap();
    let text = plain(&out.stdout);
    assert!(text.contains("managed mode: on"), "{}", describe(&out));
    assert!(text.contains("the shell is off"), "{}", describe(&out));
    assert!(
        text.contains("no shell: the policy turns it off"),
        "{}",
        describe(&out)
    );
}

#[test]
fn a_managed_first_start_serves_the_dashboard_with_no_model_or_channel() {
    let dir = home("");
    std::fs::remove_file(dir.path().join("ferrule.toml")).unwrap();
    let pol = policy(
        dir.path(),
        "reason = \"beta\"\nsandbox = \"container\"\nproviders = [\"openai\"]\n",
    );
    let _gw = gateway(
        dir.path(),
        &[
            ("FERRULE_MANAGED", "1"),
            ("FERRULE_POLICY", pol.as_str()),
            ("FERRULE_BOT_ID", "b_test"),
        ],
    );
    let env = [
        ("FERRULE_MANAGED", "1"),
        ("FERRULE_POLICY", pol.as_str()),
        ("FERRULE_BOT_ID", "b_test"),
    ];
    // `restarted_port` runs `dashboard link` without the env, so the wait
    // is done here with it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let link = loop {
        let out = command(dir.path(), &["dashboard", "link"], &env)
            .output()
            .unwrap();
        let text = plain(&out.stdout);
        if out.status.success() && text.contains("/login#") {
            break text;
        }
        assert!(std::time::Instant::now() < deadline, "{}", describe(&out));
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    let toml = std::fs::read_to_string(dir.path().join("ferrule.toml")).unwrap();
    assert!(
        toml.starts_with("# Written by ferrule on a managed bot's first start"),
        "{toml}"
    );
    let (port, token) = link_in(&link);
    let page = login(port, &token).expect("the link signs in");
    let m = page.read("managed");
    assert_eq!(m["on"], true, "{m}");
    assert_eq!(m["bot_id"], "b_test", "{m}");
    assert_eq!(m["reason"], "beta", "{m}");
    assert!(
        m["claude_plan"]
            .as_str()
            .unwrap()
            .starts_with("The Claude plan isn't available"),
        "{m}"
    );
    let (s, v) = page.post("plans/claude", json!({ "token": "sk-ant-oat01-x" }));
    assert_eq!(s, 403, "{v}");
}
