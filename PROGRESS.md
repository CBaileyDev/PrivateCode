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
| C10 | Desktop command→EngineState seam + set_model/agent/revert/compact + eviction | ✅ done |
| C11 | Desktop command-layer test harness | ✅ done |
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

## Phase-2 adversarial review (done)
Workflow `phase2-adversarial-review` (6 finder dimensions + 3 refute-by-default skeptics/finding) over the daemon + provider surface (C6–C9 + OpenAI-compat). Outcome — 2 real bugs fixed:
1. **(real, panel-split — the higher-stakes one)** A mid-turn steer emits `user(tool_result)` immediately followed by `user(steer)`; `lower_messages` sent them as consecutive `role:"user"` messages, which the Anthropic API rejects ("roles must alternate"). The recent first-party auto-merge is endpoint/version-dependent (Bedrock + claude-code#1162 still 400) and we can't run the live Anthropic check, so we normalize client-side via `push_or_merge`. The 2/3 panel refutation was the predictable failure of code-reading skeptics on a pure API-shape question; verified against the reference (which only merges system-updates) + a web check. Test: `adjacent_same_role_messages_are_merged_so_roles_alternate`.
2. **(high, confirmed 2/3)** `Error` events were constructed with `seq=0`, making them categorically un-replayable (`get_history` filters `seq > after_seq`; errors aren't in any content table). Fixed: a FRESH `next_event_seq` at all three emission sites (context_overflow, provider_error, stream_error) — not reusing `asst_seq` (would collide cursors and break dedup). The `get_history` filter is unchanged (loosening it would duplicate-deliver). Test: `error_event_carries_a_fresh_replayable_seq`.

## C10 done (desktop consolidated onto the shared coordinator)
Landed as four commits on green (per the advisor's "isolate each move" discipline):
1. **Pure refactor** — `git mv` `SessionCoordinator` daemon→core (transport-agnostic; it has zero axum imports). Daemon import sites updated; two nested `if let`s collapsed to let-chains (edition-2024 clippy). Same 82 tests, now with coordinator units under core.
2. **Hoist** — `compact_session`/`revert_session`/`unrevert_session` moved from the daemon routes into coordinator methods; revert/unrevert return `Option<SessionRow>` (None→4xx). The `"always"` permission-save folded INTO `reply_permission` **and** its lock-across-await fixed (capture action/resource under the lock, write the rule with the lock released, best-effort). Daemon routes are now thin wrappers. New `session_ops_compact_and_revert_unrevert_round_trip` over a real git workspace (no prior coverage).
3. **Primitives** — `evict_session` (idle-guarded: removes live state ONLY when no active turn / no parked permission / empty queue, check+remove under one lock — evicting a live-drain session would let a recreate race the old drain's settlement and double-spawn); `set_model` (always persists `model_config`, evicts only on a `provider_id` change since the provider is pinned at build but `model_id` is per-turn; busy provider-change → `Ok(false)` deferred); `set_agent` (pure DB write — `agent_id` is per-turn); `remove_session` (force, for DELETE). Tests: idle-guard across all three blockers, set_model contract, force-remove-when-busy.
4. **Desktop swap** — `EngineState` deleted; commands are a thin layer over `tauri::State<SessionCoordinator>` (built in `state::build_coordinator` with Anthropic + NVIDIA). `send_prompt`→`run_turn` (inherits steer/queue/backlog), `abort`→`abort_turn`, `reply_permission`→coordinator (deny-feedback + always-save), `delete`→`remove_session`, new `set_model`/`set_agent`/`compact`/`revert`/`unrevert` commands. `subscribe_session` now subscribes-then-snapshots with a replay watermark + `should_forward` dedup (was double-delivering). Lifecycle: `start_reaper` at setup, `coord.shutdown(10s)` on `RunEvent::ExitRequested` (best-effort `block_on`).
- **Deviation from advice (noted):** advisor suggested `set_model` *return an error* when a turn is active; I return `Ok(false)` instead (deferred-apply is not a failure — the config IS persisted). The UI still learns "not live yet" from the boolean and re-issues on the turn-ended event, which is the flow the advisor described. Same guarantees, cleaner contract.
- **Honest framing (advisor):** the green gate proves the desktop *compiles* and that the *core/daemon* logic it delegates to is tested. It does NOT exercise the desktop command layer at runtime (the desktop crate has zero tests yet). `subscribe_session`'s dedup and `send_prompt → run_turn` are validated only transitively (they mirror `ws.rs`, covered by `ws_roundtrip`); the `block_on` shutdown hook is a standard Tauri pattern, compiles, but is unverifiable without a GUI launch (best-effort). Record C10 as "desktop delegates to tested core; runtime coverage is C11," not "desktop verified."
- **C11 done (desktop command-layer harness):** a `#[cfg(test)]` module in `commands.rs` drives the REAL command fns headless — `tauri::test::mock_app` (dev-dep `tauri` feature `"test"`) manages a `SessionCoordinator` over an in-memory DB + `ScriptedProvider`, and a real `Channel::new(cb)` decodes the streamed `ProtocolEvent` JSON exactly as the webview's callback would. Four tests: (1) CRUD round-trip (init/create/list/get/set_agent/set_model/delete, asserting the DB reflects each); (2) `subscribe_session` + `send_prompt` → a completed turn streams `MessageCompleted` **exactly once** (no replay/live double-deliver) + the assistant message persists; (3) `get_usage` cold-session DB fallback + revert/compact/unrevert "nothing to do" → command-error shapes; (4) full permission round-trip — a `write_file` tool parks the turn, `ToolPermissionRequired` streams, a `reply_permission` DENY command resumes it to a NEW completion (the turn is blocked at the park, so the post-deny completion is causally attributable to the reply). This closes the advisor's "validated only transitively" gap for `subscribe_session`/`send_prompt`/`reply_permission`. The `block_on` shutdown hook remains GUI-launch-only (unverified, best-effort).
- **C12/C13 follow-ups (advisor-flagged):**
  - `send_prompt` now *queues/steers* a second prompt instead of returning the old "a turn is already running" error — a deliberate capability gain, but the frontend must STOP expecting that error (C12).
  - The deferred provider-change (`set_model` → `Ok(false)`) only goes live when something re-triggers eviction: either the UI re-issuing `set_model` on the turn-ended event, or the 30-min reaper. C13's turn-ended handler MUST genuinely re-call `set_model`, or a provider switch quietly lingers on the old provider for up to half an hour.
  - Wire the `delivery` arg (now accepted by `send_prompt`, defaults "steer"), the `set_model` deferred-bool, and deny-`feedback` into the frontend.

## Carry-forward notes
- **Deferred (Lagged-while-connected recovery):** the WS forward loop `continue`s on `broadcast::RecvError::Lagged`, relying on the client reconciling durable state from the DB. That holds for messages/checkpoints/usage but not for `Error` (in-memory only) — a client that lags past the 16384 buffer mid-turn could miss an error with no reconnect. Real but low-probability and a riskier hot-path change; the right fix is the forward loop re-syncing in-memory durable history on Lagged (track last-forwarded seq, replay history beyond it). Not bundled with the Error-seq fix.
- **C10 recon — RESOLVED (see "C10 done" above).** The bespoke `EngineState` (which carried the C7–C9 bugs: lock-across-await `ensure_session`, no shutdown/reaper, no steer/queue drain, no replay dedup, no provider routing) was replaced by the shared `SessionCoordinator`. The desktop is now a thin Tauri-command + Channel-forwarding layer that inherits every C6–C9 fix.
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
