# Autonomy, Self-Extension, a Browser, and Speed: Research for Ferrule

**Research report, September 24, 2026**

Max's brief (msg 3066): make Ferrule smart and fast. That means an
architecture review, never getting stuck and always finding a way forward,
its own browser, and the ability to install skills and plugins for itself.
Code references are to `main` as of M8 (`f3cd7e0`).

## TL;DR

- **Ferrule's loop has no way to recover when something goes wrong.**
  - A provider error fails the turn outright. There is no retry and no
    backoff anywhere.
  - An MCP call gets exactly one respawn: "never a retry loop".
  - Hitting `max_iterations` (60) is a hard error, and Telegram receives
    the text `internal error: …`.
  - Nothing notices when the model repeats itself.
  - All of these are cheap to fix. Together they are the biggest part of
    "never gets stuck".
- **Stuck detection is a solved pattern.** OpenHands' `StuckDetector` checks
  five concrete loop signatures:
  1. 4+ identical action/observation pairs
  2. 3+ identical action/error pairs
  3. 3+ monologue turns
  4. 6+ alternating actions
  5. repeated context-window errors

  ([OpenHands docs](https://docs.openhands.dev/sdk/guides/agent-stuck-detector))

  It is about 100 lines of Rust over data Ferrule already has.
- **"Always finds a solution" needs a verifier, not a longer loop.**
  - Ferrule's `verify_command` is only a sentence in the system prompt
    today. The model is *asked* to run it, and nothing checks that it did.
  - Claude Code enforces the same idea from outside the model: a Stop hook
    that exits with code 2 blocks the agent from finishing
    ([hooks docs](https://code.claude.com/docs/en/hooks)).
  - Ferrule should run the check itself and feed any failure back into the
    loop, within a bound.
- **The browser: `vercel-labs/agent-browser` fits best.**
  - It is Rust and Apache-2.0, and drives Chrome over CDP.
  - It already ships a stdio MCP server (`agent-browser mcp`), so Ferrule
    can use it with **no new code, just config**.
  - It has `--proxy`, `--proxy-bypass` and `--allowed-domains` flags, which
    line up with Ferrule's credential proxy and egress policy.
  - The comparison with the other browser stacks is background knowledge,
    **not verified in this pass**.
- **Self-installing plugins is the dangerous part, and the risk is measured.**
  - MCPTox (arXiv:2508.14925) tested 45 real MCP servers, 353 tools and 20
    LLMs against poisoned tool descriptions.
  - The average attack success rate was **36.5%**, and the worst model
    reached **72.8%**.
  - Today, Ferrule's MCP servers run **outside the sandbox and outside the
    credential proxy, with ferrule's full environment**.
  - An agent that installs its own MCP servers therefore needs three things
    first: MCP servers sandboxed, owner approval per install, and a scan of
    the tool descriptions.
- **Skills are already half self-installable.** They are rescanned for every
  new agent, so a skill the agent writes into `.ferrule/skills` shows up in
  the next session. MCP servers are connected once at startup and never
  again. Adding one means a restart.
- **The speed wins are in the loop, not the model.**
  - Tool calls run one after another (`agent.rs:199`), even when the model
    asks for several independent reads in one turn.
  - `Provider::complete` doesn't stream, so a Telegram user sees nothing
    until the whole turn is done.
  - Both are well-understood fixes. The ledger (Phase 0, shipped) already
    records `latency_ms` per call, so every change can be measured.
- **Proposed order:**
  1. **M9: never stuck.** Retry and backoff, stuck detector, a graceful stop
     at the limit, and an enforced verify step.
  2. **M10: MCP servers and `web_fetch` under the sandbox and the proxy.**
  3. **M11: the browser**, through agent-browser MCP.
  4. **M12: self-extension**, meaning skills and MCP servers installed with
     approval and hot-loaded.

  The speed items can go into any milestone.

---

## 1. Architecture review: what Ferrule has today

| Area | Today | Where |
|---|---|---|
| Provider seam | One narrow trait, `complete(req) -> response`. Routing, retry and streaming can all sit behind it. | `ferrule-core/src/provider.rs:27` |
| Provider retries | None. A 600 s HTTP timeout, then the error goes straight up. | `ferrule-providers/src/openai_compat.rs:21` |
| MCP retries | One lazy respawn per call: "never a retry loop". | `ferrule-mcp/src/client.rs:76` |
| Loop limit | `max_iterations = 60`, then `CoreError::MaxIterations`. No summary and no partial answer. | `ferrule-core/src/agent.rs:32`, `:228` |
| Error to user | The gateway turns any agent error into a chat reply, `internal error: {e}`. It is the right hook, but the content is poor. | `ferrule-gateway/src/router.rs:130-136` |
| Tool execution | A sequential `for` over the turn's tool calls. | `ferrule-core/src/agent.rs:199` |
| Context | `maybe_compact`: keep the last N (default 6) and summarise the head. | `ferrule-core/src/agent.rs:257` |
| Verification | `verify_command` is added to the system prompt as an instruction. It is not enforced. | `ferrule-cli/src/main.rs:376-381` |
| Skills | Discovered each time an agent is built. New skills appear in new sessions. | `ferrule-cli/src/main.rs:384-394` |
| MCP servers | Connected once when the gateway or a task run starts, then never rescanned. | `ferrule-cli/src/main.rs:292`, `:304` |
| Sandbox coverage | Only the shell tool is sandboxed. `web_fetch` builds its own client in-process, and MCP servers are spawned with `.envs(&cfg.env)` on top of ferrule's environment. Both bypass the sandbox and the credential proxy. | `main.rs:340`, `ferrule-tools/src/web.rs:76`, `ferrule-mcp/src/client.rs:157`, PLAN.md "Known limits" |
| Observability | A ledger records one line per provider call, with tokens, `latency_ms`, outcome and cost. | `ferrule-core/src/ledger.rs:23` |

The foundations are sound. The trait is narrow, each session has its own
lane, the ledger exists, and the sandbox and proxy work. What's missing sits
*around* the loop: recovery, verification, concurrency and live extension.
None of it needs a redesign.

## 2. Never getting stuck

### 2.1 Transient failures: retry with backoff

Today one HTTP 429 or 503 from the provider ends the whole turn.

**Proposal:** a `RetryingProvider` wrapper, which is just another
`impl Provider`, so the core doesn't change.

- **What to retry:** timeouts, connection errors, 408, 429 and 5xx.
- **What not to retry:** any other 4xx. A bad key or a malformed request
  won't fix itself.
- **How:** exponential backoff with jitter, honour `Retry-After`, 3–4
  attempts, and a total cap of about 2 minutes.
- **Ledger:** record each retry as `error_kind = "retried"` so the cost
  shows up.

MCP keeps its single respawn. A tool call is not idempotent the way a
completion request is, so silently repeating it is wrong. The error should
go back to the model as a tool result instead (see §2.3).

This is standard practice and not taken from any one source. The numbers
are a starting point, not measured.

### 2.2 Loops: a stuck detector

OpenHands' `StuckDetector` flags five patterns
([docs](https://docs.openhands.dev/sdk/guides/agent-stuck-detector)):

| Pattern | Threshold |
|---|---|
| The same action with the same observation | 4+ times |
| The same action ending in an error | 3+ times |
| Agent monologue (agent messages with no user input between them) | 3+ in a row |
| Alternating between two action/observation pairs | 6+ steps |
| Repeated context-window errors | — |

Ferrule has almost everything it needs. `Agent::messages` holds each tool
call's name and arguments and each result's text, and the loop already
knows whether each call failed (`ToolCallFinished { ok }`). The detector
needs that flag kept next to the result instead of only emitted as an event.

**Proposal:** after each iteration, check the tail of the history.

- **First detection:** inject a user-role note. It names the repeated call
  and says to change approach, or to stop and ask.
- **Second detection:** end the turn with a status answer (§2.4) rather
  than burning the rest of the 60 iterations.

Most of the saving is in the second step. A stuck model today burns the
full budget and then fails anyway.

### 2.3 Tool errors that say what to do next

The SWE-agent paper (arXiv:2405.15793) introduced the term
*agent-computer interface*: the design of tools and their feedback is a
first-order factor in agent success. mini-swe-agent
(github.com/SWE-agent/mini-swe-agent) makes the same point from the other
side. A loop of about 100 lines scores above 74% on SWE-bench Verified. The scaffold can stay simple as long as the feedback is
good.

For Ferrule, every tool error should say what was wrong *and* what to try
instead. Some examples:

- "path is outside the workspace; the workspace is X"
- "command not found; available shells: …"
- "the sandbox refused writing to Y; writable roots are …"

Anthropic's
[Writing effective tools for agents](https://www.anthropic.com/engineering/writing-tools-for-agents)
is the reference to review the tool descriptions against.

### 2.4 At the limit: a status answer, not an error

When `max_iterations` is reached, or after the second stuck detection:

1. Make **one more provider call with no tools**.
2. Ask for a short status: what was done, what is blocking, and the next
   step or the question for the owner.
3. Return that as the answer, marked as incomplete.

`run_lane` already delivers errors to the chat. This changes what gets
delivered, from `internal error: max_iterations` to something the owner can
act on. Scheduled tasks already record a truthful run status (M3), and
"incomplete" becomes a third outcome next to ok and error.

### 2.5 An enforced verify step

`verify_command` is a sentence in the prompt today. The Claude Code pattern
(Stop hook, exit code 2 blocks the stop;
[docs](https://code.claude.com/docs/en/hooks)) moves it outside the model:

1. When the model gives a final answer and the turn changed files, Ferrule
   runs the command itself, inside the sandbox.
2. If the command fails, the tail of its output goes back in as a message,
   and the loop continues.
3. This is bounded, for example to 3 verify rounds. Claude Code has the
   same caveat: it overrides the block after repeated consecutive blocks,
   so the loop can't run forever.

This is "the build system is truth" (`docs/research-report.md`) made
mechanical.

### 2.6 Learning from failure: reflections in memory

Reflexion (arXiv:2303.11366) has the agent write a short verbal reflection
after a failed attempt and read it on the next try. There are no weight
updates. Ferrule already has long-term memory with recall.

**Proposal:** when a turn ends as error or incomplete, or a scheduled task
fails, store a one-paragraph reflection tagged with the task or session:

- what was tried
- why it failed
- what to try next

Recall already surfaces relevant memories at the start of a turn.

This is also the honest answer to "always finds a solution". Across
attempts the agent does not repeat the same dead end, even though a single
attempt can still fail.

### 2.7 Context

OpenHands' summarising condenser defaults to 80 events and always keeps the
first 4 (`llm_summarizing_condenser.py` in github.com/All-Hands-AI/OpenHands).
Ferrule keeps the system prompt and the last 6 messages, and folds
everything else into the summary, including the first user message. On long
tasks that message, the one that says what "done" means, should survive
compaction verbatim. That is a small change to `maybe_compact`. Anthropic's
[Effective context engineering for AI agents](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)
is the broader reference.

## 3. Its own browser

### 3.1 Recommended: `vercel-labs/agent-browser` over MCP

Verified from the project itself:

- a Rust CLI under Apache-2.0 that drives Chrome over CDP
- `agent-browser mcp` runs it as a **stdio MCP server**
- flags `--proxy`, `--proxy-bypass` and `--allowed-domains`

Ferrule already speaks stdio MCP, so adding the browser is a
`[[mcp.servers]]` entry:

```toml
[[mcp.servers]]
name = "browser"
command = "agent-browser"
args = ["mcp", "--allowed-domains", "example.com,*.example.org"]
```

There are three catches.

1. **It needs Chrome or Chromium on the host.** The wizard or doctor
   should check for it, and the install should offer to fetch it.
2. **It runs as an MCP server, so today it gets ferrule's full environment
   and unfiltered network** (§1). Pointing `--proxy` at ferrule's credential
   proxy makes the browser's traffic go through the host bindings and the
   egress policy. With a MITM proxy, the browser has to trust ferrule's CA.
3. **Two quirks from using agent-browser inside this container** (Devi's
   own use, not a published source):
   - Behind a MITM proxy it needs `--ignore-https-errors`, or the CA
     installed in Chrome's store.
   - A click by element reference can silently do nothing on some Vue or
     Vuetify controls. A JS `click()` through `eval` works.

   The tool descriptions Ferrule exposes should mention the fallback.

### 3.2 Alternatives (background knowledge, not verified in this pass)

| Option | Shape | Why not first |
|---|---|---|
| `chromiumoxide` (Rust crate) | In-process CDP | Ferrule would have to write its own tools: snapshot, click, type, wait. That is weeks of work agent-browser already did. |
| Playwright MCP (Microsoft) | Node MCP server | Needs a Node runtime next to a single-binary Rust tool |
| Chrome DevTools MCP (Google) | Node MCP server | Same, and aimed at debugging more than driving |
| Stagehand (Browserbase) | TypeScript SDK | Library, not a tool server. Its own LLM calls. |
| browser-use | Python library | Python runtime. Its own agent loop fights Ferrule's. |

These rows come from general knowledge of the projects. Check them before
quoting any of them as fact.

## 4. Installing skills and plugins for itself

### 4.1 What works today

- **Skills:** the agent can write a `SKILL.md` into `.ferrule/skills/<name>/`
  with its file tools. The next session discovers it. No restart is needed.
- **MCP servers:** they are connected at startup only. A server added to
  `config.toml` does nothing until the gateway restarts, and the agent can't
  restart its own gateway.

### 4.2 Hot loading

- **Tools changing inside a server.** MCP has
  `notifications/tools/list_changed` for servers that declare the
  `listChanged` capability
  ([spec 2025-06-18, tools](https://modelcontextprotocol.io/specification/2025-06-18/server/tools)).
  Ferrule's client should handle it by re-running `tools/list` and swapping
  the server's tools in the registry.
- **Adding a server.** An `mcp_add` tool, approval-gated (§4.4), would:
  1. write the `[[mcp.servers]]` entry with `toml_edit`, which M8 already
     depends on
  2. connect it live
  3. register its tools for new sessions

  The running session gets the tools on its next turn. The prompt cache
  breaks once when that happens, because the tool list is part of the
  prefix.

### 4.3 Where to find things

The official MCP registry, `registry.modelcontextprotocol.io`, is a
*meta-registry*: it lists servers and points at their packages (npm, PyPI,
OCI). Smithery is one of the downstream sub-registries. A `mcp_search` tool
over the official registry is the neutral starting point.

For skills there is no equivalent registry. Git URLs (a repo with a
`SKILL.md`) are the practical unit.

### 4.4 Safety: the reason this goes last

- **Tool poisoning.** Malicious instructions hidden in a tool's
  *description* steer the model, even when the tool is never called. Invariant
  Labs disclosed this in April 2025
  ([post](https://invariantlabs.ai/blog/mcp-security-notification-tool-poisoning-attacks))
  and published `mcp-scan` to check for it.
- **Measured.** MCPTox (arXiv:2508.14925) ran 45 real MCP servers, 353
  tools and 20 LLMs. The average attack success rate was **36.5%** and the
  highest was **72.8%** (o1-mini).

An agent that installs its own servers is exactly the path where this
lands. It needs these pieces before self-install is on:

1. **MCP servers under the sandbox and proxy (M10).** Spawn them through
   `Sandbox::command` with the scrubbed environment, and put their HTTP
   through the credential proxy. This fixes the gap in PLAN.md's known
   limits, and it is worth doing on its own.
2. **Owner approval for every install.** Show the server, the package and
   the version, plus its tool names and descriptions. On Telegram that is
   an inline-keyboard approve/deny. Pin the exact version.
3. **Scan descriptions before approval.** Flag hidden-instruction patterns:
   "ignore previous", instructions about other tools, `<IMPORTANT>` blocks,
   and requests to read files or keys. Re-scan when `list_changed`
   arrives, because a server can change its descriptions after it is
   approved ("rug pull").
4. **Skills get the same gate when they come from outside.** Skills the
   agent writes itself don't need one: they are text Ferrule already
   trusts as much as the model's own output.

### 4.5 Skills the agent writes for itself

Voyager (arXiv:2305.16291) is the reference for a growing skill library.
The agent writes a skill, **verifies that it works**, stores it, and
retrieves it by description later. Against earlier methods it got 3.3×
more unique items and reached tech-tree milestones up to 15.3× faster. The
point that carries over is *verified before stored*. For Ferrule that
means:

- a skill is saved when the task that produced it succeeded, including the
  verify step from §2.5
- its description is written for retrieval: *when* to use it, not *what*
  it is

## 5. Speed

| Change | Effect | Cost |
|---|---|---|
| Run independent tool calls in parallel when the model returns several at once | Several reads, fetches or searches finish in the time of the slowest | A `parallel_safe` flag per tool. Reads yes; shell and writes no. |
| Stream from the provider, and show "typing" / progressive edits on Telegram | Time to first visible output drops from the whole turn to about a second | `complete_stream` on the trait with a default that falls back to `complete` |
| Stable prompt prefix (see the routing report, §3.4) | Provider-side cache hits, which lower both cost and latency | Order the prompt: system → tools → skills catalogue → history. Keep volatile parts out of the head. |
| Cheap model for compaction summaries (routing report, Phase 1) | Faster and cheaper compaction | Already on the roadmap |
| Retry and backoff (§2.1) | A 429 costs seconds instead of a failed turn | M9 |

The ledger (`latency_ms`, tokens and outcome for each call) is how to check
each change against real traffic, rather than trusting any of the estimates
here.

## 6. Windows sandbox (research item, not verified in this pass)

Native Windows has no sandbox in Ferrule today. The README says so, and
WSL2 is the recommendation. The Windows mechanisms that could fill the gap,
from general knowledge and **to be checked against Microsoft's docs before
any design**:

- **AppContainer.** A capability-based container. File access is denied
  unless an object's ACL grants the container's SID, so the workspace would
  need an ACL entry. Network access is controlled by capabilities such as
  `internetClient`.
- **Restricted tokens with a low integrity level.** The process can't write
  to medium-integrity objects. It is simpler to set up than AppContainer,
  but reads stay open.
- **Job Objects.** Kill the whole process tree on close, and limit
  processes and memory. They complement either of the above.
- **Network filtering** beyond AppContainer's on/off capabilities would
  need the Windows Filtering Platform.

OpenAI's Codex CLI is reported to have a Windows sandbox built on restricted
tokens. Its source is the first thing to read, and that has not been done
here.

## 7. Proposed milestones

| | Contents | Why this order |
|---|---|---|
| **M9 — never stuck** | `RetryingProvider` (§2.1), stuck detector (§2.2), status answer at the limit (§2.4), enforced verify (§2.5), reflections (§2.6), pin the first message in compaction (§2.7) | No new dependencies and no security surface. It changes day-to-day behaviour the most. |
| **M10 — MCP and `web_fetch` confined** | MCP servers spawned through the sandbox with the scrubbed environment; `web_fetch` and MCP HTTP through the credential proxy | Closes a known gap, and M11 and M12 both depend on it |
| **M11 — browser** | agent-browser as an MCP server, Chrome check in `doctor` and setup, proxy and allowed domains wired in, browser tool descriptions with the fallbacks | Config-level once M10 exists |
| **M12 — self-extension** | `list_changed` handling, `mcp_search` / `mcp_add` with approval and description scan, skill install from git with approval, Voyager-style verified self-written skills | Last, because it's the one with a measured 36.5% attack rate |
| Anytime | Parallel read-only tools, streaming, stable prefix | Independent of the above |

## 8. What's hype

- **"Always finds a solution."** No loop can promise that. What can be
  built is: it doesn't fail on transient errors, it notices when it's
  looping, it stops with a useful status instead of an error, and it
  remembers what didn't work. Claims should be sized to that.
- **Full autonomy in installing plugins.** The MCPTox numbers argue against
  installing without the owner approving. An install flow that asks once
  and takes one tap is the realistic version of "installs by itself".
- **A bigger agent framework.** mini-swe-agent's result says a small loop
  with good tool feedback is competitive. The work above adds guard rails
  around Ferrule's loop, not layers on top of it.

## 9. Decisions for Max

1. **Approval model for self-install.** Every install asks (recommended), or
   an allow-list of trusted sources installs without asking?
2. **Chrome on the host.** Should setup offer to download Chromium for the
   browser (a download of well over 100 MB, rough figure), or only detect
   an existing one?
3. **Milestone order.** M9 → M10 → M11 → M12 as above, or the browser
   earlier with a warning that it runs unconfined until M10?
4. **Windows sandbox priority.** Research it now (§6), or leave it until
   there are real Windows users?

**Max's answers (msg 3074, September 24, 2026):**
1. Self-install: an **allow-list of approved sources**. Installs from those
   go ahead without asking. Anything else still needs the owner.
2. Chrome: **detect an existing install only**. Setup doesn't download a
   browser.
3. Order: **M9 → M10 → M11 → M12** as proposed.
4. Windows sandbox: **research it now**.
