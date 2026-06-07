//! Phase 5 verification tests (Step 5.15).

use private_code_plugins::{HookContext, PluginConfig, PluginManager};
use private_code_providers::catalog::ModelCatalog;
use std::path::PathBuf;

#[test]
fn catalog_vendored_snapshot_offline() {
    let cat = ModelCatalog::from_vendored();
    assert!(cat.get("anthropic", "claude-opus-4-8").is_some());
    assert!(cat.get("google", "gemini-2.5-flash").is_some());
}

#[test]
fn plugin_manager_pre_turn_noop_without_plugins() {
    let mgr = PluginManager::load_all(PathBuf::from("/tmp"), &[]);
    let ctx = HookContext {
        user_prompt: "hello".into(),
        ..Default::default()
    };
    assert!(!mgr.pre_turn(&ctx).block);
}

#[test]
fn plugin_load_skips_missing_wasm() {
    let mgr = PluginManager::load_all(
        PathBuf::from("/tmp"),
        &[PluginConfig {
            path: PathBuf::from("/nonexistent/plugin.wasm"),
            config: Default::default(),
        }],
    );
    let ctx = HookContext::default();
    assert!(!mgr.pre_turn(&ctx).block);
}

#[test]
fn lsp_formats_diagnostics() {
    use private_code_lsp::Diagnostic;
    let s = private_code_lsp::LspClient::format_diagnostics(&[Diagnostic {
        message: "type mismatch".into(),
        severity: 1,
        line: 0,
        character: 0,
    }]);
    assert!(s.contains("ERROR"));
}
