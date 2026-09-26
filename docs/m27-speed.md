# M27 — speed: parallel reads, streaming replies, a cache-stable prefix

**Status:** design, 2026-09-26. User guide: [`speed.md`](speed.md).

Three gaps the research called the most *felt* (`research-number-one-harness-strategy.md` §5):
- the loop runs a response's tool calls one after another, even when they only read;
- no driver streams, so a Telegram reply appears only after the whole turn;
- the system prompt changes between sessions of the same chat, so the provider's prompt cache misses.

M27 closes all three and puts the numbers in the ledger.

## 1. Parallel read-only tool calls

### What counts as read-only

A new `Tool::read_only()` returns `false` by default. It is `true` only for:

| tool | why it's safe |
|---|---|
| `read_file`, `list_dir` | reads inside the workspace |
| `web_fetch` | a GET through the proxy |
| `recall`, `search_history` | read the memory and transcript DBs |
| `read_skill_file`, the connections' `list` | read their own stores |
| `mcp__*` with `readOnlyHint: true` | the server's own claim (see threat model) |

It is not `changes_files()`. `write_todos`, `log_diary`, `remember` and the other memory writers all say `changes_files() == false`, because they don't affect the check, but they do write, and two of them racing would reorder their writes.

Everything else stays serial: `write_file`, `shell`, agent spawns, `activate_skill`, extension tools, MCP tools without the hint, and unknown tools (a `ToolNotFound` is never in a batch).

The only edits to the read tools are those one-line `read_only()` overrides, next to `changes_files()`. Dispatch lives in the loop. M26 also touches `fs_tools.rs`, so expect a trivial merge there.

### A mixed batch: writes are barriers

The calls of one response are cut into **segments**. A run of two or more consecutive read-only calls is one parallel segment, and every other call is a segment of its own, run as today. `[read a, read b, write a, read a, list]` runs as `{read a ‖ read b}`, then `write a`, then `{read a ‖ list}`.

Why not only a read-only prefix? The observable result is the same as the serial order either way. Reads don't change what another read sees, and a write stays ordered against every call before and after it. The general form costs nothing more to build. Why not all serial when writes are mixed in? It would throw away the common "read three files, then edit one" shape.

### One segment, three phases

1. **Gate, serially, in order.** For each call: the stop check, `ToolCallStarted`, the M19 guard (`before_tool_call`, which may wait on an approval), then PreToolUse hooks. Each step races the halt, as today. Approvals therefore still come one at a time, in call order. A refused or blocked call gets its result here and is not run.
2. **Run, concurrently.** The admitted calls each go into their own tokio task, holding owned `Arc<dyn Tool>` and `ToolContext` clones. Limits:
   - concurrency is capped by a semaphore, `[agent] parallel_tools` (default 4; `1` turns it off);
   - an MCP **stdio** server takes one call at a time (§1.1);
   - each tool keeps its own timeout (MCP `timeout_secs`, `web_fetch`'s), because nothing about the call changes;
   - the sandbox and proxy apply as before, because the call still enters through `Tool::call`.

   The whole phase races the M19 halt and polls the stop flag every 25 ms. On either, the unfinished tasks are aborted (dropped, as a serial call is today). Calls that already finished keep their results, and the rest get `not run: …`.
3. **Finish, serially, in the original order.** For each call:
   - PostToolUse, only for a call that was reached, as today;
   - `ToolCallFinished`;
   - the stuck detector's `Step`;
   - the `tool_result` message, in the original order;
   - M25's misfit and repeat signals.

   A failed call is an `error: …` result like any other and never loses its neighbours'. A panicking tool becomes `error: the tool crashed`, where serially it would have taken down the run.

The difference from serial, stated plainly: call 2 of a parallel segment starts before call 1's PostToolUse hook runs. PostToolUse can't stop a later call anyway (its block only adds a note), so no hook loses power.

The M19b watchdog sees progress through the per-call events, as before. The M19 caps are asked before every *model* call, and a batch contains none.

### 1.1 MCP: the client multiplexes, the server may not

`McpClient` already multiplexes: every request has its own id and `pending` entry, stdin writes hold a lock per line, and the reader routes responses by id. So concurrent calls are legal on the wire, and the JSON-RPC spec allows them.

Many stdio servers still handle one request at a time. The M17 timeout starts when the request is *sent*, so a second call queued inside a single-threaded server would burn its timeout waiting behind the first. To prevent that, `Tool::serial_group()` returns `mcp:<server>` for a stdio server's tools, and a phase-2 task takes that group's lock before calling. Two stdio servers still run in parallel with each other. URL (Streamable HTTP) servers have no group, since each call is its own HTTP request.

### Ledger rows

Tool calls have never had ledger rows; the ledger is one row per provider call. "Its own ledger row" in the brief maps onto the per-call transcript step and events, which are unchanged. The batch's timing goes on the next provider-call row (§4).

## 2. Streaming

### Plumbing

- `CompletionRequest` gets `stream: Option<DeltaSink>`. A `DeltaSink` is an `Arc<dyn Fn(Delta)>` with a hand-written `Debug`, and `Delta` is `Text(String)` or `Progress`, the latter covering reasoning and tool-argument bytes.
- A driver streams **only** when a sink is present. Without one, the request body and the parsing are byte-for-byte today's. That keeps the eval, the tasks, `ferrule run`, compaction and sub-agents exactly as they were.
- Routing and fallback wrappers pass the request through, so the sink rides along for free.

### The three drivers

Each driver reassembles the stream into the JSON its non-streaming parser already reads, then calls that parser once. Tool arguments, thinking signatures and usage therefore go through one code path, and the wire format is parsed in one place per driver.

- **Chat Completions** sends `stream: true, stream_options: {include_usage: true}`.
  - It accumulates `delta.content`, `delta.reasoning_content`/`reasoning`, and `delta.tool_calls[]` by `index`. The id and name come from the first fragment, and `arguments` are concatenated and parsed once.
  - It keeps the last `finish_reason`, and takes usage from the final chunk.
  - A 400 on the streaming request is retried once without streaming, for a compatible server that rejects `stream_options`.
- **Anthropic Messages** sends `stream: true`.
  - `content_block_start`/`_delta`/`_stop` rebuild each block: `text_delta`, `input_json_delta` (concatenated, parsed at `_stop`), `thinking_delta` and `signature_delta`.
  - Usage comes from `message_start` (input, cache read, cache write), with `message_delta`'s fields laid over it (output, final counts).
  - `stop_reason` comes from `message_delta`.
  - The thinking-400 retry still works, because the 400 arrives before any event.
- **Responses** sends `stream: true`.
  - Text deltas come from `response.output_text.delta`.
  - The whole final response comes from `response.completed` (or `response.incomplete`), whose object is exactly the non-streaming body, usage included. `response.failed` goes through the existing status handling.

### Failure modes

- A non-2xx reply is read and classified exactly like `send()`, including `Retry-After`.
- A 2xx reply that isn't `text/event-stream` is parsed as plain JSON: **the non-streaming fallback**. A server that ignores `stream: true` works unchanged.
- The stream breaking mid-way (read error, reset) becomes `CoreError::Transient("stream broke after N events: …")`. `class()` reads it as Server or Timeout, so M19b retries it, M21 falls back on it and M25 escalates on it exactly as on a failed request.
- A stream that ends with no terminal event (`[DONE]`, `message_stop`, `response.completed`) is Transient, "stream ended early".
- An error event inside the stream goes through `error_in_body`. Anthropic's `overloaded_error` is therefore Overloaded, and a rate limit is RateLimited.
- 5 minutes with no bytes at all is Transient, "stream stalled". Anthropic pings, and the others send keep-alives, so this is a dead connection, not a slow model. The client's 10-minute total timeout still caps one call.

Each attempt starts with a `Reset` to the reply stream, so a retry, a fallback or an escalation replaces the half-shown text.

### Usage stays exact

Usage comes from the provider's own final event, never from counting deltas. A stream that never delivers usage (a compatible server ignoring `include_usage`) records zeros, as a non-streaming reply without `usage` does today, and logs a warning.

### Where the deltas go

`Agent::set_reply_stream(Option<ReplyStream>)` attaches a sink for the whole run. The agent passes it on `turn` and `status` calls only, never on compaction or learning calls. It sends a `Reset` at each call and each attempt. A tool-calling response's preamble ("let me look…") is therefore visible while it streams and replaced by the next call's text. The final answer is always the last call's text, as today.

**The gateway.** A lane whose channel can edit, with streaming on, builds a `StreamingReply` for the turn: a task fed by an unbounded channel from the sink.

- **First message** after 1 s or 60 chars of text, whichever comes first. A quick answer never streams and goes out through today's `send`.
- **Edits** at most one per second per chat. The lane is the chat, so one lane has one editor. Deltas in between only update the buffer.
- **429:** the Telegram adapter now returns `GatewayError::RateLimited { retry_after }` for `sendMessage` and `editMessageText`. The streamer sends nothing more to that chat until it passes.
- **Rollover:** past 4000 chars (Telegram's cap is 4096; the margin covers the UTF-16 counting), the current message is edited to its final chunk and a new message continues. The chunks break at a newline or space when one is within the last 20 %.
- **The final edit** carries the complete final text, cut into the same chunks: an edit per existing message, a send per extra one. Messages left over from a longer preview are edited to `…`.
  - An edit Telegram rejects as "message is not modified" counts as done.
  - Any other edit failure sends the final text as new messages, so the answer is never lost.
- **Formatting mid-stream: plain text.** The final edit carries exactly what `send` would have sent. Today that is plain text too (no `parse_mode`), so nothing changes visibly. Justification: formatting a half-received reply means unclosed code fences and entities, and Telegram rejects a broken entity with a 400. Plain text can never fail to parse. When the adapter learns formatting, only the final edit gets it.

The watchdog counts a streamed delta as progress.

`sendMessageDraft` is out of scope: not trivial, and bot-API-version dependent.

**Switches.**
- `[agent] stream = true` (default) turns streaming on or off everywhere, and `[gateway] telegram_stream` overrides it for Telegram.
- Channels that can't edit, scheduled tasks (their pseudo-channel has no adapter), `ferrule run` and the eval get today's final text.
- `ferrule chat` prints deltas as they come and, on a reset, starts a fresh line.

## 3. A cache-stable prefix

What a provider caches is the longest byte-identical prefix: tools, then system, then messages. Everything stable goes first, and anything volatile goes after it or into the latest user message.

Audit of what goes into a request today:

| part | stable? | action |
|---|---|---|
| tool definitions | sorted by name, `ToolRegistry::definitions()` | none; a test pins it |
| system prompt: base, workspace, directive, credentials note, browser, AGENTS.md baseline, validation, skills catalog, playbook, plan note | built once per agent; same inputs give the same bytes | none |
| **session recall block** | **appended to the system prompt on the first run** | **moved** (below) |
| hook notes, inbox, check failures, nudges | already user messages after the goal | none |
| time, diary, todos | not in the prompt at all | none |

**The one move.** The recall block is picked from the goal, so it differs per session. It also used to change the system prompt after the first request of every lane rebuild (a gateway restart), which invalidates the whole cache. It now goes into a user message, `[ferrule memory]\n…`, placed right before the goal of the run that recalls it.

- The system prompt is then byte-identical for every session with the same config, skills and playbook.
- Recall's own query skips messages with that marker when it picks the session's first user message.
- The message is written to the transcript, so a rebuilt lane replays it as history, a stable part of the prefix.

**Anthropic breakpoints, re-checked:**
1. The last system block, or the last tool if there's no system. Now truly stable across sessions, so this is where the gain lands.
2. The end of the request, unchanged.
3. The previous user block. On the one run that recalls, the previous user block is the new memory message, which the previous request never had, so this breakpoint writes instead of reading on that request only. Breakpoint 1 still reads, and it is the large part. On every later turn it's back to M23's behaviour: it covers the previous turn's prefix after that turn's thinking was dropped.

**What legitimately breaks the prefix** (documented in `speed.md`):
- MCP `tools/list_changed` and an MCP hot-add or suspend change the tool list;
- a skill install or removal changes the catalog;
- a learning pass that changes the playbook;
- `/model` or an escalation to another model: another cache;
- compaction rewrites the history.

All are rare and deliberate.

**Tests.**
- Byte identity: the serialized system, tools and message prefix of request *n* are identical to request *n+1* across a tool round-trip and across two turns, recall and hooks included.
- A golden request for a run with routing, streaming and parallelism all off: exactly the old request, apart from the recall move.
- Each driver's payload without a sink has no `stream` key and is unchanged.

**Cache-hit ratio.** `ferrule ledger` prints the overall cache hit (cached input over input, `eval_result` rows excluded) under the table. The dashboard's ledger summary API gains a `cache_hit_pct`.

## 4. Measuring it

The ledger stays one row per provider call; no new `call_kind`, because every consumer counts non-`eval_result` rows as calls. One optional field, `speed`, is written only when it has something in it:

- `first_token_ms`: on a streamed call's row, from the request to its first delta.
- `first_visible_ms`: once per turn, from `Agent::run` to the first thing the person could read. It is the streamer's first delivered message when there is one. Otherwise it is the end of the answer call, since the send follows at once; its latency isn't counted.
  - Stamped on the first row written after the mark exists, or on the final answer's row.
  - A turn that halts before either has none.
- `tool_batch {calls, parallel, wall_ms, sum_ms}`: the previous response's tool calls, stamped on the next provider-call row of the run.
  - `parallel` is how many ran in a parallel segment.
  - A run that ends right after its tools (a halt) loses its last batch's line; the tools still ran.

`ferrule ledger` adds a **Speed** block under the table (p50 time to first token and first visible reply, parallel batches' wall vs sum, cache hit). It prints nothing extra when there's no data.

## 5. The eval must not move

The eval runs without session recall, without a reply stream and with no sink, so its requests are unchanged. Parallelism changes timing only: the order of results and messages is the serial order. The expectation is **20/20, 11/20, $0.98** with the same token counts. If they move, the delta is explained in the PR.

## 6. Threat model

- **A lying `readOnlyHint`.** An MCP server can claim read-only and write. What it gains: its write runs concurrently with *other reads*, never with a write or past a barrier. It ran unsandboxed-or-not exactly as before, and its call still went through the guard and hooks. A server that lies already had its write; M27 adds no capability.
- **Approvals in parallel.** They are asked in phase 1, serially, before anything runs, so no approval prompt races another and a refusal can't be outrun.
- **Halt and stop mid-batch.** Aborting a task drops the tool's future, as serial dropping does, so the shell's process-group kill and MCP's pending-entry cleanup are unchanged.
- **Streaming leaks.** Streamed text is the model's reply, which the chat would get anyway. A `Reset` removes preview text from the screen, but the person may have read it. This is the same exposure as a reply, and tool output never streams, only the model's text. No secrets flow through deltas that wouldn't flow through the final message.
- **Telegram abuse.** Edits are throttled to 1/s per chat and 429s are honoured, so a long stream can't get the bot rate-limited globally.

## 7. Out of scope

- `sendMessageDraft` and formatting (Markdown/HTML) mid-stream.
- Streaming to non-edit channels, scheduled tasks and `ferrule run`.
- Parallel writes, and parallel sub-agents in one response.
- Speculative tool execution.
- Caching knobs beyond re-checking M23's three breakpoints: TTLs and a 1-hour cache.
