# PLAN.md

This file is **shared and appendable**. Multiple sessions/models work on this
repo concurrently from different machines — don't trust a stale copy of this
file over `git log`/the actual code. Keep the "Current State" section current
**in place** (edit it, don't append duplicates of it). Add a new dated entry
under "Session Log" for every session's work; never delete prior entries
(that's what `archive/`-style trimming is for, and this repo doesn't have
that convention yet — ask before introducing one).

## Current State

- **Name:** Ferrule (renamed from `agentrust` 2026-09-23). GitHub repo is
  `maximarhipkin/ferrule`. Crates are `ferrule-{core,providers,tools,memory,gateway,mcp,cli}`,
  binary is `ferrule`, config file is `ferrule.toml` /
  `~/.config/ferrule/config.toml`, agent workspace state dir is `.ferrule/`.
- **What it is today:** a local, single-user CLI agent runtime. `ferrule run`
  / `ferrule chat` drive a ReAct loop (`ferrule-core::Agent`) against any
  OpenAI-chat-completions-compatible endpoint (`ferrule-providers`), with a
  fixed small toolset (fs read/write/list, shell with a deny-list, web fetch,
  todo/diary) from `ferrule-tools`, and a single-file SQLite memory store
  (`ferrule-memory`, FTS5 BM25 + time-decay recall), plus any tools exposed
  by stdio MCP servers (`ferrule-mcp`, M4). No
  multi-agent orchestration, no OS-level sandboxing yet — see gap list below.
  **A daemon now exists and is reachable** (`ferrule-gateway`, see Session
  Log 2026-09-23/24, M1+M2): a `Channel` adapter trait, normalized
  in/outbound message model, a `Router`/`Gateway` giving one FIFO session
  lane per (channel, chat) with JSONL-transcript resume, a Telegram
  long-polling adapter, and a local stdin/stdout adapter — all wired into
  `ferrule-cli` as `ferrule gateway`, configured via a new `[gateway]`
  section in `ferrule.toml`. **M3 scheduler exists** (Session Log
  2026-09-24): SQLite-persisted cron (IANA timezone) + one-shot tasks, each
  running as a turn on its own resumable session, truthful per-run status
  rows, optional gate script, no-overlap guard, collapse-to-one missed-run
  policy — `ferrule tasks add|list|pause|resume|delete|runs|run-now`, and
  `ferrule gateway` runs the scheduler loop alongside the channels,
  configured via `[scheduler]`. **M4 stdio MCP client exists** (Session Log
  2026-09-24, M4): each `[[mcp.servers]]` entry is spawned once per
  process, its tools are listed and registered as `mcp__<server>__<tool>`,
  and they're shared by every session. Verified end to end with the real
  `ferrule run` binary against a mock LLM + mock MCP server. **A per-call
  ledger exists** (Session Log 2026-09-24, Phase 0): every provider call
  from `run`/`chat`/gateway sessions/scheduler tasks appends one JSONL row
  (provider, model, task shape, tokens incl. cached, latency, ok/error,
  cost if priced) to `<data_dir>/ferrule/ledger.jsonl`; `ferrule ledger
  [--since 7d]` summarizes it.
- **Toolchain (Devi/NanoClaw sandbox, updated 2026-09-23 late):** Debian's
  apt `rustc 1.63`/`cargo 1.65` is installed but **too old** — dependencies
  (e.g. `clap_builder 4.6`) use edition 2024 and fail to parse. Use the rustup
  stable toolchain (1.98.1) installed under `/workspace/agent/.cargo-home` +
  `/workspace/agent/.rustup` (persistent). Env: `RUSTUP_HOME`, `CARGO_HOME`,
  `CARGO_TARGET_DIR=/workspace/agent/.cargo-target`,
  `CARGO_HTTP_CAINFO=/tmp/onecli-combined-ca.pem`, prepend `.cargo-home/bin`
  to `PATH`. **Tests need `NO_PROXY=localhost,127.0.0.1,::1`** — the sandbox's
  HTTP proxy otherwise swallows the mock-server request in
  `openai_compat::tests::sends_tools_and_parses_tool_call` (env issue, not a
  code bug). **Since M4 this is no longer needed**: the provider's and the
  Telegram adapter's reqwest clients call `.no_proxy()` under `cfg!(test)`,
  and the suite is green with `NO_PROXY` unset. A *binary* talking to a
  local endpoint still needs `NO_PROXY` in this sandbox.
  **Status: `cargo check --workspace --all-targets` clean, `cargo test
  --workspace` 81/81 green** (24 pre-existing + 41 in `ferrule-gateway`
  + 7 in `ferrule-mcp` + 2 core and 7 cli ledger tests, after M1-M4 and
  Phase 0), `cargo clippy --workspace
  --all-targets` has one pre-existing warning in `ferrule-core::agent`
  (collapsible_if, predates the gateway work) and zero warnings in
  `ferrule-gateway`/`ferrule-mcp`/the ledger. `rustup
  component add clippy rustfmt` now done (was missing, only
  `cargo`/`rust-std`/`rustc` before).
- **Branding:** `docs/branding/logo.png` (512x512 mark) and
  `docs/branding/hero.png` (1600x800 README header), referenced from the top
  of `README.md`. Generated programmatically with Pillow (geometric
  metal-band-around-a-rod-bundle mark, steel + copper on near-black) — **not**
  AI-image-generated, because no `GEMINI_API_KEY` is configured for the
  `design` skill in this sandbox and `google-genai`/pip installs are blocked
  (PEP 668 + no root). If a future session has a working Gemini key, an
  AI-generated pass could replace these with something more polished; keep
  the same metal-band-binding-a-bundle motif and dark/steel/copper palette if
  you do, so the brand doesn't drift session to session.
- **Open architectural gaps vs. the "replace NanoClaw and OpenClaw" goal:**
  see the dated session-log entries below for the full writeup; short
  version: channels/messaging now has two working adapters (Telegram,
  local) reachable via `ferrule gateway`, a task scheduler (M3) and a
  stdio MCP client (M4), but still no skills/plugin system, no
  credential-injection gateway, weak sandboxing (substring deny-list only,
  no OS primitives), no multi-agent orchestration, no multi-provider
  routing (Phase 1+ of `docs/research-routing-and-local-models.md`, blocked
  on Max's decisions there). These are the largest deltas. (The
  cost/observability ledger gap closed 2026-09-24, Phase 0.)

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
