# The dashboard

One page for the whole app: what Ferrule is doing right now, what's
connected, which model is the default and whether it works, what it cost,
the scheduled tasks, the logs and the extensions. It's built for a phone,
and it works when the model doesn't: nothing on the way to the page, and
nothing on it, calls the model. The design, and the reasons behind it, are
in [m22-dashboard.md](m22-dashboard.md).

## Opening it

Send **`/dashboard`** to your bot in your private chat. The gateway answers
it itself, ahead of everything else, so it works mid-turn, with every model
down, with the kill switch on or a cap hit.

- With `cloudflared` installed (the default, `[dashboard] remote =
  "tunnel"`), the reply is "Opening the dashboard's tunnel, the link
  follows in a moment", then an `https://….trycloudflare.com/login#…`
  link. Tap it.
- Without `cloudflared`, or with `remote = "off"`, you get a
  `http://127.0.0.1:<port>/login#…` link and the `ssh -L` command that
  reaches it from another machine.
- Sent from a group, the link goes to your private chat. From anyone but
  the owner, `/dashboard` gets no reply at all.

A link works **once**, for 10 minutes. Using it signs that browser in: the
session lasts 12 hours at most and ends after 30 minutes without a
request. The tunnel closes after 30 minutes with no session and no unused
link.

**`/dashboard off`** revokes every link and session and closes the tunnel.

**A restart keeps you signed in** (M24). Sessions are kept in
`<data>/private/dashboard/sessions.json` (hashes only, owner-only), and
the page comes back on the port it had, so a local or `ssh -L` tab keeps
working. A revoked session stays revoked, and a used link stays used. A
session through a tunnel can't survive, because the next tunnel has a new
name. When one was live, the gateway opens a new tunnel after the restart
and sends you a fresh link, at most once every 10 minutes.

At the machine (when Telegram itself is the problem):

```bash
ferrule dashboard link            # a one-time link to the running gateway's page
ferrule dashboard link --remote   # the same through a quick tunnel, open until Ctrl-C
ferrule dashboard off             # revoke every link and session
ferrule dashboard                 # a link, or the page served from here when no gateway runs
```

## What's on it

- **Health** comes first. The problems are on top, each with its fix: a
  failing default model (with a working one to switch to), the kill
  switch, a stuck turn, a channel that stopped polling, a used-up cap, a
  model without prices. Below that: uptime, version, channels, the
  running turns with **Stop**, the watchdog, the heartbeat (host only),
  the last restart, the kill switch with **Stop everything / Resume**,
  and today's spend against the caps.
- **Connections** (M20): each service's state, scopes and last use;
  **Connect**, **Reconnect**, **Disconnect**.
- **Models** (M21): the default, the fallbacks, the pins and outages;
  **Set default**, **Pin**, **Unpin**, **Test**, **Add**, **Remove**.
  - **Catalog**: OpenRouter's list and your providers' own `/models`,
    searchable and sortable by price, context or name, with prices per 1M
    tokens. It shows tool-capable models only unless you ask, because
    Ferrule's agent needs tools. `:free` models are flagged: they share a
    rate-limited pool and many have no tool endpoint. **Add**, **Add as
    default** (the real test runs first; a failing model is added but
    doesn't become the default), **Add as fallback**.
  - **Recommended**: a short curated list in tiers, checked against the
    live catalog (a model that's gone or can't call tools is hidden and
    named), with a monthly estimate from your last 30 days of tokens.
  - **Fill missing prices** writes catalog prices into the config with
    their source and date. It never overwrites a price you set by hand.
- **Usage** (the ledger): 1, 7 or 30 days; cost and tokens per day as
  bars; per model, task and chat; cache hit rate, latency, error and
  retry rates; the caps, with **Edit caps**. The per-model table is the one
  `ferrule ledger` prints.
- **Tasks**: schedule, next run, model, last runs; **Pause**, **Resume**,
  **Run now**, **Schedule**, **Model**, **Delete**.
- **Logs**: the audit log and the gateway's recent warnings and errors,
  filterable, paged and redacted. Never transcripts.
- **Extensions**: MCP servers (**Disable**, **Enable**, **Remove**),
  skills (**Disable**, **Enable**), the config's hooks, and this
  workspace's `.ferrule/hooks.toml` with its SHA-256 and **Trust this
  version** / **Untrust**. See [Editing from the page](#editing-from-the-page).
- **Agents** (running sub-agents): read-only.

The page polls only the section you're looking at (health every 3 s,
usage every 30 s, logs and extensions only when you ask), and stops while
the tab is hidden. A hand edit of the config, a CLI change or a
Telegram `/model` shows at the next poll.

Disconnect, delete, remove and the kill switch ask for a confirmation
first, and so do raising a cap, disabling an MCP server and trusting hooks.

## What it never shows

Keys, tokens, the relay key, secrets-file values, the heartbeat URL's
path, URL paths in log lines, and message bodies beyond a redacted
80-character preview of a running turn. Every response also passes through
the same redactor as `/status`.

## Model prices from the command line

```bash
ferrule model catalog --search flash --tools   # prices per 1M, context, tools
ferrule model catalog --json
ferrule model recommend                        # the tiers, with a monthly estimate
ferrule model fill-prices                      # price every unpriced model from the catalog
```

`ferrule doctor` warns about each connected model without prices, since
the dollar caps can't see its spend.

## Evaluating a candidate model

Before switching models, you can see how one does on Ferrule's own
harness (M24). **Evaluate** is on every row of the Models table and the
catalog. Pick the tasks above it:

- the **smoke subset**: 4 quick tasks, one of each kind (the default);
- the **whole starter suite**: all 20.

The page first asks with an estimate: the tasks, the tokens and dollars
at the model's prices (configured, or else the catalog's), your budget,
and how the default did on the same tasks last time. The estimate is what
the suite's mock model uses on those tasks. A real model takes more turns,
often several times more, so treat it as a floor.

The eval runs in the background, one at a time, without slowing the
gateway, with progress and a **Cancel**. It runs under your caps like any
unattended run:

- the kill switch or a used-up day cap refuses it before anything is sent;
- its budget is the lower of the per-run caps and what's left of today's;
- with no cap at all, it uses `ferrule eval`'s own defaults ($5, 20M tokens);
- its calls count toward the day, under the tree `eval:<run id>`;
- a model that isn't connected runs through its provider for the eval
  only, and isn't added.

The result sits next to the default's last result on the same tasks. It's
saved like any `ferrule eval run` (`<data>/eval/<run id>/`), so `ferrule
eval report --run <id>` prints it later.

```bash
ferrule model eval openrouter/qwen/qwen3-coder              # smoke subset; asks first
ferrule model eval fast --suite starter --yes               # all 20, no question
```

The CLI prints the estimate and asks. Without a terminal it needs `--yes`.
It exits 3 when a cap stopped the run. It needs the starter suite from a
Ferrule checkout, looked up in this order: `[eval] suite`,
`$FERRULE_EVAL_SUITE`, `./evals/starter`, then the checkout the binary was
built from. It needs `python3` for the graders. An eval never starts the
dashboard or a tunnel.

## Editing from the page

The page, Telegram and the CLI run the same operations. Each one takes the
config lock, edits `ferrule.toml` in place (comments and order kept),
checks that the result still loads, writes it atomically and records an
audit entry with who did it (`dashboard`, `cli` or `telegram chat …`).
A running gateway picks the change up at once.

| What | Page | Telegram (owner) | CLI |
|---|---|---|---|
| Caps | Usage → Edit caps | `/caps`, `/caps usd_per_day 20` | `ferrule trust caps [--set KEY=VALUE…] [--yes]` |
| MCP server on/off | Extensions | `/mcp`, `/mcp off <name>` | `ferrule mcp disable\|enable <name>` |
| Remove an MCP server | Extensions | – | `ferrule mcp remove <name>` |
| Skill on/off | Extensions | `/skills`, `/skills off <name>` | `ferrule skills disable\|enable <name>` |
| Trust workspace hooks | Extensions | – (points to the page) | `ferrule hooks trust` / `untrust` |
| A task's schedule | Tasks → Schedule | – | `ferrule tasks schedule <id> "<cron>" [--tz Zone]` |
| A task's model | Tasks → Model | – | `ferrule tasks model <id> <ref>` |

- **Caps.** 0 turns a cap off. Lowering one needs no confirmation.
  Raising one or turning it off asks first: on the page, as `/caps … confirm`
  in Telegram, and with `--yes` (or a `y` at a terminal) in the CLI. The
  running hub enforces the new values from the next call.
- **MCP.** Disabling writes the name to `[mcp] disabled`. The server stays
  configured, and running agents stop it within seconds. A server the
  agent installed can be removed but not disabled.
- **Skills.** Disabling writes to `[skills] disabled`. The skill leaves the
  catalog from the next turn, since chats start fresh agents.
- **Hooks.** The page shows the file, its SHA-256 and, once a version has
  been trusted, a line diff against it. **Trust** sends the hash you were
  shown. If the file changed since then, it's refused and nothing is
  trusted. Trust is pinned to that hash: any later edit to the file needs
  trusting again. The trusted text is kept (0600) under
  `<data>/private/hooks-trusted/` for the next diff.
- **Tasks.** A schedule is checked before it's saved, and the next run
  moves at once. The built-in `ferrule-learn` task's schedule also goes
  to `[learning] schedule`/`timezone`, since that's what a restart reads.

## Settings

```toml
[dashboard]
enabled = true        # the gateway serves it on 127.0.0.1
port = 0              # 0: any free port
remote = "tunnel"     # "tunnel": /dashboard opens a cloudflared quick tunnel; "off": local only
idle_minutes = 30
session_hours = 12
link_minutes = 10

[models]
# Where prices come from when no connected provider is OpenRouter.
# Unset: OpenRouter's public list. "": none.
catalog_url = "https://openrouter.ai/api/v1/models"

[eval]
# The starter suite for Evaluate and `ferrule model eval` (an evals/starter directory).
suite = "/path/to/ferrule/evals/starter"
```

## Security in three lines

- The page listens on 127.0.0.1 only. The phone reaches it through a
  quick tunnel you open and close, and Cloudflare sees the (already
  redacted) traffic.
- The only way in is a one-use, 10-minute link sent to the owner's chat.
  It travels in the URL fragment, so no server logs it, and only its hash
  is stored.
- Sessions are HttpOnly, SameSite=Strict cookies bound to their host.
  Every change needs a CSRF header, a JSON body and a matching Origin.
  Unknown hosts are refused, which stops DNS rebinding.
