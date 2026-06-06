use futures_util::stream::BoxStream;
use private_code_protocol::event::UsageStats;
use private_code_protocol::message::ChatMessage;

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolUseStart {
        id: String,
        name: String,
    },
    ToolUseDelta {
        id: String,
        input_delta: String,
    },
    ToolUseComplete {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Terminal event. `finish_reason` mirrors the provider's stop_reason
    /// (e.g. "end_turn", "tool_use", "max_tokens", "refusal") so callers can
    /// distinguish a finished turn from a truncated one.
    MessageStop {
        usage: UsageStats,
        finish_reason: Option<String>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("API key not found for provider '{0}'")]
    ApiKeyNotFound(String),
    #[error("Network/HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API error (status {status}): {message}")]
    Api { status: u16, message: String },
    #[error("Serialization/JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Other error: {0}")]
    Other(String),
}

use async_trait::async_trait;

#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Stream a chat completion. The provider resolves its own credentials
    /// internally (the orchestrator does not handle API keys).
    async fn stream_chat(
        &self,
        model_id: &str,
        system_prompt: Option<&str>,
        max_tokens: u32,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError>;

    /// Local, synchronous token estimate over a text fragment. A coarse
    /// heuristic (≈4 chars/token) — never tiktoken (which is wrong for Claude).
    /// A structured-request estimate + the /v1/messages/count_tokens endpoint
    /// land with the model catalog (Phase 5); this suffices for Phase 1 budgeting.
    fn count_tokens(&self, model_id: &str, text: &str) -> usize;
}

/// Resolve a provider API key: OS keyring first, then an environment-variable
/// fallback. The key is NOT removed from the environment — `remove_var` is
/// thread-unsafe in a multi-threaded runtime and breaks re-resolution; child
/// processes are protected by env-scrubbing at spawn time (see security.md T2),
/// which is the correct mitigation.
pub fn resolve_api_key(provider_name: &str) -> Result<String, ProviderError> {
    if let Ok(entry) = keyring::Entry::new("private-code", provider_name)
        && let Ok(key) = entry.get_password()
    {
        return Ok(key);
    }

    let env_var_name = format!("{}_API_KEY", provider_name.to_uppercase());
    if let Ok(key) = std::env::var(&env_var_name) {
        return Ok(key);
    }

    Err(ProviderError::ApiKeyNotFound(provider_name.to_string()))
}
