//! Ecosystem services: LSP, MCP tools, WASM plugins (Phase 5).

use private_code_lsp::{LspConfig, LspManager, LspServerOverride};
use private_code_mcp::{McpClient, McpToolAdapter};
use private_code_plugins::{PluginConfig, PluginManager};
use private_code_tools::ToolRegistry;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::AppConfig;

pub struct Ecosystem {
    pub lsp: Arc<Mutex<LspManager>>,
    pub plugins: PluginManager,
}

impl Ecosystem {
    pub async fn bootstrap(
        workspace: &Path,
        global_data_dir: &Path,
        config: &AppConfig,
        tool_registry: &mut ToolRegistry,
    ) -> Self {
        let lsp_cfg = LspConfig {
            enabled: config.lsp.enabled,
            servers: config
                .lsp
                .servers
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        LspServerOverride {
                            command: v.command.clone(),
                            args: v.args.clone(),
                        },
                    )
                })
                .collect(),
        };
        let lsp = Arc::new(Mutex::new(LspManager::new(workspace, &lsp_cfg).await));

        let plugin_cfgs: Vec<PluginConfig> = config
            .plugins
            .iter()
            .map(|p| PluginConfig {
                path: resolve_plugin_path(workspace, global_data_dir, &p.path),
                config: p.config.clone(),
            })
            .collect();
        let plugins = PluginManager::load_all(workspace.to_path_buf(), &plugin_cfgs);

        register_mcp_tools(&config.mcp, tool_registry).await;

        if config.catalog_refresh_url.is_some() {
            let cache = global_data_dir.join("models.json");
            if let Some(url) = &config.catalog_refresh_url {
                private_code_providers::ModelCatalog::spawn_background_refresh(cache, url.clone());
            }
        }

        Self { lsp, plugins }
    }

    pub async fn lsp_after_write(&self, path: &Path, content: &str) -> String {
        self.lsp
            .lock()
            .await
            .notify_file_written(path, content)
            .await
            .unwrap_or_default()
    }
}

async fn register_mcp_tools(mcp_cfg: &crate::config::McpConfig, registry: &mut ToolRegistry) {
    for (name, cfg) in &mcp_cfg.servers {
        if cfg.url.is_some() {
            info!("MCP server {name}: HTTP transport deferred to runtime config");
            continue;
        }
        let mcp_cfg = private_code_mcp::McpServerConfig {
            command: cfg.command.clone(),
            args: cfg.args.clone(),
            env: cfg.env.clone(),
            url: cfg.url.clone(),
        };
        match McpClient::connect_stdio(name, &mcp_cfg).await {
            Ok(client) => {
                let client = Arc::new(client);
                match client.list_tools().await {
                    Ok(tools) => {
                        for def in tools {
                            registry.register(Box::new(McpToolAdapter::new(client.clone(), def)));
                        }
                        info!("Registered MCP tools from server {name}");
                    }
                    Err(e) => warn!("MCP {name} tools/list: {e}"),
                }
            }
            Err(e) => warn!("MCP server {name} failed to connect: {e}"),
        }
    }
}

fn resolve_plugin_path(workspace: &Path, global_data_dir: &Path, path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else if path.starts_with("./") || path.starts_with(".\\") {
        workspace.join(p)
    } else {
        global_data_dir.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_relative_plugin_path() {
        let ws = PathBuf::from("/workspace");
        let gd = PathBuf::from("/data");
        let p = resolve_plugin_path(&ws, &gd, "./plugins/foo.wasm");
        assert_eq!(p, PathBuf::from("/workspace/plugins/foo.wasm"));
    }
}
