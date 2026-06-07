//! Manages multiple LSP clients for a workspace.

use crate::client::{Diagnostic, LspClient, LspError};
use crate::discovery::{LanguageServerSpec, discover_servers};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    clients: HashMap<String, LspClient>,
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
                        clients.insert(spec.id.clone(), client);
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

    pub async fn notify_file_written(
        &self,
        path: &Path,
        content: &str,
    ) -> Result<String, LspError> {
        let rel = path.strip_prefix(&self.workspace).unwrap_or(path);
        let lang = self.language_id_for_path(path).unwrap_or("plaintext");
        let server_id = self
            .language_map
            .get(lang)
            .cloned()
            .or_else(|| self.clients.keys().next().cloned());

        let Some(sid) = server_id else {
            return Ok(String::new());
        };
        let Some(client) = self.clients.get(&sid) else {
            return Ok(String::new());
        };

        client.did_open(path, lang, content).await.ok();
        client.did_change(path, 2, content).await?;
        client.did_save(path).await?;
        // Allow diagnostics to arrive.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let diags = client.diagnostics_for(path).await;
        if diags.is_empty() {
            return Ok(String::new());
        }
        Ok(format!(
            "LSP diagnostics for {}:\n{}",
            rel.display(),
            LspClient::format_diagnostics(&diags)
        ))
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
