//! Private Code Desktop — in-process engine construction.

use private_code_core::config::AppConfig;
use private_code_core::coordinator::SessionCoordinator;
use private_code_core::ecosystem::Ecosystem;
use private_code_providers::{
    detect_providers, AnthropicProvider, ModelProvider, OpenAiCompatProvider,
};
use private_code_tools::{
    BashTool, EditTool, GlobTool, GrepTool, PatchTool, ReadFileTool, ToolRegistry, WebFetchTool,
    WriteFileTool,
};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

pub async fn build_coordinator(
    pool: SqlitePool,
    global_data_dir: PathBuf,
    workspace: &Path,
) -> SessionCoordinator {
    let config = AppConfig::load(&global_data_dir, workspace);
    let mut tool_registry = default_tool_registry();
    let ecosystem = Arc::new(
        Ecosystem::bootstrap(workspace, &global_data_dir, &config, &mut tool_registry).await,
    );
    let provider: Arc<dyn ModelProvider> = Arc::new(AnthropicProvider::new());
    let mut coord = SessionCoordinator::new(
        pool,
        global_data_dir,
        provider.clone(),
        Arc::new(tool_registry),
    )
    .with_ecosystem(ecosystem);
    coord.register_provider("anthropic", provider);
    coord.register_provider("nvidia", Arc::new(OpenAiCompatProvider::nvidia()));
    for (id, p) in detect_providers().await {
        coord.register_provider(id, p);
    }
    coord
}
