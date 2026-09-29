//! M38: the real `ferrule` binary's `instances list/new/remove` and
//! `--instance`, against a temp home (`FERRULE_ROOT` for the config and data
//! roots, `HOME` for the service units). No service is installed.

use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn ferrule(root: &Path, args: &[&str]) -> Output {
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    Command::new(env!("CARGO_BIN_EXE_ferrule"))
        .args(args)
        .env("FERRULE_ROOT", root)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env_remove("FERRULE_CONFIG")
        .env_remove("FERRULE_DATA_DIR")
        .env_remove("FERRULE_INSTANCE")
        .env("NO_COLOR", "1")
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// `instances list --json`, the ones under the temp root only (a Linux
/// runner's /etc could hold a system one).
fn list(root: &Path) -> Vec<Value> {
    let o = ferrule(root, &["instances", "list", "--json"]);
    assert!(o.status.success(), "{}", text(&o));
    let rows: Vec<Value> = serde_json::from_slice(&o.stdout).unwrap();
    rows.into_iter().filter(|r| r["scope"] == "user").collect()
}

fn names(rows: &[Value]) -> Vec<&str> {
    rows.iter().map(|r| r["name"].as_str().unwrap()).collect()
}

#[test]
fn instances_are_listed_made_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert_eq!(names(&list(root)), Vec::<&str>::new());

    // The default, set up the 0.8.0 way.
    let default = root.join("config/ferrule/config.toml");
    std::fs::create_dir_all(default.parent().unwrap()).unwrap();
    std::fs::write(&default, "# the default\n").unwrap();

    let o = ferrule(root, &["instances", "new", "work", "--user", "--no-setup"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(
        text(&o).contains("ferrule --instance work setup --user"),
        "{}",
        text(&o)
    );
    let work = root.join("config/ferrule-work/config.toml");
    assert!(std::fs::read_to_string(&work)
        .unwrap()
        .starts_with("# ferrule configuration"));

    let rows = list(root);
    assert_eq!(names(&rows), ["default", "work"]);
    assert_eq!(rows[0]["current"], true);
    assert_eq!(rows[1]["current"], false);
    assert_eq!(Path::new(rows[1]["config"].as_str().unwrap()), work);
    assert_eq!(
        Path::new(rows[1]["data"].as_str().unwrap()),
        root.join("data/ferrule-work")
    );
    assert!(!rows[1]["service"].as_str().unwrap().is_empty());
    // The human list marks this process's instance.
    let o = ferrule(root, &["--instance", "work", "instances", "list"]);
    assert!(text(&o).contains("* work"), "{}", text(&o));

    // `--instance` switches every path, and says which instance.
    let o = ferrule(root, &["--instance", "work", "config", "path"]);
    assert!(o.status.success(), "{}", text(&o));
    let out = text(&o);
    assert!(out.contains("instance  work"), "{out}");
    assert!(out.contains("ferrule-work"), "{out}");
    let o = ferrule(root, &["config", "path"]);
    assert!(text(&o).contains("instance  default"), "{}", text(&o));
    // FERRULE_INSTANCE does the same.
    let o = Command::new(env!("CARGO_BIN_EXE_ferrule"))
        .args(["config", "path"])
        .env("FERRULE_ROOT", root)
        .env("FERRULE_INSTANCE", "work")
        .env_remove("FERRULE_CONFIG")
        .env_remove("FERRULE_DATA_DIR")
        .output()
        .unwrap();
    assert!(text(&o).contains("instance  work"), "{}", text(&o));

    // Bad names and taken ones are refused.
    for bad in ["default", "Work", "a_b", "-x"] {
        let o = ferrule(root, &["instances", "new", bad, "--user", "--no-setup"]);
        assert!(!o.status.success(), "{bad}: {}", text(&o));
    }
    let o = ferrule(root, &["--instance", "Bad", "doctor", "--offline"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("lower-case"), "{}", text(&o));
    let o = ferrule(root, &["instances", "new", "work", "--user", "--no-setup"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("already"), "{}", text(&o));
    // A name with no config (a typo) is named, never the default's.
    let o = ferrule(root, &["--instance", "wrok", "doctor", "--offline"]);
    assert!(!o.status.success());
    assert!(
        text(&o).contains("no config for the instance `wrok`")
            && text(&o).contains("ferrule --instance wrok setup"),
        "{}",
        text(&o)
    );

    // Removing keeps its files unless --purge; the default can't be.
    std::fs::create_dir_all(root.join("data/ferrule-work/private")).unwrap();
    let o = ferrule(root, &["instances", "remove", "default"]);
    assert!(!o.status.success());
    let o = ferrule(root, &["instances", "remove", "nope"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("no instance `nope`"), "{}", text(&o));
    let o = ferrule(root, &["instances", "remove", "work"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("--purge"), "{}", text(&o));
    assert!(work.exists());
    assert_eq!(names(&list(root)), ["default", "work"]);

    let o = ferrule(root, &["instances", "remove", "work", "--purge", "--yes"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(!work.exists());
    assert!(!root.join("data/ferrule-work").exists());
    // The default's own files are untouched.
    assert!(default.exists());
    assert_eq!(names(&list(root)), ["default"]);
}

#[test]
fn two_instances_with_one_bot_are_a_doctor_failure() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let config = "[gateway]\ntelegram_token_env = \"TELEGRAM_BOT_TOKEN\"\n\n[gateway.whatsapp]\nphone_number_id = \"1110001\"\ninbound = \"listen\"\n\n[gateway.matrix]\nhomeserver = \"https://m.org\"\naccess_token_env = \"MATRIX_ACCESS_TOKEN\"\nuser = \"@bot:m.org\"\n\n[gateway.email]\naddress = \"Bot@Gmail.com\"\npassword_env = \"EMAIL_PASSWORD\"\n\n[gateway.signal]\naccount = \"+15550000001\"\n\n[gateway.mattermost]\nserver_url = \"https://Chat.example.com/\"\n\n[gateway.http]\nport = 18788\n\n[dashboard]\nport = 18765\n";
    for inst in ["ferrule", "ferrule-work"] {
        let c = root.join("config").join(inst);
        std::fs::create_dir_all(&c).unwrap();
        std::fs::write(c.join("config.toml"), config).unwrap();
        let p = root.join("data").join(inst).join("private");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(
            p.join("secrets.env"),
            "TELEGRAM_BOT_TOKEN=123456:not-a-real-token\nMATTERMOST_TOKEN=mm-not-a-real-token\n",
        )
        .unwrap();
    }
    let o = ferrule(
        root,
        &["--instance", "work", "doctor", "--offline", "--json"],
    );
    let out = String::from_utf8_lossy(&o.stdout);
    let last = out.lines().last().unwrap_or_default();
    let report: Value = serde_json::from_str(last).unwrap_or_else(|e| panic!("{e}: {out}"));
    let lines: Vec<&Value> = report["items"]
        .as_array()
        .unwrap_or_else(|| panic!("{report}"))
        .iter()
        .filter(|i| i["what"] == "instances")
        .collect();
    let texts: Vec<&str> = lines.iter().map(|l| l["text"].as_str().unwrap()).collect();
    assert!(
        lines
            .iter()
            .any(|l| l["level"] == "fail" && l["text"].as_str().unwrap().contains("id 123456")),
        "{texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|t| t.contains("port 18765") && t.contains("`default`")),
        "{texts:?}"
    );
    // M39: one mailbox in two instances.
    assert!(
        lines.iter().any(|l| l["level"] == "fail"
            && l["text"]
                .as_str()
                .unwrap()
                .contains("the same mailbox (bot@gmail.com on imap.gmail.com)")),
        "{texts:?}"
    );
    // M39: one WhatsApp number in two instances.
    assert!(
        lines.iter().any(|l| l["level"] == "fail"
            && l["text"]
                .as_str()
                .unwrap()
                .contains("WhatsApp number (phone number id 1110001)")),
        "{texts:?}"
    );
    assert!(
        lines.iter().any(|l| l["level"] == "fail"
            && l["text"]
                .as_str()
                .unwrap()
                .contains("Matrix bot account (@bot:m.org)")),
        "{texts:?}"
    );
    // M39: one Signal number, and one port for ferrule's own daemon.
    assert!(
        lines.iter().any(|l| l["level"] == "fail"
            && l["text"]
                .as_str()
                .unwrap()
                .contains("the same Signal number (+15550000001)")),
        "{texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|t| t.contains("port 7583 for its own signal-cli daemon")),
        "{texts:?}"
    );
    // M39: one Mattermost bot, known by its token's fingerprint.
    assert!(
        texts
            .iter()
            .any(|t| t.contains("the same Mattermost bot (https://chat.example.com, token ")),
        "{texts:?}"
    );
    // M39: one port for the HTTP API.
    assert!(
        texts
            .iter()
            .any(|t| t.contains("port 18788 for the HTTP API")),
        "{texts:?}"
    );
    // The token itself is never shown.
    assert!(!out.contains("not-a-real-token"), "{out}");
}
