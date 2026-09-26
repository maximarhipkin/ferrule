//! The committed example plugins (`examples/plugins/`) load, pin their
//! bytes and answer. Their `.wasm` files are built by `build.sh`; nothing
//! here builds or downloads anything. The rebuild check and the live GitHub
//! call are `#[ignore]`d — run them with
//! `cargo test -p ferrule-plugins --test examples -- --ignored`
//! (the first needs `rustup target add wasm32-unknown-unknown`, the second
//! reaches api.github.com).

#![cfg(feature = "runtime")]

use ferrule_core::tool::{Tool, ToolContext};
use ferrule_plugins::manifest::sha256_hex;
use ferrule_plugins::{load_dir, tools};
use ferrule_sandbox::Sandbox;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

fn dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/plugins")
        .join(name)
}

fn load(name: &str) -> Vec<Arc<dyn Tool>> {
    let plugin = load_dir(&dir(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    tools(Arc::new(plugin), &Sandbox::off())
}

async fn call(tools: &[Arc<dyn Tool>], name: &str, args: Value) -> Result<String, String> {
    let ws = tempfile::tempdir().unwrap();
    let ctx = ToolContext {
        workspace: ws.path().to_path_buf(),
        max_output_chars: 30_000,
    };
    let tool = tools
        .iter()
        .find(|t| t.definition().name == name)
        .unwrap_or_else(|| panic!("no tool {name}"));
    tool.call(args, &ctx)
        .await
        .map(|o| o.content)
        .map_err(|e| e.to_string())
}

fn manifest_sha(name: &str) -> String {
    let m: Value =
        serde_json::from_str(&std::fs::read_to_string(dir(name).join("plugin.json")).unwrap())
            .unwrap();
    m["sha256"].as_str().unwrap().to_string()
}

#[test]
fn each_manifest_pins_its_committed_module() {
    for (name, wasm) in [
        ("unit-convert", "unit_convert.wasm"),
        ("github-repo", "github_repo.wasm"),
    ] {
        let bytes = std::fs::read(dir(name).join(wasm)).unwrap();
        assert_eq!(sha256_hex(&bytes), manifest_sha(name), "{name}");
    }
}

#[tokio::test]
async fn unit_convert_converts_and_refuses_bad_arguments() {
    let t = load("unit-convert");
    let out = call(
        &t,
        "plugin__unit-convert__convert",
        json!({"value": 100, "from": "c", "to": "f"}),
    )
    .await
    .unwrap();
    assert_eq!(out, "100 c = 212 f");
    let out = call(
        &t,
        "plugin__unit-convert__convert",
        json!({"value": 1, "from": "mi", "to": "km"}),
    )
    .await
    .unwrap();
    assert_eq!(out, "1 mi = 1.609344 km");

    let units: Value = serde_json::from_str(
        &call(&t, "plugin__unit-convert__units", json!({}))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(units["temperature"], json!(["c", "f", "k"]));

    // The plugin's own error comes back as a tool error.
    let err = call(
        &t,
        "plugin__unit-convert__convert",
        json!({"value": 1, "from": "kg", "to": "km"}),
    )
    .await
    .unwrap_err();
    assert!(err.contains("can't convert kg to km"), "{err}");

    // The schema stops a wrong call before the plugin runs.
    let err = call(
        &t,
        "plugin__unit-convert__convert",
        json!({"value": "ten", "from": "c", "to": "f"}),
    )
    .await
    .unwrap_err();
    assert!(err.contains("didn't run"), "{err}");

    // No capabilities: read-only, parallel-eligible, no approval.
    for tool in &t {
        assert!(tool.read_only() && !tool.changes_files() && !tool.needs_approval());
    }
}

#[tokio::test]
async fn github_repo_declares_its_one_domain_and_checks_names_offline() {
    let plugin = load_dir(&dir("github-repo")).unwrap();
    let caps = &plugin.manifest().capabilities;
    assert!(!caps.writes());
    let caps = serde_json::to_value(caps).unwrap();
    assert_eq!(caps["http"]["domains"], json!(["api.github.com"]));
    assert_eq!(caps["secrets"], json!(["GITHUB_TOKEN"]));

    let t = tools(Arc::new(plugin), &Sandbox::off());
    assert!(t[0].read_only());
    // Refused inside the plugin before any request.
    let err = call(
        &t,
        "plugin__github-repo__repo",
        json!({"owner": "../etc", "repo": "x"}),
    )
    .await
    .unwrap_err();
    assert!(err.contains("isn't a GitHub owner"), "{err}");
}

/// `build.sh` reproduces the committed bytes.
#[test]
#[ignore = "builds the examples; needs the wasm32-unknown-unknown target"]
fn build_sh_reproduces_the_committed_modules() {
    let before = [manifest_sha("unit-convert"), manifest_sha("github-repo")];
    let status = std::process::Command::new("sh")
        .arg(dir("build.sh"))
        .status()
        .unwrap();
    assert!(status.success());
    let after = [manifest_sha("unit-convert"), manifest_sha("github-repo")];
    assert_eq!(before, after, "build.sh changed the modules; commit them");
}

/// The real API, without a token (no proxy here, so no placeholder).
#[tokio::test]
#[ignore = "reaches api.github.com"]
async fn github_repo_answers_live() {
    let t = load("github-repo");
    let out = call(
        &t,
        "plugin__github-repo__repo",
        json!({"owner": "rust-lang", "repo": "rust"}),
    )
    .await
    .unwrap();
    assert!(out.contains("rust-lang/rust"), "{out}");
}
