# M35 — Subscription sign-in: a ChatGPT plan and a Claude plan (design)

Status: design, 2026-09-27, branch `m35-subscriptions`. Written before the
code; where the build departs from it, see **As built** at the end.
User guide: [subscriptions.md](subscriptions.md).

Max wants setup to offer "sign in with your ChatGPT plan" and "sign in with
your Claude plan" next to "paste an API key", the way NanoClaw does. The two
plans are not the same kind of thing, and the design follows the rules each
vendor sets, not symmetry:

- **ChatGPT plan: native.** Ferrule signs in with OpenAI's own OAuth client
  (the one the Codex CLI uses) and calls the Codex backend from its own
  loop, through the M23 Responses driver.
- **Claude plan: through the unmodified `claude` binary.** Ferrule never
  sends a request to the Anthropic API with a subscription credential. Each
  turn runs inside `claude -p`, which Ferrule drives as a *model engine*
  and wires back into its memory, tasks, channels and approvals over MCP.

---

## 0. The rules this design is built on

**Anthropic** (legal and compliance page, verbatim):

> "Anthropic does not permit third-party developers to offer Claude.ai
> login into their own applications, or to route requests through Free,
> Pro, or Max plan credentials on behalf of their users. Moreover,
> developers may not collect, store, or intermediate Claude.ai credentials
> or session tokens — sign-in to a Claude account must complete through
> Anthropic's own flow."

The same page allows "an end user signing in to the unmodified Claude Code
binary with their own Claude subscription". Since January 2026 this is
enforced server-side: a subscription token that doesn't come from Claude
Code's own request shape is refused (`docs/research-report.md` §4.1).

`claude setup-token` prints a one-year OAuth token for a Pro, Max, Team or
Enterprise plan, used as `CLAUDE_CODE_OAUTH_TOKEN`. It only authenticates
model requests (locally configured MCP servers still work), `--bare` doesn't
read it, and every one of these outranks it: the cloud-provider variables,
`ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, `apiKeyHelper`.

**The red line, from those rules:** every model request made on a Claude
plan comes from the unmodified `claude` binary. Ferrule's own Anthropic
client never holds a subscription token. The guard in §7.6 enforces it for
the one way a user could get it wrong (pasting an `sk-ant-oat…` token as an
API key).

**OpenAI** tolerates third-party harnesses on a ChatGPT plan: Codex for
Open Source names OpenCode, Cline, pi and OpenClaw, which sign in with the
Codex CLI's OAuth client and call the same backend. That is tolerance, not
a contract. There is no published API for the backend, and it can change or
close without notice. The user guide says so where the option is offered.

---

## 1. Where the code lives

- **`ferrule-plans`** (new crate). Everything that knows about a plan:
  - `chatgpt::auth`: device code, PKCE with a loopback callback, the
    "paste the redirect URL" fallback, refresh with rotation, revoke, the
    JWT claims (account id, plan, expiry).
  - `chatgpt::store`: the sealed token file and the cross-process refresh
    lock.
  - `usage`: the plan's usage windows, persisted so status, doctor and the
    dashboard (other processes) see what the gateway saw.
  - `claude::engine`: the Claude Code engine (`Provider`), the stream-json
    parser, the process group, the per-session state.
  - `claude::bridge`: the loopback MCP server a turn's `claude` talks to
    (Ferrule's tools and the permission tool), and the stdio relay behind
    the hidden `ferrule claude-mcp` subcommand.
  - `claude::env`: the child's argv and environment (the hygiene rules of
    §7.3, in one tested function).
  - `claude::token`: the sealed setup-token and the date it was pasted.
- **`ferrule-providers`**: a `codex` flavour of the Responses driver
  (headers, a token source instead of a fixed key, one refresh on 401, the
  rate-limit headers and the `usage_limit_reached` 429). The SSE parser
  stays the M23 one. The `sk-ant-oat` refusal goes in `anthropic.rs`.
- **`ferrule-cli`**: the config, the M21 catalog (a plan entry needs no
  key), `ferrule login|logout chatgpt|claude`, the setup step, doctor,
  `ferrule status`/`/status`, the dashboard's model card, the ledger's plan
  columns and the hidden subcommand.

The new crate depends on `ferrule-core`, `ferrule-connections` (the M20
`Sealer`, PKCE and OAuth helpers are reused, not copied), `ferrule-sandbox`
and `ferrule-providers`.

## 2. Config and model ids

A plan is a provider with a `plan` field. The M21 reference form
`provider/model` does the rest:

```toml
[providers.chatgpt]
plan = "chatgpt"
model = "gpt-5.5"            # `chatgpt/gpt-5.5`

[providers.claude-code]
plan = "claude-code"
model = "sonnet"             # `claude-code/sonnet`; also opus, haiku, or a full id
```

- `base_url` and `api_key_env` become optional; a keyed provider without
  them is still an error, at load, with the same message as before.
  `chatgpt` defaults `base_url` to `https://chatgpt.com/backend-api/codex`
  (a test points it at a mock).
- A plan entry has no key. The M21 client cache keys it by the plan, and
  the credential is read per call from the store.
- `claude-code` model names pass through to `claude --model`, so `sonnet`,
  `opus` and `haiku` follow Claude Code's own aliases as they move.
- Sub-agents, scheduled tasks, `/model`, pins, `[models] fallback` and
  `[routing]` tiers name them like any other model.
- `[plans]` holds what isn't per model:

```toml
[plans.chatgpt]
issuer = "https://auth.openai.com"       # tests only
[plans.claude_code]
binary = "claude"                        # found on PATH; an absolute path works
config_dir = ""                          # "" = <data>/claude-code; "~/.claude" = reuse claude's own login
turn_timeout_minutes = 20
max_output_mb = 16
```

## 3. ChatGPT plan: sign-in

The protocol is the Codex CLI's, read from `openai/codex` at commit
**`67a709665ac7b50311b93e32612c9a8281684787`** (`codex-rs/login`,
`codex-rs/model-provider*`, `codex-rs/codex-api`). Nothing was sent to
OpenAI from this container: it can't reach `auth.openai.com` or
`chatgpt.com`.

| | Value (at that commit) |
|---|---|
| Issuer | `https://auth.openai.com` |
| Client | `app_EMoamEEZ73f0CkXaXp7hrann` (public, no secret) |
| Scope | `openid profile email offline_access api.connectors.read api.connectors.invoke` |
| Authorize extras | `id_token_add_organizations=true`, `codex_cli_simplified_flow=true`, `originator=codex_cli_rs` |
| PKCE | S256; verifier = 64 random bytes, base64url |
| Loopback redirect | `http://127.0.0.1:1455/auth/callback`, fallback port 1457 (the only two the client allows) |
| Code exchange | form POST `/oauth/token`, `grant_type=authorization_code` → `{id_token, access_token, refresh_token}` |
| Device code | JSON POST `/api/accounts/deviceauth/usercode` `{client_id}` → `{device_auth_id, user_code, interval}` (`interval` is a **string**); user goes to `https://auth.openai.com/codex/device`; poll `/api/accounts/deviceauth/token` (403/404 = pending, 15-minute limit) → `{authorization_code, code_verifier}`, then the code exchange with `redirect_uri = https://auth.openai.com/deviceauth/callback` |
| Refresh | **JSON** POST `/oauth/token` `{client_id, grant_type:"refresh_token", refresh_token}`; every field of the answer optional; a returned `refresh_token` replaces the old one |
| Permanent refresh failure | HTTP 401, `invalid_grant`, or `refresh_token_expired`/`_reused`/`_invalidated` |
| Revoke | JSON POST `/oauth/revoke` `{token, token_type_hint:"refresh_token", client_id}`; best effort |
| Account id | JWT claim `https://api.openai.com/auth`.`chatgpt_account_id` (id token), sent as `ChatGPT-Account-ID` |
| Plan | same claim object, `chatgpt_plan_type` |

**`ferrule login chatgpt`:**

1. **Device code first**, because it works on a headless box, over SSH and
   from Telegram. It prints the URL and code and polls.
2. `--browser`: PKCE with the loopback listener on 1455 (1457 if busy). It
   opens the browser when there is one and always prints the URL.
3. **Paste fallback** (`--paste`, and offered when the listener can't bind
   or the device endpoint answers 404): the user opens the URL anywhere,
   the browser lands on `http://127.0.0.1:1455/auth/callback?code=…` (which
   fails to load on a remote box, as expected), and they paste that URL
   back. `state` is checked exactly as the listener would.

The id token's claims are read without verifying its signature, as Codex
does. It came over TLS from the issuer straight into this process, and the
claims only choose a header value; nothing is authorised on them.

**Telegram:** the device code exists, so `/login chatgpt` works in the
owner chat, only there (M19 owner identity: the same check `/approve`
uses). The bot sends the URL and the code; the owner types the code on
OpenAI's page. Nothing secret crosses the chat: the code is useless without
the `device_auth_id`, which never leaves the process. The bot never asks
for a password or a token, and says so in the message.

**Store.** `<data>/private/chatgpt.sealed`, sealed with the M20 `Sealer`
(AES-256-GCM, key file `<data>/private/connections.key` or
`$FERRULE_CONNECTIONS_KEY`), AAD `ferrule-plan:chatgpt`. It holds
`{refresh_token, access_token, id_token, account_id, plan, email, expires_at,
last_refresh}`. `private/` is 0700, the file 0600 and written by tmp+rename;
the sandbox hides `private/` from commands already (M26).

**Refresh.** Before each request: when the access token's `exp` is within
5 minutes (or `last_refresh` is older than 8 days with no `exp`), refresh.
Refresh is serialized across processes (the gateway, a `ferrule run`, a
scheduled task, doctor):

1. take `<data>/private/chatgpt.lock` (the M20 `create_new` lock, taken
   over after 30 s);
2. re-read the store: if another process already refreshed, use its token
   and stop;
3. refresh, write the rotated tokens, release.

A permanent failure marks the store `signed_out` (the tokens are kept for
logout's revoke, never used again) and the owner is told once, in the
channel, to run `ferrule login chatgpt`. A transient one keeps the old
token while it's still valid.

**Logout.** Revoke the refresh token (best effort, 10 s), then delete the
store either way, and say which of the two happened.

## 4. ChatGPT plan: the model path

The M23 Responses driver, pointed at `https://chatgpt.com/backend-api/codex`
(`POST /responses`, SSE), in a `codex` flavour:

- headers: `Authorization: Bearer <access>`, `ChatGPT-Account-ID`,
  `originator: codex_cli_rs`, a `User-Agent` in the same shape
  (`ferrule/<version> (<os>; <arch>)`), `session-id` (the Ferrule session,
  also sent as `prompt_cache_key`);
- body: always `stream: true`, `store: false`,
  `include: ["reasoning.encrypted_content"]`; never `max_output_tokens` or
  `temperature` (the backend's request type has no field for them);
  `instructions` from the system messages as M23 already does;
- **401:** refresh once (through the lock) and retry once. A second 401 is
  a `Provider` error that says "sign in again", and the owner is told.
- **429 `usage_limit_reached`:** a `Transient` error whose `retry_after` is
  the time to `resets_at`. The loop's retry budget can't wait that long, so
  it goes straight to M21's fail-over; with no fallback the owner reads
  "the ChatGPT plan's usage limit is reached; it resets at 15:10 (in 2 h
  4 min)". `usage_not_included` (a plan without Codex) is a plain error
  that says which plan the account has.
- `x-codex-{primary,secondary}-{used-percent,window-minutes,reset-at}` on
  every response are written to the usage file (§6).
- models: `ferrule models list chatgpt` calls `GET /models?client_version=…`
  when signed in; the catalog also carries a short built-in list
  (`gpt-5.5`, `gpt-6-astra` at the pinned commit) so setup works before
  sign-in. `model` is free text: a new slug works without a Ferrule
  release.

The WebSocket transport, request compression and `x-codex-turn-state`
stickiness are left out: HTTP SSE is a supported transport of the same
endpoint, and the others are optimisations.

## 5. Money

A plan turn costs nothing per token; what it spends is the plan's window.
The ledger gets two columns:

- `plan`: `"chatgpt"` or `"claude-code"` on a plan row, absent otherwise;
- `notional_usd`: what the same tokens would cost at the model's API price
  (the M22 catalog's or the config's), when known.

`cost_usd` is **0** on a plan row, so the M19 spending caps, `ferrule
ledger` totals and the dashboard's spend don't count plan turns as money.
Reports show the notional figure beside it ("$0 · $0.84 at API prices").
For the engine, the `result` event's `total_cost_usd` is Claude Code's own
notional figure; it is used when Ferrule has no price for the model.

## 6. Usage windows

`<data>/plans/usage.json`, one object per plan, rewritten atomically on each
update:

```json
{"chatgpt": {"at": 1790500000, "plan": "pro",
             "windows": [{"name": "5h", "used_percent": 12.5, "resets_at": 1790506800},
                         {"name": "weekly", "used_percent": 40, "resets_at": 1790820000}],
             "limited_until": null}}
```

- ChatGPT: from the `x-codex-*` headers (the window's length names it:
  300 min is "5h", 10080 min is "weekly") and the 429 body.
- Claude Code: from `rate_limit_event` (`rate_limit_info.unifiedWindows`,
  `status`, `resetsAt`, `rateLimitType`).
- `/status`, `ferrule status`, doctor and the dashboard's model card read
  it. A reading older than 6 hours is shown with its age.

## 7. Claude plan: the Claude Code engine

### 7.1 What a turn is

A `claude-code/<model>` model is a `Provider` whose `complete()` runs the
unmodified `claude` as a child process:

```
claude -p <prompt> --output-format stream-json --verbose --include-partial-messages
       --model <model> [--resume <claude session id>]
       --append-system-prompt <ferrule's system prompt>
       --setting-sources user --strict-mcp-config --mcp-config <bridge json>
       --permission-mode default --permission-prompt-tool mcp__ferrule__approve
       --allowedTools <read-only built-ins,mcp__ferrule__*> [--disallowedTools <…>]
```

- **Never `--bare`.** It skips OAuth and keychain reads, so a plan can't
  sign in under it, and it is not the unmodified product behaviour the
  rules allow.
- **The persona** goes in with `--append-system-prompt`: Claude Code keeps
  its own system prompt (its tools depend on it), and Ferrule's workspace
  line, AGENTS.md baseline, skills catalog and learning notes are appended.
  Measured on 2.1.283 with Haiku: Claude Code's own prompt with no tools
  is 6,174 input tokens; a ~450-token appended persona added exactly 457.
  The overhead is the persona's own size. It is cached, like the base.
- **`--resume` per chat.** The engine keeps `<data>/claude-code/sessions.json`,
  Ferrule session → Claude Code session id (from the `init` event). A
  Ferrule turn sends only what claude hasn't seen: the new user message.
  With no id (first turn, or the chat switched to the engine from another
  model), the turn starts fresh, and a chat with history gets a bounded
  recap of it (the last 12 messages, 8 KB) ahead of the new message. A
  resume that fails ("No conversation found") is retried once, fresh, with
  the recap.
- **Streaming** comes from the partial-message events (`stream_event` with
  `content_block_delta`): `text_delta` goes to the M27 sink as text,
  `thinking_delta` and `input_json_delta` as progress. The final answer is
  the `result` event's text.
- **`/stop`** drops the turn's future (M19's kill switch does the same).
  The engine owns the child in a guard whose `Drop` kills the whole process
  group (the child is spawned as its own group leader), so claude's own
  subprocesses (its Bash commands, MCP servers) die with it. On Windows the
  child gets `kill_on_drop` and the M26 job ends the tree.
- **Limits:** a per-turn timeout (`turn_timeout_minutes`, default 20),
  and a cap on the bytes read from the child (`max_output_mb`, default 16):
  past it the group is killed and the turn fails with the reason.
- **Compaction:** claude compacts its own context. The engine's harness
  profile declares a window so large that Ferrule's compactor never
  triggers, and Ferrule's transcript keeps the turns as the owner saw them.

### 7.2 Credentials: three ways, one child

1. **Claude Code's own login.** Setup runs `claude` in the terminal with
   `CLAUDE_CONFIG_DIR` set to the engine's config dir, and the user signs in
   through Anthropic's own screens (`/login`). Ferrule never reads the
   credentials file or the keychain entry; it only runs claude with the
   same `CLAUDE_CONFIG_DIR`.
2. **A pasted setup-token.** The user runs `claude setup-token` (Anthropic's
   flow) and pastes the result into `ferrule login claude --token` (hidden
   input, terminal only). It is sealed in `<data>/private/claude-token.sealed`
   (AAD `ferrule-plan:claude-code`) with the date, and handed **only** to
   the `claude` child as `CLAUDE_CODE_OAUTH_TOKEN`.
3. **`CLAUDE_CODE_OAUTH_TOKEN` already exported.** Honoured. At startup
   Ferrule moves it out of its own process environment into memory (like
   (2) after unsealing), so hooks, MCP servers and commands, which inherit
   Ferrule's environment, never see it.

Which one is active is the first that exists: (3), (2), (1). Doctor says
which.

**Being honest about (2):** Ferrule stores and passes on a Claude
credential there. It never uses it itself and never sends it anywhere but
the child's environment, but it is still "storing" in the plain sense of
Anthropic's sentence. The guide says so and points anyone who wants no grey
area at (1) or (3), where Ferrule never holds the token at all. (2) exists
because the gateway usually runs as a service without the user's shell
environment.

**Never over Telegram.** No token paste and no Claude sign-in in any chat.
`/login claude` in a chat shows the state and says to run `ferrule login
claude` on the machine.

### 7.3 The child's environment and settings

Built in one function (`claude::env`), tested on its own:

- **Removed**, whatever `[sandbox] scrub_secret_env` says: `ANTHROPIC_API_KEY`,
  `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL`, `ANTHROPIC_CUSTOM_HEADERS`,
  `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX`,
  `CLAUDE_CODE_USE_FOUNDRY`, `ANTHROPIC_BEDROCK_BASE_URL`,
  `ANTHROPIC_VERTEX_PROJECT_ID`, `AWS_BEARER_TOKEN_BEDROCK`,
  `CLAUDE_CODE_OAUTH_TOKEN` (the inherited one; ours is set after),
  `CLAUDE_CONFIG_DIR`, and Claude Code's own session markers (`CLAUDECODE`,
  `CLAUDE_CODE_ENTRYPOINT`, `CLAUDE_CODE_SESSION_ID`, …) so a Ferrule run
  inside a Claude Code session doesn't confuse the child. Anything that
  outranks the plan would silently turn plan turns into API-billed ones;
  removing them makes the plan the only credential.
- **`apiKeyHelper`**: a settings key, not a variable. The engine's config
  dir is Ferrule's (below), and the engine refuses to start a turn when its
  `settings.json` has an `apiKeyHelper`, naming the file.
- **Set:** `CLAUDE_CODE_OAUTH_TOKEN` (ways 2 and 3), `CLAUDE_CONFIG_DIR`,
  `DISABLE_AUTOUPDATER=1` (the binary must stay the one the user installed;
  an update mid-turn is also a surprise), `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`
  (no telemetry or update checks from a background agent).

**`CLAUDE_CONFIG_DIR = <data>/claude-code`**, owned by Ferrule. Reasons:

- the user's `~/.claude/settings.json` may hold hooks, allow-rules, an
  `apiKeyHelper` or `bypassPermissions`, any of which would route around
  Ferrule's gates or the plan;
- the session files the engine resumes shouldn't mix with the user's own
  Claude Code history;
- it is writable for the sandboxed child without opening `~/.claude`,
  which M26's read-deny list keeps closed.

`config_dir = "~/.claude"` reuses claude's own login (and settings) for a
user who wants that; doctor then warns about the settings it finds.

**`--setting-sources user`**: only the (Ferrule-owned) user settings load.
A workspace's `.claude/settings.json` and `settings.local.json` are text
the model can write, and their hooks and permission rules would run and
allow things outside Ferrule's gates. Ferrule's own AGENTS.md baseline is
already in the appended prompt; claude still reads `CLAUDE.md` files as
memory, which is content, not policy.

**`--strict-mcp-config`**: only the bridge server loads. A workspace
`.mcp.json` would start unscanned servers outside Ferrule's MCP config and
sandbox rules. The user's Ferrule `[mcp]` servers stay Ferrule's.

### 7.4 Ferrule's tools inside claude: the bridge

Each turn starts a loopback MCP bridge (`127.0.0.1:0`, a random per-turn
token) and passes claude an `--mcp-config` with one stdio server:
`ferrule claude-mcp` (hidden subcommand) with the bridge's port and token
in its environment. The subcommand relays stdio to the bridge over TCP.
Stdio rather than an HTTP MCP server because claude's HTTP clients honour
the proxy variables Ferrule sets for egress (§7.7), and a loopback URL
through the proxy is refused as private for tools.

The bridge offers:

| Tool | Why |
|---|---|
| `approve` | the permission-prompt tool (§7.5), never offered to the model |
| `remember`, `recall`, `forget` | Ferrule's memory is the point of Ferrule; claude has none that the owner sees |
| `search_history` | past conversations in other chats and runs |
| `schedule_task`, `list_tasks`, `cancel_task` (whatever the agent has of M3's) | scheduling is Ferrule's, claude can't do it |
| `send_message` | reach another channel or chat, the gateway's job |
| `web_search` | when configured: one backend, one bill, one egress policy |
| `spawn_agent` and the board | sub-agents on whatever model they pick |

Only tools the agent already has are offered (so a read-only child gets no
`schedule_task`); the list is `req.tools` filtered by that table.
Ferrule's own file, shell and web-fetch tools are *not* offered: claude has
its own, gated by §7.5, and two of each confuses the model.

**How a bridged call runs:** it runs as a normal Ferrule tool call. When
claude calls `mcp__ferrule__remember`, the engine ends the current
`complete()` with that call as a `tool_call`, keeping the claude process
paused on its MCP request. The agent loop runs the tool (guard, M18 hooks,
transcript, ledger, OTel as for any tool) and calls `complete()` again with
the result. The engine sees the matching `Tool` message, answers the MCP
request and resumes the stream. Parallel calls queue up. So the tools
inside a claude turn get exactly the checks they get anywhere else.

### 7.5 Approvals

`--permission-prompt-tool mcp__ferrule__approve`: claude asks it before
any tool that its allow-list doesn't cover, with `{tool_name, input,
tool_use_id}`. The engine maps it to a Ferrule call and asks the M19 guard
(`Guard::before_tool_call`), then answers `{"behavior":"allow","updatedInput":
input}` or `{"behavior":"deny","message":…}`.

| Claude tool | Ferrule call |
|---|---|
| `Bash` | `shell {command}` (the M19 classifier: gated commands, bound hosts) |
| `Edit`, `MultiEdit`, `Write`, `NotebookEdit` | `write_file`, `changes_files` |
| `WebFetch` | `web_fetch {url}` |
| `WebSearch` | `web_search` |
| `Task`/`Agent` | `spawn_agent` |
| `mcp__ferrule__*` | allowed: gated again when it runs as a Ferrule tool |
| anything else | the tool name, `changes_files` unknown → declared tools rule |

- **Allowed without asking** (`--allowedTools`): `Read`, `Glob`, `Grep`,
  `LS`, `TodoWrite`, and the bridge's tools. Everything else goes through
  the prompt tool, so an ungated `ls` still passes, instantly, but claude
  can't write or run without Ferrule seeing it.
- **Disallowed** (`--disallowedTools`): in plan mode and for a read-only
  sub-agent, `Edit MultiEdit Write NotebookEdit`; `Bash` too when the
  sandbox isn't active there (plan mode relies on the read-only sandbox for
  commands).
- **Hooks (M18):** `turn_start`, `turn_end`, `model_call` and the Ferrule
  tools fire as always. Claude's built-in tools don't fire `pre_tool`/
  `post_tool`: they aren't Ferrule tools, and firing them from the prompt
  tool would fire only for the gated subset, which is worse than a clear
  "no". The guide's table says so.
- **Unattended runs** (tasks) refuse gated calls exactly as before.

### 7.6 Refusing a subscription token as an API key

`ferrule-providers`' Anthropic driver refuses a key that starts with
`sk-ant-oat` at build time and on every call, with: "this is a Claude
subscription token (from `claude setup-token`); ferrule can't use it as an
API key. Use the Claude plan through Claude Code: `ferrule login claude`."
Setup and doctor check the same.

### 7.7 Isolation

`claude` runs under the M26 sandbox, as a *helper* (`Sandbox::for_helper`):

- the workspace writable as for commands, plus the engine's config dir;
  `/tmp` as the policy says;
- the network on (it has to reach Anthropic), with the credential proxy's
  variables when the proxy is pointed at commands, so claude's requests,
  and its Bash commands', go through the M33 egress policy. The Anthropic
  hosts (`api.anthropic.com`, `claude.ai`, `console.anthropic.com`,
  `statsig.anthropic.com`) are added to the proxy's own-endpoint list when
  a `claude-code` provider is configured, so `default = "deny"` doesn't
  break the engine;
- the read denies (`private/`, the proxy CA key, the credential dirs)
  stay; `~/.claude` stays denied unless `config_dir` points there.

**The boundary, stated:**

- claude's own tools (Bash, Edit, …) run inside claude's process tree, so
  they get the same sandbox as claude, not the tighter one Ferrule gives a
  shell command. In practice the difference is the engine's config dir,
  which is writable and readable to claude's Bash. That dir holds claude's
  session files and, with way (1), claude's login.
- with ways (2) and (3) the token is in claude's environment, and claude
  passes its environment to its Bash commands. Claude Code can strip it
  (`CLAUDE_CODE_SUBPROCESS_ENV_SCRUB=1`, which needs bubblewrap on Linux);
  `[plans.claude_code] scrub_subprocess_env = true` sets it. Off by
  default because it fails the turn without bubblewrap.
- egress is advisory for a program that ignores `HTTPS_PROXY`, as for any
  command (docs/egress.md).
- on macOS, a claude login kept in the Keychain (way 1) needs Keychain
  access, which Seatbelt's profile may refuse; ways (2) and (3) don't
  need it. Not verified on a Mac.

The M26 socket allowlist doesn't affect the bridge: the relay reaches it
over loopback TCP, not a Unix socket.

### 7.8 Usage and limits

- The `result` event's `usage` goes to the ledger at `cost_usd = 0`, with
  `notional_usd` from Ferrule's price for the model or claude's
  `total_cost_usd`, and `plan = "claude-code"`.
- `rate_limit_event` goes to the usage file.
- `status: "rejected"` (or a result with `api_error_status` 429 and a
  limit message) is a `Transient` error with `retry_after` = time to
  `resetsAt`: M21 falls back, or the owner is told when it resets.
- Way (2)'s pasted date is kept; doctor warns from 30 days before the year
  is up, and fails after.

## 8. Surfaces

- **`ferrule setup`**, model step:

  | Choice | One line |
  |---|---|
  | ChatGPT plan | "Your ChatGPT Plus/Pro/Business plan, no API bill. Runs in ferrule's own loop; OpenAI tolerates this, it's not a contract." |
  | Claude plan (via Claude Code) | "Your Claude Pro/Max plan, through the claude CLI. Needs `npm install -g @anthropic-ai/claude-code`. Some ferrule features don't apply (table)." |
  | API key | "Any provider, pay per token, every ferrule feature." |
  | Local model | "Ollama, llama.cpp, LM Studio or vLLM on this machine or your LAN." |

  The Claude choice prints the feature table and then either runs `claude`
  for `/login`, takes a setup-token, or notes the exported one.
- **`ferrule login chatgpt [--browser|--paste]`**, **`ferrule login claude
  [--token]`**, **`ferrule logout chatgpt|claude`**.
- **`ferrule status`** and **`/status`**: per plan, signed in or not, the
  account/plan, the windows and when they reset, "limited until …".
- **Doctor**, a `plans` section:
  - ChatGPT: signed in, the plan, the access token's expiry, a refresh that
    works (`--ping-models` only; it rotates the token), the windows;
  - Claude: `claude` found and its version, which credential is active, the
    setup-token's age, and a **warning when `ANTHROPIC_API_KEY` or another
    variable that outranks the plan is set** (the engine strips it, but a
    `claude` the user runs by hand would bill the API).
- **Dashboard**: the model card shows "plan" and the windows.

## 9. What works under the engine

| | ChatGPT plan | Claude Code engine |
|---|---|---|
| Ferrule's loop, tools, hooks, approvals | yes | tools: claude's own, gated by approvals; Ferrule's memory/tasks/messaging via the bridge |
| Compaction | Ferrule's | claude's own |
| Repo map (M29) | yes | no (claude explores itself) |
| Per-edit lint (M29) | yes | no: claude's edits aren't Ferrule edits |
| Routing inside a turn (M25) | yes | no: one engine turn is one model; escalation to a Ferrule tier happens between turns |
| Fallback (M21) | yes | yes, between turns |
| Streaming (M27) | yes | yes |
| Sub-agents, tasks | yes | yes, each picks its model |
| `ferrule eval` | yes | runs, but graders see only the final text; not comparable to the API-model numbers |
| Sandbox (M26) | Ferrule's tools | claude runs in it as a helper (§7.7) |
| Cost | $0, notional shown | $0, notional shown |

## 10. Threat model

| Threat | Where it stops |
|---|---|
| The model reads the ChatGPT refresh token | sealed in `private/`, which commands can't read (M26); never in the environment, the transcript, the ledger or logs |
| The model reads the Claude setup-token | never in Ferrule's environment; only the claude child's. Inside claude, its Bash can read its env (§7.7), unless `scrub_subprocess_env` |
| A token ends up in a log or error | OAuth errors are fixed phrases plus the OAuth error code (M20's rule); the engine logs argv with the token var redacted, never the env; a test greps the transcript, ledger and log files for the token |
| A workspace file routes claude around the gates | `--setting-sources user` and `--strict-mcp-config`; a Ferrule-owned config dir |
| An API key silently outranks the plan | stripped from the child; doctor warns about the shell |
| Ferrule calls Anthropic with a plan token | no code path: the token only exists as the child's env; the `sk-ant-oat` guard for the user error |
| Someone else's Telegram account signs the bot in to their ChatGPT | owner-only `/login`; the device flow binds to whoever types the code on OpenAI's page, and the bot says which account signed in |
| Two processes refresh at once and one gets a revoked refresh token (rotation) | the file lock and re-read (§3) |
| claude hangs or floods | timeout, output cap, process-group kill |
| The bridge is reached by another local process | loopback only, a per-turn random token checked first, closed at turn end |

## 11. Failure modes

| Failure | What the owner sees |
|---|---|
| ChatGPT not signed in | "not signed in to the ChatGPT plan: run `ferrule login chatgpt`" (M21 treats it like a missing key) |
| Refresh refused | once per process, in the channel: "the ChatGPT sign-in expired or was revoked; run `ferrule login chatgpt`" |
| Usage limit | fallback model answers, and the owner is told the plan resets at …; with no fallback, that message |
| `claude` missing | "the claude CLI isn't installed: npm install -g @anthropic-ai/claude-code" |
| claude not signed in | the result's "Not logged in" becomes "Claude Code isn't signed in: run `ferrule login claude`" |
| Resume id gone | a fresh session with a recap, once |
| Turn timeout / output cap | the turn fails with which one, the group is killed |
| Backend shape changed | the SSE parser's error, with "the ChatGPT backend isn't a published API; see docs/subscriptions.md" |

## 12. Tests (hermetic)

- **ChatGPT** (`ferrule-plans/tests/chatgpt.rs`, `ferrule-providers`
  codex tests): a mock OAuth server on 127.0.0.1 (device code with a string
  interval, PKCE with the loopback listener, the paste fallback, refresh
  with rotation, revoked and expired refresh tokens), a mock Codex backend
  replaying the shapes above, two OS processes refreshing at once (exactly
  one refresh reaches the server), 401 → refresh → success, the usage-limit
  429 → a fail-over, and the token absent from the transcript, the ledger
  and the log output.
- **Engine** (`ferrule-plans/tests/engine.rs`, a `fake-claude` test binary
  that replays stream-json from a script and records its argv and env):
  resume, streaming, `/stop` kills the group (a grandchild's pid is gone),
  a permission-prompt round trip through a real M19 hub, a bridged Ferrule
  tool call, ledger and rate-limit capture, `rejected` → fail-over, env
  hygiene (API keys stripped, the token only in the child, never `--bare`,
  never in logs or in a Ferrule tool's env), and the `sk-ant-oat` refusal.
- Linux, macOS and Windows: all of it compiles everywhere; the fake-binary
  tests run everywhere (the process-group test is Unix-only).
- `#[ignore]`d live tests: one ChatGPT turn (needs a signed-in store and
  network), one engine turn on Haiku (needs `claude` and a plan).
- The starter eval must stay at engineered 20/20, naive 11/20, $0.98.

## 13. Out of scope

- Anthropic OAuth of any kind inside Ferrule (forbidden, §0).
- The Agent SDK: the brief allows it, but it's a Node or Python library, and
  a Rust runtime would drive it through a Node child anyway. `claude -p` is
  the same engine without a second runtime.
- The ChatGPT WebSocket transport, request compression, the `/wham/usage`
  endpoint (the headers carry the same windows on every turn), reset
  credits.
- Gemini and Copilot subscriptions.
- An interactive Claude Code session inside Telegram (`claude` without
  `-p`).
