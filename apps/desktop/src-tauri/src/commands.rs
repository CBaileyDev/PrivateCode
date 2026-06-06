//! Tauri command handlers — the bridge between the Solid.js frontend and the
//! in-process Private Code engine. Each `#[tauri::command]` is a typed RPC
//! callable from JavaScript via `invoke("command_name", { ...args })`.

use crate::state::EngineState;
use private_code_core::db;
use private_code_core::permissions::PermissionReply;
use private_code_protocol::event::ProtocolEvent;
use serde::{Deserialize, Serialize};
use tauri::{ipc::Channel, State};
use tokio_util::sync::CancellationToken;

// ─── Serializable response types ───────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
pub struct ProjectInfo {
    pub id: String,
    pub name: String,
    pub directory: String,
    pub created_at: i64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub agent_id: String,
    pub model_config: String,
    pub cost: f64,
    pub tokens_input: i64,
    pub tokens_output: i64,
    pub tokens_reasoning: i64,
    pub tokens_cache_read: i64,
    pub tokens_cache_write: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct MessageInfo {
    pub id: String,
    pub session_id: String,
    pub seq: i64,
    #[serde(rename = "type")]
    pub msg_type: String,
    pub data: String,
    pub created_at: i64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct CheckpointInfo {
    pub id: String,
    pub session_id: String,
    pub message_id: String,
    pub tree_hash: String,
    pub tool_name: String,
    pub created_at: i64,
}

// ─── Project commands ──────────────────────────────────────────────────────

#[tauri::command]
pub async fn list_projects(engine: State<'_, EngineState>) -> Result<Vec<ProjectInfo>, String> {
    let rows = db::list_projects(&engine.pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|r| ProjectInfo {
            id: r.id,
            name: r.name,
            directory: r.directory,
            created_at: r.created_at,
        })
        .collect())
}

#[tauri::command]
pub async fn init_project(
    engine: State<'_, EngineState>,
    name: String,
    directory: String,
) -> Result<ProjectInfo, String> {
    let id = uuid::Uuid::new_v4().to_string();
    db::create_project(&engine.pool, &id, &name, &directory)
        .await
        .map_err(|e| e.to_string())?;
    Ok(ProjectInfo {
        id,
        name,
        directory,
        created_at: chrono::Utc::now().timestamp(),
    })
}

// ─── Session commands ──────────────────────────────────────────────────────

#[tauri::command]
pub async fn create_session(
    engine: State<'_, EngineState>,
    project_id: String,
    title: String,
    workspace_path: String,
) -> Result<SessionInfo, String> {
    let id = uuid::Uuid::new_v4().to_string();
    let model_config =
        serde_json::json!({"provider_id": "anthropic", "model_id": "claude-opus-4-8"}).to_string();
    db::create_session(
        &engine.pool,
        &id,
        &project_id,
        &workspace_path,
        &workspace_path,
        &title,
        "build",
        &model_config,
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(SessionInfo {
        id,
        project_id,
        title,
        agent_id: "build".to_string(),
        model_config,
        cost: 0.0,
        tokens_input: 0,
        tokens_output: 0,
        tokens_reasoning: 0,
        tokens_cache_read: 0,
        tokens_cache_write: 0,
        created_at: chrono::Utc::now().timestamp(),
        updated_at: chrono::Utc::now().timestamp(),
    })
}

#[tauri::command]
pub async fn list_sessions(
    engine: State<'_, EngineState>,
    project_id: String,
) -> Result<Vec<SessionInfo>, String> {
    let rows = db::list_sessions(&engine.pool, &project_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|r| SessionInfo {
            id: r.id,
            project_id: r.project_id,
            title: r.title,
            agent_id: r.agent_id,
            model_config: r.model_config,
            cost: r.cost,
            tokens_input: r.tokens_input,
            tokens_output: r.tokens_output,
            tokens_reasoning: r.tokens_reasoning,
            tokens_cache_read: r.tokens_cache_read,
            tokens_cache_write: r.tokens_cache_write,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
        .collect())
}

#[tauri::command]
pub async fn get_session(
    engine: State<'_, EngineState>,
    session_id: String,
) -> Result<SessionInfo, String> {
    let row = db::get_session(&engine.pool, &session_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Session not found".to_string())?;
    Ok(SessionInfo {
        id: row.id,
        project_id: row.project_id,
        title: row.title,
        agent_id: row.agent_id,
        model_config: row.model_config,
        cost: row.cost,
        tokens_input: row.tokens_input,
        tokens_output: row.tokens_output,
        tokens_reasoning: row.tokens_reasoning,
        tokens_cache_read: row.tokens_cache_read,
        tokens_cache_write: row.tokens_cache_write,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

#[tauri::command]
pub async fn delete_session(
    engine: State<'_, EngineState>,
    session_id: String,
) -> Result<(), String> {
    db::delete_session(&engine.pool, &session_id)
        .await
        .map_err(|e| e.to_string())?;

    // Clean up the live session if it exists
    let mut sessions = engine.sessions.lock().await;
    if let Some(sess) = sessions.remove(&session_id) {
        if let Some(cancel) = sess.active_turn_cancel {
            cancel.cancel();
        }
    }
    Ok(())
}

// ─── Message / turn commands ───────────────────────────────────────────────

#[tauri::command]
pub async fn get_messages(
    engine: State<'_, EngineState>,
    session_id: String,
) -> Result<Vec<MessageInfo>, String> {
    let rows = db::get_messages(&engine.pool, &session_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|r| MessageInfo {
            id: r.id,
            session_id: r.session_id,
            seq: r.seq,
            msg_type: r.type_,
            data: r.data,
            created_at: r.created_at,
        })
        .collect())
}

#[tauri::command]
pub async fn send_prompt(
    engine: State<'_, EngineState>,
    session_id: String,
    prompt: String,
) -> Result<(), String> {
    engine.ensure_session(&session_id).await?;

    let (orchestrator, cancel_token) = {
        let mut sessions = engine.sessions.lock().await;
        let sess = sessions
            .get_mut(&session_id)
            .ok_or("Session not found in engine")?;
        if sess.active_turn_cancel.is_some() {
            return Err("A turn is already running".to_string());
        }
        let cancel_token = CancellationToken::new();
        sess.active_turn_cancel = Some(cancel_token.clone());
        (sess.orchestrator.clone(), cancel_token)
    };

    let s_id = session_id.clone();
    let sessions_ref = engine.sessions.clone();

    let input_id = orchestrator
        .admit_input(&s_id, &prompt, "steer")
        .await
        .map_err(|e| {
            // Release the reserved slot on admission failure
            let sessions_ref2 = sessions_ref.clone();
            let s_id2 = s_id.clone();
            tokio::spawn(async move {
                let mut s = sessions_ref2.lock().await;
                if let Some(sess) = s.get_mut(&s_id2) {
                    sess.active_turn_cancel = None;
                }
            });
            e.to_string()
        })?;

    tokio::spawn(async move {
        if let Err(e) = orchestrator
            .run_session_turn(&s_id, &input_id, cancel_token)
            .await
        {
            tracing::error!("Turn error for session {}: {}", s_id, e);
        }
        let mut s_map = sessions_ref.lock().await;
        if let Some(sess) = s_map.get_mut(&s_id) {
            sess.active_turn_cancel = None;
        }
    });

    Ok(())
}

#[tauri::command]
pub async fn abort_session(
    engine: State<'_, EngineState>,
    session_id: String,
) -> Result<(), String> {
    let mut sessions = engine.sessions.lock().await;
    if let Some(sess) = sessions.get_mut(&session_id) {
        if let Some(cancel) = sess.active_turn_cancel.take() {
            cancel.cancel();
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn reply_permission(
    engine: State<'_, EngineState>,
    session_id: String,
    permission_id: String,
    reply: String,
    feedback: Option<String>,
) -> Result<(), String> {
    // Handle "always" — persist the permission rule
    if reply == "always" {
        if let Ok(Some(sess_row)) = db::get_session(&engine.pool, &session_id).await {
            let sessions = engine.sessions.lock().await;
            if let Some(active) = sessions.get(&session_id) {
                if let Some((prompt, _)) = &active.pending_permission {
                    if prompt.permission_id == permission_id {
                        let action = prompt.action.clone();
                        let resource = prompt
                            .resources
                            .first()
                            .cloned()
                            .unwrap_or_else(|| "*".to_string());
                        let _ = db::save_permission(
                            &engine.pool,
                            &sess_row.project_id,
                            &action,
                            &resource,
                        )
                        .await;
                    }
                }
            }
        }
    }

    let mut sessions = engine.sessions.lock().await;
    if let Some(sess) = sessions.get_mut(&session_id) {
        if let Some((prompt, _)) = &sess.pending_permission {
            if prompt.permission_id == permission_id {
                let (_, resp_tx) = sess.pending_permission.take().unwrap();
                let decision = match reply.as_str() {
                    "always" | "once" => PermissionReply::Allow,
                    _ => PermissionReply::Deny { feedback },
                };
                let _ = resp_tx.send(decision);
                return Ok(());
            }
        }
    }
    Err("No matching pending permission".to_string())
}

// ─── Event subscription ────────────────────────────────────────────────────

/// Subscribe to a session's event stream via a Tauri Channel.
/// The frontend calls `invoke("subscribe_session", { sessionId })` and
/// receives typed `ProtocolEvent` objects on the returned channel.
#[tauri::command]
pub async fn subscribe_session(
    engine: State<'_, EngineState>,
    session_id: String,
    channel: Channel<ProtocolEvent>,
) -> Result<(), String> {
    let mut rx = engine.ensure_session(&session_id).await?;

    // Replay durable history
    {
        let sessions = engine.sessions.lock().await;
        if let Some(sess) = sessions.get(&session_id) {
            for event in &sess.history {
                let _ = channel.send(event.clone());
            }
        }
    }

    // Stream live events
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if channel.send(event).is_err() {
                        break; // Frontend disconnected
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    Ok(())
}

// ─── Checkpoint commands ───────────────────────────────────────────────────

#[tauri::command]
pub async fn list_checkpoints(
    engine: State<'_, EngineState>,
    session_id: String,
) -> Result<Vec<CheckpointInfo>, String> {
    let rows = db::list_checkpoints(&engine.pool, &session_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|r| CheckpointInfo {
            id: r.id,
            session_id: r.session_id,
            message_id: r.message_id,
            tree_hash: r.tree_hash,
            tool_name: r.tool_name,
            created_at: r.created_at,
        })
        .collect())
}

// ─── Config commands ───────────────────────────────────────────────────────

#[tauri::command]
pub async fn get_config() -> Result<serde_json::Value, String> {
    Ok(serde_json::json!({
        "default_model": "anthropic/claude-opus-4-8",
        "default_agent": "build",
        "agents": ["build", "plan", "general", "explore"],
        "providers": ["anthropic"],
    }))
}

#[tauri::command]
pub async fn get_usage(
    engine: State<'_, EngineState>,
    session_id: String,
) -> Result<serde_json::Value, String> {
    let sessions = engine.sessions.lock().await;
    if let Some(sess) = sessions.get(&session_id) {
        Ok(serde_json::to_value(&sess.current_usage).map_err(|e| e.to_string())?)
    } else {
        // Fall back to DB
        let row = db::get_session(&engine.pool, &session_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("Session not found")?;
        Ok(serde_json::json!({
            "input_tokens": row.tokens_input,
            "output_tokens": row.tokens_output,
            "reasoning_tokens": row.tokens_reasoning,
            "cache_read_tokens": row.tokens_cache_read,
            "cache_write_tokens": row.tokens_cache_write,
            "cost": row.cost,
        }))
    }
}
