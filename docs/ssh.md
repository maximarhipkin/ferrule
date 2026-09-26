# Remote workspaces over SSH

The agent's workspace can be a directory on another machine. Its shell
and file tools (`shell`, `read_file`, `write_file`, `edit_file`,
`list_dir`) then run there, over your own `ssh`. Everything else stays on
this machine: the model calls, the ledger, approvals, memory, the gateway
and MCP servers. The design, with the measurements behind it, is in
[m34-ssh-local.md](m34-ssh-local.md).

## The boundary, first

**The remote account is the boundary.** ferrule's sandbox
([sandbox.md](sandbox.md)) runs on *this* machine, and it doesn't reach
the remote. A command there can do whatever the SSH user can do.

Use a **dedicated, low-privilege user** that owns only the workspace. It
should have no sudo and no keys to other hosts. If you want more, put
that user in a container.

Setup, `ferrule ssh test` and `ferrule doctor` all repeat this. Doctor
also warns when the target is this machine (`localhost`, `127.0.0.1`,
`::1` or its own hostname): that bypasses the local sandbox rather than
extending it.

## Set it up

The short way is **`ferrule setup` → Remote workspace (SSH)**. It:

1. asks for a name, the host (a name, an address or a `Host` alias from
   `~/.ssh/config`), the user, the port and the remote directory;
2. asks how ssh logs in: your ssh-agent or ssh config, or a key file;
3. fetches the host's keys and shows their `SHA256:` fingerprints. Check
   one against the server's own before you say yes:
   `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`, run on the server;
4. tests the connection (see [Check it](#check-it)) before it saves;
5. writes an `[ssh.<name>]` block, and on a yes makes it the default
   workspace.

By hand, the config looks like this:

```toml
# ferrule.toml
workspace = "ssh:app"                 # run/chat/gateway use it without --workspace

[ssh.app]
host = "app.example.com"              # or a Host alias from ~/.ssh/config
user = "ferrule"                      # optional: else ssh's own default
port = 22                             # optional
path = "/srv/app"                     # the remote workspace: absolute, or ~/…
identity_file = "~/.ssh/ferrule_app"  # optional: else ssh-agent / ssh config
# ssh_config = "~/.ssh/config.work"   # optional: used instead of ~/.ssh/config (ssh -F)
# ssh = "C:\\Tools\\ssh.exe"          # optional: the ssh binary
```

Then trust the host once: `ferrule ssh trust app`.

**One-off targets.** `--workspace ssh://user@host:2222/srv/app` works
without a block. It logs in with your agent or ssh config only, and the
host must already be trusted.

`--workspace ssh:app`, `--workspace ssh://…` and `--workspace .` work on
`run`, `chat` and `gateway`, and override the config's `workspace`.

## Host keys

ferrule never trusts a host key silently.

- A host already in your `~/.ssh/known_hosts` is used as it is.
- A new host is trusted only through setup or `ferrule ssh trust <name>`.
  Both show the fingerprints and wait for your yes. The key goes to
  ferrule's own file, `<data>/ssh/known_hosts`. ferrule never writes to
  yours.
- For a scripted install, `ferrule ssh trust --fingerprint SHA256:… app`
  accepts only a key with that fingerprint, without asking.
- A headless run (the gateway) never asks. The first tool call fails and
  names the trust command.
- **A changed key is a hard stop.** Every later call fails with the same
  message, which quotes the new fingerprint and says it may be an attack.
  Doctor fails too. Check the server out of band, then remove the old
  line yourself. ferrule never removes it.

## Keys and passphrases

ferrule never reads your private key. It passes the key's *path* to
`ssh`, which reads the file or talks to your agent itself. The config
holds only that path. Keys and passphrases never reach the model, the
transcript, the ledger or the logs. ssh's error text goes through the log
redactor, and ferrule never runs `ssh -v`.

ssh runs with `BatchMode=yes`, so it can't ask for a passphrase. A
passphrase-protected key must be loaded into the agent (`ssh-add`) first.
The agent is never forwarded to the remote.

## What runs where

| | with a remote workspace |
|---|---|
| `shell`, `read_file`, `write_file`, `edit_file`, `list_dir` | **remote**: same names, schemas and output |
| `verify_command` | remote, with the shell's timeout |
| AGENTS.md | read from the remote once, at connect |
| `code_search` and the repo map | **off**: they index local files |
| per-edit lint (`lint = "auto"`) | **off**: the linters run locally |
| auto-commit | **off**: it commits a local tree |
| undo checkpoints | don't cover remote files |
| MCP servers, web tools, memory | local |
| workspace skills and `hooks.toml` | not read from the remote; global ones apply |
| scheduled tasks and sub-agents | follow the parent's remote and share its directory (no remote worktrees) |

ferrule prints what's off at startup, and the model is told in its system
prompt.

Local state that has nowhere else to go (todos, the diary, hook trust)
lives in `<data>/ssh/<name>/local`.

## Limits and rules

A remote command gets the same treatment as a local one:

- the same timeout (120 s by default) and output cap;
- the same deny patterns (`sudo `, `rm -rf /`, …), checked before
  anything is sent;
- approvals, caps, the kill switch and plan mode (M19), hooks (M18) and
  the read deny list (M26). The tools keep their names, so every rule and
  hook matcher sees them as before. The deny list is applied to the
  remote home, so `~/.ssh`, `~/.aws` and the rest are refused there too.

Because the local sandbox doesn't reach the remote, anything that would
*hold* the shell to reading removes it instead: plan mode, `mode =
"read-only"` and read-only sub-agents get no remote shell.

**`/stop`, the kill switch and a timeout** end the command and every
process it started on the remote. The remote side runs a small watchdog
in the command's process group, and it kills the group when ferrule
closes the connection.

**A dropped connection.** Before a command has started, ferrule
reconnects with backoff (0.5, 1, 2, 4 s) and runs it. Once it has
started, the call fails as *interrupted*: the command may have partly
run. It is never retried and never reported as a success. The link
reconnects by itself on the next call.

File writes are atomic (a temp file, then a rename, keeping the mode).
`edit_file` refuses to write if the file changed after it was read.

## Credentials

The credential proxy ([m20-connections.md](m20-connections.md)) reaches
remote commands through a reverse forward (`ssh -R`) bound to the
remote's loopback. Remote commands see the same placeholders as local
ones, and the proxy swaps in the real key on the way out. The proxy URL,
its token and the placeholders go over ssh's stdin, never on a command
line, so they don't show in the remote `ps`. Other users on the remote
can reach the forwarded port, but not use it: the proxy wants the per-run
token.

If the server refuses forwarding (`AllowTcpForwarding no`), commands run
without the proxy, and both the model and doctor say that remote commands
get no bound secrets.

## Check it

- **`ferrule ssh list`** shows the blocks and whether each host key is
  known.
- **`ferrule ssh test <name>`** connects and checks the host key, the
  login, the remote shell, the workspace (it exists and is writable) and
  the proxy's forward.
- **`ferrule doctor`** runs the same checks for every `[ssh.*]` block and
  the configured workspace.
- **`/status` and the dashboard** show the link:
  `workspace: ferrule@app.example.com:/srv/app — connected (multiplexed) for 312s`,
  or `— not connected: …`, or `— STOPPED: the host key changed`. The
  dashboard lists a down link as a problem.

## Speed

On Linux and macOS, ferrule keeps one multiplexed ssh connection per
host, so a command costs about 14 ms plus its own time. Windows' OpenSSH
has no multiplexing, so each command there pays a full handshake
(about 0.3 to 0.5 s).

## Not built yet

- A sandbox on the remote side.
- `code_search` and the repo map on a remote workspace.
- A per-task or per-sub-agent workspace, and git worktrees on the remote.
- Streaming shell output (the local shell doesn't stream either).
