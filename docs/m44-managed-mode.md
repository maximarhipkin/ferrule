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

| Runtime | Landlock | seccomp filter | Chrome's own sandbox |
|---|---|---|---|
| Docker, default seccomp profile | works (ABI 8 on kernel 7.0) | works | no: user namespaces are blocked (`unshare` needs `CAP_SYS_ADMIN` under the default profile) |
| Docker, custom profile allowing `clone`/`unshare` with `CLONE_NEWUSER` | works | works | expected to work; not verified here (no Docker daemon) |
| gVisor (`runsc`) | not verified here; gVisor implements its own syscall table and ferrule's start-up probe decides | not verified | not verified |
| Kernel without Landlock (< 5.13 or not in `lsm=`) | no | works | as above |

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
even when the config says on; the env is the panel's).

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
| `ferrule setup` service install, uninstall and refresh | refused | There's no systemd in the container; the container runtime is the service. |
| SSH workspaces (`ssh:`, `[ssh.*]`, `ferrule ssh`) | refused | A bot would hold keys to other machines, outside the container boundary. |
| The Claude plan (`ferrule login claude`, the dashboard's Claude button, a `claude-code` provider) | refused, with the reason in words | §0. |
| Terminal `ferrule setup` | not needed | §5: everything is on the dashboard; a first start writes a starter config. |
| Gateway restart | re-executes itself (§6.4) | There's no service manager to start it again. |
| The dashboard | always on; `remote = "tunnel"` is off | The panel's reverse proxy is the way in; no cloudflared in the image. |

**First start.** When `FERRULE_CONFIG` names a file that doesn't exist yet,
managed mode writes a starter config (a comment block, `[dashboard]` from the
env) and the gateway starts with **no channel and no model**: it serves the
dashboard and waits to be set up. Outside managed mode an empty gateway still
refuses to start, as today.

### 3.1 The policy file

Written by the panel, mounted read-only (`-v …/policy.toml:/etc/ferrule/policy.toml:ro`),
path from `[managed] policy` or `FERRULE_POLICY`. Every key is optional; a
missing key allows (the defaults match an unmanaged ferrule), so a policy
lists only what it locks.

```toml
reason = "Closed beta: set by the Ferrule panel."   # shown next to every lock

shell = true            # the shell tool (and hooks)
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
- **Enforced at the use site**: `shell` removes the tool; `browser` keeps the
  browser MCP server from being built; `extensions` refuses `mcp/add`,
  `plugins`, `skills install` and self-extension; `providers` makes a model
  on another provider unusable (as a missing key does today, with the policy's
  reason) and refuses to save one; caps are the minimum of the two;
  `channels` refuses to save or start another channel.
- **The dashboard shows locks as locked**: `GET /api/managed` returns the
  policy and its reason; the page shows a "Managed" card and greys out each
  locked control with the reason. The API refuses the locked operations with
  403 and the same words, so a hand-made request gets the same answer. Chat
  commands that would change a locked setting (`/model` to a forbidden
  provider, the settings door) answer with the reason.

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
- **A non-loopback bind needs `public_url`**, or ferrule refuses to bind it:
  without one, only loopback Hosts pass, which a reverse proxy would fail
  confusingly.
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
  work as before. Commands never see it (its name looks secret, so the
  sandbox scrubs it).
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
  rules). The session records the panel's user id for the audit log.

## 5. What a new user does, on the dashboard

What exists (M22–M39) and what this milestone adds:

| Need | Before | M44 |
|---|---|---|
| Add a model API key, pick the default | Models card: `models/provider` saves the key, `models/default` | the list honours `providers`; the forbidden ones say why |
| ChatGPT plan by device code | `plans/chatgpt/start|poll|cancel` | kept |
| Claude plan | `plans/claude` | 403 in managed mode, the button hidden with the reason |
| Telegram bot token, tested, start chatting | **missing**: "Telegram keeps `ferrule setup`'s flow" | a Telegram form: token (tested with `getMe`, webhook cleared), then "wait for a message" to allow a chat, then restart |
| HTTP API on and a key | the `http` card and `channels/keys/add` | kept |

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

1. Stop taking messages: the channels' loops and the scheduler stop.
2. Wait for running turns, up to `[gateway] stop_grace_secs` (default 20).
3. Stop the turns still running (`Router::stop`, as the dashboard's Stop
   does, "stopped: the bot is restarting"), and wait up to 5 s more for them
   to end and their replies to go out.
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
| `bind = 0.0.0.0` without `public_url` | gateway starts with the dashboard off, and says why |
| A turn outlives the grace on SIGTERM | stopped; its reply says the bot restarted |
| Re-exec fails (binary gone) | exits 1; the container runtime's restart policy decides |

## 8.1 Out of scope

The panel itself; invites and accounts; billing; per-bot resource limits
(`--memory`, `--cpus` are the panel's `docker run` flags, documented);
subdomain routing; image signing (cosign) — a follow-up.

## 9. What was verified, and how

Filled in as each part lands.
