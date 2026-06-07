//! Proactive + reactive compaction over the real turn loop, fully offline.
//! Verifies: a budget-exceeding history is folded into a summary marker (older
//! messages, including a pre-boundary tool_use, no longer reach the provider, and
//! NO durable rows are deleted); a context-overflow 400 triggers exactly one
//! compact-and-retry; and a streaming `model_context_window_exceeded` finish is a
//! SUCCESS that never compacts.

use async_trait::async_trait;
use futures_util::stream::{BoxStream, StreamExt};
use private_code_core::coordinator::{shared_ecosystem, shared_tool_registry};
use private_code_core::db;
use private_code_core::orchestrator::Orchestrator;
use private_code_core::permissions::{PermissionPrompt, PermissionReply};
use private_code_protocol::event::{ProtocolEvent, UsageStats};
use private_code_protocol::message::{ChatMessage, ContentBlock, Role};
use private_code_providers::provider::{ModelProvider, ProviderError, ProviderEvent};
use private_code_providers::testkit::ScriptedProvider;
use private_code_tools::ToolRegistry;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct Harness {
    orch: Orchestrator,
    pool: sqlx::SqlitePool,
    sid: String,
    events: mpsc::Receiver<ProtocolEvent>,
    _ws: tempfile::TempDir,
    _data: tempfile::TempDir,
}

async fn setup(provider: Arc<dyn ModelProvider>, project_config_json: &str) -> Harness {
    let ws = tempfile::tempdir().unwrap();
    if !project_config_json.is_empty() {
        let cfg = ws.path().join(".private-code");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("config.json"), project_config_json).unwrap();
    }
    let data = tempfile::tempdir().unwrap();

    let pool = db::connect_db("sqlite::memory:").await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    let pid = Uuid::new_v4().to_string();
    db::create_project(&pool, &pid, "t", ws.path().to_str().unwrap())
        .await
        .unwrap();
    let sid = Uuid::new_v4().to_string();
    let model_config =
        serde_json::json!({"provider_id":"anthropic","model_id":"claude-opus-4-8"}).to_string();
    db::create_session(
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

    let (ptx, _prx) = mpsc::channel::<(PermissionPrompt, oneshot::Sender<PermissionReply>)>(10);
    let (etx, erx) = mpsc::channel::<ProtocolEvent>(4096);

    let orch = Orchestrator::new(
        pool.clone(),
        data.path().to_path_buf(),
        provider,
        shared_tool_registry(ToolRegistry::new()),
        shared_ecosystem(None),
        ptx,
        etx,
    );
    Harness {
        orch,
        pool,
        sid,
        events: erx,
        _ws: ws,
        _data: data,
    }
}

/// Append a durable message using the real sequence counter (so the
/// orchestrator's own next_sequence stays consistent).
async fn seed(pool: &sqlx::SqlitePool, sid: &str, type_: &str, msg: &ChatMessage) {
    let mut tx = pool.begin().await.unwrap();
    let seq = db::next_sequence(&mut tx, sid).await.unwrap();
    let data = serde_json::to_string(msg).unwrap();
    db::append_message(&mut tx, &msg.id, sid, seq, type_, &data)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

fn user_text(id: &str, text: &str) -> ChatMessage {
    ChatMessage {
        id: id.into(),
        role: Role::User,
        content: vec![ContentBlock::Text { text: text.into() }],
        created_at: 0,
    }
}
fn asst_text(id: &str, text: &str) -> ChatMessage {
    ChatMessage {
        id: id.into(),
        role: Role::Assistant,
        content: vec![ContentBlock::Text { text: text.into() }],
        created_at: 0,
    }
}

fn end_turn() -> ProviderEvent {
    ProviderEvent::MessageStop {
        usage: UsageStats::default(),
        finish_reason: Some("end_turn".into()),
    }
}

#[tokio::test]
async fn proactive_compaction_folds_old_turns_keeps_rows_and_drops_pre_boundary_tooluse() {
    // Aggressive budget so a small history exceeds it; keep only the newest turn.
    let provider = Arc::new(
        ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("ok".into()),
            end_turn(),
        ]])
        .with_token_override(1000),
    );
    let h = setup(
        provider.clone(),
        r#"{"compaction":{"auto":true,"buffer_tokens":190000,"keep_tokens":2000}}"#,
    )
    .await;

    // Seed prior history including a (fulfilled) tool_use that will be dropped.
    seed(&h.pool, &h.sid, "user", &user_text("u1", "first question")).await;
    seed(
        &h.pool,
        &h.sid,
        "assistant",
        &ChatMessage {
            id: "a1".into(),
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "old_tool".into(),
                name: "x".into(),
                input: serde_json::json!({"a":1}),
            }],
            created_at: 0,
        },
    )
    .await;
    seed(
        &h.pool,
        &h.sid,
        "user",
        &ChatMessage {
            id: "tr1".into(),
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "old_tool".into(),
                content: private_code_protocol::message::ToolResultContent::Text("done".into()),
                is_error: false,
            }],
            created_at: 0,
        },
    )
    .await;
    seed(&h.pool, &h.sid, "user", &user_text("u2", "second question")).await;
    seed(
        &h.pool,
        &h.sid,
        "assistant",
        &asst_text("a2", "second answer"),
    )
    .await;

    let input_id = h
        .orch
        .admit_input(&h.sid, "newest question", "steer")
        .await
        .unwrap();
    h.orch
        .run_session_turn(&h.sid, &input_id, CancellationToken::new())
        .await
        .unwrap();

    // (1) A compaction marker row was written.
    let msgs = db::get_messages(&h.pool, &h.sid).await.unwrap();
    assert!(
        msgs.iter().any(|m| m.type_ == "compaction"),
        "a compaction marker row must exist"
    );
    // (2) No durable rows were deleted — the dropped tool_use row still exists.
    assert!(
        msgs.iter().any(|m| m.data.contains("old_tool")),
        "the original tool_use row must survive (rows are never deleted)"
    );

    // (3) The request actually sent after compaction: a System summary is present,
    //     and the pre-boundary tool_use no longer reaches the provider.
    let calls = provider.calls();
    let last = calls.last().expect("at least one provider call");
    assert!(
        last.messages.iter().any(|m| m.role == Role::System
            && m.content.iter().any(
                |c| matches!(c, ContentBlock::Text { text } if text.contains("auto-compacted"))
            )),
        "the compaction summary must be prepended as a System message"
    );
    assert!(
        !last.messages.iter().any(|m| m
            .content
            .iter()
            .any(|c| matches!(c, ContentBlock::ToolUse { id, .. } if id == "old_tool"))),
        "the pre-boundary tool_use must NOT be in the post-compaction request"
    );
    assert!(
        last.messages
            .iter()
            .any(|m| m.content.iter().any(
                |c| matches!(c, ContentBlock::Text { text } if text.contains("newest question"))
            )),
        "the newest turn must still be in the request"
    );
}

/// First stream_chat returns a context-overflow 400; the second succeeds.
struct FourHundredThenOk {
    calls: AtomicUsize,
}

#[async_trait]
impl ModelProvider for FourHundredThenOk {
    async fn stream_chat(
        &self,
        _model_id: &str,
        _system_prompt: Option<&str>,
        _max_tokens: u32,
        _messages: &[ChatMessage],
        _tools: &[serde_json::Value],
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Err(ProviderError::Api {
                status: 400,
                message: "prompt is too long: 250000 tokens > 200000 maximum".into(),
            })
        } else {
            let evs = vec![ProviderEvent::TextDelta("recovered".into()), end_turn()];
            Ok(futures_util::stream::iter(evs.into_iter().map(Ok)).boxed())
        }
    }
    fn count_tokens(&self, _m: &str, t: &str) -> usize {
        (t.chars().count() / 4).max(1)
    }
}

#[tokio::test]
async fn context_overflow_400_triggers_one_compact_and_retry() {
    let provider = Arc::new(FourHundredThenOk {
        calls: AtomicUsize::new(0),
    });
    // Normal budget so proactive compaction does NOT fire — only the 400 path does.
    let h = setup(provider.clone(), r#"{"compaction":{"auto":true}}"#).await;

    // Enough droppable history for perform_compaction to find a safe cut.
    seed(&h.pool, &h.sid, "user", &user_text("u1", "q1")).await;
    seed(&h.pool, &h.sid, "assistant", &asst_text("a1", "a1")).await;
    seed(&h.pool, &h.sid, "user", &user_text("u2", "q2")).await;
    seed(&h.pool, &h.sid, "assistant", &asst_text("a2", "a2")).await;

    let input_id = h.orch.admit_input(&h.sid, "q3", "steer").await.unwrap();
    h.orch
        .run_session_turn(&h.sid, &input_id, CancellationToken::new())
        .await
        .unwrap();

    // Exactly two provider calls (the 400, then the successful retry).
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "one retry after the 400"
    );
    let msgs = db::get_messages(&h.pool, &h.sid).await.unwrap();
    assert_eq!(
        msgs.iter().filter(|m| m.type_ == "compaction").count(),
        1,
        "exactly one compaction"
    );
    assert!(
        msgs.iter().any(|m| m.data.contains("recovered")),
        "the retried turn completed and persisted its assistant message"
    );
}

/// A successful but truncated stream (finish_reason model_context_window_exceeded).
struct TruncatedSuccessProvider;

#[async_trait]
impl ModelProvider for TruncatedSuccessProvider {
    async fn stream_chat(
        &self,
        _model_id: &str,
        _system_prompt: Option<&str>,
        _max_tokens: u32,
        _messages: &[ChatMessage],
        _tools: &[serde_json::Value],
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError> {
        let evs = vec![
            ProviderEvent::TextDelta("truncated answer".into()),
            ProviderEvent::MessageStop {
                usage: UsageStats::default(),
                finish_reason: Some("model_context_window_exceeded".into()),
            },
        ];
        Ok(futures_util::stream::iter(evs.into_iter().map(Ok)).boxed())
    }
    fn count_tokens(&self, _m: &str, t: &str) -> usize {
        (t.chars().count() / 4).max(1)
    }
}

#[tokio::test]
async fn truncation_finish_reason_is_success_and_never_compacts() {
    let mut h = setup(
        Arc::new(TruncatedSuccessProvider),
        r#"{"compaction":{"auto":true}}"#,
    )
    .await;
    seed(&h.pool, &h.sid, "user", &user_text("u1", "q1")).await;
    seed(&h.pool, &h.sid, "assistant", &asst_text("a1", "a1")).await;

    let input_id = h.orch.admit_input(&h.sid, "q2", "steer").await.unwrap();
    let res = h
        .orch
        .run_session_turn(&h.sid, &input_id, CancellationToken::new())
        .await;
    assert!(
        res.is_ok(),
        "a truncated-but-complete stream is a successful turn"
    );

    let msgs = db::get_messages(&h.pool, &h.sid).await.unwrap();
    assert!(
        !msgs.iter().any(|m| m.type_ == "compaction"),
        "model_context_window_exceeded must NOT trigger compaction"
    );
    assert!(
        msgs.iter().any(|m| m.data.contains("truncated answer")),
        "the truncated answer is persisted"
    );

    let mut completed_finish_reason = None;
    while let Ok(Some(event)) =
        tokio::time::timeout(std::time::Duration::from_millis(100), h.events.recv()).await
    {
        if let ProtocolEvent::MessageCompleted { finish_reason, .. } = event {
            completed_finish_reason = finish_reason;
            break;
        }
    }
    assert_eq!(
        completed_finish_reason.as_deref(),
        Some("model_context_window_exceeded"),
        "MessageCompleted must surface the provider stop reason"
    );
}
