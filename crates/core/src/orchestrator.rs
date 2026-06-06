use crate::checkpoint::{GitSnapshotEngine, Snapshot};
use crate::config::{AppConfig, DEFAULT_MODEL_ID};
use crate::context::{Reconcile, SystemContextRegistry};
use crate::db::{self};
use crate::permissions::{self, PermissionDecision, PermissionPrompt, PermissionRule};
use private_code_protocol::event::{DeltaPayload, ProtocolEvent, UsageStats};
use private_code_protocol::message::{ChatMessage, ContentBlock, Role, ToolResultContent};
use private_code_providers::provider::{ModelProvider, ProviderError, ProviderEvent};
use private_code_tools::tool::{ToolContext, ToolRegistry};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

pub struct Orchestrator {
    pub pool: SqlitePool,
    pub global_data_dir: PathBuf,
    pub provider: Arc<dyn ModelProvider>,
    pub context_registry: SystemContextRegistry,
    pub tool_registry: Arc<ToolRegistry>,
    pub permission_prompt_tx: mpsc::Sender<(PermissionPrompt, oneshot::Sender<PermissionDecision>)>,
    pub event_tx: mpsc::Sender<ProtocolEvent>,
}

/// A content block assembled from the provider stream, preserving arrival
/// order so the persisted assistant message reflects the real interleaving of
/// text / thinking / tool-use blocks (instead of a nondeterministic
/// HashMap-sourced reordering).
enum PartialBlock {
    Text(String),
    Reasoning {
        text: String,
        signature: Option<String>,
    },
    Tool {
        id: String,
        name: String,
        input: String,
    },
}

/// Durable boundary marker for a compaction. Stored as a `compaction`-type
/// session_message; its `summary` is prepended as a System message and messages
/// with `seq <= compacted_through_seq` are dropped from the provider request.
/// The summary lives HERE (not in the epoch baseline) so the cached source
/// baseline stays warm and a later source-driven baseline rebuild can't wipe it.
#[derive(serde::Serialize, serde::Deserialize)]
struct CompactionMarker {
    compacted_through_seq: i64,
    summary: String,
}

/// True if a provider 400 looks like a context-length overflow (so compaction
/// might recover it). Distinct from the streaming `model_context_window_exceeded`
/// finish_reason, which is a SUCCESSFUL (if truncated) turn and never compacts.
fn is_context_overflow(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("too long") || m.contains("context length") || m.contains("context window")
}

/// A clean turn boundary to cut at: a real user prompt (not a tool_result
/// carrier), so every retained assistant tool_use keeps its following tool_result.
fn is_clean_turn_start(m: &ChatMessage) -> bool {
    m.role == Role::User
        && !m
            .content
            .iter()
            .any(|c| matches!(c, ContentBlock::ToolResult { .. }))
}

/// Flatten a content block to text for token estimation / summarization.
fn block_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text { text } => text.clone(),
        ContentBlock::Reasoning { reasoning, .. } => reasoning.clone(),
        ContentBlock::ToolUse { input, .. } => input.to_string(),
        ContentBlock::ToolResult { content, .. } => match content {
            ToolResultContent::Text(t) => t.clone(),
            ToolResultContent::Json(v) => v.to_string(),
        },
    }
}

/// Deterministic, non-LLM rolling summary (Phase-1 stopgap). Cumulative: keeps
/// the prior summary body and appends role-tagged snippets of the newly dropped
/// messages. When capping, the OLDEST body content is dropped and the most-recent
/// is kept — the just-folded messages are at the tail, so head-truncating would
/// silently discard exactly the content this compaction was meant to preserve.
fn build_summary(prev: &str, dropped: &[&ChatMessage]) -> String {
    const HEADER: &str =
        "[Earlier conversation was auto-compacted to save context. Summary follows.]";
    const CAP: usize = 6000;

    // Reuse the prior summary's body (strip its header so we don't repeat it).
    let prev_body = prev
        .strip_prefix(HEADER)
        .map(|r| r.trim_start_matches('\n'))
        .unwrap_or(prev);

    let mut body = String::new();
    if !prev_body.is_empty() {
        body.push_str(prev_body);
        body.push('\n');
    }
    for m in dropped {
        let role = match m.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::System => "System",
        };
        for b in &m.content {
            let t = block_text(b);
            if t.trim().is_empty() {
                continue;
            }
            let snippet: String = t.chars().take(300).collect();
            body.push_str(role);
            body.push_str(": ");
            body.push_str(snippet.trim());
            body.push('\n');
        }
    }

    // Keep the most-recent CAP chars (drop the oldest), so newly-folded content
    // survives repeated compactions.
    let len = body.chars().count();
    if len > CAP {
        let tail: String = body.chars().skip(len - CAP).collect();
        body = format!("…(older summary truncated)\n{tail}");
    }

    format!("{HEADER}\n{body}")
}

impl Orchestrator {
    pub fn new(
        pool: SqlitePool,
        global_data_dir: PathBuf,
        provider: Arc<dyn ModelProvider>,
        tool_registry: Arc<ToolRegistry>,
        permission_prompt_tx: mpsc::Sender<(PermissionPrompt, oneshot::Sender<PermissionDecision>)>,
        event_tx: mpsc::Sender<ProtocolEvent>,
    ) -> Self {
        Self {
            pool,
            global_data_dir,
            provider,
            context_registry: SystemContextRegistry::new(),
            tool_registry,
            permission_prompt_tx,
            event_tx,
        }
    }

    pub async fn admit_input(
        &self,
        session_id: &str,
        prompt: &str,
        delivery: &str,
    ) -> Result<String, sqlx::Error> {
        let input_id = Uuid::new_v4().to_string();
        let mut tx = self.pool.begin().await?;

        let seq = db::next_sequence(&mut tx, session_id).await?;
        db::admit_session_input(&mut tx, &input_id, session_id, prompt, delivery, seq).await?;

        tx.commit().await?;
        Ok(input_id)
    }

    /// At a safe provider-turn boundary, coalesce every pending `delivery="steer"`
    /// input into the visible history as a chronological user message, in durable
    /// admission order (session.md L153: steers "promote at the next safe
    /// provider-turn boundary, including continuation inside the current drain";
    /// L165: "coalesces pending steers in durable admission order"). Returns the
    /// number promoted so the caller knows whether to refetch messages.
    ///
    /// This is the ONLY place steers fold into a running activity. It is idempotent
    /// against the coordinator's queue: the coordinator also enqueues every input
    /// id, but a steer promoted here is no longer pending, so when its queued copy
    /// is later popped `run_session_turn` finds nothing to promote and no-ops. A
    /// steer that arrives after this turn's last boundary stays pending and simply
    /// opens the next activity when the queue drains it — never lost, never double-run.
    ///
    /// `chain_watermark` is the opening input's `admitted_seq`: only steers admitted
    /// strictly after it fold in. That is the ownership-chain boundary (session.md
    /// L163) — a steer abandoned by an abort keeps a seq below any later fresh
    /// prompt, so a subsequent unrelated activity never resurrects it. Those orphan
    /// rows are preserved-but-skipped by design (recovery is deferred future work,
    /// not a leak).
    async fn promote_pending_steers(
        &self,
        session_id: &str,
        chain_watermark: i64,
    ) -> Result<usize, Box<dyn std::error::Error>> {
        let pending = db::get_pending_inputs(&self.pool, session_id).await?;
        // `get_pending_inputs` is already ordered by `admitted_seq` (durable
        // admission order); keep only steers admitted within this chain. Queued
        // inputs open their own activities and must NOT be folded in here.
        let steers: Vec<_> = pending
            .into_iter()
            .filter(|i| i.delivery == "steer" && i.admitted_seq > chain_watermark)
            .collect();
        if steers.is_empty() {
            return Ok(0);
        }

        let mut tx = self.pool.begin().await?;
        for input in &steers {
            let seq = db::next_sequence(&mut tx, session_id).await?;
            db::promote_session_input(&mut tx, &input.id, seq).await?;

            let msg_id = Uuid::new_v4().to_string();
            let msg = ChatMessage {
                id: msg_id.clone(),
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: input.prompt.clone(),
                }],
                created_at: chrono::Utc::now().timestamp(),
            };
            let msg_json = serde_json::to_string(&msg)?;
            db::append_message(&mut tx, &msg_id, session_id, seq, "user", &msg_json).await?;
        }
        tx.commit().await?;
        Ok(steers.len())
    }

    /// Ephemeral deltas: drop on a full channel rather than block the turn
    /// (the durable `message.completed` carries the final text either way).
    /// Durable events are sent with `.send().await` directly so they're never lost.
    fn emit_delta(&self, ev: ProtocolEvent) {
        let _ = self.event_tx.try_send(ev);
    }

    pub async fn run_session_turn(
        &self,
        session_id: &str,
        input_id: &str,
        cancel: CancellationToken,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // 1. Fetch Session
        let session = match db::get_session(&self.pool, session_id).await? {
            Some(s) => s,
            None => return Err(format!("Session {} not found", session_id).into()),
        };

        let workspace_path = Path::new(&session.workspace_path);
        let active_dir = Path::new(&session.active_directory);

        // 2. Interrupt recovery: Fail any running tools from previous crash
        self.recover_interrupted_tools(session_id).await?;

        // 3. Promote input to visible user message
        let mut tx = self.pool.begin().await?;
        let pending_inputs = db::get_pending_inputs(&self.pool, session_id).await?;
        let input_row = match pending_inputs.iter().find(|i| i.id == input_id) {
            Some(row) => row,
            None => {
                tx.rollback().await?;
                return Ok(()); // Already processed
            }
        };
        // This chain's ownership boundary: only steers admitted strictly after the
        // opening input fold into it (see `promote_pending_steers`).
        let chain_watermark = input_row.admitted_seq;

        let user_msg_seq = db::next_sequence(&mut tx, session_id).await?;
        db::promote_session_input(&mut tx, &input_row.id, user_msg_seq).await?;

        let user_msg_id = Uuid::new_v4().to_string();
        let user_msg = ChatMessage {
            id: user_msg_id.clone(),
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: input_row.prompt.clone(),
            }],
            created_at: chrono::Utc::now().timestamp(),
        };
        let user_msg_json = serde_json::to_string(&user_msg)?;
        db::append_message(
            &mut tx,
            &user_msg_id,
            session_id,
            user_msg_seq,
            "user",
            &user_msg_json,
        )
        .await?;
        tx.commit().await?;

        // 4. Context reconciliation
        let active_epoch = db::get_context_epoch(&self.pool, session_id).await?;
        let had_epoch = active_epoch.is_some();
        let revision = active_epoch.as_ref().map(|e| e.revision).unwrap_or(0);

        let recon = self
            .context_registry
            .reconcile(&self.pool, session_id, workspace_path, active_dir)
            .await?;
        match recon {
            Reconcile::ReplacementReady { baseline, snapshot } => {
                // New baseline. Stored in the epoch row (NOT appended to history) so
                // it is sent as the CACHED top-level `system` block every turn — the
                // whole point of the epoch. On the first turn there is no row yet, so
                // INSERT; otherwise CAS-REPLACE on the revision.
                let mut tx = self.pool.begin().await?;
                let snap_json = serde_json::to_string(&snapshot)?;
                let bseq = db::next_sequence(&mut tx, session_id).await?;
                let cas_ok = if had_epoch {
                    db::replace_context_epoch(
                        &mut tx,
                        session_id,
                        &session.agent_id,
                        &baseline,
                        &snap_json,
                        bseq,
                        revision,
                    )
                    .await?
                } else {
                    db::insert_context_epoch(
                        &mut tx,
                        session_id,
                        &session.agent_id,
                        &baseline,
                        &snap_json,
                        bseq,
                    )
                    .await?
                };
                if cas_ok {
                    tx.commit().await?;
                } else {
                    // RevisionMismatch: another writer advanced/replaced the epoch
                    // concurrently. Roll back rather than proceed as if it stuck;
                    // the current baseline is reloaded below and the reconcile
                    // retries at the next turn boundary.
                    tx.rollback().await?;
                    warn!(
                        "context epoch CAS revision mismatch on replacement (session {}); keeping existing baseline",
                        session_id
                    );
                }
            }
            Reconcile::Updated { text, snapshot } => {
                // Mid-conversation delta: a durable {role:"system"} message in history
                // plus a snapshot advance (baseline preserved → prompt cache stays warm).
                let mut tx = self.pool.begin().await?;
                let seq = db::next_sequence(&mut tx, session_id).await?;
                let sys_msg_id = Uuid::new_v4().to_string();
                let sys_msg = ChatMessage {
                    id: sys_msg_id.clone(),
                    role: Role::System,
                    content: vec![ContentBlock::Text { text: text.clone() }],
                    created_at: chrono::Utc::now().timestamp(),
                };
                let sys_json = serde_json::to_string(&sys_msg)?;
                db::append_message(&mut tx, &sys_msg_id, session_id, seq, "system", &sys_json)
                    .await?;

                let snap_json = serde_json::to_string(&snapshot)?;
                let cas_ok =
                    db::advance_context_epoch(&mut tx, session_id, &snap_json, revision).await?;
                if cas_ok {
                    tx.commit().await?;
                } else {
                    // RevisionMismatch: drop this update (incl. the system message)
                    // rather than commit a half-applied delta; the reconcile retries
                    // next turn against the reloaded epoch.
                    tx.rollback().await?;
                    warn!(
                        "context epoch CAS revision mismatch on update (session {}); skipping this delta",
                        session_id
                    );
                }
            }
            _ => {}
        }

        // The current baseline is the cached top-level system prompt for every turn.
        let system_prompt: Option<String> = db::get_context_epoch(&self.pool, session_id)
            .await?
            .map(|e| e.baseline);

        // 5. Git Checkpoint (Turn Start)
        let checkpoint_engine =
            GitSnapshotEngine::new(session_id, workspace_path, &self.global_data_dir, true);
        let turn_start_hash = checkpoint_engine.track().await?;
        if let Some(ref hash) = turn_start_hash {
            let mut tx = self.pool.begin().await?;
            let seq = db::next_sequence(&mut tx, session_id).await?;
            let chk_id = Uuid::new_v4().to_string();
            db::create_checkpoint(
                &mut tx,
                &chk_id,
                session_id,
                &user_msg_id,
                &hash.0,
                "turn",
                "turn_start",
            )
            .await?;
            tx.commit().await?;

            self.event_tx
                .send(ProtocolEvent::CheckpointCreated {
                    session_id: session_id.to_string(),
                    seq,
                    tree_hash: hash.0.clone(),
                    tool_name: "turn".to_string(),
                    kind: "turn_start".to_string(),
                })
                .await
                .ok();
        }

        // Load app config (per-turn so a changed config takes effect promptly).
        let app_config = AppConfig::load(&self.global_data_dir, workspace_path);

        // Parse model config. The provider resolves its own API key internally.
        // The default model id is the single shared const (also AppConfig's default),
        // so config and orchestrator can never disagree.
        let model_val: serde_json::Value = serde_json::from_str(&session.model_config)?;
        let model_id = model_val["model_id"].as_str().unwrap_or(DEFAULT_MODEL_ID);
        let max_tokens = model_val["max_tokens"].as_u64().unwrap_or(8192) as u32;

        let mut turn_count = 0;
        let max_turns = app_config.max_turns;

        // Initialize file read cache
        let mut file_read_cache = HashMap::new();

        while turn_count < max_turns {
            // Stop cleanly if an abort RPC arrived between turns.
            if cancel.is_cancelled() {
                info!("Turn loop cancelled before turn {}", turn_count + 1);
                break;
            }
            turn_count += 1;

            // Safe provider-turn boundary: fold any pending steer inputs into the
            // visible history (in durable admission order) BEFORE assembling this
            // turn's request, so a steer that arrived mid-drain is seen on the next
            // provider turn rather than waiting for a fresh activity (session.md L153).
            self.promote_pending_steers(session_id, chain_watermark)
                .await?;

            // Fetch current messages, applying any compaction boundary.
            let db_msgs = db::get_messages(&self.pool, session_id).await?;
            let mut chat_msgs = Self::assemble_chat_messages(&db_msgs)?;

            // Expose schemas
            let tool_schemas = self.tool_registry.list_schemas();

            // Proactive compaction: if the estimated request exceeds the context
            // window (minus the output reservation + buffer), fold older turns into
            // a summary once before streaming. Single-pass — the 400 retry below is
            // the backstop if one clean cut still doesn't fit.
            let mut compacted_this_turn = false;
            if app_config.compaction.auto {
                let estimate = self.estimate_tokens(model_id, &chat_msgs, system_prompt.as_deref());
                let window = private_code_providers::context_window(model_id) as usize;
                let budget = window
                    .saturating_sub(max_tokens as usize)
                    .saturating_sub(app_config.compaction.buffer_tokens as usize);
                if estimate > budget
                    && self
                        .perform_compaction(
                            session_id,
                            model_id,
                            app_config.compaction.keep_tokens as usize,
                        )
                        .await?
                {
                    compacted_this_turn = true;
                    let db2 = db::get_messages(&self.pool, session_id).await?;
                    chat_msgs = Self::assemble_chat_messages(&db2)?;
                }
            }

            // Run provider chat with a single context-overflow retry. The epoch
            // baseline is the cached top-level system prompt; mid-conversation
            // deltas and the compaction summary live in `chat_msgs` as system messages.
            let mut stream = loop {
                match self
                    .provider
                    .stream_chat(
                        model_id,
                        system_prompt.as_deref(),
                        max_tokens,
                        &chat_msgs,
                        &tool_schemas,
                    )
                    .await
                {
                    Ok(s) => break s,
                    // Context-too-long 400: compact once and retry (if not already).
                    Err(ProviderError::Api {
                        status: 400,
                        message,
                    }) if is_context_overflow(&message) && !compacted_this_turn => {
                        compacted_this_turn = true;
                        if app_config.compaction.auto
                            && self
                                .perform_compaction(
                                    session_id,
                                    model_id,
                                    app_config.compaction.keep_tokens as usize,
                                )
                                .await?
                        {
                            let db2 = db::get_messages(&self.pool, session_id).await?;
                            chat_msgs = Self::assemble_chat_messages(&db2)?;
                            continue;
                        }
                        self.event_tx
                            .send(ProtocolEvent::Error {
                                session_id: session_id.to_string(),
                                seq: 0,
                                code: "context_overflow".to_string(),
                                message: message.clone(),
                                retryable: false,
                            })
                            .await
                            .ok();
                        return Err(format!(
                            "context overflow, compaction could not recover: {message}"
                        )
                        .into());
                    }
                    Err(e) => {
                        self.event_tx
                            .send(ProtocolEvent::Error {
                                session_id: session_id.to_string(),
                                seq: 0,
                                code: "provider_error".to_string(),
                                message: e.to_string(),
                                retryable: true,
                            })
                            .await
                            .ok();
                        return Err(e.into());
                    }
                }
            };

            let assistant_msg_id = Uuid::new_v4().to_string();

            // Content blocks in stream-arrival order (deterministic, faithful to
            // the real text / thinking / tool interleaving).
            let mut blocks: Vec<PartialBlock> = Vec::new();
            let mut final_usage = UsageStats::default();
            // Set if the stream errors mid-flight: the turn did NOT complete cleanly.
            let mut stream_error: Option<String> = None;

            use futures_util::StreamExt;
            let mut cancelled = false;
            loop {
                // Cancellation: an abort RPC on another task stops the live stream.
                // Dropping the stream aborts the underlying HTTP request.
                let event_opt = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => { cancelled = true; break; }
                    ev = stream.next() => ev,
                };
                let event = match event_opt {
                    Some(Ok(ev)) => ev,
                    Some(Err(e)) => {
                        // Do NOT swallow-and-continue: a transport/parse error mid
                        // stream means the rest of this message is lost. Record it
                        // and stop so the turn is reported as errored, not as a
                        // clean completion with truncated content.
                        error!("Stream delta error: {}", e);
                        stream_error = Some(e.to_string());
                        break;
                    }
                    None => break,
                };

                match event {
                    ProviderEvent::TextDelta(text) => {
                        if let Some(PartialBlock::Text(s)) = blocks.last_mut() {
                            s.push_str(&text);
                        } else {
                            blocks.push(PartialBlock::Text(text.clone()));
                        }
                        self.emit_delta(ProtocolEvent::MessageDelta {
                            session_id: session_id.to_string(),
                            delta: DeltaPayload::Text { text },
                        });
                    }
                    ProviderEvent::ReasoningDelta(reasoning) => {
                        // Append to the last Reasoning block only if it is NOT yet
                        // sealed with a signature. A signature_delta arrives at the
                        // end of a thinking block, so a delta after one belongs to a
                        // NEW thinking block — otherwise two adjacent blocks would
                        // merge and concatenate distinct signatures (invalid on replay).
                        let extend_last = matches!(
                            blocks.last(),
                            Some(PartialBlock::Reasoning {
                                signature: None,
                                ..
                            })
                        );
                        if extend_last {
                            if let Some(PartialBlock::Reasoning { text, .. }) = blocks.last_mut() {
                                text.push_str(&reasoning);
                            }
                        } else {
                            blocks.push(PartialBlock::Reasoning {
                                text: reasoning.clone(),
                                signature: None,
                            });
                        }
                        self.emit_delta(ProtocolEvent::MessageDelta {
                            session_id: session_id.to_string(),
                            delta: DeltaPayload::Reasoning { reasoning },
                        });
                    }
                    ProviderEvent::ReasoningSignatureDelta(sig) => {
                        // Attach to the most recent reasoning block (signature_delta
                        // arrives at the end of a thinking block). Not surfaced as a
                        // UI delta — it is metadata for valid multi-turn replay.
                        for b in blocks.iter_mut().rev() {
                            if let PartialBlock::Reasoning { signature, .. } = b {
                                match signature {
                                    Some(existing) => existing.push_str(&sig),
                                    None => *signature = Some(sig),
                                }
                                break;
                            }
                        }
                    }
                    ProviderEvent::ToolUseStart { id, name } => {
                        blocks.push(PartialBlock::Tool {
                            id: id.clone(),
                            name: name.clone(),
                            input: String::new(),
                        });
                        self.emit_delta(ProtocolEvent::MessageDelta {
                            session_id: session_id.to_string(),
                            delta: DeltaPayload::ToolUse {
                                id,
                                name,
                                input_delta: String::new(),
                            },
                        });
                    }
                    ProviderEvent::ToolUseDelta { id, input_delta } => {
                        for b in blocks.iter_mut().rev() {
                            if let PartialBlock::Tool { id: tid, input, .. } = b
                                && *tid == id
                            {
                                input.push_str(&input_delta);
                                break;
                            }
                        }
                        self.emit_delta(ProtocolEvent::MessageDelta {
                            session_id: session_id.to_string(),
                            delta: DeltaPayload::ToolUse {
                                id,
                                name: String::new(),
                                input_delta,
                            },
                        });
                    }
                    ProviderEvent::ToolUseComplete { id, name, input } => {
                        let mut found = false;
                        for b in blocks.iter_mut().rev() {
                            if let PartialBlock::Tool {
                                id: tid,
                                name: n,
                                input: inp,
                            } = b
                                && *tid == id
                            {
                                *n = name.clone();
                                *inp = input.to_string();
                                found = true;
                                break;
                            }
                        }
                        if !found {
                            blocks.push(PartialBlock::Tool {
                                id,
                                name,
                                input: input.to_string(),
                            });
                        }
                    }
                    ProviderEvent::MessageStop {
                        usage,
                        finish_reason: _,
                    } => {
                        final_usage = usage;
                    }
                }
            }
            // Abort the stream's HTTP connection promptly on cancel.
            drop(stream);

            // Construct content blocks in arrival order. Tool calls whose
            // accumulated input is not valid JSON (e.g. a stream truncated
            // mid-tool) are recorded with an empty-object input (so the persisted
            // tool_use stays replay-valid) and their ids tracked so we return an
            // error tool_result instead of executing them with bogus input.
            let mut content_blocks = Vec::new();
            let mut malformed_tool_ids: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for b in blocks {
                match b {
                    PartialBlock::Text(text) => {
                        if !text.is_empty() {
                            content_blocks.push(ContentBlock::Text { text });
                        }
                    }
                    PartialBlock::Reasoning { text, signature } => {
                        if !text.is_empty() {
                            content_blocks.push(ContentBlock::Reasoning {
                                reasoning: text,
                                signature,
                            });
                        }
                    }
                    PartialBlock::Tool { id, name, input } => {
                        let input_val = if input.trim().is_empty() {
                            serde_json::json!({})
                        } else {
                            match serde_json::from_str::<serde_json::Value>(&input) {
                                Ok(v) => v,
                                Err(_) => {
                                    malformed_tool_ids.insert(id.clone());
                                    serde_json::json!({})
                                }
                            }
                        };
                        content_blocks.push(ContentBlock::ToolUse {
                            id,
                            name,
                            input: input_val,
                        });
                    }
                }
            }

            // Persist the assistant message ONLY if it has content. An errored or
            // cancelled turn (or a content-less stream) must not leave an empty
            // {role:assistant, content:[]} row: Anthropic 400s on an empty content
            // array, which would wedge the session on every later turn with no
            // self-heal. `asst_seq` is None when nothing was persisted.
            let asst_seq: Option<i64> = if content_blocks.is_empty() {
                None
            } else {
                let assistant_msg = ChatMessage {
                    id: assistant_msg_id.clone(),
                    role: Role::Assistant,
                    content: content_blocks.clone(),
                    created_at: chrono::Utc::now().timestamp(),
                };
                let mut tx = self.pool.begin().await?;
                let seq = db::next_sequence(&mut tx, session_id).await?;
                let asst_json = serde_json::to_string(&assistant_msg)?;
                db::append_message(
                    &mut tx,
                    &assistant_msg_id,
                    session_id,
                    seq,
                    "assistant",
                    &asst_json,
                )
                .await?;
                db::update_usage(
                    &mut tx,
                    session_id,
                    final_usage.cost,
                    final_usage.input_tokens,
                    final_usage.output_tokens,
                    final_usage.reasoning_tokens,
                    final_usage.cache_read_tokens,
                    final_usage.cache_write_tokens,
                )
                .await?;
                tx.commit().await?;
                Some(seq)
            };

            // A mid-stream transport/parse error means the turn did not complete
            // cleanly. Any partial assistant message is persisted above; we surface
            // a retryable error and stop — we do NOT present truncated output as a
            // clean completion, nor execute half-parsed tool calls.
            if let Some(err_msg) = stream_error {
                self.event_tx
                    .send(ProtocolEvent::Error {
                        session_id: session_id.to_string(),
                        seq: asst_seq.unwrap_or(0),
                        code: "stream_error".to_string(),
                        message: err_msg.clone(),
                        retryable: true,
                    })
                    .await
                    .ok();
                return Err(format!("provider stream error: {err_msg}").into());
            }

            if let Some(seq) = asst_seq {
                self.event_tx
                    .send(ProtocolEvent::MessageCompleted {
                        session_id: session_id.to_string(),
                        seq,
                        message_id: assistant_msg_id.clone(),
                        usage: final_usage.clone(),
                    })
                    .await
                    .ok();
            }

            // If the turn was cancelled mid-stream, any partial assistant message
            // is now durably persisted; stop here without executing tool calls.
            if cancelled {
                break;
            }

            // If there are no tool calls, this turn is done!
            let tool_calls: Vec<_> = content_blocks
                .iter()
                .filter_map(|cb| {
                    if let ContentBlock::ToolUse { id, name, input } = cb {
                        Some((id, name, input))
                    } else {
                        None
                    }
                })
                .collect();

            if tool_calls.is_empty() {
                break;
            }

            // Execute tool calls and collect responses
            let mut tool_results = Vec::new();

            for (call_id, tool_name, arguments) in tool_calls {
                // A tool call whose arguments failed to parse as JSON is never
                // executed — return an error tool_result so the model can retry
                // with valid arguments (the tool_use/tool_result pair is kept).
                if malformed_tool_ids.contains(call_id) {
                    tool_results.push(ContentBlock::ToolResult {
                        tool_use_id: call_id.clone(),
                        content: ToolResultContent::Text(
                            "Invalid tool call: the arguments were not valid JSON (the tool-call \
                             stream may have been truncated). Retry with complete, valid JSON arguments."
                                .to_string(),
                        ),
                        is_error: true,
                    });
                    continue;
                }

                // Durable boundary: emit tool.requested before running the tool so
                // a client reconnecting after an auto-approved tool ran can render
                // which call produced the following tool.output (api_protocol §3
                // classifies tool.requested as durable; the coordinator replays it).
                {
                    let mut tx = self.pool.begin().await?;
                    let seq = db::next_sequence(&mut tx, session_id).await?;
                    tx.commit().await?;
                    self.event_tx
                        .send(ProtocolEvent::ToolRequested {
                            session_id: session_id.to_string(),
                            seq,
                            tool_call_id: call_id.clone(),
                            tool_name: tool_name.to_string(),
                            arguments: arguments.clone(),
                        })
                        .await
                        .ok();
                }

                let tool_opt = self.tool_registry.get(tool_name);
                let tool = match tool_opt {
                    Some(t) => t,
                    None => {
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: call_id.clone(),
                            content: ToolResultContent::Text(format!(
                                "Tool {} not found",
                                tool_name
                            )),
                            is_error: true,
                        });
                        continue;
                    }
                };

                // Formulate permission action/resource
                let permission_class = tool.permission_class();

                // Extract resource from arguments
                let resource = match permission_class {
                    "read_file" | "write_file" => {
                        arguments["path"]
                            .as_str()
                            .or_else(|| arguments["patchText"].as_str()) // patch uses patchText
                            .unwrap_or("*")
                    }
                    "bash" => arguments["command"].as_str().unwrap_or("*"),
                    "web_fetch" => arguments["url"].as_str().unwrap_or("*"),
                    _ => "*",
                };

                // Load saved permission rules from DB
                let saved_rows = db::get_saved_permissions(&self.pool, &session.project_id).await?;
                let saved_rules: Vec<PermissionRule> = saved_rows
                    .into_iter()
                    .map(|r| {
                        PermissionRule {
                            action: r.action,
                            resource: r.resource,
                            decision: PermissionDecision::Allow, // all rows in permission_saved represent ALLOW
                        }
                    })
                    .collect();

                let decision = permissions::evaluate(
                    permission_class,
                    resource,
                    &session.agent_id,
                    workspace_path,
                    &saved_rules,
                );

                let granted = match decision {
                    PermissionDecision::Allow => true,
                    PermissionDecision::Deny => false,
                    PermissionDecision::Ask => {
                        // Prompt user
                        let perm_id = Uuid::new_v4().to_string();
                        let prompt = PermissionPrompt {
                            permission_id: perm_id.clone(),
                            tool_name: tool_name.to_string(),
                            action: permission_class.to_string(),
                            resources: vec![resource.to_string()],
                            preview: serde_json::to_string_pretty(arguments).unwrap_or_default(),
                        };

                        let mut tx = self.pool.begin().await?;
                        let seq = db::next_sequence(&mut tx, session_id).await?;
                        tx.commit().await?;

                        self.event_tx
                            .send(ProtocolEvent::ToolPermissionRequired {
                                session_id: session_id.to_string(),
                                seq,
                                permission_id: perm_id,
                                tool_name: tool_name.to_string(),
                                action: permission_class.to_string(),
                                resources: vec![resource.to_string()],
                                preview: prompt.preview.clone(),
                            })
                            .await
                            .ok();

                        let (resp_tx, resp_rx) = oneshot::channel();
                        if self
                            .permission_prompt_tx
                            .send((prompt, resp_tx))
                            .await
                            .is_err()
                        {
                            false
                        } else {
                            // The permission wait MUST be cancellable: a session
                            // abort (or graceful-shutdown drain) during a pending
                            // Ask would otherwise park the turn here forever. On
                            // cancel, treat as Deny and stop the turn.
                            tokio::select! {
                                biased;
                                _ = cancel.cancelled() => {
                                    cancelled = true;
                                    false
                                }
                                d = resp_rx => match d {
                                    Ok(PermissionDecision::Allow) => true,
                                    Ok(PermissionDecision::Deny) => false,
                                    _ => false,
                                },
                            }
                        }
                    }
                };

                // If cancelled while waiting for permission, stop the turn cleanly.
                if cancelled {
                    break;
                }

                if !granted {
                    tool_results.push(ContentBlock::ToolResult {
                        tool_use_id: call_id.clone(),
                        content: ToolResultContent::Text(
                            "Permission Denied by user or rule policy.".to_string(),
                        ),
                        is_error: true,
                    });
                    continue;
                }

                // If tool mutates, take a Pre-Step checkpoint
                if tool.mutates()
                    && let Some(ref hash) = checkpoint_engine.track().await?
                {
                    let mut tx = self.pool.begin().await?;
                    let seq = db::next_sequence(&mut tx, session_id).await?;
                    let chk_id = Uuid::new_v4().to_string();
                    db::create_checkpoint(
                        &mut tx,
                        &chk_id,
                        session_id,
                        &assistant_msg_id,
                        &hash.0,
                        tool_name,
                        "pre_step",
                    )
                    .await?;
                    tx.commit().await?;

                    self.event_tx
                        .send(ProtocolEvent::CheckpointCreated {
                            session_id: session_id.to_string(),
                            seq,
                            tree_hash: hash.0.clone(),
                            tool_name: tool_name.to_string(),
                            kind: "pre_step".to_string(),
                        })
                        .await
                        .ok();
                }

                // Execute the tool
                let mut tool_ctx = ToolContext {
                    workspace_path,
                    active_dir,
                    file_read_cache: &mut file_read_cache,
                    global_data_dir: &self.global_data_dir,
                    max_lines: 2000,
                    max_bytes: 50 * 1024, // 50KB cap
                };

                info!("Executing tool {} with args {}", tool_name, arguments);
                // Tool execution is cancellable too: a long bash/web_fetch must
                // not block an abort/shutdown. On cancel, record an aborted result
                // and stop the turn after persisting it.
                let execute_res = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        cancelled = true;
                        Err(private_code_tools::tool::ToolError::Other(
                            "tool execution aborted".to_string(),
                        ))
                    }
                    r = tool.run(&mut tool_ctx, arguments.clone()) => r,
                };

                let (result_content, is_err) = match execute_res {
                    Ok(val) => (ToolResultContent::Json(val), false),
                    Err(e) => (ToolResultContent::Text(e.to_string()), true),
                };

                // If tool mutates, take a Post-Step checkpoint
                if tool.mutates() {
                    let post_step_hash = checkpoint_engine.track().await?;
                    if let Some(ref hash) = post_step_hash {
                        let mut tx = self.pool.begin().await?;
                        let seq = db::next_sequence(&mut tx, session_id).await?;
                        let chk_id = Uuid::new_v4().to_string();
                        db::create_checkpoint(
                            &mut tx,
                            &chk_id,
                            session_id,
                            &assistant_msg_id,
                            &hash.0,
                            tool_name,
                            "post_step",
                        )
                        .await?;
                        tx.commit().await?;

                        self.event_tx
                            .send(ProtocolEvent::CheckpointCreated {
                                session_id: session_id.to_string(),
                                seq,
                                tree_hash: hash.0.clone(),
                                tool_name: tool_name.to_string(),
                                kind: "post_step".to_string(),
                            })
                            .await
                            .ok();
                    }
                }

                tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: call_id.clone(),
                    content: result_content,
                    is_error: is_err,
                });

                // Cancelled during this tool — persist what we have and stop.
                if cancelled {
                    break;
                }
            }

            // Persist the tool results as a single user message
            // Persist tool results ONLY if we produced any. A cancel during the
            // permission wait (or before the first tool ran) breaks the loop with
            // an empty tool_results Vec; persisting {role:user, content:[]} would
            // wedge the session (empty content array -> Anthropic 400, no
            // self-heal). With nothing persisted, recover_interrupted_tools repairs
            // the unfulfilled tool_use on the next turn.
            if !tool_results.is_empty() {
                let tool_results_msg_id = Uuid::new_v4().to_string();
                let tool_results_msg = ChatMessage {
                    id: tool_results_msg_id.clone(),
                    role: Role::User,
                    content: tool_results,
                    created_at: chrono::Utc::now().timestamp(),
                };

                let mut tx = self.pool.begin().await?;
                let tr_seq = db::next_sequence(&mut tx, session_id).await?;
                let tr_json = serde_json::to_string(&tool_results_msg)?;
                db::append_message(
                    &mut tx,
                    &tool_results_msg_id,
                    session_id,
                    tr_seq,
                    "user",
                    &tr_json,
                )
                .await?;
                tx.commit().await?;

                // Emit tool outputs
                for cb in &tool_results_msg.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } = cb
                    {
                        self.event_tx
                            .send(ProtocolEvent::ToolOutput {
                                session_id: session_id.to_string(),
                                seq: tr_seq,
                                tool_call_id: tool_use_id.clone(),
                                output: content.clone(),
                                is_error: *is_error,
                            })
                            .await
                            .ok();
                    }
                }
            }

            // A cancel during this turn's tools is observed at the loop top, but
            // break now too so we don't start another stream after a partial turn.
            if cancelled {
                break;
            }
        }

        Ok(())
    }

    /// Build the provider-visible message list from durable rows, applying the
    /// latest compaction boundary: prepend the summary as a System message and
    /// drop messages at or before the compacted_through_seq (and the markers).
    fn assemble_chat_messages(
        db_msgs: &[db::MessageRow],
    ) -> Result<Vec<ChatMessage>, serde_json::Error> {
        let marker = db_msgs
            .iter()
            .filter(|m| m.type_ == "compaction")
            .max_by_key(|m| m.seq);
        let (boundary, summary) = match marker {
            Some(m) => {
                let cm: CompactionMarker = serde_json::from_str(&m.data)?;
                (cm.compacted_through_seq, Some(cm.summary))
            }
            None => (-1, None),
        };

        let mut out = Vec::new();
        if let Some(text) = summary {
            out.push(ChatMessage {
                id: "compaction-summary".to_string(),
                role: Role::System,
                content: vec![ContentBlock::Text { text }],
                created_at: 0,
            });
        }
        for m in db_msgs {
            if m.type_ == "compaction" || m.seq <= boundary {
                continue;
            }
            out.push(serde_json::from_str(&m.data)?);
        }
        Ok(out)
    }

    /// Coarse token estimate for a request: system prompt + every content block.
    fn estimate_tokens(&self, model_id: &str, msgs: &[ChatMessage], system: Option<&str>) -> usize {
        let mut total = system
            .map(|s| self.provider.count_tokens(model_id, s))
            .unwrap_or(0);
        for m in msgs {
            for b in &m.content {
                total += self.provider.count_tokens(model_id, &block_text(b));
            }
        }
        total
    }

    /// Fold the oldest live messages into a summary marker, cutting only at a
    /// clean user turn-start within `keep_tokens`. Returns true if it compacted.
    /// Durable rows are never deleted; the summary supersedes via the new marker.
    async fn perform_compaction(
        &self,
        session_id: &str,
        model_id: &str,
        keep_tokens: usize,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let db_msgs = db::get_messages(&self.pool, session_id).await?;

        // Prior boundary + summary (cumulative across repeated compactions).
        let prev_marker = db_msgs
            .iter()
            .filter(|m| m.type_ == "compaction")
            .max_by_key(|m| m.seq);
        let (prev_boundary, prev_summary) = match prev_marker {
            Some(m) => {
                let cm: CompactionMarker = serde_json::from_str(&m.data)?;
                (cm.compacted_through_seq, cm.summary)
            }
            None => (-1, String::new()),
        };

        // Live messages = post prev-boundary, excluding markers.
        let mut live: Vec<(i64, ChatMessage)> = Vec::new();
        for m in &db_msgs {
            if m.type_ == "compaction" || m.seq <= prev_boundary {
                continue;
            }
            live.push((m.seq, serde_json::from_str(&m.data)?));
        }
        if live.len() < 2 {
            return Ok(false);
        }

        // Walk back from the end: keep the most-recent messages within keep_tokens,
        // and cut at the OLDEST clean turn-start that still fits (index > 0 so the
        // dropped set is non-empty).
        let mut acc = 0usize;
        let mut best_start: Option<usize> = None;
        for i in (0..live.len()).rev() {
            for b in &live[i].1.content {
                acc += self.provider.count_tokens(model_id, &block_text(b));
            }
            if acc > keep_tokens {
                break;
            }
            if i > 0 && is_clean_turn_start(&live[i].1) {
                best_start = Some(i);
            }
        }
        let start = match best_start {
            Some(s) => s,
            None => return Ok(false), // no safe cut (one turn, or a giant recent turn)
        };

        let boundary_new = live[start - 1].0;
        let dropped: Vec<&ChatMessage> = live[..start].iter().map(|(_, m)| m).collect();
        let summary = build_summary(&prev_summary, &dropped);

        let mut tx = self.pool.begin().await?;
        let seq = db::next_sequence(&mut tx, session_id).await?;
        let marker_id = Uuid::new_v4().to_string();
        let marker = CompactionMarker {
            compacted_through_seq: boundary_new,
            summary,
        };
        let data = serde_json::to_string(&marker)?;
        db::append_message(&mut tx, &marker_id, session_id, seq, "compaction", &data).await?;
        tx.commit().await?;

        info!(
            "Compacted session {}: folded {} message(s) through seq {} into a summary",
            session_id, start, boundary_new
        );
        Ok(true)
    }

    async fn recover_interrupted_tools(&self, session_id: &str) -> Result<(), sqlx::Error> {
        // Look at the last message. If it contains running tool uses or is an Assistant message with tool calls
        // that have no corresponding ToolResult messages, we append an error tool result message to fail them.
        let messages = db::get_messages(&self.pool, session_id).await?;
        if messages.is_empty() {
            return Ok(());
        }

        // Parse messages to find the last assistant message and check if it has unfulfilled tool uses
        let mut last_asst: Option<(String, i64, ChatMessage)> = None;
        let mut fulfilled_tool_uses = std::collections::HashSet::new();

        for m in &messages {
            if let Ok(cm) = serde_json::from_str::<ChatMessage>(&m.data) {
                match cm.role {
                    Role::Assistant => {
                        last_asst = Some((m.id.clone(), m.seq, cm));
                    }
                    Role::User => {
                        for cb in &cm.content {
                            if let ContentBlock::ToolResult { tool_use_id, .. } = cb {
                                fulfilled_tool_uses.insert(tool_use_id.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        if let Some((_msg_id, _seq, cm)) = last_asst {
            let mut interrupted_results = Vec::new();
            for cb in &cm.content {
                if let ContentBlock::ToolUse { id, name, .. } = cb
                    && !fulfilled_tool_uses.contains(id)
                {
                    // This tool use was interrupted/unfulfilled!
                    interrupted_results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: ToolResultContent::Text(format!(
                            "Tool execution '{}' was interrupted due to a session crash or reload.",
                            name
                        )),
                        is_error: true,
                    });
                }
            }

            if !interrupted_results.is_empty() {
                let res_msg_id = Uuid::new_v4().to_string();
                let res_msg = ChatMessage {
                    id: res_msg_id.clone(),
                    role: Role::User,
                    content: interrupted_results,
                    created_at: chrono::Utc::now().timestamp(),
                };
                let mut tx = self.pool.begin().await?;
                let tr_seq = db::next_sequence(&mut tx, session_id).await?;
                let tr_json = serde_json::to_string(&res_msg)
                    .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
                db::append_message(&mut tx, &res_msg_id, session_id, tr_seq, "user", &tr_json)
                    .await?;
                tx.commit().await?;
                info!(
                    "Recovered and failed unfulfilled tool uses in session {}",
                    session_id
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{
        connect_db, create_project, create_session, get_context_epoch, get_messages, run_migrations,
    };
    use futures_util::stream::{BoxStream, StreamExt};
    use private_code_protocol::message::ChatMessage;
    use private_code_providers::provider::{ModelProvider, ProviderError, ProviderEvent};
    use private_code_tools::tool::{Tool, ToolContext, ToolError};
    use sqlx::SqlitePool;
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    /// A provider that replays a scripted sequence of events per turn — the
    /// recorded-fixture harness that lets us drive the turn loop fully offline.
    struct ScriptedProvider {
        turns: StdMutex<VecDeque<Vec<ProviderEvent>>>,
    }

    #[async_trait::async_trait]
    impl ModelProvider for ScriptedProvider {
        async fn stream_chat(
            &self,
            _model_id: &str,
            _system_prompt: Option<&str>,
            _max_tokens: u32,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError>
        {
            let evs = self.turns.lock().unwrap().pop_front().unwrap_or_default();
            Ok(futures_util::stream::iter(evs.into_iter().map(Ok)).boxed())
        }
        fn count_tokens(&self, _m: &str, t: &str) -> usize {
            t.len() / 4
        }
    }

    /// A provider whose stream yields one delta then hangs forever — used to
    /// prove cancellation actually stops the turn.
    struct PendingProvider;

    #[async_trait::async_trait]
    impl ModelProvider for PendingProvider {
        async fn stream_chat(
            &self,
            _model_id: &str,
            _system_prompt: Option<&str>,
            _max_tokens: u32,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError>
        {
            let s = futures_util::stream::once(async {
                Ok(ProviderEvent::TextDelta("partial".into()))
            })
            .chain(futures_util::stream::pending::<
                Result<ProviderEvent, ProviderError>,
            >());
            Ok(s.boxed())
        }
        fn count_tokens(&self, _m: &str, t: &str) -> usize {
            t.len() / 4
        }
    }

    /// Auto-allowed (permission class "glob") read-only tool that returns a fixed result.
    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "mock"
        }
        fn description(&self) -> &str {
            "mock tool"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"name": "mock", "input_schema": {"type": "object"}})
        }
        fn mutates(&self) -> bool {
            false
        }
        fn permission_class(&self) -> &str {
            "glob"
        }
        async fn run(
            &self,
            _ctx: &mut ToolContext<'_>,
            _args: serde_json::Value,
        ) -> Result<serde_json::Value, ToolError> {
            Ok(serde_json::json!({"echo": "ok"}))
        }
    }

    async fn make_orch(
        provider: Arc<dyn ModelProvider>,
        ws: &Path,
    ) -> (Arc<Orchestrator>, String, SqlitePool) {
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        let project_id = Uuid::new_v4().to_string();
        create_project(&pool, &project_id, "t", ws.to_str().unwrap())
            .await
            .unwrap();
        let session_id = Uuid::new_v4().to_string();
        let model_config =
            serde_json::json!({"provider_id": "anthropic", "model_id": "claude-opus-4-8"})
                .to_string();
        create_session(
            &pool,
            &session_id,
            &project_id,
            ws.to_str().unwrap(),
            ws.to_str().unwrap(),
            "t",
            "build",
            &model_config,
        )
        .await
        .unwrap();

        let mut reg = ToolRegistry::new();
        reg.register(Box::new(EchoTool));

        let (ptx, _prx) = mpsc::channel(10);
        let (etx, mut erx) = mpsc::channel(4096);
        // Drain events so durable `.send().await`s never block in the test.
        tokio::spawn(async move { while erx.recv().await.is_some() {} });

        let orch = Orchestrator::new(
            pool.clone(),
            std::env::temp_dir(),
            provider,
            Arc::new(reg),
            ptx,
            etx,
        );
        (Arc::new(orch), session_id, pool)
    }

    #[tokio::test]
    async fn test_turn_loop_tool_then_text_terminates() {
        let ws = TempDir::new().unwrap();
        let provider = Arc::new(ScriptedProvider {
            turns: StdMutex::new(VecDeque::from(vec![
                vec![
                    ProviderEvent::ToolUseStart {
                        id: "tu1".into(),
                        name: "mock".into(),
                    },
                    ProviderEvent::ToolUseComplete {
                        id: "tu1".into(),
                        name: "mock".into(),
                        input: serde_json::json!({}),
                    },
                    ProviderEvent::MessageStop {
                        usage: UsageStats::default(),
                        finish_reason: Some("tool_use".into()),
                    },
                ],
                vec![
                    ProviderEvent::TextDelta("all done".into()),
                    ProviderEvent::MessageStop {
                        usage: UsageStats::default(),
                        finish_reason: Some("end_turn".into()),
                    },
                ],
            ])),
        });

        let (orch, sid, pool) = make_orch(provider, ws.path()).await;
        let input_id = orch.admit_input(&sid, "hi", "steer").await.unwrap();
        orch.run_session_turn(&sid, &input_id, CancellationToken::new())
            .await
            .unwrap();

        // The epoch was INSERTED on the first turn (the insert-on-first-turn fix)
        // and carries the baseline that becomes the cached system prompt.
        let epoch = get_context_epoch(&pool, &sid)
            .await
            .unwrap()
            .expect("epoch inserted");
        assert!(
            !epoch.baseline.is_empty(),
            "baseline must be stored for the cached system prompt"
        );

        // The loop executed the tool, fed the result back, produced text, and STOPPED.
        let msgs = get_messages(&pool, &sid).await.unwrap();
        assert!(
            msgs.iter().any(|m| m.data.contains("tool_use")),
            "assistant tool_use persisted"
        );
        assert!(
            msgs.iter().any(|m| m.data.contains("tool_result")),
            "tool result persisted"
        );
        assert!(
            msgs.iter().any(|m| m.data.contains("all done")),
            "final assistant text persisted"
        );
        assert!(
            msgs.len() <= 6,
            "loop terminated rather than running to max_turns"
        );
    }

    #[tokio::test]
    async fn test_cancellation_terminates_turn() {
        let ws = TempDir::new().unwrap();
        let (orch, sid, _pool) = make_orch(Arc::new(PendingProvider), ws.path()).await;
        let input_id = orch.admit_input(&sid, "hi", "steer").await.unwrap();

        let token = CancellationToken::new();
        let token2 = token.clone();
        let orch2 = orch.clone();
        let sid2 = sid.clone();
        let handle = tokio::spawn(async move {
            // Discard the result inside the task: run_session_turn returns a
            // non-Send Box<dyn Error>, which a JoinHandle can't carry.
            let _ = orch2.run_session_turn(&sid2, &input_id, token).await;
        });

        // Let the stream yield its first delta and then hang.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        token2.cancel();

        // Must terminate promptly — not hang on the pending stream.
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        assert!(
            res.is_ok(),
            "run_session_turn must terminate after cancel (no hang)"
        );
    }

    /// A provider that errors mid-stream after one text delta.
    struct ErrorMidStreamProvider;

    #[async_trait::async_trait]
    impl ModelProvider for ErrorMidStreamProvider {
        async fn stream_chat(
            &self,
            _model_id: &str,
            _system_prompt: Option<&str>,
            _max_tokens: u32,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError>
        {
            let s = futures_util::stream::iter(vec![
                Ok(ProviderEvent::TextDelta("partial".into())),
                Err(ProviderError::Other("boom".into())),
            ]);
            Ok(s.boxed())
        }
        fn count_tokens(&self, _m: &str, t: &str) -> usize {
            t.len() / 4
        }
    }

    /// A provider that returns the same (auto-allowed) tool call on every turn,
    /// so the loop only ends when it hits max_turns.
    struct AlwaysToolProvider;

    #[async_trait::async_trait]
    impl ModelProvider for AlwaysToolProvider {
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
                ProviderEvent::ToolUseStart {
                    id: "loop".into(),
                    name: "mock".into(),
                },
                ProviderEvent::ToolUseComplete {
                    id: "loop".into(),
                    name: "mock".into(),
                    input: serde_json::json!({}),
                },
                ProviderEvent::MessageStop {
                    usage: UsageStats::default(),
                    finish_reason: Some("tool_use".into()),
                },
            ];
            Ok(futures_util::stream::iter(evs.into_iter().map(Ok)).boxed())
        }
        fn count_tokens(&self, _m: &str, t: &str) -> usize {
            t.len() / 4
        }
    }

    fn assistant_tool_messages(msgs: &[crate::db::MessageRow]) -> Vec<ChatMessage> {
        msgs.iter()
            .filter_map(|m| serde_json::from_str::<ChatMessage>(&m.data).ok())
            .filter(|cm| {
                cm.role == Role::Assistant
                    && cm
                        .content
                        .iter()
                        .any(|c| matches!(c, ContentBlock::ToolUse { .. }))
            })
            .collect()
    }

    #[tokio::test]
    async fn test_parallel_tool_calls_preserve_stream_order() {
        let ws = TempDir::new().unwrap();
        let provider = Arc::new(ScriptedProvider {
            turns: StdMutex::new(VecDeque::from(vec![
                vec![
                    ProviderEvent::ToolUseStart {
                        id: "z".into(),
                        name: "mock".into(),
                    },
                    ProviderEvent::ToolUseComplete {
                        id: "z".into(),
                        name: "mock".into(),
                        input: serde_json::json!({"k":1}),
                    },
                    ProviderEvent::ToolUseStart {
                        id: "a".into(),
                        name: "mock".into(),
                    },
                    ProviderEvent::ToolUseComplete {
                        id: "a".into(),
                        name: "mock".into(),
                        input: serde_json::json!({"k":2}),
                    },
                    ProviderEvent::MessageStop {
                        usage: UsageStats::default(),
                        finish_reason: Some("tool_use".into()),
                    },
                ],
                vec![
                    ProviderEvent::TextDelta("done".into()),
                    ProviderEvent::MessageStop {
                        usage: UsageStats::default(),
                        finish_reason: Some("end_turn".into()),
                    },
                ],
            ])),
        });

        let (orch, sid, pool) = make_orch(provider, ws.path()).await;
        let input_id = orch.admit_input(&sid, "hi", "steer").await.unwrap();
        orch.run_session_turn(&sid, &input_id, CancellationToken::new())
            .await
            .unwrap();

        let msgs = get_messages(&pool, &sid).await.unwrap();
        let asst = assistant_tool_messages(&msgs)
            .into_iter()
            .next()
            .expect("assistant tool_use message");
        let ids: Vec<String> = asst
            .content
            .iter()
            .filter_map(|c| match c {
                ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            ids,
            vec!["z".to_string(), "a".to_string()],
            "tool_use blocks must persist in stream-arrival order (not HashMap order)"
        );
    }

    #[tokio::test]
    async fn test_malformed_tool_input_is_rejected_not_executed() {
        let ws = TempDir::new().unwrap();
        // Tool stream is truncated mid-input (no ToolUseComplete), so the
        // accumulated input "{\"oops\":" never parses.
        let provider = Arc::new(ScriptedProvider {
            turns: StdMutex::new(VecDeque::from(vec![
                vec![
                    ProviderEvent::ToolUseStart {
                        id: "t1".into(),
                        name: "mock".into(),
                    },
                    ProviderEvent::ToolUseDelta {
                        id: "t1".into(),
                        input_delta: "{\"oops\":".into(),
                    },
                    ProviderEvent::MessageStop {
                        usage: UsageStats::default(),
                        finish_reason: Some("tool_use".into()),
                    },
                ],
                vec![
                    ProviderEvent::TextDelta("done".into()),
                    ProviderEvent::MessageStop {
                        usage: UsageStats::default(),
                        finish_reason: Some("end_turn".into()),
                    },
                ],
            ])),
        });

        let (orch, sid, pool) = make_orch(provider, ws.path()).await;
        let input_id = orch.admit_input(&sid, "hi", "steer").await.unwrap();
        orch.run_session_turn(&sid, &input_id, CancellationToken::new())
            .await
            .unwrap();

        let all: String = get_messages(&pool, &sid)
            .await
            .unwrap()
            .iter()
            .map(|m| m.data.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all.contains("Invalid tool call"),
            "a malformed tool call must produce an error tool_result"
        );
        assert!(
            !all.contains("\"echo\""),
            "the tool must NOT have been executed with bogus input"
        );
    }

    #[tokio::test]
    async fn test_mid_stream_error_fails_the_turn() {
        let ws = TempDir::new().unwrap();
        let (orch, sid, pool) = make_orch(Arc::new(ErrorMidStreamProvider), ws.path()).await;
        let input_id = orch.admit_input(&sid, "hi", "steer").await.unwrap();

        let res = orch
            .run_session_turn(&sid, &input_id, CancellationToken::new())
            .await;
        assert!(
            res.is_err(),
            "a mid-stream provider error must fail the turn, not report success"
        );
        // The partial assistant text is still durably persisted.
        let msgs = get_messages(&pool, &sid).await.unwrap();
        assert!(
            msgs.iter().any(|m| m.data.contains("partial")),
            "the partial assistant text should be persisted"
        );
    }

    #[tokio::test]
    async fn test_max_turns_config_caps_the_loop() {
        let ws = TempDir::new().unwrap();
        // Project config caps the loop at 2 turns.
        let cfg_dir = ws.path().join(".private-code");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("config.json"), r#"{"max_turns": 2}"#).unwrap();

        let (orch, sid, pool) = make_orch(Arc::new(AlwaysToolProvider), ws.path()).await;
        let input_id = orch.admit_input(&sid, "hi", "steer").await.unwrap();
        orch.run_session_turn(&sid, &input_id, CancellationToken::new())
            .await
            .unwrap();

        let msgs = get_messages(&pool, &sid).await.unwrap();
        assert_eq!(
            assistant_tool_messages(&msgs).len(),
            2,
            "the loop must stop at max_turns=2 from config"
        );
    }

    #[tokio::test]
    async fn test_agents_md_change_appends_system_delta_and_bumps_revision() {
        let ws = TempDir::new().unwrap();
        std::fs::write(ws.path().join("AGENTS.md"), "v1 instructions").unwrap();

        let provider = Arc::new(ScriptedProvider {
            turns: StdMutex::new(VecDeque::from(vec![
                vec![
                    ProviderEvent::TextDelta("ok1".into()),
                    ProviderEvent::MessageStop {
                        usage: UsageStats::default(),
                        finish_reason: Some("end_turn".into()),
                    },
                ],
                vec![
                    ProviderEvent::TextDelta("ok2".into()),
                    ProviderEvent::MessageStop {
                        usage: UsageStats::default(),
                        finish_reason: Some("end_turn".into()),
                    },
                ],
            ])),
        });

        let (orch, sid, pool) = make_orch(provider, ws.path()).await;

        // Turn 1: establishes the epoch (baseline embeds AGENTS.md v1).
        let i1 = orch.admit_input(&sid, "first", "steer").await.unwrap();
        orch.run_session_turn(&sid, &i1, CancellationToken::new())
            .await
            .unwrap();
        let epoch1 = get_context_epoch(&pool, &sid).await.unwrap().unwrap();
        let baseline1 = epoch1.baseline.clone();
        let rev1 = epoch1.revision;

        // Change AGENTS.md between turns.
        std::fs::write(ws.path().join("AGENTS.md"), "v2 different instructions").unwrap();

        // Turn 2: reconcile yields Updated -> a system delta message + revision bump,
        // while the cached baseline stays byte-identical (prompt cache stays warm).
        let i2 = orch.admit_input(&sid, "second", "steer").await.unwrap();
        orch.run_session_turn(&sid, &i2, CancellationToken::new())
            .await
            .unwrap();

        let epoch2 = get_context_epoch(&pool, &sid).await.unwrap().unwrap();
        assert_eq!(epoch2.revision, rev1 + 1, "revision must bump on Updated");
        assert_eq!(
            epoch2.baseline, baseline1,
            "baseline must stay byte-identical so the prompt cache is preserved"
        );

        let msgs = get_messages(&pool, &sid).await.unwrap();
        assert!(
            msgs.iter()
                .any(|m| m.type_ == "system" && m.data.contains("v2 different")),
            "an Updated turn must append a system delta message carrying the new instructions"
        );
    }

    /// Turn 1 signals it was reached, then blocks until released, then emits a
    /// `mock` tool_use (forcing a turn 2); every later turn emits a terminal text.
    /// This opens a deterministic window AFTER turn 1's boundary scan but BEFORE
    /// turn 2's, so a steer admitted in it must fold into the SAME activity.
    struct GatedToolProvider {
        reached: StdMutex<Option<oneshot::Sender<()>>>,
        release: StdMutex<Option<oneshot::Receiver<()>>>,
    }

    #[async_trait::async_trait]
    impl ModelProvider for GatedToolProvider {
        async fn stream_chat(
            &self,
            _model_id: &str,
            _system_prompt: Option<&str>,
            _max_tokens: u32,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError>
        {
            // Only the first call holds the reached-signal and the release-gate.
            if let Some(tx) = self.reached.lock().unwrap().take() {
                let _ = tx.send(());
            }
            let release = self.release.lock().unwrap().take();
            if let Some(rx) = release {
                let _ = rx.await;
                // Turn 1: a tool call so the loop continues to a turn 2 boundary.
                let evs = vec![
                    ProviderEvent::ToolUseStart {
                        id: "tu1".into(),
                        name: "mock".into(),
                    },
                    ProviderEvent::ToolUseComplete {
                        id: "tu1".into(),
                        name: "mock".into(),
                        input: serde_json::json!({}),
                    },
                    ProviderEvent::MessageStop {
                        usage: UsageStats::default(),
                        finish_reason: Some("tool_use".into()),
                    },
                ];
                return Ok(futures_util::stream::iter(evs.into_iter().map(Ok)).boxed());
            }
            // Turn 2+: terminate.
            let evs = vec![
                ProviderEvent::TextDelta("done".into()),
                ProviderEvent::MessageStop {
                    usage: UsageStats::default(),
                    finish_reason: Some("end_turn".into()),
                },
            ];
            Ok(futures_util::stream::iter(evs.into_iter().map(Ok)).boxed())
        }
        fn count_tokens(&self, _m: &str, t: &str) -> usize {
            t.len() / 4
        }
    }

    /// A `delivery="steer"` input admitted while a turn is in flight is folded into
    /// the SAME activity at the next safe provider-turn boundary (session.md L153) —
    /// it appears as a visible user message after the opening prompt and the tool
    /// round-trip. And once coalesced, re-running it (as the coordinator's queued
    /// copy eventually would) is a clean no-op — the property that lets the
    /// coordinator enqueue every input without double-running steers.
    #[tokio::test]
    async fn steer_coalesces_into_the_running_activity() {
        let ws = TempDir::new().unwrap();
        let (reached_tx, reached_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let provider = Arc::new(GatedToolProvider {
            reached: StdMutex::new(Some(reached_tx)),
            release: StdMutex::new(Some(release_rx)),
        });
        let (orch, sid, pool) = make_orch(provider, ws.path()).await;

        // Open the activity (promoted by run_session_turn step 3).
        let a = orch.admit_input(&sid, "open", "queue").await.unwrap();
        let orch_run = orch.clone();
        let sid_run = sid.clone();
        // Discard the (non-Send) Box<dyn Error> inside the task so the JoinHandle
        // output stays Send; the DB assertions below verify the turn succeeded.
        let run = tokio::spawn(async move {
            let _ = orch_run
                .run_session_turn(&sid_run, &a, CancellationToken::new())
                .await;
        });

        // Turn 1 is now in flight: its boundary scan has already run and saw no
        // steer. Admit one NOW so it can only be coalesced at turn 2's boundary.
        reached_rx.await.unwrap();
        let s = orch.admit_input(&sid, "steer me", "steer").await.unwrap();
        release_tx.send(()).unwrap();

        run.await.unwrap();

        // The steer became a visible user message, after the opening prompt.
        let msgs = get_messages(&pool, &sid).await.unwrap();
        let parsed: Vec<(i64, ChatMessage)> = msgs
            .iter()
            .filter_map(|m| {
                serde_json::from_str::<ChatMessage>(&m.data)
                    .ok()
                    .map(|c| (m.seq, c))
            })
            .collect();
        let user_texts: Vec<String> = parsed
            .iter()
            .filter(|(_, m)| m.role == Role::User)
            .filter_map(|(_, m)| {
                m.content.iter().find_map(|c| match c {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
            })
            .collect();
        assert_eq!(
            user_texts,
            vec!["open".to_string(), "steer me".to_string()],
            "the steer folded in as a user message after the opening prompt"
        );

        // It was coalesced AT the turn-2 boundary (after turn 1's assistant
        // tool_use), proving mid-drain injection rather than a pre-turn-1 promotion.
        let steer_seq = parsed
            .iter()
            .find(|(_, m)| {
                m.role == Role::User
                    && m.content
                        .iter()
                        .any(|c| matches!(c, ContentBlock::Text { text } if text == "steer me"))
            })
            .map(|(seq, _)| *seq)
            .unwrap();
        let tool_use_seq = parsed
            .iter()
            .find(|(_, m)| {
                m.role == Role::Assistant
                    && m.content
                        .iter()
                        .any(|c| matches!(c, ContentBlock::ToolUse { .. }))
            })
            .map(|(seq, _)| *seq)
            .unwrap();
        assert!(
            steer_seq > tool_use_seq,
            "the steer was coalesced at the turn-2 boundary, after turn 1's tool_use"
        );

        // The inbox fully drained — the coalesced steer is not left pending.
        assert!(
            db::get_pending_inputs(&pool, &sid)
                .await
                .unwrap()
                .is_empty(),
            "the coalesced steer is promoted, not left pending"
        );

        // The coordinator also enqueues every input id; when it later pops this
        // steer's queued copy, run_session_turn must find nothing to promote and
        // add no messages (idempotent — no double-run).
        let before = get_messages(&pool, &sid).await.unwrap().len();
        orch.run_session_turn(&sid, &s, CancellationToken::new())
            .await
            .unwrap();
        let after = get_messages(&pool, &sid).await.unwrap().len();
        assert_eq!(
            before, after,
            "re-running an already-coalesced steer is a no-op (no double-run)"
        );
    }

    /// The ownership-chain boundary: a steer left pending by an abort (lower seq)
    /// must NOT be resurrected into a LATER unrelated activity (session.md L163).
    /// This fails without the `admitted_seq > chain_watermark` filter (the scan
    /// would coalesce the orphan steer into the fresh turn) and passes with it.
    #[tokio::test]
    async fn abandoned_steer_is_not_resurrected_into_a_later_activity() {
        let ws = TempDir::new().unwrap();
        // One terminal text turn — the fresh activity does no tool work.
        let provider = Arc::new(ScriptedProvider {
            turns: StdMutex::new(VecDeque::from(vec![vec![
                ProviderEvent::TextDelta("fresh reply".into()),
                ProviderEvent::MessageStop {
                    usage: UsageStats::default(),
                    finish_reason: Some("end_turn".into()),
                },
            ]])),
        });
        let (orch, sid, pool) = make_orch(provider, ws.path()).await;

        // An orphan steer: admitted (lower seq) but never run — the residue an
        // abort leaves behind after clearing the in-memory queue.
        orch.admit_input(&sid, "stale steer", "steer")
            .await
            .unwrap();
        // A later, unrelated fresh prompt opens a new activity (higher seq).
        let b = orch.admit_input(&sid, "fresh", "queue").await.unwrap();
        orch.run_session_turn(&sid, &b, CancellationToken::new())
            .await
            .unwrap();

        // The fresh activity must contain "fresh" but NOT the orphan steer.
        let msgs = get_messages(&pool, &sid).await.unwrap();
        let user_texts: Vec<String> = msgs
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
        assert!(
            user_texts.contains(&"fresh".to_string()),
            "the fresh prompt was promoted"
        );
        assert!(
            !user_texts.contains(&"stale steer".to_string()),
            "the abandoned steer must NOT be resurrected into a later activity"
        );

        // It is preserved-but-skipped: still a pending inbox row (recovery deferred).
        let pending = db::get_pending_inputs(&pool, &sid).await.unwrap();
        assert!(
            pending.iter().any(|i| i.prompt == "stale steer"),
            "the abandoned steer is preserved as a pending row, not lost"
        );
    }

    /// A tool whose permission_class ("write_file") maps to Ask under the build
    /// agent, so it parks the turn on the permission prompt.
    struct AskTool;

    #[async_trait::async_trait]
    impl Tool for AskTool {
        fn name(&self) -> &str {
            "write_file"
        }
        fn description(&self) -> &str {
            "ask tool"
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
            _ctx: &mut ToolContext<'_>,
            _args: serde_json::Value,
        ) -> Result<serde_json::Value, ToolError> {
            Ok(serde_json::json!({"ok": true}))
        }
    }

    #[tokio::test]
    async fn test_abort_during_pending_permission_terminates() {
        let ws = TempDir::new().unwrap();
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        let pid = Uuid::new_v4().to_string();
        create_project(&pool, &pid, "t", ws.path().to_str().unwrap())
            .await
            .unwrap();
        let sid = Uuid::new_v4().to_string();
        let model_config =
            serde_json::json!({"provider_id":"anthropic","model_id":"claude-opus-4-8"}).to_string();
        create_session(
            &pool,
            &sid,
            &pid,
            ws.path().to_str().unwrap(),
            ws.path().to_str().unwrap(),
            "t",
            "build",
            &model_config,
        )
        .await
        .unwrap();

        let provider = Arc::new(ScriptedProvider {
            turns: StdMutex::new(VecDeque::from(vec![vec![
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
            ]])),
        });

        let mut reg = ToolRegistry::new();
        reg.register(Box::new(AskTool));

        // Keep the permission receiver ALIVE but never reply — the turn parks on
        // the permission wait until cancellation.
        let (ptx, _prx) = mpsc::channel(10);
        let (etx, mut erx) = mpsc::channel(4096);
        tokio::spawn(async move { while erx.recv().await.is_some() {} });

        let orch = Arc::new(Orchestrator::new(
            pool.clone(),
            std::env::temp_dir(),
            provider,
            Arc::new(reg),
            ptx,
            etx,
        ));
        let input_id = orch.admit_input(&sid, "hi", "steer").await.unwrap();

        let token = CancellationToken::new();
        let token2 = token.clone();
        let orch2 = orch.clone();
        let sid2 = sid.clone();
        let handle = tokio::spawn(async move {
            let _ = orch2.run_session_turn(&sid2, &input_id, token).await;
        });

        // Let the turn reach the permission wait, then abort.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        token2.cancel();

        let res = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        assert!(
            res.is_ok(),
            "abort during a pending permission must terminate the turn (no hang)"
        );

        // Regression: the cancel must NOT persist an empty-content tool_results
        // user row — {role:user, content:[]} would 400 on every later turn and
        // permanently wedge the session.
        let msgs = get_messages(&pool, &sid).await.unwrap();
        assert!(
            !msgs
                .iter()
                .any(|m| serde_json::from_str::<ChatMessage>(&m.data)
                    .map(|cm| cm.content.is_empty())
                    .unwrap_or(false)),
            "no empty-content message may be persisted when cancelling a pending permission"
        );
        drop(_prx);
    }

    /// A provider whose very first stream item is an error (no content emitted).
    struct ImmediateErrorProvider;

    #[async_trait::async_trait]
    impl ModelProvider for ImmediateErrorProvider {
        async fn stream_chat(
            &self,
            _model_id: &str,
            _system_prompt: Option<&str>,
            _max_tokens: u32,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError>
        {
            let s = futures_util::stream::iter(vec![Err(ProviderError::Other("boom".into()))]);
            Ok(s.boxed())
        }
        fn count_tokens(&self, _m: &str, t: &str) -> usize {
            t.len() / 4
        }
    }

    #[tokio::test]
    async fn test_stream_error_before_content_persists_no_empty_message() {
        let ws = TempDir::new().unwrap();
        let (orch, sid, pool) = make_orch(Arc::new(ImmediateErrorProvider), ws.path()).await;
        let input_id = orch.admit_input(&sid, "hi", "steer").await.unwrap();

        let res = orch
            .run_session_turn(&sid, &input_id, CancellationToken::new())
            .await;
        assert!(
            res.is_err(),
            "an error before any content must fail the turn"
        );

        // Regression: no empty {role:assistant, content:[]} row may be persisted.
        let msgs = get_messages(&pool, &sid).await.unwrap();
        assert!(
            !msgs
                .iter()
                .any(|m| serde_json::from_str::<ChatMessage>(&m.data)
                    .map(|cm| cm.content.is_empty())
                    .unwrap_or(false)),
            "a content-less errored turn must not persist an empty-content message"
        );
    }
}
