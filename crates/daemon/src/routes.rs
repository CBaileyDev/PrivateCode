use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{sse::Event, Sse},
    Json,
};
use futures_util::stream::Stream;
use private_code_core::coordinator::SessionCoordinator;
use private_code_core::db::{self, CheckpointRow, MessageRow, ProjectRow, SessionRow};
use private_code_protocol::event::ProtocolEvent;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::path::Path as StdPath;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Deserialize)]
pub struct ProjectInitBody {
    pub name: String,
    pub directory: String,
}

#[derive(Deserialize)]
pub struct SessionCreateBody {
    pub workspace_path: String,
    pub active_directory: String,
    pub title: String,
    pub agent_id: String,
    pub model_config: serde_json::Value,
}

#[derive(Deserialize)]
pub struct PromptBody {
    pub prompt: String,
    pub delivery: String, // "steer" | "queue"
}

#[derive(Deserialize)]
pub struct PermissionReplyBody {
    pub reply: String, // "once" | "always" | "reject"
    pub feedback: Option<String>,
}

#[derive(Serialize)]
pub struct FileStatus {
    pub path: String,
    pub status: String, // "modified" | "untracked" | "deleted" | "added"
}

// 1. GET /project
pub async fn list_projects(
    State(coord): State<Arc<SessionCoordinator>>,
) -> Result<Json<Vec<ProjectRow>>, (StatusCode, String)> {
    let projects = db::list_projects(&coord.pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(projects))
}

// 2. POST /project/init
pub async fn init_project(
    State(coord): State<Arc<SessionCoordinator>>,
    Json(body): Json<ProjectInitBody>,
) -> Result<Json<ProjectRow>, (StatusCode, String)> {
    // Check if project directory already exists
    let existing = sqlx::query_as::<_, ProjectRow>("SELECT * FROM project WHERE directory = ?1")
        .bind(&body.directory)
        .fetch_optional(&coord.pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if let Some(proj) = existing {
        return Ok(Json(proj));
    }

    let id = Uuid::new_v4().to_string();
    db::create_project(&coord.pool, &id, &body.name, &body.directory)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let proj = db::get_project(&coord.pool, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            StatusCode::NOT_FOUND,
            "Failed to retrieve created project".to_string(),
        ))?;

    Ok(Json(proj))
}

// 3. GET /project/:projectID/session
pub async fn list_sessions(
    Path(project_id): Path<String>,
    State(coord): State<Arc<SessionCoordinator>>,
) -> Result<Json<Vec<SessionRow>>, (StatusCode, String)> {
    let sessions = db::list_sessions(&coord.pool, &project_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(sessions))
}

// 4. GET /project/:projectID/session/:sessionID
pub async fn get_session(
    Path((_project_id, session_id)): Path<(String, String)>,
    State(coord): State<Arc<SessionCoordinator>>,
) -> Result<Json<SessionRow>, (StatusCode, String)> {
    let sess = db::get_session(&coord.pool, &session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Session not found".to_string()))?;
    Ok(Json(sess))
}

// 5. POST /project/:projectID/session
pub async fn create_session(
    Path(project_id): Path<String>,
    State(coord): State<Arc<SessionCoordinator>>,
    Json(body): Json<SessionCreateBody>,
) -> Result<Json<SessionRow>, (StatusCode, String)> {
    let id = Uuid::new_v4().to_string();
    let model_config_str = serde_json::to_string(&body.model_config)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    db::create_session(
        &coord.pool,
        &id,
        &project_id,
        &body.workspace_path,
        &body.active_directory,
        &body.title,
        &body.agent_id,
        &model_config_str,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let sess = db::get_session(&coord.pool, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            StatusCode::NOT_FOUND,
            "Failed to retrieve created session".to_string(),
        ))?;

    Ok(Json(sess))
}

// 6. DELETE /project/:projectID/session/:sessionID
pub async fn delete_session(
    Path((_project_id, session_id)): Path<(String, String)>,
    State(coord): State<Arc<SessionCoordinator>>,
) -> Result<StatusCode, (StatusCode, String)> {
    sqlx::query("DELETE FROM session WHERE id = ?1")
        .bind(&session_id)
        .execute(&coord.pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Also remove from active coordinator map
    let mut sessions = coord.sessions.lock().await;
    sessions.remove(&session_id);

    Ok(StatusCode::NO_CONTENT)
}

// 7. POST /project/:projectID/session/:sessionID/abort
pub async fn abort_session(
    Path((_project_id, session_id)): Path<(String, String)>,
    State(coord): State<Arc<SessionCoordinator>>,
) -> Result<StatusCode, (StatusCode, String)> {
    coord
        .abort_turn(&session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::OK)
}

// 8. POST /project/:projectID/session/:sessionID/compact
pub async fn compact_session(
    Path((_project_id, session_id)): Path<(String, String)>,
    State(coord): State<Arc<SessionCoordinator>>,
) -> Result<StatusCode, (StatusCode, String)> {
    coord
        .compact_session(&session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::OK)
}

// 9. POST /project/:projectID/session/:sessionID/revert
pub async fn revert_session(
    Path((_project_id, session_id)): Path<(String, String)>,
    State(coord): State<Arc<SessionCoordinator>>,
) -> Result<Json<SessionRow>, (StatusCode, String)> {
    match coord
        .revert_session(&session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    {
        Some(row) => Ok(Json(row)),
        None => Err((
            StatusCode::BAD_REQUEST,
            "No checkpoints available to revert to".to_string(),
        )),
    }
}

// 10. POST /project/:projectID/session/:sessionID/unrevert
pub async fn unrevert_session(
    Path((_project_id, session_id)): Path<(String, String)>,
    State(coord): State<Arc<SessionCoordinator>>,
) -> Result<Json<SessionRow>, (StatusCode, String)> {
    match coord
        .unrevert_session(&session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    {
        Some(row) => Ok(Json(row)),
        None => Err((
            StatusCode::BAD_REQUEST,
            "No revert backups available to unrevert to".to_string(),
        )),
    }
}

// 11. GET /project/:projectID/session/:sessionID/message
pub async fn get_messages(
    Path((_project_id, session_id)): Path<(String, String)>,
    State(coord): State<Arc<SessionCoordinator>>,
) -> Result<Json<Vec<MessageRow>>, (StatusCode, String)> {
    let msgs = db::get_messages(&coord.pool, &session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(msgs))
}

// 12. POST /project/:projectID/session/:sessionID/message
pub async fn prompt_session(
    Path((_project_id, session_id)): Path<(String, String)>,
    State(coord): State<Arc<SessionCoordinator>>,
    Json(body): Json<PromptBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    coord
        .run_turn(&session_id, &body.prompt, &body.delivery)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::ACCEPTED)
}

// 13. POST /project/:projectID/session/:sessionID/permission/:permissionID
pub async fn reply_permission(
    Path((_project_id, session_id, permission_id)): Path<(String, String, String)>,
    State(coord): State<Arc<SessionCoordinator>>,
    Json(body): Json<PermissionReplyBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    // The coordinator handles the grant/deny AND (for "always") persisting the
    // saved rule — lock-safely and shared with the desktop front-end.
    coord
        .reply_permission(
            &session_id,
            &permission_id,
            &body.reply,
            body.feedback.as_deref(),
        )
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::OK)
}

// 14. GET /project/:projectID/session/:sessionID/file/status
pub async fn get_file_status(
    Path((_project_id, session_id)): Path<(String, String)>,
    State(coord): State<Arc<SessionCoordinator>>,
) -> Result<Json<Vec<FileStatus>>, (StatusCode, String)> {
    let session = db::get_session(&coord.pool, &session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Session not found".to_string()))?;

    let workspace_path = StdPath::new(&session.workspace_path);
    let mut file_statuses = Vec::new();

    if let Ok(repo) = git2::Repository::open(workspace_path) {
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true);
        if let Ok(statuses) = repo.statuses(Some(&mut opts)) {
            for entry in statuses.iter() {
                let status = entry.status();
                let path = entry.path().unwrap_or("").to_string();
                let status_str = if status.is_index_new() || status.is_wt_new() {
                    "untracked"
                } else if status.is_index_modified() || status.is_wt_modified() {
                    "modified"
                } else if status.is_index_deleted() || status.is_wt_deleted() {
                    "deleted"
                } else {
                    continue;
                };
                file_statuses.push(FileStatus {
                    path,
                    status: status_str.to_string(),
                });
            }
        }
    }

    Ok(Json(file_statuses))
}

// 15. GET /project/:projectID/session/:sessionID/checkpoint
pub async fn list_checkpoints(
    Path((_project_id, session_id)): Path<(String, String)>,
    State(coord): State<Arc<SessionCoordinator>>,
) -> Result<Json<Vec<CheckpointRow>>, (StatusCode, String)> {
    let check_points = db::list_checkpoints(&coord.pool, &session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(check_points))
}

// 16. GET /provider
#[derive(Serialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
}

pub async fn list_providers() -> Json<Vec<ProviderInfo>> {
    Json(vec![ProviderInfo {
        id: "anthropic".to_string(),
        name: "Anthropic".to_string(),
    }])
}

// 17. GET /config
pub async fn get_config(
    State(coord): State<Arc<SessionCoordinator>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Return base / global configuration
    let config_path = coord.global_data_dir.join("config.json");
    if config_path.exists() {
        let content = std::fs::read_to_string(config_path)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let val: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        Ok(Json(val))
    } else {
        // Return default configuration
        let default_config = serde_json::json!({
            "model": "anthropic/claude-opus-4-8",
            "small_model": "anthropic/claude-haiku-4-5",
            "shell": "/bin/zsh",
            "default_agent": "build",
            "snapshots": true,
            "permissions": [],
            "tool_output": {
                "max_lines": 2000,
                "max_bytes": 51200
            }
        });
        Ok(Json(default_config))
    }
}

fn is_durable_event(event: &ProtocolEvent) -> bool {
    !matches!(event, ProtocolEvent::MessageDelta { .. })
}

pub async fn sse_handler(
    Path((_project_id, session_id)): Path<(String, String)>,
    axum::extract::Query(query): axum::extract::Query<crate::ws::WsQuery>,
    State(coord): State<Arc<SessionCoordinator>>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, String)> {
    let event_rx = match coord.get_or_create_session(&session_id).await {
        Ok(rx) => rx,
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    };

    let after_seq = query.after_seq.unwrap_or(0);
    let history = match coord.get_history(&session_id, after_seq).await {
        Ok(h) => h,
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    };

    let stream =
        futures_util::stream::unfold((history, event_rx), |(mut history, mut rx)| async move {
            if !history.is_empty() {
                let ev = history.remove(0);
                if let Ok(sse_event) = Event::default().json_data(&ev) {
                    return Some((Ok::<_, Infallible>(sse_event), (history, rx)));
                }
            }

            while let Ok(ev) = rx.recv().await {
                if is_durable_event(&ev) {
                    if let Ok(sse_event) = Event::default().json_data(&ev) {
                        return Some((Ok::<_, Infallible>(sse_event), (history, rx)));
                    }
                }
            }
            None
        });

    Ok(Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()))
}
