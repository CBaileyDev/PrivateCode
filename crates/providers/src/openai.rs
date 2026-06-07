//! An OpenAI-compatible chat-completions provider (`POST {base}/chat/completions`,
//! `Authorization: Bearer`, OpenAI-style SSE). This is the protocol exposed by
//! OpenAI itself and by NVIDIA's `integrate.api.nvidia.com/v1`, vLLM, Ollama,
//! LM Studio, Together, Groq, and most "OpenAI-compatible" gateways — distinct
//! from Anthropic's `/v1/messages` (see [`crate::anthropic`]).
//!
//! The provider is generic over `(key_name, base_url)` so one implementation
//! serves any such endpoint; [`OpenAiCompatProvider::nvidia`] is a convenience
//! constructor for NVIDIA. The streaming decoder is a small state machine that
//! reassembles OpenAI's per-`index` tool-call argument fragments and emits a
//! single terminal [`ProviderEvent::MessageStop`] exactly once (on `[DONE]` or
//! stream end), mirroring the contract the orchestrator already relies on.

use crate::catalog::ModelCatalog;
use crate::provider::{ModelProvider, ProviderError, ProviderEvent, resolve_api_key};
use eventsource_stream::Eventsource;
use futures_util::stream::{BoxStream, StreamExt};
use private_code_protocol::event::UsageStats;
use private_code_protocol::message::{ChatMessage, ContentBlock, Role, ToolResultContent};
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::OnceLock;

/// NVIDIA's OpenAI-compatible inference gateway.
pub const NVIDIA_BASE_URL: &str = "https://integrate.api.nvidia.com/v1";
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/v1";
pub const GROQ_BASE_URL: &str = "https://api.groq.com/openai/v1";
pub const OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434/v1";
pub const LMSTUDIO_BASE_URL: &str = "http://127.0.0.1:1234/v1";

pub struct OpenAiCompatProvider {
    client: reqwest::Client,
    /// Credential namespace for key resolution (keyring account + the
    /// `<NAME>_API_KEY` env fallback), e.g. "nvidia" or "openai".
    key_name: String,
    base_url: String,
    api_key: OnceLock<String>,
}

impl OpenAiCompatProvider {
    /// Generic constructor: `key_name` selects the credential (keyring account
    /// `key_name` under service "private-code", or the `<KEY_NAME>_API_KEY` env
    /// var); `base_url` is the API root that `/chat/completions` is appended to.
    pub fn new(key_name: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            key_name: key_name.into(),
            base_url: base_url.into(),
            api_key: OnceLock::new(),
        }
    }

    /// NVIDIA's hosted models (key `nvidia` / `NVIDIA_API_KEY`).
    pub fn nvidia() -> Self {
        Self::new("nvidia", NVIDIA_BASE_URL)
    }

    pub fn openai() -> Self {
        Self::new("openai", OPENAI_BASE_URL)
    }

    pub fn deepseek() -> Self {
        Self::new("deepseek", DEEPSEEK_BASE_URL)
    }

    pub fn groq() -> Self {
        Self::new("groq", GROQ_BASE_URL)
    }

    /// Local Ollama (no API key required; port auto-detected separately).
    pub fn ollama() -> Self {
        Self::new("ollama", OLLAMA_BASE_URL)
    }

    /// Local LM Studio OpenAI-compatible endpoint.
    pub fn lmstudio() -> Self {
        Self::new("lmstudio", LMSTUDIO_BASE_URL)
    }

    /// Resolve and cache the API key (keyring → env fallback), so the OS keychain
    /// is hit at most once.
    fn resolve_key(&self) -> Result<String, ProviderError> {
        if let Some(k) = self.api_key.get() {
            return Ok(k.clone());
        }
        let k = resolve_api_key(&self.key_name)?;
        let _ = self.api_key.set(k.clone());
        Ok(k)
    }
}

#[derive(Serialize)]
struct OpenAiRequest {
    model: String,
    messages: Vec<serde_json::Value>,
    max_tokens: u32,
    stream: bool,
    /// Ask the server to emit a final usage chunk (OpenAI-spec, honoured by NVIDIA).
    stream_options: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
}

/// One in-flight tool call being reassembled across streamed deltas.
#[derive(Default)]
struct ToolCallAccum {
    id: String,
    name: String,
    args: String,
    started: bool,
}

/// Decoder state for an OpenAI-style chat-completions SSE stream.
pub struct OpenAiSseState {
    tool_calls: Vec<ToolCallAccum>,
    usage: UsageStats,
    finish_reason: Option<String>,
    finalized: bool,
}

impl OpenAiSseState {
    pub fn new() -> Self {
        Self {
            tool_calls: Vec::new(),
            usage: UsageStats::default(),
            finish_reason: None,
            finalized: false,
        }
    }
}

impl Default for OpenAiSseState {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse one streamed chunk (`{choices:[{delta, finish_reason}], usage?}`),
/// updating `state` and returning the events it produced (text/reasoning/tool
/// deltas). The terminal `MessageStop` is NOT emitted here — see [`finalize`].
pub fn parse_openai_chunk(
    val: &serde_json::Value,
    state: &mut OpenAiSseState,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    let mut out = Vec::new();

    // A usage-only chunk (sent last when stream_options.include_usage) has empty
    // choices but carries the token totals.
    if let Some(usage) = val.get("usage").filter(|u| u.is_object()) {
        let prompt = usage["prompt_tokens"].as_i64().unwrap_or(0);
        // Cached prompt tokens, when the gateway reports them. OpenAI's
        // `prompt_tokens` INCLUDES cached tokens, so subtract them out of the
        // (full-price) input bucket — otherwise cost double-counts the cache hit.
        let cached = usage["prompt_tokens_details"]["cached_tokens"]
            .as_i64()
            .unwrap_or(0);
        state.usage.cache_read_tokens = cached;
        state.usage.input_tokens = (prompt - cached).max(0);
        state.usage.output_tokens = usage["completion_tokens"].as_i64().unwrap_or(0);
    }

    let Some(choice) = val["choices"].get(0) else {
        return Ok(out);
    };
    let delta = &choice["delta"];

    // Visible text.
    if let Some(content) = delta["content"].as_str()
        && !content.is_empty()
    {
        out.push(ProviderEvent::TextDelta(content.to_string()));
    }

    // Reasoning text: DeepSeek-R1 / Nemotron-style models on NVIDIA stream chain-
    // of-thought in `reasoning_content` (some use `reasoning`). Surface as reasoning
    // (without a signature — these are not Anthropic thinking blocks).
    for key in ["reasoning_content", "reasoning"] {
        if let Some(r) = delta[key].as_str()
            && !r.is_empty()
        {
            out.push(ProviderEvent::ReasoningDelta(r.to_string()));
        }
    }

    // Tool-call fragments, keyed by `index` and reassembled across chunks.
    if let Some(tool_calls) = delta["tool_calls"].as_array() {
        for tc in tool_calls {
            let index = tc["index"].as_u64().unwrap_or(0) as usize;
            if state.tool_calls.len() <= index {
                state
                    .tool_calls
                    .resize_with(index + 1, ToolCallAccum::default);
            }
            let slot = &mut state.tool_calls[index];
            if let Some(id) = tc["id"].as_str()
                && !id.is_empty()
            {
                slot.id = id.to_string();
            }
            if let Some(name) = tc["function"]["name"].as_str()
                && !name.is_empty()
            {
                slot.name = name.to_string();
            }
            // Emit the start once we know id + name.
            if !slot.started && !slot.id.is_empty() && !slot.name.is_empty() {
                slot.started = true;
                out.push(ProviderEvent::ToolUseStart {
                    id: slot.id.clone(),
                    name: slot.name.clone(),
                });
            }
            if let Some(args) = tc["function"]["arguments"].as_str()
                && !args.is_empty()
            {
                slot.args.push_str(args);
                if slot.started {
                    out.push(ProviderEvent::ToolUseDelta {
                        id: slot.id.clone(),
                        input_delta: args.to_string(),
                    });
                }
            }
        }
    }

    if let Some(fr) = choice["finish_reason"].as_str()
        && !fr.is_empty()
    {
        state.finish_reason = Some(fr.to_string());
    }

    Ok(out)
}

/// Emit the terminal events exactly once: a `ToolUseComplete` for each fully
/// reassembled tool call (parsing its accumulated JSON arguments), then a single
/// `MessageStop` carrying catalog-computed cost. Idempotent — the second caller
/// (e.g. stream-end after `[DONE]`) gets nothing.
pub fn finalize(
    state: &mut OpenAiSseState,
    catalog: &ModelCatalog,
    provider_id: &str,
    model_id: &str,
) -> Vec<ProviderEvent> {
    if state.finalized {
        return Vec::new();
    }
    state.finalized = true;

    let mut out = Vec::new();
    for tc in &state.tool_calls {
        if tc.id.is_empty() {
            continue;
        }
        // Empty args is the no-argument call `{}`; otherwise parse the buffer.
        let input = if tc.args.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&tc.args).unwrap_or(serde_json::Value::Null)
        };
        out.push(ProviderEvent::ToolUseComplete {
            id: tc.id.clone(),
            name: tc.name.clone(),
            input,
        });
    }
    // OpenAI-compatible gateways don't return a dollar cost — compute it from the
    // model catalog (keyed by the provider's credential namespace, e.g. "nvidia"),
    // so every turn reports real spend instead of $0.00. Uncatalogued models keep
    // the existing (zero) cost honestly.
    if state.usage.cost == 0.0
        && state.usage.input_tokens + state.usage.output_tokens + state.usage.cache_read_tokens > 0
    {
        state.usage.cost = catalog.compute_cost(provider_id, model_id, &state.usage);
    }
    out.push(ProviderEvent::MessageStop {
        usage: state.usage.clone(),
        finish_reason: state.finish_reason.clone(),
    });
    out
}

/// Lower our protocol messages to the OpenAI `messages` array. `system_prompt`
/// becomes a leading `system` message; tool results become dedicated `tool`
/// messages (OpenAI requires one per `tool_call_id`); assistant tool calls become
/// `tool_calls`; reasoning blocks are dropped on input (no portable input form).
fn lower_openai_messages(
    system_prompt: Option<&str>,
    messages: &[ChatMessage],
) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();

    if let Some(sys) = system_prompt
        && !sys.is_empty()
    {
        out.push(serde_json::json!({ "role": "system", "content": sys }));
    }

    for msg in messages {
        match msg.role {
            Role::System => {
                let text = system_text(msg);
                if !text.is_empty() {
                    out.push(serde_json::json!({ "role": "system", "content": text }));
                }
            }
            Role::User => {
                // Tool results are emitted as standalone `tool` messages; plain
                // text accumulates into a single `user` message.
                let mut user_text = String::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => {
                            if !user_text.is_empty() {
                                user_text.push('\n');
                            }
                            user_text.push_str(text);
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            out.push(serde_json::json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": tool_result_text(content),
                            }));
                        }
                        _ => {}
                    }
                }
                if !user_text.is_empty() {
                    out.push(serde_json::json!({ "role": "user", "content": user_text }));
                }
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut tool_calls: Vec<serde_json::Value> = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text: t } => {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(t);
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(serde_json::json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string()),
                                }
                            }));
                        }
                        // Reasoning is not replayable as OpenAI input — drop it.
                        ContentBlock::Reasoning { .. } | ContentBlock::ToolResult { .. } => {}
                    }
                }
                if tool_calls.is_empty() {
                    if !text.is_empty() {
                        out.push(serde_json::json!({ "role": "assistant", "content": text }));
                    }
                } else {
                    // With tool_calls, content may be empty; send null to satisfy
                    // strict gateways.
                    let content = if text.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::String(text)
                    };
                    out.push(serde_json::json!({
                        "role": "assistant",
                        "content": content,
                        "tool_calls": tool_calls,
                    }));
                }
            }
        }
    }

    out
}

fn system_text(msg: &ChatMessage) -> String {
    let mut s = String::new();
    for b in &msg.content {
        if let ContentBlock::Text { text } = b {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(text);
        }
    }
    s
}

fn tool_result_text(content: &ToolResultContent) -> String {
    match content {
        ToolResultContent::Text(t) => t.clone(),
        ToolResultContent::Json(v) => serde_json::to_string(v).unwrap_or_default(),
    }
}

/// Convert our Anthropic-shaped tool schemas (`{name, description, input_schema}`)
/// to OpenAI function tools (`{type:"function", function:{name, description,
/// parameters}}`).
fn lower_tools(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t["name"],
                    "description": t.get("description").cloned().unwrap_or(serde_json::Value::Null),
                    "parameters": t.get("input_schema").cloned().unwrap_or(serde_json::json!({"type": "object"})),
                }
            })
        })
        .collect()
}

#[async_trait::async_trait]
impl ModelProvider for OpenAiCompatProvider {
    async fn stream_chat(
        &self,
        model_id: &str,
        system_prompt: Option<&str>,
        max_tokens: u32,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError> {
        let api_key = self.resolve_key()?;

        let request = OpenAiRequest {
            model: model_id.to_string(),
            messages: lower_openai_messages(system_prompt, messages),
            max_tokens,
            stream: true,
            stream_options: serde_json::json!({ "include_usage": true }),
            tools: if tools.is_empty() {
                None
            } else {
                Some(lower_tools(tools))
            },
        };

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let response = self
            .client
            .post(&url)
            .bearer_auth(&api_key)
            .header("content-type", "application/json")
            .json(&request)
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

        // Stream state: the SSE source, the decoder, a pending-event queue (one
        // chunk can yield several events), and a finished flag so we finalize once.
        struct StreamState {
            inner: BoxStream<
                'static,
                Result<
                    eventsource_stream::Event,
                    eventsource_stream::EventStreamError<reqwest::Error>,
                >,
            >,
            sse: OpenAiSseState,
            pending: VecDeque<Result<ProviderEvent, ProviderError>>,
            finished: bool,
            catalog: ModelCatalog,
            provider_id: String,
            model_id: String,
        }

        let st = StreamState {
            inner: response.bytes_stream().eventsource().boxed(),
            sse: OpenAiSseState::new(),
            pending: VecDeque::new(),
            finished: false,
            catalog: ModelCatalog::from_vendored(),
            provider_id: self.key_name.clone(),
            model_id: model_id.to_string(),
        };

        let stream = futures_util::stream::unfold(st, |mut st| async move {
            loop {
                if let Some(ev) = st.pending.pop_front() {
                    return Some((ev, st));
                }
                if st.finished {
                    return None;
                }
                match st.inner.next().await {
                    Some(Ok(event)) => {
                        // OpenAI's terminal sentinel — not JSON.
                        if event.data.trim() == "[DONE]" {
                            for ev in
                                finalize(&mut st.sse, &st.catalog, &st.provider_id, &st.model_id)
                            {
                                st.pending.push_back(Ok(ev));
                            }
                            st.finished = true;
                            continue;
                        }
                        match serde_json::from_str::<serde_json::Value>(&event.data) {
                            Ok(val) => match parse_openai_chunk(&val, &mut st.sse) {
                                Ok(evs) => st.pending.extend(evs.into_iter().map(Ok)),
                                Err(e) => st.pending.push_back(Err(e)),
                            },
                            Err(e) => st.pending.push_back(Err(ProviderError::Json(e))),
                        }
                        continue;
                    }
                    Some(Err(e)) => {
                        st.pending
                            .push_back(Err(ProviderError::Other(e.to_string())));
                        continue;
                    }
                    None => {
                        // Stream ended without an explicit [DONE]; finalize once.
                        for ev in finalize(&mut st.sse, &st.catalog, &st.provider_id, &st.model_id)
                        {
                            st.pending.push_back(Ok(ev));
                        }
                        st.finished = true;
                        continue;
                    }
                }
            }
        });

        Ok(stream.boxed())
    }

    fn count_tokens(&self, _model_id: &str, text: &str) -> usize {
        (text.chars().count() / 4).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn text_stream_reassembles_and_finalizes_once() {
        let mut st = OpenAiSseState::new();
        let mut evs = Vec::new();
        evs.extend(
            parse_openai_chunk(
                &chunk(r#"{"choices":[{"delta":{"content":"Hel"},"finish_reason":null}]}"#),
                &mut st,
            )
            .unwrap(),
        );
        evs.extend(
            parse_openai_chunk(
                &chunk(r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":null}]}"#),
                &mut st,
            )
            .unwrap(),
        );
        evs.extend(
            parse_openai_chunk(
                &chunk(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#),
                &mut st,
            )
            .unwrap(),
        );
        evs.extend(
            parse_openai_chunk(
                &chunk(r#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":2}}"#),
                &mut st,
            )
            .unwrap(),
        );
        // finalize twice — second is a no-op. Uncatalogued model → cost stays 0.
        let cat = ModelCatalog::from_vendored();
        evs.extend(finalize(&mut st, &cat, "openai", "uncatalogued-test-model"));
        evs.extend(finalize(&mut st, &cat, "openai", "uncatalogued-test-model"));

        assert_eq!(
            evs,
            vec![
                ProviderEvent::TextDelta("Hel".into()),
                ProviderEvent::TextDelta("lo".into()),
                ProviderEvent::MessageStop {
                    usage: UsageStats {
                        input_tokens: 11,
                        output_tokens: 2,
                        ..Default::default()
                    },
                    finish_reason: Some("stop".into()),
                },
            ]
        );
    }

    #[test]
    fn tool_call_arguments_reassemble_across_chunks() {
        let mut st = OpenAiSseState::new();
        let mut evs = Vec::new();
        // id+name arrive first, then argument fragments.
        evs.extend(parse_openai_chunk(&chunk(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}"#), &mut st).unwrap());
        evs.extend(parse_openai_chunk(&chunk(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]}}]}"#), &mut st).unwrap());
        evs.extend(parse_openai_chunk(&chunk(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Paris\"}"}}]}}]}"#), &mut st).unwrap());
        evs.extend(
            parse_openai_chunk(
                &chunk(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#),
                &mut st,
            )
            .unwrap(),
        );
        let cat = ModelCatalog::from_vendored();
        evs.extend(finalize(&mut st, &cat, "openai", "uncatalogued-test-model"));

        assert_eq!(
            evs[0],
            ProviderEvent::ToolUseStart {
                id: "call_1".into(),
                name: "get_weather".into()
            }
        );
        // The complete carries the reassembled JSON object.
        match evs
            .iter()
            .find(|e| matches!(e, ProviderEvent::ToolUseComplete { .. }))
            .unwrap()
        {
            ProviderEvent::ToolUseComplete { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "get_weather");
                assert_eq!(input, &serde_json::json!({ "city": "Paris" }));
            }
            _ => unreachable!(),
        }
        assert!(
            matches!(evs.last().unwrap(), ProviderEvent::MessageStop { finish_reason, .. } if finish_reason.as_deref() == Some("tool_calls"))
        );
    }

    #[test]
    fn lowering_maps_roles_tools_and_results() {
        use private_code_protocol::message::{ChatMessage, ContentBlock, Role, ToolResultContent};
        let mk = |role, content| ChatMessage {
            id: "x".into(),
            role,
            content,
            created_at: 0,
        };
        let msgs = vec![
            mk(Role::User, vec![ContentBlock::Text { text: "hi".into() }]),
            mk(
                Role::Assistant,
                vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "get_weather".into(),
                    input: serde_json::json!({ "city": "Paris" }),
                }],
            ),
            mk(
                Role::User,
                vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: ToolResultContent::Text("sunny".into()),
                    is_error: false,
                }],
            ),
        ];

        let out = lower_openai_messages(Some("be terse"), &msgs);
        assert_eq!(
            out[0],
            serde_json::json!({ "role": "system", "content": "be terse" })
        );
        assert_eq!(
            out[1],
            serde_json::json!({ "role": "user", "content": "hi" })
        );
        // Assistant tool call: arguments serialized to a string.
        assert_eq!(out[2]["role"], "assistant");
        assert_eq!(out[2]["tool_calls"][0]["function"]["name"], "get_weather");
        assert_eq!(
            out[2]["tool_calls"][0]["function"]["arguments"],
            serde_json::json!("{\"city\":\"Paris\"}")
        );
        // Tool result becomes a dedicated `tool` message.
        assert_eq!(
            out[3],
            serde_json::json!({ "role": "tool", "tool_call_id": "call_1", "content": "sunny" })
        );
    }

    /// A catalogued model gets a non-zero dollar cost at finalize (previously
    /// every OpenAI-compatible turn reported $0.00).
    #[test]
    fn finalize_computes_cost_from_catalog() {
        let mut st = OpenAiSseState::new();
        parse_openai_chunk(
            &chunk(r#"{"choices":[],"usage":{"prompt_tokens":1000000,"completion_tokens":0}}"#),
            &mut st,
        )
        .unwrap();
        let cat = ModelCatalog::from_vendored();
        let evs = finalize(&mut st, &cat, "openai", "gpt-4.1");
        let stop = evs
            .iter()
            .find_map(|e| match e {
                ProviderEvent::MessageStop { usage, .. } => Some(usage),
                _ => None,
            })
            .unwrap();
        assert!(stop.cost > 0.0, "catalogued model reports real cost");
    }

    /// Cached prompt tokens are excluded from the full-price input bucket so cost
    /// can't double-count them.
    #[test]
    fn cached_tokens_are_subtracted_from_input() {
        let mut st = OpenAiSseState::new();
        parse_openai_chunk(
            &chunk(
                r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":40}}}"#,
            ),
            &mut st,
        )
        .unwrap();
        assert_eq!(st.usage.input_tokens, 60, "input = prompt - cached");
        assert_eq!(st.usage.cache_read_tokens, 40);
        assert_eq!(st.usage.output_tokens, 10);
    }

    #[test]
    fn tools_lower_to_openai_function_shape() {
        let anthropic_tools = vec![serde_json::json!({
            "name": "get_weather",
            "description": "weather",
            "input_schema": { "type": "object", "properties": { "city": { "type": "string" } } }
        })];
        let out = lower_tools(&anthropic_tools);
        assert_eq!(out[0]["type"], "function");
        assert_eq!(out[0]["function"]["name"], "get_weather");
        assert_eq!(out[0]["function"]["description"], "weather");
        assert_eq!(
            out[0]["function"]["parameters"]["properties"]["city"]["type"],
            "string"
        );
    }
}
