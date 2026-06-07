//! MCP client integration (Phase 5, Step 5.2).
//!
//! Spawns MCP servers over stdio, discovers tools, and exposes them via
//! [`McpToolAdapter`] for registration in the core [`ToolRegistry`].

pub mod client;
pub mod config;
pub mod tool_adapter;

pub use client::{McpClient, McpError, McpToolDef};
pub use config::McpServerConfig;
pub use tool_adapter::McpToolAdapter;
