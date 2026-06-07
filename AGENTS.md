# AGENTS.md

Guidance for coding agents working in this repository.

## Cursor Cloud specific instructions

### One-time Linux system packages (not in the VM update script)

```bash
sudo apt-get install -y libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf libssl-dev pkg-config build-essential
cargo install cargo-nextest --locked
```

### Build order

Build `apps/desktop/dist` before compiling the Tauri workspace member:

```bash
cd apps/desktop && npm ci && npm run build
```

### Daemon startup

The daemon serves HTTP immediately; LSP/MCP/plugins bootstrap in a background task into shared `RwLock` slots on the coordinator.

### Lint / test

See [README.MD](README.MD#development) and `.github/workflows/ci.yml`.
