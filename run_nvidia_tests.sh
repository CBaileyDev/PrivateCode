#!/bin/bash

cd /Users/carterbarker/Downloads/PrivateCode

echo "=== Step 1: Git pull ==="
git pull origin main

echo ""
echo "=== Step 2: Running NVIDIA smoke test ==="
export NVIDIA_API_KEY='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'
cargo test -p private-code-providers --test live_smoke nvidia -- --ignored --nocapture

echo ""
echo "=== Step 3: If tool use failed, trying with llama model ==="
NVIDIA_TEST_MODEL=meta/llama-3.1-405b-instruct cargo test -p private-code-providers --test live_smoke live_nvidia_tool -- --ignored --nocapture

echo ""
echo "=== Step 4: Sanity check - offline tests ==="
cargo nextest run --workspace

echo ""
echo "=== Tests complete ==="
