//! Tauri command handlers — the bridge between the Solid.js frontend and the
//! in-process Private Code engine. Each `#[tauri::command]` is a typed RPC
//! callable from JavaScript via `invoke("command_name", { ...args })`.
//!
//! Every command is a thin wrapper over the shared [`SessionCoordinator`]; the
//! desktop owns no bespoke session state machine (see `state.rs`).

use private_code_core::coordinator::{
    durable_after, event_seq, should_forward, SessionCoordinator,
};
use private_code_core::db::{self, SessionRow};
use private_code_protocol::event::ProtocolEvent;
use serde::{Deserialize, Serialize};
use tauri::{ipc::Channel, State};

// ─── Serializable response types ───────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ProjectInfo {
    pub id: String,
    pub name: String,
    pub directory: String,
    pub created_at: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
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

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MessageInfo {
    pub id: String,
    pub session_id: String,
    pub seq: i64,
    #[serde(rename = "type")]
    pub msg_type: String,
    pub data: String,
    pub created_at: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
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

    // For lag recovery the task reads the live in-memory history directly (the
    // `sessions` map is a shared Arc) — it must NOT call `get_history`, which
    // would rebuild an evicted session; a `Lagged` implies the session is live.
    let sessions = coord.sessions.clone();
    let sid = session_id.clone();
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match rx.recv().await {
                Ok(event) => {
                    // Skip any durable event already delivered (replay or a prior
                    // lag-recovery); advance the watermark so the two paths dedup.
                    if !should_forward(&event, watermark) {
                        continue;
                    }
                    watermark = watermark.max(event_seq(&event));
                    if channel.send(event).is_err() {
                        break; // frontend disconnected
                    }
                }
                // A delta burst overflowed this connected webview's 16384 ring view,
                // evicting interleaved durable events (Error, ToolPermissionRequired
                // — which have no DB content home) it still needs. Replay the missed
                // durable events from the live in-memory history, then keep streaming.
                Err(RecvError::Lagged(_)) => {
                    let missed = {
                        let map = sessions.lock().await;
                        map.get(&sid)
                            .map(|s| durable_after(&s.history, watermark))
                            .unwrap_or_default()
                    };
                    for event in missed {
                        watermark = watermark.max(event_seq(&event));
                        if channel.send(event).is_err() {
                            return;
                        }
                    }
                }
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

// ─── Command-layer test harness ────────────────────────────────────────────
//
// Drives the REAL command functions headless: a `tauri::test::mock_app` manages
// a `SessionCoordinator` over an in-memory DB + a ScriptedProvider, and we call
// the commands directly (they are plain async fns; the `#[tauri::command]`
// macro keeps them callable). Events flow through a real `Channel` whose
// callback decodes the `ProtocolEvent` JSON — exactly the path the webview sees.
#[cfg(test)]
mod tests {
    use super::*;
    use private_code_core::coordinator::SessionCoordinator;
    use private_code_core::db::{connect_db, run_migrations};
    use private_code_providers::provider::ProviderEvent;
    use private_code_providers::testkit::ScriptedProvider;
    use private_code_providers::ModelProvider;
    use private_code_tools::ToolRegistry;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tauri::ipc::{Channel, InvokeResponseBody};
    use tauri::test::{mock_app, MockRuntime};
    use tauri::{App, Manager};
    use tempfile::TempDir;

    /// A mock app managing a coordinator over an in-memory DB + `provider` and
    /// `registry`. The returned `TempDir` doubles as the session workspace (kept
    /// alive for the test) and the coordinator's global data dir.
    async fn harness_with(
        provider: Arc<dyn ModelProvider>,
        registry: ToolRegistry,
    ) -> (App<MockRuntime>, TempDir) {
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        let dir = TempDir::new().unwrap();
        let coord =
            SessionCoordinator::new(pool, dir.path().to_path_buf(), provider, Arc::new(registry));
        let app = mock_app();
        app.manage(coord);
        (app, dir)
    }

    /// `harness_with` over an empty tool registry.
    async fn harness(provider: Arc<dyn ModelProvider>) -> (App<MockRuntime>, TempDir) {
        harness_with(provider, ToolRegistry::new()).await
    }

    /// A tool whose `permission_class` ("write_file") maps to Ask under the build
    /// agent, so a turn that calls it parks on a permission prompt.
    struct AskTool;

    #[async_trait::async_trait]
    impl private_code_tools::Tool for AskTool {
        fn name(&self) -> &str {
            "write_file"
        }
        fn description(&self) -> &str {
            "ask"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"name":"write_file","input_schema":{"type":"object"}})
        }
        fn mutates(&self) -> bool {
            false
        }
        fn permission_class(&self) -> &str {
            "write_file"
        }
        async fn run(
            &self,
            _ctx: &mut private_code_tools::ToolContext<'_>,
            _args: serde_json::Value,
        ) -> Result<serde_json::Value, private_code_tools::ToolError> {
            Ok(serde_json::json!({"ok": true}))
        }
    }

    /// Poll `collected` until `pred` finds a matching event, or panic after 5s.
    async fn wait_for<F>(collected: &Arc<Mutex<Vec<ProtocolEvent>>>, what: &str, pred: F)
    where
        F: Fn(&ProtocolEvent) -> bool,
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if collected.lock().unwrap().iter().any(&pred) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// A `Channel<ProtocolEvent>` that decodes and collects every event the
    /// command layer sends, exactly as the webview's JSON callback would.
    fn collecting_channel() -> (Channel<ProtocolEvent>, Arc<Mutex<Vec<ProtocolEvent>>>) {
        let collected = Arc::new(Mutex::new(Vec::<ProtocolEvent>::new()));
        let sink = collected.clone();
        let channel = Channel::new(move |body: InvokeResponseBody| {
            if let InvokeResponseBody::Json(s) = body {
                if let Ok(ev) = serde_json::from_str::<ProtocolEvent>(&s) {
                    sink.lock().unwrap().push(ev);
                }
            }
            Ok(())
        });
        (channel, collected)
    }

    /// init_project → create_session → list/get → set_agent → set_model →
    /// delete, all through the command surface, asserting the DB reflects each.
    #[tokio::test]
    async fn commands_crud_round_trip() {
        let (app, dir) = harness(Arc::new(ScriptedProvider::one_shot_text("hi"))).await;
        let ws = dir.path().to_str().unwrap().to_string();

        let proj = init_project(app.state(), "proj".into(), ws.clone())
            .await
            .unwrap();
        assert!(list_projects(app.state())
            .await
            .unwrap()
            .iter()
            .any(|p| p.id == proj.id));

        let sess = create_session(app.state(), proj.id.clone(), "t".into(), ws.clone())
            .await
            .unwrap();
        assert_eq!(sess.agent_id, "build");
        assert!(list_sessions(app.state(), proj.id.clone())
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == sess.id));
        assert_eq!(
            get_session(app.state(), sess.id.clone()).await.unwrap().id,
            sess.id
        );

        // set_agent persists.
        set_agent(app.state(), sess.id.clone(), "plan".into())
            .await
            .unwrap();
        assert_eq!(
            get_session(app.state(), sess.id.clone())
                .await
                .unwrap()
                .agent_id,
            "plan"
        );

        // set_model persists; the session isn't live, so eviction is a no-op → true.
        let new_model =
            serde_json::json!({"provider_id":"nvidia","model_id":"meta/llama-3.3-70b-instruct"})
                .to_string();
        assert!(set_model(app.state(), sess.id.clone(), new_model.clone())
            .await
            .unwrap());
        assert_eq!(
            get_session(app.state(), sess.id.clone())
                .await
                .unwrap()
                .model_config,
            new_model
        );

        // delete removes it.
        delete_session(app.state(), sess.id.clone()).await.unwrap();
        assert!(list_sessions(app.state(), proj.id.clone())
            .await
            .unwrap()
            .is_empty());
    }

    /// subscribe_session + send_prompt: a completed turn streams a
    /// MessageCompleted to the Channel EXACTLY once (no replay/live double-
    /// delivery) and the assistant message is persisted (reachable via
    /// get_messages).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_prompt_streams_completed_once_and_persists_messages() {
        let (app, dir) = harness(Arc::new(ScriptedProvider::one_shot_text("hello"))).await;
        let ws = dir.path().to_str().unwrap().to_string();
        let proj = init_project(app.state(), "p".into(), ws.clone())
            .await
            .unwrap();
        let sess = create_session(app.state(), proj.id.clone(), "t".into(), ws.clone())
            .await
            .unwrap();

        let (channel, collected) = collecting_channel();
        subscribe_session(app.state(), sess.id.clone(), None, channel)
            .await
            .unwrap();
        send_prompt(app.state(), sess.id.clone(), "hi".into(), None)
            .await
            .unwrap();

        // Wait (bounded) for a MessageCompleted to stream in.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let done = collected
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, ProtocolEvent::MessageCompleted { .. }));
            if done {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no MessageCompleted streamed within 5s"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        let completed = collected
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, ProtocolEvent::MessageCompleted { .. }))
            .count();
        assert_eq!(completed, 1, "MessageCompleted must stream exactly once");

        let msgs = get_messages(app.state(), sess.id.clone()).await.unwrap();
        assert!(
            msgs.iter().any(|m| m.msg_type == "assistant"),
            "the assistant message must be persisted"
        );
    }

    /// get_usage falls back to the DB for a cold (never-live) session, and the
    /// compact/revert/unrevert wrappers surface the coordinator's "nothing to
    /// do" cases as command errors.
    #[tokio::test]
    async fn usage_fallback_and_revert_compact_error_shapes() {
        let (app, dir) = harness(Arc::new(ScriptedProvider::one_shot_text("hi"))).await;
        let ws = dir.path().to_str().unwrap().to_string();
        let proj = init_project(app.state(), "p".into(), ws.clone())
            .await
            .unwrap();
        let sess = create_session(app.state(), proj.id.clone(), "t".into(), ws.clone())
            .await
            .unwrap();

        // Cold session → DB fallback returns a zeroed usage object.
        let usage = get_usage(app.state(), sess.id.clone()).await.unwrap();
        assert_eq!(usage["cost"], 0.0);
        assert_eq!(usage["input_tokens"], 0);

        // Compact on an empty transcript is a no-op success.
        compact_session(app.state(), sess.id.clone()).await.unwrap();

        // No checkpoint / no backup → command errors (the None → 4xx mapping).
        assert!(revert_session(app.state(), sess.id.clone())
            .await
            .unwrap_err()
            .contains("No checkpoints"));
        assert!(unrevert_session(app.state(), sess.id.clone())
            .await
            .unwrap_err()
            .contains("No revert backups"));
    }

    /// Full permission round-trip through the command surface: a tool call parks
    /// the turn (ToolPermissionRequired streams to the channel), a
    /// `reply_permission` DENY command resumes it, and the turn drives to a new
    /// completion. This is the event→reply loop the frontend depends on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn permission_round_trip_deny_then_turn_completes() {
        // Turn 1: a write_file tool call (→ Ask). Turn 2: text completion once the
        // denial returns as a tool_result.
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "write_file".into(),
                },
                ProviderEvent::ToolUseComplete {
                    id: "t1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({}),
                },
                ProviderEvent::MessageStop {
                    usage: Default::default(),
                    finish_reason: Some("tool_use".into()),
                },
            ],
            vec![
                ProviderEvent::TextDelta("done".into()),
                ProviderEvent::MessageStop {
                    usage: Default::default(),
                    finish_reason: Some("end_turn".into()),
                },
            ],
        ]));
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(AskTool));
        let (app, dir) = harness_with(provider, reg).await;
        let ws = dir.path().to_str().unwrap().to_string();
        let proj = init_project(app.state(), "p".into(), ws.clone())
            .await
            .unwrap();
        let sess = create_session(app.state(), proj.id.clone(), "t".into(), ws.clone())
            .await
            .unwrap();

        let (channel, collected) = collecting_channel();
        subscribe_session(app.state(), sess.id.clone(), None, channel)
            .await
            .unwrap();
        send_prompt(app.state(), sess.id.clone(), "write a file".into(), None)
            .await
            .unwrap();

        // The tool parks the turn → a ToolPermissionRequired event streams in.
        wait_for(&collected, "ToolPermissionRequired", |e| {
            matches!(e, ProtocolEvent::ToolPermissionRequired { .. })
        })
        .await;
        let perm_id = collected
            .lock()
            .unwrap()
            .iter()
            .find_map(|e| match e {
                ProtocolEvent::ToolPermissionRequired { permission_id, .. } => {
                    Some(permission_id.clone())
                }
                _ => None,
            })
            .unwrap();

        let completed_before = |c: &Arc<Mutex<Vec<ProtocolEvent>>>| {
            c.lock()
                .unwrap()
                .iter()
                .filter(|e| matches!(e, ProtocolEvent::MessageCompleted { .. }))
                .count()
        };
        let before = completed_before(&collected);

        // Deny via the command; the turn resumes and reaches a NEW completion.
        reply_permission(
            app.state(),
            sess.id.clone(),
            perm_id,
            "deny".into(),
            Some("nope".into()),
        )
        .await
        .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if completed_before(&collected) > before {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "turn did not complete after the deny (a new MessageCompleted never arrived)"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}
