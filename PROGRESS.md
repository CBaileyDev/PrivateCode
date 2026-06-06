# A-grade marathon — progress tracker

Goal: take Phases 1–3 to A grade in **every area that does not require a human to launch the GUI or make a live BYOK Claude call**. Workspace must stay green at every cluster boundary: `cargo fmt --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, workspace tests (nextest), and for the desktop `npm run typecheck && npm run build && vitest run`.

Blueprint of record: the consolidated A-grade plan (clusters C0–C16) produced by the design/review workflow. Each cluster is committed + pushed to https://github.com/CBaileyDev/PrivateCode on completion.

## Status

| Cluster | Title | Status |
|---|---|---|
| C0 | Repo hygiene + CI + supply-chain foundation | ✅ done |
| C1 | Provider parser extraction + base_url DI + testkit | ⬜ pending |
| C2 | SSE-byte replay harness + edit/patch property tests | ⬜ pending |
| C3 | Phase-1 orchestrator correctness bugs | ⬜ pending |
| C4 | Git-backed E2E turn + reconcile-arms + compaction impl | ⬜ pending |
| C5 | Permission-park + tool.run cancellability (CRITICAL prereq) | ⬜ pending |
| C6 | Daemon serve refactor (build_router/serve_daemon/DI) | ⬜ pending |
| C7 | Daemon graceful shutdown + WS round-trip test | ⬜ pending |
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

## Notes / caveats (honesty log)
- Remote CI green is **pending the user's authed push + an actual Actions run** — I verify by running each job's exact commands locally where the tool is installed.
- GUI launch and live BYOK Claude API calls are explicitly out of scope (human-required).
