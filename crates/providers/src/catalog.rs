//! Standalone model metadata catalog (Phase 5, Step 5.5).
//!
//! Returns **owned** [`ModelInfo`] — never borrowed from a provider — so pricing
//! and capability lookups are decoupled from provider lifetime. Ships a vendored
//! snapshot for zero-network cold start; optional background refresh from a URL.

use private_code_protocol::event::UsageStats;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tracing::{info, warn};

/// Immutable metadata about a model, loaded from the catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelInfo {
    pub provider_id: String,
    pub model_id: String,
    pub display_name: String,
    pub context_window: u32,
    pub max_output: u32,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub input_cost_per_mtok: f64,
    pub output_cost_per_mtok: f64,
    #[serde(default)]
    pub cache_read_cost_per_mtok: f64,
    #[serde(default)]
    pub cache_write_cost_per_mtok: f64,
}

#[derive(Debug, Clone, Default)]
pub struct ModelCatalog {
    models: HashMap<String, HashMap<String, ModelInfo>>,
}

impl ModelCatalog {
    /// Load the embedded vendored snapshot (always succeeds).
    pub fn from_vendored() -> Self {
        let raw: HashMap<String, HashMap<String, ModelEntry>> =
            serde_json::from_str(include_str!("../data/models.json"))
                .expect("vendored models.json must parse");
        Self::from_nested(raw)
    }

    /// Load from a JSON file on disk (for refreshed snapshots).
    pub fn from_file(path: &Path) -> Result<Self, std::io::Error> {
        let content = std::fs::read_to_string(path)?;
        let raw: HashMap<String, HashMap<String, ModelEntry>> =
            serde_json::from_str(&content).map_err(std::io::Error::other)?;
        Ok(Self::from_nested(raw))
    }

    fn from_nested(raw: HashMap<String, HashMap<String, ModelEntry>>) -> Self {
        let mut models = HashMap::new();
        for (provider_id, entries) in raw {
            let mut provider_map = HashMap::new();
            for (model_id, entry) in entries {
                provider_map.insert(
                    model_id.clone(),
                    ModelInfo {
                        provider_id: provider_id.clone(),
                        model_id,
                        display_name: entry.display_name,
                        context_window: entry.context_window,
                        max_output: entry.max_output,
                        capabilities: entry.capabilities,
                        input_cost_per_mtok: entry.input_cost_per_mtok,
                        output_cost_per_mtok: entry.output_cost_per_mtok,
                        cache_read_cost_per_mtok: entry.cache_read_cost_per_mtok,
                        cache_write_cost_per_mtok: entry.cache_write_cost_per_mtok,
                    },
                );
            }
            models.insert(provider_id, provider_map);
        }
        Self { models }
    }

    pub fn get(&self, provider_id: &str, model_id: &str) -> Option<ModelInfo> {
        self.models
            .get(provider_id)
            .and_then(|m| m.get(model_id))
            .cloned()
    }

    pub fn all(&self) -> Vec<ModelInfo> {
        self.models
            .values()
            .flat_map(|m| m.values().cloned())
            .collect()
    }

    /// Models whose provider has a resolvable API key (or is local).
    pub fn available(&self, available_providers: &[&str]) -> Vec<ModelInfo> {
        self.all()
            .into_iter()
            .filter(|m| available_providers.contains(&m.provider_id.as_str()))
            .collect()
    }

    pub fn default_model(&self) -> Option<ModelInfo> {
        self.get("anthropic", "claude-opus-4-8")
            .or_else(|| self.all().into_iter().next())
    }

    pub fn small(&self) -> Option<ModelInfo> {
        self.get("anthropic", "claude-haiku-4-5")
            .or_else(|| self.all().into_iter().min_by_key(|m| m.context_window))
    }

    pub fn cheapest_capable(&self, min_context: u32) -> Option<ModelInfo> {
        self.all()
            .into_iter()
            .filter(|m| m.context_window >= min_context)
            .min_by(|a, b| {
                a.input_cost_per_mtok
                    .partial_cmp(&b.input_cost_per_mtok)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    pub fn fastest_capable(&self, min_context: u32) -> Option<ModelInfo> {
        self.small()
            .filter(|m| m.context_window >= min_context)
            .or_else(|| self.cheapest_capable(min_context))
    }

    /// Compute turn cost from usage stats and catalog pricing.
    pub fn compute_cost(&self, provider_id: &str, model_id: &str, usage: &UsageStats) -> f64 {
        let Some(info) = self.get(provider_id, model_id) else {
            return usage.cost;
        };
        let input = usage.input_tokens as f64 * info.input_cost_per_mtok / 1_000_000.0;
        let output = usage.output_tokens as f64 * info.output_cost_per_mtok / 1_000_000.0;
        let cache_read =
            usage.cache_read_tokens as f64 * info.cache_read_cost_per_mtok / 1_000_000.0;
        let cache_write =
            usage.cache_write_tokens as f64 * info.cache_write_cost_per_mtok / 1_000_000.0;
        input + output + cache_read + cache_write
    }

    pub fn context_window(&self, provider_id: &str, model_id: &str) -> u32 {
        self.get(provider_id, model_id)
            .map(|m| m.context_window)
            .unwrap_or(200_000)
    }

    /// Spawn a background refresh task. Never blocks startup.
    pub fn spawn_background_refresh(cache_path: std::path::PathBuf, url: String) {
        tokio::spawn(async move {
            if let Err(e) = refresh_from_url(&cache_path, &url).await {
                warn!("Model catalog background refresh failed: {e}");
            } else {
                info!("Model catalog refreshed from {url}");
            }
        });
    }
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    display_name: String,
    context_window: u32,
    max_output: u32,
    #[serde(default)]
    capabilities: Vec<String>,
    input_cost_per_mtok: f64,
    output_cost_per_mtok: f64,
    #[serde(default)]
    cache_read_cost_per_mtok: f64,
    #[serde(default)]
    cache_write_cost_per_mtok: f64,
}

async fn refresh_from_url(
    cache_path: &Path,
    url: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let body = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    // Validate before writing.
    let _: HashMap<String, HashMap<String, ModelEntry>> = serde_json::from_str(&body)?;
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(cache_path, body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendored_snapshot_loads() {
        let cat = ModelCatalog::from_vendored();
        let opus = cat.get("anthropic", "claude-opus-4-8").unwrap();
        assert_eq!(opus.display_name, "Claude Opus 4.8");
        assert!(opus.context_window >= 1_000_000);
    }

    #[test]
    fn compute_cost_uses_catalog_pricing() {
        let cat = ModelCatalog::from_vendored();
        let usage = UsageStats {
            input_tokens: 1_000_000,
            output_tokens: 0,
            ..Default::default()
        };
        let cost = cat.compute_cost("anthropic", "claude-opus-4-8", &usage);
        assert!((cost - 5.0).abs() < 0.001);
    }

    #[test]
    fn cheapest_capable_prefers_lower_cost() {
        let cat = ModelCatalog::from_vendored();
        let m = cat.cheapest_capable(100_000).unwrap();
        assert!(m.input_cost_per_mtok <= 5.0);
    }
}
