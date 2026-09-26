# The Windows sandbox

On Windows, ferrule runs commands and stdio MCP servers under a
**restricted token inside a job object**. It needs no admin rights, no
service and no driver. The general guide is [sandbox.md](sandbox.md), and
the design is [m26-isolation.md](m26-isolation.md) §2.

```powershell
ferrule sandbox      # the policy and a test of each promise, via cmd.exe
ferrule doctor       # what is enforced here and what isn't
```

## What it does

- **Writes** land only in the workspace, `%TEMP%`/`%TMP%` (with
  `tmp = true`) and `writable_roots`. Each root gets a random capability
  SID with one inheritable allow ACE for it, and the token is
  `WRITE_RESTRICTED` to those SIDs. A write needs both your access and a
  capability's, so it fails everywhere else.
  `mode = "read-only"` gives the token no capabilities at all.
- **Ferrule's secrets can't be read.** `<data>/private`,
  `<data>/proxy/keys`, `<data>/learn`, `<data>/sessions` and every
  `deny_read` path get a protected DACL. It lets your user in only while
  *Authenticated Users* is enabled in the token, and in the sandbox token
  that group is deny-only. Ferrule, Explorer, your editor and an admin
  shell keep full access.
- **Ferrule's own process** (its memory and environment) carries the
  same condition, so a sandboxed program can't open it.
- **The job** kills the whole tree when the command ends, times out or
  ferrule dies. It caps the number of processes (`process_limit`, 256 by
  default) and, optionally, the memory (`memory_mb`).

## How it runs

`std::process::Command` can't take a token, so ferrule starts a small
**launcher**, which builds the token and the job and then starts the
program inside them:

```
ferrule ──▶ ferrule.exe __sandbox-launch (normal token) ──▶ program (restricted token, in the job)
```

The launcher is ferrule itself. Other embedders and the tests use
`ferrule-sandbox-launch.exe`. `FERRULE_SANDBOX_LAUNCHER` points at a
different one. The launcher's instructions travel in
`FERRULE_SANDBOX_SPEC`, which the program never sees.

The capability SIDs are saved per root in
`<data>/sandbox/windows-caps.json` (`FERRULE_SANDBOX_STATE` overrides
the location). The ACE for a root is written once. If you later drop the
root from the policy, its ACE stays, but no token carries that SID any
more, so it grants nothing.

## Shells

Git Bash (`C:\Program Files\Git\bin\bash.exe`) and PowerShell are tested
under the token in CI. At startup ferrule runs `exit 0` in the shell the
`shell` tool will use. If that fails, commands run **unsandboxed** with a
warning, and `ferrule doctor` names the shell and points here. Set
`require = true` to refuse to start instead.

## What it doesn't do

- **`network = false` is not enforced.** Blocking sockets needs the
  Windows Filtering Platform, which needs admin. Commands still get
  `HTTPS_PROXY` and placeholders, never the real secrets, so a program
  that ignores the proxy goes out directly, but without your keys.
  Doctor warns when `network = false` is set.
- **The default credential dirs are protected by the file tools only.**
  `~/.ssh`, `~/.aws`, the cloud dirs and the browser profiles keep
  their ACLs. OpenSSH checks its key file ACLs strictly, and the browsers
  own their profiles, so ferrule doesn't rewrite them. A shell command can
  read them. To close one to commands too, add it to `deny_read`, and
  ferrule will then protect it like its own secrets.
- **Other processes of your user** that ferrule didn't harden can
  still be opened from the sandbox.
- **The logon SID** is one of the restricting SIDs, so a sandboxed
  program can write to objects that grant it, such as other processes'
  named objects in your session. It needs this to run at all.
- **The first grant on a big tree is slow.** An inheritable ACE on a
  workspace with many files propagates through all of them once. Later
  runs find the ACE already there and skip the work.

## Unconfined MCP servers

A server with `sandbox = false` runs under the same token without write
confinement: no capabilities and no `WRITE_RESTRICTED`, but Authenticated
Users still deny-only and still in a job. It can write anywhere you can,
and it can't read ferrule's secrets, your `deny_read` paths or ferrule's
process. Doctor lists these servers.
