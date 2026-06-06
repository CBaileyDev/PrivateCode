//! Tauri command handlers — the bridge between the Solid.js frontend and the
//! in-process Private Code engine. Each `#[tauri::command]` is a typed RPC
//! callable from JavaScript via `invoke("command_name", { ...args })`.
//!
//! Every command is a thin wrapper over the shared [`SessionCoordinator`]; the
//! desktop owns no bespoke session state machine (see `state.rs`).

use private_code_core::coordinator::{event_seq, should_forward, SessionCoordinator};
use private_code_core::db::{self, SessionRow};
use private_code_protocol::event::ProtocolEvent;
use serde::{Deserialize, Serialize};
use tauri::{ipc::Channel, State};

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

impl From<SessionRow> for SessionInfo {
    fn from(r: SessionRow) -> Self {
        SessionInfo {
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
        }
    }
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
pub async fn list_projects(
    coord: State<'_, SessionCoordinator>,
) -> Result<Vec<ProjectInfo>, String> {
    let rows = db::list_projects(&coord.pool)
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
    coord: State<'_, SessionCoordinator>,
    name: String,
    directory: String,
) -> Result<ProjectInfo, String> {
    let id = uuid::Uuid::new_v4().to_string();
    db::create_project(&coord.pool, &id, &name, &directory)
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
    coord: State<'_, SessionCoordinator>,
    project_id: String,
    title: String,
    workspace_path: String,
) -> Result<SessionInfo, String> {
    let id = uuid::Uuid::new_v4().to_string();
    let model_config =
        serde_json::json!({"provider_id": "anthropic", "model_id": "claude-opus-4-8"}).to_string();
    db::create_session(
        &coord.pool,
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
    coord: State<'_, SessionCoordinator>,
    project_id: String,
) -> Result<Vec<SessionInfo>, String> {
    let rows = db::list_sessions(&coord.pool, &project_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(rows.into_iter().map(SessionInfo::from).collect())
}

#[tauri::command]
pub async fn get_session(
    coord: State<'_, SessionCoordinator>,
    session_id: String,
) -> Result<SessionInfo, String> {
    let row = db::get_session(&coord.pool, &session_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Session not found".to_string())?;
    Ok(SessionInfo::from(row))
}

#[tauri::command]
pub async fn delete_session(
    coord: State<'_, SessionCoordinator>,
    session_id: String,
) -> Result<(), String> {
    db::delete_session(&coord.pool, &session_id)
        .await
        .map_err(|e| e.to_string())?;
    // The DB row is gone, so force-remove the live state (no recreate to race).
    coord.remove_session(&session_id).await;
    Ok(())
}

/// Switch a session's model (`model_config` = JSON with `provider_id` +
/// `model_id`). Returns `true` when the change is live now, `false` when a
/// provider switch was persisted but deferred because a turn is active (the UI
/// should surface "applies after the current turn" and re-issue once idle).
#[tauri::command]
pub async fn set_model(
    coord: State<'_, SessionCoordinator>,
    session_id: String,
    model_config: String,
) -> Result<bool, String> {
    coord
        .set_model(&session_id, &model_config)
        .await
        .map_err(|e| e.to_string())
}

/// Switch a session's agent. Takes effect on the next turn (no eviction).
#[tauri::command]
pub async fn set_agent(
    coord: State<'_, SessionCoordinator>,
    session_id: String,
    agent_id: String,
) -> Result<(), String> {
    coord
        .set_agent(&session_id, &agent_id)
        .await
        .map_err(|e| e.to_string())
}

// ─── Message / turn commands ───────────────────────────────────────────────

#[tauri::command]
pub async fn get_messages(
    coord: State<'_, SessionCoordinator>,
    session_id: String,
) -> Result<Vec<MessageInfo>, String> {
    let rows = db::get_messages(&coord.pool, &session_id)
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

/// Submit a prompt. Delegates to the coordinator, which admits the input to the
/// durable inbox and either starts a drain or queues it behind the running turn
/// (steer/queue semantics + backlog cap) — the desktop no longer hand-rolls the
/// admit/spawn dance.
#[tauri::command]
pub async fn send_prompt(
    coord: State<'_, SessionCoordinator>,
    session_id: String,
    prompt: String,
    delivery: Option<String>,
) -> Result<(), String> {
    let delivery = delivery.as_deref().unwrap_or("steer");
    coord
        .run_turn(&session_id, &prompt, delivery)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn abort_session(
    coord: State<'_, SessionCoordinator>,
    session_id: String,
) -> Result<(), String> {
    coord
        .abort_turn(&session_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn reply_permission(
    coord: State<'_, SessionCoordinator>,
    session_id: String,
    permission_id: String,
    reply: String,
    feedback: Option<String>,
) -> Result<(), String> {
    // The coordinator handles grant/deny AND (for "always") persisting the saved
    // rule, lock-safely and shared with the daemon.
    coord
        .reply_permission(&session_id, &permission_id, &reply, feedback.as_deref())
        .await
        .map_err(|e| e.to_string())
}

// ─── Compaction / revert commands ──────────────────────────────────────────

#[tauri::command]
pub async fn compact_session(
    coord: State<'_, SessionCoordinator>,
    session_id: String,
) -> Result<(), String> {
    coord
        .compact_session(&session_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn revert_session(
    coord: State<'_, SessionCoordinator>,
    session_id: String,
) -> Result<SessionInfo, String> {
    match coord
        .revert_session(&session_id)
        .await
        .map_err(|e| e.to_string())?
    {
        Some(row) => Ok(SessionInfo::from(row)),
        None => Err("No checkpoints available to revert to".to_string()),
    }
}

#[tauri::command]
pub async fn unrevert_session(
    coord: State<'_, SessionCoordinator>,
    session_id: String,
) -> Result<SessionInfo, String> {
    match coord
        .unrevert_session(&session_id)
        .await
        .map_err(|e| e.to_string())?
    {
        Some(row) => Ok(SessionInfo::from(row)),
        None => Err("No revert backups available to unrevert to".to_string()),
    }
}

// ─── Event subscription ────────────────────────────────────────────────────

/// Subscribe to a session's event stream via a Tauri Channel. The frontend
/// calls `invoke("subscribe_session", { sessionId, afterSeq })` and receives
/// typed `ProtocolEvent` objects on the returned channel.
///
/// Exactly-once delivery (the C9b dedup): we subscribe to the live broadcast
/// FIRST (`get_or_create_session` returns the receiver), THEN snapshot durable
/// history — so an event emitted in the gap between subscribe and snapshot is
/// covered by the replay watermark and is skipped by the live forwarder rather
/// than delivered twice. Mirrors the daemon WS handler.
#[tauri::command]
pub async fn subscribe_session(
    coord: State<'_, SessionCoordinator>,
    session_id: String,
    after_seq: Option<i64>,
    channel: Channel<ProtocolEvent>,
) -> Result<(), String> {
    let mut rx = coord
        .get_or_create_session(&session_id)
        .await
        .map_err(|e| e.to_string())?;

    let after = after_seq.unwrap_or(0);
    let mut watermark = after;
    let history = coord
        .get_history(&session_id, after)
        .await
        .map_err(|e| e.to_string())?;
    for event in history {
        watermark = watermark.max(event_seq(&event));
        if channel.send(event).is_err() {
            return Ok(()); // frontend already disconnected
        }
    }

    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match rx.recv().await {
                Ok(event) => {
                    // Skip any durable event already delivered via replay.
                    if !should_forward(&event, watermark) {
                        continue;
                    }
                    if channel.send(event).is_err() {
                        break; // frontend disconnected
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });

    Ok(())
}

// ─── Checkpoint commands ───────────────────────────────────────────────────

#[tauri::command]
pub async fn list_checkpoints(
    coord: State<'_, SessionCoordinator>,
    session_id: String,
) -> Result<Vec<CheckpointInfo>, String> {
    let rows = db::list_checkpoints(&coord.pool, &session_id)
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
        "providers": ["anthropic", "nvidia"],
    }))
}

#[tauri::command]
pub async fn get_usage(
    coord: State<'_, SessionCoordinator>,
    session_id: String,
) -> Result<serde_json::Value, String> {
    let sessions = coord.sessions.lock().await;
    if let Some(sess) = sessions.get(&session_id) {
        Ok(serde_json::to_value(&sess.current_usage).map_err(|e| e.to_string())?)
    } else {
        // Fall back to the DB for an evicted/cold session.
        drop(sessions);
        let row = db::get_session(&coord.pool, &session_id)
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
