# M29 — edit mechanics

**Status.** Design, 2026-09-26. Branch `m29-edit-mechanics`. The user guide
is `docs/editing.md`.

Until M29, the agent had two ways to change a file. `write_file` rewrote
the whole file. `shell` ran `sed`, heredocs or `python -c`. Both are
expensive and fragile on large files: a whole-file rewrite costs the file's
tokens twice and silently drops anything the model forgot, while `sed`
fails on quoting and gives no diff. M29 adds four things. They are listed
by how often they're used:

1. `edit_file`: SEARCH/REPLACE hunks, all or nothing, with errors that say
   what to try next.
2. A **repo map** and `code_search`, built on tree-sitter. The map gives the
   model a ranked outline of the code without spending reads. The search
   tool finds definitions and references with their line numbers.
3. A **per-edit lint** that runs after `edit_file`/`write_file`. It is a
   built-in M18 PostToolUse hook.
4. Optional **atomic commits** of the agent's own changes (`[agent]
   auto_commit`), with `ferrule undo` and `/undo`.

Research background: `docs/research-number-one-harness-strategy.md` §4
item 14 (Aider's edit formats and repo map, Claude Code's `Edit`, and
SWE-agent's lint-on-edit).

## 1. `edit_file`

### Shape

```json
{"path": "src/lib.rs",
 "edits": [{"search": "fn old() {\n    1\n}", "replace": "fn new() {\n    2\n}"}]}
```

A single edit can also be given at the top level (`search`/`replace`
instead of `edits`). Models write it that way often enough that refusing it
would waste a round-trip. Hunks apply **in order**, each to the text the
previous hunks left. The file is written only if every hunk applied. That
is the same contract as Claude Code's MultiEdit, and it lets a later hunk
edit text an earlier hunk inserted.

### Matching: one ladder, never fuzzy on content

For each hunk, the rungs are tried in order. The first rung that finds
**any** match decides the hunk:

| Rung | What may differ | How the replacement goes in |
|---|---|---|
| 1. exact | nothing | verbatim |
| 2. trailing whitespace | spaces/tabs (and a stray `\r`) at line ends | verbatim, replacing the matched whole lines |
| 3. indentation | a **uniform** leading-indent offset: every non-blank SEARCH line, minus SEARCH's common indent, plus one file prefix P, equals the file line | REPLACE with its common indent swapped for P |

- Exactly one match: the hunk applies.
- More than one match: the hunk fails as *ambiguous*. There is no drop to a
  looser rung, because a looser rung only matches more.
- No match on any rung: the hunk fails as *not found*.

Why these rungs and no more:

- Trailing whitespace is invisible in a `read_file` result, so the model
  cannot copy it faithfully.
- Indentation is the most common way a model's copy drifts. It re-indents
  when quoting from memory, or when the block moved into or out of a
  nesting level. The offset has to be *uniform*, and the replacement is
  re-indented by the same offset. So what's inserted has the shape the
  model wrote, at the depth the file has.
- Anything looser (edit distance, token similarity, "closest block") can
  apply a hunk to the wrong place, and it does so silently. Aider measured
  this: its fuzzy fallback produced wrong edits that looked right. A failed
  hunk costs one round-trip. A misapplied hunk costs a debugging session,
  or ships. So content is never fuzzy.

Blank lines inside SEARCH must match blank lines, and they may carry any
whitespace (rungs 2–3). Rungs 2–3 work on whole lines. A SEARCH that starts
or ends mid-line only matches exactly (rung 1).

### Errors that say what to do

Every failure names the hunk (`edit 2 of 3`), says **nothing was written**,
and gives the next step:

- **Not found.** Shows the closest region of the file, with line numbers:
  the window of SEARCH's length that shares the most trimmed lines with
  SEARCH. It says "re-read those lines with `read_file` and copy them
  exactly, or use a shorter SEARCH that is unique". If the REPLACE text is
  already in the file exactly once, it adds "it looks already applied".
  This is the usual cause after a retry.
- **Ambiguous.** Says "matches N places (lines a, b, c)", shows the first
  few locations, and says "add a line or two of surrounding context to
  SEARCH so it matches once".
- An empty SEARCH on an existing non-empty file is refused, with the reason
  and the fix: to prepend or append, SEARCH the first or last lines.
- The file is missing and SEARCH is non-empty: the error says to create the
  file with an empty SEARCH or with `write_file`.

### Files: line endings, encodings, BOM

- **Line endings.** The file is CRLF when every `\n` in it is preceded by
  `\r`. Then matching runs on the text with `\r\n` turned into `\n`, SEARCH
  and REPLACE are normalised the same way, and the result is written back
  as CRLF. Mixed files are matched raw. Rung 2 treats a stray `\r` as
  trailing whitespace, and replaced lines take `\n`.
- **Encoding.**
  - UTF-8 with or without BOM, and UTF-16 LE/BE with a BOM, are decoded and
    written back in the same encoding with the same BOM.
  - Any other bytes are handled as **Latin-1**: one byte is one character.
    So every byte outside the edit is kept exactly, whatever the real
    encoding (Latin-1, cp1252, a stray invalid byte in otherwise-UTF-8
    text). A REPLACE with a character above U+00FF is refused there,
    because it can't be written back without guessing an encoding.
  - A file with NUL bytes and no UTF-16 BOM is binary and is refused.
- **Atomic.** The new bytes go to a temp file in the same directory
  (`.<name>.ferrule-<random>`), which is flushed, given the original's
  permissions, and renamed over the target. A crash leaves the old file or
  the new one, never half of each. Through a symlink, the target is the
  resolved file (as `write_file`), and the link itself stays.
- **Policy.** The path goes through the same `resolve` as the other file
  tools. It must be inside the workspace on real paths, so a symlink out
  of the workspace is an escape. It must also be outside M26's deny list
  (ferrule's private data, credential dirs, `deny_read`). `edit_file` reads
  before it writes, so it obeys the *read* policy as well as the
  workspace boundary.

### Creating a file

An empty SEARCH on a missing (or empty) file creates it with REPLACE.
Parent directories are created. Later hunks in the same call then apply
to that text.

### Result

The result is a unified diff (2 lines of context), capped at 60 lines with
"… N more lines". It starts with `edited <path>: N hunks, +a −b lines`,
plus which rung a hunk needed if it wasn't exact
(`edit 2: matched ignoring indentation`). The model sees what landed
without re-reading the file.

### Scheduling

`changes_files() = true` and `read_only() = false`. So it is a barrier in
M27's parallel batches, it triggers the verify check, it's removed in plan
mode and in ReadOnly sandboxes, and read-only children don't get it (same
as `write_file`).

### `write_file` stays, in every profile

Decision: every harness profile (kimi, openai, anthropic, generic) is
offered both tools, and the guidance lives in the **tool descriptions**,
not the system prompt.

- `write_file`: "Create a new file or replace a whole file. To change part
  of an existing file use edit_file — cheaper and safer." Its write is now
  atomic too.
- `edit_file`: "Change part of an existing file with exact SEARCH/REPLACE
  blocks…".

Why:

- Profiles differ in context window, compaction and reasoning retention.
  None of that says a model can't rewrite a file. Weaker and local models
  (the `generic` profile) fail SEARCH exactness more often, and
  `write_file` is their way out. No measurement says to take it from any
  profile. `--edit-tools write-only` in the eval (§5) is how to get one.
- The system prompt stays byte-identical, and M27's pinned prefix test
  holds. The guidance also sits next to the tool it's about, where every
  provider sends it.
- `[agent] edit_file = false` removes the tool, for a provider that
  misbehaves with it.

## 2. Repo map and `code_search`

### Parsing

New crate `ferrule-codemap`, on tree-sitter 0.25 with our own compact tag
queries (definitions and references). One Cargo feature per language, all
on by default:

| Feature | Grammar | Files |
|---|---|---|
| `lang-rust` | tree-sitter-rust | `.rs` |
| `lang-python` | tree-sitter-python | `.py`, `.pyi` |
| `lang-typescript` | tree-sitter-typescript (TS and TSX) | `.ts`, `.mts`, `.cts`, `.tsx`, and `.js`, `.jsx`, `.mjs`, `.cjs` |
| `lang-go` | tree-sitter-go | `.go` |
| `lang-java` | tree-sitter-java | `.java` |

- **JavaScript** is parsed by the TSX grammar, which is a superset for
  anything a tag query cares about. That saves a whole grammar in the
  binary.
- **C#** is out: its grammar alone is several MB of parser tables (the
  brief said "if cheap"; it isn't).
- Building `ferrule-cli --no-default-features` without the grammars leaves
  `code_search` as text search and the map off.

Size is measured on the release artifacts (§7).

### What's extracted

For each file:

- **Definitions**: name, kind and 1-based line. Kinds: function, method,
  class, struct, enum, trait, interface, type, module, const.
- **Referenced identifiers**: calls, type uses, and imports' leaf names.

The map is **top-level symbols**: items and their methods. Locals are
never extracted.

### Ranking: Aider-style PageRank

The graph has one node per file. For every identifier that file A
references and file B defines, there is an edge A→B with weight
`√(refs) × m`. The multiplier `m` is:

- ×10 if the identifier is mentioned in the conversation;
- ×10 for a long (≥ 8 chars) snake/camel name;
- ×0.1 for a `_private` name;
- ×0.1 if more than 5 files define the name (it carries little
  information).

PageRank runs with damping 0.85 and 30 iterations. The personalisation
vector is uniform, except that files mentioned in the conversation (path or
unique basename) weigh 100/N. Each file's rank is split over its outgoing
edges onto (file, identifier) definitions, and definitions are ranked by
that. Ties break by path and then line, so equal input gives equal output,
byte for byte.

"The conversation" means the current request plus the text of the last 20
messages.

### Rendering and budget

```
[Repo map: ranked outline of this repository — definitions with line numbers. Use code_search/read_file for detail.]
src/agent.rs
  42│pub struct Agent
  810│pub async fn run(&mut self, goal: &str, …)
src/tool.rs
  …
```

Each definition prints its first source line, trimmed to 100 chars. Files
come in rank order, and definitions within a file in line order. The map
is trimmed to the budget (tokens ≈ chars / 4). A binary search finds the
most top-ranked definitions that fit. `[agent] repo_map_tokens` defaults
to 1024, and 0 turns the map off.

### When it's on

The workspace must look like a code repo: a VCS dir (`.git`, `.hg`, `.jj`)
**or** a manifest (`Cargo.toml`, `package.json`, `pyproject.toml`,
`setup.py`, `go.mod`, `pom.xml`, `build.gradle(.kts)`), **and** at least 3
parseable source files. A home dir or a folder of CSVs gets no map and no
`code_search` schema.

The walk:

- respects `.gitignore`/`.ignore` (the `ignore` crate, which parses them;
  **git is never run**, since a repo's `core.fsmonitor` would execute);
- skips hidden dirs and M26's deny list;
- caps at 20 000 files and 1 MiB per file.

### Placement: a user message, only when it changes

The map is volatile: it changes with the files and with what the
conversation mentions. M27 pinned the system prompt's bytes so the provider
caches the prefix, and a map in the system prompt would throw that cache
away on every edit. So:

- A new core seam, `TurnContext` (next to `SessionRecall`), is asked at
  the start of every run, with the goal and the history. Its answer is
  pushed as a user message **after** the goal and the recalled memory. This
  is the same seam M27 used for memory.
- The agent keeps the last text it injected. An answer equal to it adds
  nothing. A turn with no file changes and no new mentions therefore adds
  no bytes, and the whole previous request stays a prefix of the next.
  That property is pinned by a test.
- When the map does change, the new one is *appended*. It never edits
  history, so everything before it still caches. The header says it
  supersedes earlier maps. Compaction summarises old maps like anything
  else.

The alternatives were weaker:

- Refreshing the system prompt on "real" ranking changes still invalidates
  the entire cache on every edit.
- Putting the map in the goal message would change the goal, which
  sessions, `/goal` and compaction key off.

The map is not refreshed inside a run's tool loop. `code_search` is always
current, and the next turn gets the new map.

### Cache

`<data dir>/repomap/<fnv64(workspace)>.json` holds each file's
`(size, mtime, fnv64 content hash) → tags`.

- A refresh stats every file. Unchanged `(size, mtime)` reuses the tags. A
  changed stat re-reads and re-hashes the file, and only a changed hash
  re-parses it.
- Writes go through a temp file and a rename. A corrupt or old-format
  cache is discarded, never trusted.
- The data dir is ferrule's own, outside the tools' reach
  (`private/`-style hiding isn't needed: tags are derived from files the
  agent can read anyway). The eval puts the cache in the run's state dir.

### `code_search`

```json
{"query": "run_inner", "kind": "definitions|references|all", "path": "crates/ferrule-core", "max_results": 50}
```

- It is read-only (`read_only() = true`, `changes_files() = false`), so it
  runs in M27's parallel batches.
- For supported files it answers from the tags: `path:line  def fn
  run_inner — <line>`, or `ref`. Definitions come first. The tags are
  refreshed (cheaply, as above) on each call.
- Other text files (Markdown, YAML, shell, C, …) get a **text search**:
  whole-word matches for an identifier-looking query, substring otherwise,
  case-sensitive.
- Output is capped at `max_results`. The first line gives the count, e.g.
  "`12 results (3 definitions, 9 references)`".
- It is registered under the same "looks like a code repo" rule. Plain
  `shell grep` covers other workspaces, and there the schema would be
  tokens for nothing.

## 3. Per-edit lint (a built-in PostToolUse hook)

`LintHook` implements M18's `HookHandler`. It is added as
`HookSource::Builtin` with the matcher `edit_file|write_file`, so it runs
in the ordinary hook pipeline: after user hooks of the same event, it's
inherited by children, and it's audited. The loop has no special case. It
reads `tool_input.path`, skips failed calls, and picks a linter by
extension:

| Files | Linter | Runs when (`lint = "auto"`) |
|---|---|---|
| `.rs` | `rustfmt --check --edition 2021 <file>` | a `Cargo.toml` is in the workspace |
| `.py` | `ruff check --quiet <file>` | a `ruff.toml`/`.ruff.toml`, or `[tool.ruff]` in `pyproject.toml` |
| `.go` | `gofmt -l -e <file>` | a `go.mod` |
| `.js .jsx .ts .tsx .mjs .cjs` | `eslint --no-color <file>` | an eslint config file |
| `.ts .tsx` | `tsc --noEmit -p <dir of tsconfig.json>`, filtered to the edited file | a `tsconfig.json` |

- The binary is looked up in the workspace's `node_modules/.bin` (for
  eslint and tsc) and then on `PATH`.
- It runs **in the sandbox** (`Sandbox::command`), with a timeout
  (`[agent] lint_timeout_secs`, default 10). The process group is killed on
  timeout.
- Output: stdout and stderr, the first 40 lines. When the linter exits
  non-zero or prints anything, it's returned as `additionalContext`, e.g.
  `lint (ruff check): …` ("problems in the file after your edit; some may
  predate it"). M18 appends that to the tool result.
- A clean result adds nothing.
- A timeout adds one line saying so.
- **A missing linter is silent.** `ferrule doctor` lists each linter as
  found or not.

**The default is `lint = "auto"`**. A linter runs only if it's installed
**and** the project has adopted it (the config file in the table). Why:

- A linter the project doesn't use reports style the project doesn't
  follow. That noise teaches the model to "fix" code nobody asked it to
  touch.
- A project that ships a `ruff.toml` or `tsconfig.json` has declared its
  standard, so the report is signal.
- Cost when on: well under a second for formatters, and the timeout bounds
  `tsc`.

`lint = "off"` disables it. The eval doesn't wire the CLI's hooks, so its
numbers can't move.

## 4. Optional atomic commits

```toml
[agent]
auto_commit = false                          # default
auto_commit_branch = "new"                   # or "current"
auto_commit_author = "ferrule <ferrule@localhost>"
```

This is for unattended runs (tasks, `ferrule run`). With it on, each run
that changed files ends in one commit of **exactly the files the agent
changed**. The turn is an audit record and can be undone.

### Which files are the agent's

At run start, if the workspace is a git work tree with a commit, a
`RunObserver` (new core seam around `Agent::run`, fired on every ending:
done, step limit, error, stop) snapshots two things. The first is
`git status --porcelain=v1 -z --untracked-files=all`. The second is a
content hash of every path it lists.

At run end, a path is the agent's when it is dirty now and **wasn't** dirty
at the start. A path that was already dirty at the start is never
committed, even if the agent changed it too: the owner's uncommitted work
there can't be told apart from the agent's. If its content hash changed
during the run, it's listed as "left uncommitted: `x` (you had changes
there)". So the owner's dirty work is never swept into an agent commit.
A test pins this.

### Committing

The commit is made with `git add -A -- <agent paths>` and then
`git commit --only -- <agent paths>`:

- `--only` commits those paths through a temporary index, and the owner's
  staged changes elsewhere stay staged.
- Settings: `-c user.name/-c user.email` for the committer, `--author` from
  the config, and `-c commit.gpgsign=false` (no pinentry in an unattended
  run).
- The subject is the request's first line (≤ 72 chars). The body holds the
  first lines of the answer and a `Ferrule-Auto-Commit: <session>` trailer.
  `undo` keys on that trailer.

### Git runs in the sandbox, and the repo's hooks apply

Every git command, including `undo`, runs through `Sandbox::command`.
Git reads things the agent can write: `.git/hooks`, `.git/config`
(`core.fsmonitor`, filters, `core.hooksPath`). Run outside the sandbox,
those would let the agent execute as the owner. Inside the sandbox they
have exactly the power the agent's `shell` already had.

The repo's own `pre-commit`/`commit-msg` hooks do run, so the project's
commit policy applies to the agent too. A hook that rejects the commit
leaves the changes in the working tree, and the run's note says why.

### Branch policy

- **Default, `"new"`.** At the first commit, if HEAD isn't already on a
  `ferrule/*` branch, ferrule creates `ferrule/auto-<yyyymmdd-hhmmss>` at
  HEAD and switches to it (`git switch -c`, which keeps the working tree
  and index). The owner's branch never gets an agent commit, and later
  runs in the session keep committing on the agent branch. The owner
  reviews and merges it like any other branch.
- **`"current"`.** Commits on whatever HEAD is on. That's the explicit
  opt-in, meant for a workspace dedicated to the agent.

A worktree was rejected. The agent's workspace path is fixed when its
session is built, so moving it mid-session breaks every path in the
transcript. Unattended runs also want their result where the owner looks
for it. M12 already gives children isolated worktrees.

A detached HEAD in `"current"` mode, or an unborn repo, or a merge or
rebase in progress: no commit, and the note says why.

### Never pushes

No code path runs `push`, `fetch` or `remote`. A test with a bare `origin`
checks that its refs are unchanged after commits and undos.

### Undo: `ferrule undo` and `/undo` in `ferrule chat`

`undo` reverts HEAD when **all** of these hold:

- HEAD carries the `Ferrule-Auto-Commit` trailer;
- HEAD has a parent;
- none of HEAD's files differ from HEAD in the index or the working tree
  ("nothing else changed since").

Then:

1. Move the branch back with a compare-and-swap:
   `update-ref HEAD HEAD~1 <sha>`.
2. Restore each of the commit's files from the parent into the index and
   the working tree.
3. Delete the files the commit added.

Edits the owner made elsewhere are untouched. Otherwise it refuses and
names the reason and the file. Repeated undos walk back successive agent
commits.

### Reporting

The observer's result goes to `AgentEvent::Notice`. Chat and `ferrule run`
print it to stderr, e.g. `[auto-commit] 3f2a1c9 on ferrule/auto-20260926-1402: 2 files`.
Task runs log it.

## 5. Eval

Hold these fixed:

- `--variant ab` stays engineered 20/20 and naive 11/20.
- The suite, the graders and the mock stay unchanged.

The mock scripts `shell`/`read_file` calls, so it **never calls
`edit_file`** or `code_search`. No starter fixture qualifies as a code repo
(at most 2 source files, and only `git-fix-commit` has git, with one), so
none gets the map or `code_search`.

What changes is the tool list. Both variants now also carry `edit_file`'s
schema, and `write_file`'s description is longer. The mock prices only the
messages (`prompt_chars // 4`), so its token and cost numbers can't move.
A real provider pays for the schemas on every call. The exact per-call
delta is given in `docs/editing.md` once built.

`ferrule eval run … --edit-tools both|write-only` (default `both`)
compares `write_file` alone against `edit_file` offered, with everything
else the same. The real-model command and its expected cost are in
`docs/editing.md`. Running it is Max's call.

## 6. Threat model

| Threat | What holds |
|---|---|
| `edit_file` on `../x`, an absolute path out, or a symlink out | `resolve` on real paths, the same as the other file tools; tested |
| `edit_file` on `~/.ssh/…` or ferrule's keys inside a workspace at `~` | M26 deny list in `resolve` (read policy, not only write); tested |
| A symlink swapped in between check and rename | Same residual as `write_file`. The rename replaces the path it resolved; the sandbox, not the tool, holds against a hostile workspace |
| A crash mid-write corrupts a file | Temp file plus rename in the same dir |
| A hunk applied somewhere unintended | No fuzzy matching. Ambiguity fails. The diff is shown |
| A planted `.git/hooks` or `core.fsmonitor` runs as the owner during auto-commit or undo | All git runs in the sandbox. The repo map never runs git |
| A planted `node_modules/.bin/eslint` | Linters run in the sandbox, with a timeout |
| The owner's work swept into an agent commit | Paths dirty at run start are never committed; tested |
| Agent commits land on the owner's branch | Default is a new `ferrule/*` branch; `current` is opt-in |
| Code leaves the machine | Never push. The map and cache stay in the data dir |
| Repo map as a prompt-injection vector | The map contains only file paths and each definition's first line, which the model could `read_file` anyway. No new capability |
| A huge repo makes each turn slow | 20k-file / 1 MiB caps, stat-only refresh, parse in `spawn_blocking` |

## 7. Failure modes

- **A tree-sitter parse error** (a partial file, or an unsupported
  dialect). Tags come from what parsed, since tree-sitter is
  error-tolerant. A panic or timeout in a grammar skips the file.
- **Cache unreadable.** It's rebuilt. The map is best-effort, and a
  failure means no map, never a failed turn.
- **A linter that hangs.** It's killed at the timeout, with a one-line
  note.
- **Git missing or the repo broken during auto-commit.** No commit, and
  the note says why. The run's result is unaffected.
- **Undo with changed files.** Refused, with the file named.
- **Release size.** The grammars add parser tables. The five release
  archives are measured against v0.3.0 on this branch's workflow_dispatch
  run. If the growth is out of proportion, the grammars sit behind default
  features that a packager can drop.

## 8. Out of scope

- Fuzzy or LLM-assisted patch application, and Aider's "udiff" format.
- Whole-repo semantic search (embeddings).
- LSP integration (go-to-definition via a language server).
- Auto-fixing lint output (`ruff --fix`, `rustfmt` in place). The model
  decides.
- Pushing, opening PRs, and squashing agent commits.
- A C# grammar, and tree-sitter grammars loaded at runtime (wasm).
- Running the real-model comparison. The command is documented, and Max
  runs it.
