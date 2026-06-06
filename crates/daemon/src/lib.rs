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
use private_code_providers::ModelProvider;
use private_code_tools::{
    BashTool, EditTool, GlobTool, GrepTool, PatchTool, ReadFileTool, ToolRegistry, WebFetchTool,
    WriteFileTool,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// The default tool set the real daemon serves.
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

/// Build the full axum router (all routes + loopback auth middleware) over a
/// coordinator. Pure construction — no I/O — so tests can mount it directly.
pub fn build_router(coordinator: Arc<SessionCoordinator>, token: String) -> Router {
    Router::new()
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
        .with_state(coordinator)
}

/// Serve the router on `listener` until `shutdown` is cancelled, letting
/// in-flight HTTP/WS connections finish (axum 0.7 graceful shutdown).
pub async fn serve_daemon(
    coordinator: Arc<SessionCoordinator>,
    token: String,
    listener: TcpListener,
    shutdown: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    let app = build_router(coordinator, token);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await?;
    Ok(())
}

/// Dependency-injected entrypoint used by both the real daemon and tests: the
/// caller supplies the provider, tool registry, a pre-bound listener, and a
/// shutdown token. The loopback auth token is read/created from global_data_dir.
pub async fn start_daemon_with(
    pool: sqlx::SqlitePool,
    global_data_dir: PathBuf,
    provider: Arc<dyn ModelProvider>,
    tool_registry: Arc<ToolRegistry>,
    listener: TcpListener,
    shutdown: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    // NEVER log the token value — daemon logs are not a secret store (security.md T4).
    let token = auth::get_or_create_token(&global_data_dir)?;
    let coordinator = Arc::new(SessionCoordinator::new(
        pool.clone(),
        global_data_dir,
        provider,
        tool_registry,
    ));
    // Map the (non-Send) Box<dyn Error> to a String immediately so this future
    // stays Send while we hold the result across the drain awaits below.
    let serve_result = serve_daemon(coordinator.clone(), token, listener, shutdown)
        .await
        .map_err(|e| e.to_string());

    // axum has stopped accepting and let in-flight HTTP/WS finish. Now drain the
    // coordinator's in-flight turns + router tasks under a bounded timeout, then
    // close the pool so the SQLite WAL is checkpointed cleanly.
    coordinator
        .shutdown(std::time::Duration::from_secs(10))
        .await;
    pool.close().await;

    serve_result.map_err(|e| e.into())
}

/// The real daemon entrypoint: register the production tools + Anthropic
/// provider, bind loopback, and serve until Ctrl-C.
pub async fn start_daemon(
    pool: sqlx::SqlitePool,
    global_data_dir: PathBuf,
    port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("Starting daemon on port {}", port);

    let tool_registry = Arc::new(default_tool_registry());
    let provider: Arc<dyn ModelProvider> = Arc::new(AnthropicProvider::new());

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("Listening on {}", addr);

    // Cancel on Ctrl-C so axum drains gracefully. (The full SIGTERM + drain
    // sequence is wired in cluster C7.)
    let shutdown = CancellationToken::new();
    let sd = shutdown.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("Ctrl-C received; shutting down");
            sd.cancel();
        }
    });

    start_daemon_with(
        pool,
        global_data_dir,
        provider,
        tool_registry,
        listener,
        shutdown,
    )
    .await
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

    #[tokio::test]
    async fn start_daemon_with_injects_provider_and_drains_on_shutdown() {
        use private_code_providers::testkit::ScriptedProvider;
        use private_code_providers::ModelProvider;
        use tokio_util::sync::CancellationToken;

        let temp = TempDir::new().unwrap();
        let dir = temp.path().to_path_buf();
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        let token = auth::get_or_create_token(&dir).unwrap();

        // DI: a mock provider, no network, on an ephemeral port.
        let provider: Arc<dyn ModelProvider> = Arc::new(ScriptedProvider::one_shot_text("hi"));
        let registry = Arc::new(ToolRegistry::new());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let sd = shutdown.clone();

        let handle = tokio::spawn(async move {
            // Discard the non-Send Box<dyn Error> result; we only assert the task ends.
            let _ = start_daemon_with(pool, dir, provider, registry, listener, sd).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{}/provider", addr))
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert!(resp.text().await.unwrap().contains("anthropic"));

        // Graceful shutdown: cancelling the token makes the server task return.
        shutdown.cancel();
        let res = tokio::time::timeout(std::time::Duration::from_secs(3), handle).await;
        assert!(
            res.is_ok(),
            "serve_daemon must return after the shutdown token is cancelled"
        );
    }
}
