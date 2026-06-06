use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ModelConfig {
    pub provider_id: String,
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppConfig {
    pub snapshots: bool,
    pub max_turns: usize,
    pub default_provider: ModelConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            snapshots: true,
            max_turns: 25,
            default_provider: ModelConfig {
                provider_id: "anthropic".to_string(),
                model_id: "claude-3-5-sonnet-latest".to_string(),
                api_key: None,
            },
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
