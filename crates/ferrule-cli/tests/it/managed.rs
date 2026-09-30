//! Managed mode: the commands it refuses, what doctor says, and a first
//! start on an empty home.

use std::path::Path;

use serde_json::json;

use super::channels::model_server;
use super::dashboard::{
    command, describe, gateway, home, http, http_as, link_in, login, plain, FakeTelegram,
};

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

/// `dashboard link` with the managed env, once the gateway is up.
/// (`restarted_port` runs it without the env.)
fn managed_link(dir: &Path, env: &[(&str, &str)]) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let out = command(dir, &["dashboard", "link"], env).output().unwrap();
        let text = plain(&out.stdout);
        if out.status.success() && text.contains("/login#") {
            return text;
        }
        assert!(std::time::Instant::now() < deadline, "{}", describe(&out));
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
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
    let link = managed_link(dir.path(), &env);
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

#[test]
fn a_telegram_token_is_tested_saved_and_a_chat_allowed() {
    let (model, _) = model_server();
    let tg = FakeTelegram::start();
    let dir = home(&format!(
        "default_provider = \"mock\"\n\n[providers.mock]\nbase_url = \"{model}\"\napi_key_env = \"FERRULE_TEST_KEY\"\nmodel = \"scripted\"\n\n[skills]\nenabled = false\n\n[gateway]\ntelegram_base_url = \"{}\"\n\n[dashboard]\nremote = \"off\"\n",
        tg.url
    ));
    let pol = policy(dir.path(), "reason = \"beta\"\nsandbox = \"container\"\n");
    let health_port = free_port().to_string();
    let env = [
        ("FERRULE_MANAGED", "1"),
        ("FERRULE_POLICY", pol.as_str()),
        ("FERRULE_BOT_ID", "b_test"),
        ("FERRULE_DASHBOARD_PORT", health_port.as_str()),
    ];
    let _gw = gateway(dir.path(), &env);
    let (port, token) = link_in(&managed_link(dir.path(), &env));
    let page = login(port, &token).expect("the link signs in");

    let good = "123456:TESTtokenTESTtokenTEST00";
    let (s, v) = page.post("telegram/test", json!({ "token": good }));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["name"], "m44_bot", "{v}");

    // A token that isn't one is refused before anything is called.
    let (s, v) = page.post("telegram/test", json!({ "token": "nope" }));
    assert_eq!(s, 400, "{v}");
    let (s, _) = page.post("telegram/save", json!({ "token": "" }));
    assert_eq!(s, 400);

    let (s, v) = page.post("telegram/save", json!({ "token": good }));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["restart"], true, "{v}");
    let toml = std::fs::read_to_string(dir.path().join("ferrule.toml")).unwrap();
    assert!(
        toml.contains("telegram_token_env = \"TELEGRAM_BOT_TOKEN\""),
        "{toml}"
    );
    let secrets = std::fs::read_to_string(dir.path().join("data/private/secrets.env")).unwrap();
    assert!(
        secrets.contains(&format!("TELEGRAM_BOT_TOKEN={good}")),
        "{secrets}"
    );
    assert!(!toml.contains(good), "{toml}");

    tg.say(4242, "hello bot");
    let (s, v) = page.post("telegram/wait", json!({ "token": good }));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["chats"][0]["id"], 4242, "{v}");
    let next = v["next"].clone();
    assert!(next.is_number(), "{v}");

    let (s, v) = page.post(
        "telegram/allow",
        json!({ "token": good, "chat": 4242, "next": next }),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["restart"], true, "{v}");
    let toml = std::fs::read_to_string(dir.path().join("ferrule.toml")).unwrap();
    assert!(toml.contains("telegram_allowed_chats = [4242]"), "{toml}");
    let sent = tg.all();
    assert!(
        sent.contains("\"chat_id\":4242") && sent.contains("\u{2705} Connected"),
        "{sent}"
    );

    // Allowing it again changes nothing.
    let (s, _) = page.post("telegram/allow", json!({ "token": good, "chat": 4242 }));
    assert_eq!(s, 200);
    let again = std::fs::read_to_string(dir.path().join("ferrule.toml")).unwrap();
    assert_eq!(again.matches("4242").count(), 1, "{again}");

    // The page's restart runs the bot again in this process, and the token
    // it saved reaches the channel: chat 4242 is answered, with no terminal.
    #[cfg(target_os = "linux")]
    {
        let (s, v) = page.post("gateway/restart", json!({ "confirm": true }));
        assert_eq!(s, 200, "{v}");
        let health_port: u16 = health_port.parse().unwrap();
        // Down first (the old dashboard is gone), then up.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while try_health(health_port).is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "the bot never stopped"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        wait_health(health_port, |_| true);
        tg.say(4242, "hi again");
        let (_, text) = tg.wait_for(4242, "ECHO", 0);
        assert!(text.contains("hi again"), "{text}");
    }
}

/// A port nothing listens on right now.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// `/healthz` on `port` (raw, no panic when nothing answers).
fn try_health(port: u16) -> Option<serde_json::Value> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect_timeout(
        &([127, 0, 0, 1], port).into(),
        std::time::Duration::from_secs(2),
    )
    .ok()?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok()?;
    write!(
        s,
        "GET /healthz HTTP/1.1\r\nhost: x.test\r\nconnection: close\r\n\r\n"
    )
    .ok()?;
    let mut raw = String::new();
    s.read_to_string(&mut raw).ok()?;
    serde_json::from_str(raw.split_once("\r\n\r\n")?.1).ok()
}

fn wait_health(port: u16, ok: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
    loop {
        if let Some(v) = try_health(port) {
            if ok(&v) {
                return v;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "/healthz on {port} never got there"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[test]
fn health_and_busy_answer_on_the_bind_address() {
    let dir = home("");
    std::fs::remove_file(dir.path().join("ferrule.toml")).unwrap();
    let pol = policy(dir.path(), "reason = \"beta\"\nsandbox = \"container\"\n");
    let port = free_port().to_string();
    let _gw = gateway(
        dir.path(),
        &[
            ("FERRULE_MANAGED", "1"),
            ("FERRULE_POLICY", pol.as_str()),
            ("FERRULE_BOT_ID", "b_test"),
            ("FERRULE_PUBLIC_URL", "http://bots.test/b/b_test/"),
            ("FERRULE_DASHBOARD_BIND", "127.0.0.1"),
            ("FERRULE_DASHBOARD_PORT", port.as_str()),
        ],
    );
    let port: u16 = port.parse().unwrap();
    let v = wait_health(port, |_| true);
    assert_eq!(v["managed"], true, "{v}");
    assert_eq!(v["status"], "degraded", "{v}");
    assert!(
        v["reasons"][0].as_str().unwrap().contains("no model"),
        "{v}"
    );

    let (s, headers, body) = http_as(port, "anything.test", "GET", "/healthz", &[], "");
    assert_eq!(s, 200, "{body}");
    assert_eq!(headers["cache-control"], "no-store");
    let (s, _, body) = http_as(port, "bots.test", "GET", "/b/b_test/healthz", &[], "");
    assert_eq!(s, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["managed"], true, "{v}");
    let (s, _, body) = http_as(port, "bots.test", "GET", "/busyz", &[], "");
    assert_eq!(s, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["busy"],
        false
    );
    assert_eq!(
        http_as(port, "bots.test", "POST", "/healthz", &[], "").0,
        405
    );
}

#[cfg(unix)]
#[test]
fn sigterm_drains_and_exits_zero() {
    let dir = home("");
    std::fs::remove_file(dir.path().join("ferrule.toml")).unwrap();
    let pol = policy(dir.path(), "reason = \"beta\"\nsandbox = \"container\"\n");
    let port = free_port().to_string();
    let env = [
        ("FERRULE_MANAGED", "1"),
        ("FERRULE_POLICY", pol.as_str()),
        ("FERRULE_BOT_ID", "b_test"),
        ("FERRULE_DASHBOARD_PORT", port.as_str()),
        ("RUST_LOG", "info"),
    ];
    let log = dir.path().join("gateway.log");
    let mut cmd = command(dir.path(), &["gateway"], &env);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap());
    let mut child = cmd.spawn().unwrap();
    wait_health(port.parse().unwrap(), |_| true);
    let marker = dir.path().join("data/gateway/dashboard.json");
    assert!(marker.exists());
    let pid = child.id().to_string();
    let killed = std::process::Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap();
    assert!(killed.success());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!(
                "still running after SIGTERM:\n{}",
                plain(&std::fs::read(&log).unwrap())
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let text = plain(&std::fs::read(&log).unwrap());
    assert_eq!(status.code(), Some(0), "{text}");
    assert!(text.contains("SIGTERM: shutting down"), "{text}");
    assert!(text.contains("stopping:"), "{text}");
    assert!(!marker.exists(), "the marker stays");
}

#[cfg(target_os = "linux")]
#[test]
fn a_managed_restart_runs_again_in_the_same_process() {
    let dir = home("");
    std::fs::remove_file(dir.path().join("ferrule.toml")).unwrap();
    let pol = policy(dir.path(), "reason = \"beta\"\nsandbox = \"container\"\n");
    let port = free_port().to_string();
    let env = [
        ("FERRULE_MANAGED", "1"),
        ("FERRULE_POLICY", pol.as_str()),
        ("FERRULE_BOT_ID", "b_test"),
        ("FERRULE_DASHBOARD_PORT", port.as_str()),
        ("RUST_LOG", "info"),
    ];
    let log = dir.path().join("gateway.log");
    let mut cmd = command(dir.path(), &["gateway"], &env);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap());
    let mut child = cmd.spawn().unwrap();
    let port: u16 = port.parse().unwrap();
    let before = wait_health(port, |v| v["uptime_secs"].as_u64().unwrap_or(0) >= 3);
    let up = before["uptime_secs"].as_u64().unwrap();

    let (link_port, token) = link_in(&managed_link(dir.path(), &env[..3]));
    let page = login(link_port, &token).expect("the link signs in");
    let (s, v) = page.post("gateway/restart", json!({}));
    assert_eq!(s, 409, "{v}");
    assert!(
        v["confirm"]
            .as_str()
            .unwrap()
            .starts_with("Restart the bot?"),
        "{v}"
    );
    let (s, v) = page.post("gateway/restart", json!({ "confirm": true }));
    assert_eq!(s, 200, "{v}");

    // Down for a moment, then up again with a small uptime.
    let after = wait_health(port, |v| v["uptime_secs"].as_u64().unwrap_or(99) < up);
    assert_eq!(after["managed"], true, "{after}");
    // The same process: it never exited.
    assert!(child.try_wait().unwrap().is_none(), "the process exited");
    let text = plain(&std::fs::read(&log).unwrap());
    assert_eq!(text.matches("gateway starting").count(), 2, "{text}");
    assert!(text.contains("stopping:"), "{text}");
    assert!(!text.contains("the restart couldn't start"), "{text}");
    let _ = child.kill();
    let _ = child.wait();
}
