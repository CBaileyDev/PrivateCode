//! Incremental re-indexing via a filesystem watcher (Phase 4, Step 4.5).
//!
//! Uses `notify-debouncer-full` (NOT a hand-rolled `notify` debounce): it
//! coalesces rapid saves and, crucially, stitches the split rename From/To events
//! via FS file-ids so a moved file doesn't leave a stale index entry. Debounced
//! create/modify/delete/rename events drive per-file re-indexing — at file
//! granularity, `index_file`'s delete-then-insert IS the incremental update (the
//! FTS5 triggers keep the index in sync).

use crate::symbols;
use notify_debouncer_full::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{
    DebounceEventResult, Debouncer, RecommendedCache, new_debouncer, notify,
};
use private_code_codeintel::SymbolExtractor;
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Debounce window: coalesces a burst of saves into one re-index.
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(200);

/// Re-index a single changed path. If it's an existing supported source file,
/// re-extract and replace its symbols; otherwise (deleted / moved-away /
/// unreadable / now-ignored / no symbols) drop it from the index. This is the
/// per-file unit the watcher drives — deterministic and unit-testable without any
/// filesystem-event timing.
pub async fn reindex_path(pool: &SqlitePool, root: &Path, path: &Path) -> Result<(), sqlx::Error> {
    let rel = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    match std::fs::read_to_string(path) {
        Ok(source) => {
            let syms = SymbolExtractor::new().extract(&rel, &source);
            if syms.is_empty() {
                symbols::remove_file(pool, &rel).await?;
            } else {
                symbols::index_file(pool, &rel, &syms).await?;
            }
        }
        Err(_) => {
            symbols::remove_file(pool, &rel).await?;
        }
    }
    Ok(())
}

type FullDebouncer = Debouncer<RecommendedWatcher, RecommendedCache>;

/// A running workspace watcher. Drop it (or call [`stop`](Self::stop)) to tear
/// down the watch thread and the re-index consumer.
pub struct WorkspaceWatcher {
    _debouncer: FullDebouncer,
    consumer: tokio::task::JoinHandle<()>,
}

impl WorkspaceWatcher {
    pub fn stop(self) {
        self.consumer.abort();
        // `_debouncer` drops here, ending the watch.
    }
}

/// Start watching `root` recursively; debounced filesystem events re-index the
/// affected paths into `pool`. The returned handle owns the watcher and the
/// consumer task and must be kept alive for watching to continue.
pub fn watch_workspace(pool: SqlitePool, root: PathBuf) -> Result<WorkspaceWatcher, notify::Error> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();

    let mut debouncer =
        new_debouncer(DEBOUNCE_WINDOW, None, move |result: DebounceEventResult| {
            if let Ok(events) = result {
                for event in events {
                    for path in &event.paths {
                        let _ = tx.send(path.clone());
                    }
                }
            }
        })?;
    debouncer.watch(&root, RecursiveMode::Recursive)?;

    let consumer = tokio::spawn(async move {
        while let Some(path) = rx.recv().await {
            if let Err(e) = reindex_path(&pool, &root, &path).await {
                tracing::warn!("watcher: reindex of {} failed: {e}", path.display());
            }
        }
    });

    Ok(WorkspaceWatcher {
        _debouncer: debouncer,
        consumer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect_db, run_migrations};
    use crate::symbols::search_symbols;
    use std::fs;
    use tempfile::TempDir;

    async fn fresh_pool() -> SqlitePool {
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn reindex_path_adds_updates_and_removes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let file = root.join("src/a.rs");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let pool = fresh_pool().await;

        // Create -> indexed.
        fs::write(&file, "pub fn alpha() {}").unwrap();
        reindex_path(&pool, root, &file).await.unwrap();
        assert!(!search_symbols(&pool, "alpha", 10).await.unwrap().is_empty());

        // Modify -> replaced.
        fs::write(&file, "pub fn beta() {}").unwrap();
        reindex_path(&pool, root, &file).await.unwrap();
        assert!(search_symbols(&pool, "alpha", 10).await.unwrap().is_empty());
        assert!(!search_symbols(&pool, "beta", 10).await.unwrap().is_empty());

        // Delete -> removed from the index.
        fs::remove_file(&file).unwrap();
        reindex_path(&pool, root, &file).await.unwrap();
        assert!(search_symbols(&pool, "beta", 10).await.unwrap().is_empty());
    }

    /// End-to-end watcher: a real file write, debounced, lands in the index.
    /// Polls with a generous deadline so it isn't flaky on slow CI.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_reindexes_on_file_change() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        fs::create_dir_all(root.join("src")).unwrap();
        let pool = fresh_pool().await;

        let watcher = watch_workspace(pool.clone(), root.clone()).unwrap();

        // Write after the watch is established.
        tokio::time::sleep(Duration::from_millis(100)).await;
        fs::write(root.join("src/widget.rs"), "pub struct WidgetFactory {}").unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut found = false;
        while tokio::time::Instant::now() < deadline {
            if search_symbols(&pool, "WidgetFactory", 10)
                .await
                .unwrap()
                .iter()
                .any(|h| h.name == "WidgetFactory")
            {
                found = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        watcher.stop();
        assert!(found, "the watcher must re-index a newly written file");
    }
}
