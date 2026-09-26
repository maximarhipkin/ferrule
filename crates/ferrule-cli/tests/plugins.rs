//! The real `ferrule` binary over an installed WASM plugin (M32): `plugins
//! list` shows it with what it may do, doctor loads it and notices a module
//! swapped after approval, and `plugins remove` takes it out. `plugins add`
//! needs a terminal; without one it refuses and installs nothing.

use ferrule_extensions::{AllowList, ExtensionManager, Layout, ManagerConfig, PluginRequest};
use ferrule_plugins::manifest::sha256_hex;
use ferrule_sandbox::Sandbox;
use serde_json::json;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

fn command(home: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ferrule"));
    cmd.args(args)
        .current_dir(home.join("work"))
        .env("FERRULE_CONFIG", home.join("ferrule.toml"))
        .env("FERRULE_DATA_DIR", home.join("data"))
        .env("FERRULE_TEST_KEY", "sk-test")
        .stdin(Stdio::null());
    for var in [
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "APPDATA",
        "LOCALAPPDATA",
    ] {
        cmd.env(var, home.join("home"));
    }
    cmd
}

fn ferrule(home: &Path, args: &[&str]) -> Output {
    command(home, args).output().unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

const CONFIG: &str = r#"default_provider = "mock"

[providers.mock]
base_url = "http://127.0.0.1:9/v1"
api_key_env = "FERRULE_TEST_KEY"
model = "scripted"

[sandbox]
mode = "off"
"#;

/// A `demo` plugin (the runtime's probe fixture) that may read `docs/`.
fn write_plugin(dir: &Path) {
    let wasm = wat::parse_str(include_str!(
        "../../ferrule-plugins/tests/fixtures/probe.wat"
    ))
    .unwrap();
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("probe.wasm"), &wasm).unwrap();
    let manifest = json!({
        "name": "demo",
        "version": "0.2.0",
        "description": "A test plugin.",
        "wasm": "probe.wasm",
        "sha256": sha256_hex(&wasm),
        "tools": [{"name": "echo", "description": "Echo the arguments back."}],
        "capabilities": {"files": {"read": ["docs"]}},
    });
    std::fs::write(dir.join("plugin.json"), manifest.to_string()).unwrap();
}

#[test]
fn an_installed_plugin_is_listed_checked_by_doctor_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    let home = dunce::canonicalize(dir.path()).unwrap();
    for d in ["work", "data", "home"] {
        std::fs::create_dir_all(home.join(d)).unwrap();
    }
    std::fs::write(home.join("ferrule.toml"), CONFIG).unwrap();
    let src = home.join("src/demo");
    write_plugin(&src);

    // No terminal: refused before anything is fetched.
    let out = ferrule(&home, &["plugins", "add", src.to_str().unwrap()]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("terminal"), "{}", text(&out));

    // Installed as `plugins add` would after a yes.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let m = ExtensionManager::new(ManagerConfig {
            layout: Layout::new(home.join("data")),
            allow: AllowList::default(),
            sandbox: Arc::new(Sandbox::off()),
            workspace: home.join("work"),
            skills: None,
        });
        let req = PluginRequest {
            source: src.to_string_lossy().into_owned(),
            path: None,
            sha256: None,
            replace: false,
            capabilities: None,
        };
        m.install_plugin_as_owner(req, |r| {
            assert_eq!(r.capabilities, ["read files under `docs/`"]);
            true
        })
        .await
        .unwrap();
    });

    let out = ferrule(&home, &["plugins", "list"]);
    let listed = text(&out);
    assert!(out.status.success(), "{listed}");
    assert!(listed.contains("demo 0.2.0 [owner]"), "{listed}");
    assert!(listed.contains("tools: plugin__demo__echo"), "{listed}");
    assert!(listed.contains("may: read files under `docs/`"), "{listed}");

    let doctor = text(&ferrule(&home, &["doctor", "--offline"]));
    assert!(
        doctor.contains("demo 0.2.0: 1 tool(s); may: read files under `docs/`"),
        "{doctor}"
    );

    // The module swapped after approval: doctor says it doesn't load.
    let installed = home.join("data/extensions/plugins/demo/probe.wasm");
    let mut wasm = std::fs::read(&installed).unwrap();
    wasm.push(0);
    std::fs::write(&installed, wasm).unwrap();
    let doctor = text(&ferrule(&home, &["doctor", "--offline"]));
    assert!(doctor.contains("`demo` doesn't load"), "{doctor}");
    assert!(doctor.contains("SHA-256"), "{doctor}");

    let out = ferrule(&home, &["plugins", "remove", "demo"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!home.join("data/extensions/plugins/demo").exists());
    let out = ferrule(&home, &["plugins", "list"]);
    assert!(
        text(&out).contains("no plugins installed"),
        "{}",
        text(&out)
    );
}
