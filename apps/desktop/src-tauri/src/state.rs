//! Private Code Desktop — in-process engine construction.

use private_code_core::config::AppConfig;
use private_code_core::coordinator::{shared_ecosystem, shared_tool_registry, SessionCoordinator};
use private_code_core::ecosystem::Ecosystem;
use private_code_providers::{
    AnthropicProvider, GoogleProvider, ModelProvider, OpenAiCompatProvider,
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
    let tool_registry = shared_tool_registry(default_tool_registry());
    let ecosystem_slot = shared_ecosystem(None);
    {
        let mut guard = tool_registry.write().await;
        let ecosystem =
            Ecosystem::bootstrap(workspace, &global_data_dir, &config, &mut guard).await;
        drop(guard);
        *ecosystem_slot.write().await = Some(Arc::new(ecosystem));
    }
    // Anthropic is the default provider; the rest are registered by name. ALL
    // known providers are registered unconditionally — they resolve their
    // credential lazily on first use, so a key the user pastes into Settings at
    // runtime takes effect on the next turn with no re-registration. An
    // unconfigured provider simply fails its turn with a clear key-missing error.
    let provider: Arc<dyn ModelProvider> = Arc::new(AnthropicProvider::new());
    let mut coord = SessionCoordinator::new(
        pool,
        global_data_dir,
        provider.clone(),
        tool_registry,
        ecosystem_slot,
    );
    coord.register_provider("anthropic", provider);
    coord.register_provider("openai", Arc::new(OpenAiCompatProvider::openai()));
    coord.register_provider("google", Arc::new(GoogleProvider::new()));
    coord.register_provider("nvidia", Arc::new(OpenAiCompatProvider::nvidia()));
    coord.register_provider("deepseek", Arc::new(OpenAiCompatProvider::deepseek()));
    coord.register_provider("groq", Arc::new(OpenAiCompatProvider::groq()));
    coord.register_provider("ollama", Arc::new(OpenAiCompatProvider::ollama()));
    coord.register_provider("lmstudio", Arc::new(OpenAiCompatProvider::lmstudio()));
    coord
}

#[cfg(test)]
mod tests {
    use super::*;
    use private_code_core::db::{connect_db, run_migrations};
    use tempfile::TempDir;

    /// Headless boot check of the heaviest startup step: build_coordinator runs to
    /// completion (config load, ecosystem bootstrap over a temp workspace,
    /// provider registration, provider detection) without panicking, and every
    /// known provider is registered so a key added in Settings at runtime works.
    #[tokio::test]
    async fn build_coordinator_boots_and_registers_all_providers() {
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        let dir = TempDir::new().unwrap();
        let coord = build_coordinator(pool, dir.path().to_path_buf(), dir.path()).await;
        for id in [
            "anthropic",
            "openai",
            "google",
            "nvidia",
            "deepseek",
            "groq",
            "ollama",
            "lmstudio",
        ] {
            assert!(
                coord.providers.contains_key(id),
                "provider {id} must be registered at startup"
            );
        }
    }
}
