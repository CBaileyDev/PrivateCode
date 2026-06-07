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

/// Headline feature 5.10: session export renders real conversation content to
/// Markdown and round-trips structured JSON (previously phase5.rs only checked a
/// title substring, so the raw-JSON rendering bug slipped through).
#[tokio::test]
async fn export_renders_markdown_and_round_trips_json() {
    use private_code_core::{db, export};
    use uuid::Uuid;

    let pool = db::connect_db("sqlite::memory:").await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    let pid = Uuid::new_v4().to_string();
    db::create_project(&pool, &pid, "p", "/tmp").await.unwrap();
    let sid = Uuid::new_v4().to_string();
    db::create_session(
        &pool,
        &sid,
        &pid,
        "/tmp",
        "/tmp",
        "Phase5 Export",
        "build",
        r#"{"provider_id":"anthropic","model_id":"claude-opus-4-8"}"#,
    )
    .await
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    db::append_message(
        &mut tx,
        &Uuid::new_v4().to_string(),
        &sid,
        1,
        "user",
        r#"{"id":"u1","role":"user","content":[{"type":"text","text":"export me"}],"created_at":1}"#,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let md = export::export_session_markdown(&pool, &sid).await.unwrap();
    assert!(md.contains("Phase5 Export"), "title rendered");
    assert!(md.contains("export me"), "message content rendered cleanly");
    assert!(
        !md.contains("\"created_at\""),
        "no raw ChatMessage JSON leak"
    );

    let json = export::export_session_json(&pool, &sid).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["session"]["title"], "Phase5 Export");
    assert_eq!(v["messages"].as_array().unwrap().len(), 1);
}

/// Headline feature 5.6: key resolution falls back from the OS keyring to the
/// `{PROVIDER}_API_KEY` env var. A made-up provider name has no keyring entry, so
/// resolution must use the env var; `list_configured_providers` then reports it.
/// (nextest runs each test in its own process, so mutating env here is isolated.)
#[test]
fn keyring_resolves_env_fallback_and_lists_provider() {
    use private_code_providers::keyring_store;

    let name = "phase5kf";
    let var = "PHASE5KF_API_KEY";
    // SAFETY: single-threaded per-process test; no other thread reads env here.
    unsafe { std::env::set_var(var, "sk-phase5-xyz") };

    let key = keyring_store::resolve_key_with_warning(name).unwrap();
    assert_eq!(key, "sk-phase5-xyz");
    assert!(
        keyring_store::list_configured_providers(&[name]).contains(&name.to_string()),
        "a provider with a resolvable env key is listed as configured"
    );

    // SAFETY: same as above.
    unsafe { std::env::remove_var(var) };
}
