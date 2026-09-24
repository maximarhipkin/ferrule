# M19: trust & cost (design)

Status: design, 2026-09-25. Builds on the roadmap's M19 section. Where this
document departs from it, it says so and why.

## What changes

Today an unattended Ferrule run has four brakes: the step limit, the loop
detector, M12's per-tree token budget for sub-agents, and M16's caps on the
learning pass. None of them is about money, none of them sees the whole
day, and none can be pulled by the owner from a phone. Nothing stands
between the model and `git push --force` except the model's own judgement.

M19 adds five things, all in a new crate, `ferrule-trust`, and all enforced
at one seam in the agent loop:

| | what it does | where it's enforced |
|---|---|---|
| **hard caps** | tokens and dollars, per run, per day and per task, read from the ledger | before every model call, and while a tool runs |
| **80% warning** | one Telegram message per cap and window | when a check sees spend cross the line |
| **kill switch** | `ferrule stop` or `/stop` in Telegram halts everything now and blocks new runs until `--clear` or `/resume` | the same checks, plus the scheduler's tick |
| **approval gates** | a recursive delete, a force push or a `DELETE` to a bound host waits for the owner's "yes" | before the tool call |
| **plan mode** | read-only exploration, a plan, the owner's approval, then execution | the tool set, the sandbox and the gate |

The owner's side, all of it optional:

```toml
[trust]
max_tokens_per_run = 5000000    # 0 turns a cap off
max_usd_per_run = 5.0
max_tokens_per_day = 50000000
max_usd_per_day = 20.0
max_tokens_per_task = 0         # a scheduled task's own day; off by default
max_usd_per_task = 0
warn_at = 0.8
timezone = "UTC"                # what "a day" is
owner_chat = 123456789          # Telegram chat for approvals and warnings
approval_timeout_secs = 600
plan_timeout_secs = 3600        # how long a Telegram plan waits for "yes"
gates = true
```

## 1. The seam in the agent loop

`ferrule-core` gets one new file, `guard.rs`, with one trait:

```rust
#[async_trait]
pub trait Guard: Send + Sync {
    fn begin(&self) {}                                   // a run starts
    fn before_model_call(&self) -> Option<String>;       // Some(msg): stop now
    async fn before_tool_call(&self, tool: &str, args: &Value) -> Verdict;
    async fn halted(&self) -> String;                    // resolves on stop
}
```

and a helper, `guarded_call`, that runs one tool call under a guard:
the verdict first, then the tool raced against `halted()`. `Agent` gains
`with_guard` and a few call sites in `run_inner`:

1. `begin()` once per `Agent::run`.
2. `before_model_call()` at the top of every iteration, before compaction
   and the turn call. `Some(message)` ends the run through a new `halt`
   path: the message becomes the run's answer, `Agent::incomplete` is set
   and a `RunIncomplete` event goes out. **No status call is made**,
   unlike `wrap_up`: a run stopped for spending too much must not spend
   more to say so.
3. The provider call is raced against `halted()`, so the kill switch stops
   a long call too.
4. Each tool call goes through `guarded_call`. A refusal becomes the
   tool's result (`refused by ferrule: …`), so the model sees why and can
   pick another way. A halt answers that call and every remaining call in
   the message with `not run: …` (the history must stay sendable) and
   takes the `halt` path.

The guard is its own slot, not the `Budget` slot. M12's supervisor calls
`with_budget(TreeBudget)` on every child after the CLI's factory built it,
which would overwrite a guard that lived there.

The guard trait doesn't know about caps, Telegram or plans. `ferrule-trust`
implements it (`TrustGuard`), and the CLI attaches one to every agent it
builds, the root and every sub-agent.

## 2. The cap model

### What a run, a day and a task are

- **A run** is one `Agent::run` of a root agent: one `ferrule run`, one
  chat message's turn, one scheduled task's run. A sub-agent's spend counts
  toward the run of its tree's root, whichever run of the root is the
  latest: a child that keeps working after its parent answered is charged
  to that same run until the root runs again.
- **A day** is a calendar day in `[trust] timezone` (default UTC),
  midnight to midnight. It is not M16's rolling 24 hours, on purpose: the
  80% warning is "once per window", and a rolling window has no edges, so
  a spend that hovers around 80% would warn again every time an old call
  drops out of it. A calendar day resets at a time the owner can predict.
- **A task** is one scheduled task, per day: `max_usd_per_task` is how
  much one cron job may spend in a day, all its runs and their sub-agents
  together. Per-run caps already bound a single run.

### Counting

Every number comes from the ledger, so the caps survive a restart and see
other processes (a `ferrule run` next to the gateway spends from the same
day):

- The CLI's ledger sink is wrapped in `TrustSink`, which stamps each row
  with a new optional field, `tree` (the root's session id), before
  writing it. A child's rows carry its root's tree, so after a restart
  `scheduler__<task>` rows still add up to the task's day, sub-agents
  included. Old rows without `tree` fall back to their `session_id`.
- A **run**'s spend is counted in memory as `TrustSink` passes the rows
  through. A run never outlives its process, so there's nothing to restore.
- **Day** and **task** spend come from tailing `ledger.jsonl`: read once
  at start (today's rows only are kept), then from the last byte offset at
  every check. Only complete lines count; a line another process is
  halfway through writing is read next time. A file that got shorter was
  rotated or cleared, and is read again from the start.
- Rows with an `eval` tag are skipped unless they also carry a `tree` (see
  §10).
- Tokens are `input_tokens + output_tokens`, cached input included, the
  same sum M12 and M16 use. Dollars are `cost_usd`. A row without a price
  counts toward the token caps only. When a dollar cap is set and the
  provider has no prices, `ferrule trust status` and `ferrule doctor` say
  the dollar caps can't bite.

### Checking

A check (`before_model_call`) reads the new ledger lines, then compares
this run, today and this task's today against their caps. The first cap at
or over its limit stops the run. Caps are checked before a call, so one
call can overshoot: by at most one call per agent running at that moment.
Nothing spends between model calls, so there's nothing to check there,
except that sub-agents spend while their parent waits in `wait_agent`:
every charge wakes `halted()`, which checks again, and a cap crossed by a
child stops the parent mid-tool-call too.

### M16's caps

The learning pass keeps its own caps (`[learning] max_usd_per_pass` and
friends, rolling 24 hours, `call_kind = "learn"` rows). They are a
sub-budget: learn rows count toward M19's day like any other row, and a
pass doesn't start while the kill switch is on or the day's cap is spent.
That is checked when the pass starts: a pass already running finishes
under its own caps, and the switch doesn't halt it mid-pass (its calls
don't go through the guard). M16's rolling window stays as it is. Changing it would change a shipped
behavior for no gain: the pass runs once a night.

## 3. The stop message

A run stopped at a cap answers with Ferrule's own text, no model call:

> Stopped: this run reached its token cap (max_tokens_per_run = 5,000,000).
> Spent: 5,012,340 tokens and $1.23 this run; 18,200,000 tokens and $6.10
> today (UTC). Nothing else was sent to the model. Raise the cap in
> [trust], or start again tomorrow for a day cap.

It names the cap, its config key and value, and this run's and today's
spend. The same text is the scheduled task's `incomplete` reason in
`ferrule tasks runs`, goes to the owner chat for an unattended run, and is
written to the audit log (§11).

## 4. The 80% warning

When a check sees a cap's spend at or over `warn_at` × the cap (0.8 by
default), it sends one message to the owner chat:

> ferrule: 80% of today's dollar cap is spent ($16.10 of $20.00, UTC day).
> Runs stop at $20.00.

Once per window: per run for run caps, per day for day caps, per task and
day for task caps. The audit log records each warning with its window key;
at start the day's keys are read back, so a restart doesn't warn twice.
With no Telegram, the warning goes to the log and the audit only.

## 5. The kill switch

- `ferrule stop [--reason TEXT]` writes `<data>/trust/stop` (who, when,
  why). `/stop [reason]` from an allowed Telegram chat does the same inside
  the gateway. `ferrule stop --clear` or `/resume` removes it;
  `ferrule stop --status` shows it.
- Every process that runs agents (the gateway, `run`, `chat`, `tasks
  run-now`) watches the file, once a second. The gateway flips its switch
  at once on `/stop`, without waiting for the poll.
- **Running turns** halt at their next check or mid-call (§1): the tool's
  future is dropped. For the shell, dropping it now kills the command's
  whole process group, not just the `sh` at the top (M19 adds that). An
  MCP tool call is abandoned; the server may finish it (it's a separate
  process). A provider call is abandoned; the provider may bill for it and
  no ledger row is written for it.
- **Sub-agents** each have their own guard on the same switch, so they
  halt the same way.
- **Scheduled runs**: the scheduler's tick skips every due task while the
  switch is on. The tasks stay due, so after `--clear` each gets one run
  (the M3 backlog rule).
- **New runs** answer with the stop message and make no model call: "ferrule
  is stopped (by `ferrule stop` at 14:02, reason: …). Nothing was run.
  `ferrule stop --clear` or /resume turns it back on."
- It stays on across restarts, until someone clears it. A switch that a
  restart silently cleared would be worse than none.

## 6. The classifier

`classify(tool, args, bound_hosts)` looks at `shell` commands only. It
splits the command on `;`, `&&`, `||`, `|`, `&`, newlines, `$( … )` and
backticks, unwraps `sh -c` / `bash -c` / `zsh -c`, skips wrappers (`sudo`,
`env`, `nice`, `nohup`, `time`, `command`, `xargs`, `exec`, `timeout N`,
leading `VAR=value`), and matches each simple command. Gated:

| kind | matches |
|---|---|
| recursive delete | `rm` with `-r`, `-R` or `--recursive` (with or without `-f`, any order, combined flags like `-rfv`); `find … -delete` or `find … -exec rm`; `git clean` with `-f`; `rsync … --delete*` (not gated: `rm -f file`, `rmdir`) |
| force push | `git push` with `-f` (alone or combined, `-fu`), `--force`, `--force-with-lease[=…]`, `--force-if-includes`, `--mirror`, `--delete`/`-d`, or a refspec starting with `+` or `:` |
| DELETE to a bound host | `curl -X DELETE` / `-XDELETE` / `--request DELETE`; `wget --method=DELETE`; `http`/`https`/`xh DELETE url`; `gh api -X DELETE` / `--method DELETE` (host `api.github.com`), where a URL's host matches a `[secrets]` host pattern. A `DELETE` whose URL can't be found (`curl -X DELETE "$URL"`) is gated too |

Everything is lowercased and quotes are removed before matching (`'rm'`
matches `rm`). Every path counts, not just the workspace: `rm -rf /tmp/x`
is gated too, since telling "in the workspace" apart reliably is harder
than it looks (symlinks, `cd`, relative paths).

**What it can't catch, by design:** anything a program does that the
command line doesn't show. A script file (`./clean.sh`, `make clean`,
`npm run reset`), an interpreter (`python -c 'shutil.rmtree(…)'`,
`node -e`), a variable as the command (`$CMD`), `eval "$x"` (a literal `eval '…'` is read), a decoded
payload piped into `sh`, an alias, a git hook, an MCP tool (they don't
go through the shell), `write_file` blanking a file. The classifier is a
speed bump for the honest mistake, not a boundary: the OS sandbox (M6)
limits what can be written at all, and the credential proxy (M7) decides
which hosts get real keys. Its false positives cost one approval; its
false negatives are what the sandbox is for.

`[trust] gates = false` turns the classifier off.

## 7. The approval round-trip

When a tool call is gated, the guard asks whoever the run belongs to:

| the run | who's asked |
|---|---|
| a gateway chat turn (Telegram) and its sub-agents | the owner chat |
| `ferrule run` / `chat` with a terminal on stdin | the terminal |
| a scheduled task, `ferrule run` without a terminal, the local channel | nobody: **refused** |

The **owner chat** is `[trust] owner_chat`, else the first private chat
(positive id) in `telegram_allowed_chats`. A group is never picked on its
own: anyone in it could say yes. No owner chat means Telegram-originated
gated calls are refused too; a default install without Telegram works,
it just can't approve from a phone.

The message:

> ferrule wants to run, in telegram chat 4242 (run 3f2a…):
> `git push --force origin main` — force push.
> Reply `yes` to allow it. Anything else, or no answer in 10 minutes,
> refuses it. (code k7)

The owner chat's messages are read **before** they reach a session's lane
(the lane is busy running the turn that's waiting). While approvals are
pending in that chat:

- `yes` (any case, surrounding space and a trailing `.`/`!` ignored)
  approves the only pending one. `yes k7` approves k7.
- Anything else refuses **every** pending approval in the chat, `no k7`
  refuses only k7. The owner is told it was refused, and the message isn't
  passed on to the agent.
- Two pending: a bare `yes` approves neither and the bot lists the codes
  (§12, two pending).

Only "yes" runs the command. A refusal of any kind (no, other text,
timeout, unattended, Telegram unreachable, the run halted while waiting)
returns `refused by ferrule: … (why)` as the tool's result.

The terminal: the same text on stderr, one line read from stdin, same rule.

## 8. Plan mode

`ferrule run --plan "<task>"`, or `/plan <task>` in Telegram.

1. **Explore, read-only.** The agent gets `read_file`, `list_dir`,
   `web_fetch` (GET only), memory recall and `search_history`. The shell is
   there only when the OS sandbox is really active, and then in read-only
   mode with the network off. No `write_file`, no memory writes, no MCP
   tools (a server can do anything; we can't tell reading ones apart), no
   extension installs. Sub-agents may be started, always read-only, and
   only with `worktree: false` (a worktree is a new branch, which is a
   change). The guard refuses anything that `changes_files()` and every
   gated command, whatever tools are present. `ferrule run --plan` starts
   no MCP server at all.
2. **The plan** is the run's answer, saved as `<data>/plans/<id>.json`
   (task, workspace, session, plan text, its SHA-256, status).
3. **Approval.** Telegram: the plan is sent with "Reply `yes` to run this
   plan (code …)", the same rules as §7, waiting `plan_timeout_secs` (one
   hour). CLI with a terminal: the plan is printed and a `yes` read.
   Without one: `ferrule plan approve <id>` / `ferrule plan reject <id>`,
   and `ferrule plan list`.
4. **Execution** runs in the same session (it keeps what the exploration
   read) with the normal tools, under the same caps and gates. The CLI
   builds a new agent and replays the session's transcript into it; the
   gateway drops the planning lane (`Router::retire`), so the next turn
   builds a new agent from the transcript. A plan from Telegram runs in a
   session of its own (`plan__<id>`, on a channel no adapter listens to),
   and its answer is sent to the chat that asked. When it ends, the plan
   is marked `executed` with the run's id (`plan_executed` in the audit
   log). Its first
   message is the approved plan, verbatim, with "carry it out; if you
   depart from it, say where and why in your answer."

**How the approved plan carries over.** As text in the conversation, not as
a machine-checked contract: Ferrule can't tell whether a shell command is
"in the plan". What is fixed is the record: the plan's hash is in the plan
file and the audit log next to the execution run's id, so what was approved
and what ran can be compared afterwards. The gates stay on during
execution. An approved plan that says "force-push the branch" still asks
for the force push.

"Nothing changes before approval" is held by the tool set and the sandbox,
not by the model's cooperation. What plan mode doesn't stop: `web_fetch`
reaching a URL with side effects on GET (bad practice, but it exists), and
a read-only sub-agent doing the same.

## 9. Sub-agents

A child's guard is the parent tree's: it shares the tree's run (its spend
counts toward it), the kill switch, the approval route (a child of a
scheduled task is unattended too) and the plan phase (a child started
while planning is built read-only). The supervisor's M12 token budget still
applies on top. A child can't widen any of this: it gets no config, and the
CLI builds its guard from the tree id the supervisor hands the factory.

## 10. Eval stays hermetic

`ferrule eval` builds its own agents (`ferrule-eval`'s runner), without a
guard, and its rows carry an `eval` tag, which the meter skips. So eval is
never capped by, counted against or blocked on the owner's caps, approvals
or kill switch. The kill switch is included on purpose: an A/B run
compares variants and must not depend on the owner's state.

A suite opts in with `[suite] owner_trust = true`. Both variants then run
under the owner's guard (caps, gates, kill switch; an eval run is
unattended, so gated commands are refused), and their rows are stamped with
a `tree` (`eval:<run id>`), which makes them count toward the owner's day.
Both variants, unlike M16's `owner_playbook`, which only the engineered one
gets: a cap that only one side runs under would skew the comparison.

A task the owner's guard stops (the switch, a cap) is `stopped`, with no
verdict, and `ferrule eval` exits 3, as for its own budget. The judge's
calls (rubric grading) go through eval's own sink only, so they aren't
charged to the owner's day even with `owner_trust`.

Tested: with tiny caps, the kill switch on and a destructive command in a
task, the suite runs and the rm runs, nothing reaches the owner's day and
the audit log is untouched; with `owner_trust` the same suite is stopped
before any model call, and without the switch its rm is refused
(unattended) and its calls count toward the owner's day. The full starter
suite was also run with the switch on and tiny caps.

## 11. The audit trail

`<data>/trust/audit.jsonl`, one line per event: `cap_stop`, `cap_warning`,
`stop_engaged`, `stop_cleared`, `approval_asked`, `approval_answered`
(`yes`, `no`, `timeout`, `unattended`, `unreachable`, `halted`),
`plan_proposed`, `plan_approved`, `plan_rejected`, `plan_executed`. Each has the time, the
tree, the run and the detail (the command, the cap and spend, the plan
hash). `ferrule trust audit [--since 7d]` prints it; `ferrule trust status`
shows the caps, today's spend and the switch. The ledger gets the `tree`
field; cap stops make no ledger row (no call was made).

## 12. Failure modes

- **Telegram down.** Sending an approval fails: refused at once, "couldn't
  reach the owner". A warning that can't be sent is logged and not retried
  (the audit still has it). `/stop` can't arrive; `ferrule stop` on the
  machine still works.
- **Ledger unreadable** (permissions, a broken disk). With a day or task
  cap set, the check can't be done, so the run **stops** (fail closed):
  "can't read the ledger at …: …; the day caps can't be checked". Per-run
  caps still work in memory. Malformed lines are skipped and counted.
- **Cap hit mid-tool-call.** Only sub-agents can spend while a tool runs.
  A cap they cross halts the parent's tool call through `halted()`, the
  way the kill switch does. Otherwise the tool finishes and the next check
  stops the run before the next model call.
- **Late approval.** An answer after the timeout finds nothing pending; the
  bot replies that it expired and was refused, and doesn't pass it on
  (expired codes are remembered for a day). The command never runs late.
- **Two pending approvals** (two sessions, or a parent and a child). Each
  has its own code. `yes k7` answers one; a bare `yes` answers neither and
  the bot lists the codes; any other text refuses them all. Nothing is
  ever approved by a message that could mean either.
- **A halt while waiting for approval.** The wait is raced against
  `halted()`: the call is refused, the approval is withdrawn, and a late
  `yes` finds nothing.
- **The stop file can't be read** (not "missing", but an IO error): treated
  as engaged. A switch that fails open isn't a switch.

## 13. The M18 hooks relation

M18 (lifecycle hooks) and M19 were built in parallel and reconciled when
M19 took `main` in (2026-09-25). One tool call now goes:

1. **the owner's gate** (`Guard::before_tool_call`: plan mode, the
   approval gates, and the wait for a yes);
2. **PreToolUse hooks**;
3. **the tool**;
4. **PostToolUse hooks**.

Every step is raced against `halted()`, so the kill switch or a cap
crossed by a sibling also stops a hook that hangs, the approval wait, and
the tool itself. The call site is in the tool loop in `agent.rs`. The old
`guarded_call` helper is gone, and the loop does the steps itself.

The gate runs first because it is the owner's safety policy and a hook is
extension code. Nothing in the code argued for the other order. The one
cost: the owner can be asked about a call that a hook then blocks.

- **A refused or halted call fires no hooks.**
  - When the gate refuses a call, neither PreToolUse nor PostToolUse
    fires. The tool's result is `refused by ferrule: …`, and the refusal
    is in the trust audit log.
  - A hook never sees a call the owner didn't allow, so a hook's side
    effects (hooks run as the owner, outside the sandbox) can't happen on
    it.
  - This matches M18's own rule that a call a PreToolUse hook blocked
    fires no PostToolUse: Post follows only a call that was dispatched.
  - When the run is halted, every call not yet run gets
    `not run: ferrule halted the run`, and no hook fires for them.
- **No hook can get a command past the gate.**
  - The gate has already answered before any hook runs.
  - M18 has no argument rewrite (`updatedInput` isn't read), so the gate
    sees exactly the arguments the tool will run with.
  - A hook's `permissionDecision: "allow"` means only "proceed", and it
    can't un-refuse a refused call.
  - `additionalContext` is appended to the tool's result after the call.
    It is text for the model, not an input to the gate.
  - If M18 ever adds rewriting, the rewritten call must go through the
    gate again.
- **A refusal from either side wins.** A hook can still block a call
  the gate allowed.
- **Stop hooks and the built-in check (`verify_command`) can't get around
  the caps or the kill switch.**
  - A run a Stop hook sends back goes around the loop, and every model
    call is checked by the guard first. So the run cap, the day cap and
    the switch stop it like any other run.
  - The Stop hooks and the check commands are raced against `halted()`
    too.
- **Sub-agents inherit both.**
  - A child's guard is its root's `.child()`: the same tree and caps, and
    the root's approval route (or its refusal when unattended).
  - A child's hooks are its root's PreToolUse and PostToolUse hooks,
    handed on by the supervisor.
  - The order inside the child is the same: gate first.
- **Plan mode fires no hooks.**
  - A planning run (`ferrule run --plan`, `/plan`, and its sub-agents)
    is built without the config's or the workspace's hooks, and without
    `verify_command`.
  - Hooks run as the owner outside the sandbox, which would break plan
    mode's promise that nothing changes.
  - The approved plan's run is a normal run and fires them all,
    SessionStart included.
- **Eval stays hermetic for both.**
  - `ferrule eval` builds its agents itself and never reads `[hooks]` or
    a workspace's hooks file, whether or not the suite sets
    `owner_trust`.
  - Caps and approvals reach an eval only with `owner_trust = true`.
    There is no hooks opt-in (M18 §7).

Tested in `crates/ferrule-cli/tests/trust.rs`:
- `the_gate_answers_before_pre_tool_use_hooks_and_no_hook_can_approve_past_it`: one session has a gated `rm -rf` and a plain `echo`, plus a PreToolUse hook that logs and answers allow with a note. The rm is refused with no note, and the hook logged only the echo.
- `a_stop_hook_sending_the_run_back_still_meets_the_run_cap`
- `a_planning_run_fires_no_hooks_and_the_approved_plan_does`
- `an_eval_that_opts_into_the_owners_trust_still_fires_no_hooks`

## 14. Safe defaults

| default | why |
|---|---|
| `max_tokens_per_run = 5,000,000` | a long legitimate run (60 steps at ~80k context) fits; a loop that re-sends a big context stops |
| `max_usd_per_run = 5`, `max_usd_per_day = 20` | a bad night costs a dinner, not a rent; only bite when prices are set |
| `max_tokens_per_day = 50,000,000` | ten long runs; a runaway cron is stopped the same day |
| per-task caps off | the day cap already covers a runaway task; a per-task number needs knowing the task |
| `warn_at = 0.8` | the brief's number; late enough not to nag |
| `timezone = "UTC"` | matches the scheduler's default; `setup` can set it |
| gates on | a false positive costs one "yes"; a force push costs a history |
| unattended runs refuse gated calls | nobody's there to say yes, and a timeout would only delay the same answer |
| approval timeout 10 minutes | long enough to see a phone, short enough that a turn doesn't hang for hours |
| owner chat: a private allowed chat only | a group's members could approve |
| the stop survives restarts, fails closed | a switch a restart clears isn't a switch |
| no Telegram needed | the caps, the switch (`ferrule stop`), terminal approvals and `ferrule plan approve` all work without it |

## 15. What is tested

Hermetic: a scripted provider, a mock Telegram (a local Bot API server, or
a scripted notifier at the crate level), a fake clock for day windows,
`tokio::time::pause` for timeouts.

- a run stops at each of its caps (run, day, task, tokens and dollars),
  says which and how much, makes no further model call, and writes a
  `cap_stop`; the day's spend survives a restart (a second process reads
  it from the ledger); a sub-agent's spend stops its parent; the day
  resets at midnight in the configured timezone
- the 80% warning is sent once per window, and not again after a restart
- `ferrule stop` halts a running shell command mid-call (its process group
  is gone), blocks a new run and a scheduled run, and `--clear` resumes;
  `/stop` and `/resume` do the same through the gateway
- the classifier's table, both ways, including what it can't catch
- a gated command waits for Telegram and runs only on `yes`; `no`, other
  text, a timeout, Telegram down and a scheduled run all refuse, and the
  model sees why; a late `yes` does nothing; two pending approvals by code
- plan mode: nothing in the workspace changes before approval (a write
  attempt is refused, a shell write fails); after approval the plan runs;
  a rejection runs nothing; a child started while planning is read-only
- eval with the owner's caps and switch on is unchanged; a suite with
  `owner_trust` is stopped
