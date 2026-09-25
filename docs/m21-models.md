# M21: models — several at once, a default, a model per agent (design)

Status: design, 2026-09-25, branch `m21-models` (cut from M19b's branch).
Follows M19 (trust & cost) and M19b (reliability). M22 adds a dashboard
page on top of the API in §9. This file doesn't build that page.

## Why

Max, msg 3160: "I want to upgrade the model connections: an option to
connect several in parallel, pick a default, and, for example, tell a
different agent to run on a different model." He'll do this from Telegram
as much as from `ferrule setup`.

Today a *provider* is the unit: `[providers.X]` has one `model`,
`default_provider` picks one, and an M12 role can name another provider.
A provider can't serve two models. The default can only be changed by
editing the file or running setup. Every lane is built once with a fixed
`OpenAiCompatProvider`, so a change needs a restart. Ledger rows and caps
price a call by the provider's name.

## 1. References and aliases

A model is addressed by a **reference** (ref):

| form | means |
|---|---|
| `provider/model` | that model on that provider. Only the first `/` splits, so OpenRouter's `openrouter/anthropic/claude-sonnet-5` works |
| `provider` | that provider's own `model` (its *primary*). This is the pre-M21 meaning of every provider name |
| `alias` | an owner-chosen name from `[models.aliases]` |
| `model` | a bare model id, when exactly one provider connects it |

Resolution order for a bare word: alias, then provider name, then a unique
model id. If a bare model id matches more than one provider, it's an error
that lists both full refs. Aliases may not shadow a provider name. Refs are
case-sensitive, because model ids are.

**Connected** means the provider is under `[providers]` and the model is its
primary or is listed in its `models` table. Only connected models can be
the default, a pin, a task's model, a role's model, a sub-agent's model or
a fallback. Everything that takes a ref checks this: a free-form endpoint
or model can never enter through Telegram, a task or a tool call. OpenRouter
serves hundreds of models, and the owner lists the ones they want (setup
offers its model list).

## 2. Config shape and back-compat

```toml
default_provider = "openai"          # still read: the default when [models] has none

[models]
default = "openai/gpt-5.2"           # a ref; what /model default writes
fallback = []                        # ordered refs; empty = off (the default)

[models.aliases]
fast = "groq/llama-3.3-70b-versatile"
cheap = "deepseek"

[providers.openai]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
model = "gpt-5.2"                    # the primary: unchanged meaning
profile = "openai"
price_input_per_mtok = 1.25          # the provider's prices, as before
price_cached_input_per_mtok = 0.125
price_output_per_mtok = 10.0

[providers.openai.models."gpt-5.2-mini"]   # another model on the same key
price_input_per_mtok = 0.25          # every field optional; unset = the provider's
price_cached_input_per_mtok = 0.025
price_output_per_mtok = 2.0
context_window = 400000
profile = "openai"

[agents.roles.verifier]
model = "cheap"                      # new; `provider = "…"` still works (its primary)
```

- A config with no `[models]` table and no `models` under a provider loads
  and behaves exactly as before. The default is `default_provider`'s
  primary, and every call goes to the same URL with the same model and
  profile as before. `ProviderConfig.model` stays required.
- `[models] default` wins over `default_provider` when both are set. When
  setup's "Make it the default" is used, it writes both, so older readers
  agree. `/model default` and `ferrule model default` write
  `[models] default` only, plus `default_provider` when the provider
  changes (§4).
- Per-model `profile`, `context_window` and the three prices fall back to
  the provider's field by field. A partial price set still means "no
  cost" (M19's rule): the model's three prices are merged with the
  provider's first, and then all three must be present.
- `context_window` overrides the profile's window (`HarnessProfile.context_window`)
  for that model.
- `[agents.roles.R]` takes `provider` (legacy) or `model` (a ref), not both.
- Keys stay in `api_key_env`, which is an env var name. The key itself lives
  in the private secrets file or the environment, never in the config.
  This is unchanged.

## 3. Where the current model lives and how it's resolved

**The unit is the agent.** Each agent (a chat's lane, a scheduled task's
lane, a `ferrule run`/`chat` session, a sub-agent) carries a *scope*. Its
provider is a **routed provider** that resolves a ref from that scope on
every model call. A change therefore reaches a lane at its next model call,
with no restart. The lane's harness profile is still fixed when the lane is
built. So the gateway retires the lanes a change affects (they're rebuilt
from their transcript on their next message, with the new model's profile).
A change made from another process (the CLI) reaches the running gateway at
the next call, and the profile follows at the lane's next rebuild.

Precedence, highest first:

1. **explicit one-off**: `ferrule run/chat --model <ref>` (or the legacy
   `--provider X`, which means `X`'s primary), and a sub-agent started with
   `spawn_agent(model = …)`;
2. **agent/role**: `[agents.roles.R] model` (or `provider`) for a sub-agent
   of that role, or the nearest ancestor's role (M12's rule, unchanged);
3. **task**: the scheduled task's `model` column;
4. **chat pin**: `/model use <ref>` for that chat;
5. **default**: `[models] default`, else `default_provider`'s primary.

A sub-agent inherits its root's scope. A child of a task runs on the task's
model, and a child of a pinned chat runs on the pin, unless 1 or 2 applies.

Where each piece is stored:

| what | where | who writes it |
|---|---|---|
| connected models, aliases, the default, the fallback list | the config file | setup, `ferrule model`, `/model default` |
| chat pins | `<data>/models/pins.json` | `/model use`, `ferrule model pin/unpin` |
| a task's model | the `model` column in `tasks.db` (added by migration; NULL = none) | `ferrule tasks add --model`, `ferrule tasks model` |
| a sub-agent's model | the `model` column in `agents.db` (so `resume_agent` keeps it) | `spawn_agent` |
| outages (a model marked down) | memory only, per process | the fallback rules (§5) |
| which model served each session last | memory only, per process | the routed provider |

Pins are runtime state, not configuration. Keeping them out of the config
means a hand edit of the config can't lose them, and a pin can outlive the
model's removal (§8).

**One-off in Telegram ("answer this on <model>"): skipped.** The gateway's
interceptors can answer or pass a message, not rewrite it, so
`/model once <ref> <question>` would reach the model with the command in it.
The turn would also land in a transcript whose other turns came from another
model. `/model use <ref>` followed by `/model use default` does the same job
honestly. The CLI (`--model`) and sub-agents (`spawn_agent`) do get the
one-off.

## 4. Telegram and the CLI, and how changes persist

Owner only. The owner is the M19 owner chat (`[trust] owner_chat`, else the
first positive allowed chat), or a message in an allowed group whose
**sender** is the owner. For the second case, `InboundMessage` gains
`sender_id` (Telegram's `from.id`). Anyone else gets "Only the owner can
change models." and nothing changes. `/model` from them gets the same reply,
because the list isn't theirs to see.

- `/model` lists the connected models: ref, alias, which is the default,
  this chat's pin, fallback order, a model marked down and until when, and
  a missing key.
- `/model default <ref>` sets the default.
- `/model use <ref>` pins this chat, and `/model use default` clears the pin.
- `/model fallback <ref> …` and `/model fallback off` set the fallback list.

The CLI has the same operations:

```
ferrule model list
ferrule model default <ref>
ferrule model test <ref>
ferrule model add <provider>/<model> [--alias A]
ferrule model remove <ref>
ferrule model alias <name> <ref>
ferrule model pin <channel>:<chat> <ref>
ferrule model unpin <channel>:<chat>
ferrule model fallback <ref>… | --off
```

**Persisting.** Every change is a read-modify-write of the file *as it is
now*, through `toml_edit`, so comments, the owner's hand edits since the
gateway started, and everything else survive:

1. take `<config>.lock`: a `create_new` lock file, held a few ms. A lock
   left behind for over 10 s is stale and removed;
2. read and parse the current text, apply the edit, and check that the
   result still parses as a `Config` (setup's rule). If it doesn't, nothing
   is written and the reply says why;
3. write `<config>.tmp-<pid>` and rename it over the config. On Windows the
   rename retries "access denied" for up to 5 s, as the extensions lock
   does after the CI pass, because a scanner or editor holding the file
   open causes it for a moment;
4. release the lock, write the audit row (§7), and retire the affected lanes.

`pins.json` uses the same lock, write and rename. Setup's `Target::save`
gets the same Windows retry.

`ferrule model test <ref>` and setup make **one real call** (a 1-token
completion) and say plainly what failed:

| what came back | the owner reads |
|---|---|
| 401 / 403 | the key was refused. Check `$ENV`, or replace it in setup |
| 404, or an error naming the model | the provider doesn't know the model `m` |
| connect error / timeout | couldn't reach the URL. Is the endpoint up? |
| 5xx | the provider is failing right now (HTTP n) |
| 429 | rate-limited or out of credit |
| no key in the env or secrets | no key: `$ENV` isn't set |

## 5. Fallback on outage

`[models] fallback = ["ref", …]` is ordered and **off by default** (empty).
It's off because a silent switch changes cost, quality and where data goes,
and the owner should choose that.

- The agent's retry loop (`RetryPolicy`) runs first. Only when a
  **transient** failure (a transport error, timeout, HTTP 408, 429 or 5xx)
  is still failing after the last retry does the turn move on. The provider
  is asked to fail over. It marks the model down for 5 minutes (shared by
  every lane in the process) and names the next entry in `fallback` that
  isn't the failed model and isn't down. The agent then restarts its retry
  loop on that model, in the same turn, with the same messages.
- **401/403, an unknown model and other non-transient errors are not
  outages.** They are reported, with no fallback. The same goes for a
  missing key.
- Only connected models can be listed (§1), so a fallback can never reach
  a model the owner didn't connect. A listed ref that has since been
  removed from the config is skipped.
- Calls to a model marked down go straight to the next fallback until the
  mark expires. After that the primary is tried again, and if it answers,
  the mark is cleared.
- **The owner is told once per outage**, in plain words, through the M19
  hub (the owner's chat): "openai/gpt-5.2 isn't answering (HTTP 503 after
  3 tries), so kimi/kimi-k2.6 answered instead. I'll try openai/gpt-5.2
  again in 5 minutes." They are not told again while the mark stands. In
  `ferrule run`/`chat`, the notice goes to stderr.
- If every candidate fails, the turn fails with the last error, as before.
- Caps: every call, the fallback's included, is priced by the model that
  ran (§6), so M19's caps count the fallback at its own price.

## 6. Recording the model that ran

`Provider` gets two default methods, so existing providers and the 23
`CompletionResponse` literals don't change:

- `complete_routed(req) -> (Option<Served>, Result<…>)`. `Served` is
  `{provider, model}`, for a provider that picks per call. The default
  returns `None` and calls `complete`.
- `fail_over(&served, &error) -> Option<String>`. It returns a note when
  there's another model to try. The default returns `None`.

The agent writes the served provider and model into every ledger row,
errors and retries included. Without `Served`, the row gets
`provider.name()` and the ledger context's model, which is today's row
exactly. A fallback emits `AgentEvent::ModelFallback{from, to, error}`.

Pricing moves from a provider key to (provider, model), with the provider's
price as the fallback, in both places that price rows: the ledger sink
(`ledger::build_sink`) and M19's `TrustSink` pricer (`trust::equip`). A call
is therefore priced, and counted against a cap, at the price of the model
that answered.

M19b's `/status` gains a **models** section: the default, the fallback
list, models marked down, each pin, and for each busy lane the model its
last call ran on. `ferrule status` shows the same lines.

## 7. Audit and eval isolation

Each change writes an M19 audit row (`<data>/trust/audit.jsonl`). The
events are `model.default`, `model.pin`, `model.unpin`, `model.fallback`,
`model.add`, `model.remove` and `model.alias`, with `{from, to, by}`, where
`by` is `telegram chat N` or `cli`. `model.down` and `model.up` record an
outage and the recovery. A turn's model shows in the ledger rows and not in
the audit: the ledger is where calls are recorded.

**Eval ignores the owner's model state.** `ferrule eval` builds its own
provider, as before. It uses `--model <ref>` if given (a connected ref;
with `--provider X`, `--model` still overrides the model name, as today),
else `--provider`, else `default_provider`'s primary. It never reads
`[models] default`, the pins, the task models or the fallback list. A
suite's numbers must not change because the owner switched models in
Telegram, and a fallback would mix two models into one score. The report
header names the model it used.

## 8. Failure modes

- **The default's key is removed.** Calls fail with "no key: `$OPENAI_API_KEY`
  isn't set (openai/gpt-5.2)". That isn't an outage, so there's no
  fallback. `ferrule doctor` fails on it, and `/model` shows "key missing"
  on that line.
- **A pinned model is deleted from the config.** The pin stays in
  `pins.json`, and the chat runs on the next rule down (task, then default).
  `/model` shows "pinned to X, which isn't connected anymore, so using the
  default", and a log warning is written once. Re-adding X brings the pin
  back. `/model use default` clears it.
- **The default names a removed model.** Resolution falls back to
  `default_provider`'s primary, with a warning once. If that provider is
  gone too, every call fails with "no default model: run `ferrule model
  default <ref>`", and doctor fails.
- **A task, role or sub-agent names a removed model.** That run fails with
  the reason. It doesn't fall through to the default, because the owner
  asked for that model on purpose.
- **The config is hand-edited while the gateway runs.** The routed
  provider notices the file's mtime or length change at the next call and
  reloads. If the new text doesn't parse, it keeps the last good version
  and logs one warning, until the file parses again. `/model` writes start
  from the file as it is (§4), so they don't undo the edit.
- **Two chats change the default at once.** The lock serializes them, and
  the last writer wins. Both get a reply that names the value they set, and
  both are audited. A CLI change in parallel is covered by the same lock.
- **The gateway was started with `--provider X`.** That's a process-wide
  one-off (rule 1) for the chats. `/model default` still persists, but it
  says it won't apply until the gateway restarts without `--provider`.

## 9. What M22 needs: one API

`ferrule-cli`'s `models` module exposes one struct, `Models`, that the
Telegram door, the CLI and later the dashboard all call.

**Read model.** `Models::view() -> ModelsView` (serde-serialisable):
- `models`: one row per connected model, with ref, provider, model,
  aliases, primary or not, default or not, fallback rank, key present,
  prices, context window, profile, and down-until;
- `default`, `fallback` and `pins` (channel, chat, ref, connected);
- `last_served`: session id, ref and time;
- `problems`: default missing, pin to a removed model, unparsable config.

**Mutations.** Each validates, persists (§4), audits (§7) and returns the
new view:

```
set_default(ref, by)
pin(channel, chat, ref, by)
unpin(channel, chat, by)
set_fallback(refs, by)
add_model(provider, model, alias, by)
remove_model(ref, by)
set_alias(name, ref, by)
test(ref) -> TestOutcome   // async, one real call
```

A task's model goes through the scheduler's `TaskStore::set_model`.

The owner check stays with the caller: the Telegram door checks the owner,
and M22's page will check its own session. The API takes `by` as a label
only.

## 10. Presets (setup)

Setup offers OpenAI, Anthropic, Google Gemini, OpenRouter, DeepSeek,
Moonshot (Kimi), Groq, a local Ollama, and "another OpenAI-compatible
server". Gemini
(`https://generativelanguage.googleapis.com/v1beta/openai`, `GEMINI_API_KEY`)
and Groq (`https://api.groq.com/openai/v1`, `GROQ_API_KEY`) are new. After a
provider is saved, setup offers "Add another model on this key", and the
Model provider menu gains "Default model", which lists every connected
model.

**Anthropic goes through its OpenAI-compatible endpoint**
(`https://api.anthropic.com/v1/chat/completions`). What's lost against a
native Messages-API driver, per Anthropic's compatibility notes (not
re-checked from here, since this build has no network):

- explicit prompt-cache breakpoints (`cache_control`). Long agent sessions
  re-send a large stable prefix, and native caching is the biggest cost
  lever;
- extended-thinking content in responses;
- `strict` tool schemas, PDFs, citations and the other Anthropic-only
  blocks;
- some OpenAI fields are ignored rather than rejected.

**Is a native driver worth it?** Probably yes, as a follow-up, and mainly
for caching cost if Claude becomes a daily model. It's a new `Provider`
implementation behind the same `Served`/fallback interface, so M21 doesn't
block it. It's flagged for Max, not built here.

## 11. Doctor

`ferrule doctor` checks every connected model:
- its key is present;
- its provider's model list includes it (the list is fetched once per
  provider, as now, unless `--offline`);
- the default resolves;
- pins and fallback entries name connected models.

`ferrule doctor --ping-models` also makes the one-token call from §4 per
model, and costs a few tokens each.

## 12. Tests (hermetic)

Scripted OpenAI-compatible servers (2–3 at once, one failing on cue), the
fake Telegram from M19b's tests, and a temp config and data dir. The
integration tests, through the real binary:

- two providers with several models: the default answers, and
  `/model default` switches it for the next turn and after a restart;
- a chat pinned to B answers on B while another chat stays on the default;
- a task with its own model runs on it, and the ledger shows the model per
  call;
- a sub-agent on a named connected model runs on it; an unconnected model
  is refused with the reason; the child still counts toward the parent's
  cap;
- a 503 on the primary finishes the turn on the fallback and tells the
  owner once; a 401 doesn't fall back;
- an old single-model config loads and behaves exactly as before;
- eval ignores the default, the pins and the fallback unless `--model`
  picks one.

Plus unit tests for ref parsing, resolution precedence, config merging,
the lock and write path, and the core's served/fail-over loop.

## Relation to the routing track

The roadmap's Phase 1 routing ("start cheap, escalate") needs exactly this
routed provider: a second rule source in the precedence chain. M21 doesn't
build the rule itself, which is still blocked on Max's tier choice. It
leaves the hook in place.
