//! Suites through the real agent loop, with a scripted provider: grading,
//! ledger rows, the naive/engineered difference on early context, and the
//! budget cap. No network.

use ferrule_core::{
    CompletionRequest, CompletionResponse, CoreError, HarnessProfile, LedgerRecord, LedgerSink,
    Message, Provider, Role, ToolCall, Usage,
};
use ferrule_eval::{run_suite, Caps, Env, Options, Outcome, Suite, Variant, RESULT_KIND};
use ferrule_sandbox::Sandbox;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// What the scripted model sees of one task run: the task (from the
/// fixture dir name `<task>--<variant>`), how many calls it has had, and
/// the whole request.
struct Turn<'a> {
    task: &'a str,
    step: usize,
    req: &'a CompletionRequest,
}

type Script = dyn Fn(&Turn<'_>) -> Message + Send + Sync;

/// A provider driven by a function of the turn; compaction requests get a
/// summary that keeps nothing specific.
struct Scripted {
    script: Box<Script>,
    usage: Usage,
    steps: Mutex<HashMap<String, usize>>,
}

impl Scripted {
    fn new(usage: Usage, script: impl Fn(&Turn<'_>) -> Message + Send + Sync + 'static) -> Self {
        Self {
            script: Box::new(script),
            usage,
            steps: Mutex::new(HashMap::new()),
        }
    }
}

fn workspace_of(req: &CompletionRequest) -> Option<String> {
    let sys = req.messages.iter().find(|m| m.role == Role::System)?;
    let text = sys.content.as_deref()?;
    let rest = &text[text.find("Workspace: ")? + "Workspace: ".len()..];
    Some(rest[..rest.find(". ")?].to_string())
}

#[async_trait::async_trait]
impl Provider for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let first = req.messages[0].content.as_deref().unwrap_or("");
        let message = if first.starts_with("Summarize this agent session") {
            Message::assistant(
                Some("## Session Intent\nreading files".into()),
                vec![],
                None,
            )
        } else {
            let ws = workspace_of(&req).expect("a system prompt with the workspace");
            let label = Path::new(&ws)
                .parent()
                .and_then(|p| p.file_name())
                .unwrap()
                .to_string_lossy()
                .to_string();
            let task = label.split("--").next().unwrap().to_string();
            let step = {
                let mut steps = self.steps.lock().unwrap();
                let s = steps.entry(ws).or_default();
                *s += 1;
                *s - 1
            };
            (self.script)(&Turn {
                task: &task,
                step,
                req: &req,
            })
        };
        Ok(CompletionResponse {
            message,
            usage: self.usage.clone(),
        })
    }
}

fn call(name: &str, args: serde_json::Value) -> Message {
    Message::assistant(
        None,
        vec![ToolCall {
            id: format!("c-{name}"),
            name: name.into(),
            arguments: args,
        }],
        None,
    )
}

fn done() -> Message {
    Message::assistant(Some("done".into()), vec![], None)
}

#[derive(Default)]
struct Rows(Mutex<Vec<LedgerRecord>>);

impl LedgerSink for Rows {
    fn record(&self, r: LedgerRecord) {
        self.0.lock().unwrap().push(r);
    }
}

fn env(provider: Scripted, rows: Arc<Rows>) -> Env {
    Env {
        provider: Arc::new(provider),
        provider_name: "scripted".into(),
        model: "script-1".into(),
        profile: HarnessProfile::generic(),
        sandbox: Arc::new(Sandbox::off()),
        memory_tools: None,
        ledger: Some(rows),
        pricing: None,
        transcripts: None,
    }
}

fn suite(dir: &Path, toml: &str) -> Suite {
    std::fs::write(dir.join("suite.toml"), toml).unwrap();
    Suite::load(dir).unwrap()
}

fn opts(work: &Path, variants: Vec<Variant>) -> Options {
    Options {
        variants,
        work_root: Some(work.to_path_buf()),
        ..Default::default()
    }
}

fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: 0,
    }
}

const THREE: &str = r#"
[suite]
name = "three"

[[task]]
id = "alpha"
prompt = "Write the word alpha to out.txt."
[task.grade]
command = 'test "$(cat out.txt)" = alpha'

[[task]]
id = "beta"
prompt = "Write the word beta to out.txt."
[task.grade]
command = 'test "$(cat out.txt)" = beta'

[[task]]
id = "gamma"
prompt = "Write the word gamma to out.txt."
[task.grade]
command = 'test "$(cat out.txt)" = gamma'
"#;

/// Right on alpha and beta, wrong on gamma.
fn three_script(t: &Turn<'_>) -> Message {
    match t.step {
        0 => {
            let word = if t.task == "gamma" { "delta" } else { t.task };
            call("write_file", json!({"path": "out.txt", "content": word}))
        }
        _ => done(),
    }
}

#[tokio::test]
async fn a_three_task_suite_is_graded_and_its_rows_are_tagged_eval() {
    let dir = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let s = suite(dir.path(), THREE);
    let rows = Arc::new(Rows::default());
    let env = env(Scripted::new(usage(100, 10), three_script), rows.clone());
    let run = run_suite(&s, &env, &opts(work.path(), vec![Variant::Engineered]))
        .await
        .unwrap();

    let outcome = |id: &str| run.results.iter().find(|r| r.task == id).unwrap().outcome;
    assert_eq!(outcome("alpha"), Outcome::Pass);
    assert_eq!(outcome("beta"), Outcome::Pass);
    assert_eq!(outcome("gamma"), Outcome::Fail);
    assert_eq!(run.totals.calls, 6);
    assert_eq!(run.not_run, 0);
    assert!(run.budget_stop.is_none());

    let rows = rows.0.lock().unwrap();
    assert!(!rows.is_empty());
    assert!(
        rows.iter().all(|r| r.task_shape == "eval"),
        "every row is tagged eval"
    );
    let calls: Vec<_> = rows.iter().filter(|r| r.call_kind != RESULT_KIND).collect();
    let verdicts: Vec<_> = rows.iter().filter(|r| r.call_kind == RESULT_KIND).collect();
    assert_eq!(calls.len(), 6);
    assert_eq!(verdicts.len(), 3);
    for r in &calls {
        let tag = r.eval.as_ref().unwrap();
        assert_eq!(tag.run_id, run.run_id);
        assert_eq!(tag.suite, "three");
        assert_eq!(tag.variant, "engineered");
        assert_eq!(
            r.origin.as_deref(),
            Some(format!("three/{}", tag.task).as_str())
        );
    }
    let gamma = verdicts
        .iter()
        .find(|r| r.eval.as_ref().unwrap().task == "gamma")
        .unwrap();
    let result = gamma.eval.as_ref().unwrap().result.as_ref().unwrap();
    assert_eq!(result["outcome"], "fail");

    let report = ferrule_eval::report::render(&run);
    assert!(report.contains("2/3 (67%)"), "{report}");
    assert!(report.contains("gamma (engineered)"), "{report}");
    // Fixtures are gone unless --keep.
    assert!(std::fs::read_dir(work.path()).unwrap().next().is_none());
}

/// Five big files, then the answer only the original request holds.
const EARLY: &str = r#"
[suite]
name = "early"
context_window = 4000

[[task]]
id = "recall"
prompt = """The code word is CODEWORD=tangerine. Read big1.txt to big5.txt, \
then write the code word to answer.txt."""
[task.files]
"big1.txt" = '''{BIG}'''
"big2.txt" = '''{BIG}'''
"big3.txt" = '''{BIG}'''
"big4.txt" = '''{BIG}'''
"big5.txt" = '''{BIG}'''
[task.grade]
command = 'test "$(cat answer.txt)" = tangerine || { echo "answer.txt holds $(cat answer.txt)"; exit 1; }'
"#;

/// Reads the five files, then writes whatever code word it can still see.
fn early_script(t: &Turn<'_>) -> Message {
    match t.step {
        0..=4 => call(
            "read_file",
            json!({"path": format!("big{}.txt", t.step + 1)}),
        ),
        5 => {
            let word = t
                .req
                .messages
                .iter()
                .filter_map(|m| m.content.as_deref())
                .find_map(|c| {
                    let at = c.find("CODEWORD=")? + "CODEWORD=".len();
                    Some(
                        c[at..]
                            .chars()
                            .take_while(|ch| ch.is_ascii_alphabetic())
                            .collect::<String>(),
                    )
                })
                .unwrap_or_else(|| "forgotten".into());
            call("write_file", json!({"path": "answer.txt", "content": word}))
        }
        _ => done(),
    }
}

#[tokio::test]
async fn early_context_is_lost_to_naive_truncation_and_kept_by_engineered_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    // Each file ~1000 estimated tokens: five of them overflow a 4000 window.
    let big: String = (0..90)
        .map(|i| format!("line {i:03} of filler text for the context test.\n"))
        .collect();
    let s = suite(dir.path(), &EARLY.replace("{BIG}", big.trim_end()));
    let rows = Arc::new(Rows::default());
    let env = env(Scripted::new(usage(100, 10), early_script), rows);
    let run = run_suite(
        &s,
        &env,
        &opts(work.path(), vec![Variant::Engineered, Variant::Naive]),
    )
    .await
    .unwrap();

    let get = |v: Variant| run.results.iter().find(|r| r.variant == v).unwrap();
    let (eng, naive) = (get(Variant::Engineered), get(Variant::Naive));
    assert_eq!(run.context_window, 4000);
    assert_eq!(naive.outcome, Outcome::Fail, "{naive:?}");
    assert!(naive.truncations > 0, "{naive:?}");
    assert_eq!(naive.compactions, 0);
    assert_eq!(eng.outcome, Outcome::Pass, "{eng:?}");
    assert!(eng.compactions > 0, "{eng:?}");
    assert_eq!(eng.truncations, 0);

    let report = ferrule_eval::report::render(&run);
    assert!(report.contains("engineered − naive"), "{report}");
    assert!(report.contains("+100 pts"), "{report}");
    assert!(
        report.contains("answer.txt holds forgotten"),
        "the failure shows why: {report}"
    );
}

/// Every task takes two calls; the cap allows about three.
#[tokio::test]
async fn the_budget_cap_stops_a_suite_mid_run_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let s = suite(dir.path(), THREE);
    let rows = Arc::new(Rows::default());
    let env = env(Scripted::new(usage(1000, 10), three_script), rows.clone());
    let mut o = opts(work.path(), vec![Variant::Engineered]);
    o.caps = Caps {
        max_usd: None,
        max_tokens: Some(2500),
    };
    let run = run_suite(&s, &env, &o).await.unwrap();

    assert!(run.budget_stop.as_deref().unwrap().contains("token budget"));
    assert_eq!(run.results.len(), 2, "{:?}", run.results);
    assert_eq!(run.results[0].outcome, Outcome::Pass);
    assert_eq!(run.results[1].outcome, Outcome::Stopped);
    assert_eq!(run.not_run, 1);
    // Stopped before the call after the one that crossed the cap.
    assert_eq!(run.totals.calls, 3);
    let report = ferrule_eval::report::render(&run);
    assert!(report.contains("STOPPED by the budget"), "{report}");
    assert!(report.contains("1 task run(s) not started"), "{report}");
    // A stopped run isn't counted as a failure.
    assert!(report.contains("1/1 (100%)"), "{report}");
    // Its verdict row still lands in the ledger.
    let verdicts = rows
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.call_kind == RESULT_KIND)
        .count();
    assert_eq!(verdicts, 2);
}

#[test]
fn the_dry_run_lists_every_run_and_a_worst_case_against_the_caps() {
    let dir = tempfile::tempdir().unwrap();
    let s = suite(dir.path(), THREE);
    let profile = ferrule_eval::variant::windowed(&HarnessProfile::generic(), Some(32_000));
    let text = ferrule_eval::plan::render(&ferrule_eval::plan::PlanInput {
        suite: &s,
        tags: &[],
        tasks: &[],
        variants: &[Variant::Engineered, Variant::Naive],
        repeat: 1,
        profile: &profile,
        provider: "p",
        model: "m",
        pricing: Some(ferrule_eval::Pricing {
            input: 1.0,
            cached_input: 0.1,
            output: 5.0,
        }),
        caps: Caps {
            max_usd: Some(5.0),
            max_tokens: None,
        },
    })
    .unwrap();
    assert!(text.contains("6 task run(s)"), "{text}");
    assert!(text.contains("alpha") && text.contains("gamma"), "{text}");
    assert!(text.contains("worst-case cost: $"), "{text}");
    assert!(text.contains("budget cap: $5.00"), "{text}");
}
