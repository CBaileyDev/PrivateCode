# Private Code

> **One-line pitch:** A native, instant, local-first AI coding agent — terminal *and* GUI — that beats OpenCode on startup, memory, code-intelligence speed, and long-session smoothness, with first-class multi-model orchestration.

Private Code (formerly codenamed *Forge*) is designed to be a developer's ultimate companion for local and cloud-based AI assistance. Rather than forcing a tradeoff between keyboard-native terminal interfaces and pretty but sluggish GUI applications, Private Code provides a unified **Rust core engine** that powers a terminal TUI, a Tauri 2 desktop GUI, and a headless daemon for editor integration.

---

## 🖼️ UI Preview

> **Desktop GUI** (Tauri 2 + Solid.js) — frontend preview. The shell below is captured running standalone, so the connection indicator reads *Offline*; live session/backend wiring is in active development.

| Welcome screen (dark) | Command palette (⌘K) |
|:---:|:---:|
| ![Private Code desktop GUI — dark welcome screen](docs/images/desktop-welcome-dark.png) | ![Private Code desktop GUI — command palette](docs/images/desktop-command-palette.png) |

---

## ⚡ Key Pillars & Project Angle

### 1. Performance as Correctness
*   **Sub-100ms Cold Start:** Launches instantly without runtime overhead.
*   **Low Memory Footprint:** Idle daemon consumes `< 30 MB` of RAM; the GUI workspace runs on `< 150 MB`.
*   **60 FPS Streaming Render:** Uses virtualization (`Solid.js` / Svelte 5 and `@tanstack/virtual`) and web workers for syntax highlighting so long sessions never lag.

### 2. Local-First & Private by Design
*   **Offline Capability:** First-class support for Ollama, LM Studio, and other local inference servers.
*   **No Intermediaries:** Direct client-to-model communication using your own keys (BYOK). Zero telemetry or proxied model calls.
*   **OS Keychain Storage:** API keys are secured directly via the host OS keyring.

### 3. Unified Architecture: One Brain, Multiple Faces
*   **Stateful Daemon (`axum` + WebSockets):** Session history and agent loops are maintained in a central daemon. Attach or detach from the TUI, GUI, or editor plugins seamlessly without losing context.
*   **Terminal TUI (`ratatui` + `crossterm`):** Keyboard-first, light, fast.
*   **Desktop GUI (`Tauri 2` + `Solid.js`):** Rich side-by-side diff views, model comparisons, and costs visualization.

### 4. High-Performance Code Intelligence (Our Moat)
*   **Instant Fuzzy Search:** Query `200k+` symbols in `< 50ms` using Tree-sitter parsing and a fast local search index.
*   **Structural Repo Mapping:** Feeds a compressed structural context map of your repository to the agent so it can navigate codebases intelligently.
*   **LSP & MCP Integration:** Plugs into language servers for actual diagnostics and imports MCP servers for tool discovery.

### 5. Git-Backed Checkpointing & Safety
*   **No-Fear Automation:** Private Code automatically takes a git snapshot before any file modification.
*   **Single-Key Undo:** Instantly roll back any change with a single keystroke.

---

## 🏗️ Technical Stack

*   **Core Engine:** Rust (`tokio` runtime, `sqlx` + SQLite persistence, `gix` for Git, `tree-sitter` + `nucleo` for code-intel).
*   **TUI:** Rust (`ratatui` + `crossterm`).
*   **GUI:** Tauri v2 (system webview) + Solid.js/Tailwind CSS frontend.
*   **Plugins:** WASM-sandboxed plugins via `extism`.

---

## 📂 Project Structure

```
private-code/
├── Reference/           # [READ-ONLY] Original OpenCode TypeScript codebase for reference.
│                        # DO NOT EDIT THIS DIRECTORY OR SUBFILES.
├── plan.md              # Current phase planning and architecture breakdown.
└── [Implementation Workspace]
```