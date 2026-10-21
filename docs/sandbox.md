# The sandbox

Every command the agent runs through the `shell` tool runs in an OS
sandbox, and so does every stdio MCP server. This guide covers what the
sandbox closes, how to change it, and what each OS can't do. The
Windows backend has its own page: [windows-sandbox.md](windows-sandbox.md).
The design and its as-built notes are in
[m26-isolation.md](m26-isolation.md).

| | Linux | macOS | Windows |
|---|---|---|---|
| Backend | Landlock (+ seccomp for `network = false`) | Seatbelt (`sandbox-exec`) | restricted token in a job object (PowerShell; Git Bash degrades) |
| Writes | workspace, temp, `writable_roots` | same | same |
| Denied reads | all of them | all of them | ferrule's secrets and `deny_read`; the default credential dirs only in the file tools |
| `network = false` | enforced | enforced | **not enforced** (needs admin) |
| Ferrule's own environment | closed (ptrace scoping) | closed (no `/proc`) | closed (process DACL) |
| Kill the whole tree | process group | process group | the job |

## Check it

```bash
ferrule sandbox                           # the policy, then a test of each promise
ferrule sandbox -- sh -c 'cat ~/.ssh/id_ed25519'   # run anything as the agent would
ferrule doctor                            # the same, plus what isn't enforced here
```

`ferrule sandbox` prints the backend, the writable roots, the scrubbed
variables, the **no reads** list, and then checks that a write outside
the workspace fails, a write inside works, the saved keys can't be read,
and, where it is enforced, that the network is off. On Windows it also
checks that ferrule's own process can't be opened from inside.

## Reads

Commands can read what your user can, except for one deny list. The
same list is enforced by the OS sandbox and by the in-process file tools
(`read_file`, `write_file`, `edit_file`, `list_dir`; see `docs/editing.md`),
so the model gets refused either way. The list has three parts:

1. **Ferrule's secrets.** These are always denied and can't be opened
   from the config: `<data>/private` (the saved keys, the connections
   store, the hooks trust list, dashboard sessions), `<data>/proxy/keys`,
   `<data>/claude-code` and the Matrix session file (every credential
   path the backup leaves out, except `ssh`: known_hosts and the SSH
   workspaces' local folders hold no key), `<data>/gateway` (channel
   state, which can hold credentials and messages waiting to be sent),
   `<data>/learn`, and `<data>/sessions` (the transcripts, which hold
   whatever a tool ever printed). In [owner mode](#owner-mode) the last
   two are readable to the owner's own chats.
2. **The usual credential dirs** (`deny_default_reads = true`, the
   default): `~/.ssh`, `~/.aws`, `~/.azure`, `~/.config/gcloud`,
   `~/.kube`, `~/.docker/config.json`, `~/.netrc`, `~/.git-credentials`,
   and the Chrome, Chromium, Edge, Brave and Firefox profiles in your
   OS's usual place (Safari and the cookie store on macOS as well).
3. **Your own additions**, in `deny_read`.

```toml
[sandbox]
deny_read = ["~/work/.env.production", "secrets/"]  # relative = inside the workspace
allow_read = ["~/.kube"]       # re-open a default entry
deny_default_reads = true      # false drops part 2 entirely
```

`allow_read` only removes entries of part 2 that are equal to or under
an allowed path. `allow_read = ["~/.ssh/known_hosts"]` leaves `~/.ssh`
closed, and `allow_read = ["~/.ssh"]` opens it. It never opens parts 1 or
3.

Paths that don't exist when a command starts are skipped, because
Landlock and Seatbelt need the real path. A `~/.aws` created during a
command is covered from the next command on.

**What still shows.** On Linux, the *names* of the files inside a denied
dir can still be listed (Landlock's directory-read right is inherited),
but their contents can't be read. The file tools refuse the listing too.

## Writes

- `mode = "workspace-write"` (the default): the workspace, temp dirs
  (`tmp = true`) and `writable_roots`. An MCP server also gets a state
  dir of its own.
- `mode = "read-only"`: nothing is writable, the workspace included.
- `mode = "off"`: no sandbox. Doctor warns.
- `mode = "owner"`: writes go everywhere your account can, except the
  keys and ferrule's own files. Linux only; see [Owner mode](#owner-mode).

## Network

`network = true` is the default. `network = false` blocks every socket
except Unix ones on Linux (seccomp) and every network operation on macOS.
On Windows it isn't enforced, and doctor says so.

Unix sockets are allowlisted either way (Linux and macOS), so a reachable
`docker.sock` or tmux server isn't a way out: `unix_sockets` and
`unix_sockets_default` in `[sandbox]`, described in
[egress.md](egress.md#unix-sockets).

Separately from the sandbox, commands get `HTTPS_PROXY`, pointing at
ferrule's credential proxy, so `[secrets]` placeholders are swapped for
real values on the hosts they are bound to. The same proxy enforces the
`[egress]` network policy: which hosts the model's tools and proxied
commands may reach, with private addresses and cloud metadata blocked by
default. See [egress.md](egress.md).

### Plain HTTP

Ferrule's own HTTP clients (`web_fetch`, MCP over HTTP) send `http://`
through the proxy too, not only `https://`. For plain HTTP:

- A host with no secret bound to it is forwarded unchanged.
- A secret is swapped in over plain HTTP only when the target is
  loopback (`localhost`, `127.0.0.0/8`, `::1`), which suits a local dev
  server.
- A bound host anywhere else gets **403**: "secrets only go over HTTPS".
  The request isn't sent out in cleartext with a placeholder in it.

Commands still get `HTTPS_PROXY` only. A `curl http://…` from the shell
goes direct, so it carries placeholders, never real values.

## MCP servers

A stdio MCP server runs in the same sandbox as commands, with network
always on and a state dir of its own. `sandbox = false` on a server
(`ferrule mcp add --no-sandbox`) means **hide-only**, not wide open:

- **Linux**: Landlock with all of `/` writable except the deny list, and
  no seccomp.
- **macOS**: Seatbelt allows everything except the deny list.
- **Windows**: the restricted token without write confinement, still in
  a job, so ferrule's secrets and process stay closed and the tree dies
  with ferrule.

The server can write anywhere your user can, and it keeps the network.
It still can't read ferrule's secrets or your denied paths (on Windows,
the default credential dirs stay readable to it), or ferrule's
environment. `ferrule doctor` lists each such server and what stays open
for it. Without any backend (sandbox off, or it failed to start), a
`sandbox = false` server is fully open, as it always was.

**One Linux limit:** under hide-only, a server can't create a *new*
entry directly inside a directory that contains a denied path. The
top of your home dir is the usual case, because `~/.ssh` is denied there.
Existing files there stay writable, and so does everything in the
subdirs. Landlock grants rights per existing path, and a carve can't
grant "new children of `~` except `.ssh`". If a server has to create
something at the top of `~`, create it once yourself; from then on it's an
existing entry and stays writable.

## Ferrule's own git

Some git calls are ferrule's, not the model's, and run as you outside the
sandbox: a sub-agent's worktree and the commit when it's closed, the
verifier's snapshot (`ferrule-agents/src/worktree.rs`), and the eval
fixture's first commit (`ferrule-eval/src/fixture.rs`). The worktree
calls run inside the sandbox instead, with the same pins, in the gateway
when it has a sandbox and in any process whose config asks for owner mode
(see Owner mode). They act on a repo
a sandboxed command can write to, and git runs commands its config names.
All of them go through one helper, `ferrule_sandbox::host_git`, which:

- pins `core.hooksPath=/dev/null` (no hook can live there; the old
  `$TMPDIR/ferrule-no-git-hooks` was writable by sandboxed commands),
  `core.fsmonitor=false` and `commit.gpgsign`/`tag.gpgsign=false` (so
  `gpg.program` never runs);
- lists the config with `git config --list --show-scope` and empties
  `clean`, `smudge` and `process` of every filter driver that `.git/config`,
  a worktree's `config.worktree`, or a file they include defines. Drivers
  in your global or system config still run, so **git-lfs keeps working**
  when it was installed the usual way (`git lfs install` writes
  `filter.lfs.*` to `~/.gitconfig`). A repo-level LFS install
  (`git lfs install --local`) is turned off for these calls: a worktree
  then holds pointer files;
- pins the work tree with `--work-tree` to the directory holding the
  `.git` git found, whenever the repo's config sets `core.worktree`. A
  worker may write its repo's git dir (so it can commit), so it could give
  its worktree a `core.worktree` (`extensions.worktreeConfig` plus `git
  config --worktree`) naming any directory, and closing it would have
  committed that directory's files, read as you, to a branch the parent
  can read. `-c core.worktree=…` doesn't override the repo's value;
  `--work-tree` does. A submodule's own `core.worktree` (`../../../sub`)
  names that same directory, so submodules keep working;
- kills a call that runs past 5 minutes (a FIFO planted as `.git/HEAD`
  blocks git's read forever).

The snapshot's diff passes `--no-ext-diff --no-textconv`, so diff drivers
don't run either. It needs git 2.26 or newer (`--show-scope`); with an
older git the calls fail and a child shares its parent's workspace.
Auto-commit (`[agent] auto_commit`) is different: it runs git *inside*
the sandbox, where the repo's own hooks and drivers have no more power
than the command that wrote them.

## Env

Variables whose name looks secret (`*KEY*`, `*SECRET*`, `*TOKEN*`,
`*PASSWORD*`, `*PASSWD*`, `*CREDENTIAL*`) and every provider's
`api_key_env` are stripped. `env_passthrough` keeps named ones, and
`scrub_secret_env = false` keeps them all. With `[secrets]`, the command
gets a same-shaped placeholder in their place.

## Limits (Windows)

```toml
[sandbox]
process_limit = 256   # most processes a command tree runs at once; 0 = none
memory_mb = 4096      # the whole tree's memory; unset = none
```

On Linux and macOS these keys are accepted and ignored.

## When the sandbox can't start

Ferrule proves the backend works by running the shell under it once at
startup. If that fails, commands run **unsandboxed** with a warning in
the log, and doctor names the reason. `require = true` makes ferrule
refuse to start instead.

On Windows this is the usual case with Git Bash, which can't run under the
restricted token. Set `FERRULE_SHELL=powershell` to run commands sandboxed
in PowerShell ([windows-sandbox.md](windows-sandbox.md), Shells).

## In a container (managed mode)

A bot in a [container](docker.md) never runs unsandboxed silently. When the
backend doesn't start there, commands run **only** if the panel's policy
says `sandbox = "container"` (the container is the boundary; the env
scrubbing and the deny list still apply). Without that line the `shell`
tool, hooks and the browser are off, and `/healthz` is `degraded` with the
reason. `[sandbox] mode = "off"` in the user's config counts as "doesn't
start", so it can't lift the policy. Which
container runtimes give Landlock, the seccomp filter, Unix sockets and
Chrome's own sandbox, and which policy each takes: the
[table in docker.md](docker.md#the-sandbox-inside-a-container).

## Remote workspaces

With a remote workspace ([ssh.md](ssh.md)) the shell and file tools run
on another machine over SSH, and none of this sandbox applies there: the
remote account is the boundary. Use a dedicated low-privilege user.
Plan mode and `read-only` remove the remote shell rather than trusting
it to only read.

## Owner mode

`[sandbox] mode = "owner"` is for a bot you run and talk to from a chat
and never log in to the machine for. It does everything you could do in a
terminal, with no approval prompts, and the keys stay out of its reach.
The design, with the rulings made while building it, is
[m54-autonomous.md](m54-autonomous.md). It was built without being
compiled or run; `make check` and a live run on a Linux server are still
to do.

It is opt-in. The default stays `workspace-write`.

### Turn it on and off

- **From the dashboard:** Settings, the **Autonomy** row.
  "Protected autonomous" is owner mode; "Ask me first" writes
  `workspace-write` and removes `[trust] gates = false`. Either choice
  restarts the gateway. A managed bot doesn't show the row.
- **In the config:** `mode = "owner"` under `[sandbox]`, then restart.
- **Only you can.** The whole `[sandbox]` table is guarded config that
  the bot can't change (`ferrule_admin config_set` refuses it), and the
  dashboard's switch has no chat route. `config_restore` from the chat refuses a restore that
  would bring back a guarded field.

### What commands can read and write

| | Read | Write |
|---|---|---|
| Hidden: the keys and the other credential files (see [Reads](#reads)), the default credential dirs, `<data>/gateway` | no | no |
| Protected: the set below | yes | no |
| Everything else your account can reach | yes | yes |

**The protected set** is whatever runs outside the sandbox or tells
something outside it what to run:

- the data dir (`~/.local/share/ferrule`), all of it: backups, the
  update state, plugins, the audit trail, the kill switch, and the
  binary. An MCP server's own state dir and `<data>/worktrees` (where
  sub-agents work) are granted writes inside it;
- the config file's folder (`~/.config/ferrule`, or where
  `$FERRULE_CONFIG` points);
- `~/.config/systemd` and `~/.local/share/systemd`, where a user unit or
  drop-in would run outside the sandbox at the next start;
- `~/.config/environment.d`, which sets the environment of every user
  service, `LD_PRELOAD` included.

Missing ones are created empty at startup so they are protected from the
first command. The file tools (`write_file`, `edit_file`) refuse the same
paths, which matters when the workspace contains the data dir.
`ferrule sandbox` lists them as `no writes`.

**The `mkdir` rule.** Landlock can only add rights, so a folder on the way
down to a hidden or protected path (`~`, `~/.config`, `~/.local`,
`~/.local/share`) can't get new entries. Owner mode lets commands create
a folder there. The new folder is fully writable from the next command on
(each command builds its rules fresh), so an installer that creates its
folder and fills it in one go fails once and works on a rerun. The
model's sandbox note says to `mkdir` first.

**Unchanged:** env scrubbing, credential placeholders, `no_new_privs`,
the network setting, and the Unix-socket allowlist. The allowlist stays
on for MCP servers and the browser too, so a command can't reach the user
bus (`systemd-run --user`) or a tmux server.

Owner mode has no exemptions for helpers. An MCP server with
`sandbox = false` runs under the owner sandbox, with its own state dir.

### What the bot can do from the chat

In your private chat or the dashboard chat, with no approval prompt,
`ferrule_admin` runs every change op at once and logs it
(`self_service.ran`, by `<channel> chat <id> (owner mode)`). Its answer
starts "Done:" or "Not done:". The trust gates are off for your chats
and for the sub-agents and tasks you start there. The new ops:

| Op | What it does |
|---|---|
| `package_install`, `package_remove` | `sudo -n apt-get`, see below |
| `mcp_add` | starts an MCP server, lists and scans its tools, then adds it; no restart. A header may name a key only as `${NAME}` of a key that already exists, an `env` name may not look like a key, there is no `no_sandbox`, and tools the scan blocks stop the add |
| `secret_request` | asks you for a new key by name and sites |
| `secrets` | the tool keys' names and sites, never a value |
| `dashboard_link` | sends you a one-time dashboard login |

The ops exist in every mode; outside owner mode the change ops ask first.

**Asking for a key.** `secret_request {name, hosts, why}` sends you one
message with the bot's reason and a one-time login link that opens the
Keys panel with the name and sites filled in. You can change the sites.
Paste the key there, never in a chat. The link is never in the bot's tool
result or transcript. When you save, the asking session is woken ("the
owner saved NAME; it is sent only to ...", never the value) and carries
on, and can call `mcp_add` now. The bot's reason is cut down to letters, digits and a
few marks, so it can't carry a link of its own. The wake is lost if the
gateway restarts first, the session's queue is full, or you save a
different name.

**Transcripts and notes.** `sessions/` and `learn/` are readable to your
chats' commands, read-only. They stay hidden from everyone else's.

**Hooks, gate scripts and helpers run in the sandbox.** Your `[hooks]`,
scheduler gate scripts, sub-agent worktree git, signal-cli, the
dashboard's tunnel, the transcription command, `agent-browser --version`
and claude's version check and updater go through the owner sandbox. A
hook or gate that needs the keys or a protected path stops working in
owner mode. What still runs outside it is root-owned programs found in
the system dirs (`git`, `ssh`, `systemctl`, `loginctl`, `ps`, never
looked up on `PATH`, where the bot may have put its own), `sudo` and
`apt-get` by absolute path, and ferrule's own binary. A workspace's hooks
run in the session's own sandbox, in every mode.

**Installing system packages.** `package_install` runs
`sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y
--no-install-recommends` as the gateway's user (with a 10-minute limit; if
apt doesn't know a name it runs `apt-get update` once and retries, and it
runs `apt-get clean` afterwards); `package_remove` runs `apt-get remove`. Names must be lower-case
letters, digits and `+ . -` (no paths, `.deb` files or options). A dry
run comes first: apt must read each name as that one package, and a
change that would remove `sudo`, `apt`, `openssh-server`, `systemd` or
`ca-certificates` is refused. It needs **passwordless sudo for the
gateway's user**. Without it the answer says so and that user-level
installs still work. Under a root-installed system unit
(`NoNewPrivileges=yes`) sudo can't work either, and the op fails
truthfully. Linux with apt only.

### Where the binary lives

The sandbox protects the binary, and `~/.local/bin` can't be protected
without breaking pipx and uv. So on its first owner-mode start the gateway
moves into `<data>/bin/ferrule`:

1. a hard link to the running file there (a copy if links aren't
   possible);
2. its service units rewritten to name it and reloaded;
3. the old path swapped for a symlink to it in one step;
4. a clean exit, and the service starts it from there.

The original is never renamed away, so every step is safe to stop at. The
`<data>/bin` dir is on the service's `PATH`. The old path (say
`~/.local/bin/ferrule`) is now a link to a file nothing in the sandbox can
write. **Update with `ferrule update` from then on.** `install.sh` sees a
unit that names another path, says so and restarts the service as it is;
it doesn't point it back.

Owner mode needs ferrule to run as a systemd **user** service that started
this process, and to be the only ferrule instance on the account (each
instance protects only its own data dir, so beside another one each could
write what the other runs). A bot started by hand, a system unit, another
instance, or a container in managed mode can't move, and falls back.

### When it can't hold

The gateway never runs commands unsandboxed and never exits for this. It
starts in `workspace-write` with `require` on, and tells you in your
private chat, a few seconds after start, why and what runs instead: that
commands run in the `workspace-write` sandbox, and whether it asks you
first (`[trust] gates`). `ferrule doctor` shows the same reason and
`ferrule sandbox` prints `owner     off here: <reason>`. The reasons are: not on
Linux; no systemd user service, or one it didn't start itself; another
ferrule instance on the account (or the instances couldn't be looked for);
the binary couldn't be moved; the workspace is inside a protected path;
an **SSH workspace** (its local folder is inside the data dir, so these
always fall back); or the sandbox can't be applied. If workspace-write
can't be applied either, commands are refused and chat still works.

After a fallback, hooks, gate scripts, signal-cli, the tunnel and
transcription keep running under a sandbox built from the owner policy
(keys hidden, ferrule's files read-only, writes elsewhere), so they keep
working and the keys stay hidden. Only the bot's own commands are
narrowed. Where that sandbox can't be built (macOS, Windows, no
Landlock) and the config still says `owner`, those spawns are refused.

Outside the gateway, a terminal `ferrule run` or `chat` gets owner mode
only once the service has moved its binary. Before that it falls back with
a one-line reason. After it, it runs in owner mode and without gates.

### Other people's chats

Only your private chat, the dashboard chat and what they start (a
sub-agent, a task, a `/plan`, `/goal` or `/graph` run) get owner mode.
Every other session (a group, another allowed person, the HTTP API, a
task made elsewhere) gets `workspace-write`, and can't read your
`sessions/` or `learn/`; the claude engine is narrowed per session too. A
run in the gateway that no chat traces to counts as someone else's.

**Known limit: guests share the helpers.** MCP servers, your `[hooks]`, the
browser, graph check commands and the host launcher are process-wide, and
the workspace is shared. A guest's chat that uses them reaches what the
owner sandbox reaches: never the keys or ferrule's protected files, but a
filesystem-type MCP server or the browser, steered by a guest, can read
your transcripts and learning notes. A guest can also leave a git filter
driver or a script in the shared workspace for your next session to run.
Owner-mode auto-commits turn off repo hooks, fsmonitor and signing for
that reason. The only chat today is usually yours; separate helpers per
session, or a workspace per guest, is the follow-up.

### What's left open

- **Granted folders inside the protected set stay writable.** MCP
  servers' state dirs and `<data>/worktrees` are meant to be. A folder you
  list in `writable_roots` is too, so listing a protected one reopens it;
  and a workspace inside a protected path makes owner mode fall back.
- **The apt op runs package scripts as root.** They come only from the
  configured apt sources, which the bot can't edit. A package that runs
  per-user files outside the sandbox (mail filters) could be combined
  with a writable file in `~`. The audit log lists every install.
- **Claude's login** (`~/.claude`) stays readable to claude and its Bash,
  as for any claude-engine setup.
- **Re-opening `~/.ssh`** with `allow_read` makes `authorized_keys`
  writable. A key planted there skips everything above. Don't.
- **Logging in as this account** runs whatever the bot left in
  `~/.bashrc`, what it sources, and `~/.local/bin` first on `PATH`, and a
  `ferrule` typed there is the replaceable link. The service never uses
  these. Treat a login after the bot has run as running its code, keys
  included.
- **Shared helpers for guests**, above.
- **Not on macOS or Windows.** Seatbelt matches paths: with `/` writable,
  `mv ~/.config ~/.c2` would undo a deny. Both refuse the mode with
  `owner mode needs Linux; this is macOS` (or Windows).
- **Existing limits:** file names inside a hidden directory stay
  listable, and a bind mount of a hidden path isn't covered.
- **An owner hook that needs the keys** stops working here.
- **The bot's `ferrule status` from its shell** may show less (the
  gateway's state dir is hidden); it uses `ferrule_admin status`.

## Not covered

- The system-service path (`ferrule service install` as root with
  systemd) hasn't been run end to end.
- Other processes of your user that ferrule didn't start aren't
  protected by any of this. On Linux, Landlock's ptrace scoping keeps a
  sandboxed command out of them. On Windows, only ferrule's own process
  is hardened.
