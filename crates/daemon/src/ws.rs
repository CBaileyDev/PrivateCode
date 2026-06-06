use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::IntoResponse,
};
use private_code_core::coordinator::SessionCoordinator;
use private_code_protocol::event::ProtocolEvent;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Deserialize)]
pub struct WsQuery {
    pub session_id: String,
    pub after_seq: Option<i64>,
}

#[derive(Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: serde_json::Value,
    method: String,
    params: serde_json::Value,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Serialize, Clone)]
struct JsonRpcError {
    code: i32,
    message: String,
}

async fn get_event_rx(
    coord: &SessionCoordinator,
    session_id: &str,
) -> Result<tokio::sync::broadcast::Receiver<ProtocolEvent>, String> {
    coord
        .get_or_create_session(session_id)
        .await
        .map_err(|e| e.to_string())
}

async fn get_session_history(
    coord: &SessionCoordinator,
    session_id: &str,
    after_seq: i64,
) -> Result<Vec<ProtocolEvent>, String> {
    coord
        .get_history(session_id, after_seq)
        .await
        .map_err(|e| e.to_string())
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    State(coord): State<Arc<SessionCoordinator>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, query, coord))
}

async fn handle_socket(socket: WebSocket, query: WsQuery, coord: Arc<SessionCoordinator>) {
    use futures_util::{SinkExt, StreamExt};

    let (mut ws_tx, mut ws_rx) = socket.split();

    // Subscribe to session event stream
    let event_rx_str = get_event_rx(&coord, &query.session_id).await;

    let mut event_rx = match event_rx_str {
        Ok(rx) => rx,
        Err(err_msg) => {
            let err_resp = JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::Value::Null,
                result: None,
                error: Some(JsonRpcError {
                    code: -32000,
                    message: format!("Session error: {}", err_msg),
                }),
            };
            if let Ok(msg) = serde_json::to_string(&err_resp) {
                let _ = ws_tx.send(Message::Text(msg)).await;
            }
            return;
        }
    };

    // Replay history. Track the highest durable seq replayed so the live forward
    // loop below can dedup: an event emitted between our subscribe (above) and
    // this snapshot is in BOTH the broadcast buffer and the replay, so without a
    // watermark it would be delivered twice.
    let after_seq = query.after_seq.unwrap_or(0);
    let mut replay_watermark = after_seq;
    if let Ok(history) = get_session_history(&coord, &query.session_id, after_seq).await {
        for event in history {
            replay_watermark =
                replay_watermark.max(private_code_core::coordinator::event_seq(&event));
            if let Ok(msg_str) = serde_json::to_string(&event) {
                if ws_tx.send(Message::Text(msg_str)).await.is_err() {
                    return;
                }
            }
        }
    }

    // Spawn task to read from WS and process commands
    let session_id_str = query.session_id.clone();
    let coord_clone = coord.clone();
    let (command_tx, mut command_rx) = tokio::sync::mpsc::channel::<Message>(100);

    let mut ws_tx_shared = ws_tx;
    tokio::spawn(async move {
        while let Some(msg) = command_rx.recv().await {
            if ws_tx_shared.send(msg).await.is_err() {
                break;
            }
        }
    });

    let command_tx_clone = command_tx.clone();
    let coord_fwd = coord.clone();
    let sid_fwd = query.session_id.clone();
    // Monotonic high-water of durable seqs forwarded so far (seeded from the
    // replay). Advancing it on every forwarded durable event makes the live arm
    // and the lag-recovery arm below dedup against each other.
    let mut watermark = replay_watermark;
    tokio::spawn(async move {
        use private_code_core::coordinator::{durable_after, event_seq, should_forward};
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    // Dedup: skip any durable event already delivered (via replay or
                    // a prior lag-recovery).
                    if !should_forward(&event, watermark) {
                        continue;
                    }
                    watermark = watermark.max(event_seq(&event));
                    if let Ok(msg_str) = serde_json::to_string(&event) {
                        if command_tx_clone.send(Message::Text(msg_str)).await.is_err() {
                            break;
                        }
                    }
                }
                // A burst of ephemeral deltas overflowed this connected client's
                // 16384 ring view, evicting interleaved durable events (Error,
                // ToolPermissionRequired) it still needs — and those two have NO DB
                // content home, so DB reconciliation cannot recover them. Replay the
                // durable events the client missed from the live in-memory history,
                // then KEEP streaming. The session is necessarily still live here (an
                // idle-eviction drops the sender, yielding Closed rather than Lagged).
                Err(RecvError::Lagged(_)) => {
                    let missed = {
                        let map = coord_fwd.sessions.lock().await;
                        map.get(&sid_fwd)
                            .map(|s| durable_after(&s.history, watermark))
                            .unwrap_or_default()
                    };
                    for event in missed {
                        watermark = watermark.max(event_seq(&event));
                        if let Ok(msg_str) = serde_json::to_string(&event) {
                            if command_tx_clone.send(Message::Text(msg_str)).await.is_err() {
                                return;
                            }
                        }
                    }
                }
                Err(RecvError::Closed) => break,
            }
        }
    });

    while let Some(result) = ws_rx.next().await {
        let msg = match result {
            Ok(m) => m,
            Err(_) => break,
        };

        if let Message::Text(text) = msg {
            if let Ok(req) = serde_json::from_str::<JsonRpcRequest>(&text) {
                let coord = coord_clone.clone();
                let session_id = session_id_str.clone();
                let command_tx = command_tx.clone();

                tokio::spawn(async move {
                    let res = process_rpc(req.method, req.params, &coord, &session_id).await;
                    let resp = match res {
                        Ok(res_val) => JsonRpcResponse {
                            jsonrpc: "2.0".to_string(),
                            id: req.id,
                            result: Some(res_val),
                            error: None,
                        },
                        Err(err) => JsonRpcResponse {
                            jsonrpc: "2.0".to_string(),
                            id: req.id,
                            result: None,
                            error: Some(err),
                        },
                    };
                    if let Ok(resp_str) = serde_json::to_string(&resp) {
                        let _ = command_tx.send(Message::Text(resp_str)).await;
                    }
                });
            }
        } else if let Message::Ping(payload) = msg {
            let _ = command_tx.send(Message::Pong(payload)).await;
        }
    }
}

async fn process_rpc(
    method: String,
    params: serde_json::Value,
    coord: &SessionCoordinator,
    session_id: &str,
) -> Result<serde_json::Value, JsonRpcError> {
    match method.as_str() {
        "session.prompt" => {
            let prompt = params["prompt"].as_str().ok_or(JsonRpcError {
                code: -32602,
                message: "Missing prompt parameter".to_string(),
            })?;
            let delivery = params["delivery"].as_str().unwrap_or("steer");

            coord
                .run_turn(session_id, prompt, delivery)
                .await
                .map_err(|e| JsonRpcError {
                    code: -32000,
                    message: e.to_string(),
                })?;

            Ok(serde_json::json!({"status": "accepted"}))
        }
        "session.abort" => {
            coord
                .abort_turn(session_id)
                .await
                .map_err(|e| JsonRpcError {
                    code: -32000,
                    message: e.to_string(),
                })?;
            Ok(serde_json::json!({"status": "aborted"}))
        }
        "permission.reply" => {
            let perm_id = params["permission_id"].as_str().ok_or(JsonRpcError {
                code: -32602,
                message: "Missing permission_id parameter".to_string(),
            })?;
            let reply = params["reply"].as_str().ok_or(JsonRpcError {
                code: -32602,
                message: "Missing reply parameter".to_string(),
            })?;
            // Optional free-text feedback the model sees when a request is denied.
            let feedback = params["feedback"].as_str();

            // The "always" rule is persisted by `reply_permission` itself — it
            // captures the action/resource under the sessions lock and writes the
            // rule AFTER releasing it (coordinator.rs). A duplicate save here once
            // existed and was actively harmful: it held `coord.sessions.lock()`
            // ACROSS a `db::save_permission().await` (the lock-across-await pattern
            // C9 eliminated everywhere else, blocking event routing and every other
            // session for the duration of the write), on top of being redundant.
            coord
                .reply_permission(session_id, perm_id, reply, feedback)
                .await
                .map_err(|e| JsonRpcError {
                    code: -32000,
                    message: e.to_string(),
                })?;

            Ok(serde_json::json!({"status": "ok"}))
        }
        _ => Err(JsonRpcError {
            code: -32601,
            message: format!("Method not found: {}", method),
        }),
    }
}
