# M14 — `ferrule eval`: design

Status: design, 2026-09-24. Builds on the roadmap's M14 section (commit
`ba6b0f5`, "the naive/engineered A/B design and the demonstration goal")
and §4.2 of `docs/research-number-one-harness-strategy.md`. Where this
document departs from either, it says so and why.

## What it's for

Two jobs, one mechanism:

1. **Regression-test the harness.** Every prompt, profile and threshold
   change can be checked against a fixed set of real tasks, and the report
   says what changed since the last run.
2. **Measure the harness itself.** The same model runs the same tasks
   twice: once with ferrule's engineered harness, once with a "naive"
   harness (the default behaviour ferrule replaced). The report compares
   pass rate, tokens and cost per variant. This is the ARC-AGI-3-style
   experiment from `ba6b0f5`, with ferrule's own numbers.

Max's priority (msg 3142): the A/B path lands first, as a working vertical
slice, and it runs against **any provider ferrule already supports**
(hosted models through his own keys, local models through Ollama). The
same model is used for both variants.

## Commands

```
ferrule eval run SUITE [--variant engineered|naive|ab] [--provider NAME] [--model M]
                       [--context-window N] [--tag T]… [--task ID]… [--repeat N]
                       [--max-usd X] [--max-tokens N] [--dry-run] [--keep]
ferrule eval report [SUITE_NAME] [--run RUN_ID]
```

- `SUITE` is a suite file (`evals/starter/suite.toml`) or a directory
  holding one.
- `--variant ab` runs both variants, task by task (engineered, then naive,
  for each task, so a budget stop leaves a fair comparison of the tasks
  that did run). Default `engineered`: the regression-testing use.
- `--provider`/`--model` pick the model exactly as `ferrule run` does
  (`[providers.*]` in the config); `--model` overrides the provider's model
  for this run only. Both variants always get the same provider and model.
- `--context-window N` runs both variants as if the model's window were
  `N` tokens (see "Context pressure on any provider" below).
- `--tag`/`--task` select a subset (`--tag smoke` is the few-minute subset
  of the starter suite).
- `--repeat N` runs each (task, variant) N times; pass rates count runs.
- `ferrule eval report` re-prints a run from the ledger (the latest by
  default), including its diff against the run before it.

## Suite file format

TOML, not YAML. The research note says YAML; ferrule has no YAML parser
and every other file it reads is TOML (`ferrule.toml`, the scheduler), so
TOML avoids a dependency and a second syntax. Multi-line prompts are
`"""` strings.

```toml
[suite]
name = "starter"
kind = "capability"          # or "regression"
description = "…"
max_iterations = 40          # per task, overridable per task
timeout_secs = 900           # wall clock per task run
context_window = 32000       # optional: see "Context pressure"

[[task]]
id = "fix-pagination"
tags = ["smoke", "code"]
prompt = """…"""
fixture = "fixtures/fix-pagination"     # directory copied into the workspace
files = { "NOTES.md" = "…" }            # extra inline files, optional
setup = 'python3 "{suite_dir}/gen/logs.py"'   # optional, runs in the workspace first
git = true                              # git init + one commit, optional
check = 'python3 -m unittest -q'        # the engineered variant's verify_command
max_iterations = 30

[task.grade]
command = 'python3 "{suite_dir}/graders/fix_pagination.py"'
rubric = """…"""                        # optional LLM rubric
timeout_secs = 120
```

- `{suite_dir}` in `setup`, `check` and `grade.command` becomes the suite
  file's directory (absolute, quoted by the author). Graders live **outside**
  the workspace, so the agent can't read or edit the tests it is graded by.
- `check` is what a real user would put in `[agent].verify_command`: the
  project's own test command. It is **not** the grader. Tasks where the
  check matters point it at a hidden test (`{suite_dir}/checks/…`), so the
  agent sees failures but not the test source.
- A task needs at least one grader (`command` and/or `rubric`).
- Unknown keys are an error, so a typo doesn't silently drop a grader.

## Fixture lifecycle

Per (task, variant, repeat):

1. A fresh directory `<tmp>/ferrule-eval/<run_id>/<task>--<variant>[--N]/`
   with `ws/` (the workspace) and `state/` (a private memory database and
   anything else the run needs outside the workspace). `<tmp>` is the OS
   temp dir, canonicalized (macOS's `/var` → `/private/var`), never the
   data dir: the sandbox hides the data dir from the agent.
2. `fixture` is copied in, then `files`, then `setup` runs in the
   workspace (a failing setup is an `error` result, not a model failure).
3. With `git = true`: `git init`, `git add -A`, one commit by
   `ferrule-eval <eval@ferrule.invalid>` (config passed with `-c`, so the
   user's git identity is never needed or used).
4. The agent runs. Its transcript goes to
   `<data_dir>/eval/<run_id>/<task>--<variant>.jsonl` (kept, small, what
   you read when a task fails).
5. Graders run in the workspace.
6. The directory is deleted, unless `--keep`.

Tasks run one at a time. Parallel runs would make the budget cap and the
ledger's order harder to reason about and would compete for a local
model's single GPU; it's an easy later addition.

## What each variant gets

Both variants: the same provider and model, the same tools
(`read_file`, `write_file`, `list_dir`, sandboxed `shell`, `web_fetch`,
`write_todos`, `log_diary`), the same sandbox, the same step limit and
the same context window.

| knob | engineered | naive |
|---|---|---|
| profile | the provider's profile (`kimi`, `openai`, `anthropic`, `generic`) | same, with `retain_reasoning = false`, `compaction_threshold = 1.0`, `system_directive = ""` |
| over the context budget | structured compaction at the profile's threshold, request kept verbatim, skills carried; since M15, old large tool results are first shortened to a ref (`search_history` fetches them back) | **rolling truncation**: drop the oldest non-system messages until under budget |
| `verify_command` | the task's `check` | none |
| memory tools (`remember`/`recall`; since M15 also `update_memory`/`forget`) | yes, on a fresh per-run database | no |
| `search_history` (M15) | yes, over the run's own transcript | no |
| transient-error retries | the default policy (4 tries) | none |
| stuck detector | on | off |
| system prompt | ferrule's full prompt: directive, validation policy, skills catalog of the fixture, AGENTS.md baseline | one line: "You are an autonomous agent… Workspace: …" |

The naive column is exactly `ba6b0f5`'s list. Two things it doesn't
state that I made explicit: (a) the naive system prompt is not just
"without the directive" but the one-line base, since the rest of
ferrule's prompt is harness work too (validation policy, skills, the
baseline); (b) the naive variant keeps `write_todos`/`log_diary`, because
they are ordinary tools any harness has, not memory.

**The truncation path** (`ContextOverflow::Truncate` in `AgentConfig`,
new): when the estimated context passes the trigger, drop messages from
the front, skipping the system prompt, until it fits. A tool result is
never left without the assistant message that called it: an assistant
message is dropped with its tool results, and orphaned tool results at
the new front are dropped too (OpenAI-style APIs reject them). The last
message is always kept. It emits `AgentEvent::Truncated` and logs to the
transcript. The goal is *not* pinned — that's the point.

**The stuck detector** gets an off switch (`AgentConfig::detect_stuck`),
also new. Everything else is an existing knob.

The engineered variant doesn't read the user's `[agent].verify_command`,
long-term memory, user-level skills, MCP servers or the browser: each
would make a run depend on the machine it ran on. It also doesn't start
the credential proxy (eval tasks don't need tool keys).

## Context pressure on any provider

The A/B only shows the harness effect when context runs out. With a
200k-token hosted model and small fixtures it never does, and both
variants behave the same. So the window is a run parameter:
`--context-window N` (or `[suite].context_window`) makes ferrule manage
the context of **both** variants as if the model had `N` tokens:
compaction for engineered, truncation for naive, at the same `N`. The
output reserve shrinks to `N/4` when the profile's reserve doesn't fit.
The model is unchanged.

For Ollama, `N` should match the server's real context length
(`OLLAMA_CONTEXT_LENGTH`); otherwise Ollama cuts the prompt itself,
silently, from the front — which is the naive behaviour for both
variants. `docs/eval.md` says this next to the commands.

The starter suite sets `context_window = 32000` (see "defaults to
confirm").

## Graders

**Command grader.** `grade.command` runs in the workspace, under the same
sandbox as the agent's shell, with its own timeout (default 300 s). Exit
0 passes. The tail of its output is kept in the result (4 000 chars).

**LLM-rubric grader.** A judge model grades the run against the rubric.
Kept honest by construction, not by trust:

- The judge sees the task prompt, the rubric, and **evidence ferrule
  gathered itself**: the list of changed files and their contents (capped,
  diffed against the fixture), and the command grader's output if there is
  one. The agent's final answer is included but fenced as untrusted data
  ("claims, not evidence").
- The judge answers JSON only: one entry per rubric line
  (`{"criterion", "met", "evidence"}`). **ferrule computes the verdict**:
  every criterion met. A `met` whose `evidence` quote doesn't appear
  verbatim in the evidence bundle counts as not met (a judge can't pass a
  run on a hallucinated quote). Output that isn't that JSON is a grader
  error, not a pass.
- Temperature 0. The judge is `--judge-provider` (default: the run's
  provider). The report marks a self-judged run as such.
- Judge calls are in the ledger (`call_kind = "judge"`) and count against
  the budget.

**Both graders**: the task passes only if every grader it names passes.

## Ledger schema additions

Every eval row has `task_shape = "eval"` and a new optional field:

```json
"eval": {"run_id": "…", "suite": "starter", "kind": "capability",
         "task": "fix-pagination", "variant": "naive", "repeat": 0}
```

- The agent's own calls (`call_kind` `turn`, `compaction`, `status`)
  carry it; so do judge calls (`call_kind = "judge"`).
- One **result row** per (task, variant, repeat): `call_kind =
  "eval_result"`, zero tokens in the per-call columns (so `ferrule ledger`
  never double-counts), `outcome` = `pass` | `fail` | `error` | `stopped`,
  and `eval.result` holding the verdict, each grader's result, the run's
  totals (calls, input/output tokens, cost), iterations, wall time, why it
  stopped early if it did, the ferrule version, and a hash of the task
  definition + fixture (so the diff can say "the task changed").
- `cost_usd` is filled by eval from `[providers.*].price_*` (the same
  numbers `ferrule ledger` uses); unpriced providers show `n/a`.
- `ferrule ledger` skips `eval_result` rows in its call totals.

Old ledgers parse unchanged (`eval` defaults to absent).

## Reports

**The A/B report** (`--variant ab`), printed at the end and by `ferrule
eval report`: one line per task with each variant's result, then per
variant: pass rate, input tokens, output tokens, cost, and the
engineered−naive difference.

**The diff against the last run.** For each variant, the most recent
earlier run of the same suite name, kind, variant and model: pass rate
before → now, newly passing, **newly failing** (called out), tasks whose
definition changed, tasks new or gone, and token/cost change. Runs of a
capability suite and a regression suite are never compared with each
other, and neither are different models.

**Exit codes**: 0 ran; 1 a regression suite had a failure in the variant
it gates (engineered), or any run had a grader error; 3 the budget
stopped the suite.

## Cost guards

- **A budget per suite run**: `--max-usd` (default $5, applies when the
  provider is priced) and `--max-tokens` (input+output, default 20 M, always
  applies). Every call is charged as its ledger row is written. Once a cap
  is passed, the running agent is stopped before its next call (no closing
  status call, which would spend more), that run is recorded `stopped`,
  and no further task starts. A suite can overshoot by at most one call.
- **`--dry-run`** prints the plan and spends nothing: every (task,
  variant, repeat) with its prompt's first line, graders, and a worst-case
  bound (step limit × (window + reserve) tokens, plus compaction and
  status calls, plus judge calls), priced when prices are configured, next
  to the cap. It checks that fixtures and grader scripts exist.

## Multi-agent runs (M12) in eval

**Not in M14.** Eval agents are built without `[agents]`, so
`spawn_agent` and friends aren't offered. Reasons:

- The A/B isolates the single loop's context handling, verification and
  prompts. Spawning adds a second, much bigger lever (15× the tokens in
  Anthropic's numbers) with no naive counterpart, and would swamp the
  effect being measured.
- Children run concurrently in worktrees and report asynchronously; a
  reproducible run needs a wait-for-the-tree step and per-child ledger
  attribution that eval doesn't have yet.

A later suite kind (`kind = "multi-agent"`) can opt in, reusing
`run_once`'s wait-for-the-tree loop; it is listed as an open edge, not
built.

## The starter suite

`evals/starter/`: ~20 real tasks (Python fixtures, Python graders, stdlib
only), each with a reference solution under `solutions/` that the test
suite uses to prove the grader passes a correct solution and fails the
untouched fixture. Tags:

- `smoke` — 4 tasks, a few minutes on a hosted model;
- `verify` — the prompt leaves out an edge case that the hidden `check`
  catches: passes only if the check makes the agent fix its own mistake;
- `context` — long multi-step work where a detail from the prompt is
  needed at the very end, after reading enough to fill the window;
- `code`, `data` — ordinary edits and data tasks.

Needs `python3` and `git` on PATH (macOS: Xcode command line tools or
Homebrew).

## Build order (parts)

1. **Core hooks**: truncation path, stuck switch, `Truncated` event,
   `eval` ledger field.
2. **`ferrule-eval` crate, A/B slice**: suite parsing, fixtures, command
   grader, variants, runner, budget, ledger rows, the A/B report; hermetic
   tests through the real loop with mock providers.
3. **CLI + starter suite**: `ferrule eval run` against any configured
   provider, `--dry-run`, the 20 tasks, `docs/eval.md` with "Run the A/B
   in 5 minutes"; an end-to-end test through the real binary.
4. **Diff against the last run** and `ferrule eval report`.
5. **LLM-rubric grader.**

## Defaults to confirm

- `--variant` defaults to `engineered`; the A/B is `--variant ab`.
- Budget defaults: $5 and 20 M tokens per suite run.
- The starter suite's `context_window = 32000`.
- Naive keeps `write_todos`/`log_diary`; its system prompt is one line.
- Eval never uses the user's memory, user-level skills, MCP servers,
  browser or credential proxy.
- Workspaces are deleted after grading; transcripts are kept.

## Open edges

- Multi-agent suites (above).
- Parallel task runs.
- The graders and solutions live outside the workspace but the sandbox
  doesn't restrict reads, so a determined agent could find them with
  `find /`. A read-restricted sandbox (M6's open edge) closes this.
- `est_tokens` is chars/4; a real tokenizer would make `--context-window`
  exact.
