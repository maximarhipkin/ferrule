# M18 — lifecycle hooks

A hook is a command the owner configures to run at a fixed point in an
agent's life: before a tool call, when the model says it is done, when a
sub-agent starts, and so on. The command gets a JSON description of the
moment on stdin. Its exit code and output can let things go on, block
them, or add a note for the model. `verify_command` becomes the first
built-in instance of the same mechanism.

The contract follows Claude Code's hooks (event names, the stdin payload's
keys, exit code 2, `additionalContext`, `decision: "block"`), which Codex
also follows, wherever that costs nothing. A hook script written for
Claude Code that reads `tool_name`/`tool_input` and exits 2 works here
unchanged.

**Hooks run as you, outside the sandbox.** A hook command has the owner's
full privileges: every file, the network, every secret in the owner's
environment. The sandbox confines what the *model* runs; hooks are what
the *owner* runs, so the sandbox doesn't apply to them. Only put commands
in a hook that you would run yourself. (The built-in `verify_command`
check is the one exception: it keeps running inside the sandbox, as it
always has — §6.)

## 1. Events

| Event | When | Matcher on | Exit 2 ("block") means |
|---|---|---|---|
| `SessionStart` | before an agent's first model call (once per agent) | `source` | can't block: logged, shown to the owner |
| `SessionEnd` | `ferrule run` finished, `ferrule chat` exited | — | can't block: logged, shown to the owner |
| `UserPromptSubmit` | a request arrives, before it enters the history | — | the prompt is dropped: the model never sees it and the run answers with the reason |
| `PreToolUse` | before a tool call runs | tool name | the call is refused: it never runs, and its result tells the model why |
| `PostToolUse` | after a tool call returned | tool name | the tool already ran; the reason is added to its result for the model |
| `Stop` | the model answered with no tool calls | — | the run goes on: the reason goes to the model as a user message (capped, §8) |
| `PreCompact` | before the context is folded into a summary | `trigger` | can't block (skipping compaction would overflow the context): logged, shown to the owner |
| `PostCompact` | after compaction | `trigger` | can't block: logged, shown to the owner |
| `SubagentStart` | a sub-agent is about to run its task (fires in the parent) | role | the child doesn't run: it ends as failed with the reason, and the parent is told |
| `SubagentStop` | a sub-agent finished its task (fires in the parent) | role | the child goes on with the reason as its next message (capped, §8) |

"Can't block" events treat exit 2 like any other failing exit code: a
non-blocking error.

### Payload

Every hook gets one JSON object on stdin. It is closed after writing, so
a hook that reads to EOF finishes reading. Keys that don't apply to the
event are left out.

```json
{
  "hook_event_name": "PreToolUse",
  "session_id": "4c0d…",
  "transcript_path": "/home/me/.local/share/ferrule/sessions/4c0d….jsonl",
  "cwd": "/home/me/project",
  "agent_id": "a-1b2c3d4e",
  "parent_session_id": "4c0d…",

  "tool_name": "shell",
  "tool_input": {"command": "rm -rf build"},
  "tool_use_id": "call_1",
  "tool_response": {"ok": true, "content": "…"},

  "prompt": "…",
  "stop_hook_active": false,
  "files_changed": true,
  "last_assistant_message": "…",
  "source": "startup",
  "reason": "exit",
  "trigger": "auto",
  "agent_type": "worker",
  "task": "…"
}
```

- `tool_*`: PreToolUse and PostToolUse (`tool_response` only on Post). A
  tool's result is capped at the tool's own output cap already.
- `prompt`: UserPromptSubmit.
- `stop_hook_active`: Stop and SubagentStop. It is true when the previous
  finish was sent back by a hook, so a hook can decide not to block twice.
  `files_changed` (a ferrule addition): a tool that changes files succeeded
  in this run.
- `last_assistant_message`: Stop and SubagentStop.
- `source`: SessionStart, `startup` or `resume` (the history already had a
  conversation). `reason`: SessionEnd, `exit`. `trigger`: Pre/PostCompact,
  always `auto` (ferrule has no manual compaction).
- `agent_id`, `parent_session_id`: set when the agent is a sub-agent, and
  for SubagentStart/Stop (which run in the parent, whose `session_id` it
  is). `agent_type` is the child's role, `task` its task.

Each hook also gets `FERRULE_PROJECT_DIR` (the workspace) and
`FERRULE_HOOK_EVENT` in its environment. It runs with the workspace as
its working directory.

### Output

- **Exit 0**: proceed. If stdout is a JSON object, ferrule reads:
  - `decision: "block"` with `reason`: the same as exit 2, with `reason`
    as the reason (Claude Code's JSON form);
  - `hookSpecificOutput.permissionDecision: "deny"` with
    `permissionDecisionReason`, on PreToolUse: the same (Claude Code's
    newer form); `"allow"`/`"ask"` are accepted and mean proceed — ferrule
    has no permission prompt to skip or show;
  - `hookSpecificOutput.additionalContext` (or a top-level
    `additionalContext`): a note for the model (§5).

  For SessionStart and UserPromptSubmit, plain (non-JSON) stdout is the
  note, as in Claude Code. For every other event, plain stdout goes to
  the audit log only.
- **Exit 2**: block, where the event can block. stderr is the reason
  (empty stderr: "blocked by `<command>`").
- **Any other exit code**, a hook that can't start, a timeout: a
  non-blocking error. It goes to the audit log and the owner's screen
  (the `ferrule run`/`chat` output, the gateway's log) with the tail of
  stderr. The model never sees it, and the turn goes on as if the hook
  weren't there.

On the owner's screen, `ferrule run`/`chat` print an error as
``[hook <Event> `cmd`: <error>]`` on stderr (the error is `exit code N:
<stderr tail>`, `timed out and was killed` or `couldn't start: …`) and a
block as ``[hook <Event> `cmd` blocked]``.

### Environment

A command hook runs in the shell tool's shell (`sh -c`; on Windows Git
Bash's `bash -c`, else PowerShell with `-EncodedCommand`) in the
workspace, with the payload on stdin and `FERRULE_PROJECT_DIR`,
`CLAUDE_PROJECT_DIR` (so Claude Code hook scripts work unchanged) and
`FERRULE_HOOK_EVENT` set. It runs as the owner, **outside the sandbox and
the credential proxy**: whatever the owner can do, a hook can do.

### What the model sees

- A Stop block: a user message ``[hook: Stop] This isn't done yet:\n\n<reason>``
  (SubagentStop: the same with `SubagentStop`, as the child's next message).
- A PreToolUse block: the tool isn't run, and its result is
  `error: not run: a PreToolUse hook blocked it: <reason>`.
- A PostToolUse block: the result is kept and the reason is appended, as
  a note is (§5), since the tool already ran.
- Notes: §5.

## 2. Configuration

Two places, same entry format:

**User hooks**: `[hooks]` in the config file.

```toml
[hooks]
project = false        # run the workspace's hooks (see §3)
timeout_secs = 60      # a hook's default timeout
max_stop_blocks = 3    # times Stop/SubagentStop hooks may send one run back

[[hooks.PreToolUse]]
matcher = "shell"
command = "~/.config/ferrule/hooks/guard.sh"
timeout_secs = 10

[[hooks.Stop]]
command = "notify-send 'ferrule finished'"
```

User hooks are read **only from a trusted config**: the global config
(`~/.config/ferrule/config.toml`) or the one `--config` names. A
`./ferrule.toml` in the working directory isn't trusted, since it may
sit in a workspace the agent can write to (the same rule as M13's
`[extensions] allow` and M17's `[secrets]`). Its `[hooks]` section is
ignored, and the owner is told so.

**Workspace hooks**: `.ferrule/hooks.toml` in the workspace, entries only:

```toml
[[PreToolUse]]
matcher = "write_file|shell"
command = "./scripts/lint-guard.sh"
```

It can't set `project`, the default timeout or the caps: a repo can't
raise its own limits. Any entry's `timeout_secs`, user or workspace, is
capped at 600s.

An entry has `command` (required), `matcher` (optional) and
`timeout_secs` (optional). An unknown key or event name is a config
error, not a hook that silently never fires.

## 3. Trust for workspace hooks

Workspace hooks run only when both are true:

1. `[hooks] project = true` in the trusted config: the switch, the same
   mechanism as `[skills] project`, with its wording: *Run hooks from the
   workspace (`.ferrule/hooks.toml`). A cloned repo's hooks run as you,
   outside the sandbox, so leave this off when pointing the agent at
   untrusted repos.* It defaults to **off**, unlike skills. A skill is
   text in a prompt; a hook is code run outside the sandbox.
2. The owner trusted this file as it is now: `ferrule hooks trust`
   (optionally `--workspace DIR`) records the file's SHA-256 against the
   workspace path in `<data dir>/private/hooks-trust.json`. If the file
   changes afterwards — edited by the owner, a `git pull`, or the model —
   its hooks stop running until it is trusted again.

`ferrule hooks trust` shows every entry and asks `[y/N]`, and only at a
terminal: with stdin not a terminal it refuses ("asks the owner at a
terminal"). The shell tool's stdin is never a terminal, so the model
can't answer the prompt for the owner. After the answer it compares the
fingerprint it stored with the text it showed, and undoes the trust if
the file changed in between. `ferrule hooks untrust` removes the record.

Untrusted workspace hooks never run. Each time an agent is built, the
owner is told, once per process: `ferrule: <workspace>/.ferrule/hooks.toml
has N hooks that won't run: <why>. Run \`ferrule hooks trust\` if you
trust them.` `ferrule hooks list` and `ferrule doctor` say the same.

### The model can't add or enable hooks

- No tool reads or writes hook config or the trust record. `ferrule hooks
  trust` is a CLI command, never a tool.
- User hooks come only from a trusted config. The sandbox's writable roots
  are the workspace and temp dirs, so the model can't edit the global
  config. A `./ferrule.toml` it writes is untrusted and its `[hooks]` are
  ignored.
- A `.ferrule/hooks.toml` the model writes or edits has a fingerprint
  nobody trusted, so it doesn't run.
- The trust record lives in `<data dir>/private/`, which is hidden from
  sandboxed commands and refused by the file tools.

The one limit: with `[sandbox] mode = "off"`, a shell command can write
anything the owner can, including the global config and the trust
record. No ferrule guarantee survives that, and hooks are no exception.
The same holds for a `--config`/`$FERRULE_CONFIG` file the owner keeps
inside a workspace the agent can write to: it is trusted by where it was
named, not by where it lives.

## 4. Matchers, ordering and merging

A matcher is `|`-separated alternatives. Each is an exact name or a glob
(`*` for any run of characters, `?` for one character): `shell`,
`write_file|shell`, `mcp__github__*`. A missing matcher, `""` or `"*"`
matches everything. It is matched against the tool name (Pre/PostToolUse),
`source` (SessionStart), `trigger` (Pre/PostCompact) or the child's role
(SubagentStart/Stop). A matcher on the other events is a config error.
(Claude Code's matchers are regexes. A glob covers what tool names need
and can't blow up. `a|b` and plain names mean the same in both.)

When several hooks match, they run **one after another, in a fixed
order**:

1. built-ins (the `verify_command` check);
2. user hooks, in file order;
3. workspace hooks, in file order.

The first one that blocks ends that point: later hooks for the same
event don't run, and the audit log records them as skipped. The same
command with the same matcher for the same event listed twice runs once.
User and workspace hooks for one event both run; neither replaces the
other.

Sequential rather than parallel (Claude Code runs matching hooks in
parallel): the order is predictable, a guard can rely on an earlier one,
and a blocked call doesn't also run the hooks after it. The cost is
latency when many hooks match one event. Hooks are expected to be quick.

## 5. `additionalContext`

A note a hook adds reaches the model at that point in the turn:

- SessionStart, UserPromptSubmit, PostCompact: a user message added at
  the end of the history (after the prompt, for UserPromptSubmit),
  `[hook: <Event>]\n<note>`.
- PreToolUse, PostToolUse: added to the end of that tool call's result,
  `\n\n[hook: <Event>] <note>`. A tool call must be followed by its
  result, so a separate message can't go between them.
- Stop, SubagentStop: only the block reason goes to the model. A note
  from a Stop hook that lets the run end has nowhere to go and is logged.
- SubagentStart: added to the child's task message.

A note is **only ever appended** to the newest part of the history. The
system prompt and every earlier message stay byte-for-byte the same, so
the provider's prompt cache still hits on the whole prefix. A note never
goes into the system prompt.

**Cap:** 10,000 characters per hook (`[hooks] max_context_chars`), cut
with a `[… cut]` marker. The same cap applies to a block reason. Notes
from several hooks at one point are joined, and the joined text has the
same cap.

## 6. `verify_command` is a built-in Stop hook

`with_verifier` no longer holds its own slot in the loop. It adds a
built-in Stop hook, a **check**, to the agent's hooks, ahead of every
user hook. The Stop point runs hooks in the §4 order, and a check keeps
every behaviour it had:

- It runs only when a tool that changes files succeeded in this run
  (the built-in's own filter on `files_changed`). A user Stop hook runs
  at every finish.
- It still runs inside the sandbox, through `CommandVerifier`, with
  `verify_timeout_secs`.
- It emits `VerifyStarted`/`VerifyFinished`, so the renderer and `ferrule
  eval`'s `verify_failures` count are unchanged. It doesn't emit the
  generic hook event, so nothing counts it twice.
- A failure sends the run back with the same message word for word
  (`` [ferrule] `cmd` fails, so this isn't done yet. … ``).
- It has its own cap, `max_verify_rounds`, and its own wrap-up
  (`` `cmd` still fails after N rounds of fixes ``).
- A failing check stops that finish: user Stop hooks don't run until
  the check passes.
- Once it passed, it doesn't run again at a later finish of the same
  run unless files changed since (the loop's `needs_check` flag), so a
  user Stop hook sending the run back doesn't rerun a passing check.

The existing verify tests pass unchanged, and M14's engineered eval
variant (which uses `with_verifier`) stays at 20/20.

**Gate scripts stay gates.** The scheduler's gate scripts (M9) are not
turned into hooks. A gate decides whether a session is started at all,
before any agent exists. Its contract is `wakeAgent` JSON on stdout,
with the task's own data. It is configured per task in the task store,
not in `[hooks]`. Nothing that fires a lifecycle event exists yet when a
gate runs. A gate is the closest thing to a `PreSessionStart` hook,
which neither Claude Code nor Codex has. Forcing it into the hook shape
would change its config and contract for no new capability. It would
also mean editing the scheduler, which M16 is changing in parallel.

M29 adds a second built-in: a PostToolUse **lint** hook on
`edit_file|write_file` that runs the project's own linter on the edited
file (`[agent] lint`, `docs/editing.md`).

## 7. Sub-agents, eval and the audit trail

**Sub-agents inherit PreToolUse and PostToolUse.** A child is built with
the parent's `PreToolUse` and `PostToolUse` hooks, user and workspace,
under the same trust. A guard on `shell` therefore also guards every
agent the root starts. The child's payload carries its own `session_id`
plus `agent_id` and `parent_session_id`. A child doesn't get the other
events' hooks: its start and end are the parent's `SubagentStart` and
`SubagentStop`. Its Stop is where the built-in check applies, as it did
before M18.

**SubagentStart and SubagentStop fire in the parent**, from the
supervisor. They use the parent's session id and the child's
`agent_id`, `agent_type` and `task`. They run in the child's background
task, before its first model call and after its final answer. So a
blocking SubagentStart shows up as the child failing with the hook's
reason, which the parent gets in its `agent_notice` (as for any failed
child), rather than as an error from `spawn_agent` itself. Their working
directory is the parent's workspace, not the child's worktree. A child
run that fails still fires SubagentStop (with no
`last_assistant_message`), but a block then can't send it on: it stays
failed.

The supervisor takes the hooks from the root agent when the root is
attached (`attach_root`), so there's one hook set per process: a process
has one config and one workspace. A sub-agent is built without the
config's hooks and gets only what the supervisor hands it.

**Eval is hermetic.** `ferrule eval` builds its agents itself
(`ferrule-eval`'s `variant::build`). It never reads `[hooks]` or a
workspace's hooks file, so a run is the same on any owner's machine. A
suite that wants hooks would need to opt in explicitly. That opt-in isn't
built in M18 (it needs a suite-format field, and the starter suite stays
unchanged). A real-binary test checks that the owner's hooks don't fire
during `ferrule eval run`.

**The audit log**: every hook run appends one JSON line to
`<data dir>/hooks/runs.jsonl`. The line records the time, event, source
(builtin/user/workspace), command, matcher, tool, session and agent,
exit code, duration, whether it blocked, whether it timed out, and a
short note (the reason, or the error's stderr tail). Hooks that were
skipped after an earlier block are recorded too, as `skipped`. `ferrule
hooks list` shows the configured hooks by event, with their source and
trust state, and the last runs from the log (`--runs N`).

The built-in check is recorded in the log too, marked `builtin`. The log
isn't under `private/`. Commands may read it, and they can't write it
(the data dir isn't a sandbox root).

## 8. Failure modes

- **A hook that hangs** is killed at its timeout, `timeout_secs`
  (default 60s). On Unix it runs in its own process group, and the whole
  group gets SIGKILL, so a hook's children die with it. On Windows,
  `taskkill /T /F` kills the tree. A timeout is a non-blocking error: the
  turn goes on. This makes hooks **fail open**: a PreToolUse guard that
  times out doesn't stop the call. That's the same as Claude Code, and
  it's a decision for Max (§10).
- **A hook that crashes** (can't start, killed by a signal, exit code
  other than 0 and 2): a non-blocking error. The turn goes on.
- **A hook that prints megabytes**: stdout and stderr are each kept up to
  64 KiB. The rest is read and thrown away, so the pipe never fills and
  stalls the hook. The kept text is then capped again where it's used:
  a reason or note at 10,000 characters, the audit note at 2,000.
- **A hook that never reads stdin**: the payload is written from a
  separate task, and a broken pipe is ignored.
- **A Stop hook that always blocks**: `stop_hook_active` tells it when it
  already sent this run back. Ferrule also caps it. After
  `max_stop_blocks` (default 3) sends-back in one run, the next block ends
  the run with a status answer, like the step limit: "``the Stop hook
  `cmd` still blocks after 3 tries``". The run is marked incomplete, so
  `ferrule run` exits 2. SubagentStop has the same cap per child run. The
  built-in check keeps `max_verify_rounds`, and a check failure doesn't
  use up the Stop hooks' allowance.
- **A hook config that doesn't parse**: the agent doesn't start, and the
  error names the file and entry. That's the same as any other config
  error, and better than running without a guard the owner thinks is on.
  A workspace file that doesn't parse is treated as untrusted and
  reported instead, so a broken cloned repo doesn't stop the agent.

## 9. Where it lives

- `ferrule-core::lifecycle`: events, payload, what an exit code means per
  event, the ordered hook set with its caps, and the audit trait. Also
  the `HookHandler` trait with an adapter that makes a `Verifier` a
  check. The loop calls it at each point. (`ferrule-core::hooks` already
  exists and holds the loop's other seams — budget, inbox, stop flag —
  so the new module has its own name.)
- `ferrule-hooks` (new crate): `CommandHook` (spawn, stdin, timeout,
  process-tree kill, output caps), the config and workspace-file
  formats, the trust record, the JSONL audit log, and what `hooks list`
  prints.
- `ferrule-agents`: an optional hook set on the supervisor for
  SubagentStart/Stop.
- `ferrule-cli`: a `hooks` field on `Config`, a few lines in
  `build_agent_from` to attach the hooks, SessionEnd in `run`/`chat`, the
  renderer's line for hook events, and `ferrule hooks list|trust|untrust`.

## 10. Decisions for Max

1. Workspace hooks are off by default (`project = false`), where project
   skills default to on, and need a per-file `ferrule hooks trust` on top.
2. Hooks fail open on timeout and crash, like Claude Code. A fail-closed
   option for PreToolUse (`on_error = "block"`) would be a small add.
3. Matching hooks run one after another, and the first block wins.
   Claude Code runs them in parallel.
4. `max_stop_blocks = 3` per run, separate from `max_verify_rounds`.
5. Matchers are globs, not regexes.
6. SubagentStart can't turn a `spawn_agent` call into an error: the child
   fails with the reason instead, delivered as its notice.
7. SessionEnd fires for `ferrule run` and `ferrule chat`, not for
   gateway sessions, which don't end: they idle until the process stops.
8. Eval has no hook opt-in yet.
9. A PostToolUse block can't undo the call, so it reaches the model as a
   note on the result rather than stopping anything.
10. Hook errors are shown to the owner and logged, never to the model.

## 11. Open edges

- A `--config`/`$FERRULE_CONFIG` file inside a writable workspace is
  trusted (§3); `doctor` doesn't warn about it yet.
- With `[sandbox] mode = "off"` the shell can write the trust record.
- A workspace hook's relative command (`./scripts/guard.sh`) runs in the
  root's workspace, also for a child working in its own worktree: the
  child's PreToolUse sees the child's `cwd` in the payload, but the
  command runs from the root's workspace.
- Gateway sessions never fire SessionEnd (§10.7).
- **Unverified on macOS/Windows until the batch CI pass:** the shell
  choice (`sh -c`, Git Bash, or PowerShell when Git Bash is missing;
  `cmd /C` isn't used), exit codes as Git Bash and PowerShell report them
  (exit 2 from a PowerShell script needs an explicit `exit 2`), killing a timed-out process tree (`killpg` on macOS,
  `taskkill /T /F` on Windows), piping the payload to stdin and reading
  stdout/stderr, the trust and audit files' permissions, `canonicalize` in
  the trust record's workspace key (`/private/var`, `\\?\`), and the
  integration tests, which are `#[cfg(unix)]` and use `sh` scripts.
