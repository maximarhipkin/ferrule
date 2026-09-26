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
  the proxy, and OTel export from the ledger.
