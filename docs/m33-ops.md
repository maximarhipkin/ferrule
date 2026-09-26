# M33 — ops: egress policy, OTel export, migration importers

**Status:** built, 2026-09-26 (branch `m33-ops`; as-built notes inline). User guides:
[`egress.md`](egress.md), [`otel.md`](otel.md), [`migrate.md`](migrate.md).

This milestone covers three items from `research-number-one-harness-strategy.md` §4:
- **17**: an egress domain policy in the credential proxy, plus a unix-socket
  allowlist that closes the documented `docker.sock` escape;
- **18**: OTel export from the ledger seam;
- **16**: importers from OpenClaw and Hermes.

Each part is one reviewable commit with its own tests. The SSH execution
backend is M34, so the shell's exec path stays as it is. M32's WASM plugins
land in parallel. The only thing they share with this work is
`ferrule_tools::egress::client_builder(Option<&Egress>)`, whose signature
doesn't change, so a plugin's HTTP inherits the policy for free.

## 0. What stays as it is

- **`[secrets]` behaves exactly as before.** Placeholders, host binding,
  MITM only for bound hosts, scrubbing, and "secrets only over HTTPS, except
  to loopback" are unchanged. Every existing `ferrule-proxy` test passes
  unmodified.
- **A config with no `[egress]` table changes nothing for shell commands.**
  They get the proxy variables only when secrets are live (as today) or when
  the owner writes `[egress]` rules. Pushing a working `pip install` through
  a new proxy on upgrade would be a regression nobody asked for.
- **The eval** stays at engineered 20/20, naive 11/20, $0.98.
- **Model providers aren't proxied.** They never were: their keys are
  ferrule's own, not the model's. The policy covers what the *model* can
  reach.

---

## 1. Egress policy (item 17)

### 1.1 Where it is enforced

In the credential proxy (`ferrule-proxy`), because every channel the model
can make ferrule's HTTP go through already passes it, or can be made to:

| Path | Through the proxy today | M33 |
|---|---|---|
| `web_fetch` | only when a secret is live | always |
| `web_search` (HTTP backends) | only when a secret is live | always |
| MCP servers reached by URL | only when a secret is live | always |
| MCP stdio servers' own HTTP | the child env, when a secret is live | the child env whenever the proxy runs *and* rules exist |
| shell `curl`/`pip`/`git` (via `HTTP(S)_PROXY`) | when a secret is live | when a secret is live or `[egress]` has rules |
| M32 WASM plugins | through `client_builder` | same, unchanged signature |

**The proxy now always runs** in a process that builds a sandbox. `Broker`
starts when a secret is live, as before, *or* when an egress policy is
given. The CLI always gives one, since the default policy has a job (§1.3).
The CA is created on that first start. It's the same key that `[secrets]`
already creates and hides from the sandbox (`<data>/private/proxy/keys`).

For shell commands with the network on, the proxy is still **advisory**: a
command that ignores `HTTPS_PROXY` connects directly. That's the M8 threat
model, and it's said plainly in `docs/egress.md`. An opt-in `enforce` mode
that makes it binding is deferred (§1.9).

### 1.2 Who is asking: the source

The proxy's Basic credential username says which kind of client it is. The
password is the same per-process random token:

- `ferrule`: sandboxed commands and the helpers they start (the env
  `child_env()` hands out). This is unchanged, so every existing URL works.
- `ferrule-tool`: ferrule's own in-process clients acting for the model
  (`web_fetch`, `web_search`, MCP-over-HTTP, plugins). They use
  `Broker::tool_proxy_url()` in the `Egress` the sandbox hands them.

The only difference is **loopback**:
- A shell command can reach `localhost` directly anyway, and the dev server
  it just started is the common case, so loopback is allowed for it.
- `web_fetch` is the classic SSRF vector (a page tells the model to fetch
  `http://127.0.0.1:2375/containers/create`), so for tool clients loopback
  is private like the rest (§1.4).

Per-agent or per-sub-agent policies were considered and **left out**:
- The sandbox, and so the proxy env, is a process-wide `OnceLock` that every
  agent shares.
- Per-agent tokens would mean rebuilding `Egress` per agent and threading an
  agent id through every tool constructor.
- The `source` split covers the case that matters (the model's tools versus
  the commands it runs).

Recorded as a follow-up.

### 1.3 The default: allow public, block private

```toml
[egress]
default = "allow"          # or "deny": only `allow` goes
allow = []                 # host patterns, IPs or CIDRs
deny = []                  # always wins over allow
private = "block"          # or "allow": turn the private-range guard off
private_allow = []         # private hosts/CIDRs reachable anyway (a NAS, a LAN Ollama)
```

**Why the default stays "allow all public":**
- Every workflow in the eval and the docs fetches documentation, package
  indexes and APIs nobody lists ahead of time. A default-deny egress makes
  the first `pip install` fail. Users then set `default = "allow"` and never
  come back, which leaves them *less* protected than a narrow default that
  stays on.
- The narrow default that *can* stay on is the one with no false positives
  on the public internet: **private destinations are blocked**. That covers
  loopback (for tool clients), RFC 1918, CGNAT, link-local and the cloud
  metadata endpoints. It's the SSRF class that turns "the model read a
  malicious page" into "the model read the instance's IAM credentials".
- An owner who wants an allowlist writes `default = "deny"` plus `allow`.
  `ferrule setup` offers exactly that as a starting policy (§1.8).

**Rules** are host patterns in the `[secrets]` syntax (`api.github.com`,
`*.githubusercontent.com`; a leading `*.` is the only wildcard and needs
two labels after it), plus IP literals (`203.0.113.7`, `2001:db8::1`) and
CIDRs (`10.0.0.0/8`). The order of evaluation, first match wins:

1. `deny` matches the host name or the connected IP: **denied** (`rule`).
2. The destination IP is private, and `private = "block"`, and neither
   `private_allow` nor an implicit exception (§1.5) covers it: **denied**
   (`private`).
3. `default = "deny"` and nothing in `allow` matches the name or IP:
   **denied** (`not-allowed`).
4. Otherwise **allowed**.

**IP literals.** A request to `http://93.184.215.14/` is judged like a
name:
- `deny`/`allow` match it through IP and CIDR entries;
- the private guard applies to it directly;
- under `default = "deny"` a bare IP needs an explicit entry. A host-name
  allowlist is otherwise trivially bypassed by resolving the name yourself.

Decided against refusing IP literals outright. Plenty of legitimate APIs
and every LAN device are addressed that way, and the private guard already
covers the dangerous ones.

**The private ranges** (`egress::is_private`):
- v4:
  - `0.0.0.0/8`
  - `10/8`
  - `100.64/10` (CGNAT, which Tailscale also uses)
  - `127/8`
  - `169.254/16` (link-local, **including `169.254.169.254`**)
  - `172.16/12`
  - `192.0.0/24`
  - `192.168/16`
  - `198.18/15`
  - multicast and reserved `224/3`
  - broadcast
- v6:
  - `::`
  - `::1`
  - `fc00::/7` (ULA; `fd00:ec2::254` is AWS's v6 metadata address)
  - `fe80::/10`
  - `ff00::/8`
- IPv4-mapped (`::ffff:a.b.c.d`) and IPv4-compatible forms are judged by
  their v4 address, and so are NAT64 (`64:ff9b::/96`) addresses.
- `169.254.169.254` and `fd00:ec2::254` count as **metadata**. Only an
  exact `private_allow` entry for that IP opens them: a broad
  `private = "allow"` or `private_allow = ["169.254.0.0/16"]` doesn't.

### 1.4 DNS rebinding: resolve once, connect to what was checked

The proxy resolves the name itself (`tokio::net::lookup_host`), checks
**every** returned address, and then connects to one of the checked
addresses, never to the name again. A name that resolves to both public and
private addresses is denied: an attacker controls the ordering. The TLS
handshake still uses the name, so certificates verify as before.

**With an upstream proxy** (`HTTPS_PROXY` set on ferrule itself), the
upstream connects, not ferrule. The proxy resolves the name locally just to
check it, then sends the *name* upstream. That leaves one gap, which is
documented: a name that resolves differently on the upstream (split-horizon
DNS) is judged by the local answer. When local resolution fails, which is
common where only the corporate proxy resolves, the request is **allowed**
and noted in the debug log. Blocking would break every corporate setup, and
reaching the corporate proxy's own internal view is what the owner asked
for by setting it. IP literals are always checked.

### 1.5 Implicit exceptions: local model servers and configured endpoints

Some private destinations are ferrule's own configuration, and the private
guard mustn't break them. At startup the CLI adds each such **host:port**
(not the whole host) as an implicit `private_allow` entry:

- each `[providers.*].base_url`, which makes an Ollama or llama.cpp on
  `localhost:11434`/`:8080` reachable to a shell `curl` and to embedding
  calls;
- `[memory] embedding` base URLs;
- MCP servers' `url`s;
- `[telemetry] endpoint` (§2);
- the web search backend's base URL (SearXNG on the LAN is common).

These only open the private guard. An explicit `deny` still wins, and under
`default = "deny"` a configured endpoint is *allowed* too. An owner who
configured it obviously wants ferrule to reach it.

The CLI's `shared_broker` builds the list, so `ferrule doctor` shows it.

### 1.6 What a denial looks like

**To the client:**
- **Plain HTTP:** `403 Forbidden`, header `x-ferrule-egress: denied`, and a
  text body.
- **CONNECT:** the proxy answers `200`, terminates TLS with its CA (a
  one-off leaf for the host), and then answers every request on the tunnel
  with the same 403. A `403` to the CONNECT itself makes curl print
  `CONNECT tunnel failed, response 403`, and most clients drop the body. A
  real response inside the tunnel is what gets the explanation to the model.
  A client that doesn't trust ferrule's CA gets a TLS error instead, which is
  no worse than the old `403` on CONNECT. Every ferrule client, and every
  sandboxed command when the proxy env is set, trusts it.

The body is written for the model:

```
ferrule egress policy: blocked https://169.254.169.254/ (private address 169.254.169.254: cloud metadata).
This is the owner's network policy, not a network error; retrying won't help.
If this host is needed, ask the owner to add it to [egress] private_allow (see docs/egress.md).
```

`web_fetch` now checks the response. A refusal (`x-ferrule-egress: denied`)
is a failed call carrying the proxy's text. It used
to turn a 404 page into "successful" text; now any other non-2xx page keeps
its text but starts with `HTTP <status>`, a small behaviour change noted in
the PR. An MCP server reached by URL gets the proxy's full text too, rather
than the 300-character excerpt of an error body.

**To the owner:**
- a **ledger row**: `call_kind = "egress_denied"`, `provider = "egress"`,
  `model = <host>`, `outcome = "error"`, `error_kind = <reason>`, and
  `error_message = "<source> <method> <url-ish>"` (host and port only, no
  path or query, which can carry data);
- a **trust audit event** `egress_denied` with the same detail.

`Broker::on_deny(hook)` takes an `Fn(&Denial) + Send + Sync`, and the CLI
wires it to both. Ledger readers that sum cost or count calls
already skip `eval_result`, and they now skip `egress_denied` too, so the
dashboard and `/cost` don't count a denial as a model call. The dashboard's
overview gets a "blocked egress (24 h)" count with the last few hosts. It's
a cheap query over the same rows.

Denials are rate-limited in reporting, not in enforcement: at most one row
per (source, host, reason) per 10 s, so a retry loop doesn't flood the
ledger.

### 1.7 Unix sockets: closing `docker.sock`

The known limit carried since M26: a sandboxed command can connect to any
pathname unix socket it can reach. A reachable `/var/run/docker.sock` (or a
podman socket, containerd, or the systemd user bus with `systemd-run
--user`) is a full escape, **even with `network = false`**, because seccomp
allows `AF_UNIX` and Landlock has no connect right.

**Probe results (Linux 7.0, Landlock ABI 8):**
- there's still no filesystem right for connecting to a socket;
- ABI 6's `scoped` covers *abstract* sockets only;
- `landlock_net` covers TCP only;
- mount namespaces need unprivileged user namespaces, which Ubuntu 24.04
  (and the GitHub runner) restricts via AppArmor
  (`apparmor_restrict_unprivileged_userns=1`).

So neither Landlock nor namespaces can do it.

**Linux: seccomp user notification with a connecting supervisor.**
1. When the allowlist is on, the filter returns `SECCOMP_RET_USER_NOTIF`
   for `connect(2)`, on every socket. It also refuses `io_uring_setup`,
   since `IORING_OP_CONNECT` bypasses seccomp. This is added to the existing
   filter logic: `socket()` for non-unix domains is still refused when
   `network = false`.
2. `pre_exec` installs the filter with `SECCOMP_FILTER_FLAG_NEW_LISTENER`
   and passes the listener fd to the parent over a `SOCK_CLOEXEC`
   socketpair (`SCM_RIGHTS`). Then it closes its copy before `exec`, so the
   sandboxed program never holds its own listener.
3. A supervisor thread in ferrule receives each notification:
   - The notification carries the calling *thread's* id. `pidfd_open`
     refuses a non-leader thread before 6.9, so the thread group comes
     from `/proc/<tid>/status`. Memory and cwd are read per thread.
   - It **takes a copy of the child's socket** with `pidfd_getfd`.
   - It reads the sockaddr from `/proc/<pid>/mem`.
   - It re-checks `SECCOMP_IOCTL_NOTIF_ID_VALID`, so the pid wasn't
     recycled.
   - **It makes the connect itself** on the duplicated descriptor. The
     child's socket is the same open file, so the child ends up connected,
     and the supervisor answers with the result (0 or `-errno`).
   - Because ferrule connects to what it checked, `SECCOMP_USER_NOTIF_FLAG_CONTINUE`
     is never used. With CONTINUE, a sibling thread could rewrite the
     sockaddr between the check and the kernel's re-read, which is the
     documented TOCTOU.
4. **For `AF_UNIX` paths:**
   - resolve relative to `/proc/<pid>/cwd` (absolute paths via
     `/proc/<pid>/root`);
   - `open(O_PATH)`, which follows symlinks to the real inode;
   - `fstat` must be `S_ISSOCK`;
   - `readlink /proc/self/fd/N` gives the real path, which is checked
     against the allowlist;
   - connect to `/proc/self/fd/N`, which is that same inode.

   A symlink, rename race or `..` trick changes nothing, because the inode
   checked is the inode connected. Abstract sockets (`\0name`) are allowed
   only when listed as `@name`. For any other family, the supervisor
   connects the copy to the child's sockaddr unchanged.
5. The supervisor runs one thread per notification, because a blocking TCP
   connect can take 30 s. It runs until the listener hangs up, which
   happens when the last process holding the filter exits.

**The default allowlist** (`unix_sockets_default = true`) is
`$SSH_AUTH_SOCK` (git over ssh), gpg-agent's sockets (`~/.gnupg/`,
`$GNUPGHOME/`, `/run/user/<uid>/gnupg/`), the NSS helpers (nscd,
`/run/systemd/resolve/`, `/run/systemd/userdb/`), journald and `/dev/log`,
and the PostgreSQL and MySQL socket dirs. macOS: mDNSResponder, syslog,
launchd's per-user dirs, `/private/tmp/mysql.sock` and
`/private/tmp/.s.PGSQL.*`.

**X11 is not on it** (as built, a change from the first draft): an X
connection can inject keystrokes into any other window, a terminal
included, which is an escape. `unix_sockets = ["/tmp/.X11-unix/"]` puts it
back.

**The command's own sockets.** Sockets in the writable roots and temp dirs
are *not* allowed wholesale, because `/tmp` also holds the owner's tmux
server, an editor's server socket and VS Code's IPC, each a way to run
anything. A socket there is allowed when **the process listening on it
belongs to the command**. The supervisor opens a throwaway connection of its
own, reads the listener's pid with `SO_PEERCRED`, and walks `/proc/<pid>/stat`
up to the command's root pid or its process group. Linux only: Seatbelt can't
ask who's listening, so on macOS the workspace (not `/private/tmp`) is
allowed by path. Details:
- a datagram socket there has no listener to ask, so it is refused unless
  listed;
- a socket with more than one hard link is only allowed by its exact path,
  so a hard link to `docker.sock` inside an allowed directory doesn't ride
  in on the directory rule.

**Not on it:** docker, podman, containerd, `/run/systemd/private`, the
system and session D-Bus. Owners add sockets with `unix_sockets =
["/var/run/docker.sock"]`. The listed entries are added to the defaults,
and `unix_sockets_default = false` drops the defaults.

**Not covered:** datagram `sendto`/`sendmsg` with a destination path (such
as `/dev/log`). None of the escape sockets are datagram sockets. Documented.

**When it can't run:**
- If the supervisor can't be set up, the Unix-socket status reads "not
  enforced (<why>)". Causes: no `pidfd_getfd` (kernel < 5.6), or a
  container seccomp profile that blocks it. **Docker's default profile
  refuses `pidfd_getfd` without `CAP_SYS_PTRACE`.** That's the case in the
  container this was built in, so the live path was verified on the CI
  runners, not locally.
- `ferrule doctor` and `ferrule sandbox` warn, and commands run without the
  allowlist rather than failing, unless `require = true`. Under `require`,
  `Sandbox::new` fails, and the message names `unix_sockets = ["*"]` as the
  way out.
- The probe runs once per process. It spawns `/bin/sh` under the filter,
  and before exec it connects to one listed and one unlisted socket: the
  first must succeed, the second must get `EACCES`.
- On CI (`GITHUB_ACTIONS` set) the Linux enforcement test fails instead of
  skipping, so a broken supervisor can't pass as "not supported here".

**macOS (Seatbelt):**
- `network = false` already denies all `network-outbound`, unix sockets
  included, so that case was never open.
- With the network on, the profile now adds
  `(deny network-outbound (subpath "/"))`. A path filter on a network
  operation only matches `AF_UNIX` destinations, so IP traffic is
  untouched. It is followed by `(allow network-outbound (literal (param …)))`
  or `(subpath (param …))` per allowlist entry, with the paths passed as
  `-D` parameters. (This syntax was not verified on a Mac; the macOS CI
  enforcement test is the check.)
- Seatbelt matches real paths, so entries are canonicalized first.
- `/private/var/run/mDNSResponder` is always allowed, or DNS stops working.

**Windows:** Docker Desktop listens on a named pipe
(`\\.\pipe\docker_engine`), and AF_UNIX sockets are rare. The restricted
token plus the job's default DACL checks already stop a restricted token
from opening the Docker pipe, whose DACL grants the `docker-users` group,
which a restricted token has as deny-only. That is **not** new in M33 and
not claimed as a guarantee. The allowlist is unimplemented on Windows, and
doctor says so.

### 1.8 Doctor, setup, dashboard

- **`ferrule doctor`** gets an `egress` line:
  - the default;
  - the rule counts;
  - the private guard (on/off, with the implicit exceptions listed);
  - whether shell commands get the proxy (secrets or rules) or not
    (advisory);
  - the last 24 h of denials read from the ledger (count and top hosts).
- **The `sandbox` section** gets a `Unix sockets:` line, which is one of:
  - `allowlist (N entries, plus sockets the command makes itself)`;
  - `not enforced (<why>)`, a warning;
  - `none (network off)` (macOS);
  - `any (unix_sockets = ["*"])`.

  On Windows it's a note, "not covered on Windows". `ferrule sandbox`
  prints the same line as `sockets`.
- **`ferrule setup`** gets an "Network policy" step after Sandbox. It offers
  three starting points and writes `[egress]` through `Target`, like every
  other step:
  - "Allow public, block private (recommended, the default)": writes
    nothing;
  - "Allow only a list": `default = "deny"` plus a seeded `allow` for the
    configured providers' hosts, GitHub, PyPI, npm and crates.io, which the
    owner edits;
  - "Allow everything": `private = "allow"`.
- **The dashboard** shows the blocked-egress count on the overview (§1.6).

### 1.9 Out of scope

- **`enforce` mode**, which makes the proxy binding for shell commands.
  On Linux that means the same supervisor refusing TCP connects other than
  to the proxy and loopback; on macOS, Seatbelt `(remote ip "localhost:*")`.
  The supervisor path makes this cheap on Linux, but it breaks every tool
  that ignores `HTTPS_PROXY` (ssh, most database clients, anything UDP). It
  deserves its own design and tests. Follow-up.
- **Per-agent policies** (§1.2).
- **Path/method-level rules** (`allow GET github.com/org/*`). Host-level is
  what every comparable tool ships, and paths need the proxy to MITM every
  host.
- **UDP/QUIC.** The proxy is HTTP(S) only. HTTP/3 clients fall back to TCP
  through a proxy.

### 1.10 Threat model and failure modes

| Threat | Result |
|---|---|
| A page tells the model to `web_fetch` `http://169.254.169.254/latest/meta-data/iam/…` | Denied (metadata), with a ledger row and an audit event |
| `web_fetch` `http://rebind.attacker.example/`, which resolves to 127.0.0.1 | Denied: resolved once, checked, and never re-resolved |
| A name with an A record for both a public and a private address | Denied |
| `http://[::ffff:127.0.0.1]/`, `http://0x7f.1/`, `http://2130706433/` | Denied: `url`/`http` parse these into the canonical IP before the proxy sees them, and mapped forms are judged by their v4 address |
| A sandboxed `curl` that honours the proxy env reaching a denied host | Denied, with a clear 403 |
| A sandboxed program that ignores the proxy env | Not stopped while the network is on. Documented as advisory; `enforce` is the follow-up |
| `curl --unix-socket /var/run/docker.sock` | Linux: `EACCES` from the supervisor. macOS: denied by Seatbelt. Windows: not covered |
| A symlink to `docker.sock` in the workspace | Denied: the inode is checked, not the name |
| A sibling thread swapping the sockaddr after the check | Irrelevant: ferrule connects to its own copy |
| The supervisor thread panics or is slow | The child's `connect` blocks. A panic in the handler thread drops the notification, and the kernel returns `ENOSYS` to the child when the listener closes |
| The proxy is down | ferrule's clients fail with connection refused, the same failure mode as a live secret today |

---

## 2. OTel export (item 18)

### 2.1 Dependency choice: a hand-rolled OTLP/JSON encoder

**What the SDK route costs.** The `opentelemetry` + `opentelemetry_sdk` +
`opentelemetry-otlp` stack is about 25 new crates with the `http-json`
feature, and about 60 with the default tonic/prost. The three crates bump
their minor version together every few months, and each bump is breaking.

**What we actually need** is small:
- one message type (`ExportTraceServiceRequest`) as JSON over HTTP POST;
- span and trace ids;
- a batching queue.

The OTLP/JSON mapping is stable (OTLP 1.0), and every mainstream receiver
accepts it on `/v1/traces`: the Collector, Jaeger, Tempo, Honeycomb, Grafana
Cloud, Datadog Agent, Langfuse, Phoenix.

**So the encoder is written here**, over the `reqwest` and `serde_json` the
workspace already has:
- a new crate, `ferrule-otel`, of about 600 lines, with no new third-party
  dependencies;
- it doesn't change the release archive size (no `release.yml` run needed).

**What we give up:**
- protobuf-only receivers (rare; the Collector bridges them);
- the `tracing-opentelemetry` bridge for ferrule's own debug spans. That's
  out of scope: we export the agent's work, not ferrule's internals.

### 2.2 The seam

The ledger is where every model call is already described. `trust::equip` is
the single place every agent's sink (root, chat, gateway lane, scheduler,
sub-agent) is wrapped. `ferrule-otel::OtelSink` goes there *inside* the
`TrustSink`, between it and the file sink: `TrustSink(OtelSink(file))`.
`TrustSink` stamps `tree` and `cost_usd` on a row before handing it on, so
each row the OTel sink sees already carries both. Rows pass through to the
inner sink unchanged.

A ledger row can't describe the two things spans need beyond model calls:
turn boundaries and individual tool calls. Its `tool_batch` is only an
aggregate. So `LedgerSink` gains two **default no-op** methods. No existing
sink changes, and the loop calls them only when a sink asks:

```rust
fn trace_level(&self) -> TraceLevel { TraceLevel::Off }   // Off | Spans | Content
fn trace(&self, event: TraceEvent) {}
```

`Agent` asks `trace_level()` once per run and emits:

- `TurnStarted { session_id, at, goal: Option<String> }` at the top of `run`.
  `goal` is set only at `Content`.
- `ToolCall { session_id, id, name, ok, started, elapsed, arguments: Option<Value>, result: Option<String> }`
  once per finished call, in the per-call loop where `ToolCallFinished` is
  emitted. `Ran` gains the call's finish time, and the start is
  `finished - elapsed`, which is exact even for side-by-side calls.
  `arguments` and `result` are set only at `Content`.
- `CallContent { session_id, text }`: the model's reply text (and tool-call
  names), just before its ledger row, only at `Content`.
- `TurnFinished { session_id, at, ok, incomplete: Option<String> }` at the
  end of `run`, whatever the outcome.

`TrustSink` forwards both methods to its inner sink, which is the
`OtelSink` when export is on. `OtelSink` answers the higher of its own level
and its inner sink's.

### 2.3 The span tree

```
ferrule.session <session>              trace = one per root tree (random id)
└─ ferrule.turn                         one per Agent::run
   ├─ chat <model>                      gen_ai.operation.name = "chat", one per ledger row
   ├─ execute_tool <name>               gen_ai.operation.name = "execute_tool"
   │    (mcp__server__tool → gen_ai.tool.type = "extension", ferrule.mcp.server = server)
   └─ chat <model> …
      └─ (sub-agent) ferrule.session agent-a-… parented to the spawning turn
```

**Model-call spans:**
- **Timing:** `end = timestamp`, `start = end - latency_ms`.
- **GenAI semconv attributes (v1.37):**
  - `gen_ai.operation.name = "chat"` (`"compaction"` and `"status"` rows use
    `chat` too, with `ferrule.call_kind`);
  - `gen_ai.provider.name`, `gen_ai.request.model`, `gen_ai.response.model`;
  - `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`;
  - `ferrule.usage.cached_input_tokens` and
    `ferrule.usage.cache_write_input_tokens` (custom until semconv has them);
  - `gen_ai.conversation.id = session_id`.
- **Custom attributes:** `ferrule.cost_usd`, `ferrule.iteration`,
  `ferrule.tool_calls`, `ferrule.route.tier`, `ferrule.first_token_ms`.
- **Status:** an `outcome = "error"` row gets status `ERROR` plus
  `error.type = error_kind`. A `"retried"` row is `UNSET` with
  `ferrule.retried = true`.

**Tool spans:** `gen_ai.tool.name`, `gen_ai.tool.call.id`, and
`gen_ai.tool.type`, which is `function` for built-ins, `extension` for MCP
and plugin tools (any name with `__`), and `agent` for `spawn_agent`,
`wait_agent`, `resume_agent`, `close_agent` and `list_agents`. A failed call
gets status `ERROR`.

**Turn span:** `ferrule.task_shape`, `ferrule.origin`, and summed
`gen_ai.usage.*` and `ferrule.cost_usd` over its calls.

**Session span:**
- It is emitted when the session closes: a sub-agent session when its run
  finishes, and every other session at shutdown flush or when evicted (at
  most 256 open sessions).
- Its turns are exported as they finish, so a backend shows live turns under
  a parent that arrives later. OTel allows that, and every backend renders
  it once the parent lands. Turns also carry `gen_ai.conversation.id`, so
  they're searchable meanwhile.

**Sub-agents:** a child session's parent span is the parent's *open turn*.
The parent is found from the row's `origin = "agent:<parent>"`: the root's
session id, or `agent-<id>` for a nested child. The child shares the tree's
trace id.

**Out of the trace:** rows the loop doesn't make (`learn`, `web_search`,
`embed`, `egress_denied`, `eval_result`) don't pass the agent sink, so they
aren't exported. Egress denials show up on the owner's side (ledger, audit,
dashboard). Follow-up: span events.

### 2.4 Content: off by default, scrubbed when on

```toml
[telemetry]
endpoint = "http://127.0.0.1:4318"    # OTLP/HTTP base; /v1/traces is appended
headers = { "x-honeycomb-team" = "${HONEYCOMB_KEY}" }
content = false                       # prompts, replies, tool arguments/results
service_name = "ferrule"
```

- **Off by default:** no `[telemetry]` table means nothing is exported and
  there's no thread and no queue.
- **Headers:** `${NAME}` in a header value expands the way MCP HTTP headers
  do, through the sandbox's view of the env. When `NAME` is in `[secrets]`,
  that's the **placeholder**. The exporter posts through the credential
  proxy, which swaps in the real key for the hosts it's bound to. A raw key
  written into `headers` is refused at config load: a value that
  `looks_secret` by shape (long and high-entropy, or a known prefix) and
  isn't a `${…}` reference. The error points at `[secrets]`.
- **`content = true`** adds, all truncated to 4 KiB each:
  - `gen_ai.input.messages` on the turn (the goal);
  - `gen_ai.output.messages` on model calls (the reply text);
  - `gen_ai.tool.call.arguments` and `gen_ai.tool.call.result` on tools.

  Each goes through the **scrubber** first:
  - `Broker::scrub_text` (the proxy's `Swaps::scrub`, real value →
    placeholder, for every bound secret);
  - the gateway's `Redactor` (channel tokens, provider keys by value, token
    shapes).

  A secret that reached a tool result leaves as its placeholder.

### 2.5 Batching, backpressure, shutdown

- **Handing off:** `OtelSink::record`/`trace` only clone the row or event
  and `try_send` it into a bounded `std::sync::mpsc::sync_channel(2048)`.
  Spans are built on the export thread (`trace::Tracer`), so the turn/session
  bookkeeping needs no lock on the agent's side. **When the queue is full,
  the message is dropped** and `dropped` is incremented. The agent loop
  never waits on the exporter.
- **The export thread** has its own current-thread tokio runtime, so it
  never shares the agent's runtime. It batches up to 512 spans or 2 s,
  whichever comes first, and POSTs `application/json` with a 10 s timeout.
  - A failed POST (connection refused, 5xx, timeout) drops that batch and
    adds its spans to `failed`. So does a full batch that arrives while the
    thread is backing off.
  - There is no retry queue: a dead collector must cost bounded memory.
  - The thread backs off, doubling up to 30 s, while the collector stays
    down, so a dead endpoint isn't hammered.
- **Counters:** `exported`, `dropped`, `failed` (spans), `last_error`,
  `last_ok`. Error text never carries the URL. They
  appear:
  - in `/status` (a health section, gateway);
  - in `ferrule doctor`, from `<data>/telemetry/status.json`, which the
    export thread rewrites at most every 10 s and at shutdown;
  - in the debug log.
- **Shutdown:**
  - `Exporter::shutdown(deadline)` closes the open turns and sessions
    (status `UNSET`, `ferrule.closed = "shutdown"`; `"evicted"` and
    `"superseded"` mark the other early closes), flushes the queue
    and waits for the thread up to the deadline (3 s).
  - It's called before `finish_run`'s `process::exit`, after chat's loop,
    and after the gateway's select.
  - A collector that doesn't answer within the deadline loses the batch,
    and that's counted.

### 2.6 Egress

- The exporter uses `client_builder(Some(tool egress))`, so it goes through
  the proxy and the header placeholders get swapped.
- The configured endpoint's host:port is an implicit `private_allow` (§1.5),
  so a collector on `127.0.0.1:4318` works.
- Under `default = "deny"` the endpoint is implicitly allowed too.

### 2.7 Out of scope

- Metrics and logs signals.
- The `tracing` bridge.
- gRPC and protobuf.
- Sampling: traces are low-volume, one per conversation turn.
- Exporting historical ledger rows. Follow-up: `ferrule telemetry replay`.

---

## 3. Migration importers (item 16)

### 3.1 The formats, as researched

Read from the source on 2026-09-26 rather than guessed. Paths are relative
to the repo root at the commit below.

| Tool | Repo | Release | Commit |
|---|---|---|---|
| OpenClaw | `openclaw/openclaw` | `v2026.9.6` (2026-09-23) | `eb377ac59e6c9fd6c7705028034812becf00271b` |
| Hermes Agent | `NousResearch/hermes-agent` | `v2026.9.24` (2026-09-24) | `f97608f178d1ffeca59860195ab7da295f7c8e5f` |

**OpenClaw**
- **State directory:**
  - `~/.openclaw` by default; `~/.clawdbot` is the legacy fallback when the
    new one is missing (`src/config/state-dir.ts`, `src/config/paths.ts`);
  - `$OPENCLAW_STATE_DIR` overrides it;
  - `$OPENCLAW_PROFILE=x` means `~/.openclaw-x`;
  - `$OPENCLAW_HOME` replaces `~`;
  - there's no Windows-specific path: `%USERPROFILE%\.openclaw`.
- **Config:**
  - `<state>/openclaw.json` (legacy `clawdbot.json`), or
    `$OPENCLAW_CONFIG_PATH`;
  - **JSON5** (comments, trailing commas), with `$include`.
- **Models:**
  - `agents.defaults.model` is `"provider/model"` or `{primary, fallbacks}`;
  - providers are `models.providers.<name>.{baseUrl, api, apiKey}`, with
    `api` one of `openai-completions`, `anthropic-messages`,
    `google-generative-ai`, …
- **Secret fields** (`SecretInput`, `src/config/zod-schema.secret-input.ts`)
  take one of three forms:
  - a literal string;
  - `"${ENV}"`;
  - `{source: env|store|file|exec, provider, id}`.
- **Channels** (`channels.<telegram|discord|slack>`):
  - `dmPolicy`: `pairing|allowlist|open|disabled`;
  - `allowFrom` and `groupAllowFrom` (Telegram only): `Array<string|number>`,
    where `"*"` means anyone;
  - tokens: `botToken`/`token`/`appToken`;
  - per-account overrides in `accounts.<id>`;
  - pairing approvals are in SQLite (`state/openclaw.sqlite`); the legacy
    form is `credentials/<channel>-<account>-allowFrom.json` =
    `{"allowFrom":[…]}`.
- **Workspace:** `<state>/workspace` (or `$OPENCLAW_WORKSPACE_DIR`,
  `agents.defaults.workspace`, per-agent `agents.entries.<id>.workspace`).
  It contains:
  - `MEMORY.md`: free-form markdown, **no entry delimiter**;
  - `USER.md`;
  - daily notes `memory/YYYY-MM-DD[-slug].md`;
  - `SOUL.md`, `AGENTS.md`, `IDENTITY.md`.
- **Skills:** `<workspace>/skills`, `<workspace>/.agents/skills`,
  `<state>/skills`, `~/.agents/skills`. Each is
  `<name>/SKILL.md` with AgentSkills frontmatter; `metadata.openclaw` is
  JSON5.
- **Secrets on disk:**
  - `<state>/.env`;
  - `credentials/`;
  - `agents/*/agent/auth-profiles.json` and friends;
  - the SQLite stores;
  - `skills.entries.*.apiKey`/`env`.

**Hermes Agent**
- **Home:**
  - `$HERMES_HOME`, otherwise `~/.hermes` on Linux and macOS, and
    `%LOCALAPPDATA%\hermes` on Windows (`hermes_constants.py`);
  - profiles live in `<root>/profiles/<name>/`; the active one is named in
    `<root>/active_profile`, where `default` means the root.
- **Config:** `<home>/config.yaml` (`_config_version: 46`):
  - `model.{default, provider, base_url, api_key?}`;
  - `providers.<name>.{base_url, api_key: "${VAR}", key_env, key_cmd, api_mode}`.
- **Secrets:** `<home>/.env` holds the provider keys (`ANTHROPIC_API_KEY`,
  `OPENROUTER_API_KEY`, …) and the channel tokens (`TELEGRAM_BOT_TOKEN`,
  `DISCORD_BOT_TOKEN`, `SLACK_BOT_TOKEN`, `SLACK_APP_TOKEN`). Also
  `auth.json`, `mcp-tokens/` and `pairing/`.
- **Allowlists** (`gateway/pairing.py`): env vars in `.env`, comma-separated,
  with `*` meaning all:
  - `TELEGRAM_ALLOWED_USERS`, `TELEGRAM_GROUP_ALLOWED_CHATS` (negative chat
    ids);
  - `DISCORD_ALLOWED_USERS`, `DISCORD_ALLOWED_CHANNELS`;
  - `SLACK_ALLOWED_USERS`, `SLACK_ALLOWED_CHANNELS`.

  The YAML equivalent is under `gateway.platforms.<p>[.extra].{allow_from, group_allowed_chats, allowed_channels}`.
- **Memory:** `memories/MEMORY.md` and `memories/USER.md` are entries joined
  by exactly `"\n§\n"` (`tools/memory_tool_store.py`). There are no daily
  notes.
- **Skills:** `skills/<category>/<name>/SKILL.md`, with `name` matching
  `^[a-z0-9][a-z0-9._-]*$` (≤ 64 characters).

**Not settled by reading the source:**
- OpenClaw's SQLite schemas (auth profiles, pairing) were not read, so the
  importer reads the config and the legacy JSON files and says what it
  skipped.
- The OpenClaw Windows path is inferred, not run.

### 3.2 Shape

```
ferrule import openclaw [--from <state dir>] [--workspace <dir>] [--apply] [--bind-secrets]
ferrule import hermes   [--from <home>] [--profile <name>] [--apply] [--bind-secrets]
```

- **Dry run by default:** the command prints what it *would* do, per area,
  and writes nothing. `--apply` writes.
- **Nothing of the other tool is executed or required.** The importer reads
  files only. It needs neither the tool's install, its node or python
  runtime, nor its CLI.
- **Its own readers, no new dependencies:**
  - a **JSON5 → JSON** normaliser (comments, trailing commas, unquoted keys,
    single-quoted strings), about 80 lines;
  - a **YAML subset reader** (block maps and lists, flow lists and maps,
    plain/quoted scalars, comments), about 150 lines. It's tolerant:
    anything it can't read (anchors, block scalars) becomes a skipped key,
    and the summary says so.

  Adding `json5` plus `serde_yaml` (deprecated upstream; its successors are
  young) for a one-shot migration of a handful of keys isn't worth two new
  third-party trees in a public release binary.
- **`$include` isn't followed.** It's reported as skipped, with its path.

### 3.3 What maps to what

**Memories** go into ferrule's memory store (`<data>/memory.db`):

| Source | Entries |
|---|---|
| OpenClaw `MEMORY.md` | Split at top-level bullets, otherwise at blank-line paragraphs. A heading line is kept as the prefix of the entries under it (`"Projects: …"`). |
| OpenClaw `USER.md` | Same split, extra tag `user` |
| OpenClaw `memory/YYYY-MM-DD*.md` | Same split, each prefixed `"(YYYY-MM-DD) "`, extra tag `daily` |
| Hermes `memories/MEMORY.md` / `USER.md` | Split on `"\n§\n"`; `USER.md` entries tagged `user` |

- **Tags:** every imported memory is tagged `import:<tool>` and
  `from:<file>` (the origin; the store has no origin column, and tags are
  what `memory` search and `ferrule memory` already show).
- **Dedup:**
  - Entries are deduplicated within the import first, with the store's
    own sameness test (`ferrule_memory::same_fact`: equal once normalised,
    or near-identical words).
  - An entry whose text is already a live fact (`MemoryStore::known`) is
    kept as it is. That is what makes a second run a no-op.
- **Re-runs supersede rather than duplicate:**
  - The comparison is made per source file, before anything is written.
    An entry that isn't known is matched against the live facts tagged
    with the same `import:<tool>` and `from:<file>` that no other entry has
    claimed. The first one that *resembles* it (`ferrule_memory::resembles`,
    a word overlap of 30 % or more) is **superseded** by the new text
    (`replaces = [old id]`). History is kept, and only one is live.
  - An entry that's gone from the source is left alone. Ferrule doesn't
    delete memories because the other tool did.
- **Secrets in memory:** an entry is **not imported** when:
  - one of its words has a key's shape (the config check's prefixes:
    `sk-…`, `xox…`, `ghp_…`, `AKIA…`, and the rest);
  - the gateway's redactor would change it;
  - or it contains the value of any secret the import found (six
    characters or more).

  The summary lists it by file and line, never by content. The shape check
  errs on the side of holding back, so a long hash or id can be held back
  too; the owner adds such an entry by hand.

`SOUL.md`, `AGENTS.md` and `IDENTITY.md` aren't memories. `AGENTS.md` is
already a context-baseline file for ferrule, so the summary suggests copying
it into the project. Nothing is written.

**Skills** are every `SKILL.md` directory in the search roots above. Each
goes through the **M13 path**:
- `skill::inspect` + `Review`, which shows the scan findings, bundled
  scripts and requested tools;
- then owner confirmation;
- then `commit_skill(…, Origin::Owner, …)`.

The new `ExtensionManager::install_local_skill(dir, source, confirm)` is
`approve()` for a local directory. It refuses a name that's already
installed, and a "no" installs nothing. The source label is recorded as
`import:<tool>:<path>`. What's installed is a staged copy whose
frontmatter `name:` carries the mapped name; the source directory is never
changed.

In a dry run, the summary lists skills with their scan verdicts. With
`--apply`, each skill is **confirmed one at a time at the terminal**. With
no terminal, or no ferrule config yet, skills are skipped and the summary
says why. No flag auto-approves an imported skill.

**Name handling:**
- Hermes and OpenClaw names are mapped onto ferrule's name rule (lowercase
  `a-z0-9`, `-`/`_`, ≤ 40 characters): any other character becomes `-`
  (`Weather Tool` → `weather-tool`). A name that's too long, or that two
  skills in one import share, is cut and given `-` plus six hex digits of
  a hash of its path.
- An already installed skill with the same name is skipped with a note.
  Replacing one needs `ferrule extensions` afterwards.

**Channel allowlists** are ids only, merged into `[gateway]` as a
union with what's there. Re-running changes nothing.

| Source | Ferrule |
|---|---|
| OpenClaw `channels.telegram.allowFrom` (numeric user ids; a user's DM chat id equals the user id) + legacy `credentials/telegram-*-allowFrom.json` | `telegram_allowed_chats` |
| Hermes `TELEGRAM_ALLOWED_USERS`, `TELEGRAM_GROUP_ALLOWED_CHATS` (and the YAML forms) | `telegram_allowed_chats` |
| OpenClaw `channels.discord.allowFrom` / Hermes `DISCORD_ALLOWED_USERS` | `discord_allowed_users` |
| OpenClaw `channels.discord.dm.groupChannels` / Hermes `DISCORD_ALLOWED_CHANNELS` | `discord_allowed_channels` |
| OpenClaw `channels.slack.allowFrom` / Hermes `SLACK_ALLOWED_USERS` | `slack_allowed_users` |
| Hermes `SLACK_ALLOWED_CHANNELS` | `slack_allowed_channels` |

**Not mapped, and reported:**
- **`"*"` / `ALLOW_ALL_USERS`.** Ferrule has no "anyone" allowlist, on
  purpose, so the owner decides.
- **Non-numeric Discord names.** Hermes resolves them at runtime; ferrule
  needs ids.
- **OpenClaw Telegram `groupAllowFrom`.** It lists *user* ids allowed in
  groups, while ferrule's list is *chat* ids.
- **OpenClaw's SQLite pairing approvals.**

**The token env name** is set only when ferrule has none for that channel
(`telegram_token_env = "TELEGRAM_BOT_TOKEN"`, …). See secrets below.

**Providers:**
- **The model:** OpenClaw's `agents.defaults.model` (`provider/model`, or
  `primary`) or Hermes's `model.{provider, default, base_url}` is the model.
- **The table:** each source provider maps to a ferrule `[providers.<name>]`
  when it maps cleanly:
  - a known preset (`anthropic`, `openai`, `openrouter`, `gemini`, `ollama`,
    …) gets the preset's `base_url`, `api` and key env name;
  - a custom OpenAI-compatible or Anthropic endpoint (OpenClaw `api` of
    `openai-completions`/`anthropic-messages`; Hermes `api_mode` of
    `chat_completions`/`anthropic_messages`) gets its `base_url` and the
    right `api`.
- **Doesn't map, reported:** OAuth-only providers (Hermes `nous` and
  `openai-codex` logins, OpenClaw `oauth`/`token` auth profiles), `key_cmd`,
  and Codex Responses mode.
- **What gets written:**
  - an existing `[providers.<name>]` is never overwritten;
  - the top-level `default_provider` is set to the source's default only
    when ferrule has neither `default_provider` nor `[models] default`.

**Secrets are never written to `config.toml`.** A key is found:
- as a literal in `openclaw.json`;
- in `<state>/.env` or Hermes `<home>/.env`;
- as `api_key: "<literal>"` in `config.yaml`.

The importer records only its **env name**: the preset's
(`ANTHROPIC_API_KEY`), or the variable it came from.
- **Default:** the summary lists the names, never the values, and says
  where each was found. Ferrule has no `secrets set` command; the owner
  exports the variable, saves it with `ferrule setup`, or re-runs with
  `--bind-secrets`.
- **With `--bind-secrets` (plus `--apply`),** the owner consents to copy
  the values into ferrule's secret store, `<data>/private/secrets.env`
  (0600, the store `ferrule setup` writes). Existing names aren't
  overwritten. `--bind-secrets` without `--apply` is an error.
- **References are kept as references:** `${VAR}` and
  `{source:"env", id}` refs map to that env name. The value is known only
  when the tool's own `.env` defines it; then `--bind-secrets` can copy
  it, and otherwise the summary says the value isn't there. The
  importer never reads the process environment for a value.
  `store`/`file`/`exec` refs are reported as not portable.

Channel tokens follow the same rule, under ferrule's default names.

### 3.4 Setup

- **`ferrule setup`'s guided flow** starts with an **Import** step when a
  source home is found. It checks, in order:
  - OpenClaw: `$OPENCLAW_STATE_DIR`, `~/.openclaw[-$OPENCLAW_PROFILE]`,
    `~/.clawdbot`;
  - Hermes: `$HERMES_HOME`, `~/.hermes`, `%LOCALAPPDATA%\hermes`.

  The step shows the dry-run summary and asks whether to apply it.
  - **Yes** runs the same code as `ferrule import … --apply`, with skills
    confirmed one by one and secrets bound only on a second explicit yes.
  - **No** skips it; `ferrule import` or the menu does it later.
- **The menu** gets an "Import" item, which shows what was found.

### 3.5 Failure modes

- A malformed file is reported with the file and parse error, and the rest
  of the import goes on.
- Apply is per area:
  - config edits go through one `edit_config` under the config lock,
    checked and written atomically;
  - memories and skills commit individually.

  A crash midway leaves a state the next run completes, because every step
  is idempotent.
- Huge inputs are capped: 2,000 memory entries and 200 skills per run,
  reported if hit.

### 3.6 Out of scope

- Sessions and transcripts (`state.db`, OpenClaw SQLite).
- Cron/scheduled jobs.
- MCP server definitions. Their shapes differ and they carry secrets inline;
  that's a follow-up.
- `SOUL.md` persona.
- The other direction (export).

---

## 4. Parts and tests

1. **Egress** (`ferrule-proxy::egress`, CLI wiring, doctor/setup/dashboard,
   sandbox unix sockets). Tests:
   - policy unit tests (wildcards, IP/CIDR, deny-wins, private ranges,
     mapped addresses);
   - proxy integration: a name resolving to 127.0.0.1, the metadata IP,
     a deny page on CONNECT and plain HTTP, the loopback exception for the
     command source and configured endpoints;
   - `web_fetch` and MCP-over-HTTP through the policy;
   - a real sandboxed `curl` via the proxy env (skipped when curl is
     absent);
   - the ledger and audit rows;
   - Linux: a real `UnixListener` that's allowed and one that's denied, a
     symlink, an abstract socket;
   - macOS: the same through Seatbelt.
2. **OTel** (`ferrule-otel`, core trace hooks, equip, flush). Tests use a
   mock OTLP/HTTP collector on 127.0.0.1:
   - the span tree and attributes;
   - no content by default, and scrubbed content on opt-in;
   - a full queue drops and counts;
   - a dead collector (a port that refuses, a server that never answers)
     never stalls a turn;
   - the shutdown flush meets its deadline.
3. **Importers** (`ferrule import`, `install_local_skill`, setup detection).
   Tests use fixture homes for both tools:
   - dry run writes nothing;
   - apply writes;
   - a second apply changes nothing;
   - a changed entry supersedes the old one;
   - skills are reviewed and refused without confirmation;
   - allowlists are merged;
   - a literal key never appears in `config.toml`.

Real-network checks (a real collector, a real OpenClaw/Hermes install) are
`#[ignore]`d. `docs/otel.md` and `docs/migrate.md` say how to run them.
