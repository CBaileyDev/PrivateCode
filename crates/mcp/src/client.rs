//! MCP JSON-RPC client over stdio (and optional HTTP for Streamable HTTP servers).
//!
//! Framing is newline-delimited JSON (the MCP stdio transport contract — each
//! message is a single line of JSON with no embedded newlines), NOT LSP-style
//! `Content-Length`. Requests are paired to responses by JSON-RPC `id` via a
//! pending-map + per-request oneshot, and every `request` is timeout-bounded so a
//! silent server cannot hang a caller (notably daemon startup, which registers
//! MCP tools synchronously).

use crate::config::McpServerConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};
use tracing::{debug, warn};

/// Upper bound on how long a single JSON-RPC request waits for its paired
/// response before failing. A server that exits is detected immediately (the
/// reader drains all pending requests on stream close); this only bounds a
/// server that stays alive but never answers.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("spawn: {0}")]
    Spawn(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("rpc: {0}")]
    Rpc(String),
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Maps an in-flight request id to the channel its paired response is delivered on.
type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, McpError>>>>>;

pub struct McpClient {
    name: String,
    stdin: Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    next_id: AtomicI64,
    pending: Pending,
    _child: Option<Child>,
}

impl McpClient {
    pub async fn connect_stdio(name: &str, cfg: &McpServerConfig) -> Result<Self, McpError> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_clear();
        // Allowlist env: only explicit config vars + PATH for npx/node resolution.
        cmd.env("PATH", std::env::var("PATH").unwrap_or_default());
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| McpError::Spawn(format!("{}: {e}", cfg.command)))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Other("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Other("no stdout".into()))?;

        let client = Self::with_io(name, stdin, stdout, Some(child));
        client.initialize().await?;
        Ok(client)
    }

    /// Construct a client over arbitrary byte streams, spawning the response-
    /// router task. Used by [`connect_stdio`] (over the child's stdio) and by
    /// tests (over an in-memory duplex pair).
    fn with_io<W, R>(name: &str, stdin: W, stdout: R, child: Option<Child>) -> Self
    where
        W: AsyncWrite + Send + Unpin + 'static,
        R: AsyncRead + Send + Unpin + 'static,
    {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        spawn_reader(stdout, pending.clone(), name.to_string());
        Self {
            name: name.to_string(),
            stdin: Arc::new(Mutex::new(Box::new(stdin))),
            next_id: AtomicI64::new(1),
            pending,
            _child: child,
        }
    }

    async fn initialize(&self) -> Result<(), McpError> {
        let _ = self
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "private-code", "version": env!("CARGO_PKG_VERSION") }
                }),
            )
            .await?;
        self.notify("notifications/initialized", serde_json::json!({}))
            .await
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>, McpError> {
        let resp = self.request("tools/list", serde_json::json!({})).await?;
        let mut out = Vec::new();
        if let Some(tools) = resp["tools"].as_array() {
            for t in tools {
                out.push(McpToolDef {
                    name: t["name"].as_str().unwrap_or("tool").to_string(),
                    description: t["description"].as_str().unwrap_or("").to_string(),
                    input_schema: t
                        .get("inputSchema")
                        .or_else(|| t.get("input_schema"))
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({"type":"object"})),
                });
            }
        }
        Ok(out)
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, McpError> {
        let resp = self
            .request(
                "tools/call",
                serde_json::json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        Ok(resp.get("content").cloned().unwrap_or(resp))
    }

    pub fn server_name(&self) -> &str {
        &self.name
    }

    /// Send a JSON-RPC request and await its paired response by `id`. Registers a
    /// oneshot under the id BEFORE writing (so a fast response can't race the
    /// insert), then awaits it under [`REQUEST_TIMEOUT`]. Cleans up the pending
    /// entry on every exit path.
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(e) = self.write_message(&msg).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }

        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_canceled)) => {
                // Reader task dropped the sender (stream closed) without delivering.
                Err(McpError::Other(format!(
                    "MCP '{method}': server stream closed before responding"
                )))
            }
            Err(_elapsed) => {
                self.pending.lock().await.remove(&id);
                Err(McpError::Other(format!(
                    "MCP '{method}': request timed out after {}s",
                    REQUEST_TIMEOUT.as_secs()
                )))
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_message(&msg).await
    }

    async fn write_message(&self, msg: &Value) -> Result<(), McpError> {
        // Newline-delimited JSON: one compact line per message, terminated by `\n`.
        let mut body = serde_json::to_string(msg)?;
        body.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(body.as_bytes()).await?;
        stdin.flush().await?;
        debug!("MCP {} → {}", self.name, body.trim_end());
        Ok(())
    }
}

/// Spawn the response-router task: read newline-delimited JSON from `stdout`,
/// route responses (those carrying an `id`) to the matching pending oneshot, and
/// log notifications. On stream end, fail every still-pending request so callers
/// fail fast instead of waiting for the timeout.
fn spawn_reader<R>(stdout: R, pending: Pending, log_name: String)
where
    R: AsyncRead + Send + Unpin + 'static,
{
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_message(&mut reader).await {
                Ok(Some(v)) => {
                    if let Some(id) = v.get("id").and_then(|i| i.as_i64()) {
                        if let Some(tx) = pending.lock().await.remove(&id) {
                            let payload = if let Some(err) = v.get("error") {
                                Err(McpError::Rpc(
                                    err["message"]
                                        .as_str()
                                        .unwrap_or("unknown rpc error")
                                        .to_string(),
                                ))
                            } else {
                                Ok(v.get("result").cloned().unwrap_or(Value::Null))
                            };
                            let _ = tx.send(payload);
                        } else {
                            debug!("MCP {log_name}: response for unknown id {id}");
                        }
                    } else {
                        debug!("MCP {log_name} notification ← {v}");
                    }
                }
                Ok(None) => break, // EOF: server closed stdout
                Err(e) => {
                    warn!("MCP {log_name} reader: {e}");
                    break;
                }
            }
        }
        // Stream is gone — fail anything still waiting so it doesn't hang to timeout.
        let mut p = pending.lock().await;
        for (_id, tx) in p.drain() {
            let _ = tx.send(Err(McpError::Other("MCP server stream closed".into())));
        }
    });
}

/// Read one newline-delimited JSON message. Skips blank lines and (defensively)
/// non-JSON stdout lines rather than killing the connection. Returns `Ok(None)`
/// at EOF.
async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<Value>, McpError> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(v) => return Ok(Some(v)),
            Err(e) => {
                warn!("MCP: skipping non-JSON stdout line ({e})");
                continue;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_tool_def_deserializes() {
        let v: McpToolDef = serde_json::from_str(
            r#"{"name":"read","description":"read file","input_schema":{"type":"object"}}"#,
        )
        .unwrap();
        assert_eq!(v.name, "read");
    }

    /// Build a client over an in-memory duplex pair, returning the client and the
    /// server's (read, write) halves so a test can act as the MCP server.
    fn duplex_client() -> (
        McpClient,
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        let (client_io, server_io) = tokio::io::duplex(8192);
        let (c_read, c_write) = tokio::io::split(client_io);
        let (s_read, s_write) = tokio::io::split(server_io);
        let client = McpClient::with_io("test", c_write, c_read, None);
        (client, s_read, s_write)
    }

    async fn read_req_line<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    async fn send_line<W: AsyncWrite + Unpin>(w: &mut W, v: &Value) {
        let mut s = serde_json::to_string(v).unwrap();
        s.push('\n');
        w.write_all(s.as_bytes()).await.unwrap();
        w.flush().await.unwrap();
    }

    /// A real request resolves with the server's `result` keyed by JSON-RPC id —
    /// the core pairing the old stub (hardcoded `{"tools":[]}`) never did.
    #[tokio::test]
    async fn list_tools_pairs_response_by_id() {
        let (client, s_read, mut s_write) = duplex_client();
        tokio::spawn(async move {
            let mut reader = BufReader::new(s_read);
            let req = read_req_line(&mut reader).await;
            assert_eq!(req["method"], "tools/list");
            let id = req["id"].as_i64().unwrap();
            send_line(
                &mut s_write,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "tools": [
                        {"name":"echo","description":"echoes","inputSchema":{"type":"object"}}
                    ]}
                }),
            )
            .await;
        });

        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description, "echoes");
    }

    /// Responses delivered out of request order must still pair by id.
    #[tokio::test]
    async fn requests_route_by_id_out_of_order() {
        let (client, s_read, mut s_write) = duplex_client();
        tokio::spawn(async move {
            let mut reader = BufReader::new(s_read);
            let req_a = read_req_line(&mut reader).await; // tools/list (id N)
            let req_b = read_req_line(&mut reader).await; // tools/call (id N+1)
            let id_a = req_a["id"].as_i64().unwrap();
            let id_b = req_b["id"].as_i64().unwrap();
            // Answer B first, then A.
            send_line(
                &mut s_write,
                &serde_json::json!({"jsonrpc":"2.0","id":id_b,"result":{"content":"B"}}),
            )
            .await;
            send_line(
                &mut s_write,
                &serde_json::json!({"jsonrpc":"2.0","id":id_a,"result":{"tools":[]}}),
            )
            .await;
        });

        let (list, call) = tokio::join!(
            client.list_tools(),
            client.call_tool("x", serde_json::json!({}))
        );
        assert!(list.unwrap().is_empty());
        assert_eq!(call.unwrap(), serde_json::json!("B"));
    }

    /// A JSON-RPC error response maps to `McpError::Rpc`, not a silent `Null`.
    #[tokio::test]
    async fn rpc_error_response_maps_to_err() {
        let (client, s_read, mut s_write) = duplex_client();
        tokio::spawn(async move {
            let mut reader = BufReader::new(s_read);
            let req = read_req_line(&mut reader).await;
            let id = req["id"].as_i64().unwrap();
            send_line(
                &mut s_write,
                &serde_json::json!({
                    "jsonrpc":"2.0","id":id,
                    "error": {"code": -32601, "message": "method not found"}
                }),
            )
            .await;
        });
        let err = client.call_tool("missing", serde_json::json!({})).await;
        match err {
            Err(McpError::Rpc(m)) => assert!(m.contains("method not found")),
            other => panic!("expected Rpc error, got {other:?}"),
        }
    }

    /// If the server consumes the request but closes without responding, the
    /// reader drains pending requests on EOF so the caller fails fast rather than
    /// blocking until the 30s request timeout.
    #[tokio::test]
    async fn request_fails_fast_when_server_closes_without_responding() {
        let (client, s_read, s_write) = duplex_client();
        tokio::spawn(async move {
            let mut reader = BufReader::new(s_read);
            let _req = read_req_line(&mut reader).await; // consume the request, no reply
            drop(s_write); // close the client's stdout -> EOF -> drain
        });
        let res = tokio::time::timeout(
            Duration::from_secs(2),
            client.call_tool("x", serde_json::json!({})),
        )
        .await
        .expect("must not block on the 30s request timeout");
        assert!(res.is_err(), "a closed stream fails the pending request");
    }
}
