//! Level-2 offline replay: hand-authored Anthropic SSE fixtures decoded through
//! the REAL `parse_anthropic_event` wire parser (via `testkit::parse_sse_to_events`).
//! Proves the streaming state machine — tool-index map, input_json reassembly,
//! usage/cost math, stop_reason — end-to-end with no network and no API key.

use private_code_providers::ProviderEvent;
use private_code_providers::testkit::parse_sse_to_events;

fn unwrap_all(
    events: Vec<Result<ProviderEvent, private_code_providers::ProviderError>>,
) -> Vec<ProviderEvent> {
    events
        .into_iter()
        .map(|e| e.expect("fixture should parse without error"))
        .collect()
}

#[test]
fn thinking_text_tool_yields_exact_event_sequence() {
    let raw = include_str!("fixtures/thinking_text_tool.sse");
    let events = unwrap_all(parse_sse_to_events("claude-opus-4-8", raw));

    assert_eq!(events.len(), 7, "unexpected event sequence: {events:?}");
    assert!(
        matches!(&events[0], ProviderEvent::ReasoningDelta(t) if t == "Let me check the file."),
        "events[0] = {:?}",
        events[0]
    );
    assert!(
        matches!(&events[1], ProviderEvent::TextDelta(t) if t == "Reading now."),
        "events[1] = {:?}",
        events[1]
    );
    match &events[2] {
        ProviderEvent::ToolUseStart { id, name } => {
            assert_eq!(id, "toolu_01");
            assert_eq!(name, "read_file");
        }
        other => panic!("expected ToolUseStart, got {other:?}"),
    }
    assert!(matches!(&events[3], ProviderEvent::ToolUseDelta { .. }));
    assert!(matches!(&events[4], ProviderEvent::ToolUseDelta { .. }));
    match &events[5] {
        ProviderEvent::ToolUseComplete { id, name, input } => {
            assert_eq!(id, "toolu_01");
            assert_eq!(name, "read_file");
            assert_eq!(input, &serde_json::json!({ "path": "src/main.rs" }));
        }
        other => panic!("expected ToolUseComplete, got {other:?}"),
    }
    match &events[6] {
        ProviderEvent::MessageStop {
            usage,
            finish_reason,
        } => {
            assert_eq!(finish_reason.as_deref(), Some("tool_use"));
            assert_eq!(usage.input_tokens, 1500);
            assert_eq!(usage.cache_read_tokens, 900);
            assert_eq!(usage.cache_write_tokens, 100);
            assert_eq!(usage.output_tokens, 45);
            assert!(usage.cost > 0.0, "cost should be computed from usage");
        }
        other => panic!("expected MessageStop, got {other:?}"),
    }
}

#[test]
fn input_json_split_across_three_frames_reassembles() {
    let raw = include_str!("fixtures/split_input_3frames.sse");
    let events = unwrap_all(parse_sse_to_events("claude-haiku-4-5", raw));

    let input = events
        .iter()
        .find_map(|e| match e {
            ProviderEvent::ToolUseComplete { input, .. } => Some(input.clone()),
            _ => None,
        })
        .expect("a ToolUseComplete event");
    assert_eq!(input, serde_json::json!({ "path": "a.rs" }));
}

#[test]
fn text_then_stop_accumulates_text_and_reports_end_turn() {
    let raw = include_str!("fixtures/text_then_stop.sse");
    let events = unwrap_all(parse_sse_to_events("claude-sonnet-4-6", raw));

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::TextDelta(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello, world.");

    match events.last().expect("at least one event") {
        ProviderEvent::MessageStop {
            usage,
            finish_reason,
        } => {
            assert_eq!(finish_reason.as_deref(), Some("end_turn"));
            assert_eq!(usage.output_tokens, 12);
        }
        other => panic!("expected MessageStop last, got {other:?}"),
    }
}

#[test]
fn two_tool_calls_decode_in_order_with_distinct_inputs() {
    let raw = include_str!("fixtures/two_tools.sse");
    let events = unwrap_all(parse_sse_to_events("claude-opus-4-8", raw));

    let completes: Vec<(String, String, serde_json::Value)> = events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::ToolUseComplete { id, name, input } => {
                Some((id.clone(), name.clone(), input.clone()))
            }
            _ => None,
        })
        .collect();

    assert_eq!(completes.len(), 2, "{events:?}");
    assert_eq!(completes[0].0, "toolu_a");
    assert_eq!(completes[0].1, "list_dir");
    assert_eq!(completes[0].2, serde_json::json!({ "path": "." }));
    assert_eq!(completes[1].0, "toolu_b");
    assert_eq!(completes[1].1, "read_file");
    assert_eq!(completes[1].2, serde_json::json!({ "path": "x.rs" }));
}
