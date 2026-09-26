//! Write ferrule tool plugins in Rust (ABI v1, `docs/m32-wasm-plugins.md`
//! §2): [`export!`] turns one function into the module's exports, and
//! [`host`] wraps the one import, `ferrule.host_call`.
//!
//! ```
//! use ferrule_plugin_sdk::{export, json, Value};
//!
//! fn call(tool: &str, args: Value) -> Result<Value, String> {
//!     match tool {
//!         "double" => Ok(json!(args["n"].as_f64().ok_or("`n` is a number")? * 2.0)),
//!         other => Err(format!("no tool `{other}`")),
//!     }
//! }
//!
//! export!(call);
//! # assert_eq!(call("double", json!({"n": 2})), Ok(json!(4.0)));
//! ```
//!
//! A string output reaches the model as text, anything else as pretty
//! JSON; an `Err` is a tool error the model sees. Build for
//! `wasm32-unknown-unknown` (not `wasm32-wasip1`: ferrule refuses WASI
//! imports).

pub use serde_json::{json, Value};

/// What one tool call returns.
pub type ToolResult = Result<Value, String>;

/// The four exports ferrule looks for, around `handler: fn(&str, Value) ->
/// Result<Value, String>`. Use it once, at the crate root of a `cdylib`.
#[macro_export]
macro_rules! export {
    ($handler:path) => {
        #[cfg(target_arch = "wasm32")]
        #[no_mangle]
        pub extern "C" fn ferrule_abi_version() -> i32 {
            $crate::ABI_VERSION
        }

        #[cfg(target_arch = "wasm32")]
        #[no_mangle]
        pub extern "C" fn ferrule_alloc(len: i32) -> i32 {
            $crate::__rt::alloc(len as usize) as i32
        }

        /// # Safety
        /// Called by the host with regions it wrote through `ferrule_alloc`.
        #[cfg(target_arch = "wasm32")]
        #[no_mangle]
        pub unsafe extern "C" fn ferrule_call(tp: i32, tl: i32, ap: i32, al: i32) -> i64 {
            let tool = $crate::__rt::region(tp as usize, tl as usize);
            let args = $crate::__rt::region(ap as usize, al as usize);
            let reply = $crate::__rt::dispatch(tool, args, $handler);
            $crate::__rt::leak(reply)
        }
    };
}

/// The ABI this SDK speaks; `ferrule_abi_version` returns it.
pub const ABI_VERSION: i32 = 1;

/// Not API: what [`export!`] expands to.
#[doc(hidden)]
pub mod __rt {
    use serde_json::{json, Value};

    /// A buffer the host writes into. Never freed: every call gets a fresh
    /// instance, and its memory goes with it.
    pub fn alloc(len: usize) -> *mut u8 {
        let mut buf = Vec::<u8>::with_capacity(len.max(1));
        let ptr = buf.as_mut_ptr();
        std::mem::forget(buf);
        ptr
    }

    /// # Safety
    /// `ptr..ptr+len` must be memory the host wrote.
    pub unsafe fn region<'a>(ptr: usize, len: usize) -> &'a [u8] {
        if len == 0 {
            return &[];
        }
        std::slice::from_raw_parts(ptr as *const u8, len)
    }

    /// `bytes` left in memory, as the packed `ptr << 32 | len` the host reads.
    pub fn leak(bytes: Vec<u8>) -> i64 {
        let bytes = bytes.into_boxed_slice();
        let len = bytes.len() as u64;
        let ptr = Box::into_raw(bytes) as *mut u8 as usize as u64;
        ((ptr << 32) | len) as i64
    }

    /// Run the handler and encode its reply: `{"output": …}` or
    /// `{"error": "…"}`. A panic can't be caught on wasm32 (it traps, and
    /// the host reports the trap), so this only has to handle `Err`.
    pub fn dispatch(
        tool: &[u8],
        args: &[u8],
        handler: fn(&str, Value) -> Result<Value, String>,
    ) -> Vec<u8> {
        let reply = match (std::str::from_utf8(tool), parse_args(args)) {
            (Ok(tool), Ok(args)) => match handler(tool, args) {
                Ok(output) => json!({"output": output}),
                Err(error) => json!({"error": error}),
            },
            (Err(_), _) => json!({"error": "the tool name isn't UTF-8"}),
            (_, Err(e)) => json!({"error": e}),
        };
        serde_json::to_vec(&reply).unwrap_or_else(|_| br#"{"error":"unencodable reply"}"#.to_vec())
    }

    fn parse_args(args: &[u8]) -> Result<Value, String> {
        if args.is_empty() {
            return Ok(Value::Object(Default::default()));
        }
        serde_json::from_slice(args).map_err(|e| format!("the arguments aren't JSON: {e}"))
    }
}

/// The host's ops. Each is checked against the plugin's granted
/// capabilities on every call; a denied one is an `Err` naming why.
pub mod host {
    use serde::{Deserialize, Serialize};
    use serde_json::{json, Map, Value};

    /// Returned by every op outside ferrule (native tests, other hosts).
    pub const NOT_IN_FERRULE: &str = "not inside ferrule: host ops only work in a loaded plugin";

    /// One request, through ferrule's credential proxy when it runs:
    /// `https://` only, to a domain in `capabilities.http.domains`, with a
    /// granted method. `${NAME}` in a header value becomes secret `NAME`'s
    /// placeholder, swapped for the real value by the proxy on the way
    /// out; the plugin never sees the key.
    #[derive(Debug, Clone, Default, Serialize)]
    pub struct Request {
        pub method: String,
        pub url: String,
        #[serde(skip_serializing_if = "Map::is_empty")]
        pub headers: Map<String, Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub body: Option<String>,
    }

    impl Request {
        pub fn get(url: impl Into<String>) -> Self {
            Request {
                method: "GET".into(),
                url: url.into(),
                ..Default::default()
            }
        }

        pub fn post(url: impl Into<String>, body: impl Into<String>) -> Self {
            Request {
                method: "POST".into(),
                url: url.into(),
                body: Some(body.into()),
                ..Default::default()
            }
        }

        pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
            self.headers
                .insert(name.into(), Value::String(value.into()));
            self
        }
    }

    #[derive(Debug, Clone, Deserialize)]
    pub struct Response {
        pub status: u16,
        #[serde(default)]
        pub headers: Map<String, Value>,
        /// Lossy UTF-8, capped by the host.
        pub body: String,
        /// The body was longer than the host's cap.
        #[serde(default)]
        pub truncated: bool,
    }

    impl Response {
        pub fn json(&self) -> Result<Value, String> {
            serde_json::from_str(&self.body).map_err(|e| format!("the response isn't JSON: {e}"))
        }
    }

    /// An entry of [`list_dir`].
    #[derive(Debug, Clone, Deserialize)]
    pub struct DirEntry {
        pub name: String,
        pub dir: bool,
    }

    pub fn http(req: &Request) -> Result<Response, String> {
        let mut v = serde_json::to_value(req).map_err(|e| e.to_string())?;
        v["op"] = json!("http");
        decode(call(&v)?)
    }

    /// A UTF-8 file under a granted `files.read` (or `files.write`)
    /// directory, relative to the workspace.
    pub fn read_file(path: &str) -> Result<String, String> {
        decode(call(&json!({"op": "read_file", "path": path}))?)
    }

    /// Write under a granted `files.write` directory; returns the bytes
    /// written.
    pub fn write_file(path: &str, content: &str) -> Result<u64, String> {
        decode(call(
            &json!({"op": "write_file", "path": path, "content": content}),
        )?)
    }

    pub fn list_dir(path: &str) -> Result<Vec<DirEntry>, String> {
        decode(call(&json!({"op": "list_dir", "path": path}))?)
    }

    /// Milliseconds since the Unix epoch (`clock: true`).
    pub fn now_ms() -> Result<u64, String> {
        decode(call(&json!({"op": "now"}))?)
    }

    /// `len` random bytes, hex (`random: true`).
    pub fn random_hex(len: usize) -> Result<String, String> {
        decode(call(&json!({"op": "random", "len": len}))?)
    }

    fn decode<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, String> {
        serde_json::from_value(v).map_err(|e| format!("unexpected host reply: {e}"))
    }

    /// One `ferrule.host_call`: `{"ok": …}` or `{"error": "…"}`.
    pub fn call(request: &Value) -> Result<Value, String> {
        let bytes = serde_json::to_vec(request).map_err(|e| e.to_string())?;
        let reply = raw(&bytes)?;
        let mut reply: Value =
            serde_json::from_slice(&reply).map_err(|e| format!("host reply isn't JSON: {e}"))?;
        if let Some(e) = reply.get("error") {
            return Err(e.as_str().unwrap_or("host error").to_string());
        }
        Ok(reply.get_mut("ok").map(Value::take).unwrap_or(Value::Null))
    }

    #[cfg(target_arch = "wasm32")]
    fn raw(request: &[u8]) -> Result<Vec<u8>, String> {
        #[link(wasm_import_module = "ferrule")]
        extern "C" {
            fn host_call(ptr: i32, len: i32) -> i64;
        }
        // SAFETY: the host reads `request` and writes its reply through
        // `ferrule_alloc`, returning where it put it.
        unsafe {
            let packed = host_call(request.as_ptr() as i32, request.len() as i32) as u64;
            let (ptr, len) = ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize);
            Ok(crate::__rt::region(ptr, len).to_vec())
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn raw(_request: &[u8]) -> Result<Vec<u8>, String> {
        Err(NOT_IN_FERRULE.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handler(tool: &str, args: Value) -> ToolResult {
        match tool {
            "echo" => Ok(args),
            "fetch" => {
                host::http(&host::Request::get("https://example.com")).map(|r| json!(r.status))
            }
            _ => Err(format!("no tool `{tool}`")),
        }
    }

    fn reply(tool: &str, args: &str) -> Value {
        serde_json::from_slice(&__rt::dispatch(tool.as_bytes(), args.as_bytes(), handler)).unwrap()
    }

    #[test]
    fn replies_are_output_or_error() {
        assert_eq!(reply("echo", r#"{"a":1}"#), json!({"output": {"a": 1}}));
        assert_eq!(reply("echo", ""), json!({"output": {}}));
        assert_eq!(reply("nope", "{}"), json!({"error": "no tool `nope`"}));
        assert!(reply("echo", "{")["error"]
            .as_str()
            .unwrap()
            .contains("aren't JSON"));
    }

    #[test]
    fn host_ops_say_they_need_ferrule_outside_it() {
        assert_eq!(reply("fetch", "{}"), json!({"error": host::NOT_IN_FERRULE}));
        assert_eq!(host::now_ms(), Err(host::NOT_IN_FERRULE.to_string()));
    }

    #[test]
    fn a_request_serialises_as_the_http_op_expects() {
        let r =
            host::Request::get("https://api.github.com/x").header("authorization", "Bearer ${GH}");
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            v,
            json!({"method": "GET", "url": "https://api.github.com/x", "headers": {"authorization": "Bearer ${GH}"}})
        );
    }
}
