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
| C8 | Daemon eviction reaper + steer/queue + ToolRequested | ✅ done (C8a reaper, C8b ToolRequested, C8c-1 queue/drain, C8c-2 steer) |
| C9 | Daemon lock-across-await + durable replay | ✅ done (C9a lock fix, C9b replay dedup, C9c deny-feedback, C9d cold-reconnect verified) |
| — | OpenAI-compatible provider + NVIDIA routing (user request) | ✅ done |
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
- **C10 recon (desktop EngineState):** `apps/desktop/src-tauri/src/state.rs` `EngineState` is a partial, stale mirror of `SessionCoordinator`. It carries the SAME bugs/gaps the daemon fixed in C7–C9: (a) `ensure_session` holds the sessions `Mutex` across the `db::get_session().await` (the C9a lock-across-await anti-pattern); (b) no graceful shutdown / TaskTracker; (c) no idle reaper; (d) no steer/queue drain (need to check how its run-turn handles a 2nd prompt); (e) no replay seq-watermark dedup on the Channel forward path; (f) no per-session provider routing. **Strong C10 option:** replace the bespoke `EngineState` with the shared `SessionCoordinator` (transport-agnostic — returns a `broadcast::Receiver`, exposes get_or_create/run_turn/abort/reply/get_history/shutdown), so the desktop becomes a thin Tauri-command + Channel-forwarding layer that inherits every C6–C9 fix instead of duplicating them. Decide with the advisor at C10 start. Also wire the deny `feedback` arg (added to the desktop `reply_permission` command) into the frontend at C13, and evict+recreate on `provider_id`/model change (provider is pinned at session creation).
- **C9d resolved (no event-log table):** every durable event has a REST content home — MessageCompleted/ToolRequested/ToolOutput → `session_message` (get_messages), CheckpointCreated → `checkpoint` (list_checkpoints), UsageUpdated → `session` row. The only homeless events (ToolPermissionRequired, Error) are transient and can't be pending post-settlement (the reaper evicts only when no active turn / no pending permission / empty queue). So warm in-memory replay + the C9b seq-watermark dedup suffices; the deliberately-removed DB *reconstruction* (content-less, double-counting) stays removed. Verified by `cold_reconnect_after_eviction_reconstructs_from_db`.
- **NVIDIA / OpenAI-compat provider — C13/catalog follow-ups (advisor-flagged):**
  - `context_window` is hardcoded 200k; wrong for non-Claude models (llama-3.3 is 128k). Degraded-not-broken: the orchestrator's context-overflow 400-retry compaction backstops it. Fix with the model catalog (Phase 5) keyed by model id.
  - The provider is selected at session creation and cached in the orchestrator, so a mid-session `provider_id` switch won't take effect until eviction. When C13 wires provider/model selection, it MUST evict + recreate the session on a `provider_id` change or the switch silently no-ops.
- **C8c-2 abort/steer boundary — RESOLVED (advisor-flagged, now implemented):** `abort_turn` clears the in-memory queue and cancels the active turn but leaves abandoned inputs as unpromoted `session_input` rows (session.md L163). The orchestrator's mid-turn steer scan (`promote_pending_steers`) only folds steers with `admitted_seq > chain_watermark` (the opening input's `admitted_seq`). Because abort clears the *entire* VecDeque, the only post-abort chain opener is a fresh prompt with a higher seq, so an aborted/orphan steer (lower seq) is never resurrected into a later activity. Orphans are preserved-but-skipped by design (durable recovery slice deferred — session.md L100/L161). Regression test `abandoned_steer_is_not_resurrected_into_a_later_activity` fails without the watermark, passes with it.
- **C12/C15 (frontend):** the new `compaction` message `type` needs render handling — the message store's `JSON.parse` fallback will otherwise show the raw `{compacted_through_seq,summary}` JSON. Render it as a "history compacted" divider (or hide it; the summary already reaches the model via the system prefix).
- Compaction summary is a deterministic non-LLM stub (Phase 1); `auto=true` by default but only fires near the 200K window.

## Notes / caveats (honesty log)
- Remote CI green is **pending the user's authed push + an actual Actions run** — I verify by running each job's exact commands locally where the tool is installed.
- GUI launch and live BYOK Claude API calls are explicitly out of scope (human-required).
