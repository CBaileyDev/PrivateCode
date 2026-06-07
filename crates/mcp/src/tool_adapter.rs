//! Wrap MCP tools as native [`Tool`] implementations.

use crate::client::{McpClient, McpToolDef};
use async_trait::async_trait;
use private_code_tools::tool::{Tool, ToolContext, ToolError};
use serde_json::Value;
use std::sync::Arc;

pub struct McpToolAdapter {
    server: Arc<McpClient>,
    def: McpToolDef,
    qualified_name: String,
}

impl McpToolAdapter {
    pub fn new(server: Arc<McpClient>, def: McpToolDef) -> Self {
        let qualified_name = format!("mcp_{}_{}", server.server_name(), def.name);
        Self {
            server,
            def,
            qualified_name,
        }
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn name(&self) -> &str {
        &self.qualified_name
    }

    fn description(&self) -> &str {
        &self.def.description
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "name": self.qualified_name,
            "description": self.def.description,
            "input_schema": self.def.input_schema,
        })
    }

    fn mutates(&self) -> bool {
        false
    }

    fn permission_class(&self) -> &str {
        "mcp"
    }

    async fn run(&self, _ctx: &mut ToolContext<'_>, arguments: Value) -> Result<Value, ToolError> {
        self.server
            .call_tool(&self.def.name, arguments)
            .await
            .map_err(|e| ToolError::Other(e.to_string()))
    }
}
