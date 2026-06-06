use crate::provider::{ModelProvider, ProviderError, ProviderEvent, resolve_api_key};
use eventsource_stream::Eventsource;
use futures_util::stream::{BoxStream, StreamExt};
use private_code_protocol::event::UsageStats;
use private_code_protocol::message::{ChatMessage, ContentBlock, Role};
use serde::Serialize;
use std::collections::HashMap;

pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: std::sync::OnceLock<String>,
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: std::sync::OnceLock::new(),
        }
    }
}

impl AnthropicProvider {
    pub fn new() -> Self {
        Self::default()
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
        let (price_in, price_out, price_cr, price_cw) = price_per_mtok(model_id);
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
                    ContentBlock::Reasoning { reasoning } => {
                        content.push(serde_json::json!({
                            "type": "text",
                            "text": reasoning,
                        }));
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

        let response = self
            .client
            .post("https://api.anthropic.com/v1/messages")
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

        // State variables for stream parsing
        let mut active_tool_uses: HashMap<usize, (String, String, String)> = HashMap::new(); // index -> (id, name, accumulated_json)
        let mut input_tokens = 0;
        let mut cache_read_tokens = 0;
        let mut cache_write_tokens = 0;
        let mut output_tokens = 0;
        let mut finish_reason: Option<String> = None;

        let parsed_stream = stream
            .map(move |event_res| {
                let event = match event_res {
                    Ok(e) => e,
                    Err(err) => return Err(ProviderError::Other(err.to_string())),
                };

                let val: serde_json::Value = serde_json::from_str(&event.data)?;
                let typ = val["type"].as_str().unwrap_or_default();

                match typ {
                    "message_start" => {
                        if let Some(usage) = val["message"]["usage"].as_object() {
                            input_tokens += usage
                                .get("input_tokens")
                                .and_then(|t| t.as_i64())
                                .unwrap_or(0);
                            cache_read_tokens += usage
                                .get("cache_read_input_tokens")
                                .and_then(|t| t.as_i64())
                                .unwrap_or(0);
                            cache_write_tokens += usage
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
                            active_tool_uses
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
                        } else if delta_type == "input_json_delta" {
                            let partial = val["delta"]["partial_json"].as_str().unwrap_or_default();
                            if let Some((id, _, acc)) = active_tool_uses.get_mut(&index) {
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
                        if let Some((id, name, acc)) = active_tool_uses.remove(&index) {
                            let parsed_input: serde_json::Value = serde_json::from_str(&acc)
                                .unwrap_or_else(|_| serde_json::json!({}));
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
                            output_tokens += usage
                                .get("output_tokens")
                                .and_then(|t| t.as_i64())
                                .unwrap_or(0);
                        }
                        if let Some(reason) = val["delta"]["stop_reason"].as_str() {
                            finish_reason = Some(reason.to_string());
                        }
                        Ok(None)
                    }
                    "message_stop" => {
                        let cost = (input_tokens as f64 * price_in
                            + cache_read_tokens as f64 * price_cr
                            + cache_write_tokens as f64 * price_cw
                            + output_tokens as f64 * price_out)
                            / 1_000_000.0;

                        let stats = UsageStats {
                            input_tokens,
                            output_tokens,
                            cache_read_tokens,
                            cache_write_tokens,
                            reasoning_tokens: 0,
                            cost,
                        };
                        Ok(Some(ProviderEvent::MessageStop {
                            usage: stats,
                            finish_reason: finish_reason.clone(),
                        }))
                    }
                    _ => Ok(None),
                }
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
