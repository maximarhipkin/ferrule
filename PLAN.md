# PLAN.md

This file is **shared and appendable**. Multiple sessions/models work on this
repo concurrently from different machines — don't trust a stale copy of this
file over `git log`/the actual code. Keep the "Current State" section current
**in place** (edit it, don't append duplicates of it). Add a new dated entry
under "Session Log" for every session's work; never delete prior entries
(that's what `archive/`-style trimming is for, and this repo doesn't have
that convention yet — ask before introducing one).

## Current State

Rewritten in place on 2026-10-21 (M55.6) from `git log`, the code and
`docs/roadmap.md`; the version it replaced stopped at M49 and is in git
history. Keep this section true **in place**. Everything below it is
append-only.

- **What it is.** Ferrule is a self-hosted agent runtime for one owner,
  in Rust. The repo is `maximarhipkin/ferrule` (public), the binary is
  `ferrule`, the config is `ferrule.toml` or
  `~/.config/ferrule/config.toml`, keys live in
  `<data>/private/secrets.env`, and a workspace's state is in
  `.ferrule/`. `ferrule run` and `ferrule chat` drive the agent loop from
  a terminal. `ferrule gateway` is the daemon: chat channels, the
  scheduler and the dashboard. Install is one line (`install.sh`,
  `install.ps1`) followed by `ferrule setup`; `ferrule doctor` checks the
  result. The user-facing overview is `README.md`; every milestone's
  design and status is in `docs/roadmap.md`.
- **Releases and branches** (checked 2026-10-21):
  - The latest release is **v0.13.1** (tagged 2026-10-20). It has
    everything through M49 plus the 2026-10-19 review-pass fixes. Every
    crate is at 0.13.1 except `ferrule-plugin-sdk`, which keeps 0.1.0.
  - **`main` is `2ffa669`.** It is v0.13.1 plus M50 (PR #43), three
    merges from 2026-10-20: #44 ("Allow for this run", a per-run grant
    for one gated shell command), #45 (`send_file`, the connection tools
    and MCP tools bridged into the Claude-plan engine) and #46 (Telegram
    follow-ups merged into one turn, a quieter busy notice, locations,
    venues and contacts, no link previews), and **PR #48 (Kimi's M51–M53,
    merged 2026-10-21)**: media generation (`generate_image`,
    `generate_video` and its poller, the new `ferrule-media` crate), the
    dashboard Safe (rotate a kept key, move a tool key's sites) and
    contrastive probing in the learning loop. None of this is released.
    CI on `dadf357` was red on two flaky tests (fixed by M55.1 on
    `autopilot`).
  - **`autopilot` (PR #47, open)** stacks the gap analysis's work on
    `dadf357`, one commit per item, each with a 2026-10-21 Session Log
    entry: M55.2 (the four stranded branch stacks: the core-loop and
    Responses-reasoning stack, four host-side sandbox escapes, the
    update/restore defects, a test race), M55.3, M55.4, M55.5, M55.1,
    this rewrite, and the items after it. `main` (`2ffa669`, PR #48) was
    merged into it on 2026-10-21, after M56.6.
  - Since then (2026-10-21): PR #47 merged to `main` as `2c74bdb`, then
    **M54** (protected autonomous mode, `docs/m54-autonomous.md`) landed
    on `main` as `6efe413`, then PR #50 (the owner's Telegram groups get
    owner mode) as `f70984e`. `autopilot` continues as **PR #49**: M58.1–
    M58.5, M53.2 onward and the M51/M52 follow-ups, with `main` merged in
    after M53.2 (`6efe413`) and after M51.4 (`f70984e`). PR #49 merged
    to `main` on 2026-10-21, then PR #51 (the security and approvals
    group); `autopilot` continues as **PR #52** (open), M60 and M61
    among its commits.
- **Checks.** `make check` (fmt, clippy, the workspace tests) is the
  green predicate (`AGENTS.md`). The last full run on `autopilot` before
  this rewrite: 1869 passed, 0 failed, 28 ignored across 71 test
  binaries (macOS, 2026-10-21). CI runs on Linux, macOS and Windows;
  `release.yml` builds and signs five targets on a `v*` tag. Rust stable
  through rustup, edition 2021.
- **The 25 crates** (each one's `description` in its `Cargo.toml` says
  the same):

  | Crate | What it does |
  |---|---|
  | `ferrule-core` | The agent loop, the provider and tool traits, harness profiles, compaction and transcripts |
  | `ferrule-providers` | Model drivers: OpenAI-compatible chat, Anthropic Messages, OpenAI Responses (and its ChatGPT-plan Codex flavour) |
  | `ferrule-tools` | Workspace tools: files, `edit_file`, the shell, `web_fetch`, `web_search`, the diary; egress through the proxy |
  | `ferrule-memory` | SQLite memory: FTS5 BM25 with decay, supersede and forget, merged with vector recall |
  | `ferrule-embed` | Embedders for recall: a local model2vec model (opt-in download) or any `/v1/embeddings` endpoint |
  | `ferrule-media` | Image and video generation backends (M51): one `MediaBackend` trait, OpenRouter through the credential proxy, a fake for tests, the SQLite table async video jobs live in |
  | `ferrule-gateway` | The daemon: the `Channel` trait and adapters (Telegram, Discord, Slack, WhatsApp, Matrix, email, Signal, Mattermost, an HTTP API, local), the router with one lane per chat, the scheduler, transcription |
  | `ferrule-mcp` | The MCP client (stdio, sandboxed; Streamable HTTP), and the browser through agent-browser |
  | `ferrule-skills` | Agent Skills: discovery, the catalog, activation and keyword triggers |
  | `ferrule-sandbox` | The OS sandbox: Landlock and seccomp on Linux, Seatbelt on macOS, a restricted token in a job on Windows |
  | `ferrule-proxy` | The credential gateway: placeholders, the TLS-intercepting proxy that swaps in bound keys, scrubbing, the egress policy |
  | `ferrule-agents` | Sub-agents: spawn, wait, resume and close; the board, the task list, worktrees, limits |
  | `ferrule-extensions` | Self-extension: allow-listed MCP servers, skills and plugins installed mid-session, pinned, scanned, hot-loaded |
  | `ferrule-eval` | `ferrule eval`: suites, fixtures, graders, the A/B and its reports |
  | `ferrule-hooks` | Lifecycle hooks: command hooks, workspace trust, the audit log |
  | `ferrule-learn` | The learning loop: the gated playbook, memory consolidation, the offline pass |
  | `ferrule-trust` | Trust and cost: caps from the ledger, the kill switch, approval gates, plan mode, the audit log |
  | `ferrule-connections` | Connections: OAuth (PKCE, DCR), the relay, API keys through the encrypted form, sealed tokens, the catalog |
  | `ferrule-codemap` | The tree-sitter repo map and `code_search` |
  | `ferrule-otel` | OpenTelemetry traces from the ledger seam (OTLP/HTTP JSON) |
  | `ferrule-plugins` | WASM tool plugins in wasmi: the ABI, capability-checked host calls, fuel and memory limits |
  | `ferrule-plugin-sdk` | The Rust SDK for writing a plugin |
  | `ferrule-ssh` | SSH workspaces over the system OpenSSH client |
  | `ferrule-plans` | Subscription plans: ChatGPT sign-in and its sealed tokens, usage windows, the Claude Code engine |
  | `ferrule-cli` | The `ferrule` binary and all the wiring: setup, doctor, the dashboard, config, service, update, backup, instances, self-service |

- **Shipped and merged to `main`** (one line each; scope, open edges and
  "not verified live" lists are in each milestone's doc and Session Log
  entry):
  - **M1–M10, Phase 0:** the gateway, Telegram, the scheduler, MCP over
    stdio, the per-call ledger, Agent Skills, the OS sandbox, the
    credential gateway, one-line install and setup, never stuck, MCP and
    `web_fetch` under the sandbox and proxy, the system service.
  - **M11–M19c:** the browser; sub-agents (M12 parts 1–5; part 6, named
    long-lived agents, is deferred); self-extension; `ferrule eval`;
    memory updates and reversible compaction; the learning loop; `mcp
    add` with hot-add; lifecycle hooks; trust and cost; never silently
    deaf; the live-bot fixes.
  - **M20–M30:** connections and the relay; several models with pins
    and fallback; the dashboard; native drivers; the dashboard's
    leftovers; routing phase 1 (phases 2–3 were dropped); isolation (the
    native Windows sandbox, sandboxed reads); speed and streaming;
    `web_search` and keyword skills; edit mechanics and the repo map;
    vector recall.
  - **M31–M41:** Discord and Slack; WASM plugins; ops (egress policy,
    the Unix-socket allowlist, OTel, OpenClaw and Hermes importers); SSH
    workspaces and local models; ChatGPT and Claude plan sign-in;
    self-update and self-repair; the control room; named instances; six
    more channels (M39: WhatsApp over the Cloud API, Matrix, email,
    Signal, Mattermost, an HTTP API); the build diet; daily use (`/new`,
    voice, typing, backup and restore).
  - **M42–M50:** harness engineering (all seven parts, among them
    `AGENTS.md`, `make check`, goal loops, agent graphs, pluggable
    compaction); channels drive loops and graphs; managed mode; the
    dashboard redesign; self-service from chat and dashboard; visual
    polish; connect everything from the dashboard (M50, merged after
    v0.13.1). The numbers M45 and M46 were never used.
  - After M50, on `main` only: PRs #44, #45 and #46 (above).
- **In flight** (2026-10-21):
  - **M54, protected autonomous mode** is merged to `main` (`6efe413`).
    Its open edges (a live Linux run, macOS owner mode, per-session
    helpers for guests) are in its Session Log entry.
  - **`autopilot` / PR #49 (merged 2026-10-21):** M58.1–M58.5
    (simplify, remove or make opt-in), the M53 follow-ups (M53.2,
    M53.3, M53.4, M53.5, M53.7), and the M51/M52 follow-ups (M51.4,
    M51.3, M52.2, M52.4, M52.6; M52.3 is M54's `secret_request`), one
    commit each.
  - **`autopilot` / PR #52 (open):** among its commits M60 and **M61
    screen viewing (screenshots and recordings of the owner's own
    screen, view only)**, `docs/m61-screen.md`; then **M59** (personal
    WhatsApp through WAHA, iMessage through BlueBubbles,
    `docs/m59-channels.md`) and **M54.4** (`[egress] enforce`: commands
    bound to the proxy, opt-in; every destination in the audit log,
    `ferrule egress log`).
  - **Kimi's M51–M53** are merged to `main` (PR #48, above; designs in
    `docs/research-harness2-media.md`, `docs/m51-media.md`,
    `docs/m52-dashboard-safe.md`, `docs/m53-contrastive-probing.md`).
    Their named follow-ups are built on `autopilot` (above) except a
    per-run approval card for media spend and a Cloudflare Access JWT
    check at the dashboard's origin (M52.6, documented).
  - **M55–M59, from `reports/Ferrule agent gap analysis.md`** (2026-10-20,
    in the main checkout and git-excluded, so `docs/roadmap.md`'s "Other
    open tracks" lists its items): M55 make main green and land the
    written fixes; M56 the running turn hears the owner and remembers
    what matters; M57 the coding leaders' context and loop hygiene; M58
    simplify, remove or make opt-in; M59 channels not worth chasing yet
    (personal WhatsApp, iMessage). The report also amends M52 and M54
    with numbered items (M52.1–M52.6, M54.1–M54.7); they keep their
    parent's number.
- **Milestone numbers are claimed:** M51–M53 Kimi, M54 the autonomy
  session, M55–M59 the gap analysis, M60 images in tool results
  (2026-10-21, `autopilot`), M61 screen viewing (screenshots and
  recordings of the owner's own screen, view only; 2026-10-21,
  `autopilot`). **M59 is claimed and built** (2026-10-21, `autopilot`
  worktree, PR #52): personal WhatsApp and iMessage through bridges the
  owner runs. So is M54's numbered item **M54.4** (binding egress through
  the proxy, logged). The next free number is **M62**.
  The first claim wins, and a later collision shifts the unclaimed
  numbers, never a claimed one.
- **Sync protocol for concurrent sessions** (from the gap analysis's
  "Staying synchronized" section and `AGENTS.md`):
  - **Claim before building.** When a session starts, it appends a dated
    Session Log line naming its milestone number and its worktree, not
    only when it ends.
  - **One owner per surface.** M54 owns `crates/ferrule-sandbox`,
    `ferrule-trust`'s `guard.rs` and `classify.rs`,
    `ferrule-cli/src/secrets.rs`, the config guard lists and
    `self_service/`'s op additions (`secret_request` among them). M52
    owns the dashboard's keys and Safe pages and the config editor UI.
    M56.1 owns the gateway's `router.rs` and its inbox. M51 owns its
    media module and the Telegram send branches. Anyone who must touch
    another's surface makes the smallest change and says so in their
    entry.
  - **Push early.** Push the branch at its first commit; a worktree
    branch with no upstream is invisible to the other sessions and to
    CI.
  - **PLAN.md is append-only** except this section, which is edited in
    place.
  - **Merge on green.** A change lands on `main` only when `make check`
    and the three CI jobs pass. Requiring that in GitHub's branch
    protection (M55.7) is a repository setting only the owner can turn
    on.
- **Branding.** `docs/branding/logo.png` and `docs/branding/hero.png`,
  drawn with Pillow (a metal band around a rod bundle; steel and copper
  on near-black). Keep that motif if they're ever redrawn.

## Session Log

### 2026-09-23 — Rename to Ferrule, NanoClaw/OpenClaw research, full architecture read

**Scope of this session:** deep-dive learning pass + rename + branding, per
an explicit six-item ask. No large Rust code changes were made — deliberately
(see "What was implemented vs. proposed" below).

**Rename.** Chose **Ferrule** as the new project name: a ferrule is the
metal band/cap that reinforces and binds the end of a rope, tool handle, or
fiber-optic connector — a fitting metaphor for a runtime whose job is
binding together heterogeneous model backends, tool implementations, and
memory into one durable interface. Rejected several other strong candidates
after finding real collisions via web search: `loom` (tokio-rs concurrency
crate), `rig` (0xPlaygrounds/rig, an existing Rust LLM framework), `helm`
(Kubernetes Helm), `yoke` (unicode-org/ICU4X crate), `bridle`/`agent-bridle`
(both an existing AI-coding-assistant harness crate and an agent-sandboxing
crate on docs.rs — a direct niche collision), `harness` (Harness.io, a CI/CD
company). Horse-tack/harness metaphors are a saturated naming trend in the
current "AI agent harness" space right now — worth remembering if renaming
again. No collision found for "Ferrule" in the Rust or AI-agent ecosystem.

Executed: renamed all 5 crate directories via `git mv` (history preserved),
updated the workspace `Cargo.toml` members list, the `[[bin]] name` in
`ferrule-cli/Cargo.toml`, every inter-crate path dependency, every
`use ferrule_*::` import, and did a repo-wide text substitution
(`agentrust`/`AgentRust` → `ferrule`/`Ferrule`) across `README.md`,
`docs/research-report.md`, and `tests_e2e/mock_llm.py`. Renamed the GitHub
repo itself via `PATCH /repos/maximarhipkin/agentrust` (GitHub REST API) —
confirmed success from the API response (`full_name` came back as
`maximarhipkin/ferrule`) — and updated the local `origin` remote to match.
Verification was manual/textual only (see toolchain constraint above): a
repo-wide case-insensitive grep confirms zero remaining `agentrust`
references, and every crate's `[package] name`, path dependency, and `use`
statement was cross-checked by hand against the new names.

**NanoClaw, described from first-hand knowledge** (I run on it, no web
research needed for this one): a proprietary agent platform providing —
multi-channel messaging integration (Telegram/WhatsApp groups etc.),
cron-based scheduled agent tasks, a filesystem-based skills system
(`/home/node/.claude/skills/`), persistent file-based cross-conversation
memory, an MCP tool gateway plus a *separate* HTTPS credential-injection
gateway for third-party API calls, background subagents, and admin-approval
gates for risky config/package changes. This is the actual target shape for
"replace NanoClaw" — Ferrule today covers essentially none of the
multi-channel/scheduling/gateway/approval-gate pieces yet.

**OpenClaw, verified via web search** (explicitly not assumed knowledge):
real project, `openclaw.ai` / `docs.openclaw.ai` / GitHub `openclaw/openclaw`,
created by Peter Steinberger (PSPDFKit founder) — a self-hosted, multi-channel
(Discord/Slack/Telegram/WhatsApp/etc.) AI-agent gateway. Its architecture
centers on a "Gateway as single source of truth" plus context files
(`SOUL.md`/`AGENTS.md`/`TOOLS.md`) loaded per-agent. Corroborated across 9
independent sources (official docs/site, DigitalOcean, freeCodeCamp,
Medium, GitHub org, TechRadar, Milvus blog, generect). Also verified
**ZeroClaw** in passing (a Rust "OpenClaw alternative," official repo
`zeroclaw-labs/zeroclaw`, small-footprint/fast-cold-start positioning) —
real project, but most secondary sources describing it are SEO/blogspam-tier
and specific stats (star counts, LOC counts) should be treated as
unverified/likely-exaggerated even though its existence and rough
architecture are corroborated.

**2025-2026 pain-point research** (judicious, not exhaustive — a handful of
citable sources): sandbox-escape vulnerabilities at the
"agent-authored-artifact consumed elsewhere" boundary, affecting
Cursor/Codex/Gemini CLI (Pillar Security, July 2026); sandboxing maturity is
fragmented industry-wide — Claude Code's 3-layer/5-permission-mode model is
cited as the most developed, vs. e.g. Copilot's agent mode running with full
IDE-process permissions; MCP tool-call chains are a recurring observability
blind spot, causing silent failures with no root-cause trail; ~5%
wrong-tool-selection rates compound into cascading errors in longer agent
runs; production multi-agent platforms are expensive to build ($150K–$1.5M+)
and to run ($3,200–$13,000/month); OpenTelemetry (OTel) is emerging through
2026 as the standard shape for agent traces/spans/token-usage/tool-call logs.

**Architecture read** (every `.rs` file in all 5 crates read directly, not
inferred from `Cargo.toml`):

- `ferrule-core` — the ReAct loop (`Agent::run`), typed lifecycle events over
  an mpsc channel, per-model `HarnessProfile` (context window, output
  reserve, compaction threshold ~70–75%, retained-reasoning flag,
  system-directive text; profiles exist for kimi/openai/anthropic-compatible/
  generic), structured compaction (`dedupe_tool_results` for free
  deterministic savings, then an LLM-driven checklist-template summary,
  keeping a trailing verbatim window), JSONL transcripts, a 16KB-capped
  context-baseline loader (`AGENTS.md`/`CLAUDE.md`/`GEMINI.md`/`ferrule.md`).
  **Maturity: solid, well-tested** (unit tests cover the loop, unknown-tool
  handling, reasoning-stripping).
- `ferrule-providers` — one driver, `OpenAiCompatProvider`, targeting the
  OpenAI chat-completions wire format; handles `reasoning_content` for
  interleaved-thinking models and both `prompt_tokens_details.cached_tokens`
  and `prompt_cache_hit_tokens` cache-accounting shapes. Covers
  Kimi/OpenAI/DeepSeek/OpenRouter/Groq/Ollama/llama.cpp/vLLM by virtue of
  the shared wire format. **No native Claude Messages API or Codex
  Responses API driver yet** — both are roadmap items in
  `docs/research-report.md`.
- `ferrule-tools` — fs read/write/list with lexical path-escape rejection
  (no symlink resolution — a real gap, see below), a shell tool gated by an
  11-pattern substring deny-list (not an allowlist, not an OS sandbox), a
  naive web-fetch/HTML-stripper, and diary/todo tools writing to
  `.ferrule/`. **Maturity: workable for a trusted single-user CLI, not
  safe as-is for untrusted multi-tenant or channel-facing use.**
- `ferrule-memory` — single-file SQLite, FTS5 BM25 + 7-day-half-life time
  decay, token-budgeted `assemble_context`. **Maturity: solid for what it
  does.** No vector/embedding recall, no memory-hygiene/pruning sweep (the
  kind of thing NanoClaw's own `memory-hygiene-*` task does).
- `ferrule-cli` — config loading (`ferrule.toml`), `run`/`chat`/`memory`/
  `config init` subcommands, builds the system prompt from workspace path +
  tool instructions + harness directive + context baseline + a
  memory-recall budget. **This is the entire "runtime" today** — there is
  no long-running daemon process, no session persistence across restarts
  beyond the JSONL transcript files, no scheduling.

**Top gaps vs. the NanoClaw/OpenClaw-replacement goal, ranked by size:**

1. **No channels/messaging layer at all.** This is the single biggest gap.
   NanoClaw and OpenClaw are both fundamentally multi-channel messaging
   gateways (Telegram/WhatsApp/Discord/Slack); Ferrule today is a local CLI
   with no concept of an inbound message, a channel, or a persistent
   listening process. Every other gap is smaller than this one.
2. **No task scheduler / daemon.** No cron-equivalent, no long-running
   process at all — `ferrule run` starts and exits. NanoClaw's scheduled
   background tasks (like the one that ran this very session) have no
   analog.
3. **No MCP client.** Roadmap-only (`docs/research-report.md` proposes
   `ferrule-mcp` via `rmcp`). Given MCP tool-chains are called out
   industry-wide as an observability blind spot, this should probably be
   built with first-class tracing from day one rather than bolted on later.
4. **Weak permission/sandboxing model.** A shell substring deny-list is
   trivially bypassable in spirit (it's a blocklist, not a real sandbox) and
   the fs-tool path guard is lexical-only (no symlink-escape protection).
   NanoClaw's admin-approval-gate pattern for risky changes, and the
   industry direction toward OS-level sandbox primitives
   (Landlock/Bubblewrap/Seatbelt — already on the roadmap doc as an idea),
   both point the same direction: this needs real work before Ferrule could
   safely run untrusted or remote-triggered tasks.
   *(2026-09-24, M6: the shell tool now runs in an OS sandbox and the fs
   path guard resolves symlinks. See that entry for what is still open.)*
5. **No credential-injection gateway.** NanoClaw's OneCLI-style pattern
   (transparent HTTPS proxy injecting per-service credentials) has no
   equivalent — any third-party API integration in Ferrule today would need
   its own bespoke credential handling.
6. **No skills/plugin system, no multi-agent orchestration, no
   self-improvement loop, no persisted cost/observability ledger.** All
   smaller than the above but all present in the target platforms and
   absent here; OTel-shaped tracing would be the natural fit given where the
   industry is heading.

**One new architectural idea (proposal only, not implemented):** rather than
bolting a channels layer directly onto `ferrule-cli`, introduce a
`ferrule-gateway` crate as a thin, separate long-running process that owns
*all* I/O boundaries (channel adapters, the scheduler, and — eventually — the
credential-injection proxy), communicating with one-or-more `ferrule-core`
`Agent` instances over an in-process channel or a local Unix socket. This
mirrors OpenClaw's own "Gateway as single source of truth" design (verified
above) and keeps `ferrule-core`/`ferrule-cli` usable standalone as a library
and a plain CLI, respectively — you don't have to run the gateway daemon to
use Ferrule as a scriptable one-shot agent. Tradeoff: it's a real new
process boundary and IPC surface (more moving parts, another thing to keep
alive and observe) versus just growing `ferrule-cli` in place; given the
gap-1 finding above (messaging is the single biggest missing piece and will
need its own lifecycle independent of any one agent run), the separate
long-running process seems worth the extra complexity, but this is a
one-session assessment, not a committed design — a future session should
sanity-check it against actual multi-channel requirements before building.

**What was actually implemented vs. only proposed:** implemented — the
rename (repo, crates, GitHub repo, remote, README) and the branding assets.
Everything under "architecture read" and "top gaps" above is analysis/
proposal only; no Rust code was written for the gateway idea or any other
gap, per instruction to only write code for something small and low-risk I
was confident about, and nothing in this session's scope met that bar — the
gaps above are all either too large (channels, scheduler, MCP, sandboxing)
or require product decisions (e.g. what a `ferrule-gateway` wire protocol
should look like) that shouldn't be guessed at blind.

**For the next session:** start with `git pull` (this repo moves between
sessions — three commits landed between when this session started and when
it first pulled). Cross-check whether `cargo` is available in your sandbox;
if so, please run `cargo check --workspace` for real and update the
toolchain-constraint note above — this session could only verify the rename
textually. If picking up the gateway idea, start by reading
`docs/research-report.md` section 5 (`"Recommended architecture"`) — it
already sketches a similar crate layout independently.

### 2026-09-23 (late) — First real compile + test run; diary flush bug fixed

Session: Devi (NanoClaw), Opus 5.5.

- Got a working toolchain (see Current State → Toolchain). The rename to
  Ferrule compiles: `cargo check --workspace --all-targets` clean with no
  warnings surfaced.
- `cargo test --workspace`: 24 tests. Two failures investigated:
  1. `ferrule-providers` `sends_tools_and_parses_tool_call` — environmental
     (HTTP proxy intercepting loopback). Passes with `NO_PROXY` set. Worth
     considering: build the test client with `.no_proxy()` so the suite is
     hermetic regardless of the host's proxy env.
  2. `ferrule-tools` `diary_appends` — **real, pre-existing bug** (present
     before the rename too), flaky ~1 in 3 runs. `log_diary` wrote via
     `tokio::fs::File::write_all` and returned without `flush()`; tokio's File
     hands the write to a blocking task, so the entry may not have landed when
     the tool returns or when the next append opens. In production this can
     drop or reorder diary entries. Fixed by awaiting `f.flush()`. Verified:
     0/30 failures after the fix, full suite green.
  - `transcript.rs` appends with `std::fs` (synchronous) — not affected.

**Next:** the gap list in Current State is unchanged (channels, scheduler/
daemon, MCP client, sandboxing, credential gateway). Now that the tree
compiles here, the proposed `ferrule-gateway` crate can be built and tested
for real in this sandbox.

### 2026-09-23/24 — ferrule-gateway M1-M2 (Devi, Opus 5.5)

Built the first slice of `ferrule-gateway`, the long-running daemon crate
proposed in `docs/research-report.md` section 5 (Phase 3), then continued in
the same session into M2. **M1** is the channel-agnostic daemon skeleton
(adapter trait, message model, session routing/resume). **M2** (below, after
the M1 writeup) adds the first two real `Channel` implementations and wires
the whole thing into `ferrule-cli` as a `gateway` subcommand. M3 (scheduler)
and M4 (MCP client) are still open — see "What remains" at the end.

**What was built** (`crates/ferrule-gateway/src/`):
- `message.rs` — `InboundMessage`/`OutboundMessage`, normalized across every
  future channel: `channel`, `chat_id`, `sender`, `message_id`, `text`,
  `attachments: Vec<Attachment>`, `reply_to`, `ts`. Plain serde structs, no
  channel-specific fields leak in — a Telegram update and a stdin line both
  become the same shape before the router ever sees them.
- `channel.rs` — `#[async_trait] trait Channel`: `name()`,
  `capabilities() -> ChannelCapabilities` (reactions/edits/attachments flags,
  default all-false), `run(tx) -> Result<(), GatewayError>` (the adapter's
  own inbound loop — polling, listening, reading stdin, whatever — pushing
  onto a shared `mpsc::Sender<InboundMessage>`), `send(msg)`, and optional
  `react()`/`edit()` that default to `Err(Unsupported)` so a channel without
  reaction support doesn't need a no-op override.
- `session.rs` — `session_id(channel, chat_id)` — deterministic, filesystem-
  and-Transcript-safe id (`sanitize()` maps anything non-alphanumeric/`-`/`_`
  to `_`), used both as the lane key and the JSONL transcript filename.
- `router.rs` — `Router`: one FIFO lane (a spawned tokio task fed by an
  `mpsc::Receiver<InboundMessage>`) per session id. `dispatch()` looks up or
  lazily spawns the lane and enqueues; different sessions run fully
  concurrently, same session is strictly sequential because it's one task
  draining one queue. `AgentFactory` is a
  `Fn(&str, Transcript) -> Result<Agent, GatewayError>` closure supplied by
  the embedding binary (`ferrule-cli` in M2) — the gateway crate has no
  opinion on which provider/tools/profile a session's agent uses.
- `gateway.rs` — `Gateway`: owns a set of `Channel`s and one `Router`, fans
  every adapter's inbound stream into a single funnel channel, dispatches
  each message, and returns once every adapter's `run()` has finished (the
  funnel closes when every cloned sender is dropped).
- `error.rs` — `GatewayError` (thiserror): wraps `ferrule_core::CoreError`,
  `io`, `serde_json`, plus gateway-specific `UnknownChannel`, `SessionClosed`,
  `Unsupported(&'static str)`, `Channel(String)`. `Http`/`Sqlite` variants
  are deliberately not added yet — no `reqwest`/`rusqlite` dependency until
  M2/M3 actually need them (kept `Cargo.toml` to
  `ferrule-core, tokio, async-trait, serde, serde_json, thiserror, tracing`
  + `tempfile` dev-dep, to keep this milestone's diff and dependency
  footprint scoped to what M1 actually uses).

**Design decision — session persistence without touching `ferrule-core`:**
the brief asked for "session persistence/resume on top of existing JSONL
transcripts" without listing `ferrule-core` changes as in scope. Read
`ferrule-core::transcript::Transcript` closely: `Transcript::create(dir, id)`
is safe to call repeatedly (it (re)opens the file and always appends a
`Meta` record — confirmed by its existing round-trip test), and
`read_messages()` already filters to only `type == "message"` records, i.e.
it reconstructs prior conversation turns and skips metadata/event noise.
Combined with `Agent.messages` being a public field, a lane can be (re)built
statelessly: call `Transcript::create` (creates on first contact, reopens on
restart), `read_messages()`, hand the transcript to the caller's
`AgentFactory` (which pushes a fresh system prompt — may embed live memory
recall, so it should never be replayed from history), then push every
non-`System` historical message onto the new `Agent.messages` before serving
the first live message. No new persistence format, no `ferrule-core` diff.
This is exercised directly by
`router::tests::resumes_history_from_transcript_across_router_restarts`,
which builds a `Router`, dispatches one message, **drops the whole `Router`**
(simulating a gateway restart), builds a **second, independent** `Router`
pointed at the same `sessions_dir`, dispatches a second message, and asserts
a `CountingProvider` (replies with the number of `Role::User` messages seen
in-context) reports 2, not 1 — proving history actually survived a cold
restart via the JSONL file, not via any in-memory carry-over.

**Design decision — event stream is intentionally dropped in the daemon
loop:** `Agent::run()` requires an `mpsc::Sender<AgentEvent>` for streaming
lifecycle events (tool calls, reasoning, compaction, usage). The gateway
doesn't yet stream anything to channels (no channel adapter speaks
incremental/typing-indicator updates yet), so `run_lane` creates the channel
and immediately drops the receiver half. This is safe by construction. not
a leak or a race, because `Agent::emit` already tolerates a detached
receiver (`let _ = tx.send(ev).await;` in `ferrule-core::agent`, there for
exactly this "headless mode" case). Revisit if/when a channel wants
mid-turn progress updates (e.g. Telegram "typing…" or partial edits).

**Design decision — errors are surfaced to the chat, not just logged:** a
failed `agent.run()` inside a lane is `tracing::error!`'d *and* turned into
a best-effort reply (`"internal error: {e}"`) sent back through the
originating channel, rather than only logged and silently dropped. This is
a direct response to the brief's NanoClaw warning ("an errored run logged as
successful must not be reproduced") — even though that specific flaw is
about the M3 scheduler's run-log, the same silent-failure failure mode
applies to a chat message that gets no reply at all, so the same discipline
was applied here too.

**Verification:**
- `cargo check --workspace --all-targets` — clean, first attempt.
- `cargo test --workspace` — **31/31 green** (24 pre-existing + 7 new:
  3 in `session.rs`, 3 in `router.rs`, 1 in `gateway.rs`). One real bug was
  caught and fixed by the test suite itself: the first version of the
  `gateway.rs` end-to-end test asserted the reply was delivered immediately
  after `Gateway::run()` returned, but `run()` only guarantees every inbound
  message has been *dispatched* onto its session lane — the lane's spawned
  task still needs to run the agent turn and call `Channel::send()`
  afterwards. Fixed by polling with a bounded 2s timeout instead of a bare
  assertion (same pattern already used in the `router.rs` tests' `wait_until`
  helper). Worth flagging: this is a real race inherent to the design (fire-
  and-forget lane dispatch), not just a test artifact — a future consumer of
  `Gateway::run()`'s return should not assume "returned" means "all replies
  delivered."
- `cargo clippy --workspace --all-targets` — zero warnings in
  `ferrule-gateway`; one pre-existing warning in `ferrule-core::agent`
  (`collapsible_if`) that predates this session and was left untouched
  (out of scope for this milestone's diff).
- `rustup component add clippy rustfmt` — both were missing in this sandbox
  (only `cargo`/`rust-std`/`rustc` were installed before); now installed.

**M2 — real channel adapters + CLI wiring:**

- `channels/local.rs` — `LocalChannel<R, W>`, generic over
  `AsyncBufRead + Unpin + Send` / `AsyncWrite + Unpin + Send` rather than
  hardcoding `Stdin`/`Stdout`, specifically so it can be driven by
  `tokio::io::duplex()` in-memory pipes in tests instead of a real terminal.
  `LocalChannel::stdio(chat_id)` is the real constructor used by the CLI;
  `LocalChannel::new(chat_id, reader, writer)` is the generic one tests use.
  One line of input = one `InboundMessage`; blank lines are skipped; `send()`
  writes the reply text plus a trailing newline. No capabilities (no
  edits/reactions) — it's the smoke-test channel, not a product surface.
- `channels/telegram.rs` — `TelegramChannel`, long-polling `getUpdates`
  (`?timeout=30&offset=N`, `offset` tracked in an `AtomicI64`, advanced past
  the highest `update_id` seen so it never redelivers), `sendMessage` and
  `editMessageText` for outbound (`capabilities().edits == true`; `react()`
  is left at the trait default `Unsupported` since only `edit()` was asked
  for). `base_url` is a real constructor parameter (`with_base_url`, `new`
  defaults it to `https://api.telegram.org`) precisely so the test suite
  never touches the real Telegram API — **there is no live bot token in this
  environment and none was looked for.** The test spins up a hand-rolled
  mock Bot API server (`std::net::TcpListener` + a `std::thread` looping on
  `incoming()`, since long-polling needs more than the one-shot
  accept-then-respond pattern `openai_compat`'s mock server uses): the first
  `getUpdates` returns one canned update guarded by an `AtomicBool` so
  subsequent polls return an empty result instead of redelivering it, and
  `sendMessage`/`editMessageText` bodies are parsed out of the raw request
  text and recorded for the test to assert against. Verifies the full round
  trip: a long-polled update becomes a correctly-shaped `InboundMessage`,
  and both `send()` and `edit()` produce the right JSON payloads
  (`chat_id`, `text`, `reply_to_message_id`, `message_id`).
- `lib.rs` now declares `pub mod channels;` and re-exports `LocalChannel`/
  `TelegramChannel`; `ferrule-gateway/Cargo.toml` gained one new dependency,
  `reqwest` (already a workspace dependency via `ferrule-providers`, so no
  new crate was introduced to the workspace — just a new user of it).
- **CLI wiring** (`ferrule-cli`): added `[gateway]` to `ferrule.toml`
  (`config.rs`'s new `GatewayConfig`: `local: bool`, `telegram_token_env:
  Option<String>`, `telegram_base_url` defaulting to the real API). Added
  `ferrule gateway [--provider] [--workspace] [--max-iterations]`. Refactored
  `build_agent` into `build_agent_from(provider, workspace, max_iterations,
  transcript: Option<Transcript>)` — the actual system-prompt/provider/tool
  assembly — with the old `build_agent(session_id)` now a thin wrapper that
  creates its own transcript and delegates; `run_gateway` builds the
  `AgentFactory` closure around `build_agent_from` (mapping `anyhow::Error`
  to `GatewayError::Channel` at the seam), constructs whichever channels are
  enabled in config, registers each one into **both** the `Router`'s
  name-keyed map (so replies get routed back through the right adapter) and
  the `Gateway`'s adapter list (so its `run()` loop actually polls it), and
  `bail!`s with a clear message if `[gateway]` enables nothing.

**M2 verification:**
- `cargo check --workspace --all-targets` — clean.
- `cargo test --workspace` — **36/36 green** (31 prior + 5 new: 3 in
  `channels::local`, 2 in `channels::telegram`).
- `cargo clippy --workspace --all-targets` — zero new warnings (one caught
  and fixed mid-milestone: an unused `AsyncWriteExt` import in
  `channels/local.rs`'s test module, redundant because `use super::*` already
  brings it in from the parent module's own imports). The one pre-existing
  `ferrule-core::agent` `collapsible_if` warning from M1 is still there,
  untouched, in a file this milestone never edited.

**What remains (for the next session):**
- M3 — cron + one-shot scheduler persisted in SQLite (mirror
  `ferrule-memory`'s `rusqlite` + WAL approach), truthful run-status logging
  (this is the one the brief singles out: a run must never log
  `status = success` when it errored), optional pre-check "gate" script.
  Needs `rusqlite` added at that point.
- M4 (only if time allows) — stdio MCP client via the `Tool` trait; add
  `.no_proxy()` to provider-test and (new) Telegram-adapter reqwest clients
  for fully hermetic tests (today `openai_compat`'s mock-server test still
  relies on the ambient `NO_PROXY` env var rather than being self-contained).
- Not started: feature-gating channel adapters, OS-level sandboxing — both
  explicitly deferred per `docs/research-report.md`'s own phasing, out of
  scope for this task's tool-call budget.

### 2026-09-24 — ferrule-gateway M3 scheduler (Devi, Opus 5.5)

Built M3 inside `ferrule-gateway` as a `scheduler/` module (not a new
crate: it needs the gateway's `Router` and session lanes, so a separate
crate would only add a public seam with a single consumer). Files:
`scheduler/{mod,store,gate,timing,error}.rs`. New deps: `rusqlite` and
`uuid` (both already workspace deps), `chrono`, `chrono-tz`, `croner 4`.

**What it does:**
- Tasks are `cron` (5-field, evaluated in a per-task IANA timezone set by
  `ferrule tasks add --timezone`) or `once` (RFC 3339 instant). Tasks and
  runs are stored in `tasks.db` in the data dir (same rusqlite + WAL pattern
  as `ferrule-memory`).
- **Every run gets its own row:** `running`, then `succeeded`, `failed` or
  `skipped`, with timestamps, error text and truncated output. A provider or
  agent error is recorded as `failed` with the error text, never as
  `succeeded`. That's the NanoClaw flaw this was designed against, and
  `provider_error_produces_failed_run_never_succeeded` pins it down. A run
  still `running` when the daemon starts is recovered as `failed`
  ("interrupted").
- **How a task reaches the agent:** each task runs as a turn on its own
  resumable session (the reserved pseudo-channel `"scheduler"` plus the
  task id), going through the same `Router`/`Transcript` machinery chat
  uses, so a task keeps its history across runs. Fire-and-forget
  `dispatch()` couldn't report whether the turn succeeded, so
  `Router::dispatch_and_wait` was added. It returns the turn's real
  `Result`, and the scheduler delivers the output to the task's configured
  destination (`channel` + `chat_id`) itself. The lane's own reply path
  no-ops for `"scheduler"` because that pseudo-channel is never registered
  as a real `Channel`, so the output can't be delivered twice (tested).
- **Gate script** (optional, per task): same contract as NanoClaw, so
  existing gate scripts port over unchanged. `{"wakeAgent": false}` means
  `skipped`, with no agent turn and zero tokens. `{"wakeAgent": true,
  "context": …}`, or any non-contract stdout, means the agent runs with that
  output as extra context. **A non-zero exit or a timeout means `failed`**,
  so a gate crash can't masquerade as "nothing to do". On timeout the child
  is killed (`kill_on_drop`), not orphaned. The default timeout is
  `[scheduler] gate_timeout_secs = 60`.
- **No overlap:** `start_run` refuses a second concurrent run of the same
  task and returns `RunOutcome::AlreadyRunning` without writing a row.
  Because the store is SQLite with WAL, this also holds across processes
  (the daemon and a one-off `ferrule tasks run-now`).
- **Missed runs:** `next_run_at` is always recomputed as "the next
  occurrence strictly after now" and never chained from the old value. A
  cron task that missed N slots during downtime therefore gets exactly one
  catch-up run, not a burst. A `once` task fires at most once, even late.
- **CLI:** `ferrule tasks add|list|pause|resume|delete|runs|run-now`, and
  `ferrule gateway` spawns the scheduler loop next to `Gateway::run()`.
  `[scheduler]` config: `tick_interval_secs` (30), `gate_timeout_secs` (60),
  `gate_workspace` (".").

**Real bugs found along the way:** `dispatch_and_wait` moved `sid` into
one error path and then used it in another (compile error, fixed with a
clone). `store.rs` had three MutexGuard temporary-lifetime bugs, in
`list`/`due_tasks`/`runs_for`, where `.prepare()` was called on a lock
guard dropped at the end of the statement. Each is now bound to a local
first.

**Verification:** `cargo check --workspace --all-targets` is clean.
`cargo test --workspace` is **65/65 green**, up from 36: 29 new, covering
scheduler orchestration (provider error → failed, interrupted recovery,
no-overlap, gate skip/failure/timeout, one-shot fires once), gate
contract, store, timing (non-UTC timezone + DST, strictly-after,
missed-backlog collapse) and 3 `dispatch_and_wait` router tests. `cargo
clippy`: zero new warnings; the pre-existing `ferrule-core::agent`
`collapsible_if` is untouched. The new `scheduler/*.rs` files are
rustfmt-clean.

**Open decision for Max:** the committed M1/M2 code (and some older
files) is *not* rustfmt-default clean. It uses wider lines and there's no
`rustfmt.toml`. The M3 work deliberately didn't run a crate-wide
`cargo fmt`, because that would rewrite unrelated code and cause merge
conflicts for anyone editing from another machine. Adopting a
`rustfmt.toml` plus a single format-only commit is a cheap decision, but
it's his.

**What remains:** M4, the stdio MCP client (`tools/list` → `Tool` impls
named `mcp__<server>__<tool>`, configured via `[[mcp.servers]]`). Also
`.no_proxy()` on the Telegram adapter's and the provider test's reqwest
clients, so tests stop depending on the ambient `NO_PROXY`. Process note:
this milestone was split across three agent sessions because the host
blocks all tools on long background sessions. Future milestones should be
scoped to fit one short session.

### 2026-09-24 — Research: multi-provider routing + runtime local models (Devi, Opus 5.5)

Max asked (voice note): can the agent use several providers in parallel and know
which to pick, and can it "create a local model while running" for speed/cost.
Answer: `docs/research-routing-and-local-models.md`. Grounded in `provider.rs`
(the `Provider` trait is already the right seam — a `RouterProvider` needs no
core changes), `openai_compat.rs` (already parses cached-token usage, and
already speaks Ollama's OpenAI-compatible dialect for free), and PLAN.md's own
gap list (no cost/observability ledger).

Headline numbers: RouteLLM >2x cost cut with no quality loss (arXiv:2406.18665);
FrugalGPT cascades up to 98% cheaper at GPT-4 quality (arXiv:2305.05176); LoRA
cuts trainable params ~10,000x with no added inference latency (arXiv:2106.09685).
Provider ToS (quoted verbatim in the doc): Anthropic/OpenAI/Google all bar
training *competing* models but each carves out internal classifiers/categorizers
— a small transcript-trained routing/tool-selection model reads as inside that
exception (plain reading, not legal advice). Speculative decoding needs white-box
logit access, so it's off the table against any closed API.

Proposal, phased: (0) a per-call cost/latency/outcome ledger first — nothing else
can be learned without it; (1) a rule-based `RouterProvider` (cheap tier, escalate
on error/malformed tool call, routed per-*session* not per-call to protect prompt
caching); (2) a tiny embedding+kNN classifier once the ledger has data; (3) narrow
LoRA fine-tunes (via MLX-LM on Max's M1 Pro, confirmed sized right per
`max-hardware-decision.md`) only once a specific high-volume sub-task justifies it.
Biggest first win: Phase 0 + a two-tier cascade on the scheduler's non-interactive
task turns — lowest risk, no live user waiting. Open decisions for Max in the doc:
escalation policy, which providers become tiers (need a second live driver first),
whether to scope down the "local model" framing, Phase-3 hardware, and ToS comfort.

### 2026-09-24 — ferrule-gateway M4 stdio MCP client (Devi, Opus 5.5)

Max: "ותמשיך לעבוד". This is M4, the last gateway milestone from the M1-M4
plan. It adds a new crate, `crates/ferrule-mcp` (client.rs, tool.rs,
config.rs, error.rs), rather than a module inside `ferrule-tools`. That
keeps process-spawning and JSON-RPC code out of the fixed built-in toolset,
and `ferrule-core` stays untouched: each MCP tool is just another
`Arc<dyn Tool>`.

**Protocol:** newline-delimited JSON-RPC 2.0 over the child's stdio. The
handshake sends `initialize` (protocolVersion `2025-06-18`, no client
capabilities), then `notifications/initialized`. After that the client
calls `tools/list` and follows `nextCursor` until it runs out. `tools/call`
is used per tool call. Tools are registered as `mcp__<server>__<tool>`, and
a result with `isError: true` becomes `CoreError::ToolFailed` carrying the
server's text, not a panic. Non-JSON stdout lines are skipped, and stderr
is drained into `tracing::warn`.

**Robustness:**
- Every call has a timeout: `timeout_secs`, default 60.
- A dead connection (EOF on stdout, or a failed write) gets exactly one lazy
  respawn on the next call. There is no retry loop.
- Children are started with `kill_on_drop`.
- A server that fails to start is logged and skipped, so the agent still
  runs without it.
- Config is `[[mcp.servers]]` with `name`/`command`/`args`/`env`/
  `timeout_secs` (example in `EXAMPLE_CONFIG`).

**Three bugs found in the first cut, fixed before merge (72 tests, up from 70):**
1. *One process set per session.* The first version connected MCP servers
   inside `build_agent_from`, which the gateway's `AgentFactory` calls for
   every new session. The result was N chats × M servers child processes.
   It also needed a `block_in_place` bridge, because the factory is a sync
   `Fn` and that bridge panics on a current-thread runtime. Now
   `connect_mcp_servers` runs once, async, in `run_gateway` /
   `tasks_run_now` / `build_agent`, and the resulting `Vec<Arc<dyn Tool>>` is
   cloned into each agent's registry.
2. *A slow call blocked every other call to the same server.* The
   connection mutex was held across the whole response wait. Now a cloneable
   `Handle` (stdin + pending map + alive flag) is taken out under the lock
   and the lock is released before waiting. Regression test:
   `slow_call_does_not_block_other_calls_to_the_same_server`.
3. *Server-initiated requests were treated as responses.* Any stdout
   message with a numeric `id` was routed into the pending map, but a
   server's own request (for example `ping`) carries an id from the
   *server's* id space, so it could complete one of our calls with garbage.
   It was also never answered, so the server could hang. Now anything with
   a `method` is server-initiated: `ping` gets `{}`, other requests get
   -32601, and notifications are logged. The reply is written from a
   spawned task, so a full stdin pipe can't stall stdout draining.
   Regression test:
   `server_initiated_ping_is_answered_and_not_mistaken_for_a_response`.
   **Pre-fix control run:** with that branch disabled, the test fails.

**Verification:**
- `cargo test --workspace` is 72/72 **with `NO_PROXY` unset**. The
  provider's and the Telegram adapter's reqwest clients now call
  `.no_proxy()` under `cfg!(test)`, so release binaries are unaffected.
- `cargo clippy --workspace --all-targets` shows only the old
  `ferrule-core` warning.
- New files were rustfmt'd individually. There was no crate-wide
  `cargo fmt` (that decision is still open, see the M3 entry).
- End to end: the debug `ferrule run` binary ran against a Python mock
  OpenAI endpoint plus `tests/fixtures/mock_mcp.py` configured as
  `[[mcp.servers]]`. The first request advertised all six `mcp__mock__*`
  tools, the model's `mcp__mock__echo` call reached the MCP server, its
  output came back as the `tool` message, and the final answer quoted it.
  No child process survived exit. Not yet tried: a real published MCP
  server (e.g. `@modelcontextprotocol/server-filesystem` via `npx`) and a
  real LLM.

**Not in scope, and known:**
- Only stdio transport. There is no Streamable HTTP/SSE.
- The client only consumes tools. It ignores resources, prompts, sampling,
  roots and `notifications/tools/list_changed`, so the tool list is fixed at
  startup.
- Only text content blocks are surfaced. Images and resources are dropped.
- Server `env` is merged over the parent's env, not isolated. That's fine
  for now, but it matters once the credential-injection gateway exists.

**What's next:** M1-M4 are all done. Of the remaining gaps, the cheapest
one that needs no decision is Phase 0 from
`docs/research-routing-and-local-models.md`: a per-call
cost/latency/outcome ledger. It is also the prerequisite for any routing
work.

### 2026-09-24 — Phase 0 per-call ledger (Devi, Opus 5.5)

Phase 0 of `docs/research-routing-and-local-models.md`, which needs no
decision from Max and is the prerequisite for any routing work. Every
provider call now leaves one row behind, including the ones that fail.

**Seam:** the `Agent` holds an optional `LedgerContext` (a
`LedgerSink` trait object plus task shape, origin and model), and
`call_provider` times each `Provider::complete` and records the result.
Not a `Provider` decorator: a decorator can't see the loop iteration or
tell a turn from a compaction pass, and the Phase 1 `RouterProvider` will
itself be a `Provider`, so the ledger has to sit above it to see which tier
answered. `ferrule-core` gets only the trait and the record type
(`ledger.rs`, plus a `chrono` dependency); the JSONL file sink lives in
`ferrule-cli`, so core stays storage-free.

**Row:** timestamp, session_id, task_shape, origin, provider, model,
iteration, call_kind (`turn` / `compaction`), input_tokens (includes cached,
OpenAI convention), cached_input_tokens, output_tokens, tool_calls,
latency_ms, outcome (`ok` / `error`), error_kind, error_message (truncated
to 500 chars), cost_usd. Written to `<data_dir>/ferrule/ledger.jsonl`, not
the session transcript: one file across all sessions is what the summary
and a future classifier read, and it keeps transcript resume untouched.
Task shape: `run`, `chat`, `gateway` (origin = channel) or `scheduler`
(origin = task id), derived from the session id the gateway and scheduler
already build.

**Cost:** optional `price_input_per_mtok`, `price_cached_input_per_mtok`,
`price_output_per_mtok` per `[providers.*]` entry. Cost is uncached×input +
cached×cached + output×output, per million tokens. It is computed only when
all three prices are set (a partial price list silently under-reports) and
only on ok rows. A sink write failure is logged and never fails the turn.

**`ferrule ledger [--since 7d|12h|30m|<RFC 3339>]`:** groups by
(task_shape, provider, model): calls, errors, tokens, cache hit %, p50/p95
latency (nearest rank), cost. Cost shows `-` when a group has no priced
rows and a `*` suffix when only some are priced. Malformed lines are
skipped and counted.

**Verification:**
- `cargo test --workspace` is 81/81 (9 new: 2 in core, including an error
  row from a failing provider; 7 in the cli module: percentiles, cost,
  pricing needs all three prices, aggregation, session classification,
  `--since` parsing including multibyte input, and a sink round-trip).
- Clippy shows only the old core warning. The two new files were
  rustfmt'd individually, with no crate-wide `cargo fmt`.
- End to end, with an isolated `XDG_DATA_HOME`: the debug binary ran
  against a mock LLM with a mock MCP tool (priced provider, 2 calls, the
  second 900/1200 cached), against a provider on a closed port (an error
  row with `error_kind: provider`), and as a scheduler task via
  `tasks run-now` (`task_shape: scheduler`, origin = task id). Costs
  matched a hand calculation ($0.00375 + $0.00147). `--since` with a
  future time, and a bogus value, behaved correctly. The gateway path is
  covered by the same factory code but wasn't run live (it needs a
  Telegram token).

**Not recorded yet:** the research doc also suggested "did compaction
actually shrink the transcript" and "did the turn need a retry". There is
no retry in the loop yet, and compaction shrink is left for when Phase 1
needs it. There is no rotation. The file grows about 400 bytes per call.

**Process note:** a background agent built the core half, then had all its
tools blocked after 44 calls (the known long-session block). The CLI half
was finished in the main session.

**What's next:** Phase 1 (`RouterProvider`) is blocked on Max's decisions
in the research doc: escalation policy, and which providers become tiers.
It also needs a second live driver. Of the remaining gaps, the ones that
need no decision are the skills/plugin system and OS-level sandboxing.

### 2026-09-24 — M5 Agent Skills (Devi, Opus 5.5)

The next gap that needed no decision from Max. Ferrule now reads skills
in the open Agent Skills format (agentskills.io, the one Claude Code, Codex
and others share), so an existing skill works here unchanged. Built from
the spec and its client-implementation guide, both read as primary
sources.

**New crate `ferrule-skills`**, kept separate from core (same reasoning as
`ferrule-mcp`: core stays storage- and format-free):
- `frontmatter.rs`: a small, lenient reader for SKILL.md frontmatter
  instead of a YAML dependency. It reads top-level scalars (plain, quoted
  and multi-line, `|`/`>` block scalars), skips nested maps such as
  `metadata:`, and accepts the invalid-but-common unquoted `key: a: b`,
  as the guide recommends. It tolerates CRLF and a BOM.
- `discover.rs`: the scan order is project scope first
  (`<ws>/.ferrule/skills`, `<ws>/.agents/skills`, `<ws>/.claude/skills`),
  then user scope (`[skills].paths`, `~/.config/ferrule/skills`,
  `~/.agents/skills`, `~/.claude/skills`). The first skill to claim a name
  wins, and later ones are reported as shadowed.
  - The walk follows symlinks (skill installers symlink directories into
    place), skips hidden dirs and `node_modules`, doesn't descend into a
    skill's own subdirectories, and is capped at depth 4 and 2000 dirs.
  - Validation is lenient: a bad or mismatched name only warns, and a
    missing name falls back to the directory name. A skill is skipped only
    when it has no frontmatter or no description.
  - `disable-model-invocation: true` hides a skill from the model.
- `tool.rs`: `activate_skill(name)` returns the body with its frontmatter
  stripped. The result is wrapped in `<skill_content name="…">`, followed
  by the absolute skill directory and a `<skill_resources>` listing of up
  to 50 files, which are listed but not read. Further behavior:
  - The file is read fresh on each activation.
  - Activation is deduped per session.
  - Bodies are capped at 60k chars inside the wrapper, so the closing tag
    always survives.
- `read_skill_file(name, path)` is needed because the fs tools are
  workspace-confined and skills usually live outside the workspace. It
  refuses absolute paths and `..`, and checks containment after resolving
  symlinks, so a symlink pointing out of the skill is refused too.
- Both tools take `name` as an enum of the model-visible skills. Neither
  tool is registered, and the catalog is omitted, when there are none.

**Core change (compaction):** skill instructions are now protected from
summarization. `maybe_compact` finds `<skill_content>` blocks in the part
being folded; the constants live in core and the skills crate builds its
tag from them. Those blocks are appended verbatim to the summary message
under "Skill instructions activated earlier in this session — still in
force", unless the verbatim tail still holds them. They're replaced by a
placeholder in the text sent to the summarizer, so it doesn't paraphrase
them. A second compaction carries each block once, not twice.

**Wiring:** `build_agent_from` rediscovers skills for every agent it
builds, so a skill installed while the gateway runs shows up in the next
session. It appends `[Skills]` plus the catalog to the system prompt,
after the validation policy and before long-term memory, and registers
the two tools. Config goes in `[skills]`: `enabled`, `project` (turn it
off for untrusted repos, because a repo's skills reach the system
prompt), `paths` (`~` expanded) and `disabled`. `ferrule skills
[--workspace]` lists each skill's name, scope, whether the model can see
it, its location, and every warning or skip. It works without a config
file.

**Verification:**
- `cargo test --workspace` is 104/104 (23 new): 9 frontmatter, 6
  discovery, 5 tool and 1 catalog test in `ferrule-skills`, plus 2
  compaction tests in core (block carried verbatim and hidden from the
  summarizer across two compactions; not duplicated when still in the
  tail).
- Clippy shows only the old core warning. The new crate was rustfmt'd
  file by file, with no `cargo fmt`.
- Against the real corpus of 46 installed skills in `~/.claude/skills`
  (7 of them symlinks), `ferrule skills` found all 46, 17 hidden, with
  zero diagnostics. All 29 catalog descriptions matched PyYAML's parse
  exactly.
- End to end, the debug binary ran against a mock LLM with a
  project-scope `pdf` skill shadowing the user one. The mock called:
  - `activate_skill(pdf)`, which returned the project body and resources
    list;
  - `read_skill_file(references/NOTE.md)`, which returned the file;
  - `read_skill_file(../../../../../etc/passwd)`, which was refused;
  - `activate_skill(pdf)` again, which answered "already active".

  The captured request had 29 names in the tool enum and a catalog of
  about 11.6k chars (about 100 tokens per skill).

**Not done (deliberately):**
- User-explicit activation (`/skill-name` in chat or Telegram).
- Enforcing the experimental `allowed-tools` field.
- Running a skill in a subagent.
- A trust prompt for project skills beyond the `project` switch.
- Per-session dedupe resets when the gateway rebuilds an agent from a
  transcript, so a re-activation in a new process re-sends the body. That
  is harmless, and compaction's tool-result dedupe folds exact repeats.

**What's next:** OS-level sandboxing is the remaining gap that needs no
decision. Phase 1 routing still waits on Max.

### 2026-09-24 — M6 OS sandbox for the shell tool (Devi, Opus 5.5)

The last gap that needed no decision from Max. Before this, the only thing
between a model and the machine was an 11-pattern substring deny-list on
`shell`, and a lexical-only path check on the fs tools. Now the kernel
enforces it. The design follows OpenAI Codex's sandbox (read as primary
source: `codex-rs/linux-sandbox`, `core/src/seatbelt.rs` and its `.sbpl`
policies), minus its bubblewrap layer, proxy routing and per-path carve-outs.

**New crate `ferrule-sandbox`** (only a `libc` dependency, no landlock or
seccomp crates):
- `Policy` is the `[sandbox]` config section. It has `mode`
  (`workspace-write` by default, or `read-only` / `off`), `require`,
  `network` (default true), `writable_roots` (`~/` expands, relative means
  inside the workspace, missing ones are skipped), `tmp` (default true:
  /tmp, `$TMPDIR`, /dev/shm), `scrub_secret_env` (default true) and
  `env_passthrough`. Unknown keys are an error, so a misspelt
  `netwrok = false` can't quietly leave the network on.
- `Sandbox::new` detects a backend and **probes it by running
  `sh -c 'exit 0'` under it**, so it fails at startup rather than on the
  first tool call. On failure it warns and runs unsandboxed, or refuses to
  start when `require = true`. `Sandbox::command()` returns a ready
  `std::process::Command`, which the shell tool (and the tests) spawn.
- **Linux (`linux.rs`):** raw Landlock syscalls (444-446). The ABI is
  detected at runtime and the handled rights scale with it (REFER from
  ABI 2, TRUNCATE from 3, IOCTL_DEV from 5). `/` gets read and execute;
  `/dev/null` and each writable root get write. Rules on files are cut
  down to file-only rights, which the kernel otherwise rejects. The
  ruleset is built in the parent. The `pre_exec` hook only calls
  `prctl(NO_NEW_PRIVS)`, the seccomp install and `landlock_restrict_self`,
  so it doesn't allocate after fork.
- **Network off on Linux** is a classic-BPF seccomp filter built by hand,
  for x86_64 and aarch64:
  - a wrong arch kills the process;
  - x32 syscalls get EPERM;
  - `socket()` with any domain other than AF_UNIX gets EPERM;
  - `io_uring_setup` gets EPERM, because io_uring can open sockets without
    going through the `socket` syscall;
  - everything else is allowed.

  Unix sockets keep working because they are local IPC. The unit tests run
  the program through a small BPF interpreter.
- **macOS (`seatbelt.rs`):** `/usr/bin/sandbox-exec -p <profile>` with an
  absolute path, never resolved through PATH. The profile is Codex's
  base/network/prefs `.sbpl` policies (Apache-2.0), vendored with a
  header, plus:
  - `(allow file-read*)`;
  - write roots passed as `-D` params rather than spliced into the text, so
    a path containing `"` can't rewrite the policy;
  - `file-write-unlink` anchor denies on each root;
  - an XPC-lookup deny;
  - the F_MAKECOMPRESSED / F_TRANSFEREXTENTS fcntl deny.

  **Not yet run on a real Mac.** The tests check the profile's structure
  and the argument order only.
- Env scrubbing works by name. A variable is dropped if its name contains
  KEY, SECRET, TOKEN, PASSWORD, PASSWD or CREDENTIAL, or if the config
  names it (every provider's `api_key_env` and `telegram_token_env`).
  `env_passthrough` overrides this. Values are never printed; `ferrule
  sandbox` lists names only.

**Wiring:**
- **`ShellTool`** now carries an `Arc<Sandbox>`. A bare `ShellTool::default()`
  gets `Sandbox::off()`, which still scrubs the environment. Each command
  also:
  - closes stdin, so an interactive prompt fails instead of hanging;
  - runs in its own process group, and on timeout the whole group is
    killed. Before, only `sh` died and anything it had started kept running.
  - gets a one-line note in its tool description saying where writes are
    allowed and whether the network is, so the model doesn't retry a
    "Permission denied" forever.
- **The scheduler's gate scripts had the same timeout bug:** the old doc
  comment said `kill_on_drop` killed the child, but only the `sh` died.
  They now run in a process group and the group is killed. The gates
  themselves are *not* sandboxed, because they are operator-written config.
- **fs tools:** `resolve()` now checks real paths. It canonicalizes the
  deepest existing ancestor and re-appends the part a write will create.
  A dangling symlink is refused, because a write would follow it anywhere.
  The tools act on the resolved path, not the model's string.
- **CLI:**
  - The sandbox is built once per process (`OnceLock`), so the gateway
    doesn't probe for each session; a `[sandbox]` edit needs a restart.
  - In `read-only` mode, `write_file` is removed from the registry
    (`ToolRegistry::remove`, new in core).
  - **ferrule's data dir is deliberately not writable** from the shell. It
    holds `tasks.db`, whose gate commands run unsandboxed, so write access
    there would be a one-hop escape. That broke the "persist facts with the
    memory CLI" instruction, so there are now native `remember` and
    `recall` tools that run in-process and can only add or search
    memories. The system prompt points at those instead.
  - New `ferrule sandbox [--workspace]` shows the backend, mode, network,
    writable roots and withheld variable names. It then runs real checks
    through the same `Sandbox::command` the shell tool uses:
    - no secret variable is visible;
    - a workspace write works (or is refused in read-only mode);
    - a write to a directory outside every root that this process itself
      can write ($HOME, /var/tmp or the workspace parent) is refused;
    - a loopback socket opens or is refused, via a hidden
      `--probe-net` re-exec.

    It exits non-zero if any check fails.

**Verified:**
- 127 tests pass workspace-wide. There are 23 new ones (plus one ignored
  helper), and the 5
  enforcement tests in `ferrule-sandbox/tests/enforcement.rs` run against
  the real kernel: Landlock ABI 8 on kernel 7.0 in this container. They
  skip, with a note, where no sandbox is available. They cover:
  - an outside write is refused, including from a grandchild `sh`;
  - read-only mode refuses even the workspace;
  - temp dirs work, and so do secret variables being scrubbed;
  - with the network off, the probe must fail with EPERM (errno 1)
    specifically, so it can't pass by accident. With it on, the probe
    succeeds, and AF_UNIX still works when it is off.
- Both process-group timeout tests (shell and gate) were run as controls
  with the `killpg` removed, and failed as they should.
- Clippy shows only the old core warning. The new files were rustfmt'd
  one by one, with no `cargo fmt`.
- `ferrule sandbox` in the container passed every check in all four
  modes: default, `network = false` with a `~/.cache` root, `read-only`,
  and `off`.
- End to end, the debug `ferrule run` binary ran against a mock LLM. The
  shell description carried the sandbox note, and `remember` and `recall`
  were advertised.
  - One shell call wrote inside the workspace, and its write to
    `/var/tmp` got "Permission denied". `$MOONSHOT_API_KEY` came through
    empty although the parent had it set.
  - `remember` then `recall` round-tripped.
  - `write_file` through a workspace symlink to `/var/tmp` was refused as
    an escape.

**Known limits (documented, not fixed):**
- Reads are not confined. The agent can read anything the user can, such
  as `~/.ssh` or other sessions' transcripts. Confining reads needs a
  per-toolchain allowlist, and that is a design question, not a default.
- On Linux, connecting to a pathname unix socket isn't blocked: seccomp
  allows AF_UNIX, and Landlock doesn't cover `connect()`. So a reachable
  `docker.sock` is still a way out, even with the network off.
- MCP servers run outside the sandbox, because ferrule spawns them itself
  rather than through the shell. So do the scheduler's gate scripts.
  Skill scripts run through the shell tool, so they *are* confined.
- The fs tools have a TOCTOU window: a symlink swapped in between the
  check and the write gets through. The sandbox is what holds against a
  hostile workspace; the fs tools are in-process and not sandboxed.
- Build caches outside the workspace (`~/.cargo`, `~/.cache`, `~/.npm`)
  aren't writable by default, so `cargo build` or `npm install` fail until
  they are added to `writable_roots`. The example config says so.
- There is no Windows backend. It warns and runs unsandboxed there, or
  refuses to start with `require = true`.
- Scheduling a task from inside the agent (`ferrule tasks add` via shell)
  no longer works under the sandbox, which is the point of keeping
  `tasks.db` out of reach. If agents should schedule, that needs a narrow
  tool, like `remember`.

**What's next:** every gap that needed no decision from Max is now closed.
What's left waits on him: Phase 1 routing (the five decisions in
`docs/research-routing-and-local-models.md`), rustfmt adoption, and
whether a credential-injection gateway or multi-agent orchestration comes
next.

### 2026-09-24 — M7 credential gateway (Devi, Opus 5.5)

Max picked the credential gateway as the next milestone (msg 3050) and
asked for something simpler than OneCLI's connect-per-service flow (msg
3052). The result is one line of config per secret:

```toml
[secrets]
GITHUB_TOKEN = ["api.github.com", "*.githubusercontent.com"]
TELEGRAM_BOT_TOKEN = { hosts = ["api.telegram.org"], in_url = true }
```

**New crate `ferrule-proxy`** (hyper 1 + rustls/ring + rcgen, ~2,200
lines with tests):
- **Placeholders.** `placeholder.rs` keeps a known prefix (`ghp_`,
  `github_pat_`, `glpat-`, `sk-ant-`, `sk-`, `xoxb-`, `hf_`, `sk_live_`…)
  when at least 16 random characters remain. The rest is hex of
  SHA-256(seed ‖ name ‖ counter), the same length as the real value (at
  least 16). It is stable across restarts and across a same-shape
  rotation.
- **CA and seed.** `ca.rs` creates `<data_dir>/ferrule/proxy/keys/` (dir
  0700, `ca.key` and `seed` 0600) atomically through a temp dir and a
  rename. The CA is valid 10 years; leaf certificates are valid 1 year and
  cached per host. It also writes a combined bundle (system bundle + the
  CA) for the child.
- **Host patterns** (`hosts.rs`) are exact names or `*.suffix` with at
  least two labels. URLs, ports, paths and a bare `*` are rejected.
- **The proxy** (`server.rs`) is loopback-only.
  - It requires a per-run token (407 + `Proxy-Authenticate` otherwise)
    and accepts only CONNECT.
  - It connects upstream before answering, so a dead host is a 502.
  - Unbound hosts get `copy_bidirectional`. Bound hosts are intercepted
    with ALPN http/1.1: 421 on a Host/authority mismatch, 501 on Upgrade.
    Hop-by-hop, `Accept-Encoding` and `Expect` headers are stripped.
- **Upstream chaining** (`upstream.rs`) follows `HTTPS_PROXY` /
  `ALL_PROXY` (http:// only, userinfo becomes Basic) and `NO_PROXY`.
  Without it, the proxy would be useless in this container, where all
  egress goes through OneCLI.
- **Substitution** (`subst.rs`):
  - Injection is covered below under hardening.
  - Response headers and identity bodies are scrubbed with a streaming
    matcher that carries `maxlen-1` bytes across chunks.
  - `Content-Length` is dropped when the length changes.
- **The child's environment** (`Broker::child_env`):
  - `NAME=placeholder`
  - `HTTPS_PROXY`/`https_proxy` with the token
  - `NODE_USE_ENV_PROXY=1`
  - 8 CA-bundle variables, or only `NODE_EXTRA_CA_CERTS` when no base
    bundle is found (with a warning)
- **The model** gets a `[Credentials]` note in the system prompt: which
  names exist, for which hosts, and where the placeholders work.

**CLI wiring** (`ferrule-cli`):
- `[secrets]` accepts the list form or `{ hosts, in_url }`
  (`deny_unknown_fields`, so a misspelt `in_uri` is an error).
- The secret names join the sandbox's `secret_vars`, so the real variable
  is scrubbed before the placeholder is set. The sandbox gained
  `with_env`/`extra_env` for that.
- One broker is shared by `run`, `chat` and `gateway`.
- The warnings cover three cases: none of the secrets set, no active
  sandbox, and `network = false`.
- `ferrule sandbox` lists each secret with its hosts and "(URL too)". It
  adds two checks: that commands see only placeholders, and that
  `/proc/<ferrule>/environ` is unreadable.
- `ferrule sandbox -- CMD…` runs a command exactly as the agent's shell
  tool would.

**Hardening pass, the same day.** The first version swapped placeholders
anywhere in the request: headers, URL and body. That lets a hijacked model
get a *bound* host to store the real value where the model can read it
back:
- a GitHub contents path named after the token
- a Telegram `sendMessage` text
- a `Dropbox-API-Arg` JSON header

Injection is now only in `Authorization` (Bearer, and Basic
decoded/re-encoded) and in headers whose name contains auth, key, token,
secret, password, passwd, credential or cookie. The URL path and query are
swapped only for secrets marked `in_url = true`, and bodies never. The
fly.io tokenizer documents the same echo attack for its own design.

**A correction to what Max was told earlier.** A placeholder sent to an
*unbound* host is not blocked. It passes through the blind tunnel as a
useless string. Blocking it would mean intercepting every host, which
would break tools that pin their own roots, and it would protect nothing,
because the placeholder isn't the secret.

**Verification:**
- `cargo test --workspace` is 156/156. `ferrule-proxy` has 23 unit and
  4 integration tests. The integration tests run a TLS origin through the
  real proxy with reqwest:
  - Bearer, Basic and `x-api-key` arrive real.
  - A non-credential `x-title` header keeps the placeholder.
  - The path and an `in_url` query arrive real; a non-`in_url` query
    stays a placeholder.
  - A 200 KB response is scrubbed across chunk boundaries.
  - An unbound host gets a tunnel.
  - A wrong token gets 407, and a dead host 502.
- The ignored live test goes through this container's real upstream proxy
  to httpbin.org and passes. Run it with `cargo test -p ferrule-proxy --
  --ignored`.
- **CLI checks**, all with `ferrule sandbox -- sh -c …` against
  `https://httpbin.org/basic-auth/user/…` and `curl -u user:$DEMO_PASSWORD`:
  - List form: 401 when the placeholder is in the URL (the URL isn't
    swapped), 200 when the real value is in the URL (control).
  - `in_url = true`: 200 on both.
- `cat /proc/$PPID/environ` is denied under Landlock and readable without
  it (control).
- Clippy shows only the old `ferrule-core` warning. The new files were
  rustfmt'd one by one.
- Release binary: 9.3 MB (already stripped: `strip = true`, fat LTO). It
  links only libc, libm and libgcc_s. `ferrule --version` takes about 4
  ms. An idle `ferrule gateway` uses about 9 MB RSS.

**Docs:**
- New: `docs/research-credential-gateway.md`, covering design, threat
  model, prior art with primary sources (Deno Sandbox, fly.io tokenizer,
  Anthropic sandbox-runtime, Codex, httpjail, Cloudflare Sandboxes, GitHub
  Actions masking), limits and verification.
- **README rewritten** at Max's request (msg 3062). It covers install,
  quick start, configuration recipes, the sandbox, the gateway, layout and
  development, and a roadmap split into shipped / next / planned.
- Three hand-written SVGs in `docs/assets/`: `architecture.svg`,
  `credential-gateway.svg` and `roadmap.svg`. They use the steel/copper
  palette from the branding.
- `chart-architecture.png` is left in place but no longer referenced. It
  still says "AgentRust".

**History rewrite (msg 3064).** Max didn't want Claude listed as a
co-author. Every commit was rewritten to drop the `Co-Authored-By: Claude`
trailers and force-pushed, so M6 is now `8f30ca7`. **From now on commits
carry no Claude trailer**, and the author is
`maxim <max@lizo.ai>`.

**Known limits (documented, not fixed):**
- No body injection, so OAuth `client_secret` POSTs don't work.
- No HTTP/2 or websockets on bound hosts. Any port on a bound host is
  intercepted, not just 443.
- `web_fetch` and MCP servers bypass the proxy. MCP servers still get
  ferrule's real environment.
- A bound host that echoes a credential in another encoding (e.g. Basic
  credentials as base64 from httpbin's `/headers`), or compresses its
  response anyway, isn't scrubbed.
- macOS: Go tools such as `gh` ignore `SSL_CERT_FILE` and need the CA in
  the keychain. Seatbelt's protection of ferrule's environment is
  unverified until a real-Mac run.
- The Telegram channel has no sender allow-list (found while writing the
  README). Anyone who finds the bot drives an agent with a shell and
  bound credentials. This is the cheapest high-value fix left and needs no
  decision.

**What's next:** the Telegram allow-list, then whatever Max picks from the
decision-blocked list: Phase 1 routing, multi-agent orchestration, or
code-extension plugins.

### 2026-09-24 — Roadmap: Phases 2–3 dropped (Devi, Opus 5.5)

Max (msg 3070) dropped the learned router (Phase 2) and on-the-fly local
LoRA fine-tunes (Phase 3): too much for what they buy. Rule-based routing
(Phase 1) stays under "Next". Removed from the README's Planned list and
from `docs/assets/roadmap.svg`; the routing research doc keeps both
sections, marked as dropped, for reference.

### 2026-09-24 — M8 one-line install, setup wizard, Windows (Devi, Opus 5.5)

Max (msg 3068) wanted install to work like NanoClaw's: download, run
install, answer questions, done, with no env vars to export and an easy way
to change settings later. Then (msg 3072) Windows support as well.

**Setup wizard** (`ferrule-cli`: `setup.rs`, `probe.rs`, `secrets.rs`,
`service.rs`, `doctor.rs`; new deps `inquire`, `toml_edit`):
- `ferrule setup` runs five steps: provider and key, Telegram, tool
  credentials (`[secrets]`), sandbox, background service. On a later run
  it opens on a menu with a one-line summary of each, so one part can be
  changed alone. Config edits go through `toml_edit`, so hand-written
  comments survive.
- Keys are checked live before they're saved: the provider's model list
  (the wizard offers those models), Telegram `getMe`, and GitHub `/user`
  for `GITHUB_TOKEN`.
- **Saved keys** go to `<data_dir>/private/secrets.env` (0600 in a 0700
  dir), loaded into the environment at startup before any thread starts.
  A real env var wins, so `export` still overrides.
- **Telegram allow-list:** `[gateway] telegram_allowed_chats`. With an
  empty list the bot answers each new chat once with its id and forwards
  nothing. The wizard gets your chat id by asking you to message the bot.
- **Service:** a systemd user unit on Linux, a launchd agent
  (`ai.ferrule.gateway`) on macOS; install, restart, status, logs hint.
  None on Windows yet.
- `ferrule doctor [--offline]` checks config, keys (and where each comes
  from), Telegram, the sandbox (including that a sandboxed `cat` of the
  keys file fails) and the service, with a fix for each problem.
- `ferrule config path|edit|example|init`. `edit` re-parses after the
  editor closes and falls back to nano, vi, then notepad.

**Security fix found while testing:** when the workspace contains the data
dir (e.g. `--workspace ~`), the agent's `read_file`/`write_file` could read
the saved keys or plant a CA key. Both are now refused
(`fs_tools::resolve` checks the canonical path against the hidden paths:
`private/` and `proxy/keys/`, case-folded on macOS and Windows). The shell
sandbox hides the same paths: Seatbelt with `deny` rules after every
grant, Landlock by "carving" (it can't deny a subpath, so the read grant
on `/` is split into grants for each sibling along the way down to the
hidden dirs). Side effect on Linux: the dirs on that path become
read-only for commands, so the workspace top level can't take new files.
ferrule warns once and `ferrule sandbox` explains it. The simple fix is
to keep the workspace apart from the data dir, which the README now says.
`tests_e2e/hidden_keys.py` has a mock model try all three attacks:
LEAK/TAMPERED/PLANTED all False.

**Windows** (native, x86_64-pc-windows-msvc):
- `ferrule-sandbox::Shell` picks the agent's shell once: Git Bash
  (found via `git.exe` on PATH or the usual install dirs, never
  `System32\bash.exe`, which is WSL), else `pwsh`, else Windows
  PowerShell. PowerShell gets the script with `-EncodedCommand`
  (UTF-16LE base64), so no quoting is involved, and UTF-8 output. The
  shell tool's description tells the model which shell it has. The
  scheduler's gate scripts use the same shell.
- No OS sandbox: `Sandbox::new` reports degraded, the wizard says so
  plainly and recommends WSL2 for isolation. Env scrubbing and the
  file-tool refusals still apply.
- Config in `%APPDATA%\ferrule`, data in `%LOCALAPPDATA%\ferrule`.
- Verified by `cargo check --target x86_64-pc-windows-gnu --workspace
  --all-targets` (zero warnings). Not yet run on Windows: CI is the
  first run.

**Install scripts:**
- `install.sh` (POSIX sh; passes `dash -n`): picks the musl or darwin
  target (Rosetta-aware), downloads with curl or wget, verifies sha256,
  swaps the binary in with a rename, prints a PATH hint, restarts a
  running service, then runs `ferrule setup </dev/tty` on a first install
  (`curl | sh` has no stdin). With `GITHUB_TOKEN` it downloads through the
  API, for the private repo. Tested with a fake `curl` serving a local
  release: fresh, private, bad checksum, unknown version, upgrade keeping
  config, unsupported arch, wizard under a pty, `cat install.sh | dash`.
- `install.ps1`: everything in a function, so `irm | iex` doesn't change
  the caller's session and a failure doesn't close the window. Installs to
  `%LOCALAPPDATA%\Programs\ferrule`, renames a running `ferrule.exe`
  aside instead of failing, adds the dir to the user PATH (raw registry
  value, `ExpandString`) and broadcasts the change. Tested under pwsh 7.6
  on Linux with mocked downloads and registry: fresh, private, bad
  checksum, unknown version, x86, ARM64 upgrade. Not run on real Windows.

**CI and releases** (`.github/workflows/`):
- `ci.yml`: `cargo test --workspace --locked` on ubuntu-24.04, macos-14
  and windows-latest; `ferrule sandbox` self-test on Linux and macOS; the
  two `tests_e2e` scripts on Linux. Doc-only pushes are skipped.
- `release.yml`: on a `v*` tag, builds x86_64/aarch64 Linux (static
  musl), aarch64/x86_64 macOS and x86_64 Windows, packages
  `ferrule-<target>.tar.gz|zip` plus `.sha256`, and publishes them with
  the two install scripts. `workflow_dispatch` builds without releasing.
  Checked locally with zig as the musl C compiler: x86_64 musl binary is
  static-pie, 10.3 MB, and `ferrule sandbox` passes all checks on it.
- `Cargo.lock` is committed now (was gitignored) for `--locked` builds.

**Known limits:**
- No release is tagged yet: the one-liners 404 until one exists (Max's
  call). While the repo is private the one-liners need a token; the README
  shows how.
- No Windows sandbox and no Windows service. `ferrule gateway` in a
  terminal or Task Scheduler for now.
- The Landlock carve makes the workspace top level read-only when the
  workspace contains the data dir (see above).

**First CI runs** (after the M8 push; fixes in `30fd260`, `cee5de1`):
- **Linux:** green on the first run, including the sandbox self-test and
  both `tests_e2e` scripts.
- **macOS (macos-14, first run on a real Mac):** Seatbelt enforced
  everything: hidden paths, network off, read-only, env scrubbing, temp
  dirs. Two tests failed only on the message text: Seatbelt refuses with
  EPERM ("Operation not permitted"), Landlock with EACCES ("Permission
  denied"). `ferrule_sandbox::DENIED` now holds the right one per OS. The
  shell tool's note to the model quotes it too, so on a Mac the model is
  told the text it will actually see. `ferrule sandbox` passes all checks
  on the runner.
- **Windows (windows-latest, first run anywhere):** one real bug. The MCP
  `initialize` handshake used the per-call `timeout_secs`, so a server
  that is slow to start failed at connect. The test's 1 s timeout met a
  cold Python on the runner, and an `npx` server would hit the same thing
  in use. Startup (`initialize` + `tools/list`) now gets at least 60 s
  (`McpServerConfig::startup_timeout`), and `tools/call` keeps
  `timeout_secs`. Every other crate, sandbox and cli included, passed on
  Windows.
- `ci.yml` runs `cargo test --no-fail-fast`. Before, the Windows run
  stopped at the first failing crate and never tested the rest.
- **Release dry run** (`release.yml` by hand, nothing published): all 5
  targets build and print `ferrule 0.1.0`; the arm64 Linux runner works
  on the private repo. The x86_64 musl artifact, downloaded here, passes
  its sha256, is static-pie (10.6 MB, 4.4 MB as .tar.gz) and passes every
  `ferrule sandbox` check. The run showed one bug: the Package step's
  version check unpacked `ferrule` into `dist/`, so the raw binary was
  uploaded next to the archive, and in a release the four Unix targets'
  `ferrule` files would have collided. It now unpacks outside `dist/`
  (checked locally, not re-run on CI).
- Cost note: the repo is private, so Actions minutes are billed, with
  macOS at 10x and Windows at 2x the Linux rate.

### 2026-09-24 — Research: never stuck, self-extension, a browser, speed (Devi, Opus 5.5)

Max (msg 3066) asked how to make Ferrule smart and fast: an architecture
review, never getting stuck, its own browser, and installing skills and
plugins for itself. Written up in
`docs/research-autonomy-and-self-extension.md` (code refs to `f3cd7e0`).
The main findings:
- Nothing in the loop recovers. A provider error fails the turn (no
  retry anywhere), an MCP call gets one respawn, hitting `max_iterations`
  sends `internal error: …` to Telegram, and repeats go unnoticed.
  OpenHands' `StuckDetector` (five loop signatures) ports in about 100
  lines over data `Agent` already has.
- `verify_command` is only a sentence in the system prompt. It should be
  run by the runtime, with the failure fed back, as Claude Code's Stop
  hook does.
- Browser: `vercel-labs/agent-browser` (Rust, Apache-2.0, CDP) ships an
  MCP server, so it needs config, not code. Its proxy and domain flags fit
  the credential proxy.
- Self-install is the risky part: MCPTox measured 36.5% average attack
  success from poisoned tool descriptions. MCP servers run outside the
  sandbox and the proxy today, so M10 (confine them) goes before M12
  (self-install).
- Speed: tools run one after another and the provider doesn't stream.
  Both are loop fixes, measurable with the ledger's `latency_ms`.

### 2026-09-24 — v0.1.0 released, repo public (Devi, Opus 5.5)

Max said yes to both (msg 3074). Before the visibility flip, the whole
history (22 commits) was scanned for key shapes (OpenAI/Anthropic `sk-`,
`ghp_`/`github_pat_`, Slack, AWS, Telegram bot tokens, Google, PEM,
JWT) and for client data. The only hits were test fixtures (a fake bot id
`123456789:…`, dummy `ghp_…`). Tagged `v0.1.0` on `9ab612e`, and
`release.yml` published all 5 archives, their `.sha256` and both install
scripts. No stray raw binary this time. `curl -fsSL …/install.sh | sh`
run for real in a scratch HOME: downloads, verifies, installs
`ferrule 0.1.0`, and `ferrule sandbox` passes. README drops the
private-repo token instructions (the scripts still accept
`GITHUB_TOKEN`, for private forks).

### 2026-09-24 — M9 never stuck (Devi, Opus 5.5)

Five pieces, from `docs/research-autonomy-and-self-extension.md`:

- **Retry.** `CoreError::Transient { message, retry_after }` is what a
  provider returns when trying again can help: 408, 429, 5xx, a timeout or
  a dropped connection, an HTML error page, or an `error` object inside a
  200 whose code says rate limit/overloaded/unavailable
  (`openai_compat.rs`). `Agent::call_provider` retries it with capped,
  jittered exponential backoff, honouring `Retry-After`, up to
  `config.retry`. Every attempt gets a ledger row; the ones that led to a
  retry have outcome `"retried"`. Anything else (a 400, a bad key) fails at
  once.
- **Stuck detector** (`stuck.rs`, after OpenHands' `StuckDetector`): the
  same call with the same result 4 times, the same call failing 3 times,
  or two calls taking turns 6 times. The first time, the model is told what
  it's repeating and to change course; the second time, the run stops.
- **A status at every stop.** The step limit, a second loop, or a check
  that still fails after `max_verify_rounds` fixes no longer end in an
  error: `wrap_up` asks the model (call kind `"status"`) for a short status
  (what's done, what's left, what blocks it), with a fixed fallback if it
  calls tools anyway. `Agent::incomplete` carries the reason.
  `RunStatus::Incomplete` records it for scheduled tasks, the answer is
  still delivered, and `ferrule run` exits 2.
- **`verify_command` enforced.** `Verifier` (`verify.rs`) runs when a run
  that changed files tries to finish (`Tool::changes_files`, false for the
  read-only tools and for MCP tools marked `readOnlyHint`). A failure goes
  back to the model with the tail of the output. `CommandVerifier` in
  `ferrule-tools` runs the owner's command through the shell tool's
  sandbox, with `agent.verify_timeout_secs` (default 600).
- **Goal pinned through compaction.** If the request isn't in the kept
  tail anymore, the summary carries it verbatim.

**Bug found on the way: long runs hung the lane.** `Router::run_lane` gave
the agent an event channel of 64 and kept the receiver alive without
reading it (`_erx`). `Agent::emit` awaits the send, so the 65th event
blocked the run for good, and with it every later message in that chat or
task. Now the receiver is dropped, so sends fail fast and are ignored.
`a_long_run_does_not_stall_the_lane` fails on the old code (control run)
and passes on the new one. A failed run now also tells the chat why
(transient: try again in a few minutes).

Also: `docs/research-windows-sandbox.md` (research agent). Tier 1 needs no
admin: a restricted copy of the user's own token plus a capability SID on
the workspace ACL plus a Job Object, the way Codex's unelevated backend
and Chromium do it. Network blocking (WFP) or a dedicated account needs a
one-time elevated setup.

Tests: `cargo test --workspace` green (core 30, gateway 48, tools 17,
providers 5, the rest unchanged).

### 2026-09-24 — M10 MCP and web_fetch under the sandbox and the proxy, a system service, doctor (Devi, Opus 5.5)

**Step 0, review of the previous attempt (`2539d1a`, "sandbox MCP stdio
servers"). Verdict: keep the core, rework it.** The brief assumed it was
unpushed; it was already on `main`, so the rework landed on top rather
than replacing it. What it got right: MCP servers go through the same
`Sandbox::command` as the shell, env scrubbing, canonicalised roots, the
state dir under the data dir, and the proxy env inherited. What was
wrong:
1. The server's cwd was its state dir, and relative `writable_roots`
   resolved against it instead of the workspace.
2. In read-only mode the state dir itself wasn't writable, so npx/uvx
   servers couldn't start.
3. `network = false` was inherited by servers. The owner wants the
   network open, and most servers need it.
4. HOME was redirected even when unconfined; XDG dirs and TMPDIR weren't.
5. `npx` didn't resolve to `npx.cmd` on Windows.
6. `sandbox_degraded` wasn't wired to `ferrule doctor`.
7. The tests asserted a chained exit status (`… && echo ok`), used
   `/bin/sh` rather than an MCP server, and leaned on `Sandbox::off`.
8. State-dir names could collide (sanitised server names).
Caveat that stays: an unconfined server can read `/proc/<ppid>/environ`
and the secrets file.

**Part 1 (`a80f6da`).** Fixes 1–8: the workspace is the server's cwd and
writable; the state dir stays writable in read-only mode; servers always
get the network; HOME/XDG/npm/uv/TMPDIR move into the state dir only when
confined; `writable_roots` resolve against the workspace; `npx` →
`npx.cmd` on Windows; state-dir names carry a hash; doctor warns about
every `sandbox = false` server. `ferrule-mcp/tests/sandbox.rs` drives a
real MCP server under the real sandbox and asserts which files exist,
not an exit code.

**Part 2 (`c0643b0`, test follow-up `26cfb95`).** `web_fetch` and a new
Streamable HTTP transport for MCP (`url` + `headers`, `${VAR}` from
ferrule's env, a missing variable stops that server; session id kept,
404 → re-initialise once; JSON or SSE answers) build their client with
`ferrule_tools::egress::client_builder`: HTTPS through the credential
proxy, trusting its CA, when the proxy runs. Without it they connect
directly (or via the system proxy), and doctor's new `proxy` line says
which. **Decision: plain HTTP is not proxied.** The proxy speaks CONNECT
only, and injecting into cleartext would put keys on the wire anyway, so
`HTTP_PROXY` is left alone. `ferrule-proxy/tests/egress.rs` checks
end to end that the real token reaches the origin only via the proxy
and that a direct `web_fetch` fails TLS against it. The follow-up keeps
the direct-path test's loopback origin off an ambient `HTTP_PROXY`
(it failed in this sandbox without `NO_PROXY`; CI has none).

**Part 3: dropped by Max (msg 3088), not implemented.**

**Part 4 (`98d998f`), system service.** `ferrule setup` as root on Linux
defaults to a system unit; `--system` / `--user` choose explicitly
(`decide_scope`, unit-tested: both flags, non-Linux and non-root are
errors). It creates (`useradd --system`, nologin) or reuses a `ferrule`
account; reuse is refused for uid 0, a login shell, or membership in
root/sudo/wheel/admin/adm/docker/lxd. Config is `/etc/ferrule/config.toml`
(root:ferrule 0640), data `/var/lib/ferrule/data`, workspace
`/var/lib/ferrule/workspace`, both owned by it. The unit has `User=`,
`NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome=yes`,
`PrivateTmp=yes`, `ReadWritePaths=` data and workspace only. Paths
under /home, /root or /run/user and system dirs as a workspace are
refused before anything is written. A user unit as root needs a loud
confirmation; doctor warns about one and points to `--system`.
`install.sh` as root installs to /usr/local/bin and restarts an active
system unit on upgrade. `data_dir()` honours `FERRULE_DATA_DIR`. **Not
e2e-tested:** it needs root plus systemd, and the wizard is interactive;
CI's runner could do it with sudo, left as an open edge.

**Part 5 (`085c3e7`), doctor.** (a) `browser.rs`: `$CHROME_PATH`, then
the PATH names (google-chrome-stable, google-chrome, chromium,
chromium-browser), the .deb/snap/distro paths, the macOS app bundles in
/Applications and ~/Applications, and Program Files / LOCALAPPDATA on
Windows. The one found gets a headless `--dump-dom about:blank` run in
a throwaway profile, 20 s timeout, `--no-sandbox` only as root.
Informational only, never downloads. Here it finds `/usr/bin/chromium`,
which fails with "No usable sandbox!", and (b) the next line explains
it: `kernel.apparmor_restrict_unprivileged_userns = 1`. (c) The service
warns "binary upgraded since it started" when the unit's `ExecStart`
binary is newer than the running process (`ps -o etime`, ±2 s) or
`/proc/<pid>/exe` is deleted, with the right restart command. (d) was
already covered by parts 1 and 2 (the `mcp` and `proxy` lines).

**Research doc fact-check (`34b5390`)** of
`docs/research-deployment-and-isolation.md` against `b5ddfa1`:
1. The seccomp citation `lib.rs:340-346` → arch check `lib.rs:373-374`,
   filter `linux.rs:263-271, 310, 315-395`.
2. The network default `lib.rs:105` → field `:65-66`, default `:94`.
3. Scrubbing `lib.rs:128, 254-278` → `SECRET_MARKERS` `:125`,
   `scrubbed_vars`/`is_secret_var` `:299-318`, removal `:237-239`.
4. `router.rs` holds no workspace: lanes are `spawn_lane`
   (`router.rs:108-120`), and the workspace comes from the agent factory
   (`main.rs:675-686`).
5. The sandbox module doc doesn't "explicitly" rule out containers.
6. The "running service is restarted" quote is `README.md:108-109`, not
   install.sh.
7. Not only `shell` is sandboxed: the command verifier and `ferrule
   sandbox -- cmd` are too; scheduler gate scripts aren't.
8. Hidden paths: names in a hidden dir stay listable on Linux, later
   entries and bind mounts aren't covered, missing paths are skipped;
   Seatbelt denies both, untested on a real Mac.
9. ReadOnly also allows `/dev/ptmx`/`/dev/ttys*` on macOS and drops
   `write_file`.
10. The proxy's `in_url` substitution was missing.
11. Scrubbing is case-insensitive, plus `secret_vars` and the
    `env_passthrough` exemptions.
12. install.sh runs setup only on a fresh install with a terminal.
13. Ubuntu 24.04's GA kernel is 6.8 (was "unverified"). Its claim about
    the default userns restriction was left as is.
A header note records what M10 has changed since.

Also `b5fd138`, `cargo fmt` of the whole workspace, which had never been
formatted, so the M10 diffs stay readable. fmt, clippy `-D warnings` and
`cargo test --workspace` are clean: 232 passed, 2 ignored.
`tests_e2e/hidden_keys.py` passes locally; `setup_wizard.py` needs
pexpect, so CI runs it.

**Left:** a system-service e2e run on CI, plain-HTTP proxying (would
need a forward HTTP mode), the Streamable HTTP GET stream, Chrome checks
on macOS and Windows runners, and the unconfined-server caveat above.
Next: M11 browser, M12 multi-agent, M13 self-extension (msg 3090).
### 2026-09-24 — README sales rewrite (Kimi Code)

Rewrote `README.md` around the approved sales arc: hero + badges (CI,
release v0.1.0, platforms, ~10 MB binary, 180 tests), a three-punch pitch
(harness, security, ease), a "60 seconds to your first agent run" section,
"Why ferrule wins" (ARC-AGI-3 13.3% → 38.3% + the harness story: profiles,
structured compaction, verify-as-judge, never-stuck), "Secure by default"
(the four layers), then the tightened reference sections. Nav links match
the new anchors; the private-repo `GITHUB_TOKEN` install notes, the
Windows no-sandbox honesty and the truthful roadmap are kept.

New images, all under `docs/assets/`:

- `why-ferrule-wins.svg`, `security-layers.svg`, `quickstart-flow.svg` —
  hand-written in the existing brand style (same palette, fonts and box
  language as `architecture.svg`); each validated as XML.
- `term-help.png`, `term-doctor.png`, `term-sandbox.png` — real output of
  the release binary (`cargo build --release -p ferrule-cli`, rustc
  1.98.1), run in a scratch HOME (`/tmp/ferrule-demo`) so no personal
  config, keys or skill lists leak into the shots. `ferrule doctor`'s
  provider row is green against a tiny local mock `/v1/models` server (the
  machine's real Ollama answers `data:null`, which doctor correctly flags
  as an unexpected shape). Rendering: kimi-cu screenshots of Terminal.app
  were tried first, but this Mac's Terminal default profile is white
  80×24 and stamps the owner's name/hostname in the title bar, so the real
  captured output was rendered into dark terminal-styled PNGs with Pillow
  (Menlo, brand colours) — the sanctioned fallback. No `run`/`chat`/
  `gateway`/`setup` was executed (no model tokens spent).
- `docs/branding/hero.png` unchanged: the AI regeneration was attempted
  per the brandkit skill, but no working image-generation backend exists
  on this machine — the `gemini` CLI's individual tier was retired by
  Google (`IneligibleTierError`), there are no image API keys in the
  environment and no image-gen CLIs installed. Keeping the current
  on-brand hero rather than shipping an off-brand replacement.

Verified: all referenced images exist, all relative links resolve, every
quoted `ferrule` command matches the built binary's `--help`, all SVGs
parse as XML, and the README re-read end to end against the honesty
constraints. Nothing committed; `target/` stays untracked.

### 2026-09-24 — README refreshed for M9/M10 (Kimi Code)

Rebased the README sales rewrite onto M10 (`843cd47`) and brought the
README in line with what actually shipped. Text changes: test badge and
Development counts 180 → 232 (229 on macOS — the Linux-only tests are
cfg'd out; verified with `cargo test --workspace` on this Mac: 229 passed,
0 failed); the MCP row and Sandbox section now cover sandboxed stdio
servers and Streamable HTTP servers through the credential proxy; the
credential-gateway "only the shell tool" limit is corrected (`web_fetch`
and remote MCP go through it too, HTTPS only); the "please don't cargo
fmt" note is replaced (the tree was fmt'd in `b5fd138`); the roadmap gains
M9 and M10 under Shipped, and Next gains M11 browser / M12 multi-agent /
M13 self-extension while the "sandbox MCP servers" open edge is removed
(file reads remain); the Docs list covers all six research docs.
`docs/assets/roadmap.svg` redrawn to match (M9/M10 shipped, new Next
list, taller canvas); `architecture.svg` MCP and sandbox captions updated.
The `term-*.png` captures were re-captured from a release build of the M10
code, same method as before (scratch HOME, mock `/v1/models`, Pillow
renderer). doctor gained `mcp`/`proxy`/`browser` lines and sandbox the
`[secrets]`/proxy rows, so those two PNGs were re-rendered; `--help`
output was unchanged, so `term-help.png` is byte-identical and was kept.

### 2026-09-24 — Research: path to the #1 agent harness (Kimi Code)

Max asked what ferrule needs beyond the current plan to become the agent
runtime developers choose over Hermes, OpenClaw and NanoClaw — easiest to
use, easiest to extend, smartest. Six parallel investigations (Hermes/
OpenClaw/NanoClaw deep-dive; Claude Code/Codex/OpenHands/Goose/Aider/
Letta techniques; community adoption drivers; full PLAN.md + research-doc
gap catalog; ferrule code-level architecture analysis; 2025–2026 research
frontier). Synthesis: `docs/research-number-one-harness-strategy.md`.

Headline findings: ferrule already owns the top two adoption drivers
(credential safety, harness reliability) and the answers to the top
anti-drivers (never-stuck, verify, sandbox); the gaps are breadth
(channels 2 vs 13–20+, no extension economy, no multi-agent), proof
(private repo, no published measurements), and one missing signature
feature. Genuinely new work surfaced (not in the roadmap): **the learning
loop** (offline memory consolidation + ACE-style playbook — Hermes/
OpenClaw/Codex/Letta all ship a form of it; M13 self-*extension* does not
cover it), a **`ferrule eval` primitive** (makes "smartest harness"
measurable), **reversible compaction** (`search_history` over the
transcript), a **memory update pipeline** (update/delete, goal-driven
recall), **MCP hot-add + `ferrule mcp add`**, **lifecycle hooks**, MCP
image content (hard blocker for M11), an M13 vetting story, `web_search`,
budget caps, destructive-action approval gates, plan mode, Aider-style
edit mechanics, local-model first-run polish, migration importers.
Planned work to sharpen: pull forward parallel tool calls and streaming;
M12 design deltas (summary contract, effort-scaling, verifier role,
routing-by-role, agent-relayed approvals untrusted); channels sequenced
Discord → Slack → WhatsApp. Decisions flagged for Max: go public; curated
never-open registry; positioning vs ZeroClaw; honest launch. Do-not-build
list recorded in the doc.

### 2026-09-24 — M14–M19 adopted into the plan (Kimi Code)

Max adopted the strategy synthesis into the roadmap. PLAN.md "Current
State → Next milestones" now carries the full track: M11 browser (with
the MCP-image-content prerequisite called out), M12 multi-agent (with
the strategy's design deltas: summary contract, effort-scaling, verifier
role, routing-by-role, resume/wait/close tools, task dependencies,
agent-relayed approvals untrusted), M13 self-extension (with the vetting
story: poisoning scan, version pinning, `tools/list_changed` re-scan),
then the new milestones — M14 `ferrule eval`, M15 memory update pipeline
+ reversible compaction (`search_history`), M16 the learning loop
(offline consolidation + ACE-style playbook), M17 MCP hot-add +
`ferrule mcp add`, M18 lifecycle hooks, M19 trust & cost (budget caps,
destructive-action approvals, plan mode). The README roadmap and
`docs/assets/roadmap.svg` were updated to match (routing, Codex/Claude
drivers, file-read sandboxing, Windows sandbox and code plugins move to
"designed, waiting on a decision" / Planned; the strategy backlog is
referenced from Planned). Committed and pushed so the next session works
from this plan.

### 2026-09-24 — M11 browser closed: macOS and Windows (Devi, Opus 5.5)

Part 1 (b41149a) shipped the browser on Linux; part 2 (b420023) and part 3
(580f330) made the same real-Chrome test pass on macos-14 and
windows-latest. Root causes, each found on a throwaway probe branch
(`m11-probe`, `m11-probe2`, since deleted) running the browser test alone:
- **macOS, "Failed to get the path for 1001".** Chrome finds
  `~/Library/Application Support` through CoreFoundation, which ignores
  `HOME`. Seatbelt blocked the real home, so Chrome died at start. Fix:
  `CFFIXED_USER_HOME` = the server's state dir when sandboxed. Part 2 had
  already opened the desktop services Chrome needs (window server, fonts,
  pasteboard lookups), moved agent-browser's config file out of the
  writable state dir, and recorded Chrome's own sandbox as a blocker under
  Seatbelt (`--no-sandbox` there).
- **Windows, the first tool call hung forever.** In agent-browser 0.38.1,
  `mcp.rs::run_cli` spawns the CLI with piped stdout/stderr and reads to
  EOF; the CLI's `ensure_daemon` spawns the daemon detached but with
  inherited handles, so the daemon keeps the pipe open and EOF never
  comes. Starting the daemon directly doesn't work (the CLI writes the
  config fingerprint the daemon checks). Fix: a warm-up — the browser's
  server config carries `warm_up = ["get", "url"]` on Windows only; the
  MCP client runs it with null stdio and the server's env before every
  call, so the daemon already exists (and is restarted after its idle
  timeout) when the MCP server's CLI runs. Not reachable from user config.
  Separately, `CI` and `AGENT_BROWSER_*` are removed from the env, since
  agent-browser adds `--no-sandbox` under `CI`, and that hangs Chrome on
  Windows.
- **Image content** was silently dropped; it's now named in the tool
  result (`[image/png content left out: …]`), so the model knows a
  screenshot exists. Passing it to the model stays an open edge — the
  strategy doc called it a hard blocker for M11; it isn't one for the
  done criteria (a real Chrome driven in CI on three OSes), and real
  support needs a provider-wide change.
Probe note: bash `timeout` couldn't kill the Windows process tree (a run
hung 50 min before being cancelled); the probe switched to a Python
timeout plus `taskkill /T`. Decisions for Max: keep the Windows warm-up or
file upstream and wait; image support across providers; the macOS
Chrome-sandbox-off tradeoff.

### 2026-09-24 — M12 multi-agent (Devi, Opus 5.5)

New crate `crates/ferrule-agents`; the user-facing doc is
`docs/agents.md`, the design is `docs/m12-multi-agent.md`. Branch
`m12-multi-agent`, PR #1. Every part was checked locally on Linux (fmt,
clippy `-D warnings`, `cargo test --workspace --locked`); **CI was
deferred at Max's request (24.09 17:39)** — a full 3-OS pass runs after
the roadmap batch.

- `538136e` design — the strategy deltas adopted: summary contract on
  reports, effort-scaling in the spawn description, verifier role,
  routing by role, `resume_agent`/`wait_agent`/`close_agent`, a task list
  with dependencies, agent-relayed approvals untrusted.
- `29c3a68` part 1: core hooks — a shared budget, an inbox drained before
  each model call, a cooperative stop flag.
- `0944a4a` part 2: the supervisor — spawn, wait, resume, close, list;
  depth/children/agents/token limits over a trailing window; reports
  fenced as untrusted; notices that wake an idle root.
- `8026940` part 3: the board (posts and direct messages, fenced) and the
  task list (dependencies, claims that flow down the tree and return when
  an agent closes or fails).
- `be44795` part 4: a child on its parent's repo gets a worktree and a
  `ferrule/<id>` branch; close commits leftovers and keeps the branch only
  if it holds work; a verifier checks a throwaway snapshot (HEAD + diff +
  untracked files).
- `1e17df8` part 5: the CLI — `[agents]` config and role providers,
  children built like any agent and only narrowed, `ferrule run` waits for
  its tree and is re-run on reports, gateway chats woken through the
  router, owner lock files so several processes on one data dir leave
  each other's agents alone, `ferrule agents list|close`, a doctor check,
  an e2e test driving the real binary against a scripted model.

Measured tool cost: depth 0 → 11 tools, ~5.1k chars with the prompt
addendum (~1.3k tokens); at max depth → 6 tools, ~2.6k chars (~0.7k
tokens); a child's "you are agent…" addendum ~450 chars.

**Deferred — part 6, named long-lived agents.** Open question for Max:
how a chat addresses a named agent (a `/agent <name>` prefix per
message, a sticky switch per chat, or one Telegram bot/chat per agent),
and whether a named agent's memory is a separate store or a scope in the
shared one.

**M12 open edges:**
- `enabled = true` is the default, with a 2M-token/24h tree budget —
  Max to confirm both.
- Under the gateway, a scheduled task's children outlive the task's run;
  their reports reach the task's next run. `tasks run-now` closes them.
- Every session now gets a root row in `agents.db`, even if it never
  spawns.
- A verifier snapshot copies untracked files but applies the tracked diff
  through git, so a repo `clean` filter can still run in the snapshot.
- Without an OS sandbox (mode off, or Windows today) a read-only
  verifier's "read-only" rests on its tool set and prompt only.
- MCP servers are shared per process and run in the root's workspace.

**Unverified on macOS/Windows until the batch CI pass:**
- Owner locks: `File::try_lock` semantics (flock vs `LockFileEx`), and
  the startup sweep deleting another process's lock file (Windows refuses
  to delete a file that is open).
- `Sandbox::for_child`: the read-only child and the extra writable git
  directory under Seatbelt; with no sandbox on Windows.
- Worktree git calls: path handling, `core.hooksPath` with Windows paths,
  `git apply` of CRLF patches into the verifier snapshot, removing a
  worktree while Windows holds files in it open.
- Snapshot copying of untracked files and its symlink checks.
- `canonicalize` yielding `\\?\` paths on Windows, which the workspace
  comparisons and the git-common-dir `starts_with` check rely on.
- The e2e test's environment isolation (`APPDATA`/`LOCALAPPDATA`/
  `USERPROFILE` on Windows, `dirs` resolution on macOS).
- `ferrule doctor`'s git check.

### 2026-09-24 — M13 self-extension (Devi, Opus 5.5)

Built on `m13-self-extension` (cut from main at 9c46174), in parallel
with M12 in another worktree; the two are reconciled later. Design first
(146f19c, `docs/m13-self-extension.md`), then seven parts:
- **Part 1 (8fde9b8)** — `ToolSource` and a dynamic layer in
  `ToolRegistry`: attached sources are re-queried on every provider
  request, static tools shadow dynamic ones, so a tool added mid-run is
  offered on the very next request.
- **Part 2 (f8214d9)** — MCP client: `notifications/tools/list_changed`
  is surfaced, `shutdown()` kills and waits, tools are built from a
  fresh `tools/list`.
- **Part 3 (e2ac392)** — live skills: `SkillsHandle` rediscovers on
  refresh and `LiveSkillTools` serves `activate_skill`/`read_skill_file`
  over the current set.
- **Part 4 (e549313)** — `ferrule-extensions`: the publisher-level
  allow-list (exact pins; registry-wide entries refused at load) and the
  deterministic description scan (block/warn rules, invisible-character
  normalisation, owner-only excerpts, key-order-independent digests).
- **Part 5 (6b71796)** — `extensions.lock.json` (atomic write under a
  `create_new` `.lk` file), git sources pinned to one commit and verified
  (HEAD and a clean tree) at every load, git server commands confined to
  their checkout, the private pending queue, skill directories inspected
  and copied without following symlinks, bundled text scanned.
- **Part 6 (8b446d3)** — `ExtensionManager` and the model's six tools
  (`mcp_add`, `mcp_remove`, `skill_install`, `skill_remove`,
  `skill_keep`, `extensions_list`); the six required hermetic end-to-end
  tests plus three more in `crates/ferrule-extensions/tests/self_extension.rs`.
- **Part 7 (212a0ca)** — CLI wiring: one manager per process runs every
  configured MCP server too (scanned, `list_changed` re-scanned) and
  follows the lock every 2 s; `[extensions] enabled` gates only the six
  tools; `ferrule chat` asks inline at a terminal; `ferrule extensions
  list|pending|approve|deny|remove|resume` (approve/resume need a TTY; a
  block hit needs the typed word `waive`).
The hook M17 expects is in place: `ExtensionManager::add_server` plus the
`list_changed` re-scan. Nothing of M17's own scope was built.

**Reconciled with M12** (merge of main a86afe9 into `m13-self-extension`).
**Sub-agents and extensions — the policy:** a child agent (M12) never
gets the model's six extension tools (`mcp_add`, `mcp_remove`,
`skill_install`, `skill_remove`, `skill_keep`, `extensions_list`), whatever
`[extensions] enabled` says; only the top-level agent can install, remove
or keep anything, and approving stays the owner's (CLI or the root's
terminal). A child may use servers and skills that are already
installed, narrowed by its role like any other tool: a verifier or
read-only child sees only the installed MCP tools that change nothing,
re-filtered on every request so a server installed mid-run is narrowed
too. Enforced in `self_extend::Extensions::attach` (`Reach::Root` /
`Reach::Child`), tested by `a_child_never_gets_the_extension_tools` and
`a_reading_child_sees_only_installed_tools_that_change_nothing`. Open
edge: a child's skill set is the root's (discovered from the root's
workspace), not its worktree's.

**Decisions for Max** (each has a default, so nothing waited):
- the allow-list is **publisher-level** (npm scope, git org, URL prefix),
  with exact pins required and registry-wide entries refused — narrow
  it to package-only if preferred;
- **updates are manual** — a reinstall with a new pin through the same
  allow-list, approval and scan; nothing polls registries;
- **`enabled = false` by default** — the six tool definitions cost tokens
  every turn and self-extension should be the owner's choice.

**Open edges — macOS/Windows**, each **unverified on macOS/Windows until
the batch CI pass** (everything above was tested on Linux only):
- spawning `git` and its paths, including `GIT_CONFIG_GLOBAL=NUL` on
  Windows;
- atomic rename/persist of the lock and queue files over an existing
  file;
- the `create_new` `.lk` lock file and its stale takeover;
- the TTY check (`is_terminal`) for `approve`/`resume` and the inline
  chat approver on Windows consoles;
- resolving `npx`/`uvx` (and `node`/`python3`) via `PATHEXT` on Windows;
- `canonicalize` returning `\\?\` prefixes in the checkout-confinement
  check (`inside()`) and `skill_dir_in`;
- symlink handling when inspecting and copying skill directories
  (Windows symlinks and junctions);
- 0700 permissions on `<data>/private/` (no-op on Windows);
- killing an MCP server process tree on shutdown/suspend;
- the hermetic tests' `python3` MCP fixture on Windows;
- `test -f` in the `skill_keep` test's check command;
- `replace_dir` renaming over an existing directory on Windows.

**Other open edges:** no channel (Telegram/WhatsApp) approver — the
`Approver` trait is there, only the TTY one is built; the gateway's
skill discovery roots are fixed at startup; with `sandbox = off` and on
Windows the agent's shell can write `<data>` and forge a lock entry, so
the approval gate is only as strong as the sandbox; the scan is
heuristic and a paraphrased attack can pass; updates are manual only;
no signature/provenance checks.

### 2026-09-24 — M14 `ferrule eval` (Devi, Opus 5.5)

New crate: `crates/ferrule-eval`. There's a CLI command, `ferrule eval run|report`, and a
starter suite in `evals/starter`. The user doc is `docs/eval.md`, which opens with "Run
the A/B in 5 minutes". The design is `docs/m14-eval.md`. The work is on branch
`m14-eval`, against `main`.

Every part was checked locally on Linux: fmt, clippy `-D warnings`, and
`cargo test --workspace`. **CI was deferred at Max's request**; a full 3-OS pass runs
after the roadmap batch. No real model was called: everything ran against mocks.

**Commits:**
- `66620e3` design: the suite format, fixtures, graders, ledger rows, the naive and
  engineered knobs, the reports, and the cost guards.
- `10200b0` part 1, core hooks:
  - a truncation path for the naive harness. It drops the oldest messages, keeps the
    system prompt and the last message, and leaves no orphaned tool results
  - a switch to turn off the stuck detector
  - a `Truncated` event
  - an eval tag on ledger rows
- `0917921` part 2, the crate:
  - suite loading and fixtures
  - the two variants
  - the command grader
  - eval-tagged ledger rows under a budget cap
  - the A/B report and the dry-run plan
- `ab2a5c0` part 3, the CLI and the starter suite:
  - `ferrule eval run`
  - 20 tasks with hidden graders and reference solutions, tagged smoke, context,
    verify-to-fix, code and data
  - the mock model
  - an oracle test, a real-binary A/B, and tests of the budget stop and the dry run
- `66e6f4e` part 4, the diff against the last run:
  - what changed since the last saved run of the same suite, kind and model: pass
    rate, tokens, cost, and tasks newly failing or passing, flagged when the task
    itself changed
  - `ferrule eval report`
  - regression suites gated on the engineered variant only
- `ce1ccfc` part 5, the LLM-rubric grader:
  - The judge sees the evidence: files that changed, were added or were deleted,
    plus the command grader's output.
  - The agent's answer is fenced off as claims.
  - ferrule computes each criterion. A criterion counts as met only when the judge's
    quote (at least 4 chars, whitespace collapsed) is found in the evidence.
  - `--judge-provider` picks a separate judge. When the model under test judges its
    own work, the report flags it as self-judged.
  - Judge calls go on the ledger as `call_kind = "judge"`. They are priced at the
    judge's rates and counted in the budget and the dry run.
  - Run ids now go to the millisecond.

**Measured on the mock** (the real binary, smoke subset):
- engineered 4/4, naive 2/4 (+50 pts)
- naive fails `release-notes` (the request was truncated away) and `slugify` (it
  never sees the failed check)
- full suite: engineered 20/20, naive 11/20 (+45 pts), $0.63 vs $0.45 at the mock's
  test prices

These numbers show the machinery works. They are not a measurement of a real model.
The Ollama small-window run and its README chart are still to do.

**Design defaults for Max to confirm:**
- The naive variant compacts at a threshold of 1.0, so it truncates only when the
  window is full.
- The default budget cap is $5 / 20M tokens per run.
- Exit codes: 0 passed, 1 failed, 3 stopped by the budget.
- The starter suite runs at a 32k context window.
- "What changed since the last run" is read from each run's `run.json`, not from
  the ledger (the roadmap said "from the ledger"). The ledger still holds every
  call.
- A diff matches on suite, kind and model.
- A regression suite fails only on engineered failures.
- The judge defaults to the model under test (flagged as self-judged).
- Evidence caps: 12k chars per file and 60k in total.
- Judge calls count toward the task's totals.

**M14 open edges:**
- The measured A/B on a real small local model (Ollama at 8–32k), and the chart for
  the README.
- The rubric grader has no starter-suite task. The starter suite grades with
  commands only; the rubric is covered by the crate tests.
- The task fingerprint used for "task changed" covers the suite.toml entry and the
  fixture files, not the grader scripts.

**Unverified on macOS/Windows until the batch CI pass:**
- The starter suite's `python3` graders, checks and solve scripts, and the mock model
  (`python3` vs `py` on Windows, CRLF).
- Grader commands run through the platform shell.
- The grader timeout killing the process tree.
- Git fixtures created with `-c core.hooksPath=/dev/null`, since there's no
  `/dev/null` on Windows.
- Workspace temp paths and `canonicalize`: `/private/var` on macOS, `\\?\` on Windows.
- Workspace cleanup while Windows holds files open.
- Both variants and the graders under the Seatbelt sandbox; with no sandbox on
  Windows.
- `OLLAMA_CONTEXT_LENGTH` for the Ollama app on macOS (`launchctl setenv`) and on
  Windows.
- The rubric evidence's relative paths (separators) and the `.git` skip.
- The saved-run directory under `~/Library/Application Support` and `%APPDATA%`.
- The CLI tests' environment isolation.

### 2026-09-24 — M15 memory update pipeline + reversible compaction (Devi, Opus 5.5)

The design is `docs/m15-memory.md`. The work is on branch `m15-memory`, cut from
`main` at `2f1045f`, with a PR to `main` (not merged). M17 was being built at the same
time on `m17-mcp-add`; this branch doesn't build on it.

Every part was checked locally on Linux: fmt, clippy `-D warnings`, and
`cargo test --workspace` (387 tests). **CI was deferred at Max's request**; a full
3-OS pass runs after the roadmap batch. No real model was called: everything ran
against mocks.

**Commits:**
- `fb5cd0f` design: the insert decision, `superseded_by`, what `forget` deletes,
  goal-driven recall, the transcript behind `search_history`, the ref format, the
  migration, the sub-agent policy, failure modes and tests.
- `a93fa3b` part 1, `ferrule-memory`:
  - `insert` decides NOOP (normalized text equal, or word-set Jaccard ≥ 0.9 with a
    live fact), ADD, or UPDATE (with `replaces`), and reports similar live facts
    (Jaccard ≥ 0.3)
  - `superseded_by` chains; recall searches every row and returns each hit's live
    head, so the old wording finds the correction
  - `forget` deletes a live fact with its whole chain (or one old version, re-linking
    the chain), then merges the FTS index and truncates the WAL, with
    `secure_delete` on
  - `assemble_for_goal`: goal matches first, then the newest live facts, as
    `- #id fact` lines
  - `PRAGMA user_version` 0 → 1, in one `BEGIN IMMEDIATE`, idempotent; a newer
    schema is refused. Tested against a store written by pre-M15 ferrule (a committed
    fixture) and a half-migrated one
- `c02acf5` part 2, `ferrule-core`:
  - when compaction triggers, old tool results over 4,000 chars (outside the verbatim
    tail, no skill block) are replaced in memory by a 600-char preview and a
    content-addressed ref; the summary is skipped when that was enough, and a summary
    lists the refs it folds
  - `search_history` over the session's own JSONL transcript, by query or by ref
    (paged), streamed, with over-long lines skipped
  - the `SessionRecall` hook fills the system prompt once, on the first run, from
    the session's first user message plus the request
  - `ToolResultsShortened` event; `Truncate` (the naive baseline) untouched
- `736f5f9` part 3, the CLI:
  - `update_memory`, `forget` and `remember {replaces}` for the root; `remember`
    without `replaces` for writing children (refused even if sent); `recall` only for
    read-only children
  - `search_history` on every agent with a transcript, and in eval's engineered
    variant
  - the static newest-facts block replaced by the goal-driven hook
  - `ferrule memory forget <id>`
  - binary tests: a fact stored in one `ferrule run`, corrected via `update_memory`
    in a second (by the id its prompt showed), and the third session's prompt holds
    only the correction; a read-only verifier child has `recall` and
    `search_history` and no memory writes

**Measured on the mock** (the real binary, all 20 tasks, `--variant ab`):
- engineered 20/20, naive 11/20 (+45 pts), the same verdicts as before M15
- engineered input tokens 610.5k → 513.5k, model calls 95 → 88, summaries 20 → 13;
  shortening fired on the five context tasks, and on `config-migration` it replaced
  all three summaries. Naive is unchanged to the token
- the mock never calls `search_history` (it replays scripted solutions), so this
  measures the shortening, not the fetch-back

**Design defaults for Max to confirm:**
- NOOP and "similar" are heuristics (Jaccard 0.9 / 0.3 over content words); UPDATE
  and DELETE are always the model's call, never automatic.
- `forget` is a hard delete of the whole chain, and root-only.
- Writing children can add memories, not correct or delete them.
- The memory block keeps the 2,000-char budget: up to 10 goal matches, then up to 5
  newest facts.
- Shortening: over 4,000 chars, 600-char preview, only at the compaction trigger, only
  with a transcript; up to 20 refs listed in a summary.
- `search_history` reads only the agent's own session.

**M15 open edges:**
- No per-turn re-recall on long sessions (it would change the prompt mid-session).
- Word-set heuristics, not embeddings; M16's consolidation is the backstop.
- `search_history` is a linear scan; slow on huge transcripts, but bounded.
- The eval engineered variant has no session recall (its store is fresh per run).
- A flaky M13 test seen once under load:
  `ferrule-extensions::self_extension::a_list_changed_that_introduces_a_poisoned_tool_is_caught`
  (an `eventually` timing check); passed 3/3 alone and in the full rerun.

**Unverified on macOS/Windows until the batch CI pass:**
- Transcript paths and reading them back (`<data>/sessions/<id>.jsonl`), CRLF in
  JSONL lines, and reading a file another process is appending to (Windows sharing
  modes).
- SQLite `secure_delete`, `wal_checkpoint(TRUNCATE)` and the "no trace in the file"
  test on APFS and NTFS.
- `busy_timeout` and `BEGIN IMMEDIATE` across processes (file locking differs).
- The pre-M15 fixture migration on macOS/Windows (the fixture is a binary file; check
  it isn't altered by git's line-ending settings).
- The CLI binary tests (`tests/memory.rs`): environment isolation, the data dir
  under `~/Library/Application Support` and `%APPDATA%`, and a verifier child with the
  sandbox off.
- `spawn_blocking` store access in the gateway on Windows.

### 2026-09-24 — M17 MCP hot-add + `ferrule mcp add` (Devi, Opus 5.5)

Branch `m17-mcp-add`, cut from main at 2f1045f. Design first
(`docs/m17-mcp-add.md`), then four parts:

- **Part 1:** `enabled_tools`, `max_output_chars` and `output_caps` on
  `[[mcp.servers]]`.
  - Tools left out are filtered before the scan, so they are never scanned,
    offered or called.
  - Caps only ever tighten the session's own cap.
  - The manager gains `probe` (start, list, scan, stop; registers nothing),
    `set_configured` (start, restart and stop configured servers from a
    re-read config) and a swappable sandbox for servers started later.
  - A mid-session `list_changed` on a configured server is re-scanned. A new
    poisoned tool is not activated, and neither is a tool outside
    `enabled_tools`.
- **Part 2:** `config_follow`. A running gateway, `chat`, `run` or `eval`
  polls its config every 2 s (mtime + length).
  - On a change it re-parses the file. A broken file keeps what runs.
  - New `[secrets]` are bound into the live proxy through `Broker::bind`. If
    no proxy ran, a late one starts, leaked for the process's lifetime.
  - Servers started from then on get the proxy's env.
  - `[[mcp.servers]]` goes to `set_configured`.
  - Only a trusted file is followed: `--config`, `$FERRULE_CONFIG` or the
    global config. A `./ferrule.toml` logs once that it needs a restart.
- **Part 3:** `mcp_config`. `[[mcp.servers]]` entries are added and removed
  through `toml_edit`.
  - A new entry goes after the last server.
  - `--replace` edits in place and keeps the entry's comments.
  - An inline `mcp = { servers = […] }` is edited inline.
  - The rest of the file is kept byte-for-byte.
  - Setup's `Target` is shared, with `save_then`: the config is checked and
    staged, then the secrets are saved, then the rename happens.
  - `ferrule extensions remove` on a configured name now drops its entry.
- **Part 4:** `ferrule mcp add`.
  - The same `probe` runs, in the sandbox the daemon would use, with a
    throwaway proxy holding the config's secrets and the new ones.
  - M13's scan runs; a block is refused unless `--waive`/`--skip-flagged`
    (or `waive`/`skip` typed at the terminal).
  - It refuses a secret-looking `--env`, a literal credential header, and a
    header `${NAME}` that isn't a secret.
  - `--secret NAME[=hosts]` takes its value from the secrets file, then the
    environment, then a hidden prompt. For a URL server the hosts default to
    the URL's host.
  - Only after a successful probe: the config is written through
    `toml_edit`, the values are saved (undone if the rename fails), and an
    offline doctor runs.
  - Doctor names each server and flags a header secret outside `[secrets]`.
  - `ferrule mcp list`/`remove`. An "MCP servers" step in `ferrule setup`,
    guided after Browser and as a menu item, runs the same `add_to`.

**Tests:**
- Real binary, `tests/mcp_add.rs`:
  - A `ferrule gateway` on the local channel answers message 1.
  - `ferrule mcp add demo --secret DEMO_TOKEN=api.example.com -- python3
    server.py` runs from a second process.
  - Message 2 is offered `mcp__demo__echo`, calls it, and the server sees a
    placeholder, never the key. No restart.
  - The config keeps the owner's comment, and the key is only in
    `secrets.env`.
  - A server that exits, a `GITHUB_TOKEN` in `--env`, and an
    `--enabled-tool` matching nothing each write nothing (no config change,
    no secrets file, no state dir).
- Unit tests:
  - `mcp_config`: comments, order, inline lists, secrets only one server
    used.
  - `config_follow`: trusted vs untrusted, a broken file, `enabled_tools`
    changes, removal.
  - The manager: hot-add, `list_changed` with a poisoned tool, filters and
    caps.

**Mock eval, `ferrule eval run evals/starter --variant ab`** (the real binary,
all 20 tasks), after part 2 and again at the end: engineered 20/20, naive
11/20 (+45 pts), $0.63 vs $0.45. The two runs match on every task, and the
totals match M14's own run. M17 doesn't touch the agent loop's decisions,
only which MCP tools exist.

**Design defaults for Max to confirm:**
- A 2 s poll, not a file watch.
- Only trusted configs are followed live.
- A late proxy leaks per process.
- Removing a secret takes a restart.
- The shell and the model's notes see new secrets only after a restart.
- Block hits in a configured server only warn at load; `mcp add` refuses
  first.
- `enabled_tools` filters before the scan.
- Caps are min-only.
- The doctor re-run is offline.
- A server that failed to start is retried only when its entry changes.
- The follower also runs in `chat`/`run`/`eval`.
- A new entry goes after the last server, not at the end of the file.
- `mcp remove` keeps `[secrets]`.
- The probe's state dir is removed only if the probe created it.

**M17 open edges:**
- A stdio server gets every secret's placeholder in its env, so removal
  can't name the secrets it used. Only header references are attributed.
- The interactive flow splits a command line on spaces. Quoting needs the
  `--` form.
- An installed service that runs a `./ferrule.toml` isn't followed live
  (by design); the add prints that a restart is needed.

**Unverified on macOS/Windows until the batch CI pass:**
- The follower's mtime granularity (HFS+/FAT round to 1–2 s; the length
  check covers most same-second edits).
- The atomic rename over a config another process has open (Windows sharing
  modes).
- The `canonicalize` comparison that decides whether a config is trusted
  (`/private/var` on macOS, `\\?\` on Windows).
- `python3` in the tests and fixtures (`py` on Windows).
- The secrets file's 0600 permissions on Windows.
- The probe under Seatbelt, and with no sandbox on Windows.
- The throwaway and late proxies' CA bundle, as the server's runtime
  (Node, Python) reads it on macOS and Windows.
- Killing the probe's process tree on Windows.

### 2026-09-25 — M16 the learning loop (Devi, Opus 5.5)

The design is `docs/m16-learning-loop.md`. The work is on branch `m16-learning-loop`,
cut from `main` at `7a975a4`, with a PR to `main` (not merged). M17 was built at the
same time on `m17-mcp-add` and merged first; this branch took `main` in by a merge
commit at the end (PLAN.md: both sides kept) and doesn't otherwise build on it.

Every part was checked locally on Linux: fmt, clippy `-D warnings`, and
`cargo test --workspace`. **CI was deferred at Max's request**; a full 3-OS pass runs
after the roadmap batch. No real model was called: everything ran against mocks.

**Commits:**
- `fd0fd39` design: triggers and episodes, consolidation, the playbook format and its
  caps, the reflector, the success gate, the budget, the files, revert, eval
  hermeticity, the off switch, failure modes and tests.
- `9768fb8` part 1, the new `ferrule-learn` crate:
  - a delta-only playbook: `- [pb-N] lesson` lines, the owner's lines kept byte for
    byte and shown to the reflector read-only; 300 chars a lesson, 40 lessons and
    4,000 chars injected
  - the reflector: at most one add/edit/retire per failed or retried episode, the
    transcript fenced as data; proposals screened for shape, secrets, injection
    phrases and near-duplicates (Jaccard ≥ 0.9)
  - the success gate: the episode's goal re-run with `ferrule eval`'s engineered
    harness in a scratch copy under the temp dir, the candidate playbook in its
    prompt; kept only when the check passes twice
  - memory consolidation of Jaccard ≥ 0.5 clusters through M15's UPDATE, with
    `undo_update` in `ferrule-memory` for revert
  - per-pass and per-day caps from ledger rows tagged `call_kind = "learn"`, checked
    before every call, a clean `stopped-budget` stop; three provider errors in a row
    end the pass (`stopped-errors`)
  - a lock file, `pass.json` rewritten after every step (a crashed pass is marked
    `interrupted`), before/after snapshots, `playbook.diff` and `changelog.md` per
    pass; revert restores the whole file, or inverts the pass's own lines after an
    owner edit, and undoes its merges
  - the review cursor never moves past an episode whose call failed
- `104ece9` part 2, the scheduler and the CLI:
  - built-in scheduler jobs in `ferrule-gateway`: tasks on channel `builtin`, keyed
    by name, run in place of an agent turn (a user task can't be hijacked by name);
    `ensure_builtin` adds, reschedules or removes one; an unregistered job is
    skipped and an erring or stopped-early one is logged truthfully
  - `[learning]` in config, off by default with the reason in its doc, and an
    example block; `ferrule learn run [--dry-run]`/`show`/`diff`/`revert`
  - the gateway (and `tasks run-now`) registers `ferrule-learn` only when enabled
  - task episodes from `tasks.db` (the newest failed or incomplete run per task,
    "fixed" when a later run succeeded, built-ins excluded); today's spend from the
    ledger's last 24 h, a pass refused when the ledger can't be read
  - the `[Playbook]` block after `[Skills]` in every agent's system prompt; `data/learn`
    added to the sandbox's hidden paths: the file tools refuse it, and so does the
    sandboxed shell; agents and sub-agents only see it in their prompt
- `a81f834` part 3, eval hermeticity and the binary tests:
  - `[suite] owner_playbook = true` (default false) hands the owner's block to the
    engineered variant only; documented in `docs/eval.md`
  - `runs_for` breaks a same-second tie by rowid (found by the binary test: a
    failure and its fix in the same second read as not fixed)
  - `tests/learn.rs` through the real binary: fail → fix → lesson kept → in the next
    `ferrule run` prompt, in the failed task's next `run-now` prompt, in `learn diff`
    and `learn show` → `learn revert` restores the owner's file byte for byte; a
    lesson failing its gate is in the changelog and the playbook is byte-identical;
    consolidation leaves one live fact for `memory search`, revert brings both
    back; a 150-token cap stops the pass after 2 calls (`stopped-budget`, the rest
    skipped); eval sees the playbook only on opt-in and only in the engineered
    variant; a sub-agent sees the playbook in its prompt and its `write_file` to
    `data/learn/playbook.md` is refused (data dir inside the workspace, sandbox off)
- a follow-up fix after the merge with `main`: `ferrule --help` showed the ledger's
  description glued onto `learn`'s and none on `ledger` (a doc comment left above
  the wrong variant in part 2); a binary test now checks both lines.

**Measured on the mock** (the real binary, all 20 tasks, `--variant ab`, with a
canary playbook in the data dir): engineered 20/20, naive 11/20 (+45 pts), identical
to M15 to the token (engineered 513.5k input / 88 calls / 13 compactions, naive
438.0k / 62 / 11 truncations). The canary appears in no eval transcript.

**Design defaults for Max to confirm:**
- `[learning] enabled = false`: it spends unattended and rewrites every prompt. A
  hand-written playbook is still injected (`playbook = true`). `ferrule learn run`
  works while disabled.
- Schedule `0 3 * * *` UTC; caps $0.50 / 300k tokens a pass, $1.00 / 1M tokens a day;
  at most 5 episodes and 5 memory clusters a pass.
- The gate: the episode's goal re-run in a scratch copy (data dir skipped), up to
  20 steps and 900 s, the check from `[learning] check` or `agent.verify_command`,
  two passes required. No check → adds and edits are rejected, retires still run.
- One delta per episode; lessons ≤ 300 chars, ≤ 40 injected, ≤ 4,000 chars.
- Consolidation: clusters at word-set Jaccard ≥ 0.5, merged only when the model says
  merge; always an UPDATE, never a delete.
- Sub-agents get the playbook in their prompt, never a tool that writes it.
- Eval: hermetic unless `owner_playbook = true`, which feeds only the engineered
  variant.

**M16 open edges:**
- A pass runs inside the scheduler tick, so a long pass (a slow gate) delays other
  due tasks until it ends.
- `ferrule setup` doesn't ask about learning; the example config documents it.
- The hidden-path refusal still says "(saved keys)" for `data/learn` too.
- With `[sandbox] mode = "off"` the shell tool can write `data/learn` (as it can read
  the saved keys): read-only is enforced by the file tools and the OS sandbox, not by
  the process.
- The cursor is whole seconds: an episode left for later that finished in the same
  second as one reviewed can be skipped. Rare; a (time, id) cursor would close it.
- The gate proves the lesson didn't break the task, not that it caused the fix: a
  task that passes anyway keeps any harmless lesson.
- The mock eval has no playbook-aware tasks, so the opt-in measures nothing yet.
- The M13 flaky test (`a_list_changed_that_introduces_a_poisoned_tool_is_caught`)
  failed once again under load and passed 3/3 alone.

**Unverified on macOS/Windows until the batch CI pass:**
- `copy_workspace` for the gate's scratch copy: symlinks are handled on unix only;
  long paths and file locks on Windows.
- The lock file (`create_new`, staleness by mtime) and atomic renames of
  `playbook.md`/`pass.json` over an open file on Windows.
- Scratch copies under the temp dir (`%TEMP%`, `/var/folders` symlinked to
  `/private`), and their cleanup.
- The data dir inside the workspace and the hidden-path check for `data/learn`,
  including case folding on APFS/NTFS.
- The playbook path under `~/Library/Application Support` and `%APPDATA%`, and CRLF
  in a hand-edited `playbook.md` (the owner's lines are kept byte for byte).
- The schedule's timezone (`chrono-tz`) and the check command run through `sh -c`
  vs `cmd /C`.
- The binary tests (`tests/learn.rs`): environment isolation and the scripted
  server on each OS.

### 2026-09-25 — M18 lifecycle hooks (Devi, Opus 5.5)

Branch `m18-hooks`, cut from main at a1f06ce. Design first
(`docs/m18-hooks.md`), then four parts:

- **Part 1:** `ferrule-core::lifecycle`: ten events, Claude Code's payload
  and its exit-code/JSON-output contract, name-or-glob matchers, an
  ordered `HookSet` (built-in, then user, then workspace; the first block
  wins) with caps and an audit trait.
  - `verify_command` is the built-in Stop check, with the same message,
    `max_verify_rounds`, events and `needs_check` rule as before.
  - PreToolUse/PostToolUse around every tool call; notes appended to the
    tool result or as a user message after the prompt, never in the
    system prompt; Stop capped at `max_stop_blocks`; Pre/PostCompact,
    SessionEnd, and a `HookFinished` event.
- **Part 2:** `ferrule-hooks`: command hooks (payload on stdin, per-hook
  timeout, the process group or tree killed on timeout, 64 KiB output
  caps), `[hooks]` and `.ferrule/hooks.toml` with unknown keys as
  errors, the trust record pinned to the file's SHA-256 under
  `private/`, the JSONL audit log and the `hooks list` rendering.
- **Part 3:** the CLI and the supervisor. `[hooks]` counts only from a
  trusted config; every top-level agent gets the user's and trusted
  workspace hooks; the untrusted notice once per process; children get
  the root's PreToolUse/PostToolUse via `attach_root`, and
  SubagentStart/Stop fire in the parent around each child run; SessionEnd
  for `run`/`chat`; `ferrule hooks list|trust|untrust` (trust only at a
  terminal, and only what was shown); a doctor check.
- **Part 4:** `tests/hooks.rs`, ten real-binary tests against a scripted
  model, one per "done means" item: the built-in check, a PreToolUse exit
  2 (reason in the next request, tool never ran, audited),
  `additionalContext` after the cached prefix, untrusted → trusted →
  edited workspace hooks, a hung hook killed with its child at 1 s, an
  always-blocking Stop hook capped at 3, a child's call blocked by the
  parent's PreToolUse plus SubagentStart/Stop payloads and blocks, eval
  ignoring every owner hook, and the model failing to write, trust or
  enable a hook.

**Mock eval, `ferrule eval run evals/starter --variant ab`** (the real
binary, all 20 tasks), after part 1 and again at the end: engineered
20/20, naive 11/20 (+45 pts), $0.53 vs $0.45, 88 vs 62 calls, 4 failed
checks fixed. Both runs match on every task. The built-in check keeps the
same wording and cap, so nothing moved.

**Design defaults for Max to confirm:** the ten in `docs/m18-hooks.md`
§10 — workspace hooks off by default plus per-file trust; fail open on
timeout/crash; sequential, first block wins; `max_stop_blocks = 3`;
globs not regexes; a SubagentStart block fails the child rather than
`spawn_agent`; no SessionEnd for gateway sessions; no eval opt-in; a
PostToolUse block is a note; hook errors go to the owner only.

**M18 open edges and the macOS/Windows-unverified spots:** in the M18
bullet under Current State and `docs/m18-hooks.md` §11.

### 2026-09-25 — hotfix: the Telegram bot going deaf (Devi, Opus 5.5)

Max's bot on his server stopped answering and there was no way to see why
from the chat. Two causes in the Telegram adapter:

- **No request deadline.** The `reqwest` client had no timeout, so a
  half-open connection during the long poll parked `getUpdates` forever.
  The process stayed up, so systemd's `Restart=always` never kicked in.
- **Any failed poll ended the adapter.** A network blip, a 502 from a proxy,
  a 409 or a 429 returned `Err` from `run()`. The gateway then ended with
  the channel, the process exited, and systemd restarted it 5s later,
  dropping whatever turn was in flight.

Fix: a 10s connect deadline, a 30s deadline on send/edit, and 45s on the
poll (the 30s long poll plus slack), with TCP keepalive. A failed poll is
now logged and retried with backoff (1s doubling to 60s, reset by the next
good poll; Telegram's `retry_after` wins when it's given). Only a rejected
token (401/404) stops the adapter. Three new tests use a scripted mock
server: two polls that never answer, then a message; four failed polls
(HTML 502, not-ok 500/409/429), then a message; and a 401 that ends `run()`
after one poll with no retry.

Not covered here, and queued as a reliability milestone after M19: a stuck
turn still silently blocks its chat's lane. The planned fixes are an
out-of-lane 👀 on receipt, `/status` and `/stop` answered by the gateway
itself, a turn watchdog that tells the owner, and an external dead-man alert.

### 2026-09-25 — M19 trust & cost (Devi, Opus 5.5)

The design is `docs/m19-trust-cost.md`. The work is on branch `m19-trust-cost`, cut
from `main` at `c6a3244`, with a PR to `main` (not merged). M18 (lifecycle hooks) is
being built at the same time on `m18-hooks`; this branch doesn't build on it, and the
gate sits in its own crate (`ferrule-trust`) behind a thin `Guard` seat in the agent
loop, so the two meet only at small call sites.

Every part was checked locally on Linux: fmt, clippy `-D warnings`, and
`cargo test --workspace`. **CI was deferred at Max's request**; a full 3-OS pass runs
after the roadmap batch. No real model was called: everything ran against mocks and
fake servers.

**Commits:**
- `dd81c12` design: caps per run, day and scheduled task read from the ledger, the
  80% warning, the kill switch, the classifier and its blind spots, the Telegram
  approval round-trip, plan mode, sub-agents, eval isolation, the audit trail,
  failure modes, the M18 PreToolUse relation and the defaults.
- `03f3c94` part 1: the `Guard` seam in `ferrule-core` (`Agent::with_guard`, checked
  before every model call and tool call, raced against `halted()`); the shell tool
  kills its command's whole process group when the call is dropped.
- `52d1bcc` part 2: the `ferrule-trust` crate — Hub, Meter (the ledger in the
  configured timezone), kill switch file, classifier, approvals, TrustGuard,
  PlanStore, TrustSink and `<data>/trust/audit.jsonl`.
- `a3e0bad` part 3: `[trust]` in the config and the binary's glue (`trust.rs`): every
  agent in a run tree, who approves seated per tree, `ferrule stop` and
  `ferrule trust status|audit`.
- `9f72878` part 4: the gateway — an Interceptor seat, `Scheduler::with_hold`,
  warnings and approvals to the owner chat, `/stop`, `/resume`, yes/no.
- `c6e18b1` part 5: plan mode — `ferrule run --plan`, `ferrule plan
  list|approve|reject`, `/plan` in Telegram, the read-only no-network planning sandbox,
  `Router::retire`.
- `5fc7302` part 6: eval stays hermetic unless `[suite] owner_trust = true`; the
  learning pass won't start while stopped or over a day cap; `ferrule doctor` shows
  the trust line.
- docs: this entry, `docs/eval.md` (`owner_trust`), the roadmap status.

**Eval (mock, `ferrule eval run evals/starter --variant ab`, all 20 tasks):**
engineered 20/20, naive 11/20, +45 pts; 150 calls, 951.5k input + 6.2k output
tokens, $0.98 at the mock's prices — the same numbers as before M19. The same run
with `ferrule stop` engaged and every `[trust]` cap at 1 token / $0.0001 gave the
identical report (exit 0), and `ferrule trust status` afterwards said 0 tokens today.

**Defaults for Max to confirm** (design §14):
- caps: 5,000,000 tokens and $5 per run, 50,000,000 tokens and $20 per day,
  per-task caps off, warning at 80%, the day in UTC unless `timezone` is set;
- gates on (rm -r, find -delete, rsync --delete, git clean -f, force pushes, DELETE
  to a bound or unknown host);
- unattended runs (scheduler, eval opt-in, `ferrule run` without a terminal) refuse
  gated commands rather than wait;
- approval timeout 10 minutes, a plan's 1 hour; only `yes` approves;
- the owner chat is `[trust] owner_chat` or the first *private* allowed chat, never a
  group; `/stop` works from any allowed chat, `/resume` only from the owner chat;
- the kill switch survives restarts and an unreadable stop file counts as stopped;
- eval is off by default (`owner_trust = false`);
- the learning pass is checked only when it starts.

**Open edges:**
- A learning pass and its gate agent aren't guarded mid-pass: the check is at start.
- The LLM judge's calls in an opted-in eval aren't charged to the owner.
- A plan longer than Telegram's 4,096 characters may fail to send to the owner chat;
  the plan is saved and `ferrule plan approve` still works.
- The classifier can't see through `python -c`, scripts, `make`, `$CMD` or
  `base64 | sh`; its tests say so.
- The order of M19's gate and M18's PreToolUse hooks is left to the reconciliation
  (design §13 proposes the gate first).
- The M13 flaky test (`a_list_changed_that_introduces_a_poisoned_tool_is_caught`)
  failed once in part 4 and passed on re-run.

**Unverified on macOS/Windows until the batch CI pass:**
- Killing the shell command's process group on a halt (unix `killpg`; Windows has no
  process groups in the same sense).
- The stop file's atomic write (rename over an existing file on Windows).
- TTY detection for terminal approvals and plan prompts.
- The planning sandbox: read-only and network-off rely on Landlock/seccomp, which are
  Linux-only; elsewhere the shell is dropped from a planning run because the OS
  sandbox isn't active.
- Midnight in the configured timezone (`chrono-tz`) across platforms.
- The binary tests (`crates/ferrule-cli/tests/trust.rs`, the gateway tests):
  environment isolation and the fake model and Bot API servers on each OS.

### 2026-09-25 — M19 × M18 reconciliation: `main` merged into `m19-trust-cost` (Devi, Opus 5.5)

This is a real merge of `origin/main` (M18 hooks, PR #7, and the Telegram hotfix, PR #8)
into this branch. There was no rebase or squash.

**Conflicts:**
- `PLAN.md`: both sides kept, with main's M18 and hotfix entries before M19's.
- `Cargo.lock`: ours, then regenerated by cargo. It came out the same as the auto-merge.
- `doctor.rs`: the hooks line, then the trust line.
- `main.rs`: `trust::equip` and the plan-mode note kept, plus M18's hooks and
  `end_session`. The hooks are skipped while planning.
- `agent.rs`: the tool dispatch rewritten by hand, and both sides' tests kept.
  `guard.rs` lost the `guarded_call` helper, which nothing uses now.

**The order at tool dispatch:**
1. the owner's gate
2. PreToolUse
3. the tool
4. PostToolUse

Each step is raced against the kill switch. A refused or halted call fires no hook,
and no hook can approve past the gate. Stop hooks and `verify_command` sending a run
back still meet the caps at the next model call. A planning run, and any eval (even
one with `owner_trust`), fires no hooks. All of this is settled in
`docs/m19-trust-cost.md` §13.

**New tests** (`crates/ferrule-cli/tests/trust.rs`, real binary, scripted model):
- the gate before PreToolUse, in one session with a hook and a gated `rm -rf`
- a Stop hook sending the run back still meets the run cap
- a planning run fires no hooks, and the approved plan's run does (mutation-checked)
- an eval with `owner_trust` fires no hooks

**Checks:**
- fmt clean; clippy `-D warnings` clean.
- `cargo test --workspace`: 555 passed, 0 failed, 2 ignored. That includes the hotfix's
  Telegram tests next to M19's Interceptor, and the flaky M13 test passed this time.
- Mock eval: engineered 20/20, naive 11/20, $0.98. It matches the pre-merge run line
  for line.

### 2026-09-25 — fix: the flaky M13 test was a real crash window (Devi, Opus 5.5)

`a_list_changed_that_introduces_a_poisoned_tool_is_caught` kept failing
under load with the server's status still `Active` after its tools were gone.
When a `list_changed` brought a flagged tool, the manager unloaded the
server first (the tools vanish, then it awaits the client's shutdown) and
only then recorded `Suspended`. The test could land in that gap. So could
a crash: a process that died during the shutdown would have restarted the
flagged server as active. The suspension is now recorded before the
unload. The test is unchanged, because its "status is already Suspended once the tools are
gone" assertion is exactly the ordering guard. Stress run of the test
binary, 10 in parallel × 8: 3/80 failures before the fix, 0/80 after.

### 2026-09-25 — batch CI pass: M11–M19 on macOS and Windows (Devi, Opus 5.5)

Branch `ci-fix-3os`, cut from main at 0d091af, PR #11 (not merged). This is the 3-OS pass
M12–M19 deferred. On 0d091af, CI (run 36070309686) was red on macOS, Windows and
the ubuntu e2e step. **Last completed green run: 36071977256 on 092873a (also green: PR run 36072883759 on 32cd8e6), green on ubuntu-24.04
(555 passed, e2e included), macos-14 (552 passed) and windows-latest (521 passed); 2
ignored on each, both pre-existing.** The counts differ because of `#[cfg(unix)]` gates
that already existed. This pass adds no `cfg` gate (one `cfg!(windows)` condition, below), and no test was ignored or loosened.

**Failures and fixes:**
- **Windows `\\?\` paths.** `std::fs::canonicalize` gives verbatim paths on Windows.
  They reached git (a clone couldn't create its work tree), python (graders got
  `C:\\?\\D:\\…`), cmd.exe and the owner's screen. Every `canonicalize` in the workspace
  now goes through `dunce::canonicalize`, which is std's on every other OS, so a
  one-sided swap can't break the comparisons between paths. `git.rs` also simplifies
  a clone destination and cwd it's handed. Kept on std: `doctor.rs`, which compares two
  std-canonical paths and shows neither, and the Linux-only sandbox code.
- **Skill scan findings named `references\guide.md` on Windows.** They now use `/` on
  every OS, because the owner reads them next to SKILL.md's own links.
- **macOS: the always-blocking Stop hook test saw 1 payload instead of 4.** The test
  hook named its files with `date +%s%N`, and BSD date has no `%N`, so four runs in the
  same second wrote one file. The hook did run four times. The test hook now names
  them by count; the assertion is unchanged.
- **e2e `setup_wizard.py`** didn't know the wizard's MCP prompt, or the browser prompt
  that shows only when Chrome and agent-browser are present. It knows both now.
- **Uncovered once `\\?\` was gone:** `git_commands_must_live_in_the_checkout`
  expected `root/bin/run` with `/` joined in. It matched on Windows only because
  `PathBuf::push` normalises separators after a verbatim prefix. Also, rename-api's
  reference `solve.sh` compared a Python `glob` result to `shop/__init__.py`, and glob
  gives `shop\__init__.py` on Windows. Neither the task, the grader nor the mock changed.
- **Windows: `lock::tests::concurrent_writers_lose_nothing` failed in a later run**
  (workflow_dispatch 36072885411 on 32cd8e6, `Access is denied`; PR #11's run on the same
  sha had passed). The test was right; the bug was real. Windows answers
  "access denied" for a moment in two places other systems don't. One is creating the
  extensions lock's `.lk` file while the last holder's delete is still pending (the
  delete-pending status maps to ERROR_ACCESS_DENIED). The other is renaming over
  `extensions.lock.json` while something has it open (an antivirus or indexer scan). A
  daemon and the owner's CLI updating at once could lose an update with an I/O error.
  Both now wait it out like a held lock, within the lock's 5 s wait. On other OSes a
  permission error is still an error at once, which makes this `cfg!(windows)` the one
  platform condition this pass adds. The learning loop's own lock
  (`ferrule-learn/src/files.rs`) refuses rather than waits, so there the same case
  would read as "another pass is running", once. It is left as is.
  **Not yet proven in CI:** both runs on the fix's commit ba5c48e (pull_request
  36100515458, workflow_dispatch 36100513382) never started a job. GitHub refused them
  because "recent account payments have failed or your spending limit needs to be
  increased". So the fix has passed locally on Linux only. It still needs a PR run and
  at least two dispatch runs on Windows once Actions billing is fixed.

**Now verified by run 36071977256** (the named behaviour's tests passed on that OS):
- M12: owner locks (`File::try_lock`, the startup sweep); worktree git calls and paths,
  including removing a worktree on Windows; snapshot copying; `\\?\` in the workspace
  and git-common-dir checks (via dunce); the e2e test's environment isolation on both.
- M13: spawning `git` with `GIT_CONFIG_GLOBAL=NUL`; atomic renames of the lock and queue
  files; the `.lk` lock and its stale takeover; `inside()` and `skill_dir_in` with
  canonical paths; `replace_dir` over an existing directory; the `python3` MCP fixture
  on Windows. The list_changed test that was flaky passed on all three.
- M14: the starter suite's `python3` graders, checks, solve scripts and mock model, run
  through the platform shell on all three, with every reference solution passing; git
  fixtures; macOS `/private/var` and Windows `\\?\` temp paths; workspace cleanup; the
  CLI tests' environment isolation.
- M15: transcript paths and reading them back; `secure_delete` and the "no trace" test on
  APFS and NTFS; the pre-M15 fixture migration (the binary fixture survived checkout);
  `tests/memory.rs` on each OS.
- M16: the gate's scratch copy (`copy_workspace`) and its cleanup under `%TEMP%` and
  `/var/folders`; the lock file and atomic renames; the schedule's timezone; `tests/learn.rs`.
- M17: config following and the trusted-config `canonicalize` comparison; atomic
  rename over the config; `python3` in the tests and fixtures; the live probe
  (`tests/mcp_add.rs`) on each OS.
- M18: macOS entirely (`tests/hooks.rs` and the hook tests in `tests/trust.rs` run
  there: `sh -c`, exit codes, `killpg`, payload piping, trust and audit files, the
  canonical workspace key). On Windows, only the trust-file unit tests.
- M19: the stop file's atomic write; midnight in the zone (`chrono-tz`) with the day cap
  surviving a restart; `tests/trust.rs` and the gateway tests with their fake model and
  Bot API servers on each OS. The four hook-ordering tests are unix-only.

**Still unverified, because CI can't reach it:**
- Telegram itself (the tests use a fake Bot API).
- The systemd and launchd service install (`service.rs`); there is no Windows service.
- A real sandbox backend under an agent run. Seatbelt's self-test runs on macOS, but
  no test runs the agent, an eval, a verifier child, the planning sandbox or an MCP
  probe under Seatbelt. Windows has no sandbox.
- Windows hooks: the shell choice (`sh -c`/Git Bash vs `cmd /C`/PowerShell),
  PowerShell's exit 2 and `taskkill /T /F` of a timed-out tree. `tests/hooks.rs` and the
  hook tests in `tests/trust.rs` are `#[cfg(unix)]`, so these stay open.
- Windows process-tree kills in general: the shell tool's timeout and drop tests are
  `#[cfg(unix)]`, so the shell tool's, the probe's and the MCP server's kills and M19's
  halt of a running command are untested there.
- TTY detection and the inline approver on Windows consoles (CI has no TTY).
- Symlinks and junctions on Windows (the symlink tests are `#[cfg(unix)]`).
- 0600/0700 permissions on Windows, which are a no-op there.
- `npx`/`uvx` resolution via `PATHEXT`, since no test runs a registry server.
- HFS+/FAT mtime granularity (CI is APFS/NTFS).
- The CA bundle as Node/Python read it on macOS and Windows.
- `OLLAMA_CONTEXT_LENGTH` for the Ollama app.
- The e2e wizard and hidden-keys scripts, which are Linux-only in CI.

**Checks:** fmt clean; clippy `-D warnings` clean; `cargo test --workspace` passes locally;
mock eval: engineered 20/20, naive 11/20, $0.98. It matches the M19 numbers.

### 2026-09-25 — M19b reliability: never silently deaf (Devi, Opus 5.5)

The design is `docs/m19b-reliability.md`; its **As built** section lists where the
build departs from it. The work is on branch `m19b-reliability`, cut from `main` at
`bf60851`, with a PR to `main` (not merged). It builds on M19's Interceptor, `/stop`,
`/resume`, the owner chat and `ferrule-trust`, the M16 ledger (through M19's spend
lines), the M3 scheduler, `ferrule doctor` and the systemd unit in `service.rs`.

Every part was checked locally on Linux: fmt, clippy `-D warnings`, and
`cargo test --workspace`. **CI was deferred at Max's request**; a full 3-OS pass runs
after the roadmap batch. No real model was called: everything ran against mocks, a
fake Bot API, a fake heartbeat receiver and a real unix datagram socket.

**Commits:**
- `56eca82` design: the failure modes a phone-only owner can't see, the six pieces,
  the defaults, eval hermeticity, out of scope.
- `62634a8` part 1: 👀 before the lane (Telegram `setMessageReaction`, 2 s
  best-effort), one busy notice per busy period, per-lane state in the router, the
  gateway's interceptor list, the `Redactor`, Telegram's last ok poll and errors
  without their URL.
- `617e3f1` part 2: `/status` answered by the gateway before the interceptors and
  the lanes; `<data>/gateway/status.txt` and `ferrule status`; the `tracing` ring
  of recent warnings; `offer()` never blocks the dispatcher.
- `6daf339` part 3: the no-progress watchdog (one message per stall to the owner)
  and `max_turn_minutes` through the router's `TurnDeadline`.
- `3119fd2` part 4: `running.json`, clean SIGTERM/Ctrl-C shutdown, the restart
  notice with retries, `notify_on_start`.
- `70f835f` part 5a: `WatchdogSec=120` + `NotifyAccess=main` in both units;
  `WATCHDOG=1` over a raw unix datagram only while the dispatcher and polling are
  healthy.
- `ca10e0c` part 5b: the heartbeat (`heartbeat_url`, `heartbeat_secs`) and the
  test that `ferrule eval` sends no receipt, ping, heartbeat or notice and leaves
  the marker alone.
- part 6: the `health` line in `ferrule doctor`, the design doc's As built section
  (including the shell tool's fixed 120 s ceiling), this entry, the roadmap status.

**Checks (final):** fmt clean; clippy `-D warnings` clean; `cargo test --workspace`
582 passed, 0 failed, 2 ignored (555 before M19b). The M13 flaky test didn't fail
this time.

**Eval (mock, `ferrule eval run evals/starter --variant ab`, all 20 tasks):**
engineered 20/20, naive 11/20, +45 pts; 150 calls, 951.5k input + 6.2k output
tokens, $0.98 ($0.53 / $0.45); 13 compactions / 11 truncations; 4 failed checks
fixed. Identical to the run before M19b. The eval's data directory had no
`gateway/` afterwards.

**Defaults for Max to confirm:**
- `watchdog_after_secs = 600`: one "stuck on … — /stop to cancel it" message after
  10 minutes without a model or tool call starting or finishing;
- `max_turn_minutes = 60`: a turn is ended the way `/stop` ends one;
- `poll_stale_secs = 300`: Telegram with no ok poll for 5 minutes is stale, in
  `/status`, and systemd's watchdog stops being pinged;
- `WatchdogSec=120` in the units ferrule setup writes (restart ≈2 min after pings
  stop); "the dispatcher is stuck" = one message for over 60 s;
- `notify_on_start = false`: after a clean restart, silence; after an unclean one
  the owner always hears it;
- `heartbeat_url = ""` (off), `heartbeat_secs = 60`;
- the notices go to trust's owner chat, else to the chat they're about; the busy
  notice waits until the turn in front has run 3 s; `/status` works from any allowed
  chat, not only the owner's.

**Open edges:**
- A clean stop (SIGTERM, `systemctl restart`) during a turn says nothing on the next
  start: the turn is dropped as before. Only an unclean exit leaves a marker.
- An interrupted turn is never re-run; the owner re-sends it.
- A long Telegram outage (past `poll_stale_secs`) means systemd kills and restarts
  the gateway about every 7 minutes (`poll_stale_secs` + `WatchdogSec`) until polling
  works again. Each kill leaves the
  marker, so each start queues a restart notice, retried for about two minutes; the
  ones that can't reach Telegram in that time are lost (they're still in the log).
- The heartbeat has no retry within an interval; a checker should allow a missed
  ping or two. A heartbeat URL that isn't a URL is a warning per streak and a ✗ in
  doctor, not a startup error.
- The watchdog doesn't see inside sub-agents: their progress counts only through the
  root's tool call that runs them.
- A unit written by an older ferrule has no `WatchdogSec`; doctor warns, and
  `ferrule setup` rewrites it. launchd (macOS) has no equivalent watchdog.
- A wedged dispatcher silences every chat, `/status` included; only systemd's
  watchdog (Linux, under a unit ferrule wrote) catches it, after 60 s plus
  `WatchdogSec`. Elsewhere the heartbeat, if one is set, reports it as degraded.

**Unverified on macOS/Windows until the batch CI pass:**
- SIGTERM/Ctrl-C handling and the clean shutdown (`tokio::signal::unix` on unix,
  only Ctrl-C on Windows; a Windows service stop isn't a Ctrl-C).
- `pid_alive` for the leftover marker: `kill(pid, 0)` on unix, and on Windows
  `OpenProcess` + `GetExitCodeProcess` (added after PR #12's first 3-OS CI run, where
  "can't tell" made a killed gateway's fresh marker look like a second live gateway).
- The marker's and status file's write-then-rename over an existing file on Windows,
  and `File::set_modified` in the tests.
- sd_notify and the watchdog pings (Linux only by design; off elsewhere, and the
  datagram and abstract-socket tests are `#[cfg(target_os = "linux")]`).
- The launchd plist: unchanged, no watchdog; `ferrule doctor`'s unit check is
  Linux-only.
- The binary tests (`crates/ferrule-cli/tests/health.rs`, the new eval test): the
  fake Bot API, heartbeat receiver and process killing (`libc::kill`, `#[cfg(unix)]`
  parts) on each OS.
- The shell tool's 120 s process-group kill during a watchdog'd or deadlined turn
  (`killpg` on macOS, no process groups on Windows).


### 2026-09-25 — M21 models: several at once, a default, a model per agent (Devi, Opus 5.5)

Max, msg 3160: connect several models in parallel, pick a default, and run a
given agent on a different model, from Telegram and `ferrule setup`. The design
is `docs/m21-models.md` and the user guide is `docs/models.md`. The branch is
`m21-models`, cut from `cda13b5` (M19b), with `origin/main` merged in at 0.2.0.
The PR to `main` is open and not merged. No real model was called: everything ran
against mock providers and a fake Bot API.

**Commits:**
- `bcf9b88` design: refs, `[models]`, per-model entries, precedence, pins,
  owner-only `/model`, the locked write, fallback, what's recorded, eval
  hermeticity, failure modes, the API for M22.
- `a3bad79` part 1: `ferrule-core` records the served provider and model per
  call, and `Provider::fail_over` fires only after the retries on a transient
  failure (transport, 408, 429, 5xx), at most 8 times per call.
- `afe55d8` part 2: `Catalog` and `RoutedProvider` in `models.rs`. A ref
  resolves, and each call picks one-off > role > task > chat pin > default. A
  hand-edited config is reloaded. A model that's down is skipped for 5 minutes,
  and the owner is told once. The ledger and caps price by the served model.
- `50bd7d5` part 3: the one `Models` API and `ferrule model`. Writes go under
  `<config>.lock` through `toml_edit`, into a temp file renamed with Windows'
  retry, and are audited.
- `a735fa7` part 4: Telegram `/model` (owner only) and a models section in
  `/status`. A change retires the affected lanes.
- `e974777` part 5: a task's model (`tasks add --model`, `tasks model`) and
  `spawn_agent`'s `model`.
- `c3a7fc7` part 6: the setup presets (Gemini, Groq, the Anthropic note),
  "Add another model", "Test it" and "Default model"; `doctor --ping-models`;
  `eval --model` by ref; the learning pass on the default model.
- part 7: seven end-to-end tests through the real binary (`tests/models.rs`)
  and `docs/models.md`.
- part 8: this entry, the roadmap and the Current State section.

**Checks (after merging main):**
- fmt is clean, and so is clippy with `-D warnings`.
- `cargo test --workspace`: 610 passed, 0 failed, 2 ignored.
- `tests_e2e/setup_wizard.py` and `hidden_keys.py` pass locally.

**Eval** (mock, `ferrule eval run evals/starter --variant ab`, all 20 tasks):
- pass rate: engineered 20/20, naive 11/20, +45 pts;
- usage: 150 calls, 952.0k input + 6.2k output tokens;
- cost: $0.98 ($0.53 / $0.45);
- context: 13 compactions / 11 truncations, 4 failed checks fixed.

Input was 951.5k at M19b. The 0.5k difference wasn't traced; the pass rates and
the cost are the same.

**Decisions for Max to confirm:**
- Fallback is off by default. It covers only transient failures after the
  retries. A 401/403, an unknown model or a missing key never falls back.
- A missing key fails when the agent is built, not at the first call.
- Telegram has no one-off per message: `/model use` pins the chat, and the one-off
  is `ferrule run/chat --model` or `spawn_agent`'s `model`. There's no
  agent-facing tool for changing a task's model or scheduling on a model.
- Pins live in `<data>/models/pins.json`, not in the config. A task's model is
  in tasks.db.
- In a group, the owner is the owner's own Telegram id (`sender_id`). Anyone
  else, the group's admins included, is refused.
- A new default retires every chat lane, so the next message builds on the new
  model's harness profile. Scheduled lanes are left alone.
- An alias is stored as the alias (a pin or task on `fast` follows the alias when
  it moves).
- `remove_model` refuses a provider's primary model and the current default.
- `model test` sends no `max_tokens`, because newer OpenAI models refuse it.
- A process that can't pick models per agent refuses `spawn_agent`'s `model`
  rather than ignoring it.
- `ferrule eval` never follows `[models] default`, pins or fallback.
- Anthropic goes through its OpenAI-compatible endpoint, which has no prompt
  caching, no thinking output and no PDF input. A native driver is a follow-up.
- Preset models: Groq `llama-3.3-70b-versatile`, Gemini `gemini-2.5-flash`.

**For M22:** `models::admin` reads everything through `Models::view()` (serde
`ModelsView`: models with aliases, default, fallback rank, key present, prices,
window, profile and outage; pins, the last model per session, problems). It
changes things through `set_default`, `pin`/`unpin`, `set_fallback`,
`add_model`, `remove_model`, `set_alias` and `test`, each taking a `by` for the
audit.

**Open edges:**
- A native Anthropic driver.
- The last model per session is in the gateway's memory, so a restart forgets
  it until the next call.
- A running `ferrule run` keeps its one-off even if the default moves.

**Unverified until CI runs:**
- macOS and Windows: the locked write and its rename retry, and the
  `tests/models.rs` binaries (fake servers, gateway kill).
- setup's new steps in a real pty on anything but Linux.

### 2026-09-25 — M20 connections (Devi, Opus 5.5)

The design is `docs/m20-connections.md`, and its **As built** section lists where
the build departs from it. The work is on branch `m20-connections`, cut from `main`
at `edfce24`, with a PR to `main` (not merged).

It builds on:
- M17's `set_configured` hot-add
- M19's gate, audit log and owner chat
- M19b's interceptors, `/status` and `Redactor`
- the Telegram channel and the secrets file

There is one new crate, `ferrule-connections`, plus `relay/` (the Worker).

Every part was checked on Linux: fmt, clippy `-D warnings`, and
`cargo test --workspace`. The Worker has its own tests (`node --test relay/`). No
real provider was logged into. The flows ran against:
- a mock OAuth AS (DCR + PKCE)
- a mock MCP server that wants a bearer token
- a mock relay speaking the Worker's contract
- a fake Bot API
- temp dirs

**Commits:**
- `7801832` design:
  - the flow, the catalog, OAuth, the relay (DO vs KV, threat model), the fallbacks
  - the key form, the tools, Telegram, the gate, the CLI
  - one service model for M22, audit, eval, failure modes, defaults
- `b1d7bc1` part 1: the relay Worker.
  - One Durable Object per slot. Only the relay key opens or reads one.
  - A value is written once and read once. A used mark refuses replays.
  - A value lives 5 min, an open slot 15 min.
  - No logging, size limits, a strict CSP.
  - The key form encrypts in the browser (ECDH P-256 → HKDF → AES-GCM).
  - 13 `node --test` cases.
- `3ce4157` part 2:
  - `ferrule-mcp`'s HTTP transport asks a `CredentialSource` for a header on every
    request, retries once on a 401, and scrubs the credential from errors.
  - The catalog: 7 verified services.
  - The sealed store: AES-GCM, a lock file, atomic writes.
  - OAuth: discovery, DCR, PKCE, exchange, refresh, revoke.
  - The relay client and `deploy`, the key form's decryption (cross-checked in
    node), and paste-back parsing.
- `6efa78c` part 3: the `Connections` service.
  - A request, then consent, then the callback (relay, tunnel or paste), then the
    token.
  - Refresh happens under the lock. A refused refresh sends one reconnect notice.
  - The owner-only commands, and API keys.
  - 11 hermetic flow tests.
- `4c8490d` part 4: the gateway and the CLI.
  - Telegram inline buttons: a tap is its command, trusted no more than typing it.
  - `ConnectedWrite` in M19's gate.
  - Connections merged with the config's MCP servers, applied live.
  - `ConnectionsDoor`, and connections in `/status` and `doctor` (no secrets).
  - The root agent's `connection_request`/`connection_list`.
  - `ferrule connections …`.
- part 5:
  - A running gateway also notices a connection added by another process (the
    store's hash, every 2 s), with a test.
  - `relay check` is tested against the mock relay, and `relay deploy` against a
    mock Cloudflare API (idempotent, the migration sent once, errors without
    credentials).
  - The design doc's As built section, this entry, the roadmap status.

**Checks (final, after merging `main` with M21 and 0.2.0):** fmt clean; clippy
`-D warnings` clean; `cargo test --workspace` 656 passed, 0 failed, 2 ignored (624
at part 4, before the merge). The merge needed two changes: a button tap now carries
`sender_id` (M21's field, the tapper's Telegram id), and `ferrule-connections` is
0.2.0 like every other crate.

**Eval (mock, `ferrule eval run evals/starter --variant ab`, all 20 tasks):**
- engineered 20/20, naive 11/20, +45 pts
- 150 calls, 951.5k input + 6.2k output tokens, $0.98 ($0.53 / $0.45)
- 13 compactions / 11 truncations; 4 failed checks fixed

That is identical to the run before M20. The eval's data dir had no connections
store and no `mcp/connections` afterwards.

**The relay, live.** It is deployed to Max's Cloudflare account as the Worker
`ferrule-relay`, at `https://ferrule-relay.maximarhipkin.workers.dev`.
- Its binding is `SLOTS` (class `Slot`, sqlite migration `v1`), and its secret is
  `RELAY_KEY`.
- Observability and logpush are off.
- The account holds that one script and nothing else. No test script was made.
- The relay key is in the box's secrets file, outside the repo.

The smoke test passed every step:
- `/health`
- a wrong key → 401
- a callback to an unopened slot → 404
- open → 204
- callback → 200
- a second callback → 409
- read → 200 with the code
- a second read → 410
- a replayed callback → 409
- after 5 min 20 s: a late callback → 404, and a poll → 204 (the value expired)

The deploy and the smoke test were done with curl, making `relay::deploy`'s exact
calls. ferrule's rustls client doesn't trust this container's TLS-intercepting
proxy.

**Defaults for Max to confirm:**
- no default relay URL; each owner deploys their own with `ferrule connections
  relay deploy`
- read-only by default (Gmail/Drive read scopes, Linear `read`, GitHub's read-only
  header); Atlassian, Notion and Attio have no read-only mode, so they rely on the
  gate
- `gate_writes = true`: a connected tool without `readOnlyHint` asks the owner first
- paste-back redirect `http://127.0.0.1:8976/callback`
- a login flow lives 15 min; the relay is polled every 2 s
- a decline quiets the agent's asks for that service for 10 min
- relay: a value for 5 min, an open slot for 15 min
- GitHub by PAT through the key form, not OAuth
- Google needs the owner's own OAuth client, with the id and secret in the secrets
  file; in Testing mode its refresh tokens expire after 7 days

**Open edges:**
- A config `[[mcp.servers]]` entry with a connection's name wins. It shows only as
  a log warning, not in `/status`.
- There is no `ferrule connections test`. `ferrule doctor` doesn't probe the relay;
  `relay check` does.
- `ferrule setup` has no relay step yet (M21 is reworking setup).
- The quick tunnel can't serve Google or GitHub, since their redirect URI must be
  registered.

**Unverified:**
- Real logins with each provider:
  - Google Web-client loopback redirects and the `resource` parameter
  - Google's `initialize` without auth
  - Atlassian's redirect allowlist
  - Attio's `offline_access`
  - Linear's read scope vs `/mcp/readonly`
- `ferrule connections relay deploy`/`relay check` run live through the binary
  (blocked here by the proxy's CA; tested against a mock Cloudflare API and a mock
  relay).
- macOS and Windows until this PR's CI run: the store's lock file and
  write-then-rename, and the cloudflared tunnel spawn.

### 2026-09-25 — M19c live-bot fixes: the owner never guesses why the bot doesn't answer (Devi, Opus 5.5)

A live bot stayed silent, and the owner had no way to learn why. M19c makes
every reason reach the owner in Telegram, or show in `/status`, `ferrule
status` and `ferrule doctor`. The design, the as-built notes, the owner's
checklist and the exact messages are in `docs/m19c-live-fixes.md`. The
branch is `m19c-live-fixes`, cut from `bbd4c58` (main at 0.2.0 with M21).
The PR to `main` is open and not merged. It is to ship as 0.2.1; the version
isn't bumped. Everything ran against a fake Bot API and mock models.

**Commits:**
- `f904c30` design.
- `0744ee7` part 1, the Telegram adapter and the log default:
  - the default log filter;
  - `without_url()` on the provider and MCP HTTP errors;
  - the webhook is deleted at start, and when a 409 names it;
  - 409 episodes, with `[health] telegram_conflict_secs` and
    `Channel::problem()`;
  - captions and non-text messages;
  - ignored chats warned once an hour.
- `548dcaf` part 2, provider failures:
  - `CoreError::plain_words()` and `after_attempts()`;
  - the lane's rate-limit countdown in `/status` and the busy notice;
  - OpenRouter's real 404/429 bodies through the adapter.
- part 3, doctor and status:
  - doctor's webhook, second-gateway and `:free` checks;
  - the logs command in doctor and `ferrule status`;
  - no color codes when stderr isn't a terminal;
  - `tests/live_fixes.rs` (8 end-to-end tests through the real binary);
  - the docs.

**Checks (part 3, before merging main):**
- fmt is clean, and so is clippy with `-D warnings`.
- `cargo test --workspace`: 635 passed, 0 failed, 2 ignored, in two full
  runs in a row.
- One earlier full run saw M21's
  `a_sub_agent_runs_on_a_named_connected_model_and_still_counts_toward_its_trees_budget`
  (`tests/models.rs`) fail once under load, and pass in the next three runs.
  It isn't touched here. Watch it in CI.

**After merging `origin/main` (M20 connections):**
- Resolved in `telegram.rs`: a button tap goes through `parse_update` as
  a `Parsed` with nothing unread.
- Resolved in `ferrule-mcp`: M20's transport already scrubs its errors.
- PLAN.md keeps both sides.
- fmt and clippy are clean, and `cargo test --workspace` has 681 passed,
  0 failed, 2 ignored.

**CI fix (windows-latest red, two pushes):** the telegram tests that read
the log went red on Windows (`no poll failed`). The first fix waited up to
20 s for the log line, and it failed again, so the timing theory was wrong.
The real cause: each test set a scoped `set_default` subscriber. With
parallel tests, scoped dispatchers come and go while other threads register
callsites. tracing's global interest cache then drops events at random.
It reproduced locally too: 3 of 25 runs of the gateway lib tests failed
with 16 threads, once in the token test and twice in the ignored-chat test,
which captured nothing. The fix is one global capture subscriber for the
test binary, installed once, writing to a per-thread sink, with the
interest cache rebuilt after install. After it: 0 fails in 40 runs with
16 threads and 40 runs with 64 threads. No product code changed.

**Eval** (mock, `ferrule eval run evals/starter --variant ab`, all 20 tasks):
- pass rate: engineered 20/20, naive 11/20, +45 pts;
- usage: 150 calls, 951.5k input + 6.2k output tokens;
- cost: $0.98 ($0.53 / $0.45).

The eval was run before the merge and again after it, with the same numbers
both times.

**Decisions for Max to confirm:**
- 409 threshold: 60 s (`[health] telegram_conflict_secs`). The same length
  of clean polling ends the episode.
- A webhook on the bot is deleted automatically at start, pending updates
  kept. The owner is told its host, never its path.
- Log default: `warn` from everything and `info` from ferrule's crates for
  the gateway. Other commands keep errors only.
- An ignored chat still gets the "This bot is private…" reply. The log warns
  once an hour per chat.
- A 404 with no tool endpoint isn't retried or failed over, because another
  try on the same model can't help.
- Doctor counts gateways on this machine only. A gateway on another machine
  shows up only as the 409.

**Open edges:**
- Voice transcription: what it would take is in the design doc.
- Photos for vision models.
- On Windows, doctor sees a second gateway only through the running marker.

**Unverified:**
- The logs command against a real systemd user unit.
- A real OpenRouter account and a real bot.
- macOS's `ps` scan and the end-to-end tests on macOS and Windows, until CI
  runs.

### 2026-09-25 — M22 dashboard (Devi, Opus 5.5)

The design is `docs/m22-dashboard.md`. Its **As built** section lists where
the build departs from it. The user guide is `docs/dashboard.md`. The work
is on branch `m22-dashboard`, cut from `main` at `cd614dd`, with a PR to
`main` (not merged).

It builds on:
- M19's hub (kill switch, caps, audit log, owner)
- M19b's `Health`, `RecentLog`, `/status` and `Redactor`
- M20's `Connections` and `ferrule_connections::tunnel`
- M21's `Models` API

The relay Worker wasn't touched, so the Cloudflare account is unchanged.

**Commits:**
- `7ddf1fb` design: the quick tunnel on demand (the relay is a mailbox,
  not a proxy), one-time links in the URL fragment, the session with CSRF
  and confirm, the page map, the read models and operations per section,
  polling, what's never shown, eval hermeticity, failure modes.
- `823b96c` part 1:
  - `Router::stop` ends one lane's turn. The deadline guard became a
    per-lane turn guard whose stop also drops the call in flight.
  - `Health` keeps the heartbeat's last result and the startup notice's
    text.
  - An interceptor's empty reply takes a message without answering it.
- `7c1c153` part 2: the core.
  - The 127.0.0.1-only server, the host allow-list, security headers.
  - One-time links (file-backed, SHA-256 on disk, 0600) becoming session
    cookies, CSRF and Origin on every POST.
  - `/dashboard` as the first door (owner only, the link only to the
    private chat), the quick tunnel that closes when idle, `/dashboard off`
    across processes, `ferrule dashboard`, the page shell.
- `f7d4b9d` part 3: the sections.
  - `/api/*` for all eight sections, and the page's JS for them.
  - `models::catalog`: OpenRouter's list, providers' `/models`, presets,
    an hourly cache with an offline fallback, the tool filter, `:free`,
    recommendations with a monthly estimate, add-as-default after a real
    test, `fill-prices` that never overwrites a hand-set price.
  - `TasksAdmin` (audited pause/resume/run now/delete) behind `ferrule
    tasks` and the page.
  - `ferrule model catalog|recommend|fill-prices`, the unpriced-model
    warning in `doctor`.
  - Unit tests, and `tests/dashboard.rs`: 7 integration tests on the real
    binary.
- part 4: log lines hide URL paths (the secret-scan test caught an MCP
  URL's path in a reqwest error); logs and extensions no longer poll
  (`every: 0` was read as 15 s); the docs, this entry, the roadmap.

**Checks (final, after merging `main`):** fmt clean; clippy `-D warnings`
clean; `cargo test --workspace` 692 passed, 0 failed, 2 ignored. Checked
on Linux. The macOS and Windows runs are this PR's CI.

**Eval (mock, `ferrule eval run evals/starter --variant ab`, all 20
tasks):**
- engineered 20/20, naive 11/20, +45 pts
- 150 calls, 951.5k input + 6.2k output tokens, $0.98 ($0.53 / $0.45)
- 13 compactions / 11 truncations; 4 failed checks fixed

That is identical to the run before M22. The eval's data dir had no
`gateway/dashboard.json` and no `private/dashboard/` afterwards.

**Size:** the page is 33.9 KB of embedded HTML/CSS/JS (`app.js` 30.4 KB).
There's no build step and no library.

**Defaults for Max to confirm:**
- `remote = "tunnel"`: `/dashboard` opens a quick tunnel when
  `cloudflared` is installed. `"off"` gives a local link and `ssh -L`.
- A link lives 10 min and works once. A session lasts 12 h at most and
  ends after 30 idle minutes. The tunnel closes after 30 minutes with no
  session and no unused link.
- The reference catalog is OpenRouter's public list, fetched only when a
  page or command asks, at most hourly. `[models] catalog_url = ""` turns
  it off.
- The recommended list (`crates/ferrule-cli/src/models/recommended.toml`, three tiers: value,
  strongest, free) is my pick as of today. Max should look at it.
- The catalog hides tool-less models by default. `:free` is flagged, not
  hidden.

**Open edges:**
- Sessions are in memory, so a gateway restart logs everyone out (by
  design). Unused links survive until they expire.
- A quick tunnel has no SLA. The page says so and `/dashboard` opens a new
  one.
- No "evaluate a candidate" button (out of scope; it needs the eval
  harness in the gateway).
- Extensions are read-only: no enable/disable operation exists for
  configured MCP servers.

**Unverified:**
- A real phone through a real quick tunnel. The tunnel path is M20's
  tested `tunnel::open`, and every test here ran on loopback.
- The live OpenRouter `/models` (tests use a recorded fixture). This
  container's TLS-intercepting proxy is in the way.
- RTL rendering was checked by construction (`dir="auto"` on every text
  that comes from outside), not on a device.
- macOS and Windows until this PR's CI run.

### 2026-09-25 — M23 native drivers (Devi, Opus 5.5)

The design is `docs/m23-drivers.md`. Its **As built** section lists where
the build departs from it. The user guide is `docs/models.md`, Drivers.
The work is on branch `m23-drivers`, cut from `main` at `59e0d96` (0.3.0),
with a PR to `main` (not merged).

**Commits:**
- `2293dae` design: three drivers, the neutral transcript with native
  blocks and their replay policy, Anthropic caching and thinking, usage and
  prices, stateless Responses, selection and back-compat, tests, failure
  modes.
- `4ae45cd` core: `NativeBlocks` on `Message` (replayed only in the
  current loop, cleared by compaction, redacted from logs),
  `cache_write_input_tokens` in `Usage` and the ledger,
  `CoreError::class()` and the refusal class.
- `ce38fd4` drivers: `anthropic.rs` (top-level system, tool_use and
  tool_result with multi-tool turns, id sanitizing, cache_control
  breakpoints, optional thinking passed back unchanged, the 16 000
  `max_tokens` floor, no temperature, 529/429/400 mapped to the M19b
  retry and M21 fallback classes) and `responses.rs` (`store: false`,
  `include: ["reasoning.encrypted_content"]`, effort, cached-token usage).
  Hermetic mocks on 127.0.0.1 with hand-written fixtures.
- `3006d39` selection: `api`, `thinking`, `effort`, `max_tokens` and
  `price_cache_write_per_mtok` in the config; one `client()` builder used
  by run/chat/gateway, `model test`, learn and eval; setup's Anthropic
  preset on the native driver; doctor, `model list` and the dashboard show
  the driver; `fill-prices` reads write prices.
- `9701fe2` tests: fallback mid-conversation Anthropic-with-thinking →
  Chat and Responses-with-reasoning → Anthropic through the real `Agent`
  and `RoutedProvider`; `model test` on each driver; two `#[ignore]` live
  smoke tests.
- docs: `docs/models.md`, the design's As built section, the roadmap, this
  entry.

**Checks:** fmt clean; clippy `-D warnings` clean; `cargo test --workspace`
753 passed, 0 failed, 4 ignored (two of them the new live tests). Checked
on Linux. macOS and Windows are this PR's CI.

**Eval (mock, `ferrule eval run evals/starter --variant ab`, all 20
tasks, real binary):**
- engineered 20/20, naive 11/20, +45 pts
- 150 calls, 951.5k input + 6.2k output tokens, $0.98 ($0.53 / $0.45)
- 13 compactions / 11 truncations; 4 failed checks fixed

That is identical to the run before M23 (the mock is on the Chat
driver). A first attempt counted 151 calls: the harness started before the
mock was listening, and one connection refusal was retried.

**Decisions for Max:**
- A v0.3.0 config pointing at `api.anthropic.com` moves to the native
  driver without being edited (why it's safe: design §7). `api = "chat"`
  keeps the old route.
- Thinking and reasoning aren't requested unless set (a model that thinks
  by default still does). Cache writes default to 1.25×
  input on the anthropic api.
- Responses' `max_output_tokens` is floored at 16 000 unless
  `effort = "none"`; a reasoning item without `encrypted_content` is
  dropped rather than replayed.
- The live tests default to `claude-sonnet-5` and `gpt-5-mini`.

**Open edges:** no streaming; no PDFs or citations; the 1-hour cache TTL,
preserved thinking across user turns and strict tools are follow-ups
(design §11). M25's router isn't built; its inputs (failure class, cost,
latency per call) are.

**Unverified:**
- Both drivers against the real APIs: this container has no Anthropic or
  OpenAI key. Max runs
  `ANTHROPIC_API_KEY=… OPENAI_API_KEY=… cargo test -p ferrule-providers --test live -- --ignored --nocapture`.
- The cache hit rate on a real account (the mock test checks the
  breakpoints and the arithmetic).
- A Responses-compatible host other than api.openai.com.
- macOS and Windows until this PR's CI run.

### 2026-09-25 — M24 the dashboard's leftovers (Devi, Opus 5.5)

The design is `docs/m24-dashboard-2.md`. Its **As built** section lists where
the build departs from it. The user guide is `docs/dashboard.md`. The work
is on branch `m24-dashboard-2`, cut from `main` at `59e0d96` (v0.3.0), with
a PR to `main` (not merged). `crates/ferrule-providers`, the eval starter
suite, its graders and the mock model, the README and the assets were not
touched.

**Commits:**
- `0d64e55` design.
- `23d7847` part 1: the login survives a restart. Sessions are in
  `<data>/private/dashboard/sessions.json` (0600, hashes only, `last_ms`
  written at most once a minute), and the CSRF token is derived from the
  cookie. Local sessions are bound to loopback, and `port = 0` tries the
  last port first. `ferrule dashboard revoke` is an alias of `off`. A
  tunnel session that was live at shutdown gets a new tunnel and link
  (at most one per 10 minutes).
- `b424602` part 2: evaluate a candidate. `ferrule model eval <model>
  [--suite smoke|starter] [--yes]` and the page's Evaluate show an
  estimate (catalog prices × `typical.json`, re-measured by a test), then
  confirm. The run obeys the kill switch and the day's caps, with its
  ledger rows stamped `eval:<run>`. It runs on its own thread and runtime
  with progress and Cancel, is stored like `ferrule eval`, and is shown
  beside the default's last result.
- `6ab5f8a` part 3: edit from the page. `settings_admin` and
  `tasks_admin` are shared by the page, Telegram (`/caps`, `/mcp`,
  `/skills`, `/hooks`) and the CLI (`trust caps --set`, `mcp
  disable|enable`, `skills disable|enable`, `tasks schedule`, `tasks
  model`). Every change is locked, validated and audited. Raising a cap,
  disabling or removing an MCP server, and trusting hooks ask first. Hooks
  trust is pinned to the SHA-256 the owner saw, with a diff. Caps are live
  in the hub, and skills and MCP follow without a restart.
- `07281fa` part 4: `scripts/dashboard-smoke.sh` and `.ps1` (a shared
  stdlib driver, `scripts/dashboard_smoke.py`), and the RTL test. That
  test found the running turn's activity line without `dir="auto"`.

**Checks (after merging `main` with M23):** fmt clean; clippy `-D
warnings` clean; `cargo test --workspace` 782 passed, 0 failed, 4 ignored
(746 before the merge). The merge needed one fix: `ferrule model eval`'s
estimate now passes M23's cache-write price through (0 tokens written,
since the mock writes no cache). One run hit a known flake,
`a_browser_that_hangs_is_given_up_on` (ETXTBSY, the fake Chrome was
exec'd while a parallel test was still writing it). That file is
unchanged from `main`, and the test passed 3 out of 3 re-runs. Checked on
Linux. The macOS and Windows runs are this PR's CI.

**Smoke script, run here:** 7 PASS, 1 SKIP (tunnel: no `cloudflared`), 1
FAIL (catalog). The catalog fails only in this container: its HTTPS proxy
re-signs traffic with a CA that reqwest's built-in roots (rustls +
webpki-roots) don't trust. On a normal network it's one GET to OpenRouter.

**Eval (mock, `ferrule eval run evals/starter --variant ab`, all 20
tasks):**
- engineered 20/20, naive 11/20, +45 pts
- 150 calls, 951.5k input + 6.2k output tokens, $0.98 ($0.53 / $0.45)
- 13 compactions / 11 truncations; 4 failed checks fixed

That is identical to the run before M24, both before and after merging
`main`. The eval's data dir had no
`gateway/dashboard.json`, and `private/` was empty.

**Decisions taken alone, for Max to confirm:**
- A tunnel session can't survive a restart (the new tunnel has a new
  name). The owner is sent a new link only if one was live, at most one
  every 10 minutes.
- Lowering a cap needs no confirm; raising one or setting it to 0 (no
  cap) does.
- Agent-installed MCP servers (M13) can be removed from the page but not
  disabled, because their lock file's "Suspended" means "the scan flagged
  it".
- A task's model is stored as typed, so an alias keeps following its
  target.
- Hooks trust stays off Telegram; `/hooks` points to the page and the
  CLI.

**Not verified live:** the cloudflared tunnel (not installed here), the
live OpenRouter catalog (see above), a real phone, and a real Telegram
bot. Each of these is behind a mock or the smoke script.

### 2026-09-25 — M25 routing Phase 1: start cheap, escalate when needed (Devi, Opus 5.5)

**Scope.** Built Phase 1 of `docs/research-routing-and-local-models.md` on
branch `m25-routing`: design (`docs/m25-routing.md`), core, CLI, surfaces,
the eval variant and the user guide (`docs/routing.md`). Phases 2–3 stay
dropped.

**What it does.** Each turn starts on the cheap tier (or a tier-ref floor)
and moves up one tier per failure signal: `call_failed:<class>` (the same
request goes again one tier up), `tool_errors` (2 in a row),
`check_failed`, `stop_hook`, `no_progress` (the same call 3 times or the
stuck nudge), `watchdog`, `owner` (`/model strong`, next turn only).
Sticky for the turn, back to the floor at the next one unless
`de_escalate = false`. Rows carry `route: {tier, escalated}`; every call
is priced at the model that served it. Off by default: a golden session
written by the pre-M25 code is replayed unchanged.

**Measured (mock, real binary).** `eval run evals/starter --variant ab`:
engineered 20/20, naive 11/20, $0.98, unchanged. `--variant routing
--cheap weak/mock --strong mock/mock` (the weak mock ignores failed
checks): cheap 16/20 $0.05, routed 20/20 $0.06 with 4 escalations (all
`check_failed`), strong 20/20 $0.53.

**Decisions for Max:**
- Auth errors and outages (429, 5xx, timeouts) don't escalate; M21's
  fallback handles them, and it composes with the tier chosen (design §5).
- One tier per signal, not straight to the top.
- `/model strong` is one turn; a lasting "strong" is the pin
  `/model tier:strong`.
- `strong_daily_usd` counts spend above tier 0 per UTC day, seeded from
  the ledger. Past it, floors above 0 also start on tier 0; concrete pins
  aren't limited.
- A lane's context window is the smallest of its tiers', so compaction
  holds whichever tier answers.
- Eval: the three arms share the cheap model's harness profile; the
  strong model judges rubrics unless `--judge-provider` is given; routed
  uses `Policy::default()` (every trigger on); a regression suite gates on
  routed; the default pair comes from `[routing] tiers`, config only.
- The gateway now attaches the trust hub to the models at start, so a
  model or routing change made before the first turn is audited too (a
  gap that predates M25).

**Unverified:**
- A real cheap/strong pair: no live comparison was run here. `docs/routing.md`,
  Measuring it, has the commands, the expected cost ($5–15 a full repeat at
  $3/$15 strong; $1–2 for the smoke subset) and the ignored live test.
- Escalation from a real model's failure classes (the scripted tests
  cover each class).
- macOS and Windows until this PR's CI run.

**Open edges:** a verifier sub-agent's verdict isn't a signal (free
text); the report's "failed checks fixed" line sums failed checks whether
or not the task then passed (pre-existing, visible on the cheap arm).

### 2026-09-26 — M26 isolation: a Windows sandbox, sandboxed reads, the M10 edges (Devi, Opus 5.5)

**Scope.** On branch `m26-isolation`: the design (`docs/m26-isolation.md`),
then four parts. Part 1: the read policy. Part 2: plain HTTP through the
proxy. Part 3: Windows tier 1. Part 4: hide-only unconfined servers,
doctor and `ferrule sandbox` on Windows. Then the guides
(`docs/sandbox.md`, `docs/windows-sandbox.md`).

**What it does.**
- Commands, MCP servers and the file tools can no longer read ferrule's
  secrets, the usual credential dirs or the owner's `deny_read` paths.
- Windows gets a real sandbox without admin: writes are confined, and
  the job kills the whole tree.
- `http://` fetches go through the proxy under the same rules as HTTPS.
- A `sandbox = false` server still can't read the secrets.

**Measured (mock, real binary).** `eval run evals/starter --variant ab`:
engineered 20/20, naive 11/20, $0.98, unchanged. 835 tests pass on Linux
after merging main (M25 included).

**Decisions for Max:**
- On Windows, the default credential dirs (`~/.ssh`, the cloud dirs,
  browser profiles) are closed only to the file tools. Ferrule doesn't
  rewrite ACLs it doesn't own: OpenSSH checks them, and browsers own
  theirs. Adding a path to `deny_read` opts it into the protected DACL.
- `network = false` on Windows keeps the backend active and warns, rather
  than refusing to start: writes and reads are still worth confining.
- Plain-HTTP secrets go to loopback only; a bound remote host gets 403
  instead of a cleartext placeholder request.
- Commands keep `HTTPS_PROXY` only (no `HTTP_PROXY`), so local dev servers
  aren't affected.
- `sandbox = false` now means hide-only, not wide open. Without any
  backend it stays fully open, as before.
- No separate §2.3 probe test: the real tier-1 tests settle whether the
  conditional ACE holds.
- Git Bash can't run under the write-restricted token: MSYS ACLs its
  own pipes and shared memory to the user SID. It degrades to
  unsandboxed with the warning, as the brief says, and doctor suggests
  `FERRULE_SHELL=powershell`, which runs commands sandboxed. There is no
  automatic switch, because it would silently change the syntax the model
  writes.

**Unverified:**
- All of Windows until this PR's CI run (nothing Windows runs in the build
  container).
- The macOS hide-only profile until CI.
- The system-service path, end to end.
- Plain HTTP to a real remote host (the tests use loopback servers).

**Open edges:**
- `HTTP_PROXY` for commands.
- Windows network enforcement (WFP, admin).
- The Landlock hide-only limit: no new entries directly beside a denied
  path, such as at the top of `~`.
- The Low-integrity fallback, if the conditional ACE fails somewhere.
- Git Bash under hide-only, so its reads stay confined even though its
  writes can't be.
- The README's Sandbox section still says reads are open (README
  untouched by rule).

### 2026-09-26 — M27 speed (Devi, Opus 5.5)

**Scope.** Built `docs/m27-speed.md` on branch `m27-speed` in five
commits: parallel read-only tool calls, streaming in the three drivers,
progressive replies on Telegram and in `ferrule chat`, a cache-stable
prompt prefix, and the measurement in `ferrule ledger`. User guide:
`docs/speed.md`.

**What it does.** Read-only tool calls in one response run together (up
to `[agent] parallel_tools`, default 4); writes run alone, in order, and
results go back in the order asked. The chat, anthropic and responses
drivers stream when the caller passes a sink; a server that ignores
`stream` still works. Telegram shows a reply once there's a line (or after
1 s), edits it about once a second, splits before 4096, honours 429
`retry_after` and falls back to a new message when an edit fails. A
pre-tool "let me look…" is replaced (`Delta::Reset`). The switches are
`[agent] stream` and `[gateway] telegram_stream`. The prefix is stable:
recalled memory is now a user message right after the goal, not part of
the system prompt, and the Anthropic driver's third breakpoint sits on
the previous turn's last wire message. Rows record `speed:
{first_token_ms, first_visible_ms, tool_batch}`, and `ferrule ledger`
prints the cache hit, the first-token and first-reply p50s, and parallel
batch wall time against the summed time.

**Measured (mock, real binary).** `eval run evals/starter --variant ab`:
engineered 20/20, naive 11/20; 150 calls, 951.5k input + 6.2k output
tokens, $0.98 ($0.53 / $0.45). Unchanged to the token: the eval doesn't
stream, its tasks make one call per response, and the starter suite loads
neither session recall nor the memory tools, whose `remember` description
changed. `cargo test --workspace`: 853 passed.

**Decisions for Max:**
- Memory goes after the goal, not before it as the design first said: the
  goal stays the first user message (sessions, `/goal`, compaction and the
  title key off it). Compaction carries the memory verbatim; truncate mode
  drops it with the old turns.
- `first_visible_ms` is measured at the agent (first non-empty text delta
  when a reply stream exists), not at the channel. Telegram shows it at
  most about a second later.
- `tool_batch` is recorded only for 2+ calls in a response; the ledger's
  line counts only batches that actually ran in parallel.
- Anthropic breakpoint 3 was placed per user message, so with a hook note
  or memory after the goal it landed on the new turn instead of the old
  one. It's now one mark per wire message, with a test.
- The `remember` tool says facts "are recalled at its start" instead of
  "added to its system prompt".
- A stdio MCP server gets one call at a time.

**Unverified:**
- Real Telegram (edits, 429s, the 4096 split); the gateway tests use a
  scripted channel.
- Real provider streaming and a real cache hit. The drivers are tested
  against scripted SSE, and the prefix against the request bytes.
- macOS and Windows until this PR's CI run.

**Open edges:** Telegram's non-streamed `send` still doesn't split replies
over 4096; in `ferrule chat` the streamed text can interleave with tool
event lines; memory is lost under truncate mode; `sendMessageDraft`, a
1-hour cache TTL and formatting mid-stream are left for later
(`docs/roadmap.md`).

### 2026-09-26 — M28 search and skills (Devi, Opus 5.5)

**Scope.** Built `docs/m28-search-skills.md` on branch
`m28-search-skills` in four commits: two fixes, `web_search`, its
follow-up after merging M27, and keyword-triggered skills. User guides:
`docs/web-search.md` and `docs/skills.md` (there was no skills doc
before).

**Decisions for Max:**
- `web_search` stays in plan mode, like `web_fetch`.
- A search without a set price is recorded at $0; every sent search is a
  row; the gate also honours the kill switch and dollar caps.
- Searches running in parallel each hold a place under the daily cap
  until recorded (two minutes at most, for a stopped turn).
- `doctor` makes no paid search; `&` isn't escaped in results.
- Hebrew prefixes up to 4 letters (the design said 3; `וכשהשחרור` needs
  4).
- A too-large match counts toward `max_triggered`, a refused one doesn't.
- Trigger vetting re-scans and checks the lock but not the install name
  rules.
- Sub-agents never trigger. A triggered skill counts as active for
  `activate_skill`.

**Unverified:** live searches against the four providers (mocked, live
tests `#[ignore]`); triggers on a real Telegram chat; macOS and Windows
until this PR's CI run.

**Open edges:** `web_fetch` output isn't fenced or escaped, so a page can
forge a `<skill_content>` block and suppress (not cause) a trigger;
scheduled prompts can't trigger by design; no native provider search.

### 2026-09-26 — M29 edit mechanics (Devi, Opus 5.5)

**Scope.** Built `docs/m29-edit-mechanics.md` on branch
`m29-edit-mechanics` in five commits: `edit_file`, the repo map and
`code_search`, per-edit lint, optional auto-commit with undo, and the
eval's `--edit-tools` flag. User guide: `docs/editing.md`.

**What it does.** `edit_file` applies SEARCH/REPLACE hunks all or nothing:
exact, then trailing-whitespace-insensitive, then a uniform indentation
offset, and never fuzzy on content. A miss shows the closest region with
line numbers; line endings, BOMs, UTF-16 and non-UTF-8 bytes round-trip;
the write is atomic, and so is `write_file`'s now. The new
`ferrule-codemap` crate parses Rust, Python, TS/JS, Go and Java with
tree-sitter, ranks definitions Aider-style (PageRank personalised by what
the conversation mentions) into a map of `repo_map_tokens` (1024), and
serves `code_search`. Both appear only in a workspace that looks like a
code repo. `LintHook` is a built-in PostToolUse hook that runs the
project's own linter after an edit. `[agent] auto_commit` commits exactly
the files a run made dirty, and `ferrule undo` / `/undo` revert the
latest agent commit.

**Measured (mock, real binary).** `eval run evals/starter --variant ab`:
engineered 20/20, naive 11/20; 150 calls, 951.5k input + 6.2k output
tokens, $0.98 ($0.53 / $0.45), identical with `--edit-tools write-only`.
The mock never calls `edit_file` and prices only messages. A real provider
pays ~227 more input tokens per call for the schemas (`edit_file` ~205,
`write_file`'s longer description ~22), ~34k over the A/B.
`cargo test --workspace`: 969 passed (after merging M28's `main`).

**Decisions for Max:**
- `write_file` stays in every profile. The edit advice is in the tool
  descriptions, so the system prompt's bytes (M27's pinned prefix) don't
  change.
- The repo map is a user message appended only when it changes, not part
  of the system prompt (which would lose the prompt cache on every edit).
  It isn't refreshed inside a run's tool loop; `code_search` always is.
- JavaScript is parsed with the TSX grammar, and C# is left out: its
  grammar is several MB.
- Lint defaults to `auto`: a linter runs only where the project has its
  config file, so the model isn't pushed to restyle code.
- Auto-commit is off by default and uses a new `ferrule/auto-*` branch
  unless `auto_commit_branch = "current"`. Files dirty at the run's start
  are never committed. Git runs in the sandbox, and the repo's own hooks
  apply. Sub-agents never commit.
- `--edit-tools` is shown on stderr only. The saved run doesn't record it,
  since that would touch the report code M28 is changing.

**Unverified:**
- No real model has been offered `edit_file` yet. The command and its
  expected cost (~$1–3 with gpt-5-mini) are in `docs/editing.md`.
- The real linters' output: the tests run stand-in scripts named
  rustfmt, ruff, gofmt, eslint and tsc.

**Release.** All five targets built on the branch's `release.yml` run
(actions run 36224246208). The grammars add 5.35 MB (+28%) to the stripped
binary, and `--no-default-features` drops them.

**Open edges:** the saved eval history doesn't record `--edit-tools`; the
map isn't refreshed mid-run; a workspace that is a subdirectory of a repo
auto-commits only where the sandbox lets git write the parent's `.git`;
no C#.

### 2026-09-26 — M30 vector recall (Devi, Opus 5.5)

**Scope.** Built `docs/m30-vector-recall.md` on branch
`m30-vector-recall`, in five commits: design, `ferrule-embed`, vectors
and hybrid recall in `ferrule-memory`, the recall benchmark, and the CLI
wiring (config, tools, session recall, reindex, model download, doctor,
setup). The user guide is `docs/memory.md`.

**Decisions for Max:**
- Our own ~150-line model2vec embedder on `tokenizers` (pure Rust, no
  ONNX or candle); the matrix is read row by row, not loaded.
- `[memory] embedder` defaults to `"off"`; setup recommends local.
- The default merge is weighted 0.7 with a 0.3 cosine floor. The sweep
  favoured a lower floor and weight 0.9 slightly, but that adds more
  unrelated facts per prompt on a small fixture.
- The embedding price comes only from `[memory] price_input_per_mtok`
  (never borrowed from the chat provider); an unset price is an unpriced
  row.
- The near-duplicate hint fires at cosine ≥ 0.8 (checked on the real
  model).
- The goal isn't embedded for an empty store.
- The benchmark persona is a made-up "Omer", so no real details sit in
  the public repo.

**Checks.** 1009 tests after merging main (M29), 11 ignored. The starter
eval against the mock through the real binary, with no embedder
configured, is unchanged: engineered 20/20, naive 11/20, $0.98. Binary
growth is +2.71 MB, all of it the default-on `local-embed` feature (as-built
notes in the design doc).

**Unverified:** a real paid `/v1/embeddings` provider (mocked through the
real proxy); `ferrule setup`'s download on a live terminal; macOS and
Windows until this PR's CI run.

**Open edges:** the kill switch and caps aren't checked before an
embedding request (rows still count afterwards); the CLI reindex/search
rows bypass `trust::equip`'s sink; cross-lingual recall is 3 in 10 at r@5;
brute-force cosine past ~50k rows.

### 2026-09-26 — M31 Discord and Slack (Devi, Opus 5.5)

**Scope.** Built `docs/m31-channels.md` on branch `m31-discord-slack` in
six commits: the design, core seams (`ChatRef` owners, `Channel` additions,
the shared access/pairing rules), the Discord adapter, the Slack adapter,
the CLI wiring (config, setup, doctor, status, dashboard, trust), and the
docs with doctor's Slack scope check. User guides: `docs/discord.md`,
`docs/slack.md`. Hermetic tests run a mock Discord Gateway + REST, a mock
Slack Socket Mode + Web API, and one daemon answering Telegram, Discord and
Slack at once, with a dead Discord socket beside two live channels.

**Decisions for Max:**
- Shared channels are mention-only even when allow-listed; Slack answers
  in a thread, Discord as a reply.
- The daemon never registers Discord slash commands (a mutating call);
  setup does. Slack has one `/ferrule <command>`.
- The pairing code is six digits and exists only while setup waits
  (two minutes).
- The dashboard shows a dead channel through its problems list and a
  `problem` field in the API; `app.js` wasn't touched, so there's no new
  table column.
- Setup's Discord and Slack questions default to "no" in the guided path.
- Telegram keeps text approvals and its strings; its only visible
  addition is `/undo` in the command list.

**Unverified:** no real Discord bot or Slack app has been connected (the
live tests are `#[ignore]`d; commands in the guides); the Slack manifest
hasn't been pasted into a real workspace.

**Eval.** `eval run evals/starter --variant ab` against the mock model:
engineered 20/20, naive 11/20, $0.98, same verdict on every task as the
last run.

**Open edges:** Telegram approval buttons; a channel column in the
dashboard page; WhatsApp (the options are in the design, §9); files and
voice on Discord/Slack.

### 2026-09-26 — M32 WASM tool plugins (Devi, Opus 5.5)

**Scope.** Built `docs/m32-wasm-plugins.md` on branch `m32-wasm-plugins`
in eight commits:
- the design;
- the `ferrule-plugins` runtime;
- the M19 gate for tools that declare approval;
- the install flow in `ferrule-extensions`;
- the CLI and doctor;
- the SDK;
- the examples;
- the proxy test.

The user guide is `docs/plugins.md`.

**Decisions for Max:**
- wasmi over wasmtime/extism, for size: +1.2 MB against +9.4/+16.2 MB on
  x86_64-musl. Calls run at interpreter speed, which suits tool glue and
  doesn't suit heavy compute (design §1).
- A JSON core-module ABI rather than WIT/WASI. Rust is the only tested
  language. TinyGo, AssemblyScript, Zig and C should work; javy and
  componentize-py can't (they need WASI or components).
- An allow-listed plugin installs without the owner only when it asks for
  nothing beyond clock/random. Files, http or secrets always reach the
  owner, and widening goes back to them.
- An agent-proposed local directory must be inside the workspace and
  always queues.
- `/status` and the dashboard don't list plugins, because the health
  report can't see the extension manager. `plugins list`,
  `extensions list` and doctor cover it.
- The SDK has `publish = false`: the repo has no license yet.
- The example `.wasm` files are committed. They are built reproducibly,
  so CI never needs the wasm target.

**Checks.** 1120 tests after merging main (M31), 15 ignored; fmt and
clippy `-D warnings` are clean. The starter eval against the mock through
the real binary is unchanged: engineered 20/20, naive 11/20, $0.98.

**Unverified live:** a `url:` install from a real HTTPS host, and the
`github-repo` example against the real GitHub (both are `#[ignore]`d or
mocked through the real proxy). macOS and Windows are unverified until
this PR's CI run.

**Open edges:** no per-plugin ledger rows (the `http` op is traced, not
billed). The committed example binaries can go stale against the SDK,
and the ignored rebuild test is what notices. There is no TinyGo or
AssemblyScript example.

### 2026-09-26 — M33 ops: egress policy, OTel export, importers (Devi, Opus 5.5)

**Scope.** Items 16–18 of `docs/research-number-one-harness-strategy.md` §4,
built from `docs/m33-ops.md` on branch `m33-ops` in four commits: the
design, the egress policy with the Unix-socket allowlist, OTel export, and
`ferrule import`, then the user guides `docs/egress.md`, `docs/otel.md` and
`docs/migrate.md`. The SSH backend is M34; the shell's exec path is
untouched. `ferrule_tools::egress::client_builder` keeps its signature, so
M32's plugins inherit the policy. No new third-party dependency in the
release binary (`tempfile` is dev-only), so no `release.yml` run.

**Decisions for Max:**
- The default policy stays "allow public" and blocks only private ranges
  and cloud metadata; an allowlist is opt-in (`default = "deny"`, or
  setup's "package hosts only").
- Shell commands get the proxy only when secrets are live or `[egress]`
  has rules, so an upgrade doesn't reroute `pip install`. For commands
  the proxy stays advisory; a binding `enforce` mode is a follow-up.
- Loopback is private for the model's tools (`web_fetch` SSRF) but open
  to shell commands (their own dev server).
- Unresolvable names behind a corporate upstream proxy are allowed and
  logged, not blocked.
- Denials over CONNECT are answered inside the tunnel with ferrule's CA,
  so the model reads the reason; `web_fetch` now also prefixes other
  non-2xx pages with `HTTP <status>` (a small behaviour change).
- X11 isn't in the default socket allowlist (keystroke injection).
- OTel is a hand-rolled OTLP/JSON encoder, not the `opentelemetry` crates
  (25–60 crates, breaking minors). No gRPC/protobuf, no metrics or logs.
- The importers bring their own JSON5 and YAML-subset readers rather than
  `json5` + `serde_yaml`. Skills need a terminal, one confirmation each;
  there's no auto-approve flag. Memories that look like they carry a
  secret are held back, even at the cost of some false positives.
- Per-agent egress policies were left out (the sandbox is process-wide).

**Unverified live:**
- the Linux Unix-socket supervisor can't run in this container (Docker's
  seccomp profile refuses `pidfd_getfd`); its test is mandatory on the
  Linux CI runner;
- the macOS Seatbelt socket rules were checked only by the macos-14 CI
  runner (listed: ok; unlisted and symlink: `EPERM`), never by hand;
- no real OTel collector and no real OpenClaw or Hermes install; the live
  tests are `#[ignore]`d and the guides give the commands.

**Eval.** `eval run evals/starter --variant ab` against the mock model:
engineered 20/20, naive 11/20, $0.98.

**CI fixes.** The e2e wizard answers the new Network policy question.
`hidden_keys.py` checked that `proxy/keys/ca.key` doesn't exist, but the
default egress policy now starts the proxy without secrets, so ferrule's
own CA is there. It now checks for the model's planted content instead.

**Open edges:** `enforce` mode for shell egress; per-agent policies;
egress denials as span events; `ferrule telemetry replay`; importing MCP
server definitions, scheduled jobs and OpenClaw's SQLite pairing
approvals; Unix sockets on Windows.

### 2026-09-26 — M34 SSH workspaces and local-model first run (Devi, Opus 5.5)

Branch `m34-ssh-local`. Design and as-built notes are in
`docs/m34-ssh-local.md`; the user guides are `docs/ssh.md` and
`docs/local-models.md`, with pointers from `docs/sandbox.md` and
`docs/models.md`.

**Built:**
- `edit_file`'s hunk matching as a byte-level `edit_bytes`, so the remote
  edit applies hunks with the same code.
- `ferrule-ssh`: targets (`ssh:<name>`, `ssh://…`), the link (a
  ControlMaster on Unix that also holds the proxy's `-R`, reconnect with
  backoff, nonce markers, the process-group watchdog), the stderr failure
  classes, host-key trust through keyscan and fingerprints, and the five
  remote tools with the M26 deny list resolved on the remote.
- Tests against a real `sshd` on 127.0.0.1: tools, trust, a changed key,
  auth, a dropped connection, `/stop`, the timeout and caps, and the proxy
  forward. Linux CI requires them.
- The CLI: `workspace`/`--workspace`, `ferrule ssh list|trust|test`, a
  setup step, doctor checks per host, the `/status` line, and an e2e run
  that greps the transcript, ledger and logs for the test key.
- Local models: detection of Ollama, llama.cpp, LM Studio (REST v1 and
  v0) and vLLM; each model's trained and effective window against the
  planned one, with each server's fix; the tool probe; the Ollama derived
  model on a yes; `HarnessProfile::fitted`; the dated known-good list;
  doctor, setup and `/status`.
- The dashboard's health lists a down link and the local-model problems,
  each with its fix.

**Decisions for Max:**
- The system `ssh` over russh or ssh2: no growth, no native build
  dependency, and it honours the user's agent, ssh config, jump hosts and
  hardware keys (design §1). Windows pays a handshake per command.
- Tasks and sub-agents follow the parent's remote and share its directory.
  A per-task workspace and remote worktrees are follow-ups.
- Plan mode, `read-only` and read-only children get **no** remote shell,
  since the local sandbox can't hold a remote one to reading.
- The Ollama fix is a derived `<model>-32k` model made only on a yes;
  `OLLAMA_CONTEXT_LENGTH` is printed, never set. ferrule's OpenAI
  endpoint can't pass `num_ctx` per request.
- A window smaller than the planned one is a doctor **failure**, not a
  warning: it's the silent-truncation case.
- The known-good list is the Ollama library's `tools`-tagged models on
  2026-09-26, not measured here (no GPU).
- `fitted()` isn't applied in the eval's own window path, so the eval
  can't move.

**Checks.** 1225 tests after merging main (M33), 18 ignored, with the
sshd tests required; fmt and
clippy `-D warnings` are clean. The starter eval against the mock through
the real binary is unchanged: engineered 20/20, naive 11/20, $0.98.

**Unverified live:** a real remote host across a real network (only
127.0.0.1 sshd), the Windows OpenSSH path (CI runs only the tests that
need no sshd), and real Ollama, llama.cpp, LM Studio and vLLM servers
(mocks with the documented API shapes). The macOS sshd tests run only if
the runner's sshd starts unprivileged.

**Open edges:** setup's local-model check runs in "Add a provider" and
not in "Change the model"; `/status` watches the default model only; no
remote sandbox, remote `code_search`, per-task workspace or streaming.

### 2026-09-26 — Two flaky tests (Devi, Opus 5.5)

Branch `fix-flaky-tests`. A cleanup run, not a milestone: no design or
user doc, no roadmap change.

**`a_sub_agent_runs_on_a_named_connected_model_and_still_counts_toward_its_trees_budget`
(models.rs) — a real race in the M12 wake-up path, fixed in the
product.** `Supervisor::busy` read the `finishing` counter *before* the
store. A child that stopped between the two reads looked neither running
(the store already said idle) nor finishing (the counter was read before
the child bumped it). `ferrule run`'s loop in `run_root` then saw no news
and a quiet tree, closed the tree, and printed the root's "waiting" answer.
The child's notice reached the channel a moment later, after nobody was
listening. A trace of the failing runs showed exactly that order:
`finished` → store idle → loop break → notice pushed. How a user hits it:
`ferrule run` with a sub-agent the root doesn't `wait_agent` for, when the
child finishes just as the root's turn ends. The run exits early, and the
child's work is closed unread. The fix reads the store first. A run bumps
`finishing` before it records its end and drops it only after the wake-up
is sent, so a stopped row means the notice is either counted or already
queued. `wait` already read in that order. The gateway doesn't call
`busy`, so only `ferrule run` (and the plan-approval path through
`run_root`) was affected.
Before: 25 failures in 960 runs of the single test, 8 at a time (9/480 and
16/480); the CI failure on PR #28 (ubuntu, run 36249487472) had the same
`final: ROOT_WAITING`. After: 0 of 1000 the same way.

**`the_watchdog_tells_the_owner_about_another_chats_stuck_turn`
(health.rs) — the test assumed timing; the product was right.** Windows
CI (run 36240639111, commit 6587f97) got `Stuck on a model call for 1 s`
where the test wanted the shell tool's stall. With `watchdog_after_secs =
1`, the model-call phase (recall, hooks, the HTTP round trip) was quiet
for a second on the Windows runner. That is a stall of its own, told once,
and the tool starting re-arms the watchdog for the second one. It never
failed locally: 0 of 256 under 16 CPU hogs and 32 parallel copies. A
scripted model that answers after 1.5 s reproduces the Windows message
exactly. The test now waits for the notice that names the `shell` tool
and accepts any whole-second duration. With the 1.5-s model it passes.
After: 0 of 200 runs, 8 at a time under 8 CPU hogs.

**Checks.** 20 full-suite runs (every test binary, 4 suites at a time):
both tests passed all 20. One unrelated failure turned up, listed below.
1225 tests, 18 ignored. fmt and clippy `-D warnings` are clean. The starter
eval against the mock through the real binary is unchanged: engineered
20/20, naive 11/20, $0.98.

**Other flaky tests seen, not fixed (different causes):**
- `a_dropped_socket_resumes_with_the_session_and_last_sequence`
  (ferrule-gateway/tests/discord.rs:249), 1 of 20 suites. The mock counts
  `resumed` just before it *sends* RESUMED. The client clears
  `problem()` only when it *reads* it. The test asserts `problem()` is
  None in between. It should `until(…, || r.ch.problem().is_none())`, as
  slack.rs does.
- From PR #28's CI history (macOS): `a_dropped_link_is_interrupted_and_the_next_command_reconnects`
  (ferrule-ssh/tests/sshd.rs:514), `timeouts_and_output_caps_match_the_local_shell`
  (sshd.rs:585) and `a_run_over_ssh_works_and_the_key_never_leaves_ssh`
  (ferrule-cli/tests/ssh.rs:407). M34's last two commits addressed the
  output-cap one, and none failed on the merged head.

### 2026-09-27 — M35 subscription sign-in: a ChatGPT plan and a Claude plan (Devi, Opus 5.5)

**Scope.** Setup offers "sign in with your ChatGPT plan" and "sign in with
your Claude plan" next to "paste an API key" and "a local model". Design
and as-built notes: `docs/m35-subscriptions.md`. User guide:
`docs/subscriptions.md`, linked from `docs/models.md` and `docs/migrate.md`
(the "OAuth logins not carried over" line now says how to sign in again).
Branch `m35-subscriptions`, one commit per part (design, A–F).

**The rules it's built on.** Anthropic's terms forbid third-party apps
from offering Claude.ai login or routing requests through plan credentials,
and allow "an end user signing in to the unmodified Claude Code binary with
their own Claude subscription". So every Claude-plan model request comes
from the unmodified `claude` binary; ferrule never calls the Anthropic API
with a plan token, and there is no Claude sign-in or token paste in any
chat. OpenAI tolerates third-party harnesses on a ChatGPT plan; the guide
calls that tolerance, not a contract.

**What was built.**
- A: `ferrule-plans`: ChatGPT sign-in (device code, PKCE through a loopback
  listener, a pasted redirect), the sealed store, refresh with rotation
  under a cross-process lock, logout that revokes, the usage file.
- B: the Codex flavour of the Responses driver (the plan's token, one
  refresh on a 401, the usage limit as a wait until the reset so M21 falls
  back, the rate-limit windows).
- C: ChatGPT surfaces: `ferrule login|logout chatgpt`, `/login chatgpt` by
  device code in the owner's chat only, the setup model step's four ways,
  doctor, plan-aware readiness in models/routing/dashboard, ledger rows at
  $0 with `plan` and `notional_usd`, the owner told once when a sign-in
  expires.
- D: the Claude Code engine: `claude -p` (never `--bare`) with ferrule's own
  config dir and `--resume` per chat; the child's env stripped of API keys,
  base URLs and outranking switches; claude's permission prompts asked of
  the run's guard; ferrule's tools bridged over MCP (a bridged call pauses
  the turn and runs through the agent's own gates); process-group kill,
  timeout, output cap; rate-limit events and a rejected limit with its
  reset.
- E: Claude surfaces: `ferrule login claude` (claude's own
  `auth login --claudeai`, or `--token` for a setup-token), `logout`, the
  state from `claude auth status --json`, doctor, the hidden `claude-mcp`
  relay, an exported `CLAUDE_CODE_OAUTH_TOKEN` taken out of ferrule's env
  at startup, claude's dirs hidden from ferrule's commands, Anthropic's
  hosts through egress, and a `sk-ant-oat` key refused as an API key on
  every call, in setup and in doctor.
- F: the live test, the guide, the docs.

**Decisions taken alone (for Max to overrule).**
- `claude -p` rather than the Agent SDK: the SDK is a Node/Python library a
  Rust runtime would drive through a Node child anyway.
- Way (2), a pasted setup-token, is offered at all, sealed, and the guide
  says plainly that it is storing a Claude credential. Ways (1) and (3)
  are recommended for anyone who wants no grey area. It exists because a
  gateway running as a service has no shell environment.
- Claude Code's session id rides in the reply's native blocks rather than
  a separate `sessions.json`.
- The ChatGPT sign-in uses the Codex CLI's public client id, read from
  `openai/codex` at a pinned commit.
- Claude-code models get a 1,000,000-token window by default so ferrule's
  compactor never cuts a resumed session; claude compacts itself.
- `~/.claude` is hidden from ferrule's own commands only when a
  claude-code provider exists, not added to M26's default denies.
- A `sk-ant-oat` key forces the Anthropic driver whatever `api` says, so
  the refusal can't be bypassed by `api = "chat"`.

**Live.** One run of the `#[ignore]`d
`live_two_chat_turns_through_the_real_claude_with_the_sandbox_on`: claude
2.1.283, Haiku, ferrule's sandbox in its default mode, egress through
ferrule's broker chained to the host's proxy. Transcript (no secrets):

```
you> Remember the word ferrule-otter. Reply with just: noted.
▶ remember {"content":"ferrule-otter"}
  ✓ remember (15 chars)
agent: noted.
you> Which word did I ask you to remember? One word.
agent: ferrule-otter
```

Three ledger rows at `cost_usd` 0, `plan = "claude-code"`: the paused
row of turn 1 (the bridged `remember`), its resumed row (notional
$0.023), and turn 2 (notional $0.0036, 23.5k of 23.7k input tokens
cached). An earlier run of the same test spent two more turns. It
passed the resume check but tripped an exact row count that didn't
allow for the bridged call; that count was loosened.

**Not verified live.** The ChatGPT plan end to end (this container can't
reach `auth.openai.com` or `chatgpt.com`; the protocol comes from the Codex
source at a pinned commit and is tested against mocks); `ferrule login
claude` through Anthropic's own screens (the live run used an exported
token); a spent Claude usage limit; Windows and macOS against a real
`claude` (the fake-binary tests run there in CI).

**Checks.** 1279 tests passed, 0 failed, 20 ignored. fmt and clippy
`-D warnings` are clean. The starter eval against the mock through the
real binary: engineered 20/20, naive 11/20, $0.98.

**CI fix.** The e2e wizard answers the new "How should ferrule reach a
model?" question (API key) before the provider list.

**Follow-ups.** The OTel exporter doesn't send `plan`/`notional_usd`; a
ChatGPT live turn once someone signs in; README lines (listed in the PR).

### 2026-09-27 — dashboard collar redesign (Kimi Code)

The dashboard's front end is replaced with the "collar" design, ported from
the finished prototype (`/tmp/ferrule-dash-redesign/prototype/`, mock data)
onto the live `/api/*` wiring. Only the three embedded assets changed —
`crates/ferrule-cli/src/dashboard/assets/{index.html,app.css,app.js}` — no
Rust, no routes, no docs contract touched.

- **Design system ported verbatim** from `prototype/app.css`: paper/forge
  tokens, collar mark with the live sweep (`body.running` only, 2.6 s, off
  under `prefers-reduced-motion`), sticky blurred header, numbered left rail
  (01–09, horizontal strip ≤760 px, hidden when logged out), section heads
  with the copper ring bullet, BRASS/RUST alert banners on top of every
  section, stat strips, cap ring gauges, tables with dead-row dimming, task
  cards with run-dots, the mono log grid, and the bottom toast.
- **Fonts:** Geist / Geist Mono (OFL, via fontsource's latin woff2) are
  embedded as base64 data URLs in `app.css` (@font-face 400/500/600 +
  mono 400/500) — the page has zero network dependency and no CDN link.
  Separate woff2 files couldn't be served without a new Rust route, so the
  data-URL embedding is how "embed next to app.css" landed within the
  no-`.rs`-changes boundary.
- **Bug fixes folded in:** raw `replaceChildren`/`append` calls that took
  arrays or nulls now go through a `setKids` helper (flatten + drop
  null/false) — a literal "null" text on the Models page (`m.fixed` absent)
  was caught live and fixed; `last_served` renders `view.last_served[0]`
  instead of stringifying the array; the kill-switch row builds
  "by … at …" conditionally instead of printing `nullnull`. `dir="auto"`
  stays on every user/agent-text element (the Hebrew source-scan test pins
  this). Added: a `hashchange` listener (pasted/back-button `#section`
  links navigate), a `[hidden]`-aware shell grid (logged-out card was
  squeezed into the rail's column), and `text-overflow` on the log level
  column (long audit event names overlapped).
- **Verified live** against a scratch HOME (`/tmp/ferrule-dash-check`):
  mock provider + dead default provider, local channel over a FIFO, seeded
  ledger/tasks/skills/hooks. All 9 sections screenshot-verified at desktop
  and 390 px, paper and forge (theme persists across reloads via
  `localStorage ferrule-theme`, defaults to `prefers-color-scheme`). Through
  the real UI: turn/stop (Stop button), kill/on + kill/off (the 409
  `window.confirm` step captured on screen), tasks run/pause/resume, skills
  disable/enable — each audited as `by: dashboard`. Page fully usable with
  the default model down the whole session (RUST banner + working
  "Make X the default" fix button); Hebrew task name renders RTL.
  `scripts/dashboard-smoke.sh`: 8 passed, 1 skipped (no cloudflared).
  `cargo test -p ferrule-cli --test dashboard`: 14/14, including the Hebrew
  RTL and the no-secret-in-any-response tests.
- **Known cosmetic note:** the caps editor's number inputs accept the
  stepper/keyboard as before; nothing about them changed.

### 2026-09-27 (wrap) — collar port: main-session finish + v0.5.2 merge (Kimi Code)

The porting subagent's provider hit its 5-hour quota during verification,
so the wrap landed in the main session: smoke re-run (`8 passed, 1
skipped`), an independent live visual pass (health + models, desktop and
390 px, both themes), and the rebase onto v0.5.2 — M35C had touched the
same dashboard code, so the merge folded its features into the new design:
the models table's state cell now carries the plan line (`r.plan` +
`r.usage`), and usage's per-model cost cell carries `notional_usd` ("plan;
$X at API prices"). Post-merge re-verified: build clean, smoke 8/8, both
themes live. One tool-side artifact to disregard: the Kimi desktop panel's
browser drops localStorage across reloads, so theme persistence looked
broken there; the agent's run-2 Chrome check (and the code path) confirm
it persists.

### 2026-09-27 — README: vs-the-field section, roadmap moved out (Kimi Code)

Per Max: the README's Roadmap section (Shipped/Next/Planned + the roadmap
SVG) is removed — `docs/roadmap.md` stays the plan's home and is linked
from Docs. Added **"Ferrule vs the field"** between "Why ferrule wins"
and "Secure by default" (nav updated): an honest capability table vs
OpenClaw, Hermes and NanoClaw as a new hand-written SVG
(`docs/assets/vs-field.svg`, brand style, sources =
`docs/research-number-one-harness-strategy.md`), with the cells where the
others still lead (channel breadth, ecosystem size) said plainly, the
measured eval result (+10 pts overall, 9/12 vs 1/12 on verify tasks,
same model), and the `ferrule import openclaw|hermes` line. The pitch now
names the current surfaces (Telegram/Discord/Slack/dashboard, plans and
local models). The three term-*.png captures regenerated earlier today
against the current binary land in the same commit.

### 2026-09-27 — README rebuilt for clarity (Kimi Code)

Owner feedback on the v0.5.2 README: the "pitch" title was irrelevant, and
parts were irrelevant or not understandable. Rebuilt the file top to
bottom; the working tree was clean at `6f38497` and nothing was committed
by me.

- **Opening.** `## The pitch` is gone. `## What is ferrule?` describes the
  product in plain language for a newcomer (surfaces, capabilities, the
  footprint numbers), and `## Why ferrule` is three one-line strengths with
  links to their sections: better answers from the same model (+10 pts on
  the in-repo A/B), safe by default, easy to run. The "for-loop around an
  API call" phrase is gone from text, alt texts and the
  `why-ferrule-wins.svg` panel (now "The do-it-yourself loop — one API
  call, repeated until the model stops").
- **Cut.** The "Running 0.1.0 as a Telegram bot? Upgrade." notice in
  Install (stale at v0.5.2; the substance lives in m19b-reliability.md).
- **Fixed as stale.** `term-doctor.png`/`term-help.png` regenerated from
  the v0.5.2 binary (the old ones still said v0.3.0 and predated
  `login`/`logout`); `term-sandbox.png` was content-identical (only the
  ephemeral proxy port differs) and is kept byte-for-byte. Same scratch
  HOME + mock `/v1/models` + Pillow pipeline; the doctor run is all-green
  but for the honest no-prices warning. `architecture.svg` captions:
  Channels now read Telegram · Discord · Slack · local, the sandbox row
  names the Windows restricted token, Providers says 3 drivers /
  cheap-first routing. `quickstart-flow.svg`: wizard step mentions plan
  sign-in, the channel card covers Discord/Slack. Alt texts for the three
  terminal captures rewritten to match their real content (the old doctor
  alt still described the M10 board).
- **Documentation section.** The loose Docs list is now an index:
  Getting started (README anchors + subscriptions, local-models, migrate),
  Guides (models/routing/speed; skills/plugins/MCP/self-extension/browser/
  web-search/connections/ssh; editing/memory/agents/eval/learning loop;
  dashboard/discord/slack/hooks/trust&cost/reliability; sandbox/windows/
  egress/otel), Design & research (all 7 research docs, roadmap.md, the
  m12–m35 milestone notes, PLAN.md). Every entry exists on disk.
- Verified: all 11 images resolve, all 58 doc/PLAN links resolve, all
  anchors match headers (`#documentation` added to the nav), every quoted
  `ferrule` command and subcommand form (`model …`, `tasks add --model`,
  `login chatgpt`, `import openclaw`, `eval run`, …) exists in the v0.5.2
  binary (`target/debug/ferrule`), all SVGs parse as XML, and the file was
  re-read end to end. Windows-sandbox wording stays accurate (restricted
  token + job object, Git Bash caveat).

### 2026-09-27 — M36 self-update and self-repair (Devi, Opus 5.5)

**Scope.** Max's bot went silent on a ChatGPT plan when OpenAI raised a
model's minimum Codex client version; v0.5.1 fixed it by hand. M36 makes
Ferrule keep itself, `claude` and the ChatGPT client identity current, and
repair what it recognises. Design and as-built notes:
`docs/m36-self-update.md`. User guide: `docs/updates.md`, linked from
`docs/subscriptions.md`, `docs/m33-ops.md` and `docs/egress.md`. Branch
`m36-self-update`, one commit per part (design, 1–4).

**What was built.**
- 1: `ferrule-core::failure` (a `Kind` per known failure, structure first,
  then text; its repair, how long the model stays down, the plain words;
  `CoreError::Failed`). The Codex client version from npm, then GitHub,
  then 0.157.1, cached a day; a "requires a newer version" 400 refreshes
  and retries once. UA `codex_cli_rs/<v> (<os>; <arch>) ferrule/<v>`.
- 2: `ferrule update [--check|--to TAG [--unsigned]|--apply]`: GitHub
  releases, sha256 then minisign against a compiled-in key; installed only
  while idle; atomic swap keeping `ferrule.previous`; restart; rollback and
  pin within 180 s. Setup installs the apply unit, timer and path unit
  (launchd: one agent); `setup --refresh-service` and `install.sh` upgrade
  an old install. The release workflow signs. One owner line per update or
  rollback; the state in status, `/status`, doctor, dashboard.
- 3: `claude` kept current by its install's own updater (native,
  Homebrew, WinGet, npm, pnpm), as the files' owner, daily and after a
  too-old turn (retried once).
- 4: every provider error falls back; plain words plus the raw error in
  the chat; the last-good config; the 15-minute self-check; the repair
  log.

**Decisions taken alone (for Max to overrule).**
- minisign, with the public key compiled in and the secret in an Actions
  secret; a compromised GitHub account can still sign (design §2.4).
- A root oneshot apply unit rather than sudo, polkit or setuid.
- `--unsigned` exists (with `--to` only) so a signed build can go back to
  v0.5.x.
- A 401 falls back too, and the owner hears "its key was refused" once.
- Running on the last-good config applies to every later `Config::load()`
  in the process; a supervised gateway exits 0 to restart onto the fixed
  file.
- The self-check's first round reports what's already wrong; a check that
  has never succeeded isn't "failing for 3 days"; it tries a `claude`
  update only when the problem is new.
- No `/update` chat command (the consent button covers the owner's case).

**Not verified live.** A real signed release installing and rolling back
under systemd and launchd (no release was cut; the hermetic tests use a
mock GitHub and a test key); a real `claude` update (only the fake; the
`claude` here is the one this run uses); the ChatGPT backend's refusal
(mocked from the reported body). Live `#[ignore]`d tests: the Codex version
from the real npm and GitHub, and the real release list.

**Follow-up before the merge: the dashboard's `cloudflared`.** Max's
v0.6.0 service answered `/dashboard` with a `127.0.0.1` link and an
`ssh -L` line: `cloudflared` wasn't on the unit's pinned `PATH`, and the
dashboard falls back to local without saying why.
`ferrule-connections::cloudflared` now looks in the usual install dirs
and `<data>/bin` too, and when it finds none the first `/dashboard`
downloads Cloudflare's build for the platform into `<data>/bin`,
sha256-checked against GitHub's asset digest (see the as-built notes).
The live `#[ignore]`d test fails here only on the container's TLS proxy;
the same check by hand (curl) matched the real 2026.9.3 linux-amd64
digest, and the binary ran. After it: 1356 passed, 0 failed, 23 ignored.

**Checks.** 1349 tests passed, 0 failed, 22 ignored (after merging main). fmt and clippy
`-D warnings` are clean. The starter eval against the mock through the
real binary: engineered 20/20, naive 11/20, $0.98.

**Follow-ups.** A `/update` chat command; a Windows service (so unattended
updates there); the first signed release (v0.7.0; v0.6.0 was cut unsigned before this merged) proves the chain; README
lines (listed in the PR). v0.5.x installs run the install one-liner once.

### 2026-09-27 — M37 the control room (Devi, Opus 5.5)

**Scope.** Max, from his phone through the tunnel, asked for five things:
- notices that close;
- model pickers instead of typing;
- connections that work, with keys when OAuth fails;
- a better font;
- "anything I can do in the terminal, from the dashboard".

Two addenda made connections the priority: Atlassian's three ways in,
Google's paths, the fixed callback relay, a checklist, stuck flows, and
plain errors. The design and as-built notes are in
`docs/m37-control-room.md`. The user guides are `docs/dashboard.md` and
`docs/connections.md` (new). Branch `m37-control-room`, from v0.7.0, with
one commit per part.

**What was built.**
- **Notices:**
  - `problems[{id, fixes, closable}]`;
  - closing is server-side, for a day;
  - `ferrule doctor --json` from the page;
  - fix buttons: restart a channel, restore the last good config,
    restart the gateway (user or system service).
- **Models:** selects everywhere a model is named, a checked fallback
  chain, a key tested before `secrets.env`, and plan sign-in from the
  page.
- **Connections:**
  - native `jira`, `gmail` and `google`, with sealed keys bound to their
    hosts and tested first;
  - `atlassian_token` (Rovo MCP, Basic) and `google_oauth` (the owner's
    client);
  - the relay card (deploy by CF token via `GET /accounts`, or URL + key
    checked before saving; `<relay>/cb` with Copy);
  - the checklist;
  - pending flows with Cancel and a 15-minute expiry;
  - `explain.rs`: statuses, OAuth errors, handshakes and the egress
    policy in plain words, with no codes, state or secrets; plus
    consent-page symptoms for Google and Atlassian;
  - `blocked()`: no button that can't work, `/connect` included;
  - doctor and the banner for expiring keys;
  - `ferrule connections setup`.
- **Parity:**
  - the console (split without a shell, clap parse, read/change/destructive/
    refused, child with stdin closed, audited, completion from the clap
    tree, the parity matrix walked by a test);
  - chat (a dashboard channel, revision polling, streamed edits, buttons,
    approvals from any chat);
  - config (placeholders for secrets, a typed form, the loader's line,
    an atomic save keeping `.prev`, undo, dangerous fields refused by
    name).
- **UI:**
  - Plex Sans, Hebrew and Mono as woff2 from the binary (≈129 KB,
    ≈60 KB for a Latin page), `/fonts/OFL.txt`;
  - type scale 13–30;
  - bottom bar and ≥900 px sidebar;
  - system theme with a manual toggle, set before paint by `theme.js`;
  - `textContent` only, `dir="auto"`;
  - screenshots at 390/1280 px in `docs/assets/m37/`.

  `scripts/dashboard_browser_check.mjs` (Node 22, CDP, no packages) is
  in CI on Linux, macOS and Windows.

**Decisions taken alone (for Max to overrule).**
- No raw shell or PTY on the page (design §4.5). The console runs argv
  only, so an opt-in could come later as a new class.
- Each way in is its own catalog entry on a shared tile, not a
  `[service.key]` table.
- Jira, Gmail and Google run natively (REST, IMAP/SMTP) because no vendor
  MCP server takes those keys.
- No default relay URL: the relay runs on the owner's own Cloudflare
  account, and the CF token is used once and not kept.
- Notices close for a day rather than until their cause changes.
- The browser check uses CDP from Node rather than agent-browser.
- Services with no documented key or DCR were left out (the §3.5 list).

**Verified live vs only against docs/mocks.** Everything was verified
against mocks or the vendors' docs. The container can't do a real OAuth
consent, and reqwest refuses its TLS proxy.
- The browser check connected a local MCP server with a key through the
  real page.
- The Jira, Gmail, Google-service-account, relay-deploy and Rovo paths
  are tested against mock servers.
- Live checks are `#[ignore]`d.

**Checks.** 1411 tests passed, 0 failed, 23 ignored. fmt and clippy
`-D warnings` are clean. The browser check passed 10 of 10 steps. The
starter eval against the mock through the real binary gave engineered
20/20, naive 11/20, $0.98. origin/main had nothing new to merge.

**Follow-ups.**
- A real Atlassian site, a Gmail app password, a service account and a
  relay deploy on Max's server.
- Airtable, Supabase, Cloudflare, Monday and Intercom keys.
- A "before" screenshot set.
- An opt-in shell if Max wants one.
- README lines (listed in the PR).

### 2026-09-28 — M38 named instances (Devi, Opus 5.5)

**Scope.** Several independent agents on one machine, each with its own
config, data, bot, service and dashboard, without touching the default
install. The design and as-built notes are in `docs/m38-instances.md`.
The user guide is `docs/instances.md` (new): named instances, separate
Unix users, and the one-bot-per-agent rule. Branch `m38-instances`, from
v0.8.0, with one commit per part.

**What was built.**
- **The name:** `--instance <name>` (a global flag) and
  `FERRULE_INSTANCE`, validated before anything runs. `instance.rs`
  derives every path, unit, label, system user and relay Worker name.
  `FERRULE_ROOT` moves the config and data roots, for tests. The
  default's paths and unit texts are compared with 0.8.0's.
- **The service:** `Svc { instance, scope }` behind the old free
  functions, so setup, `--refresh-service`, doctor, status, stop, restart
  and uninstall follow the instance. Named units carry
  `FERRULE_INSTANCE`, and a named instance never reads `./ferrule.toml`.
- **`ferrule instances list | new | remove`:** found by config dir, by
  unit and by system config. `new` runs the setup as a child without this
  instance's paths or secrets. `remove` keeps the files unless `--purge`,
  which asks for the name and never touches a workspace or a system user.
  Setup's menu has "Another instance".
- **Collisions:** a pure `clashes()` over each instance's facts (bot id,
  fixed dashboard port, canonical workspace, SSH target, relay URL and
  key). Doctor checks every readable instance; setup warns when it
  finishes.
- **Updates:** per-instance units, coordinated per binary. Siblings are
  the same-scope instances whose unit runs the binary being replaced:
  every lock is taken in sorted order, pins are merged, a sibling's
  `auto = false` holds timer runs and refuses gateway requests by name,
  all must be idle, all restart and are health-checked, and a rollback
  pins and records in each. A terminal `ferrule update` names the
  siblings and asks.
- **The dashboard:** `instance` in `/api/health`, in the header and the
  title. Children (doctor, fix buttons, the console) get
  `FERRULE_INSTANCE`. A named instance's relay Worker is
  `ferrule-relay-<name>`.
- **`install.sh`:** the upgrade loops over the default unit and every
  `ferrule@*.service` / `ai.ferrule.gateway.*.plist`, refreshing each
  running one on this binary with its own `--instance`.

**Decisions taken alone (for Max to overrule).**
- `ferrule@<name>.service` rather than `ferrule-<name>.service`: `@` can't
  appear in a name, so no instance can take the default's update unit
  name.
- Sibling directories (`ferrule-<name>`), not subdirectories of the
  default's.
- A system instance runs as its own user, `ferrule-<name>`.
- Per-instance update units coordinated at apply time, rather than one
  shared updater per binary.
- `auto = false` in any sibling holds the others' automatic updates.
- `list` shows the bot id, not `@username`: nothing records the username.
- Setup checks collisions once at the end, not at each prompt.
- `remove --purge` refuses a workspace inside the instance's dirs.
- The doctor note for a `FERRULE_CONFIG` that isn't the instance's own
  was dropped: every service-run doctor would have shown it.

**Verified live vs only against fakes.** No real second service was
installed: the container has no systemd user session or launchd. The
units, the coordinated update, the system-instance path and `install.sh`
are tested against fakes and stubs. `instances list/new/remove`,
`--instance`, the doctor collision check and a named instance's
dashboard and console run through the real binary against a temp home.

**Checks.** 1435 tests passed, 0 failed, 23 ignored. fmt and clippy
`-D warnings` are clean. The starter eval against the mock through the
real binary gave engineered 20/20, naive 11/20, $0.98.

**Follow-ups.**
- A second instance on Max's server (a user service), and one coordinated
  update across the two.
- A system instance on a real systemd host.
- The bot's `@username` in `instances list`, if the gateway starts
  recording it.
- README lines (listed in the PR).

### 2026-09-28 — Round-2 improvement research (Kimi Code)

Six parallel investigations at v0.9.0 (PLAN/roadmap mining, codebase
health, fresh competitive scan, eval-data analysis, adoption research,
new-user friction audit). Synthesis: `docs/research-round2-improvements.md`.
Headline findings: no LICENSE (existential for adoption); planning docs
have drifted from reality (roadmap's open tracks list shipped features);
the README's verify-task figure contradicted the saved run (fixed in
`d51e8fb` — naive was 3/12, not 1/12); the eval data shows 13/120 runs
ended on text-formatted tool calls (all failed — a repair path is the
top harness fix); MCP image content blocks the browser's screenshots;
ZeroClaw is now the direct competitor (the Sept-24 doc mischaracterized
it); a launch-readiness checklist with the phase-0 blockers. Also fixed
while writing: nothing else in the repo (read-only investigations).

### 2026-09-28 — Round-2 follow-through: launch blockers, text-call salvage, MCP media, model governance, friction top-9 (Kimi Code, finished in Kimi Work)

Works through `docs/research-round2-improvements.md`'s recommended order,
items 1, 3 (the §2 halves) and 4, plus the doc drift the research flagged.

- **Credibility blockers (§7 phase 0).** Dual LICENSE-MIT/LICENSE-APACHE
  at the root, SECURITY.md (private reporting via GitHub advisories, the
  trust-boundary contract), CONTRIBUTING.md (read PLAN.md first, build,
  test, style), `license = "MIT OR Apache-2.0"` on every crate, and the
  plugin-sdk's `publish = false` comment rewritten (the license was the
  blocker; publishing is still a decision, not a default). README gained a
  Contributing/security/license footer and its stale Development test
  count was corrected.
- **Text-formatted tool calls (§2.3, the eval's top harness failure —
  13/120 runs ended on one).** `ferrule-core/src/textcall.rs` reads calls
  a model wrote as text — `<function=…><parameter=…>` markup, a
  `<tool_call>` JSON block, a bare `{"name", "arguments"}` object — and
  the loop runs them as structured calls (the surrounding prose survives
  as the message's content). Markers that won't parse earn one nudge to
  re-emit structured; the whole rescue is capped at 3 per run so a
  text-only model can't loop, and a message with native blocks (M23
  drivers replay them verbatim) is never rewritten. Prior art:
  OpenHands' NonNativeToolCallingMixin.
- **MCP image/audio/resource content (§2.1 — this blocked the browser's
  screenshots).** Non-text parts are base64-decoded and saved under
  `<workspace>/.ferrule/mcp-content/` (16 MiB a part, 64 MiB a result),
  inside the room so `read_file` and `shell` can open them; the tool
  result names each file, type and size, and `ToolOutput.files` carries
  the same fact as data for the UI and transcript. A text-only result
  serializes byte-for-byte as before. Saving is best-effort — a write
  failure never fails the call. `docs/browser.md`'s Limits bullet
  rewritten to match.
- **Model governance (§2 item 7).** `[models] deny` (a `provider/model`,
  a bare id on every provider, a whole provider, or an alias) and
  `[models.exact]` (the connected id must be the pinned one — a provider
  that silently retargets is refused, not run), enforced in
  `Catalog::resolve`, so every pick point (default, pin, task, tier,
  `--model`) is covered at once. `ferrule doctor` says what deny refuses
  and fails on a mispin, with the fix. `resolve_unchecked` keeps a denied
  or mispinned model listable, changeable and removable (`model remove`
  uses it; so does its fallback-list sweep). `docs/models.md` gained the
  governance section and the example config shows both knobs.
- **Friction (§5, items 1–9).** install.sh offers to put itself on PATH
  in the shell's rc file (idempotent, `FERRULE_NO_RC=1` opts out, the note
  is re-printed after the wizard so it isn't buried); the Claude plan
  step returns "nothing was set up" when the `claude` CLI is missing and
  the picker comes back instead of saving nothing; the local-model step
  points at Ollama and offers a re-scan; setup's outro names the
  configured channels and says to message the bot / run `ferrule
  gateway`; the chat-id hint prints whether or not the bot looks busy;
  `ferrule mcp add` takes the command as trailing words without `--`, and
  a bad `--url` fails in plain words; `config init`'s example config is
  fully commented (no more live Kimi/OpenAI providers pointing at keys
  nobody has); a run with no provider at all says "run `ferrule setup`";
  the README's 60-second block makes a workspace dir and says $HOME is a
  bad room. Item 10 (the "60 seconds" framing) left for Max.
- **Telegram channel.** A button tap marks its choice (the keyboard gives
  way to "→ <label>", as Discord and Slack already do) so a question
  can't be answered twice; long messages split at Telegram's 4096 UTF-16
  limit at safe boundaries, a fenced code block is closed and reopened
  across pieces, reply-to rides the first piece and the buttons the last.
- **Doc drift (§1.2).** roadmap.md's M13–M38 moved out of "Next" into the
  Done table (the roadmap had listed shipped milestones as open); PLAN's
  architectural-gaps paragraph rewritten — the 2026-09-24 gap list has
  essentially all closed; vs-field.svg gained the ZeroClaw column, with
  the corrections found while building it recorded in the research doc;
  install.ps1/setup/agents.md's stale "Windows has no sandbox" strings
  corrected to the M26 restricted-token reality.
- **The owner's own git config broke tests on this Mac** (§3's class of
  bug, live): `commit.gpgsign=true` with 1Password made every test-repo
  commit fail or hang. The agents crate's internal `git` now passes
  `commit.gpgsign=false`/`tag.gpgsign=false` — a product fix too:
  ferrule's own worktree/snapshot commits never sign — and the worktree
  test helper isolates with `GIT_CONFIG_GLOBAL=/dev/null` like
  ferrule-extensions' tests already did. Two macOS-only clippy findings
  fixed along the way (a Linux-only `mut`, the Linux-only
  `PollingChannel` helper now cfg-gated).

**Verified live vs only against fakes.** The salvage path runs through
the real agent loop against a scripted provider; deny/exact and the
doctor lines are unit-tested; install.sh's rc flow runs against the
stubbed machine; MCP media saving is unit-tested at the caps. Not run
end-to-end: a real MCP server returning an image through a live gateway,
and a real model that writes its calls as text.

**Checks.** 1454 tests passed, 0 failed, 23 ignored (was 1435). fmt and
clippy `-D warnings` are clean on macOS.

**Follow-ups.**
- §3 eval-suite fixes: the progress heartbeat, `run.json` checkpointing
  with `--resume`, timeouts in the report table, a verify-loop isolation
  task, a `--variant routing` run on a real model pair, and a full eval
  rerun on v0.9.0 (README numbers then cite the run id).
- The git-fix-commit fixture's `.ferrule/` pollution (.gitignore in
  `git_init`) and `GIT_CONFIG_GLOBAL` isolation there, as done here for
  the worktree tests.
- §4 consolidation, slotted before the next user-facing milestone.
- Phase-0 leftovers: the demo GIF and the first-person origin note; then
  the growth sequence (§7). ferrule-plugin-sdk publishes when Max says
  so (drop `publish = false`).
- Friction item 10: reframe the "60 seconds" copy, or trim guided setup.

### 2026-09-28 — M39 more channels (Devi, Opus 5.5)

**Scope.** Reach the agent from WhatsApp, Matrix, email, Signal,
Mattermost and any program over HTTP, in M31's shape: the same channel
trait, chats, sessions, allowlist, mention-only shared chats, pairing,
an owner per channel, and tokens out of the model's reach. The design
and as-built notes are in `docs/m39-channels.md`; the user guide is
`docs/channels.md`. Branch `m39-channels`, one commit per part.

**What was built.**
- **A Windows flake first:** the live_fixes 409 test is timed from the
  gateway's first getUpdates, dispatch errors are told in the chat, and
  `tasks.db` waits on a busy lock.
- **The seams:** owners (`[trust] <channel>_owner`), redaction, health,
  doctor, setup and the dashboard cards (`card::Spec`) are per channel
  instead of three hard-coded lists. `send_file` sends a workspace file
  to the session's own chat; incoming files land in the inbox.
- **WhatsApp:** the Cloud API. Meta's webhook reaches the relay Worker,
  which checks the signature and keeps it in a mailbox the gateway
  drains; ferrule configures the mailbox itself. Text, media, reply
  buttons for approvals, 👀 and read receipts; the 24-hour window with an
  optional template.
- **Matrix:** `/sync` long-poll, DMs by user id, rooms on a mention, HTML
  with edits for streaming, approvals by reaction, files through the media
  repo; encrypted rooms refused with one notice.
- **Email:** IMAP IDLE and SMTP on M37's native code; threads kept,
  strangers' mail left unread, `Authentication-Results` required on the
  big providers, loop guards and a rate per address, approvals by a
  first-line keyword, attachments both ways, an app password or M37's
  Gmail connection.
- **Signal:** signal-cli's JSON-RPC daemon over HTTP, started by the
  gateway or adopted; DMs, groups on a mention, Note to Self, text styles,
  approvals by reply keyword, attachments; linking by QR in setup.
- **Mattermost:** a bot token, the WebSocket and REST v4; DMs, channels on
  a mention answered in threads, edits for streaming, approvals by
  reaction, files both ways, rate limits waited out.
- **HTTP API:** `POST /v1/messages` on 127.0.0.1:8788 with a bearer key
  per program (only its SHA-256 kept), JSON or SSE; an outbox
  (`GET /v1/events`) and a signed webhook for what nobody waited for;
  files as base64 in and links out; 429s per key; public only through a
  quick tunnel. `ferrule channels keys list | add | webhook | revoke`.
- **Every channel:** setup, a dashboard card with Test and a guide, doctor
  lines, a cross-instance clash (M38), hermetic tests against a mock, and
  an `#[ignore]`d live test keyed by `FERRULE_LIVE_*`.

**Decisions taken alone (for Max to overrule).**
- `send_file` isn't approval-gated (the design said it would be, like
  `write_file`, which isn't either). It sends only workspace files, only
  to the session's own chat.
- WhatsApp's mailbox is set by an authenticated call and stored in the
  Durable Object, so no Cloudflare token is needed after deploy; the app
  secret is therefore held on Cloudflare. Strangers on WhatsApp are never
  answered: a reply opens a billed conversation.
- A Matrix or Mattermost DM's chat is the person's user id, not the room
  or channel id, so owner notices and task results can reach them.
- Matrix encrypted rooms are refused, not supported.
- Email finds new mail by UID rather than UNSEEN, delivers at most once,
  and requires `Authentication-Results` by default only on Gmail, Yahoo,
  iCloud and Fastmail. Outlook/M365 (OAuth only) isn't supported yet.
- Signal: signal-cli and Java are detected, never vendored; a
  `run-now` process doesn't start a daemon.
- The HTTP API's CLI is `ferrule channels keys …`; a key is printed once
  and the console refuses to print one. The gateway never downloads
  cloudflared (setup does). A webhook is tried at once, then after 1, 5
  and 30 s.
- Three new trait hooks (`answered`, `busy_notices`, `note`) rather than
  special-casing the HTTP API in the router.
- Generic SVG icons on the cards, not brand assets.
- Teams, Google Chat, Twilio SMS and Zulip left as follow-ups; the design
  (§13) describes each.

**Verified live vs only against fakes.** No channel was run against a
real account: there were no WhatsApp, Matrix, mail, Signal or
Mattermost credentials in this session. Every channel is tested against
a mock server on 127.0.0.1, and through the real binary where it
matters (setup-free config, the gateway, doctor, `channels keys`).

**Checks.** 1628 tests passed, 0 failed, 28 ignored (after merging
`main`). fmt and clippy
`-D warnings` are clean. The starter eval against the mock through the
real binary gave engineered 20/20, naive 11/20, $0.98.

**Follow-ups.**
- A live run of each channel with a real account (the ignored tests).
- Teams, Google Chat, Twilio SMS, Zulip; Outlook/M365 mail by OAuth.
- Matrix end-to-end encryption.
- Files in and out for Telegram, Discord and Slack through the new seam.
- A `run-now` result for the HTTP API: its outbox copy can be overwritten
  by the running gateway's `state.json` (the webhook still fires).
- Windows flake suspects not yet fixed: sync sleeps in the file-write
  tests, the agent factory off `spawn_blocking`, rename retries in
  `health.rs`.
- README lines (listed in the PR).

### 2026-09-29 — M40 build diet (Devi, Opus 5.5)

**Scope.** Max, 29.09: "the builds pollute the disk" — the shared target
dir hit 24 GB on 28.09 and filled the disk mid-run. Make one test build
small and write fewer executables. Branch `m40-build-diet`; design and
measurements in `docs/m40-build-diet.md`.

**What was built.**
- **A baseline** from an empty target dir: 277 s, 10.66 GB, 102
  executables (8.69 GB), 1628 tests passed.
- **The finding that changes the story:** an edit does *not* leave a
  second copy. Test binaries are hashed by package version, profile,
  features and compiler, so an edit overwrites them in place (+4 KB). A
  release (every crate's version bumped) re-hashes all of them: 10.9 →
  20.1 GB. That, toolchain updates and differing `CARGO_PROFILE_*` envs
  are what fill a shared dir.
- **`[profile.dev]`**: `debug = "line-tables-only"`, and `debug = false`
  for dependencies. 10.66 → 3.65 GB. A deliberately failing test still
  printed our file:line in the backtrace. No `strip`/`split-debuginfo`.
- **One test binary per crate**: 74 integration-test files → 19 `it`
  binaries + 4 separate files (`plans/engine_env`, `mcp/sandbox`,
  `sandbox/windows`, `sandbox/windows_git_bash`: each sets an env var
  that is read once per process). 3.65 → 2.62 GB, 51 executables; a
  test run after an edit 259 → 206 s. A release now adds ~2 GB.
- Docs, CONTRIBUTING, code comments and the self-exec helpers' `--exact`
  names follow the move; a CI step guards the layout.

**Tests.** 1628 passed, 28 ignored, before and after; the name sets are
identical modulo the module prefix. clippy and fmt clean.

**A known flake got likelier.** `discord::a_dropped_socket_resumes_…`
(listed under "Other flaky tests" above, 1 of 20 suites) failed in 3 of
35 runs of the merged gateway binary: the race it has (the mock counts
RESUMED before sending it, the client clears `problem()` when it reads
it) widens when the other gateway tests share the process. Splitting
the file out would not remove the race, so the test now waits
(`until(… problem().is_none())`), as that note proposed. 0 of 30 after.

**Decisions for Max.**
- `tests/it/` for every crate, including the five with a single file, so
  the rule has no exceptions.
- `proxy/egress.rs` was merged although it sets `NO_PROXY` (to the
  loopback bypass every hermetic test wants; the other proxy tests set
  their proxy explicitly).
- Older PLAN.md sections keep their pre-M40 test paths (this file's rule:
  change only your own section); the M40 bullet above says how to read
  them. The docs/ pages and code comments were updated.
- The repo-external sweep (`cargo-sweep.py --keep 2`) is still worth
  running: it is what removes the previous release's copies.


### 2026-09-29 — M41 daily use (Devi, Opus 5.5)

**Scope.** The four things that make ferrule livable from a phone every
day: escape a broken conversation, talk to it by voice, see that it's
working, and move it to a new machine. Branch `m41-daily-use`; design,
decisions and what was verified in `docs/m41-daily-use.md`.

**What was built** (one commit each).
- **`/new` and `/reset`** in every channel: stop the lane's turn, drop
  what waited, move the transcript to `<sid>.<stamp>.jsonl` (kept, never
  deleted), memory untouched. The same 4xx twice in a lane adds a hint to
  send `/new`. `/help`; Telegram's command menu; Discord's `/new` and
  `/help` slash commands; `/new` in `ferrule chat`.
- **Voice transcription** in the router, before the agent sees the
  message: `[transcription]` with `auto` (OpenAI key → OpenAI), `openai`
  (any compatible endpoint: Groq, a local server), `command` (whisper.cpp
  or anything that prints text), `off`. Language detected unless pinned.
  A ledger row per call, a doctor `voice` line; without a backend the
  sender is told once per lane how to turn it on. Telegram now downloads
  voice notes and audio.
- **Typing indicator**: one refresher per turn (Telegram 4 s, Discord 8 s,
  Matrix 25 s and cleared, WhatsApp read receipt 20 s), stopped before the
  reply, on `/stop` and on error, and for the rest of the turn after a
  429. `[gateway] typing`, on by default. Fixed on the way: a clean
  shutdown could race a status write and leave `status.txt` behind.
- **`ferrule backup` / `ferrule restore`**: a `.tar.gz` (0600) with a
  sha256 manifest; SQLite copied with `VACUUM INTO`; caches left out;
  secrets only with `--include-secrets`. Restore refuses while the gateway
  or its service runs (and says how to stop it), checks every file and the
  version before anything moves, keeps the old data as
  `<data>.pre-restore-<stamp>`, carries secrets over when the backup has
  none, and has `--dry-run`. Doctor shows the last backup's age. User doc:
  `docs/backup.md`.

**Tests.** 1628 → 1659 passed, 28 ignored. New integration modules:
`ferrule-gateway/tests/it/{daily_use,voice,typing}.rs`,
`ferrule-cli/tests/it/backup.rs`, plus unit tests. clippy and fmt clean.

**Eval.** `eval run evals/starter --variant ab`, real binary, mock model:
engineered 20/20, naive 11/20, $0.98 total, unchanged.

**Not verified live.** No real Telegram/Discord/Matrix/WhatsApp account,
no real transcription endpoint, no real systemd/launchd service during a
restore; all are covered by mocks (see the design doc §5).

**Decisions for Max.**
- Backups are 0600 even without secrets; conversations are private too.
- Restoring a backup without secrets keeps the machine's current ones.
- An archive from a newer release line (newer major, or newer minor on
  0.x) is refused; older ones restore.
- `restore --dry-run` reports a running gateway but doesn't refuse.
- In the dashboard console, `backup` is allowed without secrets and
  `restore` is terminal-only (`--dry-run` excepted).
- Typing is on by default in the config, also for scheduled turns;
  Discord skips it while a slash command is deferred.
- The transcription ledger row records cost, not duration.

**Follow-ups.** Voice on Discord/Slack; scheduled backups with pruning;
other Telegram file kinds; typing on Slack and Mattermost.


### 2026-09-30 — M44 managed mode (Devi, Opus 5.5 plan / Sonnet 5.5 build)

**Scope.** A closed-beta service: one bot per container, run by a separate
panel, users bring their own model access, the container is the isolation
boundary, and the panel updates a bot by swapping the image tag. Branch
`m44-managed-mode`; design, plan and what was verified in
`docs/m44-managed-mode.md` (§9), user docs in `docs/docker.md`.

**What was built** (one commit per part).
- **1 The image.** A multi-stage `Dockerfile` (runtime and `-browser`
  targets, tini, uid 10001, `/data`, `HEALTHCHECK`, agent-browser 0.38.1
  pinned with a sha512 check) and a release-workflow job.
- **2 Managed mode.** `FERRULE_MANAGED` and a policy file (`sandbox = "os"
  | "container"`): the config is narrowed to the policy, no Claude plan,
  hooks/MCP add/`update`/`setup`/`ssh`/`instances` closed,
  `[sandbox] mode = "off"` refused, `/api/managed`, a `doctor` line.
- **3 Panel sign-in.** An HMAC-SHA256 token per bot, a fail-closed nonce
  file, the public URL (host, origin, cookie `Path`/`Secure`), the page and
  its fonts under `/b/<id>/`.
- **4 The dashboard's gaps.** Telegram from the page (test, save, wait for
  a first message, allow a chat); `FERRULE_HTTP_BIND`.
- **5 Health and lifecycle.** `/healthz`, `/busyz`, `ferrule health
  [--probe]`, a bounded SIGTERM drain, re-exec restarts, backup and
  restore in a container.
- **6 Measurements.** `scripts/m44-measure.sh`: idle 24 MiB, one turn's
  peak 26 MiB, about 462 bots per 16 GiB; a real-page browser peak test.
- **7 Docs.** `docs/docker.md`; additions to `dashboard.md`, `sandbox.md`,
  `channels.md`, the README, the roadmap and this file.

**Tests.** 1659 → 1705 passed on the branch, 1714 after merging M42 (main),
0 failed, 29 ignored (was 28; the new one is the real-page browser peak
test).

**Merge with M42.** One conflict in `main.rs`: M42 made `verify_command` a
list, so the managed check became `if !(planning || commands_off)` around
the loop. PLAN.md and the roadmap kept both sides.

**Eval.** `eval run evals/starter --variant ab`, real binary, mock model:
engineered 20/20, naive 11/20, $0.98 total, unchanged.

**Plan fixes made while building** (each in the commit that found it).
- The measurement script must run in managed mode: `/healthz` only exists
  there, so the first run found no gateway.
- The Signal key is `[gateway.signal] url`, not `http_url`.
- The real-Chrome test needs agent-browser ≥ 0.38 (0.27.1 has no `mcp`);
  behind this environment's TLS-intercepting proxy the peak test needs
  `FERRULE_TEST_CHROME_ARGS=--ignore-certificate-errors`.

**Not verified live.** No Docker daemon: the image has never been built,
the musl binary failed to build here (no `x86_64-linux-musl-gcc`; the
measurements use the glibc binary), the `container` policy has never run in
a container, and image sizes are estimates. No real reverse proxy, no real
Telegram bot, no panel. The first CI release run is the first real build.

**Decisions for Max.**
- Whether to merge before someone has built the image once.
- The panel itself, invites, billing and per-bot limits are out of scope
  and still need an owner.

**Follow-ups.** Build and run the image on a machine with Docker and fill
in the real sizes; cosign signing; subdomain routing.

### 2026-09-30 — M42 harness engineering, parts 1–4 (Kimi)

**Scope.** A pass over the learn-harness-engineering course (14 lectures)
and its Claude Code / Codex / DeepSeek / Pi harness breakdowns, scored
against ferrule; close the gaps found. Design and the part 5–7 designs
(goal loops whole, graph routing, pluggable compaction + session tree):
`docs/m42-harness-engineering.md`.

**What was built.**
- **The repo dogfoods:** a root `AGENTS.md` directory page (verify
  commands, hard invariants, conventions, map) and a `Makefile` —
  `make check` = fmt-check + clippy + `cargo test --workspace --locked`.
- **Layered context baseline** (`ferrule-core::baseline`): the config
  dir, then every workspace parent from the root down, then the
  workspace; broadest first in the prompt, the 16k cap spent most-
  specific-first; a lone workspace file behaves exactly as before (the
  eval fixture keeps the flat loader). CLI wires it in
  `build_agent_from`.
- **Layered verification:** `[agent] verify_command` takes a string or a
  list run in order (one built-in Stop check each, first failure sent
  back naming the command); `ferrule run --verify CMD` (repeatable)
  overrides the config for the run, threaded through `run_once` →
  `run_root` → `build_root` → `build_agent_from`. `hooks list` shows
  every check; the learn gate takes the first.
- **Model-visible means logged:** compaction writes a `fold` transcript
  record (summary + kept-tail count); `read_messages` applies folds, so a
  resume replays the compacted state instead of resurrecting the folded
  past; `read_all_logged` is the append-only audit view. `search_history`
  parses the file itself and is unaffected.

**Tests.** New: 4 baseline unit tests, 1 transcript fold unit test, 2
verify it-tests (`hooks::verify_command_accepts_a_list…`,
`hooks::run_verify_flag_overrides…`), 2 it-tests in
`ferrule-core/tests/it/history.rs` (the invariant across a compacting
run; resume replays compacted state). Full workspace suite green per
crate (the run exceeds one shell timeout, so it ran in batches), clippy
and fmt clean.

**Behavior change to note.** Resuming a compacted session (gateway,
supervisor, `run --resume` of an approved plan) now continues from the
compacted view. Old transcripts without fold records read exactly as
before.

**Not done.** Subdirectory baselines loaded on demand by the file tools;
parts 5–7 in the design doc; subdirectory `AGENTS.md` discovery tests
against real repos.

### 2026-09-30 — M42 part 5: goal loops (Kimi)

**Scope.** The full L13 goal loop on top of part 3's `--verify`: a goal,
an independent judge, a stopping condition — across sessions. Design:
`docs/m42-harness-engineering.md` §5.

**What was built.**
- `ferrule run --goal --verify CMD "…"` starts a loop (the prompt is the
  goal; the judge comes from `--verify` or `[agent] verify_command`; a
  goal without a judge is refused up front). `Run`'s prompt is optional
  now, required unless `--goal`/`--resume`.
- Loop state in `<data>/sessions/<sid>.goal.json` (goal, judge commands,
  attempts, the judge's latest failing word, timestamps), written
  atomically; a `RecordingVerifier` wraps each check and records every
  verdict as it happens.
- New `AgentConfig::verify_without_changes` (default off): the judge
  runs at finish even when the run edited nothing — set only for goal
  loops, so an idempotent goal can be met by a no-change run.
- A budget cut ends `goal pending` (exit 2, as any incomplete run) with
  the resume command printed; `ferrule run --resume <sid>` continues the
  same session (folded transcript from part 4) with a prompt carrying
  the judge's last word plus optional owner guidance. A satisfied judge
  prints `goal met` with the run count.
- The system prompt's `[Goal loop]` block states the maker-checker rule:
  the judge decides done-ness, never the agent.

**Tests.** 2 unit (`goal.rs`: state roundtrip, resume prompt) + 5
integration (`ferrule-cli/tests/it/goal.rs`: judge-to-pass with state
assertions, budget-cut pending then resume-to-met, judge runs with no
changes, no-judge refused, config-supplied judge). ferrule-cli 335+121,
ferrule-core 123+33, gateway/agents/eval/learn suites green; clippy and
fmt clean.

**Not done.** `--schedule` on a goal loop (compose with the cron
scheduler); a `goal list` / `goal abandon` admin surface; the gateway
channels don't start goal loops (CLI-only this round).

### 2026-09-30 — M42 part 6: agent graphs (Kimi)

**Scope.** L14's graph engineering over the M12 primitives: nodes,
edges, shared state, routing — and the routing is deterministic Rust,
never a model. Design and format: `docs/m42-harness-engineering.md` §6.

**What was built.**
- `ferrule graph run FILE` (`crates/ferrule-cli/src/graph.rs`): a TOML
  graph of `[[nodes]]` (`agent` on worker/planner/verifier roles,
  `check` — the `verify_command` judge in the sandbox, `approval` — a
  human gate) and `[[edges]]` (`on: pass|fail|always`). Flags:
  `--goal`, `--model`, `--workspace`, `--max-iterations`, `--max-steps`,
  `--yes`.
- The runner registers a synthetic root in the agents store and drives
  `Supervisor::spawn`/`changed` directly: fan-out spawns in parallel,
  fan-in waits for **all** inputs (the supervisor's `wait` returns at
  the first finisher — the join loops on the store instead), a straggler
  past its `timeout_secs` is closed and failed.
- Feedback arcs computed greedily in declaration order: a fail edge back
  re-fires an earlier node (capped by `max_attempts`) with the check's
  failure output carried in `{{prev}}` as the repair instruction;
  feedback edges don't gate first runs and don't count as a way out when
  success is computed.
- Verifier agents route on a final `VERDICT: PASS|FAIL` line, unclear
  fails closed. Success = `succeed_when` or every terminal node passing;
  the report names every node's pass/fail/never-ran, exit 0/2.
- Templates in `task`/`message`: `{{goal}}`, `{{prev}}`, `{{attempt}}`.
- Dashboard console parity row (`graph run`: change, `--yes` for
  approvals) plus its m37 doc row — the parity test enforces both.

**Tests.** 4 unit (feedback arcs, validation messages, verdict parsing)
+ 7 integration (`tests/it/graph.rs`: linear, rollback to the cap,
rollback with repair succeeding, verifier verdict routing + fail-closed,
fan-out/fan-in join, approval approved/denied, missing-node refused).
Two real bugs caught on the way: wait-for-one vs wait-for-all, and both
directions of a cycle marked feedback. ferrule-cli 368+138, clippy and
fmt clean.

**Not done.** A cut graph restarts (no run-state resume); gateway
channels can't start graphs; no `graph list`/`graph stop`; approval asks
at the terminal, not in the owner's chat.

### 2026-10-01 — M49 dashboard visual polish (Devi, Opus 5.5 plan / Sonnet 5.5 build)

**Scope.** Max found the page's selects and buttons amateur and wanted the
fonts (English and Hebrew), the colours and the controls redone to the
restraint of Linear, Vercel and Stripe, in light and dark, at 390 and 1280
px. Branch `m49-dashboard-polish`; design, plan, corrections and what was
verified in `docs/m49-dashboard-polish.md`.

**What was built** (one commit per part).
- **1 Baseline.** The shot options and the before set (64).
- **2 Fonts.** Inter, Heebo, Geist Mono, one variable woff2 each; the
  licence ships; 129 KB → 47 KB.
- **3 Palette.** Neutral greys, one indigo accent, copper only in the mark;
  contrast test covers field borders and the ring.
- **4 Controls.** Buttons, fields, custom select and checkbox, segmented
  chips, switches, tags, alerts, cards, dialogs, toasts in one system.
- **5 Icons and the menu.** Lucide inline icons, `menu()` for rare
  actions, icon buttons for glyphs.
- **6 Layout.** Page headers, empty states, spacing and motion.
- **7 RTL.** Mirroring, the Hebrew line box, overflow menu checks.
- **8 After screenshots** (64, WebP, 2.98 MB with the before set).
- **9 Docs.**

**Tests.** 1850 → 1852 passed, 0 failed, 29 ignored. Browser check 25 → 27
checks. Cold load of Home: 107 KB → 91 KB on the wire, FCP 100 → 76 ms.
Eval engineered 20/20, naive 11/20, $0.98. fmt and clippy clean.

**Not verified live.** Real iOS Safari; the native option list of a select
in dark; Arabic and Windows' Segoe fallback; a real tunnel.

**Follow-ups.** Hebrew strings for the strip and the pager; icons on the
alerts that `notice()` doesn't draw.

### 2026-10-01 — M48 self-service from chat and dashboard (Devi, Sonnet 5.5)

**Scope.** Max runs Ferrule from Telegram and the dashboard and never wants
to log in to the server: the bot does everything from the conversation,
asks in the chat when something needs approval, fixes and updates itself,
and every owner command is in Telegram's `/` menu. Branch
`m48-self-service`; design, plan, corrections and what was verified in
`docs/m48-self-service.md`.

**What was built** (one commit per part).
- **1 The `ferrule_admin` tool.** Typed ops run in the gateway outside the
  shell sandbox.
- **2 Approvals in the conversation.** Owner-only, single-use, bound to the
  op; buttons on Telegram.
- **3 Default behaviour.** The prompt, the tool description, the hints.
- **4 `/update`, `/restart`, `/doctor`**, the promise, rollback notes.
- **5 The Telegram menu.** One list for `/help` and the menu, per scope.
- **6 The dashboard.** Update routes, Restart without a service, Approvals
  inbox, Home update row.
- **7 Managed locks** for every op.
- **8 The e2e test, docs and the final checks.**

**Tests.** 1791 → 1846 passed on the branch, 1850 after merging main (M43), 0
failed, 29 ignored. Eval engineered 20/20, naive 11/20, $0.98. fmt and clippy clean.
Browser check 25/25 (at Part 6).

**Not verified live.** A real Telegram client; a real release install; a
real systemd restart; a real sandbox denial.

**Follow-ups.** No auto-rollback for an update with no service manager;
a live pass on a phone.

### 2026-09-30 — M47 the dashboard, redesigned (Devi, Opus 5.5 plan / Sonnet 5.5 build)

**Scope.** Max tried the page on a phone and wanted it beautiful, modern
and very easy to use, with more abilities, for beta users who mostly don't
code. Branch `m47-dashboard-redesign`; design, plan, corrections and what
was verified in `docs/m47-dashboard-redesign.md`, user docs in
`docs/dashboard.md`.

**What was built** (one commit per part).
- **1 Audit.** Before-screenshots (13 sections × 2 widths × 2 themes),
  the abilities inventory, before-numbers, ranked findings.
- **2 Visual system.** Tokens, light/dark, icons, components.
- **3 Home and first run.** Problems first, a three-step checklist.
- **4 Navigation.** Sidebar, bottom bar and More sheet, palette,
  shortcuts, sections regrouped, experts' controls folded.
- **5 New abilities.** Vision in the provider layer (5a), Telegram and
  Chat photos (5b), Memory, schedule picker and New task, Backup, Settings,
  Hebrew (5c).
- **6 Quality.** One `h1`, named controls, 44 px targets, Skip link,
  gzip for static files, sixty after-screenshots.
- **7 Docs and PR.**

**Tests.** 1721 → 1788 passed on the branch after merging main (M42 part 6), 0
failed, 29 ignored. Browser check 25 steps (27 with the screenshot options), all passed.

**Repo rule.** The standing rule against touching images conflicts with the
brief's `docs/assets/m47/`. Resolution (D13): new files only, in a new
directory, JPEG, about 7.3 MB, as M37 did with `docs/assets/m37/`. README
untouched; its lines are in the final report.

**Plan fixes made while building.** 48 corrections, each in the commit that
found it, listed at the end of the design doc. The largest: the ≤ 45 KB
gzip budget for `app.js` was missed (about 52 KB) and the wire still
halved; the screenshot rig became a mode of the browser check instead of a
second tool.

**Known flake.** `ferrule-mcp browser::tests::a_browser_that_hangs_is_given_up_on`
can fail in a full parallel run (ETXTBSY fork race); unrelated.

**Not verified live.** A real phone over the real tunnel; real vision
providers with an image; a real Telegram photo; Hebrew read by a Hebrew
speaker.

**Follow-ups.** Translate the server's own sentences; a proofreading pass
on the Hebrew; a live check on a phone.

### 2026-09-30 — M42 part 7: pluggable compaction + session tree (Kimi)

**Scope.** Pi's two state-layer ideas from the M42 design, completing the
milestone (`docs/m42-harness-engineering.md` §7).

**What was built.**
- **The compaction strategy seam** (`ferrule-core::compactor`): a
  `Compactor` trait (`summarize` the folded head); the agent keeps the
  built-in checklist pipeline on its own model when no strategy is set,
  and the fold record is unchanged either way. `TemplateCompactor` is the
  first strategy — the same template on another model — wired as
  `[agent] compaction_model`, validated at agent build and run through a
  routed provider (the catalog's fallbacks apply).
- **Session tree:** `Transcript::open` reads a session without writing;
  `fork(session_id, at)` writes a new transcript with `parent`/`fork_at`
  meta plus the fold-applied messages up to `at`; `ferrule chat --fork
  SESSION [--at N]` starts a branch from that state (the parent never
  touched); `ferrule sessions [--all]` lists the tree, newest first,
  branches marked `↳ <parent>`, archived `/new` sessions behind `--all`.
  Dashboard console parity row for `sessions` (read) + its m37 doc row.

**Tests.** 1 transcript unit (fork prefix + metas + open-never-writes), 1
core it (`a_custom_compactor_writes_the_summary`: the summary comes from
the compactor's model, the agent's own model never sees the call), 1 cli
it (run → sessions lists it → chat --fork --at → branch names its parent;
forking a missing session is refused). ferrule-core 124+34, ferrule-cli
368+139, gateway/agents green; clippy and fmt clean.

**Not done.** Forking from a pre-compaction point (the fork view is
fold-applied); `chat --resume`; strategy calls get no ledger rows (the
default path's `call_provider` owns ledger/retries); a TUI tree view.

**M42 is complete: all seven parts built.**

### 2026-10-01 — M43 channels drive loops and graphs (Kimi)

**Scope.** The owner's chat starts and gates what M42 built at the
terminal. Design: `docs/m43-channels-drive-loops.md`.

**What was built.**
- **`/goal` door** (`goal_door.rs`, a gateway interceptor): owner-only;
  starts a loop on its own router lane under the reserved pseudo-channel
  `goal` (the scheduler's M3 no-double-delivery pattern), the lane
  session id naming the goal state file so the agent factory picks the
  loop up exactly like a CLI run (judge without file changes, verdicts
  recorded). The ending — met / pending / errored — is delivered to the
  originating chat through the door's own channel handles. `/goal` alone
  lists open loops (`goal::open_loops`). `goal::prepare_as` lets the
  caller name the session.
- **`/graph <file> [--yes]` door** (`graph_door.rs`): owner-only, the
  path relative to the workspace, the graph on the gateway's **shared**
  supervisor. `graph::run` now takes `RunOpts` (optional supervisor —
  one claim per process on the agents store; `hub` for approvals;
  `quiet` collects the ANSI-stripped report) and returns a `GraphReport`
  (code + lines). Approval nodes with a hub ask the owner's chat through
  `ask_owner` (buttons, timeout, audit), failing closed on a refusal.
- `/help` lists both; the report goes to the chat that started the work.

**Tests.** `tests/it/doors.rs` against the real gateway + fake Telegram:
chat-started loop reports Goal met (state file asserted: goal, one judge
run, `goal__` session), failing judge → Goal pending with the resume
command, chat-started graph → `graph: goal met`, stranger's `/goal`
starts nothing, `/goal` lists empty. The support mock module is shared
with `channels.rs` (`pub(crate) mod support`, no duplicate-mod). Full
cli suite 383+166, clippy and fmt clean.

**Not done.** Approvals for terminal-run graphs still ask at the
terminal (the notifier lives in the gateway process); no `/graph stop`
from a chat; loops/graphs from the HTTP channel hold no webhook.

### 2026-10-19 — Telegram files both ways + a whole-repo review pass (Claude, Opus 5.5)

**Scope.** The owner reported that the bot "can't send files in
Telegram". Cause: `TelegramChannel` reported no `attachments`
capability and its send path ignored `msg.attachments`, so `send_file`
always answered "this channel (telegram) can't send files". The same
session then reviewed the whole repo by area (six read-only reviewers)
and fixed the confirmed defects with small diffs.

**Telegram.**
- Out: `sendPhoto` for a JPEG/PNG/WebP up to 10 MB (re-sent as a
  document only on Telegram's own 400, never after a timeout, so no
  duplicates), `sendDocument` for anything else up to 50 MB, the text
  as the caption when it fits 1,024 UTF-16 units, a 300 s upload
  deadline. `capabilities().attachments` is now true.
- In: documents and videos are saved to the inbox like voice, audio and
  photos (GIFs stay unread); downloads get the 300 s deadline.
- `split()` could loop forever (unbounded memory) on a fence line
  longer than ~3.3k units; reopening a fence must now make progress.
- A 429 on a non-streamed send is waited out (≤ 3 tries, ≤ 60 s each)
  instead of dropping the reply and the rest of a split one.

**Other fixes.**
- `write_todos`/`log_diary` resolve their paths through
  `fs_tools::resolve` and write atomically; read-only roles lose them
  too.
- `read_skill_file` checks that a skill's directory is still a skill.
- MCP media is written only inside the workspace.
- SMTP validates envelope addresses.
- Config page: the change guard is stricter (duplicate table names,
  `sandbox`, `signal_cli`).
- Updates: the timer has its own `last_timer`; `last_check`, which Home
  and `/update` also write, no longer makes the night's install skip.
  The stale-promise line no longer claims a restart.
- Core: a torn transcript line is skipped (warned), not a whole lost
  history; compaction sends the main calls' `temperature` and
  `max_output_tokens` (OpenAI reasoning models refused `0`/`max_tokens`,
  and every later turn failed the same way).
- Gateway: `Router::retire` on a busy lane marks it stale and rebuilds
  it at the next message that finds it idle (two agents shared one
  transcript); `tasks run-now` builds its scheduler with
  `Scheduler::sharing`, no recovery (it failed the daemon's live run);
  an empty answer becomes a one-line note (a chat refuses an empty
  message, so it was silence); Slack slash commands no longer cache a
  human-to-human DM as the bot's.
- Dashboard: Channels Save can clear optional fields; a failed photo
  save answers in the chat log; plan sign-ins say when the provider
  couldn't be added.
- MCP over HTTP decodes SSE bytes across chunk boundaries; the OTel
  exporter no longer spins during backoff.

**Checks.** `make check` exit 0: fmt and clippy clean, 1841 tests
passed, 0 failed, 28 ignored. One existing assertion was flipped
(`capabilities_report_edits_and_reactions`: Telegram now has
attachments). No new tests were written for these fixes. The live
Telegram path (a real bot token) wasn't exercised; only the mocks were.

**Open (found, not fixed — needs a design call or a bigger change).**
- Security hardening of host-side helpers (hooks, git, the proxy) and
  of session-id mapping: in progress in separate sessions, to be
  reported through SECURITY.md's private advisory flow, not here.
- Updates: backup/restore in system scope use root's own paths; macOS
  `supervised()` trusts an inherited `XPC_SERVICE_NAME`; an update while
  the launchd job is booted out rolls back and pins a good release; a
  failed rollback records nothing; a unit failure before any event
  leaves a chat update unanswered until the promise expires.
- Core: no recovery from a provider's "context too long"; a kept tail
  over the trigger re-summarizes every iteration; `compaction_model`
  calls skip retries, ledger and kill switch; stalled streams aren't
  retried and the 600 s client timeout cuts long healthy ones.
- Channels: WhatsApp relay cursor survives a mailbox wipe; WhatsApp
  held messages can wait silently; a failed Discord interaction ack
  loses the reply; `Gateway::run` doesn't drain lanes; one email that
  can't be fetched blocks the channel; `/cmd@OtherBot` acts in groups.
- MCP stdio: the request write sits outside the timeout, and a cut-off
  write breaks the connection until restart.

### 2026-10-19 — Update and restore defects from the review (Claude, Opus 5.5)

**Scope.** The five update/restore defects the 2026-10-19 review pass
confirmed (its entry is on main's working tree, not yet committed), one
commit each, each with a test that failed before the fix. The review's
other two update fixes (`last_timer`, the stale-promise line) were made
there and are left alone here: `apply_unit`'s timer arm, `State`'s fields
and `promise::expire` are untouched, so the two merge.

**Fixes.**
- **System-scope backup/restore.** `main` pins `FERRULE_CONFIG`/
  `FERRULE_DATA_DIR` to the system service's for `backup` and `restore`
  too (`on_system_files`), so `sudo ferrule restore` no longer writes
  root's own paths. After a restore into the system data dir,
  `own_system_files` gives the data to the service's user and the config
  `root:<user>` 0640, as setup does. Every restore makes the secret dirs
  (`private`, `claude-code`, `ssh`, `proxy/keys`) 0700 again; they came
  back 0755 from `create_dir_all`. Tests:
  `sudo_backup_and_restore_use_the_system_services_files`, and the round
  trip in `tests/it/backup.rs` now asserts `private/` is 0700. The chown
  itself needs root and a service user, so no test runs it.
- **`supervised()`** counts only this instance's own job: launchd's
  `XPC_SERVICE_NAME` equal to its label, or (with `INVOCATION_ID`) a
  systemd unit, user or system, whose `MainPID` is this process. VS
  Code's inherited `XPC_SERVICE_NAME` made `/restart` exit a hand-run
  gateway for good. Test: `only_the_instances_own_job_or_unit_counts_as_supervised`
  (pure `supervised_by`).
- **A restart command that fails** (a booted-out launchd job:
  `kickstart -k` can't find it) rolls back with a `Failed` event and no
  pin, and `run` returns the error; before, it pinned a good release.
  `Svc::restart` on macOS bootstraps the plist when `launchctl print`
  finds no job (`launchd_restart`, unit-tested). Test:
  `a_restart_that_cant_run_rolls_back_without_pinning`.
- **A rollback that fails** records a `Failed` event saying so and where
  `ferrule.previous` is (or that it's gone), restarts the service anyway,
  then returns the error; before, it returned early with no event, no
  pin and no restart. Test:
  `a_rollback_that_fails_is_recorded_and_the_service_restarted`.
- **A chat's update that installs nothing is answered.** The requested
  half of `apply_unit` is `requested()`: when its run recorded no ending
  event (up to date, busy, a failed check, a held lock, a sibling with
  `auto = false` and nothing newer), it pushes a `Failed` event with an
  empty `to` and the reason. `promise::claim_for` accepts `Failed`
  (`EventKind::ends_update`); the watch claims before its once-per-version
  dedupe; the line is "Ferrule wasn't updated: …"; `report()` skips
  these answers (no standing warning in /status). Test:
  `an_update_asked_for_that_installs_nothing_still_answers_the_chat`.

**Decision.** No new `EventKind::NotInstalled`: serde refuses an unknown
variant, so an older binary (after a downgrade) would read the whole
state as fresh and lose its pins. A `Failed` event with no `to` reads
fine in every version.

**Checks.** `make check` exit 0 on the final tree (cli: 432 unit and 168
integration tests, 2 + 2 ignored). Each earlier commit of the series:
`cargo fmt --check`, clippy on ferrule-cli and the cli suite, all clean.
Docs: `docs/updates.md` (restart/rollback failures, the chat's answer),
`docs/backup.md` (system service, 0700), the supervision wording in
`docs/m36-self-update.md` and `docs/m37-control-room.md`.

**Not done.** `restart_hint()` on macOS still names `kickstart -k`, which
fails for a booted-out job; a `ferrule backup` run as root leaves a
root-owned `backup.json` in the system data dir (readable, harmless).

### 2026-10-19 — four host-side sandbox escapes closed (Claude, Opus 5.5)

**Scope.** The three gaps the 2026-10-19 whole-repo review left under
"Open" (each needed a design call): ferrule's own git honouring a
sandbox-written `.git/config`, workspace hooks running model-editable
scripts as the owner, and the proxy forwarding a compressed body from a
secret-bound host unscrubbed. Worked from `main` in a worktree; one commit
per hole, each with a test that failed before the fix.

**Git (`874a57f`).** New `ferrule_sandbox::host_git::git(dir, args)`, now
the only way `ferrule-agents/src/worktree.rs` and
`ferrule-eval/src/fixture.rs` run git. It lists the config with
`--show-scope` and empties `clean`/`smudge`/`process` of every filter
driver defined outside system/global/command config (git-lfs from `git
lfs install` keeps working; a repo-level LFS install is off for these
calls); pins `core.hooksPath=/dev/null` (the old
`$TMPDIR/ferrule-no-git-hooks` was writable by sandboxed commands — found
while fixing, covered by the same test), fsmonitor and signing off; kills
a call after 300 s (a FIFO as `.git/HEAD`). The snapshot diff adds
`--no-textconv` (an empty textconv override makes git fail, so diff
drivers are handled by flags, not config). Needs git ≥ 2.26. Tests:
`worktree::drivers_and_hooks_planted_in_the_repo_never_run_outside_the_sandbox`
(before: smudge, clean, textconv and the hook all ran),
`fixture::tests::a_filter_planted_before_git_init_never_runs`, two unit
tests in `host_git`. Docs: `docs/sandbox.md` "Ferrule's own git".

**Hooks (`eb15f40`).** Chose sandboxing over fingerprinting the named
scripts: a fingerprint can't follow what a hook loads (`make guard`,
`npm run lint`, imports, a sourced `lib.sh`). `CommandHook` has an
optional sandbox; `ferrule_hooks::build` takes one and gives it to
workspace hooks only; the CLI passes the shared sandbox. Trust prompts
(CLI, dashboard, self-service card) say they run in the sandbox. Test:
`tests::workspace_hooks_run_in_the_sandbox_and_user_hooks_as_the_owner`
(a refusing sandbox: the user hook runs, the workspace hook gets the
refusal; fails with the routing line reverted). Docs: `docs/m18-hooks.md`
§3 "The fingerprint pins the commands, not the scripts", roadmap line.

**Proxy (`5fd2a05`).** Fail closed, no decoding: any content or transfer
coding other than identity/chunked, in any header of either name, gets
502 with the reason, unless the body is empty (HEAD, 304). Also fixes
`identity` + `gzip` headers slipping through (only the first was read).
Tests: `proxy::an_encoded_body_from_a_bound_host_is_withheld` (before:
200 with the real token), `any_coding_but_identity_or_chunked_is_found`.
Docs: `docs/research-credential-gateway.md` §2, §3, §5, §6.

**core.worktree (`7a1f431`), found while fixing the above.** A worker
may write its repo's git dir (so it can commit); with
`extensions.worktreeConfig` it could set `core.worktree` for its own
worktree to any directory, and close's host-side `add -A` + commit then
put that directory's files, read as the owner, on the kept branch. (For
the main worktree the redirected top usually isn't a repo, so the
snapshot's `-C top` calls failed rather than leaked; the linked-worktree
path did leak.) `host_git` now pins `--work-tree` to the directory
holding the `.git` git found whenever repo-scope config sets
`core.worktree`; `-c core.worktree` can't override it. Refusing was
rejected: submodules set `core.worktree = ../../../sub` in their own
config. Test: `worktree::a_work_tree_moved_by_the_repos_config_doesnt_move_what_close_commits`
(before: the branch held the outside `creds`, not `lib.rs`/`new.rs`).

**Checks.** `make check` exits 0 on macOS, last run with `7a1f431` in.
Two earlier runs (before the core.worktree fix) failed only on `ferrule-cli` `managed::sigterm_drains_and_exits_zero`
(`assert!(marker.exists())`), with load average 10–14 from other
sessions' builds; the test passes alone, in its full binary, and with
`--workspace` features, on this branch and on `dc837a7` alike. Linux and
Windows are left to CI: on Windows the worktree test's filters run
through Git Bash with forward-slash paths.

**Open (found, not fixed).**
- `managed::sigterm_drains_and_exits_zero` races under load: the
  gateway serves `/healthz` once `dash.bind()` returns and writes
  `dashboard.json` after (`main.rs`, `write_marker`), but the test asserts
  the marker right after the first healthy answer. Wait for the marker
  in the test, or write it before serving.
- This entry's PLAN.md neighbour on `main` (the review entry) is
  uncommitted there; merging this branch may need the two tails joined.

### 2026-10-19 — managed SIGTERM test no longer races the dashboard marker (Claude, Opus 5.5)

**Scope.** `managed::sigterm_drains_and_exits_zero` failed about 3 runs
in 11 when the whole cli test binary ran (it failed on `dc837a7` as well,
so `make check` sometimes failed). Cause: `Dashboard::bind` starts its
accept loop before it returns the port, and `run_gateway` writes
`data/gateway/dashboard.json` only after that. The test asserted the
marker as soon as `/healthz` answered. The marker can't be written
earlier without moving it into `bind`: the port isn't known before the
listener is bound, and other callers use `bind` too. So the test
now waits for the marker, up to 10 s. Product code is unchanged.

**Checks.** `cargo test -p ferrule-cli --test it --locked` ran 10 times
in a row with 0 failures. `make check` exit 0: 1841 passed, 0 failed,
28 ignored. That's macOS only; Linux and Windows weren't run locally (CI
covers them).

### 2026-10-19 — agent-loop defects from the review pass (Claude, Opus 5.5)

**Scope.** The six core/provider defects the 2026-10-19 review pass
confirmed (its "Open" list, Core line, plus two more). One commit each,
a test each. Branch `claude/eager-aryabhata-08a80f`.

**Fixed.**
- **Streams** (`common::client`): the client-wide 600 s `timeout` (which
  covered the body and cut long healthy streams) is a 600 s
  `read_timeout`; streams keep their 300 s stall limit. The retry budget
  (`RetryPolicy::budget`, 120 s) now counts only the waits between tries,
  so a stall found after 300 s is retried.
- **Stop reasons**: `CompletionResponse.stop_reason` keeps the API's word
  (Anthropic, Chat Completions and Responses fill it; every other
  constructor says `None`); `cut_off()` reads it. An empty final answer
  or one cut off by the output cap ends the run incomplete
  (`RunIncomplete`, `Agent::incomplete`); the text so far still goes out.
- **Anthropic thinking mid-loop**: text pushed after tool results (inbox,
  stuck nudge, wrap-up, PostCompact) rides in the tool_result wire
  message, so to the API the tool-use turn goes on and its thinking must
  come back (platform.claude.com/docs/en/build-with-claude/thinking,
  "Thinking with tool use", read this session). The driver had taken that
  text for a new turn and sent the turn neutral; `loop_start` now skips a
  user message that follows results.
- **`[agent] compaction_model`**: no longer a `TemplateCompactor`; the CLI
  sets `Agent::with_compaction_provider`, and the summary goes through
  `call_provider` (retries, ledger row, budget, fallback). Every
  compaction, and the wrap-up's status call, now race the kill switch.
- **"Context too long"**: the estimate adds tool schemas; on
  `FailureClass::ContextTooLong` the turn (or status) call compacts once
  regardless of the estimate and tries once more (`call_turn`).
- **Re-summarizing**: a head that is mostly an earlier summary (the kept
  tail alone over the trigger) isn't folded again until there's as much
  new to fold; the comment at the old spot claimed this, no code did it.

**Checks.** Per commit: the touched crates' tests, clippy, rustfmt; then
`make check` on the result. Its one failure is `ferrule-cli`'s
`managed::sigterm_drains_and_exits_zero` (managed.rs:485), now and then:
the gateway serves `/healthz` from `dash.bind()` before `write_marker()`,
and the test asserts the marker as soon as `/healthz` answers. It fails
the same way on the base `dc837a7` (3 of 11 runs of the cli test binary
there, 1 of 5 on this branch in the same alternating loop). The `routing_off` golden was rewritten (its "too long" turn
now recovers; estimates +27 for its two tool schemas). M24's
`typical.json` was re-measured (the mock through the real binary, base
`dc837a7` vs this branch, same paths): three starter tasks moved, all 20
still pass. release-notes 10 -> 9 calls and incident-report 12 -> 11 (one
repeat summary fewer each); config-migration 8 -> 11, since with the tool
schemas counted it now compacts three times — the base never reached
the trigger on it, though the suite's context tasks are built to
overflow the 32k window.

**Merging with main's uncommitted review-pass changes.** `compactor.rs`'s
doc comment conflicts with main's hunk there: keep main's code
(`temperature: None`, `max_output_tokens: None`) and this doc. This entry
and main's 2026-10-19 entry both append to PLAN.md: keep both.

**Open.**
- The Responses driver still uses `common::last_user` for its replay
  boundary; whether OpenAI wants reasoning items back after text added
  mid-loop wasn't checked.
- Replayed native blocks (signed thinking, encrypted reasoning) still
  aren't in the estimate; the "too long" retry covers a miss.
- The managed test's startup race (above): wait for the marker, or
  write it before the page serves.

### 2026-10-19 — Responses: does text pushed mid-loop drop reasoning? (Claude, Opus 5.5)

**Scope.** The previous entry's first Open item. The Responses driver
replays native items only after `common::last_user`, so text the loop
pushes after tool results (inbox, stuck nudge, wrap-up, PostCompact)
makes the loop's assistant turns go out neutral, without their reasoning
items. Anthropic's fix (`loop_start`, 6fe75de) rests on that API taking
the text as part of the tool-use turn. Checked whether OpenAI's docs say
the same; they don't, so no code changed. Branch
`claude/responses-reasoning-boundary`, on `claude/eager-aryabhata-08a80f`.

**Sources** (all read 2026-10-19):
- R: developers.openai.com/api/docs/guides/reasoning, "How reasoning
  works" and "Preserve reasoning across calls" › "Keeping reasoning
  items in context".
- C: developers.openai.com/cookbook/examples/responses_api/reasoning_items,
  "Caching" (marked archived).
- S: developers.openai.com/api/docs/guides/steering.

**Answer: no error, and nothing lost on models before GPT-5.6.**
- *Error:* none documented. Neither R nor C says the API rejects input
  missing a reasoning item, and the driver's neutral `function_call`
  items carry only `call_id`, no item `id`, so nothing in the input
  points at a reasoning item that isn't there. The reasoning-400 retry
  (`plain`) stays as the net.
- *OpenAI's boundary is the last user message.* R asks for every
  reasoning, function call and output item "since the last `user`
  message", the same line `last_user` draws. Anthropic is different
  because the pushed text rides inside the tool_result wire message;
  this driver sends it as its own `{"role":"user"}` input item after the
  `function_call_output`s, so a new turn starts there.
- *Models before GPT-5.6* default to `reasoning.context = "current_turn"`
  (R's table): earlier turns' reasoning can stay in the payload but isn't
  rendered into the next sample. C says the same of the previous turn's
  reasoning items (ignored and removed; sending them is harmless).
  Replaying the loop's items in front of the pushed text would change
  nothing the model sees, so dropping them loses nothing.
- *Steering* (S), OpenAI's own way to add user input mid-turn, is GPT-6
  over a WebSocket with `previous_response_id`. It doesn't apply to this
  stateless HTTP path and says nothing about reasoning.

**A wider gap found on the way (not fixed).** R: the GPT-5.6 family
defaults to `all_turns`, which renders available, compatible reasoning
from earlier turns (GPT-6.1 Sol supports it too, default not stated; R
shows it working with `store: false` when every output item is replayed).
The driver sends only `reasoning.effort`, never `reasoning.context`, and
`replayable(…, in_loop)` sends every assistant turn before the last user
message neutral (M23 design §2). So on GPT-5.6 the model loses its
earlier reasoning at every user message, not just at mid-loop text;
`loop_start` would only cover that slice.

**Checks.** No code changed. `make check` on this branch: the first run
failed only `managed::sigterm_drains_and_exits_zero` (managed.rs:485, the
startup race in the previous entry); the second exited 0 with 1850
passed, 0 failed, 28 ignored.

**Open.**
- GPT-5.6+ under `all_turns`: either replay same-model reasoning across
  turns (weigh the cached prefix, the estimate, and the size of encrypted
  items), or send `reasoning.context = "current_turn"` to keep today's
  design explicit. A design call for the owner.

### 2026-10-20 — Responses replays reasoning across turns (Claude, Opus 5.5)

**Scope.** The previous entry's Open item: GPT-5.6 defaults to
`reasoning.context = "all_turns"`, but the driver sent every turn before
the last user message neutral. Max picked option (a): replay the same
driver's, same model's reasoning on every turn. Branch
`claude/vigorous-bardeen-9619b8`, on `claude/responses-reasoning-boundary`.

**Sources** (read 2026-10-20): R and C as in the previous entry, and the
API reference, developers.openai.com/api/reference/resources/responses/methods/create,
`reasoning.context`: `auto | current_turn | all_turns`, omitted = the
model's own mode, GPT-5.6 `all_turns`, earlier models `current_turn`.
Neither page says what an earlier model or another host does when the
field is sent.

**Why (a), not (b).** (b), sending `current_turn`, changed nothing the
model saw: no earlier reasoning went out, and R says the setting "does
not create reasoning items that are not already available". It would only
add a field that OpenRouter, the ChatGPT-plan backend or an earlier model
might reject. (a) gives GPT-5.6 its earlier reasoning; earlier models
drop it unread (C: "including them is harmless"). The previous loop also
no longer flips from native to neutral at the next user message, so the
cached prefix stays byte-stable through it.

**Changed.**
- `responses.rs`: `replayable(m, API, &self.model, true)` for every
  assistant turn; `reasoning.context` stays unsent (the model's default).
  The reasoning 400/404 retry without items (`plain`) is unchanged.
- `common::last_user` is gone (Anthropic uses its own `loop_start`).
- Test `earlier_turns_reasoning_goes_back_to_the_same_model_only`: after a
  new user message the turn's output items go back verbatim; another
  model gets neutral items. The old test's tail, which asserted the
  opposite, moved into it.
- `docs/m23-drivers.md` §2 and the risk table say Responses replays every
  turn, with the sources and the cost.

**Checks.** `cargo test -p ferrule-providers --lib responses` (12
passed; the new test fails on the base, where the turn went out
neutral). `make check` exited 0: 1851 passed, 0 failed, 28 ignored. Not
checked against the live API: no call to GPT-5.6 was made.

**Open.**
- On GPT-5.6, rendered earlier reasoning takes up context that
  `est_context_tokens` doesn't count, so compaction fires late and leans
  on the "too long" compact-and-retry (one failed call each time). Counting
  it needs a token measure for encrypted items, or the last call's
  `usage.input_tokens`.
- Whether OpenAI rejects encrypted reasoning replayed after the prefix
  changed (recall appended, MCP tools hot-added) is undocumented; the
  retry without items covers it at one extra call.

### 2026-10-20 — The estimate counts replayed reasoning (Claude, Opus 5.5)

**Scope.** The previous entry's first Open item: on GPT-5.6, rendered
earlier reasoning wasn't in `est_context_tokens`, so compaction fired
late and each miss cost a failed call. Branch
`claude/estimate-native-reasoning`, on `claude/vigorous-bardeen-9619b8`.

**Measure.** The last turn's call count, not a per-item guess. An
encrypted item's size has no published mapping to tokens, and whether a
model renders earlier reasoning at all depends on its `reasoning.context`
(GPT-5.6 `all_turns`, earlier models `current_turn`), which core can't
see. The provider's count already reflects what the model rendered.
After a turn's answer is pushed, `Agent::measure` keeps
`input_tokens + output_tokens` (the prompt, plus the answer the next
prompt carries back: output tokens include reasoning tokens, OpenAI's
`output_tokens_details.reasoning_tokens`) less the guess for that
history. `est_context_tokens` adds that excess while the history it
counted still stands, checked as the same length and the same guess for
every message but tool results. A fold or a truncation ends it.
Shortening and deduping tool results don't, since they leave the
reasoning in place. The guess is never lowered.

**Only `anthropic` and `responses` answers.** Both drivers report one
call's usage (`anthropic.rs` sums fresh, cache-read and cache-write
input; `responses.rs` reads `/input_tokens`). The Claude Code engine's
usage is the `result` event of a whole `claude -p` run
(`ferrule-plans/src/claude/engine.rs`), which can take several calls,
so its answers record nothing. Chat-completions answers carry no native
blocks and record nothing either. ponytail: picked by api name in core,
so a new native driver gets added to `measure`.

**Known overcount, bounded.** The count includes the latest answer's
reasoning. Anthropic, outside the tool loop, and earlier Responses models,
after a user message, don't render it, so the first check of a new turn
can run up to one answer's reasoning high. The next call's count
replaces it.

**Changed.**
- `agent.rs`: `Measured`, `conversation_est`, `Agent::measure` (called
  after each turn's answer is pushed), and `est_context_tokens` adds the
  excess. The ponytail comment saying native blocks aren't counted is gone.
- Test `a_native_drivers_count_of_replayed_reasoning_moves_compaction_up`:
  a Responses answer counted at 6,000 against a trigger of 5,000 makes
  the next turn compact before its call, and the fold's own after-estimate
  drops back under the trigger. A `claude-code` answer with the same count
  doesn't compact. With the `measure` call disabled, the test fails at
  "compacted".
- `docs/m23-drivers.md` §2: the Responses paragraph says what the
  estimate counts now.

**Checks.** `make check` exited 0: 1852 passed, 0 failed, 28 ignored
(one more than the previous entry's 1851: the new test).
`managed::sigterm_drains_and_exits_zero` passed this run.

**Open.**
- `Message::native`'s doc in `message.rs` still says blocks go back
  "inside the tool loop in progress", but Responses has replayed them on
  every turn since 815cf3d.
- Not checked against the live API. Whether GPT-5.6's `input_tokens`
  counts rendered earlier reasoning is assumed from what usage means, not
  observed.

### 2026-10-20 — M50 connect everything from the dashboard (Claude, Opus 5.5)

**Scope.** Max: "check the agent dashboard, suggest improvements, maybe
design the dashboard new … that he will connect to everything with
ease." A walk of an empty install, a design brought to Max first, then
the build. Branch `m50-connect` (based on `fix/telegram-files-review-pass`);
review, design, decisions, corrections and what was verified in
`docs/m50-connect.md`.

**Max's decisions.** All four new abilities (MCP by address with a
sign-in or a key, keys for tools, Discord and Slack cards); Connect takes
Usage's place in the phone bar; setting up in the browser with no
terminal is its own next milestone; tests only for the new write paths'
security rules.

**What was built** (one commit per part).
- **0 Review and design.** Step counts per path, before shots.
- **1 Fixes from the walk.** Telegram's Test said "builder error" with
  no `[gateway]` table (a derived `Default` left the API URLs empty);
  Home said "running" with no model; real doc links and linked
  addresses; 413/503/408 as sentences; the session footer from the
  config; a racy managed test.
- **OpenRouter** accepted any key (its `/models` is public; `/key` now
  checks it) and connected the list's first model (now the curated
  everyday pick).
- **2 The connect panel**, **3 the Connect page** and nav, **4 calmer
  pages** (empty Models, services-first Connections, Telegram's how-to,
  Pin a chat on any channel), **5 MCP by address and keys for tools**,
  **6 Discord and Slack cards**, **7 docs and after shots**.

**Tests.** 1841 → 1844 passed, 0 failed, 28 ignored. Browser check 27/27
after every part. fmt and clippy clean. **Known flake:**
`health::status_answers_from_any_chat_while_a_turn_hangs` failed once in
the last full run (its mock server's read under load); it passed alone
3/3 and in a rerun of the integration target.

**Not verified live.** A real Discord/Slack app, a real MCP server with a
key or sign-in, the ChatGPT sign-in from the panel, a phone's browser,
the panels in Hebrew.

**Follow-ups.** Set up in the browser with no terminal (agreed as the
next milestone); Discord's slash commands and DM-code pairing from the
page; a simpler Google Calendar path; Hebrew for the panels' words.

### 2026-10-20 — Telegram feels smoother: merged follow-ups, quieter busy notice, places and contacts, no link previews (Claude, Opus 5.5)

**Scope.** The owner compared the Telegram bot with NanoClaw. Quick
follow-up messages each got "your message is queued" and a turn of their
own. A shared location was dropped without a word. The `/dashboard`
link came with a web preview. Gateway side only; other sessions own
approvals (`ferrule-trust`) and the Claude-engine tool bridge.

**What changed.**
- **Merged follow-ups** (`router.rs` `run_lane`). When a lane takes a
  person's message, it also takes every person message already waiting
  behind it and runs them as one turn: the texts in order a paragraph
  apart, all attachments, the reply to the latest id, and `answered` for
  each id. A scheduler job or a `wake` met while gathering is held for
  the next turn, never merged. Channels without busy notices (the HTTP
  API, which pairs each request with its answer) keep a turn per message.
  Mid-turn injection through the core's `Inbox` was left out: the
  supervisor already owns the root's inbox, its items arrive in tags the
  root's prompt says to treat as untrusted agent data, and a message
  during the last model call would need its own hand-back. Merging covers the reported case without touching
  `ferrule-core`.
- **Busy notice** after 30 s, not 3 s (`BUSY_NOTICE_AFTER`), and
  `[gateway] busy_notice_secs` sets it (wired in `ferrule gateway`).
- **Telegram location, venue, contact** become text (`shared()` in
  `telegram.rs`): `[location: lat, lon]`, `[venue: title, address (lat,
  lon)]`, `[contact: name, phone]`. Live-location edits are still not
  followed.
- **No link previews** on every `sendMessage` and `editMessageText` the
  bot makes (`link_preview_options.is_disabled`).
- Tests that sent two messages to one chat back to back now wait for the
  first turn (or expect one merged turn), and the streaming test's exact
  payload includes the new field. Docs: `docs/channels.md` "Messages sent
  while it works".

**Not run.** `make check` was not run in this session (the owner's
standing rule: no runs unless asked). Run it before committing.

**Follow-ups.** A short gathering window before the *first* message's
turn starts (today only messages that arrive during a turn merge); a
person's message reaching a running turn through a composite inbox.

### 2026-10-20 — "Allow for this run": one approval per command per run (Claude, Opus 5.5)

**Why.** In one Telegram run the owner was asked four times in a row to
approve `rm -rf $T` (recursive delete), each time with a new code.
`TrustGuard` asked on every match, `Approvals` issued single-use codes,
and nothing remembered a yes.

**What changed** (`ferrule-trust`, plus one word in the email channel's
`approval_line` so an emailed `always k7` with a signature still counts as
an answer; docs in `docs/m19-trust-cost.md` §7 and §11).
- A third answer, `always` / `always <code>`, and an **Allow for this
  run** button between Allow and Refuse (`Answer::Always`,
  `Approvals::open_for_run`). Only a gated question offers it. On any
  other question `always` is a plain yes.
- `Hub` keeps the grants in `RunState::granted`, keyed by the subject the
  owner was shown: "`<command>` (<kind>)". `begin_run` starts empty, and a
  grant only lands in the run that asked. `TrustGuard` checks
  `Hub::use_grant` before routing. Each use goes to the audit log as
  `approval_answered` with route `grant`, answer `granted`.
- Shell gates only: a connected service's or a plugin tool's subject
  names just the tool, so a grant would cover any arguments. Those still
  ask every time.

**Not done, on purpose.** Exempting recursive deletes inside the
workspace or temp dirs (the optional item). The observed `rm -rf $T` is
not a literal path, so the exemption would not have helped. The sandbox
confines writes to the workspace, but deleting the workspace is what the
gate exists to catch. The terminal route and the dashboard's Allow keep
yes/no only.

**Not verified.** Per the owner's global rules, nothing was built or run
and no tests were added; `make check` has not been run on this change.

### 2026-10-20 — Claude-plan bots get send_file, connections and MCP tools (Claude, Opus 5.5)

**Scope.** The owner's Telegram bot on the Claude subscription engine
said it "can't send files to Telegram" on 0.13.1, and that Gmail was
connected but it had no tool to send through it. The cause: the engine
bridges a fixed list of Ferrule tools into `claude -p`, and the list
lacked `send_file`, the connection tools and every MCP tool.

**What changed.**
- `claude/engine.rs`: `BRIDGED_TOOLS` gains `send_file`,
  `connection_request` and `connection_list`. `bridged()` also passes
  every tool named `mcp__*` (`BRIDGED_PREFIX`): native connections
  (`mcp__gmail__gmail_send`…), `[mcp]` servers and the browser. Their
  names are dynamic, and claude loads no MCP server of its own
  (`--strict-mcp-config`). The bridge needed nothing else: a bridged
  call comes back to the agent as a `tool_call` and runs the agent's own
  `SendFileTool`, which is already bound to the session's chat.
- `prompts::CHAT_FILES`, appended where the gateway factory registers
  `send_file` (a chat session with a local workspace): one fixed
  paragraph saying the chat takes files through `send_file`. Both
  engines get it, since the claude engine passes the system messages on.
  It is constant per session, so the prefix stays byte-stable. It changes
  once, on upgrade.
- Docs: m35 §7.4's bridge table and the guide's "What works" row.

**Left out on purpose.** `child_builder` registers neither `send_file`
nor the connection tools: a sub-agent's answer goes to its parent, which
sends the file. Connection requests were already root-only (M20).
Sub-agents do get MCP tools, so on the claude engine they now get them
bridged too.

**Not verified.** Per the owner's standing rule, nothing was built or
run in this session beyond `rustfmt`. `make check` and a live Telegram
turn on `claude-code/*` ("send me <file>", "what's in my Gmail") are
still to do. No tests were added (not asked).

### 2026-10-20 — Research: Harness², the 2026 field, media generation and the dashboard Safe (Kimi)

**Scope.** Max: pull latest, read google-research/harness2, research the
top 5 existing agents, audit ferrule's code/design/dashboard/security/
abilities, find what we're missing — OpenRouter image/video mid
conversation, sub-agents on another OpenRouter model, self-improvement,
a "safe" in the dashboard — and stay in sync with other sessions via
PLAN.md. Research only; no code changed.

**What was done.**
- Repo pulled (`dadf357`, clean). PLAN.md re-read: current through M50 +
  the 2026-10-20 gateway/trust/claude-bridge entries.
- Read Harness² (arXiv:2610.14224): task-level Propose–Probe–Compose
  without ground truth, harnessing-level recursion, eight editable
  harness components. Ferrule has all eight components and an offline
  learning loop (M16); it lacks *online per-task* refinement and
  *verifier-free* probing.
- Field scan (Claude Code, Codex, Gemini CLI/Antigravity, OpenCode, Pi;
  OpenClaw as the community outlier): ferrule matches hooks, subagents,
  skills, sandboxing, provider-agnosticism; gaps are subagent effort
  control, LSP diagnostics in the edit loop, WhatsApp — and **no leader
  has native image/video generation tools**.
- Code audit confirmed: `spawn_agent` already takes a `model` (M21,
  supervisor-checked) — "another agent on another OpenRouter model" is
  built; vision input (M47) and Telegram file sending exist; **no media
  generation tool exists**; the dashboard can add new `[secrets]` keys
  but cannot rotate an existing one (`tool_keys.rs` refuses existing
  names).

**Findings written up** in `docs/research-harness2-media.md` (linked
from the README's Documentation section), with three proposals:
- **M51 media generation**: `generate_image` (OpenRouter `/images`,
  sync, reference images = editing) and `generate_video` (OpenRouter
  `/videos` async job API — submit, persist job ID, an M3 one-shot task
  polls and delivers the MP4 via `send_file`). Ledger `call_kind =
  "media"` caps; everything through the credential proxy.
- **M52 dashboard Safe**: add rotate-existing-key to the M50 keys panel
  (write-only, one confirm card, config undo covers the rest).
- **M53 contrastive probing**: Harness²'s one idea worth stealing,
  scoped into M16's spine — when a failed task has no verify command,
  keep a playbook delta only if a judge prefers the re-run's transcript;
  record active skills + playbook hash in transcript meta so "which
  harness produced this run" is answerable.

**Not done.** No code, no `make check` (docs + PLAN only; nothing built
or run per the standing rule). M51–M53 await Max's go. Left uncommitted
for Max to review.

**Sync note for other sessions.** If you build any of M51/M52/M53,
edit `docs/research-harness2-media.md`'s proposal section in place and
note it here — don't fork the design into a second doc.

### 2026-10-20 (late) — M51 design: media generation from the chat (Kimi)

**Scope.** Max approved starting M51 with the design doc.

**What was written.** `docs/m51-media.md` (linked from the roadmap as
"### M51 — media generation", status: design, awaiting Max's go):
- **`ferrule-media`** client crate in the `ferrule-embed` shape: one
  `MediaBackend` trait, OpenRouter implementation through the
  credential proxy (key from provider config, never the shell),
  `FakeMedia` for tests.
- **Two chat-bound tools** next to `SendFileTool`: `generate_image`
  (sync, reference images are the editing path, 120 s timeout, saves to
  `media/` and delivers to the chat) and `generate_video` (async:
  submit, one row in a `media_jobs` SQLite table, immediate truthful
  job-id answer; no tokens spent waiting).
- **The poller is a built-in scheduler job** (`with_builtin`, the M16
  precedent), not a model task: polls pending rows every minute,
  downloads and delivers finished MP4s, fails truthfully, survives
  restarts via SQLite.
- **Caps and honesty**: `[media]` table (defaults from capability
  discovery, `max_per_day` fail-closed), ledger `call_kind = "media"`,
  first call per run trust-gated as spend, sub-agents get the tools
  without chat delivery (the M12 rule).
- Verified seams while designing: `Tool` trait in `ferrule-core`,
  gateway chat-bound tools dir, `Scheduler::with_builtin`, ledger
  call-kind precedent (`"learn"`), embed crate's proxy pattern.

**Not done.** No code; `make check` not run (docs-only, per the
standing rule). M52 and M53 are still design-only in
`docs/research-harness2-media.md`. Uncommitted for Max's review.

### 2026-10-20 (late) — M52 design: the dashboard Safe (Kimi)

**Scope.** Max: "write the plan for M52 first."

**What was written.** `docs/m52-dashboard-safe.md` (roadmap section
"### M52 — the dashboard Safe", status: design, awaiting Max's go).
Read `tool_keys.rs` in full first; the design's hinge: **rotate changes
only the value**, so the "in use elsewhere" rule that makes M50's
add/remove refuse provider keys doesn't apply — rotate is the one
operation that is safe for exactly the keys M50 won't touch
(`OPENROUTER_API_KEY`, connection secrets, channel tokens).
- **Part 1, rotate**: `POST secrets/rotate {name, value}` — allowed for
  any name in the secrets file; write-then-swap (a failed write leaves
  the old value), same one-line value checks as add, the new value also
  pushed into the running process's env so providers pick it up without
  a restart; confirm card says what breaks if the new key is wrong;
  audited by name only.
- **Part 2, edit hosts**: `POST secrets/hosts {name, hosts}` for
  `[secrets]` tool keys only, same host grammar as add, written through
  `setup::write_secret_hosts` so the config page's undo covers it.
- **Part 3, the page**: the M50 keys panel becomes Safe, linked from
  Connect and Settings; each row shows name, hosts, "used by" in words;
  Rotate/Edit hosts/Remove in a `⋯` menu. Write-only is the page's
  stated promise: no reveal, no export, ever.

**Not done.** No code; `make check` not run (docs-only). M53 remains
design-only in the research doc. Uncommitted.

### 2026-10-20 (late) — M53 design: contrastive probing (Kimi)

**Scope.** Max: "write the m53."

**What was written.** `docs/m53-contrastive-probing.md` (roadmap section
"### M53 — contrastive probing", status: design, awaiting Max's go).
Read the `ferrule-learn` module surfaces first (`pass.rs`, `reflect.rs`,
`gate.rs`, `episode.rs`) so the design sits on what exists.
- **The probe path**: episodes without a verify command — today
  reflected on and dropped — get a second gate: one re-run in the
  `WorkspaceGate` scratch copy with the candidate lesson, then a judge
  call contrasts the two transcripts (fixed rubric: finished, fewer
  retries, truthful status, no new damage). Kept only if the probe wins;
  tie/unreadable/error rejects. Fail-closed, K=1, offline, inside the
  learn caps (`call_kind = "learn"`).
- **Provenance**: "kept by judge" next to "kept by check" in
  `learn show`; judge verdict + both transcript excerpts land in the
  pass folder; revert unchanged.
- **Harness versioning**: session-start transcript meta gains active
  skill list + playbook hash + config hash (hashes, never contents), so
  "which harness produced this run" is answerable from the ledger.
- **Deliberately out, stated for the next session**: online mid-run
  harness editing (breaks the byte-stable prompt prefix — the playbook
  changing *between* runs is the ferrule-shaped version), K > 1,
  parallel probes, playbook rewrites, judging memory merges. Possible
  follow-up named: a `/probe` chat door on the M43 pattern.

**Not done.** No code; `make check` not run (docs-only). All three
designs (M51 media, M52 Safe, M53 probing) are now written and await
Max's go. Everything uncommitted for review.

### 2026-10-20 — M54 protected autonomous mode: design and build (Claude, Opus 5.5)

**Scope.** The owner runs a personal Telegram bot on an Ubuntu server and
wants it to run and improve itself (NanoClaw-style) without a terminal,
with the keys still hidden. The stopgap they run now (`[sandbox] mode =
"off"`, `[trust] gates = false`) exposes `private/secrets.env`. This
session designs the replacement: `[sandbox] mode = "owner"`. Design in
`docs/m54-autonomous.md`.

**The design in short.** Commands may write everything the account can,
except the keys (hidden) and a protected set that decides what runs
outside the sandbox (read-only): the data dir, the config dir, the user
systemd dirs and `environment.d`. The binary moves into the data dir.
Outside the sandbox, the gateway runs only root-owned programs and its
own binary. Hooks, gates, worktree git and helper version checks go
through the sandbox. Credential files outside `private/` (the Matrix
session, …) get hidden in every mode. `ferrule_admin` change ops and
the trust gates stop asking.
New ops: `package_install`/`package_remove` (apt via `sudo -n` in the
gateway), `mcp_add`, `secret_request` (a Keys link to the owner, who
pastes the key; the asking session is woken), `secrets` and
`dashboard_link`. Transcripts and learn notes become readable. A
dashboard switch is the owner's way in and out.

**Review.** A builder-check pass read the design against the code.
Its findings are folded in:
- hooks and gate scripts run unsandboxed today;
- `hooks_trust` without a card would trust them;
- worktree git's `hooksPath` sits in a writable temp dir;
- other credentials sit in the data dir;
- helpers lift the socket allowlist;
- `offered()` can't decide who gets owner mode.

The macOS (Seatbelt) side wasn't checked in that pass.

**Status.** Design approved by the owner (2026-10-20). Built 2026-10-21 in
ten tasks and, at the owner's choice, merged into local `main` (not
pushed) after merging main's PR #48 and PR #47 into the branch; `make
check` there: 1881 passed, 0 failed, 28 ignored. The merge with autopilot
also closed two leaks it brought: `shell_job tail` and its status line
read a job's log in ferrule's own process, so a log swapped for a link
to a hidden file gave away its content or size. Design, with the rulings made while building:
`docs/m54-autonomous.md`; user docs: `docs/sandbox.md`, "Owner mode".

**Built, by task.**
1. The sandbox's owner mode (`ferrule-sandbox`): `Mode::Owner`, a
   read-only `protected` list, `/` writable carved around hidden and
   protected, `MAKE_DIR` on the way down, the socket allowlist kept,
   `narrowed()` for other people's chats. Linux only: macOS and Windows
   refuse it.
2. Spawns through the sandbox: `host.rs` (`set`, `command`,
   `system_program`), hooks, scheduler gates, transcription, the tunnel,
   `agent-browser --version`, claude's version check and updater, worktree
   git (`set_git`, hooks path `/dev/null` on Unix, in every mode),
   signal-cli; `git`, `ssh`, `systemctl`, `loginctl` and `ps` through
   `system_program`.
3. Host wiring (`ferrule-cli/src/owner_mode.rs`, `main.rs`): the protected
   set, `owner_tree`, `refusal`, the fallback and its owner-policy
   launcher, `arm` for every process, every `backup::SECRETS` entry but
   `ssh` hidden plus the gateway's state dir, the file tools refusing
   protected writes, `ferrule sandbox` and doctor. 3b: the claude engine's
   `guest()` sandbox (`Provider::guest`, `RoutedProvider`).
4. No gates for the owner's trees (`TrustGuard::gates`, `trust::equip`).
5. The binary moves into `<data>/bin` (hard link, units rewritten and
   reloaded, atomic symlink swap; `setup::refresh_service_with`,
   `service::rewrite`).
6. Change ops without the card (`ask_and_run`, the autonomous prompt
   strings, `config_restore` refusing guarded fields from the chat).
7. The new ops: `package_install`/`package_remove` (`packages.rs`),
   `mcp_add`, `secrets`, `dashboard_link`.
8. `secret_request` (`keys.rs`): the Keys link with a prefill, the wake
   when the key is saved, the reason allowlist.
9. The owner's switch: Settings, Autonomy; `sandbox/mode`, page-only.
10. Docs: `docs/sandbox.md`, `docs/m54-autonomous.md` (status and
    "Corrections made while building"), `docs/roadmap.md`, README.

**Main corrections to the design** (all in the doc's "Corrections made
while building"): Linux only; `system_program` instead of `PATH` for
unsandboxed spawns; `arm` in every process; `wanted` (config asks) versus
`on` (running); guests can't read `sessions/` or `learn/`; `<data>/ssh`
stays visible, `<data>/gateway` is hidden; a hard link and an atomic
symlink swap, not a rename; `relocated()` for non-gateway processes;
refusal beside another instance; apt's plan is checked; the
`secret_request` reason is an allowlist; `describe` still runs in owner
mode; after a fallback the helpers get an owner-policy sandbox, not the
fallback one; `mcp_add` passes the header key names to the probe. The
design's rows for the key-form `node` and cloudflared's version check
were test-only code.

**Checked.** `make check` passed on macOS: 1844 passed, 0 failed, 28
ignored, with one environment override (the eval starter-suite test needs
commit signing off, because the owner's global git config signs through
1Password; M54 doesn't touch it). **Not compiled:** the Linux-only
owner-mode code (`ferrule-sandbox` `linux.rs`, `supervisor.rs`, the
`cfg(linux)` apply call). A Linux `cargo check` or a CI run is needed
before merge. No live run yet: the "Done means" list in the design doc is
still to do on a Linux server. No test was added (not asked). Existing
assertions were updated where a signature or a count changed
(`self_service/tests.rs`: 35 to 41 ops; `update/tests.rs`;
`ferrule-plans/tests/it/claude_update.rs`).

**Follow-ups.**
- Fixed here because it blocked `make check`: a pre-existing clippy error
  from PR #46 in `crates/ferrule-gateway/src/router.rs` (~835), `while
  person && merges` rewritten as `if ... { loop {...} }`.
- The eval starter-suite test depends on the machine's global git signing
  config; it fails where commits are signed through an agent such as
  1Password.
- Pre-existing: `backup::SECRETS` doesn't list `gateway/http/clients.json`
  (the HTTP API clients' key hashes), so a backup contains it, and the bot
  can ask for a backup and read it.
- macOS owner mode: ancestor unlink-deny anchors and a hard-link probe on
  a Mac.
- Per-session helpers for guests (MCP servers, hooks, the browser, graph
  check commands, the host launcher, the shared workspace and its git
  filter drivers); until then guests reach the owner sandbox's reach
  there, never the keys.
- Deferred minors worth doing: wake from `config_follow` once the key is
  bound, not after a fixed delay; remember who asked in a file, not in
  memory; prune `ORIGINS`; `supervised()`/`INVOCATION_ID` exactness and
  `manager()`'s fallback check; the apt dry run's sudoers shape and its
  English-only message matching; `mcp_add` card pre-checks and the
  duplicated `${...}` scanner; `keys::bind`'s router order against Admin;
  `Command::new` outside tests re-grepped for a spawn the table misses.

### 2026-10-21 — M51 + M52 + M53 built (Kimi)

**Scope.** Max: "start building everything m51, m52, m53." Designs:
`docs/m51-media.md`, `docs/m52-dashboard-safe.md`,
`docs/m53-contrastive-probing.md`.

**M52 — the dashboard Safe.** `tool_keys.rs` gains `rotate` (any kept
key's value replaced write-then-swap, provider keys included — the name
and hosts don't move — pushed into the running process's env so it works
without a restart) and `hosts_edit` (page-shaped tool keys only, through
`write_secret_hosts`, config undo covers it); `list` now returns every
kept key with `used_by` in words and per-op flags. The Connect panel is
renamed **Safe** with Rotate/Sites/Remove per row. Dispatched as
`POST secrets/rotate` and `POST secrets/hosts`. New dashboard API test
covers the refusals, the confirm cards, the env update and the
write-only rule (no value ever returned).

**M53 — contrastive probing.** `ferrule-learn`: new `judge.rs`
(fixed-rubric transcript contrast, strict JSON, fail-closed), `Gate`
gains `probe()` (default: unsupported → old "no check" rejection keeps
working for gates that don't probe), `WorkspaceGate` shares one
`run_agent` helper between the check gate and the probe (the gate
agent's built-in stop-check is preserved). The pass probes and judges
check-less episodes instead of rejecting them; "kept by the judge" goes
in the changelog next to "kept by the check". Transcript meta: a
`harness` line per agent build — skill names + SKILL.md hashes, playbook
hash, config hash (FNV-1a, no deps, never contents) via
`Transcript::log_harness`, wired in `build_agent_from` (covers CLI,
gateway and children).

**M51 — media generation.** New `ferrule-media` crate in the
`ferrule-embed` shape: `MediaBackend` trait, `OpenRouterMedia`
(`/images/generations` sync with reference images, `/videos` submit +
poll, placeholder key + egress client through the credential proxy),
`FakeMedia`, and `JobStore` (SQLite `media_jobs`: pending videos survive
a restart; every generation counted for the cap). Gateway:
`GenerateImageTool` (save to `media/`, deliver to the session's chat)
and `GenerateVideoTool` (submit, row, immediate job-id answer), both
chat-bound like `send_file`; the `MediaPoller` built-in scheduler task
(every minute) polls, downloads, delivers, and says failures in the
chat. `[media]` config: provider, default models, fail-closed
`max_per_day`; ledger rows `call_kind = "media"`. Wired in
`gateway_factory` + `run_gateway` (bind on router, `ensure_builtin`
like M16). Deviations from the design are listed in the doc: no per-run
approval card, no sub-agent variant, no dashboard card / doctor line.

**Also fixed (not mine):** `router.rs`'s merged-follow-ups loop tripped
clippy's `while_immutable_condition` on main (the commit's PLAN entry
says `make check` wasn't run); rewritten as `if … { loop { … } }`,
semantics unchanged.

**Checks.** `make check` exit 0: fmt and clippy clean, **1856 passed,
0 failed, 28 ignored** (1844 → 1856). New tests: the Safe API test, 3
probe tests + judge unit tests in ferrule-learn, the harness-stamp
transcript test, ferrule-media unit tests, and 4 gateway media
integration tests (image saved+sent+counted, cap refuses in words,
video answered-now-delivered-by-poller, failure said in chat).

**Not verified live.** A real OpenRouter image/video call, a real
render delivered to Telegram, the Safe from a real browser, a real
nightly pass with a probe. Sub-agent media tools and the per-run
approval card are the named follow-ups.

**Merged.** PR #48 (5 commits + 1 Windows test fix) is in main as of
4e60ea3; CI green on ubuntu-24.04, macos-14 and windows-latest. (The one
CI failure along the way was mine: a test asserted a Unix path
separator.)

### 2026-10-21 — M55.2: stranded branch stacks landed (Claude, Opus 5.5)

**Scope.** Four local branch stacks held finished fixes that never
reached main. Their commits were cherry-picked with `-x` (oldest first,
stack by stack) onto `autopilot`, from `origin/main` `dadf357`. Nothing
was pushed. Each landed commit names its original SHA, so the SHAs the
stacks' own entries cite (`874a57f`, `815cf3d`, …) still resolve.

**Landed (original SHAs).**
1. Core loop. `claude/eager-aryabhata-08a80f` → `claude/responses-reasoning-boundary`
   → `claude/vigorous-bardeen-9619b8` → `claude/estimate-native-reasoning`:
   `git merge-base --is-ancestor` confirmed each is an ancestor of the
   next, so the whole stack is `origin/main..claude/estimate-native-reasoning`:
   d6afcd7, e8b9fc3, 6fe75de, d542468, e2d3072, 805f9f5, 9beaa9c,
   c906d53, 815cf3d, e57fce7, 9e794bf, 115295d.
2. Sandbox escapes, `claude/optimistic-gagarin-4ad269`: 874a57f, eb15f40,
   5fd2a05, 3b6f9d8, 7a1f431, 8744f75.
3. Update/restore, `claude/dreamy-sutherland-7fca0d`: 11aa0c8, b38f16f,
   7dacddb, 43c639f, 92f7908, 8c218b3.
4. Test race, `claude/musing-merkle-c282d5`: 68d00a0. Only its PLAN.md
   entry lands (see Conflicts).

**Skipped.** 6b1649f, which merged `origin/main` (dbfbc40) into the
sandbox branch. Main is already here, and that merge's only conflict was
PLAN.md order. `git cherry` found no stack commit already on main as a
patch.

**Conflicts and how they were resolved.**
- `crates/ferrule-core/src/compactor.rs` (d542468): only
  `TemplateCompactor`'s doc comment conflicted. Kept the stack's doc,
  because `compaction_model` now goes through
  `Agent::with_compaction_provider`, so main's "a cheaper one when
  compaction_model names it" no longer holds. Kept main's code (84b5f39:
  `temperature` and `max_output_tokens` unset). `agent.rs` merged without
  a conflict. Its summary request keeps 84b5f39's
  `self.config.max_output_tokens`/`temperature`, so `compaction_model`
  calls now send the main calls' settings too.
- `crates/ferrule-cli/tests/it/managed.rs` (68d00a0): main's 58272da
  (M50 part 1) already waits up to 10 s for the dashboard marker. Kept
  main's loop, so this commit lands only its PLAN.md entry.
- PLAN.md: every stack entry conflicted with main's tail only because
  both sides appended. All entries are kept whole, in date order, between
  main's 2026-10-19 review entry and its 2026-10-20 M50 entry:
  update/restore, then the four sandbox escapes (with the title as
  8744f75 left it), then the managed SIGTERM entry, then the core stack's
  four entries (two dated 10-19, two dated 10-20). The core entries stay
  together because each one opens with "the previous entry's Open item".
  The Current State hunks from 3b6f9d8 and e57fce7 applied cleanly.

**A fix the check needed (9bd5ffb, its own commit).** The first
`make check` stopped at clippy. `router.rs`'s `while person && merges`
trips `clippy::while_immutable_condition`, which is deny-by-default. The
loop came from main's 0b4e77d, so `origin/main` fails the same way; the
stacks didn't cause it (0b4e77d's entry says `make check` wasn't run).
It is now `if person && merges { loop { … } }`, with the same behaviour.

**Checks.** `make check` on 9bd5ffb exited 0: fmt clean, clippy clean with
no warnings, and 1869 passed, 0 failed, 28 ignored across 71 test
binaries. `managed::sigterm_drains_and_exits_zero` passed. This ran on
macOS only. No tests were added.

**Open.** The stacks' own Open items still stand (see their entries).
`Message::native`'s doc still says blocks are replayed only inside the
loop. Nothing was checked against the live GPT-5.6 API. `restart_hint()`
on macOS still names `kickstart -k`. Two items listed as Open above are
now closed here: the managed test race (main's 58272da) and the "uncommitted
neighbour" note in the sandbox entry.

### 2026-10-21 — M55.3: long shell output keeps its tail and exit code (Claude, Opus 5.5)

**Why.** The shell appended `[exit code: N]` and then `ToolOutput::capped`
kept the *first* 30,000 characters, so a failing `cargo test` with long
output showed the passing head and hid the failures and the exit code.

**What changed.**
- `ferrule-core/src/tool.rs`: `ToolOutput::spilled(text, lead, cap,
  workspace)`. Output that fits comes back whole, exactly as before. Over
  the cap, the whole text is written to a new
  `<workspace>/.ferrule/spill/<UTC time>-<8 hex>.log`, and the result is
  `lead` (the exit-code line), the first ~2k and last ~8k characters (a
  fifth and four fifths of a smaller cap), a "N characters cut here"
  marker between them, and a last line naming the file. The write follows
  the MCP media rules: the dir must resolve inside the workspace (a
  sandboxed command can plant links there) and the file is
  `create_new`. A failed write still returns the head and tail, with the
  reason. The dir gets a `.gitignore` of `*`, so `auto_commit` never
  commits logs. `ToolOutput::spill_preview` builds the same result for a
  caller that saves elsewhere.
- `capped` compared `len()` (bytes) to a character cap, and reported
  bytes as "chars total"; both are characters now.
- Local `shell`: uses `spilled`, so the exit code comes first and last.
- SSH `shell`: the spill goes to `.ferrule/spill/` in the *remote*
  workspace through the guarded remote write (`missing` guard, so it
  never overwrites), where the remote file tools can open it.
- MCP text results: `spilled` with no lead, at the same per-server and
  per-tool caps.
- Tests changed because the shape changed: `mcp_client::enabled_tools_…`
  (now in a temp workspace, so the spill doesn't land in the crate dir)
  and `sshd::timeouts_and_output_caps_…` (exit code first, the remote
  spill file holds all 200,015 characters). No new tests.
- Docs: `m34-ssh-local.md` (the shell's limits), `m17-mcp-add.md` (caps),
  README's Tools row.

**Secrets.** No known-values scrubber is reachable from `ferrule-core`,
`ferrule-tools`, `ferrule-ssh` or `ferrule-mcp` (`Proxy::scrub_text` and
the gateway's `Redactor` live in `ferrule-proxy`/`ferrule-cli`, and
`ToolContext` carries neither). The spill holds exactly what the command
printed, which the model would have seen anyway had it fit; commands see
placeholders, not real keys. Scrubbing spill files with the live value set
is left to **M54.3**, which plans one scrub call in the tool phase and the
same call for spill files.

**Checks.** `make check` exited 0: fmt clean, clippy clean, 1869 passed,
0 failed, 28 ignored across 71 test binaries (macOS; the `sshd::` tests
ran against the system sshd).

**Open.** Nothing prunes `.ferrule/spill/` (deleting in a dir a sandboxed
command can relink is a race, so it's the owner's to clean, like
`.ferrule/mcp-content/`). An MCP result spilled while the workspace is
remote (SSH) lands in the *local* workspace, out of the remote tools'
reach, as MCP media already does. The `shell` tool's description still
says "Output is truncated if very long" (left as is: it's in the prompt
prefix, and still true).

### 2026-10-21 — M55.4: the Claude engine bridges by registry, not a hand list (Claude, Opus 5.5)

**Why.** `BRIDGED_TOOLS` in `ferrule-plans/src/claude/engine.rs` named
seven tools that don't exist (`schedule_task`, `list_tasks`,
`cancel_task`, `send_message`, `board`, `post_to_board`, `read_board`),
and every tool it didn't name never reached a Claude-plan bot:
`update_memory`, `activate_skill`/`read_skill_file`, the M12 sub-agent
tools other than `spawn_agent` (`wait_agent`, `board_post`, `task_add`,
…), the M13 extension tools, WASM `plugin__*` tools and `log_diary`.

**What changed.**
- `bridged(req.tools)` now offers every tool the agent has, less
  `CLAUDE_NATIVE` and `NOT_BRIDGED`; `mcp__*` stays bridged.
  `CLAUDE_NATIVE` is what claude has its own of: `read_file`,
  `write_file`, `edit_file`, `list_dir`, `shell`, `web_fetch`, and two
  the old list also left out, `write_todos` (`TodoWrite`) and
  `code_search` (`Grep`/`Glob`). `NOT_BRIDGED` is `approve`, the
  bridge's own permission tool, so no ferrule tool can shadow it now that
  any registered name can reach the bridge (plugin tools are
  `plugin__…` and MCP ones `mcp__…`, so none does today).
- Kept: `send_file`, `connection_request`, `connection_list`,
  `ferrule_admin`, `web_search`, `spawn_agent` and the memory tools are
  still bridged (none is native or denied).
- `disallowed()` is untouched and still reads `req.tools`, so claude
  still loses `WebSearch`/`Task`/`WebFetch`/`Bash`/the edit tools when
  the agent lacks the matching ferrule tool; its test passes unchanged.
- Docs: m35 §7.4's bridge table (real names, the rule, what's left out
  and why), the guide's "What works" row, and two m48 notes that called
  the dead names a follow-up.

**Checks.** `make check` exited 0 (1869 passed, 0 failed, 28 ignored,
71 binaries; macOS) with `GIT_CONFIG_GLOBAL` pointed at an empty file.
With this machine's global git config, `ferrule-eval`'s
`starter_suite::every_reference_solution_passes_…` failed: its fixture's
`git commit` inherits `commit.gpgsign = true` with 1Password SSH
signing, and the 1Password agent returned an error. That test doesn't
touch this change and passed in the M55.3 run an hour earlier; it is
the fixture leaking the owner's git config, not a code failure. No tests
were added.

**Open.** Not run against a live `claude` turn. The unit test the gap
analysis suggests (every bridged name resolves in the registry) wasn't
written, per the no-new-tests rule.

### 2026-10-21 — M55.5: two dashboard login fixes (Claude, Opus 5.5)

**Why.** (a) `tunnel::open` returns as soon as cloudflared prints the
`trycloudflare.com` address, and `/dashboard` minted and sent the link
right then, before Cloudflare routed the tunnel, so the first tap often
failed. (b) `login()` called `Links::consume(token)`, which deleted the
link, and only then compared its host: a link opened on the wrong address
(loopback, an older tunnel) was burnt and the right tap then said "used".

**What changed.**
- (a) `Dashboard::remote_link` returns a `RemoteLink { url, ready }`.
  `open_tunnel` now also says whether it opened the tunnel; a newly
  opened one is probed with `tunnel_answers(host, TUNNEL_READY = 20 s)`:
  `GET https://<host>/healthz`, 5 s per try, backoff 0.25 s doubling to
  2 s, until the answer is the dashboard's own JSON (`status` and
  `version` strings; Cloudflare's error pages aren't JSON). Only then is
  the token minted. If it never answers, the link is still sent, with a
  line saying the tunnel is still coming up (`door::remote_text`, used by
  both `/dashboard` paths and the after-restart relink). A tunnel that
  was already open isn't probed, so the in-chat answer for it stays
  immediate. `ferrule dashboard link --remote` waits the same way.
- (b) `Links::consume(token, host)` checks the host under the link
  file's lock and returns `Consumed::{Used, WrongHost, Invalid}`; a live
  link made for another host is refused and kept. `login()` answers a
  wrong host with its own 401 ("made for another address") and logs the
  host it came in on (Debug-quoted) and the host the link was made for,
  never the token.
- Tests changed for the new signature: `auth::tests::a_link_works_once`,
  `tampered_expired_and_cleared_links_fail`,
  `sessions_survive_a_restart_…`; and
  `dashboard::tests::a_link_made_for_the_tunnel_works_only_there`, whose
  message said the wrong-host try "spent" the link: it now reuses the
  same token on the right host instead of minting a second one. No new
  tests.
- Docs: `m22-dashboard.md` (§1: the wait, and "a link is for its host"),
  `dashboard.md` (Opening it).

**Checks.** `make check` exited 0 (1869 passed, 0 failed, 28 ignored,
71 binaries; macOS), with `GIT_CONFIG_GLOBAL` pointed at an empty file
for the reason in the M55.4 entry.

**Open.** Not tried against a real quick tunnel. The probe goes out from
the gateway to Cloudflare and back, so a machine that can't reach its own
tunnel's address (no outbound HTTPS beyond cloudflared's) waits the full
20 s and then sends the link with the "still coming up" line. A second
`/dashboard` while the first one's probe runs finds the tunnel open and
answers without waiting.

### 2026-10-21 — M55.1: the two flaky CI tests, made deterministic (Claude, Opus 5.5)

Both fixes are in the product code, not the tests: each test was
catching a real race, and the asserted behaviour is unchanged.

**`sshd::the_credential_proxy_reaches_remote_commands` (ubuntu, run
39489322936 attempt 1).** The failure was curl's exit 7 ("couldn't
connect") through the proxy forward, with
`mux_client_request_session: read from master failed: Broken pipe` on
stderr. Cause: `start_master` put the proxy's `-R` on the master's own
command line (with `ExitOnForwardFailure=yes`) and called the master
ready as soon as its control socket existed. ssh opens the control socket
*before* the server answers that `-R`. Measured here with the system
ssh/sshd and a port already listening: the socket appeared at 113 ms and
the master exited at 117 ms with "remote port forwarding failed". So a
random `rport` that was already taken on the remote (on CI the remote is
the same busy runner, and `pick_port`'s 20000–59999 overlaps Linux's
ephemeral range) was never seen as `Failure::Forward`: the "another
port, up to 3 tries" path never ran, the restarted master kept the same
port and died again, commands fell back to plain connections without the
forward, and the first thing that really used it, the test's `curl`,
couldn't connect.
- Fix (`ferrule-ssh/src/link.rs`): the master starts without `-R`, and
  `request_forward` asks for it through the master with `ssh -O forward
  -R …`, which waits for the server's answer (40 s cap). Measured: a taken
  port exits 255 with "forwarding request failed: remote port forwarding
  failed …" (`classify` already maps that to `Failure::Forward`, so
  `ensure_master` drops that master and tries a fresh port) and the
  master stays up; a free port exits 0 and is listening on return.
- All eight `sshd::` tests pass against macOS's sshd with
  `FERRULE_REQUIRE_SSHD=1`. Not run on Linux.

**`update::tests::a_running_exe_steps_aside_on_windows` (windows, run
39488885531).** It failed at "moving …\\ferrule.exe aside: The process
cannot access the file because it is being used by another process (os
error 32)". Renaming a running exe is allowed on Windows; a sharing
violation means some other handle held the file without delete sharing
for a moment (typically an antivirus scanning an exe that was just
copied and started, as the test does). The real updater can hit the same
window when it renames the freshly staged copy.
- Fix (`ferrule-cli/src/update/swap.rs`): Windows' `rename_over` renames
  through `rename_retrying`, which retries only on
  `ERROR_SHARING_VIOLATION` (32), backing off 50 ms doubling to 500 ms,
  for up to 5 s; any other error returns at once.
- **Not compiled here**: it is `#[cfg(windows)]` and this Mac has no
  Windows target, so only rustfmt has seen it. Windows CI is its first
  build.

**Docs.** `m34-ssh-local.md` ("As built": the forward is asked for
through the master, and why), `m36-self-update.md` §3.3 (the retry).

**Checks.** `make check` exited 0 (1869 passed, 0 failed, 28 ignored, 71
binaries; macOS), with `GIT_CONFIG_GLOBAL` pointed at an empty file for
the reason in the M55.4 entry. No tests were added.

**Open.** The Linux and Windows runs are the real proof: push and watch
both jobs. A master that dies later for any other reason still lets
commands fall back silently to plain connections without the forward
(the mux client's own fallback); `/status` would still say "credential
proxy forwarded" then. M55.7 (require green CI) can follow once both
jobs pass.

### 2026-10-21 — M55.6: Current State rewritten, stale docs fixed (Claude, Opus 5.5)

**Why.** "Current State" listed 10 crates (there are 24), called about
25 merged milestones "PR open, not merged", stopped at M49, said
WhatsApp was unbuilt (it shipped in M39) and quoted test counts from
M8–M10. Every new session started from that. This is the one dedicated
in-place rewrite the gap analysis's sync protocol allows; no Session
Log entry was touched.

**What changed.**
- `PLAN.md` Current State, rewritten in place and checked against
  `git log` (PR merges #1–#46, tags v0.1.0–v0.13.1, `git merge-base`
  for what v0.13.1 contains), the crates' `Cargo.toml` descriptions and
  `docs/roadmap.md`: what Ferrule is; releases and branches (v0.13.1 has
  everything through M49 plus the 2026-10-19 review fixes; `main` at
  `dadf357` adds M50 and PRs #44–#46, unreleased; `autopilot` is PR
  #47); the check numbers; a table of the 24 crates; the shipped
  milestones M1–M50 in five lines; what's in flight (M54 on
  `claude/laughing-chebyshev-5cd4a6`, `autopilot`, Kimi's M51–M53,
  M55–M59); the number claims (next free: M60); and the sync protocol.
  The old toolchain notes for the Devi sandbox were dropped (stale; git
  history has them).
- `docs/roadmap.md`: M50 is merged (PR #43), not "on branch"; every
  other "PR open, not merged" status now names its merge PR; the Done
  table gains M39–M50 and a line on M45/M46 and v0.13.1; "Other open
  tracks" is rewritten: in flight (M51–M53, M54, M55–M59 with their
  items, since the report itself is git-excluded), what's still open
  (M12 part 6, M27's streaming leftovers, two M10 edges, each checked
  in the code), and what shipped since the list was written.
- Status lines of 24 shipped milestone docs (m12–m50) that still said
  "design", "in progress", "PR open" or only "built": each now names its
  merge PR;
  `m16-learning-loop.md` says what was built and that it's off until
  `[learning] enabled`.
- `docs/m22-dashboard.md` threat model and `docs/dashboard.md`
  "Security in three lines": the forms POST typed keys as plain JSON,
  so over a tunnel Cloudflare sees them; use `ssh -L` or `ferrule
  setup` to keep a key off the tunnel. (M52.1 changes this; its entry
  updates both.)
- `docs/m48-self-service.md` D9: notes PR #44's per-run grant for shell
  gates, and that admin ops still never get one.
- `docs/research-credential-gateway.md` §5: "only the shell goes
  through the proxy" is marked as the M7-era state, with what M10, M26
  and M33 changed and pointers to `sandbox.md`/`egress.md`.
- `README.md` Project layout lists all 24 crates; the test count is
  1869.

**Checks.** `make check` exited 0, with `GIT_CONFIG_GLOBAL=/dev/null`
for the reason in the M55.4 entry (the next entry fixes it). Docs and
PLAN.md only; no code or tests changed.

**Open.** The roadmap's per-milestone sections keep their open-edge
lists as they stood at the time (the section's intro says so); they
weren't re-verified one by one.

### 2026-10-21 — Eval fixtures don't read the owner's git config (Claude, Opus 5.5)

**Why.** `ferrule-eval`'s `starter_suite::every_reference_solution_passes_…`
failed on a machine whose global git config signs commits
(`commit.gpgsign = true`, 1Password's SSH signer): the `git-fix-commit`
task's `solve.sh` commits, and its `git commit` inherited the signer
(M55.4's entry). The fixture's own commit was already safe, because
`host_git` pins `commit.gpgsign=false`. The same leak hits real runs:
an agent's `git commit` in `ferrule eval` inherits the owner's signer,
hooks, aliases and identity, so a git task's verdict depended on the
machine, and m14-eval's "the user's git identity is never needed or
used" was only true of the fixture's first commit.

**What changed.**
- `ferrule-eval/src/fixture.rs`: `hermetic(sandbox)` returns the sandbox
  with `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL` set to the null
  device (`NUL` on Windows, as `ferrule-extensions`' git and its tests
  already do) and a `ferrule-eval <eval@ferrule.invalid>` author and
  committer. `run_command` (setup, graders, checks) always uses it, and
  `runner.rs` builds the agent (its shell and its check verifier) on it.
  Decided for real runs too, not just the test: a verdict mustn't
  depend on the owner's git setup, and a signer can pop a prompt in the
  middle of an eval.
- It uses `Sandbox::with_env`, which replaces the extra env. The eval's
  sandbox has none (`ferrule eval` builds it without the credential
  broker), and the doc comment says so. `ferrule-sandbox` is untouched.
- Docs: `m14-eval.md` (Fixture lifecycle, step 3), `eval.md` (Eval is
  hermetic).

**Checks.** `make check` exited 0 with this machine's normal global git
config (1Password signing on), no `GIT_CONFIG_GLOBAL` override: 1869
passed, 0 failed, 28 ignored, 71 binaries (macOS). From here on the
override isn't needed. No tests were added or changed.

**Open.** Git older than 2.32 ignores `GIT_CONFIG_GLOBAL` and still
reads the owner's global file. The M16 learning gate builds its agent
with `variant::build` on its own sandbox, not through a fixture, so its
re-runs still see the owner's git config.

### 2026-10-21 — M52.1: dashboard key fields are sealed in the browser (Claude, Opus 5.5)

**Why.** Cloudflare terminates a quick tunnel's TLS, and the page POSTed
every key typed into it (provider, Claude plan, connection, relay,
Google client, tool key, Telegram, channel forms) as plain JSON, so the
tunnel operator could read them. M22's threat model said it "never sees
a key or token" (corrected in M55.6, now true again for a passive
reader).

**What changed.**
- `ferrule-connections/src/keyform.rs`: `KeyForm::open` is now
  `open_text` (decrypt, UTF-8) plus its old API-key checks, so the
  dashboard can open a field without those checks; the relay form's
  behaviour is unchanged.
- `ferrule-cli/src/dashboard/sealed.rs` (new): `Seals`. `issue(session)`
  makes a one-time `KeyForm` and a 16-byte key id, held in memory for
  10 minutes, at most 64 at once (oldest dropped). `open_all(session,
  body)` replaces every `{"sealed": {kid, v, epk, iv, ct}}` anywhere in
  a body with its text; a key opens one envelope, only for the session
  (its CSRF token) that asked; anything that doesn't open refuses the
  request (400), so no handler runs. The warnings it logs name what
  failed, never a value.
- `dashboard/mod.rs`: `POST /api/seal` (session, origin, JSON and CSRF
  checked like every POST) answers `{kid, public}`; every other API POST
  goes through `open_all` before `api::route`, so no handler changed.
  `ferrule_admin`'s `api::act` doesn't pass through HTTP and is
  unchanged.
- `assets/app.js`: `asSecret(v)` marks a key; `api()` seals each marked
  value at send time (a fresh key each, so the confirm re-send of `act`
  works), using a copy of `relay/worker.js`'s `encryptKey` with the key
  id as salt and associated data. `values()` marks every `data-secret`
  field; the plan, provider, relay deploy/use, Google client, MCP
  address, tool key and Telegram senders mark theirs. A marked value that
  reaches `JSON.stringify` unsealed throws. Without WebCrypto (plain
  http on a non-local address) the page sends the value as typed, as
  before; with it, a failed seal fails the action.
- Ring's ECDH keys are single-use, so the "per-session key" is a
  one-time key per field, bound to the session; that's the keyform
  scheme as it stands, with no new dependency.
- Docs: `m22-dashboard.md` threat model (what's sealed, the active
  tamperer limit, `ssh -L` or a tailnet for key entry, the no-WebCrypto
  case), `dashboard.md` "Security in three lines", the roadmap's
  open-tracks note.

**Checks.** `make check` exited 0: 1869 passed, 0 failed, 28 ignored,
71 binaries (macOS). No tests were added or changed. The page's JS isn't
run by `make check`:
`scripts/dashboard_browser_check.mjs` (CI, all three OSes, real Chrome)
connects a service with a key through the page, so it exercises the
seal end to end; it wasn't run here.

**Open.** Kimi's uncommitted `secrets/rotate` in the main checkout
takes a raw `value` from the page: when it lands, its JS sender should
wrap the value in `asSecret` (the server side needs nothing). The login
link's token is still POSTed once as typed (it is one-use). An active
tamperer still wins (documented).

### 2026-10-21 — M52.5: a key pasted into a chat stops at the gateway (Claude, Opus 5.5)

**Why.** A key pasted into Telegram (or any channel) went into the
lane: the transcript kept it and the model provider saw it, and a
refusal from the agent came only after that. The gateway is the one
place every channel's message passes before anything reads it.

**What changed.**
- `ferrule-gateway/src/paste_guard.rs` (new): `holds_a_key(text,
  redactor)` is true for one of this machine's secret values or a token
  shape the `Redactor` already hides (Telegram and Discord bot tokens,
  Slack, Meta), or a known prefix at a word's start followed by 20+ key
  characters with at least 3 letter/digit switches. The prefixes are the
  credential proxy's `KNOWN_PREFIXES` (now `pub` and re-exported by
  `ferrule-proxy`, which `ferrule-gateway` now depends on) plus Google's
  `AIza`. The switch count is what keeps words, versions and
  `snake_case` names out (`hf_transformers_version_4_40_0` has none).
  `reply(deleted)` says why, where to put the key (the dashboard's
  Connect page or `ferrule setup`, until a `secret_request` link
  exists), and that a real key should be replaced.
- `gateway.rs`: `handle()` checks first, before `/new`, `/status` and
  every interceptor, so no door, approval or `/goal` reads it.
  `stop_pasted_key` logs only the channel ("stopped before the agent;
  it wasn't kept"), deletes the message where it can, replies (without
  `reply_to` once deleted) and calls `answered` so the HTTP API closes
  the request.
- `Channel::delete` (default unsupported); Telegram's is
  `deleteMessage`; the dashboard's chat replaces its log entry with
  "(taken out: it looked like a key)".
- Docs: `channels.md`, "Keys pasted into a chat" (every channel, the
  HTTP API included, what counts as a key, what's deleted where);
  roadmap note.

**Tests changed, none added.** The first `make check` failed two
`ferrule-cli` integration tests whose stuck turn's message carried the
provider's configured key (`health::the_heartbeat_names_a_stuck_turn_but_not_its_message`,
`dashboard::no_page_or_json_response_carries_a_seeded_secret`): the
guard now stops that message, so no turn hung. Each now sends the
key-carrying message first (stopped, and still checked never to show)
and an ordinary `HANG …` message for the stuck turn; every leak
assertion is unchanged.

**Checks.** `make check` exited 0: 1869 passed, 0 failed, 28 ignored,
71 binaries (macOS).

**Open.** Discord, Slack and the other channels can't delete (no
implementation; Discord could with Manage Messages in a server). A key
in an attached file isn't looked at. Keys with no known prefix and no
known shape (Brave's, Tavily's, a password) pass unless they are this
machine's own secret values. When `secret_request` lands (M52.3/M54),
`reply()` should carry its link.

### 2026-10-21 — M56.1: a message sent mid-turn reaches that turn (Claude, Opus 5.5)

**Why.** A correction sent while a turn ran ("no, the other file") waited
for the whole turn to end; PR #46 only merged it into the next one. The
core already drains an `Inbox` before every model call, but only
sub-agent reports used it, and the supervisor owns the root's inbox.

**What changed.**
- `ferrule-core` `hooks.rs`: `Inbox::take_messages` (default empty):
  whole messages, each pushed as it is after `take`'s joined one, so a
  person's message keeps its photos and isn't framed as agent data.
  `Inboxes`, several inboxes asked in order. `Agent::add_inbox` chains a
  second inbox beside the one the factory gave (`with_inbox` still
  replaces). `deliver_inbox` pushes `take`, then `take_messages`: still
  after the last tool results and before `maybe_compact` and the model
  call, so no call/result pair is split and the prefix only grows.
- `ferrule-gateway/src/steer.rs` (new): `WhileBusy` (`steer`, default;
  `queue`, PR #46's behaviour) and `Steering`, a lane's inbox. The lane
  opens it for a turn that answers a person (`person` jobs only: never a
  scheduled task, a plan or `/goal` turn, or a wake), keyed to that
  message's `sender_id`, and closes it when `run` returns. A message is
  framed `[sent while you were working] …` with its photos as
  `ImageRef`s (file lines are already in its text).
- `router.rs`: `Router::steer(&msg)` hands a message to the running turn
  when the mode is `steer`, the channel has busy notices (not the HTTP
  API), it isn't audio while transcription is set up, nothing is queued
  in front of it, and the sender matches; otherwise the caller queues it
  as before. What `close()` returns unread (it arrived during the last
  model call) is joined into one person job at the front of the lane's
  `held` (now a `VecDeque`), so it's the next turn and never lost; a
  person job only merges from the queue while nothing is held, so order
  holds. Ids the turn read get `answered` with its reply. `/new` and the
  bot stopping count unread steered messages as dropped and say so.
  `spawn_lane` adds the steering with `add_inbox`, after the factory's
  supervisor inbox, so sub-agent reports still arrive.
- `gateway.rs`: `handle` tries `steer` before `offer`; the busy notice
  says "I passed your message to the running turn, which reads it at its
  next step — /stop to end that turn." instead of "queued".
- `[gateway] while_busy` (`config.rs`, wired in `run_gateway`), the
  sample config line, `docs/channels.md` "Messages sent while it works",
  the roadmap's M56 line.

**Tests changed, none added.** `every_message_is_seen_at_once_and_a_queued_one_is_told_once`
now expects the steered busy notice; the rest of it held unchanged,
since the held "first" call was already the turn's last, so "second"
and "third" came back as one next turn (the late path).
`a_queued_message_hears_about_a_rate_limit_wait` passes `steered:
false`.

**Checks.** `make check` exited 0: 1869 passed, 0 failed, 28 ignored,
71 binaries (macOS).

**Open.** Not run against a live chat. A steered voice note waits for
the next turn (transcription runs in the lane). A steered message
doesn't load keyword skills (M28) and gets no "can't see photos" notice.
The mode is gateway-wide, not per chat; no `/queue` override and no
debounce window. Group chats steer only from the sender of the running
turn, with no setting to widen it.

### 2026-10-21 — M56.4: proactive memory, an owner profile, decay by kind (Claude, Opus 5.5)

**Why.** The prompt said to remember facts "when asked", so a chat bot
almost never did. Every fact's score halved every 7 days, so an
8-week-old name scored 1/256 of a fresh match. Recall ran once per
agent, from the first message, and a Telegram lane keeps one agent for
many messages.

**What changed.**
- The base prompt (`build_agent_from` in `main.rs`, and its copy in
  `ferrule-eval`'s `engineered_prompt`): "save lasting facts about the
  person you work for (names, preferences, standing corrections) with
  the remember tool on your own, tagged profile or preference, without
  waiting to be asked." A one-time edit; the prefix is stable from here.
  `remember`'s `tags` description says what the two tags do.
- `ferrule-memory`: `DURABLE_TAGS` (`profile`, `preference`). Their
  facts get decay 1 in `rank_heads`; candidates carry the row's tags
  (`Candidate::durable`, both the BM25 and the vector query). Every other
  fact still halves every 7 days. `MemoryStore::profile(limit)`, live
  durable facts newest first; `assemble_for_goal_hybrid` puts up to 10 of
  them first, then the goal's matches, then the newest, in the same
  2,000-character budget. A kind column was not added: tags already
  exist, are indexed by FTS, and need no migration.
- `ferrule-core`: `SessionRecall::recall_more(message, shown)` (default
  `None`). On a later run from a person (`run_user`), the agent asks it
  with every block it already added; a new block is a user message right
  after that run's goal, where the first block goes (after the stable
  prefix, append-only). `memory_due` is now the block itself;
  `memory_more` keeps later blocks, and compaction carries them like the
  first.
- `ferrule-cli` `GoalRecall::recall_more`: one keyword query
  (`goal_query` of the message, no embedding call), up to 3 facts and
  600 characters not already listed as `- #id` in what was shown, under
  `[Long-term memory, for this message]`.
- Docs: `memory.md` "What it keeps about you" and two limits,
  `m15-memory.md` §4, `m30-vector-recall.md`'s decay line, the roadmap.

**Checks.** `make check` exited 0: 1869 passed, 0 failed, 28 ignored,
71 binaries (macOS). No tests added or changed.

**Open.** The prompt line is not measured with `ferrule eval` (the gap
analysis asked for M58.5's gate first; that gate doesn't exist yet).
Whether the model tags facts `profile`/`preference` is its call; an
untagged fact about the owner still decays. The per-message recall is
keyword-only. Re-recall runs per person message, not on a measured topic
change; with nothing new it adds nothing.

### 2026-10-21 — M56.5: a turn a restart cut short is taken up again (Claude, Opus 5.5)

**Why.** "An interrupted turn is never re-run; the owner re-sends it."
A self-update or restart stopped running turns after the grace, and a
crash left only the owner's restart notice; the request itself was gone.

**What changed.**
- `ferrule-gateway/src/resume.rs` (new): `InFlight` (the message as it
  arrived, its start, `acted`: the non-read-only tools it called, each
  once, and `resumed`) and `Marker`, which writes
  `<sessions>/<sid>.turn.json` atomically when a person's turn starts
  (before voice notes are heard), adds a tool to `acted` on its first
  `ToolCallStarted` unless the agent's own registry says `read_only`, and
  removes the file when the turn ends. `runs_again()` is `acted` empty
  and not already a second try, so a turn that crashes the process can't
  loop. `CONTINUE` is `/continue` (`/resume` is the kill switch's).
- `router.rs`: lanes get a `Marker` (with `agent.tools().clone()`, new
  accessor in `ferrule-core`). Markers only for `person` jobs on channels
  with busy notices (not the HTTP API, not scheduled, plan, goal or wake
  turns). `Router::drain` now calls `stop_for_restart`: a person's turn
  is stopped with "…; when I'm back I'll pick it up again, or ask you
  first if it had already changed something" and `TurnGuard` remembers
  it was a restart; the lane keeps the marker only then (and only if the
  run came back incomplete). `/new` deletes both files.
  `Router::interrupted()` renames every `.turn.json` to
  `.interrupted.json` and returns them; `take_up()` tells each chat and
  runs the safe ones again at once (`Router::resume`, a `resumed` job
  whose goal is a "[Resuming after a restart] … It was:\n\n<original>"
  wrapper; it doesn't merge queued messages), and offers the rest with
  Resume / Let it go buttons. `drop_interrupted` lets one go.
  `LaneSnapshot::resumable` and `MarkedTurn::resumable` (serde default)
  carry it to `running.json`.
- `gateway.rs`: `/continue` (resume) and `/continue drop|no|cancel`,
  answered before the interceptors, like `/new`.
- `health.rs` `restart_notice`: a resumable turn "isn't lost: its chat
  picks it up again, or asks there first"; the old wording stays for the
  rest.
- `main.rs` `run_gateway`: `router.interrupted()` before
  `gateway.run()` (a new turn would overwrite the same file), then
  `take_up` in a task.
- Docs: `channels.md` "After a restart" and the `/continue` row,
  `m19b-reliability.md` §4, `m44-managed-mode.md`'s drain step,
  `docker.md`, the roadmap.

**Tests changed, none added.** Struct literals of `MarkedTurn` and
`LaneSnapshot` in `health.rs` and `ferrule-cli`'s `update/tests.rs` got
`resumable: false`. `a_killed_gateway_says_what_it_interrupted_and_never_reruns_it`
now waits for the turn's marker to list `shell` before the kill, expects
the new notice wording and the chat's offer naming `shell`; it still
checks the model isn't called again.

**Checks.** `make check` exited 0: 1869 passed, 0 failed, 28 ignored,
71 binaries (macOS).

**Open.** Not run against a live restart or update. The resumed turn
re-reads the conversation, which ends with the stop message; whether a
model reliably skips work already done is untested. A marker whose
channel isn't on at the next start waits as `.interrupted.json` until
that channel's `/continue` or `/new`. Pending offers are not repeated
at a later start. Steered messages a restart cut off are in the
transcript but not in the marker.

### 2026-10-21 — M56.6: ask_user, react and edit_message as agent tools (Claude, Opus 5.5)

**Why.** The agent could only write text and send files. A question
whose answer the owner could tap, a quick reaction and a fix to its own
last reply all meant more messages.

**What changed.**
- `ferrule-gateway/src/tools/chat.rs` (new), registered next to
  `send_file` for every chat session (`chat_tools` in `gateway_factory`;
  any `channels::of_session` chat, remote workspaces included):
  - `ask_user {question, choices[2..4], timeout_secs?}`: sends the
    question with one button per choice (`/answer <code> <n>`, under
    Telegram's 64-byte `callback_data`) through `send_with_buttons`, so a
    channel without buttons gets them as lines. It waits (default 300 s,
    clamped 10–540 s, under the watchdog's 600) and returns "They picked:
    X", "They answered in their own words …" or a truthful "No answer
    within N s; the question was withdrawn …". A drop guard withdraws the
    question however the call ends (`/stop` drops the future).
  - `Questions` (held by `FileOut`, registered as the gateway's first
    interceptor in `run_gateway`): a tap, a typed number or label, or any
    other non-command text from that chat while a question waits is the
    answer, taken before steering (M56.1) could hand it to the blocked
    turn. A late or unknown `/answer` is told so.
  - `react {emoji}`: `Channel::react` on the message the turn answers
    (`Router::turn_message`, from `LaneState::message_id`). Without the
    `reactions` capability: "reactions aren't supported here".
  - `edit_message {text}`: `Channel::edit` on the bot's previous reply
    (`Router::last_reply`). Without `edits`: "not supported here"; with
    no known id it says so instead of guessing.
- How the reply's id is known: `Channel::send_with_id` (default: `send`,
  no id; Telegram's is `deliver_message(…, patient)`, Discord's and
  Slack's their `deliver`, each exactly what their `send` does). The lane
  delivers a plain reply with it, and `StreamingReply::finish` now
  returns the id of the reply's last message (shown and edited, posted,
  or sent with `send_with_id`); the lane keeps it in `last_reply`.
- The Claude-plan bridge needed no change: M55.4's `bridged()` passes
  any tool not in `CLAUDE_NATIVE`/`NOT_BRIDGED`.
- Docs: `channels.md` "Asking with buttons, reacting, editing",
  `m35-subscriptions.md` §7.4's table, the roadmap.

**Checks.** `make check` exited 0: 1869 passed, 0 failed, 28 ignored,
71 binaries (macOS). No tests added or changed.

**Open.** Not tried in a live chat. In a group, any member's message
answers a waiting question. A typed answer gets no receipt (the turn's
reply follows). `edit_message` edits only the last part of a reply that
went out in several, and knows ids only on Telegram, Discord and Slack.
`/bg <task>` from the gap analysis was not built.

### 2026-10-21 — main (PR #48, M51–M53) merged into `autopilot` (Claude, Opus 5.5)

**Why.** Kimi's PR #48 landed on `main` as `2ffa669` and PR #47 had
conflicts. Merged, not rebased; every Session Log entry is kept.

**Resolved.**
- `router.rs`: both sides had rewritten PR #46's merge loop as
  `if … { loop { … } }`; ours (with M56.1's `held` queue and M56.5's
  `resumed`) is kept, so main's clippy fix holds.
- `tools/mod.rs`: both modules, `media` (M51) and `chat` (M56.6).
- `Cargo.lock`: `ferrule-gateway` depends on `ferrule-media` and
  `ferrule-proxy`.
- PLAN.md: Kimi's 2026-10-20 and 2026-10-21 entries go before this
  branch's 2026-10-21 ones; Current State says `main` is `2ffa669`, M51–M53
  are merged, and lists the 25th crate, `ferrule-media`.
- `app.js`: the Safe's Rotate sends its value through M52.1's
  `asSecret`, so it is sealed like every other key field (the server
  needed nothing: `open_all` runs before every handler). `m22-dashboard.md`
  and `m52-dashboard-safe.md` say so; the roadmap's open-tracks line is
  now "merged".
- Semantic fixes the merge needed: `tests/it/media.rs`'s
  `CompletionResponse` gets M55.2's `stop_reason: None`, and rustfmt;
  `ferrule-cli/src/media.rs` drops a `map_err` clippy calls useless
  (`config::data_dir` already returns `anyhow::Result`).

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS).

### 2026-10-21 — M56.7: channel media, finished (Claude, Opus 5.5)

**Why.** On Telegram an MP4 or MP3 the agent sent went out as a
document with no player (M51's generated videos included), an album was
read as its first photo, and a round video message was "unread". Discord
and Slack downloaded no files.

**What changed.**
- Telegram out (`send_file` in `telegram.rs`): `sendVideo` for any
  `video/*` (with `supports_streaming`), `sendVoice` for `audio/ogg` and
  `audio/opus`, `sendAudio` for MP3 and M4A, `sendPhoto` as before, else
  `sendDocument`; a 400 from the specific method sends it again as a
  document, as photos already did. All within the Bot API's 50 MB.
  `files::mime_for` learned `webm`, `m4v` and `oga`.
- Telegram in: `poll_once` is now `fetch_updates` (one `getUpdates`)
  plus `take_update` (admit, mark a tap, take the file, the unread
  reply), and joins consecutive items sharing a `media_group_id` into one
  message (each item's text on its own line, every attachment, the first
  item's id). A round that ends inside an album polls again with
  `timeout=1`, up to `ALBUM_ROUNDS` (3), so an album split across rounds
  still arrives whole. The first-photo-only logic, its `taken` set and
  the "I only looked at the first photo" reply are gone. `video_note` is
  taken like a video (`video_note.mp4`).
- Discord in: `DiscordChannel::with_inbox` (wired in `build_channels`,
  like Telegram's). Once a message is admitted, each attachment is
  downloaded from its signed CDN URL (only `cdn.discordapp.com`,
  `media.discordapp.net` or the API's own host; no token sent), capped by
  the inbox, saved under `inbox/discord/`, and the message carries the
  files and M39's lines; a file-only message is read. Only a sticker is
  still "unread".
- Slack: not built. Its files need `url_private_download` with the bot
  token and the `files:read` scope, which `BOT_SCOPES` and the manifest
  in `docs/slack.md` don't have; adding it would make `doctor` flag every
  existing install. Documented in `slack.md` and `channels.md`.
- Docs: `channels.md` "Photos" and "Voice messages", `discord.md`,
  `slack.md`, `m39-channels.md` §2, `m41-daily-use.md`, `m47`'s
  out-of-scope line, the roadmap.

**Tests changed, none added.** `vision::an_album_is_read_once` now
expects one turn with all three photos and no "first photo" reply;
`live_fixes::messages_that_arent_text_get_plain_words` expects no such
reply either.

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS).

**Open.** Not tried against the real Bot API or Discord. Whether
Telegram accepts a given WebM or MOV through `sendVideo` is left to its
400 and the document fallback. A video message's sound isn't
transcribed. Slack file reading needs the owner to add `files:read`.

### 2026-10-21 — M56.8: keyless web search by default (Claude, Opus 5.5)

**Why.** `web_search` was off until a key or a SearXNG instance was set,
so "look it up" failed on a fresh install.

**What changed.**
- `ferrule-tools/src/search.rs`: `SearchProvider::Duckduckgo`
  (`https://html.duckduckgo.com` + `/html/`, no key; `kp` for safe
  search, `region` as `kl`, `language` ignored). `parse_ddg` reads the
  HTML page without a parser crate: each `result__a` link (its `uddg`
  redirect undone; `/y.js` ads and non-http targets skipped) and the
  `result__snippet` that follows it. A page with no results and no
  result markup (DuckDuckGo's challenge when it limits automated
  searches) is an error saying so, not an empty answer; no retry.
  Results go through the same `render`: fenced, escaped,
  `untrusted="true"`.
- Fallback: `WebSearchTool::new` keeps `SearchSettings::keyless()` for
  any other provider. When that provider's search fails (anything but the
  gate), the query goes to DuckDuckGo through `counted()`, which asks the
  gate first (so `max_searches_per_day` and the kill switch count the
  fallback as a search) and records its own `SearchCall`; the answer
  starts with "<provider> failed (<why>); these results are DuckDuckGo's,
  keyless." If both fail, the error names both. `without_fallback()`
  for tests.
- `ferrule-cli`: `[web_search]` with no `provider` is DuckDuckGo;
  `provider = "off"` (or `"none"`) takes the tool away; the error lists
  the new names. `web_search::tool` records the fallback at $0 whatever
  `price_per_search_usd` says. `egress::endpoints` adds the search
  endpoint only for a provider named in the config, so the implicit
  default obeys `[egress] default = "deny"` like `web_fetch`. Doctor
  reports DuckDuckGo without a probe (it would spend one of the searches
  it tolerates). Setup offers "DuckDuckGo, no key (the default)", and
  "Turn it off" now writes `provider = "off"` (removing the table means
  the default).
- Docs: `web-search.md` (the table, "Keyless search, and its limits":
  not an API, no terms granting automated use, rate limits and the
  challenge page, markup that can change, egress), README's tools row,
  the roadmap, the sample config.

**Tests changed, none added.** `config::web_search_is_off_by_default…`
became `…keyless_by_default…` (default is DuckDuckGo; `"off"` is
none). `ferrule-proxy`'s search tests build their tool with
`without_fallback()`, so their error cases don't leave for the real
DuckDuckGo, and the mock's provider `match` names `Duckduckgo` as
unreachable.

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS).

**Open.** Not run against the real DuckDuckGo: the parser follows its
current markup from memory of the page, and no live test was added (the
owner's rule). Region codes differ by provider, so a fallback drops
`region` and `language`. Whether owner mode (M54) should gate the
default is that session's call.

### 2026-10-21 — M57.1: deferred tool schemas and `tool_search` (Claude, Opus 5.5)

**Claim.** M57 (M57.1–M57.7, the gap analysis's context and loop
hygiene) is built in this worktree (`.claude/worktrees/autopilot`,
branch `autopilot`), one commit per item. M54's files are not touched.

**Why.** Every request carried every tool's whole schema; a few MCP
servers mean thousands of tokens per message for tools that turn never
calls.

**What changed.**
- `ferrule-core/src/tool.rs`: `Tool::deferrable()` (default false),
  `DeferTools` (`auto` | `always` | `off`), `DEFER_AUTO_OVER` (10) and
  `ToolRegistry::defer_tools`. While it defers, `definitions()` is the
  tools that aren't deferrable plus `tool_search`, sorted by name, then
  each loaded tool in load order. `tool_search`'s description lists every
  deferred tool (loaded or not) as `name: first line`, so a load only
  appends. `tool_search {query, max_results}` takes words (name hits 3
  points, description hits 1) or `select:a,b`, returns the schemas as JSON
  lines and loads them. `note_called` loads a deferred tool called by
  name; `deferred_definitions()` lists the deferred tools not loaded yet.
  `auto` turns on past 10 deferrable tools and stays on for the session.
- `agent.rs`: the run loads every deferred tool its history called (a
  resumed session) and each one the model calls; the call context
  carries `deferred_tools` (`CallContext`, `provider.rs`).
- Deferrable: MCP tools (`ferrule-mcp`), the self-extension tools,
  `connection_request`, and `generate_image`/`generate_video` (M51's
  `media.rs`: two one-method additions, nothing else there).
- The Claude engine (`ferrule-plans`) bridges every tool whole, deferred
  ones from the call context, and leaves `tool_search` out
  (`with_deferred`): Claude Code has its own tool search.
- `[agent] defer_tools` (default `auto`) in `config.rs`, set on the
  registry in `build_agent_from`; the sample config.
- Docs: new `docs/m57-context-hygiene.md`, `speed.md` (what starts a new
  cache), README (the MCP row, the docs list), the roadmap.

**Tests changed, none added.** `ferrule-plans`'s
`claudes_permission_prompt_asks_the_runs_guard` fills the new
`CallContext` field with `..Default::default()`.

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS).

**Open.** Nothing measured live: the ledger's input and cache rows on a
session with several MCP servers should show the saving. The threshold
of 10 is a guess. Search is word matching, not embeddings. The
dashboard's config form (M52's) doesn't offer `defer_tools`.

### 2026-10-21 — M57.2: per-file loop notes, budget notes, raised effort (Claude, Opus 5.5)

**Why.** `stuck.rs` caught only the same call repeated; a file fixed over
and over with a different edit each time ran on. The model never heard
how many steps or tokens it had left. Effort was one level for every
turn, where LangChain's sandwich (more for planning and verifying)
scored best.

**What changed.**
- `stuck.rs`: `EditCounts` and `EDITS_PER_FILE` (6). Each successful
  `edit_file`/`write_file` counts against its path; the sixth change to
  one file with no passing check in between returns one note, pushed
  after that turn's results (and shown as `AgentEvent::Stuck`). A
  passing check clears the counts. Gated by `detect_stuck`.
- `agent.rs`: `budget_note` pushes a user message after the turn's
  results when the steps left reach a quarter and again a tenth, and the
  same for `Budget::left()` (new, default `None`; `TreeBudget` answers
  with the tree's `max_tokens` window through
  `Supervisor::budget_left`). Never in the system prompt.
- Effort: `HarnessProfile::max_effort` ("high" in all four profiles);
  `CallContext` carries `effort` (the agent's own level) and
  `effort_floor` (set on the run's first turn and the turn after a failed
  check, when `AgentConfig::effort_boost`). `CallContext::effort_for`
  applies them only to a configured effort, never touches `"none"`, and
  raises only between known words (`effort_rank`). The Anthropic and
  Responses (so Codex) drivers use it.
- `spawn_agent {effort: low|medium|high}`: `SpawnRequest`, `AgentRow`
  (an `effort` column, added to an older `agents.db` like `model`) and
  `ChildSpec` carry it; `build_agent_from` holds it to the profile's
  `max_effort` and calls `Agent::set_effort`. An explicit level wins over
  the raised turns.
- `[agent] effort_boost` (default true), the sample config.
- Docs: `m57-context-hygiene.md` (new section), `models.md`,
  `m23-drivers.md`, `agents.md` (the effort line; the tool-size table
  notes the ~230 characters the argument adds), README, the roadmap.

**Tests changed, none added.** The `SpawnRequest` literals in
`ferrule-agents`' tests (`board.rs`, `supervisor.rs`, `common/mod.rs`,
`worktree.rs`) and `store.rs`'s test row gained `effort: None`.

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS).

**Open.** Not measured on the eval suite. Whether a per-turn change of
Anthropic's `output_config.effort` costs cache hits isn't documented by
Anthropic; the ledger's cache rows on a run with `effort` set would show
it. The owner's M19 spend caps aren't reported to the model.

### 2026-10-21 — M57.3: a stale-file guard on `edit_file` (Claude, Opus 5.5)

**Why.** `edit_file` applied SEARCH blocks to whatever the file held.
A command, a hook, a sub-agent in the same workspace or an MCP tool
could have changed it since the model read it, and an exact SEARCH that
still matched landed in a file the model no longer knew.

**What changed.**
- `ferrule-core/src/stamps.rs` (new): `FileStamps`, a shared map from a
  resolved path to the length, FNV-64 hash and modified time of the
  bytes the agent last read or wrote there. `record`, `changed`,
  `current` (the stamped files still matching the disk), `adopt` and
  `adopting` (run work, then re-stamp what it changed in the files that
  were current before it).
- `ferrule-tools`: `ReadFileTool`, `WriteFileTool` and `EditFileTool`
  take `.tracking(stamps)`. `read_file` and `write_file` stamp what they
  read or wrote; `edit_file` (`edit_path`) refuses, writing nothing,
  when the bytes it just read differ from the stamp ("changed on disk
  since you last read it … re-read it with read_file"), and stamps what
  it wrote. A never-read file has no stamp and stays editable.
- `agent.rs`: `Agent::with_file_stamps`; `fire` (every hook event) and
  `run_checks` run under `adopting`, so a formatting hook or a verify
  command that formats (`cargo fmt && cargo test`) doesn't cause a
  refusal. The built-in lint (M29) only checks and writes nothing.
- Wired in `build_agent_from` (every agent: run, chat, gateway lanes,
  sub-agents) and in the eval's engineered variants (`variant.rs`; the
  naive one has no guard). The SSH remote tools don't track; their
  `edit_file` keeps its own read-to-write guard.
- Docs: `m57-context-hygiene.md` (new section), `m29-edit-mechanics.md`
  (the errors list), the roadmap.

**Tests changed, none added.**

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS).

**Open.** A file a sub-agent changes while the parent's hooks or checks
run is adopted with them (the window is one hook or check run). Not
measured on the eval suite.

### 2026-10-21 — M57.4: cache economics for chats (Claude, Opus 5.5)

**Why.** Every breakpoint used Anthropic's default 5-minute TTL, so a
chat reply after a longer pause paid for the whole conversation again.
A compaction summary only saw message text, so which tools ran and on
which files was lost at a fold unless the summary model said so.

**What changed.**
- 1-hour TTL: `CallContext::cache_ttl` (`provider.rs`), set from
  `Agent::set_cache_ttl` on every call but a compaction summary. The
  Anthropic driver marks all three breakpoints
  `{"type": "ephemeral", "ttl": "1h"}` when asked (`cache_control`), and
  reads `usage.cache_creation.ephemeral_1h_input_tokens` into the new
  `Usage::cache_write_1h_input_tokens`. The gateway factory sets it on
  chat lanes only (`channels::of_session`), from `[gateway] cache_ttl`
  (`"1h"` default, `"5m"` sends no field, as before); sample config.
- Pricing: `LedgerRecord::cache_write_1h_input_tokens` (serde default,
  skipped at 0); `ProviderPricing::hour_writes` adds the step from the
  5-minute write price to 2 × input, in `cost_usd` and in routing's
  tier-spend count. The learning pass's rows carry it too.
- Idle pre-compaction: not built. The compactor sends one flattened user
  message (template and history as text, no tools or system prompt),
  which reads nothing from the cache; compacting an idle chat would pay
  full input for the head and a new write, where a reply within the
  hour reads the cache at 0.1 ×. Written up in the doc.
- Fold summary: `Touched` (`agent.rs`) adds a block ferrule counts
  itself after the summary text: `Tools called: \`name\` ×n`, `Files
  changed:` (`write_file`/`edit_file` that worked) and `Files read:`
  (`read_file` that worked), 50 most recent each, the rest counted. A
  later fold parses the earlier block and adds to it.
- Docs: `m57-context-hygiene.md` (new section), `speed.md`,
  `m23-drivers.md` (the write price, the follow-up done), the roadmap
  (M57 line; the M27 leftover struck).

**Tests changed, none added.** Struct literals of `Usage` and
`LedgerRecord` across the workspace gained the new field (0). The
golden `crates/ferrule-core/tests/golden/routing_off.json` was
regenerated (`FERRULE_UPDATE_GOLDEN=1`): its compaction summary now
carries the counted block, and the estimates after it moved by 23
tokens.

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS).

**Open.** Not measured: whether an hour's writes pay for themselves at
the owner's pace shows in the ledger's cache rows on a real chat.
`ferrule chat` keeps 5 minutes. The Claude Code plan engine's 1-hour
writes aren't split out (its rows are plan-billed). The compactor's
input still leaves out the calls themselves; only the counted block
names them.

### 2026-10-21 — Commands off means no shell, not an unsandboxed one (Claude, Opus 5.5)

Found while wiring M57.6. `build_agent_from` starts from
`standard_registry()`, whose `shell` is `ShellTool::default()` with
`Sandbox::off()`. When `managed::user_commands_off` says commands are
off (M44: the sandbox refuses them, or the managed policy has no shell),
it skipped registering the sandboxed shell but never removed that
default one, so the agent kept a shell with no sandbox at all. The
`Some(why)` arm now removes `shell`. `crates/ferrule-cli/src/main.rs`
only; no test added (owner's rule).

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS).

**Open.** Worth cherry-picking to `main` on its own: `main` has the
same code since M44 (`2437941f`).

### 2026-10-21 — M57.5: tool-use examples in tool definitions (Claude, Opus 5.5)

**Why.** Anthropic measured accuracy on complex parameters rising from
72% to 90% with examples in the tool definitions. Ferrule's schemas said
what each field is, never how a good call looks.

**What changed.**
- `ferrule-tools/src/edit.rs`: the `edits` property's description gives
  two examples (a change with a line of context; an insert repeating the
  line next to it). The SSH `edit_file` reuses the definition.
- `ferrule-agents/src/prompts.rs`: an `Example: {…}` line on
  `spawn_agent`, `wait_agent`, `board_post` and `task_add` (+504
  characters for an agent that can spawn, +204 at `max_depth`).
- Fixed text in descriptions, not Anthropic's `input_examples`, so every
  driver sends them; nothing varies per request.
- Not done: `ferrule_admin` (its schema is in `self_service/`, M54's).
- Docs: `m57-context-hygiene.md` (new section), `agents.md` (the size
  table), `m29-edit-mechanics.md`, the roadmap.

**Tests changed, none added.**

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS).

**Open.** Not run through `ferrule eval` (the gap analysis's M58.5
gate). `ferrule_admin`'s examples wait for M54.

### 2026-10-21 — M57.6: background processes (Claude, Opus 5.5)

**Why.** The shell waited for every command up to its 120-second
timeout, so a dev server or a watcher hung the turn and was then killed.

**What changed.**
- `ferrule-tools/src/jobs.rs` (new): `Jobs`, one table per `ShellTool`
  (`ShellTool::jobs`), and `ShellJobTool` (`shell_job {id, action:
  status|tail|kill, lines}`). A job's watcher task records its exit code
  or the kill; `kill` takes the process group (Unix) and the process
  (`start_kill`; on Windows the sandbox's job object takes the tree). At
  most 4 running (`MAX_RUNNING`), 16 finished ones remembered.
  `Jobs::stop_all` runs from `Tool::session_ended` and from `Drop`.
- `shell.rs`: `background: true` (schema and call) starts the command in
  the same sandbox, workspace and process-group setup as a foreground
  one, stdout and stderr into a new file from
  `ferrule_core::tool::new_spill_file` (split out of `save_spill`, same
  checks), and returns the job id and file at once.
- `ferrule-core`: `Tool::session_ended` (default no-op) and
  `ToolRegistry::session_ended`, called by `Agent::end_session` after
  SessionEnd. A dropped agent (a gateway lane, a sub-agent) stops its
  jobs through `Jobs`' `Drop`.
- Registered with the shell in `standard_registry`, `build_agent_from`
  and the eval's variants; removed wherever the shell is, and in an SSH
  workspace, whose shell refuses `background: true` ("nothing was
  started"). `shell_job` is deferrable (M57.1).
- Docs: `m57-context-hygiene.md` (new section, and the deferrable
  list), README (the Tools row), the roadmap.

**Tests changed, none added.**

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS). No new test covers a job; nothing was run by hand
(the owner's rule): starting, tailing and killing a job are exercised by
no test yet.

**Open.** A job's output file isn't capped. Lines aren't streamed back;
the model asks with `tail`. Processes a job's command leaves behind after
it exits aren't tracked on Unix. A finished sub-agent's jobs run until
it's closed.

### 2026-10-21 — M57.7: fork sub-agents (Claude, Opus 5.5)

**Why.** A child saw only its brief, so a parent that had just read the
code or settled a plan restated it in the task, and whatever it left out
the child read again.

**What changed.**
- `ferrule-agents/src/fork.rs` (new): `tail(history, budget)`. Leaves
  out system messages; cuts off the parent's turn in progress (the last
  assistant message with a call that has no result yet, the
  `spawn_agent` call itself, and what follows); walks back from the end
  while `est_tokens` fits the budget, each message's text cut at
  `budget` characters (about a quarter of it) and its reasoning and
  native blocks dropped; then skips tool results at the front, so the
  copy never separates a call from its results.
- `supervisor.rs`: `SpawnRequest::fork`, `Limits::fork_tokens` (16,000;
  0 refuses forks). `Supervisor::fork_from` reads the parent's
  transcript (`<sessions>/<parent session>.jsonl`, folds applied) before
  anything is inserted, so a refusal leaves nothing behind, and puts
  `prompts::fork_header` first. `start` takes the fork and logs a
  `parent` meta line, the header and the copy into the child's
  transcript before building it; the replay that follows is the usual
  one, so `resume_agent` (which passes no fork) replays the same copy.
  The task stays the run's message, so it is the newest user message.
  The spawn result notes how many messages and about how many tokens the
  child got, or that none fit. Spawning is still immediate.
- `tools.rs`, `prompts.rs`: the `fork` boolean in `spawn_agent`'s schema,
  a sentence in its description (a fork reads its copy on every call),
  about 320 characters together; `fork_header`.
- `ferrule-cli/src/config.rs`: `[agents] fork_tokens`, the sample config.
  `graph.rs` sets `fork: false`.
- Docs: `m57-context-hygiene.md` (new section), `agents.md` (config, the
  size table, "Forked children"), `m12-multi-agent.md` (the "sees
  nothing of your conversation" line), README (the Sub-agents row), the
  roadmap.

**Tests changed, none added.** The `SpawnRequest` literals in
`ferrule-agents`' tests (`board.rs`, `supervisor.rs` ×3,
`common/mod.rs`, `worktree.rs`) gained `fork: false`.

**Checks.** `make check` exited 0 with M57.6 staged and M57.7 in the
working tree: 1881 passed, 0 failed, 28 ignored, 73 binaries (macOS).

**Open.** No test covers a fork and none was run by hand (the owner's
rule); not measured through `ferrule eval`. The budget is a fixed token
count, not a share of the child's context window. Tool-call arguments
aren't cut, so one huge `write_file` call ends the copy early. A worker's
worktree is at the parent's HEAD, so the copy may show uncommitted files
the worktree lacks; the header asks the child to re-read before editing.

### 2026-10-21 — M58.2: per-turn routing stays strictly opt-in (Claude, Opus 5.5)

**Why.** The gap analysis's cut list keeps M25's cheap→strong routing
opt-in until it's measured. Its central claim has only mock numbers
(`docs/research-round2-improvements.md`, "Run `--variant routing` on a
real model pair"), and an escalation is a mid-session model switch that
costs the prompt cache.

**What was checked.** Nothing turns it on implicitly:
- `RoutingConfig::default()` has `enabled: false`.
- `Routing::on()` is `enabled && usable()`, and `floor()` returns `None`
  unless `on()`, so a tier ref is a plain model while it's off.
- The only writers of `enabled = true` are `Models::set_routing` (`ferrule
  model route set`, the dashboard's `/api/routing/set`) and setup's
  `write_routing`. Setup reaches that only from its "Routing: start cheap,
  escalate when needed" entry, never in the guided first run.
- `/model strong` refuses while it's off.
- Doctor stays silent while it's off.
- Self-service and managed mode don't touch it.
- The existing test `off_nothing_routes_and_a_tier_ref_is_just_its_model`
  covers the off path.

No code change.

**What changed (docs only).**
- `docs/routing.md`: a paragraph on how it gets turned on and why it
  isn't a default.
- `docs/m25-routing.md`: a status note.
- The roadmap: the M25 section and the M58 line.
- README: the Providers row says "Opt-in routing, off unless you turn it
  on".

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS).

**Open.** The real-pair measurement (M58.5's A/B) and Amp-style "ask the
strong model" as the simpler alternative, which the gap analysis suggests
and nobody has built.

### 2026-10-21 — M58.4: the graph runner is already out of the core (Claude, Opus 5.5)

**Why.** The cut list wants `ferrule graph` (M42 part 6) opt-in and out
of `ferrule-core`. Anthropic found planners worth their cost only beyond
what the model does reliably alone.

**What was checked.** No move was needed:
- The runner is `crates/ferrule-cli/src/graph.rs` (908 lines), plus
  `graph_door.rs` (120) for `/graph`.
- `ferrule-core` and `ferrule-agents` have no graph-only code. The runner
  uses the supervisor's public `spawn`, `get`, `changed` and `close`
  (`changed` also backs `main.rs`'s wait loop) and `ferrule-tools`'
  `CommandVerifier`.
- `graph::run` has two callers: `Cmd::Graph` in `main.rs` and
  `GraphDoor::start`, which only an owner's `/graph` reaches. The
  dashboard console's `graph run` runs the same subcommand.
- `ferrule run` and `ferrule chat` never reach it.
- The gateway's `GraphDoor` interceptor returns `None` for any message
  whose first word isn't `/graph`.

Gating it behind a cargo feature would change what `ferrule graph` users
get, so it isn't gated.

**What changed (docs only).**
- `docs/m42-harness-engineering.md` part 6: "Where it lives", and its
  stale "Still open" list fixed (M43 already starts graphs from a chat
  with approvals there).
- The roadmap: the M42 section and the M58 line.

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS).

**Open.** The gap analysis's script-shaped replacement (a script calling
`ferrule run --goal --verify` per step) isn't built. The chat command
menu still lists `/graph`. That list is in `self_service/menu.rs`, M54's
surface, so it wasn't touched.

### 2026-10-21 — M58.1: the loop's overlapping seams, consolidated (Claude, Opus 5.5)

**Why.** The gap analysis found two failure taxonomies mapping into each
other, two repeat detectors, three hand-written "not run" endings in
`run_inner`, and a compaction path that skipped `call_provider`. Each is
one more place to get a stop wrong.

**What changed** (`crates/ferrule-core`, behavior-preserving unless said):
- **(a) One `abandon_rest`.** The four places that ended a turn with
  calls unanswered now share one helper. Those places are: stopped at the
  gate, halted at the gate, interrupted mid-segment, and halted in a
  PostToolUse hook. The helper finishes the announced calls as failed,
  gives every remaining call the interrupt's "not run" result
  (`Interrupt::note`), then halts or returns `Aborted`. The order of
  events and pushes is unchanged. `unrun` is folded into it.
- **(b) One failure taxonomy.** The status rules that were
  `CoreError::class()` now return a `failure::Kind`
  (`failure::status_kind`). `FailureClass` is its coarse view
  (`Kind::class`, the one mapping), and `CoreError::class()` is
  `status_kind(e).class()`. `from_class` (the reverse mapping) is gone,
  and `classify` falls back to `status_kind`. Every error classifies as
  before: a 400 naming a model is still `BadRequest` to routing and
  `ModelGone` to `classify`. `http_status` moved to `failure.rs`.
- **(c) Repeat detection.** Routing's two counters (invalid calls in a
  row, the same call in a row) moved into `stuck::Streaks`. It keys the
  same call by `Step`'s action hash, the same tool-and-arguments identity
  `Stuck::detect` uses, instead of cloning a `(String, String)`. This is
  not one algorithm. `Stuck` also needs the same result, and its nudge
  clears its steps. Routing counts the call whatever the result and
  restarts after each move. Merging them would change when a turn
  escalates.
- **(d) Compaction through `call_provider`.** It wasn't always. The
  `[agent] compaction_model` path already was (M55.2). A `Compactor`
  strategy (M42 part 7) made its own `provider.complete` call, with no
  ledger row, retry, fallback or budget charge. The trait is now
  `request(transcript_text) -> CompletionRequest` plus an optional
  `provider()`. The agent sends it through `call_provider`, and
  `provider_for("compaction")` picks the strategy's provider first. This
  part is a behavior change, and only for a strategy. The binary sets
  none; only `tests/it/history.rs` does, and it passes unchanged. The cost
  is that a strategy can no longer make several calls (fold by topic).
- Not done: the report's other three seams (a `Guard` absorbing
  `StopFlag` and `Budget`, one `ContextSource`, verify as Stop hooks).
  They weren't in this task.
- Docs: `m42-harness-engineering.md` part 7, `m36-self-update.md` §6.1,
  the roadmap.

**Numbers.** `agent.rs` was 5,974 lines before and 5,976 after: the
duplicated blocks are gone, and the helper and its doc are new.
`ferrule-core/src` overall: +251/−213 (`error.rs` −65, `failure.rs` +66,
`stuck.rs` +33, `compactor.rs` +2). The gain is one place each to reason
about, not fewer lines.

**Tests.** None changed, none added.

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS).

**Open.** The three seams above. The gap analysis's "several hundred
fewer lines" needs those. `run_inner` alone is still about 500 lines.

### 2026-10-21 — M58.3: eleven agent tools cut to five; `log_diary` into `write_todos` (Claude, Opus 5.5)

**Why.** Every call of an agent that can spawn carried eleven agent tool
definitions, about 1.5k tokens. Claude Code ships three (`Agent`,
`SendMessage`, `TaskStop`). The diary and the todo list were two planning
surfaces for one job.

**What changed.**
- `ferrule-core/src/tool.rs`: `Tool::replaced_by()` (default `None`).
  The registry still finds such a tool by name. `ToolRegistry::all()`
  leaves it out, so it's in no `definitions()`, deferral or
  `tool_search`, and it still shadows a source's tool of that name.
- `ferrule-agents/src/tools.rs`: the offered tools are now
  - `spawn_agent` (unchanged: `fork`, `effort` and the rest);
  - `message_agent {ids?, message?, timeout_seconds?}`: `ids` alone
    waits (was `wait_agent`); one id and a `message` sends a running
    child a direct message, or resumes any other child (was
    `resume_agent`); neither lists (was `list_agents`);
  - `stop_agent {id}` (was `close_agent`);
  - `board {body?, topic?, to?, since?}`: with `body` it posts or sends a
    direct message, without it reads (was `board_post` and `board_read`);
  - `tasks {action: add|list|claim|done, …}` (was the four `task_*`).

  An agent at `max_depth` gets `board` and `tasks`. The eleven old names
  are registered too, as before, with `replaced_by` set. They're never
  offered, their old behavior stays, and each call logs a `warn!`
  deprecation. Their definitions keep only the `required` fields, for
  M25's misfit count. One helper per operation now serves both names.
- Model-visible text names the new tools: the spawn result, the child
  notice, the limit errors, "you hold task …".
- `ferrule-tools/src/diary.rs`: `write_todos {todos?, note?}`. `todos`
  overwrites `TODO.md` as before. `note` appends a timestamped line to
  `diary.md`. A call with neither is an error; it used to write an empty
  list. `log_diary {entry}` stays registered with `replaced_by` set. The
  append is one shared function.
- The system prompt's planning sentence names only `write_todos`
  (`ferrule-cli/src/main.rs`, one line; `ferrule-eval/src/variant.rs` in
  step).
- `ferrule-otel`: `message_agent` and `stop_agent` count as agent tools.

**Size** (name + description + compact JSON schema, measured from the
source with a throwaway script in the scratchpad):

| | Before | After |
|---|---|---|
| An agent that can spawn | 11 tools, 5,314 characters (5,911 with the prompt addition) | 5 tools, 4,497 (5,094) |
| An agent at `max_depth` | 6 tools, 1,967 (2,564) | 2 tools, 1,337 (1,934) |
| `write_todos` + `log_diary` | 628 | 539 |

`spawn_agent` is 2,302 of the 4,497, unchanged on purpose: its cost
guidance and M57.5's example stay.

**Tests changed, none added.** `tests/it/supervisor.rs`: the
offered-tools check looks for `board`, and the limit error for
`message_agent`. The other agent tests still call the old names, so they
now cover the aliases.

**Docs.** `agents.md` (new "The tools", the size table, old names in the
text), README (the Tools and Sub-agents rows), `m12-multi-agent.md` (a
note), `m35-subscriptions.md` (the bridge table), `m57-context-hygiene.md`,
the roadmap.

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS).

**Open.**
- Not A/B'd through `ferrule eval`, which the gap analysis asks for
  before this lands (M58.5's policy now says so too).
- Drop the old names (Kinds `Wait`…`TaskDone` and `DiaryTool`) in the
  release after this one.
- A Claude-plan agent keeps no diary. `log_diary` was bridged, and
  `write_todos` stays off the bridge because claude has `TodoWrite`.
- A resumed claude session that remembers an old name sees it gone from
  the bridge's list and has to use the new one.

### 2026-10-21 — M58.5: eval as the gate, a harness fingerprint, a grader-leak tripwire, an external-benchmark converter (Claude, Opus 5.5)

**Why.**
- Nothing measured whether a prompt or default change helped. Anthropic
  now runs a per-model eval on every system-prompt change.
- A component could only be cut on faith.
- Nothing stopped an agent from reading the eval's own graders. One of
  Harness²'s rollouts downloaded its rubric. The starter suite's verify
  tasks hand the agent the check's path, and the shell could `cat` it.

**What changed.**
- **(a) A/B of harness variants.** `ferrule eval run --ablate <part>`
  (repeatable) runs every task as `engineered` and as `ablated` (the new
  `Variant::Ablated`), the same harness without the named parts:
  - `repo-map`, `verify`, `stuck`, `compaction`, `retries`, `skills`;
  - `memory`, `edit-file`, `effort-boost`, `stale-guard`, `baseline`,
    `playbook`.

  Each part is applied in `variant::build`. `--ablate` can't be combined
  with `--variant`. The report's difference column now covers any
  two-variant run (`engineered − ablated`; `engineered − naive` is
  unchanged). `SuiteRun.ablated` records the parts, and
  `history::previous` compares `ablated` only with a run that left out
  the same ones. Comparing two builds stays the existing "since the last
  run" diff, now with the fingerprint below.
- **(b) The harness fingerprint.**
  - `HarnessStamp` (ferrule-core) gains `prompt` and `tools`.
  - `variant::build_stamped` stamps the fixture's skills, the playbook,
    `config` (the AgentConfig and profile), the system prompt with its
    workspace path replaced, and the tool definitions. It writes the
    stamp as the transcript's `harness` meta line (the learn gate's
    transcripts get it too), and the runner keeps it per task run
    (`TaskResult.harness`).
  - `history` compares stamps task by task: "harness changed on N of M
    tasks: prompt, tools", or "same harness".
  - The CLI's own sessions are unchanged (`main.rs`, M54's).
- **(c) The grader-leak tripwire** (`ferrule-eval/src/leak.rs`):
  - Hidden: the agent's shell runs in a copy of the eval's sandbox whose
    `deny_read` adds the suite file, every script a task's grade command
    or check names under the suite dir, and the files beside them in
    their own directory. `graders/` and `checks/` are hidden; `solutions/`
    is not, because the mock plays it. The checks (a new
    `Build::check_sandbox`) and the graders run in the eval's sandbox as
    before. A check whose files are hidden is announced as ferrule's to
    run, not the agent's.
  - Searched and probed: before the run, and before grading, the
    workspace is searched for copies of or links to those files. Before
    the run each file must be on the deny list, and on Unix one is
    `cat`ed from inside the sandbox.
  - A hit is `Outcome::Error` with "GRADER LEAK: …", which is exit 1.
  - With no sandbox that hides reads, `run_suite` refuses unless
    `--allow-grader-reads`. The run is then `unguarded`, and its report
    says so first. The dashboard's model eval (`model_eval/`) passes it,
    with a comment saying why.
- **(d) External benchmark.** `evals/terminal-bench/convert.py`
  (standard library, downloads nothing) turns a local checkout of
  Terminal-Bench 2 (Harbor) or 1 tasks into a suite:
  - the instruction is the prompt;
  - the environment's files, without the Dockerfile, are the fixture;
  - pytest on `tests/test_outputs.py` is the grader, copied to
    `graders/<id>/` (hidden by the tripwire);
  - `/app` is rewritten to the workspace.

  Tasks whose image has RUN steps are skipped unless `--with-builds`.
  Multi-service ones are always skipped.
- **Docs.**
  - `eval.md`: "The gate" (the policy that every prompt line, scaffold or
    default change runs an A/B before it lands), `--ablate`, the
    tripwire, "An external benchmark", the harness line in "Since the
    last run", exit status 1 and a note on the mock flow.
  - `m53-contrastive-probing.md` §4, the README's Eval row, the roadmap.

**Tests changed, none added.**
- The struct literals that gained fields: `edit_tools.rs`, `gate.rs`,
  and the helpers in `history.rs` and `transcript.rs`.
- `suite_run.rs`'s `opts()`, and the CLI tests that run `eval run` on a
  sandbox-off config (`eval.rs`, `dashboard.rs`, `hooks.rs`, `learn.rs`,
  `trust.rs`), pass `--allow-grader-reads`.

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS).

**Open.**
- No eval was run: no A/B of M58.1–M58.3 yet, no `--ablate` run, no real
  tripwire probe under Seatbelt or Landlock (the tests run with the
  sandbox off), and the Terminal-Bench converter has never seen a real
  checkout. All of these are the next real-model session's first job.
- No nightly job.
- The fingerprint's `prompt` hash includes the check command's absolute
  suite path, so it compares runs on one machine, not across machines.
- On Windows the denies are ACLs the launcher writes on the grader files.

### 2026-10-21 — M53.2: reflector evidence discipline and honest clipping (Claude, Opus 5.5)

**Claim.** This session builds the gap analysis's remaining M53 items on
`autopilot` (worktree `.claude/worktrees/autopilot`), one commit each:
M53.2, M53.3, M53.4, M53.5 and M53.7. M53.1's contrast gate and M53.6's
harness stamp are Kimi's (PR #48) and are built on, not redone. `main`
(`2c74bdb`, the PR #47 merge, no content change) was merged in first.

**Why.**
- The reflector read tool results cut at 1,500 characters in a
  12,000-character window. Harness² refuses to clip below 10,000, "in
  the range that fabricates defects": a cut test log reads like a run
  that stopped short.
- Nothing asked the reflector for evidence, said "no change" was a fine
  answer, or called a one-off defect noise. Spurious lessons follow.

**What changed.**
- `episode.rs`: `TOOL_RESULT_CHARS` 10,000 and `TAIL_CHARS` 40,000
  (about 10k tokens). An oversized result keeps its head and tail with
  `[… N characters clipped here by the renderer, not by the run]`
  between them, and the other cut markers say "by the renderer".
- `reflect.rs`: the system prompt's evidence rules (no change is the
  default; cite the moment; once is noise; outages get nothing;
  truncation markers are the renderer's). `add`/`edit`/`retire` need an
  `evidence` field, or the answer is rejected. `Proposal.evidence`.
- `judge.rs`: the truncation-marker line.
- `files.rs`/`pass.rs`: `Change::Playbook.evidence` (serde default), in
  `log.jsonl` and the changelog line.
- Docs: `m53-contrastive-probing.md` ("M53.2"), `m16-learning-loop.md`
  §1, §3 and §4, the roadmap.

**Tests changed, none added.** The answers that gained `evidence`:
`reflect.rs`'s parse tests (and one more garbage case: an add without
evidence), `pass/tests.rs`'s `ADD` and retire answer, and the CLI's
`tests/it/learn.rs` reflector answer.

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS).

**Open.**
- No real pass has run with the new prompt or limits.
- Nothing verifies that the cited moment is in the transcript.

### 2026-10-21 — main (`6efe413`, M54) merged into `autopilot` (Claude, Opus 5.5)

**Why.** M54 (protected autonomous mode) landed on `main`, and PR #49
(`autopilot`) conflicted with it. Merged, not rebased, at a clean point
after M53.2 and before M53.3.

**Resolved.**
- `PLAN.md`: every Session Log entry kept. M54's entry is dated
  2026-10-20, so it sits after the last 2026-10-20 entry (Kimi's M53
  design), before the first 2026-10-21 one. Neither side had edited
  Current State; it now says M54 is on `main` and `autopilot` is PR #49.
- `docs/roadmap.md`: both kept, our M53 follow-ups block, then M54's
  section.
- Everything else merged without a conflict. Checked by hand:
  `crates/ferrule-tools/src/jobs.rs` and `shell.rs` are `main`'s byte for
  byte, so M54's fix to M57.6's `shell_job` (a job's log is no longer
  read in ferrule's own process, where a log swapped for a link to
  `private/secrets.env` leaked it) is what runs. M54's new prompt and op
  text names no agent tool M58.3 renamed (its `task_add` is a
  `ferrule_admin` op, not the old agent tool).

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS). M54's Linux-only code is still not compiled here.

### 2026-10-21 — M53.3: learned skills and scripts, validated (Claude, Opus 5.5)

**Why.** Harness²'s ablation ranks skills (5.3 points) and scripts (3.7)
far above prompt lines (1.5), the only thing the pass could write. A
procedure the agent fumbled twice belongs in a skill it loads on demand.

**What changed.**
- `ferrule-learn`:
  - `reflect.rs`: a `skill` op (`name`, `description`, `body`,
    optional `scripts`), evidence required; the prompt says when to
    prefer a skill and lists the learned ones. `Proposal.skill`.
  - `skills.rs` (new): `SkillDraft` and its shape checks; the
    validators (M13's `inspect` and scan over SKILL.md, text files and
    scripts; reachable through the catalog; every `scripts/…` path the
    body names ships; `sh -n`/`bash -n`/`python3 -m py_compile` in the
    gate's sandbox, a missing tool skipped with a note); `keep` and
    `undo`; `learn/skills.json` (the digest lock); `LearnDir`'s
    `skills_root`, `skills_index`, `learned_skills`.
  - `pass/skill.rs` (new): shape, draft in a temp dir, validators, the
    gate or the probe and judge, keep, journal. The probe-and-judge half
    moved into `pass::judged`, shared with lessons.
  - `gate.rs`: `GateRun.skill` puts the draft in the scratch copy as a
    project skill; `Gate::sandbox` (the `WorkspaceGate`'s).
  - `files.rs`: `Change::Skill`; changelog lines; `revert` undoes kept
    skills newest first. The pass's answers may be 4,096 tokens (was
    1,024): a skill's body and scripts need room.
  - `screen.rs`: `denied` and `ferrule_data` helpers.
- `ferrule-skills`: `Scope::Learned` ("learned"; never triggers);
  `SkillsHandle::filtered` and `Keep`, one agent's view of the live set.
- `ferrule-cli`: `<data>/learned-skills` is the last discovery root (the
  gateway's live set and `ferrule skills`); `learn::skill_view` keeps a
  learned skill only while its SKILL.md matches `skills.json` and scans
  clean; `ferrule learn show` lists learned skills, `learn diff` shows
  skill changes.
- Docs: `m53-contrastive-probing.md` ("M53.3"), `skills.md` (source 5),
  `m16-learning-loop.md` (the op table), the roadmap, the README's
  learning row and Documentation list (the M53 doc wasn't linked).

**Tests changed, none added.** `reflect.rs`'s `user_message` call takes
the learned-skills list.

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS).

**Open.** No real pass has proposed a skill. Hand-editing a learned skill
stops it loading (its digest changed). Nothing checks that the gate run
activated the skill it was offered.

### 2026-10-21 — M53.4: scoped lessons and skills; add one, cut one (Claude, Opus 5.5)

**Why.** Every lesson reached every agent, so one project's or one
chat's lesson could mislead another (scoping was worth 2.9 and 3.7
points in Harness²'s ablation). A full playbook refused every new
lesson instead of letting an old one go.

**What changed.**
- `ferrule-learn`:
  - `scope.rs` (new): `workspace:<path>`, `chat:<session>`, `task:<id>`.
  - `playbook.rs`: `[pb-N @scope]` lines (`Lesson.scope`), `lessons_for`,
    `apply_in` (an add keyed to a scope), `Delta::Promote` (the pass's
    only), `Applied.scope` so retire and promote invert, and
    `prompt_block_for` (`prompt_block` is the global lessons only).
  - `episode.rs`: `Episode.scope`, read from the session's
    `learn_scope` meta line (`meta`, `SCOPE_META`).
  - `pass.rs`: the reflector and the guards see the episode's view
    (global and its scope); a repeat of another scope's lesson becomes a
    `promote`; at 90% of `max_bullets` (`near_cap`) an add or promote
    also retires the reflector's `"retire"` pick or the lesson no pass
    kept for longest (`State.touched`), gated together and journaled as
    two changes.
  - `skills.rs`: `Entry.scope` and `applies`; `keep` promotes a skill
    kept in a second scope; `learned_skills(scope)`.
  - `reflect.rs`: `Proposal.retire`, the nearly-full note.
- `ferrule-cli`: `learn::scope_for` (task, chat or workspace);
  `build_agent_from` logs it as `learn_scope` and passes it to
  `prompt_section` and `skill_view`; task episodes carry the task's
  scope; `ferrule learn show` lists scoped lessons and skills; eval's
  opt-in owner playbook gets the global lessons.
- Docs: `m53-contrastive-probing.md` ("M53.4"), `m16-learning-loop.md`
  (§3's line format and cap rule), the roadmap.

**Tests changed, none added.** The struct literals that gained a field
(`Lesson` in `screen.rs` and `reflect.rs`, `Episode` in `reflect.rs` and
`pass/tests.rs`, `State` in `files.rs`). The CLI's learning story
(`tests/it/learn.rs`): the task's lesson is now written
`[pb-1 @task:<id>]`, reaches the task's next run but not a terminal
`run` (it did before), and `learn diff` shows the tagged line.

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS), clippy clean.

**Open.** No real pass has run with scopes. A chat's lesson never reaches
the gateway's other chats until a second chat keeps it. "Unused" can
only mean "not kept again": ferrule can't see whether a lesson was
followed.

### 2026-10-21 — M53.5: visible learning with Undo; the owner's thumbs as a check (Claude, Opus 5.5)

**Why.** Learning happened in a file the owner had to go and read, so a
bad lesson stayed until someone looked, and a chat bot had no check to
learn from at all. OpenClaw shows a "Learned" row with one-press Undo;
Hermes's top complaint is a bot that always thinks it did well. The gate
answers that only if the owner sees it work and can veto it.

**What changed.**
- `ferrule-learn`:
  - `pass.rs`: `Env.on_kept` (`OnKept`, `Learned`) hears each lesson or
    skill a pass kept, numbered, after the pass releases its lock;
    `learned` writes the lines. `revert_change` undoes one change (an add
    and the retire that made room for it together, or neither) and
    journals it; `revert` and `resolve` skip changes undone that way.
    The judge is told when the original run got the owner's 👎.
  - `thumbs.rs` (new): `learn/reactions.jsonl`, a 👎's transcript cut in
    `learn/reactions/` (checked against `Mark`, the reply's end), and
    `episodes`: each 👎 after the cursor is an episode (outcome
    `thumbs-down`, goal the reacted request, fixed by a later 👍 in the
    chat). The pass reads them itself.
  - `files.rs`: `PassRecord.undone` (`ChangeRevert`); the changelog's
    "Undone one by one".
  - `judge.rs`, `reflect.rs`: the 👎 in both prompts; the judge's
    "fewer retries" reads "a visibly better final reply" for such a run.
- `ferrule-trust`: `Hub::tell_owner_choices`, `tell_owner` with buttons.
- `ferrule-gateway`: `Reaction` and `ReactionSink`; Telegram asks for
  `message_reaction` only with a sink and hands over those in allowed
  chats; `Router::with_replied` hears each reply's id once it's sent.
- `ferrule-cli`: `learn::thumbs` (the sink: the owner's 👍/👎 only, on a
  reply sent since the gateway started), built only with `[learning]
  enabled`; the scheduled job sends "Learned: …" with an Undo button
  (`/unlearn <pass> <change>`), which `OwnerDoor` handles for the owner,
  on the dashboard's chat too; `ferrule learn revert … --change N`;
  `learn diff` numbers changes; `learn show` counts reactions.
- Docs: `m53-contrastive-probing.md` ("M53.5"), `m16-learning-loop.md`
  (§7, §8), the roadmap, the README's learning row.

**Default.** Learning stays off everywhere: M54's merged code sets no
learning default, and the gap analysis's "on under protected autonomy"
would start an unattended spend. The owner turns it on.

**Tests changed, none added.** The two `Env` literals in
`pass/tests.rs` gained `on_kept: None`; `PassRecord` in `files.rs`'s
tests gained `undone`.

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS), clippy clean.

**Open.** Not run against a live Telegram bot (the reaction update, the
button, the lines). Only Telegram's reactions are read; a reaction on a
reply from before the gateway's last start, or on an earlier piece of a
split reply, is dropped. The probe re-runs the reacted request without
the chat's earlier turns. `learn/reactions/` is never pruned.

### 2026-10-21 — M53.7: an opt-in blind selection node (Claude, Opus 5.5)

**Why.** Some outputs have no check (a briefing, a contract summary) and
cost a lot when wrong. Harness²'s selector picks among candidates shown
anonymously in a shuffled order. Ferrule's routing research keeps
best-of-n out of the defaults (k + 1 agents per answer, no source shows
it paying off), and the selector errs, so this is a node a graph opts
into, nothing more.

**What changed.**
- `ferrule-cli/src/graph.rs`: `kind = "select"` with `task`, `k`
  (default 3, 2 to 5, checked at parse), `seed`, `judge_model`, and
  `role` (worker or planner). It starts k agents on the same task, keeps
  those that delivered, shuffles them by the seed (splitmix64,
  Fisher-Yates) under labels A, B, C…, and spawns a verifier with the
  task, the deliverables (16,000 characters each at most, head and tail,
  the cut marked) and Harness²'s ordered checklist: delivered, matches
  the requested shape, facts match the source, internally consistent,
  usable, finished. `pick_of` reads `PICK: <label>`; no line, `none`,
  an unknown label or two different picks fail the node, with the seed
  and the verifier's last line. The pick passes with its agent, branch
  and deliverable; every other candidate and the verifier are closed (all
  of them when nothing is picked). `wait_all`
  is the wait `wait_agents` used, shared. Select nodes run beside the
  batch's agent nodes and take turns with each other.
- Docs: `m53-contrastive-probing.md` ("M53.7", with why it isn't a
  default), `m42-harness-engineering.md` (Part 6), the roadmap.

**Tests changed, none added.**

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS), clippy clean.

**Open.** No real run. The deliverable is each agent's report, not its
files. k must fit `[agents] max_children` (4 by default) beside the
graph's other running agents. Nothing measures how often the verifier
picks the best one.

### 2026-10-21 — M51.4: voice replies, OpenRouter transcription, an ffmpeg line (Claude, Opus 5.5)

**Why.** The bot hears voice notes but can only answer in text, its
transcription knew OpenAI, Groq and local servers but not the
OpenRouter key most owners already connected, and nothing said whether
the shell could cut or convert media (ferrule ships no ffmpeg).

**What changed.**
- `ferrule-media`: `SpeechReq`, `AudioOut` and `MediaBackend::speak`.
  OpenRouter's `POST /audio/speech` (through the proxy's placeholder and
  the egress client, like images) asks for `mp3`, the only compressed
  format its spec offers (`mp3` or `pcm`), and refuses an answer that
  isn't MP3 (`sniff` knows MP3 now). `answer` is the status handling
  `checked` used, returning bytes. `JobKind::Speech` and
  `JobStore::speech`, so `max_per_day` counts voice replies.
  `FakeMedia::speak`.
- `ferrule-gateway`: `SpeakTool` (`speak {text, voice?}`, at most 4,000
  characters, deferrable): cap check, the call, `media/<unix>-<n>.mp3`,
  a jobs row, a ledger row (`call_kind = "media"`), then the file to the
  session's chat as an attachment of kind `voice`. `MediaShared::
  set_speech` turns it on. Telegram's `send_file` sends an MP3 or M4A of
  kind `voice` with `sendVoice` (the Bot API takes OGG/Opus, MP3 or
  M4A there), so no ffmpeg is needed. `OpenAiTranscriber` reads
  OpenRouter's `usage.seconds` and `usage.cost`; a reported cost wins
  over `price_per_minute` in the ledger row.
- `ferrule-cli`: `[media] speech_model` (default
  `google/gemini-3.8-flash-tts`, from OpenRouter's speech list of
  2026-10-21; `""` turns `speak` off) and `voice` (unset: the model's
  own); the chat agent gets `speak` and `CHAT_VOICE` when it's on.
  `[transcription]`'s `model_for`: on an `openrouter.ai` base URL a
  model name without `/` becomes `openai/<name>`. `ferrule doctor`'s
  `media` line: ffmpeg's version and whether it has the `subtitles`
  filter (a libass build), as a note.
- `ferrule-agents`: `prompts::CHAT_VOICE`.
- Docs: `m51-media.md` ("M51.4"), `channels.md` ("Voice messages":
  OpenRouter, answering in voice, the doctor line), `models.md`
  ("Making media"), the roadmap.

**Tests changed, none added.**

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS), clippy clean.

**Open.** No live call to OpenRouter's speech or transcription endpoint
and no voice note sent to a real chat. The speech answer carries no
cost, so its ledger row has none and the trust caps don't count it (the
daily count does). Sub-agents get `speak` with M51.3.

### 2026-10-21 — M54 follow-up: the owner's Telegram groups get owner mode (Claude, Opus 5.5)

**Why.** The owner talks to the bot from their private chat and from
several Telegram groups, with no one else. M54 treated every group as
someone else's chat (`workspace-write`, approvals, no `ferrule_admin`, no
transcripts), so the groups never got owner mode.

**What changed.**
- `TelegramChannel::with_only_owner_in_groups` (`ferrule-gateway`
  `channels/telegram.rs` `admits`): in an allowed group, only a message
  whose sender id is the owner's private chat id gets in. That includes
  button taps; an anonymous admin or anyone else is ignored (debug log).
  With no owner chat, no group message gets in. `main.rs` turns it on
  whenever `owner_mode::wanted(cfg)`.
- `owner_mode::owner_chat` counts a Telegram group (`owner_group`, a
  negative chat id) as the owner's. `owner_tree` is only asked in owner
  mode, where the sender check holds.
- `gateway_factory` gives a group `ferrule_admin` when owner mode is on
  (`Admin::autonomous`). No cards are shown there; login and key links
  still go to the private chat (`send_link`).
- `docs/sandbox.md` "Other people's chats" says so.

**Dropped.** A guest-isolation design (separate folders, memory and
claude state per guest; no MCP for guests; per-tree hooks) was written
and approved, then shelved when the owner said no one else will use the
bot. It is in the session notes, not the repo. Its findings matter only
if someone else is allowed in. Guests would share the owner's
`memory.db` through the in-process memory tools, and on the Claude plan
engine a guest's claude can read running owner turns' bridge tokens in
the shared config dir. Also, the supervisor keeps one hook set per
process, which is the last root attached.

**Checked.** `make check` passed on macOS (1881 passed, 0 failed, 28
ignored). No test was added (not asked). No live run on the Linux
server yet.

### 2026-10-21 — main (`f70984e`, PR #50) merged into `autopilot` (Claude, Opus 5.5)

**Why.** PR #50 (the M54 follow-up above: the owner's Telegram groups
get owner mode) landed on `main`, and PR #49 conflicted with it.

**What was resolved.** Only `PLAN.md` conflicted: both sides appended
2026-10-21 entries. Every entry from both sides is kept; the M54
follow-up's entry goes after `autopilot`'s M51.4, and the copy of M54's
"Follow-ups" list git offered from `main`'s side is dropped because it
is already in M54's entry above. `main.rs`, `owner_mode.rs`,
`telegram.rs` and `docs/sandbox.md` merged cleanly: the group sender
check (`admits`) sits beside M51.4's `sendVoice` branch in
`telegram.rs`, and the factory's `ferrule_admin` offer beside the media
tools in `gateway_factory`. PR #50 shelved guest isolation (its entry,
"Dropped"), so there are no new guest rules for memory, learning,
reactions or the media tools to fit inside; they keep M54's.

**Checks.** `make check` exited 0 on the merge (the numbers are in the
merge commit's message).

### 2026-10-21 — M51.3: a `media` role for sub-agents (Claude, Opus 5.5)

**Why.** Jobs like "make four variants, pick one, animate it" spend the
chat model's context and bill on tool calls a cheap model could make.
Roles were worker, planner and verifier only, and sub-agents never got
the media tools.

**What changed.**
- `ferrule-agents`: `Role::Media` (`parse`, `as_str`), its child
  prompt, `spawn_agent`'s role enum and description, and `spawn` never
  places a media child in a worktree, so its `media/` is its parent's.
- `ferrule-cli`: `child_builder` takes the gateway's `MediaShared` and
  registers `generate_image`, `generate_video` and `speak` for a media
  child (hidden paths: the strictest list); without it (terminal `run`,
  a terminal graph) a media spawn fails with the reason. The role and
  graph error messages name `media`; the example config shows
  `[agents.roles.media]`.
- `ferrule-gateway`: `generate_video` in a session with no chat says the
  gateway saves the clip at its path, not that it will send it here.
- Docs: `agents.md` (Roles, `media`), `m51-media.md` ("M51.3"),
  `models.md`, the roadmap.

**Smallest change on another surface.** `graph.rs` (M42): the
`child_builder` call gained `None` and its role error names `media`.

**Tests changed, none added.**

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS), clippy clean.

**Open.** No media child has run against OpenRouter. A video a child
starts is saved, not delivered; nothing tells the parent when it lands.

### 2026-10-21 — M52.2: the Safe's inventory, last used, Delete for every kind, history (Claude, Opus 5.5)

**Why.** Kimi's Safe could rotate a kept key and move a tool key's
sites, but it listed only names in the secrets file and `[secrets]`,
said nothing of connections, plan sign-ins or ferrule's own keys, had
no dates or last use, and could remove only a page-made tool key.

**Where keys live (traced first).** Provider keys and channel tokens
are values in `private/secrets.env` under the config's `*_env` names
(`models_page::provider_save`, `channels::card::save`); connections and
plan sign-ins are sealed files under `private/`; a name may also come
from the environment. The page says which, per entry.

**What changed.**
- `ferrule-proxy`: `server::Secret.name`; `Broker::on_use` gets a
  `SecretUse {name, host}` for each bound secret a request carries where
  it's swapped (credential headers, `Basic` included, allowed URLs), on
  both the intercepted HTTPS and the plain-HTTP paths;
  `Broker::unbind`.
- `ferrule-cli`:
  - `secret_meta.rs` (new): `secrets.meta.json` beside the secrets file,
    names and times only: created, rotated, last used and host (written
    at most once a minute per name). `secrets::set`/`remove` stamp it.
    `shared_broker` registers the use hook.
  - `dashboard/safe.rs` (new): `GET secrets` gains `groups` (model
    providers with the ledger's last call per provider, the ChatGPT
    sign-in, the Claude token; channels; connections; `[secrets]`;
    system: unclaimed kept names, the relay key, the sealing key, the
    proxy CA, the page's sign-ins); `POST secrets/delete {id}` for every
    kind (a value from the file, the process env and the proxy; a
    connection disconnected; a plan sign-in's file deleted; every page
    session ended), refusing env-set names and the sealing key and CA
    with the reason; `GET secrets/audit`, the by-name history.
  - `tool_keys.rs`: rotate rebinds a `[secrets]` key in the proxy;
    `used_by`, `named_in`, `locked` shared with `safe.rs`.
  - `app.js`: the Safe panel draws the groups with saved, changed and
    last-used lines, Delete or its reason, and a History fold;
    `lang-he.js` has the five new words.
- Docs: `m52-dashboard-safe.md` ("M52.2", with M52.3 noted as M54's
  `secret_request`), `dashboard.md` (Safe), the roadmap.

**Smallest change on another surface.** `secrets.rs` (M54's): two calls
to `secret_meta` after a successful write or removal.

**Tests changed, none added.**

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS), clippy clean; `node --check` on `app.js`.

**Open.** Not opened from a real browser; the use hook not watched on a
real proxied request. Keys saved before M52.2 have no dates until their
next write. Channel and connection tokens have no last use (ferrule's own
clients send them). Deleting the ChatGPT sign-in from the page doesn't
revoke it at OpenAI (`ferrule logout chatgpt` does). The old value isn't
kept encrypted for N days after a rotation (the report's grace period):
rotation still replaces it outright.

### 2026-10-21 — M52.4: safe config editing (Claude, Opus 5.5)

**Why.** `config/check` gave a verdict, not a diff; a page loaded before
a change from Telegram or the machine silently undid it on save; and
only one `.prev` was kept.

**What changed.**
- `dashboard/config_page.rs`: `diff` (leaf by leaf, redacted, secrets
  "(hidden)") in `config/check`'s answer; `hash` in `GET config` and
  `fresh`, which refuses a write whose `base_hash` isn't the file's
  (409, `stale`), on check, save, set, undo, the version restore and the
  autonomy switch; `<data>/config-history/` with `keep_version` (the
  file hidden by `hide`, a note beside it, deduplicated, 20 kept) called
  from `commit` before and after each write, and `remember` for the
  writers outside it; `GET config/versions`, `GET config/version?id=`
  (the diff a restore would make) and `POST config/version/restore`
  (unhide from the file as it is, one confirm, `commit` with Undo's
  guard).
- `dashboard/api.rs`: the routes; the last-good `config/restore` checks
  `base_hash` and keeps its versions (M54's chat guard unchanged).
- `app.js`: Save reviews the diff and saves on "Save these N changes";
  every config write sends `base_hash`; a **Versions** fold with Preview
  and Restore.
- Docs: `m52-dashboard-safe.md` ("M52.4"), `dashboard.md` (Config), the
  roadmap.

**Tests changed, none added.**

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS), clippy clean; `node --check` on `app.js`.

**Open.** Not used from a real browser. Config writes from other pages
and the terminal aren't versions of their own (the next page write
keeps the file as they left it). The chat's `config_set` sends no
`base_hash`, so it isn't stale-checked.

### 2026-10-21 — M52.6: step-up for Safe writes, a `__Host-` cookie, hardened remote (Claude, Opus 5.5)

**Why.** A stolen dashboard cookie could rotate, delete or re-route any
key for the rest of its 12-hour session; the cookie had no `__Host-`
prefix (OWASP's advice); and the only remote path was a quick tunnel.

**What changed.**
- `dashboard/mod.rs`: `STEP_UP_PATHS` (the Safe's rotate, delete, hosts,
  remove) answer 403 `step_up` unless the session was opened in the last
  10 minutes or its owner allowed it in their chat in the last 10;
  `POST /api/stepup` asks (`Hub::ask_in`, an owner chat that isn't the
  dashboard's, the server's own words, `op = dashboard.step_up`, five
  minutes) and `GET /api/stepup` says where it stands; the state is per
  session, in memory. The cookie is read by `cookie_name`. Opt-in
  `[dashboard] tailscale_users`: off this machine, a request without one
  of those `Tailscale-User-Login`s is refused.
- `dashboard/auth.rs`: `HOST_COOKIE` and `cookie_name` (`__Host-` over
  HTTPS at `Path=/`), used by `set_cookie` and `clear_cookie`;
  `Sessions::opened_ms`; `now_ms` public.
- `dashboard/chat.rs`: a step-up card is left out of the page's
  approvals and its code refused by `approvals/answer`.
- `config.rs`: `DashboardConfig.tailscale_users` and its example line.
  `config_page.rs`: `tailscale_users` is a guarded field.
- `app.js`: `act` meets a `step_up` 403 with "Confirm it's you" → "Ask in
  my chat", polls, and sends the write again on Allow; `lang-he.js` has
  the four new strings.
- Docs: `m52-dashboard-safe.md` ("M52.6": the two hardened remote
  setups; Cloudflare Access's origin JWT check is a follow-up),
  `m22-dashboard.md` (Threat model), `dashboard.md`, the roadmap.

**Smallest change on another surface.** `config_page.rs`'s guarded keys
(M54's list) gained `tailscale_users`.

**Tests changed, none added.**

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS), clippy clean; `node --check` on `app.js`.

**Open.** No step-up answered from a real chat; no request through a
real Tailscale Serve. Tunnel sessions from before the cookie rename end
once. Cloudflare Access JWT validation at the origin isn't built.

### 2026-10-21 — The owner's memory stays with the owner's chats (Claude, Opus 5.5)

Claim: the security and approvals group on `autopilot` (PR #49): this
fix, then M54.3, M54.6, M54.5, M54.2, M56.2 and M56.3, one commit each.

**Why.** `memory.db` was shared by every chat. M56.4's profile block,
the per-message recall and `remember`/`recall`/`forget` therefore gave
another allowed person's chat, a guest in a group, or a non-owner HTTP
API client the owner's facts, and let it change them. Lessons and
learned skills had the same shape: a global lesson (learned in two of
the owner's chats, or written by hand) and a workspace lesson reached
every session, including a guest's `/plan` run.

**What changed.** M54's owner classification decides it, in every mode:
- `owner_mode.rs`: `owner_tree(cfg, hub, tree)` (a Telegram group is
  the owner's only while `wanted(cfg)`, when the channel admits only the
  owner there); `chat_of(tree)`; `guest_memory(tree)` →
  `<data>/guest-memory/<chat>.db`; `history()` (sessions, learn) and
  `guest_hidden()` = history plus `memory.db`, `-wal`, `-shm`,
  `guest-memory/`, `learned-skills/`.
- `ferrule-sandbox` `Sandbox::narrowed`: adds `hide` in every mode, and
  still maps owner mode to `workspace-write`. The claude engine's
  `guest()` follows.
- `main.rs` `build_agent_from`: `guest` in every mode; a guest is
  narrowed, its provider is `guest()`, its memory tools and
  `GoalRecall` (so the profile block and the per-message recall) use its
  own file; `session_hidden` for `send_file` and the media tools.
  `hidden_paths(false)` still hides only sessions and learn from
  everyone, so the owner's commands outside owner mode read `memory.db`
  as before.
- Learning: `learn::scope_for(.., guest)` keys a guest run that isn't a
  chat or task by the chat it traces to; `prompt_section` and
  `skill_view` take `only`; `ferrule_learn::prompt_block_only`,
  `Playbook::lessons_only`, `skills::Entry::kept_for`.
- Docs: `memory.md` "Whose memory", `sandbox.md` (Reads, Other people's
  chats), `m54-autonomous.md` "Amendments after the build" (1), the
  roadmap.

**Smallest change on another surface.** `ferrule-sandbox` `narrowed`
(M54's) and `ferrule-learn`'s playbook and skills index (M53.4's).

**Tests changed, none added.**

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS), clippy clean.

**Open.** Shared helpers (MCP servers, the browser, hooks) aren't
narrowed per session, so a guest-steered filesystem MCP server reads
what they read (`memory.db` in every mode). On the Claude plan engine
claude's own state dir is shared by every chat. A guest's learned
skill loads but its scripts can't run from its shell. The learning pass
still reads global lessons when it reflects on a guest's episode, and a
guest lesson repeated in an owner scope is promoted to global (guest to
owner, the other direction). Guests' memory files aren't in `ferrule
memory`, the dashboard or the memory merges. No live run.

### 2026-10-21 — M54.3: known key values out of every tool result (Claude, Opus 5.5)

**Why.** Nothing scrubbed tool results: the proxy's `scrub_text` ran only
in telemetry. One prompt-injected `cat`, a misconfigured MCP server, or
a run under the old `mode = "off"` stopgap put a key into a tool result,
then the transcript and the model's provider; and M54 made `sessions/`
readable to the owner's commands, relying on "the builder checks" that
none is there.

**What changed.**
- `ferrule-core` `scrub`: a process-wide hook (`install`, `text`); with
  none installed text passes unchanged. Applied in `agent.rs` `Ran::of`,
  the one place a call becomes the result the transcript, model, hooks,
  trace and stuck detector read, and at the start of
  `ToolOutput::capped`, `spilled` and `spill_preview` (the spill file
  gets the scrubbed text too). `ferrule-tools` `Job::tail` scrubs what
  it read and drops the part line a mid-file read starts with;
  `ferrule-ssh`'s remote shell scrubs before its remote spill.
- `ferrule-cli`: `health::known_secrets` (`[secrets]`, channel tokens,
  provider keys, and now every name the secrets file set);
  `health::redactor` uses it; `health::scrub_tool_results` installs the
  hook from `main.rs` `dispatch` for every command: `Broker::scrub_text`
  (bound secrets → placeholders, through `started_broker`, which never
  starts the proxy) then `Redactor::known` (values only, no token
  shapes; new in `ferrule-gateway`).
- `ferrule doctor`: a `history` line counts known values (and their
  JSON-escaped form) in `sessions/` and `learn/`, by name, and lists up
  to five files; warn, fail while owner mode is asked for.
- Docs: `sandbox.md` ("Env", owner mode's "Transcripts and notes"),
  `m54-autonomous.md` "Amendments after the build" (2), the roadmap.

**Tests changed, none added.**

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS), clippy clean.

**Open.** The claude engine's own tools (its Bash and Read) don't pass
through the loop. Values under 6 characters are skipped, connection
OAuth tokens aren't in the list, and the non-proxy values are read once
per process. The doctor scan reads every file whole (slow on a very
large `sessions/`). No live run.

### 2026-10-21 — M54.6: a danger notice while the box is open (Claude, Opus 5.5)

**Why.** Nothing said the box was open. The owner's own stopgap
(`mode = "off"` plus `gates = false`) showed how a "temporary" setting
outlives its reason, and owner mode, deliberately ungated, had no
reminder either.

**What changed.**
- `owner_mode::openings(cfg)`: owner mode running (`on`, not after a
  fallback), `[sandbox] mode = "off"`, `[trust] gates = false`; each
  with an id, what it opens and how to close it.
- `ferrule doctor`: `open_box_check`, a warning per opening (`open`),
  the way to close it as the hint, right after the sandbox lines.
- Dashboard Home (`api.rs` `health`): a red (`top`) notice per opening,
  with "Ask me first" (`sandbox/mode {mode: "ask"}`, which drops
  `gates = false` too) and "Open Settings"; none on a managed bot.
  `notices::unclosable` keeps the sandbox-off and gates-off notices up;
  owner mode's closes for a day and comes back while still true.
- Docs: `sandbox.md` ("Turn it on and off"), `m19-trust-cost.md` §6,
  `m37-control-room.md` (closable ids), `m54-autonomous.md`
  "Amendments after the build" (3), the roadmap.

**Tests changed, none added.**

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS), clippy clean.

**Open.** The Home notice reads the config file, so an edit not yet
applied by a restart shows early (owner mode itself follows this
process's fallback). The notice text isn't translated. No live run.

### 2026-10-21 — M54.5: one gate in owner mode, for sends after outside content (Claude, Opus 5.5)

**Why.** Owner mode turned every gate off for the owner's chats. The
proxy hands the real key to its own host whoever asked, so an injected
email or web page could send mail as the owner, delete a remote branch
or `DELETE` on a bound API without ever seeing a key: a confused deputy
the placeholders don't prevent (Meta's Rule of Two).

**What changed.**
- `ferrule-trust`: `[trust] untrusted_send_gate` (default true; "gate"
  in the name keeps it guarded from `config_set`, `config_restore` and
  the Config page). `Kind::leaves_the_box` (`ConnectedWrite`,
  `ForcePush`, `HttpDelete`). `classify::untrusted_source`: `web_fetch`,
  `web_search`, `mcp__browser__*`, a connected service's read, a shell
  command whose program is an HTTP client. `RunState::untrusted`,
  `Hub::saw_untrusted` (first source, audited `untrusted_seen`) and
  `Hub::untrusted`. `TrustGuard`: `untrusted_gate`, `guest_input`; the
  ask flow moved into `ask(tool, g, after)`, whose question and refusal
  name the outside content; with the gates off it asks only for a
  leaving class after outside content.
- `ferrule-cli` `trust::equip`: `untrusted_gate` while owner mode runs
  and the setting is on; a guest's run in owner mode starts marked.
  `owner_mode::openings`' owner-mode text says what still asks.
- Docs: `m19-trust-cost.md` §6b (and the config block, the audit list),
  `sandbox.md` owner mode, `m54-autonomous.md` §5 and amendment 4, the
  roadmap.

**Tests changed, none added.**

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS), clippy clean.

**Open.** A connected service's change is asked about every time (no
per-run grant: its subject is the tool's name). Scheduled tasks that
read and then send are refused in owner mode. Not seen as outside
content: a fetch through `python`, `git clone`, an `mcp_add` server's
reads (and its writes aren't classified), a forwarded message. No live
run.

### 2026-10-21 — M54.2: the secrets file out of the process environment (Claude, Opus 5.5)

**Why.** Startup put every key in `private/secrets.env` into the process
environment with `set_var`. Any process of the same account could read
them from `/proc/<pid>/environ` (on macOS, whether Seatbelt stops
`ps eww` is unverified), every child inherited them, and the dashboard's
rotate and delete called `set_var`/`remove_var` with threads running.
One sandbox bypass meant every key.

**What changed.**
- `ferrule-sandbox` `secret_env`: a process-wide in-memory store
  (`load`, `var`, `non_empty`, `held`, `names`, `pairs`, `set`,
  `forget`). `var` is a value saved since start, else the real
  environment's, else the file's: an exported variable still wins, as
  before, and a dashboard save wins over both, as `set_var` did.
- `ferrule-cli` `secrets::load` (was `load_into_env`) fills it;
  `secrets::var`/`non_empty` wrap it; `source` and `loaded_names` read
  it. Converted readers: provider keys (`config.rs` `resolve`,
  `models.rs` `Entry::key`, whose `SAVED_KEYS` the store replaces;
  `remember_key` now sets the store), the model catalog, the proxy's
  bindings (`shared_broker`, `mcp_add`), the channel tokens,
  `transcription.rs`, doctor, setup, the importer, `health::known_secrets`,
  the sandbox self-check, the panel secret, and the dashboard's rotate
  (`secret_env::set`) and delete (`forget`). Elsewhere:
  `ferrule-gateway` `transcribe.rs`, `ferrule-connections` `seal.rs`,
  `ferrule-plugins` `host.rs` (the "would go out as its real value"
  check), `ferrule-plans` `exported_token`.
- Children: `Sandbox::command` passes a held value only for an
  `env_passthrough` name (all of them with `scrub_secret_env = false`);
  `scrubbed_vars` lists held names too, and `child_env_var` returns what
  a command would get. `ferrule-hooks`: an owner hook's `env = [...]`
  names the keys it gets; a workspace hook's is ignored.
  `instance::child_for` no longer strips names (nothing is there).
- Kept reading the real environment on purpose: the dashboard's "set in
  the environment ferrule was started with" notes, the claude plan's
  "outranking" warning, `config_follow::secret_value` and
  `connections_setup::Place::get` (they read the file directly), the
  managed-mode and proxy variables.
- Docs: `sandbox.md` "Env" (the behaviour change and its migration) and
  owner mode's open list, `m18-hooks.md` §1–2 (`env`),
  `m54-autonomous.md` amendment 5, `research-credential-gateway.md`'s
  macOS note, the config template's `env_passthrough` line, the
  roadmap.

**Behaviour change.** The owner's spawns outside the sandbox (hooks,
gate scripts, a `[transcription] command`, the tunnel) and sandboxed
commands no longer inherit the file's keys (before, a sandboxed command
got those whose names don't look secret). Migration in `sandbox.md`
"Env": a hook's `env`, `env_passthrough`, or the service environment.

**Tests changed, none added.** The dashboard's rotate test now checks
the store, not the environment.

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS), clippy clean.

**Open.** The store is plain process memory (not `mlock`ed or
encrypted), and the file isn't sealed under a keychain key (the
report's optional half). Gate scripts and a transcription command have
no `env` of their own. No live run (Linux `/proc/<pid>/environ`, macOS
`ps eww`).

### 2026-10-21 — M56.2: standing grants and deny-and-continue (Claude, Opus 5.5)

**Why.** With owner mode off and the gates on, nothing lasted past a run:
"Allow for this run" covered one shell command until the run ended, and
a connected service's change asked every time, the same send to the
owner's own address included. And a refusal only came back as the tool's
result, so a model could keep trying variants, a card each, with no end.

**What changed.**
- `ferrule-trust` `grants`: `Grants` over `<data>/private/grants.json`
  (0600, hidden from commands and the file tools in every mode, left out
  of backups), read at each check; `Grant` (shell: argv and cwd; a
  connected change: tool and an address or `@domain`), `Spec` (what
  "Always here" would keep), `add`, `revoke`, `widen`.
- `classify`: `grantable_command` (the command is exactly the one
  simple command the gate found, with nothing the shell would expand or
  chain) and `recipients` (`to`/`recipient(s)` plus `cc`/`bcc`; none
  when the call names no `to`).
- `approval`: `Answer::Remember`, `remember <code>`, `open_for_gate`;
  a `remember` where it isn't offered reads as `always` or `yes`.
- `hub`: the grants; `use_standing` (audited with route `standing`);
  `ask_gate(…, remember)` adds the **Always here** button and its line,
  and keeps the grant on that answer (`grant_added`); `denied`,
  `let_through`, `escalated` with `DENIALS_IN_A_ROW` (3) and
  `DENIALS_IN_A_RUN` (20), audited `denials_escalated`.
- `guard`: `TrustGuard::workspace`; `standing` builds the spec, and
  none for a call that leaves the box once the run read outside content
  (M54.5's rule, in owner mode or not); `ask` tries the run grant, then a
  standing one, refuses without a question once escalated, counts
  refusals and appends the next step to the refusal;
  `before_model_call` ends an escalated run with its answer.
- `ferrule-cli`: `trust::equip` takes the workspace; the dashboard's
  `GET grants`, `POST grants/revoke` and `grants/widen` (confirmed,
  audited), and a **Standing approvals** card on Settings with Revoke
  and Whole domain (Hebrew included). The email channel reads
  `remember <code>` as an answer.
- Docs: `m19-trust-cost.md` §6b, §7b, §7c and the audit list,
  `m48-self-service.md` D9, the roadmap.

**Tests changed, none added.** One refusal assertion now checks
`contains` (the refusal has the next step after "It was not run.").

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS), clippy clean; `node --check` on `app.js` passes.

**Open.** "Allow for this run" still covers a whole command when its
first gated part was granted (`a; b` with `a` granted runs `b` too); the
standing grant avoids that by taking only single commands. No CLI for
grants (the dashboard only). The escalation thresholds aren't settings.
A terminal run's question never offers Always here. No live run.

### 2026-10-21 — M56.3: self-administration from the chat (Claude, Opus 5.5)

**Why.** "Turn on web search" and "turn the browser on" had no chat op
(`config_set` covered 19 fields, none of these), and every change,
"remind me at 9" and "pause that reminder" included, needed a card tap.

**What changed.**
- `config_page.rs` `FORM`: `web_search.provider` (duckduckgo, brave,
  tavily, exa, off), `max_results`, `safe_search`, `region`, `language`,
  `max_output_tokens`, `timeout_secs`, `price_per_search_usd`,
  `max_searches_per_day`, and `browser.enabled` (29 fields). A `text`
  kind (up to 16 letters, digits, `-`, `_`) for region and language, on
  the Config page too. `OPEN` lets `browser.enabled` through
  `guarded_change` while `[browser]` stays a guarded section;
  `web_search.api_key_env` and `endpoint` stay guarded. `locks.rs`: a
  managed bot without the browser refuses turning it on.
- `mcp_add` outside owner mode: already there since M54 §6.2 (with the
  card, keys only as `${NAME}` of `[secrets]`, new ones through
  `secret_request`); nothing added.
- `self_service/quick.rs`: `tap_free` (no outside content in the turn,
  `Hub::untrusted`; `task_add`, `task_schedule`, `task_pause`,
  `task_delete` on the owner's own tasks; `model_default`, `model_here`
  to a connected, ready model) and its `Undo`; `run` (the page's handler,
  audited `self_service.ran` with the undo id; a receipt "Done without
  asking: …" with an Undo button); `undo` for `/undo_change <id>` (the
  owner, once, a day, this process; audited `self_service.undone`).
  `tool.rs` `ask_and_run` takes that path after the card's checks, only
  outside owner mode. `door.rs` handles `/undo_change`.
  `TasksAdmin::get`; `Hub::tell_in_choices`. The tool description gets
  one appended sentence.
- Docs: `m48-self-service.md` ("After M48: M56.3"), `web-search.md`,
  `browser.md`, the roadmap.

**Tests changed, none added.** The self-service rig's turn now counts as
having read outside content, so its card tests still see a card. The
gateway test `the_owner_switches_the_default_model_from_telegram_with_one_tap`
became `…_without_a_tap`: it checks the receipt's Undo button, the
answer and the audit's "(no card: M56.3)" instead of a tapped card (its
unused `tap` helper went).

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored,
73 binaries (macOS), clippy clean; `node --check` on `app.js` passes.

**Open.** The Undo list lives in memory (gone after a restart). A
deleted task comes back with a new id, in the primary owner chat. A
question already waiting in the chat still blocks a tap-free op. The
web search provider can be switched while `api_key_env` names another
provider's key (the key then goes to the new provider's host, with the
owner's tap). No live run.

### 2026-10-21 — M56.3 fix: a search provider switch never takes the old key along (Claude, Opus 5.5)

**Why.** M56.3 made `web_search.provider` settable from the chat and the
Config form, and the raw Config editor could already change it. The
loader binds `[web_search] api_key_env` to the endpoint's host
(`Config::finish`), so switching Brave to Tavily would have bound
`BRAVE_API_KEY` to `api.tavily.com`: provider A's key sent to provider
B's host (M56.3's own entry listed it as open). And a `duckduckgo`
provider with a leftover `api_key_env` bound that key to DuckDuckGo's
host.

**What changed.**
- `config_page::set` on `web_search.provider` (`search_key`): every
  switch drops a custom `endpoint`; DuckDuckGo and off drop
  `api_key_env`; Brave, Tavily and Exa take a key that already goes to
  the new host (a `[secrets]` entry bound there: the key in use, then the
  provider's own name, then any other), else the provider's own variable
  (`ferrule_tools::search::SearchProvider::key_env`: `BRAVE_API_KEY`,
  `TAVILY_API_KEY`, `EXA_API_KEY`) when it is saved and not in
  `[secrets]`, which binds it to that host alone. Nothing fits: a 409
  naming the `secret_request` (name and host) to make, and nothing
  changes. The answer says which key and host (`said`).
- `config_page::key_moved`, run with `guarded_change` by `commit` (the
  form, the editor, version restore, undo), by `check`, and by the chat's
  `config_restore` (`api.rs`): refuses a write that would bind any key to
  a host it isn't bound to now (comparing the bindings the old and new
  file load to; a file that doesn't load counts as its `[secrets]`). The
  one new binding let through is a provider's own variable on its own
  host (`WebSearchConfig::own_key`). `guarded_change` lets
  `web_search.api_key_env` change, and `endpoint` go, only together with
  `provider`.
- `WebSearchConfig::settings`: DuckDuckGo takes no key, so a leftover
  `api_key_env` beside it isn't bound to its host.
- Docs: `web-search.md` ("A new provider never inherits the old one's
  key"), `m48-self-service.md` (After M48), the roadmap's M56.3 line.

**Tests changed, none added** (the owner's standing rule). No existing
test needed a change.

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored, 73
binaries (macOS), clippy clean.

**Open.** A hand edit of the file isn't checked (whoever edits the file
decides). A provider switch from the chat asks for the card before the
key check, so a switch with no fitting key is refused after the tap. The
key a switch leaves behind stays bound in the running proxy until a
restart (`config_follow` doesn't unbind), to its old host only. No live
run.

### 2026-10-21 — M60: images in tool results reach the model (Claude, Opus 5.5)

Claims **M60** (Current State updated: the next free number is M61).

**Why.** A tool could save an image (an MCP screenshot under
`.ferrule/mcp-content/`, `generate_image`'s file under `media/`) and name
it in its text, but the model never saw the pixels: `ToolOutput::files`
went nowhere, and `read_file` on a PNG failed on UTF-8. M47 already sent
the photos people send as pixels, by path, to models that see images;
tool results had no such path.

**What changed.**
- `ferrule-core`: `ToolOutput::images: Vec<ImageRef>` and `with_images`
  (left out of the serialized form when empty); the agent carries them
  through `Ran` onto the tool's result message (`Message::with_images`),
  every place a result is pushed; compaction's dedupe clears a superseded
  result's images. `vision.rs`: `MAX_IMAGE_BYTES` (3.5 MB) and `sniff`
  moved here from `ferrule-providers` (re-exported there, same names);
  `image_file(path)` (an image by its first bytes, under the cap, as an
  `ImageRef` with the absolute path); `tool_note` and `note_for` (a tool
  image's notes: text-only model, "[image omitted: …, an earlier tool
  image]", gone).
- `ferrule-providers`: `vision::tool_parts`. Anthropic puts a tool's
  images inside its `tool_result` (text block, then `image` blocks). Chat
  Completions keeps the `tool` message text-only and sends one `user`
  message after the batch's last result with the `image_url` parts,
  headed "[Tool output, not a message from the user: the images the tool
  call(s) <ids> returned, in order.]". Responses (and the Codex backend,
  same builder) does the same after the `function_call_output`s with
  `input_image` parts. A result without images goes out byte for byte as
  before. The newest four images (photos and tool images together) go as
  pixels; `est_context_tokens` already counts them (1,600 each); the
  400-names-images retry covers them.
- Tools: `read_file` returns a PNG, JPEG, GIF or WebP as an image (text
  `[image <path>: <mime>, <n> bytes]`), refuses one over 3.5 MB with a
  clear error, and says so in its description. MCP results attach each
  saved part that is an image (a line for one over the cap).
  `generate_image` attaches the image it saved ("It isn't shown to you:
  …" when over the cap). `ferrule-plugins`: the new field in its one
  struct literal.
- No image crate in the workspace, so no downscaling: a byte cap and a
  refusal. Transcripts store the `ImageRef` (path, type, name), not
  base64.
- Docs: new `docs/vision.md`, linked from the README's Guides,
  `models.md` and `browser.md`; the roadmap's M60 line.

**Tests changed, none added** (the owner's standing rule). No existing
test needed a change.

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored, 73
binaries (macOS), clippy clean. No dashboard JS changed.

**Open.** No live run against Anthropic, OpenAI, a compatible server or
the ChatGPT-plan backend: the wire shapes follow the code paths M47's
photos already use, not a real call. Pixel dimensions aren't checked (a
provider's 400 that names images falls back to notes for that provider
from then on). An SSH workspace's `read_file` still reads text only. The
dashboard's chat doesn't show a tool's images. When a fifth image
arrives the oldest turns into a note, so the prompt cache misses from
there (M47's trade). Responses could take images inside
`function_call_output`; the follow-up user message was chosen because
the same builder serves the Codex backend.

### 2026-10-21 — M61: screen viewing, view only (Claude, Opus 5.5)

Claims **M61**, in the `autopilot` worktree (PR #52). Current State
updated: the claim, the next free number (M62), and PR #49 and #51
merged with `autopilot` continuing as PR #52.

**Why.** The owner wants to ask the bot from their chat what's on their
Mac's screen, or for a short clip of it, without remote-desktop software.
By the owner's explicit choice it is **view only**: no mouse, keyboard,
app launching or other input, not even as stubs. M60 made a tool's
images reach the model, which a screenshot needs.

**What changed.**
- `ferrule-cli/src/screen.rs` (new): `[screen]` (`enabled = false`,
  `max_record_secs = 120`, `window_minutes = 30`); `desktop()` (macOS,
  not started from SSH, `launchctl managername` says `Aqua`; asked once
  per process); `Screen`, one per gateway: the owner's yes held in
  memory for `window_minutes` across the owner's chats, asked through
  `Hub::ask_in` with Allow / Refuse, one card at a time, in owner mode
  too; a refusal goes back to the model and counts through
  `Hub::denied` (M56.2). The `screenshot {display?}` and
  `record_screen {seconds}` tools (deferrable, serial group `screen`,
  `changes_files` false); `record_screen` isn't offered with
  `max_record_secs = 0`. `test_capture` for doctor.
- Captures: `/usr/sbin/screencapture` (`-x -t png` with `-m` or `-D n`;
  `-x -v -V <secs>`), `/usr/bin/sips` (a JPEG copy at most 1600 px on
  its long side when the PNG is over M60's 3.5 MB cap; the PNG stays
  whole), and an `ffmpeg` from `PATH` or Homebrew's dirs (H.264 MP4 at
  most 1920 px wide, no audio, the `.mov` deleted; without one the
  `.mov` stays and the answer says why). All run in the shared sandbox
  as a helper with desktop services (`for_helper(media/screen)
  .with_desktop_services()`, the browser's loosening). Files go to
  `<workspace>/media/screen/` with a `.gitignore` of `*`; a folder that
  leads outside the workspace is refused. The model picks only checked
  numbers; no text of its reaches a command line.
- Every capture calls `Hub::saw_untrusted(session, tool)`, so M54.5's
  gate applies for the rest of the run, and audits `screen_capture`
  (`what`: "screenshot taken" or "recording N s", and the file name).
- `main.rs` `gateway_factory`: `Screen::offered` once (the log says why
  when `[screen]` is on and there's no desktop); a chat session gets the
  tools and `prompts::CHAT_SCREEN` (new, constant) only when it's local,
  the owner's (`owner_mode::owner_tree`) and `Screen` exists. The card
  goes to the session's chat (`self_service::offered`), else the primary
  owner chat.
- `owner_mode::guest_hidden` now takes the workspace and adds
  `<workspace>/media/screen`, so the commands, file tools and
  `send_file` of every session that isn't the owner's can't read the
  captures. Callers: `build_agent_from`, `session_hidden` (which takes
  the workspace too) and `subscription/claude.rs`.
- `dashboard/config_page.rs`: `screen` in `WHOLE`, so the editor,
  restores and the chat's `config_restore` can't turn it on (it was
  never a form field, so `config_set` couldn't).
- `doctor.rs` `screen_check`: off is a note; on with no desktop here a
  warning saying why; else one test capture in the tools' sandbox to a
  temp file deleted at once (a failure fails, naming the Screen
  Recording setting), and a note naming the service's binary to allow.
- Docs: new `docs/m61-screen.md` (what it does, what it deliberately
  doesn't, consent, outside content, privacy, permissions, limits),
  linked from the README's Guides; the roadmap's M61 line; `vision.md`
  (the screenshot as a tool image, the `sips` copy),
  `m57-context-hygiene.md` (the deferrable list), `m19-trust-cost.md`
  §6b (what marks a run), `m54-autonomous.md` (what `guest_hidden`
  adds).

**Others' surfaces, smallest changes:** `config_page::WHOLE` (M54's
config guard lists, in M52's editor) gains `screen`;
`owner_mode::guest_hidden` (M54 §7) gains the workspace parameter and
one path. No `ferrule setup` prompt changed, so `tests_e2e/` is
untouched.

**Tests changed, none added** (the owner's standing rule). No existing
test needed a change.

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored, 73
binaries (macOS), clippy clean.

**Permissions the owner grants.** macOS Screen Recording (System
Settings → Privacy & Security → Screen Recording) for the program that
starts the capture: the ferrule binary the service runs (`ferrule
doctor` names it; `<data>/bin/ferrule` in owner mode), or Terminal for a
gateway started there. Then `[screen] enabled = true` in the config
file and a gateway restart.

**Open.** No live run: no capture was taken while building this, so
`screencapture` and `sips` under the Seatbelt profile with desktop
services, how macOS attributes the permission for the LaunchAgent's
captures, and the MP4 conversion are unverified; `ferrule doctor` on the
Mac is the first check. The window can't be closed early except by a
restart. A recording holds the turn for its length plus the conversion
(not an M57.6 background job). macOS only; Linux and Windows don't offer
the tools. A recording over Telegram's 50 MB bot limit can't be sent.
Doctor's test checks the permission of the terminal it runs in, not the
service's. Nothing prunes `media/screen/`.

### 2026-10-21 — M59: personal WhatsApp and iMessage, through bridges the owner runs (Claude, Opus 5.5)

Claims **M59**, in the `autopilot` worktree (PR #52). Current State
updated: the claim, and M59 among PR #52's commits.

**Why.** The gap analysis's M59.1: the owner's contacts are on a personal
WhatsApp number and on iMessage, and the M39 WhatsApp channel needs a
Business number from Meta. A WhatsApp Web protocol client in Rust would
put a moving, unofficial protocol and its terms-of-service risk inside
ferrule, so each channel talks to a bridge the owner runs instead, the
way Signal talks to signal-cli.

**What changed.**
- `ferrule-gateway/src/channels/waha.rs` (new), channel
  `whatsapp_personal`: WAHA's REST API out (`/api/sendText`,
  `/api/sendImage` for JPEG/PNG, `/api/sendFile`, `/api/startTyping` and
  `stopTyping`, `X-Api-Key`), its WebSocket in
  (`/ws?session=…&events=message.any&events=session.status`, the key as
  its documented `x-api-key` query parameter; `ws::connect` never puts
  the URL in an error). Before each socket, `GET /api/sessions/{session}`:
  not `WORKING` (QR scan pending, stopped) is the channel's problem in
  words, retried with backoff. Ping every 30 s, three silent ones
  reconnect. Who gets in: a DM from `allowed_users` (digits; `@c.us`
  ids shortened to them, `@lid` and `@g.us` kept whole), an
  `allowed_groups` group on an @mention of the linked number or a reply
  to it, and the "Message yourself" chat when the account's own number is
  allowed; what ferrule sent is remembered ten minutes by id key and text
  so its echo there isn't answered. `Access::silent()`: a stranger is
  never told anything. Media in through WAHA's `media.url`, fetched with
  the key only from WAHA's own base address (another host is refused),
  capped by `read_capped`, saved to the inbox. No reactions (a 👀 would
  come from the owner), buttons or edits.
- `ferrule-gateway/src/channels/bluebubbles.rs` (new), channel
  `imessage`: BlueBubbles Server's REST API with the password as its
  documented `password` query parameter (never logged). Polls `POST
  /api/v1/message/query` (ASC, `after`, `with` chat/attachment/handle)
  every `poll_secs`, two minutes behind the newest message seen, deduped
  by guid; the position in `<data>/gateway/imessage/cursor`, replayed at
  most a day back; the first sweep only marks history seen. One-to-one
  conversations only (`;-;` guids); group chats, `isFromMe`, tapbacks and
  group events are ignored; addresses normalised (`+` and digits, emails
  lower case). Sends through `/api/v1/message/text` and
  `/message/attachment` (multipart via `files::multipart`), `apple-script`
  unless `private_api`, which adds typing and `selectedMessageGuid`
  replies. Attachments in through `/attachment/{guid}/download`.
- `ferrule-trust`: both channels in `OWNER_CHANNELS` (after email),
  `whatsapp_personal_owner` and `imessage_owner`, their titles.
- `ferrule-cli`: `[gateway.whatsapp_personal]` (url, session,
  `api_key_env` = `WAHA_API_KEY`, allowed_users, allowed_groups,
  max_file_mb) and `[gateway.imessage]` (url, `password_env` =
  `BLUEBUBBLES_PASSWORD`, private_api, poll_secs, allowed_users,
  max_file_mb) in `channels/settings.rs`; `channels/bridges.rs` (new): the
  adapters' settings and both dashboard cards (fields, guide, Test,
  check); `channels/mod.rs`: the table, secret envs (so the keys are
  scrubbed from commands, redacted and taken out of tool results like
  every channel token), allow-lists, the instance clash account and its
  words; `main.rs` `build_channels`; `setup/bridges.rs` (new): a guided
  step and a menu entry each (address, key checked live, WAHA's session
  waited for, numbers typed); `doctor.rs`: a line each (WAHA session
  `WORKING`, a terms-of-service note; BlueBubbles answers, `private_api`
  matches the server), and a fix: `telegram_on || … || m39_channels(…)`
  short-circuited, so with Telegram on no M39 channel had a doctor line;
  now they always run.
- `tests_e2e/setup_wizard.py`: the two new guided prompts ("Connect a
  personal WhatsApp number", "Connect iMessage"). `hidden_keys.py` drives
  no setup prompt, so it is unchanged.
- Docs: new `docs/m59-channels.md` (the risks first, why bridges and why
  WAHA and BlueBubbles, setup, who gets an answer, what works, how each
  connects, secrets, what's open, sources); `docs/channels.md` (the
  picker table, a section each); the README (overview, gateway row,
  setup list, crate line, Guides); the roadmap's M59 line.

**Shared behaviour, no change needed:** owner/guest classification
(`trust::owners` reads the channel table), the paste guard (M52.5,
`gateway.rs`), steering and the busy notice (M56.1, the router),
transcription of `audio/*` attachments, `send_file`. Neither channel can
delete a pasted message, so the reply asks the sender to.

**Others' surfaces, smallest changes:** `ferrule-trust/src/config.rs`
(two owner fields and the channel list; M54 owns `guard.rs` and
`classify.rs`, untouched). The config editor's guard already refuses
`url`, `*_env`, `allowed*` and `*owner*` keys by name.

**Tests changed, none added** (the owner's standing rule). No existing
test needed a change.

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored, 73
binaries (macOS), clippy clean.

**Open.** No live bridge was available: WAHA's WebSocket with two
`events` parameters and its query-string key, the send responses' id
shapes per engine (the echo check falls back to text), BlueBubbles'
`after` in milliseconds and `handle` on query results are as documented,
not observed. `@lid`-only contacts must be listed by that id, and the
self chat is found by the `@c.us` id only. iMessage groups aren't
supported. No pairing by code in setup (numbers are typed). The gap
analysis suggested a feature-flagged crate; these are two modules in
`ferrule-gateway`, opt-in by config like every other channel.

### 2026-10-21 — M54.4: network through the proxy, logged, opt-in (Claude, Opus 5.5)

Claims M54's numbered item **M54.4**, in the `autopilot` worktree (PR
#52). Current State updated: the claim, and M54.4 among PR #52's
commits. M54 owns `crates/ferrule-sandbox`; this is M54's own item.

**Why.** The gap analysis's M54.4: egress is advisory for programs that
ignore `HTTPS_PROXY` (ssh, database clients, UDP connect directly,
`docs/egress.md`), and the owner can't see where data went. The policy
stays allow-by-default (the analysis and M54 agree on no default-deny);
what changes is that the owner can make the proxy the only way out, and
that every destination it lets through is written down.

**What changed.**
- `ferrule-proxy`: `EgressPolicy::enforce` and `enforced()`, a
  `describe` line for it; `commands_proxied()` is true under it (so
  commands always get `HTTPS_PROXY` and the CA bundle);
  `Broker::enforced_port()`; `Destination` and `Broker::on_destination`:
  after the policy passes a CONNECT or a plain-HTTP request, the hook
  gets (source, method, host, port), at most once a minute per source,
  host and port (`DESTINATION_EVERY`). Never a path, header or body.
- `ferrule-sandbox`: `Sandbox::with_proxy_only(port)`, `proxy_port()`
  (network on, not hide-only, not the desktop/browser helper) and
  `proxy_enforcement()`. `command()` refuses every command, with the
  reason, when binding is asked for and can't be applied (no backend,
  `mode = "off"`, Windows, a Linux supervisor that can't run). macOS:
  `seatbelt::bind_to_proxy` appends `(deny network-outbound (remote ip
  "*:*"))` and `(allow network-outbound (remote ip "localhost:<port>"))`
  after the profile. Linux (cfg-gated, **CI-compiled only**: this Mac
  can't build it): `UnixSockets` gains `any` and `proxy_port`; the
  supervisor runs whenever binding is on (every Unix socket allowed when
  `unix_sockets = ["*"]`), and `connect_for` lets an IP connect through
  only to `127.0.0.0/8`/`::1`/`::ffff:127.x` on the proxy's port,
  connecting the copy itself with the address it checked (`to_proxy`,
  `ip_of` for the log line); `linux::apply` takes `proxy_only`, and
  `seccomp::filter_with(.., stream_only)` refuses every `socket()` but
  `AF_UNIX` and `AF_INET`/`AF_INET6` `SOCK_STREAM`, and `MSG_FASTOPEN` on
  `sendto`/`sendmsg`/`sendmmsg` (`filter` keeps its signature, so its
  test is unchanged). Landlock ABI 4 net rules weren't used: port-only
  (any host on the proxy's port) and no UDP. The shell tool's model note
  says direct connections are refused (a constant for the config; the
  port isn't in it, so the prefix stays byte-stable).
- `ferrule-cli`: `[egress] enforce` (default false); on Windows the
  config refuses it at load with why. `proxied(sandbox, broker)` replaces
  the five places that attached the proxy (the shared sandbox, the
  Claude-plan engine's, owner mode's, `mcp add`'s, the config follower's)
  and binds when enforced; `ferrule sandbox` binds its own and prints
  `egress    bound: …` or why not. `egress::report_destinations` writes each destination to
  the trust audit log as `egress` (`<data>/trust/audit.jsonl`), wired
  next to `report_denials`; `ferrule egress log [--since]` lists them by
  `host:port` with who asked, logged and refused counts, last seen (a
  `PARITY` row in `dashboard/console.rs`, read, so the page's console runs
  it too). Doctor's `egress` section: enforce's state (applies, can't apply and
  why, or `network = false` already), and the hosts reached in 24 h.
  Setup's Network policy summary says "commands bound to the proxy". The
  config editor can't change it (`egress` is in `WHOLE`).
- Docs: `docs/egress.md` ("Binding commands to the proxy (`enforce`)",
  what breaks and how to allow it, a socat `ProxyCommand` tunnel for ssh,
  "Where requests went: the audit log", the table), `docs/sandbox.md`
  (the table row, the Network section), `docs/m37-control-room.md` §4.1
  (the `egress log` row), the roadmap's M54.4 line.

**Tests changed, none added** (the owner's standing rule):
`seatbelt.rs`'s `unix_sockets_are_refused_then_let_back_in_from_the_allowlist`
spells out the two new `UnixSockets` fields.

**Checks.** `make check` exited 0: 1881 passed, 0 failed, 28 ignored, 73
binaries (macOS), clippy clean. The first run failed one test,
`every_subcommand_has_a_place_on_the_page_or_a_reason`, until `egress log`
had its console row and doc row. The Linux code (`linux.rs`,
`supervisor.rs`, the Landlock branch of `Sandbox::command`) isn't built on
macOS: CI's Linux job is its first compile.

**Open.** Nothing here was run with `enforce = true`: the Seatbelt rules'
syntax and effect on a real Mac, and the Linux supervisor's IP check and
the new seccomp instructions on a real kernel (CI compiles them; no test
exercises them), are unverified. First checks: `ferrule sandbox -- curl
-sS https://example.com` (goes) and `ferrule sandbox -- nc -z example.com
22` (refused). Name lookups through the system resolver (mDNSResponder;
nscd or resolved's socket) still leak a little in DNS queries. Local TCP
servers, ssh and TCP database clients stop working under it (documented,
with the tunnel). The audit log grows by one line per destination a
minute; nothing prunes it. `ferrule trust audit` now lists `egress`
events among the rest.
