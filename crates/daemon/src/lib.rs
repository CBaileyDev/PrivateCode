pub mod auth;
pub mod coordinator;
pub mod routes;
pub mod ws;

use axum::{
    middleware,
    routing::{get, post},
    Router,
};
use coordinator::SessionCoordinator;
use private_code_providers::anthropic::AnthropicProvider;
use private_code_tools::{
    BashTool, EditTool, GlobTool, GrepTool, PatchTool, ReadFileTool, ToolRegistry, WebFetchTool,
    WriteFileTool,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

pub async fn start_daemon(
    pool: sqlx::SqlitePool,
    global_data_dir: PathBuf,
    port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Get or create the loopback auth token. NEVER log the token value —
    //    daemon logs are not a secret store (security.md T4).
    let token = auth::get_or_create_token(&global_data_dir)?;
    tracing::info!("Starting daemon on port {}", port);

    // 2. Register tools
    let mut tool_registry = ToolRegistry::new();
    tool_registry.register(Box::new(ReadFileTool));
    tool_registry.register(Box::new(WriteFileTool));
    tool_registry.register(Box::new(GlobTool));
    tool_registry.register(Box::new(GrepTool));
    tool_registry.register(Box::new(EditTool));
    tool_registry.register(Box::new(PatchTool));
    tool_registry.register(Box::new(BashTool));
    tool_registry.register(Box::new(WebFetchTool::new()));
    let tool_registry = Arc::new(tool_registry);

    let provider = Arc::new(AnthropicProvider::new());

    // 3. Create Session Coordinator
    let coordinator = Arc::new(SessionCoordinator::new(
        pool,
        global_data_dir,
        provider,
        tool_registry,
    ));

    // 4. Build Axum Router
    let app = Router::new()
        .route(
            "/project",
            get(routes::list_projects).post(routes::init_project),
        )
        .route(
            "/project/:projectID/session",
            get(routes::list_sessions).post(routes::create_session),
        )
        .route(
            "/project/:projectID/session/:sessionID",
            get(routes::get_session).delete(routes::delete_session),
        )
        .route(
            "/project/:projectID/session/:sessionID/abort",
            post(routes::abort_session),
        )
        .route(
            "/project/:projectID/session/:sessionID/compact",
            post(routes::compact_session),
        )
        .route(
            "/project/:projectID/session/:sessionID/revert",
            post(routes::revert_session),
        )
        .route(
            "/project/:projectID/session/:sessionID/unrevert",
            post(routes::unrevert_session),
        )
        .route(
            "/project/:projectID/session/:sessionID/message",
            get(routes::get_messages).post(routes::prompt_session),
        )
        .route(
            "/project/:projectID/session/:sessionID/permission/:permissionID",
            post(routes::reply_permission),
        )
        .route(
            "/project/:projectID/session/:sessionID/file/status",
            get(routes::get_file_status),
        )
        .route(
            "/project/:projectID/session/:sessionID/checkpoint",
            get(routes::list_checkpoints),
        )
        .route("/provider", get(routes::list_providers))
        .route("/config", get(routes::get_config))
        .route("/ws", get(ws::ws_handler))
        .route(
            "/project/:projectID/session/:sessionID/events",
            get(routes::sse_handler),
        )
        .layer(middleware::from_fn(move |req, next| {
            auth::auth_middleware(token.clone(), req, next)
        }))
        .with_state(coordinator);

    // 5. Listen and serve on loopback
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("Listening on {}", addr);
    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use private_code_core::db::{connect_db, run_migrations};
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_daemon_authentication_and_routes() {
        let temp_dir = TempDir::new().unwrap();
        let global_data_dir = temp_dir.path().to_path_buf();

        // 1. Setup in-memory database
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();

        // 2. Generate a token
        let token = auth::get_or_create_token(&global_data_dir).unwrap();
        assert!(!token.is_empty());

        // 3. Start daemon in background on random port
        // Let's find a free port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let pool_clone = pool.clone();
        let global_dir_clone = global_data_dir.clone();
        tokio::spawn(async move {
            let _ = start_daemon(pool_clone, global_dir_clone, port).await;
        });

        // Wait for server to bind
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{}/provider", port);

        // A. Verify request without token fails (401)
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

        // B. Verify request with bad token fails (401)
        let resp = client
            .get(&url)
            .header("Authorization", "Bearer bad_token_123")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

        // C. Verify request with correct token succeeds (200)
        let resp = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        // D. Verify correct payload
        let body_str = resp.text().await.unwrap();
        assert!(body_str.contains("anthropic"));

        // E. A valid token with a DNS-rebinding Host header is still rejected (400).
        let resp = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header(reqwest::header::HOST, "127.0.0.1.evil.com")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

        // F. A valid token with a spoofed Origin is rejected (403).
        let resp = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header(reqwest::header::ORIGIN, "http://localhost.evil.com")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
    }
}
