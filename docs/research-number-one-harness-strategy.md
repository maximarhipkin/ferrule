# Research: the path to the #1 agent harness

**Date:** 2026-09-24 · **Method:** six parallel investigations — (1) Hermes/OpenClaw/NanoClaw deep-dive, (2) Claude Code/Codex/OpenHands/Goose/Aider/Letta techniques, (3) community adoption drivers, (4) full PLAN.md + research-doc gap catalog, (5) ferrule code-level architecture analysis, (6) 2025–2026 research frontier. Sources are primary where noted; unverifiable claims are flagged at the end.

**The question:** what does ferrule need — beyond its current plan — to be the agent runtime developers choose over Hermes, OpenClaw and NanoClaw? Three pillars: easiest to use, easiest to extend (plugins/MCPs), smartest and most dynamic harness.

---

## 1. The market in one table (GitHub API, 2026-09-24)

| | Stars | Shape | Extension economy | Security posture |
|---|---|---|---|---|
| **OpenClaw** | ~390k | Node gateway, 20+ channels, Control UI, multi-agent | Plugin SDK + ClawHub (~67k skills, growth real, exact count unverified) | Sandbox **off by default**; 138+ CVEs by mid-2026 (secondary, corroborated by arXiv:2603.27517 taxonomy); ClawHavoc: 341/5,705 registry skills malicious (Snyk) |
| **Hermes** (NousResearch) | ~249k | Python agent, 20+ channels, TUI + desktop app, 7 terminal backends | Skills Hub + `/learn` + self-authored skills | Approval-gating first, containers optional; "wiped my secondary drive" anecdote (unverified) |
| **NanoClaw** | ~31k | One Node process + Docker container per session, Claude Agent SDK inside | `/add-*` skill branches, tiny ecosystem | Container isolation default; credentials never enter container; CVE-2026-56694 fixed |
| **ferrule** | private, v0.1.0 | One 10 MB Rust binary, 2 channels | MCP (sandboxed) + skills; no registry | **Sandbox on by default + built-in credential proxy** — best-in-class |

The community gap is the single largest non-technical gap. It is closed by going public with the security story, not by features.

## 2. What users actually choose and abandon for (ranked by evidence)

**Drivers:** (1) **cost control** — the Systima study (Claude Code ~33k tokens before your prompt, subagent fan-out 121k→513k) made token overhead a front-page issue; (2) **credential/machine safety** — post-OpenClaw-crisis, "where do my keys live" is the first evaluation question; (3) **no lock-in / local models**; (4) **time-to-first-value** (OpenClaw's 30–60 min setup is a mass-abandonment point); (5) **reliability** — OpenClaw's #1–2 complaints are "says done when it isn't" and stuck-idling; Hermes' #1 is self-evaluation always reporting success; (6) MCP/skills breadth, **with vetting**; (7) privacy; (8) footprint/speed (ZeroClaw, Rust, 3.4 MB, 31.9k stars, exists purely on this axis — ferrule's direct niche competitor).

**Anti-drivers:** key leaks; "deleted my database/home directory" stories (Replit, Claude Code — these made sandboxing/approval gates evaluation criteria); runaway spend ($40/day anecdotes); silent stuck loops; breaking-change churn (OpenClaw's #1 complaint); rate-limit friction; astroturfing (~15% of operators distrust Hermes over suspected coordinated promotion — launch conduct matters).

**Net read:** ferrule has already built the top-2 drivers (security, cost visibility) and the answers to the top anti-drivers (never-stuck, verify, sandbox). The gap is **breadth** (channels, multi-agent, extension economy) and **proof** (public repo, published measurements).

## 3. Where ferrule stands

**Clearly ahead:** secure-by-default depth (the only runtime with kernel isolation on by default for shell *and* MCP servers, plus a built-in TLS credential proxy — nobody else has header-only injection); per-model harness engineering (profiles, structured compaction, enforced verify, stuck detector, truthful status — the exact answer to competitors' loudest complaints); footprint economics (10 MB / 4 ms / 9 MB idle vs. "sluggish" as both leaders' top complaint); the per-call ledger as routing substrate.

**Clearly behind:** channels (2 vs 13–20+); extension economy (no plugins, no registry, no hot-add — adding an MCP server today means hand-editing TOML and restarting); self-improvement (Hermes' learning loop and OpenClaw's dreaming are their signature features; ferrule has the substrates — memory, skills, ledger, scheduler — but no loop); multi-agent (all three ship it; M12 is designed, unbuilt); memory sophistication (no update/delete, no hygiene, recall never sees the session goal); execution surfaces (local only; Hermes runs shell on SSH/serverless); UI (CLI only).

---

## 4. Genuinely new work — NOT in PLAN.md's roadmap

Prioritized by pillar impact ÷ effort. Effort calibrated to the M1–M10 pace. Sources: investigator reports (§Method); key evidence cited inline.

### Tier A — the "smart and dynamic" core the owner asked for

1. **The learning loop (offline consolidation + ACE-style playbook).** The signature feature of Hermes (background review fork) and OpenClaw (dreaming), independently shipped as Codex Memories and Letta's dreaming — convergent evidence this is becoming table stakes. Ferrule version: a scheduled background pass (the M3 scheduler makes this config-free) that reviews recent sessions, extracts durable facts, dedupes (Mem0-style ADD/UPDATE/DELETE, +26% over OpenAI memory on LOCOMO, −91% p95, arXiv:2504.19413), and curates a **playbook** — delta bullets appended to the system prompt, proposed by a Reflector pass after failed/retried runs, additions gated on `verify_command` success (ACE: +10.6% on agent tasks, arXiv:2510.04618; DGM proves harness-level self-improvement is the near-term surface: SWE-bench 20→50% with frozen weights, arXiv:2505.22954). Every substrate exists: memory store, verifier, scheduler, ledger, cheap-model routing seam. **Medium–Large.** This is the one big thing missing from pillar 3; M13 (self-*extension*) is a different feature and does not cover it.
2. **`ferrule eval` — the measurement primitive.** Task suites (YAML: prompt, workspace fixture, grader = verify_command and/or LLM rubric) run through the real `Agent`, results into `ledger.jsonl` with `task_shape="eval"`. Makes "smartest harness" a measurable claim instead of a slogan, turns every future prompt/profile/threshold change into a regression-tested one (Anthropic's own eval methodology: ~20 real cases suffice; single LLM-judge with rubric is most human-aligned), and unlocks offline tuning of tool descriptions (Anthropic measured 40% task-time reduction from rewritten MCP tool descriptions) and profiles. Nothing in-repo covers this. **Medium–Large.**
3. **Reversible compaction — `search_history` tool.** Today compaction is one-way: the summary is all that survives. RLM research (MIT, arXiv:2512.24601) shows treating history as an environment to query beats ingesting it. Ferrule's history is already a JSONL file on disk: a read-only grep-over-transcript tool makes compaction lossless, directly counters measured context rot (Chroma 18-model study), and costs almost nothing. Also: truncate old large tool results (not just exact-dup dedupe), and run dedupe at push time, not only at 70%. **Small–Medium.**
4. **Memory update pipeline.** `remember` only inserts; `forget(id)` exists but is exposed to no tool; recall at session start ignores the goal (one-line fix in `main.rs:579` — pass the first user message as the query). Add: update/delete tools, a `superseded_by` column (80% of Zep's bi-temporal win, cheap), goal-driven session-start recall, per-turn re-recall on long sessions. **Small–Medium** (vector/hybrid recall stays planned as-is).

### Tier B — pillar 2: extension without friction

5. **MCP hot-add + `ferrule mcp add` + wizard MCP step.** Today: hand-edit `config.toml`, restart the daemon; the agent can't self-serve at three independent layers (can't write config, no hot reload, no tool). The wizard never mentions MCP. Build: `ferrule mcp add <name>` (guided, live `tools/list` smoke test, writes config via `toml_edit` — dep already present, binds secrets to hosts, re-runs doctor), hot registration into new sessions (AgentFactory rebuild makes this natural), `tools/list_changed` handling. This is most of M13 minus the allow-list UX. **Medium.**
6. **Lifecycle hooks.** Claude Code and Codex converge on exactly this: events (SessionStart, PreToolUse, PostToolUse, Stop, PreCompact, SubagentStop) + handlers where exit-2 blocks and feeds stderr back to the model, `additionalContext` injection. It turns ferrule's built-ins (verify_command, gate scripts) into user-land features and is the extension point teams actually automate on. **Medium.**
7. **MCP content beyond text + per-tool controls.** MCP client currently drops images/resources — a hard blocker for M11 (browser screenshots). Add per-server `enabled_tools`/`disabled_tools`, per-tool output token caps (users wiring 100-tool servers need filtering to protect context). **Small–Medium.**
8. **Skill/MCP vetting story for M13.** ClawHavoc (12–36% of registry uploads flagged risky across audits) made open registries radioactive. Ferrule's differentiator: "install anything, safely" — a `skills audit` / tool-description poisoning scan (MCPTox: 36.5% avg attack success is the quantified reason) before activation, version pinning, curated allow-list. Write this into the M13 spec *before* building; PLAN.md's one-line M13 doesn't restate it. **Medium.**
9. **Web search tool.** `web_fetch` without search forces the agent to guess URLs; every competitor ships both. Provider-agnostic `web_search` (Brave/Tavily/self-hosted), proxied like `web_fetch`. **Small.**
10. **Keyword-triggered skills.** Optional `triggers:` frontmatter — deterministic activation when the prompt matches, instead of hoping the model reads the catalog (OpenHands microagents pattern). **Small.**

### Tier C — pillar 1: trust, cost, polish

11. **Hard budget caps with a kill switch.** Cost control is the #1 adoption driver and ferrule has measurement without enforcement. Per-run/per-day/per-task token & dollar caps, surfaced in the wizard, Telegram warning at 80%. **Small.**
12. **Approval gates for destructive/irreversible actions.** The sandbox confines *where* the agent writes, not *what it destroys* inside the workspace or on bound hosts. A Telegram approve/deny round-trip for a curated dangerous-action set (rm -rf, force-push, `gh` deletions, DB mutations via bound credentials) answers the Replit/Claude-Code deletion stories that made buyers ask. Later: granular approval categories + a reviewer subagent (Codex `auto_review` pattern). **Medium.**
13. **Plan mode.** Read-only exploration → plan → user approval → execute. Three of six competitors ship it; ferrule already has the `read-only` sandbox mode as the enforcement primitive. **Medium.**
14. **Edit mechanics (Aider's playbook).** SEARCH/REPLACE edit tool (whole-file `write_file` is the most failure-prone call for mid-tier models); a repo map — tree-sitter symbols + dependency-graph ranking as an always-on ~1k-token context slice (push, not pull — different from the planned tree-sitter *search tool*); optional per-edit lint feedback (expressible as a PostToolUse hook once #6 lands); optional atomic auto-commits for reviewable unattended runs. **Small each, Medium for repo map.**
15. **Local-model first-run polish.** The community's #1 local-harness failure is context length ("raise to 64k before blaming the harness"). Wizard auto-detects Ollama, warns on small windows, auto-picks the profile and compaction threshold for the window size, suggests known-good local models. ZeroClaw owns this narrative today. **Small–Medium.**
16. **Migration importers.** Hermes' `claw migrate` and OpenClaw's memory import treat competitor-switching as onboarding. `ferrule setup` detecting `~/.openclaw`/`~/.hermes` (import MEMORY/USER.md into memory, skills are the same format, allowed-chat lists) is cheap goodwill. **Small–Medium.**

### Tier D — later, but on the radar

17. **Egress domain policy** in the existing proxy (per-domain allow/deny, unix-socket allowlist — closes the documented `docker.sock` escape). **Medium.** 18. **OTel exporter** from the ledger seam (industry-standard shape; MCP tool chains are the cited blind spot). **Small.** 19. **Recipes** (parameterized YAML workflows: instructions + toolset + entry prompt — how non-experts reuse an agent). **Medium.** 20. **Pinned, agent-editable memory block** with visible char budget (Letta pattern; later shared between M12 agents). **Medium.** 21. **SSH execution backend** ("agent on my VPS, me on Telegram" — the workflow all three communities call the magic moment). **Medium–Large.** 22. **Read-only web dashboard** (sessions, task runs, ledger, doctor). **Medium–Large.** 23. **Non-native tool-calling emulation** for models without function calling (expands the local-model story; defer until demand proven). **Medium.**

## 5. Planned work that needs sharpening or resequencing

- **Pull forward: parallel read-only tool calls** (the only serialized point is the loop itself; `readOnlyHint` is already parsed — Small) and **provider streaming + Telegram progressive edits** (the gateway drops the event receiver today; channel `edit()` already exists — Medium). These are the two most *felt* speed gaps, measurable with the ledger.
- **M12 design deltas** (fold in before building): subagent summary contract (~1–2k tokens distilled), effort-scaling rules in the spawn tool description, a verifier-subagent role, routing-by-role (planner strong / workers cheap — third independent corroboration of the pattern), `resume_agent`/`wait_agent`/`close_agent` as first-class tools, a task list with dependency edges + self-claiming, and Claude Code's rule that **agent-relayed approvals are untrusted input**. The 15× token multiplier of multi-agent belongs in the budget limits and docs.
- **Routing Phase 1 deltas:** Goose's failure classes — user correction counts as escalation-worthy failure, technical errors don't; automatic hand-back to the cheap tier after N good turns. Per-session first (protects prompt cache); the HarnessProfile/compaction coupling makes per-call routing a later problem.
- **Channels:** sequence Discord → Slack → WhatsApp ahead of several "Next" items; the `Channel` trait seam (~400 lines per adapter, Telegram as reference) makes each a Medium. Channel breadth is OpenClaw's durable moat and Hermes' #3 complaint.
- **Cheap loop fixes:** finish the stuck detector (2 of 5 OpenHands signatures missing: monologue, repeated context-window errors); gateway lane idle eviction (the daemon's one quiet leak); ledger rotation; a systematic tool-error pass against Anthropic's "Writing effective tools for agents" (every error says what to try next); deferred MCP tool schemas for many-server users.
- **M13 spec:** add the vetting story (§4.8) and `tools/list_changed` re-scan (the rug-pull hook).

## 6. What to think about — decisions for Max

1. **Go public.** Every competitor's worst press is a trust failure (OpenClaw's CVEs/ClawHavoc, Hermes' issue-edit incident, NanoClaw's CVE). Ferrule's story — sandbox on by default, placeholder credentials, truthful status, 232 tests, honest docs — only recruits users if it's visible. This is a decision, not engineering, and it gates everything else.
2. **Curated, not open.** ClawHub's 12–36% malicious rate means an open registry is an anti-feature for ferrule's brand. "Curated + sandboxed + credential-bound by default" is the extension story TypeScript competitors cannot copy cheaply. Position M13 as "install anything, safely."
3. **Positioning.** Don't fight Claude Code on model quality — the README's harness-is-the-lever thesis (13.3% → 38.3%) is the counter. Own: safe by default, cheapest to run, runs anywhere, never lies about status. Watch ZeroClaw — the direct Rust-niche competitor.
4. **Launch conduct.** The Hermes astroturfing backlash shows this audience audits for fake momentum. Honest docs (already a strength), fast issue responses, no seeding.
5. **Sequencing proposal:** the learning loop (#4.1) and `ferrule eval` (#4.2) are the two big new bets; everything in Tier B is pillar-2 plumbing that compounds with M13; Tier C items are small enough to interleave. A possible order: eval → memory update pipeline + reversible compaction → learning loop → MCP hot-add → hooks → M12 with the deltas. Channels and streaming in parallel as smaller milestones.

## 7. Do NOT build (evidence-backed)

- **Learned routers / LoRA fine-tunes** — already dropped (msg 3070); GEPA-on-prompts gets most of the benefit with far less machinery.
- **Container-per-conversation** — research-deployment recommends against; NanoClaw pays the Docker tax for it.
- **DGM-style self-modifying code** — contradicts the sandbox/allow-list security story; the playbook (§4.1) is the sandbox-compatible form of self-improvement.
- **Open public registry** — ClawHavoc.
- **Temporal knowledge graphs** — over-engineered for single-user; `superseded_by` captures 80%.
- **Raising the compaction threshold** — context-rot evidence cuts the other way.
- **Downloading a browser in setup** — owner decision stands (detect-only, M11).

## 8. Evidence flags (unverified or secondary)

ClawHub skill counts (5.7k→67k range across secondary sources); "138+ CVEs" (single secondary, though the arXiv taxonomy and named CVEs corroborate severity); Hermes "wiped my secondary drive" (Reddit via aggregator); Uber budget-burn story (rumor-tier); ZDNet 75%-prefer-Claude-Code survey (LinkedIn only); Zep/Mem0 LongMemEval numbers (vendor-adjacent); sleep-time compute's exact 5×; SICA 17→53%; GEPA "ICLR oral" status; Reddit quotes via kilo.ai aggregation (reddit.com blocked direct fetch). NanoClaw's public description is contradictory across sources — trust the first-hand notes in PLAN.md. All other quantitative claims cite primary sources fetched 2026-09-24.
