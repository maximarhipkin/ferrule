# Ferrule roadmap

Where Ferrule is and where it's going. This is the plan as it stands; the
day-by-day record lives in `PLAN.md` (Current State and Session Log), and the
reasoning behind each milestone lives in the `docs/research-*.md` reports —
most recently `docs/research-number-one-harness-strategy.md`, the
six-investigation synthesis that produced M14–M19.

## Done

| | What shipped |
|---|---|
| **M1** | `ferrule-gateway`: a `Channel` trait, one normalized message model, one FIFO session lane per chat with JSONL resume. |
| **M2** | Telegram (long polling) and local stdin/stdout channels, run by `ferrule gateway`. |
| **M3** | Scheduler: cron (with a timezone) and one-shot tasks in SQLite, each on its own resumable session, with truthful run status, an optional gate script and no overlapping runs. |
| **Phase 0** | Per-call ledger: every provider call logs tokens, latency, outcome and cost to `ledger.jsonl`, summarized by `ferrule ledger`. |
| **M4** | MCP client over stdio: server tools registered as `mcp__<server>__<tool>`, shared by every session. |
| **M5** | Agent Skills: SKILL.md folders discovered, listed in the system prompt, loaded on demand and kept through compaction. |
| **M6** | OS sandbox for the shell: Landlock (plus seccomp when the network is off) on Linux, Seatbelt on macOS, secret-looking env vars scrubbed. |
| **M7** | Credential gateway: shell commands see placeholders, and a loopback TLS proxy swaps in real keys only for the hosts each key is bound to. |
| **M8** | One-line install, `ferrule setup` wizard, `ferrule doctor`, a systemd/launchd service, Windows builds; `v0.1.0` released. |
| **M9** | Never stuck: retries with backoff, a loop detector, a status answer at every limit, `verify_command` run by Ferrule itself. |
| **M10** | MCP servers under the sandbox, `web_fetch` and MCP-over-HTTP through the credential proxy, a hardened system service as root, Chrome detection in `doctor`. |
| **M11** | The browser: agent-browser's MCP server on an installed Chrome, in the sandbox, behind the proxy, driven for real in CI on Linux, macOS and Windows (`docs/browser.md`). |
| **M12 p1–5** | Sub-agents: `spawn_agent`/`wait`/`resume`/`close`, a board and a task list, a worktree per child, a verifier on a snapshot, roles on their own providers, tree limits and budget (`docs/agents.md`). |

## Next, in order

M11–M13 were approved 2026-09-24 (msg 3090). M14–M19 come from
`docs/research-number-one-harness-strategy.md`, adopted the same day.

### M11 — a browser (done)

**Goal.** The agent can use a real browser for pages that need JavaScript,
logins or clicking, with the same confinement as everything else, and the
owner turns it on in one step.

**Scope.**
- The browser is [agent-browser](https://github.com/vercel-labs/agent-browser)'s
  MCP server (`agent-browser mcp`) driving a Chrome or Chromium that is
  **already installed**. Ferrule detects it and never downloads one (Max's
  decision).
- A `[browser]` config section. `ferrule setup` offers to turn it on when
  `doctor` finds a working Chrome.
- Browser traffic goes through the credential proxy, which Chrome trusts. An
  optional allowed-domains list; open by default.
- Tool descriptions tell the model when to use the browser, and to prefer
  `web_fetch` for static pages.

**Security model.**
- The MCP server and Chrome run under the M10 helper sandbox. The profile and
  cache live in the server's own state dir, never the owner's real profile.
- Chrome keeps its own sandbox. When it can't start inside Ferrule's (Ubuntu
  24.04 restricts unprivileged user namespaces through AppArmor), `doctor`
  explains the options: an AppArmor profile for Chrome first, the global
  sysctl second. Running Chrome with `--no-sandbox` is an explicit, flagged
  opt-out, never a silent fallback.
- Tool arguments that could loosen policy (extra Chrome flags, a different CA,
  a different domain list) are removed before the model sees the tools.

**Done means.** A real headless Chrome is driven through the MCP client and
the sandbox against a test page in CI on Linux, macOS and Windows. Config,
wiring and `doctor` are covered by unit tests. `docs/browser.md` exists.

**Status (2026-09-24): done.** CI drives a real headless Chrome through the
MCP client on ubuntu-24.04, macos-14 and windows-latest. agent-browser's CA
handling needed nothing extra on macOS or Windows.

**Open edges.**
- Tool results are text-only. Images (screenshots) and other non-text
  content are replaced by a note naming what was left out, so the model
  can't see them yet. Real image support is a provider-wide change.
- On Windows the browser runs unconfined, like every MCP server there. It
  also needs a workaround for agent-browser 0.38.1, whose daemon inherits the
  CLI's output pipe and hangs the MCP server's first call: Ferrule runs
  `agent-browser get url` before each call to start the daemon first. No
  upstream issue has been filed yet.
- On macOS, Seatbelt has to allow Chrome's desktop services (window server,
  font and pasteboard lookups), and Chrome's own sandbox can't nest inside
  Seatbelt, so it runs with `--no-sandbox` there. Ferrule's sandbox is the
  only layer.
- The shell can read the browser profile (cookies of sites the agent logged
  into). `read` with a URL skips Chrome's proxy flags.
- The `all` tool set exposes raw CDP.

### M12 — multi-agent (parts 1–5 done)

**Status.** Parts 1–5 are built (`docs/agents.md`): everything below
except named long-lived agents, which wait on a decision about how a chat
addresses one. The board and task list are SQLite in the data dir. After a
restart, running agents are marked interrupted and a parent can
`resume_agent` them. Checked on Linux; the macOS/Windows pass runs with
the batch CI.

**Goal.** One agent can hand work to others and keep going, and the owner can
keep named agents running side by side, without any of them getting more
access than a single agent has today.

**Scope.**
- A `spawn_agent` tool, in-process. It runs in the background by default,
  notifies the parent when it finishes, and can be resumed later with more
  instructions.
- Named, long-lived agents created from one config line or a Telegram
  command, each with its own sessions and memory scope.
- A shared board: entries are tagged with the agent that wrote them and an
  untrusted-origin flag, and are always presented as data, never as
  instructions. Direct agent-to-agent messages alongside it.
- When a child works on a git repo, it gets its own worktree and branch
  automatically, so parallel children don't overwrite each other.
- Limits per agent: spawn depth, concurrent children, and a token/cost budget
  enforced from the ledger.

**Design deltas from the strategy research** (§5): a summary contract on
child results (~1–2k tokens distilled), effort-scaling rules in the spawn
tool description (simple = 1 agent, comparison = 2–4, complex = 10+),
a verifier-subagent role, routing-by-role (planner on the strong model,
workers and the verifier on the cheap one), `resume_agent`/`wait_agent`/
`close_agent` as first-class tools, a task list with dependency edges that
children self-claim, and agent-relayed approvals treated as untrusted input.

**Security model.** Children run under the same sandbox and credential proxy
as the parent, never with more. Anything that came from another agent, the
board, or the web carries its origin, and the prompt says so. A child can't
raise its own limits or its parent's.

**Cost.** Multi-agent runs use many times more tokens than a single chat.
Anthropic measured about 15 times the tokens of a chat for its own
multi-agent research system. The budget
limits and the ledger are there so this is a choice the owner can see and
cap, not a surprise.

**Done means.** A parent spawns two children on the same repo; they work in
separate worktrees, post to the board, and the parent gets both results,
with depth, concurrency and budget limits tested by hitting them.

**Open questions.** How a Telegram chat addresses a named agent. Whether the
board is SQLite in the data dir (likely) or per workspace. How a resumed child
picks up after a restart.

### M13 — self-extension

**Goal.** The agent can add skills and MCP servers while it runs, without a
restart, and without the owner approving every install from a trusted place.

**Scope.**
- Hot-loading: honour `notifications/tools/list_changed`, and add or remove
  servers and skills without restarting.
- Install tools (`mcp_add`, skill install from git) that take sources from an
  **allow-list of approved sources** without asking (Max, msg 3074). Anything
  else asks the owner.
- A scan of new tool descriptions for instructions aimed at the model, and a
  re-scan whenever a server's tool list changes.
- Self-written skills, kept only after they've been verified to work.

**Security model.** This is the milestone with a measured attack surface:
tool-description poisoning succeeded 36.5% of the time on average in the
MCPTox study. Installed servers run under the M10 sandbox and proxy like any
other, the allow-list is the owner's, and a source outside it always needs a
person.

**Done means.** An allow-listed server is installed and used in the same
session; a non-listed one waits for approval; a poisoned description is
flagged by the scan in a test.

**Open questions.** What an allow-list entry names (a registry, a GitHub org,
an exact package and version). Whether updates to an allowed package are also
automatic.

### M14 — `ferrule eval`

**Goal.** "Smartest harness" becomes a measurement, not a slogan: every
prompt, profile and threshold change is regression-tested — and the
harness effect itself is demonstrated with ferrule's own numbers.

**Scope.** Task suites (a prompt, a workspace fixture, a grader — a
`verify_command` and/or an LLM rubric) run through the real agent; results
appended to `ledger.jsonl` with `task_shape="eval"`. Capability and
regression suites kept separate; about twenty real tasks are enough to see
large effects (Anthropic's eval guidance).

**The A/B demonstration** (replicates the ARC-AGI-3 experiment with our
own agent): each suite runs twice against the *same* model —

- **naive variant**: `HarnessProfile { retain_reasoning: false,
  compaction_threshold: 1.0 }` plus a rolling-truncation path in the loop
  (drop oldest non-system messages when over budget — the "default
  harness" behavior ferrule replaced), no `verify_command`, no memory
  tools, no retries or stuck detector, empty `system_directive`. All of
  these are existing knobs except the truncation path, which is a small
  `agent.rs` addition.
- **engineered variant**: the normal per-model profile with everything on.

The report compares pass rate, output tokens and cost per variant, from
the ledger. The cheap way to make the effect large and reproducible:
run a **small local model at a small context window** (Ollama, 8–32k) —
context pressure arrives at task sizes where truncation destroys the run
but compaction plus goal-pinning survives, at zero API cost. The measured
chart goes into the README next to the ARC-AGI-3 one.

**Done means.** One command runs a suite against the current build and
reports, from the ledger, what changed versus the last run; and the A/B
suite shows the naive variant losing on the tasks where harness features
matter, with mock-LLM tests proving the machinery in CI.

### M15 — memory update pipeline + reversible compaction

**Goal.** Memory stops only growing, and compaction stops being one-way.

**Scope.** Update and delete tools over the memory store (an
ADD/UPDATE/DELETE decision on insert, Mem0-style), `forget` exposed to the
agent, recall at session start driven by the session's goal, a
`superseded_by` column so updates invalidate rather than contradict. A
read-only `search_history` tool over the session transcript, and truncation
of old large tool results (keeping a reference), so anything a summary
loses stays retrievable.

**Done means.** The agent corrects a stored fact and later recall prefers
the correction; a compacted session answers a question whose answer was
only in the dropped history.

### M16 — the learning loop

**Goal.** The agent gets better at the owner's work between sessions — the
sandbox-compatible form of self-improvement.

**Scope.** A scheduled offline pass (the M3 scheduler makes this
config-free) that reviews recent sessions, deduplicates and consolidates
memories, and curates a playbook: delta bullets appended to the system
prompt, proposed by a reflector pass after failed or retried runs, with
additions gated on `verify_command` success (ACE, arXiv:2510.04618) and
never full rewrites. Cost-capped from the ledger; every change is a file
the owner can read.

**Done means.** After a failing scheduled task is fixed, the next similar
run follows the recorded lesson — visible as a playbook diff.

### M17 — MCP hot-add + `ferrule mcp add`

**Goal.** Connecting a server is one guided command, not a hand-edited
TOML file and a restart.

**Scope.** `ferrule mcp add <name>` with a live `tools/list` smoke test,
config written with `toml_edit` (comments survive), secrets bound to
hosts in the same step, `doctor` re-run at the end. New servers register
into future sessions without restarting the daemon;
`notifications/tools/list_changed` is honoured. A setup-wizard MCP step,
per-server `enabled_tools` filters and per-tool output caps.

**Done means.** A server added via the command is usable by the next
Telegram message, with no daemon restart; a tool-list change mid-session
is picked up and re-scanned (M13's hook).

### M18 — lifecycle hooks

**Goal.** Ferrule's built-ins (`verify_command`, gate scripts) become
instances of a general mechanism users can automate on — the extension
point Claude Code and Codex converged on.

**Scope.** Events: SessionStart/End, UserPromptSubmit, PreToolUse,
PostToolUse, Stop, PreCompact/PostCompact, SubagentStart/Stop. Command
handlers at first; an exit code of 2 blocks and feeds stderr back to the
model; `additionalContext` injects a note at that point in the turn.
Hooks from a workspace are gated by a trust switch, like project skills.

**Done means.** `verify_command` runs as a Stop hook; a user's PreToolUse
hook blocks a command in a test and the model sees why.

### M19 — trust & cost

**Goal.** Unattended runs are capped and safe to leave alone.

**Scope.** Hard budget caps — tokens and dollars per run, per day and per
task — with a kill switch and an 80%-spent warning delivered to the
owner's channel. Approval gates for destructive or irreversible actions
(workspace `rm -rf`, force-push, deletions through a bound-host
credential) as a Telegram approve/deny round-trip. Plan mode: read-only
exploration, the owner approves the plan, then execution — built on the
existing `read-only` sandbox mode.

**Done means.** A run stops at its cap and says so; a destructive command
waits for a Telegram approval and proceeds only on "yes"; a plan-mode run
changes nothing before approval.

## Other open tracks

- **Phase 1 routing** (`docs/research-routing-and-local-models.md`): a
  rule-based `RouterProvider` that starts cheap and escalates. **Blocked on
  Max:** what counts as "hard enough to escalate", and which providers become
  the tiers (it needs a second live provider first). Phases 2–3, a learned
  router and local LoRA, were dropped (msg 3070).
- **Native Windows sandbox** (`docs/research-windows-sandbox.md`): Windows has
  no sandbox backend today. The recommendation is a restricted token with a
  capability SID inside a Job Object, the same approach as OpenAI Codex's
  unelevated mode, which needs no admin rights. The main unknown is whether Git
  Bash and PowerShell start under that token.
- **Open edges from M10:**
  - the system-service path (root, systemd) has no end-to-end run yet;
  - plain HTTP from `web_fetch` goes direct, since the proxy only handles
    CONNECT;
  - an unconfined MCP server (`sandbox = false`, or any server on Windows) can
    still read the saved keys and `/proc/<ppid>/environ`;
  - the Streamable HTTP transport has no server-initiated stream or resumption.
    (The Chrome launch check now runs on macOS and Windows in CI, where M11
    drives a real Chrome.)
- **Anytime:** parallel read-only tool calls, streaming replies, and a stable
  prompt prefix for caching.
- **The strategy backlog** (`docs/research-number-one-harness-strategy.md`
  §4): a `web_search` tool, keyword-triggered skills, Aider-style edit
  mechanics (SEARCH/REPLACE edits, a repo map, per-edit lint, atomic
  commits), local-model first-run polish, migration importers from
  OpenClaw/Hermes, channels in the order Discord → Slack → WhatsApp, a
  read-only dashboard, an SSH execution backend, an egress domain policy in
  the proxy, and OTel export from the ledger.
