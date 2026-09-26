# M26: isolation (design)

Status: built, 2026-09-25, branch `m26-isolation` (PR to main, not
merged). Three gaps close here:

1. Native Windows had no sandbox at all.
2. Reads were open everywhere. A command could `cat ~/.ssh/id_ed25519`.
3. The M10 leftovers: plain-HTTP `web_fetch` skipped the credential
   proxy, and MCP servers with `sandbox = false` ran fully open.

The research behind the Windows part is
[research-windows-sandbox.md](research-windows-sandbox.md). The user
guides are [sandbox.md](sandbox.md) and
[windows-sandbox.md](windows-sandbox.md). Where the build departs from
this design: **As built** at the end.

## 1. The read policy

### What is denied

One list, `Sandbox::read_denies()`, feeds every enforcer: Landlock,
Seatbelt, the Windows backend, and the in-process file tools
(`read_file`, `write_file`, `list_dir`). It is the union of three parts:

| Part | Paths | Can be un-denied? |
|---|---|---|
| ferrule's secrets (`policy.hidden`, host-filled) | `<data>/private` (saved keys, the connections store and its key, the hooks trust list, dashboard sessions), `<data>/proxy/keys`, `<data>/learn`, and new in M26 `<data>/sessions` (transcripts: every secret a tool ever printed) | no |
| the default credential dirs (`deny_default_reads`, on by default) | `~/.ssh`, `~/.aws`, `~/.azure`, `~/.config/gcloud`, `~/.kube`, `~/.docker/config.json`, `~/.netrc`, `~/.git-credentials`, and the browser profiles: Chrome, Chromium, Edge, Brave and Firefox, in each OS's own place | yes, with `allow_read` |
| the owner's own additions (`deny_read`) | anything | no, the owner wrote it |

`allow_read` removes a default entry when the entry is equal to or under
an allowed path. For example, `allow_read = ["~/.ssh/known_hosts"]` won't
open `~/.ssh`, but `allow_read = ["~/.ssh"]` will. It never touches the
first part: ferrule's own secrets can't be opened from the config, because
a prompt-injected config edit must not be able to open them either.

Like `hidden`, missing paths are skipped when a command starts. Seatbelt
and Landlock both need a real inode or a real path. A `~/.aws` created
while a command runs is covered from the next command on.

`/proc/<ppid>/environ` is the process-side twin. Ferrule's environment
holds the real values the proxy swaps in:

- **Linux** already blocks it. Landlock's ptrace scoping stops a domain
  from reading another process's `environ`. M26 adds a test that runs
  from an MCP child too.
- **macOS** has no `/proc`, and the base Seatbelt profile allows
  `process-info*` only within the same sandbox (`target same-sandbox`), so
  ferrule's own process is out of reach.
- **Windows** is new, see §2.4.

### Linux: Landlock

Nothing new is needed in the mechanism. The existing carve already cuts
hidden paths out of the read grant on `/` and out of every writable root.
M26 feeds it the full list. The cost is a few more rules: one per entry of
each directory on the way down to a denied path (`/`, `/home`, `~`). The
known limit carries over: file *names* inside a denied directory stay
listable (READ_DIR is inherited), but their contents don't.

### macOS: Seatbelt

Nothing new is needed in the mechanism either. The hidden `deny` rules
come last in the profile, so they override `(allow file-read*)`.

### The file tools

`resolve()` already refuses `hidden`. It now takes `read_denies()`, and
its message says which kind of path was hit. The tools are
workspace-confined, so the list only matters when the workspace contains
one of these paths, for example when the workspace is `~`.

### Config

```toml
[sandbox]
deny_read = ["~/work/.env.production"]   # extra paths, never readable
allow_read = ["~/.kube"]                 # re-open a default deny (kubectl wants its config)
deny_default_reads = true                # the default list above
```

`ferrule sandbox` prints the effective list and tests one entry.
`ferrule doctor` counts the list and says, per platform, which parts are
enforced.

## 2. The Windows tier-1 backend

No admin, no service, no driver. The design is Codex's unelevated
backend (`codex-rs/windows-sandbox-rs`, Apache-2.0), adapted.

### 2.1 Process shape

`std::process::Command` can't take a token, so `Sandbox::command` on
Windows returns a `Command` that runs a **launcher**, which then starts
the real program:

```
ferrule ──Command──▶ launcher (normal token) ──CreateProcessAsUserW──▶ program (restricted token, in the job)
```

- **The launcher** is `ferrule.exe __sandbox-launch`, intercepted in
  `main()` before clap, or the small `ferrule-sandbox-launch.exe` bin in
  the sandbox crate for tests and other embedders. The lookup order is:
  `FERRULE_SANDBOX_LAUNCHER`, then the current exe if it is `ferrule`,
  then either name next to the current exe or in its parent dir (test
  binaries live in `deps/`). If none is found, the backend reports
  itself unavailable, with the reason.
- **The spec** travels in one env var, `FERRULE_SANDBOX_SPEC` (JSON:
  program, args, writable roots, denied reads, limits). The launcher
  removes it before building the child's environment. The environment
  itself is the one `Command` already built (scrubbed, placeholders set),
  so the existing env code stays as it is.
- **The job**: `KILL_ON_JOB_CLOSE`, `ACTIVE_PROCESS` (default 256),
  optional `JOB_MEMORY` (`memory_mb`), `DIE_ON_UNHANDLED_EXCEPTION`, and
  no breakaway. The launcher holds the only handle. When the launcher dies
  for any reason (`Child::kill`, a timeout, ferrule crashing), the handle
  closes and the whole tree goes with it. The child goes into the job at
  creation (`PROC_THREAD_ATTRIBUTE_JOB_LIST`), so there is no window
  where it runs outside.
- **Stdio**: only the three std handles are inherited
  (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`). The launcher exits with the
  child's exit code.
- **`lpDesktop = "winsta0\\default"`**: without it PowerShell fails with
  `STATUS_DLL_INIT_FAILED` under a restricted token (Codex's finding).
- **Program lookup**: the launcher resolves the program through `PATH`
  and `PATHEXT` itself. `.cmd` and `.bat` run through `cmd.exe /d /c`.
  The command line is built with MSVC quoting.

### 2.2 The token and the writes

`CreateRestrictedToken(own token, DISABLE_MAX_PRIVILEGE | LUA_TOKEN |
WRITE_RESTRICTED)`:

- **Restricting SIDs** are the capability SID for each writable root,
  then the logon SID and Everyone. Under `WRITE_RESTRICTED` the second
  access check runs only for write access. Reads and execution are
  judged as for the user; writes succeed only where a DACL also grants
  one of the restricting SIDs.
- **Capability SIDs**: one random `S-1-5-21-a-b-c-d` per writable root,
  persisted in `<data>/sandbox/windows-caps.json`
  (`FERRULE_SANDBOX_STATE` overrides the location). Per root, not per
  install. If a root is granted once and later dropped from the policy,
  its ACE stays on disk, but no later token carries that root's SID, so
  the stale ACE grants nothing.
- **Grants**: an inheritable allow ACE (modify rights, no `WRITE_DAC`, no
  `WRITE_OWNER`) for the root's SID, added once. It is idempotent: the
  launcher checks the DACL before writing. It covers the workspace, the
  temp dirs (`%TEMP%`, `%TMP%`) and `writable_roots`.
- **Default DACL** of the token is logon SID `GENERIC_ALL` plus
  OWNER RIGHTS `READ_CONTROL`. The program can open its own objects, and
  one sandboxed tree can't take over another.
- **Disabled SIDs**: Authenticated Users (`S-1-5-11`) is deny-only in the
  sandbox token. §2.3 is why.

Read-only mode is the same token with no capability SIDs at all. Only
logon and Everyone restrict, and nothing on disk grants those write
(checked: the null device and the console are handled by their own
DACLs).

### 2.3 Denied reads on Windows

A deny-read ACE for a capability SID does nothing under
`WRITE_RESTRICTED`, because the restricting SIDs are never consulted for
reads. The research doc assumed otherwise. What works instead has to
key on something the *normal* access check sees and the sandbox token
lacks:

- The sandbox token has Authenticated Users (AU) as deny-only. Every
  normal logon token has AU enabled.
- Ferrule's own secret dirs get a protected DACL, written with SDDL:
  `D:PAI(A;OICI;FA;;;SY)(XA;OICI;FA;;;<user>;(Member_of {SID(AU)}))(A;OICI;RC;;;OW)`.
  SYSTEM has full access. The user has full access **only while AU is
  enabled** in their token (a conditional ACE: `Member_of` ignores
  deny-only groups in allow ACEs). The owner gets `READ_CONTROL` only.
  Without that last ACE, the implicit owner rights would let a sandboxed
  program rewrite the DACL.
- Ferrule, Explorer, an editor, or an elevated admin shell all keep full
  access. A sandboxed program is refused.

This applies to ferrule's hidden dirs (`private`, `proxy/keys`, `learn`,
`sessions`), whose ACLs ferrule owns. It does **not** apply to
`~/.ssh`, the cloud credential dirs or the browser profiles. Rewriting
the DACLs of the user's own dirs is too invasive. OpenSSH on Windows
checks its key file ACLs strictly, and the browsers own their profile
ACLs. On Windows those entries are enforced by the file tools only.
`ferrule doctor` and `ferrule sandbox` say so. The owner can opt a path
in with `deny_read`, which ferrule treats as its own to protect.

If CI shows the conditional ACE doesn't hold, the fallback is a Low
integrity label with `NO_READ_UP` on the protected dirs and a Low
sandbox token. That is heavier, because MSYS needs a writable
`/tmp`, and it is kept as a follow-up rather than a second code path.

### 2.4 Ferrule's own process

On Windows the `environ` twin is `OpenProcess(PROCESS_VM_READ)` on
ferrule, which the same user SID normally allows. `Sandbox::new` on
Windows applies the same conditional DACL (as a kernel-object ACL, with
`PROCESS_ALL_ACCESS` in place of file rights) to ferrule's own process
and every thread it has, and sets it as the token's default DACL. Threads
and processes created later, including the launcher, inherit it. The
launcher re-applies it to itself before starting the program. This is
idempotent, and nothing but a sandboxed token is refused.

### 2.5 Network

Tier 1 does not enforce `network = false` on Windows. Blocking needs WFP,
which needs admin. Commands still get `HTTPS_PROXY` and the placeholders,
so credential injection works for tools that honour the proxy variables.
A program that ignores them goes out directly, and with placeholders only.
With `network = false` on Windows, `detect()` reports the backend as
active, and `ferrule doctor` warns that network is unenforced. The
alternative was refusing to start, but writes and reads are still worth
confining.

### 2.6 Shells

Git Bash (MSYS2) and PowerShell both run under the token in CI. For
MSYS, the risk is its `/tmp` and the `cygdrive` mount table under a
restricted token. If a shell can't hold tier 1, it runs unsandboxed with
the existing warning, and `ferrule doctor` names it. `Shell::detect`
already knows which shell the tool will use.

## 3. The M10 edges

### 3.1 Plain HTTP through the proxy

The proxy gains absolute-form forwarding: a non-CONNECT request whose
URI is `http://host[:port]/…`.

- The same `Proxy-Authorization` check, the same upstream handling
  (`HTTP_PROXY`/`http_proxy` for the upstream now too, honouring
  `NO_PROXY`), and hop-by-hop headers stripped. The request goes out in
  origin form (or absolute form to an upstream proxy), and the response
  is scrubbed like HTTPS responses are.
- **Credential injection over plain HTTP**: only when the target is
  loopback (`127.0.0.0/8`, `::1`, `localhost`). A bound host on any other
  address over `http://` gets **403 "secrets only go over HTTPS"**. It
  does not get a silent unswapped request, which would send a
  placeholder-bearing request out in cleartext and fail confusingly.
  Unbound hosts are forwarded as they are.
- Upgrades (websockets) get 501, as they do on the MITM path.

`web_fetch` (and ferrule's other in-process clients through
`egress::client_builder`) switch from `Proxy::https` to `Proxy::all`, so
`http://` goes through the proxy too.

Commands keep `HTTPS_PROXY` only. Setting `HTTP_PROXY` for every command
would be a behaviour change for local dev servers that don't honour
`NO_PROXY`. That is a follow-up, listed.

### 3.2 Unconfined MCP servers

`Sandbox::unconfined` (for `sandbox = false`) no longer turns everything
off. It becomes **hide-only**:

- **Linux**: Landlock, with the carve applied to reads and writes of
  `/` itself. The server can write anywhere the user can, but it can't
  read ferrule's secrets or the credential dirs, and ptrace scoping keeps
  it out of ferrule's `environ`. No seccomp: network stays on.
- **macOS**: `(allow default)` plus the hidden denies. A server that
  opted out because Seatbelt broke it keeps working.
- **Windows**: the restricted token without `WRITE_RESTRICTED` and
  without capability SIDs, but with AU deny-only, in a job. Writes are as
  open as the user's, ferrule's secret dirs and process are closed, and
  so is the job's kill-on-close.
- If the backend isn't there at all, it stays fully open, as before.

Doctor then warns per server, listing only what stays open ("writes
anywhere you can", "network unenforced", "~/.ssh and browser profiles
readable on Windows").

On Windows every MCP server is currently effectively unconfined. With the
new backend, `sandbox = true` servers get the full tier 1, and doctor
reports them by name if the probe failed.

### 3.3 Left alone

The system-service path (`ferrule service install`) is not run end to
end here. It is listed as not verified live.

## 4. Tests

- **Hermetic, every OS**: the policy parsing (`deny_read`, `allow_read`,
  `deny_default_reads`), `read_denies()` merging and allow rules, the
  per-OS default lists, the Windows spec round-trip and MSVC quoting
  (compiled everywhere), the SDDL strings, the proxy's plain-HTTP
  forwarding with in-process target and proxy on 127.0.0.1 (injection to
  loopback, 403 to a remote bound host, 407 without credentials, upstream
  chaining), and `web_fetch` over `http://` through an in-process broker.
- **Real enforcement, per OS in CI** (`crates/ferrule-sandbox/tests/`):
  denied reads fail and allowed reads work, from the shell and from an
  MCP-style child (the test binary re-execs itself as a long-lived
  helper under `for_helper` and under `unconfined`).
- **Windows tier 1** (`tests/windows.rs`), for both Git Bash
  (`C:\Program Files\Git\bin\bash.exe`) and PowerShell: a write outside
  the workspace is denied, a write inside works, the job kills the child
  (a `sleep` grandchild dies with the launcher), and the saved-keys dir
  is unreadable. It also checks that ferrule's process can't be opened
  for reading from inside the sandbox.
- `ferrule sandbox` self-test runs on Windows in CI (the step was
  skipped before).

## As built

Built in four parts on `m26-isolation`: the read policy, plain HTTP
through the proxy, the Windows backend, and hide-only unconfined servers
together with doctor and the self-test. Where the build differs from the
design above:

- **The config keys** are the ones in §1, plus `process_limit` (256,
  0 = none) and `memory_mb` (unset = none). Both are Windows-only and are
  ignored elsewhere.
- **Windows writes also cover `%TEMP%` and `%TMP%`** when `tmp = true`,
  because `/tmp` has no counterpart there.
- **The restricting SIDs gain RESTRICTED (`S-1-5-12`).** The named-object
  directories and a few registry keys grant it to restricted code. Without
  it, MSYS and PowerShell can't create their named objects under a
  `WRITE_RESTRICTED` token.
- **What gets a protected DACL** is `Sandbox::owned_denies()`: `hidden`
  plus `deny_read`. The default credential dirs are left alone, as §2.3
  planned. Doctor and `ferrule sandbox` tag them "(file tools only)" on
  Windows.
- **No separate probe for §2.3.** The real tier-1 tests
  (`saved_keys_and_denied_paths_are_unreadable`, under `sandbox`,
  `for_helper` and `unconfined`) are the probe. If CI shows the
  conditional ACE doesn't hold, they fail, and the Low-integrity fallback
  becomes the fix. There is no probe test to delete.
- **The start-up probe on Windows runs the shell the `shell` tool will
  use** (`Shell::get()`, with `exit 0`) instead of `/bin/sh`. A shell that
  can't start under the token therefore degrades the sandbox with the
  existing warning, and doctor names it.
- **Git Bash can't hold tier 1** (§2.6's risk, confirmed by CI). MSYS
  ACLs its signal pipe and its per-user shared memory to the user's SID,
  and under `WRITE_RESTRICTED` a write also needs a restricting SID.
  Bash dies with `couldn't create signal pipe, Win32 error 5` or
  `CreateFileMapping S-1-5-21-…, Win32 error 5`. Adding the user SID to
  the restricting list would re-open every write, so Git Bash degrades
  as §2.6 says.
  - The new `FERRULE_SHELL=powershell|bash` picks the shell; unset still
    prefers Git Bash.
  - Doctor's degrade hint suggests the switch.
  - The tier-1 tests run in PowerShell. `tests/windows_git_bash.rs`
    checks in its own process that Git Bash is either confined or
    reported degraded, never half-applied.
  - CI's Windows self-test sets `FERRULE_SHELL=powershell`.
  - An automatic switch to PowerShell was rejected: the brief says to
    degrade, and a silent change of shell also changes the syntax the
    model must write.
- **PowerShell under the token runs in ConstrainedLanguage mode** (seen in
  CI). The UTF-8 output prelude sets a .NET property, which that mode
  refuses, so it now runs only in FullLanguage, and a tier-1 test asserts
  that a PowerShell command's stderr holds no error record.
- **The restricted token's handle** needs `TOKEN_ADJUST_DEFAULT` on the
  source token so its default DACL can be set (found by CI).
- **`harden_self()` is a no-op** when ferrule's own token lacks an enabled
  Authenticated Users. Otherwise the condition would lock ferrule out of
  itself, as for a service account.
- **Hide-only on Linux** grants `/` read and write, carved around the
  deny list, with no seccomp. The carve has one limit: a *new* entry
  directly inside a directory that contains a denied path can't be
  created, for example at the top of `~` because `~/.ssh` is denied
  there. Landlock grants rights on the directory's existing children, and
  it has no rule for "new children except this one". Existing entries and
  everything below them stay writable. This is documented in
  `docs/sandbox.md` and listed as a follow-up.
- **Hide-only on macOS** is a separate profile, `(allow default)` plus
  the same deny rules, not the confined profile with extra allows.
- **`Sandbox` gained** `hides_reads()` (the deny list is enforced, which
  is also true for hide-only) and `is_hide_only()`. `is_active()` stays
  false for hide-only, so everything that meant "confined" still does.
- **Doctor**:
  - it checks the saved keys on every OS (`cmd.exe /d /c type` on
    Windows);
  - on Windows it warns about `network = false` and the file-tools-only
    defaults;
  - it names the shell when Windows degrades;
  - it warns per `sandbox = false` server with the gaps for the backend
    it has.
- **`ferrule sandbox`**:
  - it prints the "no reads" list;
  - it runs its write and read checks through `cmd.exe` on Windows;
  - it checks ferrule's own process through a hidden
    `--probe-process <pid>` re-exec;
  - it skips the network check on Windows.

  CI now runs the self-test on Windows too.
- **The test MCP server** (`mock_mcp.py`) gained a `read` tool. With it,
  the denied-read test runs from a real MCP child, sandboxed and
  `sandbox = false`, on every OS.

**Not verified live:**
- Everything Windows until this PR's CI run: nothing Windows can run in
  the build container (no Wine, no MSVC). Cross-clippy covers
  `ferrule-sandbox` for `x86_64-pc-windows-gnu`.
- macOS's hide-only profile until CI.
- The system-service path, as §3.3 says.
- Plain HTTP through the proxy to a real remote server. The tests use
  in-process servers on 127.0.0.1.

**Follow-ups:**
- `HTTP_PROXY` for commands (§3.1).
- Network enforcement on Windows (WFP, which needs admin: a tier 2).
- The Landlock carve limit for new entries beside a denied path.
- The Low-integrity fallback, if the conditional ACE turns out not to
  hold somewhere.
- Git Bash under hide-only (no `WRITE_RESTRICTED`, so MSYS should start):
  reads still confined when its writes can't be.
