//! Private Code Desktop — in-process engine construction.
//!
//! The Tauri shell runs the engine **in-process**: it owns the SQLite pool and
//! a shared [`SessionCoordinator`] — the same transport-agnostic coordinator
//! the daemon uses. The desktop therefore inherits every C6–C9 fix (lock-safe
//! session creation, steer/queue drain, idle reaper, graceful shutdown, durable
//! replay dedup) instead of mirroring a bespoke copy that drifts out of sync.
//! Tauri commands are a thin layer over it; events reach the frontend via typed
//! Tauri Channels.

use private_code_core::coordinator::SessionCoordinator;
use private_code_providers::{AnthropicProvider, ModelProvider, OpenAiCompatProvider};
use private_code_tools::{
    BashTool, EditTool, GlobTool, GrepTool, PatchTool, ReadFileTool, ToolRegistry, WebFetchTool,
    WriteFileTool,
};
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::Arc;

/// The production tool set served by the desktop engine (mirrors the daemon's
/// `default_tool_registry`).
fn default_tool_registry() -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register(Box::new(ReadFileTool));
    r.register(Box::new(WriteFileTool));
    r.register(Box::new(GlobTool));
    r.register(Box::new(GrepTool));
    r.register(Box::new(EditTool));
    r.register(Box::new(PatchTool));
    r.register(Box::new(BashTool));
    r.register(Box::new(WebFetchTool::new()));
    r
}

/// Build the shared coordinator: the default Anthropic provider plus the NVIDIA
/// OpenAI-compatible gateway as a per-session-selectable provider (chosen by a
/// session's `model_config.provider_id`), over the production tool registry.
/// The caller wraps this in `Arc`, starts the reaper, and `app.manage`s it.
pub fn build_coordinator(pool: SqlitePool, global_data_dir: PathBuf) -> SessionCoordinator {
    let provider: Arc<dyn ModelProvider> = Arc::new(AnthropicProvider::new());
    let mut coord = SessionCoordinator::new(
        pool,
        global_data_dir,
        provider,
        Arc::new(default_tool_registry()),
    );
    // NVIDIA's OpenAI-compatible gateway; the API key resolves lazily on first use.
    coord.register_provider("nvidia", Arc::new(OpenAiCompatProvider::nvidia()));
    coord
}
