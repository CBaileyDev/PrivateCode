//! Private Code Desktop — Tauri 2 binary entrypoint.
//!
//! Boots the in-process engine (SQLite pool, provider, tool registry),
//! registers all Tauri commands, and launches the Tauri application.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use private_code_desktop::commands;
use private_code_desktop::state::EngineState;

use private_code_core::db;
use private_code_providers::AnthropicProvider;
use private_code_tools::{
    BashTool, EditTool, GlobTool, GrepTool, PatchTool, ReadFileTool, ToolRegistry, WebFetchTool,
    WriteFileTool,
};
use std::sync::Arc;
use tauri::Manager;
use tracing_subscriber::EnvFilter;

fn main() {
    // Initialize structured logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            let app_handle = app.handle().clone();

            // Resolve the global data directory
            let data_dir = app_handle
                .path()
                .app_data_dir()
                .expect("Failed to resolve app data dir");
            std::fs::create_dir_all(&data_dir).expect("Failed to create app data dir");

            let db_path = data_dir.join("private_code.db");
            let db_url = format!("sqlite:{}?mode=rwc", db_path.display());

            // Run async setup on Tauri's async runtime so the sqlx pool's
            // background tasks outlive `setup`. A throwaway `Runtime` dropped here
            // would sever the pool; commands run on this same runtime.
            let pool = tauri::async_runtime::block_on(async {
                let pool = db::connect_db(&db_url)
                    .await
                    .expect("Failed to connect to database");
                db::run_migrations(&pool)
                    .await
                    .expect("Failed to run migrations");
                pool
            });

            // Register tools
            let mut tool_registry = ToolRegistry::new();
            tool_registry.register(Box::new(ReadFileTool));
            tool_registry.register(Box::new(WriteFileTool));
            tool_registry.register(Box::new(GlobTool));
            tool_registry.register(Box::new(GrepTool));
            tool_registry.register(Box::new(EditTool));
            tool_registry.register(Box::new(PatchTool));
            tool_registry.register(Box::new(BashTool));
            tool_registry.register(Box::new(WebFetchTool::new()));

            let provider = Arc::new(AnthropicProvider::new());

            let engine_state = EngineState::new(pool, data_dir, provider, Arc::new(tool_registry));

            app.manage(engine_state);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_projects,
            commands::init_project,
            commands::create_session,
            commands::list_sessions,
            commands::get_session,
            commands::delete_session,
            commands::get_messages,
            commands::send_prompt,
            commands::abort_session,
            commands::reply_permission,
            commands::subscribe_session,
            commands::list_checkpoints,
            commands::get_config,
            commands::get_usage,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Private Code");
}
