use crate::coordinator::SessionCoordinator;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::IntoResponse,
};
use private_code_core::db;
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
            replay_watermark = replay_watermark.max(crate::coordinator::event_seq(&event));
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
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    // Dedup: skip any durable event already delivered via replay.
                    if !crate::coordinator::should_forward(&event, replay_watermark) {
                        continue;
                    }
                    if let Ok(msg_str) = serde_json::to_string(&event) {
                        if command_tx_clone.send(Message::Text(msg_str)).await.is_err() {
                            break;
                        }
                    }
                }
                // The client fell behind a burst of ephemeral deltas. Skip the gap
                // and KEEP streaming rather than silently dropping the connection;
                // durable state is reconciled by the client from the shared DB.
                Err(RecvError::Lagged(_)) => continue,
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

            // If reply is "always", save to permissions_saved
            if reply == "always" {
                if let Ok(Some(sess)) = db::get_session(&coord.pool, session_id).await {
                    let sessions = coord.sessions.lock().await;
                    if let Some(active) = sessions.get(session_id) {
                        if let Some((prompt, _)) = &active.pending_permission {
                            if prompt.permission_id == perm_id {
                                let action = prompt.action.clone();
                                let resource = prompt
                                    .resources
                                    .first()
                                    .cloned()
                                    .unwrap_or_else(|| "*".to_string());
                                let _ = db::save_permission(
                                    &coord.pool,
                                    &sess.project_id,
                                    &action,
                                    &resource,
                                )
                                .await;
                            }
                        }
                    }
                }
            }

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
