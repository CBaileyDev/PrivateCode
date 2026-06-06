//! LIVE end-to-end smoke tests against the real Anthropic `/v1/messages` API.
//!
//! These are the one surface the offline suite cannot cover: that our request
//! shape is accepted by the real API and that the SSE parser handles a genuine
//! stream. They are `#[ignore]`d so `cargo test`/CI never touch the network or
//! spend tokens — run them explicitly with a key:
//!
//!   ANTHROPIC_API_KEY=sk-ant-... cargo test -p private-code-providers --test live_smoke -- --ignored --nocapture
//!
//! (The key is also read from the OS keyring entry service="private-code",
//! account="anthropic" if no env var is set — same path the daemon uses.)
//!
//! Each test SKIPS (prints a notice and returns) when no key is resolvable, so
//! `--ignored` without a configured key is a harmless no-op rather than a panic.

use futures_util::StreamExt;
use private_code_protocol::message::{ChatMessage, ContentBlock, Role};
use private_code_providers::{AnthropicProvider, ModelProvider, ProviderEvent, resolve_api_key};

fn key_available() -> bool {
    if resolve_api_key("anthropic").is_ok() {
        return true;
    }
    eprintln!(
        "SKIP: no Anthropic key (set ANTHROPIC_API_KEY or the 'private-code/anthropic' keyring entry)"
    );
    false
}

fn user(text: &str) -> ChatMessage {
    ChatMessage {
        id: "live-test-user".to_string(),
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        created_at: 0,
    }
}

/// A plain text turn: send a trivial prompt, assert the real stream yields text
/// and terminates with a stop reason. Proves the request shape is accepted and
/// the SSE decoder parses a genuine Opus 4.8 stream end to end.
#[tokio::test]
#[ignore = "live network + API tokens; run explicitly with --ignored"]
async fn live_text_turn_streams_and_completes() {
    if !key_available() {
        return;
    }
    let provider = AnthropicProvider::new();
    let mut stream = provider
        .stream_chat(
            "claude-opus-4-8",
            Some("You are a terse assistant. Answer in at most five words."),
            128,
            &[user("Reply with exactly: pong")],
            &[],
        )
        .await
        .expect("the real API must accept our request shape");

    let mut text = String::new();
    let mut finish_reason: Option<String> = None;
    while let Some(ev) = stream.next().await {
        match ev.expect("each SSE event must parse") {
            ProviderEvent::TextDelta(t) => text.push_str(&t),
            ProviderEvent::MessageStop {
                finish_reason: fr, ..
            } => finish_reason = fr,
            _ => {}
        }
    }

    eprintln!("LIVE text turn -> {text:?} (finish_reason={finish_reason:?})");
    assert!(!text.trim().is_empty(), "the model must return some text");
    assert!(
        finish_reason.is_some(),
        "a terminal MessageStop with a finish_reason must be parsed"
    );
}

/// A tool turn: offer one tool and a prompt that needs it, assert the real
/// stream yields a complete tool-use block. Proves the `tools` request shape is
/// accepted and the SSE tool-call accumulation (start -> deltas -> complete)
/// parses against a genuine stream.
#[tokio::test]
#[ignore = "live network + API tokens; run explicitly with --ignored"]
async fn live_tool_turn_yields_a_tool_use() {
    if !key_available() {
        return;
    }
    let tools = vec![serde_json::json!({
        "name": "get_weather",
        "description": "Get the current weather for a city.",
        "input_schema": {
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"]
        }
    })];

    let provider = AnthropicProvider::new();
    let mut stream = provider
        .stream_chat(
            "claude-opus-4-8",
            Some("Use the get_weather tool when asked about weather."),
            512,
            &[user(
                "What is the weather in Paris? Use the get_weather tool.",
            )],
            &tools,
        )
        .await
        .expect("the real API must accept our tools request shape");

    let mut saw_tool_use = false;
    let mut tool_name = String::new();
    let mut finish_reason: Option<String> = None;
    while let Some(ev) = stream.next().await {
        match ev.expect("each SSE event must parse") {
            ProviderEvent::ToolUseStart { name, .. } => {
                saw_tool_use = true;
                tool_name = name;
            }
            ProviderEvent::ToolUseComplete { name, input, .. } => {
                saw_tool_use = true;
                tool_name = name;
                eprintln!("LIVE tool_use -> {tool_name}({input})");
            }
            ProviderEvent::MessageStop {
                finish_reason: fr, ..
            } => finish_reason = fr,
            _ => {}
        }
    }

    assert!(
        saw_tool_use,
        "the model should have emitted a tool_use block (got finish_reason={finish_reason:?})"
    );
    assert_eq!(
        tool_name, "get_weather",
        "the offered tool must be selected"
    );
}
