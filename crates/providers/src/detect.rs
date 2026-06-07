//! Provider auto-detection (Phase 5, Step 5.4).
//!
//! Scans environment variables and local ports to discover available providers.

use crate::anthropic::AnthropicProvider;
use crate::google::GoogleProvider;
use crate::openai::OpenAiCompatProvider;
use crate::provider::{ModelProvider, resolve_api_key};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::info;

/// Well-known local inference ports.
pub const OLLAMA_PORT: u16 = 11434;
pub const LMSTUDIO_PORT: u16 = 1234;

/// Detect all providers that appear configured or reachable.
pub async fn detect_providers() -> HashMap<String, Arc<dyn ModelProvider>> {
    let mut map: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();

    if resolve_api_key("anthropic").is_ok() {
        map.insert("anthropic".into(), Arc::new(AnthropicProvider::new()));
        info!("Detected provider: anthropic (keyring/env)");
    }

    if resolve_api_key("openai").is_ok() {
        map.insert("openai".into(), Arc::new(OpenAiCompatProvider::openai()));
        info!("Detected provider: openai (keyring/env)");
    }

    if resolve_api_key("google").is_ok() {
        map.insert("google".into(), Arc::new(GoogleProvider::new()));
        info!("Detected provider: google (keyring/env)");
    }

    if resolve_api_key("nvidia").is_ok() {
        map.insert("nvidia".into(), Arc::new(OpenAiCompatProvider::nvidia()));
        info!("Detected provider: nvidia (keyring/env)");
    }

    if resolve_api_key("deepseek").is_ok() {
        map.insert(
            "deepseek".into(),
            Arc::new(OpenAiCompatProvider::deepseek()),
        );
        info!("Detected provider: deepseek (keyring/env)");
    }

    if resolve_api_key("groq").is_ok() {
        map.insert("groq".into(), Arc::new(OpenAiCompatProvider::groq()));
        info!("Detected provider: groq (keyring/env)");
    }

    if is_port_open(OLLAMA_PORT).await {
        map.insert("ollama".into(), Arc::new(OpenAiCompatProvider::ollama()));
        info!("Detected provider: ollama (localhost:{OLLAMA_PORT})");
    }

    if is_port_open(LMSTUDIO_PORT).await {
        map.insert(
            "lmstudio".into(),
            Arc::new(OpenAiCompatProvider::lmstudio()),
        );
        info!("Detected provider: lmstudio (localhost:{LMSTUDIO_PORT})");
    }

    map
}

async fn is_port_open(port: u16) -> bool {
    tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn detect_does_not_panic_without_keys() {
        let _ = detect_providers().await;
    }
}
