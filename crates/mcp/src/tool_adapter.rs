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
        // We can't know what an arbitrary MCP tool does, so treat every call as
        // potentially mutating: the orchestrator takes a checkpoint around it,
        // keeping MCP side effects undoable (the old hardcoded `false` defeated
        // checkpoints for any MCP tool that wrote to the workspace).
        true
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

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(server: &str, tool: &str) -> McpToolAdapter {
        let (client_io, _server_io) = tokio::io::duplex(1024);
        let (r, w) = tokio::io::split(client_io);
        let client = Arc::new(McpClient::with_io(server, w, r, None));
        McpToolAdapter::new(
            client,
            McpToolDef {
                name: tool.to_string(),
                description: "d".into(),
                input_schema: serde_json::json!({"type":"object"}),
            },
        )
    }

    #[tokio::test]
    async fn mcp_tool_is_qualified_mutating_and_gated() {
        let a = adapter("srv", "echo");
        // Qualified name keeps the permission/saved-rule scope per-tool.
        assert_eq!(a.name(), "mcp_srv_echo");
        // Treated as mutating so the orchestrator checkpoints around it (undoable).
        assert!(a.mutates());
        // Gated under the "mcp" permission class (prompts by default).
        assert_eq!(a.permission_class(), "mcp");
    }
}
