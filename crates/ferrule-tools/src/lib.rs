//! Built-in tools. Everything is workspace-scoped and output-capped —
//! verbose tool output is the number one source of context bloat.

pub mod diary;
pub mod fs_tools;
pub mod shell;
pub mod web;

pub use diary::{DiaryTool, WriteTodosTool};
pub use fs_tools::{ListDirTool, ReadFileTool, WriteFileTool};
pub use shell::ShellTool;
pub use web::WebFetchTool;

use ferrule_core::tool::ToolRegistry;
use std::sync::Arc;

/// The standard toolbelt every agent gets.
pub fn standard_registry() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ReadFileTool::default()));
    reg.register(Arc::new(WriteFileTool::default()));
    reg.register(Arc::new(ListDirTool::default()));
    reg.register(Arc::new(ShellTool::default()));
    reg.register(Arc::new(WebFetchTool::default()));
    reg.register(Arc::new(WriteTodosTool));
    reg.register(Arc::new(DiaryTool));
    reg
}
