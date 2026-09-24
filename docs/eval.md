# `ferrule eval`

`ferrule eval` runs a suite of real tasks against a model and grades each
result with a command. Run with `--variant ab`, it runs every task twice
with **the same model**: once with ferrule's harness, and once with a
deliberately naive harness. It then reports the difference in pass rate,
tokens and cost. The design and its reasoning are in
[`m14-eval.md`](m14-eval.md).

## Handoff: running the real A/B (for the agent that runs it)

M14 shipped the machinery and proved it only against the mock model. The
real numbers are still to be measured. Do it in this order and stop at the
first step that goes wrong:

1. **Build from a fresh `main`:** `git checkout main && git pull`, then
   `cargo build -p ferrule-cli`. The binary is `target/debug/ferrule`.
2. **Check the pipeline with the free mock** (step 1 below). Expect
   engineered 4/4 and naive 2/4 on the smoke subset. If you get something
   else, the build or the platform is off (see "On macOS"), not the
   harness. Report it and don't spend any money.
3. **Dry run on the real model:** add `--dry-run` to the step 2 command
   (Ollama) or the step 3 command (hosted). Check the model name, the
   context window and the cap before spending anything.
4. **Smoke A/B with a small cap:** `--tag smoke --variant ab`, with
   `--max-usd 2` for a hosted model or `--max-tokens 2000000` for Ollama.
   Exit status 3 means the cap stopped the run. Raise the cap only if
   the owner agrees.
5. **Full suite:** drop `--tag smoke`. Use `--repeat 3` so the numbers
   aren't one lucky run. The default cap is $5 / 20M tokens. Ask the
   owner before going above $10.
6. **Where the results land:**
   - the report goes to stdout;
   - the report, transcripts and `run.json` are saved under
     `~/Library/Application Support/ferrule/eval/<run id>/` on macOS and
     `~/.local/share/ferrule/eval/<run id>/` on Linux;
   - `ferrule eval report starter` prints the latest saved run again;
   - every call is in `ferrule ledger` with `task_shape = "eval"`.
7. **Report back:**
   - the full report text
   - provider, model and context window
   - the exit status
   - anything that broke on macOS, since none of it has run there yet

   Commit no API keys and no local `eval-*.toml` files. The config reads
   keys from the environment (`api_key_env`).

## Run the A/B in 5 minutes

You need `ferrule` (`cargo install --path crates/ferrule-cli`, or use
`target/debug/ferrule` after `cargo build`), plus `python3`, `git` and
`sh`: the starter suite's graders are Python scripts. Run the commands
below from the repository root.

### 1. Try the whole path with no model (1 minute, free)

`evals/starter/mock/model.py` is a stand-in model: a small OpenAI-compatible
server using only the Python standard library. It plays the suite's
reference solutions with the same blind spots a real model has. It shows
that the pipeline works. It doesn't measure anything.

```bash
python3 evals/starter/mock/model.py --port 8765 &      # prints its URL
cat > eval-mock.toml <<'EOF'
default_provider = "mock"

[providers.mock]
base_url = "http://127.0.0.1:8765/v1"
api_key_env = "MOCK_KEY"
model = "mock"
price_input_per_mtok = 1.0          # made-up prices, so the report shows a cost
price_cached_input_per_mtok = 0.1
price_output_per_mtok = 5.0
EOF
MOCK_KEY=x ferrule --config eval-mock.toml eval run evals/starter --tag smoke --variant ab
kill %1
```

### 2. A local model through Ollama

```bash
ollama pull qwen3-coder                 # or any model you already have
OLLAMA_CONTEXT_LENGTH=32768 ollama serve   # see the note on context length below
```

```toml
# eval-ollama.toml
default_provider = "ollama"

[providers.ollama]
base_url = "http://localhost:11434/v1"
api_key_env = "OLLAMA_API_KEY"        # Ollama ignores it; it just has to be set
model = "qwen3-coder"
```

```bash
export OLLAMA_API_KEY=ollama
ferrule --config eval-ollama.toml eval run evals/starter --tag smoke --variant ab --dry-run
ferrule --config eval-ollama.toml eval run evals/starter --tag smoke --variant ab
```

`--model NAME` switches to another pulled model without editing the file.
Both variants always use the same model.

**Context length:** the starter suite manages every model as if it had a
32k-token window (`context_window = 32000` in `suite.toml`). The naive
harness truncates, and ferrule's harness compacts, at that size. Ollama
must accept that much, and its default context is smaller. If the prompt
doesn't fit, Ollama drops the front of it *silently*, which hurts both
variants and hides the difference. So start the server with
`OLLAMA_CONTEXT_LENGTH=32768`, or pass `--context-window` with what your
server really allows. Local models have no prices in the config, so only
the token cap applies (see below).

### 3. A hosted model

Any OpenAI-compatible endpoint works: OpenAI, OpenRouter, Kimi, DeepSeek,
Groq, and so on. Add your provider's prices to get a cost column and the
dollar cap.

```toml
# eval-hosted.toml
default_provider = "openai"

[providers.openai]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
model = "gpt-5-mini"
profile = "openai"
# USD per million tokens: copy these from your provider's price page.
# These three numbers are placeholders.
price_input_per_mtok = 0.25
price_cached_input_per_mtok = 0.025
price_output_per_mtok = 2.0
```

```bash
export OPENAI_API_KEY=sk-...
ferrule --config eval-hosted.toml eval run evals/starter --tag smoke --variant ab --dry-run
ferrule --config eval-hosted.toml eval run evals/starter --tag smoke --variant ab --max-usd 2
```

A provider that is already in your normal ferrule config works as-is:
`ferrule eval run evals/starter --tag smoke --variant ab --provider NAME`.
Drop `--tag smoke` to run all 20 tasks.

### Dry run first: the worst-case cost

`--dry-run` lists the task runs and prices the worst case: every step used,
and every call a full context window. It sends nothing to the model.

```
dry run — suite starter (capability), mock via mock, context window 32.0k; nothing is sent to the model

  fix-median [smoke,code] — median() in stats.py returns the wrong value for lists with an even nu
      graders: command; steps ≤ 40; engineered checks with its verify command
  sales-summary [smoke,data] — From sales.csv, write summary.json. For each region, total the revenue
      graders: command; steps ≤ 40
  slugify [smoke,verify,code] — Create slug.py with a function slugify(title: str) -> str that turns a
      graders: command; steps ≤ 40; engineered checks with its verify command
  release-notes [smoke,context] — Write RELEASE_NOTES.md for our 4.2.0 release from the commit logs in l
      graders: command; steps ≤ 25

8 task run(s): 4 task(s) × engineered + naive × 1 repeat(s)
worst case (every step used, every call a full window): 443 calls, 14.18M input + 3.54M output tokens
  worst-case cost: $31.90
  typical runs use a small fraction of this: most tasks finish in 5-20 steps
budget cap: $5.00, 20.00M tokens (the suite stops cleanly at the first call past either)
```

The worst case is a ceiling, not an estimate. Real smoke runs use a few
percent of it. The mock's smoke A/B used about 200k tokens.

### The budget cap

Every run has a cap. It defaults to **$5 and 20M tokens**, whichever comes
first. Change it with `--max-usd X` and `--max-tokens N` (0 turns that cap
off). The dollar cap needs the provider's three `price_*` keys; without
them ferrule says so and only the token cap applies. As soon as a model
call takes the spend past a cap, the task in progress is stopped. It is
still graded, shown as stopped, and left out of the pass rates. Tasks that
haven't started are skipped. The report is still printed and saved, and
it ends with `STOPPED by the budget: … N task run(s) not started.`.
`ferrule eval` then exits with status **3**. With `--variant ab`
each task runs under both variants before the next one starts, so a
stopped run still compares like with like.

### What the report looks like

This is the smoke A/B against the mock model, from the real binary. Progress
lines go to stderr; the report goes to stdout.

```
▶ fix-median--engineered
  ✓ fix-median--engineered: pass (2 calls, 564 tokens)
▶ fix-median--naive
  ✓ fix-median--naive: pass (2 calls, 342 tokens)
  …
▶ release-notes--naive
  ✗ release-notes--naive: fail (6 calls, 78898 tokens)

ferrule eval — suite starter (capability), mock via mock, context window 32.0k, run 20260924T153312-a8ae1c

  task            engineered         naive
  fix-median      pass               pass
  sales-summary   pass               pass
  slugify         pass (1×check)     FAIL
  release-notes   pass (4×compact)   FAIL (2×trunc)

                        engineered      naive           engineered − naive
  pass rate             4/4 (100%)      2/4 (50%)       +50 pts
  errors                0               0
  input tokens          116.2k          79.5k           +36.7k
  output tokens         740             497             +243
  model calls           19              12              +7
  cost                  $0.12           $0.08           +$0.0379
  tokens per pass       29.2k           40.0k
  context events        4 compactions   2 truncations
  failed checks fixed   1               0

total: 31 calls, 195.7k input + 1.2k output tokens, cost $0.20

why they failed:
  slugify (naive): rules: ASCII only (accents folded: é -> e), apostrophes dropped, '&' is 'and', other punctuation separates words, no leading/trailing/double hyphens, at most 60…
  release-notes (naive): RELEASE_NOTES.md: [Errno 2] No such file or directory: 'RELEASE_NOTES.md'
transcripts and this report: ~/.local/share/ferrule/eval/20260924T153312-a8ae1c
```

How to read it:
- `(1×check)`: ferrule's verify command failed once, the agent got the
  output back, and fixed the problem.
- `(N×compact)`: ferrule compacted the context N times and kept the
  request.
- `(N×trunc)`: the naive harness dropped its oldest messages N times, and
  the request went with them.
- `tokens per pass`: all the tokens the variant spent, divided by the
  tasks it passed.
- `why they failed`: the first line of each failed grader's output.

The report is saved as `report.txt` next to `run.json` (every number
above, per task run) and one transcript per task run. The directory is
`<data dir>/eval/<run id>/`: `~/.local/share/ferrule` on Linux,
`~/Library/Application Support/ferrule` on macOS, or `$FERRULE_DATA_DIR`.
Every model call also lands in the ledger (`ferrule ledger`) with
`task_shape = "eval"` and a session id of `<run id>/<task>--<variant>`,
so eval spend never mixes with normal use.

### Since the last run

Every report ends with what changed since the previous saved run of the
same suite, with the same kind and model, variant by variant:

```
since the last run (same suite, kind and model):
  naive vs run 20260924T160102.418-3f9c21: pass rate 2/2 (100%) → 1/2 (50%), -50 pts; tokens +1.2k; cost +$0.0011
    NEWLY FAILING: sales-summary (task changed)
```

- `NEWLY FAILING` and `newly passing` list the tasks whose verdict
  flipped. A task counts as passed only if every graded repeat passed;
  runs the budget stopped don't count either way.
- `(task changed)`: the task's entry in `suite.toml` (prompt, grader and
  check commands, …) or its fixture files differ from last time (a
  fingerprint of them is saved per run), so the flip may be the suite's
  doing, not the model's. Scripts the commands call, such as
  `graders/*.py`, are not part of the fingerprint.
- `new` and `not run this time`: tasks only one of the two runs had.
- The first run of a suite says `no earlier run to compare with`.

The comparison is read from the saved `run.json` files, not the ledger.
To print a saved run again, with its diff:

```sh
ferrule eval report                  # the latest run of any suite
ferrule eval report starter          # the latest run of this suite
ferrule eval report starter --run 20260924T15   # a run id or a prefix of one
```

### Exit status

- `0`: the suite ran. In a `capability` suite (like the starter one),
  failed tasks are results, not errors.
- `1`: a grader couldn't decide (for example it crashed or timed out), a
  task failed under the engineered variant in a `regression` suite, or
  ferrule itself failed. (Naive is the baseline: its failures never fail
  the run.)
- `3`: the budget cap stopped the suite.

### On macOS

The commands above are the same on macOS, with some caveats:
- `python3` is the Xcode Command Line Tools one (`xcode-select --install`)
  or Homebrew's.
- The Ollama app serves on the same port. For the context length, set
  `launchctl setenv OLLAMA_CONTEXT_LENGTH 32768` and restart the app, or
  quit it and run `ollama serve` as above.
- The workspaces live under `$TMPDIR` (`/var/folders/…`, which is really
  `/private/var/folders/…`).
- The default sandbox on macOS is Seatbelt; it applies to both variants.

**None of this has been run on macOS or Windows yet.** It is unverified
until the batch CI pass. What's most likely to differ:
- the Python graders and `sh` scripts (Windows has no `sh`)
- the `/private` symlink in workspace paths
- git in the fixtures
- killing a grader that times out

## What the A/B compares

Both variants get the same model, tools, sandbox, context window, step
limit and task prompt. Only the harness differs:

| | engineered (ferrule) | naive (the baseline) |
|---|---|---|
| context full | compacts, and keeps the request verbatim | drops the oldest messages (the system prompt and the last message stay) |
| when to act on it | at 70% of the usable window (profile default) | when the window is full |
| finishing | runs the task's `check`; a failure goes back to the agent | just stops |
| system prompt | ferrule's, with the profile's directive and the validation policy | one line |
| memory, skills | yes (the fixture's own skills only) | no |
| stuck detector, retries | yes | off, and 1 attempt |

The naive harness stands for what ferrule replaces. It isn't a straw man:
truncating from the front is what most simple agent loops do.

## The starter suite (`evals/starter`)

There are 20 tasks, each with a fixture, a hidden grader (in `graders/`
or `checks/`, never in the workspace) and a reference solution (in
`solutions/`). The test `crates/ferrule-eval/tests/starter_suite.rs`
checks three things for every task: the reference solution passes, an
untouched workspace fails, and the plausible first try fails.

| tag | tasks | what it tests |
|---|---|---|
| `smoke` | fix-median, sales-summary, slugify, release-notes | one of each kind; the 5-minute A/B |
| `context` | release-notes, config-migration, incident-report, rename-api, inventory-merge | Rules given at the start, then 110–150k characters to read. That's more than the 32k window holds, so the request only survives if the harness keeps it. |
| `verify` | slugify, parse-duration, invoice-tests, roman | The prompt leaves out rules that the check enforces. Passing takes running the check and fixing what it reports. |
| `code` | fix-median, rename-api, todo-json-flag, git-fix-commit, wordfreq, lru-cache, … | everyday edits |
| `data` | sales-summary, incident-report, inventory-merge, active-buyers, csv-to-markdown, normalize-dates, repair-json, dedupe-contacts | file transformations |

Against the mock, the full suite comes out at 20/20 engineered and 11/20
naive. The nine naive failures are exactly the context and verify tasks,
by construction. A real model will score differently in both columns, and
finding out how is the point of running it.

## Writing a suite

A suite is a directory with a `suite.toml`:

```toml
[suite]
name = "mine"
kind = "regression"          # or "capability": whether a failed task fails the run
context_window = 32000       # optional: manage the model as if it had this window
max_iterations = 40
timeout_secs = 900

[[task]]
id = "fix-median"
tags = ["smoke", "code"]
fixture = "fixtures/fix-median"      # copied into a fresh workspace per run
setup = "python3 gen.py"             # optional, runs in the workspace
git = false                          # true: the workspace is a git repo with one commit
check = "python3 -m unittest -q"     # the engineered variant's verify command
prompt = "…"
[task.grade]
command = 'python3 "{suite_dir}/graders/fix-median.py"'   # exit 0 = pass
```

`{suite_dir}` is replaced with the suite's absolute path in `setup`,
`check` and `grade.command`. Commands run in the task's workspace. The
full format, including `files` for inline fixtures and per-task overrides,
is in [`m14-eval.md`](m14-eval.md#suite-file-format).

### Grading with a rubric

When exit codes can't capture it (is the summary accurate? is the error
message helpful?), give the task a rubric, one criterion per line. It can
replace `command` or come with it. With both, the task has to pass both.

```toml
[task.grade]
command = "python3 -m unittest -q"
rubric = """
- CHANGELOG.md has an entry for the new --dry-run flag
- the error for a missing config file names the path it looked for
"""
```

A judge model reads the task's prompt, the rubric and the evidence
ferrule collects itself: every file the run added, changed or deleted (up
to 12k characters each, 60k in all; `.git` is left out) and the command
grader's output. The agent's final answer goes in too, but marked as its
claims, not as evidence. The judge answers in JSON, one entry per
criterion. ferrule then decides:
- The task passes only if every criterion is met.
- A criterion counts as met only if the judge quotes the evidence and the
  quote really is in the evidence. The quote needs 4 characters or more,
  and runs of whitespace are ignored in the comparison. A quote taken
  from the agent's own answer doesn't count.
- A reply that isn't the asked-for JSON, or has the wrong number of
  entries, is an `error` for that task, not a pass or a fail.

The judge is the model under test unless you pass
`--judge-provider NAME` (a `[providers.*]` entry, with its configured
model). The report says which model judged, and flags a self-judged run.
Each judge call is one call at temperature 0. It shows up in the ledger
with `call_kind = "judge"` and counts against the budget. The dry run
counts one judge call per rubric task run. The starter suite grades with
commands only.

Other flags:
- `--task ID` (repeatable) runs single tasks.
- `--repeat N` runs each task N times per variant.
- `--context-window N` squeezes both variants further.
- `--keep` leaves the workspaces on disk and prints their path.
