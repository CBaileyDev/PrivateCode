use private_code_core::db;
use private_code_core::orchestrator::Orchestrator;
use private_code_core::permissions::{PermissionDecision, PermissionPrompt};
use private_code_protocol::event::{ProtocolEvent, UsageStats};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub struct ActiveSession {
    pub session_id: String,
    pub orchestrator: Arc<Orchestrator>,
    pub event_tx: broadcast::Sender<ProtocolEvent>,
    pub history: Vec<ProtocolEvent>, // last 1000 durable events
    pub pending_permission: Option<(PermissionPrompt, oneshot::Sender<PermissionDecision>)>,
    pub current_usage: UsageStats,
    pub active_turn_cancel: Option<CancellationToken>,
}

pub struct SessionCoordinator {
    pub sessions: Arc<Mutex<HashMap<String, ActiveSession>>>,
    pub pool: sqlx::SqlitePool,
    pub global_data_dir: PathBuf,
    pub provider: Arc<dyn private_code_providers::ModelProvider>,
    pub tool_registry: Arc<private_code_tools::ToolRegistry>,
    /// Tracks every spawned task (event/permission routers + turn drains) so a
    /// graceful shutdown can wait for them to finish under a bounded timeout.
    pub tracker: TaskTracker,
}

fn is_durable_event(event: &ProtocolEvent) -> bool {
    !matches!(event, ProtocolEvent::MessageDelta { .. })
}

/// The durable replay-cursor sequence for an event (0 for ephemeral/uncounted events).
fn event_seq(event: &ProtocolEvent) -> i64 {
    match event {
        ProtocolEvent::MessageCompleted { seq, .. }
        | ProtocolEvent::ToolRequested { seq, .. }
        | ProtocolEvent::ToolPermissionRequired { seq, .. }
        | ProtocolEvent::ToolOutput { seq, .. }
        | ProtocolEvent::CheckpointCreated { seq, .. }
        | ProtocolEvent::UsageUpdated { seq, .. }
        | ProtocolEvent::Error { seq, .. } => *seq,
        _ => 0,
    }
}

impl SessionCoordinator {
    pub fn new(
        pool: sqlx::SqlitePool,
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
            tracker: TaskTracker::new(),
        }
    }

    /// Graceful drain: cancel every in-flight turn (now interruptible even when
    /// parked on a permission, per C5), drop the live sessions so the router
    /// tasks' channels close, then wait for all tracked tasks under `timeout`.
    /// Tasks are abort-safe (a partial assistant message is already persisted),
    /// so exceeding the timeout is acceptable — we proceed regardless.
    pub async fn shutdown(&self, timeout: Duration) {
        {
            let mut sessions = self.sessions.lock().await;
            for (_id, sess) in sessions.iter_mut() {
                if let Some(cancel) = sess.active_turn_cancel.take() {
                    cancel.cancel();
                }
                // Drop any parked permission oneshot so a waiting turn unblocks.
                sess.pending_permission = None;
            }
            // Drop the ActiveSessions: their orchestrator/event senders close once
            // the in-flight turn tasks (which also hold an Arc) finish, ending the
            // router tasks' recv loops.
            sessions.clear();
        }
        self.tracker.close();
        let _ = tokio::time::timeout(timeout, self.tracker.wait()).await;
    }

    pub async fn get_or_create_session(
        &self,
        session_id: &str,
    ) -> Result<broadcast::Receiver<ProtocolEvent>, Box<dyn std::error::Error>> {
        let mut sessions = self.sessions.lock().await;
        if let Some(sess) = sessions.get(session_id) {
            return Ok(sess.event_tx.subscribe());
        }

        // Fetch session row from db to make sure it exists
        let session_row = match db::get_session(&self.pool, session_id).await? {
            Some(row) => row,
            None => return Err(format!("Session {} not found in database", session_id).into()),
        };

        let (permission_prompt_tx, mut permission_prompt_rx) = mpsc::channel(100);
        let (event_tx, mut event_rx) = mpsc::channel(4096);

        let orchestrator = Arc::new(Orchestrator::new(
            self.pool.clone(),
            self.global_data_dir.clone(),
            self.provider.clone(),
            self.tool_registry.clone(),
            permission_prompt_tx,
            event_tx,
        ));

        // Large enough that a burst of ephemeral token deltas can't evict durable
        // events before a slow client's forwarder drains them. Lagged receivers are
        // handled gracefully (they continue) and clients reconcile from the DB.
        let (b_tx, b_rx) = broadcast::channel(16384);

        let active_sess = ActiveSession {
            session_id: session_id.to_string(),
            orchestrator,
            event_tx: b_tx.clone(),
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

        // Spawn event routing task
        let session_id_str = session_id.to_string();
        let sessions_clone = self.sessions.clone();
        let b_tx_clone = b_tx.clone();
        self.tracker.spawn(async move {
            while let Some(event) = event_rx.recv().await {
                let mut s_map = sessions_clone.lock().await;
                if let Some(sess) = s_map.get_mut(&session_id_str) {
                    if is_durable_event(&event) {
                        sess.history.push(event.clone());
                        if sess.history.len() > 1000 {
                            sess.history.remove(0);
                        }
                    }

                    // Update local stats
                    match &event {
                        ProtocolEvent::UsageUpdated { usage, .. } => {
                            sess.current_usage = usage.clone();
                        }
                        ProtocolEvent::MessageCompleted { usage, .. } => {
                            sess.current_usage = usage.clone();
                        }
                        _ => {}
                    }

                    let _ = b_tx_clone.send(event);
                }
            }
        });

        // Spawn permission routing task
        let session_id_str2 = session_id.to_string();
        let sessions_clone2 = self.sessions.clone();
        self.tracker.spawn(async move {
            while let Some((prompt, resp_tx)) = permission_prompt_rx.recv().await {
                let mut s_map = sessions_clone2.lock().await;
                if let Some(sess) = s_map.get_mut(&session_id_str2) {
                    sess.pending_permission = Some((prompt, resp_tx));
                }
            }
        });

        sessions.insert(session_id.to_string(), active_sess);
        Ok(b_rx)
    }

    pub async fn run_turn(
        &self,
        session_id: &str,
        prompt: &str,
        delivery: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.get_or_create_session(session_id).await?;

        // Reserve the turn slot and grab the orchestrator handle, then DROP the
        // sessions lock before any await — never hold the Mutex across a DB write
        // (it would block event routing and every other session's operations).
        let (orchestrator, cancel_token) = {
            let mut sessions = self.sessions.lock().await;
            let sess = sessions
                .get_mut(session_id)
                .ok_or("session not found in coordinator")?;
            if sess.active_turn_cancel.is_some() {
                return Err("A turn is already running for this session".into());
            }
            let cancel_token = CancellationToken::new();
            sess.active_turn_cancel = Some(cancel_token.clone());
            (sess.orchestrator.clone(), cancel_token)
        };

        let s_id = session_id.to_string();
        let sessions_clone = self.sessions.clone();

        // Admit the input (a DB write) with the lock released.
        let input_id = match orchestrator.admit_input(&s_id, prompt, delivery).await {
            Ok(id) => id,
            Err(e) => {
                // Release the reserved slot if admission failed.
                let mut s = sessions_clone.lock().await;
                if let Some(sess) = s.get_mut(&s_id) {
                    sess.active_turn_cancel = None;
                }
                return Err(e.into());
            }
        };

        self.tracker.spawn(async move {
            if let Err(e) = orchestrator
                .run_session_turn(&s_id, &input_id, cancel_token)
                .await
            {
                tracing::error!("Error executing turn for session {}: {}", s_id, e);
            }
            let mut s_map = sessions_clone.lock().await;
            if let Some(sess) = s_map.get_mut(&s_id) {
                sess.active_turn_cancel = None;
            }
        });

        Ok(())
    }

    pub async fn abort_turn(&self, session_id: &str) -> Result<(), Box<dyn std::error::Error>> {
        let mut sessions = self.sessions.lock().await;
        if let Some(sess) = sessions.get_mut(session_id) {
            if let Some(cancel) = sess.active_turn_cancel.take() {
                cancel.cancel();
                return Ok(());
            }
        }
        Ok(())
    }

    pub async fn reply_permission(
        &self,
        session_id: &str,
        permission_id: &str,
        reply: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut sessions = self.sessions.lock().await;
        if let Some(sess) = sessions.get_mut(session_id) {
            if let Some((prompt, _)) = &sess.pending_permission {
                if prompt.permission_id == permission_id {
                    let (_, resp_tx) = sess.pending_permission.take().unwrap();
                    let decision = match reply {
                        "always" | "once" => PermissionDecision::Allow,
                        _ => PermissionDecision::Deny,
                    };
                    let _ = resp_tx.send(decision);
                    return Ok(());
                }
            }
        }
        Err("No pending permission prompt matches the requested permission ID".into())
    }

    /// Replay durable events emitted since `after_seq` for a reconnecting client.
    /// This carries live state (completed messages, tool outputs, checkpoints,
    /// usage, errors) — NOT message *content*, which the client reconciles from
    /// the shared SQLite DB. Returning only the in-memory durable log keeps replay
    /// simple and correct; the previous DB "reconstruction" emitted content-less
    /// events and double-counted, which broke attach.
    pub async fn get_history(
        &self,
        session_id: &str,
        after_seq: i64,
    ) -> Result<Vec<ProtocolEvent>, Box<dyn std::error::Error>> {
        self.get_or_create_session(session_id).await?;
        let sessions = self.sessions.lock().await;
        let sess = sessions
            .get(session_id)
            .ok_or("session not found in coordinator")?;
        Ok(sess
            .history
            .iter()
            .filter(|e| event_seq(e) > after_seq)
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream::{BoxStream, StreamExt};
    use private_code_core::db::{connect_db, create_project, create_session, run_migrations};
    use private_code_protocol::message::ChatMessage;
    use private_code_providers::provider::{ModelProvider, ProviderError, ProviderEvent};
    use std::time::Duration;
    use tempfile::TempDir;

    /// Scripted text-only provider: one assistant turn, no tools, then done.
    struct OneShotTextProvider;

    #[async_trait::async_trait]
    impl ModelProvider for OneShotTextProvider {
        async fn stream_chat(
            &self,
            _model_id: &str,
            _system_prompt: Option<&str>,
            _max_tokens: u32,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError>
        {
            let evs = vec![
                Ok(ProviderEvent::TextDelta("hello".into())),
                Ok(ProviderEvent::MessageStop {
                    usage: UsageStats::default(),
                    finish_reason: Some("end_turn".into()),
                }),
            ];
            Ok(futures_util::stream::iter(evs).boxed())
        }
        fn count_tokens(&self, _m: &str, t: &str) -> usize {
            t.len() / 4
        }
    }

    /// Drives a real turn through the coordinator (no HTTP) and asserts that a
    /// durable event reaches a subscriber AND is replayable via get_history —
    /// covering run_turn (lock fix), the event-routing task, and get_history.
    #[tokio::test]
    async fn test_run_turn_broadcasts_and_replays_durable_events() {
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        let ws = TempDir::new().unwrap();
        let ws_str = ws.path().to_str().unwrap();
        let project_id = uuid::Uuid::new_v4().to_string();
        create_project(&pool, &project_id, "t", ws_str)
            .await
            .unwrap();
        let session_id = uuid::Uuid::new_v4().to_string();
        let cfg = serde_json::json!({"provider_id": "anthropic", "model_id": "claude-opus-4-8"})
            .to_string();
        create_session(
            &pool,
            &session_id,
            &project_id,
            ws_str,
            ws_str,
            "t",
            "build",
            &cfg,
        )
        .await
        .unwrap();

        let coord = SessionCoordinator::new(
            pool,
            std::env::temp_dir(),
            Arc::new(OneShotTextProvider),
            Arc::new(private_code_tools::ToolRegistry::new()),
        );

        // Subscribe BEFORE running the turn, then run it.
        let mut rx = coord.get_or_create_session(&session_id).await.unwrap();
        coord.run_turn(&session_id, "hi", "steer").await.unwrap();

        // A durable MessageCompleted must arrive on the broadcast.
        let mut got_completed = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                Ok(Ok(ProtocolEvent::MessageCompleted { .. })) => {
                    got_completed = true;
                    break;
                }
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => break,
                Err(_) => continue,
            }
        }
        assert!(
            got_completed,
            "a durable MessageCompleted must be broadcast to subscribers"
        );

        // And it must be replayable for a reconnecting client via get_history.
        let hist = coord.get_history(&session_id, 0).await.unwrap();
        assert!(
            hist.iter()
                .any(|e| matches!(e, ProtocolEvent::MessageCompleted { .. })),
            "get_history must replay the durable MessageCompleted"
        );
    }

    /// A tool whose permission_class ("write_file") maps to Ask under the build
    /// agent, parking the turn on the permission prompt.
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

    /// Graceful shutdown must drain a turn that is PARKED on a permission prompt
    /// (no reply ever arrives): cancelling it via shutdown unblocks the permission
    /// wait (C5), so tracker.wait() completes well before the timeout.
    #[tokio::test]
    async fn shutdown_drains_a_permission_parked_turn() {
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        let ws = TempDir::new().unwrap();
        let ws_str = ws.path().to_str().unwrap();
        let project_id = uuid::Uuid::new_v4().to_string();
        create_project(&pool, &project_id, "t", ws_str)
            .await
            .unwrap();
        let session_id = uuid::Uuid::new_v4().to_string();
        let cfg =
            serde_json::json!({"provider_id":"anthropic","model_id":"claude-opus-4-8"}).to_string();
        create_session(
            &pool,
            &session_id,
            &project_id,
            ws_str,
            ws_str,
            "t",
            "build",
            &cfg,
        )
        .await
        .unwrap();

        // Provider emits a write_file tool call (-> Ask), parking the turn.
        let provider = Arc::new(private_code_providers::testkit::ScriptedProvider::new(
            vec![vec![
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
                    usage: UsageStats::default(),
                    finish_reason: Some("tool_use".into()),
                },
            ]],
        ));
        let mut reg = private_code_tools::ToolRegistry::new();
        reg.register(Box::new(AskTool));

        let coord = SessionCoordinator::new(pool, std::env::temp_dir(), provider, Arc::new(reg));
        coord.run_turn(&session_id, "hi", "steer").await.unwrap();

        // Let the turn reach the (unanswered) permission wait.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let start = tokio::time::Instant::now();
        coord.shutdown(Duration::from_secs(10)).await;
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "shutdown must drain a permission-parked turn promptly (took {:?})",
            start.elapsed()
        );
    }
}
