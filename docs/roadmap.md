# Ferrule roadmap

Where Ferrule is and where it's going. This is the plan as it stands; the
day-by-day record lives in `PLAN.md` (Current State and Session Log), and the
reasoning behind each milestone lives in the `docs/research-*.md` reports.

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

## Next, in order (approved 2026-09-24)

### M11 — a browser

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

**Open questions.** Screenshots come back as images, which the MCP client
currently drops (it keeps text only). Whether agent-browser's CA handling
works the same on macOS and Windows as on Linux.

### M12 — multi-agent

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
  - the Streamable HTTP transport has no server-initiated stream or resumption;
  - the Chrome launch check isn't run on macOS or Windows in CI.
- **Anytime:** parallel read-only tool calls, streaming replies, and a stable
  prompt prefix for caching.
