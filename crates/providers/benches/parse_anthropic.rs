//! Microbenchmark for the Anthropic SSE wire parser.
//!
//! `parse_anthropic_event` is the single source of truth for the streaming state
//! machine (it backs both the live `stream_chat` client and the offline replay
//! harness), so it runs once per wire frame on every token of every turn — the
//! hottest decode path in the engine. This bench feeds a representative turn's
//! worth of already-JSON-parsed frames (message_start → a text block → a tool-use
//! block with accumulated `input_json` → message_delta usage → message_stop cost
//! math) through a fresh `SseState` each iteration so the tool-index map and the
//! end-of-turn cost arithmetic are exercised, not just the cheap text path.

use criterion::{Criterion, criterion_group, criterion_main};
use private_code_providers::anthropic::{SseState, parse_anthropic_event};
use serde_json::{Value, json};
use std::hint::black_box;

/// A representative single-turn frame sequence: text streaming plus one tool call
/// (so the `active_tool_uses` map and `input_json` accumulation are hit) and the
/// terminal usage/cost computation. A real model id seeds the price table so the
/// `message_stop` cost math does real multiplies rather than early-returning.
fn representative_frames() -> Vec<Value> {
    vec![
        json!({"type": "message_start", "message": {"usage": {
            "input_tokens": 1024, "cache_read_input_tokens": 256, "cache_creation_input_tokens": 64
        }}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
        json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "Here is a short answer with several tokens."}}),
        json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": " And a little more streamed text."}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1,
            "content_block": {"type": "tool_use", "id": "toolu_01", "name": "read_file"}}),
        json!({"type": "content_block_delta", "index": 1,
            "delta": {"type": "input_json_delta", "partial_json": "{\"path\":\"src/"}}),
        json!({"type": "content_block_delta", "index": 1,
            "delta": {"type": "input_json_delta", "partial_json": "main.rs\"}"}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "message_delta", "usage": {"output_tokens": 512},
            "delta": {"stop_reason": "tool_use"}}),
        json!({"type": "message_stop"}),
    ]
}

fn bench_parse_anthropic(c: &mut Criterion) {
    let frames = representative_frames();

    c.bench_function("parse_anthropic_event/representative_turn", |b| {
        b.iter(|| {
            // Fresh state per iteration: tool-index map + cost accumulators reset,
            // so we measure a full turn's decode, not a warmed-up partial one.
            let mut state = SseState::new("claude-opus-4-8");
            for frame in &frames {
                let out = parse_anthropic_event(black_box(frame), &mut state);
                black_box(out.ok());
            }
        });
    });
}

criterion_group!(benches, bench_parse_anthropic);
criterion_main!(benches);
