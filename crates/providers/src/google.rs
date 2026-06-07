//! Google Gemini API provider (Phase 5, Step 5.4).
//!
//! Streaming via server-sent events on the `:streamGenerateContent` endpoint.

use crate::catalog::ModelCatalog;
use crate::provider::{ModelProvider, ProviderError, ProviderEvent, resolve_api_key};
use eventsource_stream::Eventsource;
use futures_util::stream::{BoxStream, StreamExt};
use private_code_protocol::event::UsageStats;
use private_code_protocol::message::{ChatMessage, ContentBlock, Role, ToolResultContent};
use serde::Serialize;
use std::sync::OnceLock;

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

pub struct GoogleProvider {
    client: reqwest::Client,
    api_key: OnceLock<String>,
    base_url: String,
}

impl Default for GoogleProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl GoogleProvider {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: OnceLock::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: OnceLock::new(),
            base_url: base_url.into(),
        }
    }

    fn resolve_key(&self) -> Result<String, ProviderError> {
        if let Some(k) = self.api_key.get() {
            return Ok(k.clone());
        }
        let k = resolve_api_key("google")?;
        let _ = self.api_key.set(k.clone());
        Ok(k)
    }
}

#[derive(Serialize)]
struct GeminiRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<serde_json::Value>,
    contents: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
    generation_config: serde_json::Value,
}

fn lower_gemini_messages(
    system_prompt: Option<&str>,
    messages: &[ChatMessage],
) -> (Option<serde_json::Value>, Vec<serde_json::Value>) {
    let system = system_prompt.map(|s| {
        serde_json::json!({
            "parts": [{ "text": s }]
        })
    });

    let mut contents = Vec::new();
    for msg in messages {
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "model",
            Role::System => continue,
        };

        let mut parts = Vec::new();
        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => {
                    parts.push(serde_json::json!({ "text": text }));
                }
                ContentBlock::ToolUse { id, name, input } => {
                    parts.push(serde_json::json!({
                        "functionCall": {
                            "name": name,
                            "args": input,
                            "id": id,
                        }
                    }));
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let response = match content {
                        ToolResultContent::Text(t) => serde_json::json!({ "result": t }),
                        ToolResultContent::Json(v) => v.clone(),
                    };
                    parts.push(serde_json::json!({
                        "functionResponse": {
                            "name": tool_use_id,
                            "response": response,
                            "id": tool_use_id,
                        }
                    }));
                    if *is_error {
                        // Gemini treats errors via response content; flag in text.
                        if let Some(p) = parts.last_mut()
                            && let Some(obj) = p.as_object_mut()
                        {
                            obj.insert("error".into(), serde_json::json!(true));
                        }
                    }
                }
                ContentBlock::Reasoning { reasoning, .. } => {
                    parts.push(serde_json::json!({ "text": reasoning }));
                }
            }
        }
        if !parts.is_empty() {
            contents.push(serde_json::json!({ "role": role, "parts": parts }));
        }
    }
    (system, contents)
}

fn lower_tools(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "functionDeclarations": [{
                    "name": t["name"],
                    "description": t.get("description").unwrap_or(&serde_json::Value::Null),
                    "parameters": t.get("input_schema").cloned().unwrap_or_else(|| serde_json::json!({"type":"object"})),
                }]
            })
        })
        .collect()
}

#[derive(Default)]
struct GeminiSseState {
    usage: UsageStats,
    finalized: bool,
}

fn finalize_gemini(
    state: &mut GeminiSseState,
    catalog: &ModelCatalog,
    model_id: &str,
) -> Vec<ProviderEvent> {
    if state.finalized {
        return Vec::new();
    }
    state.finalized = true;
    if state.usage.cost == 0.0 && state.usage.input_tokens + state.usage.output_tokens > 0 {
        state.usage.cost = catalog.compute_cost("google", model_id, &state.usage);
    }
    vec![ProviderEvent::MessageStop {
        usage: state.usage.clone(),
        finish_reason: Some("stop".into()),
    }]
}

pub fn parse_gemini_chunk(
    data: &str,
    catalog: &ModelCatalog,
    provider_id: &str,
    model_id: &str,
) -> Vec<ProviderEvent> {
    let mut events = Vec::new();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
        return events;
    };

    if let Some(parts) = v["candidates"][0]["content"]["parts"].as_array() {
        for part in parts {
            if let Some(text) = part["text"].as_str() {
                events.push(ProviderEvent::TextDelta(text.to_string()));
            }
            if let Some(fc) = part.get("functionCall") {
                let id = fc["id"].as_str().unwrap_or("call").to_string();
                let name = fc["name"].as_str().unwrap_or("tool").to_string();
                let input = fc
                    .get("args")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                events.push(ProviderEvent::ToolUseComplete { id, name, input });
            }
        }
    }

    if let Some(usage) = v.get("usageMetadata") {
        let mut stats = UsageStats::default();
        stats.input_tokens = usage["promptTokenCount"].as_i64().unwrap_or(0);
        stats.output_tokens = usage["candidatesTokenCount"].as_i64().unwrap_or(0);
        stats.cost = catalog.compute_cost(provider_id, model_id, &stats);
        events.push(ProviderEvent::MessageStop {
            usage: stats,
            finish_reason: v["candidates"][0]["finishReason"]
                .as_str()
                .map(String::from),
        });
    }

    events
}

#[async_trait::async_trait]
impl ModelProvider for GoogleProvider {
    async fn stream_chat(
        &self,
        model_id: &str,
        system_prompt: Option<&str>,
        max_tokens: u32,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError> {
        let key = self.resolve_key()?;
        let (system_instruction, contents) = lower_gemini_messages(system_prompt, messages);
        let gemini_tools = if tools.is_empty() {
            None
        } else {
            Some(lower_tools(tools))
        };

        let body = GeminiRequest {
            system_instruction,
            contents,
            tools: gemini_tools,
            generation_config: serde_json::json!({
                "maxOutputTokens": max_tokens,
            }),
        };

        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse&key={}",
            self.base_url.trim_end_matches('/'),
            model_id,
            key
        );

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let message = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Api { status, message });
        }

        let catalog = ModelCatalog::from_vendored();
        let model_id_owned = model_id.to_string();
        let mut state = GeminiSseState::default();

        let stream = resp.bytes_stream().eventsource().flat_map(move |item| {
            let events: Vec<Result<ProviderEvent, ProviderError>> = match item {
                Ok(c) if c.data == "[DONE]" => {
                    finalize_gemini(&mut state, &catalog, &model_id_owned)
                        .into_iter()
                        .map(Ok)
                        .collect()
                }
                Ok(c) => {
                    let evs = parse_gemini_chunk(&c.data, &catalog, "google", &model_id_owned);
                    for ev in &evs {
                        if let ProviderEvent::MessageStop { usage, .. } = ev {
                            state.usage = usage.clone();
                        }
                    }
                    evs.into_iter().map(Ok).collect()
                }
                Err(e) => vec![Err(ProviderError::Other(e.to_string()))],
            };
            futures_util::stream::iter(events)
        });

        Ok(Box::pin(stream))
    }

    fn count_tokens(&self, _model_id: &str, text: &str) -> usize {
        (text.len() / 4).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_text_delta() {
        let cat = ModelCatalog::from_vendored();
        let data = r#"{"candidates":[{"content":{"parts":[{"text":"hello"}]}}]}"#;
        let evs = parse_gemini_chunk(data, &cat, "google", "gemini-2.5-flash");
        assert!(matches!(evs.first(), Some(ProviderEvent::TextDelta(t)) if t == "hello"));
    }
}
