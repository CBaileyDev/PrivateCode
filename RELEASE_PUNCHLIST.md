# Private Code - Release Punch List (v1)

> **Purpose:** current, actionable release-readiness state for the first v1 build.
> This file supersedes the older generated checkbox dump that mixed already-fixed
> findings with still-open release gates.

## Release audit status (updated 2026-06-07)

The repo is substantially release-ready from a code perspective, but v1 should not
be tagged until the remaining human/remote gates are completed:

- Remote CI must be observed green on GitHub Actions after this branch lands.
- `release.yml` must be observed on the first `v*` tag, especially the
  cross-compiled Linux ARM CLI artifact and best-effort desktop bundles.
- macOS notarization, Windows signing, and Tauri updater signing require
  human-provided secrets/certificates.
- A live GUI smoke test and a live BYOK provider smoke test require a real desktop
  session and provider key.
- Homebrew/Scoop/AUR/Nix formulas are deferred until a real release exists to hash.

## Fixed before this pass

- Daemon binds HTTP before LSP/MCP/plugin bootstrap, avoiding startup hangs.
- LSP and MCP framing/response handling are functional and regression-tested.
- Gemini tool round-trips use unique ids and function-response names correctly.
- Gemini and OpenAI-compatible providers compute usage/cost from the model catalog.
- GUI `/init` sends the prompt instead of only adding an optimistic local bubble.
- Session export renders markdown/JSON from real chat content and validates desktop
  export ids before path-joining.
- Path validation normalizes `.`/`..` before workspace boundary checks.
- SQLite DB creation works on first run.
- Provider API keys are not placed in Gemini URLs.
- Checkpoint ordering is stable for latest-revert.
- Daemon auth token comparison is constant-time.
- `auth set` reads keys without terminal echo.
- MCP permissions are scoped per tool and MCP tool calls are checkpointed.
- LSP/MCP child processes use `kill_on_drop`; LSP document versions and URI encoding
  are hardened.
- Plugin WASM sandbox limits are bounded when the Extism feature is enabled, while
  plugin hook execution remains deferred.

## Fixed in this pass

- **Removed a checked-in NVIDIA API key** from `run_nvidia_tests.sh`; the script now
  requires `NVIDIA_API_KEY` from the caller's environment and no longer pulls a
  developer-local path before running.
- **Propagated provider stop reasons** into `ProtocolEvent::MessageCompleted` so UI
  and telemetry can distinguish `length`, context-window truncation, tool-use stops,
  and normal stops on the single-model path.
- **Made `private-code update` fail loudly** on HTTP errors and use a 10s request
  timeout instead of returning success after a failed release check.
- **Reconciled this punchlist** so resolved items are no longer repeated as open.

## Still open before tagging v1

### Required release gates

- [ ] Observe `ci.yml` green on Linux/macOS/Windows after this PR lands.
- [ ] Observe `perf.yml` green after this PR lands.
- [ ] Push a test `v*` tag or dry-run equivalent and verify `release.yml` produces
  CLI artifacts and SHA256 files for all configured targets.
- [ ] Confirm the desktop release job behavior on macOS/Linux/Windows; it is
  `continue-on-error`, so a green workflow alone does not prove bundles exist.
- [ ] Rotate/revoke the NVIDIA key that was previously committed in
  `run_nvidia_tests.sh`; removing it from HEAD does not invalidate a leaked secret.

### Human-only validation

- [ ] Desktop GUI smoke: launch the app, open a folder, configure a provider key,
  run a turn, exercise comparison/checkpoint panels, and force an error banner.
- [ ] Live BYOK smoke: run at least one real end-to-end provider turn.
- [ ] Signing/notarization: configure macOS/Windows certificates and Tauri updater
  signing keys before public desktop distribution.

### Explicit v1 deferrals

- [ ] WASM plugin hooks remain deferred: plugins can load, but orchestrator
  pre/post hooks and host functions are not release-complete.
- [ ] LSP remains rooted at daemon/app launch CWD rather than each session's
  workspace; per-session LSP rooting is deferred.
- [ ] Tauri in-app updater is deferred; CLI `private-code update` is check-only.
- [ ] Package-manager formulas are deferred until release artifact URLs and SHA256s
  exist.

## Release gate to run locally

Run these from a clean checkout after building `apps/desktop/dist`:

```bash
cd apps/desktop && npm ci && npm run build
cd ../..
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked
cargo deny check
cd apps/desktop && npm run typecheck && npm run build && npm run test
```

## Definition of Done for v1

- [ ] Local gate is green.
- [ ] Remote CI/perf workflows are observed green.
- [ ] Release workflow artifacts are inspected for all targets.
- [ ] The leaked NVIDIA key is rotated.
- [ ] Human GUI and live BYOK smoke tests pass.
- [ ] Signing/notarization expectations are either completed or clearly documented
  on the release page as unsigned test builds.
