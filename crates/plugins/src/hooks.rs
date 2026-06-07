//! Plugin lifecycle hooks.

use crate::runtime::{PluginConfig, PluginRuntime};
use std::path::PathBuf;
use tracing::{info, warn};

#[derive(Debug, Clone, Default)]
pub struct HookContext {
    pub session_id: String,
    pub user_prompt: String,
    pub tool_name: Option<String>,
    pub tool_args: Option<String>,
    pub tool_output: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct HookOutcome {
    pub injected_context: Option<String>,
    pub modified_output: Option<String>,
    pub block: bool,
}

pub struct PluginHooks {
    runtime: PluginRuntime,
}

impl PluginHooks {
    pub fn new(runtime: PluginRuntime) -> Self {
        Self { runtime }
    }

    pub fn on_pre_turn(&self, ctx: &HookContext) -> HookOutcome {
        match self.runtime.call_hook("on_pre_turn", &ctx.user_prompt) {
            Ok(text) if !text.is_empty() => HookOutcome {
                injected_context: Some(text),
                ..Default::default()
            },
            Ok(_) => HookOutcome::default(),
            Err(e) => {
                warn!("plugin pre_turn hook: {e}");
                HookOutcome::default()
            }
        }
    }

    pub fn on_post_turn(&self, _ctx: &HookContext, output: &str) -> String {
        match self.runtime.call_hook("on_post_turn", output) {
            Ok(text) if !text.is_empty() => text,
            _ => output.to_string(),
        }
    }

    pub fn on_pre_tool_call(&self, ctx: &HookContext) -> HookOutcome {
        let input = ctx.tool_args.clone().unwrap_or_default();
        match self.runtime.call_hook("on_pre_tool_call", &input) {
            Ok(text) if text == "block" => HookOutcome {
                block: true,
                ..Default::default()
            },
            _ => HookOutcome::default(),
        }
    }

    pub fn on_post_tool_call(&self, _ctx: &HookContext, output: &str) -> String {
        match self.runtime.call_hook("on_post_tool_call", output) {
            Ok(text) if !text.is_empty() => text,
            _ => output.to_string(),
        }
    }
}

pub struct PluginManager {
    hooks: Vec<PluginHooks>,
}

impl PluginManager {
    pub fn load_all(workspace: PathBuf, configs: &[PluginConfig]) -> Self {
        let mut hooks = Vec::new();
        for (i, cfg) in configs.iter().enumerate() {
            match PluginRuntime::load(format!("plugin-{i}"), cfg, workspace.clone()) {
                Ok(rt) => {
                    info!("Loaded plugin {}", cfg.path.display());
                    hooks.push(PluginHooks::new(rt));
                }
                Err(e) => warn!("Skipping plugin {}: {e}", cfg.path.display()),
            }
        }
        Self { hooks }
    }

    pub fn pre_turn(&self, ctx: &HookContext) -> HookOutcome {
        let mut merged = HookOutcome::default();
        for h in &self.hooks {
            let o = h.on_pre_turn(ctx);
            if o.injected_context.is_some() {
                merged.injected_context = o.injected_context;
            }
            if o.block {
                merged.block = true;
            }
        }
        merged
    }

    pub fn post_turn(&self, ctx: &HookContext, output: &str) -> String {
        self.hooks
            .iter()
            .fold(output.to_string(), |acc, h| h.on_post_turn(ctx, &acc))
    }

    pub fn pre_tool(&self, ctx: &HookContext) -> HookOutcome {
        self.hooks
            .iter()
            .map(|h| h.on_pre_tool_call(ctx))
            .find(|o| o.block)
            .unwrap_or_default()
    }

    pub fn post_tool(&self, ctx: &HookContext, output: &str) -> String {
        self.hooks
            .iter()
            .fold(output.to_string(), |acc, h| h.on_post_tool_call(ctx, &acc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn empty_manager_is_noop() {
        let mgr = PluginManager { hooks: vec![] };
        let ctx = HookContext {
            user_prompt: "hi".into(),
            ..Default::default()
        };
        assert!(!mgr.pre_turn(&ctx).block);
        assert_eq!(mgr.post_turn(&ctx, "out"), "out");
    }

    #[test]
    fn load_missing_plugin_is_skipped() {
        let mgr = PluginManager::load_all(
            PathBuf::from("/tmp"),
            &[PluginConfig {
                path: PathBuf::from("/nonexistent/plugin.wasm"),
                config: Default::default(),
            }],
        );
        assert!(mgr.hooks.is_empty());
    }
}
