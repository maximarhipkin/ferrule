# Subscriptions: a ChatGPT plan or a Claude plan

Ferrule can run on a plan you already pay for, instead of an API key:

- **ChatGPT plan** (Plus, Pro, Business): ferrule signs in to your ChatGPT
  account and calls the model from its own loop. Every ferrule feature
  works.
- **Claude plan** (Pro, Max, Team, Enterprise): every model request is made
  by the unmodified `claude` binary (Claude Code). Ferrule never calls the
  Anthropic API with your plan. Some ferrule features work differently
  (table below).

A plan turn costs **$0** on the ledger. What it spends is the plan's usage
window. The design and the reasons behind it are in
[m35-subscriptions.md](m35-subscriptions.md).

## Which to pick

| | ChatGPT plan | Claude plan | API key |
|---|---|---|---|
| Bill | your plan | your plan | per token |
| Model calls come from | ferrule | the `claude` binary | ferrule |
| Every ferrule feature | yes | no, see below | yes |
| Standing | OpenAI **tolerates** it; not a contract | allowed by Anthropic's terms, through Claude Code only | the vendor's API terms |
| Sign in over Telegram | yes, `/login chatgpt` (device code, owner only) | **never** | n/a |

## ChatGPT plan

```sh
ferrule setup                     # Model provider → ChatGPT plan
# or
ferrule login chatgpt             # a code to type at auth.openai.com/codex/device
ferrule login chatgpt --browser   # a browser on this machine, through 127.0.0.1:1455
ferrule login chatgpt --paste     # open the link anywhere, paste back the URL it lands on
```

The device code works on a headless box, over SSH, and from the owner's
Telegram chat (`/login chatgpt`). The bot sends the link and the code, and
never asks for a password or a token.

This makes a `[providers.chatgpt]` entry with no key:

```toml
[providers.chatgpt]
plan = "chatgpt"
model = "gpt-5.5"          # name it as chatgpt/gpt-5.5
```

`ferrule login chatgpt` and setup list the models your plan offers. `model`
is free text, so a new one works without a ferrule release.

**What OpenAI allows.** Ferrule signs in with the same public OAuth client
the Codex CLI uses, and talks to the same backend. OpenAI tolerates
third-party tools doing this (it names several in Codex for Open Source),
but **that is tolerance, not a contract**. The backend has no published API
and can change or close without notice. If it breaks, the error says so,
and an API key keeps working.

**Where the sign-in lives.** `<data>/private/plans/chatgpt.json`, sealed
(AES-256-GCM, the same key as ferrule's connections). The sandbox hides
`private/` from the model's commands. Ferrule refreshes the token before it
expires, one process at a time. When the sign-in expires or is revoked, the
owner is told once to run `ferrule login chatgpt`. `ferrule logout chatgpt`
revokes the token at OpenAI and deletes the file.

## Claude plan

Anthropic's terms allow "an end user signing in to the unmodified Claude
Code binary with their own Claude subscription", and they don't allow any
other app to sign in to Claude.ai or to use plan credentials itself. So
ferrule doesn't. On the Claude plan, **each turn runs the real `claude`
binary** (`claude -p`, never `--bare`), and ferrule wires its memory,
tasks, messaging and approvals into it.

You need Claude Code:

```sh
npm install -g @anthropic-ai/claude-code
```

### Signing in: three ways

1. **Claude Code's own sign-in** (recommended). `ferrule login claude` runs
   `claude auth login --claudeai` at your terminal, and you sign in through
   Anthropic's own pages. Claude Code keeps the credential in ferrule's own
   Claude config dir (`<data>/claude-code`). **Ferrule never reads or holds
   it.**
2. **A setup-token.** Run `claude setup-token` (Anthropic's flow), then
   `ferrule login claude --token` and paste it (hidden input at a terminal,
   or one piped line). This is for a gateway running as a service, where
   the other two don't reach.
3. **`CLAUDE_CODE_OAUTH_TOKEN` already exported.** Ferrule takes it out of
   its own environment at startup, so hooks, MCP servers and commands never
   inherit it, and hands it only to `claude`.

When more than one exists, (3) wins, then (2), then (1). `ferrule doctor`
says which one is active.

**Be clear about way 2: ferrule stores a token.** It is sealed in
`<data>/private/plans/claude-code.json`. Ferrule never uses it itself and
never sends it anywhere but the `claude` child's environment. That is still
storing a Claude credential in the plain sense of Anthropic's terms. If you
want no grey area, use way 1 or 3, where ferrule never holds the token.
Doctor warns 30 days before a setup-token's year is up.

`ferrule logout claude` deletes a stored token and signs Claude Code out of
ferrule's config dir. It doesn't touch your own `~/.claude`.

**Never over Telegram.** There is no token paste and no Claude sign-in in
any chat. `/login claude` in a chat shows the state and says to run
`ferrule login claude` on the machine.

**A setup-token is not an API key.** Put an `sk-ant-oat…` token in an
Anthropic provider's key and ferrule refuses it, on every call, in setup
and in doctor, and points you here.

The provider:

```toml
[providers.claude-code]
plan = "claude-code"
model = "sonnet"           # or opus, haiku, a full model id; name it claude-code/sonnet

[plans.claude_code]        # all optional
binary = "claude"          # found on PATH; an absolute path works
config_dir = ""            # "" = <data>/claude-code; "~/.claude" reuses your own login
turn_timeout_minutes = 20
max_output_mb = 16
```

### What works on the Claude plan

| ferrule feature | on the Claude plan |
|---|---|
| tools and approvals | claude's own tools; each one ferrule's approvals would gate is asked of ferrule first (`rm -rf` needs the owner) |
| memory, tasks, messaging | yes: ferrule's tools, bridged into claude over MCP |
| compaction | claude's own |
| repo map, per-edit lint | no: claude explores and edits by itself |
| routing inside a turn | no. Fallback to another model between turns: yes |
| streaming, sub-agents, scheduled tasks | yes |
| chats | each chat resumes its own Claude Code session |
| sandbox | claude runs inside ferrule's sandbox, and its commands with it |
| `ferrule eval` | runs, but graders see only the final text; not comparable with API-model scores |
| cost | $0 on the ledger, with Claude Code's notional price beside it |

### What ferrule does to keep the plan the plan

- The child's environment loses `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`,
  `ANTHROPIC_BASE_URL`, the Bedrock/Vertex/Foundry switches and any
  inherited `CLAUDE_CODE_OAUTH_TOKEN`. Each of those would outrank the plan
  and quietly bill the API. Doctor warns when one is set in your shell,
  because a `claude` you run by hand would still use it.
- Claude reads only ferrule's own config dir, user settings only
  (`--setting-sources user`) and ferrule's MCP config only
  (`--strict-mcp-config`). A file in the workspace can't route it around
  the gates.
- Ferrule's own commands can't read Claude Code's config dir, `~/.claude` or
  `~/.claude.json`. Only the `claude` child sees them.
- With egress on, Anthropic's hosts are allowed on port 443.
- A turn that runs past `turn_timeout_minutes`, or writes more than
  `max_output_mb`, is stopped, and so is everything it started.

### Limits

When the plan's usage limit is reached, the next model in `[models]
fallback` answers, and the owner is told when the plan resets. With no
fallback, that message is the answer. `ferrule status`, `/status`, doctor
and the dashboard show each plan's windows and when they reset.

## Status and doctor

```sh
ferrule status      # per plan: signed in or not, the account, the windows
ferrule doctor      # the plans section
```

Doctor checks:

- ChatGPT: signed in, the plan, the token's expiry, and the windows.
  `--ping-models` also tries a refresh (it rotates the token).
- Claude: `claude` found and its version, whether it's signed in (asked of
  `claude auth status`, never with a model call), which credential is
  active, the setup-token's age, and any variable that outranks the plan.
