# Research: what to improve next (round 2)

**Date:** 2026-09-28 · **Method:** six parallel investigations — PLAN.md/roadmap mining, codebase health sweep, fresh competitive scan, eval-data analysis, adoption/growth research, new-user friction audit. Repo state: v0.9.0, M1–M38 shipped, public, ~1,435 tests.

This is the successor to `research-number-one-harness-strategy.md` — whose queue is now built (M14–M34) or moot. Round 2 answers "what else, and how" at v0.9.0.

---

## 0. Fixed while writing this

- **The README/chart verify-task figure was wrong**: the saved run (`run.json`, 2026-09-25) shows naive 3/12 (25%), not 1/12 (8%). Overall 57/47 was exact. Corrected in README + `eval-ab.svg` (commit `d51e8fb`). Lesson recorded: public numbers need the run id cited next to them.

## 1. Credibility blockers (fix before any launch — mostly free)

1. **No LICENSE file** — the repo is all-rights-reserved right now; `ferrule-plugin-sdk` even carries a comment about it. MIT OR Apache-2.0 (Rust convention; ZeroClaw uses dual). Existential for adoption. *Small.*
2. **Planning docs have drifted from reality** — `docs/roadmap.md` "Other open tracks" still lists Phase-1 routing as blocked and Windows as unsandboxed (both shipped, M25/M26); its "Done" table stops at M12; PLAN.md Current State still says the repo is private and several milestones "unverified until the batch CI pass" (which happened, run 36071977256). Anyone mining these files for "what's left" gets misled. One correction pass. *Small.*
3. **Stale "no OS sandbox on Windows"** strings in `install.ps1:10-12` and `setup.rs:2351` (M8 vintage) directly contradict the M26 reality and the README. *Small.*
4. **The unverified-live cluster**: M36 self-update chain never observed installing/rolling back a signed release; M35 ChatGPT/Claude plan sign-in never run end-to-end; M23 native drivers never run against the real APIs; M25 routing never run on a real cheap/strong pair; M31 Discord/Slack never connected to real workspaces; M20/M37 connections never logged in for real. "Built" is documented; "proven" is not. A sweep of these needs the owner's accounts and ~a day. *Medium, mostly owner-gated.*
5. **GitHub metadata**: no topics, no social preview (hero.png unused there), no SECURITY.md, no CONTRIBUTING.md, Discussions off, zero issues. All 15-minute fixes with outsized conversion effect. *Small.*
6. **`ferrule` on crates.io is taken** by an unrelated database CLI — decide: publish as `ferrule-cli`, or document source-install-only. *Small decision.*

## 2. Product gaps that are now the top of the list

1. **MCP image/content parts → files.** Ferrule drops non-text MCP content today (`crates/ferrule-mcp/src/client.rs:587`); Claude Code 2.1.283 (Sept 25) now saves MCP images to files for Read/Bash. This blocks the M11 browser's screenshots — the single most-wanted content type. *Medium.*
2. **Voice notes in channels** (transcribe inbound voice via the configured provider; text replies first). Codex made voice default-on (0.156.0), Goose/OpenClaw/Hermes ship it. The most *felt* consumer gap. *Medium.*
3. **Text-tool-call repair.** The eval data is damning: 13 of 120 task runs ended on the model emitting `<function=…>` as literal text, and all 13 failed — including costing the smoke run its engineered 4/4. `agent.rs:1114` ends the turn with no salvage. When tools were offered and the content parses as a call, execute it or nudge the model once. *Medium.* (OpenHands' NonNativeToolCallingMixin is the prior art.)
4. **ACP server mode** (Agent Client Protocol) so ferrule embeds in editors — Goose, ZeroClaw, OpenHands, Codex all converge here; ferrule has zero editor surface. Distribution, not features. *Medium–Large.*
5. **Approval-evidence depth** (Codex Guardian parity, 0.158.0): approvals invalidated when new user instructions arrive; review context surviving compaction/restart. Verify ferrule's behavior and close the gap. *Small–Medium.*
6. **Telegram parity**: approval buttons (Discord/Slack have them), >4096-char splitting for non-streamed sends (a long `/plan` output silently fails today). *Small–Medium.*
7. **Model governance config** — `deny` list + exact-version pinning (Claude Code's `deniedModels`/`availableModelsMatch`), surfaced in doctor. *Small.*
8. **Extension trust upgrades**: a channel approver (the `Approver` trait exists — today only TTY), an update path (reinstall-only today), provenance/signature checks. *Medium each.*
9. **Credential proxy round 2**: HTTP/2 + websockets on bound hosts, body injection for OAuth `client_secret` POSTs. Recorded as limits since M7; blocks whole OAuth classes. *Medium–Large.*
10. **Browser hardening batch** (M11 edges): profile dir under the read deny-list (cookies!), `read <url>` honoring proxy flags, raw CDP out of the `all` toolset, file the upstream agent-browser issue. *Small each.*

## 3. Eval suite fixes (the data supports each)

- **git-fix-commit is fully harness-inflicted**: `.ferrule/` pollutes the grader's `git status` (write a `.gitignore` in `git_init`), and the fixture repo inherits the owner's global git config/1Password (isolate with `GIT_CONFIG_GLOBAL` + identity + `commit.gpgsign=false`). *Small.*
- **Progress heartbeat** during task runs (elapsed/steps/tokens every ~60s) — 15-minute silent runs read as dead. *Small.*
- **Checkpoint `run.json` after each task + `--resume`** — two aborted runs (~685 calls) left no record. *Medium.*
- **Context-gap fairness**: window ladder / per-task windows; the five context tasks went 0-vs-0 on a 30B model — the suite's core differentiator showed nothing on that hardware. Rerun on 0.9.0 (the saved run predates native drivers/streaming/parallel tools/edit_file). *Medium.*
- **Surface timeouts in the report table** (`FAIL (timeout)`), not just progress lines. *Small.*
- **Isolate the verify loop's value**: `verify_failures = 0` in all 120 runs — the model self-ran the named check. Add a task whose check is NOT named in the prompt. *Small–Medium.*
- **Run `--variant routing` on a real model pair** — M25's central claim has only mock numbers. *Small (a run).*

## 4. Codebase health (176k lines, 38 milestones in 5 days — consolidate)

Top targets, ranked by risk-reduction per effort (full table in the agent's report):
1. Split `ferrule-cli/src/main.rs` (3,192 lines; 78 of 402 commits touch it — the merge-conflict leader). *Small.*
2. Stale-docs sweep (test counts 1225-vs-1471, Windows sandbox claims, `docs/agents.md` Windows line). *Small.*
3. `doctor.rs` (2,216 lines) has almost no tests — the tool users run when things break. *Medium.*
4. Split `ferrule-core/src/agent.rs` (4,515 lines, well-tested — safe). *Medium.*
5. Shared JSONL append/read helper (re-implemented 6+ times) and shared atomic-write helper (~10 divergent copies, some on secret files). *Small each.*
6. One lock-error convention (515 `.unwrap()` sites vs 74 poison-tolerant ones; `panic=abort` mitigates in release). *Small codemod.*
7. `dashboard/api.rs` (3,162 lines, 76 endpoints) split per resource. *Small–Medium.*
8. `setup.rs` (3,055 lines, 9 tests) + thin M38 instances e2e. *Medium.*

Also: zero TODO/FIXME and zero `#[allow(dead_code)]` anywhere — the base is healthy; this is consolidation, not rescue.

## 5. New-user friction (ordered by likelihood of losing the user)

1. **macOS/Linux: `ferrule` isn't on PATH when the wizard finishes** — install.sh prints the hint *before* the 12-step wizard buries it; Windows' installer writes PATH for you. Write the rc line (idempotent, opt-out) or re-print after setup. *Small–Medium.*
2. **The 60-second block runs the agent with `$HOME` as workspace** — add `mkdir ~/ferrule-workspace && cd` and a workspace-is-home warning. *Small.*
3. **Config-without-provider error doesn't say "run `ferrule setup`"** — dead end at first `run`. *Small.*
4. **Stale Windows-sandbox strings** (§1.3). *Small.*
5. **Claude plan without Claude Code installed: setup "succeeds" with no model saved** — loop back instead of returning; document the Node.js prerequisite. *Small.*
6. **"Local model" with no server running offers no pointer to Ollama** — print the install link + re-scan. *Small.*
7. **Telegram configured but silent**: setup outro never says "message your bot"; `ferrule gateway` prints one raw log line, no readiness signal; pairing-timeout fallback doesn't print the chat-id hint. *Small–Medium.*
8. **`ferrule mcp add` `--` trap**: the natural invocation fails with a clap error that never says "put `--` before the command"; bad `--url` errors in reqwest's words. *Small.*
9. **`config init` writes live Kimi/OpenAI providers** — the example config should comment them out. *Small.*
10. **"60 seconds" vs a 12-heading wizard** — expectation mismatch; trim guided mode or reframe the copy. *Small.*

## 6. Competitive landscape shift (4 days)

- **ZeroClaw is the correction**: our docs called it "purely a footprint play" — false since v0.8.x: named agents with per-agent workspace/memory/policy (M38's exact shape), a TUI + web dashboard, WASM plugin interfaces for tools/channels/memory, 30+ channels, egress allowlists. It passed NanoClaw in stars (32.9k vs 30.8k). **It, not NanoClaw, is ferrule's direct competitor now — it deserves its own column in vs-field.svg.** Ferrule's remaining edges over it: the credential proxy, truthful-status/verify, budget kill-switch, the eval suite, importers, plan sign-in.
- Caught up this week: Hermes shipped a **credential vault** (v0.21.2) and provider **fallback**; NanoClaw 2.4.0 made credential gateways **pluggable with human approvals**; Codex shipped **Guardian** approval-review, voice-by-default, per-tool output caps; Claude Code shipped MCP-image-to-files and `/doctor prompt-audit`; OpenClaw 2026.9.6 added restart-recovery, 30-day usage reports, and per-release evidence CI.
- Nobody else shipped: default-on kernel sandbox, enforced verify loop, hard budget kill-switch, or a self-run harness benchmark. Ferrule's core wedge holds.
- HN shows no new entrant with traction this week; the theme of the week is **verification-as-a-product** (Canary YC, Microsoft's runtime risk discovery) — validates ferrule's positioning.

## 7. Adoption: the launch-readiness gap

The repo is public at 0 stars. The blockers (§1) come first. Then the playbook (evidence in the agent report): NanoClaw's Show HN anatomy (533 pts — pain + mechanism + verifiable claim), ZeroClaw's growth with *failed* HN posts (search niche + awesome-lists + docs site + README benchmark table), Hermes' astroturfing backlash (never fake momentum), and the NanoClaw "AI-slop audit" lesson — **ferrule's 6-day, 402-commit, agent-built history will be discovered; the README must lead with the build-transparency note and PLAN.md's verified-vs-mocked honesty**. Phase 0: LICENSE, metadata, SECURITY.md, demo GIF (streaming + sandbox denial in motion), first-person origin note. Phase 1: CONTRIBUTING + good-first-issues, Discussions, 5 awesome-list PRs. Phase 2: one coordinated Show HN day (Tue–Thu 8–10am ET) + r/rust + This Week in Rust. Phase 3: the drumbeat — 1–2 measurable technical posts/month mined from the research docs.

## 8. The recommended order

1. Credibility blockers (§1) — a day, mostly free.
2. README provenance discipline: run the full eval on v0.9.0 (also closes §3's rerun) and cite the run id next to the numbers.
3. Text-tool-call repair + MCP image content + eval-suite fixes (§2.3, §2.1, §3) — the measured, user-visible correctness items.
4. The friction top-5 (§5) — cheap, compounds with every future visitor.
5. The unverified-live sweep (§1.4) with the owner's accounts.
6. Then the growth sequence (§7), and the consolidation pass (§4) slotted before the next user-facing milestone.

**Do-not-build (updated):** open plugin registry (still radioactive — both Codex and Claude Code shipped *curated* marketplaces, validating the stance); voice *generation* before voice intake; a mobile app; a Discord community server before there's traffic to route.
