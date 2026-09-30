# M42 — harness engineering, applied to ourselves

**Status.** Parts 1–4 built this session (2026-09-30); parts 5–7 are
designs, not started.

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

## Part 5 — goal loops, whole (design)

Part 3's `--verify` is the one-shot slice. The full form, from L13: a
loop that keeps working a goal across sessions until the judge passes or
the budget runs out, with its state on disk.

- `ferrule run --goal "..." --verify CMD`: on a failed verify at the
  iteration or time cap, the run ends in a `goal-pending` status (not a
  failure), and `--resume` picks it up with the folded transcript and the
  loop state (attempts, last failure tail).
- Loop state as a small JSON beside the session transcript: goal, verify
  commands, attempts, spend so far, last judge output. The scheduler's
  cron tasks are the timer-loop half already; a `--schedule` on a goal
  loop composes the two.
- The maker-checker split is already the verifier sub-agent role (M12);
  a goal loop names it: the judge is never the agent doing the work.

## Part 6 — graph routing over ferrule-agents (design)

L14: once a task needs specialization, parallelism, shared state,
verification and recovery, it has stopped being a loop — it's a graph of
nodes, edges, shared state and routing. Ferrule has the nodes (supervisor
+ roles), the shared state (the board), and the isolation (worktrees).
What it lacks is explicit edges.

- A small declarative graph over the existing primitives, not a new
  framework: `[[graph.nodes]]` (role, workspace, budget) and
  `[[graph.edges]]` (`on: pass | fail | always`, `to: node`). The
  supervisor walks it; the verifier role's result is the pass/fail
  signal; a fail edge back to the implementer is the rollback edge.
- Parallel fan-out/fan-in: several worker nodes from one edge, the join
  node runs when all report (the board already carries their results).
- Human-approval node: an edge that parks the graph until an owner
  approval arrives through the channel — the approvals flow exists, this
  makes it a node.
- The orchestration tax is real (L14's own warning): one node and one
  verify edge must stay exactly as cheap as today's `verify_command`.
  A graph is opt-in per task, never the default path.

## Part 7 — pluggable compaction and a session tree (design)

Pi's two state-layer ideas, mapped to ferrule:

- **Compaction as a strategy.** Today's pipeline (dedupe → checklist
  summary, goal pinned verbatim) is hard-coded in `agent.rs`. Make it a
  trait with the built-in as the default: a `PreCompact` hook (M18) may
  already observe; a strategy would let a config choose *how* — e.g. a
  different (cheaper) model for the summary, or topic-based folding. The
  fold record from part 4 is strategy-agnostic, so the log format doesn't
  change.
- **Session tree.** Transcripts are linear with resume; `/new` (M41)
  starts fresh. Pi stores sessions as trees: branch from any turn. The
  fold record already makes transcripts a structured log; a `parent` +
  `fork_at` meta pair on a transcript plus `ferrule chat --fork SESSION
  --at N` gives branching without touching the per-session file format
  (each branch is its own file, pointing back).
