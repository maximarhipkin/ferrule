//! Loading and calling a plugin module in wasmi: the ABI checks, a fresh
//! instance per call, fuel in slices with a wall-clock deadline between
//! them, the memory cap, and the reply cap.

use crate::host::Host;
use crate::manifest::{sha256_hex, Effective, Manifest};
use crate::{PluginError, ABI_VERSION};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use wasmi::{
    Caller, Config, Engine, Extern, Linker, Memory, Module, ResourceLimiter, Store, StoreLimits,
    StoreLimitsBuilder, TypedResumableCall,
};
use wasmi_core::LimiterError;

/// The largest module accepted.
pub const MAX_WASM: usize = 32 * 1024 * 1024;
/// The largest reply (or host request) read out of a plugin's memory.
pub const MAX_REPLY: usize = 4 * 1024 * 1024;
/// Fuel handed over at a time; between slices the host checks the clock
/// and whether the caller gave up. ~50 ms of interpreter work.
const SLICE: u64 = 10_000_000;
/// Fuel a host call may move into the store to run `ferrule_alloc`.
const ALLOC_FUEL: u64 = 1_000_000;

/// A loaded plugin: its manifest and its compiled module. Calls are
/// independent; each gets a fresh instance.
pub struct Plugin {
    manifest: Manifest,
    engine: Engine,
    module: Module,
    linker: Linker<CallState>,
}

impl std::fmt::Debug for Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin")
            .field("name", &self.manifest.name)
            .field("version", &self.manifest.version)
            .finish()
    }
}

/// Why a call didn't produce output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The plugin answered `{"error": …}`.
    Tool(String),
    /// Trap, limit, malformed reply: the plugin failed.
    Failed(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Tool(m) | CallError::Failed(m) => f.write_str(m),
        }
    }
}

pub(crate) struct CallState {
    host: Option<Host>,
    deadline: Instant,
    limits: Limiter,
    /// Fuel not yet handed to the store.
    fuel_left: u64,
    cancel: Arc<AtomicBool>,
    /// Why a host function stopped the call.
    fatal: Option<String>,
}

impl Plugin {
    /// Check the hash, compile, and check the ABI: only `ferrule.host_call`
    /// imported, the four exports present with the right types, and
    /// `ferrule_abi_version` returning 1.
    pub fn load(manifest: Manifest, wasm: &[u8]) -> Result<Self, PluginError> {
        if wasm.len() > MAX_WASM {
            return Err(PluginError::Load(format!(
                "the module is {} bytes, over the {MAX_WASM}-byte cap",
                wasm.len()
            )));
        }
        let actual = sha256_hex(wasm);
        if actual != manifest.sha256 {
            return Err(PluginError::Hash {
                expected: manifest.sha256.clone(),
                actual,
            });
        }
        let mut config = Config::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, wasm)
            .map_err(|e| PluginError::Load(format!("not a valid WebAssembly module: {e}")))?;
        for import in module.imports() {
            if (import.module(), import.name()) != ("ferrule", "host_call") {
                return Err(PluginError::Load(format!(
                    "the module imports `{}.{}`; a plugin may import only `ferrule.host_call` (no WASI: build for wasm32-unknown-unknown)",
                    import.module(),
                    import.name()
                )));
            }
        }
        let mut linker = <Linker<CallState>>::new(&engine);
        linker
            .func_wrap("ferrule", "host_call", host_call)
            .map_err(|e| PluginError::Load(e.to_string()))?;
        let plugin = Plugin {
            manifest,
            engine,
            module,
            linker,
        };
        // One trial instance: the exports, their types and the version.
        let limits = plugin.manifest.limits.effective();
        let mut store = plugin.store(None, &limits, Arc::default());
        let instance = plugin
            .linker
            .instantiate_and_start(&mut store, &plugin.module)
            .map_err(|e| PluginError::Load(format!("instantiating: {e}")))?;
        let load = |e: wasmi::Error| PluginError::Load(e.to_string());
        instance
            .get_memory(&store, "memory")
            .ok_or_else(|| PluginError::Load("the module exports no `memory`".into()))?;
        instance
            .get_typed_func::<i32, i32>(&store, "ferrule_alloc")
            .map_err(|e| PluginError::Load(format!("`ferrule_alloc(i32) -> i32`: {e}")))?;
        instance
            .get_typed_func::<(i32, i32, i32, i32), i64>(&store, "ferrule_call")
            .map_err(|e| {
                PluginError::Load(format!("`ferrule_call(i32, i32, i32, i32) -> i64`: {e}"))
            })?;
        let version = instance
            .get_typed_func::<(), i32>(&store, "ferrule_abi_version")
            .map_err(|e| PluginError::Load(format!("`ferrule_abi_version() -> i32`: {e}")))?
            .call(&mut store, ())
            .map_err(load)?;
        if version != ABI_VERSION {
            return Err(PluginError::Load(format!(
                "the module speaks plugin ABI {version}; this ferrule speaks {ABI_VERSION}"
            )));
        }
        Ok(plugin)
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    fn store(
        &self,
        host: Option<Host>,
        limits: &Effective,
        cancel: Arc<AtomicBool>,
    ) -> Store<CallState> {
        let state = CallState {
            host,
            deadline: Instant::now() + limits.timeout,
            limits: Limiter {
                inner: StoreLimitsBuilder::new()
                    .memory_size(limits.memory_bytes)
                    .memories(1)
                    .tables(4)
                    .table_elements(100_000)
                    .instances(1)
                    .build(),
                denied: false,
            },
            fuel_left: limits.fuel,
            cancel,
            fatal: None,
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|s| &mut s.limits);
        let first = store.data().fuel_left.min(SLICE);
        store.data_mut().fuel_left -= first;
        // Only fails when fuel metering is off, which it isn't.
        let _ = store.set_fuel(first);
        store
    }

    /// Run `tool` with `args` (JSON). Blocking: call from a blocking
    /// thread. Every failure is an error value; nothing here panics on
    /// anything the plugin does.
    pub fn call(
        &self,
        tool: &str,
        args: &serde_json::Value,
        host: Host,
        cancel: Arc<AtomicBool>,
    ) -> Result<serde_json::Value, CallError> {
        let limits = self.manifest.limits.effective();
        let mut store = self.store(Some(host), &limits, cancel);
        let failed = |store: &Store<CallState>, what: String| {
            CallError::Failed(explain(store, what, &limits))
        };
        let instance = self
            .linker
            .instantiate_and_start(&mut store, &self.module)
            .map_err(|e| failed(&store, format!("instantiating: {e}")))?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or_else(|| CallError::Failed("no `memory` export".into()))?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&store, "ferrule_alloc")
            .map_err(|e| CallError::Failed(e.to_string()))?;
        let call = instance
            .get_typed_func::<(i32, i32, i32, i32), i64>(&store, "ferrule_call")
            .map_err(|e| CallError::Failed(e.to_string()))?;
        let args = serde_json::to_vec(args).unwrap_or_default();
        let put = |store: &mut Store<CallState>, bytes: &[u8]| {
            let ptr = alloc
                .call(&mut *store, bytes.len() as i32)
                .map_err(|e| failed(store, format!("ferrule_alloc: {e}")))?;
            memory
                .write(&mut *store, ptr as u32 as usize, bytes)
                .map_err(|_| {
                    CallError::Failed("ferrule_alloc returned a pointer outside memory".into())
                })?;
            Ok::<i32, CallError>(ptr)
        };
        let tool_ptr = put(&mut store, tool.as_bytes())?;
        let args_ptr = put(&mut store, &args)?;
        let params = (tool_ptr, tool.len() as i32, args_ptr, args.len() as i32);
        let mut run = call.call_resumable(&mut store, params);
        let packed = loop {
            match run {
                Ok(TypedResumableCall::Finished(v)) => break v,
                Ok(TypedResumableCall::OutOfFuel(pending)) => {
                    let state = store.data_mut();
                    if state.fuel_left == 0 {
                        return Err(CallError::Failed(format!(
                            "the plugin used its whole CPU budget ({} fuel) and was stopped",
                            limits.fuel
                        )));
                    }
                    if Instant::now() >= state.deadline {
                        return Err(timed_out(&limits));
                    }
                    if state.cancel.load(Ordering::Relaxed) {
                        return Err(CallError::Failed("the call was cancelled".into()));
                    }
                    let next = state.fuel_left.min(SLICE);
                    state.fuel_left -= next;
                    let _ = store.set_fuel(next);
                    run = pending.resume(&mut store);
                }
                Ok(TypedResumableCall::HostTrap(_)) => {
                    let why = store
                        .data_mut()
                        .fatal
                        .take()
                        .unwrap_or_else(|| "a host call failed".into());
                    return Err(CallError::Failed(why));
                }
                Err(e) => {
                    if let Some(why) = store.data_mut().fatal.take() {
                        return Err(CallError::Failed(why));
                    }
                    return Err(failed(&store, format!("the plugin trapped: {e}")));
                }
            }
        };
        let reply = read(&store, &memory, packed)
            .map_err(|e| CallError::Failed(format!("reading the reply: {e}")))?;
        let reply: serde_json::Value = serde_json::from_slice(&reply)
            .map_err(|e| CallError::Failed(format!("the reply isn't JSON: {e}")))?;
        if let Some(e) = reply.get("error") {
            let text = e.as_str().map_or_else(|| e.to_string(), str::to_owned);
            return Err(CallError::Tool(text));
        }
        match reply.get("output") {
            Some(v) => Ok(v.clone()),
            None => Err(CallError::Failed(
                "the reply has neither `output` nor `error`".into(),
            )),
        }
    }
}

fn timed_out(limits: &Effective) -> CallError {
    CallError::Failed(format!(
        "the plugin ran past its {}s time limit and was stopped",
        limits.timeout.as_secs()
    ))
}

/// Add what the host knows to a trap: a memory cap that was hit, a deadline
/// that passed.
fn explain(store: &Store<CallState>, what: String, limits: &Effective) -> String {
    let mb = limits.memory_bytes / (1024 * 1024);
    if store.data().limits.denied {
        return format!("{what} (the plugin hit its {mb} MiB memory cap)");
    }
    if Instant::now() >= store.data().deadline {
        return format!("{what} (past the {}s time limit)", limits.timeout.as_secs());
    }
    what
}

/// `StoreLimits`, remembering that it refused to grow memory: the plugin
/// then usually traps (an allocator's abort), and the error should say why.
struct Limiter {
    inner: StoreLimits,
    denied: bool,
}

impl ResourceLimiter for Limiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool, LimiterError> {
        let ok = self.inner.memory_growing(current, desired, maximum)?;
        self.denied |= !ok;
        Ok(ok)
    }
    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool, LimiterError> {
        self.inner.table_growing(current, desired, maximum)
    }
    fn instances(&self) -> usize {
        self.inner.instances()
    }
    fn tables(&self) -> usize {
        self.inner.tables()
    }
    fn memories(&self) -> usize {
        self.inner.memories()
    }
}

/// A packed `ptr << 32 | len` region of `memory`, capped.
fn read<T>(
    ctx: impl wasmi::AsContext<Data = T>,
    memory: &Memory,
    packed: i64,
) -> Result<Vec<u8>, String> {
    let ptr = (packed as u64 >> 32) as usize;
    let len = (packed as u64 & 0xffff_ffff) as usize;
    if len > MAX_REPLY {
        return Err(format!("{len} bytes, over the {MAX_REPLY}-byte cap"));
    }
    let mut buf = vec![0u8; len];
    memory
        .read(&ctx, ptr, &mut buf)
        .map_err(|_| "the pointer is outside the plugin's memory".to_string())?;
    Ok(buf)
}

/// `ferrule.host_call(ptr, len) -> i64`: read the request, run the op,
/// write the reply through `ferrule_alloc`. A plugin that breaks the
/// protocol (bad pointers, no alloc) stops the call with a reason.
fn host_call(mut caller: Caller<'_, CallState>, ptr: i32, len: i32) -> Result<i64, wasmi::Error> {
    let stop = |caller: &mut Caller<'_, CallState>, why: String| {
        caller.data_mut().fatal = Some(why.clone());
        wasmi::Error::new(why)
    };
    let Some(memory) = caller.get_export("memory").and_then(Extern::into_memory) else {
        return Err(stop(&mut caller, "host_call: no `memory` export".into()));
    };
    let packed = ((ptr as u32 as u64) << 32 | len as u32 as u64) as i64;
    let request = match read(&caller, &memory, packed) {
        Ok(r) => r,
        Err(e) => return Err(stop(&mut caller, format!("host_call: the request: {e}"))),
    };
    let state = caller.data();
    if state.cancel.load(Ordering::Relaxed) {
        return Err(stop(&mut caller, "the call was cancelled".into()));
    }
    let deadline = state.deadline;
    if Instant::now() >= deadline {
        let limit = "the plugin ran past its time limit and was stopped".to_string();
        return Err(stop(&mut caller, limit));
    }
    let reply = match &state.host {
        Some(host) => host.op(&request, deadline),
        None => serde_json::json!({"error": "host_call: not available while loading"}),
    };
    if Instant::now() >= deadline {
        let limit =
            "the plugin ran past its time limit (in a host call) and was stopped".to_string();
        return Err(stop(&mut caller, limit));
    }
    let bytes = serde_json::to_vec(&reply).unwrap_or_default();
    let Some(alloc) = caller
        .get_export("ferrule_alloc")
        .and_then(Extern::into_func)
        .and_then(|f| f.typed::<i32, i32>(&caller).ok())
    else {
        return Err(stop(
            &mut caller,
            "host_call: no `ferrule_alloc` export".into(),
        ));
    };
    // `ferrule_alloc` runs inside this host call, not resumably: give it
    // fuel from the budget so a slice boundary can't strand it.
    let have = caller.get_fuel().unwrap_or(0);
    if have < ALLOC_FUEL {
        let state = caller.data_mut();
        let add = state.fuel_left.min(ALLOC_FUEL - have);
        state.fuel_left -= add;
        let _ = caller.set_fuel(have + add);
    }
    let out = match alloc.call(&mut caller, bytes.len() as i32) {
        Ok(p) => p,
        Err(e) => return Err(stop(&mut caller, format!("host_call: ferrule_alloc: {e}"))),
    };
    if memory
        .write(&mut caller, out as u32 as usize, &bytes)
        .is_err()
    {
        let why = "host_call: ferrule_alloc returned a pointer outside memory".to_string();
        return Err(stop(&mut caller, why));
    }
    Ok(((out as u32 as u64) << 32 | bytes.len() as u64) as i64)
}
