//! Private Code Desktop — Tauri 2 binary entrypoint.
//!
//! Boots the in-process engine (SQLite pool + shared `SessionCoordinator`),
//! starts the idle-session reaper, registers all Tauri commands, and runs the
//! app — draining the coordinator on exit.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use private_code_desktop::commands;
use private_code_desktop::state::build_coordinator;

use private_code_core::coordinator::SessionCoordinator;
use private_code_core::db;
use std::path::PathBuf;
use std::time::Duration;
use tauri::Manager;
use tracing_subscriber::EnvFilter;

fn main() {
    // Initialize structured logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
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

            // Build the shared coordinator (Anthropic default + NVIDIA), start
            // the idle reaper, and manage it as global state.
            let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let coordinator = tauri::async_runtime::block_on(async {
                let coordinator = build_coordinator(pool, data_dir, &workspace).await;
                coordinator.start_reaper(Duration::from_secs(30 * 60), Duration::from_secs(60));
                coordinator
            });
            app.manage(coordinator);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_projects,
            commands::init_project,
            commands::open_or_create_project,
            commands::provider_status,
            commands::set_provider_key,
            commands::remove_provider_key,
            commands::create_session,
            commands::list_sessions,
            commands::get_session,
            commands::delete_session,
            commands::set_model,
            commands::set_agent,
            commands::get_messages,
            commands::send_prompt,
            commands::abort_session,
            commands::reply_permission,
            commands::compact_session,
            commands::revert_session,
            commands::unrevert_session,
            commands::subscribe_session,
            commands::list_checkpoints,
            commands::get_config,
            commands::get_usage,
            commands::export_session,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Private Code");

    // Drive the event loop, draining the coordinator's in-flight turns + router
    // tasks (under a bounded timeout) when the app is asked to exit. Best-effort:
    // a hard kill skips this, but tasks are abort-safe (partial assistant output
    // is already persisted).
    app.run(|app_handle, event| {
        if let tauri::RunEvent::ExitRequested { .. } = event {
            let coordinator = app_handle.state::<SessionCoordinator>();
            tauri::async_runtime::block_on(coordinator.shutdown(Duration::from_secs(10)));
        }
    });
}
