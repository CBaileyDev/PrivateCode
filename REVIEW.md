# Lead Developer Review — Private Code Plan

**Reviewer:** Senior lead pass (Claude Opus 4.8), reconciled against the OpenCode reference and the live 2026 toolchain.
**Date:** 2026-06-05
**Scope:** `plan.md`, `specs/{database,context_engine,api_protocol}.md`, cross-checked against `PROJECT_END_GOAL.MD` (North Star) and the read-only `Reference/` codebase.

---

## Verdict: ✅ APPROVED WITH CHANGES

The plan keeps the **right top-level architecture and stack** — one Rust core behind a ratatui TUI, a Tauri 2 + Solid.js GUI, and an axum daemon; SQLite/WAL + FTS5 + nucleo; gix-default git; a metadata-driven provider layer. The North Star §15 stack decisions are sound and **mostly stand**.

But it **must not be built exactly as written.** The plan was drafted by a weaker model that paraphrased a shipping system (OpenCode) and, in doing so, introduced **three P0 regressions** that are individually serious, plus stale 2026 API facts and a set of faithful-to-reference corrections. Every blocking issue is *"the weak model erred — restore the proven design,"* **not** a re-architecture. I verified every load-bearing claim against the reference source (`permission.ts`, `agent/agent.ts`, `session/context-epoch.ts`, `snapshot/index.ts`, `session/event.ts`, `tool/edit.ts`) and the live API surface (extism 1.30, tree-sitter 0.26 ABI, virtua, rmcp, Anthropic 2026).

This review and the rewritten `plan.md` + specs apply all P0 and P1 fixes. **Read this file first** — it is the decision record. The detailed designs live in the updated `plan.md` and `specs/`.

### The three P0 regressions (must fix before any build)

| # | Regression as written | Why it's wrong | Restored design | Ref |
|---|---|---|---|---|
| **1** | `PermissionClass{Safe, ReadWrite, Dangerous}` is wired as the **gating decision** (`Safe → auto-allow`, `Dangerous → always prompt`). | It **overrides** the action/resource rule engine. A user rule `{bash, "git status", allow}` (config's own example) would still prompt; a `deny`/`ask` on a read tool would be silently ignored. The reference has **no such class** (verified: empty grep). | Make the rule engine `evaluate(action, resource, …rulesets)` the **sole** authority. Demote the class to non-authorizing tool metadata that only seeds a default rule and decides whether to take a checkpoint. | `permission.ts` |
| **2** | A single `INSERT … ON CONFLICT DO UPDATE SET baseline=…, revision=revision+1` for **every** context-epoch write. | Overwrites the baseline on every change, so the most common reconcile outcome — *"date changed → emit one mid-conversation message → advance snapshot, keep identical baseline"* — is **unrepresentable**. This **defeats prompt-cache stability**, the entire point of the epoch system. | Three distinct ops: `insert` (rev 0, conflict-do-nothing), `advance` (snapshot-only CAS, baseline untouched), `replace` (new baseline CAS). | `session/context-epoch.ts` |
| **3** | Checkpoint = on-branch staging commit + `git reset --hard HEAD`. | **Both destructive and a no-op:** it mutates the user's branch tip/reflog/index (corrupts in-progress rebase/merge), and `reset --hard HEAD` targets HEAD — *not* the snapshot — so it discards uncommitted work while failing to actually revert. | Shadow git-dir + `write-tree`/`read-tree`/`checkout-index`, never touching the user's branch/HEAD/stash. New spec: [`specs/checkpointing.md`](specs/checkpointing.md). | `snapshot/index.ts` |

---

## How this review was produced

A 12-agent fan-out, each grounded in the actual reference and the live 2026 toolchain, consolidated into one prioritized change-list and verified by hand at the cited reference paths:

- **5 research agents** read the OpenCode reference (session runtime, context epochs, permission model, checkpoint/snapshot, edit/patch tools) and verified current crate/API facts via the web (extism, tree-sitter, virtua, rmcp, gix vs git2, sqlx).
- **6 review agents** critiqued architecture, correctness/concurrency, performance, security, testing/roadmap, and the wire protocol/data model.
- **1 alignment agent** cross-checked the plan against the North Star and resolved the 9 open decisions.

**Guiding rule:** where the plan diverged from the reference, the default was *"restore the proven design,"* not *"rearchitect."* Preferences (e.g. sqlx vs rusqlite) were **not** relitigated — North Star §15 defaults are kept unless there was a correctness reason to change.

---

## Change Log

Grouped by priority. **P0** = correctness/safety/reference-restore that blocks a faithful build. **P1** = important for a correct, performant v1. **P2** = polish / scoped-later. Each is applied in the rewritten `plan.md` and specs.

### P0 — blocking

| # | Change | Kind | Docs |
|---|---|---|---|
| 1 | Rule engine is the **sole** permission authority; `PermissionClass` demoted to non-authorizing metadata (seeds a default rule + triggers checkpoint only). | reference-restore | `plan §3, §6, §6A` |
| 2 | Permission **evaluation algorithm** restored: agent-deny short-circuit (non-overridable) → `findLast` over `[…agentRules, …savedRules]` → aggregate `deny > ask > allow` → `ask` fallback. Saved rules always coerce to `allow`. Drop the separate "Global" tier. | correctness | `plan §6A; api_protocol` |
| 3 | Context epoch = **three** ops (`insert` / `advance`-snapshot-only-CAS / `replace`-baseline-CAS), not one upsert. | correctness | `database, context_engine, plan §5` |
| 4 | Checkpoint = **shadow git-dir + write-tree/read-tree/checkout-index**; never mutate user branch/HEAD/stash. New `specs/checkpointing.md`. | safety | `plan §6, Step 1.7; checkpointing` |
| 5 | Checkpoint **write engine = git2** (open bare shadow repo, `Index::add_all`, `write_tree_to`, `checkout_index`); gix for read-only ops. Executes §15.5's own fallback clause. | correctness | `plan §1.7, §15` |
| 6 | **Remove all hardcoded model IDs** (`claude-sonnet-4-20250514`, `gpt-4o`, `gemini-2.5-pro`, "Claude 3.5 Sonnet"); resolve from the catalog (`default()/small()/cheapest_capable()`). | correctness | `plan §6B, §8, Step 4.8` |
| 7 | Built-in agent defaults restored to the reference + one §7.10 override: `build` = reference carve-outs (`doom_loop:ask`, `external_directory:ask`, `read.*.env:ask`) **+ `bash:ask`**, **not** `{*,*,allow}`. Add the missing **`explore`** subagent (the real search-only agent the plan mislabeled "general"). Add hidden `compaction/title/summary`. | reference-restore | `plan §6A, §6C` |
| 8 | `stream_chat` returns a **typed event stream with a terminal `Finish(reason, usage)`**; delete the unreachable `StreamCompletion`/`stream_completion`. Cross-provider `FinishReason` (`stop/length/tool_calls/content_filter/refusal/truncation/error`); capture the streaming `model_context_window_exceeded` as a **successful** `Truncation`, distinct from the pre-generation 400. | correctness | `plan §3, §6D` |
| 9 | Thread a `tokio_util::CancellationToken` through `stream_chat` so a mid-turn abort RPC can stop a long-lived streaming task. | correctness | `plan §3, Step 1.8, §2.2` |
| 10 | Split token counting: a **sync local `estimate_tokens(request)`** for the per-turn budget gate + an **optional async `count_tokens`** that POSTs the structured body to `/v1/messages/count_tokens`. **Never tiktoken** (wrong for Claude), never per-turn network. | correctness | `plan §3, §6D` |
| 11 | Proactive **budget-driven compaction** (pre-turn local estimate vs `context_window − reserve`; rolling summary; drop provider-native reasoning/tool messages across the boundary via a Replacement epoch). `/compact` is the manual entry. Keep the 400-overflow path as a single bounded retry. | reference-restore | `plan §6D, §6B; context_engine` |
| 12 | Add a **security threat model + hardening** (new `specs/security.md`): web_fetch SSRF guards, child-env scrubbing, authenticated WS upgrade + Origin/Host validation, closed-by-default WASM sandbox, untrusted-content trust boundary. | safety | `plan Step 1.5/2.1/2.3, §11; security` |
| 13 | Add `cargo-audit` + `cargo-deny` (committed `deny.toml`) to CI on the same triggers; commit & verify `Cargo.lock`; pin `extism`/`wasmtime`. | reference-restore | `plan §12` |
| 14 | Rewrite the WASM sample to **extism 1.x** (`Manifest` + `PluginBuilder` + `host_fn!`); `with_wasi(false)`, empty `allowed_hosts`/`allowed_paths`, capability host fns with path containment. | correctness | `plan §11, Step 5.3` |
| 15 | Desktop GUI = **in-process engine over Tauri Channels** (default) + first-class **loopback-daemon attach** mode. Drop "proxy to daemon REST/WS" + "CORS for Tauri" from the in-process path. Redefine the GUI budget as the whole desktop process tree `< 150MB` (no daemon double-count). | correctness | `plan Step 3.1/3.5/2.1, §2` |
| 16 | Fix per-session `seq` allocation: never `SELECT MAX(seq)+1` in a deferred txn (lost-write race). Serialize through the per-session coordinator and allocate from a counter inside `BEGIN IMMEDIATE`. Note event-sourcing as the V2 direction. | correctness | `database (Insert Message)` |
| 17 | Specify **durable turn-loop semantics**: partial assistant output on abort is persisted-as-terminal-partial or discarded by rule (never dangling); interrupted tools fail durably on abort/startup before the next request; idempotent admission; rename `delivery` `direct/queued` → reference `steer/queue`. | correctness | `plan Step 1.8; database` |

### P1 — important for a correct v1

| # | Change | Kind |
|---|---|---|
| 18 | Optimistic-concurrency **fencing** on the epoch replace/advance (`WHERE revision = :expected`; zero rows → retry at next boundary). Keep `session_id` as PK (one current epoch is correct). | correctness |
| 19 | **Move canonical message types to `protocol`** (`Role`, `ChatMessage`, `ContentBlock`, `ToolResultContent`, `ToolDefinition`, `UsageStats`, `FinishReason`, stream events). `providers`/`tools`/`core` depend on `protocol`, fixing the inverted DAG. | reference-restore |
| 19b| Enrich the message model to the reference's tagged-union part model (`user/assistant/synthetic/system/agent_switched/model_switched/compaction`; parts `text/reasoning/tool{pending,running,completed,error}`). | reference-restore |
| 20 | Restore `ContextSource` to the reference's **typed-codec + infallible-load** shape: `load() → Loaded | Unavailable`, per-source codec for equivalence, distinguish loaded-empty (removal) from Unavailable (retain prior, block replacement). Rename reconcile enum → `Unchanged | Updated{text,snapshot} | ReplacementReady | ReplacementBlocked`. Route AGENTS.md changes through **Updated**, not "compaction". | reference-restore |
| 21 | Add a `reasoning_effort` field to `ProviderConfig` (`none..max`); gate `temperature` to providers that still accept it (omit for Anthropic Opus 4.7/4.8). | correctness |
| 22 | Fix the **cost model**: non-overlapping usage breakdown (`non_cached + cache_read + cache_write + reasoning`); add `cache_write_cost` to `ModelInfo`; sum independently-priced categories. Render the §8 synthesis template **dynamically** (one block per candidate); add a **pre-dispatch fan-out budget guard**. | correctness |
| 23 | GUI virtual list = **virtua (`virtua/solid`)**, not `@tanstack/solid-virtual`; fix the §10 skeleton (`createVirtualizer`, not React's `useVirtualizer`; measured heights drive `translateY`, not a hardcoded `estimateSize:120`). | reference-restore |
| 24 | **Split the WS framing**: client→server commands stay request/response; server→client is a typed event stream with **Durable vs Ephemeral** classification (deltas are Ephemeral, coalesced server-side, don't advance the replay cursor). Replay on attach from a **durable cursor** (`after=seq`), not a 1000-event in-memory buffer. Align event names to the North Star taxonomy. | reference-restore |
| 25 | Permission-reply wire shape: one `permission_id` identity + `once|always|reject` (with optional feedback), not `approved: true`. | reference-restore |
| 26 | Fan-out: replace `tokio::join!` with timeout-wrapped futures + a k-of-N quorum so a hung provider can't stall synthesis; emit per-candidate status. | correctness |
| 27 | **Split `edit` and `patch`**: `edit` = exact-string unique-match str-replace + **staleness guard** (write-if-unchanged vs bytes read this turn) + CRLF/BOM handling; `patch` = a "Begin Patch" envelope parser (Add/Update/Delete/Move). Use `similar` for **display diffs only**, never to apply. | reference-restore |
| 28 | Snapshot must respect gitignore, skip files > 2 MB, and seed `info/exclude` from the source repo (else it sweeps `node_modules`/`target`/`.env`). | reference-restore |
| 29 | Lower the Mid-Conversation System Message to a `{role:"system"}` `messages[]` entry (2026 Anthropic beta) with a wrapped-text fallback; keep the immutable baseline + cache breakpoint in the top-level `system` block. | enhancement |

### P2 — polish / scoped-later

| # | Change | Kind |
|---|---|---|
| 30 | Standalone **owned** catalog service (remove `model_info()` from the provider trait — a borrowed return couples catalog to provider lifetime); `get/all/available` returning owned `ModelInfo`; config precedence with `experimental.policies`. | reference-restore |
| 31 | Managed tool output → **global data dir** (not `.private_code/` in-workspace), flat `tool_<id>`, 7-day retention, head/tail preview; fix the §6 prose (50 lines/4 KB) to match config (2000/51200). Use `notify-debouncer-full` (handles rename From/To) instead of a hand-rolled debounce. | reference-restore |
| 32 | Mark **Pro-tier** features (tantivy cross-repo, comparison/merge UI, checkpoint-history UI, team sync) and the open-core boundary explicitly; v1 ships FTS5+nucleo with headless fan-out free. Add custom-command `$ARGUMENTS` substitution. Update `PROJECT_END_GOAL.MD`'s "Forge" codename → "Private Code". | enhancement |
| 33 | **Lazy-init discipline** for <100 ms / <30 MB: eager = config + DB/migrations(WAL) + socket + token; lazy = catalog, keyring, code-intel index (background, kept in SQLite, **not** preloaded into a resident nucleo map), grammars per-language, LSP/MCP on first need. Bridge rayon↔tokio via `spawn_blocking`. Server-side delta coalescing (~8–16 ms). | enhancement |
| 34 | **CI methodology**: multi-OS matrix (Linux/macOS/Windows) via `cargo-nextest`; cold start via `hyperfine` against a `--selftest-ready` entrypoint (not `--version`); separate RSS from heap; run `criterion` off-gate on a dedicated runner with `critcmp` (a bare `cargo bench` gate is non-blocking-by-accident); add an FPS instrument + WS latency probe. | enhancement |
| 35 | **Forward-only, checksummed migrations** (`_sqlx_migrations`, fail-fast on checksum mismatch); FTS5 `symbols` objects as a **later** Phase-4 migration, not `0001`. Give `symbols` an `INTEGER PRIMARY KEY` alias (TEXT PK → non-alias rowid → VACUUM-renumber → silent FTS desync). | enhancement |

---

## Resolved Open Decisions (North Star §15)

All nine are now closed. Defaults kept unless a correctness reason forced otherwise.

| # | Decision | Resolution | Rationale |
|---|---|---|---|
| 1 | **Name** | **Private Code** (formerly codenamed *Forge*). | Already adopted in README/plan; update `PROJECT_END_GOAL.MD`. |
| 2 | **GUI framework** | **Tauri 2** (system webview). Revisit GPUI only if rendering becomes the differentiator (post-v1). | North Star default; matches skills, ships faster, reference frontend is web-based. |
| 2b| **Desktop topology** | **In-process engine over Tauri Channels** is the default (Rust shell links `core`); **loopback-daemon attach** is a first-class mode for headless/remote/multi-device. | Correctness: eliminates the secure-context `ws://127.0.0.1` mixed-content trap and the ~180 MB daemon double-count; attach mode preserves the stateful-daemon guarantee. |
| 3 | **Frontend framework** | **Solid.js** for hot paths. | North Star default; the entire reference frontend is Solid, so message-part/diff/virtua/SSE-coalescing patterns port directly. |
| 4 | **SQLite access** | **sqlx + WAL.** Persist on message/tool-result completion (not per delta); single dedicated writer task. | North Star default; the only real risk (per-delta fsync storms) is a cadence fix, not a driver swap. |
| 5 | **Git layer** | **gix for read-only ops; git2 for the checkpoint write engine** (write-tree/read-tree/force-checkout). | Correctness: gix's high-level write/checkout APIs are still unimplemented per gitoxide crate-status, and checkpointing is the highest-risk subsystem. This *executes* §15.5's own fallback clause. *(Re-verify gitoxide status at build time.)* |
| 6 | **Index engine** | v1 ships **FTS5 + nucleo**; cut over to **tantivy** only for cross-repo/Pro indexing, >~1 M symbols, or when p99 search exceeds the 50 ms budget. | North Star default with an explicit cutover threshold. |
| 7 | **License** | **`MIT OR Apache-2.0`** dual-license for the open core (Rust-ecosystem convention; Apache adds a patent grant, MIT maximizes compatibility); Pro tier outside the permissive grant. *If a single license is preferred, default to Apache-2.0.* | No correctness driver; the dual default is lowest-friction and keeps the open-core boundary clean. **Needs lead ratification.** |
| 8 | **Transport** | **Keep SSE + WebSocket.** Server→client is a typed event stream (durable-cursor replay); client→server commands are request/response over WS; SSE is the curl/simple-client fallback. (Moot on the in-process GUI path — Tauri Channels.) | North Star default; the real defect was the *framing* (per-token JSON-RPC, 1000-event buffer), now fixed — not the transport. |
| 9 | **Provider registry** | **Hybrid:** vendor a models.dev-style snapshot for offline/cold-start + optionally fetch live from a configurable URL + cache. Resolve IDs from the catalog at runtime. | Snapshot guarantees the local-first/offline promise; live-fetch keeps pricing/capabilities current; directly fixes the stale-ID problem. |

---

## Corrected Technical Facts (do not carry the old claims into the rewrite)

| Old claim (plan) | Correction (verified 2026-06-05) |
|---|---|
| Hardcoded `claude-sonnet-4-20250514`, `gpt-4o`, `gemini-2.5-pro`, "Claude 3.5 Sonnet". | All stale. Current: `claude-opus-4-8` (1M, $5/$25 per Mtok), `claude-sonnet-4-6` (1M, $3/$15), `claude-haiku-4-5` (200K, $1/$5). **Never hardcode** — resolve from the catalog. |
| `count_tokens(model_id, text: &str)` async; tiktoken implied. | 2026 Anthropic token counting is `POST /v1/messages/count_tokens` with the **full structured body**, explicitly **not** tiktoken (GPT BPE, wrong for Claude). Use a sync **local estimator** for the per-turn gate; the endpoint is an optional pre-flight. A bare `&str` can't represent the request. |
| `ProviderConfig.temperature` for all providers. | The 2026 Anthropic API replaced `budget_tokens`/`temperature` with an **`effort`** param (adaptive thinking) on Opus 4.7/4.8; sending `temperature` is rejected. Add `reasoning_effort`; gate `temperature` to OpenAI/Gemini. |
| Mid-conv updates as inline `"System Update:"` text. | 2026 Anthropic supports a mid-conversation **`{role:"system"}`** `messages[]` entry (beta). Lower to that (wrapped-text fallback); keep the immutable baseline + cache breakpoint in top-level `system`. |
| extism `Context::new()`, `Function::new(...)`, `Plugin::new(&ctx, …)`. | Verified against extism **1.30.0**: no `Context` type. Current API = `Manifest` + `PluginBuilder` + `host_fn!`. (State as direction, not a pinned version.) |
| MCP connects "via stdio or SSE". | MCP deprecated HTTP+SSE for **Streamable HTTP**; rmcp transports are stdio child-process + StreamableHttp. Drop SSE from the MCP **client**. (The daemon's *own* SSE endpoint is unrelated and fine.) |
| tree-sitter `set_language(owned Language)`. | Current tree-sitter (0.26.x line) takes a **borrowed** `Language`; grammar crates export a `LANGUAGE` constant. `parser.set_language(&tree_sitter_rust::LANGUAGE.into())`. Match grammar MAJOR to runtime ABI. |
| Reference uses `@tanstack/solid-virtual`. | It uses **virtua 0.49.1** (`virtua/solid`) in `message-timeline.tsx`. The plan's `useVirtualizer` import is the React name — a compile bug in the Solid adapter (`createVirtualizer`). |
| Reference gates via a permission class; "general" is search-only. | The reference has **no** permission class (empty grep); gating is purely the rule engine. Reference `general` denies only `todowrite` (keeps bash); the search-only agent is **`explore`**. |

---

## New Spec Files (kept deliberately lean — two only)

- [`specs/checkpointing.md`](specs/checkpointing.md) — the shadow git-dir / `write-tree` snapshot engine. The plan's `git reset --hard HEAD` design is both destructive and a no-op, so this is the single most build-blocking gap.
- [`specs/security.md`](specs/security.md) — the threat model the plan lacks: trust boundaries, web_fetch SSRF, child-env scrubbing, daemon auth, closed-by-default WASM sandbox, supply chain.

Providers, tools, and testing were **deliberately not** split into new files — they're folded into enriched `plan.md` sections to keep the plan reviewable.

---

## Open Risks (carry into implementation)

1. **In-process GUI lifecycle.** The in-process default means closing a standalone GUI window mid-turn could kill in-flight work, relaxing Topology Decision 1 ("daemon continues to completion"). Mitigation to design before Phase 3: keep the embedded engine on a detached task until the turn completes, or auto-promote long autonomous runs to the loopback-daemon mode.
2. **License ratification.** Dual `MIT OR Apache-2.0` is recommended but not yet ratified; the open-core/Pro licensing gate (offline key, never proxy calls) must be designed before any Pro feature ships.
3. **Crate/API drift.** extism (1.30), rmcp transports, tree-sitter 0.26 ABI, `notify-debouncer-full`, gix write-path status, virtua are moving targets — re-run a verify-in-research pass at implementation time rather than pinning versions from this review.
4. **seq race boundary.** The per-session serialize + atomic-counter fix is correct for a single-node daemon; do **not** parallelize/cluster the daemon without first adopting the reference `event_sequence` owner fence.
5. **Fan-out cost governance.** Even with a pre-dispatch budget guard, fan-out across N Opus-class models is (N+1)× spend; the per-turn ceiling default and the UX when it trips (pause/prompt vs cap candidates) need a product call against §7.12 ("no surprises on the bill").
6. **FTS5 rowid stability.** `symbols.id` is TEXT PK → non-alias rowid → VACUUM can renumber it → silent FTS desync. Needs an `INTEGER PRIMARY KEY` alias or a rebuild-after-VACUUM policy before code-intel ships.

---

*This review is the decision record. The corrected designs are applied in `plan.md` and `specs/`. Where this file and an older section disagree, this file wins.*
