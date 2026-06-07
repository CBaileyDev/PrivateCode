# Private Code — Release Punch List (path to v1)

> **Purpose:** the single actionable list of everything that must be fixed/finished to reach the **first release build**. Generated from a 34-agent adversarial review of **Phase 5 (Ecosystem & Packaging — the final phase)** plus a re-run of the full gate. Phases 1–4 are complete, reviewed, and green (committed through `03522b7`). **All Phase-5 work is currently UNCOMMITTED in the working tree.**
>
> **How to use (Claude Code):** work top-down — **Blockers → HIGH → MEDIUM → packaging → LOW**. Every item is a checkbox with the exact `file:line`, the problem, and the fix. After each cluster, run the **Gate** at the bottom and only commit when it is truthfully green. Use the project conventions in `AIChatContext.md` (commit trailer, per-cluster gate→commit→push to `main`). Do **not** mark a step "done" in `PROGRESS.md` unless the gate passes.

## 🔴→🟢 RELEASE AUDIT (2026-06-07, independent re-review)

A fresh release-readiness pass (4 parallel subsystem reviews + a full local gate + a
look at the **remote CI history**) found the project was **not** releasable as claimed:
**CI had never been green — 0 of 30 runs passed** (16 failures, 14 superseded). The
latest `main` run failed on the **`quality (windows-latest)`** job. The blocker and the
most serious bugs are now fixed; the rest is itemized below.

### Fixed in this pass (gate verified green locally; regression-tested)
- **[CI BLOCKER] Windows build broke the whole gate.** `tauri-build` requires
  `apps/desktop/src-tauri/icons/icon.ico` on Windows (and `icon.icns` for macOS
  bundles); both were **missing**, so the desktop crate's build script exited 1, failing
  the Windows `clippy`/`nextest` steps and reddening every CI run. The same gap blocked
  the macOS/Windows desktop bundles in `release.yml` (documented there as a "human
  ceiling"). **Fix:** generated `icon.ico` + `icon.icns` from `icon.png` via
  `tauri icon`; wired both into `tauri.conf.json` `bundle.icon`.
- **[CRITICAL — sandbox escape] Path traversal in `validate_path`**
  (`crates/tools/src/file_tools.rs`). For a not-yet-existing target, the `..` tail was
  re-attached without normalization, so `newdir/../../outside/x.txt` passed the
  `starts_with(workspace)` check and the kernel resolved the `..` at write time to land
  **outside the workspace**. Reachable in the default `build` agent (write/patch tools) →
  arbitrary-file write. **Fix:** lexical `.`/`..` normalization before the boundary
  check, + 2 regression tests (unit + end-to-end write).
- **[HIGH — first-run crash] CLI never creates its SQLite DB.** `db::connect_db` lacked
  `.create_if_missing(true)` and the CLI URL carries no `?mode=rwc`, so a fresh install
  failed with `SQLITE_CANTOPEN` on the first `tui`/`prompt`/`serve`. Not caught because
  every test uses `sqlite::memory:`. **Fix:** `.create_if_missing(true)` in `connect_db`
  (fixes CLI + desktop at the chokepoint), + regression test.
- **[HIGH — secret leak] Gemini API key in the URL.** `google.rs` sent `?key=<secret>`;
  reqwest attaches the full URL to transport errors, which are stringified into the
  user-facing error toast and the tracing log → key leak on any DNS/TLS/timeout error.
  **Fix:** send `x-goog-api-key` header instead (matches Anthropic/OpenAI header auth).
- **[HIGH — undo correctness] Revert restores the wrong checkpoint.**
  `list_checkpoints` ordered by whole-second `created_at` with no tiebreaker; the 3
  checkpoints written per turn share a second, so "revert to latest" landed on an
  arbitrary sibling. **Fix:** `ORDER BY created_at DESC, rowid DESC` (monotonic insertion
  order), + regression test.
- **[MED — security hardening] Daemon auth token compared with `==`.** Non-constant-time;
  switched both check paths to a constant-time compare, + test. (Loopback-only,
  defense-in-depth.)

### Still OPEN — recommended before tagging v1
**HIGH**
- **Daemon startup ordering is only half-fixed.** `start_daemon` now `bind`s before
  bootstrap (so the port opens and the auth test passes), but `Ecosystem::bootstrap` +
  `detect_providers` still run on the path **before `axum::serve`**. A slow/hanging
  `rust-analyzer` `initialize` still blocks request *serving* (now a hang instead of a
  refused connection). Fix: `tokio::spawn` the bootstrap and attach to the coordinator
  when ready, or make `LspManager::new` lazy + timeout-bounded. (`crates/daemon/src/lib.rs`)
- **Desktop `subscribe_session` leaks a backend task + broadcast receiver on every session
  switch / live model change** (`apps/desktop/src-tauri/src/commands.rs` ~515). The bare
  `tokio::spawn` forwarder never stops when the JS `Channel` is dropped → unbounded task
  growth. Fix: track and abort the prior forwarder per session.

**MEDIUM**
- Orchestrator drops `finish_reason` on the single-model loop → UI/telemetry never sees
  `length`/`MAX_TOKENS`/`safety` truncation (`crates/core/src/orchestrator.rs` ~831,949).
- DeepSeek / Groq / NVIDIA report **$0 cost** (no catalog pricing rows)
  (`crates/providers/data/models.json`).
- Desktop `UsagePanel` "Active Model" is hardcoded to `anthropic/claude-opus-4-8`;
  `setModelInfo` is never called (`apps/desktop/src/stores/usage.ts`).
- Desktop `DiffViewer.tsx` is **dead code** and its advertised per-hunk accept/reject does
  not exist; tool diffs render as plain JSON. Wire it in or delete it + drop the claim.
- `private-code update` is a **check-only stub** — no download/replace, **no
  checksum/signature verification**, no request timeout, exits 0 on HTTP failure
  (`cli/src/main.rs` ~565). Implement a verified updater or rename/scope it as deferred.
- `is_daemon_running` treats timeout/TLS/DNS errors as "up" → `ensure_daemon` can hand the
  TUI a dead daemon (`cli/src/main.rs` ~113).

**LOW / hardening**
- `get_config` no-config default diverges from `AppConfig::default()` (hardcoded
  `/bin/zsh`) (`crates/daemon/src/routes.rs` ~351).
- Post-step checkpoint + LSP diagnostics fire even after a tool is cancelled/aborted
  (`crates/core/src/orchestrator.rs` ~1257).
- Bash tool hardcodes `/bin/zsh`, ignores the `shell` config (`crates/tools/src/system_tools.rs` ~74).
- Daemon token accepted via `?token=` query string (log/Referer exposure; loopback-only).
- Narrow Tauri `shell.open: true` to an `^https?://` regex.
- Desktop dead components (`ThemeToggle.tsx`, `ConnectionStatus.tsx`), double message
  fetch on session click, dead usage-store helpers, lingering fan-out panes.

### Still OPEN — release/packaging & process (human / CI)
- **No `LICENSE` file** at the repo root and no `license` field in any `Cargo.toml` —
  required before any public/binary distribution (choose MIT / Apache-2.0 / proprietary).
- **Code signing + notarization** not done (macOS notarization, Windows signing, Tauri
  updater key are human-provided secrets) — desktop bundles ship **unsigned**.
- **Package-manager distribution** (Homebrew / Scoop / AUR / Nix) not authored.
- **Confirm CI is green on the remote** after the icon fix. The Windows job never reached
  `clippy`/`nextest` (it died in the build script), so a re-run is needed to prove the
  rest of the Windows gate is also green.
- **MCP (5.2)** and **Plugins (5.3)** remain deferred/stubbed — keep them honestly marked
  not-done in `PROGRESS.md`.
- Two human-only smoke tests still stand: in-webview live BYOK conversation, and the GUI
  panel walkthrough.

---

## 🖥️ GUI MADE FUNCTIONAL (2026-06-07)

After the punch-list pass, a live GUI test found the desktop app's interactive flows
didn't work (no first-run path). The GUI was overhauled into a working app: native
folder picker → project/session, in-app BYOK Settings (OS keychain), connected-model
picker, visible error toasts, working ⌘K palette; providers resolve keys per-turn. A
6-agent adversarial review's 7 runtime bugs were fixed. Gate: 214 nextest + 57 vitest,
app binary builds. Full detail + how-to-test at the TOP of `PROGRESS.md`. The remaining
human step is the live in-webview model conversation (your key).

---

## ✅ WORKED TO GREEN (2026-06-06) — this punch list has been executed

| Check | At generation | After the punch-list pass |
|---|---|---|
| `cargo fmt --all --check` | ❌ RED | ✅ clean |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | ✅ clean | ✅ clean |
| `cargo nextest run --workspace` | ❌ 181 pass / **1 FAIL** | ✅ **209 pass / 0 fail** (4 skipped) |
| `cargo deny check` | (n/a) | ✅ advisories/bans/licenses/sources ok |
| desktop `typecheck` + `build` + `vitest` | ✅ 39 | ✅ **40** |

**What landed:** §0 blockers fixed · §1 HIGH all 3 fixed + regression-tested + 5-agent adversarial self-review (0 must-fix) · §2 MEDIUM all fixed **except 5.3 plugins (formally DEFERRED)** · §3 packaging wired (human-only signing/CI surfaced) · §4 LOW done (security first; a few items deferred + documented) · §5 docs reconciled. Full detail + the honest ceilings live in `PROGRESS.md` → "Phase 5". Two human-only sign-off steps remain (GUI smoke; live BYOK provider smoke). The individual checkboxes below are the original spec, kept for traceability.

---

## Original generated state (verified 2026-06-06, BEFORE the pass)

| Check | Status |
|---|---|
| `cargo fmt --all --check` | ❌ **RED** (`crates/daemon/src/lib.rs`, `crates/lsp/src/client.rs`) |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | ✅ clean |
| `cargo nextest run --workspace` | ❌ **RED** — 181 passed, **1 FAILED** (`daemon::tests::test_daemon_authentication_and_routes`, deterministic) |
| desktop `npm run typecheck` + `vitest` | ✅ green (39 tests) |
| Phase-5 grade | **C+** (ambitious + sound foundation, but dead flagship feature, stubbed subsystems, RED gate, overstated "green" claim) |

**Severity counts (post adversarial-verification):** 3 HIGH · 5 MEDIUM · 20 LOW.

⚠️ **Honesty note (now corrected):** `PROGRESS.md` claimed "clippy clean, **182** nextest passed." That was false — tests were **181 pass / 1 fail**, and `fmt` (red) was omitted. Corrected in `PROGRESS.md` as part of §5.

---

## 0. BLOCKERS — gate must be green before anything ships

- [ ] **fmt is red.** Run `cargo fmt --all`. Offending files: `crates/daemon/src/lib.rs:328`, `crates/lsp/src/client.rs:82`. Then `cargo fmt --all --check` must exit 0.
- [ ] **Daemon won't serve until LSP servers initialize (real startup-ordering defect).** `crates/daemon/src/lib.rs` `start_daemon` runs `Ecosystem::bootstrap(...)` + `detect_providers().await` **before** `TcpListener::bind`. `LspConfigFile.enabled` defaults to `true` (`crates/core/src/config.rs` `default_true`), so bootstrap calls `LspManager::new` → `discover_servers` → `LspClient::spawn(rust-analyzer).await`, which blocks on the LSP `initialize` handshake. The HTTP listener never binds until language servers finish init (and hangs forever if one does). This is what fails `test_daemon_authentication_and_routes` (connection refused at `crates/daemon/src/lib.rs:263`).
  - **Fix:** bind + start serving FIRST, then bootstrap the ecosystem in a background task (`tokio::spawn`) and attach it to the coordinator when ready. Alternatively make `LspManager::new` non-blocking (spawn + initialize lazily on first use, with a timeout). Re-run the daemon test until green.

---

## 1. HIGH — real correctness breaks (bounded fixes, not rewrites)

- [ ] **LSP post-write diagnostics are 100% dead** — `crates/lsp/src/client.rs:289-312` (`read_message`). The header loop calls `header.clear()` at the top of every iteration and only breaks on the blank `"\r\n"` separator, so the `Content-Length:` line has already been wiped; the parse runs against `"\r\n"`, returns `Err("missing Content-Length")`, and the reader task (`client.rs:79-114`) treats that as fatal and breaks on the **first** frame. `textDocument/publishDiagnostics` is never parsed; `notify_file_written` always returns `""`. The flagship 5.1 feature does nothing. (Reached in production from `orchestrator.rs:~1233` after every mutating tool call.)
  - **Fix:** capture the `Content-Length` value while iterating (parse each non-blank line *before* clearing): accumulate `let mut content_len = None;` in the loop, `strip_prefix("Content-Length:")` on each line, break on blank, then read `content_len` bytes. **Add a unit test** feeding a valid frame.
  - **NOTE:** the identical bug exists in `crates/mcp/src/client.rs:182-204` — fix both.

- [ ] **MCP client never reads responses (entire feature non-functional)** — `crates/mcp/src/client.rs:142-159` (`request`). `tools/list` returns a hardcoded `{"tools":[]}` and every other call returns `Null`. No MCP tools ever register; `call_tool` always yields `Null`.
  - **Fix:** implement request/response pairing — a reader task that routes responses by JSON-RPC `id` to a per-request `oneshot` (an `id → Sender` map), and have `request()` `await` the real response. Also fix the wire framing (see §2 — MCP is newline-delimited JSON, not `Content-Length`).
  - **If not completing this for v1:** explicitly re-scope MCP (5.2) as **deferred, not done** in `PROGRESS.md` rather than carrying it as a completed deliverable.

- [ ] **Gemini tool round-trips are broken** — `crates/providers/src/google.rs:191` (tool-call id hardcoded to `"call"`) and `:108-111` (`functionResponse` keyed by id instead of function name). Parallel tool calls collide; single calls misroute. Breaks the core agent loop for Gemini.
  - **Fix:** assign a unique synthetic id per tool call; key `functionResponse.name` by the real function name. `ContentBlock::ToolResult` currently has no `name` field, so thread the function name through the lowering path (or carry an id→name map in provider state).

---

## 2. MEDIUM — feature gaps / correctness (fix for a credible v1)

- [ ] **Plugin hooks are loaded but never invoked** — `crates/core/src/ecosystem.rs:53,64`, `crates/plugins/src/hooks.rs:91-123`. `pre/post_turn` and `pre/post_tool` are loaded then abandoned; only tests call them. Configured plugins silently do nothing.
  - **Fix:** wire the hook calls into the orchestrator turn/tool loop (`crates/core/src/orchestrator.rs`). **Or** formally defer 5.3 and mark it not-done.
- [ ] **OpenAI-compatible providers report $0 cost** — `crates/providers/src/openai.rs:149-264`. No catalog/`compute_cost` call; core doesn't compensate. Every OpenAI/NVIDIA/DeepSeek/Groq turn shows $0.00 spend.
  - **Fix:** compute cost in-provider via the model catalog (mirror `google.rs`).
- [ ] **Gemini `finish_reason` always `None`** — `crates/providers/src/google.rs:202-213,170`. `finishReason` and `usageMetadata` arrive in separate SSE chunks; finish is only read in the usage chunk. UI/telemetry never sees MAX_TOKENS/SAFETY/length for Gemini.
  - **Fix:** accumulate `finishReason` in stream state and emit it at stream-end; map the raw vocabulary to the internal reasons.
- [ ] **GUI `/init` builds a prompt but never sends it** — `apps/desktop/src/components/InputBar.tsx:200-205`. Calls `addUserMessage` only (no `invoke("send_prompt")`), so it shows a misleading optimistic bubble and no AGENTS.md is generated.
  - **Fix:** call `send_prompt` like `handleSend` does.
- [ ] **`phase5.rs` doesn't test the headline Phase-5 features** — `crates/core/tests/phase5.rs` (48 LOC). Only catalog lookup, plugin no-ops, and diagnostic string formatting; export branches untested beyond a title substring.
  - **Fix:** add tests for export (markdown/JSON/file-write), auth/keyring, and the command wiring. (This also would have caught several bugs above.)

---

## 3. PACKAGING & RELEASE (5.11–5.14 — currently scaffold-only)

These are required to actually ship a v1 binary/app, and were honestly self-marked partial/scaffold.

- [ ] **Auto-update (5.11)** — CLI `private-code update` only checks GitHub releases; the **Tauri updater is not wired**. Wire the Tauri updater plugin (or document update as CLI-only for v1).
- [ ] **CLI packaging (5.12)** — `dist-workspace.toml` / `release.yml` exist as scaffold; needs a real tag-triggered build matrix producing signed CLI binaries. Verify `.github/workflows/release.yml` actually builds on a tag.
- [ ] **Desktop packaging (5.13)** — Tauri bundle (dmg/msi/AppImage) + **code signing / notarization** (macOS notarization, Windows signing) not done. Required for a distributable desktop app.
- [ ] **Package-manager distribution (5.14)** — Homebrew/Scoop/AUR/Nix formulas not authored.
- [ ] **Confirm CI is actually green on the remote.** `ci.yml` + `perf.yml` trigger on push to `main`, but the runs have never been *observed* (`gh` unauthenticated in the dev env). Authenticate `gh` (or check the Actions tab) and confirm the first green run before tagging a release.

---

## 4. LOW — quality, security hardening, robustness (batch before v1 sign-off)

### LSP (`crates/lsp/`)
- [ ] **Child process leak** — `client.rs:53-59`: no `kill_on_drop`, no `shutdown`/`exit`, no `Drop`. Add `.kill_on_drop(true)` and/or a `Drop` that sends `shutdown`+`exit`. (Only leaks one process at daemon exit today — no accumulation path — but fix it.)
- [ ] **`initialized` sent without awaiting `initialize` result** — `client.rs:124-136,225-238` (fixed 50ms sleep, no response pairing). Await the real `initialize` response.
- [ ] **Wrong-server routing on language miss** — `manager.rs:86-101`: falls back to an arbitrary `clients.keys().next()`. Return `Ok(String::new())` when no server matches the language.
- [ ] **Manager mutex held across a 200ms sleep + 3 notifications** — `crates/core/src/ecosystem.rs:67-74`, `manager.rs:104`: serializes post-write diagnostics across sessions. Clone the client out under a short lock, then sleep/notify without the guard. *(Reported twice — same defect.)*
- [ ] **`didOpen` re-sent every write + hardcoded `version: 2`** — `manager.rs:100-101`: violates LSP open-once/monotonic-version. Track open docs + a per-doc version counter.
- [ ] **`path_to_uri` not percent-encoded** — `client.rs:314-322`: diagnostics silently empty for paths with spaces/non-ASCII when the server normalizes the URI. Percent-encode and normalize the diagnostics map keys.
- [ ] **LSP rooted at process CWD, not per-session workspace** — `crates/daemon/src/lib.rs:173-183`, `apps/desktop/src-tauri/src/state.rs`: diagnostics misrooted/empty for sessions outside CWD. Root the LSP per session workspace.

### MCP (`crates/mcp/`)
- [ ] **Wrong wire framing** — `client.rs:170-179,182-204`: uses LSP `Content-Length`; MCP stdio is newline-delimited JSON. Switch to `\n`-delimited both directions. *(Compounds the HIGH stub — fix together.)*
- [ ] **Child process leak** — `client.rs:43-92`: no `kill_on_drop`/`Drop`. Add `.kill_on_drop(true)`.
- [ ] **MCP tools bypass the permission prompt under the `general` agent** — `crates/mcp/src/tool_adapter.rs:48-50`, `crates/core/src/permissions.rs:143` (catch-all Allow, no `mcp` rule). Latent (gated by the empty-tools bug + non-default agent today). Add `r("mcp","*",Ask)` to the default rules.
- [ ] **MCP permission/undo granularity** — `tool_adapter.rs:44-49`: all MCP tools share `action="mcp"`/`resource="*"` (one "always-allow" blankets every MCP tool), and `mutates()` is hardcoded `false` (defeats checkpoints). Use per-tool resource + a real `mutates()`.

### Plugins (`crates/plugins/`)
- [ ] **Extism host functions never registered** — `runtime.rs:72,80-95`: `Plugin::new(&manifest, [], true)` registers no host fns; `host_write_file` doesn't exist; `read_file_host` is dead. Register host fns via `with_function` (and enforce workspace-boundary path checks — see plan 5.3 sandbox requirements).
- [ ] **Sandbox inert in all real builds** — `runtime.rs:53-65`, `Cargo.toml:8-9`: the `extism` feature is never enabled downstream, so `call_hook` is a no-op returning `""` while logging "Loaded plugin". Enable the feature once hooks are wired, or clearly document plugins as a no-op stub for v1.
- [ ] **No WASM memory/timeout limits** — `runtime.rs:71` (`Manifest::new([wasm])`): the plan's 64 MB / time bound is unimplemented. Add `with_memory_max` + `with_timeout`.
- [ ] **WASI enabled (`true`) vs spec's `with_wasi(false)`** — `runtime.rs:72`. Pass `false` (closed-by-default).

### Providers
- [ ] **Gemini emits no terminal `MessageStop` on normal stream-end** — `google.rs:270-290,156-172`: Gemini sends no `[DONE]`, `finalize_gemini` is dead, and there's no stream-end arm — per-turn usage/cost under-reports (zeros) when `usageMetadata` is absent. Add a `None =>` stream-end finalize arm mirroring `openai.rs`.
- [ ] **OpenAI `input_tokens` overlaps `cache_read_tokens`** — `openai.rs:150-155`: cached tokens double-counted if `compute_cost` is ever applied. Subtract cached from input.

### CLI / Desktop
- [ ] **`auth set` echoes the API key to the terminal** — `cli/src/main.rs:528-536`: reads with echo ON (visible on screen + scrollback). Use `rpassword` or disable terminal echo. *(Good: it reads from stdin, not argv, and never prints the key back.)*
- [ ] **`export_session` interpolates an unsanitized `session_id` into a path** — `apps/desktop/src-tauri/src/commands.rs:483-500`. Bounded today (DB-not-found guard + trusted webview), but validate the id as a UUID before joining it into a filesystem path.

---

## 5. DOCS / HONESTY CORRECTIONS

- [ ] **Correct the false green-gate claim** in `PROGRESS.md` (the Phase-5 section, ~line 211): it says "clippy clean, **182** nextest passed" — replace with the truthful status (or, once fixed, the real green numbers). Do not restate "all green" until the gate actually is.
- [ ] **Reconcile the Phase-5 status table** (`PROGRESS.md` ~lines 195-209): items 5.1 (LSP), 5.2 (MCP), 5.3 (plugins) are marked ✅ but are dead/stub/inert at runtime. Mark them accurately (done / partial / deferred) once the HIGH/MEDIUM fixes land.
- [ ] **Update `AIChatContext.md`** Phase-5 status once this punch list is worked (it currently lists Phase 5 as "next — not started").

---

## 6. Definition of Done (v1) + Gate

**A step is only "done" when the full gate is truthfully green and the change is committed + pushed.**

```bash
# Rust gate (run from repo root)
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace          # must be N passed / 0 failed
cargo deny check                        # for dependency-touching changes

# Desktop gate
cd apps/desktop && npm run typecheck && npm run build && npx vitest run
```

**Release readiness checklist:**
- [ ] §0 Blockers cleared — gate green (fmt + clippy + nextest + frontend).
- [ ] §1 all three HIGH fixed, each with a regression test.
- [ ] §2 MEDIUM fixed (or explicitly deferred + documented honestly).
- [ ] §3 packaging produces a signed CLI binary **and** desktop bundle from a tagged build; remote CI confirmed green at least once.
- [ ] §4 LOW batch addressed (security items — MCP permission rule, auth echo, export id validation — prioritized).
- [ ] §5 docs reconciled; no overstated status anywhere.
- [ ] A manual GUI smoke test (the one thing not headless-verifiable): launch the desktop app, run a turn, exercise the comparison/checkpoint panels, confirm the error banner shows on a forced error.
- [ ] A live BYOK smoke test against at least one real provider (Anthropic) end-to-end.

**Commit convention** (every cluster): green gate → commit with trailer `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>` → push to `main` (https://github.com/CBaileyDev/PrivateCode).

---

*Generated from the Phase-5 adversarial review (34 agents, 28 findings verified). Phases 1–4 are complete and green; this list is the remaining gap to the first release build. Two HIGH items (LSP `read_message`, MCP `request`) and the daemon startup ordering are the highest-leverage fixes — they convert "scaffold" into "working features."*
