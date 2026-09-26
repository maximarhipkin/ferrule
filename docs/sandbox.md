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
   `<data>/learn`, and `<data>/sessions` (the transcripts, which hold
   whatever a tool ever printed).
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

## Network

`network = true` is the default. `network = false` blocks every socket
except Unix ones on Linux (seccomp) and every network operation on macOS.
On Windows it isn't enforced, and doctor says so.

Separately from the sandbox, commands get `HTTPS_PROXY`, pointing at
ferrule's credential proxy, so `[secrets]` placeholders are swapped for
real values on the hosts they are bound to.

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

## Not covered

- The system-service path (`ferrule service install` as root with
  systemd) hasn't been run end to end.
- Other processes of your user that ferrule didn't start aren't
  protected by any of this. On Linux, Landlock's ptrace scoping keeps a
  sandboxed command out of them. On Windows, only ferrule's own process
  is hardened.
