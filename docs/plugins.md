# Plugins

A plugin is a WebAssembly module plus a `plugin.json` manifest that adds
tools to the agent. Ferrule runs it in an interpreter (wasmi) with no
access to anything by default. It can't open a file, reach the network,
read the environment or see the time unless its manifest asks for it and
you approved that. Plugins sit next to MCP servers and skills. Choose a
plugin when a tool should be small, portable and sandboxed down to the
single file or domain; choose an MCP server when it needs a whole process.

Design and threat model: `docs/m32-wasm-plugins.md`.

## Using one

```sh
ferrule plugins add ./examples/plugins/unit-convert     # a local directory
ferrule plugins add git:https://github.com/me/tools@<40-hex sha> --path plugins/foo
ferrule plugins add https://example.com/foo/plugin.json --sha256 <hex>
ferrule plugins list
ferrule plugins remove unit-convert
```

`add` checks the manifest, the `.wasm`'s SHA-256, loads the module and
scans every tool text (M13's `scan_tool`). Then it shows you the tools and
what the plugin may do ("HTTPS GET to api.github.com", "secret
GITHUB_TOKEN in request headers", "read files under docs/"), and installs
on yes. It needs a terminal. Every source is pinned exactly: a git commit,
the `--sha256` of a URL install (required), and in every case the wasm
hash in the manifest. A mismatch is refused, and the error shows both
hashes.

Each tool reaches the model as `plugin__<plugin>__<tool>`. Running
sessions pick an install or removal up within seconds, and new sessions
start with it. `ferrule doctor` loads every plugin and prints its tools
and capabilities, or why it doesn't load.
`ferrule extensions list` shows plugins along with MCP servers and skills.

**The agent can propose one too.** With `[extensions] enabled = true`,
`plugin_add` and `plugin_remove` join the other extension tools. A plugin
installs without asking only when all of these hold:
- its source matches `[extensions] allow`;
- the scan is clean;
- it asks for nothing beyond `clock` and `random`.

Anything asking for files, the network or secrets waits in the queue for
`ferrule extensions approve <id>`, even from an allow-listed source. The
approval shows the capabilities. A workspace directory the agent proposes
always waits too.

**Updates.** An update that asks for more than you granted, such as a new
domain, a write directory or a secret, goes back to you, with the new
lines marked. The old version stays active until you approve. Narrowing
needs nothing.

**Tampering.** If a plugin's files change after install (a different
`.wasm` or a changed manifest), the plugin is suspended until
`ferrule extensions resume <name>`, which checks it again.

## What a plugin may do

Everything is denied unless the manifest's `capabilities` grants it:

| capability | what it allows |
|---|---|
| `files.read` / `files.write` | directories **relative to the workspace** (`"."` for all of it); paths resolve like the built-in file tools do, and the sandbox's read deny list (ferrule's own data, credential dirs, `deny_read`) still applies inside them |
| `http.domains` (+ `methods`, default GET) | HTTPS requests to those hosts only (`*.example.com` for subdomains), through the same client and credential proxy `web_fetch` uses. No sockets, no plain HTTP |
| `secrets` | `${NAME}` in a request header becomes the secret's **placeholder**, and the proxy swaps the real value in on the hosts it's bound to (`ferrule secrets`). With no proxy running, or a secret not bound, the request is refused, so a real key never leaves in a plugin's request and the plugin never sees one |
| `clock` / `random` | the time in milliseconds / OS random bytes |

There's no op for environment variables. A module importing anything
other than `ferrule.host_call` (WASI included) doesn't load.

Output from a plugin with `http` reaches the model fenced as
`<plugin_output … untrusted="true">`, like web pages.

**Limits** per call (manifest `limits`, each capped by the host):

| limit | default | cap |
|---|---|---|
| `fuel` | 1 000 000 000 | 10 000 000 000 |
| `memory_mb` | 64 | 256 |
| `timeout_secs` | 30 | 120 |
| `output_chars` | the session's tool output cap | 100 000 |

A loop, a memory bomb, a trap, a timeout or a malformed reply becomes a
tool error with a one-line reason. The agent and the daemon keep running,
and the next call gets a fresh instance.

**Guards.** Hooks (M18), approvals, caps and the kill switch (M19), plan
mode (M20) and sub-agent reach apply as for any tool. A tool with
`"approval": true` asks you on every call. A tool marked `read_only` runs
in parallel with other reads only when the plugin was granted no write
directory and no HTTP method other than GET/HEAD.

**Arguments** are checked against the tool's JSON Schema before the
plugin runs. The supported keywords are listed in the design doc (§6). A
schema using any other keyword is refused at install, so no rule goes
silently unchecked.

## Writing one (Rust)

```toml
# Cargo.toml
[lib]
crate-type = ["cdylib"]

[dependencies]
ferrule-plugin-sdk = { path = "…/crates/ferrule-plugin-sdk" }  # not on crates.io yet
```

```rust
use ferrule_plugin_sdk::host::{self, Request};
use ferrule_plugin_sdk::{export, json, Value};

fn call(tool: &str, args: Value) -> Result<Value, String> {
    match tool {
        "hello" => Ok(json!(format!("hello, {}", args["name"].as_str().unwrap_or("you")))),
        "status" => {
            let r = host::http(&Request::get("https://api.example.com/status")
                .header("authorization", "Bearer ${EXAMPLE_KEY}"))?;
            Ok(json!(r.body))
        }
        other => Err(format!("no tool `{other}`")),
    }
}

export!(call);
```

Build with `cargo build --release --target wasm32-unknown-unknown`, then
put its SHA-256 in `plugin.json`:

```json
{
  "name": "hello",
  "version": "0.1.0",
  "description": "Says hello.",
  "wasm": "hello.wasm",
  "sha256": "<sha256sum hello.wasm>",
  "tools": [{
    "name": "hello",
    "description": "Greet someone.",
    "parameters": {"type": "object", "properties": {"name": {"type": "string"}},
                   "additionalProperties": false},
    "read_only": true
  }],
  "capabilities": {}
}
```

The SDK's `host` module wraps every op: `http`, `read_file`,
`write_file`, `list_dir`, `now_ms`, `random_hex`. The two examples in
`examples/plugins/` are complete projects:
- `unit-convert` is pure compute with no capabilities;
- `github-repo` makes one proxied GET with `${GITHUB_TOKEN}`.

`examples/plugins/build.sh` rebuilds both reproducibly and re-pins their
manifests. It needs `rustup target add wasm32-unknown-unknown`. The built
`.wasm` files are committed so that tests never build them.
`cargo test -p ferrule-plugins --test examples -- --ignored` checks that a
rebuild gives the same bytes, and also calls the live GitHub API.

### Other languages

The ABI is plain core WebAssembly with JSON across the boundary. The
module exports:
- `memory`;
- `ferrule_abi_version() -> i32`, returning 1;
- `ferrule_alloc(len: i32) -> i32`;
- `ferrule_call(tool_ptr, tool_len, args_ptr, args_len) -> i64`, which
  returns `ptr << 32 | len` of `{"output": …}` or `{"error": "…"}`.

It may import `ferrule.host_call(ptr, len) -> i64`, which takes
`{"op": "http" | "read_file" | "write_file" | "list_dir" | "now" | "random", …}`
and returns `{"ok": …}` or `{"error": "…"}`. The design doc (§2) has the
details.

| language | status |
|---|---|
| Rust + `ferrule-plugin-sdk` | supported, tested |
| TinyGo (`-target=wasm-unknown`), AssemblyScript, Zig, C (`wasm32-freestanding`) | should work with about 40 lines of glue; untested |
| javy (JS), componentize-py (Python), anything built for `wasm32-wasip1` | not supported: these need WASI or the component model |

## Building without plugins

The `plugins` feature (default on) carries the interpreter, about 1.2 MB
of the binary. A build without it still reads the lock file and lists
the installed plugins, but loads none of them and installs nothing. Both
`plugins list` and doctor say so.
