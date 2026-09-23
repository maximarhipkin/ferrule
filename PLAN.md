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
  `maximarhipkin/ferrule`. Crates are `ferrule-{core,providers,tools,memory,cli}`,
  binary is `ferrule`, config file is `ferrule.toml` /
  `~/.config/ferrule/config.toml`, agent workspace state dir is `.ferrule/`.
- **What it is today:** a local, single-user CLI agent runtime. `ferrule run`
  / `ferrule chat` drive a ReAct loop (`ferrule-core::Agent`) against any
  OpenAI-chat-completions-compatible endpoint (`ferrule-providers`), with a
  fixed small toolset (fs read/write/list, shell with a deny-list, web fetch,
  todo/diary) from `ferrule-tools`, and a single-file SQLite memory store
  (`ferrule-memory`, FTS5 BM25 + time-decay recall) . No daemon, no
  messaging/channels, no MCP client, no multi-agent orchestration, no OS-level
  sandboxing yet — see gap list below.
- **Toolchain constraint (important for every future session in this
  sandbox):** this container has **no Rust toolchain and no root** (`cargo`,
  `rustc` are both absent; `apt-get install` fails with a dpkg lock-permission
  error). `cargo check`/`cargo test` cannot be run here. If your session has a
  working `cargo`, please actually run `cargo check --workspace` and note the
  result here — until then, changes in this repo from *this* sandbox are only
  textually/manually verified (grep for stray old names, cross-check
  `Cargo.toml` package names / path deps / `use` statements by hand).
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
  see the dated session-log entry below for the full writeup; short version:
  no channels/messaging layer, no MCP client, no task scheduler/daemon, no
  skills/plugin system, no credential-injection gateway, weak sandboxing
  (substring deny-list only, no OS primitives), no cost/observability ledger,
  no multi-agent orchestration. These are the largest deltas.

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
