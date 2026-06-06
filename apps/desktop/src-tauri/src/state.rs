//! Private Code Desktop — Tauri 2 backend state management.
//!
//! The Tauri shell runs the engine **in-process**: it owns the SQLite pool,
//! the session coordinator, provider, and tool registry. No second daemon
//! process is spawned. Events flow to the frontend via typed Tauri Channels.

use private_code_core::orchestrator::Orchestrator;
use private_code_core::permissions::{PermissionPrompt, PermissionReply};
use private_code_protocol::event::{ProtocolEvent, UsageStats};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;

/// Per-session live state managed by the in-process engine.
pub struct ActiveSession {
    pub session_id: String,
    pub orchestrator: Arc<Orchestrator>,
    pub event_tx: broadcast::Sender<ProtocolEvent>,
    pub history: Vec<ProtocolEvent>,
    pub pending_permission: Option<(PermissionPrompt, oneshot::Sender<PermissionReply>)>,
    pub current_usage: UsageStats,
    pub active_turn_cancel: Option<CancellationToken>,
}

/// Global application state shared across all Tauri commands.
/// Wrapped in `tauri::State<EngineState>` and accessed via dependency injection.
pub struct EngineState {
    pub sessions: Arc<Mutex<HashMap<String, ActiveSession>>>,
    pub pool: SqlitePool,
    pub global_data_dir: PathBuf,
    pub provider: Arc<dyn private_code_providers::ModelProvider>,
    pub tool_registry: Arc<private_code_tools::ToolRegistry>,
}

impl EngineState {
    pub fn new(
        pool: SqlitePool,
        global_data_dir: PathBuf,
        provider: Arc<dyn private_code_providers::ModelProvider>,
        tool_registry: Arc<private_code_tools::ToolRegistry>,
    ) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            pool,
            global_data_dir,
            provider,
            tool_registry,
        }
    }

    /// Ensure an active session exists. Returns a broadcast receiver for event
    /// subscription. If the session is already live, reuses the existing one.
    pub async fn ensure_session(
        &self,
        session_id: &str,
    ) -> Result<broadcast::Receiver<ProtocolEvent>, String> {
        let mut sessions = self.sessions.lock().await;
        if let Some(sess) = sessions.get(session_id) {
            return Ok(sess.event_tx.subscribe());
        }

        let session_row = private_code_core::db::get_session(&self.pool, session_id)
            .await
            .map_err(|e| format!("DB error: {}", e))?
            .ok_or_else(|| format!("Session {} not found", session_id))?;

        // Permission channel: prompts from orchestrator → pending_permission slot
        let (permission_prompt_tx, mut permission_prompt_rx) = mpsc::channel(100);

        // Event channel: orchestrator → broadcast → Tauri channel subscribers
        let (event_mpsc_tx, mut event_mpsc_rx) = mpsc::channel(4096);

        let orchestrator = Arc::new(Orchestrator::new(
            self.pool.clone(),
            self.global_data_dir.clone(),
            self.provider.clone(),
            self.tool_registry.clone(),
            permission_prompt_tx,
            event_mpsc_tx,
        ));

        let (broadcast_tx, broadcast_rx) = broadcast::channel(16384);

        let active_sess = ActiveSession {
            session_id: session_id.to_string(),
            orchestrator,
            event_tx: broadcast_tx.clone(),
            history: Vec::new(),
            pending_permission: None,
            current_usage: UsageStats {
                input_tokens: session_row.tokens_input,
                output_tokens: session_row.tokens_output,
                reasoning_tokens: session_row.tokens_reasoning,
                cache_read_tokens: session_row.tokens_cache_read,
                cache_write_tokens: session_row.tokens_cache_write,
                cost: session_row.cost,
            },
            active_turn_cancel: None,
        };

        sessions.insert(session_id.to_string(), active_sess);

        // Spawn event router: mpsc (orchestrator) → broadcast (subscribers)
        let session_id_owned = session_id.to_string();
        let sessions_ref = self.sessions.clone();
        let broadcast_tx_clone = broadcast_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = event_mpsc_rx.recv().await {
                // Store durable events in history for replay on new subscriptions
                let is_ephemeral = matches!(event, ProtocolEvent::MessageDelta { .. });
                if !is_ephemeral {
                    let mut s_map = sessions_ref.lock().await;
                    if let Some(sess) = s_map.get_mut(&session_id_owned) {
                        sess.history.push(event.clone());
                        // Cap history to prevent unbounded growth
                        if sess.history.len() > 2000 {
                            sess.history.drain(..500);
                        }
                        // Track usage from completed messages
                        if let ProtocolEvent::MessageCompleted { ref usage, .. }
                        | ProtocolEvent::UsageUpdated { ref usage, .. } = event
                        {
                            sess.current_usage = usage.clone();
                        }
                    }
                }
                // Forward to all broadcast subscribers (Tauri channels)
                let _ = broadcast_tx_clone.send(event);
            }
        });

        // Spawn permission router: orchestrator prompts → session's pending slot
        let session_id_owned2 = session_id.to_string();
        let sessions_ref2 = self.sessions.clone();
        tokio::spawn(async move {
            while let Some((prompt, resp_tx)) = permission_prompt_rx.recv().await {
                let mut s_map = sessions_ref2.lock().await;
                if let Some(sess) = s_map.get_mut(&session_id_owned2) {
                    sess.pending_permission = Some((prompt, resp_tx));
                }
            }
        });

        Ok(broadcast_rx)
    }
}
