#!/usr/bin/env bash
set -euo pipefail

if [[ -z "${NVIDIA_API_KEY:-}" ]]; then
  echo "NVIDIA_API_KEY must be set in the environment before running live NVIDIA smoke tests." >&2
  exit 1
fi

echo "=== Step 1: Running NVIDIA text smoke test ==="
cargo test -p private-code-providers --test live_smoke live_nvidia_text_turn_streams_and_completes -- --ignored --nocapture

echo ""
echo "=== Step 2: Running NVIDIA tool smoke test ==="
if ! cargo test -p private-code-providers --test live_smoke live_nvidia_tool_turn_yields_a_tool_use -- --ignored --nocapture; then
  echo ""
  echo "=== Step 2b: Default tool smoke failed; retrying with llama-3.1-405b ==="
  NVIDIA_TEST_MODEL=meta/llama-3.1-405b-instruct cargo test -p private-code-providers --test live_smoke live_nvidia_tool_turn_yields_a_tool_use -- --ignored --nocapture
fi

echo ""
echo "=== Step 3: Sanity check - offline tests ==="
cargo nextest run --workspace

echo ""
echo "=== Tests complete ==="
