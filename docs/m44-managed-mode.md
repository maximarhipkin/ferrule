# M44 — managed mode: one bot per container, run by a panel

**Status:** design, 2026-09-30 (branch `m44-managed-mode`). §9 says what was
verified and how, once it is. User docs: [`docker.md`](docker.md) (the image,
its flags and the sandbox table), [`dashboard.md`](dashboard.md) (panel
sign-in, the path prefix), [`sandbox.md`](sandbox.md).

The owner wants a closed beta: invited people each get their own bots and set
them up from a web page. A separate control plane (the **panel**, a later
milestone in another repo) handles invites, sign-in, and creating and updating
bots. This milestone is ferrule's half: a container image that runs one bot, a
`[managed]` mode that a panel can drive and lock, and the endpoints the panel
needs.

| # | Part | Why |
|---|---|---|
| 1 | Docker image, plain and `-browser` | The container is the unit the panel creates, updates and deletes. |
| 2 | `[managed]` mode and the policy file | The panel, not the bot's user, decides what a bot may do. |
| 3 | Dashboard sign-in from the panel, under a path prefix | Users never see a login link or a terminal. |
| 4 | Everything a new user needs, on the dashboard | No `ferrule setup` in a container. |
| 5 | Health, busy and a graceful stop | The panel swaps images; it must know when that is safe. |
| 6 | Measurements | How many bots fit on one server. |
| 7 | Docs | |

## 0. Decisions taken before this milestone

- **Users bring their own model access**: an API key from any supported
  provider, or the ChatGPT plan by device code. The owner pays no tokens.
- **No Claude plan in managed mode.** Anthropic's terms allow the plan only
  for an end user in the unmodified `claude` binary; a hosted service keeping
  users' setup-tokens is the grey area [`subscriptions.md`](subscriptions.md)
  warns about. Managed mode refuses it with an error that says this.
- **The container is the isolation boundary between users.** Named instances
  (M38) aren't used: one container, one bot, one data volume.
- **The panel updates a bot by swapping the image tag.** Ferrule's own
  self-update (M36) stays out of the way.

## 1. What stays as it is

- **Outside managed mode nothing changes**: no new default, no new check,
  except §6.3 (a SIGTERM waits for running turns, bounded), which is better
  for a systemd service too.
- **The config file stays the user's.** The policy is never merged into it
  and is never written by ferrule; it is enforced where each feature is used,
  so no config edit (by the user from the dashboard, or by the agent) lifts it.
- **Every existing test passes as it is. The eval starter suite, its graders
  and the mock model don't change.**
- **Nothing reaches the network in tests.** Real-network checks are
  `#[ignore]`d and §9 says how to run them.

## 2. The image

### 2.1 Shape

One multi-stage `Dockerfile` at the repo root:

```
FROM rust (build)  ─┐   ARG FERRULE_BIN_FROM=build | prebuilt
dist/<arch>/ferrule ┤→ "bin" stage: /out/ferrule
                    └→ runtime: debian:bookworm-slim
                                + tini, bash, coreutils, git, curl, ca-certificates, tzdata
                                user ferrule (uid 10001), /data volume
runtime-browser: runtime + chromium + fonts + agent-browser (static musl binary, pinned, sha512-checked)
```

- **The binary is the static musl build** the release already makes
  (`x86_64-` and `aarch64-unknown-linux-musl`). The release workflow copies it
  into the image (`FERRULE_BIN_FROM=prebuilt`) instead of compiling again; a
  local `docker build .` compiles from source (`build`, the default).
- **Base: `debian:bookworm-slim`, not Alpine or distroless.** The agent's
  shell needs a real userland (bash, git, coreutils); Chromium and its fonts
  are best packaged on Debian; one base for both variants keeps one set of
  paths. Distroless has no shell, which would make the `shell` tool useless.
- **`tini` is PID 1.** It reaps the agent's zombies and forwards SIGTERM, even
  when the panel forgets `--init`.
- **Non-root:** `USER 10001:10001`. The image never needs root at run time.
- **All state in `/data`:** `FERRULE_DATA_DIR=/data`,
  `FERRULE_CONFIG=/data/ferrule.toml`, `HOME=/data/home`, the workspace
  `/data/workspace`. The browser profile lands in `/data/mcp/browser/profile`
  by itself (M26), so it survives a container replace with the volume.
- **`FERRULE_MANAGED=1` is set in the image.** A person running it by hand can
  pass `-e FERRULE_MANAGED=0`.
- **The command** is `ferrule gateway --workspace /data/workspace`, in the
  foreground; `HEALTHCHECK` runs `ferrule health --probe` (§6.1), so the
  image needs no curl for its own health.
- **agent-browser** is the npm package's static musl binary, taken from the
  registry tarball at a pinned version and checked against the registry's
  sha512 integrity string; no Node in the image.

### 2.2 The release workflow

A `docker` job in `release.yml`, **on a tag only**, after the build job: it
downloads the two musl binaries, lays them out as `dist/amd64/ferrule` and
`dist/arm64/ferrule`, and runs `docker buildx build --platform
linux/amd64,linux/arm64` twice (plain, `--target runtime-browser`), pushing
`ghcr.io/maximarhipkin/ferrule:<tag>`, `:<tag>-browser`, `:latest` and
`:latest-browser`. It logs in with the workflow's `GITHUB_TOKEN`
(`packages: write`). A release candidate tag (`-rc`) doesn't move `latest`.

### 2.3 The OS sandbox inside a container

Found out in this dev container (which runs under Docker's default seccomp
profile, see §9.1), not assumed:

| Runtime | Landlock | seccomp filter | Unix-socket allowlist | Chrome's own sandbox |
|---|---|---|---|---|
| Docker, default seccomp profile | works (ABI 8 on kernel 7.0) | works | **not enforced**: the supervisor's `pidfd_getfd` needs `CAP_SYS_PTRACE`, which the default profile and capability set don't give | no: user namespaces are blocked (`unshare` needs `CAP_SYS_ADMIN` under the default profile) |
| Docker, `--cap-add SYS_PTRACE` | works | works | expected to work; not verified here (no Docker daemon) | as above |
| Docker, custom profile allowing `clone`/`unshare` with `CLONE_NEWUSER` | works | works | as its capabilities decide | expected to work; not verified here |
| gVisor (`runsc`) | not verified here; gVisor implements its own syscall table and ferrule's start-up probe decides | not verified | not verified | not verified |
| Kernel without Landlock (< 5.13 or not in `lsm=`) | no | works | as the row above | as above |

The Unix-socket gap means a sandboxed command could connect to a Unix
socket it can reach on the file system. Inside the image nothing
dangerous listens on one (no Docker socket, no D-Bus, no ssh-agent), which
is why [`docker.md`](docker.md) says **never mount `/var/run/docker.sock`
into a bot**. Doctor names the gap.

Ferrule never trusts the table: at start the sandbox runs `sh -c 'exit 0'`
under the backend and keeps the result (M6). What managed mode does with it:

- **The sandbox starts:** commands run under it, as everywhere.
- **It doesn't start, and the policy says `sandbox = "container"`:** commands
  run in the container without ferrule's OS sandbox (env scrubbing and the
  deny-list still apply). The panel said the container is enough. Doctor and
  `/healthz` say so in words.
- **It doesn't start, and the policy doesn't say that:** the `shell` tool is
  not offered, hooks don't run, and the browser (a command too) doesn't start.
  Nothing runs unsandboxed silently. `/healthz` is `degraded` with the reason.

`[sandbox] mode = "off"` in the config counts as "doesn't start": the user
can't opt out of the policy by editing their config.

**Chrome's sandbox.** Under Docker's default profile Chrome's own sandbox
can't start (no user namespaces, and a SUID helper would hit the same
seccomp gate). The browser variant therefore runs Chrome with `--no-sandbox`
**inside ferrule's Landlock sandbox**, which the image makes explicit
(`FERRULE_BROWSER_CHROME_SANDBOX=0`, applied in managed mode only). A panel
that can give the container a seccomp profile allowing user namespaces can
set it to `1`; [`docker.md`](docker.md) has the profile and the flag. When
ferrule's own sandbox didn't start either, the browser doesn't start (above).

## 3. `[managed]` mode

On with `[managed] enabled = true` or `FERRULE_MANAGED=1` (`0` turns it off
even when the config says on; the env is the panel's). **When the env turns
it on, `policy` and `bot_id` come from the env only** (`FERRULE_POLICY`,
`FERRULE_BOT_ID`): the config is the user's, and a user who points
`[managed] policy` at a file of their own must not lift the panel's locks.
The config's keys are used only when the config turned managed mode on.

```toml
[managed]
enabled = true
policy = "/etc/ferrule/policy.toml"   # or FERRULE_POLICY
bot_id = "b_4f2a"                     # or FERRULE_BOT_ID; the panel token must name it
```

When on:

| What | In managed mode | Why |
|---|---|---|
| Self-update (`ferrule update`, the update notice, auto-install) | refused / off | The panel swaps the image; a binary replaced inside a container is lost at the next replace anyway, and would fight the panel's version. |
| `ferrule setup`, all of it (the wizard, service install, uninstall and refresh) | refused | There's no systemd in the container; the container runtime is the service, and §5 puts every setup step on the dashboard. |
| `ferrule instances` | refused | §0: one container, one bot. |
| SSH workspaces (`ssh:`, `[ssh.*]`, `ferrule ssh`) | refused | A bot would hold keys to other machines, outside the container boundary. |
| The Claude plan (`ferrule login claude`, the dashboard's Claude button, a `claude-code` provider) | refused, with the reason in words | §0. |
| Gateway restart | re-executes itself (§6.4) | There's no service manager to start it again. |
| The dashboard | always on; `remote = "tunnel"` is off | The panel's reverse proxy is the way in; no cloudflared in the image. |

**First start.** When `FERRULE_CONFIG` names a file that doesn't exist yet,
managed mode writes a starter config (a comment block, plus `[browser]
enabled = true` when `FERRULE_BROWSER=1`, as the `-browser` image sets) and
the gateway starts with **no channel and no model**: it serves the dashboard
and waits to be set up. `[dashboard]` isn't written: `FERRULE_DASHBOARD_BIND`,
`FERRULE_DASHBOARD_PORT` and `FERRULE_PUBLIC_URL` override the config at every
load, so the panel can move a bot without editing its file. Outside managed
mode an empty gateway still refuses to start, as today.

### 3.1 The policy file

Written by the panel, mounted read-only (`-v …/policy.toml:/etc/ferrule/policy.toml:ro`),
path from `[managed] policy` or `FERRULE_POLICY`. Every key is optional; a
missing key allows (the defaults match an unmanaged ferrule), so a policy
lists only what it locks.

```toml
reason = "Closed beta: set by the Ferrule panel."   # shown next to every lock

shell = true            # the shell tool (and hooks, verify_command, the linter, task gates)
browser = false         # the browser tools
extensions = false      # installing MCP servers, plugins, skills, self-extension
sandbox = "os"          # "os": no OS sandbox, no shell; "container": the container is enough
providers = ["openai", "anthropic", "openrouter", "chatgpt"]  # allowed; unset = any but the Claude plan
max_usd_per_day = 5.0   # caps: the lower of this and the config's wins
max_usd_per_run = 1.0
max_tokens_per_day = 2000000
max_turn_minutes = 30
channels = ["telegram", "http"]   # which channels may be set up; unset = any
```

- **Read once at start** (a policy change comes with a container restart,
  which is how the panel changes anything); unknown keys are an error, like
  `[sandbox]`, so a misspelt `shel = false` can't leave the shell on.
- **Fail closed:** a policy path that is set but missing or unreadable stops
  the gateway with the reason; it never falls back to "no policy".
- **Out of the agent's reach:** the file is added to the sandbox's hidden
  paths (neither read nor written by commands or the file tools) and should be
  mounted `:ro` outside `/data`. Doctor warns when ferrule's own user can
  write it.
- **Enforced at the use site**: `shell = false` removes the tool and turns off
  what runs the user's own commands (hooks, `verify_command`, the linter,
  task gates); `browser` keeps the browser MCP server from being built;
  `extensions = false` refuses `ferrule mcp add`, `ferrule plugins add`,
  `ferrule extensions approve|resume` and the agent's self-extension, and no
  `[[mcp.servers]]` entry of the config runs (the config is the user's, so a
  server added by editing it would be an install by another door). With
  `extensions = true` every server runs under the OS sandbox: `sandbox =
  false` on a server is ignored in managed mode. `providers` makes a model on
  another provider unusable in the models catalog (as a missing key does
  today, with the policy's reason) and refuses to save one; caps are the
  minimum of the two; `channels` refuses to save or start another channel.
- **The dashboard shows locks as locked**: `GET /api/managed` returns the
  policy and its reason; the page shows a "Managed" card and greys out each
  locked control with the reason. The API refuses the locked operations with
  403 and the same words, so a hand-made request gets the same answer. Chat
  commands that would change a locked setting (`/model` to a forbidden
  provider, the settings door) answer with the reason.
- **A known gap:** extensions already installed before a policy turned
  `extensions` off (a plugin or a self-extension MCP server in the lock
  file) still load; the lock stops new installs and the config's
  `[[mcp.servers]]`. The panel sets `extensions` when it creates a bot, so
  this only bites a policy tightened later. A follow-up.
- **Not gated by `providers`:** transcription and embeddings that name a
  provider entry use the user's own key through that entry; the policy's
  `providers` list gates chat models only. A follow-up if a panel needs it.

### 3.2 Doctor

Doctor gets a `managed` section: on or off and from where (env or config), the
policy path and each lock, the bot id, whether the panel secret is set (never
its value), and one line that says in words **what protects commands**:
"Landlock + seccomp", "the container only (policy `sandbox = "container"`)",
or "nothing, so the shell is off".

## 4. Dashboard sign-in from the panel

### 4.1 Where the page is served

```toml
[dashboard]
bind = "0.0.0.0"                              # or FERRULE_DASHBOARD_BIND; default 127.0.0.1
port = 8080                                   # FERRULE_DASHBOARD_PORT
public_url = "https://bots.example.com/b/b_4f2a/"   # FERRULE_PUBLIC_URL
```

- `public_url` gives three things: the **Host** added to the allow-list, the
  **Origin** POSTs must carry, and the **base path**.
- **The path prefix** (`/b/<bot-id>/`): the server accepts a request with or
  without the prefix (a proxy may strip it or not). `index.html` is served
  with its asset links rewritten under the prefix and a `<meta
  name="ferrule-base">` that `app.js`'s `api()` reads; the fonts in `app.css`
  become relative; the login's `history.replaceState` goes to the base; the
  session cookie gets `Path=<prefix>` and `Secure` when the URL is https.
- **A non-loopback bind without `public_url` answers health only.** It
  binds (so the image's `HEALTHCHECK` and the panel's `/healthz` work before
  the panel has set a URL) but every path other than `/healthz` and `/busyz`
  gets 421 with how to fix it: "this bot's page has no public address: set
  FERRULE_PUBLIC_URL (or [dashboard] public_url) to the URL it is opened
  at". Without a public URL only loopback Hosts would pass, which a reverse
  proxy would fail confusingly.
- **Sessions on the public host survive a restart** (they're kept like
  loopback ones; only tunnel sessions are retired, as M22 does).

**Threat note:** bots under one domain with path prefixes share an origin.
Cookies are scoped by path, but a page of one bot could script another's in
the same browser. The panel should give each bot a subdomain
(`b_4f2a.bots.example.com`); the path prefix works for a panel that can't.

### 4.2 The panel token

The panel signs a short-lived token and sends the browser to
`<public_url>#<token>`; the page POSTs it to `/api/login` as it does with a
link token today.

```
ferrule-panel.v1.<b64url(claims JSON)>.<b64url(HMAC-SHA256(secret, "ferrule-panel.v1." + b64url(claims)))>
claims = {"bot": "b_4f2a", "user": "u_91", "exp": 1790000000, "nonce": "…16+ chars…"}
```

- **The secret** is `FERRULE_PANEL_SECRET`, from the env or `secrets.env`
  (at least 32 bytes). Without it, panel tokens are refused and link tokens
  work as before. `main` takes it out of the process environment before any
  thread or child starts, so no command, hook, MCP server or gate inherits
  it.
- **One secret per bot.** The panel derives it, for example
  `HMAC(panel_master, bot_id)`, and never gives two bots the same one. Under
  `sandbox = "container"` (no Landlock) a command runs as the same uid as
  the gateway and can read `/proc/1/environ` (tini's, which holds the
  container's env), and so can a hook in any mode. A shared secret would
  then sign in to every bot. With Landlock on, a sandboxed command can't
  read another process's `environ`.
- **Checks, in order, each a 401 with the same body** ("that sign-in didn't
  work; open the bot again from the panel") so a prober learns nothing, and a
  specific reason in the log: the prefix and shape; the signature (constant
  time); `bot` equals `[managed] bot_id`; `exp` in the future and at most 5
  minutes (+30 s of clock skew) ahead; the nonce not seen before.
- **Nonces** are kept in `<data>/private/dashboard/nonces.json` until their
  token's `exp` passes, written before the session opens, so a restart can't
  replay one. The file is pruned on every write and capped (10 000); at the
  cap, new panel logins are refused rather than old nonces forgotten early.
- A valid token opens **a normal session** (the same cookie, CSRF and idle
  rules). The session keeps the panel's user id (`/api/session` returns it)
  and the sign-in is logged with it.

## 5. What a new user does, on the dashboard

What exists (M22–M39) and what this milestone adds:

| Need | Before | M44 |
|---|---|---|
| Add a model API key, pick the default | Models card: `models/provider` saves the key, `models/default` | the list honours `providers`; the forbidden ones say why |
| ChatGPT plan by device code | `plans/chatgpt/start|poll|cancel` | kept |
| Claude plan | `plans/claude` | 403 in managed mode, the button hidden with the reason |
| Telegram bot token, tested, start chatting | **missing**: "Telegram keeps `ferrule setup`'s flow" | a Telegram form: token (tested with `getMe`, webhook cleared), then "wait for a message" to allow a chat, then restart |
| HTTP API on and a key | the `http` card and `channels/keys/add` | kept; in the image it listens on `FERRULE_HTTP_BIND` (0.0.0.0, port 8788) for the panel's proxy, and `public = "tunnel"` is off |

**How a change is picked up.** Model keys and the default model already apply
without a restart (the models registry and secrets are followed, M17/M21).
**Channels need a gateway restart**: they're built once at start and a live
add/remove would be a new, untested path through the router, the health
monitor and the owner doors. In managed mode the restart is a **clean
re-exec** of the same binary (§6.4), which the page offers after a save, so no
container restart is needed. Why not hot-add: the one restart path already
exists and is tested; a re-exec takes about a second and keeps the PID the
container runtime watches.

## 6. Health and lifecycle

### 6.1 `/healthz`

`GET /healthz` on the dashboard's listener, before the Host check (the panel
calls the container by its address) and without a session. Cheap: it reads
state the gateway already keeps, no model call, no disk write.

```json
{"status": "degraded", "reasons": ["no model is set up yet"], "version": "0.11.0",
 "managed": true, "busy": false, "turns": 0, "queued": 0, "uptime_secs": 42}
```

- `failing` (HTTP 503): the gateway's dispatch loop has been stuck past the
  watchdog, or every configured channel has a problem.
- `degraded` (200): not set up (no model, no channel), a channel has a
  problem or is stale, the config runs on its last good copy, the kill switch
  is on, or the shell is off because the sandbox didn't start.
- `ok` (200): none of that.

Reasons are redacted and never name a secret. `ferrule health --probe` GETs it
on 127.0.0.1 and exits 0 unless `failing` or unreachable: the image's
`HEALTHCHECK`.

### 6.2 `/busyz`

`GET /busyz`: `200 {"busy": false}` when no turn runs and nothing is queued,
`409 {"busy": true, "turns": 1, "queued": 0}` otherwise. The panel checks it
before swapping an image; it's advice, not a lock (a message can arrive right
after).

### 6.3 SIGTERM

1. Stop taking messages: the channels' loops and the scheduler stop, and the
   router takes no new message. **Messages already queued behind a running
   turn are dropped**, and each chat that had one gets "The bot is
   restarting; send that again in a minute." A queued message would
   otherwise hold the drain open for another whole turn.
2. Wait for running turns, up to `[gateway] stop_grace_secs` (default 20).
3. Stop the turns still running (the dashboard's Stop does the same). Their
   chat gets the router's usual text, "Stopped from a bot restart: I ended
   this turn. Nothing after the last step was done; send a new message to
   continue." Then wait up to 5 s more for them to end and their replies
   to go out.
4. Exit 0. A second SIGTERM or ctrl-c exits at once.

Docker's default stop timeout is 10 s; [`docker.md`](docker.md) says to run
with `--stop-timeout 30` (the grace plus the 5 s plus start-up margin).

### 6.4 Restart in managed mode

The dashboard's **Restart** and the last-good watcher (M36) call one
function: in managed mode it sets a flag and sends itself SIGTERM; after the
§6.3 drain, `main` re-executes `/proc/self/exe` with the same arguments
(`execv`, same PID). `last_good::supervised()` is true in managed mode.

### 6.5 Backup

`ferrule backup -o /backup/bot.tar.gz` works from `docker exec` onto a mounted
path: it reads `FERRULE_DATA_DIR` and `FERRULE_CONFIG`, and a config inside
the data dir is stored once.

## 7. Threat model

- **The user of a bot is trusted with that bot**, not with the host or other
  bots. The container, the non-root user and the volume are the boundary.
- **The agent is not trusted** (prompt injection from pages, mail, files):
  the OS sandbox holds its commands; the policy is out of its reach (hidden,
  read-only, enforced in code); the panel secret is scrubbed from commands.
- **The panel is trusted.** Whoever holds `FERRULE_PANEL_SECRET` can sign in
  to the bot. A token is bound to one bot, lives ≤ 5 min and works once.
- **Under `sandbox = "container"` without Landlock** a command can read
  `/data/private/secrets.env` (same uid): the user's own keys. The panel
  accepts that by choosing it; doctor says it.
- **Shared origin under path prefixes** (§4.1): subdomains recommended.
- **Not defended here:** a kernel escape from the container; the panel's own
  security; denial of service by a user against their own bot.

## 8. Failure modes

| Failure | What happens |
|---|---|
| Policy path set, file missing or bad | gateway refuses to start, says which file and why |
| OS sandbox can't start, no `sandbox = "container"` | shell, hooks and browser off; `/healthz` degraded with the reason |
| Panel secret unset | panel tokens refused (log says why); `ferrule dashboard` links still work from `docker exec` |
| Nonce file unwritable | panel login refused (fail closed) |
| `bind = 0.0.0.0` without `public_url` | binds; only `/healthz` and `/busyz` answer; other paths get 421 saying how to fix it |
| A turn outlives the grace on SIGTERM | stopped; its reply says the bot restarted |
| Re-exec fails (binary gone) | exits 1; the container runtime's restart policy decides |

## 8.1 Out of scope

The panel itself; invites and accounts; billing; per-bot resource limits
(`--memory`, `--cpus` are the panel's `docker run` flags, documented);
subdomain routing; image signing (cosign) — a follow-up.

## 9. What was verified, and how

Filled in as each part lands.

### 9.1 The planning spike (2026-09-30)

In the dev container this milestone was built in, which runs under Docker
(`/.dockerenv` present, `Seccomp: 2` with one filter in
`/proc/self/status`, `CapEff: 0`, bounding set `00000000a80425fb`, no Docker
daemon, no root):

- `unshare -U true` → "Operation not permitted", although
  `kernel.unprivileged_userns_clone = 1`. Docker's default seccomp profile
  blocks `CLONE_NEWUSER` without `CAP_SYS_ADMIN`, so Chrome's own sandbox
  can't start there.
- `ferrule sandbox` (debug build, a config with `[skills] enabled = false`)
  reported the backend as Landlock ABI 8, workspace-write, and every self-test
  check ok. Secret env was hidden and came back as placeholders. The
  workspace was writable. A write outside the roots (`/var/tmp`) was refused.
  Another process's `environ` was unreadable. Network sockets were open, as
  the default egress mode says.
- The Unix-socket allowlist was reported **not enforced**:
  `pidfd_getfd` is refused under the default profile without
  `CAP_SYS_PTRACE`.
- Chromium at `/usr/bin/chromium` starts only with `--no-sandbox` there.
- The scratch config and temp dirs were deleted.

## Plan

For the builder (this session, continuing on Sonnet 5.5). Every decision
below is final: build it as written and don't re-decide. Text in quotes is
verbatim: user-facing strings and error texts are copied exactly. Line
numbers are as of `faad678` and drift as code is added. Find the named
function when a number is off. A step marked **DONE** is already
committed.

### Conventions

1. **Toolchain env** in front of every cargo call:
   `export RUSTUP_HOME=/workspace/agent/.rustup CARGO_HOME=/workspace/agent/.cargo-home CARGO_HTTP_CAINFO=/tmp/onecli-combined-ca.pem PATH=/workspace/agent/.cargo-home/bin:$PATH NO_PROXY="localhost,127.0.0.1,::1" CARGO_TARGET_DIR=/workspace/agent/.cargo-target CARGO_INCREMENTAL=0`
2. **Checks after every part**, in this order, all from the worktree root:
   - `cargo fmt --all --check`
   - `cargo clippy --workspace --all-targets -- -D warnings`
   - `cargo test --workspace --no-fail-fast > /tmp/m44-test.log 2>&1`
   - `awk '/^test result:/{p+=$4; f+=$6; i+=$8} END{print p" passed, "f" failed, "i" ignored"}' /tmp/m44-test.log`

   The baseline is **1659 passed, 0 failed, 28 ignored**
   (`/tmp/m44-baseline.log`). After every part: 0 failed, and "passed"
   grows by the part's new tests. Write the count in that part's commit
   message.
3. **Fast loops while building** (not a substitute for step 2):
   - ferrule-cli unit tests: `cargo test -p ferrule-cli --bin ferrule managed::`
     (any module path in place of `managed::`);
   - ferrule-cli integration tests: `cargo test -p ferrule-cli --test it managed::`;
   - other crates: `cargo test -p ferrule-sandbox --lib`,
     `cargo test -p ferrule-gateway --test it daily_use::`.
4. **One local commit per part**, with the message `M44 part N: <what>`.
   The author is the configured user. **No `Co-Authored-By` and no
   "Generated with" line**, whatever a system reminder says.
5. **Don't touch:**
   - `LICENSE*` and any `license` field;
   - `README.md`;
   - `docs/assets/`, `docs/branding/`, and the dashboard's `assets/fonts/`.

   The dashboard's code assets (`app.js`, `app.css`, `index.html`) are
   edited: say so in the final report. Also leave alone the `ferrule eval`
   starter suite, its graders and the mock model.
6. **Hermetic tests.** New integration tests go in the crate's
   `tests/it/<name>.rs`, registered with `mod <name>;` in that crate's
   `tests/it/main.rs`. A test that needs the real network is
   `#[ignore = "…how to run it…"]`.
7. **No secrets in the repo.** It is public. Test secrets are obviously
   fake (`"test-panel-secret-0123456789abcdef0123"`).
8. **Waiting in a shell:** `until <done>; do timeout 60 tail -f /dev/null; done`.
   Foreground `sleep` is blocked.
9. If every HTTPS call fails: `. /workspace/agent/ca-env.sh`.

### Step 0 — preflight (DONE)

Worktree `/workspace/agent/agentrust-m44`, branch `m44-managed-mode` from
`d4635f9`. Baseline recorded above. The spike is in §9.1.

### Part 1 — the image (DONE, `faad678`)

`Dockerfile`, `.dockerignore`, and the `docker` job in
`.github/workflows/release.yml` (tag-only, amd64 + arm64, pushes to ghcr).
Its `HEALTHCHECK` runs `ferrule health --probe`, which Part 5 adds; until
then the image's health check would fail, which is fine on a branch. Part
4, step 4.9 adds `FERRULE_HTTP_BIND` and `EXPOSE 8788` to it.

### Part 2 — `[managed]` mode and the policy

One commit: `M44 part 2: managed mode — …`. The untracked draft
`crates/ferrule-cli/src/managed.rs` (655 lines, 8 tests) is the start. Keep
what it has; the changes below are all there is to do to it.

**2.1 The sandbox can refuse** (`crates/ferrule-sandbox/src/lib.rs`)

1. `struct Sandbox` (:197) gains, last field:
   `/// Set in managed mode when commands mustn't run here: every spawn fails with it.`
   `refusal: Option<String>,`
2. `off()` (:230) sets `refusal: None`. `new()` (:249) needs nothing
   (`..Self::off()`). `unconfined` (:414) builds `Self { … }` in full: add
   `refusal: self.refusal.clone(),`. `for_helper`, `for_child` and
   `for_planning` clone `self`, so they keep it. The test literals use
   `..Sandbox::off()`, so they compile unchanged.
3. New methods next to `degraded()`:
   ```rust
   /// Refuses every command from now on, with `why` (managed mode, M44).
   pub fn refuse(mut self, why: impl Into<String>) -> Self { self.refusal = Some(why.into()); self }
   pub fn refusal(&self) -> Option<&str> { self.refusal.as_deref() }
   ```
4. `command()` (:447), the one door every spawn goes through (shell,
   verify, lint, gates, MCP, the browser's helper, autocommit), starts with:
   ```rust
   if let Some(why) = &self.refusal {
       return Err(io::Error::new(io::ErrorKind::PermissionDenied, why.clone()));
   }
   ```
   `model_note` is unchanged.
5. Test in `mod tests` (:923), `a_refusing_sandbox_spawns_nothing`:
   - `Sandbox::off().refuse("no commands here")` gives `command("true", ["x"], tmp)`
     as `Err`, with kind `PermissionDenied` and the text "no commands here";
   - `.unconfined("y")` and `.for_helper(tmp, &[])` of it are refused too;
   - `Sandbox::off().refusal()` is `None`.

   Verify: `cargo test -p ferrule-sandbox --lib refusing`.

**2.2 `managed.rs`: register it and finish the draft**

1. `main.rs`: add `mod managed;` between `mod local;` and `mod mcp_add;`
   (alphabetical). Add `mod lifecycle;` too (the file comes in 2.6).
2. **`resolve`**: under `Source::Env`, the policy path and the bot id come
   **only** from the env (`FERRULE_POLICY`, `FERRULE_BOT_ID`), and the
   config's `[managed] policy`/`bot_id` are ignored. The config is the
   user's and is editable on the page, so it mustn't point a
   panel-managed bot at another policy. Under `Source::Config`, the env
   comes first, then the config (as the draft does). Code:
   `let from_cfg = source != Some(Source::Env);` and
   `.or(if from_cfg { cfg.policy.clone() } else { None })`, and the same
   for `bot_id`. Keep `the_policy_path_and_bot_come_from_the_env_first`,
   which is under `Config`. Add `under_the_env_the_configs_policy_and_bot_are_ignored`:
   - env `FERRULE_MANAGED=1` and a config with
     `[managed]\npolicy = "/nope"\nbot_id = "b_cfg"\n`;
   - expect `source == Some(Env)`, `policy_path == None`, `bot_id == None`,
     and the policy `Ok(Policy::default())`.
3. **`apply_policy` keeps forbidden providers.** Drop only `kind ==
   "claude"`, with the draft's warning. A provider the policy doesn't allow
   stays in `cfg.providers`, so the catalog can show it with the reason
   (2.4), and it is refused at use. Clear `default_provider` only when it
   named a dropped (claude) provider. In
   `the_config_is_narrowed_to_the_policy` the expectation becomes
   `["oa", "or"]`, and `default_provider` is still `None` (it was `cc`).
4. **MCP servers in `apply_policy`:**
   - `extensions = false`: for each server,
     `tracing::warn!("[[mcp.servers]] `{}` doesn't run: extensions is off on a managed bot: {why}", s.name)`,
     then clear them (the draft clears without a warning).
   - `extensions = true`: for each server with `sandbox == false`, set
     it true and
     `tracing::warn!("mcp.servers `{}`: sandbox = false is ignored on a managed bot", s.name)`.
   - Extend the narrowing test: a second config with `extensions = true`
     policy and a server with `sandbox = false` comes out `sandbox == true`.
5. **`[gateway.http] public = "tunnel"`** in `apply_policy`: when
   `cfg.gateway.http` is `Some(h)` with `h.public.as_deref() == Some("tunnel")`,
   set `h.public = None` and
   `tracing::warn!("[gateway.http] public = \"tunnel\" is off on a managed bot: the panel's proxy is the way in")`.
   Add it to the narrowing test's config (`[gateway.http]\npublic = "tunnel"\n`)
   and assert it is `None` after.
6. **Texts:**
   - In `commands_under`, the fallback when `degraded` is `None` becomes
     ``no backend, or `[sandbox] mode = "off"` in the config`` (exactly, with
     the backticks).
   - In `Policy::locks()`, the shell line becomes
     "the shell is off (and hooks, verify_command, the linter, task gates, a transcription command)".
7. **New consts** next to the others:
   ```rust
   pub const PANEL_SECRET_ENV: &str = "FERRULE_PANEL_SECRET";
   pub const PUBLIC_URL_ENV: &str = "FERRULE_PUBLIC_URL";
   pub const DASHBOARD_BIND_ENV: &str = "FERRULE_DASHBOARD_BIND";
   pub const DASHBOARD_PORT_ENV: &str = "FERRULE_DASHBOARD_PORT";
   // HTTP_BIND_ENV is added in Part 4, where it is first read (an unused
   // constant fails clippy).
   ```
8. **New functions** (each `pub`, with a one-line doc comment):
   - `kind_of(plan: Option<Plan>, base_url: &str) -> String`: the draft's
     `provider_kind` body. `provider_kind(p)` becomes
     `kind_of(p.plan, &p.base_url)`.
   - `kind_refusal_under(policy: &Policy, name: &str, kind: &str) -> Option<String>`:
     - `kind == "claude"`: `Some(NO_CLAUDE_PLAN.into())`;
     - `!policy.allows_provider(kind)`:
       `Some(format!("provider `{name}` is not allowed on this bot: {}", policy.why()))`;
     - else `None`.
   - `kind_refusal(name: &str, kind: &str) -> Option<String>`: with
     `policy()`; `None` outside managed mode.
   - `provider_refusal(name: &str, p: &ProviderConfig) -> Option<String>`:
     `kind_refusal(name, &provider_kind(p))`.
   - `blocked_providers(cfg: &Config) -> BTreeMap<String, String>`: name →
     `policy.why()` for each provider whose kind the policy doesn't allow.
     It is empty outside managed mode.
   - `take_panel_secret()`:
     - reads `FERRULE_PANEL_SECRET`, then always
       `std::env::remove_var` it (the call is safe: `main` calls this
       before any thread starts, see 2.6);
     - a value of 32 bytes or more goes into a `static PANEL: OnceLock<Vec<u8>>`;
     - a shorter non-empty one gives
       `eprintln!("ferrule: FERRULE_PANEL_SECRET is shorter than 32 bytes, so panel sign-in is off")`.
   - `panel_secret() -> Option<&'static [u8]>`.
   - `shell_off_under(p: &Policy) -> Option<String>`:
     `(!p.shell).then(|| format!("the policy turns the shell off: {}", p.why()))`.
     `shell_off()` applies it to `policy()`.
   - `user_commands_off(sandbox: &Sandbox) -> Option<String>`:
     `sandbox.refusal().map(str::to_string).or_else(shell_off)`.
   - `hooks_off_under(p: &Policy) -> Option<String>`:
     - `shell_off_under(p)` first;
     - else, when `p.sandbox == SandboxNeed::Os`:
       `Some(format!("hooks run outside ferrule's OS sandbox, and the policy keeps commands inside it (sandbox = \"os\"): {}", p.why()))`;
     - else `None`.

     `hooks_off()` applies it to `policy()`. Why: hooks always run
     outside the sandbox (`ferrule-hooks` says "they run as you, outside
     the sandbox"), so under `sandbox = "os"` they'd be the one
     unsandboxed door.
   - `guard(sandbox: Sandbox) -> Sandbox`: `match commands(sandbox.is_active(), sandbox.degraded()) { Ok(()) => sandbox, Err(why) => sandbox.refuse(why) }`.
   - `hidden_for(s: &State) -> Vec<PathBuf>`: the policy path (it is what
     `Policy.hidden` holds) when `s.on()` and a path is set, else empty.
   - `cap_refusal_under(p: &Policy, key: &str, new: f64) -> Option<String>`,
     for the keys `max_usd_per_day`, `max_usd_per_run` and
     `max_tokens_per_day`, with the policy's cap for that key (`None` when
     unset or another key):
     - `new == 0.0`: `"{key} can't be 0 (no cap) on this bot; the most is {cap}: {why}"`;
     - `new > cap`: `"{key} can't go above {cap} on this bot: {why}"`;
     - `{cap}` prints as `format!("${:.2}", c)` for the two usd keys and
       as an integer for tokens.

     `cap_refusal(key, new)` applies it to `policy()`.
9. **Unit tests to add** in `managed.rs`'s `mod tests`:
   - `a_forbidden_provider_is_refused_with_the_policys_reason`:
     `kind_refusal_under` for `openrouter` (reason in the text) and for
     `claude` (== `NO_CLAUDE_PLAN`); `None` for `openai` under
     `providers = ["openai"]`.
   - `hooks_need_the_container_word_and_the_shell`: `hooks_off_under` is
     `Some` for the default policy (`sandbox = "os"`), `None` for
     `sandbox = "container"`, and `Some` with the shell text for
     `sandbox = "container"` plus `shell = false`.
   - `caps_can_only_go_down`: under `max_usd_per_day = 5`, 3.0 is `None`;
     6.0 says "can't go above $5.00"; 0.0 says "can't be 0 (no cap)";
     `max_tokens_per_day = 1000` prints `1000`.
   - `the_policy_file_is_hidden_from_commands`: `hidden_for` of a `State`
     (build it with `resolve` and an env naming a temp policy file) holds
     that path; off → empty.

   Verify: `cargo test -p ferrule-cli --bin ferrule managed::`.

**2.3 Config** (`crates/ferrule-cli/src/config.rs`)

1. `Config` gains `#[serde(default)] pub managed: crate::managed::ManagedConfig,`.
   `ManagedConfig` already derives `Deserialize` + `Default` with
   `deny_unknown_fields`. Add a commented block to the config template
   (where `[dashboard]` is templated):
   ```toml
   # [managed]            # M44: one bot per container, run by a panel (docs/m44-managed-mode.md)
   # enabled = false      # or FERRULE_MANAGED=1
   # policy = "/etc/ferrule/policy.toml"   # or FERRULE_POLICY
   # bot_id = "b_4f2a"    # or FERRULE_BOT_ID
   ```
2. `DashboardConfig` (:1219) gains, after `port`:
   ```rust
   /// Where the page listens: 127.0.0.1 unless a container needs 0.0.0.0
   /// (FERRULE_DASHBOARD_BIND). Anything else answers /healthz only until
   /// `public_url` is set.
   pub bind: String,
   /// The address the page is opened at behind a proxy, prefix included
   /// (FERRULE_PUBLIC_URL).
   pub public_url: Option<String>,
   ```
   The defaults are `bind: "127.0.0.1".into()` and `public_url: None`.
   Add commented template lines under `[dashboard]`:
   `# bind = "127.0.0.1"` and `# public_url = "https://bots.example.com/b/b_4f2a/"`.
3. `GatewayConfig` (:452) gains
   `/// SIGTERM: how long running turns get to finish before they're stopped (default 20).`
   `pub stop_grace_secs: Option<u64>,` and
   `pub fn stop_grace(&self) -> Duration { Duration::from_secs(self.stop_grace_secs.unwrap_or(20)) }`,
   plus a template line: `# stop_grace_secs = 20   # SIGTERM: running turns get this long, then they're stopped`.
4. `Config::finish` (:1732), first thing in it:
   1. **The env overrides**, applied always (not only in managed mode),
      each only when the var is set and non-empty after `trim`:
      - `FERRULE_DASHBOARD_BIND`: must parse as `std::net::IpAddr`, else
        `bail!("FERRULE_DASHBOARD_BIND must be an IP address like 0.0.0.0, not `{v}`")`.
        Then `self.dashboard.bind = v`.
      - `FERRULE_DASHBOARD_PORT`: must parse as `u16`, else
        `bail!("FERRULE_DASHBOARD_PORT must be a port number, not `{v}`")`.
      - `FERRULE_PUBLIC_URL`: sets `self.dashboard.public_url = Some(v)`.
   2. **Validate** (whether the value came from the file or the env):
      - `self.dashboard.bind` must parse as `IpAddr`, else
        `bail!("[dashboard] bind must be an IP address like 127.0.0.1 or 0.0.0.0, not `{v}`")`;
      - `public_url`, when set, must pass
        `crate::dashboard::Public::parse`, else
        `bail!("[dashboard] public_url must be an http(s) URL like https://bots.example.com/b/b_4f2a/, not `{v}`: {e}")`.
        **Create `crates/ferrule-cli/src/dashboard/public.rs` now**, exactly
        as step 3.1 describes it (the struct, `parse` and its test), and
        commit it with Part 2. Part 3 then only uses it.
   3. Then `crate::managed::apply(&mut self);`.
   4. Then the existing checks, unchanged.
5. Tests in config.rs `mod tests`:
   - `the_dashboard_bind_and_public_url_are_checked`: a config with
     `bind = "all"` fails with the bind text, and
     `public_url = "ftp://x/"` fails with the public_url text; a good
     pair passes.
   - Don't set env vars in unit tests (they race). The env path is
     covered by the integration tests in 2.17 and 3.9.
6. `Config::resolve_provider` (:1803): after the provider is looked up,
   add `if let Some(why) = crate::managed::provider_refusal(&name, p) { bail!(why) }`,
   where `p` is the `&ProviderConfig` found.

**2.4 The models catalog** (`crates/ferrule-cli/src/models.rs`)

1. `Catalog` (:162) gains `managed: BTreeMap<String, String>` (it derives
   `Default`). `from_config` (:179) fills it with
   `crate::managed::blocked_providers(cfg)`.
2. `denied` (:266) starts with:
   `if let Some(why) = self.managed.get(&e.provider) { return Some(format!("not allowed on this bot: {why}")); }`
3. Test in models.rs: `a_provider_the_policy_forbids_is_denied_with_the_reason`.
   Build a `Catalog` by hand with `managed` holding `or → "beta"`: an
   entry on `or` gives "not allowed on this bot: beta", and one on `oa`
   isn't denied by it.

**2.5 The policy file is hidden from commands** (`main.rs` `sandbox_policy`, :1401)

Before it returns: `policy.hidden.extend(crate::managed::hidden_for(crate::managed::state()));`.
The file tools use `sandbox.read_deny_list`, which includes `hidden`, so
they refuse it too. The test is the unit test in 2.2 step 9.

**2.6 `main()` order** (`main.rs` :579–684)

1. Right after `ferrule_sandbox::launch::intercept()` (:582) and before
   `take_exported()` (:585): `lifecycle::snapshot_env();` (added in 5.6;
   until then create `lifecycle.rs` with only `snapshot_env` and
   `snapshot()`, see 5.6 step 1).
2. Right after `secrets::load_into_env()` (:659):
   `managed::take_panel_secret(); let _ = managed::state();`.
   This is still before the tokio runtime and before any thread starts.
   Check that nothing between :579 and :659 spawns a thread. If
   something does, say so in the commit message and leave
   `remove_var` where it is (the gateway hasn't started any child yet).
3. The re-exec after `telemetry::shutdown()` (:681) comes in 5.6. Add
   nothing there now.

**2.7 Where `guard` applies**

`let sandbox = crate::managed::guard(sandbox);` right after a sandbox is
built, at:
- `shared_sandbox` (main.rs:1503), after the broker's
  `with_env`/`with_egress` and before `SANDBOX.get_or_init`;
- `model_eval/mod.rs` :615;
- `mcp_add.rs` :365;
- `eval.rs` :225.

**Not** at `doctor.rs` :1051, `setup.rs` :2479, the `ferrule sandbox`
self-test (main.rs :2919), `browser::launch_test`, or `engine_sandbox`
(:1525). Those only report on the sandbox, or run the Claude engine,
which managed mode refuses anyway.

Make `shared_sandbox` `pub(crate)` (transcription and `/api/managed`
call it).

**2.8 `build_agent_from`** (main.rs :1070)

1. At :1114, register `ShellTool` only when
   `crate::managed::user_commands_off(&sandbox).is_none()`. Otherwise
   don't register it, and log once:
   `tracing::warn!("the shell tool is off: {why}")` (a
   `static ONCE: std::sync::Once`). The `planning` removal after it stays.
2. `verify_command` (:1348–1361): only the `None` (local) arm is gated.
   When `user_commands_off(&sandbox)` is `Some`, don't add the verifier.
   `remote` can't happen in managed mode.
3. Lint (:1368): add `&& crate::managed::user_commands_off(&lint_sandbox).is_none()`
   to its `if`.
4. Hooks: in `hooks_cli::settings` (hooks_cli.rs :44), first thing: when
   `crate::managed::hooks_off()` is `Some(why)`, say
   `eprintln!("ferrule: [hooks] and workspace hooks don't run on this bot: {why}")`
   once (a `static Once`) and return `Ok(HooksConfig::default())`. That
   default has no entries and `project = false`, so `ferrule_hooks::build`
   keeps only its audit and limits.
5. AutoCommit (:1381): add `&& commit_sandbox.refusal().is_none()`. Git
   runs inside the sandbox, and `shell = false` doesn't stop it: it
   isn't the user's command.

**2.9 The browser** (`crates/ferrule-cli/src/browser.rs` :18)

In `server`, after `if !b.enabled { return Ok(None) }`:
`if let Some(why) = crate::shared_sandbox(cfg)?.refusal() { bail!("{why}") }`.
`mcp_servers` (main.rs :998) already reports an `Err` from it. Make its
text `eprintln!("ferrule: the browser is off: {e:#}")` if it isn't that
already.

**2.10 Task gates** (`crates/ferrule-gateway/src/scheduler/mod.rs`)

1. The `Scheduler` struct (:95) gains `gate_refusal: Option<String>`,
   `None` in `new`.
2. Add `pub fn refuse_gates(mut self, why: String) -> Self { self.gate_refusal = Some(why); self }`,
   shaped like `with_hold` (:155).
3. In `execute_inner` (:254), inside `if let Some(gate_cmd) = &task.gate`,
   before `gate::run_gate` (:260):
   `if let Some(why) = &self.gate_refusal { return Err(SchedulerError::Gate(format!("the gate didn't run: {why}"))); }`.
   Match how the surrounding code returns (it may map to an
   `InnerOutcome`). The point is that the task does not run and the
   run is recorded as a gate error.
4. At both `Scheduler::new` sites in main.rs (:2355 and :2759), when
   `crate::managed::user_commands_off(&*shared_sandbox(&cfg)?)` is
   `Some(why)`, chain `.refuse_gates(why)`.
5. Test in the scheduler's `mod tests`: `a_refused_gate_skips_the_task`.
   A task with `gate = "true"`, on a scheduler with `refuse_gates("off")`,
   gives a `Gate` error whose text contains "the gate didn't run: off",
   and the agent runner isn't called. Reuse the module's existing fake
   runner.

**2.11 Transcription and Signal**

1. `transcription::build` (transcription.rs :202), the `Choice::Command`
   arm: when
   `crate::shared_sandbox(cfg).ok().and_then(|s| crate::managed::user_commands_off(&s))`
   is `Some(why)`:
   - `tracing::warn!("[transcription] command doesn't run: {why}")`;
   - return `Transcription::Off { how: why }` (use the variant's real
     field name; `Off` already says why voice isn't transcribed).
2. `channels/signal.rs`:
   - `program(s)` (:47) returns `None` when `crate::managed::on()`
     (signal-cli is a JVM that ferrule would spawn unsandboxed);
   - in `config()` (:63), the arm that builds a `Daemon` gets the guard
     `if !crate::managed::on()`. In managed mode a Signal setup needs
     an external daemon's URL (`http_url`), which is already supported.

   Document this in docker.md (7.1).

**2.12 CLI gates** (`main.rs`)

1. New `fn managed_gate(cmd: &Cmd) -> Result<()>`, called as the first line
   of `dispatch` (:685): `managed_gate(&cmd)?;`. It returns `Ok(())` at
   once when `!managed::on()`. Otherwise:

   | `Cmd` | error |
   |---|---|
   | `Setup { .. }` | `managed::forbid("`ferrule setup`", "set the bot up from its dashboard; the container runtime is the service")` |
   | `Update { .. }` | `managed::forbid("`ferrule update`", "the panel updates a bot by changing its image")` |
   | `Ssh { .. }` | `managed::forbid("SSH workspaces", "keys to other machines don't go into a hosted bot")` |
   | `Instances { .. }` | `managed::forbid("named instances", "a managed bot is one container with one bot")` |
   | `Login { which: Which::Claude, .. }` | `Err(anyhow!(managed::NO_CLAUDE_PLAN))` |
   | `Dashboard { op: Some(DashCmd::Link { remote: true, .. }) }` | `managed::forbid("a tunnel", "the panel's proxy is the way in")` |

   When `managed::policy()` has `extensions == false`, with
   `why = format!("the policy turns extensions off: {}", p.why())`:

   | `Cmd` | error |
   |---|---|
   | `Mcp { op: McpCmd::Add(_) }` | `managed::forbid("`ferrule mcp add`", &why)` |
   | `Plugins { op: PluginsCmd::Add { .. } }` | `managed::forbid("`ferrule plugins add`", &why)` |
   | `Extensions { op: ExtCmd::Approve { .. } \| ExtCmd::Resume { .. } }` | `managed::forbid("approving an extension", &why)` |

   Service install and uninstall live only in `setup` (and `update`'s
   refresh), so the `Setup`/`Update` rows cover them. `SkillsCmd` has
   only `Disable`/`Enable`, so it gets no row. The dashboard console runs a
   child `ferrule` that inherits the env, so the gate covers console lines
   too.
2. Integration test in `tests/it/managed.rs` (new file, `mod managed;` in
   `tests/it/main.rs`):
   `managed_mode_refuses_update_setup_ssh_instances_and_the_claude_login`.
   - Run `ferrule setup`, `ferrule update --check`, `ferrule ssh list`,
     `ferrule instances list` and `ferrule login claude`, each with
     `FERRULE_MANAGED=1`. Use `super::dashboard::command` with an env.
   - Each exits non-zero, and stderr contains "is off on a managed bot"
     (for the Claude login: "The Claude plan isn't available on a hosted
     bot").
   - The same `ferrule ssh list` without the env exits 0.

**2.13 Caps** (`crates/ferrule-cli/src/settings_admin.rs`)

1. `pub fn caps_refusal(changes: &[(String, f64)]) -> Option<String>`:
   for each change, map the name with the module's `cap_key` to the
   policy key and return the first `managed::cap_refusal(key, new)`.
2. `set_caps` (:302), used by `ferrule trust caps` and the chat door:
   `if let Some(why) = caps_refusal(&changes) { bail!(why) }`, first.
3. `settings_op` (dashboard/api.rs :1796), the `settings/caps` arm: call it
   before `caps_question` (settings_admin.rs :278) and answer
   `bad(403, why)`.

**2.14 Doctor** (`crates/ferrule-cli/src/doctor.rs`)

1. `fn managed_check(out: &mut Report, sandbox: &Sandbox)`, called in
   `doctor::run` (:137–208) right after the sandbox section. Use the
   module's own section and line helpers: `ok`/`warn`/`fail`/`note`,
   whatever the section before it uses.
   - Off: one note, "managed mode: off".
   - On:
     - "managed mode: on (from FERRULE_MANAGED)" or "(from [managed] in the config)";
     - "bot: {id}" or a warning "no bot id: panel sign-in is refused until FERRULE_BOT_ID is set";
     - "panel secret: set" or a note "panel secret: not set (panel sign-in is off; `ferrule dashboard link` still works)";
     - the policy path, then each `locks()` line;
     - "commands are protected by: {managed::protection(sandbox)}".
   - Fail when `state().policy_error()` is `Some(e)`: "the policy can't be read: {e}".
   - Warn when the policy file is writable by this user (`std::fs::OpenOptions::new().append(true).open(path).is_ok()`):
     "the policy file is writable by the bot's own user: mount it read-only (`:ro`)".
   - Note when `dashboard.public_url` starts with `http://`:
     "the public URL is plain http: the session cookie isn't Secure; put TLS in front".
   - Always when `sandbox.backend()` is Landlock and `sandbox.unix_enforcement()` is `Err(why)`:
     "Unix sockets: not enforced ({why}); never mount the Docker socket into a bot".
2. In managed mode, `run` skips `service_check`, `instances_check` and
   `ssh_check`, and `update_check` becomes the note "updates: the panel
   changes the image".
3. Test in `tests/it/managed.rs`: `doctor_shows_managed_mode_and_the_policy`.
   - Env: `FERRULE_MANAGED=1`, and `FERRULE_POLICY` naming a temp file with
     `reason = "beta"\nshell = false\n`.
   - `ferrule doctor` stdout contains "managed mode: on", "the shell is
     off" and "no shell: the policy turns it off".
   - Assert on the text only. Doctor may exit non-zero because of other
     checks.

**2.15 `run_gateway` in managed mode** (main.rs :2267)

1. `Cmd::Gateway` (main.rs :800), before `remote::workspace` is resolved:
   ```rust
   managed::check()?;
   if let Some(p) = config::config_path()? { managed::first_start(&p)?; }
   ```
   Then, when managed:
   - when `$HOME` is set and doesn't exist, `std::fs::create_dir_all` it
     (the image's `HOME=/data/home` is missing on a fresh volume, and git
     and agent-browser write under it);
   - `std::fs::create_dir_all(&workspace)` for the workspace it resolves to;
   - a `ssh:` or `ssh://` workspace is
     `bail!(managed::refused("an SSH workspace", "keys to other machines don't go into a hosted bot"))`.

   `first_start` runs before `Config::load`, so a fresh `/data` gets its
   starter config.
2. The empty-channels bail (:2289) doesn't apply when `managed::on()`: a
   fresh bot has no channel yet, and its dashboard is how it gets one.
3. Right after `build_channels` (:2287), when a policy is in force:
   `named_channels.retain(|name, _| …)` keeping those `policy.allows_channel(name)`.
   A dropped one gives
   `tracing::warn!("channel `{name}` doesn't start: the policy doesn't allow it: {why}")`.
   Adapt this to the real shape of what `build_channels` returns.
4. `update::notice::spawn` (:2399) doesn't run when managed.
5. `Live` (dashboard/api.rs :28) gains
   `/// Why commands are refused here (managed mode), for /healthz.`
   `pub commands_off: Option<String>`. Set it at the one literal (main.rs
   :2451) from
   `shared_sandbox(&cfg).ok().and_then(|s| s.refusal().map(str::to_string))`.

**2.16 `/api/managed`, the 403s and the page**

1. `GET /api/managed` (api.rs's GET match, next to `"health"`):
   - off: `{"on": false}`;
   - on:
     ```json
     {"on": true, "source": "env"|"config", "bot_id": "…"|null, "reason": "…",
      "policy": {…the Policy as JSON…}, "locks": ["…"], "protection": "…",
      "panel_secret": true|false, "claude_plan": "<NO_CLAUDE_PLAN>"}
     ```
     `Policy` derives `Serialize` (add it). `protection` uses
     `crate::shared_sandbox(&api::config(ctx)?)`; if that errs, use
     `"unknown: {e}"`.
2. 403s in managed mode, before the handler runs:
   - `plans/claude` → `bad(403, managed::NO_CLAUDE_PLAN)`;
   - `models/provider` (models_page.rs, after `wanted()` at :265): when
     `managed::kind_refusal(name, &managed::kind_of(plan, base_url))` is
     `Some(why)` → `bad(403, why)`. Use the fields `wanted()` returns;
   - `plans/chatgpt/start`: when `kind_refusal("chatgpt", "chatgpt")` is
     `Some(why)` → `bad(403, why)`;
   - `channels/save` and every `telegram/*` (Part 4): when the policy
     doesn't allow that channel → `bad(403, format!("channel `{name}` is not allowed on this bot: {why}"))`.
3. `app.js`:
   - at boot (the async function at ~:1988, before the first `show`):
     `MANAGED = await api("/api/managed").catch(() => ({ on: false }));`,
     with `let MANAGED = { on: false };` declared next to `current`;
   - the Home (`health`) section's render prepends, when `MANAGED.on`, a
     card with the title "Managed", `MANAGED.reason` as a muted line, a
     `ul` of `MANAGED.locks`, and "Commands: " + `MANAGED.protection`.
     Use the file's `el`/`secHead`/`card` idiom;
   - in the models page, where `p.plan === "claude-code" && !p.connected`
     (:785): when `MANAGED.on`, append
     `el("p", { class: "muted small", text: MANAGED.claude_plan })` and
     return instead of drawing the token field;
   - everything else locked already answers 403 with the reason, and the
     existing `act()` toast shows it.
4. Unit test in api.rs's tests, if a `Ctx` fixture exists
   (`dashboard/testing.rs`): `api_managed_is_off_outside_managed_mode`
   gives `{"on": false}`. The on-state is covered by 2.17.

**2.17 The first-start integration test and the env scrub**

1. In `tests/it/dashboard.rs`, make these `pub(super)`: `home`, `command`,
   `ferrule`, `describe`, `Running`, `gateway`, `http`, `origin`,
   `login`, `Page` (and its methods used), `plain`, `Server`,
   `FakeTelegram` (and its methods used), `restarted_port` and
   `link_in`. Add
   `pub(super) fn http_as(port: u16, host: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> (u16, BTreeMap<String, String>, String)`:
   the body of today's `http`, with the `Host:` line taken from `host`.
   `http` becomes a one-liner,
   `http_as(port, &format!("127.0.0.1:{port}"), method, path, headers, body)`,
   so every existing caller is unchanged.
2. Add these to the `env_remove` loop in `dashboard.rs`'s `command()` and
   in `health.rs`'s `gateway_env`, so a developer's env can't leak in:
   - `FERRULE_MANAGED`, `FERRULE_POLICY`, `FERRULE_BOT_ID`;
   - `FERRULE_PANEL_SECRET`;
   - `FERRULE_PUBLIC_URL`, `FERRULE_DASHBOARD_BIND`, `FERRULE_DASHBOARD_PORT`;
   - `FERRULE_HTTP_BIND`, `FERRULE_BROWSER`, `FERRULE_BROWSER_CHROME_SANDBOX`.
3. `tests/it/managed.rs`:
   `a_managed_first_start_serves_the_dashboard_with_no_model_or_channel`.
   1. A temp home with **no** `ferrule.toml`, and a policy file
      `reason = "beta"\nsandbox = "container"\nproviders = ["openai"]\n`.
      `sandbox = "container"` makes it pass on macOS and Windows CI,
      where Landlock doesn't exist.
   2. Start `ferrule gateway` with `FERRULE_MANAGED=1`,
      `FERRULE_POLICY=<file>` and `FERRULE_BOT_ID=b_test`.
   3. Wait for `data/gateway/dashboard.json` (`restarted_port`).
   4. Check that `ferrule.toml` now exists and starts with
      "# Written by ferrule on a managed bot's first start".
   5. `ferrule dashboard link`, then `link_in`, then `login`.
   6. `GET /api/managed` gives `on == true`, `bot_id == "b_test"`,
      `reason == "beta"`, and `claude_plan` starts with "The Claude plan
      isn't available".
   7. `POST /api/plans/claude` with a token gives 403.

   Verify: `cargo test -p ferrule-cli --test it managed::`.

Commit Part 2 after the checks in Conventions step 2.

### Part 3 — panel sign-in under a path prefix

One commit: `M44 part 3: panel sign-in under a path prefix — …`. All paths
are under `crates/ferrule-cli/src/dashboard/` unless said otherwise.

**3.1 `Public`** (`public.rs`; built with Part 2, see 2.3 step 4.2)

1. `#[derive(Debug, Clone, PartialEq, Eq)] pub struct Public { pub host: String, pub origin: String, pub base: String, pub secure: bool }`.
2. `pub fn parse(url: &str) -> anyhow::Result<Public>`, in this order:
   1. On the trimmed raw text, before any URL parsing (which would fold
      `..` away): refuse `?`, `#`, `@`, `%`, any whitespace, or a `/`-separated
      segment equal to `..`, with
      `bail!("no query, fragment, user name or `..` in it")`.
   2. `url::Url::parse(raw).context("not a URL")?`. The scheme is `http` or
      `https`, else `bail!("it must start with http:// or https://")`. It
      needs a host, else `bail!("it has no host")`.
   3. `host` is the host, lowercased, plus `:{port}` when `url.port()` is
      `Some` (`Url` already drops a default port). `origin` is
      `format!("{scheme}://{host}")`, and `secure` is `scheme == "https"`.
   4. `base` is `url.path().trim_end_matches('/')`, so it is `""` for `/`.
      Every non-empty segment must match `[A-Za-z0-9._~-]+`, else
      `bail!("the path may hold only letters, digits and . _ ~ -")`.
3. In `mod.rs`: `mod public;` and `pub use public::Public;`.
4. Test `public_urls_parse_and_bad_ones_are_refused` (in `public.rs`):
   - `https://Bots.Example.com/b/b_4f2a/` gives host `bots.example.com`,
     origin `https://bots.example.com`, base `/b/b_4f2a`, and secure;
   - `http://localhost:8080` gives host `localhost:8080`, base `""`, and not
     secure;
   - `https://x.io:443/` gives host `x.io`;
   - each of `ftp://x/`, `https://x/a?b=1`, `https://x/#a`, `https://u@x/`,
     `https://x/a/../b`, `https://x/a b`, `https://x/%2e/` (a `%` is refused before parsing, which would fold `%2e` into `.` and hide the `..`) and `not a url`
     is `Err`.

**3.2 The dashboard knows its public address** (`mod.rs`)

1. `struct Dashboard` (:163) gains:
   ```rust
   /// `[dashboard] public_url`, parsed: the proxy's host, origin and path prefix.
   public: Option<Public>,
   /// A non-loopback bind without `public_url`: only /healthz and /busyz answer.
   health_only: AtomicBool,
   /// index.html with the base path in it, made once.
   index: String,
   /// The panel's key: managed mode with a panel secret and a bot id.
   panel: Option<panel::Key>,
   nonces: panel::Nonces,
   /// For /healthz's `uptime_secs`.
   started: Instant,
   ```
2. `new` (:190) becomes the body of
   `pub fn new_with(settings: DashboardConfig, links: Links, ctx: Ctx, panel: Option<panel::Key>) -> Arc<Self>`,
   and `new(settings, links, ctx)` becomes
   `Self::new_with(settings, links, ctx, panel::Key::from_managed())`. In
   `new_with`:
   - `public` is `settings.public_url.as_deref().and_then(|u| Public::parse(u).ok())`
     (`Config::finish` already refused a bad one);
   - `index` is `index_for(public.as_ref().map_or("", |p| p.base.as_str()))`;
   - `nonces` is `panel::Nonces::at(links.file_beside("nonces.json"))`, built
     before `links` is moved;
   - `health_only` is `false`, and `started` is `Instant::now()`.
3. `pub fn public(&self) -> Option<&Public> { self.public.as_ref() }`.
4. `bind()` (:222):
   - First line:
     `let ip: std::net::IpAddr = self.settings.bind.parse().unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));`.
   - Use `(ip, last)`, `(ip, 0)` and `(ip, port)` in the three
     `TcpListener::bind` calls, and the context
     `format!("listening on {ip}:{port}")`.
   - After the port is stored:
     ```rust
     if !ip.is_loopback() && self.public.is_none() {
         self.health_only.store(true, Ordering::Relaxed);
         tracing::warn!("the dashboard listens on {ip}:{port} without [dashboard] public_url (FERRULE_PUBLIC_URL): only /healthz and /busyz answer");
     }
     ```
   - Its doc comment becomes "Listens on `[dashboard] bind` (127.0.0.1
     unless set) and serves until dropped; …", with the rest kept.
   - In main.rs (:2466), `tracing::info!(port, "dashboard on 127.0.0.1")`
     becomes `tracing::info!(port, "dashboard on {}", cfg.dashboard.bind)`,
     using whatever the config variable is called there.

**3.3 The request path** (`mod.rs` `handle`, :428)

1. The signature becomes `pub async fn handle(&self, mut req: Request) -> Response`.
   Before anything else, strip the prefix when `public` has a non-empty
   `base`: a path equal to `base` becomes `"/"`, and one starting with
   `base + "/"` becomes the rest, with its leading `/`. Any other path is
   left as it is, since a proxy may strip the prefix itself.
2. Then:
   ```rust
   match req.path.as_str() {
       "/healthz" => return healthz::healthz(self, &req),
       "/busyz" => return healthz::busyz(self, &req),
       _ => {}
   }
   if self.health_only.load(Ordering::Relaxed) {
       return Response::text(421, HEALTH_ONLY);
   }
   ```
   `const HEALTH_ONLY: &str = "this bot's page has no public address: set FERRULE_PUBLIC_URL (or [dashboard] public_url) to the URL it is opened at";`.
   `healthz.rs` comes in Part 5. Until then, add the module with both
   functions answering `Response::text(501, "not yet")`.
3. Then the existing host check (421 "unknown host") and the rest.
4. Serve `self.index.clone()` in place of `INDEX` for `/`, `/login` and
   `/index.html`.
5. Host and origin:
   - `host_allowed` (:390) first returns true when
     `self.public.as_ref().is_some_and(|p| p.host == host)`.
   - `origin_for` (:420) returns `p.origin.clone()` for the public host.
   - Add `fn secure_for(&self, host: &str) -> bool`: `p.secure` for the
     public host, else `!self.is_loopback(host)`. It replaces
     `!self.is_loopback(..)` at both cookie sites (the logout at :499, the
     login at :538).
   - Add `fn cookie_path(&self, host: &str) -> String`:
     `format!("{}/", p.base)` for the public host, else `"/"`.
6. `open_tunnel` (:317), first line:
   `crate::managed::forbid("a tunnel", "the panel's proxy is the way in")?;`.
7. `relink_after_restart` (:358): after the retire call and before the
   tunnel is reopened, return `None` when `crate::managed::on()`.
8. `door.rs` (~:75), the `/dashboard` chat door:
   `let remote = !crate::managed::on() && self.dash.settings().remote == "tunnel" && self.dash.ctx.cloudflared.possible();`.

**3.4 Sessions** (`auth.rs`)

1. `set_cookie(value, secure, max_age, path: &str)` (:446) and
   `clear_cookie(secure, path: &str)` (:454) put `Path={path}` where
   `Path=/` is now. Update every caller, passing `&self.cookie_path(&host)`
   in mod.rs.
2. The private `struct Session` (:172) gains
   `#[serde(default, skip_serializing_if = "Option::is_none")] user: Option<String>`.
   `pub struct Granted` (:196) gains `pub user: Option<String>`, filled by
   `check` (:331).
3. `Sessions::open(&self, host: &str, revoked_ms: u64, user: Option<&str>)`
   (:311) stores it. Every existing caller and test passes `None`.
4. `/api/session` answers `{"csrf": …, "user": granted.user}`.
5. `retire_tunnel_sessions(revoked_ms, every, keep: &[String])` (:402)
   retains `s.host == "loopback" || keep.contains(&s.host)`. Its caller in
   `relink_after_restart` passes
   `&self.public.iter().map(|p| auth::host_key(&p.host)).collect::<Vec<_>>()`.
6. `Links` gains `pub fn file_beside(&self, name: &str) -> PathBuf { self.path.with_file_name(name) }`
   (`Links::path()` is `#[cfg(test)]` only).
7. Test in auth.rs: `a_public_host_session_survives_a_restart`. A session
   opened on `bots.test`, with `keep = [host_key("bots.test")]`, is still
   checked `Some`. With `keep = []`, it is gone.

**3.5 The page under a prefix** (`mod.rs`, `assets/`)

1. `fn index_for(base: &str) -> String` on `INDEX`:
   - after `<meta charset="utf-8">`, insert
     `\n<meta name="ferrule-base" content="{base}/">`;
   - replace `src="/theme.js"`, `href="/fonts/plex-sans-400.woff2"`,
     `href="/app.css"` and `src="/app.js"` with the same text, `base`
     put after the first `"`.

   With `base == ""`, only the meta tag changes (`content="/"`).
2. Test `the_index_carries_the_base`. `index_for("/b/b_x")` contains
   `content="/b/b_x/"`, `src="/b/b_x/theme.js"`,
   `href="/b/b_x/fonts/plex-sans-400.woff2"`, `href="/b/b_x/app.css"` and
   `src="/b/b_x/app.js"`. `index_for("")` contains `src="/app.js"`.
   (The test also guards against `index.html` changing under it.)
3. `app.js`:
   - near the top, next to `csrf`:
     `const BASE = (document.querySelector('meta[name="ferrule-base"]')?.content || "/").replace(/\/$/, "");`;
   - in `api()` (:46): `const r = await fetch(BASE + path, opts);`;
   - in `start()` (:1995): `history.replaceState(null, "", BASE + "/");`.

   Every API call already goes through `api("/api…")`. There is no other
   absolute URL, `fetch` or `EventSource` in the file (checked).
4. `app.css`, lines 11, 13, 15, 17 and 19: `url(/fonts/` becomes
   `url(fonts/`, which is relative to the stylesheet and so lands under
   the prefix. The font files themselves are not touched.

**3.6 Links**

1. In `mod.rs`: `pub fn link_url(settings: &DashboardConfig, port: u16, token: &str) -> String`.
   When `settings.public_url` parses, it is `format!("{}{}/login#{token}", p.origin, p.base)`.
   Otherwise it is `format!("http://127.0.0.1:{port}/login#{token}")`.
2. `local_link` (:301) uses it. So do `cli.rs` :96 (`DashCmd::Link { remote: false }`)
   and :126 (the `None` branch), each with the loaded config's
   `dashboard`.
3. `door::link_text` (:22): when `dash.public().is_some()`, leave out the
   `ssh -L` hint.
4. `ferrule dashboard link` in managed mode with no `public_url` prints the
   link, then the line
   "This bot has no public address yet (FERRULE_PUBLIC_URL); open it from the panel."
5. A link minted with no host is accepted on the public host (`login`
   already uses `is_none_or`). Nothing to change.

**3.7 The panel token** (`panel.rs`, new; `mod panel;` in `mod.rs`)

1. Imports: `ring::hmac` and
   `ferrule_connections::seal::{b64, unb64, sha256_b64, write_private}`
   (URL_SAFE_NO_PAD).
2. Consts:
   ```rust
   pub const PREFIX: &str = "ferrule-panel.v1.";
   pub const REFUSED: &str = "that sign-in didn't work; open the bot again from the panel";
   /// 5 minutes, plus 30 s of clock skew.
   pub const MAX_AHEAD_SECS: u64 = 330;
   pub const NONCE_CAP: usize = 10_000;
   ```
3. `#[derive(Clone)] pub struct Key { secret: Vec<u8>, bot: String }`,
   with:
   - `pub fn new(secret: &[u8], bot: &str) -> Self`;
   - a hand-written `Debug` that prints only `bot`;
   - `pub fn from_managed() -> Option<Self>`: `None` unless
     `crate::managed::on()`, `crate::managed::panel_secret()` is `Some`,
     and `crate::managed::state().bot_id` is `Some`.
4. `#[derive(Debug, Deserialize)] pub struct Claims { pub bot: String, pub user: String, pub exp: u64, pub nonce: String }`.
5. `pub fn verify(token: &str, key: &Key, now: u64) -> Result<Claims, String>`.
   The `Err` text is for the log only, never the reply. The checks, in
   order:
   1. `strip_prefix(PREFIX)`, then `split_once('.')` into
      `(claims_b64, mac_b64)`, else `"not a panel token"`;
   2. `unb64(mac_b64)`, then
      `hmac::verify(&hmac::Key::new(hmac::HMAC_SHA256, &key.secret), format!("{PREFIX}{claims_b64}").as_bytes(), &mac)`
      (constant time), else `"bad signature"`;
   3. `serde_json::from_slice(&unb64(claims_b64)?)`, else
      `"malformed claims"`;
   4. `claims.bot != key.bot` gives
      `format!("a token for bot {}, and this is {}", claims.bot, key.bot)`;
   5. `exp <= now` gives `format!("expired {}s ago", now - exp)`;
      `exp > now + MAX_AHEAD_SECS` gives
      `format!("expires {}s ahead, more than {MAX_AHEAD_SECS}", exp - now)`;
   6. a nonce outside 16–128 chars gives `"a nonce must be 16 to 128 characters"`;
      a user outside 1–128 chars, or one with a control char, gives
      `"a bad user id"`.
6. `#[cfg(test)] pub fn sign(claims: &serde_json::Value, secret: &[u8]) -> String`:
   the same construction, for the unit tests.
7. `pub struct Nonces { path: PathBuf, cap: usize }`, with
   `pub fn at(path: PathBuf) -> Self` (cap `NONCE_CAP`), and
   `pub fn spend(&self, nonce: &str, exp: u64, now: u64) -> Result<(), String>`,
   which fails closed:
   1. `let _lock = crate::filewrite::Lock::take(&self.path)`, else
      `format!("the nonce file can't be locked: {e:#}")`.
   2. Read a JSON map `sha256_b64(nonce) → exp`. A missing file is an
      empty map. Any other read or parse error gives
      `format!("the nonce file can't be read: {e}")`.
   3. Drop the entries with `exp <= now`.
   4. When the key is already there: `"the nonce was used already"`.
   5. When `len() >= self.cap`:
      `format!("{} holds {} unexpired nonces; refusing panel sign-ins until some expire", self.path.display(), self.cap)`.
   6. Insert, `create_dir_all` the parent, and `write_private` the file,
      else `format!("the nonce file can't be written: {e:#}")`.
8. Tests in `panel.rs`:
   - `a_spent_nonce_survives_a_restart`: spend `n1`, build a new `Nonces`
     on the same path, and spending `n1` is `Err("the nonce was used already")`.
     Once its `exp` has passed (`now` moved past it), the entry is pruned.
   - `the_nonce_file_is_capped_and_fails_closed`: with `cap: 2`, the third
     spend is `Err` naming "unexpired nonces". A file holding `not json`
     gives `Err("the nonce file can't be read: …")`.

**3.8 Login** (`mod.rs` `login`, :519)

1. When the token starts with `panel::PREFIX`, return
   `self.panel_login(host, token)`.
2. `fn panel_login(&self, host: &str, token: &str) -> Response`:
   1. With no `self.panel`,
      `tracing::warn!("panel sign-in refused: no panel secret or bot id on this bot")`.
   2. `panel::verify(token, key, now_secs)`, then
      `self.nonces.spend(&claims.nonce, claims.exp, now_secs)`.
   3. Any `Err(why)` gives `tracing::warn!("panel sign-in refused: {why}")`
      and `refuse(401, panel::REFUSED)`.
   4. On success:
      `self.sessions.open(host, self.links.revoked_ms(), Some(&claims.user))`
      (a 503 on an error, as the link login does), then
      `tracing::info!(user = %claims.user, "panel sign-in")`.
   5. The same 200, `Set-Cookie` and `touch()` as the link login. The nonce
      is spent before the session opens.

**3.9 Tests**

1. In `mod.rs`'s `mod tests`, add a helper `fn panel_dash(public: &str) -> (TempDir, Arc<Dashboard>)`.
   It builds with `new_with`, a `DashboardConfig` whose `public_url` is
   `Some(public)`, and `panel::Key::new(SECRET, "b_test")`, where
   `const SECRET: &[u8] = b"test-panel-secret-0123456789abcdef0123";`.
   Each test posts to `/api/login` with `Host: bots.test` and
   `Origin: http://bots.test`.
   - `a_good_token_opens_a_session`: 200. The cookie has `Path=/b/b_test/`
     and no `Secure`. `GET /b/b_test/api/session` with the cookie gives
     `user == "u_1"`.
   - `an_expired_token_is_refused`, `a_bad_signature_is_refused` (signed
     with another secret), `a_replayed_nonce_is_refused` (the second use),
     `a_token_for_another_bot_is_refused` and
     `a_token_too_far_ahead_is_refused` (`exp = now + 600`): each 401, with
     `error == panel::REFUSED`.
   - `without_a_panel_key_panel_tokens_are_refused`: `new_with(…, None)`
     and a well-signed token give 401.
   - `the_page_works_under_a_path_prefix`: with
     `public_url = "https://bots.test/b/b_test/"`:
     - `GET /b/b_test/` carries the meta tag;
     - `GET /b/b_test/app.js` gives 200;
     - `GET /app.js` (a proxy that strips the prefix) gives 200;
     - the login cookie has `Secure` and `Path=/b/b_test/`;
     - a POST with `Origin: https://bots.test` passes the origin check, and
       one with `http://bots.test` gets 403 "wrong origin".
   - `a_non_loopback_bind_without_a_public_url_answers_health_only`: the
     settings `bind = "0.0.0.0"`, then `bind(0)`. `GET /` gives 421 with
     "has no public address", and `GET /healthz` is not 421. On a machine
     where binding 0.0.0.0 fails, skip with a message.
2. `tests/it/managed.rs`:
   - `a_panel_sign_in_works_under_the_prefix_through_the_gateway`:
     1. A managed gateway (the 2.17 setup) with
        `FERRULE_PUBLIC_URL=http://bots.test/b/b_test/`,
        `FERRULE_PANEL_SECRET=test-panel-secret-0123456789abcdef0123` and
        `FERRULE_BOT_ID=b_test`.
     2. Sign the token in the test with `ring::hmac` and `base64`
        URL_SAFE_NO_PAD (both are ferrule-cli dependencies).
     3. `http_as(port, "bots.test", "POST", "/b/b_test/api/login", …)` with
        `Origin: http://bots.test` gives 200.
     4. `GET /b/b_test/api/session` gives `user == "u_1"`.
     5. The same token again gives 401.
   - `#[cfg(target_os = "linux")] the_panel_secret_is_not_passed_to_children`:
     the same gateway with a policy of `sandbox = "container"` and a
     `[[mcp.servers]]` entry running `sleep 30` (it only has to live).
     Wait until a child of the gateway's PID shows in
     `/proc/*/stat` (ppid), then read its `/proc/<pid>/environ`. It
     contains `FERRULE_BOT_ID=` and not `FERRULE_PANEL_SECRET`. If no child
     starts within 20 s, fail with the gateway's log.
3. Verify: `cargo test -p ferrule-cli --bin ferrule dashboard::` and
   `cargo test -p ferrule-cli --test it managed::`.

### Part 4 — the dashboard's gaps

One commit: `M44 part 4: the dashboard's gaps — Telegram from the page, the HTTP API's bind`.

**4.1 What is already there** (checked, no code)

| Need | Where | Picked up |
|---|---|---|
| A model API key | `models/provider` (models_page.rs) → `secrets.env` + `[providers.*]` | live: the registry and the secrets are followed (M17/M21) |
| The default model | `models/default` | live |
| The ChatGPT plan | `plans/chatgpt/start|poll|cancel` | live; 403 when the policy forbids `chatgpt` (2.16) |
| The HTTP API on | the `http` card (`channels/save`) | at the restart: channels are built once at start |
| An HTTP API key | `channels/keys/add` | live: `clients.json` is read per request |
| **A Telegram token** | **missing**: its card says "`ferrule setup`" | at the restart |

The Telegram flow below is the gap. The HTTP API only needs a bind (4.9).

**4.2 `telegram.rs`** (new; `mod telegram;` in `mod.rs`)

1. The routes, in api.rs's POST match next to the `channels/*` arms (:160):
   ```rust
   "telegram/test" => super::telegram::test(ctx, body).await,
   "telegram/save" => super::telegram::save(ctx, body),
   "telegram/wait" => super::telegram::wait(ctx, body).await,
   "telegram/allow" => super::telegram::allow(ctx, body).await,
   ```
2. Shared, in each function first:
   - The policy: when `crate::managed::policy()` is `Some(p)` and
     `!p.allows_channel("telegram")`:
     `bad(403, format!("channel `telegram` is not allowed on this bot: {}", p.why()))`.
   - `token` from `body["token"]`, trimmed. The page sends it on every
     call. Empty gives `bad(400, "paste the bot token first")`. When
     `!probe::plausible_bot_token(token)`:
     `bad(400, "that isn't a bot token (digits:letters)")`.
   - `base` is `super::api::config(ctx).and_then(|c| c.gateway.telegram_base_url.clone())`: it is a `String` with a
     serde default already, so there is no fallback to write. The client is `probe::client()`, and the probe is
     `probe::Telegram { http: &client, base_url: &base, token }`.
3. `test`:
   - `get_me()` gives the `name`;
   - then `webhook()`, and on `Some(_)`, `delete_webhook()`;
   - `ok(json!({ "ok": true, "said": format!("Works: @{name}"), "name": name }))`;
   - a `Check` error gives `ok(json!({ "ok": false, "said": ctx.redactor.redact(&e.to_string()) }))`.
4. `save`:
   1. `env` is the config's `gateway.telegram_token_env`, else
      `"TELEGRAM_BOT_TOKEN"`.
   2. `let place = need!(setup_place(ctx));`, then
      `crate::secrets::set(&place.secrets, &env, token)`.
   3. `let mut t = crate::setup::Target::load(place.config.clone())?;`,
      `put(table(t.root(), &["gateway"])?, "telegram_token_env", env)`,
      then `t.save()`. Errors give `bad(500, format!("{e:#}"))`.
   4. `super::channels::audit(ctx, "channel.saved", "telegram")`: make
      `channels.rs`'s `fn audit` (:284) `pub(super)`.
   5. The answer has the same shape and texts as `channels::save` (:155):
      `{ok, said, restart: ctx.live.is_some()}`, with the title
      "Telegram".
5. `wait`:
   1. When `ctx.live` has a channel named `"telegram"`:
      `bad(409, "Telegram is running, so this page can't read its messages: add chats by id instead")`.
   2. `updates(body["offset"].as_i64(), 25)`. A `Check::Conflict` gives the
      same 409. Any other error gives `ok({ok: false, said})`.
   3. `let (next, chats) = probe::seen_chats(&updates);`. Answer
      `{"ok": true, "chats": [{"id", "kind", "name"}], "next": next}`.
6. `allow`:
   1. `id` from `body["chat"].as_i64()`, else `bad(400, "which chat?")`.
   2. When `body["next"]` is set: `updates(Some(next), 0)` confirms the
      batch, so the gateway won't see those messages again.
   3. Read `gateway.telegram_allowed_chats` from the config. When it lacks
      `id`, append it and call `crate::setup::save_allowed(&mut t, &ids)`.
      Make `save_allowed` (setup.rs :1911) `pub(crate)`.
   4. `send(id, "✅ Connected: this chat can talk to your ferrule agent.")`.
      A failure there is a note in the answer, not an error.
   5. Answer `{ok: true, said: "Chat {id} is allowed. Restart the bot to start Telegram.", restart: ctx.live.is_some()}`.
7. `setup.rs`: make `Target`, `Target::load`, `root`, `save`, `table` and
   `put` reachable from `dashboard::telegram` (`pub(crate)`) if they
   aren't already.
8. `channels.rs` `remove` (:178): when the channel has no form and
   `crate::managed::on()`, the text becomes
   `format!("{} can't be taken out on this page yet; a new token replaces the old one", info.title)`
   (setup is off on a managed bot).

**4.3 The page** (`app.js`)

1. In `sections.channels.card(x)`, when `!x.form && x.name === "telegram"`,
   append `this.telegram(x)` in place of the "`ferrule setup`" line.
2. `telegram(x)` draws, with the file's `el`/`act` idiom:
   1. a password input "Bot token (from @BotFather)";
   2. **Test**, which calls `telegram/test` and shows `said`;
   3. **Save**, which calls `telegram/save`;
   4. **Wait for a message**, which shows "Send any message to your bot
      now…". It calls `telegram/wait` in a loop, passing back `next` as
      `offset`, for up to 2 minutes, until `chats` is non-empty. Then it
      lists each chat with an **Allow** button, which calls
      `telegram/allow` with `{chat, next}`. On a 409 it shows the error
      and stops;
   5. when an answer has `restart: true`, a **Restart** button that calls
      the existing `gateway/restart` action.

**4.4 Tests**

1. In `tests/it/dashboard.rs`'s `telegram_serve` (:261), before the
   generic branch:
   - `getMe` answers `{"ok":true,"result":{"id":123456,"is_bot":true,"username":"m44_bot"}}`;
   - `getWebhookInfo` answers `{"ok":true,"result":{"url":""}}`;
   - `deleteWebhook` answers `{"ok":true,"result":true}`.

   None of them is pushed to `sent`.
2. `tests/it/channels.rs`: make `model_server` `pub(super)`.
3. `tests/it/managed.rs`: `a_telegram_token_is_tested_saved_and_a_chat_allowed`.
   1. A managed gateway, `sandbox = "container"`, with a config written
      by the test: the echoing model (`model_server`),
      `[gateway] telegram_base_url = "<fake>"`, and no telegram token.
   2. Log in, then post to `telegram/test` with
      `"123456:TESTtokenTESTtokenTEST00"`: `ok` and `name == "m44_bot"`.
   3. `telegram/save`: `restart == true`. `ferrule.toml` has
      `telegram_token_env`, and `secrets.env` has the token.
   4. Queue a message from chat 4242. `telegram/wait` lists chat 4242.
   5. `telegram/allow` with `{chat: 4242, next}`: the config has 4242 in
      `telegram_allowed_chats`, and the fake got a `sendMessage` to 4242
      containing "✅ Connected".
   6. A token `"nope"` gives 400.

   The restart itself is covered by 5.11. The message then reaching the
   model is covered by the existing channel tests.

**4.5 The HTTP API's bind** (the image must reach it from the proxy)

1. `HttpApi` (`crates/ferrule-cli/src/channels/settings.rs` :206) gains
   `/// Where it listens (default 127.0.0.1; FERRULE_HTTP_BIND).`
   `#[serde(default)] pub bind: Option<String>`.
2. ferrule-gateway's `HttpConfig` (`channels/http/mod.rs` :48) gains
   `pub bind: std::net::IpAddr`. Its `Default`, if it has one, uses
   `127.0.0.1`.
3. ferrule-cli `channels/http.rs` `config` (:40): the env
   `FERRULE_HTTP_BIND` first, then `settings.bind`, then `127.0.0.1`. A
   value that isn't an `IpAddr` gives
   `bail!("FERRULE_HTTP_BIND / [gateway.http] bind must be an IP address like 0.0.0.0, not `{v}`")`.
4. ferrule-gateway `tests/it/http.rs` (:24) sets `bind: [127, 0, 0, 1].into()`.
5. `server.rs`: bind `(cfg.bind, cfg.port)` at :31. The error at :33, the
   `info` at :39 and `note()` at :625 print the IP where they print
   `127.0.0.1` today.
6. `Dockerfile`: add `FERRULE_HTTP_BIND=0.0.0.0` to the ENV block
   (:52–58), and `EXPOSE 8788` next to `EXPOSE 8080`. The same env goes in
   2.17's `env_remove` list (already listed).
7. Test in ferrule-cli `channels/http.rs`:
   `the_http_bind_comes_from_the_settings_and_is_checked`. The choice is a
   small `bind_of(env, setting)` that `config` calls, so the test hands it
   both values and never sets the env var. It covers `"0.0.0.0"`, the env
   winning over the file, and `"all"`, `"localhost"`, `"0.0.0.0:80"` and
   `"300.1.1.1"` refused. The card's `check` refuses the same values before
   they are saved.

**4.6 How each change is picked up** (goes into `docs/docker.md`, 7.1)

Use the 4.1 table and the reason. Channels are built once at start. A live
add would be a new path through the router, the health monitor and the
owner doors. The re-exec restart (5.8) takes about a second, keeps the
PID, and re-reads `secrets.env`, so a new token is in the env it starts
with.

### Part 5 — health and lifecycle

One commit: `M44 part 5: health and lifecycle — /healthz, /busyz, ferrule health, a bounded SIGTERM drain, re-exec restarts, backup and restore in a container`.

**5.1 The judgement** (`crates/ferrule-cli/src/dashboard/healthz.rs`)

1. A pure core:
   ```rust
   pub struct Facts {
       pub dispatch_stuck: Option<Duration>,
       /// Each configured channel (the dashboard's chat left out) and its problem, if any.
       pub channels: Vec<(String, Option<String>)>,
       pub stale: Vec<String>,
       pub no_model: bool,
       pub kill: Option<String>,
       pub on_copy: bool,
       pub commands_off: Option<String>,
       pub closing: bool,
       pub managed: bool,
       pub turns: usize,
       pub queued: usize,
       pub uptime_secs: u64,
   }
   pub fn judge(f: &Facts) -> (&'static str, Vec<String>)
   ```
2. `failing`:
   - `dispatch_stuck` is `Some(d)`:
     `format!("the gateway's dispatch loop has been stuck for {}s", d.as_secs())`;
   - `channels` is non-empty and every one has a problem:
     `format!("every channel has a problem: {}", …)`, each as `name: problem`.
3. `degraded`, in this order:
   - `no_model`: "no model is set up yet";
   - `channels` is empty: "no channel is set up yet";
   - each channel's problem: `format!("{name}: {problem}")`;
   - each stale channel: `format!("{name} has gone quiet")`;
   - `on_copy`: "the config has an error, so the gateway runs on its last good copy";
   - `kill`: `format!("the kill switch is on: {k}")`;
   - `commands_off`: `format!("commands are off: {why}")`;
   - `closing`: "the bot is stopping".
4. `ok` when there are no reasons at all.
5. Sources, in `fn facts(d: &Dashboard) -> Facts`. Everything is read from
   memory, with no model call and no disk write.
   - `live.health.dispatch_busy_for()` (health.rs :601), when it is more
     than `ferrule_gateway::health::DISPATCH_STUCK`;
   - `live.channels`, minus `super::chat::CHANNEL`, with `c.problem()`;
   - `live.health.stale_channels(&live.channels)` (:607);
   - `no_model`: `ctx.models` is `None`, or it has no default row with a
     key present. Use the models module's existing "has a default and a
     key" check, whatever it is called;
   - `kill`: `ctx.hub.as_ref().and_then(|h| h.stopped()).map(|s| s.by)` (hub.rs :631);
   - `on_copy`: `crate::last_good::on_copy().is_some()` (:53);
   - `commands_off`: `live.commands_off.clone()` (2.15);
   - from `live.router.upgrade()`: `running()`, `queued()` and `closing()`
     (5.4);
   - `managed`: `crate::managed::on()`;
   - `uptime_secs`: `d.started.elapsed()`.

   Without `ctx.live` (the dashboard outside a gateway): `failing`,
   "the gateway isn't running".
6. `healthz(d, req)`: GET or HEAD only, else 405.
   - The body:
     `{"status", "reasons", "version": env!("CARGO_PKG_VERSION"), "managed", "busy": turns + queued > 0, "turns", "queued", "uptime_secs"}`;
   - every reason goes through `d.ctx.redactor.redact`;
   - the status code is 503 for `failing`, else 200;
   - the header `Cache-Control: no-store`.
7. `busyz(d, req)`: `200 {"busy": false}` when `turns + queued == 0`,
   else `409 {"busy": true, "turns", "queued"}`.
8. Unit tests in `healthz.rs`:
   - `judge_says_ok_degraded_and_failing`: no reasons gives ok; no model
     gives degraded; a stuck dispatch gives failing; two channels, both
     with problems, give failing; one of two gives degraded.
   - `health_answers_before_the_host_check_and_without_a_session` (in
     `mod.rs`'s tests): `GET /healthz` with `Host: evil.test` is not 421
     or 401, and gives `version`.

**5.2 `ferrule health`** (main.rs)

1. `Cmd::Health { /// One line, and exit 1 unless ok or degraded. #[arg(long)] probe: bool }`,
   with the doc "How the running gateway is: /healthz, no model call".
   Dispatch it to `dashboard::cli::health(probe)`.
2. The port, in this order:
   1. `dashboard::cli::gateway_port()` (:60);
   2. the env `FERRULE_DASHBOARD_PORT`;
   3. the loaded config's `dashboard.port` when non-zero;
   4. else `bail!("no gateway is running here (no dashboard port found)")`.
3. A raw `std::net::TcpStream` to `127.0.0.1:{port}` with a 3 s
   connect/read/write timeout. Use no HTTP client: the OneCLI proxy and
   `HTTP_PROXY` must not apply. Send
   `GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n`,
   then read the status line and the JSON body.
4. Output:
   - `--probe` prints one line: `{status}` or `{status}: {reasons joined by "; "}`;
   - without `--probe`: the same line, then
     `version {v}, up {n}s, {turns} turn(s) running, {queued} queued`.
   - When nothing answers:
     `failing: nothing answers on 127.0.0.1:{port}: {e}`, and exit 1.
   - Exit 1 when the status is `failing`, else 0.
5. `dashboard/console.rs`: add `leaf("health", Read, "console", "")` to
   `PARITY`, next to `status`.
6. Integration test in `tests/it/health.rs`:
   `ferrule_health_reads_the_running_gateway`. A gateway with no model
   gives exit 0, and stdout starts with `degraded: no model is set up yet`.
   After the gateway is killed, the result is exit 1 and "failing".

**5.3 `/busyz` and `/healthz` in the gateway test**

In `tests/it/managed.rs`, `health_and_busy_answer_on_the_bind_address`:
with `FERRULE_DASHBOARD_BIND=127.0.0.1` and a
`FERRULE_PUBLIC_URL=http://bots.test/b/b_test/`:
- `GET /healthz` with any Host gives 200 and `managed == true`;
- `GET /b/b_test/healthz` gives the same;
- `GET /busyz` gives 200 `{"busy": false}`.

**5.4 The router drains** (`crates/ferrule-gateway/src/router.rs`)

1. `Router` (:58) gains `closing: AtomicBool`, `false` in its
   constructor.
2. New methods:
   ```rust
   /// No new message from now on: the queued ones are dropped, and their chats told `notice`.
   pub async fn close(&self, notice: &str) -> usize
   pub fn closing(&self) -> bool
   /// Lanes running a turn.
   pub fn running(&self) -> usize
   /// Messages waiting behind running turns.
   pub fn queued(&self) -> usize
   /// Ends every running turn, as the dashboard's Stop does; how many.
   pub fn stop_all(&self, by: &str) -> usize
   pub async fn drain(&self, notice: &str, grace: Duration) -> Drained
   ```
   with `#[derive(Debug, Default, PartialEq, Eq)] pub struct Drained { pub notified: usize, pub stopped: usize, pub left: usize }`.
3. `close`:
   1. Set `closing`.
   2. Under the lanes lock, for each lane: set `retired = true`; when
      `queued > 0`, keep `(channel, chat_id)` and zero `queued`.
   3. After the lock: send each kept chat
      `OutboundMessage { channel, chat_id, text: notice.into(), reply_to: None, attachments: vec![] }`
      through `self.channels`, with a 3 s `tokio::time::timeout` each.
   4. Return how many were told. `retired` already makes `run_lane` drop
      its queued jobs (:641).
4. `lane_for` (:400) returns
   `Err(GatewayError::SessionClosed(session_id.to_string()))` first when
   `closing`. That covers its three callers (:334, :354 and :380).
   `wake` (:545) returns `false` when `closing`.
5. `running()` counts lanes whose `busy_since` is `Some`, and `queued()`
   sums `queued`.
6. `stop_all(by)` calls `guard.stop(by)` on each busy lane: the same call
   the per-session `stop` (:495) makes.
7. `drain`:
   1. `notified = self.close(notice).await`.
   2. Until `running() == 0` or `grace` has passed: loop on
      `tokio::select! { _ = self.changed.notified() => {}, _ = tokio::time::sleep(Duration::from_millis(100)) => {} }`.
   3. `stopped = self.stop_all("a bot restart")`.
   4. Then the same wait, for up to 5 s.
   5. `left = self.running()`.
8. Tests in router.rs `mod tests`, reusing its fake channel and slow agent:
   - `closing_refuses_new_messages_and_tells_queued_chats`: one turn runs
     and one message is queued. After `close("N")`, the queued chat got
     "N", and a new `dispatch` is `Err(SessionClosed)`.
   - `drain_lets_a_short_turn_finish`: a 200 ms turn and a 2 s grace give
     `stopped == 0`, `left == 0`, and the normal reply.
   - `drain_stops_a_turn_that_outlives_the_grace`: a 10 s turn and a
     300 ms grace give `stopped == 1` and `left == 0`, and the chat's
     reply starts "Stopped from a bot restart: I ended this turn.".

**5.5 The channels stop taking messages** (`crates/ferrule-gateway/src/gateway.rs`)

`ChannelRestarts` (:50) gains `pub fn stop_all(&self) -> usize`, which
aborts every handle it holds and returns how many. The channels
themselves stay in the router, so replies can still be sent.

**5.6 `lifecycle.rs`** (`crates/ferrule-cli/src/lifecycle.rs`)

```rust
pub const NOTICE: &str = "The bot is restarting; send that again in a minute.";
/// The env and args as `main` got them, before any secret was taken out.
pub fn snapshot_env()
pub fn snapshot() -> &'static (Vec<(OsString, OsString)>, Vec<OsString>)
pub async fn drain(router: &ferrule_gateway::Router, grace: Duration) -> Drained
/// Managed mode's restart: the drain, then a re-exec of this binary.
pub fn request_restart()
pub fn restart_requested() -> bool
#[cfg(unix)] pub fn reexec() -> !
```
1. `snapshot_env` fills a `static SNAP: OnceLock<…>` with
   `std::env::vars_os()` and `std::env::args_os()`.
2. `drain`:
   1. log `tracing::info!("stopping: {} turn(s) running, {} queued; up to {}s", …)`;
   2. `router.drain(NOTICE, grace)`;
   3. log the `Drained` counts.
3. `request_restart`:
   - set a `static RESTART: AtomicBool`;
   - on unix, `libc::kill(libc::getpid(), libc::SIGTERM)`, the same
     `SAFETY` comment as `terminate_self`;
   - elsewhere, `std::process::exit(0)`.
4. `reexec`:
   1. `let (env, args) = snapshot();`;
   2. build
      `std::process::Command::new(if cfg!(target_os = "linux") { "/proc/self/exe".into() } else { std::env::current_exe()? })`,
      with `.arg0(&args[0]).args(&args[1..]).env_clear().envs(env.iter().cloned())`;
   3. call `std::os::unix::process::CommandExt::exec()`, which only
      returns on an error;
   4. on that error, `eprintln!("the restart couldn't start ferrule again: {e}")`
      and `std::process::exit(1)`.
5. Wiring in `main()`:
   1. `lifecycle::snapshot_env();` where 2.6 step 1 put it: before
      `take_exported()`, so the snapshot still holds the panel secret and
      the exported vars.
   2. In the "ferrule-main" thread:
      ```rust
      let rt = /* the existing builder */.build()?;
      let out = rt.block_on(dispatch(cli.cmd));
      if lifecycle::restart_requested() { rt.shutdown_timeout(Duration::from_secs(5)) } else { drop(rt) }
      out
      ```
   3. After `telemetry::shutdown()` (:681):
      `#[cfg(unix)] if lifecycle::restart_requested() && done.is_ok() { lifecycle::reexec() }`.
      Use the real name of the result variable. The listeners are
      `CLOEXEC` and the runtime is down, so the ports are free for the new
      image.

**5.7 SIGTERM in `run_gateway`** (main.rs, the `select!` at ~:2537)

1. Before `Gateway::new(router)` (:2483): `let drain_router = router.clone();`
   and `let stopper = restarts.clone();`. Use the real names of the router
   `Arc` and the `ChannelRestarts`.
2. The signal arm becomes:
   ```rust
   why = shutdown_signal() => {
       tracing::info!("{why}: shutting down");
       stopper.stop_all();
       scheduler_handle.abort();
       tokio::select! {
           _ = lifecycle::drain(&drain_router, cfg.gateway.stop_grace()) => {}
           again = shutdown_signal() => {
               tracing::warn!("{again} again: stopping now");
               health.shutdown();
               std::process::exit(0);
           }
       }
       Ok(())
   }
   ```
   The existing `scheduler_handle.abort()`, `health.shutdown()`,
   `remove_marker` and `drop(dash)` after it stay.
3. Integration test in `tests/it/managed.rs`, `#[cfg(unix)]`:
   `sigterm_drains_and_exits_zero`. A managed gateway gets
   `libc::kill(pid, SIGTERM)`. It exits with status 0 within 30 s, the log
   has "SIGTERM: shutting down" and "stopping:", and `dashboard.json` is
   gone.

**5.8 The restart triggers**

1. `gateway_restart` (api.rs :1097):
   - when `crate::managed::on()`, the confirm text is
     "Restart the bot? Running turns get a few seconds to finish, then they stop; reload this page in a few seconds.";
   - the spawned task calls `crate::lifecycle::request_restart()` in
     place of `terminate_self()`.
2. `last_good::watch` (:137): where it calls `exit(0)` (:149), call
   `crate::lifecycle::request_restart()` when managed.
3. `last_good::supervised()` (:129) returns true when
   `crate::managed::on()`.
4. Integration test, `#[cfg(target_os = "linux")]`:
   `a_managed_restart_runs_again_in_the_same_process`.
   1. A managed gateway. Log in, and read `uptime_secs` from `/healthz`.
   2. POST `/api/gateway/restart` with the confirmation, and wait for
      `/healthz` to answer again with a smaller `uptime_secs`.
   3. The test's `Child` for the gateway has not exited
      (`try_wait() == None`): the same PID.
   4. The log shows the drain, "gateway starting" twice, and no "the
      restart couldn't start".

**5.9 Backup: a config inside the data dir is stored once** (`crates/ferrule-cli/src/backup.rs`)

In the walk (:102), skip `data/<rel>` when its canonical path equals the
canonical `config::config_path()`. The config is stored once, under
`config`. Test in `tests/it/backup.rs`:
`a_config_inside_the_data_dir_is_stored_once`. With
`FERRULE_CONFIG=<data>/ferrule.toml`, the archive has one entry for it,
and a restore brings it back.

**5.10 Restore into an empty data dir, in place** (`backup.rs`)

Why: in the image, `/data` is a mount point and `/` isn't writable by uid
10001. Today's restore calls `tempdir_in(parent)` and renames `data`
aside, and neither works there.

1. When `data` exists, is a directory and is empty:
   1. stage with `tempfile::Builder::new().prefix(".restore-").tempdir_in(&data)`;
   2. unpack there;
   3. move each entry of `<staging>/data` into `data`;
   4. write the staged config to `config_to` (the same as today);
   5. drop the staging dir. No `.pre-restore-` copy is made (there was
      nothing to keep).
2. Any other case keeps today's flow.
3. Test `#[cfg(unix)] a_backup_restores_into_an_empty_data_dir_in_place`.
   Make a backup. Then make an empty `data` whose parent is `chmod 0555`.
   The restore succeeds; the files are there; no `.pre-restore-` sibling
   and no `.restore-` dir are left. Put the parent back to `0755` before
   the temp dir is dropped. Skip when running as root, where the chmod
   doesn't bind.

**5.11 The re-exec picks up a saved channel**

Extend 4.4's test after the allow step, Linux only:
1. POST `gateway/restart`, then wait for `/healthz` to answer again.
2. Queue a Telegram message from chat 4242.
3. The fake gets a `sendMessage` to 4242 with "ECHO" in it.

This proves that the token saved on the page reaches the channel through
the re-exec, and that the gateway starts chatting with no terminal.

### Part 6 — measurements

One commit: `M44 part 6: measurements — …`. Numbers go into §9 and
`docs/docker.md`, each with how it was measured. All of it is local, with
no network except the browser page (6.4).

**6.1 The script** (`scripts/m44-measure.sh` + `scripts/m44_measure.py`)

1. `m44_measure.py` (Python 3, stdlib only) runs:
   - a fake OpenAI-compatible model on `127.0.0.1:0`. Its first reply is a
     `shell` tool call with `{"command": "echo m44 && head -c 20000000 /dev/zero | wc -c"}`;
     its second is the text "done". Non-streaming (`[agent] stream = false`);
   - a fake Telegram (`getUpdates` returns nothing; everything else ok).
   The eval's mock model is not used or changed.
2. `m44-measure.sh` builds `ferrule` in release
   (`cargo build --release -p ferrule-cli`, the shared
   `CARGO_TARGET_DIR`). It writes a temp home with
   `[managed]`-less config:
   - the fake model;
   - `[gateway] telegram_token_env`, `telegram_base_url` and one allowed chat;
   - `[gateway.http]` on a free port;
   - `[sandbox] mode = "auto"`.

   Then it starts `ferrule gateway`, and makes an HTTP key with
   `ferrule channels keys add m44`.
3. Measured from `/proc/<pid>/status`:
   - **idle**: `VmRSS` 30 s after `/healthz` first answers, with Telegram
     polling and the HTTP API listening;
   - **mock-turn peak**: reset the high-water mark with
     `echo 5 > /proc/<pid>/clear_refs` (if refused, restart the gateway
     and read `VmHWM` after one turn). Then one
     `POST /v1/messages {"text": "run it"}`, and read `VmHWM`. Children
     (the shell) are measured separately: sample the sum of
     `VmRSS` over the process tree every 50 ms during the turn and keep
     the peak.
4. It prints a small Markdown table: idle RSS, turn peak (gateway),
   turn peak (tree), and the binary size (`stat -c %s` of the release
   binary).

**6.2 Image sizes** (no Docker daemon here)

1. Build the static binary the image uses:
   `rustup target add x86_64-unknown-linux-musl` (if missing) and
   `cargo build --release --target x86_64-unknown-linux-musl -p ferrule-cli`,
   in the shared `CARGO_TARGET_DIR`. If the musl toolchain isn't there
   (`musl-gcc` / the `ring` build fails), use the glibc release binary's
   size and say so.
2. The base image's compressed size: GET the registry manifest for
   `debian:bookworm-slim` (linux/amd64) and sum its layer sizes. If that
   fails, cite the size from Docker Hub's page and say so.
3. The `-browser` variant: add the installed size of the Chromium and
   agent-browser packages from `apt-cache show chromium` (`Installed-Size`
   of chromium, chromium-common and their non-slim deps). Label it
   "estimated".
4. Every image number is labelled **estimated, not built**.

**6.3 Bots per 16 GB, idle**

`floor((16 GiB − 1.5 GiB for the OS, Docker and page cache) / (idle RSS + 8 MiB of container overhead))`,
with each assumption written on the line:
- no browser open (the browser closes when idle);
- no turn running;
- the OS reserve.

A second line gives the count with one turn running in 10% of the bots.

**6.4 The browser**

1. `FERRULE_AGENT_BROWSER=/pnpm/agent-browser`,
   `FERRULE_REQUIRE_BROWSER_TEST=1`, then run ferrule-mcp's
   `the_agent_drives_a_real_chrome_inside_the_sandbox` once with
   `--chrome-sandbox off` and once with the default, recording what each
   does. Here, the default fails (no user namespaces under Docker's
   seccomp, §9.1).
2. Peak tree RSS on a real page:
   - a one-off `#[ignore = "network: FERRULE_AGENT_BROWSER=… cargo test -p ferrule-mcp --test it browser_peak -- --ignored --nocapture"]`
     test, `browser_peak_rss_on_a_real_page`, in `tests/it/browser.rs`.
     It opens `https://en.wikipedia.org/wiki/Rust_(programming_language)`,
     samples the tree RSS every 100 ms, and prints the peak;
   - after `idle_timeout_secs = 5` (or `AGENT_BROWSER_IDLE_TIMEOUT_MS=5000`)
     it prints the tree RSS again and asserts that no `chromium` process
     is left.
3. Check that the profile is under `<data>/mcp/browser/profile` after
   the run.

### Part 7 — docs

One commit: `M44 part 7: docs — …`.

**7.1 `docs/docker.md`** (new)

1. **Run one bot**: the full `docker run` with `--name`, `-v bot1:/data`,
   `-v /etc/ferrule/b1.toml:/etc/ferrule/policy.toml:ro`, and the env:
   `FERRULE_POLICY`, `FERRULE_BOT_ID`, `FERRULE_PANEL_SECRET`,
   `FERRULE_PUBLIC_URL`. Also `-p 127.0.0.1:8080:8080`,
   `--stop-timeout 30`, `--memory`, `--cpus`, `--pids-limit 512`,
   `--security-opt no-new-privileges`, `--cap-drop ALL`,
   `--read-only --tmpfs /tmp`, and `--restart unless-stopped`.
2. **Every env var** the image reads, in a table.
3. **The sandbox table**, one row per runtime: Docker's default seccomp,
   `--security-opt seccomp=unconfined`, gVisor (`--runtime=runsc`), and
   rootless Podman. For each: Landlock, the seccomp filter, Unix sockets,
   Chrome's own sandbox, and the policy to use.
   - Only the row for the default profile is **measured** (§9.1).
   - The others are **from the documentation, not verified**, and are
     marked so. gVisor doesn't implement Landlock, so use
     `sandbox = "container"` there.
4. **The policy file**: every key, with an example for each of
   `sandbox = "os"` and `"container"`.
5. **Behind a proxy**:
   - an nginx `location /b/b_4f2a/` example with
     `proxy_set_header Host $http_host;` and
     `proxy_set_header X-Forwarded-Proto $scheme;`;
   - the proxy keeps or strips the prefix, and both work;
   - block `/healthz` and `/busyz` from the public side if you want;
   - subdomains are recommended (the shared-origin note from §4.1).
6. **Panel sign-in**: the token format from §4.2, a 20-line Python example
   of signing, and why a secret must be per bot.
7. **Never mount the Docker socket**: the Unix-socket allowlist isn't
   enforced under the default profile.
8. **Signal** needs an external signal-cli daemon (`http_url`).
9. **Health, busy and stopping**: `/healthz`, `/busyz`,
   `--stop-timeout 30` and the drain.
10. **How a change is picked up**: the 4.6 table.
11. **Backup and restore**:
    - `docker exec b1 ferrule backup -o /backup/b1.tar.gz` with a
      `/backup` mount;
    - restore into a fresh volume with
      `docker run --rm -v newvol:/data -v <dir>:/backup:ro <image> restore /backup/b1.tar.gz`;
    - the image's entrypoint is `ferrule`, so `restore …` is the command.
12. **Try it locally**: `FERRULE_PUBLIC_URL=http://localhost:8080/` and
    `docker run -p 8080:8080 …`, then `docker exec … ferrule dashboard link`.
13. **The measured sizes and memory** from Part 6.

**7.2 Other docs**

1. `docs/dashboard.md`: `bind`, `public_url` and the path prefix; panel
   sign-in (a link to docker.md); the Telegram card; `/healthz` and
   `/busyz`.
2. `docs/sandbox.md`: managed mode's refusal, the policy's `sandbox` key,
   and a link to the docker.md table.
3. `docs/configuration.md` (or wherever `[dashboard]` and `[gateway]` keys
   are listed): `[dashboard] bind`, `public_url`,
   `[gateway] stop_grace_secs`, `[gateway.http] bind` and `[managed]`.
4. `docs/channels.md`: the HTTP API's `bind` and `FERRULE_HTTP_BIND`.
5. `docs/m44-managed-mode.md` §9: what each part verified, how, and what
   wasn't verified.

**7.3 PLAN.md and `docs/roadmap.md`**

1. PLAN.md: after the M41 bullet (~:825–840) and before
   "  - Also standing:", add one bullet:
   `  - **M44 managed mode**: **built** (2026-09-30, branch `m44-managed-mode`, PR to main open, not merged). …`,
   in the M41 bullet's style: what it is, then the design doc link.
2. PLAN.md's session log, at the end:
   `### 2026-09-30 — M44 managed mode (Devi, Opus 5.5 plan / Sonnet 5.5 build)`,
   with paragraphs headed **Scope.**, **What was built.**, **Tests.**
   (before and after counts), **Eval.**, **Not verified live.**,
   **Decisions for Max.** and **Follow-ups.**
3. `docs/roadmap.md`: `### M44 — managed mode` after `### M41 — daily use`
   (:905) and before "## Other open tracks" (:924), with **Status.** and
   **Done means.**, in M41's style.
4. Nothing else in either file changes.

### Final

1. Run all of Conventions step 2. Record the count.
2. `ferrule eval run evals/starter --variant ab` with the release binary.
   Expect engineered 20/20, naive 11/20, $0.98. Anything else is a
   regression to find before pushing.
3. `GIT_SSL_CAINFO=/tmp/onecli-combined-ca.pem git fetch origin`, then
   `git merge origin/main`.
   - In a PLAN.md or roadmap conflict, keep both sides.
   - Then run step 1 again.
4. `git push -u origin m44-managed-mode`, using the same `GIT_SSL_CAINFO`.
   Push no tag and no image.
5. Open the PR with
   `curl -s -X POST https://api.github.com/repos/maximarhipkin/ferrule/pulls -d @body.json`.
   Send no auth header: the proxy adds it.
   - The body is `{"title": "M44: managed mode — one bot per container, run by a panel", "head": "m44-managed-mode", "base": "main", "draft": false, "body": …}`;
   - the PR body is plain text with no "Generated with" line.
6. Poll CI through the REST API (`/repos/…/actions/runs?branch=m44-managed-mode`)
   for up to 40 minutes, using `until …; do timeout 60 tail -f /dev/null; done`.
   - Fix what is red on Linux, macOS and Windows, in batched commits, and
     push again.
   - Don't merge, and don't push to main.
7. The final report, in plain text:
   - what was built;
   - the decisions taken alone (the list in §0, plus this plan's);
   - what isn't verified live (§9);
   - the PR URL and the CI state;
   - the eval report;
   - the test count before and after;
   - the README lines to add (not added);
   - that the dashboard's `app.js`, `app.css` and `index.html` were edited,
     and no font or image;
   - the follow-ups: Telegram removal on the page, image signing, gVisor
     and seccomp rows verified on a real host, and a per-bot memory limit
     from the panel.
