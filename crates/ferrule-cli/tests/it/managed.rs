//! Managed mode: the commands it refuses, what doctor says, and a first
//! start on an empty home.

use std::path::Path;

use serde_json::json;

use super::dashboard::{command, describe, gateway, home, http, http_as, link_in, login, plain};

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

const SECRET: &str = "test-panel-secret-0123456789abcdef0123";

/// A panel token as the panel would sign it.
fn panel_token(bot: &str, exp_from_now: u64, nonce: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
    use base64::Engine;
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + exp_from_now;
    let claims = json!({ "bot": bot, "user": "u_1", "exp": exp, "nonce": nonce });
    let body = format!("ferrule-panel.v1.{}", B64.encode(claims.to_string()));
    let mac = ring::hmac::sign(
        &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, SECRET.as_bytes()),
        body.as_bytes(),
    );
    format!("{body}.{}", B64.encode(mac.as_ref()))
}

#[test]
fn a_panel_sign_in_works_under_the_prefix_through_the_gateway() {
    let dir = home("");
    std::fs::remove_file(dir.path().join("ferrule.toml")).unwrap();
    let pol = policy(dir.path(), "reason = \"beta\"\nsandbox = \"container\"\n");
    let _gw = gateway(
        dir.path(),
        &[
            ("FERRULE_MANAGED", "1"),
            ("FERRULE_POLICY", pol.as_str()),
            ("FERRULE_BOT_ID", "b_test"),
            ("FERRULE_PUBLIC_URL", "http://bots.test/b/b_test/"),
            ("FERRULE_PANEL_SECRET", SECRET),
        ],
    );
    // Wait for the page (`dashboard link` reads the gateway's marker).
    let port = {
        let env = [("FERRULE_MANAGED", "1"), ("FERRULE_POLICY", pol.as_str())];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let out = command(dir.path(), &["dashboard", "link"], &env)
                .output()
                .unwrap();
            let text = plain(&out.stdout);
            if out.status.success() && text.contains("/login#") {
                // With no FERRULE_PUBLIC_URL here, the link is on loopback.
                break link_in(&text).0;
            }
            assert!(std::time::Instant::now() < deadline, "{}", describe(&out));
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    };
    let token = panel_token("b_test", 60, "0123456789abcdef");
    let login = |token: &str| {
        http_as(
            port,
            "bots.test",
            "POST",
            "/b/b_test/api/login",
            &[
                ("origin", "http://bots.test"),
                ("content-type", "application/json"),
            ],
            &json!({ "token": token }).to_string(),
        )
    };
    let (status, headers, body) = login(&token);
    assert_eq!(status, 200, "{body}");
    let cookie = headers["set-cookie"].split(';').next().unwrap().to_string();
    let (status, _, body) = http_as(
        port,
        "bots.test",
        "GET",
        "/b/b_test/api/session",
        &[("cookie", &cookie)],
        "",
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["user"],
        "u_1"
    );
    // The same token again.
    assert_eq!(login(&token).0, 401);
    // The health-only guard is off: the loopback host still answers.
    assert_eq!(http(port, "GET", "/", &[], "").0, 200);
}

/// The direct children of `pid`, from /proc.
#[cfg(target_os = "linux")]
fn children_of(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(child) = e.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
            continue;
        };
        // "pid (comm) S ppid …": comm may hold spaces, so count from the last ')'.
        let ppid = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|p| p.parse::<u32>().ok());
        if ppid == Some(pid) {
            out.push(child);
        }
    }
    out
}

#[cfg(target_os = "linux")]
#[test]
fn the_panel_secret_is_not_passed_to_children() {
    let config = "[[mcp.servers]]\nname = \"sleeper\"\ncommand = \"sleep\"\nargs = [\"30\"]\nsandbox = false\n";
    let dir = home(config);
    let pol = policy(dir.path(), "reason = \"beta\"\nsandbox = \"container\"\n");
    let gw = gateway(
        dir.path(),
        &[
            ("FERRULE_MANAGED", "1"),
            ("FERRULE_POLICY", pol.as_str()),
            ("FERRULE_BOT_ID", "b_test"),
            ("FERRULE_PANEL_SECRET", SECRET),
        ],
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        for child in children_of(gw.0.id()) {
            let Ok(env) = std::fs::read(format!("/proc/{child}/environ")) else {
                continue;
            };
            let env = String::from_utf8_lossy(&env).into_owned();
            if !env.contains("FERRULE_BOT_ID=") {
                continue;
            }
            assert!(!env.contains("FERRULE_PANEL_SECRET"), "{env}");
            assert!(!env.contains(SECRET), "{env}");
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no child of the gateway started"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}
