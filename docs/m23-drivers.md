# M23: native Anthropic and OpenAI Responses drivers (design)

Status: design, 2026-09-25, branch `m23-drivers` (cut from `main` at the
v0.3.0 release). The user guide is `docs/models.md`.

## Why

Today Ferrule has one driver, `openai_compat.rs` (Chat Completions).
Anthropic goes through its OpenAI-compatible endpoint, and that route loses:
- prompt caching (every turn pays full input price);
- extended thinking;
- strict tools;
- PDFs and citations.

OpenAI's newest agentic and Codex models are served best, and some only,
by `/v1/responses`. Routing Phase 1 (M25) needs tiers that can sit on
different providers, so a conversation has to survive moving between them.

M23 adds two drivers next to the chat one and keeps everything above the
`Provider` trait unchanged. Streaming stays out, as it is today. PDFs,
citations and strict tools stay out too: nothing above the driver produces
them yet (§11).

## 1. Three drivers, one trait

`ferrule-providers` gets:

| module | api | endpoint |
|---|---|---|
| `openai_compat` | `chat` | `POST {base_url}/chat/completions` (unchanged) |
| `anthropic` | `anthropic` | `POST {base_url}/messages`, `x-api-key`, `anthropic-version: 2023-06-01` |
| `responses` | `responses` | `POST {base_url}/responses` |

`base_url` means the same thing for all three: the prefix that ends in
`/v1`. That keeps the Anthropic preset's `https://api.anthropic.com/v1`
valid as it is.

Other shared pieces:
- `common` holds the error helpers the chat driver already has: status →
  retryable, `retry-after`, error bodies, truncation, and the
  `without_url` wrapping.
- `Api` is an enum: `Chat | Anthropic | Responses`.
- `infer_api(base_url)` returns `Anthropic` when the host is
  `api.anthropic.com`, and `Chat` otherwise.
- `build(api, name, base_url, key, model, DriverOptions) -> Arc<dyn Provider>`
  is the only place the CLI constructs a driver.

`DriverOptions` holds `thinking`, `effort` and `max_tokens`, all optional
(§4, §6). The chat driver ignores them.

### What M25 gets

M25 needs a uniform trait, per-call cost and latency, and a failure class it
can escalate on.
- The trait doesn't change. Every driver is an `Arc<dyn Provider>`, so a
  `RouterProvider` can hold one of each.
- Cost and latency are already per call in the ledger. M23 adds
  cache-write tokens (§5).
- New: `CoreError::class() -> FailureClass`, with the values
  `RateLimited`, `Overloaded`, `Server`, `Timeout`, `Connect`, `Auth`,
  `BadRequest`, `ContextTooLong`, `ModelNotFound`, `Refused`, `Malformed`
  and `Other`. It is derived from the error the drivers already produce,
  since every one carries `HTTP {status}` and the provider's message.
  `is_transient()` doesn't change meaning.

No router is built here.

## 2. The neutral transcript

`Message` (role, content, tool_calls, tool_call_id, reasoning) stays the
neutral type every layer reads. M15, M12, M16 and the dashboard never see
a wire format. M23 adds one optional, serde-default field:

```rust
pub native: Option<NativeBlocks>   // skipped when None
pub struct NativeBlocks { pub api: String, pub model: String, pub items: Vec<serde_json::Value> }
```

`items` is the assistant turn exactly as the provider returned it:
- **anthropic:** the whole `content` array (thinking, redacted_thinking,
  text and tool_use blocks);
- **responses:** the whole `output` array (reasoning items with
  `encrypted_content`, the message, the function_calls).

It exists for one reason: both APIs want their own blocks back, byte for
byte, inside a tool loop. Anthropic needs signed thinking blocks. OpenAI
needs the reasoning items, and says: "pass back all reasoning items,
function call items, and function call output items, since the last
`user` message".

The neutral fields are always filled as well:
- `content` is the text;
- `tool_calls` are the calls;
- `reasoning` is the thinking text or summary, when any came back.

So every other reader, and every other driver, works from those.

Transcripts are JSONL. An old transcript has no `native`, and a v0.3.0
binary ignores the new field, since `Message` has no `deny_unknown_fields`.

### Redaction

`NativeBlocks` has a hand-written `Debug` that prints
`NativeBlocks { api, model, <N items> }`, so a `{:?}` in a log line can
never print thinking or signatures. It is not in any event and not in the
ledger.

Thinking reaches the owner only through `AgentEvent::Reasoning` (the
`reasoning` text), which the CLI prints only with `--show-reasoning` and
the gateway never renders. That is the M21 behavior, unchanged.

The transcript file does hold `native`, as it holds every message today.
It sits under the data dir, which the shell sandbox already hides.

### Replay policy: current tool loop only, same api and model

When building a request, a driver uses `native.items` for an assistant
message only if all of these hold:
1. `native.api` is this driver's api;
2. `native.model` is this request's model;
3. the message comes **after the last user message**, i.e. it belongs to
   the tool loop in progress.

Every other assistant message is rebuilt from the neutral fields: text
plus tool calls, with no reasoning.

This covers what each API requires: Anthropic's thinking inside a
tool-use loop, and OpenAI's reasoning items "since the last user message".
It also avoids the one way replaying older blocks breaks. On Anthropic,
thinking blocks are bound to the exact prefix they were made under. For
accounts created from 31.08.2026 on, a replayed block whose `system`,
`tools` or earlier messages changed is a 400. Ferrule changes that prefix
between turns in several ways:
- session-start recall appends to the system prompt;
- M17 hot-adds MCP tools;
- M15 compaction rewrites history and keeps a verbatim tail.

Within one loop the prefix is stable. Compaction can fire mid-loop, so it
clears `native` on the tail it keeps (one line in `maybe_compact`), because
the prefix those blocks were bound to is gone.

**The cost.** Earlier turns' reasoning isn't replayed. The "preserved
thinking" of Opus 4.5+ and Sonnet 4.6+ covers only the current loop. And
on the first request after a new user message, the previous loop's
assistant turns change bytes: they lose their thinking, so that part is
re-written to the cache once. The prefix before the previous user message
still hits.

The alternative was to replay everything and handle each 400 after a
change. That costs one failed request per call until the next compaction.
It is listed as a follow-up (§11): opt-in once Ferrule's prefix is proven
stable.

**Safety net (Anthropic).** A 400 whose message names thinking, a
signature or a block is retried **once** at once. The retry drops every
native block and every `thinking` or `output_config` field the driver
added, and is logged as a warning. A still-failing call is a normal
`BadRequest`.

### Lossy conversions, defined

| from → to | kept | dropped |
|---|---|---|
| anthropic → chat | text, tool calls and results (same ids) | thinking blocks and signatures. `reasoning` text is sent as `reasoning_content` only when the profile retains reasoning, and never for a message with `native` from another api |
| anthropic → responses | text, calls (as `function_call` items with `call_id`), results | thinking |
| responses → anthropic | text, calls (ids sanitized, §3), results | reasoning items, encrypted content |
| responses → chat | text, calls, results | reasoning items |
| chat → anthropic | text, calls (ids sanitized), results | `reasoning_content` text: thinking without a signature can't be sent back |
| chat → responses | text, calls, results | `reasoning_content` |
| anthropic model A → model B | text, calls, results | A's blocks (rule 2). The API would drop most of them anyway, and not every model reads another's |

Nothing is ever *invented*:
- no fake thinking block;
- no reasoning item without its encrypted content;
- no signature.

A conversation that moves to another provider mid-loop continues from the
neutral fields, which hold everything a tool loop needs: the calls, their
ids and their results.

## 3. The Anthropic Messages driver

**Request.**
- `model`, `max_tokens`, `messages`, `system`, `tools` and `tool_choice`
  (`auto`, only when tools exist; forced choice is a 400 on Opus 5.5 and
  Fable 5.1).
- `thinking` and `output_config.effort`, only when configured (§4).
- `temperature` is **never sent**. It is a 400 on Opus 4.7+, Sonnet 5 and
  Fable. Ferrule's only non-default uses are 0.0 for compaction, the eval
  judge and the learning pass, and 0.0 never guaranteed determinism anyway.
- `max_tokens` is required by the API. It is the request's value, else the
  config's `max_tokens`, else 16 000. On Anthropic it caps thinking plus
  text, and thinking is on by default on Sonnet 5, Opus 5 and Opus 5.5. So
  a request that asks for less than 16 000 is raised to 16 000 unless the
  config sets `thinking = "disabled"`. The learning pass asks for 1 024,
  which a thinking model can spend entirely on thinking and return no text.
  It is only a cap, so raising it costs nothing unless it's used.

**Shaping messages.**
- Leading system messages become the top-level `system` (a list of text
  blocks).
- A later system message becomes a user text block wrapped as
  `[system] …`. Anthropic's mid-conversation system role is beta.
- A user message becomes `[{type:"text"}]`.
- An assistant message becomes text plus `tool_use` blocks (`input` =
  arguments, or `{}` when they aren't an object). With a native replay
  (§2) it is sent verbatim instead.
- A tool result becomes `tool_result{tool_use_id, content}`. Consecutive
  results are grouped into **one** user message, which the API requires
  for parallel calls.
- Consecutive same-role messages are merged, and empty text blocks are
  skipped (the API rejects them).
- A user text that follows tool results joins the same user message,
  after the results.
- Tool ids are sanitized to `[A-Za-z0-9_-]`, anything else becoming `_`,
  and the same mapping applies to `tool_use.id` and `tool_use_id`. Kimi's
  `functions.read:0` would otherwise be rejected. The neutral transcript
  keeps the original id.
- A conversation must start with a user message. If it doesn't (a
  compaction tail can start with an assistant message), a `[continued]`
  user text is put first.

**Tools.** `{name, description, input_schema}`. `strict` isn't sent: the
chat driver doesn't send it either, and schemas from MCP servers aren't
guaranteed to meet strict mode's rules.

**Prompt caching.** Explicit `cache_control: {type:"ephemeral"}` (5
minutes) breakpoints, at most 3 of the 4 allowed:
1. the last system block. When there's no system prompt, the last tool
   instead; the render order is tools → system → messages, so that one
   breakpoint covers both;
2. the last block of the last message. That is where the next request
   reads;
3. the last block of the message before the latest user message, so the
   previous turn's prefix is still readable after the step in §2 where
   thinking is dropped.

The prefix is byte-stable because the conversion is deterministic
(serde_json with ordered maps), and a test asserts request 2's prefix is
byte-identical to request 1's. A prefix below the model's minimum (512 to
4 096 tokens) silently isn't cached, which is harmless.

**Response.**
- The `content` blocks are read by `type`, not position.
- `text` blocks are joined into `content`.
- `tool_use` blocks become `ToolCall`s, keeping their ids.
- `thinking` text is joined into `reasoning`. It is empty under the
  default `display: "omitted"`.
- `redacted_thinking` only goes into `native`.
- The whole `content` array goes into `native`, with `api = "anthropic"`
  and the response's `model`.

**stop_reason.**
- `end_turn`, `tool_use` and `stop_sequence` are normal.
- `max_tokens` is normal too: the text so far is returned, as the chat
  driver does with `length`. A tool_use cut off mid-input is dropped by
  the API anyway.
- `pause_turn` (a server tool's long turn) is returned as is. Ferrule
  sends no server tools, so it shouldn't happen.
- `refusal` is a `CoreError::Provider("refused: {stop_details.category}")`.
  Its class is `Refused`, and it isn't transient, so no retry and no
  fallback. The M21 rule is that fallback is for outages only, and a
  refusal carrying the owner's text to another vendor is the owner's
  decision, not ours.

**Usage.** It is normalized to Ferrule's convention, where input includes
cached tokens:
- `input_tokens = input + cache_creation + cache_read`;
- `cached_input_tokens = cache_read`;
- `cache_write_input_tokens = cache_creation` (new field, §5).

**Errors.** Anthropic's error body is
`{"type":"error","error":{"type","message"}}`.

| status / type | becomes | class |
|---|---|---|
| 529 `overloaded_error` | Transient | Overloaded |
| 429 `rate_limit_error`, `retry-after` | Transient with retry_after | RateLimited |
| 500, 502, 503, 504, 408 and connection errors | Transient | Server / Timeout / Connect |
| 400 `invalid_request_error` | Provider (the message) | BadRequest, or ContextTooLong when it says "prompt is too long" |
| 401, 403 | Provider | Auth |
| 404 `not_found_error` | Provider | ModelNotFound |

These feed M19b's retry loop (`call_provider`) and M21's fallback
unchanged. A retry keeps the transcript exactly as it was.

## 4. Thinking (Anthropic)

The config is per provider, and per model under `[providers.X.models.Y]`.

```toml
thinking = "adaptive"   # or "disabled", or a number: a budget in tokens (older models)
effort = "high"         # low | medium | high | xhigh | max → output_config.effort
```

**Default: neither is set, and Ferrule sends no `thinking` field.**

"Off by default" means Ferrule doesn't ask for thinking and never shows it.
It can't mean "send `disabled`":
- on Opus 5.5, `disabled` is a 400;
- on Opus 5, `disabled` with `xhigh` effort is a 400;
- on Sonnet 5, Opus 5 and Opus 5.5, the model's own default is to think.

Sending nothing gives each model its own default:
- no thinking on Haiku 4.5 and Opus 4.8;
- adaptive thinking on the 5-series.

Whatever comes back is carried through the loop (§2) and never shown.

**The settings:**
- `"adaptive"` sends `{type:"adaptive"}`. That is valid on 4.6+.
- `"disabled"` sends `{type:"disabled"}`. Use it only where the model
  accepts it.
- A number `N` sends `{type:"enabled", budget_tokens:N}`, with
  `max_tokens` raised to `N + 4096` if it's lower. This is for the older
  models (Haiku 4.5, 4.5-series) only; it's a 400 on Sonnet 5, Opus 4.7+
  and Fable.

Ferrule doesn't guess by model name. A 400 says it plainly, and
`ferrule model test` shows that message.

`display` isn't sent, so on the new models the text is omitted and
`reasoning` is empty. That fits "never shown to the owner by default".

**The in-loop rule.** When the trailing tool loop has an assistant
`tool_use` turn but no native blocks for this model (the loop started on
another provider and fell back here), the configured `thinking` field is
left out of that request. With `thinking` set, the API can ask for the last
assistant turn to start with a thinking block that Ferrule doesn't have. The
next user turn starts clean and thinks again.

The 400 retry (§2) is the second safety net.

## 5. Usage, ledger and prices

**Usage.** `Usage` gets `cache_write_input_tokens: u64` (serde default 0):
- **anthropic** fills it from `cache_creation_input_tokens`;
- **responses** and **chat** leave it at 0. Neither reports writes, and
  OpenAI bills them at the input price.

`AgentEvent::Usage` and the agent's running total carry it along.

**Ledger.** `LedgerRecord` gets `cache_write_input_tokens`, skipped when 0,
so old rows parse and new rows from other drivers look the same.

**Prices.** `ProviderPricing` gets `cache_write: f64`. The cost is:

```
(input − cached − write) × input  +  cached × cached_input  +  write × cache_write      (per Mtok)
```

When `write = 0` this is the pre-M23 formula, so every existing number
(including the eval's $0.98) stays the same.

Price config gets `price_cache_write_per_mtok` on the provider and on each
model. When it's unset:
- on an anthropic-api provider it is **1.25 × input**, Anthropic's 5-minute
  write price, which is the only TTL Ferrule uses;
- otherwise it equals input.

The catalog reads OpenRouter's `input_cache_write` when it's present.
`fill-prices` writes it only for anthropic-api providers, and never
overwrites a hand-set price, as in M22.

**Setup prices.** The M21 Anthropic preset gets `claude-sonnet-5`'s prices:
$2 / $0.20 cached / $10, with the write price at $2.50 (1.25 × input).

**Hit rate.** It is measurable from the ledger as `cached / input`. The
test in §9 computes it from the mock's usage.

## 6. The OpenAI Responses driver

**Stateless.** Every call sets `store: false` and
`include: ["reasoning.encrypted_content"]`, sends the **full input**, and
never sends `previous_response_id`. The reasons:
- Ferrule owns the transcript. M15 compaction and M12 resume rewrite or
  rebuild it, and a server-side chain would disagree with it silently.
- Fallback across providers (M21) needs the full neutral transcript anyway.
- `store: true` keeps the owner's conversations on OpenAI's servers for
  30 days. That's a privacy change nobody asked for.
- OpenRouter's Responses API is stateless only: it rejects `store: true`
  and `previous_response_id` with a 400 (checked in its docs,
  2026-09-25). One code path serves both.

The cost is re-sending input, which OpenAI's automatic prompt caching
mostly discounts (`cached_tokens` in usage).

**Encrypted reasoning.** It is requested (above), kept verbatim in
`native` as part of the `output` array, and replayed only inside the
current loop for the same model (§2). That's what keeps a reasoning model's
chain intact across tool calls without `store`.

**Request.**
- `model`, `store`, `include`.
- `instructions`: the leading system messages, joined.
- `input`, `tools`, `max_output_tokens` (only when set), and
  `parallel_tool_calls: true`.
- `reasoning: {effort}` only when `effort` is configured. Accepted values
  (model-dependent): none, minimal, low, medium, high, xhigh, max. The
  model's default otherwise.
- `temperature` is never sent. Reasoning models reject it, and the Chat
  driver remains for models that want it.

**Input items.**
- A user message becomes `{role:"user", content:"…"}`.
- A later system message becomes `{role:"developer", content:"…"}`.
- An assistant message becomes `{role:"assistant", content:"…"}` (when it
  has text) followed by one
  `{type:"function_call", call_id, name, arguments}` per call. Arguments
  are a JSON **string**. There's no `id`, since items aren't stored. A
  native replay (§2) sends the stored output items instead.
- A tool result becomes
  `{type:"function_call_output", call_id, output:"…"}`.

**Tools.** `{type:"function", name, description, parameters, strict:false}`.

**Response.**
- `output` is read by `type`:
  - a `message`'s `output_text` parts are joined into `content`, and a
    `refusal` part becomes a `Refused` error;
  - `function_call` becomes `ToolCall{id: call_id, arguments: parsed}`;
  - a `reasoning` item's `summary[].text` goes into `reasoning`.
- The whole `output` array goes into `native`.
- `status: "incomplete"` with `incomplete_details.reason ==
  "max_output_tokens"` is returned like `max_tokens` above. Any other
  reason becomes a `Provider` error naming it.
- `status: "failed"`, or an `error` object in a 200, becomes an error
  classified by `error.code`.

**Usage.**
- `input_tokens` is used as is: it already includes cached tokens.
- `cached_input_tokens` = `input_tokens_details.cached_tokens`.
- `output_tokens` includes reasoning tokens, as billed.

**Errors.** OpenAI's `{"error":{"message","type","code"}}` uses the same
status table as the chat driver (`common`). `context_length_exceeded` is
classed `ContextTooLong`.

**Hosts.** The driver works for `api.openai.com/v1` and any host that
serves `/v1/responses`. OpenRouter's (`https://openrouter.ai/api/v1`) is
documented as stateless, with function tools, `function_call` /
`function_call_output` items and reasoning effort. Its docs don't show
`input_tokens_details` in usage, so there `cached_input_tokens` may stay 0.
Nothing is assumed beyond that. It is never inferred: a Responses provider
is always `api = "responses"`, written explicitly.

## 7. Selection and back-compat

**The `api` field.** `[providers.X] api = "chat" | "anthropic" | "responses"`
is optional:
- when unset, it's inferred from `base_url` (§1);
- an explicit value always wins;
- any other value is a config error naming the three.

**Where it's read.**
- `Entry` (M21) gets `api` and the driver options, merged from the model's
  and the provider's config.
- `Route.client` becomes `Arc<dyn Provider>`, and the client cache key
  includes the api and options.
- Every other construction site goes through `build()`:
  - eval's `--provider`;
  - the eval judge;
  - the learning pass;
  - roles;
  - `ferrule model test` and setup's test;
  - `doctor --ping-models`.

**Setup and `ferrule model add`.** The Anthropic preset writes
`api = "anthropic"` explicitly, so the file says which driver runs. The
setup note stops saying "native is planned" and says that native is what's
used, with caching.

**Doctor.** It prints each provider's driver: `api: anthropic (inferred)` /
`(set)`.

**A v0.3.0 config loads unchanged.** The only behavior change is that a
provider whose `base_url` is `api.anthropic.com` moves from the chat driver
to the native one.

Why that is safe:
- Same key, same `x-api-key` auth (the preset's key works on both
  endpoints).
- Same `base_url` meaning.
- Same model ids.
- Same `profile`.
- It fails the same way on outage (429/529/5xx are transient), so M21
  fallback sees the same thing.

What changes for the owner:
- cached turns get cheaper;
- the ledger shows cache reads and writes;
- `temperature` is no longer sent. It was 0.0 only in compaction and the
  learning pass, and it's a 400 on the new models anyway.

The known difference is thinking. Through the compatible endpoint a
5-series model already thought by default, but its thinking was discarded.
Natively it is carried through the loop, which is what the model expects.

An owner who wants the old route writes `api = "chat"`, which is tested.

**Transcripts.** Old ones load (there's no `native`). A session that
started on the chat driver continues on the native one from the neutral
fields, which is the cross-driver path of §2.

## 8. Everything above the driver

| | why it keeps working |
|---|---|
| M12 sub-agents | they get a provider from `Models::route`, so any driver |
| M15 compaction | the summary call is a plain request. The kept tail loses `native` (§2). `search_history` reads neutral fields |
| M16 learning pass | a plain request. The raised `max_tokens` floor protects it on thinking models |
| M19 caps and cost | cost comes from `Usage`. The write price is new, and 0 writes gives the old cost |
| M19b retry and watchdog | Transient and retry-after, unchanged |
| M21 fallback | `fail_over` is unchanged. Conversion happens inside each driver from the neutral messages, so falling back **across drivers mid-conversation** works on the same message list (tested, §9) |
| M22 dashboard | the model test goes through `build()`. Catalog prices get `cache_write`. The page shows `api` |
| M14 eval | a variant's provider can have any `api`. The starter suite's mock is a chat server and is untouched |

## 9. Tests (hermetic)

Fixtures are hand-written from each API's documented wire format and served
by an in-process mock on 127.0.0.1.

**Anthropic.**
- The request shape: system as a top-level field, tools, grouped tool
  results, merging, id sanitizing, no temperature, `max_tokens` floor.
- A multi-tool turn: two `tool_use` blocks come back as two ToolCalls, and
  the next request has one user message with two `tool_result`s.
- Cache accounting: usage normalized, with the cost at the read and write
  prices. Request 2's prefix is byte-identical to request 1's, the
  breakpoints sit where §3 puts them, and the hit rate is computed from the
  mock's usage.
- Thinking passthrough: a thinking block with its signature comes back
  unmodified on the next request of the loop. It is dropped after a new
  user message and for another model. `Debug` doesn't print it.
- Errors: 529 is Transient; 429 is Transient with retry-after; a 400 is
  BadRequest, and a thinking 400 is retried once without blocks; 401 is
  Auth; a refusal is Refused.

**Responses.**
- The request shape: `store:false`, `include`, `instructions`, items,
  string arguments, no `previous_response_id`.
- A multi-call turn.
- Reasoning items replayed verbatim within the loop.
- Cached tokens.
- An incomplete status, errors in the body, and 429 with retry-after.

**Core.**
- `FailureClass` for each error shape.
- `Usage` and ledger serde back-compat.
- The cost formula with writes.

**CLI.**
- `infer_api`, and an explicit value winning.
- An old v0.3.0 config loads and routes Anthropic to the native driver,
  and `api = "chat"` keeps the old route.
- `ferrule model test` on each driver against a mock.
- Doctor prints the driver.
- Cross-driver fallback mid-conversation:
  - an Anthropic mock answers with thinking + tool_use, then keeps
    returning 529;
  - the fallback is a chat mock;
  - the chat request has the tool call and its result, and no thinking,
    signature or `native`;
  - the same test runs responses → anthropic, where the ids are sanitized.

**Live smoke tests.** One `#[ignore]`d test per driver runs a two-step tool
call against the real API. They read `ANTHROPIC_API_KEY` or
`OPENAI_API_KEY`, and skip with a message when the key isn't set. There
are no live keys in this container. Max runs them with:

```
ANTHROPIC_API_KEY=… cargo test -p ferrule-providers --test live -- --ignored anthropic
OPENAI_API_KEY=…    cargo test -p ferrule-providers --test live -- --ignored responses
```

## 10. Failure modes

| what | what happens |
|---|---|
| Thinking config the model rejects | 400 in plain words, from `model test` too. No retry |
| A 5-series model thinks and uses up `max_tokens` | the 16 000 floor. Otherwise the text so far comes back, as with `length` today |
| Prefix changed under a replayed block | compaction clears `native`, and blocks are never replayed across user turns. A thinking 400 retries once without blocks |
| Tool id with characters Anthropic rejects | sanitized, deterministically, both sides |
| A Responses host that ignores `include` | no `encrypted_content`, so the reasoning item is replayed without it. If the host rejects that, the call fails as a 400, and it shows up in `model test` |
| Refusal | not retried, not fallen back, surfaced |
| OpenRouter Responses without cached-token usage | `cached_input_tokens` 0, the cost priced as uncached (an over-estimate, never an under-estimate) |

## 11. Out of scope and follow-ups

- Streaming, as today.
- PDFs and citations: there's no document input above the driver yet.
- Strict tools: MCP schemas would need validating first.
- The 1-hour cache TTL.
- Replaying reasoning across user turns (Opus 5.5's "preserved thinking"),
  opt-in once Ferrule's prompt prefix is stable (the roadmap's "stable
  prompt prefix for caching").
- Server-side fallback (`fallbacks: "default"`), and the thinking-binding
  beta header.
- M25's router itself.
