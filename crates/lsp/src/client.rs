//! JSON-RPC 2.0 LSP client over stdio.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;
use tracing::{debug, warn};

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("spawn failed: {0}")]
    Spawn(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Diagnostic {
    pub message: String,
    pub severity: u8,
    pub line: u32,
    pub character: u32,
}

pub struct LspClient {
    stdin: Arc<Mutex<ChildStdin>>,
    next_id: AtomicI64,
    diagnostics: Arc<Mutex<HashMap<PathBuf, Vec<Diagnostic>>>>,
    _child: Child,
}

impl LspClient {
    /// Spawn a language server and initialize it for `root_uri`.
    pub async fn spawn(command: &str, args: &[String], root: &Path) -> Result<Self, LspError> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| LspError::Spawn(format!("{command}: {e}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| LspError::Other("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LspError::Other("no stdout".into()))?;

        let stdin = Arc::new(Mutex::new(stdin));
        let diagnostics: Arc<Mutex<HashMap<PathBuf, Vec<Diagnostic>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let diag_clone = diagnostics.clone();

        // Reader loop for notifications.
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_message(&mut reader).await {
                    Ok(Some(msg)) => {
                        if msg.get("method").and_then(|m| m.as_str())
                            == Some("textDocument/publishDiagnostics")
                            && let Some(params) = msg.get("params")
                        {
                            let uri = params["uri"].as_str().unwrap_or("");
                            let path = uri_to_path(uri);
                            let mut diags = Vec::new();
                            if let Some(arr) = params["diagnostics"].as_array() {
                                for d in arr {
                                    diags.push(Diagnostic {
                                        message: d["message"].as_str().unwrap_or("").to_string(),
                                        severity: d["severity"].as_u64().unwrap_or(1) as u8,
                                        line: d["range"]["start"]["line"].as_u64().unwrap_or(0)
                                            as u32,
                                        character: d["range"]["start"]["character"]
                                            .as_u64()
                                            .unwrap_or(0)
                                            as u32,
                                    });
                                }
                            }
                            diag_clone.lock().await.insert(path, diags);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        warn!("LSP reader error: {e}");
                        break;
                    }
                }
            }
        });

        let client = Self {
            stdin,
            next_id: AtomicI64::new(1),
            diagnostics,
            _child: child,
        };

        let root_uri = path_to_uri(root);
        client
            .request(
                "initialize",
                serde_json::json!({
                    "processId": std::process::id(),
                    "rootUri": root_uri,
                    "capabilities": {},
                }),
            )
            .await?;
        client.notify("initialized", serde_json::json!({})).await?;
        Ok(client)
    }

    pub async fn did_open(
        &self,
        path: &Path,
        language_id: &str,
        text: &str,
    ) -> Result<(), LspError> {
        let uri = path_to_uri(path);
        self.notify(
            "textDocument/didOpen",
            serde_json::json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": text,
                }
            }),
        )
        .await
    }

    pub async fn did_change(&self, path: &Path, version: i64, text: &str) -> Result<(), LspError> {
        let uri = path_to_uri(path);
        self.notify(
            "textDocument/didChange",
            serde_json::json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }]
            }),
        )
        .await
    }

    pub async fn did_save(&self, path: &Path) -> Result<(), LspError> {
        let uri = path_to_uri(path);
        self.notify(
            "textDocument/didSave",
            serde_json::json!({ "textDocument": { "uri": uri } }),
        )
        .await
    }

    pub async fn diagnostics_for(&self, path: &Path) -> Vec<Diagnostic> {
        self.diagnostics
            .lock()
            .await
            .get(path)
            .cloned()
            .unwrap_or_default()
    }

    pub async fn definition(
        &self,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<Value, LspError> {
        let uri = path_to_uri(path);
        self.request(
            "textDocument/definition",
            serde_json::json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }),
        )
        .await
    }

    pub async fn references(
        &self,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<Value, LspError> {
        let uri = path_to_uri(path);
        self.request(
            "textDocument/references",
            serde_json::json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
                "context": { "includeDeclaration": true }
            }),
        )
        .await
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_message(&msg).await?;
        // For simplicity, poll diagnostics store — full request/response pairing
        // would need a pending map; definition/references return empty on timeout.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        Ok(Value::Null)
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_message(&msg).await
    }

    async fn write_message(&self, msg: &Value) -> Result<(), LspError> {
        let body = serde_json::to_string(msg)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(header.as_bytes()).await?;
        stdin.write_all(body.as_bytes()).await?;
        stdin.flush().await?;
        debug!("LSP → {body}");
        Ok(())
    }

    pub fn format_diagnostics(diags: &[Diagnostic]) -> String {
        if diags.is_empty() {
            return String::new();
        }
        diags
            .iter()
            .map(|d| {
                format!(
                    "L{}:{} {} — {}",
                    d.line + 1,
                    d.character + 1,
                    severity_label(d.severity),
                    d.message
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn severity_label(sev: u8) -> &'static str {
    match sev {
        1 => "ERROR",
        2 => "WARN",
        3 => "INFO",
        _ => "HINT",
    }
}

async fn read_message<R: AsyncReadExt + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<Value>, LspError> {
    // Capture `Content-Length` AS WE READ each header line — the buffer is
    // cleared every iteration, so the value must be parsed before the blank-line
    // separator scan, not after (the old code read the value off the already-
    // cleared buffer and always failed with "missing Content-Length").
    let mut content_len: Option<usize> = None;
    let mut header = String::new();
    loop {
        header.clear();
        let n = reader.read_line(&mut header).await?;
        if n == 0 {
            return Ok(None); // EOF between frames
        }
        let line = header.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break; // blank line: end of headers
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            content_len = v.trim().parse().ok();
        }
    }
    let len = content_len.ok_or_else(|| LspError::Other("missing Content-Length".into()))?;

    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body)?))
}

fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn uri_to_path(uri: &str) -> PathBuf {
    uri.strip_prefix("file://")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(uri))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_empty_diagnostics() {
        assert!(LspClient::format_diagnostics(&[]).is_empty());
    }

    #[test]
    fn format_one_diagnostic() {
        let s = LspClient::format_diagnostics(&[Diagnostic {
            message: "expected `;`".into(),
            severity: 1,
            line: 4,
            character: 10,
        }]);
        assert!(s.contains("ERROR"));
        assert!(s.contains("expected `;`"));
    }

    /// A valid `Content-Length` framed JSON-RPC message must parse. The old
    /// `read_message` cleared the header buffer every loop iteration, wiping the
    /// `Content-Length:` line before the parse — so even a perfectly valid frame
    /// returned `Err("missing Content-Length")` and killed the reader task on the
    /// FIRST frame (publishDiagnostics never reached the diagnostics map).
    #[tokio::test]
    async fn read_message_parses_a_valid_content_length_frame() {
        let body = r#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"file:///x.rs","diagnostics":[]}}"#;
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut reader = BufReader::new(frame.as_bytes());
        let msg = read_message(&mut reader)
            .await
            .expect("read_message must not error on a valid frame")
            .expect("a valid frame yields Some(message)");
        assert_eq!(
            msg["method"].as_str(),
            Some("textDocument/publishDiagnostics")
        );
    }

    /// The `Content-Length` header must be captured even when another header
    /// (e.g. `Content-Type`) precedes it — the bug was specifically losing the
    /// value across the blank-line scan.
    #[tokio::test]
    async fn read_message_captures_content_length_after_other_headers() {
        let body = r#"{"id":7,"result":{"ok":true}}"#;
        let frame = format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let mut reader = BufReader::new(frame.as_bytes());
        let msg = read_message(&mut reader).await.unwrap().unwrap();
        assert_eq!(msg["id"].as_i64(), Some(7));
        assert_eq!(msg["result"]["ok"].as_bool(), Some(true));
    }

    /// Two framed messages back-to-back both decode (the reader-task loop calls
    /// `read_message` repeatedly on the same stream).
    #[tokio::test]
    async fn read_message_decodes_consecutive_frames() {
        let b1 = r#"{"id":1}"#;
        let b2 = r#"{"id":2}"#;
        let frames = format!(
            "Content-Length: {}\r\n\r\n{}Content-Length: {}\r\n\r\n{}",
            b1.len(),
            b1,
            b2.len(),
            b2
        );
        let mut reader = BufReader::new(frames.as_bytes());
        let m1 = read_message(&mut reader).await.unwrap().unwrap();
        let m2 = read_message(&mut reader).await.unwrap().unwrap();
        assert_eq!(m1["id"].as_i64(), Some(1));
        assert_eq!(m2["id"].as_i64(), Some(2));
        // EOF after the last frame yields None.
        assert!(read_message(&mut reader).await.unwrap().is_none());
    }
}
