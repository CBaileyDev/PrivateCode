# Project North Star

> **Name:** **Private Code** (the former *Forge* codename is retired). See [`REVIEW.md`](REVIEW.md).
> **One-line pitch:** A native, instant, local-first AI coding agent — terminal *and* GUI — that beats opencode on startup, memory, code-intelligence speed, and long-session smoothness, with first-class multi-model orchestration.
> **Status:** Vision / spec · **Owner:** Carter (CBaileyDev) · **Stack thesis:** Rust core + Tauri 2 GUI + ratatui TUI over one shared engine.

This document is the single source of truth for *what we are building and why*. It is written to double as a contract that coding agents (Claude Code, Codex CLI, etc.) can execute against — every major decision is stated explicitly so there's nothing to guess.

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Vision & North Star](#2-vision--north-star)
3. [Guiding Principles](#3-guiding-principles)
4. [Performance Doctrine](#4-performance-doctrine)
5. [Tech Stack](#5-tech-stack)
6. [Architecture](#6-architecture)
7. [Feature Set](#7-feature-set)
8. [Keep vs. Build (vs. opencode)](#8-keep-vs-build-vs-opencode)
9. [Design & UX](#9-design--ux)
10. [Differentiators / Moat](#10-differentiators--moat)
11. [Security & Privacy](#11-security--privacy)
12. [Distribution & Packaging](#12-distribution--packaging)
13. [Roadmap](#13-roadmap)
14. [Monetization](#14-monetization)
15. [Open Decisions](#15-open-decisions)

---

## 1. Executive Summary

We are rebuilding the opencode concept — an open, provider-agnostic AI coding agent — with a **Rust engine** instead of a TypeScript/Bun runtime. The product ships as a **single static binary** that powers three frontends over one shared core: a **terminal TUI**, a **Tauri 2 desktop GUI**, and a **headless daemon** for automation/editor integration.

The bet: the LLM round-trip is network-bound and identical for everyone, so we don't compete on "faster AI." We compete on everything *around* the model — instant startup, tiny memory footprint, native-speed code search/indexing over huge repos, a GUI that stays buttery across thousand-message sessions, and best-in-class **multi-model orchestration**. These are exactly the axes where a native rewrite wins and where we have an unfair advantage (prior work on symbol indexing in Codex Atlas, capability routing in Aegis, and a daily multi-AI ensemble workflow).

| | Decision |
|---|---|
| **Core language** | Rust |
| **GUI** | Tauri 2 (system webview) + Solid/Svelte frontend |
| **TUI** | ratatui + crossterm |
| **Architecture** | Client–server; stateful daemon owns all state |
| **Persistence** | SQLite |
| **Provider model** | Trait-based, metadata-driven (models.dev-style), BYOK, local-first |
| **Distribution** | Single static binary + native installers |
| **License** | Open-core (MIT/Apache core; paid Pro tier) — *to confirm* |

---

## 2. Vision & North Star

**Problem.** Existing agents force a tradeoff: terminal tools are fast and keyboard-native but ugly/limited; GUI tools are pretty but heavy (Electron-class) and choke on long sessions. Most treat code as flat text and shell out for search. None do real multi-model orchestration well.

**Thesis.** A Rust engine with a stateful daemon can serve a gorgeous, *fast* GUI **and** a keyboard-first TUI from the same brain — with native code intelligence and multi-model orchestration as the moat.

**Target users.**
- **Primary:** power developers who live in the terminal but want an optional polished GUI, and who run multiple models (cloud + local).
- **Secondary:** developers on lower-spec machines who can't stomach Electron-class memory use.
- **Tertiary:** teams/automation pipelines using the headless daemon.

**Non-goals (v1).**
- Not a full IDE. We integrate with editors; we don't replace them.
- Not a hosted/cloud SaaS. Local-first; the server runs on *your* machine.
- Not trying to out-train or proxy models. We're provider-agnostic glue + tooling.
- No telemetry-driven product. Privacy is a feature.

**Win condition.** A developer installs one binary, runs it in any repo, and within seconds has a session that feels *native* in both terminal and GUI, searches a 200k-symbol codebase instantly, and can fan a task out to multiple models and synthesize the result — all using their own keys or local models.

---

## 3. Guiding Principles

1. **Performance is correctness.** A stutter or a slow index is a bug, not a nice-to-have. (See §4 for budgets.)
2. **One engine, many faces.** All logic lives in the core crate. TUI, GUI, and daemon are thin shells. Never fork behavior into a frontend.
3. **Stateful server, dumb clients.** The daemon owns sessions, history, and context so any client can attach/detach/reconnect — including from another device.
4. **Local-first & private.** Works fully offline with local models. No data leaves the machine unless the user picks a cloud provider. Zero mandatory telemetry.
5. **Keyboard-first, mouse-optional.** Everything reachable without leaving the keyboard, in *both* TUI and GUI.
6. **Interop over reinvention.** Adopt existing conventions (MCP, LSP, `AGENTS.md`, markdown commands, models.dev metadata) so we plug into the ecosystem for free.
7. **Safety by default.** Permission-gated tools, git-backed checkpoints, readable diffs before apply.
8. **Design is a feature, not a coat of paint.** The UI is opinionated, calm, and information-dense without being noisy.

---

## 4. Performance Doctrine

What "performance" actually means here, stated so we never chase the wrong metric.

**We optimize:**
- Cold start, idle memory, time-to-first-keystroke.
- Local CPU work: repo walking, indexing, fuzzy/structural search, diffing, parsing, patch application.
- UI responsiveness under long sessions (streaming render, no main-thread jank).
- Concurrency: streaming tokens + running tools + watching files without pauses.

**We do *not* pretend to optimize:**
- LLM latency — network-bound, identical across languages. We *hide* it well (streaming, optimistic UI, parallelism) but never claim to make the model "faster."

**Performance budgets (targets, v1):**

| Metric | Target |
|---|---|
| Cold start (binary → ready) | < 100 ms |
| Idle memory (daemon, no session) | < 30 MB |
| Idle memory (GUI window open) | < 150 MB |
| Fuzzy search over 200k symbols | < 50 ms to first results |
| Incremental re-index on file save | < 100 ms |
| GUI frame budget under token stream | 60 fps (16.6 ms/frame), no dropped frames |
| Apply + diff render for a 500-line patch | < 100 ms |

These are aspirational guardrails; instrument and track them from Phase 1.

---

## 5. Tech Stack

### Core engine (Rust)

| Concern | Choice | Notes |
|---|---|---|
| Async runtime | `tokio` | Multi-threaded scheduler |
| HTTP client + streaming | `reqwest` + `eventsource-stream` / `reqwest-eventsource` | SSE token streams |
| Server (daemon) | `axum` (hyper/tower) | REST + SSE/WebSocket |
| Realtime channel | `tokio-tungstenite` / axum WS | Bidirectional agent events |
| Persistence | `sqlx` (SQLite) | Async, compile-time-checked queries, migrations. `rusqlite` is a simpler sync alt. |
| File walking | `ignore` | ripgrep's walker, respects `.gitignore` |
| Fuzzy matching | `nucleo` | Helix's matcher; extremely fast |
| Symbol/full-text index | SQLite **FTS5** (simple) → `tantivy` (advanced) | Start with FTS5; graduate to tantivy if needed |
| Code parsing | `tree-sitter` + grammars | Per-language symbol extraction |
| Diffing | `imara-diff` / `similar` | Fast, Myers/Histogram |
| File watching | `notify` | Incremental re-index triggers |
| Git ops | `gix` (gitoxide) | Pure-Rust, fast. `git2` (libgit2) is the mature fallback. |
| Provider layer | custom trait + `genai` as reference | Metadata-driven, see §7 |
| LSP client | `lsp-types` + `async-lsp` | JSON-RPC over stdio to language servers |
| MCP client | `rmcp` (official Rust MCP SDK) | Tool/resource discovery |
| Plugins | `extism` (WASM, wasmtime under the hood) | Sandboxed, polyglot plugins |
| Serialization | `serde` + `serde_json` / `toml` | Config + protocol |
| Config schema | `schemars` | Generate JSON Schema for editor autocomplete |
| CLI | `clap` | Args/subcommands |
| Errors | `thiserror` (lib) + `color-eyre` (app) | |
| Tracing/logging | `tracing` + `tracing-subscriber` | |

### Terminal UI

`ratatui` + `crossterm` — Elm-ish model/update/view, keyboard-driven, no web wrapper.

### Desktop GUI

- **Shell:** **Tauri 2** — Rust backend (the core engine) + system webview. ~10× lighter than Electron; full design freedom.
- **Frontend framework:** **Solid.js** (preferred) or **Svelte 5** for the hot paths — lighter reconciliation than React. React is acceptable if velocity matters more, with strict virtualization/memoization discipline.
- **Styling:** Tailwind CSS + a small token system (see §9).
- **Virtualization:** `@tanstack/*-virtual` — never render an unbounded message list.
- **Syntax highlighting:** Shiki (accurate) run in a **web worker**, or a tree-sitter-wasm highlighter; precompute spans, stream into the DOM.
- **Markdown:** streaming-friendly incremental renderer (append on token, don't re-parse the whole buffer).

> **Alternative GUI path (v2 consideration):** a fully GPU-accelerated native UI via **GPUI** (the framework Zed uses). This is the absolute ceiling for responsiveness and the most impressive technically, but it's a steeper build with a smaller ecosystem and slower to a polished look. Recommendation: ship Tauri 2 first; revisit GPUI only if the rendering layer becomes the differentiator.

### Build / tooling

Cargo workspace · `cargo` for the engine/TUI · `pnpm` + Vite for the GUI frontend · `cargo-dist` or Tauri's bundler for releases · `cargo-nextest` for tests · `criterion` for benchmarks (enforce the §4 budgets).

---

## 6. Architecture

### Topology

```
                ┌──────────────────────────────────────────────┐
                │                Frontends                      │
                │   TUI (ratatui)   GUI (Tauri 2)   Editor ext  │
                └───────┬──────────────┬──────────────┬─────────┘
                        │  REST + SSE / WebSocket (local)        
                ┌───────▼──────────────▼──────────────▼─────────┐
                │              Headless Daemon (axum)            │
                │   router · auth · event stream · session API   │
                └───────────────────────┬────────────────────────┘
                ┌───────────────────────▼────────────────────────┐
                │                 Core Engine (crate)             │
                │  agent loop · orchestrator · tool registry      │
                │  provider layer · context engine · permissions  │
                │  code-intel index · LSP client · MCP client     │
                │  session/state (SQLite) · checkpoints (git)     │
                └─────────────────────────────────────────────────┘
```

**Why this shape:** keep opencode's single best decision — a stateful server with thin clients — so sessions survive restarts and you can attach from anywhere. The mistake would be welding the agent to one UI.

### Cargo workspace layout

```
forge/
├── crates/
│   ├── core/            # the engine: agent loop, orchestrator, context, sessions
│   ├── providers/       # provider trait + impls (anthropic, openai, google, ollama, lmstudio…)
│   ├── tools/           # file ops, bash, search, patch — each a Tool impl
│   ├── codeintel/       # tree-sitter parsing, indexing (FTS5/tantivy), repo map, search
│   ├── lsp/             # LSP client wrapper
│   ├── mcp/             # MCP client wrapper
│   ├── protocol/        # shared request/event types (serde) used by daemon + clients
│   ├── daemon/          # axum server exposing protocol over REST + SSE/WS
│   ├── tui/             # ratatui client
│   └── cli/             # `forge` entrypoint: serve | run | tui | ...
├── apps/
│   └── desktop/         # Tauri 2 app (Rust shell links `core`; web frontend in src/)
└── Cargo.toml           # workspace
```

### Component responsibilities

- **core** — owns the turn loop: build context → call provider (stream) → parse tool calls → gate via permissions → execute tools → feed results back → repeat until done. Emits a typed event stream.
- **orchestrator** (inside core) — single-model by default; can fan a task to N models and run a synthesis/critique pass (see §10).
- **providers** — a `Provider` trait (`stream_chat`, `count_tokens`, capabilities). Models and pricing come from a metadata registry (models.dev-style), not hardcoded.
- **tools** — a `Tool` trait (`name`, `schema`, `run`, `permission_class`). Read, write, edit, patch, bash, glob, grep/search, web-fetch, etc.
- **codeintel** — walk (respecting gitignore) → parse with tree-sitter → extract symbols → index → serve fuzzy + structural queries; re-index incrementally on file events.
- **protocol** — the wire types; the only thing frontends depend on besides transport.
- **daemon** — auth (local token), session CRUD, event SSE/WS, attach/detach.

### Turn data flow

1. Client sends a user message to the daemon → forwarded to core.
2. Context engine assembles the prompt (history + `AGENTS.md` + retrieved code-intel + open files).
3. Provider streams tokens; daemon relays them to all attached clients via SSE/WS.
4. Tool calls are parsed, checked against the permission policy (auto-allow / prompt / deny).
5. Approved tools run; results stream back into the loop.
6. File-mutating tools snapshot a git checkpoint first; diffs are rendered before/after apply.
7. Session + messages persisted to SQLite continuously.

### Event protocol (sketch)

A single typed event stream so every client renders identically: `session.created`, `message.delta`, `message.completed`, `tool.requested`, `tool.permission_required`, `tool.output`, `checkpoint.created`, `usage.updated`, `error`.

---

## 7. Feature Set

### 7.1 Agent system
- **Built-in agents** switchable with one key:
  - **build** — full-access agent for development.
  - **plan** — read-only: denies edits, prompts before bash; for exploring/planning.
- **Subagents** — a `general` subagent for multi-step searches; invokable inline (`@general`).
- **Custom agents** — user-defined agents (model + tools + system prompt) declared in config.
- **Capability routing** — route subtasks to the cheapest/fastest model that can handle them (carryover from Aegis).

### 7.2 Multi-model orchestration  ⭐ *differentiator*
- **Fan-out:** send one task to multiple models in parallel.
- **Synthesis pass:** a designated model merges/critiques the candidates into one answer.
- **Roles:** assign models to roles (architect / implementer / reviewer) per task.
- **Comparison view:** side-by-side outputs in the GUI; pick or merge.
- This is the feature almost no competitor does well, and the one you understand at depth.

### 7.3 Tools
File `read` / `write` / `edit` / `patch`, `bash` (permission-gated), `glob`, `grep`/structural search, `web-fetch`, plus any tools surfaced via MCP. Each tool declares a schema and a permission class.

### 7.4 Providers & models
- Provider-agnostic via a trait + **metadata registry** (models.dev-style) — add providers without code changes where possible.
- **BYOK** — bring your own keys; nothing proxied.
- **Local-first** — Ollama and LM Studio are first-class providers, not afterthoughts (you run local models on the M5 and the 9950X3D box — make that path excellent).
- **Model routing** — default "smart" model + fast model for cheap ops; switch with `/model`.
- Prompt caching support where the provider offers it.

### 7.5 Code intelligence  ⭐ *differentiator*
- tree-sitter parsing → symbol extraction → fast index (FTS5 → tantivy).
- **Instant fuzzy + structural search** over very large symbol sets (the Codex Atlas capability, brought in-engine).
- **Repo map** — a compact structural overview fed into context so the model isn't guessing.
- Incremental re-index on save via `notify`.

### 7.6 LSP integration
Connect to language servers (rust-analyzer, typescript-language-server, pyright, etc.) so the agent gets *real* diagnostics, types, and references — grounding edits in compiler truth, not vibes.

### 7.7 MCP support
Add MCP servers in config; their tools/resources auto-register and become available to the agent.

### 7.8 Sessions & state
Persistent sessions in SQLite. Resume after restart. Attach/detach. Multi-device reconnect (server holds the state). Multiple parallel sessions, each with its own context.

### 7.9 Checkpointing & undo  ⭐ *trust*
Git-backed snapshot before each agent edit; one-keystroke rollback to any checkpoint. Makes letting the agent run feel safe.

### 7.10 Permissions & safety
Policy engine: per-tool-class rules (auto-allow / prompt / deny). `plan` agent is read-only by default. Bash prompts unless whitelisted. All destructive ops show a diff/preview first.

### 7.11 Context engine
`/init` analyzes the repo and generates an `AGENTS.md` (persistent project context). Markdown **custom commands** (per-user / per-project) with named argument placeholders. Smart retrieval pulls relevant code-intel into the prompt automatically.

### 7.12 Cost & usage transparency
Live token + cost counter per turn and per session, surfaced in the UI. Per-model breakdown. No surprises on the bill.

### 7.13 Plugins
WASM-sandboxed plugin system via `extism` with lifecycle hooks (pre/post turn, tool wrap, event taps). Polyglot (any language that compiles to WASM) and safe — no embedding a JS runtime just for plugins.

### Feature priority matrix

| Feature | Priority | Differentiator? |
|---|---|---|
| Agent loop + core tools | P0 | — |
| Provider layer + BYOK + local | P0 | — |
| Sessions/state (SQLite) | P0 | — |
| TUI | P0 | — |
| Daemon split | P0 | — |
| Permissions + checkpoints | P1 | trust |
| GUI (Tauri 2) | P1 | — |
| Code intelligence index | P1 | ⭐ |
| Multi-model orchestration | P1 | ⭐ |
| LSP | P2 | — |
| MCP | P2 | — |
| Plugins (WASM) | P2 | — |
| Cost transparency | P2 | — |

---

## 8. Keep vs. Build (vs. opencode)

| Keep / adopt | Build new / better |
|---|---|
| Client–server, stateful daemon | Rust engine (vs Bun/TS) → instant start, low memory |
| HTTP + SSE transport (add WS) | Single static binary distribution |
| Provider abstraction via models.dev metadata | Native code-intelligence index (instant search on huge repos) |
| LSP integration | First-class multi-model orchestration |
| MCP support | Long-session GUI that never janks (virtualized/streamed) |
| `AGENTS.md` + markdown commands | Git-backed checkpointing/undo |
| Plugin concept | Plugins as **WASM** (sandboxed, polyglot) vs JS hooks |
| `build` / `plan` agent model | Local-model-first provider path |

Standing on the shared conventions buys ecosystem interop for free; the right-hand column is where we actually win.

---

## 9. Design & UX

### Philosophy
Calm, dense, fast. Information-rich without noise. The UI gets out of the way — no spinners-as-theater, no layout shift, no decorative motion that costs frames. It should feel like a precision tool, not a chat toy.

### Layout (shared mental model across TUI + GUI)
- **Left:** session list / file tree / context.
- **Center:** conversation (virtualized), with inline tool calls, diffs, and code blocks.
- **Right (GUI) / toggle (TUI):** detail panel — diff viewer, model comparison, usage/cost.
- **Bottom:** input with model selector, agent selector, and command palette entry.
- **Command palette** (`Ctrl/Cmd-K`): everything reachable from here.

### GUI performance techniques (non-negotiable)
- **Virtualize** the message list and any long list.
- **Incremental streaming render** — append nodes as tokens arrive; never re-parse the whole transcript per chunk.
- **Off-main-thread** syntax highlighting and diffing (web workers, or compute Rust-side and stream spans).
- **`requestAnimationFrame` batching** for stream updates; coalesce token bursts.
- **Stable keys + memoization** so React/Solid never re-renders settled messages.

### Interaction model
Keyboard-first everywhere. `Tab` switches agents. `/model` and `/agent` quick-switch. Diffs accept/reject with single keys. Mouse fully supported in GUI but never *required*.

### Theming
Token-based theme system (CSS variables in GUI, palette in TUI) sharing the same named colors. Ship a refined default dark theme + light; support user themes. Respect terminal truecolor.

### Visual identity
Pick a restrained type pairing (one mono for code/UI, one sans for prose), a tight neutral palette with a single accent, and a simple mark. Keep it consistent across TUI ASCII branding, GUI, and site.

---

## 10. Differentiators / Moat

Ranked by how hard they are to copy and how well they map to your existing strengths:

1. **Multi-model orchestration** — fan-out + synthesis + role assignment. You do this daily; almost no shipped agent does it well. Hardest to copy, highest-signal.
2. **Native code intelligence** — instant fuzzy/structural search and a repo map over 200k+ symbols. You've built the core of this (Codex Atlas). A JS clone can't easily match the speed.
3. **Lightweight native everything** — single binary, sub-100ms start, tiny memory. Structural, not a feature flag.
4. **Long-session GUI that stays smooth** — unglamorous engineering most webview apps get wrong.
5. **Local-first** — excellent Ollama/LM Studio path for privacy and offline.
6. **Git-backed checkpointing** — trust that makes autonomous runs comfortable.

---

## 11. Security & Privacy

- **No mandatory telemetry.** Any analytics strictly opt-in and local-summarized.
- **Keys** stored in the OS keychain (Keychain / Credential Manager / libsecret), never plaintext config.
- **Local daemon auth** — loopback-only by default; token-gated; explicit opt-in (and warning) for LAN exposure.
- **Permission engine** gates every side-effecting tool; destructive ops preview first.
- **Plugin sandbox** — WASM with capability-scoped host functions; no ambient filesystem/network.
- **Supply chain** — `cargo audit` / `cargo deny` in CI; pin and review dependencies.

---

## 12. Distribution & Packaging

- **Single static binary** for the engine/TUI/daemon (`forge`), no runtime dependency.
- **Native installers** for the desktop GUI: `.dmg` (mac arm64 + x64), `.exe`/MSI (Windows), `.deb`/`.rpm`/`.AppImage` (Linux).
- **Package managers:** Homebrew, Scoop/Choco, AUR, Nix, an install script.
- **Auto-update** for the GUI (Tauri updater); version-pinned channels (stable/beta).
- **CI** builds all targets on tag; `cargo-dist` for the binary, Tauri bundler for the app.

---

## 13. Roadmap

| Phase | Goal | Exit criteria |
|---|---|---|
| **0 — Spec & scaffold** | Workspace, protocol types, CI, perf harness | `cargo build` workspace; benches stubbed; this doc ratified |
| **1 — Prove the core** | Agent loop + 1 provider (Anthropic) + core tools + SQLite, driven by a minimal TUI | A real coding session feels great in the terminal; budgets §4 instrumented |
| **2 — Daemon split** | Pull core behind the daemon; TUI becomes a client over the protocol | Attach/detach + resume works; multiple clients see the same stream |
| **3 — GUI** | Tauri 2 app with virtualization/streaming from day one | 1000-message session at 60fps; diffs/usage panels working |
| **4 — Differentiators** | Code-intel index → multi-model orchestration → checkpointing | Instant search on a 200k-symbol repo; fan-out+synthesis shipped; one-key undo |
| **5 — Ecosystem** | LSP, MCP, WASM plugins, providers breadth, packaging | Real LSP diagnostics in-loop; MCP tools auto-register; installers on all OSes |

> **Reality check:** opencode is a multi-year, 800+-contributor, ~14k-commit project. A 1:1 clone solo is not the framing. A *better-on-the-axes-that-matter* product is — by going MVP-first and leaning on the moat features rather than matching every feature.

---

## 14. Monetization

Open-core is the natural fit (it's also your stated highest-value play — a cross-platform Tauri AI client).

- **Free / open source (MIT or Apache-2.0):** the engine, TUI, daemon, BYOK, local models, core tools. This drives adoption and trust.
- **Pro (paid):** the polished desktop GUI's advanced features — multi-model orchestration UI, team/session sync, advanced code-intel (tantivy-backed cross-repo), checkpoint history UI, premium themes, priority updates.
- **Sell via** Polar / Gumroad / Lemon Squeezy (one-time license + optional updates, or low monthly).
- **Never** inject usage markups or proxy model calls — BYOK stays clean; that's part of the pitch.
- Keep the free tier genuinely useful so the open project thrives; gate *convenience and polish*, not *capability*.

---

## 15. Open Decisions

> **✅ All nine RESOLVED** in the lead review — see the decision table in [`REVIEW.md`](REVIEW.md). Summary: Name = Private Code · GUI = Tauri 2 (engine in-process over Tauri Channels; loopback-daemon attach as a first-class mode) · Frontend = Solid.js · DB = sqlx + WAL · Git = gix read-only + git2 for the checkpoint write engine · Index = FTS5 + nucleo (tantivy = Pro/cross-repo) · License = `MIT OR Apache-2.0` (needs final ratification) · Transport = SSE + WebSocket · Provider registry = vendored snapshot + optional live fetch. The original list is kept below for context.

These were the open calls before building:

1. **Name.** *Forge* is a placeholder. (Your past names lean mythic — Aegis, Medea. Worth a short list.)
2. **GUI framework final call.** Tauri 2 (recommended, ships faster, matches your skills) vs GPUI (max rendering perf, bigger lift). Default: Tauri 2; revisit at Phase 3.
3. **Frontend framework.** Solid vs Svelte vs React for the GUI. Default: Solid for hot paths.
4. **SQLite access.** `sqlx` (async, checked) vs `rusqlite` (simple, sync). Default: `sqlx`.
5. **Git layer.** `gix` (pure-Rust, fast) vs `git2` (mature). Default: `gix`, fall back to `git2` if a feature is missing.
6. **Index engine.** Start FTS5; decide the tantivy cutover threshold.
7. **License.** MIT vs Apache-2.0 for the open core.
8. **Transport.** SSE-only vs SSE+WebSocket for the realtime channel. Default: add WS for bidirectional events.
9. **Provider registry source.** Vendor a snapshot of models.dev metadata vs fetch live vs maintain our own.

---

*End of document. This is a living spec — update it as decisions land.*
