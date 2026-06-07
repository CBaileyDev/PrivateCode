# AGENTS.md

Guidance for coding agents working in this repository.

## Cursor Cloud specific instructions

### One-time Linux system packages (not in the VM update script)

The Tauri desktop crate is a workspace member, so `cargo build --workspace` links against WebKit on Linux. Install once per VM image:

```bash
sudo apt-get install -y libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf libssl-dev pkg-config build-essential
```

`cargo-nextest` is required for Rust tests (see `.github/workflows/ci.yml`). Install once:

```bash
cargo install cargo-nextest --locked
```

Rust toolchain is pinned in `rust-toolchain.toml` (`stable` + `rustfmt` + `clippy`).

### Build order

`apps/desktop/src-tauri` embeds `apps/desktop/dist` at compile time. **Build the frontend before any Cargo step that compiles the desktop crate:**

```bash
cd apps/desktop && npm ci && npm run build
```

### Running services

| Surface | Command | Notes |
|---|---|---|
| Engine smoke (no API key) | `cargo run -p private-code-cli -- selftest` | In-process coordinator, mocked provider |
| Headless daemon | `cargo run -p private-code-cli -- serve --port 48123` | Bearer token at `~/.local/share/private-code/daemon_token` |
| Desktop GUI | `cd apps/desktop && npm run app` | Tauri dev; engine runs in-process (no daemon) |
| TUI | `cargo run -p private-code-cli -- tui --workspace /path/to/repo` | Auto-starts daemon if needed |

Live AI turns require a provider API key (OS keyring or `{PROVIDER}_API_KEY` env) or a local server (Ollama `:11434`, LM Studio `:1234`).

### Lint and test (matches CI)

From repo root:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked
```

Frontend (from `apps/desktop/`):

```bash
npm run typecheck && npm run build && npm run test
```

### Gotchas

- **OpenSSL**: Rust `reqwest`/TLS crates need `libssl-dev` on Ubuntu; build fails with `openssl-sys` errors without it.
- **Frontend-only dev**: `npm run dev` (Vite on `:5173`) previews UI only; full desktop E2E uses `npm run app`.
- **Daemon auth**: REST/WS/SSE require `Authorization: Bearer <token>` from the daemon token file.
- **LSP at startup**: Misconfigured LSP servers can slow daemon boot; optional for most dev work.
