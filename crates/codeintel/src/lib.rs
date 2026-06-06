//! Code-intelligence engine (Phase 4): tree-sitter symbol extraction, FTS5 +
//! nucleo search, incremental re-indexing, and the structural repo map.
//!
//! This module currently contains only the two foundation **spikes** the design
//! review gated the rest of Phase 4 on (real `SymbolExtractor` /
//! `LanguageRegistry` / index layers land in P4-C1+):
//!
//! - tree-sitter loads + parses + queries Rust AND TypeScript under one 0.26
//!   runtime (ABI compatibility across grammar-crate versions).
//! - FTS5 is compiled into the sqlx-bundled SQLite (`CREATE VIRTUAL TABLE …
//!   USING fts5` applies).

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

    /// FTS5 must be compiled into the sqlx-bundled SQLite, else `CREATE VIRTUAL
    /// TABLE … USING fts5` fails with "no such module: fts5" at migration time.
    #[tokio::test]
    async fn spike_fts5_is_available_in_sqlx_sqlite() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE VIRTUAL TABLE t USING fts5(name, signature, filepath)")
            .execute(&pool)
            .await
            .expect("FTS5 virtual table must create (FTS5 compiled into sqlx SQLite)");
        sqlx::query(
            "INSERT INTO t(name, signature, filepath) VALUES ('main', 'fn main()', 'src/main.rs')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let hit: (String,) = sqlx::query_as("SELECT name FROM t WHERE t MATCH ?1")
            .bind("main")
            .fetch_one(&pool)
            .await
            .expect("FTS5 MATCH must return the inserted row");
        assert_eq!(hit.0, "main");
    }
}
