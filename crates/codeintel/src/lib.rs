//! Code-intelligence engine (Phase 4): tree-sitter symbol extraction, FTS5 +
//! nucleo search, incremental re-indexing, and the structural repo map.
//!
//! A pure-CPU crate: it parses source, fuzzy-ranks, and formats the repo map.
//! The DB-backed symbols index/search lives in `private-code-core` (all SQLite
//! access is centralized there); core depends on this crate for the [`Symbol`]
//! type and the [`SymbolExtractor`].
//!
//! The `spikes` test module keeps the cross-grammar ABI regression guard (Rust
//! 0.24 + TypeScript 0.23 under the 0.26 runtime). FTS5 availability is covered
//! by core's `0002_symbols` migration test.

mod extract;
mod registry;
mod symbol;

pub use extract::SymbolExtractor;
pub use registry::{LangDef, LanguageRegistry};
pub use symbol::Symbol;

#[cfg(test)]
mod spikes {
    use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

    /// Extract the first capture's text for `query_src` over `source` parsed with
    /// `language`. Returns `None` if the language fails to set (ABI mismatch), the
    /// parse fails, or there is no capture.
    fn first_capture(
        language: &tree_sitter::Language,
        source: &str,
        query_src: &str,
    ) -> Option<String> {
        let mut parser = Parser::new();
        parser.set_language(language).ok()?; // Err here == ABI mismatch
        let tree = parser.parse(source, None)?;
        let query = Query::new(language, query_src).ok()?;
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
        // tree-sitter 0.25+ returns a StreamingIterator, not a std Iterator.
        while let Some(m) = matches.next() {
            if let Some(cap) = m.captures.first() {
                return Some(source[cap.node.byte_range()].to_string());
            }
        }
        None
    }

    #[test]
    fn spike_treesitter_rust_loads_parses_and_queries() {
        let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let name = first_capture(
            &lang,
            "fn main() {}",
            "(function_item name: (identifier) @n)",
        );
        assert_eq!(
            name.as_deref(),
            Some("main"),
            "Rust grammar ABI + query must work"
        );
    }

    #[test]
    fn spike_treesitter_typescript_loads_parses_and_queries() {
        // The OLDEST grammar in the set (0.23.2) under the 0.26 runtime — the ABI
        // landmine the design review flagged. tree-sitter-typescript exposes two
        // grammars; use the TypeScript one.
        let lang: tree_sitter::Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        let name = first_capture(
            &lang,
            "function foo() {}",
            "(function_declaration name: (identifier) @n)",
        );
        assert_eq!(
            name.as_deref(),
            Some("foo"),
            "TypeScript 0.23 grammar must be ABI-compatible with the 0.26 runtime"
        );
    }
}
