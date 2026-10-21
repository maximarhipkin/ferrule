//! Loop detection over a run's tool calls, after OpenHands' `StuckDetector`
//! (docs.openhands.dev/sdk/guides/agent-stuck-detector). A model that
//! repeats itself would otherwise burn every remaining iteration and fail
//! anyway; catching the loop early leaves room to change course, or to stop
//! with a status the owner can act on.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// One tool call and what came back, reduced to what comparisons need.
#[derive(Debug, Clone)]
pub struct Step {
    tool: String,
    action: u64,
    observation: u64,
    ok: bool,
}

impl Step {
    pub fn new(tool: &str, arguments: &serde_json::Value, output: &str, ok: bool) -> Self {
        Self {
            tool: tool.to_string(),
            action: hash(&(tool, arguments.to_string())),
            observation: hash(output),
            ok,
        }
    }

    fn same(&self, other: &Step) -> bool {
        self.action == other.action && self.observation == other.observation
    }
}

fn hash<T: Hash + ?Sized>(value: &T) -> u64 {
    let mut h = DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}

/// What the tail of the run looks like when it's going in circles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stuck {
    /// The same call, with the same arguments, got the same result each time.
    Repeating { tool: String, times: usize },
    /// The same call failed each time.
    Failing { tool: String, times: usize },
    /// Two calls, taking turns, each getting the same result as before.
    Alternating { first: String, second: String },
}

const REPEAT: usize = 4;
const FAIL: usize = 3;
const ALTERNATE: usize = 6;

impl Stuck {
    /// Checks the most recent steps; `None` while the run still makes
    /// progress.
    pub fn detect(steps: &[Step]) -> Option<Stuck> {
        let last = steps.last()?;
        if steps.len() >= REPEAT && steps[steps.len() - REPEAT..].iter().all(|s| s.same(last)) {
            return Some(Stuck::Repeating {
                tool: last.tool.clone(),
                times: REPEAT,
            });
        }
        if steps.len() >= FAIL
            && steps[steps.len() - FAIL..]
                .iter()
                .all(|s| !s.ok && s.action == last.action)
        {
            return Some(Stuck::Failing {
                tool: last.tool.clone(),
                times: FAIL,
            });
        }
        if steps.len() >= ALTERNATE {
            let tail = &steps[steps.len() - ALTERNATE..];
            let (a, b) = (&tail[0], &tail[1]);
            if !a.same(b)
                && tail
                    .iter()
                    .enumerate()
                    .all(|(i, s)| s.same(if i % 2 == 0 { a } else { b }))
            {
                return Some(Stuck::Alternating {
                    first: a.tool.clone(),
                    second: b.tool.clone(),
                });
            }
        }
        None
    }

    /// Told to the model the first time: what it's repeating and what to do
    /// instead.
    pub fn nudge(&self) -> String {
        let what = match self {
            Stuck::Repeating { tool, times } => {
                format!("The last {times} calls to `{tool}` used the same arguments and got the same result.")
            }
            Stuck::Failing { tool, times } => {
                format!("The last {times} calls to `{tool}`, with the same arguments, all failed.")
            }
            Stuck::Alternating { first, second } => {
                format!("You keep alternating between `{first}` and `{second}` and getting the same results.")
            }
        };
        format!(
            "[ferrule] {what} Doing it again won't change the outcome. Try a different approach, \
             or if you can't make progress, stop and say what is blocking you."
        )
    }

    /// Why the run was stopped, for the status answer and the logs.
    pub fn reason(&self) -> String {
        match self {
            Stuck::Repeating { tool, .. } => {
                format!("it kept repeating the same `{tool}` call after being warned")
            }
            Stuck::Failing { tool, .. } => {
                format!("the same `{tool}` call kept failing after being warned")
            }
            Stuck::Alternating { first, second } => {
                format!("it kept alternating between `{first}` and `{second}` after being warned")
            }
        }
    }
}

/// M25's no-progress counts, kept only while the provider routes: invalid
/// tool calls in a row, and the same call in a row. A call is the same
/// when its tool and arguments are, as [`Stuck`] compares them
/// ([`Step`]'s action). Two rules differ from [`Stuck`]. The result
/// doesn't count here. A move up a tier starts both counts again, so the
/// next tier gets the same allowance.
#[derive(Debug, Default)]
pub struct Streaks {
    misfits: u32,
    last: Option<u64>,
    repeats: u32,
}

impl Streaks {
    /// Counts `step`, whose call didn't fit its tool when `misfit`:
    /// invalid calls in a row, and the same call in a row, after it.
    pub fn record(&mut self, step: &Step, misfit: bool) -> (u32, u32) {
        self.misfits = if misfit { self.misfits + 1 } else { 0 };
        self.repeats = match self.last {
            Some(last) if last == step.action => self.repeats + 1,
            _ => 1,
        };
        self.last = Some(step.action);
        (self.misfits, self.repeats)
    }

    /// The turn moved up a tier: both counts start again.
    pub fn moved(&mut self) {
        self.misfits = 0;
        self.repeats = 0;
    }
}

/// M57.2: how many times one file may be changed in a run, with no passing
/// check between, before the model is told to step back.
pub const EDITS_PER_FILE: usize = 6;

/// M57.2: a run's successful edits per file, after LangChain's per-file
/// loop detection. [`Stuck`] catches the same call repeated; this catches
/// a file fixed over and over, a different edit each time, which is how
/// a wrong approach usually looks.
#[derive(Debug, Default)]
pub struct EditCounts {
    counts: std::collections::HashMap<String, usize>,
    warned: std::collections::HashSet<String>,
}

impl EditCounts {
    /// A call to `tool` that worked. When it changed a file (`edit_file`,
    /// `write_file`), it's counted, and the note to add comes back the
    /// first time that file reaches [`EDITS_PER_FILE`].
    pub fn record(&mut self, tool: &str, arguments: &serde_json::Value) -> Option<String> {
        if !matches!(tool, "edit_file" | "write_file") {
            return None;
        }
        let path = arguments["path"].as_str()?.trim();
        let path = path.strip_prefix("./").unwrap_or(path);
        if path.is_empty() {
            return None;
        }
        let n = self.counts.entry(path.to_string()).or_default();
        *n += 1;
        let n = *n;
        (n >= EDITS_PER_FILE && self.warned.insert(path.to_string())).then(|| {
            format!(
                "[ferrule] You have changed `{path}` {n} times in this run with no passing check in between. \
                 Before you edit it again, step back: re-read the file and the last error, and ask whether \
                 the approach is right. Many small fixes to one file usually mean the plan is off."
            )
        })
    }

    /// A check passed: the counts start again.
    pub fn checked(&mut self) {
        self.counts.clear();
        self.warned.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(tool: &str, arg: u32, out: &str) -> Step {
        Step::new(tool, &json!({ "n": arg }), out, true)
    }

    fn failed(tool: &str, arg: u32, out: &str) -> Step {
        Step::new(tool, &json!({ "n": arg }), out, false)
    }

    #[test]
    fn progress_is_not_stuck() {
        let steps: Vec<Step> = (0..10)
            .map(|i| step("shell", i, &format!("out {i}")))
            .collect();
        assert_eq!(Stuck::detect(&steps), None);
        assert_eq!(Stuck::detect(&[]), None);
    }

    #[test]
    fn four_identical_calls_with_identical_results() {
        let mut steps = vec![step("shell", 1, "same"); 3];
        assert_eq!(Stuck::detect(&steps), None);
        steps.push(step("shell", 1, "same"));
        assert_eq!(
            Stuck::detect(&steps),
            Some(Stuck::Repeating {
                tool: "shell".into(),
                times: 4
            })
        );
    }

    #[test]
    fn a_changing_result_is_progress() {
        // Polling that sees something new each time is fine.
        let steps: Vec<Step> = (0..6)
            .map(|i| step("shell", 1, &format!("{i} of 10 done")))
            .collect();
        assert_eq!(Stuck::detect(&steps), None);
    }

    #[test]
    fn three_failures_of_the_same_call() {
        let steps = vec![
            failed("read_file", 1, "no such file"),
            failed("read_file", 1, "no such file (2)"),
            failed("read_file", 1, "x"),
        ];
        assert_eq!(
            Stuck::detect(&steps),
            Some(Stuck::Failing {
                tool: "read_file".into(),
                times: 3
            })
        );
        // Different arguments each time: exploring, not stuck.
        let steps = vec![
            failed("read_file", 1, "e"),
            failed("read_file", 2, "e"),
            failed("read_file", 3, "e"),
        ];
        assert_eq!(Stuck::detect(&steps), None);
    }

    #[test]
    fn two_calls_taking_turns() {
        let mut steps = Vec::new();
        for _ in 0..3 {
            steps.push(step("read_file", 1, "a"));
            steps.push(step("write_file", 2, "b"));
        }
        assert_eq!(
            Stuck::detect(&steps),
            Some(Stuck::Alternating {
                first: "read_file".into(),
                second: "write_file".into()
            })
        );
        steps.pop();
        assert_eq!(Stuck::detect(&steps), None);
    }
}
