# Windows Sandboxing: Research for Ferrule

**Research report, September 24, 2026**

Researched from primary sources only: Microsoft Learn/Win32 API docs, the real source of
OpenAI Codex CLI's Windows sandbox (`github.com/openai/codex`, crates `windows-sandbox-rs`,
`mxc-sandbox`, `core`, `protocol`, `tui`), and Chromium's sandbox design doc. Native Windows
is currently the one platform `ferrule-sandbox` has no backend for — `Sandbox::detect()`
(`crates/ferrule-sandbox/src/lib.rs`) hard-fails on `#[cfg(windows)]` with `"Windows has no
sandbox backend yet (under WSL2, the Linux build has one)"`. This report is scoped to that gap.

## TL;DR

- **Recommended mechanism: a self-derived restricted token + AppContainer-style capability SID,
  applied via `CreateRestrictedToken` + explicit ACEs, wrapped in a Job Object.** This is exactly
  what OpenAI Codex CLI calls its "Unelevated" / `RestrictedToken` backend
  (`codex-rs/windows-sandbox-rs/src/token.rs`, function `create_workspace_write_token_with_caps_from`).
  It needs **no admin rights and no one-time elevated setup** — confirmed both by Microsoft's own
  docs on `CreateRestrictedToken` ("If a process calls `CreateProcessAsUser` using a restricted
  version of its own token, the calling process does not need to have the
  `SE_ASSIGNPRIMARYTOKEN_NAME` privilege" — [Win32 docs](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-createrestrictedtoken))
  and by Chromium's sandbox doc, which states plainly that its (architecturally similar)
  restricted-token + job-object + alternate-desktop design is "a user-mode only sandbox... the
  user does not need to be an administrator in order for the sandbox to operate correctly."
- **A second, stronger mode needs one-time elevated setup**: Codex's "Elevated" backend runs the
  sandboxed process under a **dedicated low-privilege Windows account** created by an elevated
  setup helper (`identity.rs`: `setup::OFFLINE_USERNAME`/`ONLINE_USERNAME`, `LogonUserW`), and its
  network blocking via the Windows Filtering Platform (WFP) is explicitly documented as needing
  to "run from the already-elevated setup helper" (`wfp.rs`, `install_wfp_filters_for_account`).
  Ferrule should build the unelevated mode first and treat WFP/dedicated-account as a later,
  opt-in upgrade — matching Codex's own two-tier design.
- **Hidden paths inside a writable workspace are solvable**: an explicit, inheritable **deny ACE**
  for the sandbox's capability SID, added after every grant (mirroring the existing Seatbelt
  backend's own "hidden paths last" pattern in `seatbelt.rs`), added via `SetEntriesInAclW` +
  `SetSecurityInfo`. Windows evaluates deny ACEs before allow ACEs in the same DACL, so this
  works even though the parent directory is otherwise writable
  (`codex-rs/windows-sandbox-rs/src/acl.rs`, `add_deny_read_ace`/`add_deny_write_ace`).
- **Child-of-a-child confinement is a Job Object default**, not something Ferrule has to build:
  once a process is assigned to a job, "by default any child processes it creates... are also
  associated with the job" unless the child explicitly opts out with `CREATE_BREAKAWAY_FROM_JOB`
  *and* the job allows `JOB_OBJECT_LIMIT_BREAKAWAY_OK` — so Ferrule's job must simply never set
  that limit ([Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)).
- **Top open risk, marked unverified below**: whether Git Bash and PowerShell actually *launch*
  under a restricted token with a low-box/AppContainer-style SID attached, and whether
  network-off still permits loopback (localhost dev servers). I could not find a primary
  Microsoft doc confirming either for a bare restricted token (as opposed to a full
  AppContainer); see §3.

---

## 1. Mechanisms for an unelevated, no-admin, no-driver process

### 1.1 Restricted tokens (`CreateRestrictedToken`)

[`CreateRestrictedToken`](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-createrestrictedtoken)
derives a new token from an existing one (typically the caller's own primary token) by disabling
SIDs, removing privileges, and/or adding **restricting SIDs**. Key facts from the doc:

- Flags: `DISABLE_MAX_PRIVILEGE` (disables all privileges except `SeChangeNotifyPrivilege`),
  `SANDBOX_INERT` (a hint other components can check), `LUA_TOKEN` (creates a "Limited User
  Account" token, stripping admin-equivalent SIDs), `WRITE_RESTRICTED` (restricts only write
  access, not read/execute — relevant to Ferrule's "reads stay open, writes are confined" policy).
- Dual access-check semantics, quoted verbatim: **"The system performs two access checks: one
  using the token's enabled SIDs, and another using the list of restricting SIDs. Access is
  granted only if both access checks allow the requested access rights."** A restricting SID adds
  a *second, independent* gate on top of the normal DACL check — this is the primitive both
  Codex's capability-SID system and (conceptually) AppContainer's capability model build on.
- No elevation needed for the common case: **"If a process calls `CreateProcessAsUser` using a
  restricted version of its own token, the calling process does not need to have the
  `SE_ASSIGNPRIMARYTOKEN_NAME` privilege."** This is what makes the "Unelevated" design possible.
- Explicit warning: **"Applications that use restricted tokens should run the restricted
  application on desktops other than the default desktop"** — ties directly into Job Objects' UI
  restrictions and Chromium's alternate-desktop layer (§1.4, §2.2).

Codex's real code (`codex-rs/windows-sandbox-rs/src/token.rs`) builds exactly this: a restricted
token from the **current signed-in user's own token**, with capability SIDs from `cap.rs` as
restricting SIDs — `create_workspace_write_token_with_caps_from(base_token, psid_capabilities)`
and `create_readonly_token_with_caps_from(...)`. A second pair of functions,
`create_workspace_write_token_with_caps_and_user_from(base_token, psid_capabilities,
additional_restricting_sids)`, is documented in-code as "intended for the elevated sandbox
backend, where the token user is the dedicated sandbox account rather than the real signed-in
user" — i.e. the same primitive, just fed a different base token.

**Can enforce**: write restriction to specific roots (via `WRITE_RESTRICTED` + object ACLs keyed
to a capability SID — see §1.3/§4), removal of privileges (no `SeDebugPrivilege`,
`SeBackupPrivilege`, etc. against the rest of the system), a second access-check gate independent
of the normal DACL.
**Cannot enforce alone**: network blocking (a token has no network policy — needs WFP, §1.5, or
AppContainer network capabilities), process/child confinement (needs a Job Object, §1.4), reading
of *other* processes' memory/environment (partially covered by privilege removal, but the real
guarantee comes from integrity levels + the restricting SID also gating `PROCESS_VM_READ`/`OpenProcess`
ACL checks against Ferrule's own process object, §1.2).

### 1.2 Integrity levels (mandatory labels)

Not independently re-verified this pass beyond what Chromium's design doc states (§2.2): Chromium
runs its most restricted (renderer) processes at the "Untrusted" integrity level as one of its
four core layers. Windows' mandatory integrity control blocks a lower-IL process from opening a
higher-IL process/thread/object for write access (`PROCESS_VM_WRITE`, `WRITE_DAC`, etc.) even if
the DACL would otherwise allow it — this is the mechanism that would stop a sandboxed child from
reaching back into Ferrule's own process object to read its environment block (the Windows
analogue of `/proc/<pid>/environ`), on top of whatever the restricted token already denies.
**I did not fetch a dedicated Microsoft Learn page on mandatory integrity control levels this
pass — this paragraph is inferred from the restricted-token doc's warning plus Chromium's design
doc, not from a Microsoft integrity-level primary source. Marking the specific claim "low IL
blocks `OpenProcess` for write against a higher-IL target" as unverified against a Microsoft
primary source in this report**, though it is well-established Windows security architecture.

### 1.3 AppContainer / LPAC

[AppContainer isolation](https://learn.microsoft.com/en-us/windows/win32/secauthz/appcontainer-isolation)
defines six isolation categories: Credential, Device, File, Network, Process, and Window
isolation. On file isolation, the doc states: "Read-write access can be granted to specific
persistent files and registry keys. Read-only access is less restricted." On network isolation:
"Granular access can be granted for Internet access, Intranet access, and acting as a server" —
i.e. AppContainer's network story is capability-based (declare `internetClient`, etc.), not a
firewall configured per-run.

[`CreateAppContainerProfile`](https://learn.microsoft.com/en-us/windows/win32/api/userenv/nf-userenv-createappcontainerprofile)
is the entry point for a *non-packaged* Win32 app to get an AppContainer SID without MSIX: "Creates
a per-user, per-app profile for an AppContainer... The function creates a profile for the current
user." No admin requirement is stated in the Requirements table (target is "Windows 8 desktop
apps"), and the failure mode for insufficient rights is `E_ACCESSDENIED`, which the doc does not
tie to admin — consistent with AppContainer being designed for use by standard user processes (it
is how UWP/Store apps, which never run elevated, get their sandboxing). I did **not** find or
fetch Microsoft's LPAC (Less Privileged AppContainer)-specific doc in this pass — LPAC adds the
`lpacAppExperience`/`lessPrivilegedAppContainer` capability that further restricts even default
AppContainer capabilities; **the LPAC-specific behavior beyond the general AppContainer doc above
is unverified in this report.**

Direct evidence of feasibility without a private SDK: Codex's `windows-sandbox-rs` crate depends
on the public `windows` crate (`windows = { version = "0.58", features = ["Win32_Foundation",
"Win32_NetworkManagement_WindowsFirewall", "Win32_System_Com", "Win32_System_Variant"] }` —
`codex-rs/windows-sandbox-rs/Cargo.toml`) and does not use `CreateAppContainerProfile` at all for
its "legacy"/"Unelevated" backend — it uses plain restricted tokens with capability SIDs that are
**not** real AppContainer SIDs, just restricting SIDs used the same way AppContainer uses
capability SIDs (see `cap.rs`, §1.1). Only Codex's newer, closed-source "MXC" backend
(`codex-rs/mxc-sandbox`) uses a real Microsoft-internal AppContainer runner
(`appcontainer_common::base_container_runner::BaseContainerRunner`) — not usable by third parties,
confirmed by the crate's own doc comment: "This crate routes a command through the current Codex
executable and directly into Microsoft's MXC `BaseContainerRunner`... It never invokes MXC's
AppContainer dispatcher, edits host ACLs, creates sandbox users, runs setup, or requests
elevation." **Conclusion for Ferrule: real AppContainer via `CreateAppContainerProfile` is
plausible and admin-free per the Microsoft doc, but Codex — the one real, actively-maintained
prior-art project examined here — chose plain restricted tokens over it for its no-setup mode,
which is evidence AppContainer profile creation carries friction (registry/profile artifacts,
possibly requiring cleanup — see §3) that a restricted token avoids.**

### 1.4 Job Objects

[Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects) give
process-tree containment and resource/UI limits, no elevation needed to create or assign one.
Key facts:

- Child confinement is the **default**, not opt-in: "After a process is associated with a job, by
  default any child processes it creates using `CreateProcess` are also associated with the job."
  A child escapes only via `JOB_OBJECT_LIMIT_BREAKAWAY_OK` (needs the child to pass
  `CREATE_BREAKAWAY_FROM_JOB` explicitly) or `JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK` (silent, no
  flag needed on the child, escapes *all* children). **Trap for Ferrule: simply never set either
  limit** — the safe state is the default.
- `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: "closing the last job object handle terminates all
  associated processes and then destroys the job object itself" — this is the Windows analogue of
  Ferrule's existing Unix `kill_process_group` (`lib.rs`, currently a `#[cfg(unix)]`-only SIGKILL,
  with a no-op stub on other platforms that a Windows implementation should replace). Job-Object-
  based kill also naturally cleans up grandchildren, which a raw process-group signal does not
  always guarantee.
- Nested jobs (a job inside a job) are only supported since Windows 8 / Server 2012 — worth noting
  since it constrains combining Ferrule's own job with any job a spawned build tool might itself
  create.
- UI restrictions (`JOB_OBJECT_UILIMIT_*`, e.g. blocking clipboard, `SystemParametersInfo`,
  desktop handles) are set via `SetInformationJobObject` — a **narrower** alternative to a full
  alternate desktop, possibly a cheaper first cut if desktop-switching turns out to have
  compatibility issues with console shells (unverified how PowerShell/Git Bash behave under
  `JOB_OBJECT_UILIMIT_HANDLES`, §3).

### 1.5 Windows Filtering Platform (WFP) for network blocking

A restricted token or Job Object has no network policy of its own — Ferrule's existing
`Policy.network: bool` needs a separate mechanism on Windows. Codex's `wfp.rs`
(`install_wfp_filters_for_account`) installs **persistent, account-scoped** WFP filters: the doc
comment states this "is intended to run from the already-elevated setup helper," and filters are
scoped with `FWPM_CONDITION_ALE_USER_ID` (an ALE_USER_ID condition blob built from the target
account's SID) and flagged `FWPM_FILTER_FLAG_PERSISTENT`. **This confirms, from real code, that
Codex's WFP-based network block requires one-time elevated setup and a stable account/SID to
scope filters to — it is not applied ad hoc, per-process, without setup.** This directly supports
recommending network blocking as part of the *second-tier* ("Elevated") mode rather than the
first cut (§5).

AppContainer's own network isolation (§1.3, capability-based: omit `internetClient` /
`internetClientServer` / `privateNetworkClientServer` capabilities to deny network) is the
no-admin alternative in principle, but whether omitting those capabilities also blocks
**loopback** could not be verified in this pass — see §3.

### 1.6 ACL-based alternatives (per-sandbox SID on the workspace)

Not really an *alternative* to the restricted-token approach — it's the mechanism that makes it
work for file access at all, and it is also directly the answer to Q4 (§4). A capability SID (real
AppContainer SID or, as in Codex's legacy backend, a private randomly-generated restricting SID)
is meaningless for access control until something attaches it to a DACL: either (a) the OS
auto-grants AppContainer SIDs limited access to their own profile folder (§1.3), or (b) Ferrule
explicitly grants/denies that SID on specific paths via `SetEntriesInAclW`/`SetSecurityInfo`, as
Codex's `acl.rs` does for both the writable workspace root (grant) and hidden subpaths (deny).
This is a real, working, already-shipped design (§4), not a theoretical alternative.

---

## 2. How real shipping projects do it

### 2.1 OpenAI Codex CLI (`github.com/openai/codex`, crate family `codex-rs`)

Codex ships **three** selectable Windows sandbox implementations
(`WindowsSandboxImplementation::{Elevated, Unelevated, Mxc}` in
`codex-rs/tui/src/windows_sandbox.rs`; both `Elevated` and `Unelevated` map to the same
`SandboxType::WindowsRestrictedToken` in `codex-rs/protocol/src/sandbox.rs` — confirming they are
architecturally the *same mechanism*, differing only in whose base token is restricted):

- **Unelevated / RestrictedToken** (the one directly relevant to Ferrule's "no admin" constraint):
  restricts the **current user's own token** (`token.rs`,
  `create_workspace_write_token_with_caps_from`), attaches capability SIDs from `cap.rs`
  (`CapSids { workspace, readonly, workspace_by_cwd, writable_root_by_path }` — per-workspace SIDs
  so denies/grants for one workspace don't leak into another), grants/denies file access via
  explicit ACEs (`acl.rs`, §4), and — per `codex-rs/core/src/windows_sandbox.rs` — still runs a
  non-elevated **preflight** step (`run_legacy_setup_preflight()`) even in this mode, i.e. it is
  low-setup, not zero-setup.
- **Elevated**: same token-restriction machinery, but the base token belongs to a **dedicated,
  low-privilege Windows account** created by a one-time elevated setup helper (`identity.rs`
  imports `setup::OFFLINE_USERNAME`/`ONLINE_USERNAME`, calls `LogonUserW` with
  `LOGON32_LOGON_INTERACTIVE`). This tier is also the only one wired to persistent WFP network
  filters (§1.5). `core/src/windows_sandbox.rs` has a distinct `WindowsSandboxSetupMode::Elevated`
  path (`prepare_elevated_sandbox()` → `codex_windows_sandbox::run_elevated_setup(...)`,
  Windows-only; the non-Windows build stub simply `anyhow::bail!`s).
- **MXC**: wraps a Microsoft-internal, closed-source AppContainer SDK
  (`appcontainer_common::base_container_runner::BaseContainerRunner`). Confirmed not reusable by a
  third party (no source, no public API) — but its README is still useful evidence of what a
  "real" managed AppContainer setup considers achievable without elevation or host-ACL edits: "It
  never invokes MXC's AppContainer dispatcher, edits host ACLs, creates sandbox users, runs setup,
  or requests elevation," and on networking: "Supported managed network access allows IPv4 and
  IPv6 loopback clients and servers, including the dedicated proxy listeners, while denying direct
  non-loopback egress and general inbound network access" (`mxc-sandbox/README.md` — the one piece
  of evidence found that loopback can survive a network-off AppContainer-style policy, but it
  describes Microsoft's private runner, not a general Win32 guarantee, so the general claim stays
  marked unverified in §3 while citing this as project-specific precedent). MXC also documents an
  AppContainer-specific limitation worth carrying into Ferrule's own design notes: volume-root
  grants "do not recurse" (root + immediate children only), and the child environment cannot be
  explicitly emptied (an SDK limitation, not a Ferrule design choice).

Test-coverage pattern worth copying (§5 CI plan): the MXC README states "Portable tests validate
policy translation and wrapper arguments. Actual enforcement... require[s] the smoke suite on
supported Windows" — Codex itself splits its Windows sandbox tests into (a) pure policy/ACL-plan
logic, testable on any OS, and (b) real enforcement, tested only on Windows CI runners. This is
the same split Ferrule's Seatbelt backend already uses (SBPL-text-generation tests run on every OS
in CI; actual enforcement is unverified until run on real macOS).

### 2.2 Chromium (`docs/design/sandbox.md`, `sandbox/win`)

Chromium's Windows sandbox (`chromium.googlesource.com/chromium/src/+/main/docs/design/sandbox.md`)
layers four mechanisms on its most-restricted (renderer) processes:

1. **Restricted token** — "heavily stripped token lacking most privileges," running at the lowest
   "Untrusted" integrity level.
2. **Job Object** — "system-wide restrictions like preventing desktop switching, clipboard access,
   and child process creation."
3. **Alternate desktop** — isolates sandboxed windows from the interactive desktop, preventing
   message-based (shatter) attacks — directly matches `CreateRestrictedToken`'s own warning (§1.1)
   about running restricted apps off the default desktop.
4. **Integrity levels** — mandatory access control, renderer at "Untrusted."

Elevation is explicitly disclaimed: **"The Windows sandbox is a user-mode only sandbox. There are
no special kernel mode drivers, and the user does not need to be an administrator in order for
the sandbox to operate correctly."** Architecture is a **broker/target split**: the broker
(browser process, unsandboxed) "specif[ies] the policy for each target process," spawns targets,
and "perform[s] the policy-allowed actions on behalf of the target process" via IPC-intercepted
Windows API calls — heavier machinery than Ferrule plausibly needs (Ferrule sandboxes a shell
command, not a renderer process parsing attacker HTML), but it independently corroborates the
core primitive stack: restricted token + job object + integrity level, admin-free. Newer hardening
(ASLR/CFG/ACG process-mitigation policies) is noted but isn't itself sandboxing in the token/ACL
sense and isn't pursued further here.

### 2.3 Others (Deno, Firefox, Sandboxie-Plus, Windows Sandbox/`wsb`, MSIX/AppContainer launchers, crates.io)

**Not researched to a primary-source standard in this pass** — tool budget was spent going deep
on Codex (the most directly analogous prior art: a Rust CLI agent sandboxing shell commands for
an LLM, exactly Ferrule's situation) and corroborating with Chromium and Microsoft's own docs. I
did not fetch Deno's, Firefox's, or Sandboxie-Plus's actual source or design docs, and did not
survey crates.io for existing restricted-token/AppContainer crates that could shorten Ferrule's
implementation. Windows Sandbox/`.wsb` is architecturally a full VM-based container (Hyper-V), not
a same-process technique comparable to what Ferrule needs — noted from general knowledge, **not
verified against Microsoft's own Windows Sandbox architecture doc in this pass**. **Everything in
this subsection is an open research gap, not a finding.**

---

## 3. Practical traps

- **Does Git Bash (MSYS2) or PowerShell start inside a restricted token / AppContainer?
  Unverified.** No primary Microsoft doc or Codex code comment directly confirms this. Ferrule's
  own `shell.rs` resolves Git Bash first (real Git for Windows `bash.exe`, explicitly never
  `System32\bash.exe`, WSL's launcher, which "runs in another filesystem"), then `pwsh.exe`, then
  `powershell.exe` — a Windows backend must be tested against all three outcomes, since which one
  a user has installed isn't controlled by Ferrule. MSYS2/Git Bash spawns via its own POSIX-
  emulation layer (fork emulation, pty handling), exactly the kind of thing that can break under
  an unfamiliar token or job object (console handle inheritance, job breakaway needed for MSYS2's
  helper processes) — flagging this as the single highest-priority item to test on a real Windows
  box before shipping.
- **Is the workspace readable/writable without ACL changes, and what's left behind on crash?**
  Not automatically — a capability/restricting SID has no access to anything until Ferrule
  explicitly grants it (§1.6, §4). Codex's `cap.rs` generates a **persisted** capability SID per
  Codex home directory (`cap_sid_file(codex_home) = codex_home.join("cap_sid")`, via
  `make_random_cap_sid_string()`), not a fresh SID per run — granted ACEs on disk are long-lived
  and identity-stable across runs, so a crash mid-run leaves grant/deny ACEs attached to real
  files with no automatic cleanup. Codex's `acl.rs` ships an explicit `revoke_ace` counterpart
  precisely because there's no other cleanup path. Ferrule needs either (a) a persisted, revocable
  SID plus an explicit "revoke leftover ACEs" step (in `ferrule doctor` — see §5), or (b) a fresh
  SID per invocation, avoiding stale ACEs at the cost of paying the ACL-grant cost every run.
- **Are %TEMP% and named pipes usable? Unverified.** Not checked against a primary source in this
  pass. `Sandbox::writable_roots()` in `lib.rs` already includes tmp-directory handling
  (Unix-specific: `/tmp`, `$TMPDIR`, `/dev/shm`); a Windows implementation needs its own
  `%TEMP%`/`%TMP%` resolution and — per the same ACL mechanism as the workspace — an explicit
  grant, since a restricted/AppContainer token doesn't automatically get access to the real user's
  `%TEMP%` the way an AppContainer gets its *own* per-app temp folder via
  `CreateAppContainerProfile` (§1.3). Named pipes were not investigated.
- **Do Node/Python/cargo/git work under it? Unverified**, beyond the general expectation (shared
  with Codex's own design, since Codex is itself a dev-tool CLI sandboxing arbitrary build/test
  commands) that this is solved in practice for the restricted-token approach — no code comment or
  doc found stating a specific tool is known-incompatible. Needs empirical testing on
  `windows-latest` CI, not just doc research (§5).
- **Does network-off break localhost? Not fully verified for Ferrule's exact mechanism.** The one
  concrete data point is Codex's MXC README (§2.1): loopback stays reachable while non-loopback
  egress is denied. That's for the closed MXC/AppContainer runner, not for a bare restricted
  token, which (§1.1/§1.5) has **no network policy of its own at all** — network blocking on the
  restricted-token tier would have to come from WFP (needs elevated setup, §1.5) or from omitting
  AppContainer network capabilities (needs a real AppContainer). **Practical implication: on the
  no-admin tier, "network off" may not be enforceable at all without WFP** — state this plainly
  to the user rather than silently claiming it.

---

## 4. Hidden paths inside a writable workspace

**Yes, this is solvable with a deny ACE, and it's exactly how Codex's real shipped code does it**
(`codex-rs/windows-sandbox-rs/src/acl.rs`, functions `add_deny_read_ace`/`add_deny_write_ace`,
both delegating to `add_deny_ace(path, psid, kind: DenyAceKind::{Read,Write})`). Mechanism, quoted
from the in-code doc comment on `add_deny_read_ace`:

> "Adds a deny ACE to prevent reads for the given SID on the target path. `SetEntriesInAclW`
> places newly-created deny ACEs before allow ACEs, which keeps the resulting DACL in the order
> Windows expects for denies to win. The ACE is inheritable so a deny applied to a materialized
> directory also covers files and directories later created underneath it."

Implementation shape: open the path with `WRITE_DAC | READ_CONTROL` (plus
`FILE_FLAG_BACKUP_SEMANTICS` for directories), refuse to apply a read-deny to a filesystem root
(`ensure_handle_is_not_filesystem_root` — a real safety check worth copying), read the existing
DACL via `GetSecurityInfo`, build an `EXPLICIT_ACCESS_W` with `grfAccessMode: DENY_ACCESS`,
`grfInheritance: CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE`, and `Trustee: TRUSTEE_IS_SID(psid)`,
merge it in with `SetEntriesInAclW`, commit with `SetSecurityInfo`. `DenyAceKind::mask()` uses
`FILE_GENERIC_READ | GENERIC_READ_MASK` for reads and a broader write+delete mask
(`FILE_GENERIC_WRITE | FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_WRITE_EA |
FILE_WRITE_ATTRIBUTES | GENERIC_WRITE_MASK | DELETE | FILE_DELETE_CHILD`) for writes.

This is structurally identical to what Ferrule's own Seatbelt backend already does for the same
requirement on macOS — `seatbelt.rs`'s hidden-paths block is appended **last, after every other
grant including network**, with the doc comment "Last, so they override both the blanket read
grant and any writable root the hidden path sits in." The Windows version of the same rule: apply
hidden-path deny ACEs after granting the writable-root ACE, relying on `SetEntriesInAclW`'s
"denies before allows" ordering rather than grant-order — same intent (hidden always wins),
different mechanical guarantee (SBPL evaluates rules bottom-up-last-match; Windows DACLs evaluate
deny-before-allow within one ACL regardless of the order the two grants were requested in).

**Cost**: this requires `WRITE_DAC`/`WRITE_OWNER`-class access to every hidden path at setup time
(to edit its ACL) — a real permission requirement on top of the base restricted-token design.
Ferrule's own (unsandboxed) process needs standard-user write access to those paths' security
descriptors, which it already has if it owns them (matching the existing `hidden: Vec<PathBuf>`
policy field's description in `lib.rs`: "the host's secrets file and the proxy's CA key"). Each
hidden path also pays a real ACL-edit syscall per sandboxed run (or per workspace, if
cached/reused — see the crash/leftover discussion in §3) — non-free, but the same order of cost
Codex already pays in production.

---

## 5. Recommendation for Ferrule

### 5.1 What to build first

**Tier 1 (build first, no admin): restricted token + capability/restricting SID + Job Object,
modeled directly on Codex's "Unelevated" backend.**

1. On `Sandbox::new`, when `cfg!(windows)`: call `CreateRestrictedToken` on a duplicate of the
   current process token (`OpenProcessToken` + `DuplicateTokenEx` to get a primary token, matching
   the "restrict your own token" path the Win32 doc explicitly says needs no
   `SE_ASSIGNPRIMARYTOKEN_NAME`), adding one or more restricting SIDs generated/persisted the way
   Codex's `cap.rs` does (a random per-install SID, or per-workspace SIDs if Ferrule wants the same
   workspace isolation Codex's `workspace_by_cwd` map provides).
2. Grant that SID read+write access on `writable_roots(workspace)` (already computed by existing,
   cross-platform code in `lib.rs`) via `SetEntriesInAclW`/`SetSecurityInfo` (an ALLOW ACE this
   time, same API family as §4's deny path).
3. Apply hidden-path **deny** ACEs for that SID on `hidden_paths()` (§4), after the grant step.
4. Create a `Job Object` with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` set and **no**
   `JOB_OBJECT_LIMIT_BREAKAWAY_OK`/`SILENT_BREAKAWAY_OK` (§1.4's default-confinement trap), assign
   the sandboxed process to it at spawn time. Use the job handle to replace the current
   `kill_process_group`'s Unix-only body (`lib.rs` already has the `#[cfg(not(unix))]` no-op stub
   to fill in).
5. In `Sandbox::command()`, add a `#[cfg(windows)]` branch parallel to the existing Linux
   (`linux::apply`) and Seatbelt branches: build the base `Command` normally (so `Shell::get()`'s
   Git-Bash/pwsh/powershell resolution in `shell.rs` is untouched), then wrap spawn with
   `CreateProcessAsUser`-equivalent logic using the restricted token and job handle. This likely
   means dropping down from `std::process::Command` to raw `CreateProcessAsUserW` (via the
   `windows` crate, the same dependency Codex uses — `windows = { version = "0.58", features =
   [...] }`, `codex-rs/windows-sandbox-rs/Cargo.toml`) since `std::process::Command` has no hook
   for a caller-supplied token or job pre-assignment; this is the single biggest structural
   difference from the Linux/macOS backends, which stay on `std::process::Command` the whole way.
6. `Policy.network`: on Tier 1, **be honest that it's not enforced** (§3) — surface this via
   `model_note()`/`degraded`, the same pattern the Linux backend uses when `network: false` can't
   be guaranteed. Do not claim network isolation on Tier 1.

**Tier 2 (opt-in, needs one-time elevated setup): WFP network filters, optionally a dedicated
low-privilege account.** Only build this once Tier 1 ships and if network-off turns out to matter
enough to users to justify a setup step — mirrors Codex's own two-tier split and its explicit
"only from the already-elevated setup helper" constraint on WFP (§1.5).

### 5.2 What `ferrule doctor`/the wizard should say

Following `doctor.rs`'s existing `Report`/`Level::{Ok,Note,Warn,Fail}` pattern:

- `Level::Ok` — "Windows sandbox: restricted-token backend active, writes confined to
  &lt;workspace&gt; and configured roots." (Tier 1, working.)
- `Level::Warn` with a `hint()` — "Windows sandbox: network isolation is not enforced on this
  backend (`network: false` is honored for the model prompt, not the OS)." — Tier 1 genuinely
  cannot block network without WFP/elevated setup; don't let the doctor claim more than is true.
- `Level::Fail`/hint if ACL grant on the workspace fails (e.g. a network drive or a filesystem
  that doesn't support Windows ACLs, like some FAT-formatted removable/network paths) — hint
  should say to move the workspace to an NTFS volume, since `SetEntriesInAclW`/`SetSecurityInfo`
  need a security-descriptor-capable filesystem.
- On first run, if a stale capability SID's ACEs are detected on paths outside the current
  workspace (the crash-leftover problem, §3), a `Level::Note` "Cleaning up sandbox permissions
  from a previous run" with an automatic `revoke_ace`-style pass — this should run inside
  `ferrule doctor` explicitly, not only best-effort at exit, exactly because Codex found this
  problem real enough to ship a dedicated `revoke_ace` function.

### 5.3 CI on `windows-latest`

CI already runs `cargo test --workspace --locked --no-fail-fast` on `windows-latest`
(`.github/workflows/ci.yml`) and builds `x86_64-pc-windows-msvc` in `.github/workflows/release.yml`
— both directly reusable. Recommended additions, mirroring the Seatbelt precedent (pure
policy-building logic tested everywhere, real enforcement tested only where the OS is real):

1. Keep ACL-plan/token-flag construction logic (which SIDs, which masks, which paths) as plain
   Rust functions with unit tests that run on **every** CI OS, same as Seatbelt's SBPL-text tests
   currently do (`lib.rs`'s existing `#[cfg(test)]` module is the pattern to extend).
2. Add a `#[cfg(windows)]`-gated integration test, run only on the `windows-latest` job, that
   actually spawns a sandboxed command and asserts: (a) a write inside `writable_roots` succeeds,
   (b) a write outside fails with access-denied, (c) a read/write against a `hidden_paths` entry
   inside the workspace fails, (d) a grandchild process (e.g. `cmd /c cmd /c ...`) is still killed
   by the job handle. This is the direct Windows analogue of the "Sandbox self-test" step CI
   already runs on Linux/macOS only (`ci.yml`) — extend that step's OS gate to include Windows once
   the backend exists, rather than inventing a separate mechanism.
3. Explicitly test all three shell-resolution outcomes from `shell.rs` (Git Bash present, only
   `pwsh.exe`, neither) under the sandboxed spawn path, since §3 flags shell startup under
   restriction as the least-verified claim in this whole report — this needs to be empirically
   proven, not just argued from docs.

### 5.4 Honest list of what stays unenforced (Tier 1)

- **Network isolation** (§1.5, §3) — no policy at all without WFP; WFP needs elevated one-time
  setup tied to an account SID.
- **Process memory/environment confidentiality beyond what token+ACL provide** — a low-IL,
  restricted-token sandboxed child is not proven equivalent to Unix's Landlock-enforced
  `/proc/<pid>/environ` denial; the integrity-level claim in §1.2 is itself flagged unverified
  against a primary Microsoft source in this report.
- **LPAC-specific hardening** (§1.3) — not researched; general AppContainer isolation doc was
  fetched, the LPAC-specific capability doc was not.
- **Whether interactive dev tooling (Git Bash/MSYS2, Node, Python, cargo, named pipes) actually
  works under the restricted token** (§3) — no primary source found either way; this is a "test on
  real Windows CI before shipping" item, not a documented guarantee.
- **Crash cleanup of granted/denied ACEs** — Codex's own `revoke_ace` existing as a dedicated
  function is itself evidence this isn't automatic; Ferrule needs the same explicit cleanup path,
  ideally surfaced through `ferrule doctor` (§5.2).
- **Deno, Firefox, Sandboxie-Plus, Windows Sandbox/`wsb`, MSIX launchers, and any relevant
  crates.io crate** (§2.3) — not researched to a primary-source standard this pass; a follow-up
  pass could plausibly shorten the implementation if a maintained `windows`-crate-based
  restricted-token/ACL helper crate already exists on crates.io.
