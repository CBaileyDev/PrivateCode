use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tracing::{info, warn};

/// The single default model id. Used by both `AppConfig::default` and the
/// orchestrator's per-turn fallback so they can never diverge. Phase-1 stopgap
/// until the model catalog (Phase 5) is the source of truth; never hardcode a
/// model id elsewhere — resolve from here or the session's `model_config`.
pub const DEFAULT_MODEL_ID: &str = "claude-opus-4-8";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ModelConfig {
    pub provider_id: String,
    pub model_id: String,
    // NOTE: there is deliberately no `api_key` field. Provider keys are resolved
    // ONLY via the OS keyring then a `{PROVIDER}_API_KEY` env fallback
    // (providers::resolve_api_key); per the threat model (security.md T4) a key
    // must never be persisted to a config file. A previous unused `api_key:
    // Option<String>` here was dead surface that — via the published JsonSchema —
    // invited users to write a plaintext key into config.json, the one storage
    // location the threat model forbids. Removed.
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompactionConfig {
    /// Proactively compact before a turn whose estimated tokens would exceed the
    /// model's context window (minus the output reservation and buffer).
    pub auto: bool,
    /// Headroom kept free below the context window when deciding to compact.
    pub buffer_tokens: u32,
    /// Token budget for the most-recent messages retained after compaction
    /// (older messages are folded into a summary).
    pub keep_tokens: u32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            auto: true,
            buffer_tokens: 4_096,
            keep_tokens: 120_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct LspConfigFile {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub servers: HashMap<String, LspServerOverride>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct LspServerOverride {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: HashMap<String, McpServerEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpServerEntry {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PluginEntry {
    pub path: String,
    #[serde(default)]
    pub config: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CustomCommand {
    pub prompt: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SharingMode {
    #[default]
    Disabled,
    Manual,
    Auto,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct SharingConfig {
    #[serde(default)]
    pub mode: SharingMode,
    #[serde(default)]
    pub export_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct CostConfig {
    /// Warn when session cost exceeds this USD threshold (0 = disabled).
    #[serde(default)]
    pub warn_threshold_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppConfig {
    pub snapshots: bool,
    pub max_turns: usize,
    pub default_provider: ModelConfig,
    #[serde(default)]
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub lsp: LspConfigFile,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub plugins: Vec<PluginEntry>,
    #[serde(default)]
    pub commands: HashMap<String, CustomCommand>,
    #[serde(default)]
    pub sharing: SharingConfig,
    #[serde(default)]
    pub cost: CostConfig,
    /// Optional URL to refresh the model catalog in the background.
    #[serde(default)]
    pub catalog_refresh_url: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            snapshots: true,
            max_turns: 25,
            default_provider: ModelConfig {
                provider_id: "anthropic".to_string(),
                model_id: DEFAULT_MODEL_ID.to_string(),
            },
            compaction: CompactionConfig::default(),
            lsp: LspConfigFile::default(),
            mcp: McpConfig::default(),
            plugins: Vec::new(),
            commands: HashMap::new(),
            sharing: SharingConfig::default(),
            cost: CostConfig::default(),
            catalog_refresh_url: None,
        }
    }
}

fn merge_json(a: &mut serde_json::Value, b: serde_json::Value) {
    match (a, b) {
        (serde_json::Value::Object(a_map), serde_json::Value::Object(b_map)) => {
            for (k, v) in b_map {
                merge_json(a_map.entry(k).or_insert(serde_json::Value::Null), v);
            }
        }
        (a, b) => {
            *a = b;
        }
    }
}

pub fn parse_jsonc(content: &str) -> Result<serde_json::Value, serde_json::Error> {
    let clean = json_comments::StripComments::new(content.as_bytes());
    serde_json::from_reader(clean)
}

impl AppConfig {
    pub fn load(global_data_dir: &Path, workspace_path: &Path) -> Self {
        // Start with default
        let default_config = AppConfig::default();
        let mut merged_val = serde_json::to_value(&default_config).unwrap();

        // 1. Try global config
        let global_config_path = global_data_dir.join("config.json");
        if global_config_path.exists()
            && let Ok(content) = std::fs::read_to_string(&global_config_path)
        {
            match parse_jsonc(&content) {
                Ok(val) => {
                    merge_json(&mut merged_val, val);
                    info!("Loaded global config from {:?}", global_config_path);
                }
                Err(e) => {
                    warn!(
                        "Failed to parse global config at {:?}: {}",
                        global_config_path, e
                    );
                }
            }
        }

        // 2. Try project config (.private-code/config.json)
        let project_config_path = workspace_path.join(".private-code").join("config.json");
        if project_config_path.exists()
            && let Ok(content) = std::fs::read_to_string(&project_config_path)
        {
            match parse_jsonc(&content) {
                Ok(val) => {
                    merge_json(&mut merged_val, val);
                    info!("Loaded project config from {:?}", project_config_path);
                }
                Err(e) => {
                    warn!(
                        "Failed to parse project config at {:?}: {}",
                        project_config_path, e
                    );
                }
            }
        }

        // Deserialize final merged value
        match serde_json::from_value::<AppConfig>(merged_val) {
            Ok(config) => config,
            Err(e) => {
                warn!(
                    "Merged config failed to validate: {}. Falling back to default.",
                    e
                );
                default_config
            }
        }
    }

    pub fn generate_schema() -> String {
        let schema = schemars::schema_for!(AppConfig);
        serde_json::to_string_pretty(&schema).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_jsonc_parsing_with_comments() {
        let jsonc_text = r#"
        {
            // This is a comment
            "snapshots": false, /* block comment */
            "max_turns": 10
        }
        "#;
        let parsed = parse_jsonc(jsonc_text).unwrap();
        assert_eq!(parsed["snapshots"].as_bool(), Some(false));
        assert_eq!(parsed["max_turns"].as_u64(), Some(10));
    }

    #[test]
    fn test_hierarchical_merge() {
        let global_dir = TempDir::new().unwrap();
        let ws_dir = TempDir::new().unwrap();

        // Write global config override
        let global_config_path = global_dir.path().join("config.json");
        std::fs::write(&global_config_path, r#"{"max_turns": 15}"#).unwrap();

        // Write project config override
        let pc_dir = ws_dir.path().join(".private-code");
        std::fs::create_dir_all(&pc_dir).unwrap();
        let project_config_path = pc_dir.join("config.json");
        std::fs::write(&project_config_path, r#"{"snapshots": false}"#).unwrap();

        let config = AppConfig::load(global_dir.path(), ws_dir.path());

        assert_eq!(config.max_turns, 15);
        assert!(!config.snapshots);
        assert_eq!(config.default_provider.provider_id, "anthropic"); // inherited from default
    }

    #[test]
    fn test_schema_generation() {
        let schema = AppConfig::generate_schema();
        assert!(schema.contains("\"title\": \"AppConfig\""));
    }
}
