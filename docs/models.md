# Models

Ferrule can have several models connected at once. One of them is the
default. A chat, a scheduled task, a role or a single sub-agent can run on
another one. The design, and the reasons behind it, are in
[m21-models.md](m21-models.md).

## Naming a model

Everything that takes a model takes a **ref**:

| you write | it means |
|---|---|
| `openai/gpt-5.2-mini` | that model on that provider (only the first `/` splits, so `openrouter/anthropic/claude-sonnet-5` works) |
| `openai` | the provider's own `model`, as before M21 |
| `fast` | an alias from `[models.aliases]` |
| `gpt-5.2-mini` | a model id, when only one provider has it |

Only a **connected** model can be named: a provider's own model, or one
listed under it. `ferrule model list` shows them all.

## Connecting models

The easiest way is `ferrule setup` → Model provider:

- **Add a provider** has presets for OpenAI, Kimi, OpenRouter, DeepSeek,
  Google Gemini, Groq, Anthropic and Ollama, and a custom endpoint.
- A provider's menu has **Add another model on it** (same key, optional
  alias) and **Test it**.
- **Default model** appears once more than one model is connected.
- **A server running here** (Ollama, llama.cpp, LM Studio or vLLM) is
  found and offered first. Setup checks that the model can call tools and
  that the server's window is as big as ferrule plans for, and offers the
  fix when it isn't ([local-models.md](local-models.md)).

Setup makes one real call after each change and says plainly if it failed
(the key was refused, the model isn't known, the endpoint can't be
reached, the provider is failing or rate-limited, or the key is missing).
A failed test doesn't undo the change.

Keys are never written to the config. Setup saves them to the private
secrets file, or you export them. The config only names the variable
(`api_key_env`).

## Drivers

Each provider is spoken to by one of three drivers, set with `api`:

| `api` | endpoint | inferred when |
|---|---|---|
| `"chat"` | `/chat/completions` (OpenAI-compatible) | anything else |
| `"anthropic"` | Anthropic's native `/v1/messages` | `base_url` is `api.anthropic.com` |
| `"responses"` | OpenAI's `/v1/responses`, stateless | never; set it yourself |

When `api` is unset, Ferrule infers it from `base_url`. A value you write
always wins. `ferrule doctor`, `ferrule model list` and the dashboard show
the driver and whether it was set or inferred.

**Anthropic** runs on the native Messages API. Prompt caching is on, so a
long conversation pays the cheaper cached price for everything it has
already sent. Cache writes cost 1.25× the input price unless you set
`price_cache_write_per_mtok`. A config from v0.3.0 that points at
`api.anthropic.com` moves to the native driver by itself. To keep the old
OpenAI-compatible route, write `api = "chat"`.

All three drivers stream a reply as the model writes it where that shows
(Telegram, `ferrule chat`); how, the switches, and what keeps the prompt
cache hitting are in `docs/speed.md`.

**OpenAI Responses** keeps no state on OpenAI's side (`store: false`).
Reasoning comes back encrypted and is sent back unchanged on the next call
of the same turn. It works against `api.openai.com` and hosts that
implement the same endpoint. For OpenRouter, keep `"chat"`.

Ferrule doesn't ask for thinking or reasoning unless you set it. A model
that thinks by default still does, and its thinking is carried through the
turn. Either way it is never shown in the chat and never written to logs:

```toml
[providers.anthropic]
base_url = "https://api.anthropic.com/v1"
api_key_env = "ANTHROPIC_API_KEY"
model = "claude-sonnet-5"
# api = "anthropic"                    # inferred from base_url
thinking = "adaptive"                  # "disabled", or a budget (older models): 8000
effort = "medium"                      # optional
max_tokens = 32000                     # optional; at least 16000 is sent
price_input_per_mtok = 3.0
price_cached_input_per_mtok = 0.3
price_cache_write_per_mtok = 3.75      # optional; 1.25× input by default
price_output_per_mtok = 15.0

[providers.openai]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
model = "gpt-5.2"
api = "responses"
effort = "low"                         # "none", "low", "medium", "high"
```

`thinking`, `effort` and `max_tokens` can also be set on one model under
`[providers.<name>.models."<id>"]`. `api` belongs to the provider only.

If a fallback moves a conversation to a model on another driver, the new
model gets the plain conversation: text, tool calls and tool results. The
old model's thinking or reasoning stays behind.

By hand, a config looks like this:

```toml
default_provider = "openai"            # the default when [models] has none

[models]
default = "openai/gpt-5.2"             # wins over default_provider
fallback = []                          # off unless you list models

[models.aliases]
fast = "groq/llama-3.3-70b-versatile"

[providers.openai]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
model = "gpt-5.2"                      # the provider's own model
price_input_per_mtok = 1.25
price_cached_input_per_mtok = 0.125
price_output_per_mtok = 10.0

[providers.openai.models."gpt-5.2-mini"]   # another model on the same key
price_input_per_mtok = 0.25            # each field optional; unset = the provider's
price_cached_input_per_mtok = 0.025
price_output_per_mtok = 2.0
context_window = 400000
profile = "openai"
```

A config from before M21 (one `model` per provider and no `[models]`)
works unchanged. Ferrule edits the file in place and keeps your comments.

## From the terminal

```
ferrule model list [--json]           # models, default, fallback, pins, problems
ferrule model default fast            # the default for every chat, task and agent without its own
ferrule model test [ref]              # one real call; says plainly what failed
ferrule model add openai/gpt-5.2-mini --alias mini
ferrule model remove openai/gpt-5.2-mini
ferrule model alias fast groq/llama-3.3-70b-versatile   # no ref: remove the alias
ferrule model pin telegram:42 fast    # a chat's model
ferrule model unpin telegram:42
ferrule model fallback groq deepseek  # --off to turn it off
```

`ferrule run --model <ref>` and `ferrule chat --model <ref>` run once on a
model, ahead of everything else. `--provider X` still works and means `X`'s
own model.

A change made in the terminal reaches a running gateway at its next model
call. There's no restart.

## From Telegram (owner only)

| | |
|---|---|
| `/model` | the connected models, the default and this chat's model |
| `/model default <ref>` | the default for every chat |
| `/model use <ref>` | this chat's model; `/model use default` clears it |
| `/model fallback <ref> …` | the fallback order; `/model fallback off` |
| `/model test <ref>` | one real call |

The owner is the owner's chat, or the owner writing in a group. Anyone
else gets "Only the owner can change models." A refused change says
`Nothing changed:` and the reason. `/status` has a models section: the
default, fallback, models that are down, pins, and the model each session
last ran on.

A gateway started with `--provider` stays on it until it restarts without
that flag. `/model` says so.

## Which model runs

Highest first:

1. **one-off**: `ferrule run/chat --model`, or `spawn_agent` with a `model`;
2. **role**: `[agents.roles.<role>] model = "<ref>"` (or `provider = "…"`);
3. **task**: `ferrule tasks add … --model <ref>`, changed with
   `ferrule tasks model <id> <ref>|default`;
4. **chat pin**: `/model use <ref>` or `ferrule model pin`;
5. **default**: `[models] default`, else `default_provider`'s model.

A sub-agent runs in its root's tree. A child of a task starts on the task's
model, and a child of a pinned chat starts on the pin, unless 1 or 2 names
another model. A sub-agent's `model` must be connected, or the spawn is
refused with the reason. On any model, the child shares its root's caps,
budget, gates and plan mode.

With `[routing]` on, the default starts on a cheap model and moves up to a
stronger one only when a turn fails, and any level above can name a tier
(`tier:cheap`, `tier:strong`) instead of a model: see
[routing.md](routing.md).

## When a model is down

Fallback is **off** until you list models. With
`fallback = ["groq", "deepseek"]`, a turn whose model still fails after its
retries moves on to the next listed model, within the same turn. This
happens for a network error, a timeout, HTTP 408, 429 or 5xx. The failed
model is skipped for 5 minutes, and then tried again. The owner is told
once per outage.

A refused key (401/403), an unknown model name or a missing key is not an
outage. It's reported and never falls back. A model that isn't connected is
never used.

## What's recorded

- The **ledger** has the provider and model that actually served each call,
  failed attempts included. `ferrule ledger` groups by them. Caps price
  each call by its own model's prices, fallbacks included. Under the table
  it adds the cache hit, time to first token and first reply, and parallel
  tool batches (`docs/speed.md`).
- The **audit log** (`<data>/trust/audit.jsonl`) has every change:
  `model.default`, `model.pin`, `model.unpin`, `model.fallback`, `model.add`,
  `model.remove`, `model.alias`, `model.task`, and `model.down`/`model.up`.
  Each has the old value, the new value, and who made the change.
- Telegram's `/status` shows the model each session last ran on.

## Doctor

`ferrule doctor` checks every provider's key. When more than one model is
connected, it also lists them with the default and fallback, warns about a
model whose key is missing, and fails when the default or a fallback names
something that isn't connected. `ferrule doctor --ping-models` also makes
one real call to every connected model. It costs a few tokens each.
For a local server it also compares the server's real window with the one
ferrule plans for, and `--ping-models` tells a model that can't call tools
from a broken chat template ([local-models.md](local-models.md)).

## Eval

`ferrule eval` never follows `[models] default`, a pin or the fallback
list. A run uses `--provider`/`--model` or `default_provider`, so it stays
comparable with the last one. `--model` also takes an alias or
`provider/model`.
