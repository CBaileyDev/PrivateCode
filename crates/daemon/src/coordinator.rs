use private_code_core::db;
use private_code_core::orchestrator::Orchestrator;
use private_code_core::permissions::{PermissionDecision, PermissionPrompt};
use private_code_protocol::event::{ProtocolEvent, UsageStats};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Max number of inputs that may be queued behind an active turn before
/// `run_turn` rejects further admissions (session.md inbox backlog limit).
const MAX_BACKLOG: usize = 32;
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
    /// `Some` iff a drain chain is running for this session. This is the single
    /// concurrency invariant: it is set ONLY by `run_turn` (atomically with the
    /// spawn decision) and cleared ONLY by the drain loop (atomically with
    /// `queued.pop_front()` returning `None`). `abort_turn` never touches it.
    /// That makes parallel drains and stranded queue items impossible.
    pub active_turn_cancel: Option<CancellationToken>,
    /// FIFO of admitted-but-not-yet-run input ids (`session_input.id`) waiting
    /// behind the active turn (session.md: "queue inputs form a FIFO of future
    /// activities ... promotes exactly one queued input ... at a time").
    pub queued: VecDeque<String>,
    /// Cancels this session's router tasks so eviction tears them down
    /// deterministically (rather than relying on sender-drop alone).
    pub session_cancel: CancellationToken,
    /// Last time this session saw activity; the reaper evicts idle sessions.
    pub last_activity: std::time::Instant,
}

pub struct SessionCoordinator {
    pub sessions: Arc<Mutex<HashMap<String, ActiveSession>>>,
    pub pool: sqlx::SqlitePool,
    pub global_data_dir: PathBuf,
    pub provider: Arc<dyn private_code_providers::ModelProvider>,
    pub tool_registry: Arc<private_code_tools::ToolRegistry>,
    /// Tracks every spawned task (event/permission routers + turn drains + reaper)
    /// so a graceful shutdown can wait for them to finish under a bounded timeout.
    pub tracker: TaskTracker,
    /// Cancelled on shutdown to stop the reaper loop (which otherwise sleeps
    /// forever and would block tracker.wait()).
    pub shutdown_token: CancellationToken,
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
            shutdown_token: CancellationToken::new(),
        }
    }

    /// Spawn the idle-session reaper. Every `interval`, it evicts sessions that
    /// have no active turn, no pending permission, and have been idle longer than
    /// `idle_ttl`. Eviction frees in-memory live state only — DB rows are
    /// untouched and the session transparently rebuilds on next access. The loop
    /// stops when `shutdown_token` is cancelled.
    pub fn start_reaper(&self, idle_ttl: Duration, interval: Duration) {
        let sessions = self.sessions.clone();
        let shutdown = self.shutdown_token.clone();
        self.tracker.spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(interval) => {}
                }
                let mut map = sessions.lock().await;
                let evict: Vec<String> = map
                    .iter()
                    .filter(|(_, s)| {
                        s.active_turn_cancel.is_none()
                            && s.pending_permission.is_none()
                            && s.queued.is_empty()
                            && s.last_activity.elapsed() > idle_ttl
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in evict {
                    if let Some(sess) = map.remove(&id) {
                        // Tear down the router tasks deterministically.
                        sess.session_cancel.cancel();
                    }
                }
            }
        });
    }

    /// Graceful drain: cancel every in-flight turn (now interruptible even when
    /// parked on a permission, per C5), drop the live sessions so the router
    /// tasks' channels close, then wait for all tracked tasks under `timeout`.
    /// Tasks are abort-safe (a partial assistant message is already persisted),
    /// so exceeding the timeout is acceptable — we proceed regardless.
    pub async fn shutdown(&self, timeout: Duration) {
        // Stop the reaper loop (it sleeps forever otherwise).
        self.shutdown_token.cancel();
        {
            let mut sessions = self.sessions.lock().await;
            for (_id, sess) in sessions.iter_mut() {
                if let Some(cancel) = sess.active_turn_cancel.take() {
                    cancel.cancel();
                }
                // Tear down the router tasks and drop any parked permission oneshot
                // so a waiting turn unblocks.
                sess.session_cancel.cancel();
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
        if let Some(sess) = sessions.get_mut(session_id) {
            sess.last_activity = std::time::Instant::now();
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
        let session_cancel = CancellationToken::new();

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
            queued: VecDeque::new(),
            session_cancel: session_cancel.clone(),
            last_activity: std::time::Instant::now(),
        };

        // Spawn event routing task (ends on session_cancel or when the sender closes).
        let session_id_str = session_id.to_string();
        let sessions_clone = self.sessions.clone();
        let b_tx_clone = b_tx.clone();
        let event_cancel = session_cancel.clone();
        self.tracker.spawn(async move {
            loop {
                let event = tokio::select! {
                    biased;
                    _ = event_cancel.cancelled() => break,
                    ev = event_rx.recv() => match ev {
                        Some(e) => e,
                        None => break,
                    },
                };
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

        // Spawn permission routing task (ends on session_cancel or sender close).
        let session_id_str2 = session_id.to_string();
        let sessions_clone2 = self.sessions.clone();
        let perm_cancel = session_cancel.clone();
        self.tracker.spawn(async move {
            loop {
                let item = tokio::select! {
                    biased;
                    _ = perm_cancel.cancelled() => break,
                    p = permission_prompt_rx.recv() => match p {
                        Some(x) => x,
                        None => break,
                    },
                };
                let (prompt, resp_tx) = item;
                let mut s_map = sessions_clone2.lock().await;
                if let Some(sess) = s_map.get_mut(&session_id_str2) {
                    sess.pending_permission = Some((prompt, resp_tx));
                }
            }
        });

        sessions.insert(session_id.to_string(), active_sess);
        Ok(b_rx)
    }

    /// Admit a prompt and either start a drain chain for it or queue it behind
    /// the active turn. A second prompt arriving mid-turn is NOT rejected — it is
    /// admitted to the durable inbox and run FIFO when the current turn settles
    /// (session.md: "queue inputs form a FIFO ... promotes exactly one queued
    /// input ... at a time"). The backlog cap is the only rejection path.
    pub async fn run_turn(
        &self,
        session_id: &str,
        prompt: &str,
        delivery: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.get_or_create_session(session_id).await?;

        // Phase 1: enforce the backlog cap and grab the orchestrator handle.
        // Reject ONLY when a drain is active AND the queue is already full — we
        // check before admitting so a rejected prompt never orphans a DB row.
        // (The Mutex is dropped before the DB write below: never hold it across
        // an await — that would block event routing and every other session.)
        let orchestrator = {
            let mut sessions = self.sessions.lock().await;
            let sess = sessions
                .get_mut(session_id)
                .ok_or("session not found in coordinator")?;
            sess.last_activity = std::time::Instant::now();
            if sess.active_turn_cancel.is_some() && sess.queued.len() >= MAX_BACKLOG {
                return Err(format!(
                    "input backlog full ({} queued); retry once the session drains",
                    MAX_BACKLOG
                )
                .into());
            }
            sess.orchestrator.clone()
        };

        let s_id = session_id.to_string();
        let sessions_clone = self.sessions.clone();

        // Phase 2: admit the input (a DB write) with the lock released. No slot is
        // reserved yet, so a failed admission has nothing to roll back.
        let input_id = orchestrator.admit_input(&s_id, prompt, delivery).await?;

        // Phase 3: enqueue the input, then start a drain chain IFF none is running.
        // Setting `active_turn_cancel` here (under the lock, atomically with the
        // spawn decision) is the only place it is ever set — that, plus the drain
        // loop being the only place it is cleared, is what serializes the chain.
        // NOTE (accepted limitation, session.md L165 defers multi-caller admit):
        // two concurrent same-session `run_turn`s can push in an order that differs
        // from their `admitted_seq`. Single-client prompts serialize, both items
        // still run with no loss, so we accept this rather than re-query the DB at
        // settlement (which would reintroduce the lost-wakeup race).
        let drain = {
            let mut sessions = self.sessions.lock().await;
            let sess = sessions
                .get_mut(session_id)
                .ok_or("session not found in coordinator")?;
            sess.queued.push_back(input_id);
            if sess.active_turn_cancel.is_some() {
                None
            } else {
                let cancel_token = CancellationToken::new();
                sess.active_turn_cancel = Some(cancel_token.clone());
                // The queue was empty before our push (active_turn_cancel.is_none()
                // implies no drain, which implies an empty queue), so this pops the
                // very input we just admitted.
                let first = sess.queued.pop_front().expect("just pushed an input");
                Some((sess.orchestrator.clone(), cancel_token, first))
            }
        };

        let Some((orchestrator, first_cancel, first_input_id)) = drain else {
            return Ok(()); // Queued behind the running drain; it will be picked up.
        };

        self.tracker.spawn(async move {
            let mut current_input = first_input_id;
            let mut current_cancel = first_cancel;
            loop {
                if let Err(e) = orchestrator
                    .run_session_turn(&s_id, &current_input, current_cancel.clone())
                    .await
                {
                    tracing::error!("Error executing turn for session {}: {}", s_id, e);
                }

                // Settlement hand-off — MUST stay synchronous under ONE lock
                // acquisition with no `.await` between `pop_front` and the
                // `active_turn_cancel = None` clear. That atomicity is the proof
                // that no parallel drain spawns and no queued item is stranded.
                let mut s_map = sessions_clone.lock().await;
                let Some(sess) = s_map.get_mut(&s_id) else {
                    break; // Session evicted/torn down (e.g. shutdown).
                };
                match sess.queued.pop_front() {
                    Some(next) => {
                        // Mint a fresh token so a later abort targets exactly the
                        // next turn, and keep the slot owned across the hand-off.
                        let next_cancel = CancellationToken::new();
                        sess.active_turn_cancel = Some(next_cancel.clone());
                        sess.last_activity = std::time::Instant::now();
                        current_input = next;
                        current_cancel = next_cancel;
                    }
                    None => {
                        sess.active_turn_cancel = None;
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    /// Interrupt the active turn and stop the drain chain (session.md L163:
    /// "stops the current chain while preserving pending/unpromoted durable inbox
    /// rows for a later fresh wake"). We drop the in-memory queue so the drain
    /// settles to empty and stops — the dropped inputs remain unpromoted
    /// `session_input` rows (a later prompt is their fresh wake). Crucially we do
    /// NOT `take()` `active_turn_cancel`: the drain loop owns clearing it, so the
    /// slot stays owned and no parallel drain can spawn in the cancellation
    /// window. (`shutdown` is terminal and may take it.)
    pub async fn abort_turn(&self, session_id: &str) -> Result<(), Box<dyn std::error::Error>> {
        let mut sessions = self.sessions.lock().await;
        if let Some(sess) = sessions.get_mut(session_id) {
            sess.last_activity = std::time::Instant::now();
            sess.queued.clear();
            if let Some(cancel) = &sess.active_turn_cancel {
                cancel.cancel();
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
            sess.last_activity = std::time::Instant::now();
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
    use private_code_protocol::message::{ChatMessage, ContentBlock, Role};
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

        coord.shutdown(Duration::from_secs(5)).await;
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

    /// The reaper evicts an idle session but leaves a session with an active turn.
    #[tokio::test]
    async fn reaper_evicts_idle_sessions_but_not_active_ones() {
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        let ws = TempDir::new().unwrap();
        let ws_str = ws.path().to_str().unwrap();
        let project_id = uuid::Uuid::new_v4().to_string();
        create_project(&pool, &project_id, "t", ws_str)
            .await
            .unwrap();
        let cfg =
            serde_json::json!({"provider_id":"anthropic","model_id":"claude-opus-4-8"}).to_string();
        let idle_sid = uuid::Uuid::new_v4().to_string();
        let busy_sid = uuid::Uuid::new_v4().to_string();
        for sid in [&idle_sid, &busy_sid] {
            create_session(&pool, sid, &project_id, ws_str, ws_str, "t", "build", &cfg)
                .await
                .unwrap();
        }

        let coord = SessionCoordinator::new(
            pool,
            std::env::temp_dir(),
            Arc::new(OneShotTextProvider),
            Arc::new(private_code_tools::ToolRegistry::new()),
        );
        coord.start_reaper(Duration::from_millis(100), Duration::from_millis(30));

        let _rx1 = coord.get_or_create_session(&idle_sid).await.unwrap();
        let _rx2 = coord.get_or_create_session(&busy_sid).await.unwrap();
        // Mark busy_sid as having an active turn so the reaper must skip it.
        {
            let mut map = coord.sessions.lock().await;
            map.get_mut(&busy_sid).unwrap().active_turn_cancel = Some(CancellationToken::new());
        }

        // Past the idle TTL + several reaper ticks.
        tokio::time::sleep(Duration::from_millis(400)).await;

        {
            let map = coord.sessions.lock().await;
            assert!(!map.contains_key(&idle_sid), "idle session must be evicted");
            assert!(
                map.contains_key(&busy_sid),
                "a session with an active turn must NOT be evicted"
            );
        }

        coord.shutdown(Duration::from_secs(5)).await;
    }

    /// A text-only provider whose FIRST `stream_chat` call blocks until a oneshot
    /// fires. This pins turn 1 "in flight" so later prompts are forced to queue,
    /// making the FIFO drain deterministic instead of racing the fast happy path.
    struct GatedProvider {
        gate: std::sync::Mutex<Option<oneshot::Receiver<()>>>,
    }

    impl GatedProvider {
        fn new(rx: oneshot::Receiver<()>) -> Self {
            Self {
                gate: std::sync::Mutex::new(Some(rx)),
            }
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for GatedProvider {
        async fn stream_chat(
            &self,
            _model_id: &str,
            _system_prompt: Option<&str>,
            _max_tokens: u32,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError>
        {
            // Only the first call holds the receiver; later turns find None and
            // proceed immediately.
            let waiter = self.gate.lock().unwrap().take();
            if let Some(rx) = waiter {
                let _ = rx.await;
            }
            let evs = vec![
                Ok(ProviderEvent::TextDelta("ok".into())),
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

    /// Two prompts admitted while a turn is in flight are NOT rejected (the old
    /// code returned "a turn is already running") — they are queued and drained
    /// FIFO as separate activities once the active turn settles.
    #[tokio::test]
    async fn queued_prompts_run_fifo_as_separate_turns() {
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        let ws = TempDir::new().unwrap();
        let ws_str = ws.path().to_str().unwrap();
        let project_id = uuid::Uuid::new_v4().to_string();
        create_project(&pool, &project_id, "t", ws_str)
            .await
            .unwrap();
        let sid = uuid::Uuid::new_v4().to_string();
        let cfg =
            serde_json::json!({"provider_id":"anthropic","model_id":"claude-opus-4-8"}).to_string();
        create_session(&pool, &sid, &project_id, ws_str, ws_str, "t", "build", &cfg)
            .await
            .unwrap();

        let (gate_tx, gate_rx) = oneshot::channel();
        let coord = SessionCoordinator::new(
            pool.clone(),
            std::env::temp_dir(),
            Arc::new(GatedProvider::new(gate_rx)),
            Arc::new(private_code_tools::ToolRegistry::new()),
        );

        // Turn 1 starts and blocks in the provider; the next two are admitted
        // while it is in flight. They must succeed (no rejection) and queue.
        coord.run_turn(&sid, "first", "queue").await.unwrap();
        coord.run_turn(&sid, "second", "queue").await.unwrap();
        coord.run_turn(&sid, "third", "queue").await.unwrap();
        {
            let map = coord.sessions.lock().await;
            let sess = map.get(&sid).unwrap();
            assert!(sess.active_turn_cancel.is_some(), "a drain is running");
            assert_eq!(
                sess.queued.len(),
                2,
                "two inputs queued behind the active turn"
            );
        }

        // Release turn 1; the drain runs all three FIFO, one activity at a time.
        gate_tx.send(()).unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut user_texts: Vec<String> = Vec::new();
        while tokio::time::Instant::now() < deadline {
            let msgs = db::get_messages(&pool, &sid).await.unwrap();
            user_texts = msgs
                .iter()
                .filter_map(|m| serde_json::from_str::<ChatMessage>(&m.data).ok())
                .filter(|m| m.role == Role::User)
                .filter_map(|m| {
                    m.content.iter().find_map(|c| match c {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                })
                .collect();
            if user_texts.len() >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            user_texts,
            vec![
                "first".to_string(),
                "second".to_string(),
                "third".to_string()
            ],
            "queued prompts promote in FIFO order"
        );

        assert!(
            db::get_pending_inputs(&pool, &sid)
                .await
                .unwrap()
                .is_empty(),
            "inbox fully drained"
        );
        {
            let map = coord.sessions.lock().await;
            let sess = map.get(&sid).unwrap();
            assert!(
                sess.active_turn_cancel.is_none(),
                "slot released after drain"
            );
            assert!(sess.queued.is_empty());
        }
        coord.shutdown(Duration::from_secs(5)).await;
    }

    /// Once the backlog reaches `MAX_BACKLOG` behind an active turn, further
    /// admissions are rejected (and the rejected prompt is never queued).
    #[tokio::test]
    async fn backlog_cap_rejects_admission_when_queue_full() {
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        let ws = TempDir::new().unwrap();
        let ws_str = ws.path().to_str().unwrap();
        let project_id = uuid::Uuid::new_v4().to_string();
        create_project(&pool, &project_id, "t", ws_str)
            .await
            .unwrap();
        let sid = uuid::Uuid::new_v4().to_string();
        let cfg =
            serde_json::json!({"provider_id":"anthropic","model_id":"claude-opus-4-8"}).to_string();
        create_session(&pool, &sid, &project_id, ws_str, ws_str, "t", "build", &cfg)
            .await
            .unwrap();

        let (gate_tx, gate_rx) = oneshot::channel();
        let coord = SessionCoordinator::new(
            pool.clone(),
            std::env::temp_dir(),
            Arc::new(GatedProvider::new(gate_rx)),
            Arc::new(private_code_tools::ToolRegistry::new()),
        );

        // Turn 1 starts and blocks; fill the backlog to exactly MAX_BACKLOG.
        coord.run_turn(&sid, "active", "queue").await.unwrap();
        for i in 0..MAX_BACKLOG {
            coord
                .run_turn(&sid, &format!("q{i}"), "queue")
                .await
                .unwrap();
        }
        {
            let map = coord.sessions.lock().await;
            assert_eq!(
                map.get(&sid).unwrap().queued.len(),
                MAX_BACKLOG,
                "queue filled to cap"
            );
        }

        // The next admission must be rejected and must not enqueue.
        assert!(
            coord.run_turn(&sid, "overflow", "queue").await.is_err(),
            "run_turn rejects once the backlog is full"
        );
        {
            let map = coord.sessions.lock().await;
            assert_eq!(
                map.get(&sid).unwrap().queued.len(),
                MAX_BACKLOG,
                "the rejected prompt was not queued"
            );
        }

        // Releasing the gate lets the drain finish; shutdown cancels the rest.
        drop(gate_tx);
        coord.shutdown(Duration::from_secs(5)).await;
    }

    /// Abort interrupts the active turn AND stops the drain chain, while the
    /// queued input is preserved as an unpromoted inbox row (session.md L163:
    /// "stops the current chain while preserving pending/unpromoted durable inbox
    /// rows for a later fresh wake").
    #[tokio::test]
    async fn abort_stops_chain_and_preserves_queued_rows() {
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        let ws = TempDir::new().unwrap();
        let ws_str = ws.path().to_str().unwrap();
        let project_id = uuid::Uuid::new_v4().to_string();
        create_project(&pool, &project_id, "t", ws_str)
            .await
            .unwrap();
        let sid = uuid::Uuid::new_v4().to_string();
        let cfg =
            serde_json::json!({"provider_id":"anthropic","model_id":"claude-opus-4-8"}).to_string();
        create_session(&pool, &sid, &project_id, ws_str, ws_str, "t", "build", &cfg)
            .await
            .unwrap();

        // Turn 1 emits a write_file tool call (-> Ask), parking on the permission.
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
        let coord =
            SessionCoordinator::new(pool.clone(), std::env::temp_dir(), provider, Arc::new(reg));

        coord.run_turn(&sid, "first", "queue").await.unwrap();
        // Let turn 1 reach the (unanswered) permission park.
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Enqueue a second prompt behind the parked turn.
        coord.run_turn(&sid, "second", "queue").await.unwrap();
        {
            let map = coord.sessions.lock().await;
            let sess = map.get(&sid).unwrap();
            assert!(sess.active_turn_cancel.is_some());
            assert_eq!(
                sess.queued.len(),
                1,
                "second is queued behind the parked turn"
            );
        }

        // Abort: C5 makes the permission wait interruptible, and abort_turn clears
        // the in-memory queue. The drain then settles to an empty queue and stops.
        coord.abort_turn(&sid).await.unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            {
                let map = coord.sessions.lock().await;
                let sess = map.get(&sid).unwrap();
                if sess.active_turn_cancel.is_none() && sess.queued.is_empty() {
                    break;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the drain chain must stop after an abort"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // "second" was never promoted: it remains an unpromoted inbox row,
        // preserved for a later fresh wake. "first" was promoted before it parked.
        let pending = db::get_pending_inputs(&pool, &sid).await.unwrap();
        assert!(
            pending.iter().any(|i| i.prompt == "second"),
            "the aborted queue row is preserved unpromoted"
        );
        assert!(
            !pending.iter().any(|i| i.prompt == "first"),
            "the active turn's input was promoted before it parked"
        );
        coord.shutdown(Duration::from_secs(5)).await;
    }
}
