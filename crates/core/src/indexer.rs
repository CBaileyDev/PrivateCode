//! Workspace indexing glue (Phase 4): walk the tree (codeintel), extract symbols
//! per source file (codeintel), and persist them to the FTS5 index (core
//! `symbols`). Stored filepaths are **relative to the workspace root** (portable,
//! and what the repo map renders).
//!
//! C4 indexes inline (blocking fs + parse on the caller). C5 moves the heavy walk
//! off the cold-start path onto a rayon pool; this function stays the per-file
//! unit it builds on.

use crate::symbols;
use private_code_codeintel::{EntryType, Symbol, SymbolExtractor, WalkOptions, walk};
use rayon::prelude::*;
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
        // Best-effort persistence: a per-file DB error (e.g. a transient
        // SQLITE_BUSY) must NOT abort indexing of every remaining file — log and
        // skip, matching the read/parse skip-and-continue above.
        match symbols::index_file(pool, &rel_str, &syms).await {
            Ok(()) => indexed += 1,
            Err(e) => tracing::warn!("index_workspace: failed to persist {rel_str}: {e}"),
        }
    }
    Ok(indexed)
}

/// Background variant of [`index_workspace`] for the cold-start path: parse files
/// in parallel on the rayon pool and stream per-file symbol batches over a
/// bounded channel to the (serialized) SQLite writer. The CPU-bound walk+parse
/// runs off the async runtime (`spawn_blocking` + rayon); the bounded channel
/// applies backpressure so a fast parser can't outrun the DB writer. One shared
/// `SymbolExtractor` compiles each grammar's query once (its registry is
/// Mutex-guarded, hence `Sync`).
pub async fn index_workspace_background(
    pool: &SqlitePool,
    root: &Path,
) -> Result<usize, sqlx::Error> {
    let root = root.to_path_buf();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, Vec<Symbol>)>(64);

    let producer = tokio::task::spawn_blocking(move || {
        let extractor = SymbolExtractor::new();
        let files: Vec<_> = walk(&root, &WalkOptions::default())
            .into_iter()
            .filter(|e| e.file_type == EntryType::File)
            .collect();
        files.par_iter().for_each(|entry| {
            let rel = entry.path.strip_prefix(&root).unwrap_or(&entry.path);
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            let Ok(source) = std::fs::read_to_string(&entry.path) else {
                return;
            };
            let syms = extractor.extract(&rel_str, &source);
            if !syms.is_empty() {
                // blocking_send is correct here: rayon workers are not tokio
                // runtime threads. Full channel == backpressure, not deadlock.
                let _ = tx.blocking_send((rel_str, syms));
            }
        });
        // `tx` drops here, closing the channel so the consumer loop ends.
    });

    let mut indexed = 0usize;
    while let Some((rel, syms)) = rx.recv().await {
        // Best-effort (see index_workspace): a per-file persistence error skips
        // that file rather than aborting the whole background index.
        match symbols::index_file(pool, &rel, &syms).await {
            Ok(()) => indexed += 1,
            Err(e) => tracing::warn!("index_workspace_background: failed to persist {rel}: {e}"),
        }
    }
    let _ = producer.await;
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

    #[tokio::test]
    async fn background_indexing_produces_the_same_index() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/config.rs"), "pub struct AppConfig {}").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("notes.txt"), "not code").unwrap();

        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        let n = index_workspace_background(&pool, root).await.unwrap();
        assert_eq!(n, 2, "the rayon+mpsc pipeline indexes the same two files");

        let hits = search_symbols(&pool, "AppConfig", 10).await.unwrap();
        assert!(
            hits.iter()
                .any(|h| h.name == "AppConfig" && h.filepath == "src/config.rs"),
            "background-indexed symbols are searchable, got {hits:?}"
        );
    }
}
