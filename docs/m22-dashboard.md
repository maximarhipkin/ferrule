# M22: the dashboard — one page for the whole app (design)

Status: built, 2026-09-25, branch `m22-dashboard` (PR to main, not
merged). Follows M19b (reliability), M20 (connections) and M21 (models).
Where the build departs from this design: **As built** at the end. The
user guide is [dashboard.md](dashboard.md).

## Why

The owner runs `ferrule gateway` on a closed server (no inbound ports) and
talks to it from a phone over Telegram. `/status` answers "is it alive",
but everything else (which model is the default and whether it's failing,
what a connection's state is, what the last week cost, which scheduled
task failed, what the guard refused) is spread over a dozen CLI commands
that need SSH. v0.1.0 once went silently deaf; M19b made that visible, M22
makes it *fixable* from the phone.

One page, answering "what is it doing right now?" first, and then
everything else, with the fixes next to the problems. **It works when the
model doesn't**: nothing on the way to the page, and nothing on it, calls
the LLM.

## Remote access

### The choice: a `cloudflared` quick tunnel, opened on demand

Two outbound-only pieces exist from M20:

| | Worker relay (`ferrule-relay`) | `cloudflared` quick tunnel |
|---|---|---|
| what it is | a one-read mailbox: a callback lands, the gateway picks it up once | an HTTP proxy: `https://<random>.trycloudflare.com` → `127.0.0.1:<port>` |
| serving a page | would need the Worker to become a request/response proxy over a polling mailbox: every asset and every API call becomes two round trips through KV, a new protocol, and the Worker holds dashboard traffic | works as is: the browser talks HTTP to the local server |
| account changes | a redeploy with new code | none (quick tunnels need no account) |
| install | nothing | `cloudflared` on the machine (M20 already uses it as the OAuth fallback) |

M22 uses the **quick tunnel**, reusing `ferrule_connections::tunnel::open`
and M20's lookup (`[connections] cloudflared = "<path>"|"off"`, else
`PATH`). The Worker isn't touched: **the Cloudflare account is unchanged**.

The tunnel opens only when the owner asks (`/dashboard`, or `ferrule
dashboard link --remote`), and closes on `/dashboard off`, when the gateway
stops, and after `idle_minutes` (default 30) with no authenticated request
and no unused login link.

Without `cloudflared` (or with `[dashboard] remote = "off"`), `/dashboard`
says so and gives the local link with the SSH port-forward that reaches it.

### Threat model

- **Who can reach the server.** Locally: anything on the machine can
  connect to `127.0.0.1:<port>` (never `0.0.0.0`; there's no setting that
  binds elsewhere). Remotely: anyone who learns the random tunnel host
  while it's open. Neither gets anything without a login link: every page
  except the static shell and `/api/login` needs a session.
- **Cloudflare** terminates TLS and sees the traffic (pages, API JSON,
  cookies). That's the price of no inbound port. What the page shows is
  already redacted (below), so Cloudflare never sees a key or token.
- **The login link** is the only credential: 32 random bytes, one use,
  10 minutes. It travels in the URL **fragment** (`/login#<token>`), which
  browsers never send, so Telegram's link preview, Cloudflare's logs and
  the server's own logs never see it; the page's script POSTs it once. Only
  its SHA-256 is stored on disk.
- **Telegram** carries the link, so whoever can read the owner's Telegram
  chat can log in within 10 minutes. The owner chat is already the root of
  trust (M19: `/stop`, approvals).
- **Browser attacks.** CSRF: every mutation needs the session cookie
  (`SameSite=Strict`) *and* a per-session token in a header *and* a JSON
  body *and* a matching `Origin` when the browser sends one. DNS
  rebinding: requests whose `Host` isn't `127.0.0.1`/`localhost`/`[::1]`
  (with the port), the open tunnel's host, or the host a live link or
  session was issued for are refused. XSS: the page builds DOM with
  `textContent`, never `innerHTML` with data; the CSP forbids inline
  script and any other origin. Clickjacking: `frame-ancestors 'none'`.
- **Not defended:** a compromised machine or Telegram account, or
  Cloudflare itself. No SLA: a quick tunnel can drop; the page says so and
  `/dashboard` opens a new one.

## Auth

- **`/dashboard`** (Telegram, owner chat only, `hub.owner()` against the
  chat or the sender, as `/model` does) opens the tunnel if needed and
  sends a fresh login link. From any other chat: no reply at all, and the
  message doesn't reach a lane (so a non-owner learns nothing and no model
  call is made). It's an interceptor ahead of M19's owner door, so it's
  answered mid-turn, with every model down, with the kill switch on or a
  cap hit.
  Opening a tunnel takes a few seconds: the reply comes at once ("opening
  a tunnel, the link follows"), and the link follows as a second message.
- **`/dashboard off`** revokes: every unused link, every session, and the
  tunnel.
- **`ferrule dashboard link [--remote]`** on the machine mints a link for
  when Telegram is unreachable. Links live in
  `<data>/private/dashboard/links.json` (0600, under the config-style lock,
  written by atomic rename), so the CLI process mints them and the
  gateway's server accepts them. `--remote` opens a tunnel from the CLI
  process to the gateway's port and keeps it (and prints the link) until
  Ctrl-C.
- **`ferrule dashboard`** prints a local link to the running gateway's
  page, or, when no gateway runs, serves the page itself in the foreground
  (everything but the live lanes, channels and scheduler) until Ctrl-C.
- **Session:** a successful login sets `ferrule_dash=<32 random bytes>`:
  `HttpOnly; SameSite=Strict; Path=/`, `Secure` when the host is the
  tunnel's (HTTPS). Sessions are in memory (a gateway restart logs
  everyone out), time out after `idle_minutes` without a request (30) and
  `session_hours` after login (12), and are bound to the host they logged
  in on. Several at once are fine.
- **CSRF:** `/api/login` and `GET /api/session` return the session's CSRF
  token; the page keeps it in memory and sends `X-Ferrule-Csrf` on every
  POST. A POST without the cookie, the token, `Content-Type:
  application/json`, or with a foreign `Origin` gets 403 and does nothing.
- **Confirm:** destructive operations (disconnect, delete a task, remove a
  model, the kill switch) take `"confirm": true` in the body, which the
  page only sends after a `confirm()` dialog. Without it: 409 and a
  question, nothing done.
- **No passwords**, nothing to set up: the owner chat (or shell access to
  the machine) is the only way in.

## The page

`GET /` serves one HTML file, one script and one stylesheet, embedded in
the binary with `include_str!`; no build step, no library. Mobile first:
one column, section tabs at the top, big buttons. Chat names, messages
and model names are shown with `dir="auto"` so RTL text (Hebrew chat
titles) lays out on its own without flipping the page.

1. **Health** (first screen). The problems strip on top: every model
   that's down with its reason, the kill switch, a stale channel, a stuck
   turn, a missing price (the dollar caps can't see it), each with its fix
   as a button (switch the default, add a fallback, test another model,
   clear the stop). Then: up since, version, channels and their last
   successful poll, each running turn (chat, what it's doing, for how
   long, since the last progress, queue) with **Stop this turn**, the
   watchdog's recent warnings, the heartbeat (host and interval, last
   result; never the URL's path), the last restart and why, the kill switch
   with **Stop everything / Resume**, and spend today against the caps.
2. **Connections** (M20). Each service: state (connected / needs
   reconnect / expired), scopes, write access, connected / refreshed at,
   the last error. **Connect** (the same flow as Telegram's button: the page
   shows the reply and its link buttons), **Reconnect**, **Disconnect**.
3. **Models** (M21). The default, the fallbacks, pins per chat and per
   task, recent outages and what served last; **Set default**, **Pin** /
   **Unpin**, **Test**, **Add**, **Remove**, fallbacks. Then:
   - **Catalog**: each configured provider's `/models`, searchable and
     sortable: price per 1M tokens in/out, cached in, context window, tool
     support. Tool-capable only by default ("ferrule's agent needs
     tools"), `:free` ids flagged ("a shared, rate-limited pool; many
     have no tool endpoint (M19c)"). **Add**, **Add as default** (runs
     M21's real `test` first; a failing test adds the model but leaves the
     default alone), **Add as fallback**.
   - **Recommended**: a curated, dated list in three tiers, each entry
     re-checked against the live catalog (gone or tool-less ones are
     hidden and listed as such), priced from the catalog, with a monthly
     estimate from the owner's last 30 days of tokens.
   - **Fill missing prices** from the catalog.
4. **Usage** (the ledger). A range selector (1, 7, 30 days); totals; cost
   and tokens per day as inline SVG bars; per model, per task / chat; cache
   hit rate, p50/p95 latency, error and retry rates; the caps and how close
   each is.
5. **Tasks** (M3). Each task: schedule, next run, model, enabled, the last
   runs with status and duration; **Pause / Resume / Run now / Delete**.
6. **Logs**. The trust audit log (M19) and the gateway's recent warnings
   and errors, newest first, filterable by kind and text, paged, redacted.
   Never transcripts.
7. **Extensions**. MCP servers (configured and installed, active /
   suspended / not loaded, the reason, tools), skills, hooks per event.
   Read-only.
8. **Agents** (M12). Open agent trees, each agent's role, status, task,
   model and tokens. Read-only.

## Read models and operations

The dashboard never edits a file itself: it calls the same APIs Telegram
and the CLI call, which audit, lock and write atomically.

| section | read | operations | status |
|---|---|---|---|
| health | `Router::snapshot`, `Health` (uptime, version, stale channels, dispatcher, **last heartbeat**, **last start**), `Channel::last_ok_poll`, `RecentLog`, `Hub::stopped`, `Hub::today` + `TrustConfig` | stop a turn: **`Router::stop(session)`** (new); kill switch: `Hub::engage` / `Hub::clear` (exist, audited) | new: `Router::stop`, the heartbeat's last result, the last start kept after it's sent |
| connections | `Connections::snapshot` | `start` (as Telegram's button), `disconnect` | exist (M20) |
| models | `Models::view` | `set_default`, `pin`, `unpin`, `set_fallback`, `add_model`, `remove_model`, `test` | exist (M21); after a change the dashboard retires lanes exactly as `/model` does |
| catalog | **`models::catalog`** (new): fetch, cache, parse, filter, recommend, estimate | **`fill_prices`**, **`add_from_catalog`** (new; through `add_model` then a price write in the same config edit style) | new |
| usage | `ledger::read_records` + `aggregate` (the code `ferrule ledger` runs) | — | exists; per-day and per-chat grouping added next to `aggregate` |
| tasks | **`TasksAdmin::view`** (new) over `TaskStore` | **`pause`, `resume`, `delete`, `run_now`** (new; audited as `task_paused` … in the trust audit log) | new; `ferrule tasks pause/resume/delete/run-now` move onto it |
| logs | `Audit::read`, `RecentLog::last` | — | exist |
| extensions | `ExtensionManager::list` (the gateway's live one when there is one), `HooksConfig::entries` | — (read-only: no enable/disable operation exists for configured servers) | exist |
| agents | `AgentStore::all` | — | exists |

**Prices.** `[providers.X.models."<model>"]` gets `price_source`
(`"openrouter catalog 2026-09-25"`) next to the three prices. A model with
prices and no source was priced by hand and is never overwritten; fill
only writes models with no price at all (their own or their provider's).
`ferrule model catalog [--provider p] [--search q] [--tools] [--json]`,
`ferrule model recommend [--json]` and `ferrule model fill-prices` are the
CLI over the same functions; `ferrule doctor` warns about every connected
model without a price ("the dollar caps don't see its spend").

**The catalog source.** `GET {base_url}/models` for each configured
OpenAI-compatible provider (with its key when it has one). OpenRouter's
shape (`pricing.prompt` / `completion` / `input_cache_read` in USD per
token, `context_length`, `supported_parameters`) gives prices and tool
support; another shape gives ids only and prices from M21's presets where
known, else "price unknown", tool support "unknown". Cached per provider in
`<data>/models/catalog/<provider>.json`, refetched at most hourly, and the
cache is served (with its date) when the fetch fails. Nothing fetches it
unless a page or command asks.

**Recommended** is `crates/ferrule-cli/src/models/recommended.toml`
(dated, tiers `value`, `strongest`, `free`, a one-line reason each),
compiled in. Prices always come from the live catalog, never the file.

## Live updates

Polling. The page asks `GET /api/<section>` for the visible section every
5 s (health: every 3 s), and stops while the tab is hidden (Page
Visibility API) or after the session ends. Cheap when nobody's looking:
no page open is zero requests, and the server does nothing between
requests (no push state, no per-client task). SSE would need a long-lived
connection through the tunnel per tab, and gains little at 3–5 s.

Every response is built fresh from the APIs above, so a hand edit of the
config, a CLI change, or a Telegram `/model` shows up at the next poll.

## Never shown

- Keys, tokens, OAuth secrets, the relay key, `[secrets]` values: the API
  never reads them into a response (a key is `key_env` + present/missing,
  as M21's view already does). Every response body also passes through
  M19b's `Redactor` (configured secret values, the Telegram token,
  provider keys, anything bot-token-shaped) before it's sent.
- The heartbeat URL: host only.
- Message bodies: a running turn shows its activity (tool name and short
  argument summary, redacted, as `/status` does) and the first 80
  characters of its message, redacted; never transcripts.
- The login token: in the fragment only; never logged, only its hash on
  disk.

## Eval stays hermetic

The dashboard lives in the gateway and `ferrule dashboard`; `ferrule
eval` starts neither, so it never binds a port, writes the dashboard files,
fetches a catalog or runs `cloudflared`. A binary test runs a suite with
`[dashboard]` configured and a fake `cloudflared` that leaves a mark, and
checks that nothing is written or run.

## Failure modes

- **The tunnel or Cloudflare is down.** `/dashboard` says the tunnel
  didn't open (and why, e.g. "cloudflared isn't installed"), and gives the
  local link and `ssh -L` for the machine. A tunnel that dies is noticed
  on the next `/dashboard`, which opens a new one. The page, polling a
  dead host, says "the dashboard can't be reached: send /dashboard for a
  new link".
- **Two browser sessions.** Both work; each has its own CSRF token.
  Operations serialise on the config lock (and each API's own), last one
  wins, and both pages show the result at the next poll.
- **The config is hand-edited while the page is open.** Every read goes
  to the file (M21's view reloads it); the next poll shows the edit. An
  operation re-reads the config under the lock, so it applies to the
  edited file, not the page's copy. A config broken by hand makes
  operations fail with M21's "doesn't parse" error, shown on the page.
- **A stale page mutates.** Operations name their target (`set default
  deepseek`, `delete task t-12`) rather than sending state, so a stale
  page can't clobber a newer change; a target that's gone is an error
  ("no task t-12"). Destructive ones need the confirm flag. An expired
  session gets 401 and the page says "session ended: send /dashboard".
- **Every model is down.** The page is built without a model call, so it
  loads, with the outage first and three buttons: set another default
  (tested first), add a fallback, test a model.
- **A gateway restart** logs everyone out (sessions are in memory) and
  closes the tunnel; unused links in the file survive until they expire.

## Defaults

```toml
[dashboard]
enabled = true        # the gateway serves it on 127.0.0.1
port = 0              # 0: any free port, written to <data>/gateway/dashboard.json
remote = "tunnel"     # "tunnel": /dashboard opens a cloudflared quick tunnel; "off": local only
idle_minutes = 30     # a session, and the tunnel, close after this without a request
session_hours = 12    # a session ends this long after login regardless
link_minutes = 10     # a login link expires after this if unused
```

## Out of scope

- Editing caps, hooks, MCP servers or skills from the page (no such
  operations exist; read-only).
- The "evaluate a candidate" action (runs the starter suite on a model)
  may come later; it needs the eval harness wired into the gateway.
- Transcripts, sub-agent control, and any write to the relay Worker.
- A dashboard for `ferrule chat` / `ferrule run`.

## As built

The design held. Where the build differs, or adds something:

- **Polling** is per section: health every 3 s, agents 5 s, models,
  connections and tasks 10 s, usage 30 s. Logs and extensions refresh
  only when asked or when a filter changes. It stops while the tab is
  hidden, as designed.
- **Log lines hide URL paths.** A warning quoting a failing request (for
  example "error sending request for url (http://host/mcp/<token>)")
  shows `http://host/…`. The secret-scan test caught an MCP server's URL
  path in a reqwest error, so the rule the heartbeat and extensions already
  followed now covers log text too.
- **The reference catalog** is `[models] catalog_url`. Unset, it's
  OpenRouter's public list, read when no connected provider is OpenRouter.
  `""` turns it off, which every hermetic test does. A connected
  OpenRouter provider is itself the reference.
- **`ferrule model catalog`** takes `--search`, `--tools` and `--json`. No
  `--provider`: rows name their source, and `--search` narrows them.
- **Tasks.** `TasksAdmin` is behind `ferrule tasks pause|resume|delete` and
  the page. Telegram has no task commands, so nothing there moved.
  `ferrule tasks run-now` still runs the task in its own process. The
  page's **Run now** makes the task due, so the gateway's scheduler runs it
  at its next tick, with its no-overlap guard and gate.
- **Eval hermeticity** is tested by running the smoke suite through the
  binary with `[dashboard]` configured and checking that no marker or links
  file was written. `ferrule eval` never builds the dashboard, so no fake
  `cloudflared` was needed.
- **Asset size:** `app.js` 30.4 KB, `app.css` 3.0 KB, `index.html` 0.5 KB,
  33.9 KB in total, embedded with `include_str!`.

**Tests.**
- Unit tests in `dashboard/` (auth, http, api), `models/catalog.rs` and
  `tasks_admin.rs`.
- `tests/dashboard.rs` runs the real binary against a fake Telegram and
  scripted model servers, one of them serving a recorded OpenRouter
  `/models` (`tests/fixtures/openrouter-models.json`). It covers:
  - the one-time link: used twice, a non-owner, tampered, no session or
    CSRF, a foreign Origin;
  - `/dashboard off`;
  - the kill switch from the page;
  - a stuck turn shown and stopped from the page;
  - the default and pins agreeing with Telegram and the config, and a hand
    edit showing on refresh;
  - every model down: the outage on top, and a catalog pick as the default
    fixing the next turn;
  - the catalog: the tool filter, `:free`, prices per 1M, the offline
    cache, a missing recommended entry, the monthly estimate;
  - `ferrule doctor` on an unpriced model;
  - the usage against `ferrule ledger`;
  - a seeded-secret scan of every page and GET;
  - the eval never touching the dashboard.
- The connect/disconnect test drives M20's API against its mock
  authorization server and relay.
