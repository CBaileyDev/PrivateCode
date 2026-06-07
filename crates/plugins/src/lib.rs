//! WASM plugin runtime via Extism (Phase 5, Step 5.3).

pub mod hooks;
pub mod runtime;

pub use hooks::{HookContext, HookOutcome, PluginHooks, PluginManager};
pub use runtime::{PluginConfig, PluginRuntime, PluginRuntimeError};
