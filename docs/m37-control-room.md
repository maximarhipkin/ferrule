# M37 — The control room (design)

Status: design, 2026-09-27, branch `m37-control-room`. Written before the
code; where the build departs from it, see **As built** at the end. User
guides: [dashboard.md](dashboard.md), [connections.md](connections.md).

Max, 27.09, from his phone, through the `/dashboard` tunnel, with Ferrule
running as a Linux system service:

> need to fix this, add ability to close these messages, and in the
> fallback also a select box from my existing models … fix all the
> connections, somehow add more connections, maybe set them as icons … if
> it can't connect … from the OAuth, give me an option using an API key or
> secret key or app key … change the font … I need the ability to manage
> my agent from the dashboard, not only from the terminal. I want the
> dashboard to be like the terminal: anything I can do, from there.

His screenshot never arrived. "These messages" is taken to be every strip
the page shows on top: the `problems()` banner first, and every other
alert, notice and error strip.

Five parts, each its own commit or commits:

1. Notices that close, and a fix button on every problem that has one (§1).
2. Models picked from lists, never typed; the fallback chain (§2).
3. Connections that work, more of them, as tiles, with keys (§3).
4. Terminal parity: a parity matrix, a command console, chat, config (§4).
5. A UI rebuilt for real use on a phone (§5).

---

## 0. What doesn't change

- **The auth model.** A one-use link, a session cookie (12 h at most, 30
  min idle), the owner only, the host allow-list, `Origin` checked on every
  POST, a JSON content type, and the CSRF header. Every new write endpoint
  goes through the same `Dashboard::handle` gate: none is added beside it.
- **Secrets never go back to the browser.** Every response still passes
  `redact_value`. A key typed into the page is write-only: the page shows
  "set" or "not set" and the last four characters at most where the store
  already shows them.
- **The page works with every model down.** Nothing on the page or on the
  way to it calls a model. The chat panel (§4.3) is the only part that
  needs one, and it says so when none answers.
- **No raw shell and no PTY** (§4.5).
- The eval starter suite, its graders and the mock model.

## 1. Notices

### 1.1 What a notice is

Today `/api/health` returns `problems: [{what, fix, section, action?,
suggest?, top?}]` and the page draws each as a strip. From M37 every
problem also carries:

| field | meaning |
|---|---|
| `id` | stable: `kind:subject` where the code knows the kind (`kill`, `model-down:<ref>`, `channel:<name>`, `cap:<name>`, `update`, `selfcheck:<key>`, `local`, `workspace`, `stuck:<session>`, `prices`). Anything else: `h:` + the first 16 hex of BLAKE2 over `section + what`. |
| `closable` | false for the kill switch and for "no model can answer". |
| `fixes` | `[{label, action, body}]`, each a POST the page can make. |

Other strips the page draws (a section that can't load, the connection
lost banner, a toast) are client-side: a toast gets a ×, and a section
error gets a × that hides it until the next navigation. They are not
stored because they are not facts about the machine.

### 1.2 Dismissing

`POST /api/notices/dismiss {id}` hides one; `POST /api/notices/restore
{id?}` shows one, or every hidden one. `GET /api/health` returns the
visible problems in `problems` and the hidden ones in `hidden` (with the
time each comes back), so the page can show "N hidden · show".

The store is `<data>/dashboard/notices.json`, owner-only (0600 on Unix),
written atomically (temp file + rename):

```json
{ "v": 1, "owners": { "telegram:123": { "model-down:openai/gpt-5.5": { "at": 1790000000, "until": 1790086400, "what": "h:…" } } } }
```

- **Per owner.** Keyed by the owner's primary chat ref (`channel:id`), or
  `owner` when none is configured. The page has one owner, but the key
  keeps a second owner (M31 allows one per channel) from hiding the first
  one's notices, and the file honest if that changes.
- **Server-side**, so a dismissal survives a reload, another device and a
  restart.
- **Comes back.** A hidden notice that is still true returns after 24 h,
  with "hidden yesterday, still true" on it. 24 h because every
  notice is either something that fixes itself within hours (a stale
  channel, a flaky model) or something the owner has to act on eventually
  (a cap, an update failure). A day is long enough not to nag on a phone
  and short enough that a real outage doesn't disappear for good. A
  notice whose text changes (another error, another model) is a new
  notice: the `what` hash in the entry no longer matches.
- **Pruned.** An entry whose notice is gone at the next health read, or
  that is older than 7 days, is dropped.
- **Never hidden for good:** the kill switch and "no model can answer"
  (the default and every fallback down or keyless). The API refuses to
  dismiss them (`409`, "this one can't be hidden: …"); the page has no ×.

### 1.3 Fix buttons

Every problem with a known repair gets one or more buttons. Each is an
existing or new POST endpoint; nothing runs a shell.

| problem | button | endpoint |
|---|---|---|
| kill switch on | Turn it off | `kill/off` (exists) |
| default model down / keyless | Make `<healthy>` the default; Set a fallback | `models/default` (exists); opens the fallback chain |
| no fallback while the default is flaky | Set a fallback | the chain editor (§2.1) |
| models without prices | Fill prices | `catalog/fill-prices` (exists) |
| channel problem or stale | Restart `<channel>` | `channels/restart` (new, §1.4) |
| config didn't load / reverted | Restore the last good config | `config/restore` (new, asks first, §4.4) |
| update check failed / update available | Check for an update | `console/run {line: "update --check"}` (a console job, §4.2); installing is `update --yes` in the console, which confirms |
| `claude` outdated / missing | Run doctor | `doctor/run`; there is no separate `update claude` command: `[update] claude = true` has `ferrule update` keep it current |
| tunnel lost | none on the page | a page reached through a lost tunnel can't be clicked; `/dashboard` in the owner's chat opens a new one |
| a stuck turn | Stop it | `turn/stop` (exists) |
| a connection needs reconnecting | Reconnect | `connections/connect` (exists, fixed §3) |
| a service keeps the gateway and it needs a restart | Restart the gateway | `gateway/restart` (only under a service) |

### 1.4 Restarting a channel

Channel adapters run as tasks the gateway spawned at start; there is no
handle to stop and respawn one today. The gateway keeps, per channel, a
`JoinHandle` and the factory closure, so `channels/restart {name}` aborts
the task and spawns a new one with a fresh adapter from the current
config. The inbound queue and the router are untouched.

**Restarting the gateway** is offered too, but only under a service
manager that restarts on exit (the check M36's last-good already uses:
`INVOCATION_ID` for systemd, `XPC_SERVICE_NAME` for launchd), and only
after a confirm that says what happens: the gateway exits, the service
starts it again on the current config, the page's tunnel ends with it,
and M24's relink sends a fresh link to the owner's chat. That is what a
config change outside `[[mcp.servers]]` and `[secrets]` (the two the
running process follows, M17) needs.

### 1.5 Doctor on the page

`ferrule doctor --json` prints `{"ok":bool,"items":[{level, what, text,
hints:[…], fix?:{label, action, body}}]}` instead of the lines. The same
`Report` collects both, so the two can't drift. The page's **Run doctor**
spawns the service's own binary with `--json` (as the service user, with
the service's config, stdin closed), reads the items and shows each with
its level and its fix button. `--ping-models` is a checkbox (it spends a
few tokens).

## 2. Models: pick, don't type

### 2.1 The fallback chain

`GET /api/models` already lists the configured models; `models/fallback`
already takes an ordered list. The page's editor is a list of select
boxes, each offering the configured models (with a health dot and the
price), with **↑ ↓ ×** on each row and **+ add** at the end, whose last
option is "from the catalog…" (the catalog picker, adding the model first
and then appending it). Saving posts the whole list. The API refuses an
unknown model, a duplicate, and the default itself (`400` with why).

### 2.2 Selects everywhere a model is named

The default, a chat's pin, a task's model, a sub-agent's model, the
routing tiers (fast / strong), and the eval candidate all become a select
over the same list (`GET /api/models/choices`: reference, alias, provider,
healthy, key present, price, tools). Free text stays only in the catalog
search.

### 2.3 Adding a model with its key

`POST /api/models/provider {provider, key?, base_url?}` writes the key
into the secrets file (`<data>/private/secrets.env`, 0600, the same file
`ferrule setup` writes; the running process binds new secrets by itself,
M17) under the provider's env name, never into the TOML and
never back to the page. `GET /api/models/provider/list?provider=` calls the
provider's `/models` with that key (server-side, through the same proxy
the agent uses) and returns ids only. **Test** runs the existing
`models/test`. A key that fails its test is not saved.

### 2.4 Plan sign-in from the page

- **ChatGPT plan.** `POST /api/plans/chatgpt/start` runs M35's
  `auth::start_device` and returns `{page, user_code}`; the page shows the
  code large, with a copy button and the page as a tappable link (not a
  popup, §3.2). `GET /api/plans/chatgpt/poll` finishes the device flow in
  the background task started by `start` and reports `pending | done
  {email, plan} | failed`.
- **Claude plan.** `claude setup-token` has to run where a browser is;
  the page shows the one command and a paste box for the token it prints.
  `POST /api/plans/claude {token}` runs M35's `subscription::claude::login`.
  The token is write-only.

## 3. Connections

### 3.1 Why they fail (found by reading the code)

In order of how likely each is to be Max's failure:

1. **The phone never gets the sign-in link.** The page's `act()` calls
   `window.open` after an `await`; mobile browsers (and desktop popup
   blockers) block that. The link is shown nowhere else: the toast lasts 5
   s, and nothing goes to Telegram. From Max's phone, **Connect** looks
   like it does nothing.
2. **A key can't reach the service's store.** `ferrule connections add`
   runs against the user's paths, not the system service's (only setup,
   doctor, config and update switch to the system paths, `main.rs`
   593–609), so on a system service a key added at the terminal lands
   where the service never looks. And the page refuses API-key services
   outright.
3. **The paste-back has nowhere to go on the page.** With no relay, the
   OAuth callback falls back to "paste the address you land on"; with no
   owner chat configured the paste goes to the terminal, and the page has
   no box. Google with the owner's client always takes this path when
   there's no relay.
4. **No relay by default,** and Google needs `FERRULE_GOOGLE_CLIENT_ID` /
   `_SECRET` that nothing on the page can set.
5. `connections add --write` is ignored for API keys (the flag never
   reaches `add_key`, which hardcodes read-only).
6. One 401 marks an API-key connection "needs reconnect" at once, with no
   retry; a single transient 401 from a busy server kills it.
7. Each flow starts its own cloudflared besides the dashboard's.
8. A DCR registration with a `trycloudflare.com` redirect can be refused
   by stricter servers (seen as "invalid redirect_uri").
9. The relay URL is read once per process.

### 3.2 Fixes

- **Links are anchors.** Every link a POST returns is shown in a modal as
  a big tappable link with the code (if any) and a copy button, until the
  owner closes it. No `window.open`.
- **A paste box** for the OAuth paste-back, on the connection's details
  sheet, always offered while a flow is open:
  `POST /api/connections/paste {name, url}` → `Connections::intercept`
  with the owner actor.
- **Keys from the page.** `POST /api/connections/key {name, fields,
  write}`: the fields are checked against the service's key form, then a
  **test call** runs with the key (§3.4); only
  a key that passes is sealed into the store. The key is never logged,
  never returned, and never reaches the model: it is injected by the MCP
  client as a header bound to the service's hosts (M20 §3.5).
- **`--write` for keys** reaches `add_key`.
- **A 401 on a key** is retried once after 2 s before the connection is
  marked.
- **`ferrule connections` uses the system paths** when the system service
  is installed and the user's own paths have no config, like setup and
  doctor.

### 3.3 Key-based alternatives (as built)

The design first had a `[service.key]` table hung off each OAuth service.
What shipped is simpler: **each way in is its own catalog entry**, and
the ones for one service share a `tile`. An entry carries its `option`
name, what it `covers`, a phone-sized `guide`, and its `fields`: what the
key form asks for, with secret ones write-only. So a service with a
sign-in and a key shows two lines on one tile, ordered simplest first.
Previews go last. An entry with `fixed_callback = true` signs in only
through the relay's fixed address.

Three entries have `native` set: `jira`, `gmail` and `google`. Their
tools run inside ferrule and call the vendor's REST API with the saved
key. No MCP server is involved, since none of those vendors host one
that takes that key.

### 3.4 The test call (as built)

A key is tested before it's sealed, with the same call its tools will make:

- **A native way in** runs its probe: Jira's `/rest/api/3/myself`, an
  IMAP login for Gmail, a JWT exchange plus one Drive list for a service
  account.
- **An MCP server** gets `initialize` and `tools/list` with the key in its
  header.

**Test** on a connected line does the same again. Either way the answer
is a sentence: "Works: …, N tools" or exactly what failed. It says which
side refused, and why in plain words. A status code, the `state` or the
key is never in it (`explain.rs`, §3.7). A key given an end date
(`expires`) adds "the key expires in N days".

A way in that can't work yet is **blocked**, and gets no button. Instead
it says what's missing and offers the simpler options. `/connect` in the
owner chat uses the same function (`Connections::blocked`). Two cases:

- `google_oauth` and the Google previews without the owner's client;
- any `fixed_callback` entry without a live relay.

### 3.5 Services (as built)

**Atlassian**, in this order:

1. `jira` (native): site + email + API token (+ optional end date).
   Covers Jira issues (search, read, create, update, comment, move) and
   Confluence pages (search, read). No admin switch.
2. `atlassian_token`: Rovo MCP with `Authorization: Basic {email:token}`,
   or a service account's `Bearer` key with the email left empty. The
   admin must allow API tokens for Rovo MCP. A 401 says so and switches
   to 1.
3. `atlassian`: Rovo MCP over OAuth with DCR, `fixed_callback`. The admin
   must add the relay's domain as `https://<relay-host>/**`, and the tile
   shows the exact pattern. A domain refusal is explained, with a button
   to switch to 1.

**Google** (what each covers is said on the tile):

- `gmail` (native): the address + an app password, IMAP to read, SMTP to
  send. Sending is a write tool, so it is listed only with "allow
  changes" and each send asks.
- `google` (native): a service-account JSON key. Covers Drive
  search/read, Sheets read/write, Docs read, Calendar list/create, for
  what is shared with the account.
- `google_oauth` (native tools, the owner's OAuth client
  `FERRULE_GOOGLE_CLIENT_ID/_SECRET`, `fixed_callback`). The same tools,
  as the owner. The guide says to publish the app, because Testing
  expires after 7 days.
- `gmail_mcp` and `gdrive_mcp`: Google's preview MCP servers, last.

**Others** as before, plus key options where the vendor documents one:
github (PAT), linear (sign-in, or `linear_key`), stripe (restricted key),
huggingface, sentry, notion and attio (sign-in). Owners add their own in
`[[connections.custom]]`.

**Not shipped, and why.** The services in the first draft of this table
had no documented key for their hosted MCP server, or no DCR:

- Airtable, Supabase, Cloudflare, Monday, Intercom, Todoist, Canva,
  Webflow and Dropbox are left for later (§8).
- For Notion and Attio, only sign-in is documented.
- Slack, HubSpot, Asana, Box, Figma, ClickUp and Vercel need a registered
  app, or only allow listed clients.
- Zendesk, PayPal, Trello and Square: as listed in the first draft.

### 3.6 Tiles

The Connections section becomes a grid of tiles: an inline SVG icon (a
simple monochrome glyph per service, drawn for Ferrule — not the vendor's
trademarked logo file — shipped in the binary, `currentColor` so it
follows the theme), the name, and a state dot. A tap opens the details
sheet: state, scopes, account, last use, **Connect / Reconnect /
Disconnect / Test**, the key form, the paste box and the vendor's note.
No CDN, no remote images.

### 3.7 The fixed callback, the checklist, stuck flows, errors

- **The relay card** tops the page:
  - **Deploy.** Takes a Cloudflare API token (template "Edit Cloudflare
    Workers", linked). The account comes from `GET /accounts`, with a
    choice when there are several. It deploys `relay/worker.js`, saves
    the URL and key, and checks end to end. The token isn't kept.
  - **Use an existing relay.** Takes a URL + key, checked (health, then a
    slot written and read back) before saving.
  - **Once live**, it shows `<relay>/cb` with Copy, and which services
    call back through it.
- **The checklist** (`/api/connections/checklist`,
  `ferrule connections setup`) has one line per real check:
  - `relay`: missing, ready or unreachable;
  - `google_client`;
  - each preview;
  - Atlassian.

  Each line shows its state and the button for its next action. A line is
  never offered as ready on a guess.
- **Pending flows** are listed with Cancel and dropped after `flow_ttl`
  (15 min).
- **Error mapping** (`ferrule-connections/src/explain.rs`) turns each
  failure into plain words for the page. It covers:
  - an HTTP status from a token endpoint or an MCP server;
  - an OAuth `error=` on the callback;
  - a handshake that isn't MCP;
  - the owner's own egress policy blocking a private address.

  A switch-to-key button comes with it where one exists. For Google and
  Atlassian, the page also lists the **symptoms**: the messages their
  consent pages show, in their words, each with its fix, for errors that
  never reach ferrule.
- **Doctor and the Home banner** name a connection that stopped, or whose
  key expires within 7 days (`Connections::attention(7)`). The fix button
  opens its tile.
- **CLI twin:** `ferrule connections setup [relay|google-client|<tile>|<service>] [--write]`.
  It asks which way in, then that way's fields, reading secrets without
  echo. It uses the same code as the page.

## 4. Terminal parity

### 4.1 The parity matrix

Every `ferrule` subcommand, and where it lives on the page. "console" means
the command console (§4.2) runs it; "page" means a first-class control.
"terminal only" carries the reason. The class is the command as typed:
**read** runs, **change** confirms, **destructive** confirms with the red
button, **refused** says what does it instead. Flags move some (`update
--check` reads, `update --yes` is destructive, `learn run --dry-run` reads).
The table is `PARITY` in `dashboard/console.rs`, copied here.

| command | page | class | notes |
|---|---|---|---|
| `setup` | terminal only | refused | `--refresh-service` from the console; the wizard asks at the terminal; `setup --refresh-service` runs here |
| `doctor` | Home (Run doctor) + console | read |  |
| `update` | fix buttons + console | change | needs `--yes` (the page's confirm is the yes) or `--check` |
| `run` | console | change | one agent turn, with its tools |
| `graph run` | console | change | an agent graph, end to end; approval nodes need `--yes` here |
| `sessions` | console | read | with each branch's parent |
| `chat` | Chat | refused | a REPL isn't a console command: use the Chat page |
| `memory add` | console | change |  |
| `memory search` | console | read |  |
| `memory reindex` | console | change |  |
| `memory model download` | console | change | needs `--yes` |
| `memory recent` | console | read |  |
| `memory forget` | console | destructive |  |
| `config path` | Config + console | read |  |
| `config edit` | Config (the raw editor) | refused | an editor isn't a web thing: use the Config page's editor |
| `config example` | console | read |  |
| `config init` | terminal only | refused | the service already has its config: change it on the Config page |
| `gateway` | terminal only | refused | it is the running service |
| `status` | Home + console | read |  |
| `health` | console | read |  |
| `backup` | console | change | `--include-secrets` is terminal only |
| `restore` | terminal only | refused | `--dry-run` checks a backup from the console; the rest refuses while the gateway runs, and the page is the gateway: stop it and restore at the terminal |
| `dashboard link` | console | change | `--remote` is terminal only (it holds a terminal open) |
| `dashboard off` | session menu + console | destructive | logs out every browser, this one too |
| `model list` | Models + console | read |  |
| `model default` | Models + console | change |  |
| `model test` | Models + console | read |  |
| `model add` | Models + console | change |  |
| `model remove` | Models + console | destructive |  |
| `model alias` | Models + console | change |  |
| `model pin` | Models + console | change |  |
| `model unpin` | Models + console | change |  |
| `model fallback` | Models + console | change |  |
| `model catalog` | Models + console | read |  |
| `model recommend` | console | read |  |
| `model fill-prices` | console | change |  |
| `model eval` | Eval + console | change | needs `--yes` (it spends money) |
| `model route set` | Routing + console | change |  |
| `model route off` | Routing + console | change |  |
| `tasks add` | Tasks + console | change | `--gate` is terminal only (a gate is a shell command) |
| `tasks list` | Tasks + console | read |  |
| `tasks model` | Tasks + console | change |  |
| `tasks schedule` | Tasks + console | change |  |
| `tasks pause` | Tasks + console | change |  |
| `tasks resume` | Tasks + console | change |  |
| `tasks delete` | Tasks + console | destructive |  |
| `tasks runs` | Tasks + console | read |  |
| `tasks run-now` | Tasks + console | change |  |
| `learn run` | console | change | `--dry-run` only reads |
| `learn show` | console | read |  |
| `learn diff` | console | read |  |
| `learn revert` | console | destructive |  |
| `ledger` | Usage + console | read |  |
| `agents list` | Agents + console | read |  |
| `agents close` | Agents + console | destructive |  |
| `skills disable` | Extensions + console | change |  |
| `skills enable` | Extensions + console | change |  |
| `hooks list` | console | read |  |
| `hooks trust` | terminal only | refused | no flag skips its question, on purpose; trusting hooks lets their commands run as you later, and it asks at the terminal |
| `hooks untrust` | console | change |  |
| `extensions list` | Extensions + console | read |  |
| `extensions pending` | Extensions + console | read |  |
| `extensions approve` | Extensions + console | change |  |
| `extensions deny` | Extensions + console | change |  |
| `extensions remove` | Extensions + console | destructive |  |
| `extensions resume` | Extensions + console | change |  |
| `ssh list` | console | read |  |
| `ssh trust` | console | change | needs `--fingerprint` |
| `ssh test` | console | read |  |
| `import openclaw` | console | read | `--apply` changes |
| `import hermes` | console | read | `--apply` changes |
| `plugins add` | console | change |  |
| `plugins list` | console | read |  |
| `plugins remove` | console | destructive |  |
| `mcp add` | console | change | needs `-y`; `--no-sandbox` is terminal only |
| `mcp list` | Extensions + console | read |  |
| `mcp remove` | Extensions + console | destructive |  |
| `mcp disable` | Extensions + console | change |  |
| `mcp enable` | Extensions + console | change |  |
| `instances list` | console | read |  |
| `instances new` | terminal only | refused | it runs another instance's setup wizard, which asks at the terminal |
| `instances remove` | terminal only | refused | it removes another instance: run it at the terminal |
| `channels keys list` | Channels (HTTP API card) + console | read |  |
| `channels keys add` | Channels (HTTP API card) | refused | it prints a key once: make it on the HTTP API card, where it isn't kept in the console's output |
| `channels keys webhook` | terminal only | refused | it prints a signing secret once: run it at the terminal |
| `channels keys revoke` | Channels (HTTP API card) + console | destructive |  |
| `connections list` | Connections + console | read |  |
| `connections add` | Connections | refused | keys and sign-ins go in on the Connections page, where a key never lands in a log |
| `connections remove` | Connections + console | destructive |  |
| `connections catalog` | Connections + console | read |  |
| `connections relay deploy` | Connections (callback address card) | refused | it asks for a Cloudflare token: use the callback address card on Connections |
| `connections relay check` | Connections + console | read |  |
| `connections setup` | Connections (the checklist) | refused | the page's checklist is the same list; it asks at the terminal |
| `eval run` | Eval + console | change | it spends money; `--dry-run` only reads |
| `eval report` | Eval + console | read |  |
| `stop` | Home + console | change | `--status` only reads |
| `undo` | console | change |  |
| `trust status` | console | read |  |
| `trust audit` | console | read |  |
| `trust caps` | console | read | `--set` needs `--yes` and confirms |
| `login` | Models (§2.4) | refused | sign in from the Models page (Sign in) |
| `logout` | Models + console | destructive |  |
| `plan list` | console | read |  |
| `plan approve` | console | change |  |
| `plan reject` | console | change |  |
| `sandbox` | terminal only | refused | it runs any command: a shell by another name |
| `claude-mcp` | terminal only | refused | internal: claude speaks to it over stdio |

A unit test walks the clap tree (`Cli::command()`) and fails when a leaf
subcommand has no entry in `PARITY` or no row in this table: every new subcommand has to be given a
route, or "terminal only" with a reason.

### 4.2 The command console

A line, not a terminal:

- The page sends `{line}`. The server splits it itself (words, single and
  double quotes, backslash escapes; no `$`, globs, pipes, `;`, `&&` or
  redirections — those are refused, not passed on) and parses it with
  clap's `try_get_matches_from` on the real command tree. `--help` and
  parse errors are rendered in-process and returned as the job's output.
- Each leaf has a class: **read** (runs), **change** (the page confirms
  first, `409` until `confirm: true`), **destructive** (confirms, with the
  red button), or **refused** with the reason and, when there is one, the
  flag or page control that does it instead.
- An allowed line runs as a child: the service's own binary
  (`current_exe()`), as the service's user, with `FERRULE_CONFIG` set to
  the config the gateway loaded, `NO_COLOR=1`, **stdin closed**, and
  stdout and stderr captured into a job. A command that tries to prompt
  gets EOF and fails; the classifier also knows every prompting command
  and refuses it up front with the flag that avoids the prompt (`update
  --yes`, `memory model download --yes`, `mcp add -y`, `ssh trust
  --fingerprint`, `trust caps raise --yes`, `setup --refresh-service`).
  With the flag the page's confirm has already happened.
- **Streaming by polling.** The dashboard's HTTP server is one request per
  connection with no chunked encoding, by design. `GET
  /api/console/job?id=&from=N` returns the output from byte N and whether
  the job is done, with its exit code; the page polls every 500 ms while
  a job runs. `POST /api/console/cancel {id}` kills it. Jobs keep 256 KB of
  output (the head is kept, the middle elided) and are dropped 10 minutes
  after they end. One job at a time per session; a 10-minute timeout.
- **Completion** from the clap tree: `GET /api/console/complete?line=`
  returns the subcommands and flags that can follow, with their help
  lines, and the model references and task/connection/skill names where an
  argument takes one.
- Every run is written to the audit log (`by=dashboard`) with the line,
  the class, the exit code and the duration.
- **Refused outright**: a user `--config` (the page manages the service's
  config only), `sandbox`, `gateway`, `chat`, `claude-mcp`, `config edit`,
  `setup` without `--refresh-service`, `relay deploy`, `mcp add
  --no-sandbox`, `tasks add --gate`.

### 4.3 Chat from the page

**Its own session**, not the owner's Telegram lane. A `DashboardChannel`
(channel `dashboard`, chat `owner`) is registered with the router like any
adapter: a message typed on the page is an inbound message; the agent's
replies, its streamed edits and its buttons land in a log the page polls
(`GET /api/chat?from=N`, every second while a turn runs, 5 s otherwise).
Why its own session: the owner's Telegram lane has its own history and a
turn may be running in it; sharing it would interleave two surfaces'
messages in one history, and a reply meant for the page would land on the
phone's Telegram too. The `dashboard__owner` session gets the owner's
trust route (like Telegram's private chat), so approvals go to the owner.

**Approvals inline.** Every pending approval the trust hub holds, from any
lane, is listed on the page (`GET /api/approvals`) with **Allow** and
**Refuse**, which answer it exactly as the Telegram buttons do (`yes CODE`
/ `no CODE` through the hub). This needs a list method on the hub's
approvals, added in `ferrule-trust`.

Edge: with no owner chat configured at all, `ask_owner` has nowhere to
send, and the page's list is the only place an approval can be answered.

As built: the page's chat is added to the router's channels and to the
owner's notifier only when `[dashboard] enabled`, and appended **last** to
the hub's owners, so it becomes the primary chat (the one questions and
warnings go to) only when no other owner chat is configured. It is not one
of `/connect`'s or the plan chats' channels. The log keeps the last 300
entries, polled by revision (`?from=REV` returns only what changed since,
an edited message whole again); a message is capped at 16 KB. Outside the
gateway (`ferrule dashboard` on its own) `GET /api/chat` says
`listening: false` and the page shows why instead of a box that can't
work. `POST /api/approvals/answer {code, allow}` answers through the
hub's approvals (`decide`) and writes `approval_allowed` /
`approval_refused` (`by=dashboard`) to the audit log.

### 4.4 Config

- **A form per section** for the fields the page can safely change:
  `[agent]` (model, max turns), `[trust]` caps, `[dashboard]`,
  `[update]`, `[connections]` (relay, cloudflared), `[memory]`, and the
  channels' non-secret fields. Each field validated inline (the same
  types the loader uses), saved through `config set`'s own code.
- **A raw TOML editor**: the file as text, `POST /api/config/check` parses
  it with the real loader and returns the errors with their line, `POST
  /api/config/save` checks again, keeps the file it replaces as
  `<config>.prev`, and writes atomically (temp + fsync + rename). The page
  never saves a file that doesn't read. The running process follows
  `[[mcp.servers]]` and `[secrets]` by itself (M17); for anything else the
  page says a restart is needed and offers it (§1.4). **Reload from
  last-good** (`config/restore`) copies M36's last good copy
  (`<data>/gateway/config.last-good.toml`, written after every good start)
  over the file, after checking it reads; **Undo the last save** puts
  `<config>.prev` back.
- **Command-bearing fields can't be changed from the page**: an MCP
  server's `command`/`args`/`env`, `[[hooks]]`, task gates, `[sandbox]`
  turned off, `[tools] bash` policy loosened. Each is equivalent to
  running a shell command on the machine. A save that changes one is
  refused with the field named; the terminal can still do it.
- Secrets never appear: the editor shows the TOML, which by M19 holds no
  secret (secrets live in `secrets.env`); any `*_key`, `token`, or
  `secret` field found in it is redacted on the way out and must stay
  unchanged on the way in.

As built (`dashboard/config_page.rs`): `GET /api/config` returns the
text, the form's fields with their values, and whether an undo copy and a
last good copy exist; `POST /api/config/check`, `/save {text}`, `/set
{key, value}` (one form field, from a fixed list, typed), `/undo`; the
last good copy is `config/restore` from §1.3, which asks first. A change
is compared leaf by leaf (a table in an array is named by its `name`, so
reordering MCP servers isn't one) and refused when it touches a whole
section that runs something or decides where a secret goes (`[sandbox]`,
`[hooks]`, `[extensions]`, `[ssh]`, `[secrets]`, `[egress]`, `[browser]`,
`[plans]`, `[plugins]`) or any field whose name holds `command`, `gate`,
`hook`, `path`, `allowed`, `owner`, `workspace`, `relay`, `roots`, or is
`args`, `env`, `binary`, `program`, `cloudflared`, `endpoint`, `issuer`,
`*_env` or `*url`: stricter than the list above, on purpose (an allowed
chat lets someone in; a changed `*_url` or `*_env` moves where a key is
sent). Undo is guarded the same way (a terminal edit made after the page's
save isn't undone from the page); restoring the last good copy is not,
because it is the way out of a file a terminal broke. A value the
redactor hid on the way out comes back as the file's value when it's
returned unchanged.

### 4.5 No raw shell (a decision for Max)

The page does not get a shell or a PTY. The page is reachable from the
public internet through a quick tunnel; a stolen session cookie (or a
browser bug) would then be a remote shell on Max's server as the service
user. The console runs `ferrule` subcommands only, parsed and classified,
with every command-bearing path refused, and that is the line.

Designed so an opt-in could come later: the console's job runner takes an
argv, not a string, and the classifier is the only gate. A later
`[dashboard] shell = true` (off by default, refused through a tunnel,
local or `ssh -L` only, and with a second factor) would add a class, not a
new runner.

## 5. The UI

### 5.1 Fonts

**IBM Plex Sans** (text, 400 and 600), **IBM Plex Sans Hebrew** (400, 600,
behind a `unicode-range` so it's fetched only when Hebrew is on the page:
Max's chats are in Hebrew), and **IBM Plex Mono** (400), all SIL Open Font
License 1.1, the licence text shipped at `/fonts/OFL.txt` and in the repo
beside the files. Served as separate `woff2` files with a year's cache,
from the binary, instead of today's base64 Geist inside the CSS (which
makes every CSS change re-download 90 KB of fonts). The payload is in
**As built**.

Type scale: 13 / 15 / 17 / 20 / 24 / 30 px (≈1.2), body 15 px, line
height 1.5, tabular numerals in tables and stats.

### 5.2 Layout and components

- **Phone first.** A bottom tab bar with the five sections used most
  (Home, Chat, Models, Connections, More), reachable by a thumb; "More"
  opens the rest as a sheet. On a desktop (≥ 900 px) a left sidebar with
  every section.
- Cards, tiles, forms with inline validation, selects, toggles, modals
  (and bottom sheets on a phone), toasts that stack and close, empty
  states, skeletons while a section loads.
- **Light and dark follow the system** (`prefers-color-scheme`), with the
  manual toggle kept.
- Built with `textContent` only, as today: nothing from the server is
  parsed as HTML. User and agent text in `dir="auto"`.
- Screenshots before and after, phone (390 px) and desktop (1280 px), from
  the real binary against a temp config, in `docs/assets/m37/`.

## 6. Threat model

| threat | answer |
|---|---|
| A stolen session drives the console | Every change confirms; no shell, no command-bearing fields, no `sandbox`, no `--no-sandbox`; every run audited; `/dashboard off` revokes. |
| CSRF on a new endpoint | Same gate as every other POST (`Origin`, JSON, cookie, CSRF header). |
| A key leaks to the page | Keys are write-only; responses pass the redactor; tests check that a saved key never appears in any GET. |
| A key leaks to the model | Keys are injected by the MCP client as headers bound to the service's hosts; they're never in a tool's input or output. |
| A key is tested against the wrong host | The probe and the MCP URL come from the catalog, not the request. |
| The console's parser is fooled into a shell | There is no shell: argv goes straight to `Command`; metacharacters are refused before parsing. |
| The raw config editor sets a command | The command-bearing diff check refuses the save. |
| A notice hides a real outage | The kill switch and "no model can answer" can't be hidden; everything else returns after 24 h if still true. |
| A chat message from the page is an owner command | It is: the page is owner-only. `/stop`, `/resume` and approvals work there exactly as in the owner's private chat. |

## 7. Failure modes

- **No gateway** (`ferrule dashboard` standalone): the console still works
  (it spawns the binary); chat, restart-channel and approvals say they need
  the gateway.
- **Every model down:** everything but chat works; chat says so.
- **The console's child hangs:** 10-minute timeout, and **Cancel**.
- **The notices file is corrupt:** treated as empty (nothing hidden), and
  rewritten on the next dismissal.
- **A save of the config races the watcher:** the atomic rename means the
  watcher sees the old or the new file, never half of one.

## 8. Out of scope

- A raw shell or PTY (§4.5).
- Restarting the service when no service manager runs it (a gateway in a
  terminal can't be brought back from the page).
- Deploying the relay anywhere but Cloudflare (§3.7 deploys to Cloudflare with an API token, no wrangler).
- Vendor logos as image files (the tiles use simple glyphs).
- Services whose auth can't be described precisely yet (§3.5 lists them as
  follow-ups).

## 9. As built

Where the build departs from the sections above:

- **Connections:** §3.3–3.5 and §3.7 describe what shipped.
  - Each way in is its own catalog entry on a shared tile.
  - Three native ways in: Jira, Gmail and Google.
  - The relay card, the checklist, pending flows, the error mapping,
    the symptoms and `ferrule connections setup`.

  Relay deploy no longer needs wrangler: it uses the Cloudflare API with
  a token.
- **Payload** (§5.1). Fonts, as woff2:

  | file | bytes |
  |---|---|
  | Plex Sans 400 | 20,984 |
  | Plex Sans 600 | 22,260 |
  | Plex Mono 400 | 17,268 |
  | Plex Sans Hebrew 400 | 33,260 |
  | Plex Sans Hebrew 600 | 35,152 |

  That's ≈129 KB in all, and ≈60 KB for a page with no Hebrew.
  `OFL.txt` is 4,456 B. Icons are inline SVG paths in `app.js`, so
  there's no icon file.
  - `app.js`: 104,709 B (28,447 gzipped).
  - `app.css`: 20,814 B (5,574 gzipped).
  - `index.html` and `theme.js`: ≈1.3 KB each.

  `theme.js` sets the theme before first paint, because the CSP allows
  no inline script.
- **Screenshots** (§5.2): only the "after" set is in `docs/assets/m37/`
  (Home, Connections, Models, Chat and Console at 390 and 1280 px), made
  by `scripts/dashboard_browser_check.mjs --shots`. No "before" set was
  captured when the branch was cut.
- **Browser check** (§6 tests):
  - It drives Chromium over the DevTools protocol from Node 22 with no
    packages, rather than through agent-browser, so it runs the same on a
    laptop and in CI.
  - It covers: hide a notice, pick a fallback, connect with a key (a
    local MCP server, allowed through `[egress] private_allow`), run a
    console command, chat, and every section rendered with no stray
    `null`/`undefined`.
  - CI runs it on Linux, macOS and Windows.
- **Egress block, plain.** A connection blocked by the owner's own egress
  policy says so: "Your egress policy blocks <host>…". Before, it was
  reported as a server that doesn't speak MCP.
