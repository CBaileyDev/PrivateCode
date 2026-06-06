pub mod file_tools;
pub mod output_store;
pub mod system_tools;
pub mod tool;

pub use file_tools::{EditTool, GlobTool, GrepTool, PatchTool, ReadFileTool, WriteFileTool};
pub use output_store::OutputStore;
pub use system_tools::{BashTool, WebFetchTool};
pub use tool::{Tool, ToolContext, ToolError, ToolRegistry};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let registry = ToolRegistry::new();
        assert!(registry.list_schemas().is_empty());
    }
}
