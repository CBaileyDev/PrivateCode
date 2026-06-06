use crate::checkpoint::{GitSnapshotEngine, Snapshot, TreeHash};
use crate::db;
use crate::orchestrator::Orchestrator;
use crate::permissions::{PermissionPrompt, PermissionReply};
use private_code_protocol::event::{ProtocolEvent, UsageStats};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Max number of inputs that may be queued behind an active turn before
/// `run_turn` rejects further admissions (session.md inbox backlog limit).
const MAX_BACKLOG: usize = 32;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub struct ActiveSession {
    pub session_id: String,
    pub orchestrator: Arc<Orchestrator>,
    pub event_tx: broadcast::Sender<ProtocolEvent>,
    pub history: Vec<ProtocolEvent>, // last 1000 durable events
    pub pending_permission: Option<(PermissionPrompt, oneshot::Sender<PermissionReply>)>,
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
    /// The default provider, used when a session's `provider_id` is not in
    /// `providers` below.
    pub provider: Arc<dyn private_code_providers::ModelProvider>,
    /// Named providers selected per-session by `model_config.provider_id`
    /// (e.g. "anthropic" → default, "nvidia" → OpenAI-compatible). Empty by
    /// default; the production daemon registers the extra ones it serves.
    pub providers: HashMap<String, Arc<dyn private_code_providers::ModelProvider>>,
    pub tool_registry: Arc<private_code_tools::ToolRegistry>,
    /// Tracks every spawned task (event/permission routers + turn drains + reaper)
    /// so a graceful shutdown can wait for them to finish under a bounded timeout.
    pub tracker: TaskTracker,
    /// Cancelled on shutdown to stop the reaper loop (which otherwise sleeps
    /// forever and would block tracker.wait()).
    pub shutdown_token: CancellationToken,
}

/// Whether an event enters `sess.history` (the 1000-cap durable replay buffer).
/// CRITICAL: every EPHEMERAL event variant must be excluded here, NOT just
/// filtered by `event_seq`. A multi-model turn emits ~N×hundreds of
/// `CandidateDelta`s; if they entered `sess.history` they would evict the real
/// durable events (`MessageCompleted`, `Error`, `ToolPermissionRequired`) within
/// a single fan-out, re-breaking lag recovery / cold reconnect. Candidate events
/// are pure UI streaming — only the synthesized `MessageCompleted` is durable.
fn is_durable_event(event: &ProtocolEvent) -> bool {
    !matches!(
        event,
        ProtocolEvent::MessageDelta { .. }
            | ProtocolEvent::CandidateStarted { .. }
            | ProtocolEvent::CandidateDelta { .. }
            | ProtocolEvent::CandidateCompleted { .. }
    )
}

/// The durable replay-cursor sequence for an event (0 for ephemeral/uncounted events).
pub fn event_seq(event: &ProtocolEvent) -> i64 {
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

/// Whether a live broadcast event should be forwarded to a client that already
/// replayed durable events up to `watermark`. Forwards everything with no replay
/// cursor (`event_seq == 0`: ephemeral deltas and any unsequenced durable event)
/// and everything strictly newer than the watermark; skips a durable event whose
/// seq was already in the replayed range — the exactly-once dedup. Keying on
/// `seq > 0` (not "is durable") means a durable-but-unsequenced event can never be
/// silently dropped.
pub fn should_forward(event: &ProtocolEvent, watermark: i64) -> bool {
    let seq = event_seq(event);
    seq == 0 || seq > watermark
}

/// The durable events in `history` strictly newer than `after_seq`, in order.
/// This is the recovery primitive both event forwarders (daemon WS + desktop
/// subscribe) use after a broadcast `Lagged`: a burst of ephemeral deltas can
/// overflow a slow-but-connected subscriber's 16384 ring view and evict
/// interleaved durable events (`Error`, `ToolPermissionRequired`) from THAT
/// receiver — but never from `sess.history`, which the event router populates
/// with durable events ONLY (deltas are excluded by `is_durable_event`) under
/// the same lock before broadcasting. So replaying `durable_after(history,
/// last_forwarded_seq)` on a `Lagged` recovers exactly the durable events the
/// subscriber missed, without resurrecting an evicted session (a `Lagged`
/// implies the session is still live; eviction drops the sender → `Closed`).
pub fn durable_after(history: &[ProtocolEvent], after_seq: i64) -> Vec<ProtocolEvent> {
    history
        .iter()
        .filter(|e| event_seq(e) > after_seq)
        .cloned()
        .collect()
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
            providers: HashMap::new(),
            tool_registry,
            tracker: TaskTracker::new(),
            shutdown_token: CancellationToken::new(),
        }
    }

    /// Register a named provider, selected per-session when a session's
    /// `model_config.provider_id` equals `id`. Call before wrapping in `Arc`.
    pub fn register_provider(
        &mut self,
        id: impl Into<String>,
        provider: Arc<dyn private_code_providers::ModelProvider>,
    ) {
        self.providers.insert(id.into(), provider);
    }

    /// Pick the provider for a session from its `model_config` JSON: the registered
    /// provider whose key matches `provider_id`, else the default. Unknown or
    /// unparseable configs fall back to the default rather than failing the turn.
    fn select_provider(
        &self,
        model_config: &str,
    ) -> Arc<dyn private_code_providers::ModelProvider> {
        let provider_id = serde_json::from_str::<serde_json::Value>(model_config)
            .ok()
            .and_then(|v| v["provider_id"].as_str().map(str::to_string));
        if let Some(id) = provider_id
            && let Some(p) = self.providers.get(&id)
        {
            return p.clone();
        }
        self.provider.clone()
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
        // Fast path: the session is already live. Touch activity and subscribe.
        {
            let mut sessions = self.sessions.lock().await;
            if let Some(sess) = sessions.get_mut(session_id) {
                sess.last_activity = std::time::Instant::now();
                return Ok(sess.event_tx.subscribe());
            }
        }

        // Slow path with the sessions Mutex RELEASED — never hold it across the DB
        // await below (that would block event routing and every other session's
        // operations). Verify the session exists in the DB.
        let session_row = match db::get_session(&self.pool, session_id).await? {
            Some(row) => row,
            None => return Err(format!("Session {} not found in database", session_id).into()),
        };

        // Build the channels, orchestrator, and ActiveSession speculatively. This is
        // all synchronous — no await, and CRUCIALLY no task spawn yet. If we lose the
        // double-check below we simply drop this; because nothing was spawned there
        // is nothing to tear down.
        let (permission_prompt_tx, permission_prompt_rx) = mpsc::channel(100);
        let (event_tx, event_rx) = mpsc::channel(4096);

        // Route to the session's configured provider (default if unregistered).
        let provider = self.select_provider(&session_row.model_config);
        let orchestrator = Arc::new(Orchestrator::new(
            self.pool.clone(),
            self.global_data_dir.clone(),
            provider,
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

        // Re-acquire the lock and double-check: another caller may have created the
        // session while our DB await ran. If so, return the winner's subscription
        // and drop our un-spawned build (its channels just close).
        let mut sessions = self.sessions.lock().await;
        if let Some(sess) = sessions.get_mut(session_id) {
            sess.last_activity = std::time::Instant::now();
            return Ok(sess.event_tx.subscribe());
        }

        // We won the race. Spawn the router tasks NOW (synchronous spawns — safe
        // under the lock since there is no await between lock and spawn), then
        // insert and hand back the subscription.
        let mut event_rx = event_rx;
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
        let mut permission_prompt_rx = permission_prompt_rx;
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

    /// Resolve a parked permission. `reply` is `"always"` | `"once"` (both grant)
    /// or anything else (deny, optionally carrying `feedback` the model sees).
    /// `"always"` additionally persists a saved rule for the session's project.
    ///
    /// The saved-rule write happens AFTER releasing the `sessions` lock — we
    /// capture the prompt's action/resource under the lock, send the decision,
    /// then do the DB write with the lock dropped (never await while holding
    /// `sessions`; that would block event routing and every other session). The
    /// save is best-effort: failing to persist the rule must not fail the grant
    /// (which unblocks the turn) — the user is simply asked again next time.
    pub async fn reply_permission(
        &self,
        session_id: &str,
        permission_id: &str,
        reply: &str,
        feedback: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let save_rule: Option<(String, String)> = {
            let mut sessions = self.sessions.lock().await;
            let Some(sess) = sessions.get_mut(session_id) else {
                return Err(
                    "No pending permission prompt matches the requested permission ID".into(),
                );
            };
            sess.last_activity = std::time::Instant::now();
            let matches = matches!(
                &sess.pending_permission,
                Some((p, _)) if p.permission_id == permission_id
            );
            if !matches {
                return Err(
                    "No pending permission prompt matches the requested permission ID".into(),
                );
            }
            let (prompt, resp_tx) = sess.pending_permission.take().unwrap();
            let decision = match reply {
                "always" | "once" => PermissionReply::Allow,
                _ => PermissionReply::Deny {
                    feedback: feedback.map(str::to_string),
                },
            };
            let _ = resp_tx.send(decision);
            if reply == "always" {
                let resource = prompt
                    .resources
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "*".to_string());
                Some((prompt.action.clone(), resource))
            } else {
                None
            }
        };

        // Persist the "always" rule outside the lock (best-effort).
        if let Some((action, resource)) = save_rule
            && let Ok(Some(sess_row)) = db::get_session(&self.pool, session_id).await
            && let Err(e) =
                db::save_permission(&self.pool, &sess_row.project_id, &action, &resource).await
        {
            tracing::warn!("failed to persist 'always' permission rule: {e}");
        }
        Ok(())
    }

    /// Replay durable events emitted since `after_seq` for a (re)connecting client.
    /// This carries live state (completed messages, tool outputs, checkpoints,
    /// usage, errors, permission prompts) — NOT message *content*, which the
    /// client reconciles from the shared SQLite DB. Returning only the in-memory
    /// durable log keeps replay simple and correct; the previous DB
    /// "reconstruction" emitted content-less events and double-counted, which
    /// broke attach.
    ///
    /// NOTE: this rebuilds the session if it isn't live (the initial subscribe
    /// path wants that). For *connected* lag recovery the forwarders instead read
    /// the live history directly via [`durable_after`] — they must NOT resurrect
    /// an evicted session. The replay is bounded: `Error` and
    /// `ToolPermissionRequired` have no DB content home, so once a session is
    /// idle-evicted they are gone — a cold reconnect after eviction cannot recover
    /// them (the deferred durable-event-log would close that residual gap).
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

    // ── Session-store operations (pure DB / git; no live-state mutation) ──────
    //
    // Hoisted here so the daemon HTTP routes and the desktop Tauri commands share
    // one implementation instead of duplicating the SQL/git logic across the two
    // front-ends.

    /// Compact a session's transcript by pruning all but roughly the most recent
    /// 10 messages. The summary already reaches the model via the system prefix
    /// (Phase-1 compaction), so this only bounds the stored row count.
    pub async fn compact_session(
        &self,
        session_id: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM session_message WHERE session_id = ?1 AND seq < \
             (SELECT MAX(seq) - 10 FROM session_message WHERE session_id = ?1)",
        )
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Revert the workspace to its most recent non-backup checkpoint, first
    /// snapshotting the current (dirty) state as a `revert_backup` so the change
    /// is reversible via [`unrevert_session`]. Returns the refreshed session row,
    /// or `Ok(None)` when there is no checkpoint to revert to (the caller maps
    /// that to a client error).
    pub async fn revert_session(
        &self,
        session_id: &str,
    ) -> Result<Option<db::SessionRow>, Box<dyn std::error::Error>> {
        let checkpoints = db::list_checkpoints(&self.pool, session_id).await?;
        let session = db::get_session(&self.pool, session_id)
            .await?
            .ok_or("Session not found")?;

        let Some(cp) = checkpoints.iter().find(|cp| cp.kind != "revert_backup") else {
            return Ok(None);
        };

        let engine = GitSnapshotEngine::new(
            session_id,
            std::path::Path::new(&session.workspace_path),
            &self.global_data_dir,
            true,
        );

        // Back up current dirty state so unrevert can restore it.
        if let Ok(Some(backup_hash)) = engine.track().await {
            let mut tx = self.pool.begin().await?;
            let b_msg_id = uuid::Uuid::new_v4().to_string();
            let _ = db::create_checkpoint(
                &mut tx,
                &uuid::Uuid::new_v4().to_string(),
                session_id,
                &b_msg_id,
                &backup_hash.0,
                "revert_backup",
                "revert_backup",
            )
            .await;
            let _ = tx.commit().await;
        }

        engine.restore(&TreeHash(cp.tree_hash.clone())).await?;

        // Append a system message + record the revert pointer atomically.
        let sys_msg_id = uuid::Uuid::new_v4().to_string();
        let sys_msg = private_code_protocol::message::ChatMessage {
            id: sys_msg_id.clone(),
            role: private_code_protocol::Role::System,
            content: vec![private_code_protocol::ContentBlock::Text {
                text: format!(
                    "Workspace successfully reverted to checkpoint: {}",
                    cp.tree_hash
                ),
            }],
            created_at: chrono::Utc::now().timestamp(),
        };
        let sys_json = serde_json::to_string(&sys_msg)?;
        let revert_state = serde_json::json!({
            "message_id": cp.message_id.clone(),
            "tree_hash": cp.tree_hash.clone(),
        })
        .to_string();

        let mut tx = self.pool.begin().await?;
        let seq = db::next_sequence(&mut tx, session_id).await?;
        db::append_message(&mut tx, &sys_msg_id, session_id, seq, "system", &sys_json).await?;
        sqlx::query("UPDATE session SET revert = ?1 WHERE id = ?2")
            .bind(&revert_state)
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        Ok(db::get_session(&self.pool, session_id).await?)
    }

    /// Undo the most recent [`revert_session`]: restore the `revert_backup`
    /// snapshot, drop it, clear the session's revert pointer, and append a system
    /// message. Returns the refreshed row, or `Ok(None)` if there is no backup.
    pub async fn unrevert_session(
        &self,
        session_id: &str,
    ) -> Result<Option<db::SessionRow>, Box<dyn std::error::Error>> {
        let checkpoints = db::list_checkpoints(&self.pool, session_id).await?;
        let session = db::get_session(&self.pool, session_id)
            .await?
            .ok_or("Session not found")?;

        let Some(cp) = checkpoints.iter().find(|cp| cp.kind == "revert_backup") else {
            return Ok(None);
        };

        let engine = GitSnapshotEngine::new(
            session_id,
            std::path::Path::new(&session.workspace_path),
            &self.global_data_dir,
            true,
        );
        engine.restore(&TreeHash(cp.tree_hash.clone())).await?;

        sqlx::query("DELETE FROM checkpoint WHERE id = ?1")
            .bind(&cp.id)
            .execute(&self.pool)
            .await?;
        sqlx::query("UPDATE session SET revert = NULL WHERE id = ?1")
            .bind(session_id)
            .execute(&self.pool)
            .await?;

        let sys_msg_id = uuid::Uuid::new_v4().to_string();
        let sys_msg = private_code_protocol::message::ChatMessage {
            id: sys_msg_id.clone(),
            role: private_code_protocol::Role::System,
            content: vec![private_code_protocol::ContentBlock::Text {
                text: "Workspace successfully un-reverted.".to_string(),
            }],
            created_at: chrono::Utc::now().timestamp(),
        };
        let sys_json = serde_json::to_string(&sys_msg)?;
        let mut tx = self.pool.begin().await?;
        let seq = db::next_sequence(&mut tx, session_id).await?;
        db::append_message(&mut tx, &sys_msg_id, session_id, seq, "system", &sys_json).await?;
        tx.commit().await?;

        Ok(db::get_session(&self.pool, session_id).await?)
    }

    /// Evict a session's live state IFF it is idle — mirroring the reaper's guard
    /// (no active turn, no pending permission, empty queue). Evicting a session
    /// with a running drain would be a correctness bug: a fresh access could
    /// rebuild the session and spawn a NEW drain while the old drain is still
    /// mid-turn; when the old drain reaches its settlement hand-off it re-looks
    /// up the session by id (it cannot tell "its" session from the recreate) and
    /// would either steal the new drain's queued input or clear the new drain's
    /// `active_turn_cancel`, letting a third drain spawn in parallel — the exact
    /// double-drain the single-token invariant exists to prevent. The check and
    /// the removal therefore happen under ONE lock acquisition with no await
    /// between them.
    ///
    /// Returns `true` when no stale live state remains afterward (evicted, or the
    /// session was never live), `false` when a busy session blocked eviction.
    pub async fn evict_session(&self, session_id: &str) -> bool {
        let mut sessions = self.sessions.lock().await;
        let idle = match sessions.get(session_id) {
            None => return true, // nothing cached → next access rebuilds fresh
            Some(sess) => {
                sess.active_turn_cancel.is_none()
                    && sess.pending_permission.is_none()
                    && sess.queued.is_empty()
            }
        };
        if idle {
            if let Some(sess) = sessions.remove(session_id) {
                sess.session_cancel.cancel(); // tear down its router tasks
            }
            true
        } else {
            false
        }
    }

    /// Switch a session's model. The new `model_config` is ALWAYS persisted. The
    /// orchestrator re-reads `model_id` per turn, so a model change within the
    /// same provider takes effect on the next turn with no eviction. A
    /// `provider_id` change, however, only takes effect once the live session is
    /// rebuilt (the provider is pinned at session build) — so we evict, but only
    /// when idle.
    ///
    /// Returns `Ok(true)` when the change is fully live, `Ok(false)` when a
    /// provider switch was persisted but a busy session deferred it (the caller
    /// should surface "applies after the current turn" and retry the eviction
    /// once the turn settles — e.g. on the turn-ended event).
    pub async fn set_model(
        &self,
        session_id: &str,
        model_config: &str,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let provider_of = |cfg: &str| -> Option<String> {
            serde_json::from_str::<serde_json::Value>(cfg)
                .ok()
                .and_then(|v| v["provider_id"].as_str().map(str::to_string))
        };
        let old_provider = db::get_session(&self.pool, session_id)
            .await?
            .map(|r| provider_of(&r.model_config))
            .ok_or("Session not found")?;
        let new_provider = provider_of(model_config);

        db::update_session_model(&self.pool, session_id, model_config).await?;

        // Same provider → no live state to refresh (model_id is per-turn).
        if new_provider == old_provider {
            return Ok(true);
        }
        // Provider changed → must rebuild; only possible when idle.
        Ok(self.evict_session(session_id).await)
    }

    /// Switch a session's agent. The orchestrator re-reads `agent_id` from the DB
    /// at the start of each turn, so this is a pure DB write — it takes effect on
    /// the next turn with no eviction (a turn already mid-flight keeps its agent).
    pub async fn set_agent(
        &self,
        session_id: &str,
        agent_id: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        db::update_session_agent(&self.pool, session_id, agent_id).await?;
        Ok(())
    }

    /// Force-remove a session's live state regardless of activity. Unlike
    /// [`evict_session`] (idle-guarded, for model/agent refresh) this is for
    /// DELETE: the DB row is being removed, so there is no recreate to race the
    /// torn-down drain. Cancels the active turn and the router tasks; a drain
    /// mid-settlement finds the session gone and breaks cleanly.
    pub async fn remove_session(&self, session_id: &str) {
        let mut sessions = self.sessions.lock().await;
        if let Some(sess) = sessions.remove(session_id) {
            if let Some(cancel) = &sess.active_turn_cancel {
                cancel.cancel();
            }
            sess.session_cancel.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect_db, create_project, create_session, run_migrations};
    use futures_util::stream::{BoxStream, StreamExt};
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

    /// `get_or_create_session` releases the sessions Mutex across its DB await, so
    /// concurrent callers for the SAME id race to create it. The relock +
    /// double-check must collapse them to exactly ONE live session (no duplicate
    /// insert, no orphan router tasks), and every caller must get a working
    /// subscription to that one session's broadcast.
    #[tokio::test]
    async fn concurrent_get_or_create_collapses_to_one_session() {
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

        let coord = Arc::new(SessionCoordinator::new(
            pool,
            std::env::temp_dir(),
            Arc::new(OneShotTextProvider),
            Arc::new(private_code_tools::ToolRegistry::new()),
        ));

        // Fire many concurrent get_or_create for the same id.
        let mut handles = Vec::new();
        for _ in 0..16 {
            let c = coord.clone();
            let s = sid.clone();
            // Map the (non-Send) Box<dyn Error> to String so the JoinHandle is Send.
            handles.push(tokio::spawn(async move {
                c.get_or_create_session(&s).await.map_err(|e| e.to_string())
            }));
        }
        let mut receivers = Vec::new();
        for h in handles {
            receivers.push(h.await.unwrap().expect("get_or_create must succeed"));
        }

        // Exactly one live session exists despite the race.
        {
            let map = coord.sessions.lock().await;
            assert_eq!(map.len(), 1, "the race must collapse to a single session");
        }

        // Every subscription is wired to that one session: a single turn's durable
        // MessageCompleted reaches all 16 receivers.
        coord.run_turn(&sid, "hi", "queue").await.unwrap();
        for mut rx in receivers {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            let mut got = false;
            while tokio::time::Instant::now() < deadline {
                match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                    Ok(Ok(ProtocolEvent::MessageCompleted { .. })) => {
                        got = true;
                        break;
                    }
                    Ok(Ok(_)) => continue,
                    Ok(Err(_)) => break,
                    Err(_) => continue,
                }
            }
            assert!(
                got,
                "each racing subscriber must receive the shared turn's completion"
            );
        }

        coord.shutdown(Duration::from_secs(5)).await;
    }

    /// Cold reconnect after eviction: once a turn has settled and the reaper has
    /// evicted the session, a reconnecting client gets an EMPTY in-memory replay
    /// (no stale/duplicate events, no panic) and reconstructs the conversation +
    /// usage purely from the DB. This is the evidence that "durable replay from
    /// DB" needs no event-log table — every durable event has a REST content home
    /// (messages → session_message, usage → session row), and the only transient
    /// events (permission prompt, error) can't be pending post-settlement.
    #[tokio::test]
    async fn cold_reconnect_after_eviction_reconstructs_from_db() {
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

        let coord = SessionCoordinator::new(
            pool.clone(),
            std::env::temp_dir(),
            Arc::new(OneShotTextProvider),
            Arc::new(private_code_tools::ToolRegistry::new()),
        );

        // Run a turn to completion (wait for the durable MessageCompleted).
        let mut rx = coord.get_or_create_session(&sid).await.unwrap();
        coord.run_turn(&sid, "hi there", "queue").await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                Ok(Ok(ProtocolEvent::MessageCompleted { .. })) => break,
                Ok(Ok(_)) => continue,
                _ => continue,
            }
        }

        // Evict the live session (what the reaper does post-settlement).
        {
            let mut map = coord.sessions.lock().await;
            if let Some(sess) = map.remove(&sid) {
                sess.session_cancel.cancel();
            }
            assert!(!map.contains_key(&sid), "session evicted");
        }

        // Cold reconnect: history rebuilds empty (no stale/duplicate replay).
        let replay = coord.get_history(&sid, 0).await.unwrap();
        assert!(
            replay.is_empty(),
            "post-eviction in-memory replay is empty, not stale; got {} events",
            replay.len()
        );

        // The conversation is fully reconstructable from the DB.
        let msgs = db::get_messages(&pool, &sid).await.unwrap();
        let texts: Vec<String> = msgs
            .iter()
            .filter_map(|m| serde_json::from_str::<ChatMessage>(&m.data).ok())
            .flat_map(|cm| cm.content)
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text),
                _ => None,
            })
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("hi there")),
            "the user prompt survives in the DB"
        );
        assert!(
            texts.iter().any(|t| t.contains("hello")),
            "the assistant reply survives in the DB"
        );

        coord.shutdown(Duration::from_secs(5)).await;
    }

    /// A provider whose `count_tokens` returns a fixed sentinel, so a test can
    /// tell which provider `select_provider` returned.
    struct SentinelProvider(usize);
    #[async_trait::async_trait]
    impl ModelProvider for SentinelProvider {
        async fn stream_chat(
            &self,
            _m: &str,
            _s: Option<&str>,
            _mt: u32,
            _msgs: &[ChatMessage],
            _t: &[serde_json::Value],
        ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError>
        {
            Ok(futures_util::stream::empty().boxed())
        }
        fn count_tokens(&self, _m: &str, _t: &str) -> usize {
            self.0
        }
    }

    /// `should_forward` is the exactly-once replay dedup: a client that replayed
    /// durable events 1..=3 must, on the live stream, see only seq>3 — but every
    /// seq-0 event (ephemeral deltas) always flows. This fails without the
    /// watermark (the overlapping seq 2,3 double-deliver).
    #[test]
    fn should_forward_dedups_replayed_range_but_passes_deltas() {
        let completed = |seq: i64| ProtocolEvent::MessageCompleted {
            session_id: "s".into(),
            seq,
            message_id: "m".into(),
            usage: UsageStats::default(),
        };
        let delta = ProtocolEvent::MessageDelta {
            session_id: "s".into(),
            delta: private_code_protocol::event::DeltaPayload::Text { text: "x".into() },
        };

        // Replayed [1,2,3] → watermark 3. Live overlap [2,3,4]: only 4 forwards.
        let watermark = 3;
        assert!(
            !should_forward(&completed(2), watermark),
            "2 already replayed"
        );
        assert!(
            !should_forward(&completed(3), watermark),
            "3 already replayed"
        );
        assert!(should_forward(&completed(4), watermark), "4 is new");
        // Ephemeral (seq 0) deltas always forward, regardless of watermark.
        assert!(should_forward(&delta, watermark));
        // With no replay (watermark 0), every durable event forwards.
        assert!(should_forward(&completed(1), 0));
    }

    /// `select_provider` routes by `model_config.provider_id`: a registered name
    /// wins, anything else (incl. unparseable) falls back to the default.
    #[tokio::test]
    async fn select_provider_routes_by_provider_id() {
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        let mut coord = SessionCoordinator::new(
            pool,
            std::env::temp_dir(),
            Arc::new(SentinelProvider(1)), // default
            Arc::new(private_code_tools::ToolRegistry::new()),
        );
        coord.register_provider("nvidia", Arc::new(SentinelProvider(2)));

        // A registered provider_id selects that provider.
        assert_eq!(
            coord
                .select_provider(r#"{"provider_id":"nvidia","model_id":"meta/llama"}"#)
                .count_tokens("m", "x"),
            2
        );
        // An unregistered provider_id falls back to the default.
        assert_eq!(
            coord
                .select_provider(r#"{"provider_id":"anthropic","model_id":"claude-opus-4-8"}"#)
                .count_tokens("m", "x"),
            1
        );
        // Unparseable config falls back to the default rather than failing.
        assert_eq!(coord.select_provider("not json").count_tokens("m", "x"), 1);
    }

    /// Hoisted session-store ops: a compact prune plus a full revert → unrevert
    /// round-trip over a REAL git workspace. None of these were covered by a test
    /// before they moved from the daemon route into the coordinator, so "the same
    /// tests pass" proves nothing about the move — this is the regression net.
    #[tokio::test]
    async fn session_ops_compact_and_revert_unrevert_round_trip() {
        use crate::checkpoint::GitSnapshotEngine;

        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        let data_dir = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        let ws_path = ws.path();
        let ws_str = ws_path.to_str().unwrap();

        // Init the workspace as a git repo with an initial commit (file = "v1").
        let repo = git2::Repository::init(ws_path).unwrap();
        {
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.name", "Test").unwrap();
            cfg.set_str("user.email", "test@example.com").unwrap();
        }
        let file = ws_path.join("f.txt");
        std::fs::write(&file, "v1\n").unwrap();
        {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("f.txt")).unwrap();
            index.write().unwrap();
            let oid = index.write_tree().unwrap();
            let tree = repo.find_tree(oid).unwrap();
            let sig = repo.signature().unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
                .unwrap();
        }

        let project_id = uuid::Uuid::new_v4().to_string();
        create_project(&pool, &project_id, "t", ws_str)
            .await
            .unwrap();
        let session_id = uuid::Uuid::new_v4().to_string();
        let model_cfg =
            serde_json::json!({"provider_id":"anthropic","model_id":"claude-opus-4-8"}).to_string();
        create_session(
            &pool,
            &session_id,
            &project_id,
            ws_str,
            ws_str,
            "t",
            "build",
            &model_cfg,
        )
        .await
        .unwrap();

        let coord = SessionCoordinator::new(
            pool.clone(),
            data_dir.path().to_path_buf(),
            Arc::new(OneShotTextProvider),
            Arc::new(private_code_tools::ToolRegistry::new()),
        );

        // Snapshot the v1 state and record it as a (non-backup) checkpoint.
        let engine = GitSnapshotEngine::new(&session_id, ws_path, data_dir.path(), true);
        let v1_hash = engine.track().await.unwrap().unwrap();
        {
            let mut tx = pool.begin().await.unwrap();
            db::create_checkpoint(
                &mut tx,
                &uuid::Uuid::new_v4().to_string(),
                &session_id,
                "m1",
                &v1_hash.0,
                "edit",
                "tool",
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
        }

        // Mutate the workspace to v2 (uncommitted).
        std::fs::write(&file, "v2\n").unwrap();

        // ── compact: append >10 messages, then prune to roughly the last 10 ──
        for i in 1..=15 {
            let mut tx = pool.begin().await.unwrap();
            let seq = db::next_sequence(&mut tx, &session_id).await.unwrap();
            db::append_message(
                &mut tx,
                &uuid::Uuid::new_v4().to_string(),
                &session_id,
                seq,
                "user",
                &format!("m{i}"),
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
        }
        let before = db::get_messages(&pool, &session_id).await.unwrap().len();
        coord.compact_session(&session_id).await.unwrap();
        let after = db::get_messages(&pool, &session_id).await.unwrap().len();
        assert!(
            after < before && after <= 11,
            "compact must prune old rows (before={before}, after={after})"
        );

        // ── revert: workspace returns to v1; pointer set; backup snapshot taken ──
        let reverted = coord
            .revert_session(&session_id)
            .await
            .unwrap()
            .expect("a checkpoint to revert to");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "v1\n",
            "revert restores the v1 snapshot"
        );
        assert!(
            reverted.revert.is_some(),
            "revert records the revert pointer"
        );
        let cps = db::list_checkpoints(&pool, &session_id).await.unwrap();
        assert!(
            cps.iter().any(|c| c.kind == "revert_backup"),
            "revert snapshots the current (v2) state as a backup"
        );

        // ── unrevert: workspace returns to v2; pointer cleared; backup dropped ──
        let unreverted = coord
            .unrevert_session(&session_id)
            .await
            .unwrap()
            .expect("a backup to unrevert");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "v2\n",
            "unrevert restores the pre-revert v2 state"
        );
        assert!(
            unreverted.revert.is_none(),
            "unrevert clears the revert pointer"
        );
        let cps = db::list_checkpoints(&pool, &session_id).await.unwrap();
        assert!(
            !cps.iter().any(|c| c.kind == "revert_backup"),
            "unrevert drops the backup checkpoint"
        );

        // ── nothing left to unrevert → Ok(None) ──
        assert!(coord.unrevert_session(&session_id).await.unwrap().is_none());
    }

    /// `evict_session` must mirror the reaper's idle guard: a session with an
    /// active turn, a parked permission, OR a queued input must NOT be evicted —
    /// evicting a live-drain session would let a recreate race the old drain's
    /// settlement and spawn a parallel drain. A non-live session counts as
    /// already-evicted.
    #[tokio::test]
    async fn evict_session_is_guarded_by_idleness() {
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

        let coord = SessionCoordinator::new(
            pool,
            std::env::temp_dir(),
            Arc::new(OneShotTextProvider),
            Arc::new(private_code_tools::ToolRegistry::new()),
        );

        // A non-live session counts as evicted (next access rebuilds fresh).
        assert!(
            coord.evict_session(&session_id).await,
            "non-live session counts as evicted"
        );

        // Make it live, then evict while idle.
        coord.get_or_create_session(&session_id).await.unwrap();
        assert!(coord.sessions.lock().await.contains_key(&session_id));
        assert!(
            coord.evict_session(&session_id).await,
            "idle live session evicts"
        );
        assert!(
            !coord.sessions.lock().await.contains_key(&session_id),
            "evicted session is gone"
        );

        // An ACTIVE TURN blocks eviction (the load-bearing case).
        coord.get_or_create_session(&session_id).await.unwrap();
        {
            let mut s = coord.sessions.lock().await;
            s.get_mut(&session_id).unwrap().active_turn_cancel = Some(CancellationToken::new());
        }
        assert!(
            !coord.evict_session(&session_id).await,
            "an active turn blocks eviction"
        );
        assert!(
            coord.sessions.lock().await.contains_key(&session_id),
            "busy session survives the evict attempt"
        );

        // A PARKED PERMISSION blocks eviction.
        {
            let mut s = coord.sessions.lock().await;
            let sess = s.get_mut(&session_id).unwrap();
            sess.active_turn_cancel = None;
            let (tx, _rx) = oneshot::channel::<PermissionReply>();
            sess.pending_permission = Some((
                PermissionPrompt {
                    permission_id: "p1".into(),
                    tool_name: "write_file".into(),
                    action: "write".into(),
                    resources: vec!["*".into()],
                    preview: String::new(),
                },
                tx,
            ));
        }
        assert!(
            !coord.evict_session(&session_id).await,
            "a parked permission blocks eviction"
        );

        // A QUEUED input blocks eviction.
        {
            let mut s = coord.sessions.lock().await;
            let sess = s.get_mut(&session_id).unwrap();
            sess.pending_permission = None;
            sess.queued.push_back("queued-input".into());
        }
        assert!(
            !coord.evict_session(&session_id).await,
            "a queued input blocks eviction"
        );

        // Fully idle again → evicts.
        {
            let mut s = coord.sessions.lock().await;
            s.get_mut(&session_id).unwrap().queued.clear();
        }
        assert!(
            coord.evict_session(&session_id).await,
            "idle again → evicts"
        );
        assert!(!coord.sessions.lock().await.contains_key(&session_id));
    }

    /// `set_model` always persists the new config, but only evicts the live
    /// session when the PROVIDER changes (the provider is pinned at session
    /// build; `model_id` is re-read per turn). A provider change against a busy
    /// session persists the config but defers the eviction → `Ok(false)`.
    #[tokio::test]
    async fn set_model_persists_always_and_evicts_only_on_provider_change() {
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

        let mut coord = SessionCoordinator::new(
            pool.clone(),
            std::env::temp_dir(),
            Arc::new(SentinelProvider(1)),
            Arc::new(private_code_tools::ToolRegistry::new()),
        );
        coord.register_provider("nvidia", Arc::new(SentinelProvider(2)));

        coord.get_or_create_session(&session_id).await.unwrap();

        // Same provider, new model_id → stays live (model_id is per-turn); DB updated.
        let same_provider =
            serde_json::json!({"provider_id":"anthropic","model_id":"claude-haiku-4-5"})
                .to_string();
        assert!(
            coord.set_model(&session_id, &same_provider).await.unwrap(),
            "same-provider change is immediately live"
        );
        assert!(
            coord.sessions.lock().await.contains_key(&session_id),
            "same-provider change must NOT evict"
        );
        assert_eq!(
            db::get_session(&pool, &session_id)
                .await
                .unwrap()
                .unwrap()
                .model_config,
            same_provider
        );

        // Provider change while idle → evicts; next access rebuilds with nvidia.
        let new_provider =
            serde_json::json!({"provider_id":"nvidia","model_id":"meta/llama-3.3-70b-instruct"})
                .to_string();
        assert!(
            coord.set_model(&session_id, &new_provider).await.unwrap(),
            "idle provider change is live"
        );
        assert!(
            !coord.sessions.lock().await.contains_key(&session_id),
            "provider change evicts the live session"
        );
        assert_eq!(
            db::get_session(&pool, &session_id)
                .await
                .unwrap()
                .unwrap()
                .model_config,
            new_provider
        );

        // Provider change while BUSY → persisted but eviction deferred (Ok(false)).
        coord.get_or_create_session(&session_id).await.unwrap();
        {
            let mut s = coord.sessions.lock().await;
            s.get_mut(&session_id).unwrap().active_turn_cancel = Some(CancellationToken::new());
        }
        let back =
            serde_json::json!({"provider_id":"anthropic","model_id":"claude-opus-4-8"}).to_string();
        assert!(
            !coord.set_model(&session_id, &back).await.unwrap(),
            "busy provider change defers eviction"
        );
        assert!(
            coord.sessions.lock().await.contains_key(&session_id),
            "busy session is not evicted"
        );
        assert_eq!(
            db::get_session(&pool, &session_id)
                .await
                .unwrap()
                .unwrap()
                .model_config,
            back,
            "model_config is persisted even when the eviction is deferred"
        );
    }

    /// `remove_session` (DELETE path) force-removes live state even when a turn
    /// is active — unlike the idle-guarded `evict_session`.
    #[tokio::test]
    async fn remove_session_force_removes_even_when_busy() {
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

        let coord = SessionCoordinator::new(
            pool,
            std::env::temp_dir(),
            Arc::new(OneShotTextProvider),
            Arc::new(private_code_tools::ToolRegistry::new()),
        );

        coord.get_or_create_session(&session_id).await.unwrap();
        {
            let mut s = coord.sessions.lock().await;
            s.get_mut(&session_id).unwrap().active_turn_cancel = Some(CancellationToken::new());
        }
        // evict refuses (busy); remove forces it regardless.
        assert!(!coord.evict_session(&session_id).await);
        coord.remove_session(&session_id).await;
        assert!(
            !coord.sessions.lock().await.contains_key(&session_id),
            "remove_session force-removes a busy session"
        );
    }

    /// `set_agent` is a raw `sqlx::query` (no compile-time column check), so a
    /// column-name typo would only surface at runtime — pin the write.
    #[tokio::test]
    async fn set_agent_persists_to_the_db() {
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

        let coord = SessionCoordinator::new(
            pool.clone(),
            std::env::temp_dir(),
            Arc::new(OneShotTextProvider),
            Arc::new(private_code_tools::ToolRegistry::new()),
        );

        coord.set_agent(&session_id, "plan").await.unwrap();
        assert_eq!(
            db::get_session(&pool, &session_id)
                .await
                .unwrap()
                .unwrap()
                .agent_id,
            "plan",
            "set_agent must persist the new agent_id"
        );
    }

    /// The `"always"` branch of `reply_permission` persists a saved rule for the
    /// session's project. This is the lock-safe save we folded in + reordered in
    /// C10; only the grant/deny path was covered before, the DB write was dark.
    #[tokio::test]
    async fn reply_permission_always_persists_a_saved_rule() {
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

        let coord = SessionCoordinator::new(
            pool.clone(),
            std::env::temp_dir(),
            Arc::new(OneShotTextProvider),
            Arc::new(private_code_tools::ToolRegistry::new()),
        );
        coord.get_or_create_session(&session_id).await.unwrap();

        // Park a permission prompt for a write to `/foo`.
        let _rx = {
            let mut s = coord.sessions.lock().await;
            let (tx, rx) = oneshot::channel::<PermissionReply>();
            s.get_mut(&session_id).unwrap().pending_permission = Some((
                PermissionPrompt {
                    permission_id: "perm-1".into(),
                    tool_name: "write_file".into(),
                    action: "write".into(),
                    resources: vec!["/foo".into()],
                    preview: String::new(),
                },
                tx,
            ));
            rx
        };

        coord
            .reply_permission(&session_id, "perm-1", "always", None)
            .await
            .unwrap();

        let saved = db::get_saved_permissions(&pool, &project_id).await.unwrap();
        assert_eq!(saved.len(), 1, "'always' must persist exactly one rule");
        assert_eq!(saved[0].action, "write");
        assert_eq!(saved[0].resource, "/foo");

        // The parked oneshot received the grant.
        assert_eq!(_rx.await.unwrap(), PermissionReply::Allow);
    }

    /// The recovery primitive both event forwarders depend on after a broadcast
    /// `Lagged`: `durable_after` returns the durable events strictly newer than the
    /// watermark and NEVER an ephemeral delta (which has no recovery need).
    #[test]
    fn durable_after_filters_by_seq_and_skips_ephemeral() {
        use private_code_protocol::event::DeltaPayload;
        let history = vec![
            ProtocolEvent::MessageDelta {
                session_id: "s".into(),
                delta: DeltaPayload::Text { text: "tok".into() },
            },
            // A fan-out candidate delta is ephemeral too — it must never be
            // recovered (event_seq == 0), exactly like a MessageDelta.
            ProtocolEvent::CandidateDelta {
                session_id: "s".into(),
                candidate_index: 0,
                delta: DeltaPayload::Text {
                    text: "cand".into(),
                },
            },
            ProtocolEvent::ToolPermissionRequired {
                session_id: "s".into(),
                seq: 3,
                permission_id: "p".into(),
                tool_name: "write_file".into(),
                action: "write".into(),
                resources: vec!["/f".into()],
                preview: String::new(),
            },
            ProtocolEvent::Error {
                session_id: "s".into(),
                seq: 5,
                code: "provider_error".into(),
                message: "boom".into(),
                retryable: true,
            },
            ProtocolEvent::MessageCompleted {
                session_id: "s".into(),
                seq: 7,
                message_id: "m".into(),
                usage: UsageStats::default(),
            },
        ];

        // From watermark 4: recover seq 5 and 7; the already-seen seq-3 permission
        // and the ephemeral delta (seq 0) are excluded.
        let recovered = durable_after(&history, 4);
        assert_eq!(recovered.len(), 2);
        assert!(matches!(recovered[0], ProtocolEvent::Error { seq: 5, .. }));
        assert!(matches!(
            recovered[1],
            ProtocolEvent::MessageCompleted { seq: 7, .. }
        ));

        // From watermark 0: all three durables recover; the deltas are still skipped.
        let from_zero = durable_after(&history, 0);
        assert_eq!(from_zero.len(), 3);
        assert!(
            !from_zero.iter().any(|e| matches!(
                e,
                ProtocolEvent::MessageDelta { .. } | ProtocolEvent::CandidateDelta { .. }
            )),
            "ephemeral deltas (message and candidate) are never recovered"
        );
    }

    /// The load-bearing invariant for multi-model orchestration: ALL three
    /// candidate event variants are EPHEMERAL — excluded from `sess.history` by
    /// `is_durable_event`, exactly like `MessageDelta`. If any one of them were
    /// durable, a single fan-out turn's hundreds of candidate deltas would evict
    /// the real durable events (`MessageCompleted`, `Error`,
    /// `ToolPermissionRequired`) from the 1000-cap buffer, silently regressing
    /// lag recovery and cold reconnect. This test fails closed the moment a new
    /// candidate variant is added without excluding it.
    #[test]
    fn candidate_events_are_ephemeral_only_synthesis_persists() {
        use private_code_protocol::event::DeltaPayload;
        let started = ProtocolEvent::CandidateStarted {
            session_id: "s".into(),
            candidate_index: 0,
            model_id: "anthropic/claude-opus-4-8".into(),
        };
        let delta = ProtocolEvent::CandidateDelta {
            session_id: "s".into(),
            candidate_index: 0,
            delta: DeltaPayload::Text { text: "x".into() },
        };
        let completed = ProtocolEvent::CandidateCompleted {
            session_id: "s".into(),
            candidate_index: 0,
            usage: UsageStats::default(),
            finish_reason: Some("end_turn".into()),
            error: None,
        };
        for ev in [&started, &delta, &completed] {
            assert!(
                !is_durable_event(ev),
                "candidate events must be ephemeral (excluded from sess.history)"
            );
            // Ephemeral ⇒ no replay cursor ⇒ always-forward, never recovered.
            assert_eq!(event_seq(ev), 0, "candidate events carry no durable seq");
            assert!(
                should_forward(ev, 9999),
                "ephemeral candidate events always forward to live subscribers"
            );
        }

        // The synthesized answer, by contrast, IS durable and replayable.
        let synth = ProtocolEvent::MessageCompleted {
            session_id: "s".into(),
            seq: 1,
            message_id: "m".into(),
            usage: UsageStats::default(),
        };
        assert!(
            is_durable_event(&synth),
            "the synthesized MessageCompleted is the one durable record of the turn"
        );
    }

    /// A provider whose `stream_chat` fails immediately — drives the orchestrator's
    /// `provider_error` path so a durable `Error` lands in `sess.history`.
    struct ErroringProvider;

    #[async_trait::async_trait]
    impl ModelProvider for ErroringProvider {
        async fn stream_chat(
            &self,
            _model_id: &str,
            _system_prompt: Option<&str>,
            _max_tokens: u32,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError>
        {
            Err(ProviderError::Other("simulated provider failure".into()))
        }
        fn count_tokens(&self, _m: &str, t: &str) -> usize {
            t.len() / 4
        }
    }

    /// The two in-memory-only durable events a connected-client lag could drop —
    /// `ToolPermissionRequired` and `Error` — DO land in `sess.history` with a real
    /// seq, so `durable_after(history, last_forwarded_seq)` recovers them. This is
    /// the foundation of the forwarders' lag-recovery: without it a lagged client
    /// would silently lose a permission prompt (turn hangs) or an error (spinner
    /// wedged), since neither event has a DB content home to reconcile from.
    #[tokio::test]
    async fn lagged_recovery_history_carries_permission_and_error() {
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

        // ── Session A: a write_file tool call parks on ToolPermissionRequired ──
        let perm_sid = uuid::Uuid::new_v4().to_string();
        create_session(
            &pool,
            &perm_sid,
            &project_id,
            ws_str,
            ws_str,
            "t",
            "build",
            &cfg,
        )
        .await
        .unwrap();
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
        coord.run_turn(&perm_sid, "hi", "steer").await.unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            {
                let map = coord.sessions.lock().await;
                if let Some(sess) = map.get(&perm_sid) {
                    let recoverable = durable_after(&sess.history, 0);
                    if let Some(ProtocolEvent::ToolPermissionRequired { seq, .. }) = recoverable
                        .iter()
                        .find(|e| matches!(e, ProtocolEvent::ToolPermissionRequired { .. }))
                    {
                        assert!(*seq > 0, "ToolPermissionRequired must carry a durable seq");
                        break;
                    }
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "permission prompt never reached the in-memory history"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        coord.shutdown(Duration::from_secs(5)).await;

        // ── Session B: the turn errors before any output ──
        let err_sid = uuid::Uuid::new_v4().to_string();
        create_session(
            &pool,
            &err_sid,
            &project_id,
            ws_str,
            ws_str,
            "t",
            "build",
            &cfg,
        )
        .await
        .unwrap();
        let coord2 = SessionCoordinator::new(
            pool.clone(),
            std::env::temp_dir(),
            Arc::new(ErroringProvider),
            Arc::new(private_code_tools::ToolRegistry::new()),
        );
        coord2.run_turn(&err_sid, "hi", "steer").await.unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            {
                let map = coord2.sessions.lock().await;
                if let Some(sess) = map.get(&err_sid) {
                    let recoverable = durable_after(&sess.history, 0);
                    if let Some(ProtocolEvent::Error { seq, .. }) = recoverable
                        .iter()
                        .find(|e| matches!(e, ProtocolEvent::Error { .. }))
                    {
                        assert!(*seq > 0, "Error must carry a durable seq");
                        break;
                    }
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "error never reached the in-memory history"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        coord2.shutdown(Duration::from_secs(5)).await;
    }
}
