//! Offline test doubles for the provider layer, behind the `testkit` feature so
//! they never ship in release builds. Two fidelity levels:
//!
//! * **Level 1** — [`ScriptedProvider`] replays pre-parsed [`ProviderEvent`]s
//!   (one `Vec` per turn) and records the `(model, system, messages, tools)` of
//!   every call, for driving and asserting orchestrator/coordinator loop logic.
//! * **Level 2** — [`parse_sse_to_events`] feeds raw SSE text through the very
//!   same [`parse_anthropic_event`] wire parser the live client uses, so the
//!   tool-index map, input_json accumulation and usage/cost math are exercised
//!   end-to-end, fully offline.

use crate::anthropic::{SseState, parse_anthropic_event};
use crate::provider::{ModelProvider, ProviderError, ProviderEvent};
use async_trait::async_trait;
use futures_util::stream::{BoxStream, StreamExt};
use private_code_protocol::event::UsageStats;
use private_code_protocol::message::ChatMessage;
use std::collections::VecDeque;
use std::sync::Mutex;

/// A recorded invocation of [`ScriptedProvider::stream_chat`].
#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub model_id: String,
    pub system_prompt: Option<String>,
    pub max_tokens: u32,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<serde_json::Value>,
}

/// Level-1 mock provider: replays scripted [`ProviderEvent`] turns and records
/// each call for assertions.
pub struct ScriptedProvider {
    turns: Mutex<VecDeque<Vec<ProviderEvent>>>,
    calls: Mutex<Vec<RecordedCall>>,
    /// When set, `count_tokens` returns this fixed value regardless of input —
    /// lets a test force the per-message budget high enough to trigger compaction.
    token_override: Option<usize>,
}

impl ScriptedProvider {
    /// One `Vec<ProviderEvent>` per turn, consumed front-to-back. A turn beyond
    /// the script yields an empty stream.
    pub fn new(turns: Vec<Vec<ProviderEvent>>) -> Self {
        Self {
            turns: Mutex::new(turns.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
            token_override: None,
        }
    }

    /// Convenience: a single turn that streams `text` then stops with `end_turn`.
    pub fn one_shot_text(text: impl Into<String>) -> Self {
        Self::new(vec![vec![
            ProviderEvent::TextDelta(text.into()),
            ProviderEvent::MessageStop {
                usage: zero_usage(),
                finish_reason: Some("end_turn".to_string()),
            },
        ]])
    }

    /// Force `count_tokens` to a fixed value (for budget/compaction tests).
    pub fn with_token_override(mut self, n: usize) -> Self {
        self.token_override = Some(n);
        self
    }

    /// All recorded calls, in order.
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }

    /// The model id of the most recent call, if any.
    pub fn last_model_id(&self) -> Option<String> {
        self.calls
            .lock()
            .unwrap()
            .last()
            .map(|c| c.model_id.clone())
    }

    /// Number of times `stream_chat` has been invoked.
    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    async fn stream_chat(
        &self,
        model_id: &str,
        system_prompt: Option<&str>,
        max_tokens: u32,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError> {
        self.calls.lock().unwrap().push(RecordedCall {
            model_id: model_id.to_string(),
            system_prompt: system_prompt.map(|s| s.to_string()),
            max_tokens,
            messages: messages.to_vec(),
            tools: tools.to_vec(),
        });
        let events = self.turns.lock().unwrap().pop_front().unwrap_or_default();
        let stream = futures_util::stream::iter(events.into_iter().map(Ok)).boxed();
        Ok(stream)
    }

    fn count_tokens(&self, _model_id: &str, text: &str) -> usize {
        if let Some(n) = self.token_override {
            return n;
        }
        (text.chars().count() / 4).max(1)
    }
}

/// A provider whose stream never resolves — for cancellation / abort tests.
pub struct PendingProvider;

#[async_trait]
impl ModelProvider for PendingProvider {
    async fn stream_chat(
        &self,
        _model_id: &str,
        _system_prompt: Option<&str>,
        _max_tokens: u32,
        _messages: &[ChatMessage],
        _tools: &[serde_json::Value],
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError> {
        let stream =
            futures_util::stream::pending::<Result<ProviderEvent, ProviderError>>().boxed();
        Ok(stream)
    }

    fn count_tokens(&self, _model_id: &str, text: &str) -> usize {
        (text.chars().count() / 4).max(1)
    }
}

/// Level-2 replay: decode raw SSE text (the `data:` payloads of an Anthropic
/// `/v1/messages` stream) through the SAME parser the live client uses. Pricing
/// is seeded from `model_id`. Returns the events in order; non-yielding frames
/// (`Ok(None)`) are dropped, mirroring the live `filter_map`.
pub fn parse_sse_to_events(model_id: &str, raw: &str) -> Vec<Result<ProviderEvent, ProviderError>> {
    let mut state = SseState::new(model_id);
    let mut out = Vec::new();
    for frame in raw.split("\n\n") {
        // SSE permits multiple `data:` lines per frame; concatenate with '\n'.
        let mut data = String::new();
        for line in frame.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            }
        }
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(data) {
            Ok(val) => match parse_anthropic_event(&val, &mut state) {
                Ok(Some(ev)) => out.push(Ok(ev)),
                Ok(None) => {}
                Err(e) => out.push(Err(e)),
            },
            Err(e) => out.push(Err(ProviderError::Json(e))),
        }
    }
    out
}

fn zero_usage() -> UsageStats {
    UsageStats {
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        cost: 0.0,
    }
}
