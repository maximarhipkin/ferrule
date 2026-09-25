# M20: connections — the agent connects services itself (design)

Status: design, 2026-09-25, branch `m20-connections`. Follows M17 (hot-add
of MCP servers), M19 (trust & cost) and M19b (reliability).

## Why

The owner runs `ferrule gateway` on a server that takes no inbound
connections, and uses it from a phone over Telegram. Today, giving the
agent Jira or Gmail means SSH, a token pasted into `secrets.env`, and
`ferrule mcp add`. M20 makes it one tap:

1. the agent notices it needs a service (or the owner asks for it) and
   finds it in a built-in **catalog** of official remote MCP servers;
2. the owner's Telegram chat gets **one "Connect X" button**, with what the
   agent will be able to do;
3. the owner taps it, logs in at the vendor and approves;
4. ferrule catches the OAuth callback **without any inbound port**, stores
   the token encrypted, and refreshes it itself;
5. the service's tools appear in the running gateway at once (M17's
   hot-add), in the same chat session. The model never sees a token.

For services that only take an API key, the button opens a one-time form
that encrypts the key in the browser to a key only this ferrule holds; the
key never passes through the chat or the model.

Rules that hold throughout:

- **The agent can only ask.** Only the owner starts a connection (a tap
  or `/connect` in the owner chat, or the CLI at the machine) and only the
  owner's login at the vendor completes it.
- **Read-only by default** where the vendor has a read-only scope or
  endpoint. Where it doesn't, the Telegram button says so plainly, and
  every tool of a connected service that doesn't declare itself read-only
  goes through M19's approval gate (§9).
- **A token never appears** in the model's context, a tool result, the
  audit log, `/status`, the status file, a log line or an error message.
- `/connections` and `/disconnect <name>` work from Telegram.

Composio (a hosted broker holding the tokens) is a possible fallback for
services with no official MCP server. It is a follow-up, **not this
milestone**: it puts every token in a third party's hands, which is the
opposite of this design.

## 1. The flow

```
agent ── connection_request {service:"notion", reason} ──► ferrule
                                                           │ owner chat:
                                                           ▼
   "The agent asks to connect Notion: <reason>.
    Notion has no read-only mode: … every change asks you first."
    [ Connect Notion ]  [ Not now ]
          │ (URL button → the vendor's consent page, PKCE + state)
          ▼
   vendor login + consent ──302──► https://<relay>/cb?code&state
                                        │ stored ≤5 min, one read
   ferrule ── POST /poll (outbound) ────┘
          │ code + PKCE verifier ──► vendor token endpoint
          ▼
   token sealed (AES-256-GCM) in <data>/private/connections/
   hot-add: the MCP server starts, its tools join every agent
   owner chat: "Notion connected: 14 tools."
   requesting chat: woken with "[ferrule] Notion is connected …"
```

A connection is started by one of:

| who | how | may start? |
|---|---|---|
| the agent | tool `connection_request` | no — it sends the owner the button, and returns only "asked the owner" |
| the owner in Telegram | `/connect <name> [write]`, or the button | yes, only from the owner chat |
| anyone at the machine | `ferrule connections add <name>` | yes (the machine is the trust root, as for `ferrule mcp add`) |
| a non-owner chat | `/connect` | refused: "only the owner chat can connect services" |

The **owner chat** is M19's: `[trust] owner_chat`, else the first private
chat in `[gateway] telegram_allowed_chats`. With no owner chat there is no
Telegram connect; the CLI still works.

The button carries the consent URL itself (a Telegram URL button), so
approving is one tap. The URL holds no secret: the `state` in it is a hash
(§4), the PKCE verifier and everything else stay in ferrule's memory.
"Not now" is a callback button that withdraws the request.

**The same session sees the tools.** Connected servers join the process's
`ExtensionManager`, a `ToolSource` every agent re-reads per model call, so
a running session gets them at its next call — no restart, no new session.
When the request came from an agent, that chat is woken (M12's router
waker) with a one-line note naming the new tools, so the agent carries on
with the task it asked for.

## 2. Catalog

A TOML file in the repo, `crates/ferrule-connections/catalog.toml`,
compiled in. Adding a service is adding a table. The owner can add their
own entries in the config (`[[connections.custom]]`), and `/connect
https://mcp.example.com/mcp` or `ferrule connections add <url>` connects any
remote MCP server that supports OAuth discovery and DCR.

```toml
[[service]]
name = "linear"                     # the MCP server name → tools mcp__linear__*
title = "Linear"
url = "https://mcp.linear.app/mcp"
auth = "oauth"                      # "oauth" | "api_key"
client = "dcr"                      # oauth: "dcr" | "owner" (a client the owner made)
scopes = ["read"]                   # asked for by default
write_scopes = ["read", "write"]    # asked for by `/connect linear write`
read_only = "scope"                 # "scope" | "endpoint" | "header" | "none"
docs = "https://linear.app/docs/mcp"
```

API-key entries say how the key is sent and where the owner makes one:

```toml
[[service]]
name = "github"
auth = "api_key"
url = "https://api.githubcopilot.com/mcp/"
header = "Authorization"
header_value = "Bearer {key}"
headers = { "X-MCP-Readonly" = "true" }    # dropped in write mode
key_url = "https://github.com/settings/personal-access-tokens/new"
```

Seed (checked on 2026-09-25 against each vendor's docs, and each host's
OAuth metadata fetched live):

| name | URL | auth | read-only by default |
|---|---|---|---|
| `atlassian` | `https://mcp.atlassian.com/v2/mcp` | OAuth 2.1 + DCR | **none** — no read-only scope; writes gated |
| `attio` | `https://mcp.attio.com/mcp` | OAuth + DCR, OAuth only | **none** — writes gated |
| `gmail` | `https://gmailmcp.googleapis.com/mcp/v1` | OAuth, owner's Google client (no DCR) | `gmail.readonly`; write adds `gmail.compose` |
| `gdrive` | `https://drivemcp.googleapis.com/mcp/v1` | OAuth, owner's Google client (no DCR) | `drive.readonly`; write adds `drive.file` |
| `github` | `https://api.githubcopilot.com/mcp/` | API key (fine-grained PAT) as Bearer | `X-MCP-Readonly: true` |
| `notion` | `https://mcp.notion.com/mcp` | OAuth + DCR, OAuth only | **none** — writes gated |
| `linear` | `https://mcp.linear.app/mcp` | OAuth + DCR | scope `read`; write adds `write` |

Kept all six vendors; dropped nothing. Notes that the build carries into
the catalog and the button text:

- Atlassian's documented endpoint is now `/v2/mcp` (the `/v1/mcp` path of
  older guides still answers). Its org admins can restrict OAuth clients by
  redirect domain; a relay on `*.workers.dev` may need adding there.
- Google's Gmail and Drive MCP servers are a Developer Preview. They need
  the owner's own OAuth client (§3.3).
- GitHub's remote server does OAuth only for a list of known hosts, with
  app credentials ferrule doesn't have, so the catalog uses a PAT through
  the key form. A GitHub OAuth app of the owner's own is a follow-up.
- Linear also takes an API key as Bearer; the catalog uses OAuth.
- Attio documents no token revocation, so disconnecting Attio deletes the
  token here and tells the owner where to revoke it.

## 3. OAuth

### 3.1 Always

- **PKCE S256** on every flow, with a 32-byte random verifier. An
  authorization server whose metadata lists `code_challenge_methods_supported`
  without `S256` is refused.
- **`state`** is the relay slot id (§4): unguessable, single use, and the
  CSRF check.
- **`resource`** (RFC 8707) = the MCP server URL, on the authorize and
  token requests, as the MCP authorization spec asks. A catalog entry can
  turn it off (`resource_param = false`, set for Google).
- The authorization code is exchanged once; a replayed code finds no
  pending flow and is refused before any network call.

### 3.2 Discovery and DCR

For a service without pinned endpoints:

1. `GET <origin>/.well-known/oauth-protected-resource<path>`, then without
   the path, and the `resource_metadata` of a 401's `WWW-Authenticate` →
   `authorization_servers[0]`. Missing (Atlassian): the MCP server's origin
   is the authorization server.
2. `GET <issuer>/.well-known/oauth-authorization-server` (RFC 8414, path
   inserted), else `/.well-known/openid-configuration`.
3. **DCR** (RFC 7591) when there's a `registration_endpoint`: one client
   per connection, `token_endpoint_auth_method = none`, redirect URI = the
   callback path in use (§5), `client_name = "Ferrule"`. Its id (and a
   secret if one is issued) is kept, sealed, with the connection.

### 3.3 Fixed clients (Google)

Google offers no DCR. The owner creates one OAuth client once, in a Google
Cloud project of their own:

- type **Web application**, with the relay's `https://<relay>/cb` and the
  loopback `http://127.0.0.1:8976/callback` as redirect URIs;
- the Gmail/Drive APIs and the MCP services (`gmailmcp.googleapis.com`,
  `drivemcp.googleapis.com`) enabled;
- a consent screen. **An External app left in "Testing" gets refresh
  tokens that expire after 7 days** — every Google connection would need
  reconnecting weekly. Use **Internal** (Workspace accounts) or publish the
  app.

The id and secret go into the secrets file, never the config:
`FERRULE_GOOGLE_CLIENT_ID`, `FERRULE_GOOGLE_CLIENT_SECRET` (the sandbox
scrubs `*SECRET*` names from every child's environment). `ferrule
connections add gmail` at a terminal asks for them if missing; from
Telegram, `/connect gmail` without them answers with these steps. Google
also gets `access_type=offline` and `prompt=consent`, or it issues no
refresh token.

### 3.4 Tokens: storage, refresh, revocation

- Stored in `<data>/private/connections/connections.json` (the private dir
  is hidden from the sandbox and from file tools): per connection its
  name, catalog entry or URL, scopes granted, mode (read/write), state,
  times, and a **sealed** blob with the tokens and client credentials.
  Sealed with AES-256-GCM (ring), a fresh nonce per write, the connection
  name as associated data, under a 32-byte key in
  `<data>/private/connections.key` (0600) or `$FERRULE_CONNECTIONS_KEY`.
  Honestly: the key sits on the same disk. It keeps tokens out of backups
  of the store, out of `grep`, and out of anything that copies the JSON;
  it doesn't stop someone who can read the private dir as the ferrule
  user.
- Not in the config: a system service's `/etc/ferrule` is read-only to it,
  and the config is meant to be shareable.
- **Refresh** happens before expiry (60 s early) and once on a 401. A
  refresh is serialized across processes by a lock file next to the store,
  and re-reads the store first: if another process already refreshed, its
  token is used. A rotated refresh token replaces the old one.
- **Refresh fails for good** (`invalid_grant`, a revoked or expired grant):
  the connection is marked `needs_reconnect`, its tools are suspended
  (removed from the manager), and the owner gets **one** message with a
  **Reconnect** button. Once per break, recorded in the store, so restarts
  and other processes don't repeat it. A transient failure (network, 5xx)
  fails that one tool call and changes nothing.
- **Revocation** on disconnect: RFC 7009 at the metadata's
  `revocation_endpoint` (refresh token, then access token); Google's
  `oauth2.googleapis.com/revoke`. Without one (Attio, a GitHub PAT) the
  token is deleted here and the reply says where to revoke it at the
  vendor. Disconnect succeeds even if revocation fails; the reply says so.

### 3.5 Injection

A connected service runs as an HTTP MCP server inside the ferrule process
(M17's HTTP transport, never a child process, so the token is never in an
environment). `McpServerConfig` gains a non-serialized `auth` field: a
bearer source the transport asks for a token **per request**. On a 401 it
asks once for a refreshed one and retries. The transport already marks
header values sensitive; it also scrubs the current token out of any
server error text before it becomes an `McpError` — the only path from a
response to the model.

## 4. The relay (callback path 3, the default)

A small Cloudflare Worker, source in `relay/` (one ES module, no
dependencies, readable in one sitting), deployed by the owner to their own
account with `ferrule connections relay deploy`.

### Contract

| request | who calls it | does |
|---|---|---|
| `GET /health` | ferrule | `{"ok":true,"relay":"ferrule-relay","v":1}` |
| `POST /poll` + `Authorization: Bearer <relay key>`, body `{"secret":"<b64url 32 bytes>"}` | ferrule | opens the slot `id = b64url(SHA-256(secret))` if new (open 15 min); returns its value **and deletes it** (200), 204 while empty, 410 once read (a value-free "used" mark stays until the slot's window ends, so a replayed callback gets 409). An expired slot is gone; polling it opens a fresh, empty one |
| `GET /cb?state=<id>&code=…` (or `error=…`) | the vendor's redirect, in the owner's browser | writes `{code, error, error_description, iss}` into an **open** slot `id = state`: first write wins, ≤ 4 KiB; answers a static page ("done, go back to Telegram"), never echoing the code |
| `GET /key` | the owner's browser | the static API-key form (§6) |
| `POST /drop/<id>` | the key form | writes the encrypted key into an open slot: first write wins, ≤ 8 KiB |

- **Keys:** ferrule makes a random 32-byte `secret` per flow. The slot id
  is its SHA-256, and that id is the OAuth `state`. Seeing the state (in
  the consent URL, the browser history, the vendor's logs) lets one write
  to the slot, once, and never read it.
- **The relay key** (32 random bytes, a Worker secret, and
  `FERRULE_RELAY_KEY` in ferrule's secrets file) is needed to open or read
  a slot. Strangers can't create slots, so the relay is **not an open
  store**: a write to a slot nobody opened is refused.
- **TTL:** a value lives 5 minutes after it's written, an open slot 15
  minutes; a Durable Object alarm deletes everything past that. One read
  deletes at once.
- **No logging** of codes or bodies: the Worker has no `console.log`, and
  the deploy turns Workers Logs off.
- **Limits:** state must be 43 base64url characters; bodies over the size
  limits are refused unread.

### Durable Objects, not KV

Workers KV is eventually consistent: a write can take up to ~60 s to be
seen in another location, a read of a missing key is cached (so a poll
before the redirect can hide the code for a minute), deletes spread
lazily, and there's no compare-and-set for "first write wins". A Durable
Object per slot (`idFromName(id)`) is one single-threaded instance with
strongly consistent storage: read-then-delete is exact, first write wins
is a plain check, and the TTL is an alarm. SQLite-backed DOs are on the
Workers free plan. Cost per flow: one DO, a few hundred small requests
(ferrule polls every 2 s while the button is open).

### Threat model

- **The relay operator** (whoever controls the Cloudflare account) could
  read authorization codes. A code alone is useless without the PKCE
  verifier, which never leaves ferrule. The operator also serves the key
  form's JavaScript, so a malicious operator could change it to leak API
  keys: the key form trusts the operator; the OAuth path doesn't. That is
  why each owner deploys their **own** relay, and why there is **no
  default relay URL**.
- **Someone who learns a state** (from the consent URL) can write a fake
  code into the slot first. ferrule then fails the exchange (the code
  doesn't match the verifier), tells the owner, and the owner taps again.
  Denial, not compromise.
- **Someone without the relay key** can't open, read or use slots. They
  can spend the owner's free-plan request quota with junk requests (a DoS
  on connecting, nothing more; the relay holds nothing of value for more
  than 5 minutes).
- **Cloudflare** sees the traffic, as for any Worker.

### Deploy and check

`ferrule connections relay deploy [--name ferrule-relay]`:

- reads `CLOUDFLARE_API_TOKEN` (or `CLOUDFLARE_API_TOKEN_WORKERS`) and
  `CLOUDFLARE_ACCOUNT_ID` from the environment or the secrets file; never
  prints them;
- uploads `relay/worker.js` as an ES module with a `SLOTS` Durable Object
  binding and the relay key as a secret binding (`PUT
  /accounts/{id}/workers/scripts/{name}`, multipart). The DO migration
  (`new_sqlite_classes`) is sent only when the script doesn't have it yet
  (read from `GET …/workers/scripts`), because Cloudflare rejects a
  repeated migration tag. Re-running it is safe: same name, same key;
- turns on the `workers.dev` route, and writes `[connections] relay_url`
  to the config and `FERRULE_RELAY_KEY` to the secrets file.

`ferrule connections relay check` opens a slot, writes a test value
through `/cb`, reads it, reads again (204), and reports each step. `ferrule
doctor` runs the cheap half (`/health` and an authenticated poll of an
empty slot).

## 5. Fallbacks when the relay isn't there

Chosen when a flow starts, in this order:

1. **Relay** — `relay_url` set, `FERRULE_RELAY_KEY` present, `/health`
   answers within 5 s.
2. **Quick tunnel** — the relay is missing or down and `cloudflared` is
   found (`[connections] cloudflared`, else `PATH`). ferrule listens on a
   loopback port, runs `cloudflared tunnel --url http://127.0.0.1:<port>`,
   reads the `https://*.trycloudflare.com` URL from its output, and uses
   `<tunnel>/cb` as the redirect URI. The tunnel lives only while that
   consent is pending (≤ 15 min). It works only for **DCR** services, where
   ferrule registers the redirect URI itself; Google's client has fixed
   redirect URIs, so a Google flow skips it.
3. **Paste-back** — neither. The redirect URI is the loopback
   `http://127.0.0.1:8976/callback` (`[connections] loopback_redirect`).
   After consent the owner's phone browser fails to open it; the button
   message says: "copy the address from the browser bar and send it here".
   ferrule intercepts that message in the owner chat before any agent sees
   it, checks the state against a pending flow, and exchanges the code.
   Safe: the code is single use and bound to the PKCE verifier. A replayed
   or edited URL (unknown state, a flow already finished, a code that the
   vendor refuses) is refused. Any owner-chat message that looks like a
   callback URL (`state=` and `code=`) is intercepted, matching or not, so
   a code never reaches a model.

The owner sees which one is used ("via the relay", "via a temporary
tunnel", "paste the address back here").

## 6. API-key services: the one-time form

For `auth = "api_key"`, the button opens

```
https://<relay>/key#s=<slot id>&k=<one-time P-256 public key>&t=GitHub
```

The fragment never leaves the browser (it isn't sent to the relay). The
form's script generates its own ephemeral P-256 key, derives a shared key
with ECDH → HKDF-SHA256 (salt = slot id, info = `ferrule key form v1`) →
AES-256-GCM, encrypts the key with the slot id as associated data, and
POSTs only `{v, epk, iv, ct}` to `/drop/<slot id>`. ferrule, polling the
slot as for OAuth, holds the matching private key in memory only
(single-use), decrypts, **tests the key against the MCP server** (an
`initialize` + `tools/list`), and seals it like a token. The relay stores
only ciphertext it can't read.

Honest limit: the relay operator serves the form's script (§4). Without a
relay: the tunnel serves the same form locally; with paste-back only, the
answer is `ferrule connections add github` at a terminal (hidden input),
never the chat.

## 7. The agent's tools

Registered for the root agent of a gateway or chat session only (never a
sub-agent, never in plan mode, never in eval):

- `connection_request {service, reason, write?}` — the service is a
  catalog name or an `https://` MCP URL. Sends the owner the button and
  returns one of: "Asked the owner to connect Notion; its tools appear once
  they approve", "Notion is already connected", "a request for Notion is
  already waiting", "unknown service — known: …", "no owner chat to ask".
  Never a URL, a state or a token.
- `connection_list {}` — the catalog with each service's state
  (`connected`, `needs_reconnect`, `waiting for the owner`, `not
  connected`) and tool count.

Rate: at most one pending request per service, and a request the owner
declined can't be repeated for 10 minutes.

## 8. Telegram

- **Buttons.** `ChannelCapabilities` gains `buttons`. `Channel` gains
  `send_with_buttons(msg, rows)`, defaulting to the text with each URL on
  its own line, so other channels keep working. Telegram sends an
  `inline_keyboard`: URL buttons for consent and callback buttons (data ≤
  64 bytes) for "Not now" / "Reconnect". A tapped callback button becomes
  an ordinary inbound message whose text is the button's data (e.g.
  `/connect notion`), from the tapping user in that chat, after the same
  allowed-chats check; the tap is answered (`answerCallbackQuery`) so the
  spinner stops. Button data is treated exactly like typed text: it grants
  nothing a typed command wouldn't.
- **Interceptors** gain a defaulted `intercept_reply` returning text plus
  button rows; the gateway uses it. M19's owner door and M21's `/model`
  are untouched.
- **Commands** (owner chat only; elsewhere a one-line refusal):
  - `/connect <name|url> [write]` — the button.
  - `/connections` — each connection: state, read/write, tool count,
    connected since, token expiry; a Reconnect button per broken one.
  - `/disconnect <name>` — revokes, deletes, removes the tools, and says
    which of those happened.
- `/status` gains a Connections section (names and states only).

## 9. Approval for writes

M19's gate classifies shell commands. It gains one more kind: **a tool of
a connected service that doesn't declare `readOnlyHint`**, when
`[connections] gate_writes` (default on). The owner is asked, as for
`rm -rf`, naming the tool and the service; an unattended run (a scheduled
task) is refused, as for any gated call. The hint is only a hint, which is
why the default is read-only scopes where they exist: the gate is the
speed bump, the scope is the wall.

## 10. CLI, setup, doctor

```
ferrule connections list                 # the catalog, and what's connected
ferrule connections add <name|url> [--write]   # consent from a terminal
ferrule connections remove <name>        # revoke + delete
ferrule connections test <name>          # refresh if due, initialize, tools/list
ferrule connections relay deploy [--name N]
ferrule connections relay check
```

`add` at a terminal prints the consent URL, waits on the relay (or
tunnel), and also accepts the redirected URL pasted at the prompt; for an
API key it reads hidden input. A running gateway picks the new connection
up within 2 s (M17's follower watches the store too).

`ferrule doctor` gets one line: the relay (reachable, key accepted) or
which fallback applies, whether `cloudflared` is found, the connections
and any that need reconnecting, and a missing Google client if a Google
service is connected. `ferrule setup` is unchanged beyond a pointer to
`ferrule connections relay deploy` (M21 is reworking it).

## 11. One model for every surface (M22's dashboard)

All surfaces — the agent tools, Telegram, the CLI, and M22's dashboard —
call one `Connections` service in `crates/ferrule-connections`:

- **Read model**, serializable: `snapshot()` → the catalog (name, title,
  auth, read-only story, docs), each connection (name, service, mode,
  scopes granted, state, connected at, token expires at, last refresh,
  last error — as fixed phrases, tool count), pending flows (service, who
  asked, path in use, started, expires), and the relay (URL, reachable,
  fallback in use). Never a token, a code, a state or a client secret.
- **Mutations:** `request(service, write, reason, by)` (agent),
  `start(service, write, by)` → a consent link (owner only),
  `decline(service)`, `complete_pasted(url)`, `disconnect(name)`,
  `test(name)`. Each takes who is acting; the owner check lives in the
  service, not in the surface.

## 12. Audit trail

M19's audit log (`<data>/trust/audit.jsonl`) gains `connection_requested`,
`connection_started`, `connection_connected`, `connection_failed`,
`connection_declined`, `connection_refresh_failed`,
`connection_disconnected` and `connection_refused` (a non-owner tried).
Each has the service, the path (relay/tunnel/paste/terminal), who acted
(owner chat, agent session, terminal), scopes and a fixed-phrase reason.
Never a code, state, token or key. `ferrule trust audit` shows them.

## 13. Eval stays hermetic

`ferrule-eval` doesn't depend on `ferrule-connections` and builds its
agents without the extension manager, so no eval run can see a connected
service, call the relay, or send a button. A test runs the starter suite
with a store holding a connection to a mock server and a relay URL
pointing at a mock, and asserts neither is ever contacted.

## 14. Failure modes

| what | what happens |
|---|---|
| relay down | the tunnel if `cloudflared` is found and the service does DCR, else paste-back; the message says which |
| `cloudflared` missing | paste-back |
| consent abandoned | the flow expires after 15 min: the slot is gone, the button's link is dead, nothing is stored; `/connect` again makes a new one |
| code expired or refused by the vendor | "Notion refused the login (invalid_grant); tap Connect again" with a new button; nothing stored |
| refresh revoked | one message with a Reconnect button; tools suspended; `/connections` shows it |
| two connects of one service | the newer flow replaces the older; the old link stops working |
| the agent asks for a service already waiting | "already waiting for the owner"; no second button |
| a hand-edited config while running | `[connections]` settings are re-read at each flow start; a `[[mcp.servers]]` entry with a connection's name wins, and the connection is shown as shadowed |
| a hand-edited or corrupt store | kept running on what was loaded, one warning; never overwritten until it parses again |
| the store key is missing but the store isn't | connections show `needs_reconnect` (their tokens can't be opened); nothing is deleted |

## Defaults (decisions for Max)

- The catalog: all six vendors kept, seven entries (Gmail and Drive are
  separate servers). GitHub by PAT, not OAuth.
- Read-only by default: Gmail/Drive read scopes, Linear `read`, GitHub
  read-only header. Atlassian, Notion and Attio have no read-only mode;
  their writes go through the approval gate.
- **No default relay URL.** Each owner deploys their own; the relay
  operator could otherwise serve a key-stealing form.
- Commands: `/connect`, `/connections`, `/disconnect`.
- Relay TTLs: 5 min for a code, 15 min for an open slot.
- Paste-back loopback port 8976.
