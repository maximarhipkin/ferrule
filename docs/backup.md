# Backup and restore

Everything ferrule knows about you lives in one data dir: conversations,
memory, scheduled tasks, the ledger, skills and plugin state. `ferrule
backup` puts it, with the config, in one `.tar.gz`. `ferrule restore` puts
it back, and moves what was there aside instead of deleting it.

```sh
ferrule backup                          # ferrule-backup-20260929-120919.tar.gz, here
ferrule backup -o ~/backups/ferrule.tar.gz
ferrule restore --dry-run ~/backups/ferrule.tar.gz
ferrule restore ~/backups/ferrule.tar.gz
```

For a named instance ([instances.md](instances.md)), put the global flag
first: `ferrule --instance work backup`. Its file is then called
`ferrule-work-backup-<time>.tar.gz`. The time in the name is UTC.

## What's in a backup

- **Databases** (`memory.db`, `tasks.db`, `agents.db` and any other SQLite
  file) are copied with SQLite's `VACUUM INTO`. The copy is consistent even
  while the gateway is writing, so a backup doesn't need it stopped.
- **Everything else in the data dir, as it is:** `sessions/` (every chat's
  transcript), `ledger.jsonl`, skills, plugin state, hooks, learning, trust,
  MCP, task plans and the gateway's state.
- **The config file** (`ferrule.toml`, or the instance's `config.toml`).
- **`manifest.json`,** first in the archive: the ferrule version, the
  instance, the UTC time, whether secrets are in it, and each file's size
  and sha256.

**Left out as caches:** `models/` (downloaded weights), `update/`, `bin/`,
`worktrees/`, `eval/` run output, `telemetry/`, `sandbox/`, the gateway's
running marker and status file, and SQLite's `-wal`/`-shm`/`-journal`
files. A restore doesn't need any of them. Models download again, or you
can move `models/` back from the old data (see below).

**Left out as secrets, unless you ask:** `private/` (`secrets.env`, the
subscription sign-ins, the dashboard's login links), `claude-code/`,
`ssh/`, the proxy's CA key (`proxy/keys/`) and the Matrix session.

```sh
ferrule backup --include-secrets
```

This adds them, prints a warning, and the manifest says `"secrets": true`.
A backup with your keys in it is as sensitive as this machine. Keep it
somewhere only you can read.

Every backup file is created readable by you only (mode 0600 on Linux and
macOS), with or without secrets: conversations and memory are private
too. The file is written as `….partial` and renamed when it's complete, so
a backup that was cut short never looks like a finished one. An existing
file is never overwritten.

## Restoring

`ferrule restore FILE` does this, in order:

1. **Refuses while this instance's gateway runs,** and says how to stop it:
   `systemctl --user stop ferrule` (or `sudo systemctl stop …` for a system
   service), `launchctl bootout gui/$(id -u)/…` on macOS, or the pid of a
   gateway running in a terminal.
2. **Checks the whole archive before anything moves.** Every file is
   checked against the manifest's size and sha256. A missing file, an extra
   file, a changed file, a path outside the data dir or a damaged archive is
   refused, and nothing changes. An archive from a newer ferrule is refused
   with "run `ferrule update` first". That means a newer major version, or
   a newer minor version while ferrule is 0.x.
3. **Moves the current data dir aside** to `<data>.pre-restore-<time>`,
   next to it. Nothing is deleted. The config file, if the backup has one,
   is moved to `<config>.pre-restore-<time>` the same way.
4. **Puts the backup in place.** The files are unpacked into a hidden
   folder next to the data dir while they're checked, and only then renamed
   into place. A restore that fails halfway leaves the old data where it
   was.

**Secrets on restore.** A backup without secrets doesn't take yours away.
The secret paths above are copied from the data that was moved aside into
the restored data, so the same keys keep working. On a new machine there
are none to copy, so run `ferrule setup` to set your keys again.

**`--dry-run`** does steps 1 and 2 (it reports a running gateway but
doesn't stop there), prints what a restore would move and where, and
changes nothing. Use it to check a backup file is intact.

After a restore, start the gateway again. When you're happy, you can
delete the `.pre-restore-*` folder, or move its `models/` back to skip
downloading the models again.

## Doctor

`ferrule doctor` has a `backup` line: how long ago the last backup was
made and where it went (recorded in `<data>/backup.json`), or that none
has been made yet. It's information, never a warning.

## Scheduled backups

Ferrule doesn't schedule backups itself yet. Its scheduled tasks run
prompts, not commands. A cron job or a systemd timer does it today:

```sh
# crontab -e: every night at 03:30, into ~/backups
30 3 * * * cd ~/backups && ferrule backup >> ~/backups/backup.log 2>&1
```

Old files aren't pruned for you. Delete the ones you don't need, e.g.
`find ~/backups -name 'ferrule-backup-*.tar.gz' -mtime +30 -delete`.
