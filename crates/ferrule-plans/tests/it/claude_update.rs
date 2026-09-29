//! M36 §5: keeping `claude` current, against the stand-in `claude`
//! (`src/bin/fake-claude.rs`), never the real one: its install method is
//! told by where it is, its own updater runs with no input, and a turn that
//! failed on an outdated claude is updated and tried once more.

use async_trait::async_trait;
use ferrule_core::failure::{classify, Kind};
use ferrule_core::message::Message;
use ferrule_core::provider::{CompletionRequest, Provider};
use ferrule_plans::claude::update::{self, Method};
use ferrule_plans::claude::{ClaudeCode, EngineConfig, Repairer};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const FAKE: &str = env!("CARGO_BIN_EXE_ferrule-fake-claude");

/// A copy of the fake where the native installer puts claude, so its
/// version file is this test's own.
fn native_install(root: &Path) -> PathBuf {
    let dir = root.join(".local/share/claude/versions/2.1.283");
    std::fs::create_dir_all(&dir).unwrap();
    let claude = dir.join(format!("claude{}", std::env::consts::EXE_SUFFIX));
    copy_fake(&claude);
    claude
}

/// Copies the fake and waits until it runs: a test forking at the moment
/// of the copy holds the file open for writing ("text file busy").
fn copy_fake(to: &Path) {
    std::fs::copy(FAKE, to).unwrap();
    for _ in 0..100 {
        let run = std::process::Command::new(to)
            .arg("--version")
            .env("CLAUDE_CONFIG_DIR", to.with_extension("cfg"))
            .output();
        match run {
            Err(e) if e.raw_os_error() == Some(26) => std::thread::sleep(Duration::from_millis(20)),
            _ => return,
        }
    }
}

fn seen(claude: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(claude.parent().unwrap().join("fake-update.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

#[test]
fn a_native_install_updates_itself_with_no_input_and_no_autoupdater_block() {
    let tmp = tempfile::tempdir().unwrap();
    let claude = native_install(tmp.path());
    let install = update::detect(&claude).unwrap();
    assert_eq!(install.method, Method::Native);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let uid = std::fs::metadata(&claude).unwrap().uid();
        assert_eq!(install.owner.0, uid);
        assert_eq!(update::blocked(&install), None, "our own files");
    }
    let config = tmp.path().join("config");
    let done = update::update(&install, &config, Duration::from_secs(60)).unwrap();
    assert_eq!(
        done,
        update::Updated {
            from: "2.1.283".into(),
            to: "2.2.0".into()
        }
    );
    let seen = seen(&claude);
    assert_eq!(seen["autoupdater_off"], serde_json::Value::Null);
    assert_eq!(seen["stdin_closed"], true);
}

#[test]
fn a_failed_update_says_why() {
    let tmp = tempfile::tempdir().unwrap();
    let claude = native_install(tmp.path());
    std::fs::write(claude.parent().unwrap().join("fake-update-fails"), "").unwrap();
    let install = update::detect(&claude).unwrap();
    let e = update::update(&install, tmp.path(), Duration::from_secs(60)).unwrap_err();
    let text = format!("{e:#}");
    assert!(
        // `claude.exe update failed` on Windows.
        text.contains(" update failed") && text.contains("EACCES"),
        "{text}"
    );
}

#[test]
fn an_install_ferrule_cant_update_names_the_command() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("opt/tools");
    std::fs::create_dir_all(&dir).unwrap();
    let claude = dir.join(format!("claude{}", std::env::consts::EXE_SUFFIX));
    copy_fake(&claude);
    let install = update::detect(&claude).unwrap();
    assert_eq!(install.method, Method::Unknown);
    let e = update::update(&install, tmp.path(), Duration::from_secs(60)).unwrap_err();
    assert!(format!("{e}").contains("the way you installed it"), "{e}");
    assert!(!claude.parent().unwrap().join("fake-update.json").exists());
}

/// A repairer that runs the update right here and counts.
#[derive(Debug)]
struct Here {
    claude: PathBuf,
    config: PathBuf,
    runs: AtomicUsize,
    works: bool,
}

#[async_trait]
impl Repairer for Here {
    async fn update_claude(&self, why: &str) -> anyhow::Result<()> {
        assert!(why.contains("needs an update"), "{why}");
        self.runs.fetch_add(1, Ordering::SeqCst);
        if !self.works {
            anyhow::bail!("no way to update it here");
        }
        update::update(
            &update::detect(&self.claude)?,
            &self.config,
            Duration::from_secs(60),
        )?;
        Ok(())
    }
}

fn engine(claude: &Path, root: &Path, repair: Option<Arc<Here>>) -> ClaudeCode {
    let workspace = root.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut cfg = EngineConfig::new(claude.to_path_buf(), root.join("claude-code"), workspace);
    cfg.private_dir = Some(root.join("private"));
    cfg.repair = repair.map(|r| r as Arc<dyn Repairer>);
    ClaudeCode::new("claude-code", "haiku", cfg)
}

fn ask(text: &str) -> CompletionRequest {
    CompletionRequest {
        messages: vec![Message::user(text)],
        tools: vec![],
        max_output_tokens: None,
        temperature: None,
        stream: None,
    }
}

#[tokio::test]
async fn an_outdated_claude_is_updated_and_the_turn_tried_once_more() {
    let tmp = tempfile::tempdir().unwrap();
    let claude = native_install(tmp.path());
    let here = Arc::new(Here {
        claude: claude.clone(),
        config: tmp.path().join("claude-code"),
        runs: AtomicUsize::new(0),
        works: true,
    });
    let engine = engine(&claude, tmp.path(), Some(here.clone()));
    let r = engine.complete(ask("[needs-update] hi")).await.unwrap();
    assert!(
        r.message
            .content
            .unwrap()
            .contains("you said [needs-update] hi"),
        "the retry answered"
    );
    assert_eq!(here.runs.load(Ordering::SeqCst), 1);
    // Current now: the next turn needs nothing.
    engine.complete(ask("[needs-update] again")).await.unwrap();
    assert_eq!(here.runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn without_a_way_to_update_the_turn_fails_as_claude_too_old() {
    let tmp = tempfile::tempdir().unwrap();
    let claude = native_install(tmp.path());
    let e = engine(&claude, tmp.path(), None)
        .complete(ask("[needs-update] hi"))
        .await
        .unwrap_err();
    assert_eq!(classify(&e), Kind::ClaudeTooOld, "{e}");

    let here = Arc::new(Here {
        claude: claude.clone(),
        config: tmp.path().join("claude-code"),
        runs: AtomicUsize::new(0),
        works: false,
    });
    let e = engine(&claude, tmp.path(), Some(here.clone()))
        .complete(ask("[needs-update] hi"))
        .await
        .unwrap_err();
    assert_eq!(
        classify(&e),
        Kind::ClaudeTooOld,
        "the turn's own error: {e}"
    );
    assert_eq!(here.runs.load(Ordering::SeqCst), 1, "one try, no loop");
}

#[tokio::test]
async fn another_failure_is_not_a_reason_to_update() {
    let tmp = tempfile::tempdir().unwrap();
    let claude = native_install(tmp.path());
    let here = Arc::new(Here {
        claude: claude.clone(),
        config: tmp.path().join("claude-code"),
        runs: AtomicUsize::new(0),
        works: true,
    });
    let e = engine(&claude, tmp.path(), Some(here.clone()))
        .complete(ask("[crash]"))
        .await
        .unwrap_err();
    assert_ne!(classify(&e), Kind::ClaudeTooOld);
    assert_eq!(here.runs.load(Ordering::SeqCst), 0);
}
