# Research — Harness², the 2026 harness field, and what ferrule is missing

Date: 2026-10-20. Status: research, no code. Sources are cited inline;
every claim about ferrule was checked against the code and docs named.

## 1. Harness² (google-research/harness2)

[Harness²: Recursive Agent Harnessing for an Open
World](https://github.com/google-research/harness2) (arXiv:2610.14224,
Oct 2026) is Google's framework for *recursive self-improvement of the
harness itself*, model weights fixed:

- **Task-level recursion** — a contrastive **Propose–Probe–Compose**
  step: propose edits to the harness, probe each edit by re-running the
  same task, compose a refined harness from the behavioural contrasts —
  **without ground-truth feedback**. Scales two ways: parallel (K
  general + K domain candidates) or sequential (K deepening steps).
- **Harnessing-level recursion** — a persistent *improvement harness*
  keeps the adopted harnesses and evidence-supported lessons from each
  task, guiding future refinement.
- **Eight editable components** around the model: system prompt,
  guardrails, memory, tool guidance, subagents, skills, scripts,
  plugins.
- Results: a single step gives up to +11.5 points on WorkBuddy-Bench,
  +4.5 on JobBench, +11.8 pp partial-pass on LAB, over GLM-5 and Gemini
  3.5 Flash base harnesses.

### What ferrule already has of this

Ferrule has all eight editable components in some form: the layered
context baseline (M42 part 2) is the system prompt; `ferrule-trust` and
the sandbox are guardrails; `ferrule-memory` (FTS5+vector, M15/M30);
tool guidance lives in prompt blocks; sub-agents with worktrees (M12)
and a per-spawn `model` choice (M21); Agent Skills (M5/M28); scripts
via the sandboxed shell; WASM plugins (M32). The learning loop (M16) is
harnessing-level recursion in embryo: failed runs → gated one-line
playbook deltas → re-run must pass twice before a lesson is kept, and
the playbook is a file the owner can read and `ferrule learn revert`.

### What Harness² has that ferrule doesn't

1. **Online, per-task refinement.** M16 is an offline nightly pass over
   *failed* runs. Harness² refines the harness *during* the current
   task, before finishing it. Ferrule's closest seam is `run --verify`
   (M42 part 3): the goal loop retries until the check passes, but it
   never edits its own prompt/tooling mid-run.
2. **Probing without a verifier.** Harness²'s key trick is composing
   from *contrasts between executions* when no ground truth exists.
   Ferrule's gate always needs a check command. A ferrule version would
   be: when a task has no `verify_command`, probe two candidate
   harness edits, run both, and keep the one whose transcript the agent
   itself (a judge call) scores higher — a contrastive judge instead of
   a verifier. This is the piece worth stealing; the rest of Harness²
   (benchmark runners, Vertex plumbing) is eval infrastructure ferrule
   already has in `ferrule eval`.
3. **Explicit component versioning.** Harness² names and snapshots the
   eight components per run, so "which harness produced this run" is a
   data question. Ferrule snapshots config implicitly (transcript meta)
   but a skill edit mid-session is invisible to the ledger.

## 2. The top five harnesses, October 2026

Consensus across current comparisons (Terminal-Bench 2.1, the Pragmatic
Engineer survey, vendor docs): **Claude Code, Codex, Gemini CLI /
Antigravity, OpenCode, Pi** — with OpenClaw as the community
chat-channel outlier ferrule already imports from.

| Harness | What it's best at | What ferrule already matches | What ferrule lacks |
|---|---|---|---|
| **Claude Code** | long autonomous sessions; hooks, subagents, skills | 10-event hooks (M18), sub-agents (M12), skills (M5), per-subagent model (M21) | subagent *effort* control; Slack triggers; cloud scheduling |
| **Codex** (Apache-2.0, Rust) | delegation, cloud tasks, OS sandbox modes, AGENTS.md | OS sandbox on 3 OSes (M6/M26), AGENTS.md layering (M42), remote workspaces over SSH (M34) | cloud-task execution (local-first by design — not a gap to close) |
| **Gemini CLI / Antigravity** | 1M-token context, free tier | streaming, parallel reads, cache discipline (M27) | nothing structural; context size is the provider's, not ours |
| **OpenCode** | provider-agnostic, LSP diagnostics in the loop, build/plan dual agents | any OpenAI-compatible endpoint (M21/M25), plan mode (M19) | LSP diagnostics fed back to the model after edits (`ferrule-codemap` is structural only) |
| **Pi** | radical minimalism: 4 tools, <1k-token prompt, session trees | session tree + fork (M42 part 7) | ferrule's system prompt and tool count are far larger; Pi is the simplicity benchmark to keep quoting |

And the field-wide findings worth internalising:

- **The harness, not the model, is the differentiator** — several leaders
  run the same frontier models; what differs is session management, hook
  depth and graceful failure. Ferrule's bet is correct.
- **Review is the bottleneck, not generation.** Linear's telemetry: agent
  teams tripled weekly PRs and total dev time still rose ~17%. Small
  diffs and reviewer engagement predict merges. Ferrule's atomic commits
  (M29) and approval gates align; an explicit "small diff" norm in
  AGENTS.md templates would too.
- **None of the five has native image/video generation tools.** Media
  generation inside a chat-channel agent is unclaimed territory — and
  ferrule, living in Telegram/Discord/Slack with `send_file`, is better
  placed to take it than any terminal harness.

## 3. The ferrule audit, 2026-10-20

### Code and architecture

24 crates, `make check` green at 1844 tests + 28 ignored (last full
run, M50). The layering is clean: `ferrule-core` (loop, transcript,
compaction), `ferrule-providers` (OpenAI-compatible + responses +
vision), `ferrule-tools`, then gateway/agents/channels/dashboard in
`ferrule-cli`. Invariants hold: compaction never splits a tool call
from its result, secrets never reach the model, sandbox on by default,
truthful statuses, byte-stable prompt prefix.

### Security

Strong relative to the field: Landlock/Seatbelt/restricted-token
sandboxes, the credential-intercepting proxy (M7), trust gates with
"allow for this run" (ferrule-trust), write-only dashboard keys (M50).
Known open edges, unchanged from M10/M26 and the 2026-10-19 review:
HTTP/2 and websockets on bound hosts, body injection, plain HTTP from
`web_fetch` bypassing the proxy, and unconfined MCP servers able to
read the secrets file. **None of these blocks the proposals below** as
long as media endpoints go through the proxy like everything else.

### Dashboard

After M47/M49/M50 it is genuinely good: connect page, one connect panel
per service, MCP-by-address, keys for tools, config editing with undo
(`config_page.rs`: GET hides secrets, save/check/set/undo), approvals
inbox, memory, tasks, backup. **The one hole: you can add a new
`[secrets]` key from the page but cannot rotate an existing one** —
`tool_keys.rs` refuses any name that already exists. Rotating a leaked
OpenRouter key still means SSHing in to edit `private/secrets.env`.

### Abilities — the real gap: media generation

Ferrule *sees* photos (M47, `vision.rs`, model allow-list, byte-sniffed,
3.5 MB cap) and *sends* files back (Telegram photo/document since
2026-10-19). It **cannot create or edit images or video**. Meanwhile
OpenRouter — already a first-class ferrule provider — shipped both:

- **Image API** (June 2026): `/api/v1/images/generations`,
  OpenAI-compatible, sync, 30+ models, capability discovery
  (text-to-image, image-to-image editing, reference images, aspect
  ratios). Also via chat completions with an image-output modality.
- **Video API** (April 2026): `/api/v1/videos`, **async job API** —
  submit prompt → job ID → poll → download MP4. Veo 3.1, Seedance 2.0,
  Wan 2.7, Sora 2 Pro, with normalised parameters (resolution,
  duration, aspect, audio, `frame_images` for image-to-video,
  `input_references`) and capability discovery.

The async shape is exactly the one the OpenRouter guide itself calls
"the shape of tool an agent can drive unattended — submit, persist the
ID, check back, deliver." Ferrule has every piece of that: the M3
scheduler for the check-back, `send_file` for the delivery, the proxy
for the key.

### Sub-agents with another model: already built

The "open another agent that takes another model from OpenRouter" ask
is M21: `spawn_agent` takes a `model` argument, checked against
`[agents].allowed_models` by the supervisor, run under the same sandbox
and proxy. What is missing is only (a) media-capable models being
useful once spawned (needs §4.1), and (b) effort/cost guidance — the
spawn prompt already carries effort-scaling rules from M12.

## 4. Proposals, in priority order

### M51 — media generation tools (the headline)

Two tools in `ferrule-tools`, provider calls through `ferrule-providers`
and the credential proxy:

1. **`generate_image {prompt, model?, aspect?, reference_images?}`** —
   sync call to OpenRouter `/images/generations` (or chat-completions
   image modality), saves into the workspace, returns the path. With
   `reference_images` it is an editor. Bound to the session's chat like
   `send_file`, so "make me a logo" in Telegram answers with the image.
2. **`generate_video {prompt, model?, duration?, aspect?,
   first_frame?}`** — submits to `/videos`, persists the job ID, and
   registers a one-shot M3 task that polls and delivers the MP4 via
   `send_file` when done. The agent answers immediately ("working on
   it, job `…`"); the video arrives minutes later. No turn hangs.

Honesty rules, per our invariants: the tool's doc states cost per call
(video is dollars, not cents), a `[media]` config table caps
spend/day through the existing ledger (`call_kind = "media"`), and the
trust layer gates the first call per run like any spend. Capability
discovery drives the dashboard's model picker instead of a hardcoded
list.

### M52 — the dashboard Safe

Extend the M50 keys panel into a **Safe** page: list `[secrets]` names
and hosts (already exists), add (exists), **rotate** (new: replace the
value of an existing name — same write-only rule, the old value never
displayed), remove (exists). One confirm card per change, undo for the
config side via the existing `config_page` undo. This closes "edit the
vault without SSH".

### M53 — contrastive probing, borrowed from Harness²

Scoped small, in the M16 spine rather than a new subsystem: when a
learning-pass episode has no verify command, allow **one** probe —
apply the candidate lesson, re-run the task, and let a judge call
compare the two transcripts (rubric: did it finish, fewer retries,
truthful status). Keep the delta only if the judge prefers it. This is
Propose–Probe–Compose with K=1, sequential, no benchmark infra. Also:
record the active skill list + playbook hash in each transcript's meta,
so "which harness produced this run" becomes answerable from the
ledger.

### Smaller, cheap

- **LSP diagnostics into the edit loop** (OpenCode's edge): after a
  SEARCH/REPLACE edit, run the workspace's language server (if any) and
  append new diagnostics to the tool result. M29 already runs a
  per-edit lint; this generalises it.
- **Subagent effort control**: `spawn_agent` gains `effort =
  low|medium|high`, mapped to the provider's reasoning setting.
- **"Small diffs" line** in the default AGENTS.md template, from the
  review-bottleneck finding.
- **WhatsApp** remains the missing channel (M31 §9).

### Deliberately not proposed

Cloud task execution (against local-first design), Harness²'s full
benchmark apparatus (`ferrule eval` covers the need), automatic
playbook *rewrites* (ACE's context-collapse finding — deltas only, M16
already right), image understanding services beyond what chat models
do natively (OCR etc. — MCP servers cover it).

## 5. Synchronisation note

PLAN.md is the sync surface: this file is linked from the README's
Documentation section, and the 2026-10-20 PLAN entry points here. If
another session ships M51/M52/M53 under a different name, it should
edit this file's proposals in place rather than fork the design.
