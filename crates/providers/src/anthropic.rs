use crate::provider::{ModelProvider, ProviderError, ProviderEvent, resolve_api_key};
use eventsource_stream::Eventsource;
use futures_util::stream::{BoxStream, StreamExt};
use private_code_protocol::event::UsageStats;
use private_code_protocol::message::{ChatMessage, ContentBlock, Role};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Default Anthropic API base. Overridable per-instance (tests / proxies) via
/// [`AnthropicProvider::with_base_url`] or the `PRIVATE_CODE_ANTHROPIC_BASE_URL`
/// environment variable.
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: OnceLock<String>,
    base_url: String,
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        let base_url = std::env::var("PRIVATE_CODE_ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        Self {
            client: reqwest::Client::new(),
            api_key: OnceLock::new(),
            base_url,
        }
    }
}

impl AnthropicProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a provider pointed at a specific base URL (e.g. a local mock
    /// server in tests, or a corporate proxy). The path `/v1/messages` is
    /// appended to this base.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: OnceLock::new(),
            base_url: base_url.into(),
        }
    }

    /// Resolve the Anthropic API key once and cache it (keyring → env fallback),
    /// instead of hitting the OS keychain on every turn.
    fn resolve_key(&self) -> Result<String, ProviderError> {
        if let Some(k) = self.api_key.get() {
            return Ok(k.clone());
        }
        let k = resolve_api_key("anthropic")?;
        let _ = self.api_key.set(k.clone());
        Ok(k)
    }
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<serde_json::Value>,
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
}

/// Mutable decode state for the Anthropic `/v1/messages` SSE stream. One per
/// turn. Carries the in-flight tool-call accumulators, running token counters,
/// the captured stop_reason, and the per-model price table so cost can be
/// computed at `message_stop`.
pub struct SseState {
    /// content-block index -> (tool_use id, name, accumulated input JSON).
    active_tool_uses: HashMap<usize, (String, String, String)>,
    input_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    output_tokens: i64,
    reasoning_tokens: i64,
    finish_reason: Option<String>,
    price_in: f64,
    price_out: f64,
    price_cr: f64,
    price_cw: f64,
}

impl SseState {
    /// Initialize decode state, seeding the price table from the model id.
    pub fn new(model_id: &str) -> Self {
        let (price_in, price_out, price_cr, price_cw) = price_per_mtok(model_id);
        Self {
            active_tool_uses: HashMap::new(),
            input_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            finish_reason: None,
            price_in,
            price_out,
            price_cr,
            price_cw,
        }
    }
}

/// Decode a single Anthropic SSE event (already parsed to JSON), advancing
/// `state` and optionally yielding a [`ProviderEvent`]. This is the single
/// source of truth for the wire state machine: the live `stream_chat` and the
/// offline replay harness (`testkit::parse_sse_to_events`) both call it, so the
/// tool-index map, input_json accumulation, usage/cost math and stop_reason are
/// exercised identically online and offline.
pub fn parse_anthropic_event(
    val: &serde_json::Value,
    state: &mut SseState,
) -> Result<Option<ProviderEvent>, ProviderError> {
    let typ = val["type"].as_str().unwrap_or_default();

    match typ {
        "message_start" => {
            if let Some(usage) = val["message"]["usage"].as_object() {
                state.input_tokens += usage
                    .get("input_tokens")
                    .and_then(|t| t.as_i64())
                    .unwrap_or(0);
                state.cache_read_tokens += usage
                    .get("cache_read_input_tokens")
                    .and_then(|t| t.as_i64())
                    .unwrap_or(0);
                state.cache_write_tokens += usage
                    .get("cache_creation_input_tokens")
                    .and_then(|t| t.as_i64())
                    .unwrap_or(0);
            }
            Ok(None)
        }
        "content_block_start" => {
            let index = val["index"].as_u64().unwrap_or(0) as usize;
            let block_type = val["content_block"]["type"].as_str().unwrap_or_default();
            if block_type == "tool_use" {
                let id = val["content_block"]["id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let name = val["content_block"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                state
                    .active_tool_uses
                    .insert(index, (id.clone(), name.clone(), String::new()));
                Ok(Some(ProviderEvent::ToolUseStart { id, name }))
            } else {
                Ok(None)
            }
        }
        "content_block_delta" => {
            let index = val["index"].as_u64().unwrap_or(0) as usize;
            let delta_type = val["delta"]["type"].as_str().unwrap_or_default();
            if delta_type == "text_delta" {
                let text = val["delta"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                Ok(Some(ProviderEvent::TextDelta(text)))
            } else if delta_type == "thinking_delta" {
                let thinking = val["delta"]["thinking"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                Ok(Some(ProviderEvent::ReasoningDelta(thinking)))
            } else if delta_type == "signature_delta" {
                let signature = val["delta"]["signature"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                Ok(Some(ProviderEvent::ReasoningSignatureDelta(signature)))
            } else if delta_type == "input_json_delta" {
                let partial = val["delta"]["partial_json"].as_str().unwrap_or_default();
                if let Some((id, _, acc)) = state.active_tool_uses.get_mut(&index) {
                    acc.push_str(partial);
                    Ok(Some(ProviderEvent::ToolUseDelta {
                        id: id.clone(),
                        input_delta: partial.to_string(),
                    }))
                } else {
                    Ok(None)
                }
            } else {
                Ok(None)
            }
        }
        "content_block_stop" => {
            let index = val["index"].as_u64().unwrap_or(0) as usize;
            if let Some((id, name, acc)) = state.active_tool_uses.remove(&index) {
                let parsed_input: serde_json::Value =
                    serde_json::from_str(&acc).unwrap_or_else(|_| serde_json::json!({}));
                Ok(Some(ProviderEvent::ToolUseComplete {
                    id,
                    name,
                    input: parsed_input,
                }))
            } else {
                Ok(None)
            }
        }
        "message_delta" => {
            if let Some(usage) = val["usage"].as_object() {
                state.output_tokens += usage
                    .get("output_tokens")
                    .and_then(|t| t.as_i64())
                    .unwrap_or(0);
            }
            if let Some(reason) = val["delta"]["stop_reason"].as_str() {
                state.finish_reason = Some(reason.to_string());
            }
            Ok(None)
        }
        "message_stop" => {
            let cost = (state.input_tokens as f64 * state.price_in
                + state.cache_read_tokens as f64 * state.price_cr
                + state.cache_write_tokens as f64 * state.price_cw
                + state.output_tokens as f64 * state.price_out)
                / 1_000_000.0;

            let stats = UsageStats {
                input_tokens: state.input_tokens,
                output_tokens: state.output_tokens,
                cache_read_tokens: state.cache_read_tokens,
                cache_write_tokens: state.cache_write_tokens,
                reasoning_tokens: state.reasoning_tokens,
                cost,
            };
            Ok(Some(ProviderEvent::MessageStop {
                usage: stats,
                finish_reason: state.finish_reason.clone(),
            }))
        }
        _ => Ok(None),
    }
}

use async_trait::async_trait;

#[async_trait]
impl ModelProvider for AnthropicProvider {
    async fn stream_chat(
        &self,
        model_id: &str,
        system_prompt: Option<&str>,
        max_tokens: u32,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError> {
        let api_key = self.resolve_key()?;
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-api-key",
            reqwest::header::HeaderValue::from_str(&api_key)
                .map_err(|_| ProviderError::Other("Invalid API key format".to_string()))?,
        );
        headers.insert(
            "anthropic-version",
            reqwest::header::HeaderValue::from_static("2023-06-01"),
        );
        headers.insert(
            "anthropic-beta",
            reqwest::header::HeaderValue::from_static(
                "prompt-caching-2024-07-31,mid-conversation-system-2026-04-07",
            ),
        );
        headers.insert(
            "content-type",
            reqwest::header::HeaderValue::from_static("application/json"),
        );

        let system_val = system_prompt.map(|prompt| {
            serde_json::json!([
                {
                    "type": "text",
                    "text": prompt,
                    "cache_control": { "type": "ephemeral" }
                }
            ])
        });

        let mut anthropic_messages = Vec::new();
        for msg in messages {
            let role = match msg.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System => "system",
            };

            let mut content = Vec::new();
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        content.push(serde_json::json!({
                            "type": "text",
                            "text": text,
                        }));
                    }
                    ContentBlock::Reasoning {
                        reasoning,
                        signature,
                    } => {
                        // Only replay an extended-thinking block that carries its
                        // signature, as a proper {type:"thinking"} block. Reasoning
                        // without a signature cannot be validly replayed (Anthropic
                        // requires the signature) and must NOT be sent as plain text
                        // — that loses the cryptographic proof and pollutes context —
                        // so it is dropped from the replayed messages here.
                        if let Some(sig) = signature {
                            content.push(serde_json::json!({
                                "type": "thinking",
                                "thinking": reasoning,
                                "signature": sig,
                            }));
                        }
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        content.push(serde_json::json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input,
                        }));
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content: tool_content,
                        is_error,
                    } => {
                        let tool_content_val = match tool_content {
                            private_code_protocol::message::ToolResultContent::Text(text) => {
                                serde_json::json!([
                                    {
                                        "type": "text",
                                        "text": text,
                                    }
                                ])
                            }
                            private_code_protocol::message::ToolResultContent::Json(val) => {
                                serde_json::json!([
                                    {
                                        "type": "text",
                                        "text": serde_json::to_string(&val).unwrap_or_default(),
                                    }
                                ])
                            }
                        };
                        content.push(serde_json::json!({
                            "type": "tool_result",
                            "tool_use_id": tool_use_id,
                            "content": tool_content_val,
                            "is_error": is_error,
                        }));
                    }
                }
            }

            anthropic_messages.push(AnthropicMessage {
                role: role.to_string(),
                content,
            });
        }

        let request_payload = AnthropicRequest {
            model: model_id.to_string(),
            messages: anthropic_messages,
            system: system_val,
            max_tokens,
            stream: true,
            tools: if tools.is_empty() {
                None
            } else {
                Some(tools.to_vec())
            },
        };

        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let response = self
            .client
            .post(&url)
            .headers(headers)
            .json(&request_payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            let message = if let Ok(err_val) = serde_json::from_str::<serde_json::Value>(&text) {
                err_val["error"]["message"]
                    .as_str()
                    .unwrap_or(&text)
                    .to_string()
            } else {
                text
            };
            return Err(ProviderError::Api { status, message });
        }

        let stream = response.bytes_stream().eventsource();
        let mut state = SseState::new(model_id);

        let parsed_stream = stream
            .map(move |event_res| {
                let event = match event_res {
                    Ok(e) => e,
                    Err(err) => return Err(ProviderError::Other(err.to_string())),
                };
                let val: serde_json::Value = serde_json::from_str(&event.data)?;
                parse_anthropic_event(&val, &mut state)
            })
            .filter_map(|x| async {
                match x {
                    Ok(Some(ev)) => Some(Ok(ev)),
                    Ok(None) => None,
                    Err(err) => Some(Err(err)),
                }
            });

        Ok(parsed_stream.boxed())
    }

    fn count_tokens(&self, _model_id: &str, text: &str) -> usize {
        // ≈4 chars/token (char count, not bytes — bytes overcounts multibyte UTF-8).
        (text.chars().count() / 4).max(1)
    }
}

/// The model's context window in tokens. Phase-1 stopgap (all current Claude
/// families are 200K) keyed by family, until the model catalog supplies it.
pub fn context_window(model_id: &str) -> u32 {
    // All current Claude families (opus / sonnet / haiku) are 200K tokens.
    // Keyed on the id for when the catalog introduces differing windows.
    let _ = model_id;
    200_000
}

/// Per-million-token prices `(input, output, cache_read, cache_write)` in USD.
/// A Phase-1 stopgap until the model catalog (Phase 5) supplies pricing — keyed
/// by model-family substring rather than assuming a single model's rates.
fn price_per_mtok(model_id: &str) -> (f64, f64, f64, f64) {
    let m = model_id.to_ascii_lowercase();
    if m.contains("opus") {
        (5.0, 25.0, 0.5, 6.25)
    } else if m.contains("haiku") {
        (1.0, 5.0, 0.1, 1.25)
    } else {
        // sonnet / default
        (3.0, 15.0, 0.3, 3.75)
    }
}
