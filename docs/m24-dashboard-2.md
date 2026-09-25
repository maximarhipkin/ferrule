# M24: the dashboard's leftovers (design)

Status: in progress, 2026-09-25, branch `m24-dashboard-2` (PR to main, not
merged). Follows M22 ([m22-dashboard.md](m22-dashboard.md)), which left
four things out: a login that survives a restart, evaluating a candidate
model, editing settings from the page, and a smoke script the owner runs
on their own machine. The user guide is [dashboard.md](dashboard.md).
Where the build departs from this design: **As built** at the end.

## 1. The login survives a restart

### What persists

M22 kept sessions in memory, so every gateway restart (a deploy, the
watchdog, an upgrade) logged the phone out. M24 writes them next to the
links, in `<data>/private/dashboard/sessions.json`, through the same
`seal::write_private` (mode 0600 on Unix) under the same `filewrite::Lock`
as `links.json`.

A row is `{hash, host, opened_ms, last_ms}`:

- **`hash`** is the SHA-256 of the cookie. The raw cookie is never
  written anywhere. Someone who can read the file still can't sign in,
  the same way `links.json` works since M22.
- **The CSRF token isn't stored at all.** It's derived: `SHA-256("csrf:"
  + cookie)`. The server works it out again from the cookie on every
  request. Only the holder of the cookie can compute it, and the cookie
  is HttpOnly, so page script never sees it. One secret, not two, and
  nothing new in the file. (M22 drew the CSRF token at random. Derived is
  as strong, because the cookie is 32 random bytes.)
- **Times are wall-clock milliseconds**, where M22 used `Instant`, which
  means nothing in a new process. Idle is `now - last_ms`, and the
  absolute limit is `now - opened_ms`. The limits are the same: 30
  minutes idle, 12 hours absolute. A clock that jumps back counts as
  "just used", but the absolute limit still ends the session.
- **`last_ms` is written at most once a minute per session.** The idle
  timer is 30 minutes, so a crash can shorten a session by up to a
  minute. It never lengthens one. Writing on every 3-second poll would
  mean a file write per request for no gain.

### Revocation holds across the restart

- `/dashboard off` and `ferrule dashboard off` (plus the new alias
  `ferrule dashboard revoke`, the name the brief uses) empty
  `sessions.json` and set `revoked_ms` in `links.json`, as M22 did. The
  file is emptied before the call returns, so no later start can load a
  revoked session.
- A row opened before `revoked_ms` is also refused, and dropped, when it's
  read. `off` from the CLI while the gateway is down therefore still
  wins, even if the gateway kept an old copy in memory, because every
  check compares against `revoked_ms`.
- The gateway re-reads the file when it changes (same mtime-and-length
  poll as the config follower). `ferrule dashboard off` from another
  process ends sessions in the running gateway within one request.

### The one-time link stays one-time

That was already true: `consume` removes the hash from `links.json`
under the lock before it answers. M24 adds a test that uses a link, starts
a "new process" on the same data dir, and gets a refusal for the same
link.

### Local sessions and the port

A cookie ignores the port. M22 bound a session to the full `Host`
(`127.0.0.1:43127`) and `port = 0` picked a new port on every start, so a
local session couldn't survive even with the file. M24 changes two
things:

- **Loopback sessions are bound to "loopback"**, not to one port. The
  page still refuses any `Host` outside its allow-list (M22's DNS-rebinding
  check). The allow-list lets through only ours, on whatever port we
  serve.
- **With `port = 0`, the gateway first tries the last port it used**
  (kept in the gateway marker `dashboard.json`), then any free one. An
  `ssh -L` command the owner saved keeps working after a restart.

### The quick tunnel after a restart

A quick tunnel dies with the process, and the next one gets a new
`….trycloudflare.com` name. A cookie is scoped to its host name, so a
tunnel session **can't** survive a restart: the browser will never send
it to the new name. The choices were:

1. Say nothing. The owner taps the old tab, gets a Cloudflare error, and
   has to know to send `/dashboard` again.
2. Always open a tunnel at start and send a link. That opens a public
   door nobody asked for, after every restart.
3. **Only if a tunnel session was live when the process stopped**: open a
   new tunnel at start and send the owner a new one-time link, with the
   reason ("The gateway restarted, so the dashboard has a new address").
   ← chosen

The third sends a link only when someone was actually using the page
remotely, which is the case where the page just broke under them. It's
the same message and the same one-time, 10-minute link as `/dashboard`,
sent to the owner's private chat. It adds nothing that isn't already
available by sending `/dashboard`. Guards:

- "Live" means unexpired and not revoked at startup: not idle for 30
  minutes, not past 12 hours, not opened before `revoked_ms`.
- Once the new link is sent, the dead tunnel-host rows are dropped. They
  can never be used again, and keeping them would only keep a stale
  credential on disk.
- At most one automatic link every 10 minutes (`auto_link_ms` in
  `sessions.json`), so a crash loop can't flood the chat.
- `remote = "off"`, or no `cloudflared`: no tunnel and no message. The
  local sessions carry on as above.
- The owner chat unknown: nothing is sent, and the log says why.

## 2. Evaluate a candidate

"Would this model do my work as well as the one I have?" M14's harness
already answers that. M24 lets the owner run it on a candidate from the
Models and Catalog views, and from the CLI:

```
ferrule model eval <model> [--suite smoke|starter] [--yes]
```

### Where the suite comes from

The starter suite is a directory (tasks, fixtures, Python graders), not
part of the binary. The dashboard and `model eval` look for it in this
order:

1. `[eval] suite = "<dir>"` in the config.
2. `$FERRULE_EVAL_SUITE`.
3. `./evals/starter`, from the working directory.
4. The checkout the binary was built from (a compile-time path: right for
   `cargo install --path` and `target/debug/ferrule`).

If none is found, the page says so and names the setting. It doesn't
guess. `--suite smoke` is the suite's `smoke` tag (4 tasks). `starter` is
all 20.

### The cost shown first

The estimate is **catalog prices × the suite's typical token use**. The
typical use is measured by running the stdlib mock model (engineered
variant) on each task. It's recorded per task as input, cached-input and
output tokens in `crates/ferrule-cli/src/model_eval/typical.json`. A test
re-measures it whenever `python3` is present and fails if the table has
drifted, so the table can't quietly go stale. The suite, its graders and
the mock stay untouched.

The prices come from the configured prices when set, else from the
catalog (M21), the same lookup as `ferrule eval`. With no prices, the
page shows the token estimate and says the dollar cap can't see this
model's spend. The page labels the number an **estimate**: a real model
uses more turns than the mock. The hard bound is the cap, and the page
shows the cap next to the estimate.

### Confirm, and the M19 caps

Running it spends money, so it takes a confirm on the page (`confirm:
true`, a 409 until then, like every other paid or destructive
operation) and `--yes` on the CLI. The CLI asks at a terminal, and
without a terminal it refuses unless `--yes` is given.

The candidate eval is the owner's spend, unlike a plain `ferrule eval`,
so it obeys M19:

- **It refuses to start** while the kill switch is on, or when the day's
  $ or token cap is already used up.
- **Its budget** (the harness's own `Caps`) is the lesser of the per-run
  cap (`max_usd_per_run`, `max_tokens_per_run`) and what's left of the
  day's caps. The harness stops the suite when it's spent. The run is
  recorded as "stopped by the budget", the same as `ferrule eval`'s exit 3.
- **Its rows count toward the owner's day.** Each ledger row is stamped
  with the tree `eval:<run id>`. That is what the M19 meter counts, and
  it's how an `owner_trust` suite is charged, so the gateway's later
  turns see the spend.
- **The kill switch mid-run** (`/stop`, the page's "Stop everything",
  `ferrule stop`) cancels the eval within a second.

The harness itself doesn't change. Everything above sits in the `Env` and
`Options` that `ferrule eval` already hands it.

### In the background, with progress

A dashboard eval runs on **its own thread with its own Tokio runtime**.
The graders and the agent loop can't stall the gateway's runtime, its
turns or the page. One eval at a time per process: a second request gets
409 with the running one's progress. Progress is the harness's own
progress lines (`▶ task`, `✓ task: pass (…)`) plus done-out-of-planned.
The page polls `GET /api/eval` every 3 s while one runs. **Cancel** is a
confirmed POST.

### Where the result goes

The result is stored exactly as `ferrule eval run` stores one:

- the calls and the verdict rows go to the ledger with `task_shape =
  "eval"`;
- `report.txt`, `run.json` and the transcripts go under
  `<data>/eval/<run id>/`.

`ferrule eval report starter` prints it, and `history::diffs` compares it
with later runs.

The page shows it next to **the current default's last result on the
same suite and subset**: the latest saved run of the same suite, the same
task set (smoke or all) and the default's model, engineered variant.
When there's none, the page says so and offers to evaluate the default
too.

### Hermeticity

An eval never starts the dashboard or a tunnel. `ferrule model eval` goes
through `model_eval`, which never touches `dashboard::`. M22's test that
runs `ferrule eval` and then checks no dashboard marker or private dir
exists is extended to `ferrule model eval`.

## 3. Edit from the page

Same shape as M21's Models API (`models/admin.rs`): **a read model plus
mutating operations** in one module, `settings_admin.rs`. It's shared by
the page, the CLI and Telegram. Every operation:

- takes the config's `filewrite::Lock`, edits through `toml_edit` (so the
  owner's comments and layout survive), validates the result by parsing
  it as a `Config`, and saves with an atomic rename. A change that
  wouldn't parse is refused and nothing is written;
- is audited (`settings.caps`, `settings.mcp`, `settings.skill`,
  `hooks.trust`, `task.schedule`, `model.task`), with `by` set to
  `dashboard`, `cli` or `telegram`;
- returns `{said, view}`, as M21 does, so the page re-renders from the
  answer.

On the page, each operation also needs the CSRF header, the JSON content
type, the Origin check and, where marked, `confirm: true`.

| Operation | Confirm | Takes effect |
|---|---|---|
| Caps: set any of per-run, per-day, per-task $ and tokens | when it **raises** a cap or turns one off (0) | at once in the gateway (the hub's caps are now live), and in other processes at their next config read |
| MCP: disable, enable a configured server | disable | the config follower stops or starts it within seconds (M17) |
| MCP: remove | yes | the same as `ferrule mcp remove`; secrets stay |
| Skills: disable, enable | no | the next turn (the skill set is rebuilt per turn) |
| Hooks: trust the workspace file | yes, and the hash must match | the next hook event |
| Hooks: untrust | no | the next hook event |
| Tasks: edit schedule (cron or once) and timezone | no | the scheduler's next tick; the next run is recomputed |
| Tasks: set model (or back to default) | no | its next run |

Lowering a cap is always safe, so it has no confirm. Raising one is a
money decision, so it has one.

**MCP disable** is a new `[mcp] disabled = ["name", …]` list, filtered out
in `mcp_servers()`. The config follower already re-applies that list, so
disable and enable take effect without a restart. It covers the
configured servers. The agent-installed ones (M13) can be **removed**
from the page, but disabling them is left out: their lock file has only
Active or Suspended, and Suspended means "the scan flagged it". Reusing
it for "the owner turned it off" would blur what the scan said. Adding a
server stays with `ferrule mcp add` (M17) and the M13 flow, because both
run the scan.

**Hooks trust is pinned to the hash.** The page shows:

- the workspace's `.ferrule/hooks.toml`, redacted;
- its SHA-256;
- the hash it was trusted at, if any;
- a line diff against the copy kept when it was last trusted through
  M24. The trust store records only the hash, so the text is kept in
  `<data>/private/hooks-trusted/<sha>.toml`, 0600.

The trust request carries the SHA-256 the owner saw. The operation
compares it with the file on disk, trusts it, and reads the pinned hash
back. If either check differs, it untrusts and refuses: "the file changed
while you were reading it". That is `ferrule hooks trust`'s own check, now
shared. The CLI keeps its terminal prompt.

**Tasks:** a schedule edit is checked by
`ferrule_gateway::initial_next_run_at`, the parser `ferrule tasks add`
uses. A bad cron line or timezone is refused and nothing is written. Built-in tasks can be edited
but, as in M22, not deleted.

### The CLI and Telegram

- `ferrule trust caps [--set KEY=VALUE…] [--yes]`: raising a cap needs
  `--yes` or a terminal "y".
- `ferrule mcp disable|enable <name>`
- `ferrule skills disable|enable <name>`
- `ferrule hooks trust` (unchanged prompt, now through the shared op)
- `ferrule tasks schedule <id> <schedule> [--tz Z]`
- `ferrule tasks model` (M21, now through the shared op)

Telegram gets one owner-only door for the same operations:

- `/caps`, and `/caps <key> <value>`. Raising a cap needs `confirm` as the
  last word.
- `/mcp`, and `/mcp on|off <name>`
- `/skills`, and `/skills on|off <name>`

Hooks trust stays on the page and the CLI: it means reading a file and a
hash, which a chat message does poorly. The command answers with where
to do it.

## 4. The smoke script

`scripts/dashboard-smoke.sh` (bash) and `scripts/dashboard-smoke.ps1`
(PowerShell) take about 2 minutes on the owner's machine. Each step
prints PASS, FAIL or SKIP, and the script exits non-zero on any FAIL:

1. **Build**: build `ferrule`, or use `$FERRULE_BIN`.
2. **Mock model**: start the starter suite's mock model (stdlib Python).
3. **Mock Telegram**: start a fake Telegram Bot API (stdlib Python, in the
   script) that sends `/dashboard` from the owner's chat and records the
   bot's replies.
4. **Temp config**: write a config pointing at both mocks, with the data
   dir under a temp dir.
5. **Gateway**: start `ferrule gateway` and wait for the marker.
6. **Link**: `/dashboard` comes back as a link, with a tunnel link if
   `cloudflared` is installed, else a local one.
7. **Login**: load the page and log in with the link. Check the cookie
   and CSRF, and GET `/api/health`.
8. **Tunnel**: if `cloudflared` is on PATH, fetch the page through the
   `trycloudflare.com` link. Otherwise SKIP with "install cloudflared to
   test the tunnel".
9. **Catalog**: fetch the live OpenRouter catalog once
   (`ferrule model catalog --json`) and check it parses and has models.
   This is the only network call besides the tunnel.
10. **Clean up**: `/dashboard off`, stop everything, remove the temp dir.

The script needs no API key and no bot token: the model and Telegram are
mocks. It never touches the owner's real config or data dir.

**RTL test:** the page is served with user content (chat names, message
previews, task names, log lines) in elements carrying `dir="auto"`. That
makes Hebrew display right-to-left inside a left-to-right page. A test
serves the page with a Hebrew chat name and message, and asserts two
things: the HTML templates in `app.js` put `dir="auto"` on every
user-content element, and the Hebrew text arrives through the API
unchanged.

## Threat model (what M24 adds)

- **The sessions file is read.** It holds only hashes of 32-byte random
  cookies, so it's useless for signing in (the same as `links.json`). It's
  0600 and lives under `private/`.
- **The sessions file is written** by someone else on the machine. They
  could add a hash of a cookie they chose. That needs write access to the
  owner's data dir, which already gives them the config, the keys and the
  hooks. Out of scope, as in M22.
- **A stolen cookie now outlives a restart.** It's bounded by the same
  12 hours and 30 minutes idle, and killed by `/dashboard off`. That is
  the intended trade.
- **The auto link after a restart** goes only to the owner's private
  chat, is one-time and lasts 10 minutes, the same as `/dashboard`. An
  attacker who can make the gateway restart gains a link sent to the
  owner, not to themselves.
- **A paid eval triggered by CSRF**: blocked by the CSRF header, the
  Origin check and the confirm. It's also bounded by the caps.
- **Hooks trust through the page** runs code as the owner on the next
  hook. It needs a session, CSRF, the confirm, and the exact hash the
  page showed. A file changed after it was shown is refused.
- **Secrets**: every new response passes through `redact_value`, as in
  M22. The hooks file text is redacted before it's shown. The secret-scan
  test covers every new endpoint.

## Failure modes

- A corrupt `sessions.json` is read as empty, which logs everyone out,
  and the file is rewritten. It never locks the owner out: `/dashboard`
  still works.
- A crash between trusting and reading the hash back leaves the file
  trusted at the hash on disk, which may not be the hash the owner saw.
  The check runs right after, in the same call, so the window is tiny.
  The worst case is the pre-M24 CLI's worst case.
- An eval's thread panics: the job shows "failed" with the panic text,
  redacted. The gateway is unaffected.
- The gateway restarts mid-eval: the eval dies with it. Rows already
  written stay in the ledger and count toward the day. There's no
  `run.json`, so the page shows no result. That isn't worth a resume
  (it's a few dollars at most, under the caps).
- A config edit races a hand edit: the lock serializes our writers, and
  a hand edit between our read and write is lost only if it lands in
  that window. That's the same as M21.

## Out of scope

- Adding MCP servers or skills from the page (it needs the M13/M17 scan
  flow).
- Disabling agent-installed MCP servers (see §3).
- Editing hooks. The page trusts or untrusts the file. Editing it stays
  in the editor.
- A persistent tunnel name (a named Cloudflare tunnel needs an account).
- Resuming an eval after a restart.
- Transcripts, as in M22.

## As built

(Filled in at the end.)
