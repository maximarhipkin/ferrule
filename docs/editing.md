# Editing code with ferrule

How the agent changes files, finds its way around a repository, checks
its edits and, if you ask, commits them. The design and the reasons behind
each choice are in `docs/m29-edit-mechanics.md`.

| What | On by default | Turn it off / on |
|---|---|---|
| `edit_file` (SEARCH/REPLACE) | yes | `[agent] edit_file = false` |
| `write_file` | yes, in every profile | (read-only sandboxes and plan mode drop both) |
| Repo map | in a code repo | `[agent] repo_map_tokens = 0` |
| `code_search` | in a code repo | (no switch; it's absent outside code repos) |
| Per-edit lint | when the project has adopted the linter | `[agent] lint = "off"` |
| Auto-commit and undo | no | `[agent] auto_commit = true` |

## `edit_file`

The model changes part of a file by quoting the text to replace:

```json
{"path": "src/lib.rs",
 "edits": [{"search": "fn old() {\n    1\n}", "replace": "fn new() {\n    2\n}"},
           {"search": "old();", "replace": "new();"}]}
```

- **All or nothing.** Edits apply in order, each to the text the previous
  one left. The file is written only if every edit applied. A crash
  mid-write leaves the old file or the new one (temp file plus rename).
- **Exact, never fuzzy.** A SEARCH must match exactly one place. The only
  slack is invisible trailing whitespace and a uniform indentation offset
  (the replacement is re-indented to match). A SEARCH that matches twice
  fails as ambiguous rather than picking one.
- **Errors say what to do.** "Not found" shows the closest region of the
  file with line numbers, and notices when the edit is already in the
  file. "Ambiguous" lists the matching lines. Nothing is written on any
  error.
- **The file keeps its shape.** CRLF stays CRLF. UTF-8 (with or without a
  BOM) and UTF-16 with a BOM are written back as they came. Any other
  bytes are kept exactly, byte for byte, outside the edit. Binary files
  are refused.
- **Creating a file.** An empty SEARCH on a missing file creates it.
- **Result.** A unified diff of what landed, capped at 60 lines, so the
  model doesn't need to re-read the file.
- **Same rules as the other file tools.** Inside the workspace on real
  paths, and never in the sandbox's deny list (`docs/sandbox.md`).

### `write_file` stays

Every harness profile (anthropic, openai, kimi, generic) is offered both
tools. The advice lives in the tool descriptions: `write_file` is for new
files and whole rewrites, `edit_file` for changing part of a file. Weaker
and local models miss SEARCH exactness more often, and `write_file` is
their way out. `write_file`'s writes are now atomic as well.

For a provider that misbehaves with the tool:

```toml
[agent]
edit_file = false
```

## Repo map and `code_search`

Both appear only when the workspace looks like a code repository: a
`.git`/`.hg`/`.jj` dir or a manifest (`Cargo.toml`, `package.json`,
`pyproject.toml`, `setup.py`, `go.mod`, `pom.xml`, `build.gradle`), **and**
at least 3 source files. A home directory or a folder of CSVs gets neither,
and pays nothing for them.

- **The repo map** is a ranked outline: the definitions most relevant to
  what the conversation mentions, each with its line number and first
  line. It's added as a message at the start of a turn, only when it
  changed, so the provider's prompt cache is kept. Its size:

  ```toml
  [agent]
  repo_map_tokens = 1024   # 0 = no map; code_search stays
  ```

- **`code_search`** finds where a symbol is defined and used:
  `path:line  def fn run_inner — <line>`, definitions first. It
  understands Rust, Python, TypeScript/JavaScript, Go and Java (tree-sitter).
  Other text files get a whole-word text search. It is read-only, so it
  runs in parallel with other reads.

The walk respects `.gitignore`, skips hidden dirs and the sandbox's deny
list, and stops at 20 000 files and 1 MiB per file. Git is never run for
it. Parsed tags are cached in ferrule's data dir and refreshed by file
size and mtime.

## Per-edit lint

After each `edit_file`/`write_file`, ferrule runs the project's own linter
on the file and appends what it reports to the tool result:

| Files | Linter | Runs when the project has |
|---|---|---|
| `.rs` | `rustfmt --check` | a `Cargo.toml` |
| `.py` | `ruff check` | `ruff.toml`, `.ruff.toml` or `[tool.ruff]` in `pyproject.toml` |
| `.go` | `gofmt -l -e` | a `go.mod` |
| `.js .jsx .ts .tsx .mjs .cjs` | `eslint` | an eslint config, or `eslintConfig` in `package.json` |
| `.ts .tsx` | `tsc --noEmit` (lines about the edited file) | a `tsconfig.json` |

- A linter runs only if it's installed (`node_modules/.bin`, then `PATH`)
  **and** the project has its config between the file and the workspace
  root. A linter the project doesn't use would only teach the model to
  "fix" style nobody asked for.
- It runs in the sandbox with a timeout. A clean result adds nothing; a
  missing linter is silent. `ferrule doctor` lists which ones it found.
- It's a built-in PostToolUse hook (`docs/m18-hooks.md`), so it runs
  before your own hooks and is audited like them.

```toml
[agent]
lint = "auto"            # or "off"
lint_timeout_secs = 10
```

## Auto-commit and undo

For unattended runs (tasks, `ferrule run`). Off by default.

```toml
[agent]
auto_commit = true
auto_commit_branch = "new"                    # or "current"
auto_commit_author = "ferrule <ferrule@localhost>"
```

With it on, each run that changed files ends in one git commit of
**exactly the files the agent changed**:

- **Your uncommitted work is never swept in.** Files that were already
  dirty when the run started are never committed, even if the agent
  touched them too. The note lists those as "left uncommitted".
- **Your branch is left alone.** By default the first commit creates
  `ferrule/auto-<date>-<time>` at HEAD and switches to it, and later runs
  keep committing there. Review and merge it like any other branch.
  `"current"` commits on the branch you're on: use it for a workspace
  dedicated to the agent.
- **The commit** is `[agent] <first line of the request>`, with the answer's
  first lines in the body and a `Ferrule-Auto-Commit: <session>` trailer.
  Your staged changes elsewhere stay staged.
- **Git runs in the sandbox**, and the repo's own `pre-commit` and
  `commit-msg` hooks apply. A rejected commit leaves the changes in the
  working tree, and the note says why.
- **Nothing is ever pushed.**
- **No commit** outside a git work tree, before the repo's first commit,
  during a merge or rebase, or on a detached HEAD in `"current"` mode.
  Sub-agents never commit.

The result is printed to stderr, e.g.
`[auto-commit] 3f2a1c9 on ferrule/auto-20260926-1402: 2 files`.

### Undo

```bash
ferrule undo                    # in the workspace, or --workspace DIR
```

In `ferrule chat`, type `/undo`. Either one takes back the latest agent
commit: the branch moves back to its parent, and the commit's files are
restored (files it added are removed). It refuses when HEAD isn't an agent
commit or one of its files changed since, and names the file. Your other
edits are untouched. Repeated undos walk back successive agent commits.

## Measuring it: `--edit-tools`

`ferrule eval run … --edit-tools both|write-only` (default `both`) compares
the two ways of changing files, with everything else the same.
`write-only` takes `edit_file` away from every variant.

**The mock model never calls `edit_file`** (nor `code_search`). It plays
scripted `shell`/`read_file`/`write_file` solutions. So against the mock
both modes give the same result, and it only proves the pipeline:

```
                        engineered       naive
  pass rate             20/20 (100%)     11/20 (55%)
  model calls           88               62
total: 150 calls, 951.5k input + 6.2k output tokens, cost $0.98
```

That's the starter suite, `--variant ab`, from the real binary, identical
with `--edit-tools both` and `--edit-tools write-only`. No starter fixture
is a code repo, so none gets the repo map or `code_search`.

**What the schemas cost a real model.** The mock prices only the messages,
so its numbers can't move. A real provider is sent the tool schemas on
every call. Against v0.3.0, each call now carries:

| Schema | Tokens per call (chars / 4) |
|---|---|
| `edit_file` (new, 820 chars) | ~205 |
| `write_file`'s longer description (160 chars, was 73) | ~22 |
| **Total, both variants, every starter task** | **~227** |
| `code_search` (673 chars), only in a code repo | ~168 |

On the starter suite that's about 150 calls × 227 ≈ 34k more input tokens
per `--variant ab` run, most of it served from the provider's prompt cache
after the first call of each task. `--edit-tools write-only` gives the old
tool list except for `write_file`'s description.

**The real-model comparison** (Max's call, with a provider set up as in
`docs/eval.md` step 3):

```bash
ferrule --config eval-hosted.toml eval run evals/starter --variant engineered --edit-tools write-only --repeat 3 --dry-run
ferrule --config eval-hosted.toml eval run evals/starter --variant engineered --edit-tools write-only --repeat 3 --max-usd 5
ferrule --config eval-hosted.toml eval run evals/starter --variant engineered --edit-tools both       --repeat 3 --max-usd 5
```

The second run's "since the last run" section compares the two. Expected
cost: the mock's engineered side used ~0.5M input tokens per pass over the
suite. At gpt-5-mini's prices ($0.25 in / $2 out per million) a real model
using 1–3× that costs about $0.15–0.50 per pass, so **about $1–3 for the
pair with `--repeat 3`**. A frontier model at $3 / $15 costs roughly ten
times that. `--dry-run` prints the worst case, and `--max-usd` caps each
run. The saved run doesn't record `--edit-tools` yet, so note which run
was which.

What to look at: pass rate, tokens per pass (a whole-file rewrite costs
the file twice), and failed checks fixed. The starter files are small,
so expect the gap to be smaller here than on a real repository.
