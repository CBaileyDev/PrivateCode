use crate::checkpoint::{GitSnapshotEngine, Snapshot};
use crate::context::{Reconcile, SystemContextRegistry};
use crate::db::{self};
use crate::permissions::{self, PermissionDecision, PermissionPrompt, PermissionRule};
use private_code_protocol::event::{DeltaPayload, ProtocolEvent, UsageStats};
use private_code_protocol::message::{ChatMessage, ContentBlock, Role, ToolResultContent};
use private_code_providers::provider::{ModelProvider, ProviderEvent};
use private_code_tools::tool::{ToolContext, ToolRegistry};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
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
                if had_epoch {
                    db::replace_context_epoch(
                        &mut tx,
                        session_id,
                        &session.agent_id,
                        &baseline,
                        &snap_json,
                        bseq,
                        revision,
                    )
                    .await?;
                } else {
                    db::insert_context_epoch(
                        &mut tx,
                        session_id,
                        &session.agent_id,
                        &baseline,
                        &snap_json,
                        bseq,
                    )
                    .await?;
                }
                tx.commit().await?;
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
                db::advance_context_epoch(&mut tx, session_id, &snap_json, revision).await?;
                tx.commit().await?;
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

        // Parse model config. The provider resolves its own API key internally.
        let model_val: serde_json::Value = serde_json::from_str(&session.model_config)?;
        let model_id = model_val["model_id"].as_str().unwrap_or("claude-opus-4-8");
        let max_tokens = model_val["max_tokens"].as_u64().unwrap_or(8192) as u32;

        let mut turn_count = 0;
        let max_turns = 25;

        // Initialize file read cache
        let mut file_read_cache = HashMap::new();

        while turn_count < max_turns {
            // Stop cleanly if an abort RPC arrived between turns.
            if cancel.is_cancelled() {
                info!("Turn loop cancelled before turn {}", turn_count + 1);
                break;
            }
            turn_count += 1;

            // Fetch current messages
            let db_msgs = db::get_messages(&self.pool, session_id).await?;
            let mut chat_msgs = Vec::new();
            for m in db_msgs {
                let cm: ChatMessage = serde_json::from_str(&m.data)?;
                chat_msgs.push(cm);
            }

            // Expose schemas
            let tool_schemas = self.tool_registry.list_schemas();

            // Run provider chat. The epoch baseline is the cached top-level system
            // prompt; mid-conversation deltas live in `chat_msgs` as system messages.
            let stream_res = self
                .provider
                .stream_chat(
                    model_id,
                    system_prompt.as_deref(),
                    max_tokens,
                    &chat_msgs,
                    &tool_schemas,
                )
                .await;

            let mut stream = match stream_res {
                Ok(s) => s,
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
            };

            let assistant_msg_id = Uuid::new_v4().to_string();
            let mut accumulated_text = String::new();
            let mut accumulated_reasoning = String::new();

            // Map tool_use_id to (tool_name, accumulated_input)
            let mut accumulated_tool_uses: HashMap<String, (String, String)> = HashMap::new();
            let mut final_usage = UsageStats::default();

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
                        error!("Stream delta error: {}", e);
                        continue;
                    }
                    None => break,
                };

                match event {
                    ProviderEvent::TextDelta(text) => {
                        accumulated_text.push_str(&text);
                        self.emit_delta(ProtocolEvent::MessageDelta {
                            session_id: session_id.to_string(),
                            delta: DeltaPayload::Text { text },
                        });
                    }
                    ProviderEvent::ReasoningDelta(reasoning) => {
                        accumulated_reasoning.push_str(&reasoning);
                        self.emit_delta(ProtocolEvent::MessageDelta {
                            session_id: session_id.to_string(),
                            delta: DeltaPayload::Reasoning { reasoning },
                        });
                    }
                    ProviderEvent::ToolUseStart { id, name } => {
                        accumulated_tool_uses.insert(id.clone(), (name.clone(), String::new()));
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
                        if let Some((_, input)) = accumulated_tool_uses.get_mut(&id) {
                            input.push_str(&input_delta);
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
                        accumulated_tool_uses.insert(id, (name, input.to_string()));
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

            // Construct content blocks
            let mut content_blocks = Vec::new();
            if !accumulated_text.is_empty() {
                content_blocks.push(ContentBlock::Text {
                    text: accumulated_text,
                });
            }
            if !accumulated_reasoning.is_empty() {
                content_blocks.push(ContentBlock::Reasoning {
                    reasoning: accumulated_reasoning,
                });
            }
            for (id, (name, input_str)) in &accumulated_tool_uses {
                let input_val = serde_json::from_str(input_str).unwrap_or(serde_json::Value::Null);
                content_blocks.push(ContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input_val,
                });
            }

            let assistant_msg = ChatMessage {
                id: assistant_msg_id.clone(),
                role: Role::Assistant,
                content: content_blocks.clone(),
                created_at: chrono::Utc::now().timestamp(),
            };

            // Save assistant message
            let mut tx = self.pool.begin().await?;
            let asst_seq = db::next_sequence(&mut tx, session_id).await?;
            let asst_json = serde_json::to_string(&assistant_msg)?;
            db::append_message(
                &mut tx,
                &assistant_msg_id,
                session_id,
                asst_seq,
                "assistant",
                &asst_json,
            )
            .await?;

            // Update stats
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

            self.event_tx
                .send(ProtocolEvent::MessageCompleted {
                    session_id: session_id.to_string(),
                    seq: asst_seq,
                    message_id: assistant_msg_id.clone(),
                    usage: final_usage.clone(),
                })
                .await
                .ok();

            // If the turn was cancelled mid-stream, the partial assistant message
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
                            match resp_rx.await {
                                Ok(PermissionDecision::Allow) => true,
                                Ok(PermissionDecision::Deny) => false,
                                _ => false,
                            }
                        }
                    }
                };

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
                let execute_res = tool.run(&mut tool_ctx, arguments.clone()).await;

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
            }

            // Persist the tool results as a single user message
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

        Ok(())
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
}
