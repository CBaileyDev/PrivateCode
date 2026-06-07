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
use std::collections::{HashMap, VecDeque};

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

pub struct GoogleProvider {
    client: reqwest::Client,
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
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
        }
    }

    /// Resolve the API key (keyring → env) on EACH turn — not cached — so a key
    /// added/removed/changed in the GUI Settings takes effect immediately.
    fn resolve_key(&self) -> Result<String, ProviderError> {
        resolve_api_key("google")
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
    /// Last-seen finishReason. Gemini may deliver `finishReason` and
    /// `usageMetadata` in the SAME or in SEPARATE SSE chunks, so it is
    /// accumulated and emitted once at stream end (the old code only read it
    /// inside the usage chunk → telemetry saw `None` whenever they arrived apart).
    finish_reason: Option<String>,
}

/// Normalize Gemini's finishReason vocabulary to the internal form.
fn map_gemini_finish_reason(raw: &str) -> String {
    match raw {
        "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "SAFETY" => "safety",
        "RECITATION" => "recitation",
        other => other,
    }
    .to_string()
}

/// Emit the single terminal `MessageStop` (idempotent). Gemini sends no `[DONE]`
/// sentinel and may omit `usageMetadata`, so finalize MUST run at stream end to
/// guarantee exactly one terminal event carrying the accumulated usage, cost, and
/// finishReason (the old code's only `MessageStop` lived inside the usage chunk,
/// so a stream without `usageMetadata` reported nothing and `finalize_gemini` was
/// dead code).
fn finalize_gemini(
    state: &mut GeminiSseState,
    catalog: &ModelCatalog,
    provider_id: &str,
    model_id: &str,
) -> Vec<ProviderEvent> {
    if state.finalized {
        return Vec::new();
    }
    state.finalized = true;
    if state.usage.cost == 0.0 && state.usage.input_tokens + state.usage.output_tokens > 0 {
        state.usage.cost = catalog.compute_cost(provider_id, model_id, &state.usage);
    }
    vec![ProviderEvent::MessageStop {
        usage: state.usage.clone(),
        finish_reason: state
            .finish_reason
            .clone()
            .or_else(|| Some("stop".to_string())),
    }]
}

/// Parse one SSE chunk into streaming events (text + tool calls) and ACCUMULATE
/// terminal state (usage, finishReason) into `state`. It never emits
/// `MessageStop` — that is produced exactly once by [`finalize_gemini`].
fn parse_gemini_chunk(data: &str, state: &mut GeminiSseState) -> Vec<ProviderEvent> {
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

    if let Some(fr) = v["candidates"][0]["finishReason"].as_str() {
        state.finish_reason = Some(map_gemini_finish_reason(fr));
    }
    if let Some(usage) = v.get("usageMetadata") {
        // Guard each field on presence so a partial usageMetadata chunk can't zero
        // a value seen earlier (and so a thoughts-only chunk can't fabricate an
        // output total). Matches Reference gemini.ts mapUsage.
        if let Some(prompt) = usage["promptTokenCount"].as_i64() {
            state.usage.input_tokens = prompt;
        }
        // `candidatesTokenCount` is VISIBLE-text-only and excludes thinking
        // tokens; sum it with `thoughtsTokenCount` for the inclusive output count
        // (otherwise output — and thus cost — under-reports for thinking models).
        if let Some(candidates) = usage["candidatesTokenCount"].as_i64() {
            let thoughts = usage["thoughtsTokenCount"].as_i64().unwrap_or(0);
            state.usage.output_tokens = candidates + thoughts;
        }
        if let Some(thoughts) = usage["thoughtsTokenCount"].as_i64() {
            state.usage.reasoning_tokens = thoughts;
        }
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
            "{}/models/{}:streamGenerateContent?alt=sse",
            self.base_url.trim_end_matches('/'),
            model_id
        );

        let resp = self
            .client
            .post(&url)
            // Authenticate with a header, NOT a `?key=` query param. reqwest attaches
            // the full request URL to transport errors (DNS/TLS/timeout), and those
            // errors are stringified into user-facing error events and the tracing
            // log — a query-param key would leak the user's secret there. The header
            // form keeps the key out of every URL the error machinery can capture.
            .header("x-goog-api-key", key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let message = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Api { status, message });
        }

        // Drive the SSE source with `unfold` (not `flat_map`) so a normal stream
        // END triggers exactly one terminal `finalize_gemini` — Gemini never sends
        // a `[DONE]` sentinel, so flat_map would silently drop the MessageStop.
        struct GState {
            inner: BoxStream<
                'static,
                Result<
                    eventsource_stream::Event,
                    eventsource_stream::EventStreamError<reqwest::Error>,
                >,
            >,
            sse: GeminiSseState,
            pending: VecDeque<Result<ProviderEvent, ProviderError>>,
            finished: bool,
            catalog: ModelCatalog,
            model_id: String,
        }

        let gst = GState {
            inner: resp.bytes_stream().eventsource().boxed(),
            sse: GeminiSseState::default(),
            pending: VecDeque::new(),
            finished: false,
            catalog: ModelCatalog::from_vendored(),
            model_id: model_id.to_string(),
        };

        let stream = futures_util::stream::unfold(gst, |mut st| async move {
            loop {
                if let Some(ev) = st.pending.pop_front() {
                    return Some((ev, st));
                }
                if st.finished {
                    return None;
                }
                match st.inner.next().await {
                    Some(Ok(c)) if c.data.trim() == "[DONE]" => {
                        for ev in finalize_gemini(&mut st.sse, &st.catalog, "google", &st.model_id)
                        {
                            st.pending.push_back(Ok(ev));
                        }
                        st.finished = true;
                    }
                    Some(Ok(c)) => {
                        for ev in parse_gemini_chunk(&c.data, &mut st.sse) {
                            st.pending.push_back(Ok(ev));
                        }
                    }
                    Some(Err(e)) => {
                        st.pending
                            .push_back(Err(ProviderError::Other(e.to_string())));
                    }
                    None => {
                        // Normal stream end (no [DONE]): emit the one MessageStop.
                        for ev in finalize_gemini(&mut st.sse, &st.catalog, "google", &st.model_id)
                        {
                            st.pending.push_back(Ok(ev));
                        }
                        st.finished = true;
                    }
                }
            }
        });

        Ok(stream.boxed())
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
        let mut st = GeminiSseState::default();
        let data = r#"{"candidates":[{"content":{"parts":[{"text":"hello"}]}}]}"#;
        let evs = parse_gemini_chunk(data, &mut st);
        assert!(matches!(evs.first(), Some(ProviderEvent::TextDelta(t)) if t == "hello"));
    }

    /// finishReason and usageMetadata accumulate across chunks and surface in the
    /// single terminal MessageStop emitted by finalize — not as `None` (the old
    /// bug) when they arrive apart. parse itself emits NO MessageStop.
    #[test]
    fn gemini_finish_reason_accumulated_and_emitted_at_finalize() {
        let cat = ModelCatalog::from_vendored();
        let mut st = GeminiSseState::default();
        // Text chunk: no finishReason/usage yet.
        let _ = parse_gemini_chunk(
            r#"{"candidates":[{"content":{"parts":[{"text":"hi"}]}}]}"#,
            &mut st,
        );
        // Terminal metadata chunk (finishReason + usage, no content parts).
        let evs = parse_gemini_chunk(
            r#"{"candidates":[{"finishReason":"MAX_TOKENS"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":3}}"#,
            &mut st,
        );
        assert!(
            !evs.iter()
                .any(|e| matches!(e, ProviderEvent::MessageStop { .. })),
            "MessageStop is emitted only at finalize"
        );

        let fin = finalize_gemini(&mut st, &cat, "google", "gemini-2.5-flash");
        assert_eq!(fin.len(), 1, "exactly one terminal MessageStop");
        match &fin[0] {
            ProviderEvent::MessageStop {
                usage,
                finish_reason,
            } => {
                assert_eq!(finish_reason.as_deref(), Some("length"));
                assert_eq!(usage.input_tokens, 5);
                assert_eq!(usage.output_tokens, 3);
            }
            other => panic!("expected MessageStop, got {other:?}"),
        }
        // Idempotent.
        assert!(
            finalize_gemini(&mut st, &cat, "google", "gemini-2.5-flash").is_empty(),
            "finalize is idempotent"
        );
    }

    /// Gemini's candidatesTokenCount is visible-only; thinking tokens
    /// (thoughtsTokenCount) must be summed into output_tokens and recorded as
    /// reasoning_tokens (else output + cost under-report for thinking models).
    #[test]
    fn gemini_usage_sums_thoughts_into_output_and_records_reasoning() {
        let cat = ModelCatalog::from_vendored();
        let mut st = GeminiSseState::default();
        // Mirrors the recorded fixture: prompt 55, visible 15, thoughts 45.
        let _ = parse_gemini_chunk(
            r#"{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":55,"candidatesTokenCount":15,"thoughtsTokenCount":45}}"#,
            &mut st,
        );
        let fin = finalize_gemini(&mut st, &cat, "google", "gemini-2.5-flash");
        match &fin[0] {
            ProviderEvent::MessageStop { usage, .. } => {
                assert_eq!(usage.input_tokens, 55);
                assert_eq!(usage.output_tokens, 60, "candidates(15) + thoughts(45)");
                assert_eq!(usage.reasoning_tokens, 45);
            }
            other => panic!("expected MessageStop, got {other:?}"),
        }
    }

    /// finalize defaults finishReason to "stop" when Gemini never sent one.
    #[test]
    fn gemini_finalize_defaults_finish_reason() {
        let cat = ModelCatalog::from_vendored();
        let mut st = GeminiSseState::default();
        st.usage.input_tokens = 1;
        let fin = finalize_gemini(&mut st, &cat, "google", "gemini-2.5-flash");
        assert!(matches!(
            &fin[0],
            ProviderEvent::MessageStop { finish_reason, .. } if finish_reason.as_deref() == Some("stop")
        ));
    }

    /// Gemini's wire `functionCall` carries no `id` (verified against the Reference
    /// recording), so each call must get a UNIQUE synthetic id — otherwise two
    /// parallel calls collide on the orchestrator's tool-result routing.
    #[test]
    fn gemini_parallel_function_calls_get_distinct_ids() {
        let mut st = GeminiSseState::default();
        let data = r#"{"candidates":[{"content":{"parts":[
            {"functionCall":{"name":"get_weather","args":{"city":"Paris"}}},
            {"functionCall":{"name":"get_weather","args":{"city":"London"}}}
        ]}}]}"#;
        let evs = parse_gemini_chunk(data, &mut st);
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
