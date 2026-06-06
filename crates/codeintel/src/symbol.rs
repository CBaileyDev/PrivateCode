//! The structured symbol metadata produced by [`crate::SymbolExtractor`] and
//! stored in the FTS5 index. Field set mirrors `specs/database.md §1.H` (the
//! `symbols` table) so extraction and persistence share one shape.

use serde::{Deserialize, Serialize};

/// One extracted code symbol (function, struct, trait, …).
///
/// Line numbers are **1-based** (human-facing, matching editors and the repo
/// map); column numbers are **0-based** (tree-sitter native). `symbol_uid` is
/// unique per definition site within the workspace, so re-indexing a file
/// (remove_file + index_file) cannot collide same-named siblings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    /// Stable-per-location unique id: `"{filepath}|{start_line}|{start_column}|{name}"`.
    pub symbol_uid: String,
    pub filepath: String,
    pub name: String,
    /// `"function" | "struct" | "trait" | "impl" | "enum" | "type_alias" |
    /// "const" | "module" | "macro" | …` — the query capture tag.
    pub kind: String,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    /// Pragmatic signature: declaration text from the node start up to the body
    /// (`{`/variant list), whitespace-collapsed. Not a compiler-grade signature.
    pub signature: String,
    /// Name of the nearest enclosing `impl`/`trait`/`mod`/`struct`/`enum`, if any
    /// — e.g. a method's `impl` type. `None` for top-level items.
    pub parent_scope: Option<String>,
}

impl Symbol {
    /// Build the location-unique id from a filepath, 1-based line, 0-based column,
    /// and name.
    pub fn make_uid(filepath: &str, start_line: u32, start_column: u32, name: &str) -> String {
        format!("{filepath}|{start_line}|{start_column}|{name}")
    }
}
