# M34 — SSH workspaces and local-model first run (design)

Status: design, 2026-09-26, branch `m34-ssh-local`. Written before the
code; where the build departs from it, see **As built** at the end.
User guides: [ssh.md](ssh.md), [local-models.md](local-models.md).

Two items from the strategy doc (§4):

- **Item 21, SSH execution backend.** The workspace can live on another
  machine. The agent's shell and file tools act there; everything else
  (the model, the ledger, approvals, memory, the gateway) stays here.
- **Item 15, local-model first run.** Pointing ferrule at Ollama or
  llama.cpp should work the first time. The #1 failure is a silently
  shrunk context window: the model is trained for 128K, the server runs it
  at 4K, and the front of every prompt is dropped without an error.

The two parts share nothing but the milestone. §1–§9 are SSH and §10–§15
are local models.

---

## 1. Transport: the system `ssh`, measured

There were three candidates. OpenSSH is the `ssh` binary already on the
machine (Linux, macOS, and Windows 10+ ship it). `russh` 0.63.3 is pure
Rust. `ssh2` 0.9.6 binds libssh2.

Each library was added to a small host binary: tokio plus reqwest with
rustls, the TLS stack ferrule already links. It was built with ferrule's
release profile (`opt-level=3`, fat LTO, one codegen unit, strip,
`panic=abort`) for `x86_64-unknown-linux-musl`, with `musl-gcc` as the C
compiler. Latency was measured against OpenSSH 9.2p1's `sshd` on
127.0.0.1, with an ed25519 key, running `echo hi`. It was one machine and
three runs each, so read the numbers for ranking only.

| | musl release binary | growth | connect + auth | per command, connection reused | per command, no reuse |
|---|---|---|---|---|---|
| base (tokio + reqwest/rustls) | 2 786 120 B | — | — | — | — |
| **OpenSSH (system `ssh`)** | 2 786 120 B | **0** | (in the next column) | **14 ms** (ControlMaster) | 335 ms |
| russh 0.63 (`ring`, `rsa`, `flate2`, no defaults) | 4 974 568 B | +2.19 MB | 66–72 ms | 45 ms | — |
| ssh2 0.9 (`vendored-openssl`) | 7 492 904 B | +4.71 MB | 67–76 ms | 45 ms | — |

**On the five release targets:**

- **OpenSSH** adds no code, so it builds wherever ferrule builds.
- **russh** with `ring` builds on all five: ring is already in the tree for
  rustls. Its default `aws-lc-rs` backend needs cmake and a C toolchain,
  and on Windows NASM too.
- **ssh2** vendors OpenSSL, so every target needs a C compiler and Perl.
  On `x86_64-pc-windows-msvc` it needs Strawberry Perl as well. That is a
  new native dependency on each release runner.

**How each handles the user's own SSH setup:**

| | ssh-agent | `~/.ssh/config` (Host aliases, ProxyJump, Include, Match) | known_hosts (hashed, `@cert-authority`, `@revoked`) | FIDO / hardware keys |
|---|---|---|---|---|
| OpenSSH | native, incl. Windows' named-pipe agent | native, the reference implementation | native | native |
| russh | Unix socket; Pageant/pipe only with extra crates | none; `russh-config` parses a subset (no Match, partial ProxyJump) | plain and hashed entries via `check_known_hosts_path`; no `@cert-authority`/`@revoked` | no |
| ssh2 | yes (libssh2 agent, incl. Pageant) | none | libssh2 `knownhost` API; no `@cert-authority` | no |

**Decision: the system `ssh`.** Zero growth, no build risk, and it honours
every setting the user already has: agent, config, jump hosts, certificates
and hardware keys. With multiplexing it is also the fastest, at 14 ms
against 45 ms. The libraries' ~40 ms floor on a reused connection looks
like Nagle/delayed-ACK on the channel. It is fixable, but it doesn't matter
for the ranking.

What ferrule gives up:

- **One process per command.** This is fine: a tool call is already a
  model round trip.
- **Error reporting through stderr text.** §3 classifies it; the strings
  are stable across OpenSSH 7–9 and covered by tests.
- **Windows' OpenSSH has no ControlMaster.** Every command there pays the
  full handshake (~0.3–0.5 s). [ssh.md](ssh.md) says so. A persistent
  russh session is the follow-up if Windows users need it.

No feature gate: nothing is linked.

`ssh` is found on `PATH`. `[ssh.<name>] ssh = "…"` or `FERRULE_SSH`
overrides it. Tests use the override, and so does a Windows user with a
non-standard install.

## 2. Configuration and targets

The workspace is a local directory as before, or one of two remote forms:

```toml
# ferrule.toml
workspace = "ssh:app"                 # used when --workspace isn't given

[ssh.app]
host = "app.example.com"              # or a Host alias from ~/.ssh/config
user = "ferrule"                      # optional: else ssh's own default
port = 22                             # optional
path = "/srv/app"                     # the remote workspace, absolute
identity_file = "~/.ssh/ferrule_app"  # optional: else ssh-agent / ssh config
# ssh_config = "~/.ssh/config.work"   # optional: an extra -F file
# ssh = "C:\\Tools\\ssh.exe"          # optional: the ssh binary
```

- **Targets.** `--workspace ssh:app` names a block. `--workspace
  ssh://user@host:2222/srv/app` is the one-off form: no block, no identity
  file, agent or ssh config only. The top-level `workspace` key takes the
  same values, plus a local path. `run`, `chat` and `gateway` use it when
  `--workspace` is omitted.
- **Per task and per sub-agent.** Scheduled tasks and sub-agents have no
  workspace setting today: a task runs in the gateway's workspace, and a
  child in a worktree of its parent's. With a remote workspace, both
  **follow the parent's remote**:
  - Tasks run there.
  - A sub-agent shares the remote directory. There are no worktrees on the
    remote; see the follow-ups.

  A per-task `workspace` is a scheduler change. It is listed as a
  follow-up, not built.
- **The local anchor.** ferrule still needs a local directory for the
  things that only exist locally: checkpoints (undo), `.ferrule/` todos and
  diary, and hook trust. For a remote workspace that directory is
  `<data>/ssh/<name>/local`. Workspace skills and `hooks.toml` are **not**
  read from the remote. Global skills and hooks apply as always.

## 3. The link: one per target, reconnecting

`ferrule-ssh::Link` owns everything about one target.

**ssh options**, the same on every call:

```
-T -o BatchMode=yes -o StrictHostKeyChecking=yes -o UpdateHostKeys=no
-o UserKnownHostsFile="<~/.ssh/known_hosts> <~/.ssh/known_hosts2> <data>/ssh/known_hosts"
-o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=3
-o LogLevel=ERROR -o ForwardAgent=no -o ForwardX11=no -o ClearAllForwardings=no
[-p port] [-l user] [-i identity -o IdentitiesOnly=yes] [-F extra config]
```

- `BatchMode` means ssh never prompts: no passphrase, no "are you sure",
  no password. Any prompt would hang a headless gateway.
- `StrictHostKeyChecking=yes` means an unknown host is refused, never
  added (§4).
- The user's known_hosts files are read, and ferrule's own file is added
  after them. ferrule writes only to its own file.
- The agent is never forwarded. A shell on the remote must not be able to
  sign with the owner's keys.

**Multiplexing on Unix.** The link starts a master itself:
`ssh -M -N -o ControlPersist=no -o ControlPath=<dir>/<hash>`. It is a
child that ferrule owns, killed on drop, in a 0700 directory under the
temp dir, and the path is kept short for macOS' 104-byte socket limit.
Commands then run with `-o ControlMaster=no -o ControlPath=…`. If the
master is gone, ssh falls back to a direct connection by itself, and the
link restarts the master with backoff.

**Windows** has no multiplexing. Each command is a full connection.

**Failures** are classified from ssh's exit code 255 and its stderr:

| class | stderr contains | what happens |
|---|---|---|
| `UnknownHost` | `No … host key is known`, `Host key verification failed` (without the next row) | stop; the error says to run `ferrule ssh trust <name>` (§4); not retried |
| `HostKeyChanged` | `REMOTE HOST IDENTIFICATION HAS CHANGED` | **hard stop**; the link is poisoned for the process's life; the message quotes the new fingerprint and says it may be an attack |
| `Auth` | `Permission denied (` | stop; the message lists what was tried (agent, the named key) and hints `ssh-add` for a passphrase-protected key |
| `Unreachable` | `Connection refused`, `timed out`, `Could not resolve`, `No route`, `Network is unreachable`, `Connection closed by`, `Broken pipe` | retried with backoff (0.5, 1, 2, 4 s) **only if the command hadn't started**, see §5 |
| `Forward` | `remote port forwarding failed` | another port, up to 3 tries; then run without the proxy (§7) |

Poisoned or stopped links fail fast with the same message on every later
call. The model can't retry its way into a MITM.

## 4. Host keys: never trusted silently

- **First contact** goes through `ferrule setup` (the SSH step) or `ferrule
  ssh trust <name|url>`. Both run `ssh-keyscan -p <port> <host>`, where the
  host is the resolved one from `ssh -G` so aliases and `HostName` work.
  Both show every key's SHA256 fingerprint with `ssh-keygen -lf -`, then
  ask the owner to compare it with the server's own:
  `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`. Only a yes appends
  the keys to `<data>/ssh/known_hosts`, as `[host]:port` when the port
  isn't 22.
- **Headless** (a gateway with a new target) it never asks. The first tool
  call fails with `UnknownHost` and the trust command. A command that can't
  confirm is refused. `ferrule ssh trust --fingerprint SHA256:… <name>`
  accepts the key only if the scanned fingerprint matches the one given,
  for scripted installs.
- **A key the user already trusts** (their own known_hosts) is used as is.
  Setup reports "already known (your ~/.ssh/known_hosts)".
- **A changed key** is `HostKeyChanged`: a hard stop, doctor fails, and the
  message says to verify out of band and then delete the old line. ferrule
  never deletes it.

## 5. Running a command remotely

**The remote side is POSIX `sh`.** The login shell may be bash, zsh or
fish. ssh passes one command string to it, so that string is fixed and
single-quoted:

```
exec sh -c 'IFS= read -r n; s=$(dd bs=1 count="$n" 2>/dev/null); eval "$s"'
```

On stdin ferrule then sends:

1. the length of the real script, then the script (fixed per operation,
   part of ferrule's source);
2. its parameters, one per line, `\ooo`-escaped so any byte survives, and
   decoded remotely with `printf '%b'`;
3. for writes, the file's bytes until EOF.

Nothing the model chose is ever spliced into a command line, and nothing
secret is in `argv`. The proxy URL with its token and the placeholders
travel on stdin, so they don't show up in the remote `ps`.

**The shell tool's wrapper:**

- `cd` to the workspace. A missing workspace gets its own exit code and a
  clear message.
- Export the env, then run the model's command under `sh -c`, with stdin
  from `/dev/null`, in the background.
- A **watchdog** `(read -r _; kill -KILL 0)` blocks on the session's stdin.
  sshd starts every session in a new process group, so when ferrule closes
  the pipe (`/stop`, timeout, the process ending) the watchdog kills the
  whole group. That includes grandchildren, as `kill_process_group` does
  locally. On a normal exit the wrapper kills the watchdog first.
- Before the command, the wrapper prints `<nonce> start` on stderr. After
  it, `<nonce> exit <code>`. The nonce is 128 random bits, so the command's
  own output can't forge a marker. ferrule strips both from the output.

**What the markers decide:**

| seen | meaning | result |
|---|---|---|
| neither, ssh 255 + `Unreachable` | never started | safe to retry with backoff |
| `start`, no `exit` | started, connection lost | **interrupted**: an error saying the command may have partly run; never a success, never retried |
| both | finished | `[exit code: N]` exactly as the local shell |

**The same limits as the local shell:**

- The timeout is the local one (120 s default). It fires the same drop.
- Output is capped at the same `max_output_chars`, formatted as stdout,
  `\n[stderr]\n` + stderr, `\n[exit code: N]`.
- The same deny patterns (`sudo `, `rm -rf /`, …) are checked before
  anything is sent.
- The local shell does not stream (it waits for the output), so the remote
  one doesn't either.

**`/stop`, the kill switch and timeouts** all end the same way. The future
is dropped. The local `ssh` is killed with `kill_on_drop`. The channel
closes. The remote watchdog kills the group. The test checks that the
remote process is gone.

## 6. File tools and paths remotely

`read_file`, `write_file`, `edit_file` and `list_dir` keep their names,
schemas and output. The model sees the same tools; only the description
says where they act.

- **Paths.** The model's path is joined and normalized **as a POSIX
  string**, not with `std::path`, because the local machine may be
  Windows. It is then resolved on the remote by a small `sh` function:
  climb to the deepest existing part, follow symlinks with `readlink`
  (bounded at 40 hops, a dangling one fails), then `cd -P … && pwd -P`.
  This is the same rule as the local `resolve()`. A symlink out of the
  workspace is an escape.
- **The remote deny list.** The M26 read denies (`~/.ssh`, `~/.aws`,
  `~/.gnupg`, `~/.config/gh`, …, both the Linux and the macOS browser
  profile dirs) are computed for a Unix home. The remote `$HOME` is read
  once at connect. `deny_read`/`allow_read` entries apply too: `~/…`
  against the remote home, POSIX-absolute ones as they are, relative ones
  against the remote workspace. The check runs **inside the same remote
  script**, on the resolved path, so there is one round trip per tool
  call. On a macOS remote (`uname` = Darwin) the comparison folds case.
- **Writes are atomic.** A temp file in the same directory, the original's
  mode kept, `mv -f`.
- **`edit_file` runs in two steps.** First read the bytes and their
  `cksum`. Then apply the hunks locally, with the same code as the local
  tool, factored out as a pure bytes-in/bytes-out function. Then write,
  passing the expected `cksum`. If the file changed in between, the write
  refuses ("changed while editing, read it again").
- **`list_dir`** prints `dir\t` / `file\t` sorted, as locally. A symlink
  to a directory shows as `file`, as with `DirEntry::file_type` locally.

**What doesn't follow the remote, and why:**

| feature | remote workspace | why |
|---|---|---|
| `code_search`, repo map (M29) | **off**, with a note in the prompt | both walk and parse the whole tree with tree-sitter in-process; remotely that is a full download per session. Follow-up: run a `ferrule` binary on the remote |
| per-edit lint (`[agent] lint = "auto"`) | **off**, with a note | the linters are found and run locally against local files |
| auto-commit (`[agent] auto_commit`) | **off**, with a note | it runs `git` in-process on the local tree |
| `verify_command` | **runs remotely** | through the remote shell, same timeout |
| checkpoints / undo | **off** for remote paths | they snapshot local files |
| AGENTS.md baseline | **read remotely** | one `read_file`, same size cap |
| MCP servers | **local** | they are local processes; the model is told |
| sub-agent worktrees | **none**: children share the remote dir | no remote git worktree yet |

The notes reach the owner at startup (stderr and `doctor`) and the model
in the system prompt ("Workspace: ssh:app, /srv/app on app.example.com …
code_search isn't available").

## 7. Credentials: the proxy through `ssh -R`

The broker (M20's credential proxy) listens on local loopback. Remote
commands reach it through a reverse forward,
`-R 127.0.0.1:<rport>:127.0.0.1:<broker port>`, on each command with
`ExitOnForwardFailure=yes`:

- **`rport`** is random per link. When it's taken ("remote port forwarding
  failed"), ferrule picks another, up to 3 times. Through a mux master a
  repeated `-R` is accepted; this was measured.
- **The CA** is uploaded once per link to a 0700 directory in the remote
  `${TMPDIR:-/tmp}`.
- **The env.** `broker.child_env()` is rewritten: the proxy URLs' port
  becomes `rport`, and the CA-bundle variables point at the uploaded file.
  The result is sent on stdin (§5). The placeholders are unchanged.
- **The token.** The forward is loopback-only on the remote, but other
  users there can reach it. That is fine: the proxy requires the per-run
  token in `Proxy-Authorization`, exactly as it does locally.
- **Refused forwarding.** If the server refuses forwarding
  (`AllowTcpForwarding no`), commands run without the proxy. The model's
  note says "remote commands get no bound secrets", and so does doctor.

This uses the proxy's public API only (`addr`, `child_env`,
`ca_cert_path`, `model_note`). It changes none of its internals, which are
M33's.

## 8. Security posture, stated plainly

- **The remote account is the boundary.** ferrule's sandbox (Landlock,
  Seatbelt, the Windows token) runs on *this* machine. A remote command
  runs as the SSH user with everything that user can do. Setup, doctor
  and [ssh.md](ssh.md) all say so, and recommend a dedicated low-privilege
  account that owns only the workspace, with no sudo and no keys to other
  hosts.
- **The local sandbox policy** does not become a remote one.
  - `read-only` mode removes the remote shell (it can't be held to it),
    and removes the write tools as it does locally.
  - Plan mode removes the remote shell, as for an unsandboxed local one.
  - A read-only sub-agent gets no remote shell.
  - The egress allow-list doesn't apply to the remote host's network.
- **Doctor warns** when the target is this machine (`localhost`,
  `127.0.0.1`, `::1`, or the local hostname). That is a sandbox bypass,
  not a remote.
- **Keys never enter ferrule.** ssh reads the key file or talks to the
  agent itself. ferrule stores only the *path* the owner named, never the
  key or a passphrase. There is nothing to seal in `private/`, because
  nothing secret is stored. `BatchMode` means ssh can't ask for a
  passphrase, so a protected key must be in the agent.
- **ssh's stderr goes through the log redactor**, and ferrule never passes
  `-v`. The test greps the transcript, the ledger and the logs for the
  test key's private bytes and its comment.
- **Remote sandbox:** not built. The cheap version would wrap each command
  in `ferrule sandbox -- …` when a ferrule binary is on the remote PATH.
  It needs a remote ferrule of a compatible version and its own doctor
  check, so it is a follow-up. The owner can already put the SSH user in
  a container or a restricted account.
- **M19** approvals, caps, kill switch and plan mode, **M18** hooks, and
  **M26** read policy apply unchanged. The tools keep their names, so
  every gate, hook matcher and approval rule sees them as before.

## 9. Surfaces

- **`ferrule setup` → "Remote workspace (SSH)".**
  1. Ask for the host, user, port, path, and the key (agent or a key path).
  2. `ssh -G` to resolve.
  3. Keyscan and confirm the fingerprint (§4).
  4. A test command (`uname -sr; echo $HOME; test -d <path>`).
  5. Write `[ssh.<name>]`, and optionally `workspace = "ssh:<name>"`.
- **`ferrule ssh`**: `list`, `trust <name|url>`, `test <name|url>`.
- **`ferrule doctor`** checks each `[ssh.*]` block and the configured
  workspace:
  1. reachability;
  2. host key (known / unknown / **changed** = fail);
  3. auth;
  4. the remote shell (`sh` works and prints `uname`);
  5. the workspace path (exists, is a directory, is writable);
  6. forwarding for the proxy.

  It also shows the remote-account boundary note, and a warning when the
  target is local.
- **`/status` and the dashboard health:**
  `workspace: ssh:app (ferrule@app.example.com:/srv/app) · link up, 14 ms`,
  or `· down: <class>`. It is a section added to the health report, read
  from the link's last state; there is no extra round trip.

---

## 10. Local models: what the servers really say

Checked against each project's source or docs on 2026-09-26:

| server | version checked | listing | trained window | effective window | tools |
|---|---|---|---|---|---|
| **Ollama** | v0.34.4 (2026-09-23), `api/types.go`, `openai/openai.go`, `envconfig/config.go` | `GET /api/tags` → `models[].name`, `capabilities` | `POST /api/show {model}` → `model_info["<arch>.context_length"]` | `GET /api/ps` → `models[].context_length` (loaded only); `/api/show` `parameters` may hold `num_ctx N` (a Modelfile value) | `capabilities` contains `"tools"`; else chat with tools is 400 `… does not support tools` |
| **llama.cpp** `llama-server` | v0.5.0 (2026-09-23), `tools/server/server-context.cpp`, `common/common.h` | `GET /v1/models` → `data[0].id` | `data[0].meta.n_ctx_train` | `GET /props` → `default_generation_settings.n_ctx` (per slot), `total_slots`; also `data[0].meta.n_ctx` | `--jinja` is on by default now (`use_jinja = true`); `chat_template_caps` in `/props` |
| **LM Studio** | 0.4.x REST v1 (docs repo, `1_developer/2_rest/list.md`) | `GET /api/v1/models` → `models[].key`, `type` | `max_context_length` | `loaded_instances[].config.context_length` | `capabilities.trained_for_tool_use` |
| LM Studio 0.3.x | `/api/v0/models` | `data[].id` | `max_context_length` | `loaded_context_length` (when loaded) | `capabilities` list (`tool_use`) |
| **vLLM** | v0.30.0 (2026-09-22), `entrypoints/serve/engine/protocol.py` `ModelCard` | `GET /v1/models` → `data[].id`, `owned_by: "vllm"` | — | `data[].max_model_len` | needs `--enable-auto-tool-choice --tool-call-parser …`; else 400 naming those flags |

**Ollama's context.** Unless a Modelfile or a request sets `num_ctx`, the
server uses `OLLAMA_CONTEXT_LENGTH`, whose default is "4k/32k/256k based on
VRAM" (`envconfig`). Most laptops get 4096.

**The OpenAI-compatible endpoint can't raise it per request.** ferrule
talks to `/v1/chat/completions`, and `openai/openai.go` v0.34.4 builds its
`options` only from `stop`, `max_tokens`, `temperature`, `seed`, the
penalties and `top_p`. There is no `num_ctx`. The brief's "per-request
`num_ctx`" works only on the native `/api/chat`, which ferrule doesn't
speak.

So the fixes that work are:

1. **A derived model.** `POST /api/create {"model":"<m>-32k", "from":"<m>",
   "parameters":{"num_ctx":32768}}` adds a new model name that shares the
   weights. It leaves the user's model and the server's config alone.
   Setup offers it and does it only on a yes. The config then names
   `<m>-32k`.
2. **The server env.** `OLLAMA_CONTEXT_LENGTH=32768` with `systemctl edit
   ollama` / `launchctl setenv` / the Windows env. This is printed, never
   applied, because it's the user's server config.
3. **A Modelfile** by hand: the same as 1, printed for people who prefer
   it.

**llama.cpp** defaults `n_ctx` to the trained size (`0` = trained), but the
per-slot size is `n_ctx / --parallel` unless the KV cache is unified. The
fix is `-c N` (and `-np 1`), printed. **LM Studio**: `lms load <key>
--context-length N`, printed. **vLLM**: `--max-model-len N` and the
tool-parser flags, printed.

## 11. Detection

`ferrule setup`'s provider step and `ferrule doctor` probe, in parallel,
with a 700 ms budget each:

| server | where | how it's recognized |
|---|---|---|
| Ollama | `OLLAMA_HOST` or `127.0.0.1:11434` | `GET /api/version` → `{"version"}` |
| llama.cpp | `127.0.0.1:8080` | `GET /props` with `default_generation_settings` |
| LM Studio | `127.0.0.1:1234` | `GET /api/v1/models` (then `/api/v0/models`) |
| vLLM | `127.0.0.1:8000` | `GET /v1/models` with `owned_by == "vllm"` |

Every configured provider whose `base_url` is loopback or a private
address is identified the same way, so a server on another port or a LAN
box is covered.

Setup lists what it found, e.g. "Ollama 0.34.4 at 127.0.0.1:11434 · 3
models", and offers each as a provider preset with the models listed and
the window per model.

## 12. Windows, profile and compaction

For the chosen model ferrule reads **trained** (what the model can do) and
**effective** (what the server will give a request).

- **For Ollama the effective window is known only once the model is
  loaded.** The tool probe (§13) loads it, then `/api/ps` is read.
- **The effective window is what ferrule plans for.** It is written as
  `[providers.X.models."m"] context_window = N`, the key that already
  overrides the profile's window.
- **`HarnessProfile::fitted(window)`**, used whenever a window is set, not
  only for local models:
  - `output_reserve = min(profile's reserve, max(window / 4, 1024))`;
  - the threshold stays the profile's, except below 16K, where it rises to
    0.80. On a small window the fixed part (system prompt plus tool
    schemas) is most of the budget, and compacting at 70% would compact
    every turn.
  - Today a set `context_window` keeps the profile's reserve, so 8192 with
    `generic`'s 16 000 reserve underflows the trigger. That is a bug this
    fixes.
  - For windows at least 4× the reserve (every hosted model configured in
    the repo) nothing changes. The eval doesn't set a window, so its
    numbers can't move.
- **Profile:** `generic` for local servers, as today. The
  thinking-retention profiles are for hosted APIs that echo reasoning
  back.
- **Floors:**

  | effective window | outcome |
  |---|---|
  | < 8192 | fail |
  | < 16 384 | "too small for agent work" |
  | < 32 768 | a warning that it works but compacts often |

  The fix is offered for the first two.

## 13. The tool-calling probe

One chat call with one tool (`lookup_order(order_id)`), the user message
"Look up order A-1729 with the lookup_order tool", `temperature` 0, and
`max_tokens` 256.

| what comes back | verdict | tells the user |
|---|---|---|
| a structured `tool_calls` entry naming `lookup_order` | **works** | — |
| HTTP 400/500 with `does not support tools` (Ollama), `enable-auto-tool-choice` / `tool-call-parser` (vLLM), `tools` + `jinja` (llama.cpp) | **can't call tools** (server/model) | pick a tools-capable model or start the server with the flags |
| 200, text content containing the tool name plus a call shape (`{"name"`, `<tool_call>`, `[TOOL_CALLS]`, `<|python_tag|>`, `"arguments"`) | **template broken** | the model *tried*; the server's chat template didn't turn it into a tool call. Update the server, use the model's official template (Ollama: re-pull; llama.cpp: `--jinja`, `--chat-template-file`) |
| 200, plain text, no attempt | **can't call tools** (model) | the model ignored the tool; use another one |

Ollama's `capabilities` lacking `"tools"` short-circuits to "can't call
tools" with the reason "the model's template has no tool support".

## 14. Doctor and `/status`

- **Doctor, per local provider:**
  - the server kind and version;
  - the effective vs trained vs planned window. A window **smaller than
    the one ferrule plans for** is a failure: it is the silent-truncation
    case, and the message carries the fix;
  - with `--ping-models`, the tool probe, where "template broken" and
    "can't call tools" are warnings.

  `--offline` skips all of it.
- **`/status`, in the gateway,** checks the default model's server at
  start and every 10 minutes (window only), and runs the tool probe once
  at start. Lines appear only when something is wrong:
  - `local: qwen3-coder window 4096 < 32768 planned (Ollama drops the
    front of long prompts)`
  - `local: qwen3-coder can't call tools (template broken)`

## 15. Known-good local models (2026-09-26)

These come from the Ollama library pages on 2026-09-26: each is marked
`tools` and lists its trained window. They were **not** run through
ferrule's probe or eval here; there's no GPU on the build machine, so the
evidence is the library's capability flag plus the probe the user runs.
[local-models.md](local-models.md) keeps the table with the date:

| model | size (default tag) | trained window | note |
|---|---|---|---|
| `qwen3-coder:30b` | 19 GB | 256K | setup's current Ollama default; MoE, fast on 24 GB+ |
| `gpt-oss:20b` | 14 GB | 128K | reasoning; fits 16 GB |
| `devstral:24b` | 14 GB | 128K | agentic coding |
| `qwen3.6:27b` | 18 GB | 256K | general + tools |
| `granite4.1:8b` / `:3b` | 5.3 / 2.1 GB | 128K | small machines; expect weaker tool use |

## 16. Tests

- **SSH: a real `sshd` on 127.0.0.1**, started per test on a free port
  with a throwaway host key, user key and `authorized_keys`, as the
  current user.
  - **Why real sshd.** The thing being tested is the interaction with
    OpenSSH: its stderr classes, known_hosts handling, multiplexing,
    `-R`, and session process groups. An in-process russh server would
    test ferrule against a server ferrule doesn't use.
  - **When sshd is missing.** Found at `/usr/sbin/sshd` (Linux CI) or
    `FERRULE_TEST_SSHD`. The tests skip when it's missing, unless
    `FERRULE_REQUIRE_SSHD=1`, which the Linux CI job sets. The macOS CI
    job tries `/usr/sbin/sshd` as a non-root user and records whether it
    runs.
  - **What's covered:**
    - the shell and every file tool remotely;
    - an unknown key refused;
    - a changed key is a hard stop;
    - an auth failure is clear;
    - a dropped connection mid-command is interrupted;
    - `/stop` (a dropped future) leaves no remote process;
    - timeout and output caps;
    - no key bytes in the transcript, ledger or logs;
    - the proxy path, by a remote `curl` through the forward to a real
      `Broker`.
- **Windows CI** compiles everything and runs the parsing, escaping,
  classification, path and config tests. They need no sshd.
- **Local models:** mock Ollama, llama.cpp, LM Studio (v1 and v0) and vLLM
  servers on 127.0.0.1 with the JSON shapes from §10. They cover:
  - detection;
  - the small-window warning and each server's fix text;
  - the derived-model create call (Ollama);
  - `fitted()` for 4K/8K/32K/128K;
  - both probe failure modes plus the server-refuses mode.
- **The eval**, `ferrule eval run evals/starter --variant ab`, stays at
  20/20 engineered, 11/20 naive, $0.98.

## Follow-ups (not in M34)

- A remote sandbox: wrap commands in a remote `ferrule sandbox`.
- A remote `code_search` / repo map through a remote ferrule.
- A per-task and per-sub-agent `workspace`, and git worktrees on the
  remote.
- A persistent russh session for Windows, which has no ControlMaster.
- Streaming shell output, for local and remote alike.
- Native Ollama `/api/chat` with per-request `num_ctx`.

## As built

Built on branch `m34-ssh-local` in seven commits. Where the build departs
from the design above:

- **The master holds the forward.** On Unix the `-R` forward to the
  credential proxy is set up once, on the ControlMaster connection, not
  on each command. The master's remote session is `cat` reading
  ferrule's end of a pipe, not `-N`: however ferrule dies, the pipe
  closes and the master goes with it. On Windows each command is its own
  connection and carries its own `-R`.
- **`ssh_config`** is passed as `ssh -F`, which *replaces*
  `~/.ssh/config` rather than adding to it. The config comment and
  [ssh.md](ssh.md) say so.
- **The `/status` line** is the link's own state rather than a
  latency: `workspace: ferrule@app.example.com:/srv/app — connected
  (multiplexed) for 312s, 1 reconnect(s)`, `— not connected: <why>`, or
  `— STOPPED: the host key changed`. The dashboard shows the same line
  and lists a down link as a problem, with its fix.
- **The local-model `/status` lines compare against the planned window**
  (the configured `context_window`, else the profile's; 128 000 for
  `generic`), not a fixed 32 768: `local: qwen3-coder:30b window 4096 <
  128000 planned (…)`. Only the default model is watched. The dashboard
  lists the same lines as problems.
- **`fitted()`** is applied where the runtime builds a harness for a
  model (the catalog entry, `model_eval`, routing). The eval's own
  `windowed()` path is untouched, so its numbers can't move.
- **Setup's detection** runs in "Add a provider". "Change the model" on
  an existing local provider doesn't re-run the probe or the window
  check; doctor does.
- **Ollama after the probe.** Setup and doctor re-read `/api/ps` after
  the tool call, since the call is what loads the model.
- **CI.** The Linux job installs `openssh-server` if needed and sets
  `FERRULE_REQUIRE_SSHD=1`. macOS runs the sshd tests when its
  `/usr/sbin/sshd` starts unprivileged and skips them otherwise; Windows
  runs only the tests that need no sshd.
- **Known-good models** are the Ollama library's `tools`-tagged models
  (2026-09-26), not measured here: the build machine has no GPU.

Everything else (the transport, the failure classes, host-key trust, the
marker protocol, the watchdog, the remote deny list, the floors and the
probe's three outcomes) is as designed.
