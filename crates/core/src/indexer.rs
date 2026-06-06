//! Workspace indexing glue (Phase 4): walk the tree (codeintel), extract symbols
//! per source file (codeintel), and persist them to the FTS5 index (core
//! `symbols`). Stored filepaths are **relative to the workspace root** (portable,
//! and what the repo map renders).
//!
//! C4 indexes inline (blocking fs + parse on the caller). C5 moves the heavy walk
//! off the cold-start path onto a rayon pool; this function stays the per-file
//! unit it builds on.

use crate::symbols;
use private_code_codeintel::{EntryType, SymbolExtractor, WalkOptions, walk};
use sqlx::SqlitePool;
use std::path::Path;

/// Index every supported source file under `root`, returning the number of files
/// that contributed symbols. Best-effort: unreadable / non-UTF8 / unsupported
/// files are skipped (extraction yields no symbols). Paths are normalized to
/// forward slashes so the index/repomap are stable across platforms.
pub async fn index_workspace(pool: &SqlitePool, root: &Path) -> Result<usize, sqlx::Error> {
    let extractor = SymbolExtractor::new();
    let mut indexed = 0usize;
    for entry in walk(root, &WalkOptions::default()) {
        if entry.file_type != EntryType::File {
            continue;
        }
        let rel = entry.path.strip_prefix(root).unwrap_or(&entry.path);
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let Ok(source) = std::fs::read_to_string(&entry.path) else {
            continue; // binary / non-UTF8 / unreadable
        };
        let syms = extractor.extract(&rel_str, &source);
        if syms.is_empty() {
            continue; // unsupported language or no top-level symbols
        }
        symbols::index_file(pool, &rel_str, &syms).await?;
        indexed += 1;
    }
    Ok(indexed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect_db, run_migrations};
    use crate::symbols::search_symbols;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn indexes_a_workspace_with_relative_paths() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/config.rs"), "pub struct AppConfig {}").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("notes.txt"), "not code").unwrap();

        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        let n = index_workspace(&pool, root).await.unwrap();
        assert_eq!(n, 2, "two Rust files contribute symbols (txt skipped)");

        let hits = search_symbols(&pool, "AppConfig", 10).await.unwrap();
        let hit = hits.iter().find(|h| h.name == "AppConfig").unwrap();
        assert_eq!(
            hit.filepath, "src/config.rs",
            "filepaths are stored relative to root with forward slashes"
        );
    }
}
