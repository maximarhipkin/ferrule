# M42 — harness engineering, applied to ourselves

**Status.** All seven parts built (2026-09-30).

**Source.** A pass over
[learn-harness-engineering](https://github.com/walkinglabs/learn-harness-engineering)
(14 lectures, 8 projects) and its four frontier harness breakdowns —
Claude Code, Codex, DeepSeek Harness, Pi — scored against ferrule's own
harness. Ferrule already shipped most of the course's five subsystems
(hooks with the 10-event contract, `verify_command` as an independent
judge, sub-agents with worktree isolation, structured compaction,
resumable transcripts, the ledger, OTel, the eval A/B, the learning
loop). This milestone closes the gaps the comparison actually found.

## Part 1 — the repo gets its own harness (done)

Ferrule reads `AGENTS.md` in every workspace it runs in, but its own repo
had none, and no single command that means "the repo is green."

- `AGENTS.md` at the repo root, a directory page in Codex's sense (~100
  lines: the verify commands, the hard invariants — compaction never cuts
  a tool call from its results, secrets never reach the model, the
  sandbox stays on by default, truthful statuses, byte-stable prompt
  prefix — and pointers to PLAN.md, CONTRIBUTING.md and docs/).
- `Makefile`: `make check` = fmt-check + clippy + the workspace suite.
  It is the consistent-state predicate an agent (or a person) runs before
  every commit.

## Part 2 — layered context baseline (done)

`load_context_baseline` read one file at the workspace root, first match
won. Claude Code and Pi both load instructions hierarchically: broadest
first, most specific last in the prompt.

`ferrule_core::load_context_baseline_layered(workspace, global_dir)` now
merges: the instance's config dir (user level, e.g.
`~/.config/ferrule/AGENTS.md`), then every parent directory of the
workspace from the root down, then the workspace itself. Per directory
the first of `AGENTS.md / CLAUDE.md / GEMINI.md / ferrule.md` wins; each
directory is read once; the 16k cap is spent on the most specific layers
first; several layers are headed `# From <path>`. A lone workspace file
returns exactly what the flat loader did (the eval fixture keeps the flat
loader, for determinism). Subdirectory baselines loaded on demand — the
second half of Claude Code's design — are still open: they belong to the
file tools, not the session start.

## Part 3 — layered verification and `run --verify` (done)

The course's L09: verification is layered (syntax → unit → e2e), run in
order, each failure fed back. And L13's `/goal`: a goal, a verification
method, a stopping condition — the person writing the code doesn't grade
their own homework.

- `[agent] verify_command` accepts one string (unchanged) or a list run
  in order: `verify_command = ["cargo fmt --check", "cargo test"]`. Each
  entry is its own built-in Stop check; the first failure goes back to
  the model naming the command that failed, the rest are skipped. Works
  over SSH workspaces too (one `RemoteVerifier` per command).
- `ferrule run --verify CMD` (repeatable) overrides the config for that
  run. Together with the existing budget caps and `--max-iterations`,
  that is the goal loop's three parts from the command line: the prompt
  is the goal, `--verify` is the independent judge, the budget is the
  stopping condition.

## Part 4 — model-visible means logged (done)

DeepSeek Harness's strongest constraint: anything that reaches a model
request must be reconstructable from the session log, asserted by a
runtime invariant. Ferrule's transcripts were append-only but compaction
broke the rule twice: the summary message reached every later request
without ever being logged, and a resume replayed the *un-compacted*
history (the fold existed only in RAM, so a long session resumed into an
already-overfull window).

- New `fold` transcript record: `{type: "fold", message: <summary>,
  kept: N}` — the summary that replaced all but the last N logged
  messages. The folded messages stay in the file (append-only;
  `search_history` reads them, unchanged).
- `read_messages()` applies folds: a resume now replays the compacted
  state. `read_all_logged()` is the append-only view for audits.
- The invariant has a test (`everything_the_model_saw_is_in_the_transcript`,
  `crates/ferrule-core/tests/it/history.rs`): across a compacting run,
  every message of every recorded provider request — except the system
  prompt, rebuilt per agent by design — appears verbatim in the log.

## Part 5 — goal loops, whole (done)

Part 3's `--verify` was the one-shot slice; this is the full form, from
L13: a loop that keeps working a goal across sessions until the judge
passes or the budget runs out, with its state on disk.

- `ferrule run --goal --verify CMD "the goal"` starts a loop: the prompt
  is the goal, the `--verify` checks (or `[agent] verify_command` when no
  flag is given) are the independent judge, the budget
  (`--max-iterations`, the caps) is the stopping condition. A goal
  without a judge is refused up front.
- Loop state lives in `<data>/sessions/<sid>.goal.json`: goal, judge
  commands, judge runs so far, the judge's latest failing word,
  timestamps. Every verdict is recorded as it happens — the judge's word
  is the loop's memory across sessions.
- The judge runs at finish **whether or not the run changed files**
  (new `AgentConfig::verify_without_changes`, set only for goal loops):
  a goal like "all tests green" can be met by a run that edits nothing,
  and an idempotent judge proves it.
- A run the budget cuts short ends `goal pending` (exit 2, as any
  incomplete run) with the resume command printed; `ferrule run --resume
  <sid>` continues with the folded transcript plus a prompt carrying the
  judge's last word and any extra owner guidance. A met goal prints
  `goal met` with the judge-run count.
- The maker-checker rule is in the system prompt: the agent is told the
  judge — not it — decides done-ness, and to finish and let the judge
  speak.

Still open from the L13 design: `--schedule` on a goal loop (compose
with the cron scheduler for timer-driven retries), and a `goal list` /
`goal abandon` admin surface.

## Part 6 — graph routing over ferrule-agents (done)

L14: once a task needs specialization, parallelism, shared state,
verification and recovery, it has stopped being a loop — it's a graph of
nodes, edges, shared state and routing. `ferrule graph run FILE` walks a
declarative graph over the M12 primitives — and the walker is
deterministic Rust, never a model: it routes, it doesn't decide the work.

```toml
goal = "implement feature X with tests green"

[[nodes]]
id = "implement"
task = "Implement: {{goal}}\n\nWhat came back:\n{{prev}}"

[[nodes]]
id = "verify"
kind = "check"            # the deterministic judge, in the sandbox
command = "cargo test"

[[nodes]]
id = "ship"
kind = "approval"         # parks for the owner (--yes auto-approves)
message = "Green. Ship?"

[[edges]]
from = "implement" to = "verify"
[[edges]]
from = "verify" to = "implement" on = "fail"   # the rollback edge
[[edges]]
from = "verify" to = "ship" on = "pass"
```

- **Nodes** are `agent` (worker/planner/verifier children, spawned and
  waited through the supervisor, worktree-isolated, per-node `model`,
  `timeout_secs`, `max_attempts`), `check` (a command ferrule runs
  itself, exit 0 is a pass — the same `CommandVerifier` as
  `verify_command`) or `approval` (a human gate).
- **Edges** carry `on: pass | fail | always`. A feedback arc set is
  computed greedily in declaration order: a fail edge back to an earlier
  node is a rollback that re-fires it (capped by its `max_attempts`), and
  the check's failure output rides back in `{{prev}}` as the repair
  instruction.
- **Fan-out/fan-in**: several edges out of one node run their agents in
  parallel; a node with several inputs runs only when every one is
  satisfied (the batch waits for *all*, not the first).
- **Verifier agents** end with `VERDICT: PASS|FAIL`; anything unclear
  fails closed.
- **Success**: `succeed_when = "<node>"`, or every terminal node (no
  non-feedback way out) passing. The report is truthful per node:
  pass/fail/never ran, exit 0 or 2.
- **Caps**: per-node `max_attempts`, per-node timeouts (a straggler is
  closed, not waited on forever), a global `max_steps`, and the
  supervisor's own tree limits underneath.

The orchestration tax rule from L14 holds: one node and one check stay
exactly `verify_command`'s job — a graph is opt-in per task, never the
default path.

Still open: a run's own resumable state (a cut graph restarts), gateway
channels starting graphs, `graph list`/`graph stop` for long ones, and
approval routed to the owner's chat instead of the terminal.

## Part 7 — pluggable compaction and a session tree (done)

Pi's two state-layer ideas, mapped to ferrule:

- **Compaction as a strategy.** `ferrule_core::compactor::Compactor` is
  the seam (`summarize(transcript_text) -> summary`); the agent keeps the
  built-in checklist pipeline on its own model when none is set, and the
  fold record doesn't change either way. The first strategy is
  `TemplateCompactor` — the same template on a *different* model, wired
  as `[agent] compaction_model = "fast"` (resolved and validated at agent
  build, routed through the model catalog with fallbacks). Pi's other
  examples (topic-based folding, a custom prompt) are now one trait impl
  away. A strategy's calls don't get ledger rows yet (the agent's
  `call_provider` path, which owns ledger/retries, stays the default
  path's).
- **Session tree.** A transcript forks: `Transcript::open` reads a
  session without writing to it, `fork(session_id, at)` writes a new
  transcript with `parent`/`fork_at` meta and the fold-applied messages
  up to `at` (all of them when unset). `ferrule chat --fork SESSION
  [--at N]` starts a branch from that state — the parent is never
  touched — and `ferrule sessions` lists the tree, newest first, a branch
  showing `↳ <parent>`. Archived (`/new`) sessions show with `--all`.

Still open: forking from a *pre-compaction* point (the folded past is in
the log but the fork view is fold-applied), `chat --resume`, and a TUI
tree view.
