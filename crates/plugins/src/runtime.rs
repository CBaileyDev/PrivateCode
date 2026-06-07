use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginConfig {
    pub path: PathBuf,
    #[serde(default)]
    pub config: HashMap<String, String>,
}

#[derive(Debug, thiserror::Error)]
pub enum PluginRuntimeError {
    #[error("load failed: {0}")]
    Load(String),
    #[error("call failed: {0}")]
    Call(String),
    #[error("{0}")]
    Other(String),
}

/// Extism runtime wrapper with workspace-bounded host functions.
pub struct PluginRuntime {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    wasm_path: PathBuf,
    plugin_config: HashMap<String, String>,
    workspace: PathBuf,
}

impl PluginRuntime {
    pub fn load(
        name: impl Into<String>,
        cfg: &PluginConfig,
        workspace: PathBuf,
    ) -> Result<Self, PluginRuntimeError> {
        let wasm_path = cfg.path.clone();
        if !wasm_path.exists() {
            return Err(PluginRuntimeError::Load(format!(
                "plugin not found: {}",
                wasm_path.display()
            )));
        }
        Ok(Self {
            name: name.into(),
            wasm_path,
            plugin_config: cfg.config.clone(),
            workspace,
        })
    }

    pub fn call_hook(&self, hook: &str, input: &str) -> Result<String, PluginRuntimeError> {
        // With the `extism` feature, run the WASM hook; without it (the default and
        // the v1 build), plugins are a no-op — exactly one cfg block compiles, so
        // each is the function's tail expression.
        #[cfg(feature = "extism")]
        {
            self.call_hook_extism(hook, input)
        }
        #[cfg(not(feature = "extism"))]
        {
            let _ = (hook, input);
            Ok(String::new())
        }
    }

    #[cfg(feature = "extism")]
    fn call_hook_extism(&self, hook: &str, input: &str) -> Result<String, PluginRuntimeError> {
        use extism::{Manifest, Plugin, Wasm};
        use std::time::Duration;
        let wasm = Wasm::file(&self.wasm_path);
        // Bound the sandbox per plan 5.3: 64 MiB (1024 × 64 KiB pages) and a 5s
        // execution timeout, with WASI CLOSED by default (the 3rd `Plugin::new`
        // arg). NOTE (5.3 deferral): host functions are still NOT registered, so a
        // plugin cannot reach the filesystem — wiring `with_function` host fns
        // (workspace-bounded) is the remaining work to un-defer plugins for v1.
        let manifest = Manifest::new([wasm])
            .with_memory_max(1024)
            .with_timeout(Duration::from_secs(5));
        let mut plugin = Plugin::new(&manifest, [], false)
            .map_err(|e| PluginRuntimeError::Load(e.to_string()))?;
        let out: String = plugin
            .call(hook, input)
            .map_err(|e| PluginRuntimeError::Call(e.to_string()))?;
        Ok(out)
    }

    pub fn read_file_host(&self, path: &str) -> Result<String, PluginRuntimeError> {
        let p = self.workspace.join(path);
        let canon = p
            .canonicalize()
            .map_err(|e| PluginRuntimeError::Other(e.to_string()))?;
        let root = self
            .workspace
            .canonicalize()
            .map_err(|e| PluginRuntimeError::Other(e.to_string()))?;
        if !canon.starts_with(&root) {
            return Err(PluginRuntimeError::Other(format!(
                "path outside workspace: {path}"
            )));
        }
        std::fs::read_to_string(canon).map_err(|e| PluginRuntimeError::Other(e.to_string()))
    }

    pub fn get_config(&self, key: &str) -> Option<String> {
        self.plugin_config.get(key).cloned()
    }
}
