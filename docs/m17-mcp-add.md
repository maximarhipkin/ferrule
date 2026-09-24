# M17 — MCP hot-add: `ferrule mcp add`

The owner adds an MCP server with one command. The command checks the server
live, scans its tools, binds its secrets to hosts, and writes the config. A
gateway that is already running starts offering the new tools on its next
message, with no restart. This builds on M13 (`docs/m13-self-extension.md`):
the extension manager, the description scan and the `list_changed` re-scan.
M13 §13 left `ExtensionManager::add_server` as "the M17 hook".

## 1. The command

```
ferrule mcp add <name> [-- <command> <args>…]   # stdio server
ferrule mcp add <name> --url https://host/mcp   # Streamable HTTP server
  --env KEY=VALUE            non-secret env for a stdio server (repeatable)
  --header K=V               a header for a URL server; `${VAR}` expands (repeatable)
  --secret NAME[=h1,h2]      bind a secret to hosts (repeatable); hosts default
                             to the URL's host for a URL server
  --enabled-tool T           offer only these tools; a trailing `*` is a prefix
  --max-output N             this server's cap on a tool result, in chars
  --output-cap TOOL=N        a cap for one tool
  --timeout SECS             per-call timeout
  --no-sandbox               run it unconfined (loud; doctor flags it)
  --waive                    accept tools the scan blocks (non-interactive)
  --skip-flagged             leave out the tools the scan blocks
  --replace                  replace an existing configured server of that name
  --yes                      no questions; fail where one would be needed
  --workspace DIR            the directory the smoke test runs the server in
ferrule mcp list             = ferrule extensions list, servers only
ferrule mcp remove <name>    configured: drop the [[mcp.servers]] entry;
                             installed: = ferrule extensions remove
```

If there is no command, no `--url` and a terminal is attached, the command
asks for each part: stdio or URL, the command line or URL, and headers and
secrets. With `--yes`, or with no terminal, nothing is asked. A missing
answer is then an error, and nothing gets written.

### Flow

1. **Validate.** Check the name (`[A-Za-z0-9_-]`, 1–64 chars) and that
   exactly one of command or URL is given. `--replace` is needed if the name
   is already in `[[mcp.servers]]`. The name also must not belong to an
   installed (lock) server or to the built-in `browser`.
2. **No plaintext keys.** Refuse an `--env` whose name looks secret (the
   sandbox's own secret markers: `TOKEN`, `KEY`, `SECRET`, …), and point the
   user to `--secret`. Also refuse a credential-bearing header (`Authorization`,
   `*-api-key`, `*-token`, `cookie`) whose value has no `${…}`. Interactively
   the command instead offers to move the value into a secret.
3. **Secrets.** For each `--secret NAME=hosts`, find the value in this
   order: the value already saved in the private secrets file, then the
   CLI's environment, then (interactive only) a hidden prompt. Nothing is
   saved yet.
4. **Smoke test.** Start the server exactly as the daemon will. That means
   the same `McpServerConfig`, the same sandbox policy and the same state
   dir. It gets a throwaway credential proxy that holds the existing
   secrets plus the new ones. The test runs `initialize` then `tools/list`,
   and stops the server afterwards. If it fails, the command prints the
   error and exits non-zero, and **nothing is written**.
5. **Scan.** Run M13's description scan on the listed tools, after
   `enabled_tools` has filtered them, since a tool that is never offered
   never reaches the model. Show the tools with a scan report. A **block**
   hit refuses the add unless the owner accepts it:
   - Interactively, typing `waive` keeps the flagged tools, and typing
     `skip` writes an `enabled_tools` list without them.
   - Non-interactively, `--waive` or `--skip-flagged` does the same.
   Warnings are shown and don't stop the add.
6. **Confirm.** The command asks "Add it?" unless `--yes` is given.
7. **Write.** The secret values go to the private secrets file (0600). Then
   `[secrets] NAME = [hosts]` and the new `[[mcp.servers]]` table go into the
   config through `toml_edit`. That keeps the file's comments and order and
   puts the new table after the last server. The edited document is re-parsed as a
   `Config` before it is written, and the write is atomic (temp file plus
   rename, as `ferrule setup` does).
8. **Doctor.** Re-run `ferrule doctor --offline`. Its MCP line now names the
   configured servers. Then print the new server's tools and "running
   gateways pick it up within a few seconds".

### One code path

- **The probe.** Starting the server, listing its tools and scanning them
  is `ExtensionManager::probe(cfg) -> Probe { infos, findings, blocked,
  sandbox_degraded }`. It uses the same `start_client` + `Surface::of` that
  `add_server` (configured servers) and `prepare_mcp` (the model's
  `mcp_add`, owner approvals) use. `mcp add` and the setup wizard's MCP step
  both call `mcp_add::run` / `mcp_add::guided` in the CLI, which call
  `probe`.
- **Persistence split (M13 §9).** Owner-added servers go in the config file,
  under `[[mcp.servers]]`, origin `configured`. The model's `mcp_add` still
  writes the lock and goes through the allow-list or the owner's approval.
  So `ferrule mcp add` is not a second install path for the model. It is the
  owner's hand-edit of the config, with the checks done for them.
- **`list` and `remove`.** `ferrule mcp list` is the `extensions list`
  printer, filtered to servers. `ferrule mcp remove` removes a configured
  entry through the same `toml_edit` writer and otherwise calls
  `extensions remove`. `ferrule extensions remove <configured name>` now
  does the same, instead of refusing.

## 2. How the running daemon learns about the new server

The options were a file watch, IPC, or a re-read per session. The choice is
the **simplest one that behaves the same on all three OSes: a poll.** A
"config follower" task in each process that runs agents (`gateway`, `chat`)
re-reads the config file every `SYNC_EVERY` (2 s). This is the same clock as
M13's lock sync. It uses the file's modification time and length as a cheap
check, and parses the file only when those change. A file watcher (inotify,
FSEvents, ReadDirectoryChangesW) behaves differently per OS around editors'
atomic renames. IPC would need a socket or named pipe per OS. Re-reading per
session is not enough, because the MCP servers are started once per process
and shared by every session.

When the file does change:

1. **Secrets first.** A `[secrets]` entry that is new, or whose hosts
   changed, is bound into the running credential proxy by
   `Broker::bind(name, rule, value)`, which is new. The value comes from
   the process environment first, then the secrets file. The file is read
   directly, never through `set_var`, which is not thread-safe. If the
   process started with no proxy (no `[secrets]` had a value), the follower
   starts one at this point. The manager's sandbox is then replaced with one
   that carries the proxy's current env (placeholders, `HTTPS_PROXY`, CA
   bundle) and egress, so the servers started from then on see it.
   Replacing a value adds a new placeholder but keeps the old one working:
   servers already running still hold the old one. Removing a secret takes
   effect at the next restart.
2. **Then servers.** The follower computes `[[mcp.servers]]`, plus the
   browser server, just as startup does, and calls
   `ExtensionManager::set_configured(list)`. This new reconciler diffs the
   list against the last list it applied, not against what is live:
   - a server with a new name is started through `add_server`
   - a server that was removed is stopped
   - a server whose config changed is stopped and started again
   A server that failed to start is not retried every tick. It is retried
   only when its entry changes.
3. Sessions already take the manager as a live `ToolSource`, which is
   re-queried on every provider request. So the next message, in a new
   session or one already running, sees the new tools.

**Trust.** The follower only follows a config that may carry the allow-list:
`--config` / `$FERRULE_CONFIG`, or the global config. A `./ferrule.toml` in
the working directory may be inside the agent's own writable workspace. If
the agent could write a server into it and have it start live, that would be
a self-extension path around M13's approval gate. For such a file the
follower logs once that MCP changes there need a restart, which is the
behaviour before M17.

What still needs a restart: `[sandbox]` changes, removing a secret, and
anything outside `[[mcp.servers]]` / `[secrets]` / `[browser]`. For shell
commands and the model's system-prompt note on placeholders, a newly bound
secret also waits for the next restart. The note is built per session from
the proxy, but the shell's sandbox is the process's first one. This is
listed as an open edge.

## 3. Filters and caps

New `[[mcp.servers]]` keys:

```toml
[[mcp.servers]]
name = "github"
url = "https://api.githubcopilot.com/mcp/"
headers = { Authorization = "Bearer ${GITHUB_TOKEN}" }
enabled_tools = ["search_*", "get_issue"]   # only these are offered
max_output_chars = 8000                     # this server's results
output_caps = { get_issue = 2000 }          # one tool's results
```

- **`enabled_tools`.** Empty means every tool. An entry is an exact name,
  or a prefix ending in `*`. The filter is applied in the manager before the
  scan, so a hidden tool is never scanned, offered or re-activated. It is
  applied again in `ferrule_mcp::build_tools` as a backstop. A tool that
  appears mid-session through `list_changed` is filtered the same way.
- **Caps.** A tool's result is capped at the smallest of the session cap
  (`ToolContext::max_output_chars`), `output_caps[tool]` and
  `max_output_chars`. A per-server cap can only make results smaller. It can
  never let a result past the session's own budget.

## 4. Removal

- `ferrule mcp remove <name>`, for a configured server, deletes that
  `[[mcp.servers]]` table through `toml_edit`. The rest of the file keeps
  its comments. Running processes stop the server within a few seconds
  (§2).
- `[secrets]` entries and saved values are kept, since other things may use
  them. The command lists which ones only this server referenced, with the
  line that removes each one (`ferrule setup` → Tool credentials).
- `--purge` also deletes the server's state dir.
- An installed (lock) server goes to the M13 path unchanged.

## 5. Failure modes

| What | Result |
|---|---|
| The server doesn't start, or `initialize`/`tools/list` fails or times out | error with the server's message, exit 1, **nothing written** (no config, no secret) |
| The scan blocks a tool | refused unless `waive`/`--waive` or `skip`/`--skip-flagged`; nothing written on refusal |
| `enabled_tools` matches nothing | refused: "none of the server's tools match", listing the tools |
| Config unparseable or unwritable | error naming the path, nothing written, secrets not saved (they are saved only after the config write would succeed, see below) |
| The name is taken | refused without `--replace`; an installed/browser name is always refused |
| A plaintext credential in `--env`/`--header` | refused, pointing to `--secret` |
| The secrets file can't be written | error, the config is not written |
| The daemon can't start the server later (e.g. a secret missing in its env) | the follower logs a warning, and `doctor` shows it; other servers are unaffected |

Order of writes: the config text is fully prepared and validated first. Then
the secret values are saved. Then the config is renamed into place. If the
rename fails, the values just saved are removed again.

## 6. Setup wizard

`ferrule setup` gets an "MCP servers" part, as the last guided step and as a
menu item. It lists the configured servers and offers "Add a server" (the
same `mcp_add` guided flow, so the same probe, scan and writer) and "Remove"
(the same remover).

## 7. Tests (hermetic)

- `toml_edit`: adding and removing a server keeps comments, key order and
  other tables. This is a unit test in `mcp_config.rs`.
- `enabled_tools` hides tools, and a per-tool cap truncates: unit tests in
  `ferrule-mcp` and a manager test with the python fixture.
- `set_configured` starts, stops and restarts. After a mid-session
  `list_changed` on a hot-added server, a new clean tool is offered, a new
  poisoned tool is not, and a new tool outside `enabled_tools` is not.
- The real binary: a failing smoke test writes nothing (no config change, no
  secrets file). A clean `mcp add` writes config and secret.
- The real binary: `ferrule gateway` with a local stdin/stdout channel and a
  scripted mock provider. Message 1, then `ferrule mcp add` from a second
  process, then message 2: the model is offered `mcp__<name>__echo` and its
  call succeeds, with no restart.
