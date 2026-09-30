# Ferrule roadmap

Where Ferrule is and where it's going. This is the plan as it stands; the
day-by-day record lives in `PLAN.md` (Current State and Session Log), and the
reasoning behind each milestone lives in the `docs/research-*.md` reports —
most recently `docs/research-round2-improvements.md`, the round-2
six-investigation synthesis at v0.9.0; its predecessor,
`docs/research-number-one-harness-strategy.md`, produced M14–M19.

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
| **M13** | Self-extension: skills and MCP servers hot-load mid-session and self-install from an owner allow-list, behind a poisoning scan and exact pins (`docs/m13-self-extension.md`). |
| **M14 p1–5** | `ferrule eval`: task suites through the real agent loop with command and rubric graders, a naive/engineered A/B on one model, and a diff against the last run (`docs/eval.md`). |
| **M15** | Memory update pipeline and reversible compaction: `update_memory`/`forget` with supersede, goal-driven recall, and `search_history` over the transcript (`docs/m15-memory.md`). |
| **M16** | The learning loop: `ferrule learn` reviews failed runs and keeps a playbook lesson only when the task's check passes twice with it (`docs/m16-learning-loop.md`). |
| **M17** | `ferrule mcp add` with a live smoke test, secrets bound in the same step, and hot add/remove in a running gateway (`docs/m17-mcp-add.md`). |
| **M18** | Lifecycle hooks: ten events with Claude Code's payload and exit-code contract, `verify_command` as the built-in Stop check, workspace hooks behind a SHA-256 trust pin (`docs/m18-hooks.md`). |
| **M19** | Trust & cost: token and dollar caps read from the ledger, a kill switch, approval gates on destructive actions, and plan mode (`docs/m19-trust-cost.md`). |
| **M19b** | Never silently deaf: 👀 receipts, `/status` at any time, a turn watchdog, restart notices and an optional heartbeat (`docs/m19b-reliability.md`). |
| **M19c** | Live-bot fixes: every reason the bot stays silent is said in the chat or shown by `ferrule status`/`doctor` (`docs/m19c-live-fixes.md`). |
| **M20** | Connections: the owner taps one Telegram button to connect a service; tokens are sealed, refreshed per request and never seen by the model (`docs/connections.md`). |
| **M21** | Models: several connected at once with prices and windows, a default, pins per chat/task/role, and an optional fallback (`docs/models.md`). |
| **M22** | The dashboard: one loopback page for health, connections, models, usage, tasks and logs, opened by a one-use link, working with every model down (`docs/dashboard.md`). |
| **M23** | Native drivers: Anthropic Messages and OpenAI Responses beside the chat API, with Anthropic prompt caching priced in the ledger (`docs/models.md`). |
| **M24** | Dashboard leftovers: logins survive restarts, candidate-model evaluation and audited edits of caps, MCP, skills, hooks trust and tasks from the page (`docs/dashboard.md`). |
| **M25** | Routing, phase 1: `[routing] tiers` start every turn cheap and escalate one tier on a failure signal; off by default (`docs/routing.md`). |
| **M26** | Isolation: sandboxed reads (`deny_read`), a native Windows sandbox (a restricted token in a job object, no admin), plain-HTTP `web_fetch` through the proxy, hide-only unconfined MCP servers (`docs/sandbox.md`, `docs/windows-sandbox.md`). |
| **M27** | Speed: parallel read-only tool calls, streaming replies in Telegram and `ferrule chat`, a cache-stable prompt prefix, and the timings in `ferrule ledger` (`docs/speed.md`). |
| **M28** | `web_search` (Brave, Tavily, Exa, SearXNG) through the proxy with a daily cap, and keyword-triggered skills (`docs/web-search.md`, `docs/skills.md`). |
| **M29** | Edit mechanics: `edit_file` SEARCH/REPLACE, a tree-sitter repo map with `code_search`, per-edit lint, optional auto-commit with `ferrule undo` (`docs/editing.md`). |
| **M30** | Vector recall: a local opt-in embedder or any `/v1/embeddings` endpoint merged with BM25; exactly BM25 when it's off (`docs/memory.md`). |
| **M31** | Discord and Slack channels, outbound only, with per-channel allowlists, pairing codes and approval buttons (`docs/discord.md`, `docs/slack.md`). |
| **M32** | WASM tool plugins in wasmi with deny-by-default capabilities, installed through M13's flow (`docs/plugins.md`). |
| **M33** | Ops: an egress policy in the proxy, a Unix-socket allowlist, OTel traces, and OpenClaw/Hermes importers (`docs/egress.md`, `docs/otel.md`, `docs/migrate.md`). |
| **M34** | SSH workspaces over the system `ssh` with strict host keys, and local-model first run for Ollama/llama.cpp/LM Studio/vLLM (`docs/ssh.md`, `docs/local-models.md`). |
| **M35** | Subscription sign-in: a ChatGPT plan natively (Codex OAuth) and a Claude plan through the unmodified `claude` binary (`docs/subscriptions.md`). |
| **M36** | Self-update (signed releases with rollback) and self-repair (failure classification, fallbacks, a last-good config) (`docs/updates.md`). |
| **M37** | The control room: closable notices with fix buttons, model pickers, connections that work, terminal parity, a phone UI (`docs/dashboard.md`, `docs/connections.md`). |
| **M38** | Named instances: several agents on one machine, each with its own config, data, bot, service and dashboard, with coordinated updates (`docs/instances.md`). |

## Milestones

Every one of these has shipped; the sections below are the record, with
each one's scope, design deltas and open edges as they stood at the time.
M11–M13 were approved 2026-09-24 (msg 3090). M14–M19 come from
`docs/research-number-one-harness-strategy.md`, adopted the same day.
M21 came from Max on 2026-09-25 (msg 3160).

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

**Status.** Built 2026-09-24 on branch `m13-self-extension` (PR open, not yet
merged; the macOS/Windows parts are unverified until the batch CI pass). Design:
`docs/m13-self-extension.md`. With M12: sub-agents never get the install tools —
only the top-level agent installs; children use what's installed, narrowed by role.

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

**Open questions — answered as defaults, for Max to confirm.**
- *What an allow-list entry names:* a **publisher** — an npm scope, a git org,
  a URL prefix — or, tighter, one package or one exact version. Every install
  still carries an exact pin (npm/PyPI version, git commit). Registry-wide
  entries (`npm:*`, `git:https://github.com/*`) are refused.
- *Whether updates are automatic:* **no.** An update is a reinstall with a new
  pin, through the same allow-list, approval and scan.
- *On by default:* **no** — `[extensions] enabled = false`; the owner turns it
  on. Configured servers are scanned and re-scanned either way.

### M14 — `ferrule eval` (parts 1–5 done)

**Status.** Built (`docs/eval.md`). What it does:
- `ferrule eval run` runs a suite through the real agent loop, on any provider.
- The naive/engineered A/B uses the same model for both variants.
- Tasks are graded by commands and/or an LLM rubric.
- Rubric criteria are checked against quotes from the evidence.
- Calls go on the ledger tagged eval, under a budget cap.
- The dry run prints a worst-case price before anything is sent to the model.
- Every report ends with the diff against the last run. `ferrule eval report`
  re-prints a saved run.

It ships with a 20-task starter suite and a mock model for trying it with no key. Not
done yet: the measured run on a real small local model and its chart. It has been
checked on Linux; the macOS/Windows pass runs with the batch CI.

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

**Status.** Built (`docs/m15-memory.md`; PR open). `remember` recognises a
fact it already has and shows similar ones; `update_memory` supersedes a fact
(`superseded_by`) and recall returns the correction even for the old wording;
`forget` deletes a chain for good. The session-start memory block comes from
the session's goal. When compaction triggers, old large tool results shrink to
a preview plus a ref, and `search_history` fetches them, or anything a summary
dropped, from the session transcript. Sub-agents: writing children add only,
read-only children recall only. Checked on Linux; the macOS/Windows pass runs
with the batch CI.

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

**Status.** Built (`docs/m16-learning-loop.md`; PR open). `ferrule learn
run` (or a built-in nightly task once `[learning] enabled = true`; off by
default) reviews failed or retried runs. A reflector proposes one playbook
delta each, and an addition is kept only when the task's check passes twice
in a scratch copy with the lesson in the prompt. Near-duplicate memories are
merged through M15's UPDATE. Every pass is a folder of readable files, with
`ferrule learn show`/`diff`/`revert`. Caps per pass and per day come from the
ledger (`call_kind = "learn"`). Sub-agents see the playbook but can't write
it; eval doesn't see it unless a suite opts in. Checked on Linux; the
macOS/Windows pass runs with the batch CI.

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

### M17 — MCP hot-add + `ferrule mcp add` (done)

**Status.** Built (design: `docs/m17-mcp-add.md`).
- `ferrule mcp add <name> [-- cmd…] | --url …` starts the server as the
  daemon will (same sandbox, a throwaway proxy with its secrets), runs
  `initialize` + `tools/list`, and scans the tools with M13's scanner.
  A failure, a scan block without `--waive`/`--skip-flagged`, or a key
  in `--env`/a header writes nothing.
- `--secret NAME[=hosts]` binds a key to hosts in `[secrets]`; the value
  goes to the private secrets file, never the config. Written through
  `toml_edit`, so comments and order survive. An offline doctor runs at
  the end and now names each server.
- A running gateway or chat polls its config every 2 s and starts, stops
  or restarts servers and binds new secrets live: the next message has
  the new tools. Only `--config`, `$FERRULE_CONFIG` or the global config
  are followed.
- `enabled_tools`, `max_output_chars` and `output_caps` per server; a
  mid-session `list_changed` is re-scanned.
- `ferrule mcp list`/`remove`, and an "MCP servers" step in `ferrule setup`
  on the same code.

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

### M18 — lifecycle hooks (done)

**Status.** Built (design: `docs/m18-hooks.md`).
- Ten events — SessionStart/End, UserPromptSubmit, PreToolUse,
  PostToolUse, Stop, PreCompact/PostCompact, SubagentStart/Stop — with
  Claude Code's JSON payload on stdin and its exit-code contract: 0
  proceeds (stdout may be JSON), 2 blocks with stderr as the reason the
  model sees, anything else is logged and shown to the owner, never the
  model. Matchers are names or globs.
- Command hooks have a per-hook timeout; on timeout the whole process
  tree is killed and the turn goes on. `additionalContext` is appended
  after the cached prefix, never in the system prompt, capped at 10,000
  characters.
- `verify_command` is the built-in Stop check, with the same message,
  cap and events as before; a Stop hook that always blocks is capped
  (`max_stop_blocks`, default 3) and the run ends incomplete. Gate
  scripts stay gates (they decide whether a session starts at all).
- User hooks come from the trusted config only (`--config`,
  `$FERRULE_CONFIG`, the global config). Workspace hooks
  (`.ferrule/hooks.toml`) need `[hooks] project = true` and `ferrule
  hooks trust`, at a terminal, pinned to the file's SHA-256; any edit
  untrusts them, and the owner is told what won't run.
- **Hooks run with the owner's privileges, outside the sandbox.** The
  model can't add, edit or enable one: no tool touches hook config, the
  trust record is under `private/`, and `trust` needs a terminal.
- Sub-agents inherit PreToolUse/PostToolUse; SubagentStart/Stop fire in
  the parent. Every run goes to `<data dir>/hooks/runs.jsonl`; `ferrule
  hooks list` shows the hooks and recent runs. Eval never loads hooks.

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

**Status.** Built (`docs/m19-trust-cost.md`; PR open). Caps on tokens and
dollars per run, per day and per scheduled task, read from the ledger so
they hold across processes and restarts, with a one-time warning at 80%
sent to the owner's Telegram chat. A kill switch (`ferrule stop`, `/stop`
from any allowed chat, `/resume` from the owner's) halts running calls and
holds the scheduler. `rm -r`, force pushes and DELETEs to a bound host wait
for the owner's `yes` over Telegram or at the terminal; an unattended run
refuses them. Plan mode (`ferrule run --plan`, `/plan`) explores read-only
and runs the plan only once approved. Sub-agents share their root's caps,
switch, gates and plan phase. Eval is untouched unless a suite sets
`owner_trust = true`. With M18 merged in, the owner's gate answers
before PreToolUse hooks, a refused call fires no hooks, and plan mode and
eval fire none (§13 of the doc). Everything but the Telegram round-trips works
without Telegram. Checked on Linux; the macOS/Windows pass runs with the
batch CI.

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

### M19b — reliability: never silently deaf

**Status.** Built (`docs/m19b-reliability.md`; PR open). The owner's server
takes no inbound connections, so everything reaches the phone: 👀 on every
message the gateway admits, before the lane, and one "busy, queued"
notice; `/status` from any allowed chat while a turn hangs, and `ferrule
status` on the box; one watchdog message after 10 minutes without
progress, and `max_turn_minutes` to end a turn like `/stop`; a restart
notice after a crash or kill naming the interrupted turn (never re-run);
systemd's watchdog restarting a gateway that can't poll; an optional
heartbeat for an outside checker (healthchecks.io today, M20's relay
later). Eval stays hermetic. Checked on Linux; the macOS/Windows pass runs
with the batch CI.

**Done means.** From a phone alone the owner can tell that a message
arrived, what the agent is doing or where it's stuck, and that the
process is down — and a wedged or crashed gateway comes back and says so.

### M21 — models

**Status.** Built (`docs/m21-models.md`, user guide `docs/models.md`; PR
open). Several models connected at once, named `provider/model`, by a
provider, an alias or a unique model id, with their own prices, context
window and profile; old single-model configs unchanged. A default,
changed from `ferrule setup`, `ferrule model default` or `/model default`
in Telegram (owner only), written under a lock that survives Windows'
rename. A model per chat (`/model use`), per task (`ferrule tasks add
--model`), per role and per sub-agent (`spawn_agent`'s `model`, connected
models only, inside its root's caps, gates and plan mode); one-off > role >
task > chat pin > default. An optional fallback list (off by default) for
outages only, told to the owner once. The model that ran is in the ledger,
the audit log and `/status`, and caps price by it. `ferrule model test`,
setup and `ferrule doctor --ping-models` make one real call and say why a
model fails. Presets: OpenAI, Anthropic (OpenAI-compatible endpoint, no
prompt caching), Gemini, OpenRouter, DeepSeek, Kimi, Groq, Ollama. The
eval never follows the owner's default, pins or fallback. M22's dashboard
reads and changes all of it through one `Models` API.

**Done means.** The owner connects two models, makes one the default from
the phone, pins a chat and a task to the other, and a sub-agent runs on a
third; each call's model shows in the ledger, and an outage moves the turn
to the fallback with one message.
### M20 — connections

**Status.** Built (`docs/m20-connections.md`; PR open).
- The agent asks for a service; the owner taps one Telegram button and logs in.
- The login code comes back through the owner's own Cloudflare Worker relay (one
  read, 5 minutes). Else it comes back through a cloudflared quick tunnel (DCR
  services), else through a pasted address.
- Tokens are sealed on disk, refreshed per request and never seen by the model.
- API keys come through a form that encrypts in the browser.
- The seven-service catalog: Atlassian, Attio, Gmail, Google Drive, GitHub,
  Notion, Linear.
- Access is read-only by default. Tools that can change something go through
  M19's gate.
- `/connections` and `/disconnect` work from Telegram; `ferrule connections` works
  at the terminal.
- Eval stays hermetic.
- Checked on Linux, and the relay was checked live. Real provider logins aren't
  verified yet.

**Done means.** The owner connects a service from a phone with one tap
and a login, and its tools appear. No token ever reaches the model, a
tool result, the audit log, `/status` or an error.

### M19c — live-bot fixes

**Status.** Built and merged (`docs/m19c-live-fixes.md`; PR #15; ships as 0.2.1).
Found on a live bot that stayed silent. Every reason it doesn't answer is
now told to the owner in Telegram, or shown by `/status`, `ferrule
status` and `ferrule doctor`:
- The gateway logs warn plus info from ferrule, with no token in any line.
- A 409 lasting a minute is told once, naming both causes, and its end too.
- A webhook on the bot is removed at start, keeping waiting messages.
- A caption becomes the text; a voice message, photo or file gets a plain
  reply.
- An ignored chat is warned once an hour.
- OpenRouter's no-tool-endpoint 404 and free-pool 429 come out in plain
  words, and a rate-limit wait counts down.
- Doctor warns about a webhook, a second gateway and a `:free` model, and
  prints where the service logs are.

**Done means.** An owner whose bot doesn't answer finds the reason in the
chat, or by going /status → `ferrule status` → `ferrule doctor` → the
journal, without reading code.

### M22 — the dashboard

**Status.** Built and merged (`docs/m22-dashboard.md`, user guide
`docs/dashboard.md`; PR #16).
- One page on 127.0.0.1: health first (with a failing default's fix on
  top), connections, models with an OpenRouter catalog and
  recommendations, usage from the ledger, tasks, logs, extensions and
  sub-agents.
- The owner sends `/dashboard`, gets a one-use 10-minute link, and a
  cloudflared quick tunnel opens on demand. `/dashboard off` revokes it
  all.
- Session cookies, CSRF, Origin checks and a host allow-list. Destructive
  operations ask to confirm.
- It works when the model doesn't: nothing on it calls the model, and
  `/dashboard` is answered by the gateway itself.
- `ferrule model catalog|recommend|fill-prices`; `doctor` warns on
  unpriced models.
- Eval stays hermetic. Checked on Linux; the Cloudflare account is
  unchanged.

**Done means.** From a phone, with every model down, the owner opens the
page, sees the outage on top, picks a catalog model as the default, and
the next message works.

### M23 — native drivers

**Status.** Built (`docs/m23-drivers.md`; user guide in `docs/models.md`,
Drivers). PR to `main` open, not merged.
- Three drivers behind the one `Provider` trait: OpenAI-compatible Chat,
  Anthropic's native Messages API, and OpenAI's Responses API, stateless
  (`store: false`, encrypted reasoning).
- `api = "chat" | "anthropic" | "responses"`, inferred from `base_url`
  when unset. A v0.3.0 Anthropic config moves to native by itself;
  `api = "chat"` keeps the old route.
- Anthropic prompt caching with cache writes priced in the ledger;
  optional thinking and reasoning, replayed unchanged within a turn,
  never shown or logged.
- Fallback across drivers mid-conversation carries the plain transcript
  only. `model test`, doctor and the dashboard show each model's driver.
- Failure classes and per-call cost and latency are ready for M25's
  router, which isn't built here.

**Done means.** An Anthropic user's second turn is billed mostly at the
cached price, and a conversation that falls over from Claude to another
driver mid-turn keeps going.

### M24 — the dashboard's leftovers

**Status.** Built, PR open (`docs/m24-dashboard-2.md`, user guide
`docs/dashboard.md`).
- The login survives a gateway restart: sessions are stored hashed, the
  CSRF token is derived, and a live tunnel session gets a new link.
- Evaluate a candidate model from the page or with `ferrule model eval`:
  an estimate, a confirm, under the owner's caps and kill switch, with
  the result beside the default's.
- Edit from the page: caps (confirm on raise), MCP, skills, hooks trust
  pinned to the file's hash with a diff, and a task's schedule and model.
  The same audited operations serve Telegram and the CLI.
- `scripts/dashboard-smoke.sh|.ps1` walks the whole path in about 2
  minutes, and an RTL test keeps Hebrew readable.

**Done means.** After a restart the phone is still logged in (or has a
new link), the owner tries a cheaper model on the real suite before
switching, and changes caps, extensions, hooks trust and tasks without
editing the config by hand.

### M25 — routing, Phase 1

**Status.** Built (`docs/m25-routing.md`; user guide `docs/routing.md`).
PR to `main` open, not merged.
- `[routing] tiers`, cheap → strong. Every turn starts cheap and moves up
  one tier on a failure signal only: a call failure retrying won't fix,
  invalid tool calls in a row, a failed check, a Stop hook, no progress,
  the watchdog, or `/model strong`. Sticky for the turn, back down at the
  next one. Off by default, byte-identical when off.
- Tier refs set the floor for pins, tasks, roles and sub-agents; M21's
  fallback still covers outages. Ledger rows say which tier served and why
  it moved; an optional daily cap on spend above the cheap tier.
- `ferrule model route`, `/model strong|tiers`, the dashboard's Routing
  section and API, doctor.
- `ferrule eval --variant routing` compares cheap only, routed and strong
  only on pass rate and cost.

**Done means.** On a real pair, `routed` passes close to `strong` at close
to `cheap`'s price, and the ledger says why each escalation happened.

### M26 — isolation

**Status.** Built (`docs/m26-isolation.md`; user guides `docs/sandbox.md`
and `docs/windows-sandbox.md`). PR to `main` open, not merged.
- Sandboxed reads: ferrule's secrets, the usual credential dirs and
  browser profiles, and the owner's `deny_read` are closed to commands, MCP
  servers and the file tools. `allow_read` re-opens a default.
- A native Windows sandbox with no admin: a restricted token in a job
  object. Writes stay in the workspace and temp, the tree dies with the
  command, and ferrule's secrets and process are shut. The network isn't
  enforced there.
- Plain-HTTP `web_fetch` goes through the credential proxy. A
  `sandbox = false` MCP server still can't read the secrets, and doctor
  lists what it can do.

**Done means.** A prompt-injected command can't `cat ~/.ssh/id_ed25519` or
the saved keys on any OS, and a Windows user gets the same write
confinement as Linux and macOS.

### M27 — speed

**Status.** Built (`docs/m27-speed.md`; user guide `docs/speed.md`).
PR to `main` open, not merged.
- Read-only tool calls from one response run in parallel (`[agent]
  parallel_tools`, default 4); writes stay ordered barriers, approvals stay
  serial, results keep the model's order.
- All three drivers stream when asked. Telegram replies grow by edits
  (throttled to 1/s, 429s honoured, rollover past 4000 chars, never lost on
  a failed edit); `ferrule chat` prints as it comes. `[agent] stream`,
  `[gateway] telegram_stream`.
- A cache-stable prefix: recalled memory moved out of the system prompt,
  and the Anthropic previous-turn breakpoint fixed; a test pins the bytes.
- `ferrule ledger` shows cache hit, time to first token and first reply,
  and parallel batches' wall vs summed time. The eval is unchanged.

**Done means.** On a live Telegram chat the first words show within about
a second, the ledger's cache hit climbs across sessions, and a turn of
several reads takes about as long as its slowest one.

### M28 — search and skills

**Status.** Built (`docs/m28-search-skills.md`; user guides
`docs/web-search.md` and `docs/skills.md`). PR to `main` open, not merged.
- `web_search`: Brave, Tavily, Exa or SearXNG through the credential
  proxy, off by default, every search in the ledger with a daily cap.
- Keyword-triggered skills: `triggers:` in SKILL.md loads a skill with the
  message that names it. Only a person's message counts, the skill is
  re-vetted at each match, and it lands after the message so the cached
  prefix holds.
- Fixes: the eval's "failed checks fixed" line, and `HTTP_PROXY` for
  sandboxed commands.

**Done means.** The agent answers a question about this week's news with
sources, within the owner's search cap, and a skill loads the moment the
owner says its keyword, and never because a web page did.

### M29 — edit mechanics

**Status.** Built (`docs/m29-edit-mechanics.md`; user guide
`docs/editing.md`). PR to `main` open, not merged.
- `edit_file`: SEARCH/REPLACE edits that apply all or nothing, never
  fuzzily, keep the file's line endings and encoding, and fail with the
  closest region and what to try next. `write_file` stays in every
  profile.
- A tree-sitter repo map (Aider-style PageRank, cache-friendly placement)
  and `code_search` for definitions and references, in code repos only.
- Per-edit lint with the project's own rustfmt/ruff/gofmt/eslint/tsc, as
  a built-in PostToolUse hook.
- Optional auto-commit of exactly the agent's files on its own branch,
  with `ferrule undo` and `/undo`.
- `ferrule eval --edit-tools write-only` to measure `edit_file` against
  whole-file rewrites.

**Done means.** On a real model, `--edit-tools both` passes at least as
often as `write-only` for fewer tokens per pass, and an unattended run's
changes arrive as one reviewable, undoable commit.

### M30 — vector recall

**Status.** Built (`docs/m30-vector-recall.md`; user guide
`docs/memory.md`). PR to `main` open, not merged.
- Memory recall by meaning as well as keywords: a local multilingual
  model (a 531 MB opt-in download, no key) or any `/v1/embeddings`
  endpoint through the credential proxy, merged with BM25.
- Every M15 promise kept: time decay, supersede, `forget`, the token
  budget, and exactly BM25 when the embedder is off or failing.
- A benchmark (80 facts, 48 queries) decides the merge; hybrid ships
  because it beats BM25 on paraphrase and Hebrew↔English.

**Done means.** Asked "where do we ship production?", the agent recalls
"the deploy target is render"; a Hebrew question finds an English fact
some of the time; and with the embedder unset, nothing differs from M15.

### M31 — Discord and Slack

**Status.** Built (`docs/m31-channels.md`; user guides `docs/discord.md`
and `docs/slack.md`). PR to `main` open, not merged.
- Discord (Gateway v10) and Slack (Socket Mode) adapters, both outbound
  only, run by the same daemon as Telegram.
- Per-channel allowlists, mention-only shared channels, pairing by a
  one-time code in `ferrule setup`; strangers never reach the model.
- The owner can live on any channel; approvals get buttons on Discord and
  Slack.
- A dead socket shows in `/status`, `ferrule status`, the watchdog, the
  dashboard and `ferrule doctor`, and stops only its own channel.

**Done means.** A real bot on each answers its owner, refuses a stranger,
and survives a dropped socket. WhatsApp comes next (options in the design,
§9).

### M32 — WASM tool plugins

**Status.** Built (`docs/m32-wasm-plugins.md`; user guide
`docs/plugins.md`). PR to `main` open, not merged.
- Tools as small WebAssembly modules run in wasmi. A module can reach
  nothing it wasn't granted: workspace directories, HTTPS domains through
  the credential proxy, secrets as placeholders only, and the clock.
- Installed through M13's flow: exact pins and a SHA-256, the scan, and
  the owner approving the capabilities (again whenever they widen).
  Tampering suspends the plugin.
- `ferrule plugins add/list/remove`, the agent's `plugin_add`, doctor, a
  Rust SDK and two examples.

**Done means.** The owner installs a plugin from a git commit, sees
exactly what it may touch, and the agent uses it in the same session. A
plugin that loops, bloats or asks for an undeclared host fails as a tool
error, and a plugin never holds a real key.

### M33 — ops: egress policy, OTel export, importers

**Status.** Built (`docs/m33-ops.md`; user guides `docs/egress.md`,
`docs/otel.md`, `docs/migrate.md`). PR to `main` open, not merged.
- An egress policy in the credential proxy: public hosts by default,
  private ranges and cloud metadata blocked, allow/deny lists, no DNS
  rebinding, a refusal the model can read and the owner can audit.
- A Unix-socket allowlist that closes the `docker.sock` escape (Linux
  seccomp supervisor, macOS Seatbelt).
- OTel traces (OTLP/HTTP JSON) from the ledger seam, GenAI semconv,
  content off by default and scrubbed when on, never blocking the agent.
- `ferrule import openclaw|hermes`: memories, skills, allowlists,
  providers, keys by name; dry run first, idempotent, offered by setup.

**Done means.** A page that tells the model to fetch the metadata
address gets a clear refusal; `curl --unix-socket /var/run/docker.sock`
fails in the sandbox; a turn shows up as a trace in Jaeger; and an
OpenClaw or Hermes user runs one command to bring their memories and
allowlists over, twice, with nothing changing the second time.

### M34 — SSH workspaces and local-model first run

**Status.** Built (`docs/m34-ssh-local.md`; user guides `docs/ssh.md` and
`docs/local-models.md`). PR to `main` open, not merged.
- The workspace can be a directory on another machine: the shell and file
  tools run there over the system `ssh`, and everything else stays local.
  Host keys are never trusted silently, a changed key is a hard stop, and
  ferrule never touches a private key. The remote account is the
  boundary.
- A dropped connection interrupts a command rather than retrying it;
  `/stop` kills the remote process group; the credential proxy reaches
  remote commands through `ssh -R`.
- Ollama, llama.cpp, LM Studio and vLLM are found by setup and doctor.
  The window the server really gives is checked against the one ferrule
  plans for, with the fix, and a probe tells a model that can't call
  tools from a broken chat template.

**Done means.** The agent edits and tests a project on a remote host
without the owner's key ever leaving ssh, and a first run against a local
Ollama either works or says exactly which window or template to fix.

### M35 — subscription sign-in: a ChatGPT plan and a Claude plan

**Status.** Built (`docs/m35-subscriptions.md`; user guide
`docs/subscriptions.md`). PR to `main` open, not merged.
- **ChatGPT plan, native.** `ferrule login chatgpt` signs in with the
  Codex CLI's public OAuth client (device code, a loopback browser flow, or
  a pasted redirect), and the Responses driver calls the Codex backend from
  ferrule's own loop. The token is sealed, refreshed one process at a time,
  and revoked on logout. `/login chatgpt` works by device code in the
  owner's own chat. OpenAI tolerates this; it is not a contract, and the
  guide says so.
- **Claude plan, through the unmodified `claude` binary.** Each turn runs
  `claude -p` (never `--bare`) with ferrule's own config dir, resuming the
  chat's Claude Code session. Ferrule's memory, tasks and messaging reach
  claude over MCP, and claude's tool permissions are asked of ferrule's
  approvals. Ferrule never calls the Anthropic API with a plan token: a
  setup-token pasted as an API key is refused on every call. Sign-in is
  Claude Code's own, a pasted setup-token (stored sealed, and the guide
  says plainly that this is storing one), or an exported
  `CLAUDE_CODE_OAUTH_TOKEN`. Never over Telegram.
- Plan turns are ledgered at $0 with the notional API price beside them; a
  spent usage window falls back to the next model and tells the owner when
  it resets. Setup, status, doctor and the dashboard show each plan.

**Done means.** A user with only a ChatGPT or a Claude subscription gets
through setup and chats, in the terminal and in Telegram, with no API key,
and every Claude-plan model request comes from Claude Code itself.

### M36 — self-update and self-repair

**Status.** Built (`docs/m36-self-update.md`; user guide
`docs/updates.md`). PR to `main` open, not merged.
- **The ChatGPT client identity stays current.** The Codex client version
  is learned from npm (then GitHub, then a compiled-in one), cached a day,
  and refreshed at once when the backend refuses a model as "requires a
  newer version of Codex"; the turn is retried once.
- **`ferrule update`.** Signed GitHub releases (sha256 plus minisign
  against a compiled-in key; the release workflow signs). A service gets a
  privileged apply unit that installs when no turn runs, restarts the
  gateway, and rolls back and pins a release that doesn't come up healthy.
  The owner hears one line per update or rollback. Without a service, the
  owner is told a release is out.
- **`claude` stays current** through its own install's updater (native,
  Homebrew, WinGet, npm, pnpm), daily and when a turn fails because it's
  too old.
- **Self-repair.** Every failure is classified; every provider error tries
  the escalation and the fallbacks before the chat gets plain words and
  the raw error. A broken config runs on the last good copy. A 15-minute
  self-check tells the owner about new and cleared problems, and a repair
  log shows in doctor and the dashboard.

**Done means.** When OpenAI, Anthropic or Ferrule moves on, Max's bot keeps
answering with no SSH and no terminal; what it can't fix, it says in plain
words once. Existing v0.5.x installs run the install one-liner once.

### M37 — the control room

**Status.** Built (`docs/m37-control-room.md`; user guides
`docs/dashboard.md`, `docs/connections.md`). PR to `main` open, not merged.
- **Notices close** (for a day), each with its fix buttons; hidden ones
  are listed with Show again.
- **Models are picked, not typed.** The fallback chain is checked, and a
  provider key is tested before it's saved.
- **Connections work.**
  - Atlassian's three ways in, in order.
  - Google's app password, service account and own-OAuth paths.
  - A fixed callback relay deployed from the page.
  - A checklist, stuck flows cancelled and expired.
  - Plain errors with a switch to a key.
  - Expiring keys named in doctor.
  - `ferrule connections setup`.
- **Terminal parity:** a console of `ferrule` commands (no shell), chat
  with approvals, and the config with secrets hidden.
- **A UI for a phone:** IBM Plex, a bottom bar, a desktop sidebar, and a
  system theme. A browser check runs in CI on three OSes.

**Done means.** Max runs his agent from his phone: he closes what he's
read, fixes what's broken with a button, connects Jira and Google with a
key when OAuth won't, and does anything the terminal does. The exception
is a raw shell, which is his decision.

### M38 — named instances

**Status.** Built (`docs/m38-instances.md`; user guide
`docs/instances.md`). PR to `main` open, not merged.
- **Several agents, one machine.** `ferrule --instance work …` (or
  `FERRULE_INSTANCE`) gives an agent its own config, data, secrets, bot,
  service and dashboard. The default instance stays exactly as it was.
- **`ferrule instances list | new | remove`.** `new` runs the setup;
  `remove` keeps the files unless `--purge`, which asks first. Setup on an
  existing install offers "Another instance".
- **Collisions caught.** Doctor and setup fail on a shared bot (naming the
  other instance), dashboard port, workspace, SSH workspace or relay.
- **Updates stay safe.** Instances on one binary update together; a pin
  or `auto = false` in any of them holds the rest, and a rollback covers
  all.
- **Everything is per instance:** the service and update units, doctor,
  status, the dashboard's console, fix buttons and restart, the relay
  Worker's name, and the install one-liner's upgrade.

**Done means.** Max runs a second agent beside his own on the same server,
with its own bot and dashboard, in one command, and neither one's update,
restart or removal touches the other.

### M39 — more channels

**Status.** Built (`docs/m39-channels.md`; user guide
`docs/channels.md`). PR to `main` open, not merged.
- **Six new ways in:** WhatsApp, Matrix, email, Signal, Mattermost, and an
  HTTP API for scripts, n8n or Zapier, beside Telegram, Discord and Slack.
- **The same rules everywhere:** an allowlist, mention-only in shared
  chats, pairing, an owner per channel, approvals in the channel, and
  tokens the model never sees.
- **Files both ways** where the platform allows: what people send lands
  in the inbox, and `send_file` sends a workspace file back.
- **Each one is set up the same way:** a setup step, a dashboard card with
  a token form, Test and a guide, doctor lines, and a check that two
  instances don't share an account.
- **The HTTP API** gives each program its own key, answers as JSON or a
  stream, keeps what nobody waited for in an outbox, and can call a signed
  webhook; it stays on this machine unless a tunnel is turned on.

**Done means.** Max's agent answers him on WhatsApp or by mail, a team
reaches it from Matrix, Signal or Mattermost, and an n8n flow calls it
with a key, each set up from the phone with Test showing it works.

### M40 — build diet

**Status.** Built (`docs/m40-build-diet.md`). PR to `main` open, not
merged.
- **Smaller test builds:** a dev profile keeps line tables for our code
  and no debug info for dependencies; backtraces still name file:line.
- **One test binary per crate:** `tests/it/`, add a module, not a file.
- **From an empty dir:** 10.66 GB → 2.62 GB, 102 → 51 executables; a
  release adds ~2 GB to a shared target dir instead of ~9.4 GB.

**Done means.** A milestone run no longer fills the 72 GB disk, CI is
green on three OSes with the same test count, and a contributor knows
where a new test goes.

### M41 — daily use

**Status.** Built (`docs/m41-daily-use.md`; user docs `docs/channels.md`
and `docs/backup.md`). PR to `main` open, not merged.
- **`/new`** in every chat: a fresh conversation, the old one kept, memory
  untouched; a conversation that keeps failing the same way suggests it.
- **Voice messages** reach the agent as text: an OpenAI-compatible
  endpoint (OpenAI, Groq, a local server) or a local command such as
  whisper.cpp, any language.
- **"Typing…"** while ferrule works, on Telegram, Discord, Matrix and
  WhatsApp.
- **`ferrule backup` / `ferrule restore`:** one checked file with
  everything ferrule knows, and a restore that never deletes what was
  there.

**Done means.** Max escapes a broken conversation from his phone, talks to
ferrule by voice in Hebrew, sees it working, and can move an instance to a
new machine with two commands.

### M44 — managed mode

**Status.** Built (`docs/m44-managed-mode.md`; user docs `docs/docker.md`,
with additions in `docs/dashboard.md`, `docs/sandbox.md` and
`docs/channels.md`). PR to `main` open, not merged. Nothing in it has run
in a real container: there is no Docker daemon in the dev environment.
- **An image**: one bot per container, a non-root user, `/data` for
  everything, a `-browser` variant, a health check.
- **`[managed]`**: a policy file the panel writes and the bot cannot edit
  decides what the bot may do; no Claude plan, no self-update, no `ssh`.
- **Panel sign-in**: a short-lived token signed with a per-bot secret opens
  the dashboard under `/b/<id>/`.
- **Telegram from the page**, `/healthz` and `/busyz`, `ferrule health`, a
  bounded drain on SIGTERM.
- **Measured**: about 24 MiB idle and 26 MiB at a turn's peak, so about 460
  bots per 16 GiB.

**Done means.** A panel can start a bot with one `docker run`, sign a user
into its page, and update it by swapping the image tag, with the user
needing no shell.

### M42 — harness engineering, applied to ourselves

**Status.** Parts 1–4 built (`docs/m42-harness-engineering.md`, from a
pass over the learn-harness-engineering course and its Claude Code /
Codex / DeepSeek / Pi breakdowns); parts 5–7 designed there, not started.
- **The repo dogfoods:** a root `AGENTS.md` directory page and a
  `Makefile` whose `make check` is the green predicate.
- **Layered context baseline:** user-level (config dir) → workspace
  parents → workspace, most specific last in the prompt, the cap spent on
  the most specific first.
- **Layered verification:** `[agent] verify_command` takes a list run in
  order, and `ferrule run --verify CMD` overrides it per run — the goal
  loop's three parts from the command line.
- **Model-visible means logged:** compaction writes a `fold` record, a
  resume replays the compacted state, and an invariant test asserts every
  model-visible message is in the log.

**Done means.** An agent fresh to this repo runs `make check` and knows
the rules; a layered config dirs' rules reach every session; a run can't
finish until an ordered list of checks passes; and nothing the model sees
is missing from the transcript.

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
- **Anytime:** parallel read-only tool calls, streaming replies and a stable
  prompt prefix shipped as M27. Left over: streaming to channels that
  can't edit, formatting mid-stream, `sendMessageDraft`, and a 1-hour
  cache TTL.
- **The strategy backlog** (`docs/research-number-one-harness-strategy.md`
  §4): a `web_search` tool, keyword-triggered skills, Aider-style edit
  mechanics (SEARCH/REPLACE edits, a repo map, per-edit lint, atomic
  commits), local-model first-run polish, migration importers from
  OpenClaw/Hermes, channels in the order Discord → Slack → WhatsApp, a
  read-only dashboard, an SSH execution backend, an egress domain policy in
  the proxy, and OTel export from the ledger. (Importers, egress and OTel
  shipped as M33.)
