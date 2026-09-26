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
  `maximarhipkin/ferrule`. Crates are `ferrule-{core,providers,tools,memory,gateway,mcp,skills,sandbox,proxy,cli}`,
  binary is `ferrule`, config file is `ferrule.toml` /
  `~/.config/ferrule/config.toml`, agent workspace state dir is `.ferrule/`.
- **What it is today:** a local, single-user CLI agent runtime. `ferrule run`
  / `ferrule chat` drive a ReAct loop (`ferrule-core::Agent`) against any
  OpenAI-chat-completions-compatible endpoint (`ferrule-providers`), with a
  fixed small toolset (fs read/write/list, a sandboxed shell, web fetch,
  todo/diary, remember/recall) from `ferrule-tools`, and a single-file SQLite memory store
  (`ferrule-memory`, FTS5 BM25 + time-decay recall), plus any tools exposed
  by MCP servers (`ferrule-mcp`, M4; by URL too since M10). Since M12
  (`ferrule-agents`) an agent can start background sub-agents with their
  own worktrees, a board and a task list — see `docs/agents.md`.
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
  [--since 7d]` summarizes it. **Agent Skills exist** (Session Log
  2026-09-24, M5, `ferrule-skills`): SKILL.md folders in the
  agentskills.io / Claude format are discovered from the workspace
  (`.ferrule|.agents|.claude/skills`) and user dirs (`~/.agents/skills`,
  `~/.claude/skills`, `[skills].paths`), their names and descriptions go
  into the system prompt, and the model loads one with `activate_skill`
  and reads its bundled files with `read_skill_file`. Compaction carries
  activated skill instructions forward verbatim. `ferrule skills` lists
  what a workspace would load. **Shell commands run in an OS sandbox**
  (Session Log 2026-09-24, M6, `ferrule-sandbox`): Landlock (+ a seccomp
  filter when `network = false`) on Linux, Seatbelt on macOS. By default
  writes are confined to the workspace and temp dirs, the network is on,
  and secret-looking env vars never reach the command. Configured by
  `[sandbox]`, checked by `ferrule sandbox`. Memory writes moved from the
  `ferrule memory` CLI to native `remember`/`recall` tools, since the
  shell can no longer write ferrule's data dir. **A credential gateway
  exists** (Session Log 2026-09-24, M7, `ferrule-proxy`): each `[secrets]`
  entry maps an env var to the hosts allowed to receive it. Shell commands
  get a same-shaped placeholder in that variable, and a loopback
  TLS-intercepting proxy swaps the real value in only on requests to
  those hosts, only in `Authorization` or credential-named headers (the
  URL too with `in_url = true`, never bodies), and scrubs it back out of
  their responses. Other hosts get a blind tunnel. Design, threat model,
  prior art and limits: `docs/research-credential-gateway.md`. **Install
  and setup are one step** (Session Log 2026-09-24, M8): `curl … install.sh
  | sh` (Linux/macOS) or `irm … install.ps1 | iex` (Windows) downloads a
  release archive, checks its sha256 and starts `ferrule setup`, a wizard
  for the provider and key, Telegram (with a chat allow-list), tool
  credentials, the sandbox and a systemd/launchd service. Keys live in
  `<data_dir>/private/secrets.env`, hidden from the shell sandbox and the
  file tools. `ferrule doctor` checks everything, `ferrule config
  path|edit|example` covers hand edits. **Windows builds and runs**, with
  no OS sandbox (Git Bash or PowerShell as the shell). CI
  (`.github/workflows/ci.yml`) tests Linux, macOS and Windows, all three
  green since `cee5de1`;
  `release.yml` builds 5 targets on a `v*` tag. **`v0.1.0` is released**
  (2026-09-24, msg 3074) and the Linux one-liner was run for real against
  it while the repo was public. **The repo is private again** (msg 3076,
  "for now"), so installing needs `GITHUB_TOKEN` (README) and CI minutes
  count.
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
  --workspace` 180/180 green after M8; 232 passed, 2 ignored after M10** (plus 2 `#[ignore]`d: the proxy's
  live end-to-end test, run with `-- --ignored`, and a sandbox helper),
  `tests_e2e/setup_wizard.py` and `tests_e2e/hidden_keys.py` pass, and
  `cargo check --target x86_64-pc-windows-gnu --workspace --all-targets`
  is clean (C code via `zig cc`; a Windows *link* isn't possible here, CI
  does it). Earlier M7 counts: 27 tests in `ferrule-proxy`, 23 unit + 4
  integration,
  `cargo clippy --workspace
  --all-targets` has one pre-existing warning in `ferrule-core::agent`
  (collapsible_if, predates the gateway work) and zero warnings in
  `ferrule-gateway`/`ferrule-mcp`/`ferrule-skills`/the ledger. `rustup
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
  stdio MCP client (M4), Agent Skills (M5), but still no plugin
  (code-extension) system, no multi-provider
  routing (Phase 1 of `docs/research-routing-and-local-models.md`, blocked
  on Max's decisions there; Phases 2–3, the learned router and local LoRA,
  were dropped by Max on 2026-09-24, msg 3070). These are the largest deltas. (The
  cost/observability ledger gap closed 2026-09-24, Phase 0; the skills
  half of "skills/plugin system" closed the same day, M5; OS sandboxing
  for the shell tool closed the same day too, M6; M10 put MCP servers
  under it. Reads are its open edge; the macOS backend passed on a real
  Mac in CI. The credential-injection gateway closed the same day, M7;
  M10 routed `web_fetch` and MCP servers by URL through it. Its open
  edges are HTTP/2 and websockets on bound hosts, body injection, plain
  HTTP (never proxied) and a real-Mac run. The Telegram sender
  allow-list closed with M8. Native Windows has no sandbox backend;
  AppContainer or a restricted token is the research item.)
- **M9 never stuck is done** (2026-09-24, Session Log): transient provider
  errors are retried with backoff, a loop gets one warning and then the
  run stops, every stop (step limit, loop, a check that keeps failing)
  ends with a status answer instead of an error, `verify_command` is run
  by ferrule itself, and compaction keeps the request verbatim.
- **M10 is done** (2026-09-24, Session Log): MCP stdio servers run under
  the shell's `Sandbox` — the workspace as cwd and writable, a state dir
  of their own (HOME/XDG/npm/uv caches and TMPDIR moved there when
  confined), network always on, `sandbox = false` as a flagged opt-out.
  `web_fetch` and MCP servers by URL (new Streamable HTTP transport, `url`
  + `headers` with `${VAR}`) send HTTPS through the credential proxy when
  it runs. As root on Linux, `ferrule setup` installs a **system** unit
  run as a `ferrule` system user (no login, no sudo) under
  `NoNewPrivileges`/`ProtectSystem=strict`/`ProtectHome`/`PrivateTmp`,
  config in `/etc/ferrule`, data and workspace in `/var/lib/ferrule`;
  `--system`/`--user` choose explicitly, and a user unit as root needs a
  confirmation. `ferrule doctor` also reports the proxy, finds an
  installed Chrome/Chromium and tries a headless launch (never
  downloads), notes Ubuntu's AppArmor userns restriction and warns when
  the service still runs a binary that was upgraded since it started.
  **Open edges:** the system-service path has no e2e run (it needs root
  and systemd; unit text and decisions are unit-tested only); plain HTTP
  from `web_fetch` goes direct, not through the proxy (it only does
  CONNECT); an unconfined MCP server (`sandbox = false`, or any server on
  Windows) can still read the saved keys and `/proc/<ppid>/environ`;
  Streamable HTTP has no server-initiated GET stream or resumption; the
  Chrome launch check isn't exercised on Windows or macOS in CI.
  **Known limits:** `--system` is Linux-only; a system service can't use
  paths under `/home`, `/root` or `/run/user` (`ProtectHome`); binary
  drift is judged from `ps` elapsed time and file mtime (±2 s), plus
  `/proc/<pid>/exe` on Linux. Part 3 of the brief was dropped by Max
  (msg 3088).
- **Next milestones** (M11–M13 order approved 2026-09-24, msg 3090; M14–M19
  adopted 2026-09-24 from `docs/research-number-one-harness-strategy.md` —
  the six-investigation synthesis; the reader-facing version with scope,
  security model and done criteria is `docs/roadmap.md`):
  - **M11 browser**: **done** (2026-09-24, parts 1–3): agent-browser's
    MCP server driving an installed Chrome, in the sandbox, behind the
    credential proxy — `crates/ferrule-mcp/src/browser.rs`,
    `docs/browser.md`. CI drives a real headless Chrome through the MCP
    client on ubuntu-24.04, macos-14 and windows-latest. **Open edges:**
    tool results stay text-only — image and other non-text MCP content is
    replaced by a note naming it, not passed to the model (needs a
    provider-wide `ToolOutput` change); Windows runs the browser
    unconfined (no sandbox backend) and depends on a warm-up
    (`agent-browser get url` before each call) that works around
    agent-browser 0.38.1's daemon inheriting the CLI's stdout pipe — no
    upstream issue filed yet; macOS Seatbelt opens Chrome's desktop
    services and Chrome's own sandbox is off under it (`--no-sandbox`);
    on Linux `/proc` stays writable to Chrome; the shell can read the
    browser profile; `read` with a URL bypasses Chrome's proxy flags;
    the `all` tool set exposes raw CDP.
  - **M12 multi-agent**: **parts 1–5 shipped** (2026-09-24, branch
    `m12-multi-agent`, PR #1; see the M12 session-log entry); part 6
    (named long-lived agents) deferred on an open question. Original
    scope: an in-process `spawn_agent` tool (background by
    default, notifies the parent when done, resumable); named long-lived
    agents from one config line or a Telegram command; a shared board
    whose entries are tagged with the source agent and an untrusted-origin
    flag and treated as data, never instructions, plus direct
    agent-to-agent messages; an automatic git worktree and branch when a
    child works on a repo; limits on depth, concurrency and budget per
    agent; children run under the same sandbox and credential proxy.
    **Design deltas from the strategy research (§5):** a summary contract
    on child results (~1–2k tokens), effort-scaling rules in the spawn
    tool description, a verifier-subagent role, routing-by-role (planner
    strong / workers cheap), `resume_agent`/`wait_agent`/`close_agent` as
    first-class tools, a task list with dependency edges and
    self-claiming, and agent-relayed approvals treated as untrusted input.
  - **M13 self-extension** (was M12): **built** (2026-09-24, parts 1–7 on
    branch `m13-self-extension`, merged to main as PR #2; CI deferred to
    the roadmap batch): skills and MCP servers hot-loaded mid-session,
    self-installed from an **allow-list of approved sources** without
    asking (msg 3074), anything else queued for the owner
    (`ferrule extensions pending|approve|deny`) — **with the vetting
    story (§4.8):** a tool-description poisoning scan before activation
    (MCPTox: 36.5% average attack success), exact version / commit pins
    plus surface digests, and a `tools/list_changed` re-scan. New crate
    `crates/ferrule-extensions`; design `docs/m13-self-extension.md`.
    Off by default (`[extensions] enabled = false`). **Open edges:** see
    the 2026-09-24 M13 Session Log entry — the macOS/Windows-sensitive
    spots there are **unverified on macOS/Windows until the batch CI
    pass**; no channel (Telegram) approver; the gateway's skill roots are
    fixed at startup; the approval gate is only as strong as the sandbox
    (off, or Windows); the scan is heuristic; updates are manual only.
    **With M12:** sub-agents never get the extension tools — only the
    root installs; children use installed tools narrowed by role.
  - **M14 `ferrule eval`**: **parts 1–5 done** (2026-09-24, branch
    `m14-eval`; see the M14 session-log entry). It was scoped in §4.2:
    task suites (a prompt, a workspace fixture, a verify_command and/or
    LLM-rubric grader) run through the real agent, with results written
    to the ledger with `task_shape="eval"`. Every harness change becomes
    regression-testable. `ferrule eval run` runs a suite. It does a
    naive/engineered A/B on one model, from any provider, and prints the
    diff against the last run. `ferrule eval report` re-prints a saved
    run. There is a 20-task starter suite with a stdlib mock model.
    `docs/eval.md` is the user doc; `docs/m14-eval.md` is the design.
    **Not done:** the measured chart in the README. It needs a real
    model run, and the README was off-limits for this batch.
    **Next: the real A/B.** Max's other agent runs it, following the
    steps in `docs/eval.md` § "Handoff: running the real A/B": the free
    mock check first, then a dry run, the smoke subset under a small
    cap, and the full suite with `--repeat 3`.
  - **M15 memory pipeline + reversible compaction** (§4.3/§4.4): **built**
    (2026-09-24, parts 1–3 on branch `m15-memory`, PR to main open, not
    merged; CI deferred to the roadmap batch; see the M15 session-log
    entry). Design: `docs/m15-memory.md`. `remember` answers NOOP for a
    known fact and lists similar live ones; `update_memory` supersedes
    (`superseded_by`, recall maps old wording to the live fact); `forget`
    hard-deletes a chain with no trace left in FTS or the file; the
    session-start memory block is picked from the session's goal. Old
    large tool results are shortened to a preview plus a ref when
    compaction triggers, and `search_history` reads the session's own
    transcript by query or by ref. Root: full memory access; writing
    child: add only; read-only child: recall only. Schema is at
    `user_version` 1, migrated in place from pre-M15 stores. **Open
    edges:** no per-turn re-recall on long sessions (it would break the
    prompt cache); the NOOP/similar heuristics are word-set Jaccard, not
    embeddings; `search_history` is a linear scan of the transcript;
    shortening happens only at the compaction trigger; the eval engineered
    variant gets `search_history` but no session recall (its store is fresh
    per run, so there's nothing to recall); the mock model never calls
    `search_history`, so the eval measures shortening only. **Unverified on
    macOS/Windows until the batch CI pass:** see the M15 session-log entry.
  - **M16 learning loop** (§4.1): **built** (2026-09-25, parts 1–3 on
    branch `m16-learning-loop`, PR to main open, not merged; CI deferred
    to the roadmap batch; see the M16 session-log entry). Design:
    `docs/m16-learning-loop.md`. A pass (`ferrule learn run`, or the
    built-in `ferrule-learn` scheduler task when `[learning] enabled`,
    off by default) reviews failed/incomplete scheduled runs and
    retried/stopped sessions, has a reflector propose one add/edit/retire
    per episode, and keeps an add or edit only when the task's check
    passes twice in a scratch copy. Kept lessons go to
    `<data>/learn/playbook.md` (the owner's lines untouched) and into
    every agent's system prompt after `[Skills]`, sub-agents included,
    read-only. Near-duplicate memories are merged through M15's UPDATE.
    Ledger caps per pass and per day (`call_kind = "learn"`). Every pass
    is a folder of files; `ferrule learn show`/`diff`/`revert`. Eval
    stays hermetic unless a suite sets `owner_playbook = true`. **Open
    edges and the macOS/Windows-unverified list:** see the M16
    session-log entry.
  - **M17 MCP hot-add + `ferrule mcp add`** (§4.5): **parts 1–4 done**
    (2026-09-24, branch `m17-mcp-add`; see the M17 session-log entry).
    Guided add with a live `initialize` + `tools/list` smoke test and
    M13's scan, secrets bound to hosts in the same step, config written
    via `toml_edit`, registration into a running gateway without a
    restart (a 2 s config follower), `enabled_tools` and output caps, a
    setup-wizard MCP step, `ferrule mcp list`/`remove`.
    `docs/m17-mcp-add.md` is the design.
  - **M18 lifecycle hooks** (§4.6): **parts 1–4 done** (2026-09-25,
    branch `m18-hooks`; see the M18 session-log entry). Ten events with
    Claude Code's payload and exit-code contract (0 proceeds, 2 blocks
    with stderr as the reason, anything else is logged and shown to the
    owner only), command hooks with a per-hook timeout and a process-tree
    kill, `additionalContext` appended after the cached prefix (capped),
    `verify_command` as the built-in Stop check with its old behaviour,
    user hooks from the trusted config only, workspace hooks behind
    `[hooks] project` plus `ferrule hooks trust` pinned to the file's
    SHA-256, children inheriting PreToolUse/PostToolUse with
    SubagentStart/Stop in the parent, a JSONL audit log and `ferrule
    hooks list`. Hooks run as the owner, outside the sandbox.
    `docs/m18-hooks.md` is the design. **Open edges:** a `--config` inside
    a writable workspace is trusted; `[sandbox] mode = "off"` lets the
    shell write the trust record; a workspace hook's relative command
    runs from the root's workspace even for a child in a worktree;
    gateway sessions never fire SessionEnd. **Unverified on
    macOS/Windows until the batch CI pass:** the shell choice (`sh -c`,
    Git Bash, or PowerShell; `sh -c` vs `cmd /C`/PowerShell), exit codes
    (2 from PowerShell), killing a timed-out process tree (`killpg` on
    macOS, `taskkill /T /F` on Windows), stdin/stdout piping of the
    payload and output, the trust and audit files' permissions, and the
    `canonicalize`d workspace key; the integration tests are
    `#[cfg(unix)]`.
  - **M19 trust & cost** (§4.11–4.13): **built** (2026-09-25, parts 1–6
    on branch `m19-trust-cost`, PR to main open, not merged; CI deferred
    to the roadmap batch; see the M19 session-log entry). Design:
    `docs/m19-trust-cost.md`. A `Guard` seam in the agent loop
    (`ferrule-core`), the owner's state in a new `ferrule-trust` crate,
    and thin wiring in the CLI (`trust.rs`, `plan.rs`) and the gateway.
    Token and dollar caps per run, per day and per scheduled task, read
    from the ledger; a warning at 80% to the owner's chat; a kill switch
    (`ferrule stop`, `/stop`, `/resume`) that halts calls in flight and
    holds the scheduler; approval gates on `rm -r`, force pushes and
    DELETEs to a bound host (Telegram or the terminal; unattended runs
    refuse); plan mode (`ferrule run --plan`, `ferrule plan`, `/plan`).
    Sub-agents share their root's guard. Eval is hermetic unless a suite
    sets `owner_trust = true`. **Open edges and the macOS/Windows-unverified
    list:** see the M19 session-log entry.
  - **M19b reliability — never silently deaf**: **built** (2026-09-25,
    parts 1–6 on branch `m19b-reliability`, PR to main open, not merged;
    CI deferred to the roadmap batch; see the M19b session-log entry).
    Design and as-built notes: `docs/m19b-reliability.md`. From a phone
    alone: a 👀 receipt on every admitted message (before the lane) and one
    "busy, queued" notice per busy period; `/status` from any allowed chat,
    answered while a turn hangs, and `ferrule status` on the box; a turn
    watchdog (one message to the owner after `watchdog_after_secs` without
    progress) and `max_turn_minutes`; a running marker and a restart notice
    naming the interrupted turn (never re-run); systemd's watchdog
    (`WatchdogSec=120`, pings only while polling works); an optional
    outbound heartbeat. Eval emits none of it (tested). **Open edges and
    the macOS/Windows-unverified list:** see the M19b session-log entry.
  - **M21 — models**: **built** (2026-09-25, parts 1–7 on branch
    `m21-models`, PR to main open, not merged; see the M21 session-log
    entry). Design: `docs/m21-models.md`; user guide: `docs/models.md`.
    Several models connected at once (`provider/model`, a provider, an
    alias or a unique model id; per-model price, window and profile under
    `[providers.X.models."m"]`; old configs unchanged), a `[models]`
    default, fallback and aliases. `models.rs` resolves a ref and picks
    one-off > role > task > chat pin > default per call through a
    `RoutedProvider`; `models/admin.rs` is the one `Models` API (`view()`,
    `set_default`, `pin`/`unpin`, `set_fallback`, `add_model`,
    `remove_model`, `set_alias`, `test`) behind `ferrule model`, Telegram's
    owner-only `/model`, setup, and M22's page later. Pins in
    `<data>/models/pins.json`, a task's model in tasks.db, a sub-agent's in
    agents.db. Fallback off by default, transient failures only, owner told
    once. The served model is in the ledger, caps, the audit log and
    `/status`. `ferrule doctor --ping-models`, `ferrule eval --model`.
    **Decisions for Max and open edges:** see the M21 session-log entry.
  - **M20 connections**: **built** (2026-09-25, parts 1–5 on branch
    `m20-connections`; PR to main open, not merged). Design and as-built
    notes are in `docs/m20-connections.md`; the relay is in
    `relay/worker.js`.
    - The agent can only ask (`connection_request`). The owner taps one
      Telegram button to connect, and `/connect`, `/connections`,
      `/disconnect` and `/decline` work from the owner's chat only.
    - The login code comes back one of three ways:
      - through the owner's own Cloudflare Worker relay (one Durable
        Object per slot, one read, 5 min);
      - else a cloudflared quick tunnel (DCR services only);
      - else a pasted address.
    - Tokens are sealed in `<data>/private/connections/` and refreshed
      per request inside the MCP HTTP transport. The model never sees
      them.
    - API keys come in only through a form that encrypts in the browser,
      or at the terminal.
    - Access is read-only by default. A connected service's tools that
      can change something go through M19's gate.
    - `ferrule connections list|add|remove|catalog|relay deploy|relay
      check`.
    - Max's relay is live at `https://ferrule-relay.maximarhipkin.workers.dev`.
    - **Open edges and the unverified list:** see the M20 session-log
      entry.
  - **M19c — live-bot fixes**: **built and merged** (2026-09-25, PR #15;
    to ship as 0.2.1; see
    the M19c session-log entry). Design, as-built notes, the owner's
    "doesn't answer" checklist and the exact messages:
    `docs/m19c-live-fixes.md`.
    - Log default `warn,<ferrule crates>=info`, with `without_url()` on
      every HTTP error.
    - 409 episodes are told once after `[health] telegram_conflict_secs`
      (60), with recovery, and exposed through `Channel::problem()`.
    - A webhook is deleted at start, keeping pending updates.
    - Captions and non-text messages; ignored chats warned hourly.
    - `CoreError::plain_words()` and `after_attempts()`, and the lane's
      rate-limit countdown.
    - Doctor's webhook, second-gateway (`health::running_gateways`) and
      `:free` checks; the logs command in doctor and status.
    **Decisions for Max:** see the M19c session-log entry.
  - **M22 dashboard**: **built and merged** (2026-09-25, PR #16). Design and as-built
    notes are in `docs/m22-dashboard.md`; the user guide is
    `docs/dashboard.md`.
    - One page, served on 127.0.0.1 only by the gateway (or by `ferrule
      dashboard` when none runs): health, connections, models with an
      OpenRouter catalog and recommendations, usage, tasks, logs,
      extensions and agents.
    - The owner sends `/dashboard` and gets a one-use, 10-minute link. It
      is answered by the gateway itself, ahead of every other door, so it
      works mid-turn, with every model down or the kill switch on. A
      cloudflared quick tunnel opens on demand and closes when idle;
      `/dashboard off` revokes everything.
    - Sessions are cookies (HttpOnly, SameSite=Strict, host-bound, idle
      and absolute timeouts). Every POST needs CSRF, JSON and a matching
      Origin. Destructive operations ask to confirm first.
    - Every change goes through the existing APIs (M21's `Models`, M20's
      `Connections`, M19's hub) or a new one in the same style:
      `Router::stop`, `TasksAdmin`, `models::catalog`. Nothing on it calls
      the model. Every response passes the `Redactor`.
    - `ferrule dashboard [link [--remote] | off]`, `ferrule model catalog
      | recommend | fill-prices`; `doctor` warns on unpriced models.
    - **Decisions for Max and open edges:** see the M22 session-log
      entry.
  - **M23 native drivers**: **built** (2026-09-25, branch `m23-drivers`,
    PR to main open, not merged). Design and as-built notes are in
    `docs/m23-drivers.md`; the user guide is `docs/models.md`, Drivers.
    - `ferrule-providers` has three drivers behind `Provider`: Chat
      (OpenAI-compatible, as before), Anthropic's native Messages API and
      OpenAI's Responses API (stateless: `store: false` and encrypted
      reasoning).
    - `api = "chat" | "anthropic" | "responses"` per provider, inferred
      from `base_url` (only `api.anthropic.com` → anthropic). `thinking`,
      `effort` and `max_tokens` per provider or per model.
    - Provider-native blocks ride on the neutral transcript and are
      replayed only within the loop, to the same api and model; compaction
      and cross-driver fallback drop them. Redacted from logs and never
      shown.
    - Cache writes are counted and priced (`price_cache_write_per_mtok`,
      1.25× input by default on the anthropic api). `CoreError::class()`
      gives M25 its failure classes.
    - **Decisions for Max and open edges:** see the M23 session-log entry.
  - **M24 the dashboard's leftovers**: **built, PR open** (2026-09-25,
    branch `m24-dashboard-2`). Design and as-built notes are in
    `docs/m24-dashboard-2.md`; the user guide is `docs/dashboard.md`.
    - The login survives a restart: sessions are stored as hashes in
      `private/dashboard/sessions.json`, the CSRF token is derived, and
      local sessions are bound to loopback. A live tunnel session gets a
      new link after a restart.
    - Evaluate a candidate from the page or with `ferrule model eval`: an
      estimate first, a confirm, the owner's caps and kill switch, run in
      the background with Cancel, the result next to the default's.
    - Edit from the page, with the same audited ops on Telegram (`/caps`,
      `/mcp`, `/skills`) and the CLI: caps, MCP disable/enable/remove,
      skills, hooks trust pinned to the SHA-256 with a diff, a task's
      schedule and model.
    - `scripts/dashboard-smoke.sh|.ps1`, and an RTL test (`dir="auto"`,
      Hebrew unchanged through the API).
    - **Decisions for Max and open edges:** see the M24 session-log
      entry.
  - **M25 routing, Phase 1**: **built** (2026-09-25, branch `m25-routing`,
    PR to main open, not merged). Design and as-built notes are in
    `docs/m25-routing.md`; the user guide is `docs/routing.md`.
    - `[routing] tiers` (two or more connected models, cheap → strong).
      Every turn starts on the floor and moves up one tier per failure
      signal: a call failure retrying won't fix, 2 invalid tool calls in a
      row, a failed verify check, a Stop hook, the same call 3 times, the
      watchdog, or `/model strong`. Sticky for the turn; back down at the
      next turn by default. Off by default and byte-identical when off.
    - Tier refs (`tier:cheap`, `tier:strong`, `tier:N`) as pins, task,
      role and `spawn_agent` models set the floor; a concrete model isn't
      routed. M21's fallback applies to whichever tier is chosen.
    - Ledger rows carry `route: {tier, escalated}`; every call is priced at
      the model that served it; optional `strong_daily_usd` cap.
    - Surfaces: `ferrule model route [set|off]`, `/model strong|tiers`,
      the dashboard's Routing section and API, doctor, setup.
    - `ferrule eval run <suite> --variant routing --cheap A --strong B`:
      cheap only vs routed vs strong only; `evals/routing/weak_mock.py`
      shows it with no model.
    - **Decisions for Max and open edges:** see the M25 session-log entry.
  - **M26 isolation**: **built** (2026-09-26, branch `m26-isolation`, PR to
    main open, not merged). Design and as-built notes are in
    `docs/m26-isolation.md`; the user guides are `docs/sandbox.md` and
    `docs/windows-sandbox.md`.
    - Sandboxed reads: one deny list (ferrule's secrets, now including
      `<data>/sessions`; `~/.ssh`, the cloud credential dirs and browser
      profiles by default; the owner's `deny_read`, with `allow_read` to
      re-open a default). It is enforced by Landlock, Seatbelt, the Windows
      backend and the file tools.
    - Windows tier 1, no admin: a launcher starts the command under a
      restricted token (write-restricted to per-root capability SIDs,
      Authenticated Users deny-only) inside a job. The job kills the tree,
      caps the process count and, optionally, the memory. Ferrule's secret
      dirs and its own process carry a DACL that shuts the sandbox token
      out. `network = false` isn't enforced there, and doctor says so.
    - Plain-HTTP `web_fetch` goes through the proxy (absolute-form
      forwarding). Secrets go over `http://` to loopback only; a bound
      remote host gets 403.
    - `sandbox = false` MCP servers run hide-only (writes open, the deny
      list and ferrule's process still closed), and doctor lists what stays
      open per server.
    - **Decisions for Max and open edges:** see the M26 session-log entry.
  - **M27 speed**: **built** (2026-09-26, branch `m27-speed`, PR to main
    open, not merged). Design and as-built notes are in
    `docs/m27-speed.md`; the user guide is `docs/speed.md`.
    - Read-only tool calls of one response run in parallel segments
      (`Tool::read_only()`, `[agent] parallel_tools`, default 4); writes
      are barriers, approvals and hooks stay serial, results keep the
      model's order; a stdio MCP server takes one call at a time.
    - `CompletionRequest.stream: Option<DeltaSink>`: all three drivers
      stream only when given a sink, else byte-for-byte as before. The
      agent resets the stream at each call and retry.
    - Telegram replies grow by edits (`StreamingReply` in the gateway,
      `Channel::post`, `GatewayError::RateLimited`); `ferrule chat` prints
      as it comes. `[agent] stream`, `[gateway] telegram_stream`.
    - Recalled memory is a user message after the goal, so the system
      prompt is the same across sessions; the Anthropic previous-turn
      breakpoint is one per wire message; a test pins the prefix bytes.
    - Ledger rows carry an optional `speed` (first token, first reply, tool
      batch); `ferrule ledger` prints cache hit and speed under the table.
    - **Decisions for Max and open edges:** see the M27 session-log entry.
  - **M28 search and skills**: **built** (2026-09-26, branch
    `m28-search-skills`, PR to main open, not merged). Design and as-built
    notes are in `docs/m28-search-skills.md`; the user guides are
    `docs/web-search.md` and `docs/skills.md`.
    - `web_search` (Brave, Tavily, Exa, SearXNG), off by default. The key
      stays in the credential proxy; every search is a ledger row, with
      `[web_search] max_searches_per_day` and an optional price toward the
      dollar caps. `ferrule setup` and `doctor` cover it.
    - Keyword-triggered skills: `triggers:` in SKILL.md. Whole words and
      phrases, Unicode case and accents, Hebrew prefixes, no regex. Only a
      person's message is matched (`Agent::run_user`); the skill goes in
      after it, never into the system prompt. At most 2 a message within
      4000 tokens, re-scanned and lock-checked at every match; project
      skills only with `project_triggers`.
    - Fixes: "failed checks fixed" counts only passing task runs;
      sandboxed commands get `HTTP_PROXY`/`http_proxy`.
    - **Decisions for Max and open edges:** see the M28 session-log entry.
  - **M29 edit mechanics**: **built** (2026-09-26, branch
    `m29-edit-mechanics`, PR to main open, not merged). Design in
    `docs/m29-edit-mechanics.md`; user guide `docs/editing.md`.
    - `edit_file`: SEARCH/REPLACE hunks, all or nothing, exact plus two
      whitespace rungs (never fuzzy on content), errors that show the
      closest region, CRLF/UTF-16/BOM/Latin-1 preserved, atomic write.
      `write_file` stays in every profile (now atomic too); the advice is
      in the tool descriptions. `[agent] edit_file`.
    - New crate `ferrule-codemap`: tree-sitter tags (Rust, Python,
      TS/JS, Go, Java, one feature each), an Aider-style PageRank repo map
      injected through a `TurnContext` seam only when it changes (the
      prefix stays cached), a per-file tag cache, and `code_search`. Only
      in a workspace that looks like a code repo. `[agent] repo_map_tokens`.
    - Per-edit lint as a built-in M18 PostToolUse hook: rustfmt, ruff,
      gofmt, eslint, tsc, each only when installed and configured by the
      project; in the sandbox, with a timeout. `[agent] lint`.
    - Optional auto-commit (`[agent] auto_commit`, off): a `RunObserver`
      seam around `Agent::run` commits exactly the agent's files on a
      `ferrule/auto-*` branch (or the current one), in the sandbox, never
      pushing; `ferrule undo` and `/undo` take it back.
    - `ferrule eval run … --edit-tools both|write-only`.
    - **Decisions for Max and open edges:** see the M29 session-log entry.
  - **M30 vector recall**: **built** (2026-09-26, branch
    `m30-vector-recall`, PR to main open, not merged). Design and as-built
    notes are in `docs/m30-vector-recall.md`; the user guide is
    `docs/memory.md`.
    - `ferrule-embed`: an `Embedder` trait with a fake, an OpenAI-compatible
      `/v1/embeddings` backend (key via the credential proxy, a ledger row
      per request) and a local model2vec backend
      (`potion-multilingual-128M`, pure Rust, no key). The local backend
      reads matrix rows from disk and is behind the default-on
      `local-embed` feature.
    - The weights are never shipped. `ferrule setup` / `ferrule memory
      model download` fetch them only on a yes, at a pinned revision and
      SHA-256.
    - Memory rows carry a vector plus its model id (with dim); models are
      never mixed; `user_version` stays 1. Recall merges BM25 with cosine
      (weighted 0.7, floor 0.3; RRF optional), keeping decay, supersede,
      `forget` and the budget.
    - Any embedder failure gives exactly BM25, silently. `ferrule memory
      reindex` is resumable and paced; doctor shows the state.
    - Benchmark: 80 facts, 48 queries. All-query MRR goes .358 → .684,
      paraphrase r@1 .2 → .6, cross-lingual r@5 0 → .3. Default
      `embedder = "off"`: turning it on is an opt-in download or a paid
      endpoint.
    - **Decisions for Max and open edges:** see the M30 session-log entry.
  - **M31 Discord and Slack**: **built** (2026-09-26, branch
    `m31-discord-slack`, PR to main open, not merged). Design in
    `docs/m31-channels.md`; user guides `docs/discord.md` and
    `docs/slack.md`.
    - Two new adapters behind the `Channel` trait, both outbound
      WebSockets (no public URL): Discord's Gateway v10 (heartbeat, resume,
      close codes, per-route REST buckets) and Slack Socket Mode (ack
      first, `event_id` de-dup, `disconnect`, Markdown → mrkdwn).
    - Allowlists per channel (DMs by user, shared channels by mention
      only), strangers never reach the model; pairing by a six-digit code
      in `ferrule setup` only.
    - The owner generalized to `ChatRef` (`[trust] discord_owner`,
      `slack_owner`, `owner_channel`); Telegram's owner and strings stay
      as they were. Approvals get Allow/Refuse buttons on Discord/Slack.
    - One daemon runs every channel; a dead socket or a rejected token
      shows in `/status`, `ferrule status`, the heartbeat, the watchdog,
      the dashboard's problems and `ferrule doctor` (reads only), and
      stops that channel alone.
    - `ferrule setup` gets Discord and Slack steps (token check, slash
      commands, invite URL, pairing). Text only.
    - **Decisions for Max and open edges:** see the M31 session-log entry.
  - **M33 ops**: **built** (2026-09-26, branch `m33-ops`, PR to main
    open, not merged). Design and as-built notes in `docs/m33-ops.md`;
    user guides `docs/egress.md`, `docs/otel.md`, `docs/migrate.md`.
    - Egress policy in the credential proxy (`[egress]`): public hosts
      allowed, private ranges and cloud metadata blocked, `allow`/`deny`
      host patterns, IPs and CIDRs, `default = "deny"` for an allowlist.
      Resolve once and connect to the checked address (no DNS
      rebinding); configured endpoints (providers, MCP, search, the
      collector) are implicit exceptions. `web_fetch`, `web_search`, MCP
      over HTTP and plugins always go through it; shell commands only
      when secrets or rules exist (advisory). A refusal is a 403 the model
      can read, a ledger row, an audit event, a doctor line and a
      dashboard count.
    - Unix-socket allowlist (`[sandbox] unix_sockets`): closes the
      `docker.sock` escape. Linux: a seccomp user-notification supervisor
      that checks the real inode and connects it itself (no TOCTOU), plus
      sockets the command made (by `SO_PEERCRED`). macOS: Seatbelt path
      rules. Windows: not covered. Where `pidfd_getfd` is refused (Docker's
      default profile) it says "not enforced"; `require = true` fails.
    - `ferrule-otel`: OTLP/HTTP JSON traces from the ledger seam, a
      hand-rolled encoder with no new third-party crates. Session → turn →
      chat / execute_tool spans with GenAI semconv, sub-agents under the
      spawning turn; content off by default and scrubbed when on; bounded
      queue, 512/2 s batches, backoff, 3 s shutdown flush; counters in
      doctor and `/status`.
    - `ferrule import openclaw|hermes`: memories (deduped, re-runs
      supersede, secret-looking entries held back), skills (the M13 review,
      confirmed one by one), allowlists (a union), providers, and keys by
      name only (`--bind-secrets` copies values). Dry run by default,
      idempotent; own JSON5 and YAML-subset readers. `ferrule setup`
      offers it when it finds either tool.
    - **Decisions for Max and open edges:** see the M33 session-log entry.
  - Also standing: a native **Windows sandbox** is being researched
    (`docs/research-windows-sandbox.md`). Unsequenced small wins from the
    strategy doc (§4): `web_search`, keyword-triggered skills,
    `edit_file` SEARCH/REPLACE, a repo map, local-model first-run polish,
    channels Discord → Slack → WhatsApp, the stuck
    detector's two missing signatures, gateway lane idle eviction, ledger
    rotation.

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
- the macOS Seatbelt socket rules were never run on a real Mac by hand;
  the macOS CI test is the check;
- no real OTel collector and no real OpenClaw or Hermes install; the live
  tests are `#[ignore]`d and the guides give the commands.

**Eval.** `eval run evals/starter --variant ab` against the mock model:
engineered 20/20, naive 11/20, $0.98.

**Open edges:** `enforce` mode for shell egress; per-agent policies;
egress denials as span events; `ferrule telemetry replay`; importing MCP
server definitions, scheduled jobs and OpenClaw's SQLite pairing
approvals; Unix sockets on Windows.
