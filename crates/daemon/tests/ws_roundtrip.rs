//! Full WS-over-HTTP round-trip against the real axum server with a mock
//! provider: session.prompt -> streamed events -> tool.permission_required ->
//! permission.reply -> tool.output -> message.completed, then a reconnect with
//! after_seq that replays the durable completion exactly once. Fully offline.

use futures_util::{SinkExt, StreamExt};
use private_code_core::db;
use private_code_protocol::event::UsageStats;
use private_code_providers::testkit::ScriptedProvider;
use private_code_providers::ModelProvider;
use private_code_providers::ProviderEvent;
use private_code_tools::{ToolRegistry, WriteFileTool};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Read the next JSON event (a message carrying a "type"), skipping JSON-RPC
/// responses (which have no "type"), until the deadline.
async fn next_event<S>(ws: &mut S, deadline: tokio::time::Instant) -> Option<Value>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    if v.get("type").is_some() {
                        return Some(v);
                    }
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => return None,
        }
    }
}

#[tokio::test]
async fn ws_round_trip_prompt_permission_output_completed_and_replay() {
    let ws_dir = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let ws_path = ws_dir.path().to_str().unwrap();

    let pool = db::connect_db("sqlite::memory:").await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    let pid = Uuid::new_v4().to_string();
    db::create_project(&pool, &pid, "t", ws_path).await.unwrap();
    let sid = Uuid::new_v4().to_string();
    let cfg = json!({"provider_id":"anthropic","model_id":"claude-opus-4-8"}).to_string();
    db::create_session(&pool, &sid, &pid, ws_path, ws_path, "t", "build", &cfg)
        .await
        .unwrap();

    // Turn 1: a write_file tool call (build agent => Ask). Turn 2: text + end.
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptedProvider::new(vec![
        vec![
            ProviderEvent::ToolUseStart {
                id: "tu1".into(),
                name: "write_file".into(),
            },
            ProviderEvent::ToolUseComplete {
                id: "tu1".into(),
                name: "write_file".into(),
                input: json!({ "path": "out.txt", "content": "hi" }),
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
    reg.register(Box::new(WriteFileTool));
    let registry = Arc::new(reg);

    let token = private_code_daemon::auth::get_or_create_token(data_dir.path()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let sd = shutdown.clone();
    let dir = data_dir.path().to_path_buf();
    let server = tokio::spawn(async move {
        let _ = private_code_daemon::start_daemon_with(
            pool,
            dir,
            provider,
            vec![],
            registry,
            None,
            listener,
            sd,
        )
        .await;
    });

    let url = format!(
        "ws://127.0.0.1:{}/ws?session_id={}&token={}",
        addr.port(),
        sid,
        token
    );

    // ---- First connection: drive the turn through the permission round-trip. ----
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":1,"method":"session.prompt","params":{"prompt":"please write","delivery":"queue"}}).to_string(),
    ))
    .await
    .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);

    // Expect a tool.permission_required with a non-empty permission_id.
    let mut permission_id = None;
    let mut perm_seq = 0i64;
    while let Some(ev) = next_event(&mut ws, deadline).await {
        if ev["type"] == "tool_permission_required" {
            permission_id = ev["permission_id"].as_str().map(|s| s.to_string());
            perm_seq = ev["seq"].as_i64().unwrap_or(0);
            break;
        }
    }
    let permission_id =
        permission_id.expect("a tool.permission_required event with a permission_id");
    assert!(!permission_id.is_empty());

    // Reply "once" to allow the tool.
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":2,"method":"permission.reply","params":{"permission_id":permission_id,"reply":"once"}}).to_string(),
    ))
    .await
    .unwrap();

    // Expect tool.output (ok) then a message.completed with seq > the permission seq.
    let mut saw_tool_output = false;
    let mut completed_seq = None;
    while let Some(ev) = next_event(&mut ws, deadline).await {
        match ev["type"].as_str() {
            Some("tool_output") => {
                assert_eq!(ev["is_error"], json!(false), "tool ran without error");
                saw_tool_output = true;
            }
            Some("message_completed") => {
                let seq = ev["seq"].as_i64().unwrap_or(0);
                if saw_tool_output && seq > perm_seq {
                    completed_seq = Some(seq);
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(
        saw_tool_output,
        "tool.output must arrive after the permission reply"
    );
    let completed_seq = completed_seq.expect("a message.completed after the tool output");

    // The tool actually wrote the file.
    assert_eq!(
        std::fs::read_to_string(ws_dir.path().join("out.txt")).unwrap(),
        "hi"
    );

    drop(ws);

    // ---- Reconnect with after_seq: the durable completion replays exactly once. ----
    let reconnect_url = format!(
        "ws://127.0.0.1:{}/ws?session_id={}&token={}&after_seq={}",
        addr.port(),
        sid,
        token,
        completed_seq - 1
    );
    let (mut ws2, _) = tokio_tungstenite::connect_async(&reconnect_url)
        .await
        .unwrap();

    // Read the replayed history (a short burst), then assert exactly one
    // message.completed for `completed_seq`.
    let replay_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut completed_count = 0;
    while let Some(ev) = next_event(&mut ws2, replay_deadline).await {
        if ev["type"] == "message_completed" && ev["seq"].as_i64() == Some(completed_seq) {
            completed_count += 1;
        }
    }
    assert_eq!(
        completed_count, 1,
        "the durable message.completed must be replayed exactly once on reconnect"
    );

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
}
