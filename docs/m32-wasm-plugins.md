# M32 — WASM tool plugins (design)

Status: design, 2026-09-26, branch `m32-wasm-plugins`. Written before the
code; where the build departs from it, see **As built** at the end.
User guide: [plugins.md](plugins.md).

An MCP server is a whole program: its own runtime (node, python, a binary),
its own dependencies, and on every OS a different sandbox story (M26). That
is the right shape for a connector to a big service, and it is far too much
for a tool that converts units, queries a JSON document or calls one REST
endpoint. M32 adds a lighter kind of extension: a **plugin** is one `.wasm`
file plus a manifest. It runs inside ferrule's own process, in an
interpreter, with **no ambient authority at all**. It can reach a file, the
network, a secret, the clock or randomness only through host functions that
ferrule implements, and only the ones its manifest declares and the owner
approved. The strategy doc's line is "curated + sandboxed + credential-bound
by default"; a plugin is all three by construction, not by OS policy.

What stays the same: plugins install through M13's flow (allow-list, exact
pin, scan, approval queue, lock file), reach the model through M17's hot-add
path, and every existing guard (M18 hooks, M19 approvals, caps and kill
switch, M20 plan mode, sub-agent reach, M27 parallel rules) applies to a
plugin tool exactly as to any other tool.

## 1. Runtime: wasmi, measured

Three candidates, each added to the real release profile (`opt-level=3`,
fat LTO, one codegen unit, strip, `panic=abort`) of a small host binary for
`x86_64-unknown-linux-musl`. The bench loads a WAT module (compiled at build
time), instantiates it 1000 times, calls a "copy 32 bytes through linear
memory" export 10 000 times, and runs a 50M-iteration integer loop (an LCG)
once. One machine, one run each; the numbers are for ranking, not a
benchmark suite.

| | release binary | growth | build (clean) | compile module | instantiate | call (JSON-ish, 32 B) | 50M-iteration loop |
|---|---|---|---|---|---|---|---|
| base (no runtime) | 582 336 B | — | 40 s | — | — | — | — |
| **wasmi 2.0** (`std`, no default features) | 1 802 944 B | **+1.2 MB** | 65 s | 0.08–0.17 ms | 76–90 µs | 1.1–1.4 µs | 286 ms |
| wasmtime 49 (`cranelift`, `runtime`, `std`) | 9 949 992 B | +9.4 MB | 312 s | 6.2–6.6 ms | 9–11 µs | 8–11 µs¹ | 78 ms |
| extism 1.30 (default features²) | 16 803 368 B | +16.2 MB | 458 s | 43 ms (`Plugin::new`) | (in the above) | 11 µs (noop) | — |

¹ wasmtime's call time includes first-touch page faults on a fresh linear
memory in this bench; a warm call is well under a microsecond. It doesn't
change the ranking: per-call overhead of every candidate is noise next to a
model round trip (hundreds of milliseconds).
² extism with `default-features = false` does not compile (41 errors,
`wasmtime::Error: std::error::Error` not satisfied); with defaults it pulls
wasmtime 43 with its cache, profiling and `ittapi` features.

The bench was built with `CC=gcc` for the musl target because `musl-gcc`
isn't installed on the measuring machine; wasmtime and extism need a C
compiler through the `cc` crate, wasmi doesn't.

**Decision: wasmi.**

- **Size.** +1.2 MB against +9.4 MB (wasmtime) and +16.2 MB (extism). The
  release binary today is a few MB; wasmtime would roughly double or triple
  it for a feature most users won't touch in week one.
- **Builds everywhere with no new toolchain.** wasmi is pure Rust with no
  build script that needs C. The five release targets (x86_64 and aarch64
  linux-musl, x86_64 and aarch64 apple-darwin, x86_64-pc-windows-msvc) build
  it with what they already have. Cranelift also supports all five, but
  wasmtime pulls `cc`-built C code, and aarch64-musl cross builds are where
  that bites. The `release.yml` dispatch run verifies all five (§12).
- **Fuel.** wasmi has deterministic fuel metering with resumable calls
  (`call_resumable` returns when fuel runs out, and the host decides whether
  to top up). That gives a CPU budget that doesn't depend on the machine
  *and* a point to check a wall-clock deadline and cancellation, with no
  signal handlers or epoch threads.
- **Startup.** Compiling a module is a sub-millisecond lazy translation,
  against 6 ms of Cranelift per module (and 43 ms for extism). A plugin is
  loaded per session and instantiated per call, so startup matters more than
  throughput.
- **The price, stated plainly:** pure compute is about **3.7× slower** than
  under wasmtime's JIT (286 ms vs 78 ms for 50M loop iterations). A plugin
  that crunches for seconds would be better on wasmtime. Plugins are tools
  a model calls: parse, convert, query, call an API. For that shape the
  interpreter is fast enough and the size and portability win. If a heavy
  plugin shows up, a `plugins-jit` feature on wasmtime is a contained
  change: the ABI (§2) is runtime-neutral.

**Feature gate.** The runtime sits behind a cargo feature `plugins` on
`ferrule-cli`, **on by default, and release builds ship with it on.** At
+1.2 MB the gate isn't needed for size; it exists so a downstream build can
drop the interpreter entirely (`--no-default-features`). With it off,
`ferrule plugins …` says the build has no plugin support and installed
plugins are listed as unavailable.

## 2. ABI: a JSON core-module ABI, not WIT

**Decision: plain core WebAssembly with a four-export, one-import JSON
ABI.** The component model and WIT give typed interfaces and good bindings
generators, but they need a component-model runtime (wasmtime; wasmi has
none), a `wit-bindgen` toolchain on the plugin author's side, and they push
every plugin through WASI preview 2 adapters. Tool calls are JSON anyway:
the model sends JSON arguments and reads text. A JSON boundary costs one
serialise/parse per call (microseconds) and makes the ABI implementable
from any language that compiles to `wasm32` with no bindings generator.

A plugin module **exports**:

| export | signature | meaning |
|---|---|---|
| `memory` | memory | linear memory the host reads and writes |
| `ferrule_abi_version` | `() -> i32` | must return `1` |
| `ferrule_alloc` | `(len: i32) -> i32` | a buffer of `len` bytes for the host to write into |
| `ferrule_call` | `(tool_ptr, tool_len, args_ptr, args_len: i32) -> i64` | run one tool; returns `ptr << 32 \| len` of a UTF-8 JSON reply |

The reply is `{"output": "<text>"}` or `{"error": "<text>"}`. `output` may
also be any JSON value, which the host renders as pretty JSON.

It **imports** at most one function:

| import | signature | meaning |
|---|---|---|
| `ferrule.host_call` | `(ptr, len: i32) -> i64` | one JSON request, one JSON reply (`ptr << 32 \| len`, written through `ferrule_alloc`) |

A request is `{"op": "http" | "read_file" | "write_file" | "list_dir" | "now" | "random", …}`,
the reply `{"ok": …}` or `{"error": "…"}`. Every op is checked against the
granted capabilities on every call (§4).

**Any other import is refused at load**, WASI included: a module that
imports `wasi_snapshot_preview1.fd_write` (or anything else) does not load,
and the error names the import. That rule is what makes "no ambient
authority" true: there is no function to call.

A fresh instance serves each call: no state leaks from one call to the next
or between two sessions, and a call that corrupted its memory can't affect
the next. Instantiation is 76–90 µs (§1).

### Languages

| language | status |
|---|---|
| **Rust** (`wasm32-unknown-unknown`) with `ferrule-plugin-sdk` | **supported and tested**: both examples |
| TinyGo (`-target=wasm-unknown`) | should work (it can export functions and import `ferrule.host_call` with `//go:wasmimport`); **untested** |
| AssemblyScript | should work (exports, `@external("ferrule", "host_call")`, no WASI by default); **untested** |
| Zig, C (`wasm32-freestanding`, clang) | should work; **untested** |
| javy (JS), componentize-py (Python) | **not supported**: both produce modules that need WASI (javy) or the component model (componentize-py) |
| Rust/Go/C targeting `wasm32-wasip1` | refused at load (WASI imports) |

Only Rust ships with an SDK and examples. The ABI is small enough that
another language needs about 40 lines of glue; `plugins.md` spells it out.

## 3. The manifest

`plugin.json`, next to the `.wasm`:

```json
{
  "name": "github-repo",
  "version": "0.1.0",
  "description": "Look up a GitHub repository.",
  "wasm": "github_repo.wasm",
  "sha256": "9f2c…64 hex",
  "tools": [{
    "name": "repo",
    "description": "Stars, open issues and default branch of a GitHub repository.",
    "parameters": {"type": "object", "properties": {"owner": {"type": "string"}, "repo": {"type": "string"}},
                   "required": ["owner", "repo"], "additionalProperties": false},
    "read_only": true,
    "approval": false
  }],
  "capabilities": {
    "http": {"domains": ["api.github.com"], "methods": ["GET"]},
    "secrets": ["GITHUB_TOKEN"],
    "files": {"read": [], "write": []},
    "clock": false,
    "random": false
  },
  "limits": {"fuel": 2000000000, "memory_mb": 32, "timeout_secs": 20, "output_chars": 20000}
}
```

- `name`: 1–40 chars of `a-z0-9-_`, no `__` (M13's rule). Each tool is
  offered to the model as **`plugin__<plugin>__<tool>`**.
- `sha256`: the hex SHA-256 of the `.wasm` file. Checked at install, at
  every load, and in `sync`: a mismatch refuses the install or suspends the
  installed plugin, and the message shows both hashes.
- `parameters`: JSON Schema, **a subset** ferrule validates itself (§6).
  A keyword outside the subset is refused at install, so nothing in a
  schema is silently unchecked.
- `read_only`: the plugin's claim that the tool only reads. Honoured only
  when the granted capabilities agree (§7).
- `approval`: the tool must be approved by the owner on every call, through
  M19's gate (§7).
- `capabilities`: deny by default; everything missing is denied (§4).
- `limits`: optional, each capped by the host maximum (fuel 10 000 000 000,
  memory 256 MiB, 120 s, 100 000 chars). Defaults: fuel 1 000 000 000
  (a couple of seconds of interpreter work), 64 MiB, 30 s, the session's
  normal tool output cap.

## 4. Capabilities, deny by default

A capability is a promise the owner reads at approval time and ferrule
enforces on every host call. There is nothing else to enforce: the module
has no other way out (§2).

### Files

- `files.read` / `files.write`: directories **relative to the workspace**
  (`"."` is the whole workspace). Absolute paths, `..` and `~` are refused
  at install.
- A path the plugin passes is resolved exactly as the built-in file tools
  resolve it (`fs_tools::resolve`: lexical normalisation, then the real
  path with symlinks followed, then "inside the workspace"), then checked
  to be inside one of the granted directories.
- **M26's read policy applies unchanged:** the sandbox's `read_deny_list`
  (ferrule's private data, default credential dirs, `deny_read`) is passed
  as the hidden list, so a denied path is refused even inside a granted
  directory.
- Writes go through the file tools' atomic write. Size caps: 4 MiB read,
  4 MiB write, 1000 directory entries.
- No host environment at all: the plugin can't read an env variable. There
  is no op for it.

### Clock and randomness

`now` (Unix milliseconds) and `random` (up to 1024 bytes from the OS) exist
only when `clock: true` / `random: true`. They are the only capabilities an
allow-listed plugin may have and still install without the owner (§5):
neither reaches anything outside the process.

### Network: only the host `http` op, through the credential proxy

- `http.domains`: exact host names, or `*.example.com` for subdomains
  (not the apex). `http.methods`: default `["GET"]`.
- The request must be `https://`, to a granted domain, with a granted
  method. Plain `http://` is refused: nothing a plugin should talk to needs
  plaintext, and a plaintext request can carry a swapped secret in the
  clear. An IP address or `localhost` is refused unless literally granted.
- The request goes out through **the same client `web_fetch` and
  `web_search` use**: `ferrule_tools::egress::client_builder(sandbox.egress())`.
  When the credential proxy (M20/M26 broker) is running, that means through
  it: its CA, its host bindings, its scrubbing. No proxy internals change.
- **Secrets.** A header value may contain `${NAME}` for a `NAME` in
  `capabilities.secrets`. The host expands it to what a sandboxed child
  would see, `Sandbox::child_env_var(NAME)`, the same expansion MCP HTTP
  headers get. When the secret is bound in the proxy that is the
  **placeholder**; the proxy swaps it for the real value on the bound
  hosts only. The host refuses the expansion when there is no proxy, or
  when the value it would send is the real value from ferrule's own
  environment: **a plugin's request never carries a real key that ferrule
  holds**, and the plugin itself never sees even the placeholder (the
  expansion is on the host side, after the plugin's request is parsed).
  `${…}` anywhere else (URL, body) is refused, so a plugin can't make the
  host echo a placeholder back to it.
- Response bodies are capped at 2 MiB (web_search's `MAX_BODY`); the proxy
  scrubs real secret values out of responses as it does for every child.
- **Untrusted marking.** A plugin that has the `http` capability can
  return text an attacker wrote. Its output reaches the model inside
  `<plugin_output plugin="…" tool="…" untrusted="true">…</plugin_output>`,
  escaped like web search results, so the prompt's rule for untrusted
  content applies.
- **Ledger.** Plugin calls cost no tokens and appear in the per-model-call
  ledger rows exactly like `web_fetch` does (as a tool call in the turn);
  `web_fetch` has no ledger row of its own and neither does a plugin's
  `http` op. Each op is traced (`tracing` target `ferrule_plugins`) with
  plugin, method, host and status, never headers or bodies.

### Limits

- **Fuel.** Each call gets the manifest's fuel (capped). The host runs the
  call in slices of 10M fuel with `call_resumable`, so between slices it
  can check the wall-clock deadline and the cancel flag. Out of fuel →
  "the plugin used its whole CPU budget".
- **Wall clock.** `timeout_secs` (default 30) covers the whole call,
  host `http` ops included (each op gets the remaining time as its HTTP
  timeout).
- **Memory.** `StoreLimits` caps linear memory (default 64 MiB) and table
  growth; a `memory.grow` past it fails, which in Rust aborts → a trap.
  Modules declaring an initial memory above the cap don't instantiate.
- **Output.** The reply is capped at 4 MiB before it's parsed; the text is
  cut to the output cap with `ToolOutput::capped`.
- **Isolation of failures.** The call runs on a blocking thread
  (`spawn_blocking`). A trap (unreachable, out-of-bounds, stack overflow,
  divide by zero), a timeout, an OOM, a malformed reply or a panic inside
  the host code becomes a `ToolFailed` error with a one-line reason. None of
  them crashes the agent or the daemon; the next call gets a fresh instance.

## 5. Packaging and install (M13's flow)

```
ferrule plugins add <path | url:https://…/plugin.json | git:https://host/repo@<sha>[#subdir]> [--sha256 HEX] [--yes]
ferrule plugins list
ferrule plugins remove <name> [--purge]
ferrule extensions approve <id> / deny <id> / resume <name>     (shared with MCP and skills)
```

The model's side: `plugin_add {source, sha256?, path?}` and `plugin_remove
{name}` join M13's six extension tools; `extensions_list` lists plugins.

**Sources and pins.** Every source is pinned exactly; nothing floats.

| source | pin | who |
|---|---|---|
| `git:https://…@<40-hex sha>` (+ `path`) | the commit (M13's `fetch_pinned` + `verify`) and the wasm hash | agent or owner |
| `url:https://…/plugin.json` | **`sha256` is required**, must equal the manifest's `sha256` and the downloaded `.wasm`'s hash; the `.wasm` URL is resolved relative to the manifest URL | agent or owner |
| a local directory | the wasm hash | owner CLI; an agent proposing a workspace path always goes to the queue |

**The decision** (in order):

1. Parse and validate the manifest (name, tools, schema subset, capability
   syntax, limits), verify the hash, **load the module** (imports and
   exports checked, `ferrule_abi_version` returns 1). Any failure refuses
   with the reason; nothing is written.
2. Scan: M13's `scan_tool` on every tool's name, description and schema,
   and on the plugin description. A **block** finding refuses the
   agent's request and files it in the pending queue for the owner.
3. Not on the allow-list → the pending queue.
4. Any `files`, `http` or `secrets` capability → **the pending queue,
   always**, even when allow-listed: the owner approves the capabilities,
   not just the source. The approval screen (`ferrule extensions approve`)
   shows every capability in plain words ("read files under `docs/`",
   "HTTPS GET to api.github.com", "secret GITHUB_TOKEN in request headers")
   and every tool with its flags.
5. Otherwise (allow-listed, clean scan, at most `clock`/`random`) → installed.

The owner's CLI (`ferrule plugins add`) does the same checks and asks at the
terminal instead of queueing; `--yes` accepts what the owner would be asked,
except a scan block, which needs the interactive `waive` (as for skills).

**Storage.** `<data>/extensions/plugins/<name>/{plugin.json, <wasm>}`, and
a `plugins` table in the lock file:

```
source, pin, wasm_sha256, manifest_sha256, capabilities (as granted),
tools {name → digest of name+description+schema+flags}, origin, status,
reason, installed_at, waivers
```

The table is `#[serde(default)]`, so the lock version stays 1 and older
lock files load. (A pre-M32 binary rewriting the lock file drops the table;
the plugins then show as orphaned directories until re-added. Documented.)

**Re-approval on widening.** An update (`--replace`, or a new pin) whose
capabilities are **not a subset** of the granted ones goes to the queue
with the difference shown, whatever the origin; the old version stays
active until then. Narrowing is fine.

**Sync.** At start and on `sync`, each active plugin's files are re-hashed
and its manifest's capabilities compared with the lock. A different wasm
hash, a changed manifest, or capabilities beyond the granted ones →
**suspended**, with the reason, and `extensions resume` re-checks. So
editing a plugin's files in place can never widen what it may do.

**Hot-add.** A plugin's tools live in the extension manager's live set,
which the registry already asks on every `definitions()`/`call()` (M13/M17
`ToolSource`). An install is visible on the next model call of every
session that has the manager attached, and new sessions start with it.

**Doctor** prints one line per plugin: name, version, tools, capabilities,
status, and a hash check. `/status` and the dashboard list plugins next to
MCP servers if the change stays small.

## 6. Argument validation

Before the plugin runs, the host checks the arguments against the tool's
`parameters`. The validator is in-house (no `jsonschema` crate, which pulls
a regex engine, URL parsing and a network resolver for `$ref`) and supports:

`type` (string, number, integer, boolean, object, array, null, or a list of
them), `properties`, `required`, `additionalProperties` (bool or schema),
`items`, `enum`, `const`, `minimum`, `maximum`, `minLength`, `maxLength`,
`minItems`, `maxItems`, and the annotation keywords `description`, `title`,
`default`, `examples`.

Any other keyword (`$ref`, `pattern`, `oneOf`, `format`, …) is **refused at
install** with its name. A failed validation returns the path and the rule
to the model ("`/unit`: must be one of km, mi") without running the plugin.

## 7. Guards and parallel calls

- **M18 hooks** run on a plugin tool call like any call (they see the full
  `plugin__…` name and the arguments).
- **M19.** A tool with `"approval": true` is gated: `Tool::needs_approval()`
  (new, default false) reaches the guard through `GuardedCall`, and the
  trust guard classifies it as a new kind, "tool that declares it needs
  approval", routed exactly like a force push: owner chat, terminal, or
  refused unattended. Caps and the kill switch apply before any tool.
- **M20 plan mode.** Extension tools (MCP and plugins) aren't attached
  while planning, as today.
- **Sub-agents** get the extension tools by the same reach rules as MCP
  tools (`Reach`): a reading-only child only sees read-only ones.
- **M27 parallel.** `changes_files()` is true when the plugin was granted
  any `files.write` directory, or any `http` method other than GET/HEAD.
  `read_only()` is `manifest read_only && !changes_files()`. A plugin that
  declares `read_only` but was granted a write capability runs serially.
  Calls to one plugin may overlap (each has its own instance), so there is
  no serial group.

## 8. SDK and examples

`crates/ferrule-plugin-sdk`: a small, `publish`-ready crate (license,
description, no path-only deps) with:

- `export!(handler)`: generates `ferrule_abi_version`, `ferrule_alloc`,
  `ferrule_call` around `fn handler(tool: &str, args: serde_json::Value) -> Result<Value, String>`.
- `host::http(Request) -> Result<Response, String>`, `host::read_file`,
  `write_file`, `list_dir`, `now_ms`, `random_bytes`, each a thin JSON
  wrapper over `ferrule.host_call`.
- Built natively too (the workspace's tests), where the host functions
  return "not inside ferrule".

`examples/plugins/`, each its own cargo project (not a workspace member:
they target `wasm32-unknown-unknown` as `cdylib`s):

- `unit-convert`: pure compute (length, mass, temperature, data sizes); no
  capabilities, so an allow-listed copy installs without the owner.
- `github-repo`: one GET to `api.github.com` with
  `Authorization: Bearer ${GITHUB_TOKEN}`; needs owner approval, shows the
  proxy-placeholder path.

**Fixtures (no network in tests or CI).** Two layers:

1. Runtime and capability tests use modules written in **WAT** and compiled
   in-process by the `wat` crate (a dev-dependency, pure Rust). Every
   hostile behaviour (infinite loop, memory bomb, huge reply, trap, WASI
   import, path escape) is a few lines of WAT, readable in the test.
2. The two examples are built from source by `examples/plugins/build.sh`
   (needs `rustup target add wasm32-unknown-unknown`) and the **built
   `.wasm` files are committed** next to their manifests. A normal test
   loads them, checks the manifest hash, and calls them. CI therefore never
   downloads a wasm target. An `#[ignore]`d test (`cargo test -p
   ferrule-plugins -- --ignored examples_rebuild`) rebuilds them and checks
   the build still produces a loadable plugin. The committed binaries can go
   stale against the SDK source; the ignored test and `build.sh` are how to
   notice, and `plugins.md` says so.

## 9. Threat model

| threat | defence |
|---|---|
| A malicious plugin reads `~/.ssh` or ferrule's saved keys | no WASI, no file op outside granted workspace dirs, M26 deny list on top, symlinks resolved before the check |
| It exfiltrates over the network | no sockets; the only egress is `http` to granted domains and methods, over HTTPS, through the proxy; the owner approved those domains |
| It steals an API key | it never gets one: `${NAME}` expands host-side, only for approved secrets, only to the placeholder; the proxy swaps it on bound hosts only |
| It spins or eats memory to take the daemon down | fuel, wall clock, memory cap, output cap, fresh instance per call, blocking thread, every failure is a tool error |
| Prompt injection in its output | `untrusted="true"` fencing for plugins with `http`; the scan on its descriptions at install |
| A swapped `.wasm` after install | hash at install, load and sync; mismatch suspends |
| An update quietly widens its reach | capability subset check; widening re-queues for the owner |
| A tool claims read-only but writes | read-only is derived from the grants, not the claim |
| A malicious plugin exploits the interpreter | wasmi is memory-safe Rust; this is the residual risk, same class as a bug in the JSON parser. Not defended beyond that. |

## 10. Failure modes

- **Hash mismatch** at install: refused, both hashes shown. In sync:
  suspended; `resume` re-checks.
- **Bad module** (WASI import, missing export, wrong ABI version, invalid
  wasm): refused at install with the reason; in sync, suspended.
- **Trap / out of fuel / timeout / OOM / oversized or malformed reply**:
  `ToolFailed("plugin `x`: <reason>")` to the model. Session continues.
- **Denied capability at run time** (path outside grants, undeclared
  domain, undeclared secret): the host op returns `{"error": …}` to the
  plugin, which usually passes it on; the call itself doesn't trap.
- **Proxy not running** and the plugin uses a secret: the op fails with
  "the credential proxy isn't running". Without secrets, http works
  without the proxy (like web_fetch does).
- **A pre-M32 binary** rewrites the lock: the plugins table is dropped.
- **Built without the `plugins` feature**: add refuses; listed plugins
  show "not available in this build".

## 11. Out of scope

- The component model / WIT, WASI (any preview), and so javy and
  componentize-py plugins.
- A JIT (wasmtime) backend; §1 says when it would be worth it.
- Plugin-to-plugin calls, long-lived plugin state, streaming output,
  plugins providing prompts or resources (MCP's other surfaces).
- A plugin registry or signing. Pins and the owner's approval are the
  trust anchor, as for MCP servers and skills.
- Raw sockets, WebSockets, plain HTTP.
- Per-plugin ledger rows (plugin calls cost nothing; the `http` op is
  traced, not billed).

## 12. Tests

Hermetic: WAT fixtures, the committed example builds, mock upstreams on
127.0.0.1, the real credential proxy. Real network only in `#[ignore]`d
tests.

- **Capabilities:** a file outside the grants and outside the workspace is
  denied; an M26-denied path inside a granted dir is denied; a WASI import
  is refused at load (no network or anything else outside the host op);
  an undeclared domain and method are refused; the placeholder is swapped
  by the real proxy on the way to a TLS mock upstream on 127.0.0.1, the
  upstream sees the real value and the plugin sees neither
  (`crates/ferrule-proxy/tests/plugin.rs`).
- **Limits:** an infinite loop (fuel), a host op that sleeps (timeout), a
  memory bomb, a huge reply, a trap → each a tool error, and the next call
  on the same plugin works.
- **Install:** hash mismatch refused; scan block refused and queued;
  capabilities → approval; widening → re-approval while the old version
  stays; hot-add into a registry that's already attached; sync suspends a
  tampered wasm.
- **Schema, parallel, guards:** argument validation; unsupported keyword
  refused; `read_only` follows manifest and grants; an `approval` tool goes
  through the M19 gate.

`release.yml` is dispatched once on the branch to prove the five targets
build with the feature on, and to measure the archive growth per target.

## 13. As built

Built on branch `m32-wasm-plugins` in seven parts (design, runtime,
approval gate, install flow, CLI, SDK, examples, proxy test). The design
held with these differences:

- **CLI.** It is `ferrule plugins add <source> [--path] [--sha256]
  [--replace] [--workspace]`, `list` and `remove <name>`. There is no
  `--yes` or `--purge`: `add` always asks at a terminal (it refuses
  without one), and `remove` deletes the plugin's directory. A bare
  `https://…/plugin.json` is taken as `url:`, and any other `https://` as
  `git:`. Plain `http://` is refused.
- **Allow-listed sources.** They install without the owner only when the
  plugin asks for nothing beyond `clock`/`random`, or for nothing beyond
  what an earlier approval already granted. A local directory proposed by
  the agent must be inside the workspace and always queues. An owner "yes"
  given before the capabilities were known (a pending request approved
  from its summary) is asked again with them.
- **`/status` and the dashboard** don't list plugins. The health report
  lives in `ferrule-gateway`, which doesn't see the extension manager, so
  it wasn't the small change §5 allowed. `ferrule plugins list`,
  `ferrule extensions list` and doctor cover it.
- **SDK.** `ferrule-plugin-sdk` has the full crates.io metadata but
  `publish = false`: the repository has no license yet. To publish, add
  one and drop that line. The random op's wrapper is `random_hex(len)`
  (the op returns hex), not `random_bytes`. There are no path-only
  dependencies (serde and serde_json only).
- **Examples.** `unit-convert` does length, mass and temperature (no data
  sizes). `build.sh` builds with `--remap-path-prefix`, so the committed
  modules carry no builder paths, and a rebuild is byte-identical:
  `unit_convert.wasm` 86,827 B, `github_repo.wasm` 97,899 B. The ignored
  tests are `build_sh_reproduces_the_committed_modules` and
  `github_repo_answers_live` in `crates/ferrule-plugins/tests/examples.rs`.
- **Timeout test.** It is a loop against a short `timeout_secs`
  (`a_deadline_stops_a_loop`), not a sleeping host op. The deadline is
  checked between fuel slices either way.

Where the tests are:

| what | where |
|---|---|
| load checks, capabilities, limits, fencing, read-only, cancel | `crates/ferrule-plugins/tests/runtime.rs` (WAT probe) |
| the committed examples | `crates/ferrule-plugins/tests/examples.rs` |
| placeholder swap through the real proxy, undeclared domain, no route without it | `crates/ferrule-proxy/tests/plugin.rs` |
| the M19 gate for `approval` tools | `crates/ferrule-trust/tests/trust.rs` (`a_tool_that_asks_for_approval_goes_through_the_gate`) |
| install: pin, scan, queue, capabilities, widening, tamper, hot-add, schema refusal | `crates/ferrule-extensions/tests/plugins.rs` |
| the CLI and doctor through the binary | `crates/ferrule-cli/tests/plugins.rs` |
