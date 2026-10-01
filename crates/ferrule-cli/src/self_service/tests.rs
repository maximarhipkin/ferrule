//! M48 part 1: the tool's ops, the card, and the approval it waits for.

use super::*;
use crate::config::Config;
use ferrule_core::tool::{Tool, ToolContext};
use ferrule_gateway::Redactor;
use serde_json::json;
use std::path::Path;
use std::sync::Mutex;

const CONFIG: &str = r#"default_provider = "a"

[providers.a]
base_url = "http://127.0.0.1:2/v1"
api_key_env = "PATH"
model = "a-one"
[providers.a.models.a-two]

[providers.b]
base_url = "http://127.0.0.1:2/v1"
api_key_env = "PATH"
model = "b-large"
"#;

/// The owner's chat, as the hub's notifier sees it.
#[derive(Default)]
struct Told(Mutex<Vec<String>>);

#[async_trait::async_trait]
impl ferrule_trust::Notifier for Told {
    async fn send(&self, _chat: i64, text: &str) -> Result<(), String> {
        self.0.lock().unwrap().push(text.to_string());
        Ok(())
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    config: std::path::PathBuf,
    hub: Arc<Hub>,
    told: Arc<Told>,
    admin: Arc<Admin>,
    here: ChatRef,
}

impl Rig {
    fn new(ask_for: Duration) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let hub = Arc::new(
            Hub::new(
                Default::default(),
                &data,
                &data.join("ledger.jsonl"),
                Arc::new(ferrule_trust::SystemClock),
                vec![],
            )
            .unwrap(),
        );
        hub.set_owner(Some(42));
        let told = Arc::new(Told::default());
        hub.set_notifier(Some(told.clone()));
        let config = dir.path().join("ferrule.toml");
        std::fs::write(&config, CONFIG).unwrap();
        let cfg: Config = toml::from_str(CONFIG).unwrap();
        let mut ctx = Ctx::bare(Arc::new(Redactor::new([
            "sk-SEEDED-0123456789abcdef".into()
        ])));
        ctx.data = Some(data.clone());
        ctx.config_path = Some(config.clone());
        ctx.hub = Some(hub.clone());
        ctx.models = Some(Arc::new(crate::models::Models::new(
            config.clone(),
            Some(data.join("pins.json")),
            &cfg,
        )));
        let admin = Admin::with_timeout(hub.clone(), ask_for);
        admin.bind_own(ctx);
        Self {
            _dir: dir,
            config,
            hub,
            told,
            admin,
            here: ChatRef::new("telegram", "42"),
        }
    }

    fn tool(&self) -> AdminTool {
        AdminTool::new(self.admin.clone(), "telegram__42", self.here.clone())
    }

    async fn call(&self, args: Value) -> Result<String, String> {
        self.tool()
            .call(args, &ToolContext::default())
            .await
            .map(|o| o.content)
            .map_err(|e| e.to_string())
    }

    /// Starts a call and waits until the owner has been asked.
    fn start(&self, args: Value) -> tokio::task::JoinHandle<Result<String, String>> {
        let tool = self.tool();
        tokio::spawn(async move {
            tool.call(args, &ToolContext::default())
                .await
                .map(|o| o.content)
                .map_err(|e| e.to_string())
        })
    }

    async fn asked(&self) -> String {
        for _ in 0..200 {
            if let Some(t) = self.told.0.lock().unwrap().last() {
                return t.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the owner was never asked");
    }

    fn reply(&self, text: &str) -> Option<String> {
        self.hub.approvals().answer(self.here.clone(), text)
    }

    fn config_text(&self) -> String {
        std::fs::read_to_string(&self.config).unwrap()
    }
}

fn long() -> Duration {
    Duration::from_secs(30)
}

#[test]
fn the_ops_read_their_arguments_and_nothing_else() {
    assert_eq!(
        Op::from_args(&json!({"op": "model_default", "model": "b"})).unwrap(),
        Op::ModelDefault { model: "b".into() }
    );
    // A claimed approval, an unknown field and an unknown op are refused.
    for bad in [
        json!({"op": "model_default", "model": "b", "approved": true}),
        json!({"op": "model_default", "model": "b", "shell": "rm -rf ~"}),
        json!({"op": "run_shell", "line": "ls"}),
        json!({"op": "model_default"}),
    ] {
        assert!(Op::from_args(&bad).is_err(), "{bad}");
    }
    // Every named op has a variant, and the two lists don't overlap.
    for name in READ_OPS.iter().chain(CHANGE_OPS) {
        let args = json!({
            "op": name, "model": "m", "models": ["m"], "key": "agent.stream",
            "value": true, "caps": {"daily_usd": 1.0}, "name": "n", "sha": "s",
            "id": "i", "prompt": "p", "schedule": "every 1h",
        });
        let op = Op::from_args(&args).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(op.name(), *name);
        assert_eq!(op.is_change(), CHANGE_OPS.contains(name));
    }
    assert_eq!(READ_OPS.len() + CHANGE_OPS.len(), 35);
}

#[test]
fn the_digest_follows_every_argument() {
    let a = Op::ModelDefault { model: "a".into() };
    let b = Op::ModelDefault { model: "b".into() };
    assert_eq!(a.digest(), a.clone().digest());
    assert_ne!(a.digest(), b.digest());
    assert_eq!(a.digest().len(), 12);
    assert_ne!(Op::Backup.digest(), Op::Restart.digest());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_runs_without_asking() {
    let rig = Rig::new(long());
    let out = rig.call(json!({"op": "models"})).await.unwrap();
    assert!(out.contains("Default: a/a-one"), "{out}");
    assert!(out.contains("b/b-large"), "{out}");
    assert!(rig.told.0.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_asks_with_the_exact_change_and_runs_after_the_tap() {
    let rig = Rig::new(long());
    let call = rig.start(json!({"op": "model_default", "model": "b/b-large"}));
    let card = rig.asked().await;
    assert!(
        card.contains("Switch the default model to b/b-large (now a/a-one)"),
        "{card}"
    );
    assert!(rig.config_text().contains("default_provider = \"a\""));
    assert!(rig.reply("yes").unwrap().starts_with("Approved"));
    let out = call.await.unwrap().unwrap();
    assert!(out.starts_with("Approved and done:"), "{out}");
    let now = rig.config_text();
    assert!(now.contains("default = \"b/b-large\""), "{now}");
    // The change is audited as the chat that was approved, and the run is
    // recorded.
    let log = rig.hub.audit().read(None).unwrap();
    let ran = log
        .iter()
        .find(|e| e.event == "self_service.ran")
        .expect("a self_service.ran row");
    assert_eq!(ran.detail["op"], "model_default");
    assert_eq!(ran.detail["ok"], true);
    let asked = log.iter().find(|e| e.event == "approval_asked").unwrap();
    assert!(asked.detail["op"]
        .as_str()
        .unwrap()
        .starts_with("model_default:"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_changes_nothing() {
    let rig = Rig::new(long());
    let before = rig.config_text();
    let call = rig.start(json!({"op": "model_default", "model": "b/b-large"}));
    rig.asked().await;
    rig.reply("no");
    let out = call.await.unwrap().unwrap();
    assert!(out.starts_with("The owner refused it"), "{out}");
    assert!(out.contains("nothing changed"), "{out}");
    assert_eq!(rig.config_text(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_answer_in_time_changes_nothing() {
    let rig = Rig::new(Duration::from_millis(150));
    let before = rig.config_text();
    let out = rig
        .call(json!({"op": "model_default", "model": "b/b-large"}))
        .await
        .unwrap();
    assert!(out.starts_with("No answer in"), "{out}");
    assert!(out.contains("nothing changed"), "{out}");
    assert_eq!(rig.config_text(), before);
    // A late tap finds nothing to approve.
    assert!(rig.reply("yes").unwrap().contains("expired"));
    assert_eq!(rig.config_text(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_change_waits_for_the_first_answer() {
    let rig = Rig::new(long());
    let first = rig.start(json!({"op": "model_default", "model": "b/b-large"}));
    rig.asked().await;
    let err = rig
        .call(json!({"op": "model_default", "model": "a/a-two"}))
        .await
        .unwrap_err();
    assert!(err.contains("A question is already waiting"), "{err}");
    rig.reply("no");
    first.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_model_is_refused_before_anyone_is_asked() {
    let rig = Rig::new(long());
    let err = rig
        .call(json!({"op": "model_default", "model": "nope/none"}))
        .await
        .unwrap_err();
    assert!(err.contains("Not asked"), "{err}");
    assert!(
        err.contains("a/a-one"),
        "the error lists the real names: {err}"
    );
    assert!(rig.told.0.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tool_writes_the_config_whatever_the_shell_may() {
    // The tool runs in the host, not in the shell's sandbox: a config file
    // the shell can't write (read-only here) is still replaced by an
    // approved op, which writes a fresh file beside it and renames it.
    let rig = Rig::new(long());
    let mut mode = std::fs::metadata(&rig.config).unwrap().permissions();
    mode.set_readonly(true);
    std::fs::set_permissions(&rig.config, mode).unwrap();
    let call = rig.start(json!({"op": "config_set", "key": "agent.stream", "value": false}));
    let card = rig.asked().await;
    assert!(card.contains("agent.stream"), "{card}");
    rig.reply("yes");
    let out = call.await.unwrap().unwrap();
    assert!(out.starts_with("Approved and done:"), "{out}");
    assert!(
        rig.config_text().contains("stream = false"),
        "{}",
        rig.config_text()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_config_read_shows_only_the_form_settings() {
    let rig = Rig::new(long());
    let out = rig
        .call(json!({"op": "config_get", "key": "agent.stream"}))
        .await
        .unwrap();
    assert!(out.starts_with("agent.stream ="), "{out}");
    let err = rig
        .call(json!({"op": "config_get", "key": "providers.a.api_key_env"}))
        .await
        .unwrap_err();
    assert!(err.contains("isn't shown"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unbound_tool_says_it_is_starting() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Arc::new(
        Hub::new(
            Default::default(),
            dir.path(),
            &dir.path().join("ledger.jsonl"),
            Arc::new(ferrule_trust::SystemClock),
            vec![],
        )
        .unwrap(),
    );
    let tool = AdminTool::new(
        Admin::new(hub),
        "telegram__42",
        ChatRef::new("telegram", "42"),
    );
    let err = tool
        .call(json!({"op": "status"}), &ToolContext::default())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("still starting"), "{err}");
}

#[test]
fn the_tool_is_offered_only_in_the_owners_private_chat() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::new(
        Default::default(),
        dir.path(),
        &dir.path().join("ledger.jsonl"),
        Arc::new(ferrule_trust::SystemClock),
        vec![],
    )
    .unwrap();
    hub.set_owner(Some(42));
    assert_eq!(
        offered(&hub, "telegram__42"),
        Some(ChatRef::new("telegram", "42"))
    );
    assert_eq!(
        offered(&hub, "dashboard__owner"),
        Some(ChatRef::new("dashboard", "owner"))
    );
    // Someone else's chat, a group, a task and a sub-agent's session.
    for session in [
        "telegram__43",
        "telegram__-1001",
        "scheduler__t1",
        "telegram__42__child",
        "cli",
    ] {
        assert_eq!(offered(&hub, session), None, "{session}");
    }
}

#[test]
fn the_definition_lists_every_op_and_takes_no_extra_field() {
    let rig = Rig::new(long());
    let def = rig.tool().definition();
    assert_eq!(def.name, "ferrule_admin");
    let ops = def.parameters["properties"]["op"]["enum"]
        .as_array()
        .unwrap();
    assert_eq!(ops.len(), 35);
    assert_eq!(def.parameters["additionalProperties"], false);
    assert_eq!(rig.tool().serial_group().as_deref(), Some("ferrule_admin"));
    assert!(!rig.tool().needs_approval());
    let _: &Path = rig.config.as_path();
}

#[test]
fn the_description_names_every_op() {
    for op in READ_OPS.iter().chain(CHANGE_OPS) {
        assert!(
            ferrule_agents::prompts::ADMIN_DESCRIPTION.contains(op),
            "{op} is missing from the tool description"
        );
    }
}
