# OpenTelemetry export

Ferrule can send what its agents do to any OpenTelemetry backend as traces: a
span per session, per turn, per model call and per tool call, carrying the
GenAI semantic-convention attributes (model, tokens, cost). It's off until you
set an endpoint. The design and its as-built notes are in
[m33-ops.md](m33-ops.md) §2.

The export is **OTLP/HTTP with JSON bodies** to `<endpoint>/v1/traces`. The
OpenTelemetry Collector, Jaeger, Grafana Tempo, Honeycomb, Grafana Cloud, the
Datadog Agent, Langfuse and Arize Phoenix all take it. gRPC and protobuf
aren't supported; put a Collector in front of a receiver that only speaks
those.

## Turn it on

```toml
[telemetry]
endpoint = "http://127.0.0.1:4318"     # OTLP/HTTP base; /v1/traces is appended
headers = { "x-honeycomb-team" = "${HONEYCOMB_KEY}" }
content = false                        # prompts, replies, tool arguments and results
service_name = "ferrule"               # the default
```

A local Jaeger to try it with:

```bash
docker run --rm -p 16686:16686 -p 4318:4318 jaegertracing/all-in-one
# [telemetry] endpoint = "http://127.0.0.1:4318", then run anything:
ferrule run "list the files here"
# open http://127.0.0.1:16686 and pick the service "ferrule"
```

A collector on `127.0.0.1` or your LAN works under the default
[egress policy](egress.md): the configured endpoint's host and port are let
through the private-address guard, and under `default = "deny"` too.

**Headers carry keys by name, never by value.** Write `${VAR}` and export
`VAR` in the environment ferrule runs in (or save it with `ferrule setup`). A
header whose value looks like a key, or an `Authorization` header (or one named like
a secret, `*TOKEN*`, `*KEY*`) with a literal value, is refused when the config loads, with a pointer
here. A header variable that looks like a secret (`*_KEY`, `*_TOKEN`, …) is
bound to the collector's host in `[secrets]` automatically, so the request
leaves ferrule carrying a placeholder and the credential proxy swaps the real
value in only for that host.

```toml
# Honeycomb
[telemetry]
endpoint = "https://api.honeycomb.io"
headers = { "x-honeycomb-team" = "${HONEYCOMB_API_KEY}" }

# Grafana Cloud (the header is "Basic <base64 of instance:token>")
[telemetry]
endpoint = "https://otlp-gateway-prod-eu-west-2.grafana.net/otlp"
headers = { "Authorization" = "Basic ${GRAFANA_OTLP_BASIC}" }
```

No credentials in the endpoint URL either (`https://user:pass@…` is refused).

## The span tree

```
ferrule.session <session id>          one trace per root tree
└─ ferrule.turn                       one per agent run (a message, a task, a goal)
   ├─ chat <model>                    one per model call
   ├─ execute_tool <name>             one per tool call
   └─ chat <model> …
      └─ ferrule.session agent-…      a sub-agent, under the turn that spawned it
```

**Model calls** (`chat <model>`):

- `gen_ai.operation.name = "chat"`, `gen_ai.provider.name`,
  `gen_ai.request.model`, `gen_ai.response.model`;
- `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`,
  `ferrule.usage.cached_input_tokens`, `ferrule.usage.cache_write_input_tokens`;
- `gen_ai.conversation.id` (the session id);
- `ferrule.cost_usd`, `ferrule.iteration`, `ferrule.tool_calls`,
  `ferrule.route.tier`, `ferrule.first_token_ms`, `ferrule.call_kind` for
  compaction and status calls;
- a failed call has status `ERROR` and `error.type`; a retried one has
  `ferrule.retried = true`.

**Tool calls** (`execute_tool <name>`): `gen_ai.tool.name`,
`gen_ai.tool.call.id` and `gen_ai.tool.type`, which is `function` for the
built-ins, `extension` for MCP and plugin tools (with `ferrule.mcp.server`),
and `agent` for the sub-agent tools. A failed call has status `ERROR`.

**Turns** carry `ferrule.task_shape`, `ferrule.origin`, and the token and cost
totals of their calls.

**When things arrive.** A turn is exported when it finishes. A session span
is exported when the session closes: a sub-agent's when it finishes, the rest
at exit, or when more than 256 sessions are open at once (the oldest is
closed, `ferrule.closed = "evicted"`). Until then a backend shows the turns
under a parent that hasn't arrived yet. Every backend fills it in once it
does, and the turns can be found by `gen_ai.conversation.id` meanwhile.
Spans still open at exit are closed with `ferrule.closed = "shutdown"`.

**Not exported:** calls the agent loop doesn't make (learning-loop reviews,
search, embeddings), eval results, and egress refusals. Refusals are on your
side already: the ledger, `ferrule trust audit`, the dashboard.

## Content: off unless you ask

By default spans carry names, timings, token counts and cost, and no text.
`content = true` adds, each cut to 4 KiB:

- `gen_ai.input.messages` on the turn (the goal or message);
- `gen_ai.output.messages` on model calls (the reply text);
- `gen_ai.tool.call.arguments` and `gen_ai.tool.call.result` on tools.

Each goes through the scrubber first: every bound `[secrets]` value becomes its
placeholder, then the gateway's redactor takes out channel tokens, provider
keys and token-shaped strings. A key that reached a tool result leaves as its
placeholder, not as itself. Still: turning content on sends your
prompts and your files' contents to the backend. Choose one you'd give that
to.

## When the collector is down

Exporting never slows the agent down:

- Rows and events go into a bounded queue (2048). **When it's full, they're
  dropped** and counted; the agent never waits.
- A separate thread sends batches of up to 512 spans, or whatever it has
  every 2 seconds, with a 10 s timeout per request.
- A failed request drops that batch (no retry queue, so a dead collector
  costs bounded memory), counts its spans as failed, and backs off, doubling
  up to 30 s.
- At exit, ferrule closes the open spans and waits up to **3 seconds** for the
  last batch.

The counters (`exported`, `dropped`, `failed`, the last error and the last
success) are in:

- `ferrule doctor`'s `telemetry` section, from `<data>/telemetry/status.json`,
  which the exporting process rewrites every 10 s and at exit. A last error
  or failed spans are a warning;
- the gateway's `/status` health section;
- the debug log.

Error text never includes the endpoint URL.

## Testing it

The tests run a mock collector on `127.0.0.1`: `cargo test -p ferrule-otel`.
They check the span tree and attributes, that there's no content by default
and scrubbed content when asked, that a full queue drops and counts, that a
refusing port and a collector that never answers don't stall a turn, and that
the shutdown flush keeps its deadline.

Against a **real collector** (not run in CI):

```bash
docker run --rm -p 16686:16686 -p 4318:4318 jaegertracing/all-in-one
FERRULE_OTEL_LIVE_ENDPOINT=http://127.0.0.1:4318 \
  cargo test -p ferrule-otel --test export -- --ignored live
# optional, for a hosted backend: FERRULE_OTEL_LIVE_HEADER='x-honeycomb-team=…'
```

It sends one turn as the service `ferrule-live-test` and checks that the
collector took every span.

## Not in this

- Metrics and logs signals: traces only.
- Ferrule's own internal debug spans (`tracing`): the export is the agent's
  work, not ferrule's internals.
- Sampling: a trace is one conversation turn, which is low volume.
- Replaying past ledger rows into a backend (a possible `ferrule telemetry
  replay`).
