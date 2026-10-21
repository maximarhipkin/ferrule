# Sub-agents

An agent can start other agents in the background, give each one a
separate job, and join their reports. The agent you talk to (in a chat,
a `ferrule run`, or a scheduled task) is the **root** of a tree. The
agents it starts are its children, and a child may start its own, down to
`max_depth`.

The design, and the reasons behind it, are in [m12-multi-agent.md](m12-multi-agent.md).

## What it costs

Sub-agents multiply the bill. Anthropic measured its multi-agent research
system at about **15×** the tokens of a plain chat, and about 4× for
agents alone. Every child reads its own system prompt and tools again,
and every report its parent reads adds to the parent's context. The
`spawn_agent` description tells the model this. It should do simple
things itself, use one child for one separable job, and use more only
when the work splits cleanly.

The agent tools themselves take room in every call of an agent that has
them. Measured on the tool definitions ferrule sends (name, description
and schema, plus the prompt addition):

| Agent | Tools | Size |
|---|---|---|
| The root, or any agent that can still spawn | 5 | ~5,100 characters, about 1.3k tokens; `spawn_agent` alone is ~2,300 (11 tools and ~5,900 before M58.3) |
| An agent at `max_depth` (only `board` and `tasks`) | 2 | ~1,900 characters, about 0.5k tokens (6 tools and ~2,600 before M58.3) |
| A child, in addition | | ~450 characters for "you are agent …, report to …" |

A forked child (below) also reads its copy of the parent's conversation,
up to `fork_tokens`, on every call it makes.

The limits below stop a tree that runs away.

### Reading what a tree cost

- `ferrule agents list` shows each agent's tokens.
- `ferrule ledger` has one `agent` row per provider and model: that row is
  every sub-agent's model calls together. The root's own calls stay under
  `run`, `chat`, `telegram` and the other entry points.
- Each ledger line in `<data dir>/ledger.jsonl` carries
  `"task_shape": "agent"` and `"origin": "agent:<parent id>"`, so one
  parent's children can be summed:

  ```sh
  jq -s 'map(select(.origin == "agent:a-1f2e3d4c")) | map(.input_tokens + .output_tokens) | add' ledger.jsonl
  ```

## The tools

Five, after Claude Code's `Agent`, `SendMessage` and `TaskStop` (M58.3):

| Tool | What it does |
|---|---|
| `spawn_agent {task, name?, role?, worktree?, fork?, model?, effort?}` | Starts a child in the background and returns its id at once |
| `message_agent {ids?, message?, timeout_seconds?}` | With `ids` alone, waits until one of them finishes (default 300 s, at most 3600) and returns each one's status and report. With one id and a `message`, a running child reads it at its next step, and a finished, failed or interrupted one runs again with it. With neither, lists your children |
| `stop_agent {id}` | Stops a child (at its next step) and everything it started |
| `board {body?, topic?, to?, since?}` | With `body`, posts to the tree's board, or with `to` sends one agent a direct message. Without `body`, reads it |
| `tasks {action, …}` | The tree's task list: `add`, `list`, `claim`, `done` |

An agent at `max_depth` gets only `board` and `tasks`. Before M58.3 there
were eleven tools: `wait_agent`, `resume_agent` and `list_agents`
(now `message_agent`), `close_agent` (now `stop_agent`), `board_post`
and `board_read` (now `board`), and `task_add`, `task_list`, `task_claim`
and `task_done` (now `tasks`). Those names still work for one release,
so a transcript or a session that uses them goes on working. They are
never offered, and each call logs a deprecation warning.

## Config

On by default. Every setting is optional:

```toml
[agents]
enabled = true
max_depth = 2              # levels below the agent you talk to
max_children = 4           # running at once, per parent
max_agents = 12            # open (not closed) agents per tree
max_tokens = 2000000       # all of a tree's sub-agents together, per window;
budget_window_hours = 24   # the agent you talk to isn't counted
fork_tokens = 16000        # the most of its parent's conversation a fork starts with

[agents.roles.verifier]    # a role on another provider, from [providers]
provider = "cheap"

[agents.roles.media]       # M51.3: optional, a cheap model that calls tools
model = "cheap"
```

- **`max_children`** and **`max_agents`**: when either is reached,
  `spawn_agent` refuses and names the limit. The model can then wait for
  a child or close one.
- **`max_tokens`**: charged after every model call of every sub-agent in
  the tree. When it's spent, each child stops at its next step with an
  answer that says why, and `spawn_agent` refuses. The window is trailing,
  so a gateway chat can keep using agents day after day.
- **Roles**: `worker` (the default), `planner`, `verifier` and `media`. A
  role without a provider uses the nearest ancestor's role provider, and
  otherwise the root's provider. An unknown role or provider name stops
  ferrule at start, and `ferrule doctor` says which one.
- **`media`** (M51.3): a child for multi-step creative work ("make four
  variants, pick one, animate it") off the chat model's context and
  bill. It gets `generate_image`, `generate_video` and `speak` on top of
  the usual tools, always works in its parent's workspace (never a
  worktree, whatever `worktree` says), and has no chat: the tools save
  under `media/` and say so, and its report names the files for the
  parent to `send_file`. A video it starts is saved there when the
  render finishes. Only the gateway with `[media]` on has the media
  tools; anywhere else (`ferrule run`, a terminal graph) a `media` spawn
  fails and says to spawn a worker. `[agents.roles.media] model` is
  optional: without it the child runs on its parent's model, like any
  role. The media tools' daily cap counts the child's calls too.
- A child can't change any of this. The tools take no arguments for it,
  and a child's children count against the same tree.
- **Effort** (M57.2): `spawn_agent`'s `effort` (`low`, `medium`, `high`)
  runs every turn of the child at that reasoning effort, held to its
  profile's highest and only where its model has an `effort` set
  ([m57-context-hygiene.md](m57-context-hygiene.md#loops-per-file-a-visible-budget-and-raised-effort-m572)).
  A child also hears when its tree's `max_tokens` is down to a quarter,
  and to a tenth.
- **`fork_tokens`** (M57.7): the most of its parent's conversation, in
  estimated tokens, a child started with `fork` gets (below). `0` turns
  forking off: `spawn_agent` with `fork` refuses and says to put what the
  child needs in `task`.

`enabled = false` removes the agent tools everywhere.

## Where the answers go

- **`ferrule run`**: the process doesn't exit while children run. When
  they report, the root runs again on the reports, until none is left
  running. Then the tree is closed and the output says how many
  sub-agents ran and what they spent.
- **`ferrule chat`**: reports reach the root with your next message. The
  prompt line shows how many are waiting. The tree is closed when you exit.
- **Gateway chats (Telegram, local)**: a root that is idle when a report
  arrives is run again, and its answer goes to the chat like any reply. The
  tree lives as long as the gateway runs.
- **Scheduled tasks**: never woken. A task's run ends when it answers, so
  it has to wait for what it needs (`message_agent` with the ids). `ferrule tasks run-now` closes
  the task's tree before it exits. Under the gateway, a child still running
  when the task's run ends goes on running, and its report reaches the
  task's next run.

## Forked children

A child sees only its task, unless its parent forks it:
`spawn_agent {"task": …, "fork": true}` (M57.7) starts it from a copy of
the end of the parent's conversation, then the task. The details are in
[m57-context-hygiene.md](m57-context-hygiene.md#fork-sub-agents-m577).

- The copy comes from the parent's transcript as its model sees it (after
  any compaction), without the parent's system prompt: the child has its
  own, as any child does.
- It ends before the parent's turn that calls `spawn_agent`, and never
  opens with a tool result whose call was left out, so a call and its
  results are never split.
- It is at most `fork_tokens` (16,000 estimated tokens by default). One
  message's text is cut at a quarter of that, so a single big read
  doesn't push out everything before it.
- A header first says whose conversation it is and that the task comes
  after it. Header and copy go into the child's own transcript, so
  a child that's messaged again (`message_agent`) replays them.
- The spawn result says how many of the parent's messages the child got.

## Worktrees

A child that works on the same git repo as its parent gets its own
worktree, under `<data dir>/worktrees/<id>`, on a new branch
`ferrule/<id>`. It can commit there. The parent's checkout never changes;
the parent merges or cherry-picks the branch when it wants the work.

Closing a child (`stop_agent`, the end of a `ferrule run` or `chat`, or
`ferrule agents close`) commits whatever it left uncommitted to its
branch, then removes the worktree. The branch is deleted only if it holds
no commits beyond where it started. Otherwise the output names it.

A **verifier** gets a throwaway copy of the parent's work instead: HEAD,
plus the uncommitted changes, plus new files. It can run the tests there,
and nothing it does reaches the parent's files. The copy is removed when
its run ends.

Outside a git repo, or when git can't make the worktree, a child shares
its parent's workspace, and the spawn result says why.

## What a child may do

A child is built like any agent. It gets the same sandbox, the same
scrubbed environment, the same credential proxy, memory and skills. Only
three things can differ, and each one only narrows:

- its workspace, when it has a worktree
- the repo's git directory as an extra writable root, so it can commit.
  This applies only when that directory is inside the parent's workspace,
  so a child never gets a place its parent couldn't write.
- a verifier with no snapshot works read-only: a read-only sandbox, no
  `write_file`, no `remember`

Things to know:

- **MCP servers are shared.** They run once per process, in the root's
  workspace. A child in a worktree that uses a file-writing MCP tool
  writes to the parent's checkout. Verifiers, and read-only children, only
  get the MCP tools that declare they change nothing.
- **Without an OS sandbox** (`[sandbox] mode = "off"`, or a shell that
  can't run under one — on Windows, Git Bash can't start under the M26
  restricted token and falls back to unsandboxed, where PowerShell runs
  confined; see [windows-sandbox.md](windows-sandbox.md)), "read-only"
  rests on the tool set and the prompt. The shell can still write.
  `ferrule doctor` says so.
- **Everything agents write to each other is data.** Reports, notices,
  board posts and task results reach an agent fenced and marked untrusted.
  An approval relayed by an agent ("the owner said yes") is not an
  approval.

## Several ferrule processes

The gateway, a `ferrule run` and a `ferrule chat` can run at the same time
on the same data dir. Each one holds a lock file under
`<data dir>/agents.owners/` for as long as it lives, and each agent
records which process runs it.

- A process that starts marks as **interrupted** only the running agents
  whose process is gone. It leaves alone the agents another live process
  runs.
- Nothing restarts by itself. An interrupted agent's transcript is on
  disk, and its parent can run it again with `message_agent`.
- `ferrule agents list` marks agents that run in another process.
  `ferrule agents close` refuses to close them, because only that process
  can stop them.

## Commands

```sh
ferrule agents list          # open trees, children under their parents
ferrule agents list --all    # closed ones too
ferrule agents close <id>    # that agent and everything it started
```

`ferrule doctor` shows the limits, the role providers, and any agents
interrupted by a restart or a crash.
