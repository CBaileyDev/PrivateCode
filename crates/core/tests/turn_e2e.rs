//! End-to-end turn test over a REAL git workspace. Unlike the in-module unit
//! tests (non-git temp dir, dropped permission receiver, drained events), this
//! exercises the paths the task requires: git checkpoints actually fire, the
//! interactive Ask->Allow permission round-trip runs, and the emitted event
//! sequence + persisted messages + checkpoint restore are all asserted.

use async_trait::async_trait;
use private_code_core::checkpoint::{GitSnapshotEngine, Snapshot, TreeHash};
use private_code_core::db;
use private_code_core::orchestrator::Orchestrator;
use private_code_core::permissions::{PermissionDecision, PermissionPrompt};
use private_code_protocol::event::{ProtocolEvent, UsageStats};
use private_code_protocol::message::{ChatMessage, ContentBlock, Role};
use private_code_providers::ProviderEvent;
use private_code_providers::testkit::ScriptedProvider;
use private_code_tools::{Tool, ToolContext, ToolError, ToolRegistry};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// A mutating write tool (permission_class "write_file" -> Ask under the build
/// agent) that writes a file into the workspace so pre/post-step checkpoints fire.
struct WriteMockTool;

#[async_trait]
impl Tool for WriteMockTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "writes a file"
    }
    fn schema(&self) -> Value {
        json!({
            "name": "write_file",
            "input_schema": {
                "type": "object",
                "properties": { "path": {"type": "string"}, "content": {"type": "string"} },
                "required": ["path", "content"]
            }
        })
    }
    fn mutates(&self) -> bool {
        true
    }
    fn permission_class(&self) -> &str {
        "write_file"
    }
    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, ToolError> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("path".into()))?;
        let content = args["content"].as_str().unwrap_or("");
        std::fs::write(ctx.workspace_path.join(path), content).map_err(ToolError::Io)?;
        Ok(json!({ "status": "written", "path": path }))
    }
}

fn init_git_repo(path: &Path) {
    let repo = git2::Repository::init(path).unwrap();
    {
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test User").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();
    }
    // Initial commit so HEAD/index are valid.
    std::fs::write(path.join("README.md"), "init\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    index.write().unwrap();
    let oid = index.write_tree().unwrap();
    let sig = repo.signature().unwrap();
    let tree = repo.find_tree(oid).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();
}

#[tokio::test]
async fn git_backed_turn_persists_messages_events_and_checkpoints() {
    let ws_dir = TempDir::new().unwrap();
    let ws = ws_dir.path();
    let data_dir = TempDir::new().unwrap();
    let data_path = data_dir.path().to_path_buf();
    init_git_repo(ws);

    // DB + project + session (build agent => write_file is Ask).
    let pool = db::connect_db("sqlite::memory:").await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    let project_id = Uuid::new_v4().to_string();
    db::create_project(&pool, &project_id, "t", ws.to_str().unwrap())
        .await
        .unwrap();
    let session_id = Uuid::new_v4().to_string();
    let model_config = json!({"provider_id":"anthropic","model_id":"claude-opus-4-8"}).to_string();
    db::create_session(
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

    // Provider: turn 1 calls write_file; turn 2 (after tool_result) ends.
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            ProviderEvent::ToolUseStart {
                id: "tu1".into(),
                name: "write_file".into(),
            },
            ProviderEvent::ToolUseComplete {
                id: "tu1".into(),
                name: "write_file".into(),
                input: json!({ "path": "out.txt", "content": "hello" }),
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
    ]));

    let mut reg = ToolRegistry::new();
    reg.register(Box::new(WriteMockTool));

    let (ptx, mut prx) =
        mpsc::channel::<(PermissionPrompt, oneshot::Sender<PermissionDecision>)>(10);
    let (etx, mut erx) = mpsc::channel::<ProtocolEvent>(4096);

    // Permission responder: auto-Allow every prompt (exercises Ask->Allow).
    let responder = tokio::spawn(async move {
        while let Some((_prompt, resp_tx)) = prx.recv().await {
            let _ = resp_tx.send(PermissionDecision::Allow);
        }
    });

    // Event collector.
    let events = Arc::new(StdMutex::new(Vec::<ProtocolEvent>::new()));
    let events2 = events.clone();
    let collector = tokio::spawn(async move {
        while let Some(ev) = erx.recv().await {
            events2.lock().unwrap().push(ev);
        }
    });

    let orch = Orchestrator::new(
        pool.clone(),
        data_path.clone(),
        provider,
        Arc::new(reg),
        ptx,
        etx,
    );
    let input_id = orch
        .admit_input(&session_id, "please write", "steer")
        .await
        .unwrap();
    orch.run_session_turn(&session_id, &input_id, CancellationToken::new())
        .await
        .unwrap();

    // Drop the orchestrator so the channels close and the helper tasks finish.
    drop(orch);
    let _ = responder.await;
    let _ = collector.await;
    let events = Arc::try_unwrap(events).unwrap().into_inner().unwrap();

    // (a) Persisted messages: user prompt, assistant tool_use, user tool_result
    //     (is_error=false), and a final assistant message containing "done".
    let msgs = db::get_messages(&pool, &session_id).await.unwrap();
    let parsed: Vec<ChatMessage> = msgs
        .iter()
        .filter_map(|m| serde_json::from_str(&m.data).ok())
        .collect();
    assert!(
        parsed.iter().any(|m| m.role == Role::User
            && m.content.iter().any(
                |c| matches!(c, ContentBlock::Text { text } if text.contains("please write"))
            )),
        "user prompt persisted"
    );
    assert!(
        parsed.iter().any(|m| m.role == Role::Assistant
            && m.content
                .iter()
                .any(|c| matches!(c, ContentBlock::ToolUse { .. }))),
        "assistant tool_use persisted"
    );
    assert!(
        parsed.iter().any(|m| m
            .content
            .iter()
            .any(|c| matches!(c, ContentBlock::ToolResult { is_error, .. } if !*is_error))),
        "non-error tool_result persisted"
    );
    assert!(
        parsed.iter().any(|m| m.role == Role::Assistant
            && m.content
                .iter()
                .any(|c| matches!(c, ContentBlock::Text { text } if text.contains("done")))),
        "final assistant text persisted"
    );

    // (b) >=3 checkpoints with the three kinds and non-empty tree hashes.
    let cps = db::list_checkpoints(&pool, &session_id).await.unwrap();
    assert!(
        cps.len() >= 3,
        "expected >=3 checkpoints, got {}",
        cps.len()
    );
    let kinds: std::collections::HashSet<&str> = cps.iter().map(|c| c.kind.as_str()).collect();
    for k in ["turn_start", "pre_step", "post_step"] {
        assert!(kinds.contains(k), "missing checkpoint kind {k}");
    }
    assert!(
        cps.iter().all(|c| !c.tree_hash.is_empty()),
        "tree hashes non-empty"
    );

    // (c) Event sequence: ToolRequested -> ToolPermissionRequired -> ToolOutput(ok)
    //     -> a post_step CheckpointCreated -> MessageCompleted.
    let tool_requested_idx = events
        .iter()
        .position(|e| matches!(e, ProtocolEvent::ToolRequested { .. }));
    let tool_output_idx = events
        .iter()
        .position(|e| matches!(e, ProtocolEvent::ToolOutput { .. }));
    assert!(
        tool_requested_idx.is_some(),
        "a durable ToolRequested must be emitted"
    );
    assert!(
        tool_requested_idx < tool_output_idx,
        "ToolRequested must precede ToolOutput"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProtocolEvent::ToolPermissionRequired { .. })),
        "ToolPermissionRequired emitted"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProtocolEvent::ToolOutput { is_error, .. } if !*is_error)),
        "ToolOutput(ok) emitted"
    );
    assert!(
        events.iter().any(
            |e| matches!(e, ProtocolEvent::CheckpointCreated { kind, .. } if kind == "post_step")
        ),
        "post_step CheckpointCreated emitted"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProtocolEvent::MessageCompleted { .. })),
        "MessageCompleted emitted"
    );

    // (d) The mutating tool actually wrote the file.
    assert_eq!(
        std::fs::read_to_string(ws.join("out.txt")).unwrap(),
        "hello"
    );

    // (e) Restoring the turn_start tree removes the file (proves it's a usable
    //     restore point taken before the write).
    let turn_start = cps.iter().find(|c| c.kind == "turn_start").unwrap();
    let engine = GitSnapshotEngine::new(&session_id, ws, &data_path, true);
    engine
        .restore(&TreeHash(turn_start.tree_hash.clone()))
        .await
        .unwrap();
    assert!(
        !ws.join("out.txt").exists(),
        "restore removed the post-turn file"
    );
}
