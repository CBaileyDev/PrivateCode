//! Generic LSP JSON-RPC client (Phase 5, Step 5.1).
//!
//! Manages language-server lifecycle over stdio and collects diagnostics.

pub mod client;
pub mod discovery;
pub mod manager;

pub use client::{Diagnostic, LspClient, LspError, Position};
pub use discovery::{LanguageServerSpec, discover_servers};
pub use manager::{LspConfig, LspManager, LspServerOverride};
