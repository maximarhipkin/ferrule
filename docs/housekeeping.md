# Housekeeping: what an always-on bot prunes

An always-on gateway keeps making files and log lines that nothing used to
remove. Since wave A (2026-10-21) a daily pass prunes them by age, then by
size. It looks at exactly the places below and nothing else: **never a
transcript, memory, the ledger, the config or a key.**

## What is pruned

| Name | Where | What it holds | Rule |
|---|---|---|---|
| `spill` | `<workspace>/.ferrule/spill/` | long tool output saved whole (M55.3) and background jobs' logs (M57.6) | files older than `keep_days`, then the oldest past `max_mb` |
| `mcp-content` | `<workspace>/.ferrule/mcp-content/` | images and files MCP servers returned (the browser's screenshots) | same |
| `screen` | `<workspace>/media/screen/` | M61's screenshots and recordings | same |
| `reactions` | `<data>/learn/reactions/` | M53.5's transcript cuts kept for a 👎 | same |
| `thumbs` | `<workspace>/.ferrule/thumbs/` | the Files page's video posters (M65, made again when asked for) | same |
| `trash` | `<workspace>/.ferrule/trash/` | what the Files page deleted (M65) | files older than 7 days, whatever `keep_days` says |
| `audit` | `<data>/trust/audit.jsonl` | the trust audit log | `egress` and `egress_denied` lines older than `keep_days`; every other event older than `audit_keep_days` |

`<workspace>` is the gateway's workspace (for `ferrule housekeeping run`,
`--workspace`, default the current directory); `<data>` is ferrule's data
directory. A file's age is its last-modified time. The code is
`crates/ferrule-cli/src/housekeeping.rs` (`targets` lists the places).

Not pruned, on purpose: `sessions/` (transcripts), `memory.db`, the
learning loop's playbook, lessons and `reactions.jsonl`, `ledger.jsonl`
(the spend caps read it), `media/` outside `media/screen/` (what the owner
asked to be made), `media.db`, `tasks.db`, the config, `private/` and the
secrets file. A sub-agent's worktree goes when the agent is closed, with
its own `.ferrule/`.

## Settings

```toml
[housekeeping]
enabled = true          # the daily pass in the gateway (04:20 UTC)
keep_days = 14          # files and egress lines; 0 keeps them all
audit_keep_days = 90    # the audit log's other events; 0 keeps them all
max_mb = 500            # per directory, after the age rule; 0 = no size cap
```

The daily pass is the built-in task `ferrule-housekeeping` (it shows in
`ferrule tasks list`); it reads the settings each time it runs, and
`enabled = false` removes the task at the next start. Its one-line result
is the run's detail.

## By hand

```bash
ferrule housekeeping run --dry-run     # what would go, per place
ferrule housekeeping run               # prune now, whatever `enabled` says
ferrule doctor                         # the `housekeeping` line: what each place holds
```

## How it deletes

- **No links.** A place is pruned only when every part of its path under
  the workspace or the data dir is a real directory, so a `.ferrule` or
  `spill` that is a link to somewhere else is left alone. Inside, links
  are neither followed nor removed; the walk goes four levels down at
  most. Empty directories under a place are removed; the place itself
  stays.
- **The audit log** is rewritten through a temporary file and a rename
  while holding `trust/audit.lock`, which every writer of the log (the
  trust hub, the proxy's egress lines) holds shared while it appends, so
  no line is lost in the swap. A line that doesn't parse is kept.
- **A running background job's log** older than `keep_days` (quiet that
  long) or past the size cap can be removed; on Linux and macOS the job
  keeps writing to the removed file, whose space comes back when the job
  ends, and `shell_job tail` says the log isn't the job's file any more.
