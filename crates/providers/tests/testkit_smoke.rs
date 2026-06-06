//! Confirms the `testkit` feature is wired so the crate's own integration tests
//! can use it under a plain `cargo test` (no `--features` flag). The richer SSE
//! replay + fixture assertions live in `sse_replay.rs` (cluster C2).

use futures_util::StreamExt;
use private_code_providers::testkit::{ScriptedProvider, parse_sse_to_events};
use private_code_providers::{ModelProvider, ProviderEvent};

#[tokio::test]
async fn scripted_provider_replays_events_and_records_the_call() {
    let provider = ScriptedProvider::new(vec![vec![
        ProviderEvent::TextDelta("hi".into()),
        ProviderEvent::TextDelta(" there".into()),
    ]]);

    let mut stream = provider
        .stream_chat("claude-haiku-4-5", Some("sys"), 256, &[], &[])
        .await
        .unwrap();

    let mut got = Vec::new();
    while let Some(ev) = stream.next().await {
        got.push(ev.unwrap());
    }

    assert_eq!(
        got,
        vec![
            ProviderEvent::TextDelta("hi".into()),
            ProviderEvent::TextDelta(" there".into()),
        ]
    );
    // The call was recorded for assertion.
    assert_eq!(provider.call_count(), 1);
    assert_eq!(
        provider.last_model_id().as_deref(),
        Some("claude-haiku-4-5")
    );
    assert_eq!(provider.calls()[0].max_tokens, 256);
}

#[test]
fn parse_sse_decodes_text_then_stop_through_the_real_parser() {
    let raw = concat!(
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n",
        "\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n",
        "\n",
    );

    let events = parse_sse_to_events("claude-opus-4-8", raw);
    assert!(
        matches!(events.first(), Some(Ok(ProviderEvent::TextDelta(t))) if t == "hello"),
        "first event should be the text delta, got {events:?}",
    );
    assert!(
        matches!(events.last(), Some(Ok(ProviderEvent::MessageStop { .. }))),
        "last event should be MessageStop, got {events:?}",
    );
}
