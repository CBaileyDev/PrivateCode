use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("Path traversal / out of bounds: {0}")]
    PathOutOfBounds(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("SSRF security block: {0}")]
    SsrfBlocked(String),
    #[error(
        "Staleness error: file has changed since it was read in this turn. Current file content does not match the version read earlier. Please read the file again to obtain the latest state before attempting edits."
    )]
    StaleFile,
    #[error("Edit failed: {0}")]
    EditFailed(String),
    #[error("Command failed: {0}")]
    CommandFailed(String),
    #[error("Other error: {0}")]
    Other(String),
}

use async_trait::async_trait;

pub struct ToolContext<'a> {
    pub workspace_path: &'a Path,
    pub active_dir: &'a Path,
    pub file_read_cache: &'a mut HashMap<PathBuf, String>,
    pub global_data_dir: &'a Path,
    pub max_lines: usize,
    pub max_bytes: usize,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> Value;
    fn mutates(&self) -> bool;
    fn permission_class(&self) -> &str;

    async fn run(
        &self,
        context: &mut ToolContext<'_>,
        arguments: Value,
    ) -> Result<Value, ToolError>;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|b| b.as_ref())
    }

    pub fn list(&self) -> &HashMap<String, Box<dyn Tool>> {
        &self.tools
    }

    pub fn list_schemas(&self) -> Vec<Value> {
        self.tools.values().map(|t| t.schema()).collect()
    }
}
