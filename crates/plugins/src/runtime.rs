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
        // Extism requires the WASM module to export the hook function.
        // When no WASM is present in tests, return input unchanged.
        #[cfg(feature = "extism")]
        {
            return self.call_hook_extism(hook, input);
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
        let wasm = Wasm::file(&self.wasm_path);
        let manifest = Manifest::new([wasm]);
        let mut plugin = Plugin::new(&manifest, [], true)
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
