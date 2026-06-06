# Private Code Master Development Plan

This document serves as the exhaustive, production-grade master specification and development plan for **Private Code**. It maps out the entire implementation lifecycle from repository scaffolding to cross-platform binary release, documenting all data models, trait definitions, database schemas, execution loops, and rendering pipelines.

> **⚑ Read [`REVIEW.md`](REVIEW.md) first.** A senior lead pass (reconciled against the OpenCode reference and the live 2026 toolchain) **approved this plan with changes** and resolved all 9 North Star open decisions. `REVIEW.md` is the authoritative change record; this plan and the specs have been updated to apply every P0/P1 fix. Where this document and `REVIEW.md` disagree, `REVIEW.md` wins.
>
> **Corrected designs now live in dedicated specs** — read these alongside the relevant sections:
> - [`specs/checkpointing.md`](specs/checkpointing.md) — the safe shadow-git-dir snapshot engine (replaces the old `git reset --hard HEAD` design, which was both destructive and a no-op).
> - [`specs/security.md`](specs/security.md) — the threat model (prompt injection, SSRF, secret handling, daemon auth, WASM sandbox).
> - [`specs/database.md`](specs/database.md) — three-op context-epoch model, race-free sequence allocation, split migrations.
> - [`specs/context_engine.md`](specs/context_engine.md) — codec/`Unavailable` ContextSource model, corrected reconcile enum.
> - [`specs/api_protocol.md`](specs/api_protocol.md) — durable/ephemeral event split, durable-cursor replay, authenticated WS/SSE.
>
> **Resolved stack decisions (North Star §15):** Name = **Private Code**; GUI = **Tauri 2** with the engine **in-process over Tauri Channels** (loopback-daemon attach as a first-class mode); Frontend = **Solid.js**; DB = **sqlx + WAL**; Git = **gix read-only + git2 for the checkpoint write engine**; Index = **FTS5 + nucleo** (tantivy is Pro/cross-repo); License = **`MIT OR Apache-2.0`**; Transport = **SSE + WebSocket**; Provider registry = **vendored snapshot + optional live fetch**. **Never hardcode model IDs** — resolve from the catalog.

---

## Table of Contents
1. [System Topology & Architecture](#1-system-topology--architecture)
2. [Crate & Directory Layout](#2-crate--directory-layout)
3. [Core Traits & API Interface Design](#3-core-traits--api-interface-design)
4. [SQLite Persistence Schema](#4-sqlite-persistence-schema)
5. [System Context & Epoch Lifecycle Engine](#5-system-context--epoch-lifecycle-engine)
6. [Tool Gating, Truncation & Checkpoints](#6-tool-gating-truncation--checkpoints)
    *   [6A. Permission Engine](#6a-permission-engine)
    *   [6B. Configuration System](#6b-configuration-system)
    *   [6C. Agent System](#6c-agent-system)
    *   [6D. Error Handling & Retry Strategy](#6d-error-handling--retry-strategy)
7. [Comprehensive Step-by-Step Roadmap](#7-comprehensive-step-by-step-roadmap)
    *   [Phase 0: Spec & Scaffold](#phase-0-spec--scaffold) (4 steps)
    *   [Phase 1: Prove the Core](#phase-1-prove-the-core) (12 steps)
    *   [Phase 2: Daemon Split](#phase-2-daemon-split) (9 steps)
    *   [Phase 3: GUI Development](#phase-3-gui-development) (15 steps)
    *   [Phase 4: Moat & Differentiators](#phase-4-moat--differentiators) (14 steps)
    *   [Phase 5: Ecosystem & Packaging](#phase-5-ecosystem--packaging) (15 steps)
8. [Multi-Model Orchestration Engine](#8-multi-model-orchestration-engine)
9. [Code Intelligence & Indexing System](#9-code-intelligence--indexing-system)
10. [Tauri 2 / Solid.js UI Rendering Pipelines](#10-tauri-2--solidjs-ui-rendering-pipelines)
11. [WASM Plugin Sandbox Specification](#11-wasm-plugin-sandbox-specification)
12. [Verification, Benchmarking & CI/CD Pipeline](#12-verification-benchmarking--cicd-pipeline)
13. [Open-Core / Pro Boundary](#13-open-core--pro-boundary)

---

## 1. System Topology & Architecture

Private Code is designed as a client-server local-first AI coding agent. It decouples the heavy lifters (model streaming, git operations, database queries, file writing, symbol indexing) from the visualization layers (TUI, GUI, editor integrations) via a local daemon.

```
+------------------------------------------------------------+
|                         CLIENTS                            |
|  +------------------+  +----------------+  +------------+  |
|  | Terminal TUI     |  | Tauri 2 GUI    |  | Editor Ext |  |
|  | (ratatui/crosstrm)|  | (Solid.js Web) |  | (VS Code)  |  |
|  +--------+---------+  +-------+--------+  +-----+------+  |
+-----------|--------------------|-----------------|---------+
            |                    |                 |          
            +----------+         |         +-------+          
                       |         |         |                  
                       v         v         v                  
+------------------------------------------------------------+
|                  LOCAL DAEMON (axum server)                |
|  - Unix Domain Sockets / Local Loopback TCP                |
|  - JSON-RPC over WebSocket (bidirectional agent stream)    |
|  - SSE / REST Endpoints for session status & exports       |
+----------------------------+-------------------------------+
                             |                                
                             v                                
+------------------------------------------------------------+
|                     CORE RUST ENGINE                       |
|  +------------------------------------------------------+  |
|  | Agent Turn Loop Coordinator                          |  |
|  | Context Engine (AGENTS.md, snapshots, dates)        |  |
|  | SQLite Storage (SQLx: history, telemetry, checkpoints)|  |
|  | Git Snapshot Engine (gix: rollbacks, branch diffs)   |  |
|  | Code Intel Indexer (tree-sitter, FTS5/tantivy)       |  |
|  | LSP & MCP Client Drivers                             |  |
|  +------------------------------------------------------+  |
+------------------------------------------------------------+
```

### Key Topographical Decisions:
1.  **State Authorization Boundary:** The TUI, GUI, and VS Code extension are strictly stateless visualization layers. If a GUI window is closed mid-turn, the daemon continues executing the agent loop to completion, writing all mutations to the database. Re-attaching a client replays the event logs.
2.  **Concurrency Rules:** Async I/O (file ops, LSP diagnostics, provider streams) runs on the `tokio` multi-threaded scheduler. Blocking CPU work (tree-sitter repo maps, indexing 200k symbols, diffing) runs on `rayon`, **bridged** to the async world via `spawn_blocking` (one-shot) or a global rayon pool returning over a `tokio::oneshot` — never `par_iter` directly inside a tokio task (it blocks a worker).
3.  **Lazy initialization (cold-start discipline):** to hit <100 ms / <30 MB, only **config + DB/migrations(WAL) + socket + token** are eager. The model **catalog** (first lookup), **keyring** (first provider call), **code-intel index** (background; lives in SQLite, not a resident in-memory map), **tree-sitter grammars** (per language, first parse), and **LSP/MCP servers** (first need) are all lazy.
4.  **Local Loopback and Security:** The daemon binds to `127.0.0.1` (or local Unix sockets) exclusively by default. The bearer token (generated on startup, stored `0600`) is required on REST, the WS upgrade, **and** SSE; `Origin`/`Host` are validated (see `specs/security.md`).

---

## 2. Crate & Directory Layout

To maintain strict modular boundaries, the Cargo workspace isolates distinct features. Crates are named using the `private-code-` prefix.

```
private-code/
├── Cargo.toml                  # Cargo workspace configuration
├── README.MD                   # Project vision and stack overview
├── plan.md                     # This master development plan
├── specs/                      # Detailed specification documents
│   ├── database.md
│   ├── context_engine.md
│   └── api_protocol.md
├── cli/                        # CLI Entrypoint Crate
│   ├── Cargo.toml
│   └── src/
│       └── main.rs             # CLI arguments parsing (clap) and daemon spawn hooks
├── apps/
│   └── desktop/                # Tauri 2 application (Phase 3)
│       ├── src-tauri/          # Rust backend (links to daemon crate)
│       └── src/                # Solid.js + Tailwind frontend
└── crates/
    ├── protocol/               # Event and payload serialization types
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       ├── message.rs      # Serializable message content blocks and roles
    │       ├── event.rs        # Typed event stream (session, message, tool, checkpoint, usage, error)
    │       └── config.rs       # Configuration schema types (schemars-annotated)
    ├── core/                   # The primary brain: database, state coordinator, context
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       ├── db/             # Migrations, SQL queries, transaction logs
    │       │   ├── mod.rs
    │       │   └── migrations/ # Embedded SQLx migration files
    │       ├── session/        # Session structure, resume capabilities, input queue
    │       │   ├── mod.rs
    │       │   ├── store.rs    # Session CRUD backed by SQLite
    │       │   ├── runner.rs   # Turn-by-turn execution coordinator
    │       │   └── schema.rs   # Session ID newtypes and validation
    │       ├── context/        # Context epochs, baseline formatting, snapshots
    │       │   ├── mod.rs
    │       │   ├── source.rs   # ContextSource trait + built-in sources
    │       │   ├── epoch.rs    # Epoch lifecycle state machine
    │       │   └── registry.rs # Scoped context source registry
    │       ├── agent/          # Agent definitions, selection, switching
    │       │   ├── mod.rs
    │       │   └── builtin.rs  # build / plan / general subagent definitions
    │       ├── permission/     # Permission policy engine
    │       │   ├── mod.rs
    │       │   ├── engine.rs   # Wildcard matching, rule evaluation
    │       │   └── saved.rs    # Persistent per-project remembered permissions
    │       ├── config/         # Configuration loading and resolution
    │       │   ├── mod.rs
    │       │   └── loader.rs   # Hierarchical config discovery (global -> project -> .opencode)
    │       ├── orchestrator.rs # The main turn loop state-machine
    │       └── checkpoint.rs   # Git-backed snapshot creation and rollback
    ├── providers/              # Model integration layer
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       ├── traits.rs       # Core ModelProvider trait and capability declarations
    │       ├── anthropic.rs    # Anthropic API SSE implementation
    │       ├── openai.rs       # OpenAI-compatible API (also covers DeepSeek, Groq)
    │       ├── google.rs       # Gemini API implementation
    │       ├── local.rs        # Ollama / LM Studio OpenAI-compat wrapper
    │       ├── catalog.rs      # Metadata registry (models.dev-style) with pricing + capabilities
    │       └── keyring.rs      # OS keychain integration for secure key storage
    ├── tools/                  # Environment and code mutation tools
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       ├── traits.rs       # Base Tool trait, ToolRegistry, and execution context
    │       ├── fs_read.rs      # read_file, glob, find tools
    │       ├── fs_write.rs     # write_file, create_directory tools
    │       ├── patch.rs        # Structural Myers/Histogram file diffing and editing
    │       ├── grep.rs         # Regex/literal search across workspace (uses `ignore` crate)
    │       ├── shell.rs        # Gated subprocess command execution
    │       ├── web_fetch.rs    # HTTP fetch tool for documentation retrieval
    │       └── output_store.rs # Managed tool output truncation and file storage
    ├── codeintel/              # Syntax analysis, search, indexer
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       ├── parser.rs       # tree-sitter grammars loading and symbol extraction
    │       ├── walker.rs       # Gitignore-respecting file walker (uses `ignore` crate)
    │       ├── index.rs        # SQLite FTS5 / tantivy indexing backends
    │       ├── fuzzy.rs        # Nucleo-based fuzzy matching interface
    │       └── repomap.rs      # Structural skeleton builder for context injection
    ├── daemon/                 # Headless server exposing protocol over REST + WS
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       ├── server.rs       # Axum router, middleware, auth token generation
    │       ├── ws.rs           # WebSocket upgrade handler and event broadcasting
    │       ├── sse.rs          # SSE fallback endpoint
    │       └── routes/         # REST route handlers (project, session, message, etc.)
    ├── lsp/                    # Language Server Protocol client wrapper
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       └── client.rs       # LSP JSON-RPC client implementation over stdio
    ├── mcp/                    # Model Context Protocol client implementation
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       └── client.rs       # MCP Server discovery and tool/resource registration
    ├── plugins/                # WASM plugin execution sandbox
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       ├── runtime.rs      # Extism WASM runtime with host function bindings
    │       └── hooks.rs        # Plugin lifecycle hook definitions (pre/post turn, tool wrap)
    └── tui/                    # Terminal user interface client
        ├── Cargo.toml
        └── src/
            ├── lib.rs
            ├── app.rs          # App state definition and setup
            ├── view.rs         # Ratatui layout elements (panels, message list, input bar)
            ├── update.rs       # Action processors (keyboard events, WS message handling)
            ├── markdown.rs     # Terminal markdown renderer (inline code, bold, etc.)
            └── theme.rs        # Color palette definitions, truecolor support
```

---

## 3. Core Traits & API Interface Design

To prevent component-coupling, we define clear interfaces between crates. Each trait is designed to be object-safe where possible and uses `async_trait` for async methods.

### Provider Trait (`private-code-providers`)

> **Type ownership:** the canonical message/content/usage/stream types below (`Role`, `ChatMessage`, `ContentBlock`, `ToolResultContent`, `ToolDefinition`, `UsageStats`, `FinishReason`, the stream-event enum) live in **`crates/protocol`**, not in `providers`. `providers`, `tools`, and `core` all depend on `protocol`. Defining them in `providers` (as the first draft did) inverts the dependency DAG and drags `reqwest`/`eventsource`/`keyring` across the workspace. The `ContentBlock` model is the reference's richer tagged-union part model (assistant parts `text | reasoning | tool{state: pending|running|completed|error}`; message variants `user|assistant|synthetic|system|agent_switched|model_switched|compaction`).

```rust
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// Role in the conversation. Uses an enum rather than a bare string
/// to prevent typos and enable match-exhaustiveness checking.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

/// Message-level variant (mirrors the reference message kinds). The DB
/// `session_message.type` column is this discriminant; `data` is the serialized
/// `content: Vec<ContentBlock>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    User, Assistant, Synthetic, System, AgentSwitched, ModelSwitched, Compaction,
}

/// A content part. Assistant content can carry text, reasoning, and tool calls
/// with a lifecycle state (the original flat `Text | ToolCall | ToolResult`
/// had nowhere to put reasoning parts or per-tool running/error state).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    Text { text: String },
    Reasoning { text: String, signature: Option<String> },  // extended-thinking part (signature preserved for replay)
    ToolCall { id: String, name: String, arguments: serde_json::Value, state: ToolState },
    ToolResult { id: String, content: Vec<ToolResultContent>, is_error: bool },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolState { Pending, Running, Completed, Error }

/// Tool results can contain text or references to managed output files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToolResultContent {
    Text { text: String },
    File { path: String, mime: String },
}

/// Immutable metadata about a model, loaded from the catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub provider_id: String,
    pub display_name: String,
    pub context_window: usize,
    pub max_output_tokens: usize,
    pub supports_tool_use: bool,
    pub supports_streaming: bool,
    pub supports_prompt_caching: bool,
    pub input_cost_per_mtok: f64,   // USD per million tokens
    pub output_cost_per_mtok: f64,
    pub cache_read_cost_per_mtok: Option<f64>,
}

/// Reasoning depth, mirroring the reference's ReasoningEffort. In the 2026
/// Anthropic API the `effort` parameter replaced `budget_tokens`/`temperature`
/// on Opus 4.7/4.8 — sending `temperature` to those models is rejected.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort { None, Low, Medium, High, Max }

/// Per-request configuration passed to stream_chat.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub model_id: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    /// Preferred control on modern Anthropic models.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Gated to providers that still accept it (OpenAI/Gemini); OMITTED for
    /// Anthropic Opus 4.7/4.8 (the provider impl drops it or 400s).
    pub temperature: Option<f32>,
    pub max_tokens: Option<usize>,
    /// Provider-specific request body overrides (headers, extra fields).
    pub extra_headers: std::collections::HashMap<String, String>,
    pub extra_body: serde_json::Value,
}

/// The stream yields a typed event sequence whose TERMINAL item carries the
/// usage + finish reason. This replaces the original `TokenDelta` + the
/// unreachable `StreamCompletion`/`stream_completion` (nothing yielded it, so
/// usage and stop_reason were silently lost — breaking cost accounting and
/// finish-reason control flow).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamEvent {
    Text(String),
    Reasoning(String),                                   // extended thinking deltas
    ToolCallStart { id: String, name: String },
    ToolCallDelta { id: String, arguments: String },
    ToolCallComplete { id: String },
    StepFinish { usage: UsageStats },                    // a provider "step" within a turn finished
    Finish { reason: FinishReason, usage: UsageStats },  // TERMINAL — always the last item
}

/// Usage statistics returned after a complete model turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageStats {
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub reasoning_tokens: usize,
    pub cache_read_tokens: usize,
    pub cache_write_tokens: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("Rate limited by provider; retry after {retry_after_secs:?}s")]
    RateLimit { retry_after_secs: Option<u64> },
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("JSON serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Authentication failed: invalid or missing API key")]
    AuthenticationFailed,
    #[error("Context length exceeded: {used} tokens used, {limit} allowed")]
    ContextLengthExceeded { used: usize, limit: usize },
    #[error("Provider error: {0}")]
    Unknown(String),
}

/// Cross-provider finish reason (the original `StopReason` was Anthropic-only
/// and force-fit OpenAI/Gemini + 2026 refusal/context-window into `EndTurn`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,            // natural end of turn
    ToolCalls,       // model wants tools
    Length,          // hit max_tokens (a truncated REQUEST cap)
    Truncation,      // streaming `model_context_window_exceeded` — a SUCCESSFUL response with
                     // valid partial output; distinct from the pre-generation 400 (do NOT compact on it)
    ContentFilter,
    Refusal,         // Opus 4.7+ refusal; capture stop_details into provider_metadata
    Error,
    Unknown,
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Stream a chat completion. The returned stream yields `StreamEvent`s and
    /// MUST end with a terminal `StreamEvent::Finish { reason, usage }`.
    /// `cancel` lets a mid-turn abort RPC (arriving on another task) stop the
    /// SSE loop: on cancel, abort the underlying reqwest stream and yield a
    /// final `Finish` with usage-so-far.
    async fn stream_chat(
        &self,
        config: &ProviderConfig,
        history: Vec<ChatMessage>,
        system_prompt: &str,
        tools: &[ToolDefinition],
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError>;

    /// SYNC, LOCAL estimate over the full structured request — used on the hot
    /// per-turn compaction/budget path. Never hits the network. (A bare `&str`
    /// can't represent system+tools+multi-block messages; tiktoken is GPT BPE
    /// and is WRONG for Claude — undercounts ~15-20%.)
    fn estimate_tokens(&self, config: &ProviderConfig, request: &ChatRequest) -> usize;

    /// OPTIONAL accurate count. For Anthropic this POSTs the full structured
    /// body to `/v1/messages/count_tokens`. Used as a pre-flight when accuracy
    /// matters — NEVER called per turn (a network round-trip × N in fan-out
    /// would blow the <100ms/60fps and RPM budgets).
    async fn count_tokens(&self, config: &ProviderConfig, request: &ChatRequest)
        -> Result<usize, ProviderError> { Ok(self.estimate_tokens(config, request)) }
}

// NOTE: model capabilities/pricing are NOT on the provider trait. `model_info`
// returning `Option<&ModelInfo>` borrowed from the provider couples catalog
// data to provider lifetime (you'd need to construct every provider just to
// price a model or run cheapest_capable()). The catalog is a STANDALONE service
// returning OWNED `ModelInfo` — see §6B and Step 5.5.

/// Tool schema passed to the model so it knows what tools are available.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema
}

/// The fully-assembled request — what `estimate_tokens`/`count_tokens` measure
/// and what `stream_chat` sends. Bundling it makes the token estimate operate
/// over the SAME structured payload that is dispatched (system + tools +
/// multi-block history), which a bare `&str` could not represent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub system_prompt: String,
    pub history: Vec<ChatMessage>,
    pub tools: Vec<ToolDefinition>,
}
```

### Tool Trait (`private-code-tools`)
```rust
use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;

/// NON-AUTHORIZING metadata. This does NOT gate the tool — the action/resource
/// rule engine (§6A) is the SOLE permission authority. `mutates` answers exactly
/// one question: "does this tool change the worktree, so take a git checkpoint
/// before it runs?" (The reference has no Safe/ReadWrite/Dangerous concept;
/// wiring a class as the gate overrode valid user rules — a P0 regression.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolMeta {
    /// true → snapshot before execution (see specs/checkpointing.md §5).
    pub mutates: bool,
}

/// Runtime context available to every tool execution.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub workspace_root: PathBuf,
    pub session_directory: PathBuf,
    pub session_id: String,
    pub agent_id: String,
    /// Absolute path to the managed tool-output storage directory.
    pub tool_output_dir: PathBuf,
}

/// The raw output of a tool before truncation/bounding.
#[derive(Debug)]
pub struct ToolOutput {
    /// Human-readable textual content.
    pub content: Vec<ToolOutputContent>,
    /// Optional structured data (e.g., JSON) that persists even after
    /// textual content is truncated.
    pub structured: Option<serde_json::Value>,
    pub is_error: bool,
}

#[derive(Debug)]
pub enum ToolOutputContent {
    Text(String),
    File { path: String, mime: String },
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("Permission denied for this tool")]
    PermissionDenied,
    #[error("Tool execution failed: {0}")]
    ExecutionFailed(String),
    #[error("Invalid arguments: {0}")]
    InvalidArguments(#[from] serde_json::Error),
    #[error("Path is outside workspace boundaries: {0}")]
    PathTraversal(String),
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;

    /// Non-authorizing metadata — drives checkpointing only, NOT gating.
    fn meta(&self) -> ToolMeta;

    /// The permission action string used by the permission engine.
    /// Defaults to the tool name. Tools like `bash` may return
    /// a more specific action like "bash:<command_prefix>".
    fn permission_action(&self) -> String {
        self.name().to_string()
    }

    /// The resource identifiers checked against the permission ruleset.
    /// For filesystem tools this is the file path(s); for bash it is the command.
    fn permission_resources(&self, args: &Value) -> Vec<String>;

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> Result<ToolOutput, ToolError>;
}

/// Central registry that owns all available tools for a session.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.iter().find(|t| t.name() == name).map(|t| t.as_ref())
    }

    /// Generate tool definitions for the provider's tool-use API.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|t| ToolDefinition {
            name: t.name().to_string(),
            description: t.description().to_string(),
            parameters: t.parameters_schema(),
        }).collect()
    }
}
```

---

## 4. SQLite Persistence Schema

Private Code uses SQLite for durable storage via `sqlx` (WAL). **[`specs/database.md`](specs/database.md) is the authoritative schema** — it corrects this section's draft in three load-bearing ways: (1) the **three-operation context-epoch model** (insert / advance-snapshot-only-CAS / replace-baseline-CAS) replaces the single overwrite-everything upsert that defeated prompt-cache stability; (2) `session.seq_counter` + `BEGIN IMMEDIATE` allocation replaces the racey `MAX(seq)+1`; (3) `checkpoint.commit_hash` → **`tree_hash`** (+ `kind`), `session_message.type` carries the richer reference message variants, and `delivery` uses `steer`/`queue`. The SQL below is retained for narrative; where it differs from `specs/database.md`, the spec wins.

```
  +-------------------------------------------------------------+
  |                          project                            |
  |  - id: TEXT (PK)                                            |
  |  - name: TEXT                                               |
  |  - directory: TEXT                                          |
  |  - created_at: INTEGER                                      |
  +------------------------------+------------------------------+
                                 | 1
                                 |
                                 | N
  +------------------------------v------------------------------+
  |                           session                           |
  |  - id: TEXT (PK)                                            |
  |  - project_id: TEXT (FK -> project.id)                      |
  |  - parent_id: TEXT (FK -> session.id, nullable)             |
  |  - workspace_path: TEXT                                     |
  |  - active_directory: TEXT                                   |
  |  - title: TEXT                                              |
  |  - agent_id: TEXT                                           |
  |  - model_config: TEXT (JSON)                                |
  |  - cost: REAL                                               |
  |  - tokens_input: INTEGER                                    |
  |  - tokens_output: INTEGER                                   |
  |  - tokens_reasoning: INTEGER                                |
  |  - tokens_cache_read: INTEGER                               |
  |  - tokens_cache_write: INTEGER                              |
  |  - revert: TEXT (JSON, nullable)                            |
  |  - permission: TEXT (JSON, nullable)                        |
  |  - created_at: INTEGER                                      |
  |  - updated_at: INTEGER                                      |
  +------------------------------+------------------------------+
                                 | 1
                                 |
                                 | N
  +------------------------------v------------------------------+
  |                       session_message                       |
  |  - id: TEXT (PK)                                            |
  |  - session_id: TEXT (FK -> session.id)                      |
  |  - seq: INTEGER                                            |
  |  - type: TEXT ('user_input', 'model_turn', 'system_update') |
  |  - data: TEXT (JSON content blocks)                         |
  |  - created_at: INTEGER                                      |
  +-------------------------------------------------------------+
                                 | 1
                                 |
                                 | 1
  +------------------------------v------------------------------+
  |                    session_context_epoch                    |
  |  - session_id: TEXT (PK, FK -> session.id)                  |
  |  - agent_id: TEXT                                           |
  |  - baseline: TEXT                                           |
  |  - snapshot: TEXT (JSON maps source key -> hash/snapshot)   |
  |  - baseline_seq: INTEGER                                    |
  |  - replacement_seq: INTEGER (nullable)                      |
  |  - revision: INTEGER                                        |
  +-------------------------------------------------------------+
  
  +-------------------------------------------------------------+
  |                    permission_saved                         |
  |  - project_id: TEXT (FK -> project.id)                      |
  |  - action: TEXT                                             |
  |  - resource: TEXT                                           |
  |  - created_at: INTEGER                                      |
  |  (PK: project_id, action, resource)                         |
  +-------------------------------------------------------------+
```

### Full SQL Migrations Script:
```sql
-- Migration 0001_initial_schema.sql

CREATE TABLE project (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    directory TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE session (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL,
    parent_id TEXT,
    workspace_path TEXT NOT NULL,
    active_directory TEXT NOT NULL,
    title TEXT NOT NULL,
    agent_id TEXT NOT NULL DEFAULT 'build',
    model_config TEXT NOT NULL, -- JSON: { provider_id, model_id, temperature, max_tokens }
    cost REAL NOT NULL DEFAULT 0.0,
    tokens_input INTEGER NOT NULL DEFAULT 0,
    tokens_output INTEGER NOT NULL DEFAULT 0,
    tokens_reasoning INTEGER NOT NULL DEFAULT 0,
    tokens_cache_read INTEGER NOT NULL DEFAULT 0,
    tokens_cache_write INTEGER NOT NULL DEFAULT 0,
    revert TEXT, -- JSON: { message_id, tree_hash }  (see specs/database.md)
    permission TEXT, -- JSON: per-session permission ruleset override
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY(project_id) REFERENCES project(id) ON DELETE CASCADE,
    FOREIGN KEY(parent_id) REFERENCES session(id) ON DELETE SET NULL
);

CREATE INDEX idx_session_project ON session(project_id);
CREATE INDEX idx_session_parent ON session(parent_id);

CREATE TABLE session_message (
    id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    type TEXT NOT NULL, -- 'user_input', 'model_turn', 'system_update', 'tool_result'
    data TEXT NOT NULL, -- JSON serialization of ContentBlock enum
    created_at INTEGER NOT NULL,
    FOREIGN KEY(session_id) REFERENCES session(id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX idx_session_message_seq ON session_message(session_id, seq);
CREATE INDEX idx_session_message_session_created ON session_message(session_id, created_at);

CREATE TABLE session_context_epoch (
    session_id TEXT PRIMARY KEY NOT NULL,
    agent_id TEXT NOT NULL DEFAULT 'build',
    baseline TEXT NOT NULL,
    snapshot TEXT NOT NULL, -- JSON key-value store of context keys -> snapshots
    baseline_seq INTEGER NOT NULL,
    replacement_seq INTEGER,
    revision INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY(session_id) REFERENCES session(id) ON DELETE CASCADE
);

CREATE TABLE session_input (
    id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL,
    prompt TEXT NOT NULL,
    delivery TEXT NOT NULL, -- 'steer' or 'queue'
    admitted_seq INTEGER NOT NULL,
    promoted_seq INTEGER,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(session_id) REFERENCES session(id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX idx_session_input_admitted ON session_input(session_id, admitted_seq);
CREATE UNIQUE INDEX idx_session_input_promoted ON session_input(session_id, promoted_seq);

CREATE TABLE checkpoint (
    id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    tree_hash TEXT NOT NULL,   -- git tree object (NOT a commit); + kind in specs/database.md
    tool_name TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(session_id) REFERENCES session(id) ON DELETE CASCADE
);

CREATE INDEX idx_checkpoint_session ON checkpoint(session_id);

CREATE TABLE permission_saved (
    project_id TEXT NOT NULL,
    action TEXT NOT NULL,
    resource TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (project_id, action, resource),
    FOREIGN KEY(project_id) REFERENCES project(id) ON DELETE CASCADE
);
```

---

## 5. System Context & Epoch Lifecycle Engine

Following `Reference/CONTEXT.md`, the context manager compiles instructions (`AGENTS.md`) and ambient details (directory, platform, date) without repeating them every turn. **[`specs/context_engine.md`](specs/context_engine.md) is authoritative** and corrects this section: `ContextSource::load` returns **`Loaded | Unavailable`** (not `Result<Value, String>` — an `Unavailable` source must retain the prior snapshot, not error), equivalence is **codec-based**, and the reconcile result is **`Unchanged | Updated{text,snapshot} | ReplacementReady | ReplacementBlocked`** (an `AGENTS.md` edit is an **Updated** — a mid-conversation `{role:"system"}` message with the baseline **preserved** — *not* "TriggerCompaction"). The Rust sketches below are indicative; the spec governs.

### State Transitions & Epoch Compaction Flow:
```
  [Start Turn]
       │
       ▼
  Loads Context Sources (Date, Env, Files, AGENTS.md)
       │
       ├─────────────────────────────────┐
       ▼ (Observation Success)           ▼ (Observation Failure)
  Evaluate diff against Snapshot    Retain Prior Snapshot & Wait
       │
       ├─────────────────────────┬─────────────────────────┐
       ▼ (No Changes)            ▼ (Minor changes)         ▼ (Major change/Incompatible)
  Reconciliation Unchanged  Emit Mid-Conv Sys Msg     Trigger Epoch Replacement
       │                    (Appended to history)     (Compact and start new Epoch)
       │                                 │                         │
       └─────────────────────────┼─────────────────────────┘
                                 ▼
                         [Provider Call]
```

### Data Modeling in Rust:
```rust
use std::collections::HashMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceSnapshotValue {
    pub value: serde_json::Value,
    pub removed_representation: Option<String>,
}

pub type SystemContextSnapshot = HashMap<String, SourceSnapshotValue>;

// Corrected to match specs/context_engine.md (the earlier draft used
// EpochReplacementNeeded/ObservationBlocked and a fallible load — both wrong).
#[derive(Debug)]
pub enum Reconcile {
    Unchanged,
    Updated { text: String, snapshot: SystemContextSnapshot },     // mid-conv {role:"system"} message; baseline PRESERVED
    ReplacementReady { generation: Generation },                   // new baseline; ends the epoch
    ReplacementBlocked,                                            // a source is Unavailable; retry next boundary
}

#[async_trait::async_trait]
pub trait ContextSource: Send + Sync {
    fn key(&self) -> &str;
    /// Infallible by design: a transient failure maps to Unavailable, NOT an
    /// error — an Unavailable source retains the prior snapshot and blocks
    /// replacement, rather than corrupting the baseline.
    async fn load(&self, location: &Location) -> SourceLoad; // Loaded(Value) | Unavailable
    fn compare(&self, previous: &serde_json::Value, current: &serde_json::Value) -> SourceCompare; // codec equivalence
    fn encode(&self, value: &serde_json::Value) -> serde_json::Value;
    fn render_baseline(&self, current: &serde_json::Value) -> String;
    fn render_update(&self, previous: &serde_json::Value, current: &serde_json::Value) -> String;
    fn render_removal(&self, previous: &serde_json::Value) -> Option<String>;
}
```

---

## 6. Tool Gating, Truncation & Checkpoints

When the model calls a tool, Private Code passes it through this pipeline. **The permission decision is made entirely by the action/resource rule engine (§6A)** — the tool's `mutates` metadata only decides whether to take a checkpoint first.

```
       [Model invokes Tool]
                 │
                 ▼
   [Permission engine: evaluate(action, resource, …rules)]   ← the SOLE gate (§6A)
        /              │               \
   (allow)          (ask)            (deny)
      │           [prompt user]         │
      │          once/always/reject     │
      │         /          \            │
      │   (approved)    (rejected)      │
      ▼       ▼              ▼           ▼
   [tool.mutates? → take git checkpoint]   [fail tool: feedback → model]
                 │
                 ▼
        [Execute tool]
                 │
                 ▼
       [Verify Output Size]
         /              \
    (< Limit)       (> Limit)
       /                  \
 [Return Plain]    [Write Managed Output File (global data dir)]
                   [Append head/tail preview to Msg]
```

### Git Checkpointing Implementation Strategy:
**See the normative [`specs/checkpointing.md`](specs/checkpointing.md).** Summary: snapshots use a **shadow git directory** (`$DATA_DIR/snapshot/<project>/<hash(worktree)>`) whose work-tree is the user's workspace. A snapshot is `write-tree` (a bare **tree** object — **no commit, no branch, no HEAD, no stash**); restore is `read-tree` + `checkout-index -a -f`; per-file revert is `checkout <tree> -- <file>`. The write engine is **git2** (gix's high-level write/checkout APIs are not yet implemented; gix is used for read-only diffs). Snapshots respect gitignore, skip files > 2 MB, and seed `info/exclude` from the source repo. The original "staging commit on the active branch + `git reset --hard HEAD`" design was **deleted**: it mutated the user's branch/reflog/index *and* reset to HEAD rather than the snapshot (destructive **and** a no-op).

### Managed Tool Output Files:
If a tool's output exceeds the configured limit (`tool_output.max_lines = 2000`, `max_bytes = 51200`):
1.  Write the full output to a flat file in the **global data dir** (`$DATA_DIR/tool_outputs/tool_<uuid>.txt`) — **not** inside the workspace (which would pollute git and could be captured by a snapshot). 7-day retention.
2.  Return a bounded preview to the model: head = `ceil(max_lines/2)` lines, tail = `floor(max_lines/2)` lines, prefixed `[Truncated — full output at <path>]`. The bounded preview is the durable record (the file is a convenience); a storage failure must **not** fail the tool.

---

## 6A. Permission Engine

The permission engine controls what tools the agent is allowed to execute and under what conditions. It follows the reference project's wildcard-matching rule evaluation model.

### Permission Rule Structure:
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    pub action: String,     // e.g., "write_file", "bash", "*"
    pub resource: String,   // e.g., "src/**", "*", "rm *"
    pub effect: PermissionEffect,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionEffect {
    Allow,   // Auto-approve
    Ask,     // Prompt the user (default for unknown actions)
    Deny,    // Block entirely
}

pub type PermissionRuleset = Vec<PermissionRule>;
```

### Evaluation Algorithm (the reference's, not a flat list):
The original "1. Agent 2. Saved 3. Global 4. Ask" linear walk loses two properties the reference relies on — the **agent-deny short-circuit** (a security property) and **cross-resource aggregation**. Restore the real algorithm (`permission.ts`):

1. **Resolve the agent ruleset.** A missing/unresolved agent fails **closed** with `{*, *, deny}`.
2. **Agent-deny short-circuit (non-overridable):** if **any** resource is denied under the *agent rules alone*, return `Deny` immediately. A saved "always allow" can **never** override an agent deny.
3. Otherwise build `all = [...agent_rules, ...saved_rules]`. **Saved rules are always coerced to `effect = allow`** (there is no saved-deny).
4. **Per resource:** `findLast` match over `all` (last matching rule wins), with an **`ask` fallback** when no rule matches.
5. **Aggregate across all resources:** `deny > ask > allow` (any deny → deny; else any ask → ask; else allow).

There is **no separate "Global" tier** — global rules are authored into the agent's permission set. The `PermissionClass`/`ToolMeta` from §3 plays **no part** in this decision; it only triggers checkpointing.

When the result is `Ask`, the daemon:
1. Emits a `tool.permission_required` event (Durable) minting one `permission_id` (see `api_protocol.md §3`).
2. Suspends the turn loop (a `oneshot`/`CancellationToken`-aware await) at a safe boundary.
3. Waits for a `permission.reply` carrying `permission_id` and `reply ∈ {once, always, reject}` (the original boolean `approved` cannot express "always").
4. On `always`, saves `{action, resource}` to `permission_saved` for this project.
5. On `reject`, the tool fails with a corrected-error result whose optional `feedback` text is fed back to the model.

### Built-in Agent Permission Defaults (restored to the reference + one §7.10 override):

The original table was factually wrong — `build = {*,*,allow}` strips every guard (editing `.env` / external dirs would auto-approve), and the described "general" agent is actually the reference's **`explore`** agent. Corrected:

| Agent | Mode | Default rules |
|---|---|---|
| `build` | primary | `*:allow`, **plus carve-outs:** `doom_loop:ask`, `external_directory.*:ask`, `read.*.env:ask` / `*.env.*:ask` (`*.env.example:allow`), plan-mode gating — **plus `bash:ask`** (North Star §7.10: "bash prompts unless whitelisted"). **Not** `{*,*,allow}`. |
| `plan` | primary | `build` defaults + `edit/write:deny` (with a `data/plans/*:allow` carve-out) + `plan_exit:allow`. Read-only exploration. |
| `general` | subagent | denies **only** `todowrite`; **keeps bash**. Multi-step helper. |
| `explore` | subagent | `*:deny` then allow `grep/glob/list/bash/read/webfetch/websearch`. **This** is the search-only/no-write agent (the one the plan mislabeled "general"). |
| `compaction`, `title`, `summary` | hidden | each `*:deny`. Internal one-shot helpers. |

Unresolved agents fail closed (`{*,*,deny}`); the engine fallback is `ask`. A cloned repo's `AGENTS.md` never widens these rules (see `security.md` T1).

---

## 6B. Configuration System

Private Code uses a hierarchical JSONC configuration system that mirrors the reference project's approach. Configuration is loaded from multiple locations and merged with later entries taking priority.

### Discovery Order (lowest to highest priority):
1. `~/.config/private-code/config.json` — Global user defaults.
2. `<project-root>/private-code.json` or `<project-root>/private-code.jsonc` — Project-level config.
3. `<project-root>/.private-code/config.json` — Hidden directory config.
4. Environment variables (`PRIVATE_CODE_MODEL`, `PRIVATE_CODE_PROVIDER`, etc.).

### Configuration Schema (key fields):
```json
{
  "$schema": "./config-schema.json",
  "model": "anthropic/claude-opus-4-8",
  "small_model": "anthropic/claude-haiku-4-5",
  "shell": "/bin/zsh",
  "default_agent": "build",
  "snapshots": true,
  "reasoning_effort": "high",
  "compaction": { "auto": true, "buffer_tokens": 8000 },
  "permissions": [
    { "action": "bash", "resource": "cargo *", "effect": "allow" }
  ],
  "agents": {
    "architect": {
      "model": "anthropic/claude-opus-4-8",
      "system": "You are a senior software architect...",
      "permissions": [
        { "action": "write_file", "resource": "*", "effect": "deny" }
      ]
    }
  },
  "providers": {
    "ollama": {
      "base_url": "http://localhost:11434"
    }
  },
  "mcp": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path"]
    }
  },
  "commands": {
    "test": {
      "prompt": "Run the test suite and report any failures."
    }
  },
  "tool_output": {
    "max_lines": 2000,
    "max_bytes": 51200
  }
}
```

The `schemars` crate generates a JSON Schema from the Rust config struct, enabling autocomplete in editors.

> **Model references are catalog IDs (`provider/model`), never hardcoded API IDs.** `claude-opus-4-8` shown above is an illustrative *current* alias resolved from the catalog at load time; the config and code must **never** bake in dated API IDs. Defaults flow through `default()`/`small()`/`cheapest_capable()`/`fastest_capable()` against the catalog (§6B / Step 5.5). **Config precedence:** ordinary settings forward-merge (later location wins); `experimental.policies` (e.g. `provider.use`) merge in **reverse** (user-global wins) — match the reference's `config.md` shapes (plural `agents`/`permissions`/`providers`; `permissions: [{action, resource, effect}]`; per-agent `system`, `steps`, `disabled`).

---

## 6C. Agent System

Agents define the persona, permissions, and model routing for the AI assistant.

### Built-in Agents:

| Agent      | Mode      | Description                                                            | Default Model     |
|------------|-----------|------------------------------------------------------------------------|--------------------|
| `build`    | primary   | Development agent. Reads/writes/searches; **bash is `ask`** by default (§6A). | Config default     |
| `plan`     | primary   | Read-only exploration; edits/writes denied (plans-dir carve-out).      | Config default     |
| `general`  | subagent  | Multi-step helper via `@general`. Denies only `todowrite`; **keeps bash**. | Fast/cheap model   |
| `explore`  | subagent  | **Search-only** subagent: `*:deny` then allow grep/glob/list/bash/read/web. | Fast/cheap model   |
| `compaction` / `title` / `summary` | hidden | Internal one-shot helpers (each `*:deny`).                | Fast/cheap model   |

### Agent Switching:
*   **TUI:** Press `Tab` to cycle primary agents. Type `@general <query>` to invoke the subagent inline.
*   **GUI:** Agent selector dropdown in the input bar.
*   Switching agents triggers a Context Epoch replacement (new baseline system prompt, agent-specific permissions).

### Custom Agents:
Users define custom agents in config. Each agent specifies:
*   `model` — Override the default model.
*   `system` — Custom system prompt text.
*   `permissions` — Per-agent permission ruleset.
*   `steps` — Max tool-use turns before the agent must stop.
*   `color` — Display color in the UI.

---

## 6D. Error Handling & Retry Strategy

Private Code uses `thiserror` for library-level errors and `color-eyre` for application-level error reporting.

### Provider Error Recovery:
*   **Rate Limits:** Automatic exponential backoff (1s → 2s → 4s → ... up to 60s). The UI shows a countdown timer.
*   **Network Errors:** Retry up to 3 times with 2s delay. Surface the error to the user after exhaustion.
*   **Context Length Exceeded (pre-generation 400):** trigger compaction (a Replacement epoch) and retry **once**; a second overflow, or overflow-after-output, is terminal. Do **not** confuse this with the *streaming* `model_context_window_exceeded` finish — that is a **successful** response with valid partial output (`FinishReason::Truncation`); compacting on it would discard good output.
*   **Authentication Failures:** Surface immediately — do not retry. Prompt the user to reconfigure their API key.

### Proactive Compaction (the primary path):
Don't wait for a 400. **Before each provider turn**, compute a sync **local** token estimate of the assembled request and compare against `context_window − max(requested_output, compaction.buffer)`. When over budget and older complete turns exist, compact first: keep the full transcript durable but replace its active model representation with a rolling summary, and **drop provider-native assistant/reasoning/tool messages across the boundary** (avoids replaying stale thinking signatures / encrypted reasoning) via a Replacement epoch. `/compact` is the manual entry onto this same machinery. Config: `compaction: { auto, buffer_tokens, keep_tokens }`.

### Tool Error Recovery:
*   **Permission Denied / Rejected:** Feed the denial reason back to the model as a tool error result so it can adjust.
*   **Path Traversal:** Hard error — the tool refuses and the model receives a clear error message.
*   **Execution Failures:** Return the stderr/error output to the model as an error tool result.

---

## 7. Comprehensive Step-by-Step Roadmap

### Phase 0: Spec & Scaffold

#### Step 0.1: Project Directory Structure initialization
*   Initialize root `Cargo.toml`.
*   Establish directory tree: `crates/protocol`, `crates/core`, `crates/providers`, `crates/tools`, `crates/tui`, `cli`.
*   Add placeholders files `src/lib.rs` and `Cargo.toml` in all sub-libraries.

#### Step 0.2: Base Protocol Data Types definition
*   Implement `crates/protocol/src/message.rs` and `crates/protocol/src/event.rs`.
*   Validate `serde` structures parse correctly for messages, system updates, and tool invocations.

#### Step 0.3: Setup Cargo workspace configuration
*   Verify that `cargo build` and `cargo check` compile successfully.
*   Setup dependency resolution in root `Cargo.toml` using workspace-level key mapping.

#### Step 0.4: Scaffolding test templates
*   Setup `cargo-nextest` configurations.
*   Setup `criterion` benchmark scaffolding in `crates/core/benches/`.

---

### Phase 1: Prove the Core

#### Step 1.1: Database Schema & Migration Runner
*   Configure `sqlx` SQLite with WAL on connect (`journal_mode=WAL`, `foreign_keys=ON`, `busy_timeout`, `synchronous=NORMAL`). One dedicated **single-writer** task owns the write connection; readers use a separate pool.
*   Write **forward-only, checksummed** migrations (`_sqlx_migrations`; fail-fast on checksum mismatch; never edit an applied migration). `0001_core.sql` ships session/message/epoch/input/checkpoint/permission only — the FTS5 `symbols` objects ship in a **later** Phase-4 migration (see `specs/database.md §3`).
*   Allocate per-session `seq` from `session.seq_counter` under `BEGIN IMMEDIATE` — **never** `SELECT MAX(seq)+1` (lost-write race, see `database.md §2`).
*   Create helpers: `create_session`, `get_session`, `list_sessions`, `append_message`, `get_messages`.
*   Write unit tests for schema creation, CRUD, and a **concurrent-append race test** asserting no dropped `seq`.

#### Step 1.2: Protocol Types & Event Framing
*   Implement `crates/protocol/src/message.rs`: `Role`, `ContentBlock`, `ChatMessage`, `ToolResultContent`.
*   Implement `crates/protocol/src/event.rs`: Typed event enum covering all daemon-to-client events.
*   Validate round-trip serde serialization with unit tests for every variant.

#### Step 1.3: Model Client Interface & Anthropic API Implementation
*   Define the `ModelProvider` trait in `private-code-providers`.
*   Implement `AnthropicProvider` using `reqwest` + `eventsource-stream` for SSE.
*   Handle Anthropic-specific SSE event types: `content_block_start`, `content_block_delta`, `content_block_stop`, `message_delta`, `message_stop`.
*   Parse tool-use blocks from the streamed response.
*   Implement `UsageStats` extraction from the `message_delta` event.
*   Resolve the API key **lazily, on the first provider call** (not at boot — cold-start budget): OS keyring → environment variable fallback → error. Keys never enter the daemon's own process env or child envs (scrub `*KEY*`/`*TOKEN*`/`*SECRET*` before spawning bash/MCP — `security.md` T2/T4).
*   Write integration test (requires real API key, gated behind `#[cfg(feature = "integration")]`).

#### Step 1.4: Context Engine Base Setup
*   Implement the `ContextSource` trait and the `SystemContextRegistry`.
*   Implement built-in sources: `core/environment` (working dir, platform, VCS status) and `core/date`.
*   Implement `core/instructions` source that reads `AGENTS.md` files.
*   Implement `SystemContext::initialize` (renders baseline) and `SystemContext::reconcile` (detects changes).
*   Implement Context Epoch persistence to `session_context_epoch` table.
*   Write unit tests for reconciliation: unchanged, minor update (emits mid-conversation message), major change (triggers epoch replacement).

#### Step 1.5: Core Tools Implementation
*   Implement `ToolRegistry` and base `Tool` trait.
*   Implement `read_file` tool: reads file contents with optional line range, validates path is within workspace.
*   Implement `write_file` tool: writes file contents, creates parent directories, validates path bounds.
*   Implement `glob` tool: uses the `ignore` crate to list matching files.
*   Implement `grep` tool: regex/literal search using the `ignore` walker.
*   Implement `edit` tool (separate from `patch`): **exact-string unique-match** str-replace — reject equal `old`/`new` and empty `old`; zero matches fails; >1 match without `replace_all` fails (no fuzzy matching in Phase 1). Add a **staleness guard** (write only if the file is unchanged vs the bytes read this turn; on mismatch tell the model the file changed and to re-read), CRLF/LF detection, and UTF-8 BOM preservation.
*   Implement `patch` tool: a parser for the **"Begin Patch" envelope** (Add/Update/Delete/Move; context-anchored chunks applied sequentially; permission-check all targets first; on failure report the ops already applied). The `similar` crate is used **only to render display diffs — never to apply** (it computes diffs, it cannot apply them; the original "patch applies unified diffs via `similar`" was doubly wrong).
*   Implement `bash` tool: spawns a subprocess with a **scrubbed child environment** (allowlist; strip `*KEY*`/`*TOKEN*`/`*SECRET*` and the daemon token — see `security.md` T2), captures stdout/stderr, enforces a timeout (120s default). Gated `bash:ask` by default.
*   Implement `web_fetch` tool with **SSRF guards** (`security.md` T3): resolve DNS, reject loopback/link-local/private/CGNAT at connect time, re-validate on every redirect hop, pin against rebinding, cap redirects + response size.
*   Implement tool output truncation via `output_store.rs`: if output exceeds limits, write a managed output file to the **global data dir** and return a head/tail preview (see §6).

#### Step 1.6: Permission Engine (Basic)
*   Implement wildcard-matching rule evaluator (`evaluate(action, resource, rules) -> effect`).
*   Implement `PermissionEngine` that checks agent rules + saved rules + config rules.
*   Wire the permission check into the tool execution pipeline.
*   For Phase 1 (no daemon yet): permission prompts are handled synchronously in the TUI via a blocking dialog.

#### Step 1.7: Git Checkpoint System
*   Implement `checkpoint.rs` per **[`specs/checkpointing.md`](specs/checkpointing.md)** — a **shadow git dir** in the data dir, write engine = **git2** (gix for read-only diffs only).
*   `track()`: snapshot the worktree with `write-tree` → a **tree** hash (no commit/branch/HEAD). Capture at turn start and before/after each `mutates` tool step.
*   `restore()`: `read-tree` + `checkout-index -a -f`. `revert()`: per-file `checkout <tree> -- <file>`, deleting files absent from the tree.
*   Respect gitignore, skip files > 2 MB, seed `info/exclude` from the source repo (never sweep in `node_modules`/`target`/`.env`).
*   `/revert` resets to a recorded `tree_hash`, never to the user's branch HEAD.
*   Edge cases: not a git repo → disabled no-op + warn; user's dirty index / in-progress rebase → **unaffected** (shadow repo has its own index).

#### Step 1.8: Turn Coordinator State Machine
*   Implement `orchestrator.rs` — the main agent turn loop:
    1. Admit user input (persist to `session_input`).
    2. Check context reconciliation at safe boundary.
    3. Assemble message history + system prompt.
    4. Call provider's `stream_chat` with tool definitions.
    5. Parse the streaming `StreamEvent`s: accumulate text/reasoning, detect tool calls, read the terminal `Finish { reason, usage }`.
    6. For each tool call: **evaluate permission (the rule engine)** → if `mutates` take a checkpoint → execute → persist result.
    7. If tool calls were made: loop back to step 3 (feed results back to model).
    8. If no tool calls: persist final assistant message, update usage stats, emit `message.completed`.
*   Run the loop on the **single per-session coordinator** (one writer per session); allocate `seq` from the per-session counter under `BEGIN IMMEDIATE` (no `MAX(seq)+1` race).
*   **Durable semantics:** partial assistant output on abort/error is persisted as a terminal partial (or discarded by a stated rule) — never left dangling, never silently continued. On abort **and on startup**, durably fail any tool still projected `running` ("Tool execution interrupted") before assembling the next request (no replay of side effects).
*   **Idempotent admission:** `admit(id)` returns an existing row unchanged; only the serialized runner promotes admitted→promoted and writes the visible user message in the same commit. `delivery` is `steer | queue` (steer promotes at the next safe boundary including mid-drain; queue is FIFO, one activity at a time).
*   Configurable max turns per drain (default: 25, the reference's cap).
*   Abort via a `CancellationToken` threaded into `stream_chat` (an abort RPC on another task stops the live stream).

#### Step 1.9: Ratatui Terminal Interface Layout
*   Setup crossterm raw mode event listener loop in `crates/tui`.
*   Implement Elm-architecture app model: `AppState`, `Action` enum, `update()`, `view()`.
*   Implement layout panels:
    *   **Left panel:** Message history with role-colored prefixes and streaming text.
    *   **Right panel (toggle with `Ctrl-D`):** Session info, active agent, token usage, cost.
    *   **Bottom bar:** Text input field, model indicator, agent indicator.
*   Implement scrollback buffer for message history (ring buffer, max 10k lines).
*   Implement basic terminal markdown rendering: bold, italic, inline code, code blocks with language label.
*   Implement truecolor theme system with named palette variables.

#### Step 1.10: TUI Loop Integration
*   Wire the TUI input field to the orchestrator's prompt admission.
*   Display streaming token deltas in real-time as they arrive from the provider.
*   Display tool call execution inline (tool name, arguments summary, result preview).
*   Display permission prompts inline with `[y]es / [n]o / [a]lways` key bindings.
*   Implement `/model <name>` command to switch models mid-session.
*   Implement `/agent <name>` and `Tab` to switch agents.
*   Implement `/revert` command to trigger git rollback.
*   Implement `/compact` command to trigger context compaction.
*   Implement `/clear` to start a new session.

#### Step 1.11: Configuration Loader
*   Implement hierarchical config discovery (global → project → `.private-code/`).
*   Parse JSONC config files.
*   Merge configurations with proper priority ordering.
*   Validate config against the schema; warn on unrecognized fields.

#### Step 1.12: Phase 1 Verification Checks
*   Verify workspace startup takes less than 100ms (measured with `std::time::Instant`).
*   Confirm memory footprint constraints (<30MB) are satisfied on empty runs.
*   Verify multi-turn conversation with tool use works end-to-end.
*   Verify session persistence: quit and resume with full history.
*   Verify git checkpoint creation and `/revert` rollback.
*   Run `cargo clippy --all-targets -- -D warnings` with zero warnings.
*   Run `cargo test --workspace` with all tests passing.

---

### Phase 2: Daemon Split

#### Step 2.1: Axum Server Setup (`private-code-daemon`)
*   Initialize `crates/daemon` with `axum` router.
*   Generate a cryptographically secure auth token on startup; store it `0600` in the data dir.
*   Bind to `127.0.0.1` (or Unix Domain Socket on macOS/Linux) with configurable port.
*   Middleware (see `security.md` T5): require the bearer token on **REST, the WS upgrade, AND SSE** (not just REST); **validate `Origin`** against an allowlist and **reject non-loopback `Host`** (defeats DNS-rebinding — WS upgrades are not CORS-preflighted, so loopback binding alone is not a boundary). Request logging via `tracing`. (No "CORS for Tauri" — the desktop GUI's default path is in-process Tauri Channels, not a browser fetch against the daemon.)

#### Step 2.2: REST Route Handlers
*   Implement all REST routes from `specs/api_protocol.md`:
    *   `GET /project`, `POST /project/init`
    *   Session CRUD: create, get, list, delete.
    *   Session actions: abort, compact, revert, unrevert.
    *   Message list and prompt submission.
    *   Permission reply.
    *   File status.
    *   Provider and config listing.

#### Step 2.3: WebSocket Bidirectional Interface
*   Implement WebSocket upgrade handler at `/ws`.
*   Implement event broadcasting: when the orchestrator emits events, fan them out to all connected WebSocket clients for that session.
*   Implement client message routing: parse JSON-RPC requests from WS clients and dispatch to the core engine.
*   Handle multiple simultaneous clients attached to the same session.

#### Step 2.4: SSE Fallback Endpoint
*   Implement `GET /project/:projectID/session/:sessionID/events` as an SSE stream.
*   Use `tokio::sync::broadcast` channels to share events between WS and SSE consumers.

#### Step 2.5: State Persistence & Session Ownership
*   Move SQLite connection pool ownership exclusively to the daemon process.
*   Implement session coordinator: tracks which sessions are actively running, prevents duplicate execution.
*   Implement attach/detach semantics: a client connecting mid-turn receives a replay of all events since the turn started.

#### Step 2.6: Event Replay on Client Attach
*   When a new client connects to an active session, replay:
    *   The current turn's accumulated text deltas.
    *   Any pending permission requests.
    *   Current usage stats.
*   Use a bounded in-memory event log per session (last 1000 events) for replay.

#### Step 2.7: CLI Entrypoint Refactoring
*   Implement `private-code serve` — starts the daemon in the foreground.
*   Implement `private-code tui` — starts the daemon (if not running) and launches the TUI client.
*   Implement `private-code prompt "<text>"` — headless one-shot: sends a prompt, prints the response, exits.
*   Auto-discover running daemon via socket file; start one if absent.

#### Step 2.8: TUI Client-Server Adaptation
*   Refactor TUI to communicate exclusively over WebSocket.
*   Implement robust reconnection with exponential backoff (100ms → 200ms → ... up to 5s).
*   Implement WebSocket ping/pong heartbeat (30s interval).
*   Handle daemon process crash gracefully: show error banner, attempt reconnection.

#### Step 2.9: Phase 2 Verification
*   Monitor WS round-trip latency. Target: < 5ms for local loopback.
*   Verify daemon idle memory < 30MB.
*   Verify attach/detach: start TUI session, close TUI, reopen TUI, see full history.
*   Verify concurrent clients: open two TUI instances, both see the same streaming output.
*   Run `cargo bench` for RPC overhead measurement.

---

### Phase 3: GUI Development

#### Step 3.1: Tauri 2 Workspace Setup
*   Initialize `apps/desktop` via `npx -y @tauri-apps/cli@latest init`.
*   Configure `tauri.conf.json`: window dimensions, title, CSP policy, auto-updater.
*   Link the Tauri Rust backend directly to the `private-code-core` engine (the **in-process** default: the Tauri shell *is* the engine — owns the SQLite pool + agent loop). No bundled second daemon process.
*   Expose the engine via `#[tauri::command]` for request/response and **one `Channel<ProtocolEvent>` per session** for the typed event stream. (Drop "proxy to the daemon's REST/WS" and the browser-`ws://127.0.0.1` path — a secure-context `tauri://localhost` page is blocked from opening an insecure loopback WS, and a second engine would double-count the memory budget.)
*   Support a **loopback-daemon attach** mode (same protocol over WS) for headless/remote/multi-device; the GUI uses it for those cases only.
*   Verify the app launches and renders a blank window.

#### Step 3.2: Solid.js & Vite Project Setup
*   Initialize Solid.js project with Vite in `apps/desktop/src/`.
*   Configure `pnpm` workspace for the frontend.
*   Install dependencies: `solid-js`, **`virtua`** (the `virtua/solid` adapter — variable-height, bottom-anchored timeline; matches the reference), `tailwindcss`, `postcss`, `autoprefixer`.
*   Set up Tailwind CSS with a custom token-based design system.

#### Step 3.3: Design System & Theming
*   Define CSS custom properties (variables) for the token system:
    *   Colors: `--bg-primary`, `--bg-secondary`, `--bg-tertiary`, `--text-primary`, `--text-muted`, `--accent`, `--border`, `--error`, `--success`, `--warning`.
    *   Typography: `--font-mono` (JetBrains Mono or similar), `--font-sans` (Inter).
    *   Spacing: `--space-1` through `--space-12`.
    *   Radii: `--radius-sm`, `--radius-md`, `--radius-lg`.
*   Ship a refined dark theme as default. Define a light theme.
*   Ensure theme tokens match the TUI's named palette for visual consistency across frontends.
*   Implement theme switching via a reactive `createSignal` store.

#### Step 3.4: Core Layout Shell
*   Implement the three-panel responsive layout:
    *   **Left sidebar (collapsible):** Session list, file tree, context sources.
    *   **Center:** Conversation message list (the primary viewport).
    *   **Right panel (collapsible):** Detail panel — diff viewer, model comparison, usage/cost.
    *   **Bottom bar:** Input field, model selector dropdown, agent selector, command palette trigger.
*   Implement keyboard shortcuts: `Ctrl/Cmd-B` toggle left sidebar, `Ctrl/Cmd-E` toggle right panel.
*   Implement `Ctrl/Cmd-K` command palette (searchable list of all commands).

#### Step 3.5: WebSocket Client Layer
*   Implement a reactive event-stream client in Solid.js using `createSignal`/`createStore`. On the **in-process default**, subscribe to the per-session **Tauri `Channel<ProtocolEvent>`** (no socket); the WS client is used only in **loopback-attach** mode.
*   In attach mode, connect to the daemon's WS endpoint with the auth token from the Tauri Rust backend.
*   Parse JSON-RPC events and dispatch to the appropriate stores (message store, permission store, usage store).
*   Handle reconnection with exponential backoff.
*   Implement connection status indicator in the UI (connected / reconnecting / offline).

#### Step 3.6: Virtual Message List
*   Implement `MessageList` using **`virtua`'s `<Virtualizer>`** (`virtua/solid`) — native variable-height rows + bottom-anchoring + `scrollToIndex` + shift-on-prepend (not `@tanstack/solid-virtual`).
*   Dynamically measure row heights (messages vary in size).
*   Implement auto-scroll to bottom on new messages (with "scroll to bottom" button when user has scrolled up).
*   Implement stable keys so completed messages never re-render.
*   Test with 1,000+ messages: verify smooth scrolling at 60 FPS.

#### Step 3.7: Streaming Text Renderer
*   Implement `requestAnimationFrame`-batched rendering:
    1. Buffer incoming `Text` deltas in a queue.
    2. Every frame (16.6ms), flush the queue and append text nodes to the current message.
    3. Never re-parse settled content.
*   Handle `Reasoning` deltas: render in a collapsible "thinking" block.
*   Handle tool call deltas: render tool name and arguments as they stream in.

#### Step 3.8: Incremental Markdown Renderer
*   Implement a streaming-friendly markdown renderer:
    *   Parse markdown incrementally (append-only; never re-parse the whole buffer).
    *   Handle: headings, bold, italic, links, code spans, code blocks, lists, blockquotes.
    *   Code blocks: syntax-highlighted via Shiki in a Web Worker.
*   Implement a `<CodeBlock>` component with language label, line numbers, and copy button.

#### Step 3.9: Syntax Highlighting Worker
*   Create a Web Worker that runs Shiki.
*   Messages: `{code, language}` → `{html}` (pre-tokenized spans).
*   Cache highlighted results by content hash.
*   Highlight only visible code blocks (lazy highlight on scroll into view).

#### Step 3.10: Diff Viewer Component
*   Implement a side-by-side and unified diff viewer for tool outputs.
*   Render file diffs with green/red line highlighting.
*   Implement accept/reject buttons per diff hunk.
*   Implement keyboard navigation: `j/k` to navigate hunks, `a` to accept, `r` to reject.

#### Step 3.11: Permission Dialog
*   Implement a modal/inline prompt for permission requests.
*   Show: tool name, action, resource paths, a summary of what will happen.
*   Buttons: "Allow Once", "Always Allow", "Reject" (with optional feedback text input).
*   Keyboard: `y` = once, `a` = always, `n` = reject.

#### Step 3.12: Usage & Cost Panel
*   Implement a real-time usage dashboard in the right panel:
    *   Input/output/reasoning/cache token counts (current turn + session total).
    *   Estimated cost in USD (current turn + session total).
    *   Active model name and provider.
*   Update reactively as `usage.updated` events arrive.

#### Step 3.13: Session Management UI
*   Left sidebar session list: create, rename, delete, switch sessions.
*   Show session metadata: title, agent, model, cost, last active time.
*   Implement session search/filter.

#### Step 3.14: Input Bar & Commands
*   Implement a rich input bar:
    *   Multi-line text input (grows vertically, max 10 lines before scroll).
    *   File attachment dropzone (drag files to attach as context).
    *   Model selector dropdown (shows available models grouped by provider).
    *   Agent selector (icon + name).
*   Implement slash commands: `/model`, `/agent`, `/revert`, `/compact`, `/clear`, `/help`.
*   Implement `@` mentions for subagent invocation.

#### Step 3.15: Phase 3 Verification
*   1,000-message session: measure frame rate under continuous token streaming. Target: 60 FPS.
*   GUI idle RAM < 150MB.
*   Verify all TUI capabilities are replicated in the GUI.
*   Verify theming: switch between dark and light mode without layout shift.
*   Verify keyboard-only navigation: complete an entire session without touching the mouse.

---

### Phase 4: Moat & Differentiators

#### Step 4.1: Tree-sitter Grammar Integration
*   Add tree-sitter as a dependency in `private-code-codeintel`. **API note (0.26.x line):** `set_language` takes a **borrowed** `Language` and grammar crates export a `LANGUAGE` constant — `parser.set_language(&tree_sitter_rust::LANGUAGE.into())`. Match each grammar crate's MAJOR version to the runtime ABI (ABI mismatch is the common breakage). *(Verify exact versions at build time.)*
*   Bundle grammars for the initial language set: Rust, TypeScript/JavaScript, Python, Go, C/C++, Java, Ruby, PHP. Load each grammar **lazily on first parse of that language** (not all at startup — cold-start budget).
*   Implement `LanguageRegistry` that maps file extensions to grammar + query files.
*   Write tree-sitter queries for each language to extract: functions, methods, structs/classes, interfaces/traits, enums, type aliases, constants.
*   Implement `SymbolExtractor::extract(path, source_code) -> Vec<Symbol>` producing structured symbol metadata (name, kind, start/end line, signature, parent scope).

#### Step 4.2: File Walker & Gitignore Integration
*   Implement a workspace file walker using the `ignore` crate.
*   Respect `.gitignore`, `.ignore`, and `.private-code-ignore` files.
*   Implement configurable max file size limit (default: 1MB) to skip binaries.
*   Return an iterator of `WalkEntry { path, file_type, size }`.

#### Step 4.3: FTS5 Full-Text Search Index
*   Create the `symbols` table and `symbols_fts` virtual table (FTS5) as defined in the schema.
*   Implement sync triggers to keep FTS5 in sync with the base `symbols` table.
*   Implement `index_file(path, symbols)` — inserts/updates symbols for a given file.
*   Implement `remove_file(path)` — clears all symbols for a deleted/moved file.
*   Implement `search(query) -> Vec<SearchResult>` using FTS5 `MATCH` with `bm25()` ranking.
*   Write benchmarks: verify <50ms search over 200k symbols.

#### Step 4.4: Nucleo Fuzzy Matching Layer
*   Integrate the `nucleo` crate (Helix's fuzzy matcher).
*   Implement `fuzzy_search(query, &[Symbol]) -> Vec<(Symbol, Score)>` with configurable result limit.
*   Indexing runs **in the background off the cold-start path** on a rayon pool (bridged to tokio via `spawn_blocking` or a oneshot return), streaming per-file batches over an mpsc channel. Symbols live in **SQLite/FTS5**; do **not** preload a resident `nucleo::Matcher` of all symbols at startup (violates the <30MB idle budget) — build the fuzzy matcher lazily/incrementally for the current query set.
*   Expose both FTS5 (for full-text) and nucleo (for interactive fuzzy) search APIs.

#### Step 4.5: Incremental Re-indexing via File Watcher
*   Hook the file watcher into the indexing pipeline using **`notify-debouncer-full`** (not a hand-rolled `notify` debounce — it stitches split rename From/To events via FS IDs, so file moves don't leave stale index entries).
*   On file change events (create/modify/delete/rename):
    1. The debouncer coalesces rapid saves (configurable window) and resolves renames.
    2. Re-parse the changed file with tree-sitter.
    3. Diff old vs new symbols.
    4. Update the FTS5 index and nucleo matcher incrementally.
*   Verify re-index time < 100ms for a single file change.

#### Step 4.6: Structural Repo Map Generator
*   Implement `RepoMap::generate(workspace_root) -> String` that produces a compact structural overview:
    ```
    src/main.rs
      fn main()
      mod config
    src/config.rs
      struct AppConfig
      impl AppConfig
        fn load() -> Result<Self>
        fn save(&self) -> Result<()>
    ```
*   Limit output size to fit within a configurable token budget (default: 4000 tokens).
*   Use a ranking heuristic to include the most "important" symbols (recently modified, referenced by many files, in the current working directory).
*   Inject the repo map into the system context as a `ContextSource`.

#### Step 4.7: Code Intelligence Context Integration
*   Implement a `ContextSource` that provides relevant code snippets to the model:
    *   When the user mentions a symbol name, retrieve its definition and usage sites.
    *   When a file is being edited, include its full content + related symbols.
*   Implement a retrieval heuristic: extract entity names from the user's prompt and search the index.
*   Cap the total injected context to a configurable token budget.

#### Step 4.8: Multi-Model Orchestration — Configuration
*   Implement the orchestration configuration schema:
    ```json
    {
      "orchestration": {
        "mode": "fan-out",
        "candidates": ["anthropic/claude-opus-4-8", "openai/<model>", "google/<model>"],
        "synthesizer": "anthropic/claude-opus-4-8",
        "roles": {
          "architect": "anthropic/claude-opus-4-8",
          "implementer": "openai/<model>",
          "reviewer": "google/<model>"
        }
      }
    }
    ```
*   Parse and validate the orchestration config.

#### Step 4.9: Multi-Model Orchestration — Fan-Out Engine
*   Implement `Orchestrator::fan_out(prompt, candidates) -> Vec<CandidateResult>`:
    1. Dispatch the same prompt to N models in parallel using `tokio::join!`.
    2. Stream all responses simultaneously.
    3. Collect final outputs and usage stats from each candidate.
*   Implement timeout and error handling: if one candidate fails, proceed with the others.
*   Emit per-candidate streaming events so the GUI can show live progress.

#### Step 4.10: Multi-Model Orchestration — Synthesis Pass
*   Implement `Orchestrator::synthesize(candidates, synthesizer_model) -> SynthesizedResult`:
    1. Build a synthesis prompt containing all candidate outputs.
    2. Call the synthesizer model with instructions to merge, critique, and produce the best output.
    3. Stream the synthesis result.
*   The synthesis prompt template is configurable but ships with a strong default (see Section 8).

#### Step 4.11: Multi-Model Orchestration — Role-Based Routing
*   Implement role-based routing: route subtasks to specific models based on their assigned role.
*   Example workflow: `architect` model designs the approach → `implementer` model writes the code → `reviewer` model reviews and suggests improvements.
*   Implement a pipeline coordinator that chains role-based calls sequentially.

#### Step 4.12: Comparison & Merge UI (GUI)
*   Implement a side-by-side comparison view in the GUI:
    *   Show each candidate's output in its own pane with the model name labeled.
    *   Syntax-highlight code blocks in each pane.
    *   Allow the user to select, diff, or merge outputs.
*   Implement keyboard navigation: `←/→` to switch between candidates, `Enter` to accept, `m` to merge.

#### Step 4.13: Checkpoint History UI
*   Implement a visual checkpoint timeline in the GUI right panel.
*   Show: timestamp, tool name, files changed, commit hash (truncated).
*   One-click revert to any checkpoint.
*   Show the diff between the current state and the selected checkpoint.

#### Step 4.14: Phase 4 Verification
*   Fuzzy search over 200k symbols: target < 50ms to first results.
*   Incremental re-index on file save: target < 100ms.
*   Repo map generation for a 10k-file project: target < 2 seconds.
*   Multi-model fan-out: verify parallel dispatch, verify all candidates complete, verify synthesis output.
*   Checkpoint revert: verify clean rollback to any checkpoint.

---

### Phase 5: Ecosystem & Packaging

#### Step 5.1: LSP Client Integration
*   Implement a generic LSP JSON-RPC client in `private-code-lsp` using `lsp-types` + `async-lsp`.
*   Manage language server lifecycle: spawn, initialize, shutdown.
*   Implement discovery: detect `rust-analyzer`, `typescript-language-server`, `pyright`, `gopls` based on project file types.
*   Implement `textDocument/publishDiagnostics` listener: collect errors/warnings after file saves.
*   Implement `textDocument/definition` and `textDocument/references` for jump-to-definition support.
*   Inject LSP diagnostics into the agent's tool results: after writing a file, automatically check for compiler errors and feed them back.
*   Implement configurable LSP overrides in the config file.

#### Step 5.2: MCP Client Integration
*   Integrate the `rmcp` (Rust MCP) SDK inside `crates/mcp`.
*   Implement MCP server lifecycle management: spawn servers from config, connect via **stdio (child process) or Streamable HTTP** — MCP deprecated HTTP+SSE in favor of Streamable HTTP, and rmcp's transports are stdio + StreamableHttp (the daemon's *own* SSE endpoint is a separate, unrelated thing).
*   Auto-discover tools and resources exposed by connected MCP servers.
*   Register MCP tools into the `ToolRegistry` so they appear as regular tools to the agent.
*   Implement MCP server health checks and graceful reconnection.
*   Support MCP server configuration in the config file:
    ```json
    "mcp": {
      "filesystem": { "command": "npx", "args": ["-y", "@mcp/server-filesystem", "/path"] },
      "github": { "command": "npx", "args": ["-y", "@mcp/server-github"] }
    }
    ```

#### Step 5.3: WASM Plugin Execution Module
*   Implement the Extism runtime wrapper in `crates/plugins/runtime.rs`.
*   Define host function bindings:
    *   `host_read_file(path) -> content` — reads files within workspace boundaries only.
    *   `host_write_file(path, content)` — writes files within workspace boundaries only.
    *   `host_log(level, message)` — structured logging.
    *   `host_get_config(key) -> value` — read plugin-specific config.
*   Implement plugin lifecycle hooks in `crates/plugins/hooks.rs`:
    *   `on_pre_turn` — called before each agent turn; can inject context.
    *   `on_post_turn` — called after each agent turn; can post-process output.
    *   `on_pre_tool_call` — called before each tool execution; can modify arguments or block.
    *   `on_post_tool_call` — called after each tool execution; can modify output.
*   Implement plugin loading from config:
    ```json
    "plugins": [
      { "path": "./plugins/my-plugin.wasm", "config": { "key": "value" } }
    ]
    ```
*   Sandbox enforcement: plugins cannot access the network, cannot read files outside the workspace, and have bounded memory (64MB default).

#### Step 5.4: Provider Breadth Expansion
*   Implement `OpenAIProvider` covering OpenAI, DeepSeek, Groq, and any OpenAI-compatible API.
*   Implement `GoogleProvider` for Gemini API (streaming via server-sent events).
*   Implement `OllamaProvider` for local model support via the Ollama REST API.
*   Implement `LMStudioProvider` using the OpenAI-compatible API.
*   Each provider implements prompt caching where supported (Anthropic beta header, OpenAI system caching).
*   Implement provider auto-detection: scan environment variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`) and local ports (Ollama on 11434, LM Studio on 1234).

#### Step 5.5: Model Metadata Catalog
*   Implement `catalog.rs` as a **standalone, owned** service (NOT a method on the provider trait — a borrowed `ModelInfo` couples catalog data to provider lifetime). It returns **owned** `ModelInfo` via `get/all/available`, nested by provider.
*   Ship a **vendored models.dev-style snapshot** (model id, display name, context window, max output, capabilities, pricing incl. `cache_read_cost` + `cache_write_cost`) so cold start and offline work with **zero network**.
*   Refresh is **lazy/background** (optional fetch from a configurable URL, cached) — **never block startup** on a network call.
*   Selection helpers return owned info: `default()`, `small()`, `cheapest_capable(req)`, `fastest_capable(req)`, with availability gating.

#### Step 5.6: OS Keychain Integration
*   Implement `keyring.rs` using the `keyring` crate to store/retrieve API keys:
    *   macOS: Keychain.
    *   Linux: libsecret / GNOME Keyring.
    *   Windows: Credential Manager.
*   Implement `private-code auth set <provider>` CLI command to securely store keys.
*   Implement `private-code auth list` to show configured providers (no key values).
*   Fallback: if keyring is unavailable, read from environment variables with a warning.

#### Step 5.7: Cost Transparency System
*   Implement per-turn cost calculation: `(input_tokens * input_cost) + (output_tokens * output_cost) + (cache_tokens * cache_cost)`.
*   Implement per-session cost accumulation stored in the `session.cost` column.
*   Implement real-time cost display in both TUI (status bar) and GUI (right panel).
*   Implement per-model cost breakdown for multi-model orchestration sessions.
*   Implement configurable cost warnings: alert the user when session cost exceeds a threshold.

#### Step 5.8: Slash Commands & Custom Commands
*   Implement the built-in slash command system:
    *   `/model <name>` — switch active model.
    *   `/agent <name>` — switch active agent.
    *   `/revert` — rollback to last checkpoint.
    *   `/compact` — trigger context compaction.
    *   `/clear` — start a new session.
    *   `/help` — show available commands.
    *   `/cost` — show session cost breakdown.
    *   `/share` — export session.
*   Implement custom commands from config:
    ```json
    "commands": {
      "test": { "prompt": "Run the test suite and fix any failures." },
      "review": { "prompt": "Review the recent changes and suggest improvements." }
    }
    ```
*   Trigger with `/test`, `/review`, etc.

#### Step 5.9: AGENTS.md Generation (`/init`)
*   Implement the `/init` command that:
    1. Walks the repository.
    2. Generates a tree-sitter repo map.
    3. Sends the repo overview to the model with a prompt: "Generate an AGENTS.md file for this project."
    4. Writes the resulting `AGENTS.md` to the project root.
*   The generated AGENTS.md includes: project overview, architecture notes, coding conventions, testing instructions, and key file paths.

#### Step 5.10: Session Export & Sharing
*   Implement session export as Markdown: dump the full conversation as a formatted `.md` file.
*   Implement session export as JSON: dump raw session data for programmatic access.
*   Implement configurable sharing modes: `manual` (user initiates), `auto` (after each session), `disabled`.

#### Step 5.11: Auto-Update System (GUI)
*   Implement Tauri's built-in updater for the desktop app.
*   Configure update channels: `stable` and `beta`.
*   Implement update notification in the GUI: "A new version is available. Update now?"
*   For the CLI binary: implement `private-code update` command that downloads the latest release.

#### Step 5.12: CLI Binary Packaging
*   Configure `cargo-dist` for cross-platform binary releases:
    *   macOS: `aarch64-apple-darwin`, `x86_64-apple-darwin`.
    *   Linux: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`.
    *   Windows: `x86_64-pc-windows-msvc`.
*   Implement GitHub Actions CI/CD pipeline:
    *   On tag push: build all targets, run tests, publish release artifacts.
    *   Generate checksums and signatures for each artifact.
*   Create installer scripts: `curl -fsSL https://privatecode.dev/install.sh | sh`.

#### Step 5.13: Desktop App Packaging
*   Use Tauri's bundler for native installers:
    *   macOS: `.dmg` (universal binary).
    *   Windows: `.exe` (NSIS) and `.msi`.
    *   Linux: `.deb`, `.rpm`, `.AppImage`.
*   Configure code signing for macOS and Windows.

#### Step 5.14: Package Manager Distribution
*   Publish to Homebrew (macOS), Scoop/Chocolatey (Windows), AUR (Arch Linux).
*   Provide a Nix flake for NixOS users.
*   Automate formula/manifest updates on each release.

#### Step 5.15: Phase 5 Verification
*   LSP integration: verify diagnostics are returned after a file write that introduces an error.
*   MCP integration: verify a connected MCP server's tools are available to the agent.
*   Plugin: verify a simple WASM plugin's `on_pre_turn` hook fires correctly.
*   All providers: verify streaming chat completion works for Anthropic, OpenAI, Google, and Ollama.
*   Packaging: verify the binary installs and runs on macOS, Linux, and Windows.
*   Auto-update: verify the update notification appears when a newer version is available.

## 8. Multi-Model Orchestration Engine

One of the defining features of Private Code is its robust multi-model orchestrator.

### The Fan-Out & Synthesis Pipeline:
```
                [User Prompt: "Write a high-performance HTTP router"]
                                       │
                                       v
         ┌─────────────────────────────┼─────────────────────────────┐
         ▼                             ▼                             ▼
   [Candidate 1:                 [Candidate 2:                 [Candidate N:
   catalog model A]             catalog model B]              catalog model C]
         │                             │                             │
         v                             v                             v
   Candidate 1 Output            Candidate 2 Output            Candidate 3 Output
         │                             │                             │
         └─────────────────────────────┼─────────────────────────────┘
                                       │
                                       v
                     [Synthesis & Critique Prompt Dispatch]
                       - "Synthesize and critique inputs..."
                                       │
                                       v
                         [Final Optimized Output]
```

### Synthesis Coordinator System Prompt (rendered DYNAMICALLY for N candidates):
The template emits **one block per candidate**, tagged with the candidate's model id — not two hardcoded slots (which dropped 3+ candidates and left blanks at 1):

```
You are the Lead Architect. Merge, critique, and synthesize the following N implementation proposals into one cohesive, production-grade output.

{{#each candidates}}
<candidate model="{{this.model_id}}">
{{this.output}}
</candidate>
{{/each}}

Evaluate correctness, safety edge-cases, performance, and structure. Produce the best synthesis, citing which candidate each decision came from.
```

### Cost & failure governance (non-optional for fan-out):
*   **Pre-dispatch budget guard:** before fanning out, estimate `(local_input_estimate × input_price + max_output × output_price)` per candidate **+** the synthesizer, sum, and compare to a configurable per-turn ceiling. If exceeded, pause/prompt or cap the candidate count — fan-out is `(N+1)×` spend and §7.12 promises "no surprises on the bill."
*   **Timeouts + quorum:** wrap each candidate in `tokio::time::timeout` under a total fan-out deadline with a **k-of-N quorum** so a single hung provider can't stall synthesis (the plain `tokio::join!` awaits *all* with no timeout — the stated "proceed with the others" is unreachable as written). Emit per-candidate failure/timeout status to the GUI.

---

## 9. Code Intelligence & Indexing System

To avoid saturating LLM context windows, Private Code maps codebase symbols (classes, functions, types) locally and extracts relevant blocks on demand.

### Indexing Pipeline:
```
  [File Watcher (notify)]
            │
            ▼
    [Check .gitignore]
            │
            ├── (Matches) ──> [Skip File]
            ▼
  [Load file content into memory]
            │
            ▼
  [Parse with Tree-sitter] (Per-language grammar models)
            │
            ▼
  [Extract Symbol Metadata]
  - Name, range (start, end), type (struct, func), signature
            │
            ▼
  [Store in SQLite Database]
  - Full-Text Search (FTS5) index for rapid name matches
  - Relational mapping of symbol dependencies
```

### DB Schema for Symbol Indexes:
**See the authoritative schema in [`specs/database.md §1.H`](specs/database.md).** Two corrections over an earlier draft: (1) `symbols.id` is **`INTEGER PRIMARY KEY`** (a rowid **alias**) so the FTS5 external-content `rowid` is stable across `VACUUM` (a TEXT PK is a non-alias rowid that `VACUUM` can renumber, silently desyncing the index); the logical id is a separate `symbol_uid TEXT UNIQUE`. (2) The FTS5 external-content index needs **all three** sync triggers (`AFTER INSERT`, `AFTER DELETE` with the `'delete'` command, `AFTER UPDATE` = delete-then-insert) — a single `AFTER INSERT` trigger leaves stale rows. These tables ship in a **Phase-4** migration, not `0001`.

---

## 10. Tauri 2 / Solid.js UI Rendering Pipelines

To handle long chat sessions containing massive tool logs and code blocks, the desktop GUI uses an optimized rendering pipeline.

### GUI Virtualization Flow:
```
  [Incoming JSON WebSocket Token Event]
                   │
                   v
  [Buffered updates queued in memory] (16.6ms throttle window)
                   │
                   v
  [Merge & construct delta node properties]
                   │
                   v
    [Request animation frame callback] (requestAnimationFrame)
                   │
                   v
  [Identify visible elements via TanStack Virtualizer]
                   │
                   v
  [Render visible items / Recycle off-screen DOM nodes]
                   │
                   v
  [Syntax Highlight rendered items via Web Worker] (Off-main-thread)
```

### Solid.js Message List component implementation skeleton:

> **Use `virtua` (`virtua/solid`), not `@tanstack/solid-virtual`.** The reference ships **virtua 0.49.1** (`Virtualizer`/`VirtualizerHandle`) in `message-timeline.tsx` for exactly this hot path — native variable-height rows, bottom-anchoring, `scrollToIndex`, and shift-on-prepend (which the agent transcript needs). The skeleton below is illustrative pseudocode; **`useVirtualizer` is the React name and does not exist in the Solid adapter** (`createVirtualizer`), and `estimateSize: () => 120` with a hardcoded `height: virtualRow.size` clips variable-height messages — let **measured** heights drive `translateY`. Port to virtua's `<Virtualizer>` component for production.

```typescript
import { createSignal, createMemo, For, onMount } from "solid-js";
// Reference uses virtua's <Virtualizer> (createVirtualizer); the hook below is tanstack-style pseudocode.
import { useVirtualizer } from "@tanstack/solid-virtual";

interface Message {
  id: string;
  role: "user" | "assistant" | "system" | "tool";
  content: string;
}

export function MessageList(props: { messages: Message[] }) {
  let parentRef: HTMLDivElement | undefined;
  
  const count = () => props.messages.length;
  
  const rowVirtualizer = useVirtualizer({
    get count() { return count(); },
    getScrollElement: () => parentRef,
    estimateSize: () => 120, // estimated height of message node
    overscan: 5,
  });

  return (
    <div
      ref={parentRef}
      class="w-full h-full overflow-y-auto px-4 py-2 bg-neutral-900 scrollbar-thin"
    >
      <div
        class="relative w-full"
        style={{ height: `${rowVirtualizer.getTotalSize()}px` }}
      >
        <For each={rowVirtualizer.getVirtualItems()}>
          {(virtualRow) => {
            const message = () => props.messages[virtualRow.index];
            return (
              <div
                class="absolute top-0 left-0 w-full"
                style={{
                  height: `${virtualRow.size}px`,
                  transform: `translateY(${virtualRow.start}px)`,
                }}
              >
                <MessageItem
                  message={message()}
                  index={virtualRow.index}
                  measureRef={rowVirtualizer.measureElement}
                />
              </div>
            );
          }}
        </For>
      </div>
    </div>
  );
}
```

---

## 11. WASM Plugin Sandbox Specification

To support extensibility without sacrificing safety, Private Code uses `extism` to execute plugins compiled to WebAssembly.

### Host Function Scoping & Execution Boundary:
```
  +-------------------------------------------------------------+
  |                   WASM Plugin Sandbox                       |
  |                                                             |
  |  - Extism runtime executes WebAssembly code modules         |
  |  - Memory sandboxed (cannot access host process address)    |
  |                                                             |
  |                     HOST FUNCTION INTERFACE                 |
  |  - Read workspace file: host_read_file(path)                |
  |  - Write workspace file: host_write_file(path, content)     |
  |  - Logging: host_log(msg)                                   |
  +------------------------------+------------------------------+
                                 |
                        (Restricted access)
                                 v
  +-------------------------------------------------------------+
  |                      Host Environment                       |
  |  - Validates paths within workspace boundaries              |
  |  - Intercepts network calls (denied by default)             |
  +-------------------------------------------------------------+
```

### WASM Hook Declarations:
```rust
// extism 1.x API (the pre-1.0 `extism::Context`, `Function::new`, and
// `Plugin::new(&ctx, bytes, funcs, true)` were ALL removed). Verified against
// extism 1.30.0: build with Manifest + PluginBuilder + the `host_fn!` macro.
// CLOSED BY DEFAULT — with_wasi(false), no allowed_hosts, no allowed_paths;
// every FS capability is a host fn that canonicalizes + verifies containment.
use extism::{host_fn, Manifest, Plugin, PluginBuilder, Wasm};

host_fn!(host_read_file(user_data: PluginCtx; path: String) -> String {
    let ctx = user_data.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.read_within_workspace(&path)        // rejects `..`/symlink escapes
       .map_err(|e| extism::Error::msg(e.to_string()))
});

pub struct PluginInstance { plugin: Plugin }

impl PluginInstance {
    pub fn new(wasm_bytes: &[u8], ctx: PluginCtx) -> Result<Self, extism::Error> {
        let manifest = Manifest::new([Wasm::data(wasm_bytes.to_vec())]);
        // NOTE: no allowed_hosts / allowed_paths => no ambient network or FS.
        let plugin = PluginBuilder::new(manifest)
            .with_wasi(false)
            .with_function("host_read_file", [extism::PTR], [extism::PTR],
                           extism::UserData::new(ctx), host_read_file)
            // …host_write_file / host_log / host_get_config likewise…
            .build()?;
        Ok(Self { plugin })
    }

    pub fn trigger_on_pre_tool_call(&mut self, payload: &str) -> Result<String, extism::Error> {
        Ok(self.plugin.call::<&str, &str>("on_pre_tool_call", payload)?.to_string())
    }
}
```

---

## 12. Verification, Benchmarking & CI/CD Pipeline

To maintain high performance and prevent code degradation over time, Private Code enforces strict resource limits monitored through CI/CD pipelines.

### Testing Strategy:
*   **Recorded-fixture provider harness.** Record real provider SSE once and replay it deterministically in tests (model it on the reference's `packages/http-recorder`). This makes the agent loop, streaming parse, tool-call extraction, and usage accounting testable without live API keys or flakiness.
*   **Snapshot tests** for context/prompt assembly (baseline render, mid-conversation update render, epoch transitions) — the cache prefix is byte-sensitive, so assert on exact bytes.
*   **Property + fuzz tests on the patch/edit applier** (the highest-risk tool): generate file + edit pairs and assert idempotence, exact-match semantics, the staleness guard, and CRLF/BOM preservation; fuzz the "Begin Patch" parser.
*   **Concurrency tests:** the per-session `seq` race (no dropped writes), epoch CAS retry, and the checkpoint non-destructiveness test (see `checkpointing.md §9`).
*   **Integration tests** behind a `--features integration` flag for genuinely live API paths; everything else runs offline via fixtures under `cargo nextest`.

### Performance Budgets (Tracked off-gate — see CI note below):
*   **Startup time:** Checked via system CLI benchmarks (< 100ms cold start).
*   **Memory Footprint:** Checked via heap allocations monitoring and process memory snapshots (< 30MB daemon, < 150MB GUI).
*   **Fuzzy search latency:** Benchmarked using `Criterion` over simulated symbol files index datasets (100k, 200k, 500k symbols) target: `< 50ms`.

### GitHub Actions Workflow:
**CI methodology corrections.** The original `verify.yml` ran on a **single OS** (`macos-latest`, a noisy shared runner), **hard-gated `cargo bench`** (criterion exits 0 on regression, so that gate was non-blocking-by-accident *and* flaky), timed only `--version` (which exits before real init) with **BSD `time -l`** (macOS-only, RSS-only), and had **no supply-chain audit**. Corrected:

- **Quality gate (multi-OS matrix):** `fmt` + `clippy -D warnings` + `cargo nextest run` on `{ubuntu, macos, windows}`.
- **Supply chain (blocking):** `cargo audit` + `cargo deny check` (committed `deny.toml`), `Cargo.lock` verified.
- **Perf is tracked, not PR-gated on wall-clock:** `criterion` micro-benches run **off-gate** on a dedicated/scheduled runner, compared with `critcmp` (alert on regression, don't fail PRs on shared-runner noise). Cold start measured with **`hyperfine --warmup`** against a `--selftest-ready` entrypoint (not `--version`); RSS measured per-OS (not BSD-only). FPS via an rAF-timestamp histogram + dropped-frame counter; WS round-trip via a ping/echo p50/p99 probe.

```yaml
# .github/workflows/verify.yml
name: Verify
on:
  push: { branches: [ main, dev ] }
  pull_request: { branches: [ main, dev ] }

jobs:
  quality:
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { components: rustfmt, clippy }
      - uses: Swatinem/rust-cache@v2
      - uses: taiki-e/install-action@v2
        with: { tool: cargo-nextest }
      - run: cargo fmt --all -- --check
      - run: cargo clippy --all-targets --workspace -- -D warnings
      - run: cargo nextest run --workspace

  supply-chain:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: taiki-e/install-action@v2
        with: { tool: cargo-audit, cargo-deny }
      - run: cargo audit
      - run: cargo deny check
      - run: cargo verify-project    # Cargo.lock present & valid
```

> Perf budgets (cold start, RSS, search latency, FPS) are tracked on a **dedicated** perf job/runner with `hyperfine`/`criterion` + `critcmp`, surfaced as a report — **not** as a flaky PR gate on a shared CI runner.

---

## 13. Open-Core / Pro Boundary

Per North Star §14, Private Code is **open-core** (`MIT OR Apache-2.0`). The boundary must be explicit so Pro features aren't silently built into unmarked core phases:

| Tier | Included |
|---|---|
| **Free / OSS** | The engine, TUI, headless daemon, BYOK + local models, all core tools, sessions/state, permissions + git-backed checkpoints, **FTS5 + nucleo** code intelligence, LSP, MCP, WASM plugins, and **headless multi-model fan-out**. |
| **Pro (paid)** | The polished desktop GUI's advanced surfaces: the **multi-model comparison/merge UI** (Step 4.12), the **checkpoint-history timeline UI** (Step 4.13), **tantivy-backed cross-repo** code intelligence, team/session sync, premium themes, priority updates. |

Steps 4.12 / 4.13 and any `tantivy` cross-repo work are **Pro** and gated behind an offline license check that **never** proxies model calls or adds usage markups (BYOK stays clean — that is part of the pitch). The licensing gate (offline key, channel verification) must be designed before any Pro feature ships (see `REVIEW.md` open risks).

**Custom commands** support `$ARGUMENTS` / named-argument placeholder substitution (North Star §7.11): a config command like `"review": { "prompt": "Review $ARGUMENTS for correctness." }` invoked as `/review src/auth.rs` substitutes the argument before dispatch.
