use dns_lookup::lookup_host;
use serde_json::{Value, json};
use std::net::IpAddr;
use std::time::Duration;
use url::Url;

use crate::tool::{Tool, ToolContext, ToolError};
use async_trait::async_trait;

// 1. Bash Tool
pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a bash command in a subprocess. Captured output is returned."
    }

    fn schema(&self) -> Value {
        json!({
            "name": "bash",
            "description": self.description(),
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The command line string to run" }
                },
                "required": ["command"]
            }
        })
    }

    fn mutates(&self) -> bool {
        true
    }

    fn permission_class(&self) -> &str {
        "bash"
    }

    async fn run(
        &self,
        context: &mut ToolContext<'_>,
        arguments: Value,
    ) -> Result<Value, ToolError> {
        let command_str = arguments["command"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'command' argument".to_string()))?;

        // T2 — Scrub environment: build clean env from an allowlist
        let mut clean_env = std::collections::HashMap::new();
        let allowlist = [
            "PATH",
            "HOME",
            "USER",
            "TERM",
            "SHELL",
            "LANG",
            "LC_ALL",
            "CARGO_HOME",
            "RUSTUP_HOME",
        ];
        for key in &allowlist {
            if let Ok(val) = std::env::var(key) {
                clean_env.insert(key.to_string(), val);
            }
        }

        // Spawn subprocess using tokio's async process manager
        let shell = if cfg!(target_os = "windows") {
            ("cmd.exe", "/C")
        } else {
            ("/bin/zsh", "-c")
        };

        let mut cmd = tokio::process::Command::new(shell.0);
        cmd.arg(shell.1)
            .arg(command_str)
            .current_dir(context.active_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env_clear()
            .envs(clean_env);

        let child = cmd
            .spawn()
            .map_err(|e| ToolError::CommandFailed(format!("Failed to spawn shell: {}", e)))?;

        let output_store = crate::output_store::OutputStore::new(
            context.global_data_dir.to_path_buf(),
            context.max_lines,
            context.max_bytes,
        );

        // Run command with a 120s timeout
        let timeout_fut = tokio::time::timeout(Duration::from_secs(120), child.wait_with_output());

        match timeout_fut.await {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let exit_code = output.status.code().unwrap_or(-1);

                let combined = if stderr.is_empty() {
                    stdout
                } else if stdout.is_empty() {
                    stderr
                } else {
                    format!("{}\n{}", stdout, stderr)
                };

                let processed = output_store.process_output(&combined);
                Ok(json!({
                    "exit_code": exit_code,
                    "output": processed,
                }))
            }
            Ok(Err(e)) => Err(ToolError::CommandFailed(format!(
                "Error waiting for command: {}",
                e
            ))),
            Err(_) => Err(ToolError::CommandFailed(
                "Command execution timed out after 120 seconds".to_string(),
            )),
        }
    }
}

// 2. WebFetch Tool (SSRF Guarded)
pub struct WebFetchTool;

impl Default for WebFetchTool {
    fn default() -> Self {
        Self
    }
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch the text content of a URL with security guards (SSRF protection, redirect check, size limits)."
    }

    fn schema(&self) -> Value {
        json!({
            "name": "web_fetch",
            "description": self.description(),
            "input_schema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The URL to fetch (HTTP/HTTPS)" }
                },
                "required": ["url"]
            }
        })
    }

    fn mutates(&self) -> bool {
        false
    }

    fn permission_class(&self) -> &str {
        "web_fetch"
    }

    async fn run(
        &self,
        context: &mut ToolContext<'_>,
        arguments: Value,
    ) -> Result<Value, ToolError> {
        let url_str = arguments["url"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'url' argument".to_string()))?;
        let mut target_url = Url::parse(url_str)
            .map_err(|e| ToolError::InvalidArguments(format!("Invalid URL: {}", e)))?;

        // Build a redirect-NONE client per call. (Never fall back to a
        // redirect-following client — that would bypass the per-hop SSRF guard.)
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| ToolError::Other(format!("failed to build HTTP client: {}", e)))?;

        let mut redirect_count = 0;
        let max_redirects = 5;

        loop {
            let host = target_url
                .host_str()
                .ok_or_else(|| ToolError::InvalidArguments("Missing host in URL".to_string()))?;
            let port = target_url.port_or_known_default().unwrap_or(80);

            // T3 SSRF Guard - DNS resolution
            let ips = lookup_host(host).map_err(|e| {
                ToolError::SsrfBlocked(format!("DNS resolution failed for {}: {}", host, e))
            })?;
            if ips.is_empty() {
                return Err(ToolError::SsrfBlocked(format!(
                    "No IP address found for host: {}",
                    host
                )));
            }

            let ip = ips[0];
            validate_ip(ip)?;

            // DNS pinning - connect directly to the IP, set Host header
            let mut pin_url = target_url.clone();
            pin_url
                .set_host(Some(&ip.to_string()))
                .map_err(|_| ToolError::Other("Failed to set pinned IP host".to_string()))?;
            // If port is custom, set it back as set_host might reset it or we want to ensure port consistency
            if target_url.port().is_some() {
                pin_url
                    .set_port(Some(port))
                    .map_err(|_| ToolError::Other("Failed to set port".to_string()))?;
            }

            let mut response = client
                .get(pin_url.as_str())
                .header("Host", host)
                .send()
                .await?;

            let status = response.status();
            if status.is_redirection() {
                redirect_count += 1;
                if redirect_count > max_redirects {
                    return Err(ToolError::SsrfBlocked("Too many redirects".to_string()));
                }

                if let Some(loc_header) = response.headers().get("Location") {
                    let loc_str = loc_header.to_str().map_err(|_| {
                        ToolError::Other("Invalid Location header encoding".to_string())
                    })?;
                    let next_url = target_url.join(loc_str).map_err(|e| {
                        ToolError::InvalidArguments(format!(
                            "Failed to resolve redirect Location: {}",
                            e
                        ))
                    })?;
                    target_url = next_url;
                    continue;
                } else {
                    return Err(ToolError::Other(
                        "Redirect response missing Location header".to_string(),
                    ));
                }
            }

            if !status.is_success() {
                return Err(ToolError::Other(format!(
                    "HTTP request failed with status: {}",
                    status
                )));
            }

            // Bounded streaming read — cap at 2 MB WITHOUT buffering the whole body
            // first (content_length can be absent on chunked responses, so a
            // post-hoc check would still let an attacker-controlled body exhaust memory).
            const CAP: usize = 2 * 1024 * 1024;
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if buf.len() + chunk.len() > CAP {
                    return Err(ToolError::SsrfBlocked(
                        "Response exceeds 2 MB limit".to_string(),
                    ));
                }
                buf.extend_from_slice(&chunk);
            }
            let text = String::from_utf8_lossy(&buf).to_string();

            let output_store = crate::output_store::OutputStore::new(
                context.global_data_dir.to_path_buf(),
                context.max_lines,
                context.max_bytes,
            );
            let processed = output_store.process_output(&text);

            return Ok(Value::String(processed));
        }
    }
}

fn validate_ip(ip: IpAddr) -> Result<(), ToolError> {
    match ip {
        IpAddr::V4(ipv4) => {
            if ipv4.is_loopback()
                || ipv4.is_private()
                || ipv4.is_link_local()
                || ipv4.is_multicast()
                || ipv4.is_unspecified()      // 0.0.0.0 (can route to localhost)
                || ipv4.is_broadcast()
            {
                return Err(ToolError::SsrfBlocked(format!(
                    "Access to private/local IP address {} is blocked",
                    ip
                )));
            }
            let octets = ipv4.octets();
            if octets[0] == 100 && (octets[1] & 0xC0) == 64 {
                return Err(ToolError::SsrfBlocked(format!(
                    "Access to CGNAT IP address {} is blocked",
                    ip
                )));
            }
        }
        IpAddr::V6(ipv6) => {
            // An IPv4-mapped address (::ffff:a.b.c.d) must be validated as its v4 form,
            // otherwise ::ffff:169.254.169.254 (cloud metadata) would slip through.
            if let Some(v4) = ipv6.to_ipv4_mapped() {
                return validate_ip(IpAddr::V4(v4));
            }
            if ipv6.is_loopback() || ipv6.is_multicast() || ipv6.is_unspecified() {
                return Err(ToolError::SsrfBlocked(format!(
                    "Access to private/local IP address {} is blocked",
                    ip
                )));
            }
            let segments = ipv6.segments();
            if (segments[0] & 0xFFC0) == 0xFE80 {
                return Err(ToolError::SsrfBlocked(format!(
                    "Access to link-local IP address {} is blocked",
                    ip
                )));
            }
            if (segments[0] & 0xFE00) == 0xFC00 {
                return Err(ToolError::SsrfBlocked(format!(
                    "Access to unique local IP address {} is blocked",
                    ip
                )));
            }
        }
    }
    Ok(())
}
