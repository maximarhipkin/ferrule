# ferrule-plugin-sdk

Write [ferrule](https://github.com/maximarhipkin/ferrule) tool plugins in
Rust. A plugin is a `wasm32-unknown-unknown` module plus a `plugin.json`
manifest; ferrule runs it in a sandboxed interpreter with only the
capabilities the owner approved.

```rust
use ferrule_plugin_sdk::{export, json, Value};

fn call(tool: &str, args: Value) -> Result<Value, String> {
    match tool {
        "double" => Ok(json!(args["n"].as_f64().ok_or("`n` is a number")? * 2.0)),
        other => Err(format!("no tool `{other}`")),
    }
}

export!(call);
```

```sh
cargo build --release --target wasm32-unknown-unknown
```

The manifest, the capabilities and the host ops (`host::http`,
`host::read_file`, …) are in
[docs/plugins.md](https://github.com/maximarhipkin/ferrule/blob/main/docs/plugins.md).
Outside ferrule (in native unit tests) every host op returns an error
saying so, and the tool function can be called directly.
