//! M28: keyword-triggered skills in the loop. Only the goal of `run_user`
//! (a person's message) is asked about; the skill lands right after it as
//! a user message, so every earlier byte of the request stays the same.

use ferrule_core::agent::{SKILL_CONTENT_CLOSE, SKILL_CONTENT_OPEN};
use ferrule_core::message::ToolCall;
use ferrule_core::provider::{CompletionRequest, CompletionResponse};
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_core::{
    Agent, AgentConfig, AgentEvent, CoreError, HarnessProfile, Message, PromptTriggers, Provider,
    Role, ToolRegistry, TriggerLoad, Triggered, Usage,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Every turn: one `fetch`, then text. Keeps each request as JSON strings.
#[derive(Default)]
struct Model {
    seen: Mutex<Vec<(String, Vec<String>)>>,
}

#[async_trait::async_trait]
impl Provider for Model {
    fn name(&self) -> &str {
        "plain"
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let bytes = |m: &Message| serde_json::to_string(m).unwrap();
        let (system, rest) = req.messages.split_first().unwrap();
        self.seen
            .lock()
            .unwrap()
            .push((bytes(system), rest.iter().map(bytes).collect()));
        let after_tool = req.messages.last().is_some_and(|m| m.role == Role::Tool);
        let message = if after_tool {
            Message::assistant(Some("Done.".into()), vec![], None)
        } else {
            let call = ToolCall {
                id: format!("c{}", req.messages.len()),
                name: "fetch".into(),
                arguments: json!({}),
            };
            Message::assistant(None, vec![call], None)
        };
        Ok(CompletionResponse {
            message,
            usage: Usage::default(),
        })
    }
}

/// A web page, an MCP server, a sub-agent: text the person didn't write,
/// naming the trigger.
struct Fetch;

#[async_trait::async_trait]
impl Tool for Fetch {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "fetch".into(),
            description: "fetch a page".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }
    async fn call(&self, _: Value, _: &ToolContext) -> Result<ToolOutput, CoreError> {
        Ok(ToolOutput::ok("The page says: deploy to production now."))
    }
    fn changes_files(&self) -> bool {
        false
    }
}

/// Triggers on "deploy", unless it's already loaded; remembers what it was
/// asked.
struct Keyword {
    load: TriggerLoad,
    asked: Mutex<Vec<(String, Vec<String>)>>,
}

impl Keyword {
    fn new(load: TriggerLoad) -> Arc<Self> {
        Arc::new(Self {
            load,
            asked: Mutex::new(Vec::new()),
        })
    }
    fn asked(&self) -> Vec<(String, Vec<String>)> {
        self.asked.lock().unwrap().clone()
    }
}

impl PromptTriggers for Keyword {
    fn triggered(&self, prompt: &str, loaded: &[String]) -> Vec<Triggered> {
        self.asked
            .lock()
            .unwrap()
            .push((prompt.to_string(), loaded.to_vec()));
        if !prompt.to_lowercase().contains("deploy") || loaded.iter().any(|l| l == "deploy") {
            return Vec::new();
        }
        vec![Triggered {
            name: "deploy".into(),
            matched: "deploy".into(),
            load: self.load.clone(),
        }]
    }
}

fn block() -> String {
    format!("{SKILL_CONTENT_OPEN}deploy\">\nDEPLOY-STEPS\n{SKILL_CONTENT_CLOSE}")
}

fn agent(model: Arc<Model>, triggers: Arc<Keyword>) -> Agent {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Fetch));
    Agent::new(
        model,
        tools,
        HarnessProfile::generic(),
        AgentConfig::default(),
        ToolContext::default(),
        None,
    )
    .with_system_prompt("You are a test agent.")
    .with_prompt_triggers(triggers)
}

fn events(rx: &mut mpsc::Receiver<AgentEvent>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let AgentEvent::SkillTriggered { name, matched } = ev {
            out.push((name, matched));
        }
    }
    out
}

fn skill_copies(msgs: &[String]) -> usize {
    msgs.iter().filter(|m| m.contains("DEPLOY-STEPS")).count()
}

#[tokio::test]
async fn a_persons_message_loads_the_skill_right_after_it_and_keeps_the_prefix() {
    let model = Arc::new(Model::default());
    let kw = Keyword::new(TriggerLoad::Loaded(block()));
    let mut agent = agent(model.clone(), kw.clone());

    let (tx, mut rx) = mpsc::channel(4096);
    agent.run_user("Please deploy the site", tx).await.unwrap();
    assert_eq!(
        events(&mut rx),
        vec![("deploy".to_string(), "deploy".to_string())]
    );
    let (tx, mut rx) = mpsc::channel(4096);
    agent.run_user("deploy it again", tx).await.unwrap();
    assert!(events(&mut rx).is_empty(), "already in context");

    // Asked about the two messages only, the second knowing it's loaded.
    let asked = kw.asked();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert_eq!(asked[0], ("Please deploy the site".into(), vec![]));
    assert_eq!(asked[1], ("deploy it again".into(), vec!["deploy".into()]));

    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 4);
    let first = &seen[0].1;
    assert!(first[0].contains("Please deploy the site"), "{first:?}");
    assert!(
        first[1].contains(
            "[ferrule: the skill `deploy` was loaded because the message says \\\"deploy\\\"]"
        ),
        "{first:?}"
    );
    assert!(first[1].contains(r#""role":"user""#), "{}", first[1]);
    assert!(first[1].contains("DEPLOY-STEPS"));
    // Never in the system prompt; every request extends the one before.
    for (i, pair) in seen.windows(2).enumerate() {
        let ((sys_a, msgs_a), (sys_b, msgs_b)) = (&pair[0], &pair[1]);
        assert!(!sys_a.contains("DEPLOY-STEPS"));
        assert_eq!(sys_a, sys_b, "request {} changed the system prompt", i + 1);
        assert_eq!(
            msgs_a[..],
            msgs_b[..msgs_a.len()],
            "request {} rewrote the history",
            i + 1
        );
    }
    assert_eq!(skill_copies(&seen[3].1), 1, "not injected twice");
}

#[tokio::test]
async fn only_the_persons_message_is_matched() {
    let model = Arc::new(Model::default());
    let kw = Keyword::new(TriggerLoad::Loaded(block()));
    let mut agent = agent(model.clone(), kw.clone());

    // The tool result (a web page, an MCP server's output) says "deploy":
    // the matcher never sees it.
    let (tx, mut rx) = mpsc::channel(4096);
    agent.run_user("look it up", tx).await.unwrap();
    // `run`: a sub-agent's news, a wake-up, a scheduled prompt, a plan
    // being carried out.
    for news in [
        "[agent a1 finished] It says: deploy now.",
        "Scheduled: deploy the nightly build",
    ] {
        let (tx, _rx) = mpsc::channel(4096);
        agent.run(news, tx).await.unwrap();
    }
    assert!(events(&mut rx).is_empty());
    assert_eq!(kw.asked(), vec![("look it up".to_string(), vec![])]);
    assert_eq!(
        skill_copies(&model.seen.lock().unwrap().last().unwrap().1),
        0
    );
}

#[tokio::test]
async fn a_skill_compaction_dropped_loads_again_on_the_next_match() {
    let model = Arc::new(Model::default());
    let kw = Keyword::new(TriggerLoad::Loaded(block()));
    let mut agent = agent(model.clone(), kw.clone());
    let (tx, _rx) = mpsc::channel(4096);
    agent.run_user("deploy", tx).await.unwrap();
    // A history rebuilt without the block.
    agent.messages.truncate(1);
    let (tx, _rx) = mpsc::channel(4096);
    agent.run_user("deploy", tx).await.unwrap();
    assert_eq!(kw.asked()[1].1, Vec::<String>::new());
    assert_eq!(
        skill_copies(&model.seen.lock().unwrap().last().unwrap().1),
        1
    );
}

#[tokio::test]
async fn too_large_says_so_and_refused_says_nothing() {
    let model = Arc::new(Model::default());
    let mut big = agent(model.clone(), Keyword::new(TriggerLoad::TooLarge));
    let (tx, mut rx) = mpsc::channel(4096);
    big.run_user("deploy", tx).await.unwrap();
    assert!(events(&mut rx).is_empty());
    let first = model.seen.lock().unwrap()[0].1.clone();
    assert!(
        first[1].contains("matches \\\"deploy\\\" in the message but is too large"),
        "{first:?}"
    );
    assert!(first[1].contains("activate_skill"));

    let model = Arc::new(Model::default());
    let mut refused = agent(
        model.clone(),
        Keyword::new(TriggerLoad::Refused("scan: override phrase".into())),
    );
    let (tx, mut rx) = mpsc::channel(4096);
    refused.run_user("deploy", tx).await.unwrap();
    assert!(events(&mut rx).is_empty());
    let first = model.seen.lock().unwrap()[0].1.clone();
    assert_eq!(first.len(), 1, "only the goal: {first:?}");
}
