# M54 — protected autonomous mode

Status: **built 2026-10-21, merged into local `main` (not pushed).** Design approved by the owner
(2026-10-20). All ten tasks of the build plan below are done. No test was added (the owner's rule).
After merging main's M51–M53 (PR #48) and autopilot (PR #47), `make check` passes on macOS:
1881 passed, 0 failed, 28 ignored. The Linux-only owner-mode code (`ferrule-sandbox` `linux.rs`,
`supervisor.rs`, the `cfg(linux)` apply call) has **not been compiled yet**: a Linux `cargo check`
or a CI run is needed before it ships, and no live run has happened. The design below has been corrected where the build changed it; every correction
is listed in "Corrections made while building". The code locations in the design are from `main`
at `dadf357`. The user guide is the "Owner mode" section of [sandbox.md](sandbox.md).

## Why

The owner runs a personal ferrule bot on Telegram on an Ubuntu server
(a systemd user unit as `ubuntu`, data in `~/.local/share/ferrule`). They
want it to run and improve itself, the way NanoClaw does, and never open
a terminal themselves. Today they have two options, and neither works:

- **The default** (`mode = "workspace-write"`, approvals on). Every change
  waits for a tap. The bot can't install a system package. It can't add
  an MCP server from its shell (`ferrule mcp add` reads
  `proxy/keys/seed`, which is hidden). It can't read its own transcripts.
- **The stopgap** (`[sandbox] mode = "off"` and `[trust] gates = false`).
  Nothing asks, but the bot's commands can read `private/secrets.env`.

M54 adds a third, opt-in mode. The bot can do everything the owner could
do in a terminal, with no approvals. The keys stay out of its reach.

## Goals

With `[sandbox] mode = "owner"`, the bot does all of this from a Telegram
chat, with no approval prompts:

1. Install tools: system packages (apt), and anything user-level
   (pipx, uv, npm, curl installers).
2. Change its own config.
3. Add and remove MCP servers, including ones that need a key.
4. Ask for a new secret by name. The owner pastes the value on a one-time
   dashboard page; it never goes through the chat.
5. Send the owner a dashboard login link.
6. Read its own session transcripts and learning notes.
7. Update itself and restart.

Throughout, `cat ~/.local/share/ferrule/private/secrets.env` from its
shell still fails, and so does every other way of reading the keys that
§5 lists.

## Non-goals and anti-goals

- **No general root.** Root reads every file, keys included. The bot
  gets exactly two root actions: installing and removing apt packages
  (§4.1). Editing `/etc` and managing system services stay the owner's.
- **No changing its own key protection.** The sandbox mode, where a key
  may be sent, provider URLs, who may talk to the bot, and hooks'
  commands stay owner-only, from the dashboard. These are today's guarded
  config keys (`config_page.rs` `WHOLE` and `guarded_key`). Changing
  any of them would let the bot reach or redirect a key.
- **Not safety from the bot.** It can delete its workspace, fill the
  disk, or kill its own gateway. Owner mode protects the keys and nothing
  else.
- **Anti-goal: a speed bump sold as protection.** Hiding `private/` is
  not enough on its own. A bot that can write the config or the ferrule
  binary turns the sandbox off and restarts itself in two commands.
  Owner mode only counts if it closes those routes too.
- **No default-deny egress.** Commands hold only placeholders, and the
  proxy swaps in a real key only on requests to that key's own hosts.
  So open egress can't leak a key; the highest-risk path is a key moved
  to a new host, and §2 and §5 close that. A deny-by-default egress
  policy with one-tap "allow host" would bring back the approvals the
  owner asked to be rid of. M33's `[egress]` still works for an owner
  who wants it.
- macOS and Windows. Owner mode is Linux-only. *Correction made while
  building:* Seatbelt matches paths, not inodes. With `/` writable,
  renaming a parent folder would undo a deny: `mv ~/.config ~/.c2`
  makes the config writable, and the same trick on a parent of
  `private/` makes the keys readable. Hard links are untested there
  too. On Linux, Landlock rules bind to inodes, and renaming a carved
  parent needs a right it doesn't grant. macOS waits for a probe on a
  Mac; until then, both macOS and Windows refuse the mode.

## The boundary, in one sentence

Commands can read and write everything the account can, **except** the
keys (unreadable) and anything that decides what runs outside the sandbox
(read-only). Every change to ferrule itself goes through `ferrule_admin`,
which runs in the gateway and won't touch the guarded keys.

## Design

### 1. `[sandbox] mode = "owner"` (ferrule-sandbox)

A fourth `Mode`. It is built on the existing hide-only sandbox
(`Sandbox::unconfined`, `lib.rs` ~420), plus three things:

| | Read | Write |
|---|---|---|
| **Hidden**: `private/`, `proxy/keys`, every other credential file the backup leaves out (§2b), the gateway's state dir `<data>/gateway`, the default read denies (`~/.ssh`, cloud credentials, browser profiles), claude's login dir when the claude engine runs (as today) | no | no |
| **Protected** (§2) | yes | no |
| Everything else the account can reach | yes | yes |

- **Protected paths are new.** `Policy` gets `protected: Vec<PathBuf>`,
  filled in by the host like `hidden` is.
  - On Landlock, the write carve skips `hidden ∪ protected`; the read
    carve skips `hidden` only (`linux.rs` `ruleset`, `carve`).
  - No Seatbelt path: owner mode is Linux-only (see Non-goals).
  - **Protection walls every root that contains a protected path; a
    granted root inside one stays writable.** *Corrected while
    building* (`ferrule-sandbox/src/lib.rs` `granted_roots`,
    `writable_roots`; `linux.rs` `ruleset`): owner mode still grants the
    workspace, `writable_roots`, the helper roots and temp, on top of
    `/`. The carve walls each of them around the protected paths it
    contains, so a workspace that contains the data dir (or is `~`)
    doesn't reopen it. A granted root that lies inside a protected path
    isn't walled, because it is a deliberate grant:
    - each MCP server's own state dir;
    - `<data>/worktrees/<id>`, where a sub-agent works;
    - the workspace, or a `writable_roots` entry, if the owner put one
      inside a protected path. `writable_roots` is a guarded key only the
      owner sets, so listing a protected dir there reopens it.

    The gateway refuses owner mode when its workspace is inside a
    protected path (`owner_mode.rs` `refusal`), so the workspace case
    never runs. The MCP and worktree grants hold nothing the gateway
    runs outside the sandbox (§4).
  - Helpers have no exemptions in owner mode. An MCP server with
    `sandbox = false` and the browser helper run under the owner sandbox,
    with the socket allowlist; on Linux today, hide-only and `desktop`
    lift the allowlist. If Chrome needs a socket, it is added to the
    allowlist by name.
- **The carve limit and `mkdir`.** Landlock can only add rights. So a
  directory on the way down to a hidden or protected path (`~`,
  `~/.config`, `~/.local`, `~/.local/share`) can't get new entries; only
  the entries already in it are writable. That would break
  "`mkdir ~/proj`" and first-run installers. In owner mode those levels
  also get `MAKE_DIR`:
  - `mkdir` works there, and the new folder is fully writable from the
    next command on (each command's ruleset is built fresh).
  - An installer that creates its folder and fills it in one go fails
    once, then works on a rerun.
  - The sandbox note tells the model this.
  - What `MAKE_DIR` reaches inside hidden or protected trees is an empty
    directory, which can't be filled.
- **The Unix-socket allowlist stays on.** Hide-only lifts it today
  (`restricts_unix`). Owner mode keeps it, so neither the user bus
  (`systemd-run --user`, `systemctl --user`) nor a tmux server outside
  the sandbox is reachable.
- **Unchanged:** env scrubbing, credential placeholders, `no_new_privs`,
  the network setting.
- **`require` is always on.** A degraded owner-mode sandbox would mean
  readable keys. If it can't be applied, the gateway doesn't run
  commands unsandboxed. It also doesn't exit, which would leave a
  no-terminal owner with a dead bot. Instead it falls back to
  `workspace-write` (with `require` on) and tells the owner why in
  their chat. *As built:* if that can't be applied either, it starts
  with commands refused (`Sandbox::refuse`), so chat still works
  (`owner_mode.rs` `after_failure`, `owner_line`). What the process's
  own helpers run in meanwhile is in "Corrections made while building".
- **Ferrule's own environment.** The gateway's environment may hold
  keys. Sandboxed commands can't read `/proc/<gateway>/environ` (the
  Landlock ptrace scope). With `require` on and §4 in place, nothing the
  model can influence runs outside a sandbox. So owner mode doesn't move
  the keys out of the process environment; that stays a separate
  hardening item. (Done since: amendment 5, M54.2.)
- **Read-only cases map to `ReadOnly`**, as `WorkspaceWrite` does: a
  read-only sub-agent (`for_child`) and plan mode (`for_planning`).
- **Other people's chats keep the stricter sandbox.** A session gets
  owner mode if it is the owner's private chat or the dashboard chat, or
  descends from one: a sub-agent of it, or a scheduled task created
  there. Every other session (a group, another allowed person, the HTTP
  API, a task created anywhere else) gets `workspace-write`.
  `self_service::offered` can't be the test, because it also says no to
  tasks and sub-agents. Today the sandbox is one per process
  (`shared_sandbox`, `engine_sandbox`), so this needs a narrowed second
  one, chosen per session; the claude engine needs it chosen per launch.

### 2. The protected set

Each item below is something that runs outside the sandbox, or tells
something outside the sandbox what to run.

| Path | Why |
|---|---|
| the data dir (`~/.local/share/ferrule`), all of it | config backups, the update state, the lock store, plugins, the trust records (`trust/audit.jsonl`, the `trust/stop` kill switch; hooks trust is already in `private/`), and now the binary (§3). So the bot can't erase its own audit trail. MCP helpers' own state dirs stay writable: they are explicit grants, and Landlock rules add up. |
| the config file's directory (`~/.config/ferrule`, or `$FERRULE_CONFIG`'s) | one edit to `[sandbox]` plus a restart undoes everything |
| `~/.config/systemd`, `~/.local/share/systemd` | a user unit or drop-in runs outside the sandbox at the next start |
| `~/.config/environment.d` | sets the environment of every user service, `LD_PRELOAD` included |

The in-process file tools (`fs_tools.rs`) already refuse hidden paths.
They also refuse writes into protected ones, which matters when the
workspace contains the data dir.

Shell startup files are deliberately left off the list. They source
other files, and an interactive login puts `~/.local/bin` first on
PATH, so protecting a few named files wouldn't close the route. The
honest rule goes in the docs instead: a shell logged in as this account
runs whatever the bot left in it (§ what's left).

### 2b. Every credential file ferrule holds is hidden

`hidden_paths()` hides `private/` and `proxy/keys`. Other credentials
also sit in the data dir: for example, the Matrix `access_token` in
`<data>/gateway/matrix/session.json`. Today that file is readable in
every mode.

The backup already keeps one list of the data dir's secret paths:
`backup::SECRETS` (`private`, `claude-code`, `ssh`, `proxy/keys`,
`gateway/matrix/session.json`). From now on, `hidden_paths()` hides
every entry on that list, in every mode, except `ssh` (see "Corrections
made while building"). It hides the gateway's state dir too
(`main.rs` `hidden_paths`). So a credential added to the
backup's list is hidden from commands too, and the hard rule (secrets
never reach the model) covers all of them. Connection tokens are
already under `private/connections`.

### 3. The binary moves into the data dir

`install.sh` puts ferrule in `~/.local/bin`. Protecting it there would
carve `~/.local/bin`, and then pipx, uv tools and claude's own updater
could no longer add a file to it. Instead, on the first start in owner
mode:

1. The gateway puts its binary at `<data>/bin/ferrule` as a hard link
   to the running file, so it costs no disk. It copies (mode 0755) when
   a hard link isn't possible.
2. It rewrites its service units to the new path, using the code
   `setup --refresh-service` already uses, and reloads them without a
   restart.
3. It swaps the old path for a symlink in one step: the link is made
   beside it and renamed over it. Writing through the symlink is
   refused, because the target is protected. The original is never
   renamed away.
4. It exits cleanly, and the service starts the binary in `<data>/bin`.

*As built:* `owner_mode.rs` `relocate` and `move_in`. "Corrections made
while building" says why it isn't a rename.

After that, `current_exe()` is protected. Updates (`real_apply`) and
re-exec restarts go there too.

If there is no service to rewrite, or the move fails, the gateway starts
in `workspace-write` instead and tells the owner why in their chat (the
truthful-status rule). It never runs owner mode with a writable binary.

### 4. Nothing the model can influence runs outside the sandbox

The gateway itself is unsandboxed. In owner mode, `~/.local/bin`, the
workspace, `~/.gitconfig` and every user-level install are writable by
commands. Any program the gateway runs from those, or any script it
runs from the workspace, is code the bot wrote.

**The rule:** in owner mode, the gateway runs only two kinds of thing
outside the sandbox.

- Root-owned programs, called by absolute path:
  - `/usr/bin/sudo` and `/usr/bin/apt-get`, for §6.1;
  - `git`, `ssh`, `systemctl`, `loginctl` and `ps`, found by
    `ferrule_sandbox::host::system_program` (`host.rs`): in owner mode
    the first entry of `/usr/local/sbin`, `/usr/local/bin`,
    `/usr/sbin`, `/usr/bin`, `/sbin`, `/bin` that is owned by root and
    not writable by group or other, never a `PATH` lookup. The bot can
    write `~/.local/bin`, which is often first on `PATH`. With no match
    it is `/usr/bin/<name>`, so the spawn fails "not found" instead of
    searching `PATH`.
- Its own binary, which is protected (§3).

Everything else it starts goes through the owner sandbox
(`Sandbox::command`, the way autocommit's git already does,
`autocommit.rs` ~53). The known spawns:

| Spawn | Today | In owner mode |
|---|---|---|
| command hooks (`ferrule-hooks/src/command.rs`) | the owner's `[hooks]`: unsandboxed by design, "runs as the owner" (a workspace's hooks run in the session's sandbox in every mode, since "Workspace hooks run in the sandbox") | sandboxed |
| scheduler gate scripts (`ferrule-gateway/src/scheduler/gate.rs`) | unsandboxed | sandboxed |
| sub-agent worktree git (`ferrule-agents/src/worktree.rs`) | unsandboxed; `core.hooksPath` points into temp, which commands can write | sandboxed, with `<data>/worktrees` granted. Sandboxing it also covers commands that `.git/config` or `~/.gitconfig` define (filters, the pager, ssh). It still goes through `ferrule_sandbox::host_git` (`git_with`, the launcher starting each git), which also pins the repo's own filter drivers off and its work tree in place. |
| claude's `--version` and claude's updater (`update/claude.rs`, `ferrule-plans/src/claude/update.rs`) | unsandboxed | the engine sandbox (`engine_sandbox`), which leaves claude's own login readable. An updater that needs another user (`as_owner`, a root-installed npm) can't switch users under `no_new_privs`; it fails and says so. |
| the connections' key-form `node` and cloudflared's version check (`ferrule-connections/src/keyform.rs`, `cloudflared.rs`) | listed here in error | both spawns are inside `#[cfg(test)]` code; nothing to route |
| `agent-browser --version` (`ferrule-mcp/src/browser.rs`) | unsandboxed | sandboxed |
| the transcription command (`ferrule-gateway/src/transcribe.rs`) | unsandboxed | sandboxed |
| cloudflared (the dashboard tunnel, `ferrule-connections/src/tunnel.rs`) | unsandboxed | sandboxed (it needs only the network) |
| `git` for extension installs (`ferrule-extensions/src/git.rs`) | `git` from `PATH` | `system_program("git")`, root-owned, unsandboxed |
| the SSH workspace link and its sibling tools (`ferrule-ssh/src/link.rs` `ssh_program`, `trust.rs` `sibling`) | by default `ssh`, `ssh-keygen`… from `PATH`; a target's own `ssh` path and `FERRULE_SSH` come first, and a sibling prefers the directory of that `ssh` | by default `system_program(name)`; the explicit settings are the owner's and stay as they are |
| `git --version` in `ferrule doctor` (`doctor.rs`) | `git` from `PATH` | `system_program("git")` |
| `systemctl`, `loginctl` and `ps` (`service.rs` `manager`, the `ps` call) | from `PATH` | `system_program(name)`, else `/usr/bin` or `/bin` (so a host that keeps them elsewhere, NixOS, still finds them) |
| signal-cli (`ferrule-gateway/src/channels/signal/daemon.rs`, `ferrule-cli/src/channels/signal.rs`) | unsandboxed | sandboxed (`host::command`); its data dir is writable in owner mode |
| `claude auth status`, `login`, `logout` and `claude --version` (`subscription/claude.rs`, `ferrule-plans/src/claude/cli.rs`) | unsandboxed | the engine sandbox with claude's config dir granted, the one claude's updater gets |

The builder greps for every `Command::new` in the workspace's crates,
adds any spawn missing from this table, and records its row here. The
rows from `git` for extensions to `claude auth` were added while
building.

Two consequences:

- `hooks_trust` without a card is harmless in owner mode: a trusted
  hook runs sandboxed like any command.
- An owner hook or gate that needs the keys or a protected path stops
  working in owner mode. The user docs say so.

**Other modes.** The worktree `hooksPath`-in-temp hole is real in
`workspace-write` too. There, worktree git moves into the sandbox as
well, so the fix ships for every mode. The owner's `[hooks]` and gates
keep their current unsandboxed behaviour outside owner mode; a
workspace's hooks run in the session's sandbox in every mode.

### 5. No approvals

- **`ferrule_admin` change ops run at once.** `self_service/tool.rs`
  `ask_and_run` skips the `hub.ask_in` card. It still runs the op's
  checks:
  - `resolve`
  - the managed locks
  - `update::card` and `restart::card` as preconditions

  Every run is audited: `self_service.ran` with
  `by = "<channel> chat <id> (owner mode)"`.
- **Trust gates are off**, the same as `[trust] gates = false`, whatever
  the config says, except one (amendment 4): a send, a force push or a
  `DELETE` to a bound host after the run read outside content.
- **What stays owner-only:**
  - `config_set` keeps refusing guarded fields (unchanged).
  - `config_restore` from the chat now also refuses a restore that
    would change a guarded field. Without that, restoring the last-good
    config could bring back `mode = "off"`. It reuses `guarded_change`,
    and applies in every mode.
  - The M13/M32 extension approval queues are unchanged (behind
    `[extensions] enabled`, off by default). The bot adds MCP servers
    through `mcp_add` (§6) instead.

### 6. New `ferrule_admin` ops

These ops exist in every mode. In the default mode, the change ops ask
first, like every other change op; owner mode only removes the asking.

| Op | Kind | What it does |
|---|---|---|
| `package_install {packages}` / `package_remove {packages}` | change | see §6.1 |
| `mcp_add {name, command \| url, env, headers, enabled_tools, replace}` | change | `mcp_add::add_to` in the gateway, see §6.2 |
| `secret_request {name, hosts, why}` | read (no card: the owner's paste is the approval) | see §6.3 |
| `secrets` | read | names and hosts from `GET secrets` (`tool_keys::list`), never values |
| `dashboard_link` | read | the `/dashboard` code (`dashboard/door.rs`): the gateway sends the link straight to the owner's private chat (`Hub::tell_in`). The tool result is only "sent, valid N min". |

`mcp_remove` already exists.

**6.1 Packages.** On Linux with apt only (otherwise a plain "not here").

- Names must match `^[a-z0-9][a-z0-9+.-]*$`. Anything else is refused,
  including paths, `.deb` files and options.
- The gateway runs `/usr/bin/sudo -n /usr/bin/apt-get install -y
  --no-install-recommends -- <names>`, with `DEBIAN_FRONTEND=noninteractive`
  and a 10-minute limit.
  - If apt says "Unable to locate package", it runs `apt-get update`
    once and retries.
  - Then it runs `apt-get clean` (the server has under 500 MB free).
  - The result is apt's last lines, clipped.
- If `sudo -n` needs a password, the answer says so plainly, and says
  that user-level installs from the shell still work.

**6.2 MCP servers.** `mcp_add::add_to` with `yes`, `no_doctor`:

- It starts the server, lists and scans its tools, then writes the
  config. `config_follow` hot-loads it, with no restart.
- It runs in the gateway, so it can read `proxy/keys`.
- Restrictions:
  - `${NAME}` in a header, and `secrets`, may only name existing
    `[secrets]` entries. The bot never binds a key to a new host.
  - No `no_sandbox`.
  - Scan-blocked tools stop the add. Waiving the scan is the owner's
    call.
- A stdio server needs no key arguments: it already gets every
  `[secrets]` name as a placeholder, like commands do.
- *As built* (`self_service/run.rs` `mcp_keys` and `mcp_add`): an `env`
  entry whose name looks like a key (`ferrule_sandbox::looks_secret`) is
  refused too: "keys are added by the owner: use secret_request". The
  key names a header references (`${NAME}`) are passed to the add as
  `secrets` (names only, no hosts), so the probe's own broker loads the
  saved value. Without that, a URL server whose key was saved after the
  gateway started would fail its probe.

**6.3 Asking for a key.** The flow for "connect an MCP server that needs
a key":

1. The bot calls `secret_request {name: "GITHUB_TOKEN", hosts:
   ["api.github.com"], why}`. The checks are the ones `secrets/add`
   makes (`tool_keys.rs`): the name's shape, the name is unused, and
   the hosts are valid.
2. The gateway mints a login link (`Links::mint`) that opens the Keys
   panel with the name and hosts filled in. The owner sees the hosts
   and can edit them. The link goes to the owner's private chat. The
   tool result says only that it was sent. *As built*
   (`self_service/keys.rs`): the bot's `why` is shown to the owner right
   above the link, so it is cut down to letters, digits, `_`, spaces and
   `, ' " ( ) - ! ? ;` (everything else becomes a space), redacted, then
   clipped to 300 characters. No `.`, `:`, `/`, `@` or `#` survives, so
   a reason can't carry a link of its own. An owner whose chat is the
   HTTP API gets no link: it goes only to a person's chat.

   **Coordination with M52 (the dashboard Safe).** M52 plans a one-time
   paste link. If it lands first, `secret_request` sends that link
   instead of a login link; steps 3–4 don't change. If M54 lands
   first, M52 swaps its link in behind the same op. Either way there
   is one paste flow, not two.
3. The owner pastes the key. `secrets/add` saves it and
   `config_follow` binds it into the running proxy.
4. The gateway remembers which session asked for which name (in memory).
   When the save lands, it wakes that session (`Router::wake`): "the
   owner saved GITHUB_TOKEN; it is sent only to api.github.com". The bot
   then calls `mcp_add` on its own.

### 7. Transcripts and learning notes

In owner mode, `sessions/` and `learn/` leave `hidden_paths()`
(`main.rs` `hidden_paths(owner)`). They stay read-only, since the data
dir is protected. *As built:* they stay hidden from every session that
isn't the owner's. `Sandbox::narrowed(hide)` adds them to a guest's
hidden list, the claude engine gets the same list (`EngineConfig`
`guest_hidden`), and `send_file` is built with the matching list.

The builder checks that nothing in them is a live credential. Dashboard
links never enter a transcript: `/dashboard` answers are interceptor
replies, and `tell_in` goes straight to the channel. In owner mode there
are no approval codes to find.

### 8. The owner's switch

Nothing in the chat or the bot's shell can change `[sandbox]`, so the
dashboard needs the way back. A row on Settings:

> **Autonomy**: Protected autonomous · Ask me first

It calls a page-only route, `sandbox/mode {mode, confirm}`. There is no
`ferrule_admin` route to it.

- "Ask me first" writes `mode = "workspace-write"` and removes
  `gates = false`.
- Either choice restarts the gateway.

To turn owner mode on, the owner flips the row. The current stopgap bot
(sandbox off) can also write the setting itself.

### 9. The prompt

Two strings depend on the mode:

- the `ferrule_admin` description (`prompts::ADMIN_DESCRIPTION`). In
  owner mode it says changes run at once and are logged.
- the sandbox note (what's writable, the protected set, the `mkdir`
  rule, "change ferrule only through `ferrule_admin`").

Each is constant for a given mode, so the prefix stays byte-stable. It
changes once, when the mode does, and that takes a restart anyway. The
new ops go into the op enum for every mode: one prefix change on
upgrade.

## What's closed, what's left

**Closed** (the plan lists a check for each; per the owner's rule none
was written or run, and `ferrule doctor` names an owner mode that fell
back):

| Attempt | Stopped by |
|---|---|
| `cat private/secrets.env`, `proxy/keys/*` | hidden (Landlock) |
| edit `config.toml` to turn the sandbox off or move a key to a new host, then restart | config dir protected; `config_set` and `config_restore` refuse guarded fields |
| replace the ferrule binary, then kill the gateway so systemd restarts it | the binary is in the protected data dir (§3) |
| a new user unit, a drop-in, `environment.d` with `LD_PRELOAD` | protected (§2) |
| `systemd-run --user`, `systemctl --user`, `tmux send-keys` to a server outside | the Unix-socket allowlist, for commands and helpers alike (§1) |
| `sudo`, `crontab`, `at`, `pkexec` | `no_new_privs` (setuid and setgid are ignored) |
| a fake `git`, `ssh`, `systemctl`, `claude`, `agent-browser` or `cloudflared` placed on `PATH`; a git hook, filter or fsmonitor; a hook or gate script the bot wrote or trusted | §4: the gateway runs the sandboxable ones in the sandbox, and finds the others in root-owned system dirs |
| a credential file in the data dir outside `private/` (the Matrix session, …) | §2b |
| a dashboard session, to change what the bot can't | links and sessions live in `private/dashboard`; the bot's link goes to the owner's chat, not the tool result |
| `ptrace`, or `/proc/<gateway>/environ` or `mem` | Landlock's ptrace scoping, and Yama |

**Left open** (in the user docs):

- **The apt op runs package scripts as root.** They come only from the
  configured apt sources, and the bot can't add a source. A package that
  runs per-user files outside the sandbox (mail filters, for example)
  could be combined with a writable file in `~`. The audit log shows
  every install.
- **Claude's login** (`~/.claude`) stays readable to claude and its Bash,
  as M35 §7.7 says. Owner mode doesn't change that.
- **Re-opening `~/.ssh`.** If the owner re-opens it with `allow_read`,
  `authorized_keys` becomes writable. A key planted there skips
  everything above, so the docs say not to.
- **Logging in as this account.** A shell logged in as `ubuntu` runs
  the bot's files: `~/.bashrc`, whatever it sources, and `~/.local/bin`
  first on PATH. So does a person who types `ferrule` and gets the
  replaceable `~/.local/bin/ferrule` symlink. Logging in after the bot
  has run is like running its code, keys included. The service itself
  never uses these.
- **Existing limits:** file names inside a hidden directory stay
  listable, and a bind mount of a hidden path isn't covered.
- **Added while building** (the user docs, `sandbox.md` "Owner mode",
  have each in full):
  - Other people's chats share the process-wide helpers (MCP servers,
    hooks, the browser, graph check commands) and the workspace, all of
    which reach what the owner sandbox reaches, never the keys or the
    protected files. A filesystem MCP server or the browser, steered by a
    guest, can read the owner's `sessions/` and `learn/`. A guest's repo
    filter driver can run when an owner session uses the repo.
  - After a fallback, the helpers keep owner-policy reach while the
    bot's own commands are narrowed.
  - The apt op needs passwordless sudo for the gateway's user.
  - Owner mode needs to be the only ferrule instance on the account, a
    systemd user service, and a local workspace (an `ssh:` workspace
    always falls back).
  - Managed containers always fall back.
  - A terminal `ferrule run` or `chat` gets owner mode only once the
    service has moved its binary, and then without gates.
  - `install.sh` no longer upgrades a service that has moved: use
    `ferrule update`.
  - A wake is lost if the gateway restarts, the session's queue is full,
    or the owner saves a different name than the one asked for.
  - macOS needs ancestor unlink-deny anchors and a hard-link probe.

## Done means

On a Linux box set up like the owner's (a user unit, apt, `sudo -n`
works), with `mode = "owner"`, in a Telegram chat with no approval
prompt at any point:

1. "Connect the GitHub MCP server". The bot sends a key link. The owner
   pastes the key. The bot is woken, adds the server, and calls one of
   its tools.
2. "Send me a dashboard link". The link arrives, and it isn't in the
   bot's tool result or transcript.
3. "Install jq". jq runs from the bot's shell.
4. "Set X to Y and restart". The bot comes back with the change.
5. "What did we talk about yesterday?" It answers from `sessions/`.
6. From its shell, `cat ~/.local/share/ferrule/private/secrets.env`
   fails. So do writing `~/.config/ferrule/config.toml` and
   `~/.local/share/ferrule/bin/ferrule`, and `systemctl --user
   daemon-reload`.
7. `make check` is green on Linux, macOS and Windows.

## Decisions (defaults, for the owner to confirm)

1. **Root is limited to apt install and remove.** No general `sudo` for
   the bot (§ non-goals). *Confirmed by the owner, 2026-10-20.*
2. **The bot can't change its own key protection.** Sandbox, key hosts,
   URLs, owners and hooks' commands stay on the dashboard.
3. **The binary moves** to `<data>/bin/ferrule` as a hard link, with a
   symlink left behind and the service rewritten (§3).
4. **Only the owner's chats get owner mode.** Other people's chats keep
   `workspace-write` (§1). *Owner, 2026-10-20: only they talk to it
   today, but they sometimes want an agent others can message. So the
   per-session narrowing is built, not replaced by an only-the-owner
   rule.*
5. **In owner mode, hooks and task gates run in the sandbox** (§4). An
   owner hook that needs the keys stops working there. *The owner has
   none that need the keys, 2026-10-20.*
6. **Logging into the server's shell isn't protected** (§ what's left).
7. **New top-level folders** in `~`, `~/.config` and `~/.local/share`
   take `mkdir` first, and are usable from the next command (§1).
8. **A dashboard switch** is the owner's way in and out (§8).
9. The config value is `"owner"`.

## Left to the builder

- Exact op names and argument names.
- The Keys panel's prefill URL format.
- Where the per-session narrowed sandbox is chosen.
- The wording of the sandbox note and the tool description.
- How `config_restore` tells the chat from the page.
- Test placement: one module per crate in `tests/it/`.

## Corrections made while building

Rulings made during the build that changed the design above. The design
text is fixed where it said otherwise; this is the record of why. Code
paths are under `crates/`.

1. **Linux only.** Seatbelt matches paths. With `/` writable,
   `mv ~/.config ~/.c2` undoes a deny, and hard links are unverified
   there. `Sandbox::new` (`ferrule-sandbox/src/lib.rs`) refuses owner
   mode with `owner mode needs Linux; this is macOS` or `...; this is
   Windows`. The Seatbelt owner-only code was removed. Owner mode on a
   Mac needs ancestor unlink-deny anchors and a hard-link probe on a
   Mac.
2. **`system_program`: no `PATH` lookup for what stays unsandboxed.**
   The bot can write `~/.local/bin`, often first on the gateway's
   `PATH`, so a fake `git` or `systemctl` run by name outside the
   sandbox is a way out. `host::system_program(name)`
   (`ferrule-sandbox/src/host.rs`) returns the first match in the
   system dirs (§4) that is owned by uid 0 and has no group or other
   write bit. With the host launcher set and no match it is
   `/usr/bin/<name>` and the spawn fails "not found". With no launcher
   it is the bare name. Used for `git` in extension installs, `ssh` and
   its siblings, `git --version` in `doctor`, and `systemctl`,
   `loginctl` and `ps` (`service.rs`). signal-cli goes through
   `host::command` instead (sandboxed), and claude's `auth` commands
   take the engine sandbox, as its updater does. The design's rows for
   the key-form `node` and cloudflared's version check were wrong: both
   spawns are in test code.
3. **Every process arms its own spawns, not only the gateway.**
   `owner_mode::arm(&cfg, workspace)` runs from `main.rs` `dispatch`
   before any subcommand (the gateway and `tasks run-now` arm
   themselves with their workspace; the sandbox probes run inside a
   sandbox already). A `ferrule doctor` started by the gateway would
   otherwise run shadowed programs unsandboxed. Outside the gateway
   the sandbox is built without the credential proxy, and nothing is
   started or written for it. A sandbox that can't be built becomes one
   that refuses every listed spawn.
4. **`wanted` is not `on`.** `owner_mode::wanted(cfg)`: the config asks
   for owner mode, held or fallen back. It decides what runs outside the
   sandbox: arming the host launcher, and the checks that a spawn is
   never unconfined (`update/claude.rs`, `browser.rs`,
   `subscription/claude.rs`), since whatever the bot wrote while owner
   mode held outlives it. `owner_mode::on(cfg)`: owner mode is actually
   running. Cards, gates and guest narrowing follow that.
5. **Guests don't see sessions or learn.** Owner mode makes
   `sessions/` and `learn/` readable to the owner's commands, but the
   narrowed sandbox for other chats must not read them.
   `Sandbox::narrowed(hide)` adds them to the guest's hidden list, the
   claude engine keeps the list for `guest()` (`EngineConfig`
   `guest_hidden`), and `send_file` is built with it. Who counts as a
   guest follows `owner_mode::owner_tree`: a gateway session that isn't
   a channel or scheduler session (`/plan`, `/goal`, `/graph`) takes the
   chat that started it, and one the gateway can't trace is a guest.
   Outside the gateway, an unknown tree is the owner's.
6. **`<data>/ssh` is not hidden.** It holds `known_hosts` and, per
   target, the local anchor of an SSH workspace (todos, diary, hook
   trust). Hiding it breaks SSH workspaces in every mode. §2b hides
   every `backup::SECRETS` entry except `ssh`; identity files live
   outside the data dir, and `known_hosts` is read-only through the
   protected data dir.
7. **`<data>/gateway` is hidden in every mode.** Channel state holds
   credentials and unasked outbound messages: WhatsApp's `held.json` can
   hold a `/dashboard` login link. The cost: `ferrule status` from the
   bot's shell may lose detail. The bot uses `ferrule_admin status`.
8. **The binary move is a hard link and an atomic swap, not a rename.**
   A rename leaves the old path missing until the symlink lands, and
   other instances sharing the file lose it. Now `<data>/bin/ferrule`
   is a hard link to the running file (a copy on `EXDEV`), the units are
   rewritten and reloaded, and only then is the old path replaced by a
   symlink in one `rename`. Crash-safe at every step
   (`owner_mode.rs` `relocate`, `move_in`, `place`, `swap_in_link`).
   - `Already` means `current_exe()` is under `<data>/bin` and the
     installed gateway unit and the update unit (if there is one) both
     name it. If a unit names anything else, the units are rewritten
     and the gateway restarts once.
   - If another instance's unit runs the same file, the move copies and
     skips the swap.
   - A protected binary with more than one name gets a fresh copy
     (`detach`), because Landlock judges paths, and a leftover hard link
     at the bot-writable old path would reach the protected inode. If
     the service started this process the gateway restarts onto the new
     file; otherwise it sets the re-exec path.
   - `install.sh` no longer upgrades a service that has moved: it sees a
     unit that names another path, says so and restarts it as it is.
     Updates go through `ferrule update`.
9. **`relocated()` for processes that aren't the gateway.** `ferrule
   chat`, `run`, `tasks run-now` and dashboard-started runs run in owner
   mode only if the binary is already protected: it runs from
   `<data>/bin` and the installed units name it. Otherwise they fall
   back to `workspace-write` with a one-line reason on stderr. So terminal
   runs are narrowed until the service has moved its binary, and once it
   has, they run in owner mode without gates (§5; `owner_tree` is true
   for them).
10. **One instance per account.** Each instance protects only its own
    data dir, so beside another instance each bot could write what the
    other runs. Owner mode refuses when another ferrule instance of the
    same scope exists, and when instance discovery fails (fail closed)
    (`owner_mode.rs` `other_instances`).
11. **The apt op checks apt's own plan.** Names are validated first
    (§6.1), a name ending in `-` is refused (apt reads it as "remove"; a
    trailing `+` is a real name, `g++`). Then a dry run (`-s`): every
    requested name must appear in apt's plan as exactly that package
    (an `Inst`/`Remv` line, or "already the newest version"/"is not
    installed"), or the op refuses with "apt didn't read `<name>` as one
    package"; and a `Remv` line naming `sudo`, `apt`, `openssh-server`,
    `systemd` or `ca-certificates` refuses it. The command runs under
    `LC_ALL=C` so those lines are apt's English ones
    (`self_service/packages.rs`). Cost: an exotic but legitimate name
    that apt words differently is refused.
12. **The `secret_request` reason is an allowlist.** The bot's `why` is
    shown to the owner above the paste link, so a blocklist of link
    shapes was not enough. `keys.rs` `reason` keeps letters, digits,
    `_`, spaces and `, ' " ( ) - ! ? ;`, turns everything else into a
    space, redacts, then clips. The reason loses its periods and any URL
    or file name.
13. **`describe` still runs in owner mode.** `ask_and_run` keeps its
    validation (the card's checks, such as "prompt too long to show")
    and drops only the card text, so there is one validation path whoever
    asks. Cost: one extra `describe` call per change op.
14. **The owner-policy fallback launcher.** After a fallback the process's
    own helpers (hooks, gates, signal-cli, cloudflared, transcription,
    worktree git) don't go back to unsandboxed, and don't get the
    narrowed workspace-write sandbox either, so they keep writing their
    own state. They get a sandbox built from the owner policy,
    with this binary's real path added to the protected set
    (`owner_mode.rs` `fallback_launcher`): keys hidden, ferrule's files
    read-only, writes elsewhere. Where that can't be built (macOS,
    Windows, no Landlock) and the config asks for owner mode, host
    spawns are refused. This replaces the build plan's Task 3 Step 4,
    which set the launcher from the fallback sandbox. The cost: after a
    fallback the helpers keep owner-policy reach while the bot is
    narrowed.
15. **`mcp_add` passes the key names its headers use.** `${NAME}`
    entries in `headers` go to the add as `secrets` (names only), so the
    probe broker loads the saved value
    (`run.rs` `mcp_add`).

Smaller rulings:

- Protection and grants: see §1 (corrected there).
- In every mode, `service.rs` `manager()` now looks for `systemctl` and
  `loginctl` in `/usr/bin` and `/bin` before `PATH` (owner mode adds the
  root-owned check of `system_program`).
- An MCP server with `sandbox = false` is built as
  `for_helper(state_dir, writable_roots).unconfined(..)`, so its state
  dir stays granted in owner mode (`ferrule-mcp/src/client.rs`).
- Owner mode's `Sandbox::unix_sockets` doesn't count `/` as an "own
  dir": the roots are the ones `workspace-write` would give.
- `config_restore` from the chat refuses when the last-good config
  differs in a guarded field. A config that doesn't parse counts as
  empty, so any guarded field in the last-good copy refuses from the
  chat; the dashboard restores (`dashboard/api.rs`).
- In owner mode autocommit's git runs with `core.hooksPath=/dev/null`,
  `core.fsmonitor=false` and `commit.gpgsign=false`
  (`autocommit.rs`): a guest can write `.git` in the shared workspace.
  Repo hooks don't run on owner-mode auto-commits.
- Worktree git uses `core.hooksPath=/dev/null` in every mode, so the
  temp-dir hole is closed outside owner mode too. Since the merge with
  "Ferrule's own git calls no longer run the repo's filters or a planted
  hook" that pin is `ferrule_sandbox::host_git`'s, on every platform
  (`ferrule-agents/src/worktree.rs` `run_git`). Without a launcher, in
  owner mode that helper's `git` is `host::system_program("git")`.
- `send_link` refuses an owner chat on the HTTP channel, and
  `dashboard_link` and `secret_request` are audited (`self_service.link`,
  `self_service.secret_request`, with `by`). Their answers say "Sending…",
  not "I sent…": delivery is the channel's, after the answer.
- The wake after a key is saved waits `SYNC_EVERY` + 1 s, so
  `config_follow` has bound the key into the running proxy
  (`keys.rs`). Who asked is in memory only.
- On a managed bot `package_install` and `package_remove` are refused,
  and `mcp_add` unless extensions are on (`self_service/locks.rs`).
- The dashboard's Autonomy row writes `workspace-write` for "Ask me
  first", drops `gates = false` only when it is there, and restarts the
  gateway; `off` is shown as "Sandbox off (set on the machine)"
  (`dashboard/config_page.rs` `sandbox_mode`).
- Known gap, not caused by M54: the backup (`backup::SECRETS`) doesn't
  list `gateway/http/clients.json`, which holds the HTTP API clients'
  key hashes, so a backup the bot can ask for and read contains them.

## Amendments after the build

Made on `autopilot` (2026-10-21), from the gap analysis's M54.2–M54.6
and a privacy hole found on review. Code paths are under `crates/`.

1. **The owner's memory is the owner's sessions' only, in every mode.**
   `memory.db` was shared by every chat, so a guest's chat got the
   owner's profile block, per-message recall and `recall` results.
   `owner_mode::owner_tree(cfg, hub, tree)` now decides it in every mode
   (a Telegram group counts only while `wanted(cfg)`, when
   `with_only_owner_in_groups` holds). A guest session gets its own
   `<data>/guest-memory/<chat>.db` (`owner_mode::guest_memory`, keyed by
   `chat_of`), `Sandbox::narrowed` in every mode (`guest_hidden` adds
   `memory.db` and its WAL files, `guest-memory/` and `learned-skills/`
   to §7's two), and only its own scope's lessons and learned skills
   (`learn::prompt_section`/`skill_view` with `only`,
   `ferrule_learn::prompt_block_only`, `skills::Entry::kept_for`). A
   guest's `/plan`, `/goal` or `/graph` run learns in its chat's scope,
   not the shared workspace scope (`learn::scope_for`). User docs:
   `memory.md`, "Whose memory".
2. **Known key values never reach a transcript (M54.3).** §7 made
   `sessions/` readable and left "the builder checks that nothing in
   them is a live credential" to inspection. Now it holds by
   construction: `ferrule-core` `scrub` is one process-wide hook,
   applied in `agent.rs` `Ran::of` (where a call becomes the result the
   transcript, the model, hooks, the trace and the stuck detector read)
   and at the start of `ToolOutput::capped`, `spilled` and
   `spill_preview`, `jobs.rs` `Job::tail` and the SSH shell's remote
   spill, so a cut never leaves part of a key. `ferrule-cli`
   (`health::scrub_tool_results`, from `main.rs` `dispatch`) installs
   it: the proxy's bound secrets become their placeholders
   (`Broker::scrub_text`), then every other known value
   (`health::known_secrets`: `[secrets]`, channel tokens, provider keys,
   everything the secrets file set) becomes `[redacted]`
   (`Redactor::known`, values only, no token shapes). `ferrule doctor`'s
   `history` line scans `sessions/` and `learn/` for those values
   (and their JSON-escaped form) and reports names, counts and files,
   never a value; it fails in owner mode while any are there. Left open:
   the claude engine's own tools (its Bash, Read) don't pass through the
   loop, values under 6 characters are skipped, connection OAuth tokens
   aren't in the list, and the values are read once at startup (a
   `[secrets]` key bound later is covered by the proxy).
3. **A danger notice while the box is open (M54.6).** §8 gave the owner
   a switch but nothing that says the box is open, and the old stopgap
   showed how a "temporary" setting outlives its reason.
   `owner_mode::openings(cfg)` lists owner mode running (`on`, so not
   after a fallback), `[sandbox] mode = "off"` and `[trust] gates =
   false`, each with what it opens and how to close it. `ferrule doctor`
   warns once per opening (`open`), with the way to close it as the hint.
   The dashboard's Home (`api.rs` `health`) shows each as a red (`top`)
   notice with "Ask me first" (`sandbox/mode {mode: "ask"}`, which also
   drops `gates = false`) and "Open Settings"; a managed bot gets no
   buttons. The sandbox-off and gates-off notices can't be closed
   (`notices::unclosable`); owner mode's closes for a day, as it's a
   choice the dashboard offers, and comes back while still true.
4. **One gate stays in owner mode (M54.5).** §5 turned the gates off
   ("not safety from the bot"). That left a confused deputy open: the
   proxy swaps the real key in for its own host whoever asked, so an
   injected email or page could send mail as the owner, delete a remote
   branch or `DELETE` on a bound API without ever seeing a key. Now
   `[trust] untrusted_send_gate` (default on, guarded by its name)
   keeps `ConnectedWrite`, `ForcePush` and `HttpDelete`
   (`Kind::leaves_the_box`) gated in a run that has read outside
   content: `classify::untrusted_source` (`web_fetch`, `web_search`,
   the browser, a connected service's read, a shell HTTP client) marks
   the run (`Hub::saw_untrusted`, `RunState::untrusted`, audited as
   `untrusted_seen`), and a guest's run in owner mode starts marked
   (`TrustGuard::guest_input`). `TrustGuard::untrusted_gate` is set by
   `trust::equip` only while owner mode runs. The question names the
   source; a shell command keeps "Allow for this run"; a scheduled task
   is refused. User docs: `m19-trust-cost.md` §6b.
5. **The secrets file stays out of the environment (M54.2).** §3 left
   the keys in the gateway's environment, relying on `require` and
   Landlock's ptrace scope; on macOS whether Seatbelt stops `ps eww` is
   unverified, and every child inherited the keys. Now
   `secrets::load` puts the file into `ferrule_sandbox::secret_env`, an
   in-memory store, instead of `set_var`. Every reader goes through
   `secret_env::var` (`secrets::var` in `ferrule-cli`): a value saved
   since start, else the real environment, else the file's, so an
   exported variable still wins. The providers' keys
   (`config.rs`, `models.rs` `Entry::key`, which replaces the old
   `SAVED_KEYS`), the proxy's binding (`shared_broker`, `mcp_add`), the
   channel tokens, transcription (`ferrule-gateway` `transcribe.rs`),
   the sealing key (`ferrule-connections` `seal.rs`), the plugin host's
   real-value check, the claude plan's exported token, doctor and setup
   read it there; the dashboard's rotate and delete change the store,
   no longer `set_var`/`remove_var` with threads running. No child
   inherits a held value: `Sandbox::command` passes one only for an
   `env_passthrough` name (or all with `scrub_secret_env = false`),
   `child_env_var` agrees, and an owner's hook gets its `env` names
   (`ferrule-hooks`). Left: the owner's other spawns (gate scripts, a
   transcription command) get the real environment only, a behaviour
   change documented in `sandbox.md` "Env"; the store is plain memory,
   not locked or encrypted, and the file isn't sealed under a keychain
   key. User docs: `sandbox.md` "Env", `m18-hooks.md` §1–2.

## Build plan

> **For agentic workers:** REQUIRED SUB-SKILL: use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to build this plan task by task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal.** Add `[sandbox] mode = "owner"`: the owner's bot does
everything from the chat without approvals, and the keys, plus whatever
decides what runs outside the sandbox, stay out of its reach.

**Architecture.** There are three layers:

- `ferrule-sandbox` learns a fourth mode and a read-only `protected`
  list.
- `ferrule-cli` fills in the protected list, picks the narrowed sandbox
  for other people's chats, moves the binary, and routes every other
  spawn through the sandbox.
- `ferrule_admin` stops asking and gains six ops.

**Tech.** Rust, edition 2021; Landlock and Seatbelt as today;
`toml_edit` for config writes; the dashboard's plain JS (`app.js`).

**Spec.** Everything above in this file; §n refers to its sections.

### Global constraints

- **The owner's standing rules:**
  - no tests unless the owner asks;
  - nothing built, run or verified unless asked;
  - no commit unless asked.

  The "Check" lines below list what to run once the owner asks for it.
- The AGENTS.md hard rules: the sandbox stays on by default; secrets
  never reach the model; every run ends in a truthful status; the
  prompt prefix stays byte-stable.
- Owner mode is Linux-only (a correction made while building; see
  "Non-goals"). `Sandbox::new` refuses it elsewhere with
  `owner mode needs Linux; this is macOS` or
  `owner mode needs Linux; this is Windows`.
- Format what you touch with `rustfmt --edition 2021`. Dependencies go
  in the root `[workspace.dependencies]`.
- Two config values exist, and no third:
  - `owner`, the new mode;
  - `workspace-write`, which the dashboard calls "Ask me first".

### Review focus

These are inputs the spec implies but no step exercises. A reviewer
reads for each one; each becomes a test if the owner asks for tests.

1. **A protected dir that doesn't exist at start** must still be
   unwritable (`~/.config/environment.d`, `~/.local/share/systemd`).
   The host creates them, empty, before building the policy (Task 3).
2. **A symlink from a writable place into a protected or hidden path**
   (`ln -s ~/.config/ferrule ~/x; echo > ~/x/config.toml`) is refused:
   - Landlock and Seatbelt judge the real path;
   - the file tools canonicalize before checking (Task 3).
3. **The workspace vs. protected paths:**
   - a workspace that contains the data dir (`~`) leaves the data dir
     read-only (protection beats grants, Task 1);
   - a workspace inside a protected path makes owner mode fall back
     (Task 3).
4. **Package names that aren't names** (`-o…`, `./x.deb`, `a b`,
   `pkg/foo`) are refused before sudo runs. A removal that would take
   `sudo`, `apt`, `openssh-server`, `systemd` or `ca-certificates` with
   it is refused after a dry run (Task 7).
5. **`secret_request` for a name already in use, `FERRULE_*`, or
   `LD_PRELOAD`** is refused with the same words `secrets/add` uses. The
   link never appears in the tool result or a transcript (Task 8).

---

### Task 1: the sandbox's owner mode (`ferrule-sandbox`)

**Files:**
- Modify: `crates/ferrule-sandbox/src/lib.rs` (`Mode`, `Policy`,
  `Sandbox::new`, `unconfined`, `for_child`, `writable_roots`,
  `command`, `restricts_unix`, `unix_status`, `model_note`)
- Modify: `crates/ferrule-sandbox/src/linux.rs` (`ruleset`, `carve`,
  `carve_into`, `apply`)
- ~~Modify: `crates/ferrule-sandbox/src/seatbelt.rs` (`profile`)~~
  *Corrected while building: no Seatbelt change; see Step 5.*

**Interfaces (produced):**
- `Mode::Owner`, read from the config as `"owner"` (kebab-case, like
  the others).
- `Policy.protected: Vec<PathBuf>`, `#[serde(skip)]`, filled in by the
  host the way `hidden` is.
- `Sandbox::is_owner(&self) -> bool`.
- `Sandbox::narrowed(&self) -> Sandbox`: a clone with `Owner` →
  `WorkspaceWrite`, otherwise unchanged. *As built:* `narrowed(&self,
  hide: &[PathBuf])`, which also adds `hide` to the hidden list
  (correction 5).
- `Sandbox::protected_paths(&self) -> Vec<PathBuf>`: canonical,
  missing ones skipped, like `hidden_paths`.
- `linux::apply(cmd, abi, network, writable, hidden, protected, owner: bool, unix)`.

Steps:

- [ ] **Step 1: `Mode::Owner` and `Policy.protected`.** Add the
  variant. Its doc comment says writes go everywhere but `hidden` and
  `protected`.
- [ ] **Step 2: `Sandbox::new`.**
  - On Windows, `Mode::Owner` returns `Err` with the Global-constraints
    sentence. (*As built:* on every non-Linux target; Step 5.)
  - It forces `policy.require = true` for `Owner`. The host turns the
    error into a refusal (Task 3).
- [ ] **Step 3: `writable_roots` for `Owner`.** Return `/` first (on
  Linux and macOS; *as built: Linux only*, correction 1), then what `WorkspaceWrite` returns: the workspace,
  `writable_roots`, `helper_roots`, temp. Rules add up, so the extra
  ones only matter where they sit inside a protected path.
- [ ] **Step 4: protection in the Landlock carve.**
  - `carve(root, access, walls, level)`: `level` is the mask a directory
    on the way down gets. Today that is `READ_DIR`; in owner mode it is
    `READ_DIR | MAKE_DIR`.
  - In `ruleset`, the read carve's walls stay `hidden`.
  - Each writable root's walls are `hidden` plus every protected path
    that does **not** contain the root:
    `protected.iter().filter(|p| !root.starts_with(p))`.
  - Result: `/` is carved around every protected path, while a root
    that lies inside one is a deliberate grant: an MCP state dir, or
    `<data>/worktrees/<id>` as a child's workspace. That is §1's
    rule (as corrected there).
- [ ] **Step 5: macOS and Windows refuse owner mode.** This is a
  correction made while building: no Seatbelt changes. `Sandbox::new`
  returns the Global-constraints error on any non-Linux target.
- [ ] **Step 6: helpers lose their exemptions in owner mode.**
  - `unconfined(&self, reason)` returns `self.clone()` with
    `network = true`, not hide-only, when `self.is_owner()`.
  - `restricts_unix` reads
    `!self.hide_only && !(self.desktop && !self.is_owner())` plus the
    current checks.
- [ ] **Step 7: read-only cases.** In `for_child(read_only = true)`,
  map `Owner` to `ReadOnly`, as `WorkspaceWrite` already is.
  `for_planning` follows, since it calls `for_child`.
- [ ] **Step 8: `model_note` for `Owner`.** One fixed sentence, built
  only from the mode and the network setting so the prefix stays
  stable:

  > Commands run in a sandbox that writes everywhere this account can,
  > except ferrule's own files and the saved keys. Change ferrule only
  > through ferrule_admin. A new folder directly in ~, ~/.config,
  > ~/.local or ~/.local/share can be filled from your next command:
  > mkdir it first. Elsewhere a refused write fails with
  > "{DENIED}".
- [ ] **Step 9:** `rustfmt --edition 2021` on the three files.

**Check (owner's call).** `cargo test -p ferrule-sandbox`. A Linux probe:
- `cat <data>/private/secrets.env` → `Permission denied`;
- `touch ~/.config/ferrule/x` → `Permission denied`;
- `mkdir ~/newdir` succeeds, and `touch ~/newdir/f` works in the next
  command.

### Task 2: spawns through the sandbox (§4)

**Files:**
- Create: `crates/ferrule-sandbox/src/host.rs`
- Modify:
  - `crates/ferrule-sandbox/src/lib.rs` (`pub mod host`)
  - `crates/ferrule-hooks/src/command.rs`
  - `crates/ferrule-gateway/src/scheduler/gate.rs`
  - `crates/ferrule-gateway/src/transcribe.rs`
  - `crates/ferrule-mcp/src/browser.rs`
  - `crates/ferrule-connections/src/{tunnel.rs,cloudflared.rs,keyform.rs}`
  - `crates/ferrule-plans/src/claude/{update.rs,cli.rs}`
  - `crates/ferrule-cli/src/update/claude.rs`
  - `crates/ferrule-agents/src/worktree.rs`

**Interfaces (produced):**
- `ferrule_sandbox::host::set(sandbox: Sandbox)`: once per process, the
  owner sandbox, set by the gateway only in owner mode (Task 3).
- `ferrule_sandbox::host::command(program: impl AsRef<OsStr>, args:
  impl IntoIterator<Item = impl AsRef<OsStr>>, cwd: &Path) ->
  io::Result<std::process::Command>`:
  - unset: a plain `Command` with the args and `current_dir(cwd)`;
  - set: `sandbox.command(program, args, cwd)`.

  `// ponytail: one process-wide launcher, not a sandbox threaded
  through five crates. Per-session launchers if a guest ever needs
  hooks.`
- `ferrule_agents::worktree::set_git(f: GitCommand)`, where
  `pub type GitCommand = Arc<dyn Fn(&Path) -> io::Result<std::process::Command> + Send + Sync>`
  returns a `git` command running in that dir. Unset, `git()` builds
  `Command::new("git")` as today.

Steps:

- [ ] **Step 1:** write `host.rs`: a `OnceLock<Sandbox>`, `set`, and
  `command` as above.
- [ ] **Step 2:** replace `Command::new(…)` with `host::command(…)` at:
  - the hook run (`command.rs` `run`);
  - `run_gate`;
  - `CommandTranscriber::call`;
  - `find_agent_browser`'s `--version`;
  - `tunnel.rs`'s cloudflared, `cloudflared.rs`'s version check, and
    `keyform.rs`'s `node`.

  Where the site uses `tokio::process::Command`, wrap it with
  `tokio::process::Command::from(std_cmd)`, then keep its existing
  `.stdin/.stdout/.kill_on_drop` calls. Keep `process_group(0)`.
- [ ] **Step 3: claude's version check and updater.**
  - `ferrule_plans::claude::cli::version` and `update::apply` (the fn
    at ~255) take a new `sandbox: Option<&ferrule_sandbox::Sandbox>`.
    With `Some`, the command comes from `sandbox.command(…)`.
  - `crates/ferrule-cli/src/update/claude.rs` passes
    `Some(&engine_sandbox(cfg, &config_dir)?)` when
    `owner_mode::on(cfg)`, and `None` otherwise. (*As built:* the test
    is `owner_mode::wanted`, and a sandbox that can't be built refuses.)
  - If `as_owner` needs another user, it returns the error: "claude's
    updater runs as another user; owner mode can't switch users".
- [ ] **Step 4: worktree git.**
  - `git()` uses the `GitCommand` when one is set.
  - `core.hooksPath` becomes `/dev/null` on Unix: git then finds no
    hooks, and nothing in temp is trusted. Windows keeps the temp dir.
- [ ] **Step 5:** `rustfmt` on the touched files.

**Check (owner's call).**
`cargo test -p ferrule-hooks -p ferrule-gateway -p ferrule-agents -p ferrule-mcp -p ferrule-plans -p ferrule-connections`.
Unset launchers keep today's behaviour, so the existing tests stay
green.

### Task 3: host wiring (`ferrule-cli`)

**Files:**
- Create: `crates/ferrule-cli/src/owner_mode.rs`
- Modify:
  - `crates/ferrule-cli/src/main.rs`: `mod owner_mode`;
    `sandbox_policy`; `hidden_paths`; `shared_sandbox`;
    `engine_sandbox`; `build_agent_from`; gateway start (~2639)
  - `crates/ferrule-cli/src/backup.rs` (`SECRETS` becomes `pub`, if it
    isn't)
  - `crates/ferrule-cli/src/hooks_cli.rs`, if the hooks build call
    changes

**Interfaces (produced, in `owner_mode.rs`):**
- `pub fn on(cfg: &Config) -> bool`: `cfg.sandbox.mode == Mode::Owner`.
- `pub fn protected_paths() -> Vec<PathBuf>`: the data dir, the config
  file's dir (`$FERRULE_CONFIG`'s, or the global config's),
  `~/.config/systemd`, `~/.local/share/systemd` and
  `~/.config/environment.d`. Each missing dir is created first
  (`create_dir_all`), so its protection never depends on a parent
  being carved (review focus 1).
- `pub fn owner_tree(hub: &Hub, tree: &str) -> bool`, decided in this
  order:
  1. `self_service::offered(hub, tree).is_some()` → true (the owner's
     private chat, the dashboard chat).
  2. A channel session (`channels::of_session(tree)` is `Some`) → false.
  3. A scheduler session (`scheduler__<id>`) → look the task up in
     `<data>/tasks.db`:
     - true if `(task.channel, task.chat_id)` is an owner chat
       (`offered` on its session id);
     - true if the task has no chat;
     - false otherwise.
  4. Anything else (`ferrule run`, `chat`, goal and graph runs) → true.

  A sub-agent's tree is its root's, so it follows the root.
- `pub fn refusal(cfg: &Config, workspace: &Path) -> Option<String>`:
  why owner mode can't run here, in one sentence the owner reads:
  - the binary isn't in `<data>/bin` yet and couldn't be moved
    (Task 5);
  - the workspace is inside a protected path;
  - the sandbox can't be applied.

Steps:

- [ ] **Step 1: `hidden_paths(owner: bool) -> Vec<PathBuf>`.** It
  hides every `backup::SECRETS` entry under the data dir, keeping the
  `private` `create_dir_all` and its `0700` mode. With `!owner` it adds
  `learn` and `sessions`, as today. The three callers (`main.rs` ~1739,
  ~2683, ~3432) pass `owner_mode::on(cfg)`, or `false` where no config
  is in hand.
- [ ] **Step 2: `sandbox_policy`.** When `owner_mode::on(cfg)`, set
  `policy.protected = owner_mode::protected_paths()`.
- [ ] **Step 3: the fallback.** In the gateway start (~2639, before
  `shared_sandbox`):
  - if `owner_mode::refusal(cfg, &workspace)` is `Some(why)`, clone the
    config with `sandbox.mode = WorkspaceWrite` and use that config from
    then on;
  - after the hub exists, call `hub.tell_owner(format!("Owner mode is
    off this run: {why}. I'm in the ask-first mode until that's
    fixed."))`.

  If `Sandbox::new` fails with `require` forced on, use
  `Sandbox::off().refuse(why)`: chat works, commands are refused. The
  owner is told the same way. Never run unsandboxed.
- [ ] **Step 4: process-wide launchers.** *As built, replacing the
  plan's gateway-only text* (`owner_mode::arm`, called from `main.rs`
  `dispatch` for every subcommand, and from the gateway and
  `tasks run-now` with their workspace):
  - When the config asks for owner mode (`owner_mode::wanted`),
    `host::set(…)` takes the shared sandbox `.for_helper(<data>/worktrees, &[])`.
    After a fallback it takes `fallback_launcher` instead: a sandbox
    built from the owner policy (hidden and protected walls, writes
    elsewhere) with this binary's real path added to the protected set.
    If that can't be built, it takes a sandbox that refuses every spawn.
  - Worktree git (`worktree::set_git`) gets the same sandbox with the
    workspace granted too, so a worker's close-out can commit to the
    repo's git dir. In the gateway this also holds in every mode where
    the shared sandbox is active.
- [ ] **Step 5: narrowing in `build_agent_from`.** After `tree` is
  known (~1376): if `sandbox.is_owner() && !owner_mode::owner_tree(&trust::hub(&cfg)?, &tree)`,
  then `sandbox = Arc::new(sandbox.narrowed(&hide))` (*as built:* with
  `owner_mode::guest_hidden()`). The same check wraps
  the provider: `RoutedProvider::guest(self) -> Self` (Task 3b).
- [ ] **Step 6: file tools.** `WriteFileTool::hiding` and the edit
  tool get `hidden` plus `sandbox.protected_paths()` (owner mode only),
  so a workspace that contains the data dir can't be written through
  them (review focus 3).
- [ ] **Step 7: what the owner can read back.** `ferrule sandbox` and
  doctor's sandbox line say `owner`, and list the protected paths where
  they list the hidden ones today. On a fallback, doctor gives the
  `refusal` reason.
- [ ] **Step 8:** `rustfmt` on the touched files.

**Check (owner's call).** `cargo test -p ferrule-cli --test it` and
`ferrule sandbox` on Linux with `mode = "owner"`. It should list the
hidden and protected paths, and `unix: allowlist`.

#### Task 3b: the claude engine's narrowed sandbox

**Files:**
- Modify:
  - `crates/ferrule-core/src/provider.rs` (`Provider` trait)
  - `crates/ferrule-plans/src/claude/engine.rs`
  - `crates/ferrule-cli/src/models/routing.rs` (`RoutedProvider`)

**Interfaces:**
- `Provider::guest(&self) -> Option<Arc<dyn Provider>>`: a new method
  with the default `None`, so no implementor breaks.
- The claude engine overrides it: the same engine, sharing its state
  (`Arc` fields), with `sandbox.map(|s| s.narrowed(&hidden))` (*as
  built:* `hidden` is `EngineConfig` `guest_hidden`).
- `RoutedProvider::guest(self) -> Self` sets a flag. On each call it
  uses `p.guest().unwrap_or(p)`.

`// ponytail: only the claude engine runs tools in its own process;
other providers return None.`

- [ ] **Step 1:** add the trait method with its default.
- [ ] **Step 2:** the engine override.
- [ ] **Step 3:** the `RoutedProvider` flag.

### Task 4: no gates for the owner's trees

**Files:**
- Modify: `crates/ferrule-trust/src/guard.rs`,
  `crates/ferrule-cli/src/trust.rs` (`equip`)

**Interfaces:**
- `TrustGuard::gates(self, on: bool) -> Self`, a new `gates: bool`
  field defaulting to `true`. The check at `guard.rs` ~129 becomes
  `if !self.gates || !self.hub.config().gates`.

Steps:

- [ ] **Step 1:** add the field and the builder.
- [ ] **Step 2:** in `equip`, chain
  `.gates(!(owner_mode::on(cfg) && owner_mode::owner_tree(&hub, tree)))`.

### Task 5: the binary moves into the data dir (§3)

**Files:**
- Modify:
  - `crates/ferrule-cli/src/owner_mode.rs`
  - `crates/ferrule-cli/src/setup.rs` (`refresh_service`)
  - `crates/ferrule-cli/src/service.rs`, where units are installed
  - gateway start in `main.rs`

**Interfaces:**
- `setup::refresh_service_with(exe: Option<PathBuf>) -> Result<()>`.
  `refresh_service()` becomes `refresh_service_with(None)`. With
  `Some(exe)`, that path wins over `installed_exe()`.
- `owner_mode::relocate() -> Result<Moved, String>`, where
  `enum Moved { Already, Restarting }`.

Steps:

- [ ] **Step 1: `relocate`.**
  - If `current_exe()` (canonical) is under `<data>/bin`, return
    `Already`.
  - Otherwise (*as built, replacing the plan's rename*):
    1. `create_dir_all(<data>/bin)`.
    2. Hard-link the exe to `<data>/bin/ferrule` through a file beside
       it and a rename. On any failure, copy (mode `0755`).
    3. `setup::refresh_service_with(Some(new))`, which rewrites the
       units and reloads them without a restart.
    4. Swap a symlink in at the old path: make `<old>.ferrule-link`,
       then rename it over the old path.
    5. Return `Restarting`.
  - If there's no service (`service::status()` isn't `Installed`),
    return `Err("owner mode needs ferrule to run as a service")`, before
    moving anything.
- [ ] **Step 2:** the `systemctl` that `service::install` runs uses the
  absolute `/usr/bin/systemctl` (or `/bin/systemctl` if that's where it
  is), not a PATH lookup.
- [ ] **Step 3: gateway start.** If owner mode is on and
  `relocate()` returns:
  - `Restarting`: exit cleanly (`restart::How::Terminate`, the service
    starts the new path);
  - `Err(why)`: feed `why` into Task 3's `refusal`.

### Task 6: change ops without the card (§5)

**Files:**
- Modify:
  - `crates/ferrule-cli/src/self_service/{mod.rs,tool.rs,run.rs}`
  - `crates/ferrule-agents/src/prompts.rs`
  - `crates/ferrule-cli/src/dashboard/{api.rs,config_page.rs}`
  - where `Admin::new` is called

**Interfaces:**
- `Admin::new(hub: Arc<Hub>, autonomous: bool) -> Arc<Admin>`, with
  `pub fn autonomous(&self) -> bool`. Owner mode passes `true`.
  `with_timeout` (tests) passes `false`.
- `prompts::OWNER_ADMIN_AUTONOMOUS` and
  `prompts::ADMIN_DESCRIPTION_AUTONOMOUS`: same shape as their
  siblings. They say changes run at once and are logged, and list the
  new ops from Task 7.
- `config_page::guarded_change` becomes `pub(crate)`.

Steps:

- [ ] **Step 1: `ask_and_run`.** When `admin.autonomous()`, skip the
  card, after `resolve`, `locks::refusal`, and `update::card` /
  `restart::card` (kept as preconditions; their text is dropped). Run
  the op, and audit `self_service.ran` with
  `by = "{channel} chat {chat} (owner mode)"`. Its answer starts
  "Done:" or "Not done:", not "Approved…".
- [ ] **Step 2: the strings.** `AdminTool::definition` picks
  `ADMIN_DESCRIPTION_AUTONOMOUS` when `autonomous`, and the gateway
  factory (`main.rs` ~2688) appends `OWNER_ADMIN_AUTONOMOUS`. Each is a
  constant per mode (byte-stable).
- [ ] **Step 3: the `config_restore` guard.**
  - `run.rs` `route_of(ConfigRestore)` sends `{"from_chat": true}`.
  - `api.rs` `config_restore`: when `body["from_chat"] == true`, parse
    the current file and the last good copy. If `guarded_change(&now,
    &good)` is `Some(field)`, answer 403: "`{field}` differs in the
    last good config, so only the owner can restore it, on the
    dashboard."

### Task 7: the new ops (§6)

**Files:**
- Create: `crates/ferrule-cli/src/self_service/packages.rs`
- Modify:
  - `crates/ferrule-cli/src/self_service/{mod.rs,run.rs,describe.rs,locks.rs,tool.rs}`
  - `crates/ferrule-cli/src/dashboard/door.rs` (factor out the link
    sending)

**Interfaces:**
- **`Op` variants:**
  - `PackageInstall { packages: Vec<String> }`
  - `PackageRemove { packages: Vec<String> }`
  - `McpAdd { name: String, command: Vec<String>, url: Option<String>, env: Vec<String>, headers: Vec<String>, enabled_tools: Vec<String>, replace: bool }`
  - `SecretRequest { name: String, hosts: Vec<String>, why: String }`
  - `Secrets`
  - `DashboardLink`
- **Op lists:**
  - `READ_OPS` gains `secrets`, `secret_request`, `dashboard_link`.
  - `CHANGE_OPS` gains `package_install`, `package_remove`, `mcp_add`.
- **`Args` gains:** `packages`, `command`, `url`, `env`, `headers`,
  `enabled_tools`, `replace`, `hosts`, `why` (the tool's JSON schema
  too).
- **`packages.rs`:**
  - `pub fn valid(name: &str) -> bool`: `^[a-z0-9][a-z0-9+.-]*$`, at
    most 128 characters, and not ending in `.deb`.
  - `pub async fn install(names: &[String]) -> Result<String, String>`
  - `pub async fn remove(names: &[String]) -> Result<String, String>`
- **`door.rs`:** `pub async fn send_link(dash: &Dashboard, hub: &Hub,
  to: &ChatRef, suffix: &str) -> Result<(), String>`. This is the
  `/dashboard` code, with `suffix` added before the `#` (Task 8 uses
  it).

Steps:

- [ ] **Step 1: `packages.rs`.**
  - It runs on Linux only, with `/usr/bin/apt-get` present. Otherwise:
    "System packages are apt-only, and this machine has no apt."
  - Each run is
    `/usr/bin/sudo -n /usr/bin/env DEBIAN_FRONTEND=noninteractive /usr/bin/apt-get <verb> -y …`,
    with a 10-minute limit.
  - **`install`:**
    1. Run `install -y --no-install-recommends -- <names>`.
    2. If the output has "Unable to locate package", run `update` once,
       then retry.
    3. Then run `clean`.
  - **`remove`:**
    1. Run `remove -s -- <names>` first.
    2. If any `Remv <pkg>` line names `sudo`, `apt`, `openssh-server`,
       `systemd` or `ca-certificates`, refuse with the list (review
       focus 4).
    3. Then run `remove -y -- <names>`, then `clean`.
  - If sudo says a password is required, answer: "This account can't
    use sudo without a password, so I can't install system packages.
    User-level installs (pipx, uv, npm, a binary in ~/.local/bin) still
    work from my shell."
  - The result is the last 20 lines, through `clip` and the redactor.
- [ ] **Step 2: `mcp_add` in `run.rs`.**
  - It refuses:
    - any `${NAME}` in `headers` whose NAME isn't in `cfg.secrets`;
    - any `env` entry whose name `looks_secret`.

    Message for both: "keys are added by the owner: use secret_request".
  - It builds `mcp_add::AddArgs { yes: true, no_doctor: true,
    workspace: <gateway workspace>, … }`, with `no_sandbox`, `waive`
    and `skip_flagged` false. Then it calls
    `mcp_add::add_to(ctx.config_path, args)`.
  - The answer lists the returned tool names. `config_follow` loads
    the server.
- [ ] **Step 3: `secrets`.** Format `tool_keys::list` as
  "NAME → host, host" lines. It never shows a value.
- [ ] **Step 4: `dashboard_link`.**
  - With a page (`Bound::Page`), call `door::send_link(…, "")`. The
    tool result is "I sent the owner a one-time dashboard link (valid
    N min)."
  - Without a page: "The dashboard isn't on in this process."
- [ ] **Step 5: cards for ask-first mode** (`describe.rs`):
  - `package_install`: "Install system packages with sudo apt-get:
    {names}."
  - `package_remove`: "Remove system packages: {names}."
  - `mcp_add`: "Add the MCP server {name} ({command or url}); it starts
    now and its tools are scanned first."
- [ ] **Step 6: managed locks** (`locks.rs`):
  - `package_*` is refused under any managed policy (the container is
    the image's);
  - `mcp_add` falls under the extensions lock, like `mcp_on`.

### Task 8: asking for a key (§6.3)

**Files:**
- Create: `crates/ferrule-cli/src/self_service/keys.rs`
- Modify:
  - `crates/ferrule-cli/src/self_service/{mod.rs,run.rs}`
  - `crates/ferrule-cli/src/dashboard/tool_keys.rs`
  - `crates/ferrule-cli/src/dashboard/assets/app.js`

**Interfaces (in `keys.rs`):**
- `pub fn bind(router: Weak<Router>)`: called from
  `Admin::bind_router`.
- `pub fn asked(name: &str, session: &str)`: a static
  `Mutex<HashMap<String, String>>`, where the last session to ask for a
  name wins.
- `pub fn saved(name: &str, hosts: &[String])`: if a session asked for
  `name`, it calls
  `router.wake(session, format!("[the owner saved the key {name}; it is sent only to {}]", hosts.join(", ")))`
  and forgets the name.
- `tool_keys`: the `add` checks (the name's shape, the name unused,
  the hosts parsing) move into
  `pub(crate) fn check_new(ctx: &Ctx, name: &str, hosts: &str) -> Result<Vec<String>, (u16, String)>`,
  which both `add` and the op call.

Steps:

- [ ] **Step 1: `keys.rs`** as above.
- [ ] **Step 2: the `secret_request` run.**
  1. `check_new`, with the same error words as the page (review focus
     5).
  2. `keys::asked`.
  3. `door::send_link(dash, hub, here, &format!("?key={name}&hosts={}", hosts.join(",")))`,
     URL-encoded.
  4. Tell the owner, in one message before the link: "The bot asks for
     a key: {name}, sent only to {hosts}. Why: {why}."
  5. The tool result: "I sent the owner a link to paste {name}; you're
     woken when it's saved."
- [ ] **Step 3: `tool_keys::add`.** After the save succeeds, call
  `self_service::keys::saved(name, &hosts)`.
- [ ] **Step 4: `app.js`.**
  - Before the login code's `history.replaceState` (~3533), read
    `key` and `hosts` from `location.search`.
  - After login, open the Connect section's `toolKeyPanel`, with
    `name` and `hosts` prefilled and the key field focused.
  - No new CSS.

**M52 note.** If M52's one-time paste link exists by now, Step 2.3
calls it instead. Steps 1, 3 and 4 stay.

### Task 9: the owner's switch (§8)

**Files:**
- Modify:
  - `crates/ferrule-cli/src/dashboard/{config_page.rs,api.rs}`
  - `crates/ferrule-cli/src/dashboard/assets/app.js`

**Interfaces:**
- `POST /api/sandbox/mode {mode: "owner" | "ask", confirm}`, handled by
  `config_page::sandbox_mode(ctx, body) -> Answer`. Page-only: no
  `route_of` maps to it.
- `GET settings` gains `"autonomy": "owner" | "ask" | "off"`.

Steps:

- [ ] **Step 1: the handler.**
  - It asks for confirmation (`confirmed`) with: "Restart in {Protected
    autonomous | Ask me first} mode?"
  - It writes `[sandbox] mode` through `setup::Target`. For "ask" it
    also removes `[trust] gates` when that is `false`.
  - It audits `dashboard.sandbox.mode`.
  - It restarts as `gateway/restart` does.
- [ ] **Step 2: the Settings row.** On the Settings page in `app.js`,
  add **Autonomy** with two choices, using existing components:
  - **Protected autonomous**: "does everything itself; can't see your
    keys";
  - **Ask me first**.

  When the value is "off", show it as "Sandbox off (set on the
  machine)", with both choices available.

### Task 10: docs and the log

**Files:**
- Modify:
  - `docs/sandbox.md` (an "Owner mode" section: the §1 table, the
    `mkdir` rule, and what's left from "What's closed, what's left")
  - `docs/roadmap.md` (an M54 entry)
  - `README.md` (the Documentation list gains this file)
  - this file (status, and corrections made while building)
  - `PLAN.md` (the dated entry, updated in place)

Steps:

- [ ] **Step 1:** write the docs. Every claim cites the code or this
  design.
- [ ] **Step 2:** the PLAN.md entry says what was built, and what was
  not run (per the owner's rules).

**Order.** 1 → 2 → 3 (+3b) → 4 → 5 → 6 → 7 → 8 → 9 → 10. Tasks 4 and 9
are independent of their neighbours once 3 is in.
