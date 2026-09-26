//! Built without the `runtime` feature: manifests parse, nothing loads.

use crate::host::Host;
use crate::manifest::Manifest;
use crate::PluginError;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[derive(Debug)]
pub struct Plugin {
    manifest: Manifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    Tool(String),
    Failed(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Tool(m) | CallError::Failed(m) => f.write_str(m),
        }
    }
}

impl Plugin {
    pub fn load(_manifest: Manifest, _wasm: &[u8]) -> Result<Self, PluginError> {
        Err(PluginError::NoRuntime)
    }
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    pub fn call(
        &self,
        _tool: &str,
        _args: &serde_json::Value,
        _host: Host,
        _cancel: Arc<AtomicBool>,
    ) -> Result<serde_json::Value, CallError> {
        Err(CallError::Failed(PluginError::NoRuntime.to_string()))
    }
}
