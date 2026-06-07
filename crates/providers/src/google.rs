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
use std::collections::HashMap;
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

    // Gemini matches a `functionResponse` to its `functionCall` BY NAME, and our
    // `ContentBlock::ToolResult` carries only the tool_use_id (no name). Build an
    // id→name map from the preceding `ToolUse` blocks (which always come before
    // their result in message order) so the response can be keyed by the real
    // function name.
    let mut id_to_name: HashMap<String, String> = HashMap::new();
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
                    id_to_name.insert(id.clone(), name.clone());
                    // Gemini's functionCall is keyed by name; it carries no id
                    // (matches the Reference lowering and the recorded wire shape).
                    parts.push(serde_json::json!({
                        "functionCall": {
                            "name": name,
                            "args": input,
                        }
                    }));
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    // Key by the real function name (Gemini matches by name), not
                    // the internal tool_use_id.
                    let fn_name = id_to_name
                        .get(tool_use_id)
                        .cloned()
                        .unwrap_or_else(|| tool_use_id.clone());
                    let content_val = match content {
                        ToolResultContent::Text(t) => serde_json::json!(t),
                        ToolResultContent::Json(v) => v.clone(),
                    };
                    let mut response = serde_json::json!({
                        "name": &fn_name,
                        "content": content_val,
                    });
                    if *is_error && let Some(obj) = response.as_object_mut() {
                        obj.insert("error".into(), serde_json::json!(true));
                    }
                    parts.push(serde_json::json!({
                        "functionResponse": {
                            "name": &fn_name,
                            "response": response,
                        }
                    }));
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
    /// Monotonic counter minting unique synthetic ids for Gemini tool calls
    /// (Gemini's wire `functionCall` carries no id, so two parallel calls would
    /// otherwise collide on the orchestrator's result routing).
    tool_call_seq: u64,
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

fn parse_gemini_chunk(
    data: &str,
    state: &mut GeminiSseState,
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
                // Gemini sends no id; honour one if present, else mint a unique one.
                let id = fc["id"].as_str().map(str::to_string).unwrap_or_else(|| {
                    let n = state.tool_call_seq;
                    state.tool_call_seq += 1;
                    format!("call_{n}")
                });
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
                    let evs = parse_gemini_chunk(
                        &c.data,
                        &mut state,
                        &catalog,
                        "google",
                        &model_id_owned,
                    );
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
        let mut st = GeminiSseState::default();
        let data = r#"{"candidates":[{"content":{"parts":[{"text":"hello"}]}}]}"#;
        let evs = parse_gemini_chunk(data, &mut st, &cat, "google", "gemini-2.5-flash");
        assert!(matches!(evs.first(), Some(ProviderEvent::TextDelta(t)) if t == "hello"));
    }

    /// Gemini's wire `functionCall` carries no `id` (verified against the Reference
    /// recording), so each call must get a UNIQUE synthetic id — otherwise two
    /// parallel calls collide on the orchestrator's tool-result routing.
    #[test]
    fn gemini_parallel_function_calls_get_distinct_ids() {
        let cat = ModelCatalog::from_vendored();
        let mut st = GeminiSseState::default();
        let data = r#"{"candidates":[{"content":{"parts":[
            {"functionCall":{"name":"get_weather","args":{"city":"Paris"}}},
            {"functionCall":{"name":"get_weather","args":{"city":"London"}}}
        ]}}]}"#;
        let evs = parse_gemini_chunk(data, &mut st, &cat, "google", "gemini-2.5-flash");
        let ids: Vec<String> = evs
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::ToolUseComplete { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 2, "both tool calls decode");
        assert_ne!(ids[0], ids[1], "parallel tool calls must get distinct ids");
    }

    /// On the way back to Gemini, `functionResponse` must be keyed by the real
    /// function NAME (Gemini matches by name), not the internal tool_use_id.
    #[test]
    fn gemini_tool_result_keyed_by_function_name() {
        use private_code_protocol::message::{ChatMessage, ContentBlock, Role, ToolResultContent};
        let mk = |role, content| ChatMessage {
            id: "x".into(),
            role,
            content,
            created_at: 0,
        };
        let msgs = vec![
            mk(
                Role::User,
                vec![ContentBlock::Text {
                    text: "weather?".into(),
                }],
            ),
            mk(
                Role::Assistant,
                vec![ContentBlock::ToolUse {
                    id: "call_0".into(),
                    name: "get_weather".into(),
                    input: serde_json::json!({"city":"Paris"}),
                }],
            ),
            mk(
                Role::User,
                vec![ContentBlock::ToolResult {
                    tool_use_id: "call_0".into(),
                    content: ToolResultContent::Text("sunny".into()),
                    is_error: false,
                }],
            ),
        ];
        let (_system, contents) = lower_gemini_messages(None, &msgs);

        // The assistant functionCall is keyed by name.
        let fc = contents
            .iter()
            .flat_map(|c| c["parts"].as_array().cloned().unwrap_or_default())
            .find(|p| p.get("functionCall").is_some())
            .expect("a functionCall part exists");
        assert_eq!(fc["functionCall"]["name"], "get_weather");

        // The tool result's functionResponse is keyed by the function NAME.
        let fr = contents
            .iter()
            .flat_map(|c| c["parts"].as_array().cloned().unwrap_or_default())
            .find(|p| p.get("functionResponse").is_some())
            .expect("a functionResponse part exists");
        assert_eq!(
            fr["functionResponse"]["name"], "get_weather",
            "functionResponse must be keyed by the function name, not the tool_use_id"
        );
        assert_eq!(fr["functionResponse"]["response"]["name"], "get_weather");
    }
}
