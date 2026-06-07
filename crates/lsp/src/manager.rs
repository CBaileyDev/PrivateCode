//! Manages multiple LSP clients for a workspace.

use crate::client::{Diagnostic, LspClient};
use crate::discovery::{LanguageServerSpec, discover_servers};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LspServerOverride {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LspConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub servers: HashMap<String, LspServerOverride>,
}

pub struct LspManager {
    clients: HashMap<String, Arc<LspClient>>,
    language_map: HashMap<String, String>,
    workspace: PathBuf,
}

impl LspManager {
    pub async fn new(workspace: &Path, config: &LspConfig) -> Self {
        let mut clients = HashMap::new();
        let mut language_map = HashMap::new();

        if config.enabled {
            let mut specs = discover_servers(workspace);
            for (id, ov) in &config.servers {
                specs.retain(|s| s.id != *id);
                specs.push(LanguageServerSpec {
                    id: id.clone(),
                    command: ov.command.clone(),
                    args: ov.args.clone(),
                    language_ids: vec![],
                });
            }

            for spec in specs {
                match LspClient::spawn(&spec.command, &spec.args, workspace).await {
                    Ok(client) => {
                        info!("Started LSP server {}", spec.id);
                        for lang in &spec.language_ids {
                            language_map.insert(lang.clone(), spec.id.clone());
                        }
                        clients.insert(spec.id.clone(), Arc::new(client));
                    }
                    Err(e) => warn!("Failed to start LSP {}: {e}", spec.id),
                }
            }
        }

        Self {
            clients,
            language_map,
            workspace: workspace.to_path_buf(),
        }
    }

    pub fn language_id_for_path(&self, path: &Path) -> Option<&'static str> {
        match path.extension()?.to_str()? {
            "rs" => Some("rust"),
            "ts" | "tsx" => Some("typescript"),
            "js" | "jsx" => Some("javascript"),
            "py" => Some("python"),
            "go" => Some("go"),
            _ => None,
        }
    }

    /// Resolve the LSP client + language id + workspace-relative path for a
    /// written file. Returns a CLONED `Arc<LspClient>` so the caller can run the
    /// `notify_file` + diagnostics wait WITHOUT holding the manager lock across
    /// those awaits (the old code held it across a 200ms sleep, serializing
    /// post-write diagnostics across every session). Returns `None` when no server
    /// handles the file's language — NO arbitrary-server fallback (the old
    /// `clients.keys().next()` would route, say, a `.txt` write to rust-analyzer).
    pub fn resolve_for_path(&self, path: &Path) -> Option<(Arc<LspClient>, &'static str, PathBuf)> {
        let lang = self.language_id_for_path(path)?;
        let server_id = self.language_map.get(lang)?;
        let client = self.clients.get(server_id)?.clone();
        let rel = path
            .strip_prefix(&self.workspace)
            .unwrap_or(path)
            .to_path_buf();
        Some((client, lang, rel))
    }

    pub async fn diagnostics_for(&self, path: &Path) -> Vec<Diagnostic> {
        for client in self.clients.values() {
            let d = client.diagnostics_for(path).await;
            if !d.is_empty() {
                return d;
            }
        }
        Vec::new()
    }
}
