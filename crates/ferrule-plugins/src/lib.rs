//! M32: WASM tool plugins. A plugin is a `.wasm` core module plus a
//! `plugin.json` manifest (tools, JSON Schemas, capabilities, limits,
//! SHA-256). It runs in wasmi with no imports but `ferrule.host_call`,
//! whose every op is checked against the capabilities the owner granted.
//! Design: `docs/m32-wasm-plugins.md`; user guide: `docs/plugins.md`.

pub mod host;
pub mod manifest;
pub mod schema;
mod tool;

#[cfg(feature = "runtime")]
mod runtime;
#[cfg(feature = "runtime")]
pub use runtime::{CallError, Plugin};

#[cfg(not(feature = "runtime"))]
mod stub;
#[cfg(not(feature = "runtime"))]
pub use stub::{CallError, Plugin};

pub use host::Host;
pub use manifest::{Capabilities, Manifest, ToolSpec, MANIFEST_FILE};
pub use tool::{tools, PluginTool};

use std::path::Path;

/// The plugin ABI this host speaks: `ferrule_abi_version()` must return it.
pub const ABI_VERSION: i32 = 1;

/// Whether this build can run plugins (the `runtime` feature).
pub const AVAILABLE: bool = cfg!(feature = "runtime");

/// The largest `plugin.json` read.
const MAX_MANIFEST: u64 = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("{0}")]
    Manifest(String),
    #[error("the .wasm's SHA-256 is {actual}, but {expected} was expected; refusing it")]
    Hash { expected: String, actual: String },
    #[error("{0}")]
    Load(String),
    #[error("this ferrule was built without plugin support (the `plugins` feature)")]
    NoRuntime,
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// Read and parse `<dir>/plugin.json`.
pub fn read_manifest(dir: &Path) -> Result<Manifest, PluginError> {
    let path = dir.join(MANIFEST_FILE);
    let meta = std::fs::metadata(&path)
        .map_err(|e| PluginError::Manifest(format!("{}: {e}", path.display())))?;
    if meta.len() > MAX_MANIFEST {
        return Err(PluginError::Manifest(format!(
            "{} is over {MAX_MANIFEST} bytes",
            path.display()
        )));
    }
    Manifest::parse(&std::fs::read_to_string(&path)?)
}

/// Load the plugin in `dir`: manifest, hash, module, ABI.
pub fn load_dir(dir: &Path) -> Result<Plugin, PluginError> {
    let manifest = read_manifest(dir)?;
    let wasm = std::fs::read(dir.join(&manifest.wasm))
        .map_err(|e| PluginError::Load(format!("{}: {e}", dir.join(&manifest.wasm).display())))?;
    Plugin::load(manifest, &wasm)
}
