# M38 — Named instances (design)

Status: built, 2026-09-28, branch `m38-instances`. Written before the
code; where the build departs from it, see **As built** at the end. User
guide: [instances.md](instances.md).

Several independent agents on one machine, each with its own config, data,
bot, service and dashboard. Today there is one agent per user (or one per
machine as the root system service); a second one means a second Unix
user or a container. From M38:

```sh
ferrule instances new work        # setup for an agent called "work"
ferrule --instance work doctor
FERRULE_INSTANCE=work ferrule status
```

Five parts, each its own commit or commits:

1. The instance name and everything derived from it: paths, units,
   labels, the system user (§1–§2).
2. Every place that names the service goes through the instance: setup,
   `--refresh-service`, doctor, status, stop, restart, the update units and
   the update's restart, `install.sh` and uninstall (§3–§4).
3. `ferrule instances list | new | remove` (§5).
4. Collisions caught before they bite: the bot, the dashboard port, the
   workspace, the SSH workspace, the relay (§6).
5. The dashboard knows its instance; setup offers "add another instance"
   (§7–§8).

---

## 0. What doesn't change

- **The default instance, byte for byte.** No `--instance` and no
  `FERRULE_INSTANCE` means the paths, unit names, unit texts, launchd
  labels, system user and hints of 0.8.0, exactly. Max's server runs the
  default as a user service (`ubuntu`, data in `~/.local/share/ferrule`);
  others run it as the root system service. An upgrade to 0.9 must not
  move, rename or rewrite anything of theirs. Tests pin this: every
  derived path and every unit text for the default is compared with the
  0.8.0 literal.
- `FERRULE_CONFIG` and `FERRULE_DATA_DIR` still win over anything derived.
- The secrets file stays `<data>/private/secrets.env`. It is per instance
  because the data dir is.
- The eval starter suite, its graders and the mock model.

## 1. The name

`--instance <name>` is a global flag; it sets `FERRULE_INSTANCE`, which is
what everything reads (so a child process and a unit inherit it the same
way).

A name is 1–24 characters of `[a-z0-9-]`, starting and ending with a
letter or digit. Why each rule:

| rule | why |
|---|---|
| lower-case ASCII, digits, `-` | Safe verbatim in a systemd unit name, a launchd label, a Unix user name, a Windows path and a Cloudflare Worker name, with no escaping anywhere. |
| ≤ 24 | `ferrule-<name>` is the system user, and Linux caps user names at 32. |
| alphanumeric at both ends | `ferrule--x`, `ferrule-x-` and `-x` read as typos and are awkward on a command line. |
| not `default` | The default instance has no name. `--instance default` would be a second name for it and a second set of paths; refusing it says "leave `--instance` out". |

An empty `FERRULE_INSTANCE` is the default (so `FERRULE_INSTANCE=` in a
shell profile un-sets it). An invalid one stops every command before it
does anything: a typo must not quietly fall back to the default agent and
act on it.

## 2. What a name derives

One rule for directories: **the default's `ferrule` becomes
`ferrule-<name>`**. Siblings, not subdirectories. The default's data dir
already holds the agents, the ledger and the private dir, so
`remove --purge` of one instance can never reach into another's, and a
system instance's directories don't sit inside a directory another system
user owns (the default's `/var/lib/ferrule` is `ferrule:ferrule 750`, so
`ferrule-work` couldn't even traverse into it).

| | default (unchanged) | `work` |
|---|---|---|
| config (user) | `<config>/ferrule/config.toml` | `<config>/ferrule-work/config.toml` |
| data (user) | `<data>/ferrule` | `<data>/ferrule-work` |
| config (system) | `/etc/ferrule/config.toml` | `/etc/ferrule-work/config.toml` |
| home (system) | `/var/lib/ferrule` | `/var/lib/ferrule-work` |
| data / workspace (system) | `/var/lib/ferrule/{data,workspace}` | `/var/lib/ferrule-work/{data,workspace}` |
| update state (system) | `/var/lib/ferrule/update` | `/var/lib/ferrule-work/update` |
| system user | `ferrule` | `ferrule-work` |
| systemd unit | `ferrule.service` | `ferrule@work.service` |
| update units | `ferrule-update.{service,timer,path}` | `ferrule-update@work.{service,timer,path}` |
| launchd labels | `ai.ferrule.gateway`, `ai.ferrule.update` | `ai.ferrule.gateway.work`, `ai.ferrule.update.work` |
| macOS logs | `~/Library/Logs/ferrule/gateway.log` | `~/Library/Logs/ferrule-work/gateway.log` |
| hints | `systemctl --user restart ferrule` | `systemctl --user restart ferrule@work` |

`<config>` and `<data>` are `dirs::config_dir()` and `dirs::data_dir()`
(`data_local_dir()` on Windows), as today.

**Why `ferrule@work.service`.** `ferrule-work.service` reads better, but
the update units already use `ferrule-update.*`, so an instance called
`update` would take the default's update unit name, and `a`'s update unit
`ferrule-a-update` would be the instance `a-update`'s gateway. `@` can't
appear in a name, so `ferrule@<name>` and `ferrule-update@<name>` can't
collide with anything. The unit file is a concrete `ferrule@work.service`,
not a template: systemd loads an instance's own file before it looks for
`ferrule@.service`, and each instance's unit pins different paths anyway.
launchd labels use `.<name>`, since a name has no dots.

**The system user per instance.** A second system instance runs as
`ferrule-<name>`, not as `ferrule`. Two agents under one account can read
each other's secrets files, memory and workspace: exactly the separation
the milestone is for. It costs one `useradd` per instance.

**The unit says which instance it is.** A named instance's gateway unit and
update unit carry `Environment="FERRULE_INSTANCE=<name>"` beside the
pinned `FERRULE_CONFIG` (and the system unit's `FERRULE_DATA_DIR`). The
default's units carry nothing new.

**No `./ferrule.toml` for a named instance.** The default still picks up a
`ferrule.toml` in the current directory (a project config). A named
instance never does: `ferrule --instance work status` run from a project
directory must describe `work`, not the project.

**Windows** has no background service yet, so an instance there is its
config and data dir only. `instances list` says so.

`FERRULE_ROOT`, when set, replaces `<config>` with `$FERRULE_ROOT/config`
and `<data>` with `$FERRULE_ROOT/data` for every instance, the default
included. It exists because `dirs` ignores `APPDATA` on Windows, so a
test can't move the home any other way. It is documented for tests and
portable installs; nothing sets it.

## 3. The service, per instance

`service.rs` keeps its constants (they are the default's names, and the
0.8.0 tests keep using them). A `Svc { instance, scope }` value derives
every name in §2; each function that today reads a constant takes it
from a `Svc`. The free functions (`status()`, `install()`, `restart()`,
`logs_hint()`, …) keep their signatures and use the current instance and
scope, so `setup`, `doctor`, `status`, `stop` and the update flow become
instance-aware without each call site changing. `instances list/remove`
and the update's siblings (§4) build a `Svc` for another instance.

Scope is unchanged: root on Linux means system, unless `--user`. For a
named instance, "is there a system install" asks about *its* system
config and unit (`/etc/ferrule-work/config.toml`,
`/etc/systemd/system/ferrule@work.service`), never the default's.

The gateway's own restarts (M36 self-repair, M37's restart button and the
channel restart) already restart the process they run in: the gateway
exits and its supervisor, which is its own unit, starts it again. They
touch no other instance by construction.

`install.sh`'s upgrade block today refreshes one unit. From M38 it loops
over every gateway unit on the machine it can see: the default one plus
`ferrule@*.service` (user units, and system ones as root), or
`ai.ferrule.gateway*.plist` on macOS. For each that is active and runs
this `$BIN`, it runs `"$BIN" [--instance <name>] setup --refresh-service`,
falling back to a plain restart of that unit. A unit that runs another
binary gets the same note as today and a plain restart (it keeps the
other binary until someone points it here). A test runs `install.sh`
against stub `curl`, `systemctl`, `launchctl` and `id` and checks which
units it refreshes.

## 4. Updates: per-instance units, coordinated per binary

Two ways to go:

- **One shared updater** per binary. One timer instead of N, but every
  question it answers belongs to some instance: which config's
  `[update] channel`, which state dir, which instance the gateway's "update
  now" request came from, whose `auto = false` wins. It needs its own
  install and remove story (who owns it when the instance that created it
  is removed?), and it doesn't exist today.
- **Per-instance update units**, as M36 already installs, one set per
  instance, *coordinated* at apply time. Setup and remove stay symmetric,
  each gateway's request path stays in its own data dir, and system
  instances' units run with their own paths.

**Chosen: per-instance units, coordinated.** The binary is the shared
thing, so the apply run treats every instance whose unit runs the same
binary (a *sibling*) as part of the update:

1. **Siblings** are the instances in the same scope whose installed unit's
   `ExecStart` binary canonicalises to the binary being replaced.
2. **Locks**: the run takes the apply lock in its own state dir and in
   every sibling's, in sorted order. If any is held, another instance's
   updater is on it: this one stops ("another update is running").
3. **Pins**: a version pinned in any sibling's state (a rollback there) is
   pinned for this run too.
4. **`[update] auto = false` anywhere wins.** A timer run skips the install
   (it only checks) if any sibling's config says `auto = false`, and says
   which sibling held it. A request from a gateway (the owner pressed
   "update") is treated as asking too, since it comes from one owner and
   restarts the others. Its report names the sibling that said no.
   `ferrule update` in a terminal says which other instances restart and
   asks before it goes on (`--yes` answers).
5. **Idle** means every sibling's gateway is idle.
6. **Restart and health**: after the swap every sibling's service
   restarts, and each must come up healthy on the new version.
7. **Rollback**: if any doesn't, the old binary comes back, every sibling
   restarts again, and the version is pinned in every sibling's state,
   with the event recorded in each.

So a pinned instance (a rollback pin, or `auto = false`) is never
silently upgraded by another instance's updater. Instances that run
*different* binaries (one pinned to its own copy) are not siblings, and
each updates on its own.

## 5. `ferrule instances`

- **`list`**: one row per instance: name (`default` shown for the
  default), config path, data dir, service state (`running`, `stopped`,
  `not installed`, `none on this OS`, with `system` or `user`), dashboard
  port (the configured one, or the one the running gateway wrote to
  `<data>/gateway/dashboard.json`), and the bot (`@username` if the
  gateway recorded it, else the Telegram bot id, the part of the token
  before `:`, which isn't secret). `--json` for scripts. An instance is
  found by its config dir (`ferrule-*/config.toml` with a valid name), by
  its unit (`ferrule@*.service`, `ai.ferrule.gateway.*.plist`), and as root
  by its system config (`/etc/ferrule-*/config.toml`). The default is
  listed when its config or unit exists.
- **`new <name> [--system | --user]`**: validates the name, refuses one
  that exists, then runs `ferrule --instance <name> setup` as a child with
  this terminal. The child gets `FERRULE_INSTANCE` and none of this
  process's `FERRULE_CONFIG`, `FERRULE_DATA_DIR` or loaded secrets (§9).
- **`remove <name> [--purge] [--yes]`**: stops and removes the instance's
  service and update units. Without `--purge` the config and data stay,
  and it says where. With `--purge` it asks you to type the name (or
  `--yes`) and deletes the config dir and the data dir. It never deletes
  a workspace (it's yours) and never deletes a system user: it prints
  `userdel ferrule-<name>`. `remove` refuses the default: `ferrule setup`
  → service → remove does that, as today.

## 6. Collisions

Checked by `doctor` (across every instance it can read) and by `setup`
at the moment the value is chosen:

| what | when it clashes | severity | why |
|---|---|---|---|
| Telegram bot | two instances' tokens have the same bot id | fail, naming the other instance | Two pollers on one bot get HTTP 409 and each sees half the messages. |
| dashboard port | both enabled, the same non-zero port | fail | The second gateway can't bind; its dashboard never starts. `0` (any free port) never clashes. |
| workspace | the same directory (canonical) | fail | Two agents editing one tree, each with its own memory of it. |
| SSH workspace | the same host, port and path | fail | The same, remotely. |
| relay | the same `relay_url` with different keys | fail | A Worker has one key; one instance's callbacks are refused. |
| relay | the same `relay_url` and key | note | It works, but redeploying from one instance rotates the key and breaks the other. |

The bot id comes from each instance's token: its secrets file, else the
environment of *that* instance's unit. It is compared as an id and shown
as an id (never the token). A system instance's secrets file belongs to
its own user; a non-root doctor that can't read it says so once, instead
of guessing.

Relay naming: a named instance's default Worker name is
`ferrule-relay-<name>`, so deploying one from the page doesn't redeploy
(and re-key) the default's `ferrule-relay`.

## 7. The dashboard

- `child_env` passes `FERRULE_INSTANCE` to every child (doctor, the fix
  buttons, the console), so they act on the page's own instance.
- The console refuses `--instance`, as it refuses `--config`, and refuses
  `instances new` and `instances remove` (they act on other instances,
  and `new` needs a terminal).
- `/api/health` carries `instance` (absent for the default). The page puts
  it in the header beside the version and in the title (`ferrule · work`).
- A restart from the page restarts only its own gateway: it already exits
  and lets its own unit start it again.

## 8. Setup: add another instance

`ferrule setup` on the default instance, on a machine where the default is
already set up, first asks: **Change this agent's settings** or **Add
another agent (a named instance)**. The second asks for a name, validates
it, and runs `instances new` for it (keeping `--system`/`--user`). A first
setup, `--refresh-service` and a named instance's setup don't ask.

## 9. Threat model

- **Secrets stay in their instance.** Each instance's secrets file is in
  its own data dir. A process loads its own file into its environment
  (an existing variable wins), so a child started for *another* instance
  (`instances new`, setup's "add another") must not inherit them: it
  gets its environment minus every variable the secrets file loaded, minus
  `FERRULE_CONFIG` and `FERRULE_DATA_DIR`. Otherwise the new instance
  would silently reuse the first one's Telegram token (the 409 in §6) and
  keys.
- **System instances are separate accounts.** `ferrule-<name>` owns only
  its home, data and workspace; the config is `root:ferrule-<name> 640`.
  One agent's commands can't read another's secrets or memory.
- **User instances share a Unix account**, so they share a trust
  boundary: an agent's commands run as that user and could read the
  sibling's files (the sandbox still confines what they touch). The
  guide says so: for isolation between agents, use system instances, or
  separate users.
- `--instance` from the dashboard's console is refused, so the page can
  never act on an instance it doesn't belong to.
- An invalid name is refused before anything runs, so it can't reach a
  path or a unit name with `/`, `..` or a specifier in it.

## 10. Failure modes

| what | what happens |
|---|---|
| A typo in `--instance` (valid, but no such instance) | Setup creates it; every other command says there's no config for `work` and names `ferrule instances list`. |
| `FERRULE_CONFIG` set in the shell and `--instance` given | The variable wins, as always. `doctor` notes that the config isn't the instance's own. |
| Two updaters start at once for one binary | One takes every lock; the other stops with "another update is running". |
| A sibling's gateway doesn't come up after an update | Rolled back for all, pinned in all (§4). |
| A sibling's config doesn't load during an update | It counts as `auto` unset (on), as for the instance itself in M36. |
| `remove` of a running instance | Stopped first; its data stays unless `--purge`. |
| A named system instance's user exists and can log in | Refused, as for `ferrule` today. |

## 11. Out of scope

- A Windows service (none exists for the default either).
- Moving an existing default instance to a name, or renaming an instance.
- Sharing one bot between instances (Telegram allows one poller).
- A multi-instance dashboard: each instance has its own.
- Deleting system users.

---

## As built

Built as designed, in four commits (the name and paths; updates per
binary; the dashboard and the relay; `install.sh`). Where it departs:

- **§5 `list` shows the bot id only.** The gateway doesn't record its
  bot's `@username` anywhere another process can read it, so the column is
  `bot id <n>` (or `none`). The dashboard port is the one a running
  gateway wrote to `<data>/gateway/dashboard.json`, else the configured
  fixed port.
- **§5 `new` without a terminal.** `--no-setup`, or no terminal on stdin,
  writes the config file (the setup header only) and prints
  `ferrule --instance <name> setup [--system|--user]` instead of starting
  the setup. That's how the tests make instances.
- **§5 `remove --purge` refuses a workspace inside the instance's dirs.**
  "Never delete a workspace" can't hold if the workspace is inside the
  data dir, so `--purge` stops and says to move it first. Without
  `--purge`, and for a workspace elsewhere, nothing changes.
- **§6 setup checks at the end, not per value.** The collision check runs
  once, when setup finishes, against the config it saved, and prints each
  clash as a warning. Checking at each prompt would mean threading the
  other instances through every step for the same answer a few seconds
  later.
- **§8 is a menu item, not a first question.** Setup on the default
  instance, once it's set up, has an **Another instance** item in its menu.
  It asks for a name, validates it, and runs `instances new` with the
  same scope. A first setup doesn't show it, and neither does a named
  instance's setup.
- **§10's `FERRULE_CONFIG` note was dropped.** The units pin
  `FERRULE_CONFIG`, and `--config` sets it, so "the config isn't the
  instance's own" would fire on every service-run doctor. The variable
  still wins, silently, as it always has.
- **§10, a typo in `--instance`.** A named instance with no config says
  `no config for the instance `<name>``, with its setup command and
  `ferrule instances list`. The default's message is unchanged.
- **§4, an instance with no service of its own.** When the updater runs
  for an instance that has no service but has siblings that do (a
  terminal `ferrule update`), the siblings are restarted and health-checked
  and the instance itself reports `restart_needed`.
- **§3, `install.sh` as root.** It looks at the system units
  (`/etc/systemd/system/ferrule.service` and `ferrule@*.service`) when any
  exists, otherwise at root's user units, which is the old rule extended
  to named units. The shell test covers the user and macOS paths; the
  system path is the same loop with another directory and no `--user`.
  `install.ps1` is unchanged: Windows has no service, and its "a gateway
  is still running the old version" line already covers every instance.
- **§7, the header.** The name is a small outlined label after the
  version (`#inst`, hidden for the default), set from `/api/health`.

**Tests.** Every derived path, unit text and label for the default is
compared with 0.8.0's (`units-0.8.0/` fixtures); names; `clashes()` over
each shared thing and the unknowns; instance discovery by config and by
unit; the coordinated update (siblings restart together, one that doesn't
come up rolls back and pins all, a sibling's pin, busy gateway, lock or
`auto = false` holds it); `instances list/new/remove` and `--instance` /
`FERRULE_INSTANCE` through the real binary against a temp home; a shared
bot as a doctor failure; a named instance's page and console; and
`install.sh` against stub `curl`, `uname`, `id`, `systemctl` and
`launchctl` (Unix only).

**Not verified live.** No real second service was installed: the
container has no systemd user session or launchd. The system-instance
path (`useradd ferrule-<name>`, the unit, the update units as root) and a
coordinated update across two real services are tested against fakes
only.
