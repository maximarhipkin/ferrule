# Several agents on one machine

One Ferrule install can run several independent agents. Each one has its
own config, data, secrets, memory, bot, background service and dashboard.
There are two ways to do it:

- **Named instances** (from 0.9): one Unix account, one `ferrule` binary,
  one agent per name. This is the easy way, and what this guide is mostly
  about.
- **Separate Unix users** (or containers): each user installs and sets up
  Ferrule as usual. This was the only way before 0.9 and it's still the
  one to pick when the agents must not be able to read each other's
  files (see [Isolation](#isolation-which-way-to-pick)).

Either way, **one Telegram bot per agent**. Telegram lets one process poll
a bot. Two agents on one bot token both get HTTP 409 and each sees about
half the messages, so the bot looks broken to everyone. Make a second bot
with [@BotFather](https://t.me/BotFather) for the second agent. `ferrule
doctor` and `ferrule setup` catch a shared token and name the other
instance.

The design, with the reasons, is [m38-instances.md](m38-instances.md).

## Named instances

```sh
ferrule instances new work          # makes the instance and runs its setup
ferrule --instance work doctor      # any command, for that instance
FERRULE_INSTANCE=work ferrule status
ferrule instances list
ferrule instances remove work       # stops its service, keeps its files
```

The agent you already have is the **default instance**. It has no name,
and nothing about it changes: its paths, service, units and hints are the
same as before 0.9. `ferrule setup` on a machine that has it set up offers
**Another instance** in its menu, which is the same as `instances new`.

### Names

`--instance <name>` works on every command, before or after the
subcommand, and sets `FERRULE_INSTANCE`, so anything it starts inherits it.
A name is 1–24 characters of lower-case letters, digits and `-`, starting
and ending with a letter or digit. `default` isn't a name: leave
`--instance` out for the default. An invalid name stops the command before
it does anything, and a name with no config yet says so (with the setup
command for it) instead of falling back to the default.

`FERRULE_CONFIG` and `FERRULE_DATA_DIR` still win over the instance's own
paths when they are set.

### What each instance gets

The default's `ferrule` becomes `ferrule-<name>`, always as a sibling
directory, never inside the default's:

| | default | `work` |
|---|---|---|
| config (user) | `~/.config/ferrule/config.toml` | `~/.config/ferrule-work/config.toml` |
| data and secrets (user) | `~/.local/share/ferrule` | `~/.local/share/ferrule-work` |
| config (system) | `/etc/ferrule/config.toml` | `/etc/ferrule-work/config.toml` |
| home (system) | `/var/lib/ferrule` | `/var/lib/ferrule-work` |
| system user | `ferrule` | `ferrule-work` |
| systemd unit | `ferrule.service` | `ferrule@work.service` |
| update units | `ferrule-update.{service,timer,path}` | `ferrule-update@work.{service,timer,path}` |
| launchd labels | `ai.ferrule.gateway`, `ai.ferrule.update` | `ai.ferrule.gateway.work`, `ai.ferrule.update.work` |
| relay Worker | `ferrule-relay` | `ferrule-relay-work` |

The user paths are Linux's. macOS uses `~/Library/Application Support`
and Windows uses `%APPDATA%` / `%LOCALAPPDATA%`, with the same
`ferrule-<name>` rule. A named instance never reads a `./ferrule.toml` in
the current directory: `ferrule --instance work status` describes `work`
wherever you run it.

On Windows there's no background service yet, for any instance, so a
named instance there is its own config and data only.

### The commands

**`ferrule instances list [--json]`** shows each instance: its config,
its data dir, its service (`running`, `stopped`, `not installed`, as a
`user` or `system` service), its dashboard port (the running gateway's,
else the configured one) and its bot (the bot's
id, which is the part of the token before the `:` and isn't secret). The
one this command runs as is marked `*`. As root on Linux it also lists
the system instances.

**`ferrule instances new <name> [--system | --user] [--no-setup]`** makes
the config file, then runs `ferrule --instance <name> setup` in this
terminal. The setup doesn't inherit the current instance's keys or
paths, so you can't end up with a second agent on the first one's bot by
accident. `--no-setup`, or no terminal, stops after the file and prints
the setup command to run. `--system` (as root, on Linux) makes a system
instance, run by its own user `ferrule-<name>`.

**`ferrule instances remove <name> [--purge] [--yes] [--system | --user]`**
stops and removes the instance's service and its update units. Its config
and data stay where they are, and it tells you where. `--purge` deletes
them too, after you type the name (or with `--yes`). It deletes only that
instance's own config dir and data dir. It never deletes a workspace
outside them (and refuses `--purge` when the workspace is inside one, so
you move it first), and it never deletes a system user: it prints the
`userdel ferrule-<name>` line for you. The default can't be removed this
way: `ferrule setup` → service → remove, as before.

### Two agents mustn't share these

`ferrule doctor` checks every instance it can read against the one it runs
as, and `ferrule setup` checks when it finishes:

| shared | result |
|---|---|
| the Telegram bot (the same bot id) | **fail**, naming the other instance. Make another bot. |
| the dashboard's fixed port | **fail**: the second gateway can't bind it. `port = 0` (any free port, the default) never clashes. |
| the workspace directory | **fail**: two agents editing one tree, each with its own memory of it. |
| the SSH workspace (`host:port/path`) | **fail**, the same thing remotely. |
| the relay URL with different keys | **fail**: a relay has one key, so one agent's sign-ins are refused. |
| the relay URL with the same key | **note**: it works, but redeploying it from one instance re-keys it and breaks the other. |

A token is compared by its bot id and shown only as that id. A system
instance's secrets belong to its own user. A doctor that can't read them
says which instances it couldn't check, rather than guessing.

### Updates

Each instance has its own update units, so each one's `[update]` settings
and its dashboard's "update" button work as before. When several
instances in the same scope run **the same binary**, an update of that
binary is done for all of them together:

- It takes the update lock in every one of them. If another instance's
  updater holds one, this run stops.
- A version pinned in any of them (after a rollback) is pinned for all.
- `[update] auto = false` in any of them turns the others' automatic runs
  into checks, and an update asked for from a dashboard is refused with a
  message that names the instance that holds it.
- It waits until every one of their gateways is idle, installs once,
  restarts them all and checks each one comes up healthy.
- If any doesn't come up, the old binary comes back for all of them, they
  all restart again, and the version is pinned in each.

So one instance's updater never quietly upgrades an instance that is
pinned or has updates off. `ferrule update` in a terminal names the other
instances it will restart and asks first (`--yes` answers). Instances that
run different copies of the binary update on their own.

The install one-liner (`install.sh`), run again to upgrade, refreshes and
restarts every running instance whose service runs the binary it just
replaced, the default and each named one.

### The dashboard

Each instance has its own dashboard, on its own link from its own bot.
A named instance's page shows the name beside the version in the header,
and the browser tab reads `ferrule · <name>`. The console, the fix
buttons and `connections setup` on that page act on that instance, and
its restart button restarts only that instance's gateway. The console
refuses `--instance`, `instances new` and `instances remove`, so a page
can't reach another agent.

## Separate Unix users

Before 0.9, and still when the agents mustn't read each other's files:
give each agent its own account and install Ferrule in each.

```sh
sudo useradd -m agent2
sudo -iu agent2
curl -fsSL https://raw.githubusercontent.com/maximarhipkin/ferrule/main/install.sh | sh
```

Each user gets the usual default instance, with its own user service, its
own `~/.local/share/ferrule`, its own binary in `~/.local/bin`, and its own
updates. Setup turns on lingering for that account, so its user service
runs with nobody logged in (if it can't, it prints the `loginctl` line to
run). Nothing in one account can see the other. The collision checks above
don't cross accounts, so keep the two bots, dashboard ports and relays
apart yourself.

A container per agent works the same way.

## Isolation: which way to pick

- **User instances** (`--user`, the default when you aren't root) share
  one Unix account. They don't share config, keys or memory, but an
  agent's shell commands run as that account and could read the other
  instance's files. The sandbox limits what a command touches, but the
  account boundary is what really separates them. Fine for two agents of
  your own.
- **System instances** (`--system`, as root on Linux) each run as their
  own user (`ferrule-<name>`), which owns only its home, data and
  workspace. One agent's commands can't read another's secrets or memory.
- **Separate Unix users or containers**, when the agents belong to
  different people or must stay apart for some other reason.
