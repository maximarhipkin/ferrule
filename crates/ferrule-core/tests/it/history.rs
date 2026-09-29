//! Reversible compaction, end to end through the agent loop: a model that
//! lost a fact to compaction gets it back with `search_history`; an old
//! large tool result is shortened to a reference and fetched back in full;
//! session-start recall runs once. The "model" is a scripted provider that
//! decides from what it can see in each request, like a real one would.

use ferrule_core::message::ToolCall;
use ferrule_core::provider::{CompletionRequest, CompletionResponse};
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_core::{
    Agent, AgentConfig, AgentEvent, ContextOverflow, CoreError, HarnessProfile, Message, Provider,
    Role, SearchHistoryTool, SessionRecall, ToolRegistry, Transcript, Usage, SEARCH_HISTORY,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

type Brain = dyn Fn(&CompletionRequest, usize) -> Message + Send + Sync;

/// A provider driven by a function of the request and the turn number.
/// Summary requests (no tools offered) are answered with a summary that
/// deliberately leaves the details out, and recorded.
struct Model {
    brain: Box<Brain>,
    turns: Mutex<usize>,
    summaries: Mutex<usize>,
    requests: Mutex<Vec<String>>,
}

impl Model {
    fn new(
        brain: impl Fn(&CompletionRequest, usize) -> Message + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            brain: Box::new(brain),
            turns: Mutex::new(0),
            summaries: Mutex::new(0),
            requests: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl Provider for Model {
    fn name(&self) -> &str {
        "scripted"
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let message = if req.tools.is_empty() {
            *self.summaries.lock().unwrap() += 1;
            Message::assistant(
                Some("Goal: see request. Progress: some lookups done.".into()),
                vec![],
                None,
            )
        } else {
            self.requests.lock().unwrap().push(visible(&req));
            let turn = {
                let mut t = self.turns.lock().unwrap();
                *t += 1;
                *t
            };
            (self.brain)(&req, turn)
        };
        Ok(CompletionResponse {
            message,
            usage: Usage::default(),
        })
    }
}

/// Everything the model can see in a request.
fn visible(req: &CompletionRequest) -> String {
    req.messages
        .iter()
        .filter_map(|m| m.content.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

fn last_tool_result(req: &CompletionRequest) -> Option<&str> {
    req.messages
        .last()
        .filter(|m| m.role == Role::Tool)
        .and_then(|m| m.content.as_deref())
}

fn call(id: &str, name: &str, args: serde_json::Value) -> Message {
    Message::assistant(
        None,
        vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args,
        }],
        None,
    )
}

fn say(text: &str) -> Message {
    Message::assistant(Some(text.into()), vec![], None)
}

/// A read-only tool answering `{"what": …}` from a fixed table.
struct Lookup(Vec<(&'static str, String)>);

#[async_trait::async_trait]
impl Tool for Lookup {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "lookup".into(),
            description: "look something up".into(),
            parameters: json!({"type": "object", "properties": {"what": {"type": "string"}}}),
        }
    }
    async fn call(
        &self,
        args: serde_json::Value,
        _: &ToolContext,
    ) -> Result<ToolOutput, CoreError> {
        let what = args["what"].as_str().unwrap_or("");
        let text = self
            .0
            .iter()
            .find(|(k, _)| *k == what)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| format!("nothing about {what}"));
        Ok(ToolOutput::ok(text))
    }
    fn changes_files(&self) -> bool {
        false
    }
}

struct Setup {
    agent: Agent,
    _dir: tempfile::TempDir,
    transcript: Option<Transcript>,
}

fn setup(
    model: Arc<Model>,
    lookup: Lookup,
    window: usize,
    config: AgentConfig,
    with_transcript: bool,
    with_search: bool,
) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let transcript = with_transcript.then(|| Transcript::create(dir.path(), "session").unwrap());
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(lookup));
    if let (true, Some(t)) = (with_search, &transcript) {
        tools.register(Arc::new(SearchHistoryTool::new(t)));
    }
    let mut profile = HarnessProfile::generic();
    profile.context_window = window;
    profile.output_reserve = 0;
    profile.compaction_threshold = 1.0;
    let agent = Agent::new(
        model,
        tools,
        profile,
        config,
        ToolContext {
            workspace: dir.path().to_path_buf(),
            max_output_chars: 40_000,
        },
        transcript.clone(),
    )
    .with_system_prompt("You are a test agent.");
    Setup {
        agent,
        _dir: dir,
        transcript,
    }
}

async fn run(agent: &mut Agent, goal: &str) -> (String, Vec<AgentEvent>) {
    let (tx, mut rx) = mpsc::channel(4096);
    let answer = agent.run(goal, tx).await.unwrap();
    let events = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    (answer, events)
}

fn count(events: &[AgentEvent], f: impl Fn(&AgentEvent) -> bool) -> usize {
    events.iter().filter(|e| f(e)).count()
}

/// (b) The fact is looked up early, the session is compacted, the summary
/// leaves the fact out, and the agent answers from the dropped history by
/// calling `search_history`.
#[tokio::test]
async fn after_compaction_the_agent_answers_from_dropped_history() {
    let padding = "background noise ".repeat(90); // ~1.5k chars
    let lookup = Lookup(vec![
        ("vault", format!("The vault code is 7319. {padding}")),
        ("weather", format!("It is sunny. {padding}")),
        ("traffic", format!("Roads are clear. {padding}")),
        ("news", format!("Nothing happened. {padding}")),
    ]);
    let model = Model::new(|req, turn| {
        let seen = visible(req);
        if let Some(result) = last_tool_result(req).filter(|r| r.contains("matching messages")) {
            let code = result
                .split("vault code is ")
                .nth(1)
                .map(|s| s.chars().take(4).collect::<String>())
                .unwrap_or_else(|| "unknown".into());
            return say(&format!("The vault code is {code}."));
        }
        match turn {
            1 => call("1", "lookup", json!({"what": "vault"})),
            2 => call("2", "lookup", json!({"what": "weather"})),
            3 => call("3", "lookup", json!({"what": "traffic"})),
            4 => call("4", "lookup", json!({"what": "news"})),
            _ if seen.contains("7319") => say("I still see it: 7319."),
            _ => call("5", SEARCH_HISTORY, json!({"query": "vault code"})),
        }
    });
    let config = AgentConfig {
        compaction_keep_last: 4,
        ..Default::default()
    };
    // ~1.2k tokens trigger: four 1.5k-char results pass it.
    let mut s = setup(model.clone(), lookup, 1_200, config, true, true);
    let (answer, events) = run(&mut s.agent, "What is the vault code? Look around first.").await;

    assert!(count(&events, |e| matches!(e, AgentEvent::Compacted { .. })) >= 1);
    assert!(*model.summaries.lock().unwrap() >= 1);
    // The model really had lost it: the request that led to the search held
    // no trace of the code.
    let requests = model.requests.lock().unwrap();
    let searched_at = requests
        .iter()
        .position(|r| !r.contains("7319") && r.contains("[Compaction summary"));
    assert!(searched_at.is_some(), "the fact never left the context");
    assert_eq!(answer, "The vault code is 7319.");
    // Nothing was shortened: the results were under the threshold.
    assert_eq!(
        count(&events, |e| matches!(
            e,
            AgentEvent::ToolResultsShortened { .. }
        )),
        0
    );
}

fn big_log() -> String {
    let mut s: String = (0..2_000).map(|i| format!("line {i:04}: ok\n")).collect();
    s.push_str("FINAL LINE: exit status 42 (SECRET-TAIL)\n");
    s
}

/// (c) An old large tool result is shortened to a preview and a reference;
/// the agent fetches it back in full with `search_history`. Shortening was
/// enough, so no summary call was made.
#[tokio::test]
async fn an_old_large_result_is_shortened_and_fetched_back() {
    let log = big_log(); // ~32k chars, ~8k tokens: fits a 9k-token window alone
    let lookup = Lookup(vec![
        ("build log", log.clone()),
        ("status", format!("all green {}", ".".repeat(8_000))),
    ]);
    let model = Model::new(|req, turn| {
        let seen = visible(req);
        if let Some(result) = last_tool_result(req).filter(|r| r.starts_with("[r")) {
            let status = result
                .split("exit status ")
                .nth(1)
                .map(|s| s.chars().take(2).collect::<String>())
                .unwrap_or_else(|| "?".into());
            return say(&format!("The build exited with status {status}."));
        }
        if turn == 1 {
            return call("1", "lookup", json!({"what": "build log"}));
        }
        if turn == 2 {
            return call("2", "lookup", json!({"what": "status"}));
        }
        // Find the ref in a shortened result and fetch it.
        let marker = "search_history {\"ref\": \"";
        match seen.find(marker) {
            Some(i) if !seen.contains("SECRET-TAIL") => {
                let r: String = seen[i + marker.len()..].chars().take(17).collect();
                call("3", SEARCH_HISTORY, json!({"ref": r}))
            }
            _ => say("no ref to fetch"),
        }
    });
    let config = AgentConfig {
        compaction_keep_last: 2,
        ..Default::default()
    };
    let mut s = setup(model.clone(), lookup, 9_000, config, true, true);
    let (answer, events) = run(&mut s.agent, "Why did the build fail?").await;

    assert_eq!(answer, "The build exited with status 42.");
    let shortened: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolResultsShortened {
                shortened,
                est_tokens_before,
                est_tokens_after,
            } => Some((*shortened, *est_tokens_before, *est_tokens_after)),
            _ => None,
        })
        .collect();
    // The build log first; later the status result ages out of the tail too.
    assert!(!shortened.is_empty(), "{events:?}");
    assert_eq!(shortened[0].0, 1);
    assert!(shortened[0].2 < shortened[0].1 / 3);
    // The first compaction needed no summary. (Reading the whole log back
    // in may trigger one later; that's the normal path.)
    let requests = model.requests.lock().unwrap();
    let after_shortening = requests
        .iter()
        .find(|r| r.contains("[ferrule: an older tool result (lookup, "))
        .expect("the model saw the shortened result");
    assert!(!after_shortening.contains("[Compaction summary"));
    assert!(!after_shortening.contains("SECRET-TAIL"));
    assert!(
        after_shortening.contains("line 0000: ok"),
        "the preview is kept"
    );
    // The transcript still has the full text: it was never rewritten.
    let t = s.transcript.as_ref().unwrap();
    let on_disk = t.read_messages().unwrap();
    assert!(on_disk
        .iter()
        .any(|m| m.content.as_deref() == Some(log.as_str())));
}

/// Without a way to fetch it back, nothing is shortened: no transcript, or
/// no `search_history` tool, or the naive `Truncate` baseline.
#[tokio::test]
async fn no_shortening_without_a_way_back_or_in_truncate_mode() {
    let brain = |_: &CompletionRequest, turn: usize| match turn {
        1 => call("1", "lookup", json!({"what": "build log"})),
        2 => call("2", "lookup", json!({"what": "status"})),
        _ => say("done"),
    };
    let cases: [(bool, bool, ContextOverflow); 3] = [
        (false, true, ContextOverflow::Compact),
        (true, false, ContextOverflow::Compact),
        (true, true, ContextOverflow::Truncate),
    ];
    for (with_transcript, with_search, overflow) in cases {
        let model = Model::new(brain);
        let lookup = Lookup(vec![
            ("build log", big_log()),
            ("status", "all green".into()),
        ]);
        let config = AgentConfig {
            compaction_keep_last: 2,
            overflow,
            ..Default::default()
        };
        let mut s = setup(
            model.clone(),
            lookup,
            6_000,
            config,
            with_transcript,
            with_search,
        );
        let (answer, events) = run(&mut s.agent, "Why did the build fail?").await;
        assert_eq!(answer, "done");
        let case = format!("transcript={with_transcript} search={with_search} {overflow:?}");
        assert_eq!(
            count(&events, |e| matches!(
                e,
                AgentEvent::ToolResultsShortened { .. }
            )),
            0,
            "{case}"
        );
        let any_shortened = model
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.contains("[ferrule: an older tool result"));
        assert!(!any_shortened, "{case}");
        match overflow {
            ContextOverflow::Truncate => {
                assert!(count(&events, |e| matches!(e, AgentEvent::Truncated { .. })) >= 1);
                assert_eq!(*model.summaries.lock().unwrap(), 0, "{case}");
            }
            ContextOverflow::Compact => {
                assert!(*model.summaries.lock().unwrap() >= 1, "{case}");
            }
        }
    }
}

/// A summary that folds shortened results lists their references, so the
/// pointer survives the fold.
#[tokio::test]
async fn a_summary_keeps_the_refs_of_the_results_it_folds() {
    let log = big_log();
    let lookup = Lookup(vec![
        ("build log", log.clone()),
        ("a", "x".repeat(3_000)),
        ("b", "y".repeat(3_000)),
        ("c", "z".repeat(3_000)),
    ]);
    let model = Model::new(|_, turn| match turn {
        1 => call("1", "lookup", json!({"what": "build log"})),
        2 => call("2", "lookup", json!({"what": "a"})),
        3 => call("3", "lookup", json!({"what": "b"})),
        4 => call("4", "lookup", json!({"what": "c"})),
        _ => say("done"),
    });
    let config = AgentConfig {
        compaction_keep_last: 2,
        ..Default::default()
    };
    let mut s = setup(model.clone(), lookup, 2_500, config, true, true);
    run(&mut s.agent, "Look at everything").await;
    assert!(*model.summaries.lock().unwrap() >= 1);
    let summary = s
        .agent
        .messages
        .iter()
        .filter_map(|m| m.content.as_deref())
        .find(|c| c.starts_with("[Compaction summary"))
        .expect("a summary")
        .to_string();
    assert!(
        summary.contains(&ferrule_core::result_ref(&log)),
        "{summary}"
    );
    assert!(summary.contains("Why") || summary.contains("Look at everything"));
}

struct Recorder {
    queries: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl SessionRecall for Recorder {
    async fn recall(&self, goal: &str) -> Option<String> {
        self.queries.lock().unwrap().push(goal.to_string());
        Some("[Long-term memory]\n- #4 The staging port is 5782".into())
    }
}

/// Session-start recall: asked once, with the session's first request and
/// this run's goal, and added once, as a user message right after the goal
/// (M27: the system prompt stays the same bytes for every session).
#[tokio::test]
async fn session_recall_runs_once_with_the_session_goal() {
    let model = Model::new(|_, _| say("ok"));
    let mut s = setup(
        model.clone(),
        Lookup(vec![]),
        100_000,
        AgentConfig::default(),
        true,
        true,
    );
    // A resumed session: its history already has a first request.
    s.agent
        .messages
        .push(Message::user("Set up the staging database"));
    s.agent.messages.push(say("Done."));
    let recorder = Arc::new(Recorder {
        queries: Mutex::new(Vec::new()),
    });
    let mut agent = std::mem::replace(
        &mut s.agent,
        Agent::new(
            model.clone(),
            ToolRegistry::new(),
            HarnessProfile::generic(),
            AgentConfig::default(),
            ToolContext::default(),
            None,
        ),
    )
    .with_session_recall(recorder.clone());
    run(&mut agent, "Which port did we pick?").await;
    run(&mut agent, "And the user name?").await;

    let queries = recorder.queries.lock().unwrap();
    assert_eq!(queries.len(), 1);
    assert!(queries[0].contains("Set up the staging database"));
    assert!(queries[0].contains("Which port did we pick?"));
    let system = agent.messages[0].content.as_deref().unwrap();
    assert_eq!(system, "You are a test agent.");
    let texts: Vec<&str> = agent
        .messages
        .iter()
        .filter_map(|m| m.content.as_deref())
        .collect();
    let memory: Vec<usize> = (0..texts.len())
        .filter(|&i| texts[i].contains("[Long-term memory]"))
        .collect();
    assert_eq!(memory.len(), 1);
    assert_eq!(texts[memory[0] - 1], "Which port did we pick?");
    // The model saw it on the very first call.
    assert!(model.requests.lock().unwrap()[0].contains("#4 The staging port is 5782"));
}
