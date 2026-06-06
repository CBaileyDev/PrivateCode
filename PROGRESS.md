# A-grade marathon — progress tracker

Goal: take Phases 1–3 to A grade in **every area that does not require a human to launch the GUI or make a live BYOK Claude call**. Workspace must stay green at every cluster boundary: `cargo fmt --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, workspace tests (nextest), and for the desktop `npm run typecheck && npm run build && vitest run`.

Blueprint of record: the consolidated A-grade plan (clusters C0–C16) produced by the design/review workflow. Each cluster is committed + pushed to https://github.com/CBaileyDev/PrivateCode on completion.

## Status

| Cluster | Title | Status |
|---|---|---|
| C0 | Repo hygiene + CI + supply-chain foundation | ✅ done |
| C1 | Provider parser extraction + base_url DI + testkit | ✅ done |
| C2 | SSE-byte replay harness + edit/patch property tests | ✅ done |
| C3 | Phase-1 orchestrator correctness bugs | ✅ done |
| C4 | Git-backed E2E turn + reconcile-arms + compaction impl | ✅ done |
| C5 | Permission-park + tool.run cancellability (CRITICAL prereq) | ✅ done |
| C6 | Daemon serve refactor (build_router/serve_daemon/DI) | ✅ done |
| C7 | Daemon graceful shutdown + WS round-trip test | ✅ done |
| C8 | Daemon eviction reaper + steer/queue + ToolRequested | ⬜ pending |
| C9 | Daemon lock-across-await + durable replay | ⬜ pending |
| C10 | Desktop command→EngineState seam + set_model/agent/revert/compact + eviction | ⬜ pending |
| C11 | Desktop command-layer test harness | ⬜ pending |
| C12 | Desktop frontend bugs (XSS, session bleed, locks, panics) | ⬜ pending |
| C13 | Wire model/agent dropdowns + slash commands | ⬜ pending |
| C14 | MessageList virtualization (virtua) + real Shiki-in-worker | ⬜ pending |
| C15 | Frontend store test suite (vitest + mockIPC) | ⬜ pending |
| C16 | Perf instrumentation (--selftest + criterion + perf.yml) | ⬜ pending |

Dependency order: C0 → C1 → {C2,C3,C4} → C5,C6 → {C7,C8,C9} → C10 → {C11,C12,C13} → {C14,C15} → C16

## Phase-1 adversarial review (done)
Workflow `phase1-adversarial-review` (13 agents) confirmed 6 real bugs in C1–C5; all fixed:
1. **(critical)** compaction summary prepended as a leading inline `role:"system"` → Anthropic 400 (can't be messages[0]; rejected on non-opus). Fixed: anthropic.rs now gates inline system on opus+valid-position and otherwise wraps as a `<system-update>` user message (reference-grounded `lower_messages`).
2–4. **(high ×3)** empty-content messages (`content:[]`) persisted on stream-error / permission-cancel / stream-cancel → wedge the session (400, no self-heal). Fixed: orchestrator never persists empty assistant/tool_result rows; anthropic.rs drops empty-content messages on replay.
5. **(medium)** `build_summary` head-truncated (dropped newly-folded content on repeated compactions). Fixed: keep the most-recent content.
6. **(low)** adjacent thinking blocks merged + concatenated signatures. Fixed: a signature seals a reasoning block; the next delta starts a new one.

## Carry-forward notes
- **C12/C15 (frontend):** the new `compaction` message `type` needs render handling — the message store's `JSON.parse` fallback will otherwise show the raw `{compacted_through_seq,summary}` JSON. Render it as a "history compacted" divider (or hide it; the summary already reaches the model via the system prefix).
- Compaction summary is a deterministic non-LLM stub (Phase 1); `auto=true` by default but only fires near the 200K window.

## Notes / caveats (honesty log)
- Remote CI green is **pending the user's authed push + an actual Actions run** — I verify by running each job's exact commands locally where the tool is installed.
- GUI launch and live BYOK Claude API calls are explicitly out of scope (human-required).
