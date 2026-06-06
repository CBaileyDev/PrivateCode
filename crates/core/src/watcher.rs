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
use private_code_codeintel::{EntryType, SymbolExtractor, WalkOptions, walk};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Compute a changed path's index key (relative to the workspace root, forward
/// slashes). Canonicalizing the root defeats the macOS `/var → /private/var`
/// symlink trap: FSEvents delivers symlink-RESOLVED paths, so stripping the raw
/// (unresolved) root would fail and leave an absolute, machine-specific key that
/// fragments the index. Try the canonical root first (matches resolved paths),
/// then the raw root (matches as-watched paths on Linux/inotify); only fall back
/// to the absolute path if neither prefixes it.
fn rel_key(canonical_root: &Path, root: &Path, path: &Path) -> String {
    path.strip_prefix(canonical_root)
        .or_else(|_| path.strip_prefix(root))
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Debounce window: coalesces a burst of saves into one re-index.
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(200);

/// Re-index a single changed path. If it's an existing supported source file,
/// re-extract and replace its symbols; otherwise (deleted / moved-away /
/// unreadable / now-ignored / no symbols) drop it from the index. This is the
/// per-file unit the watcher drives — deterministic and unit-testable without any
/// filesystem-event timing.
pub async fn reindex_path(pool: &SqlitePool, root: &Path, path: &Path) -> Result<(), sqlx::Error> {
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let rel = rel_key(&canonical_root, root, path);

    // A created / renamed-IN directory arrives as a single directory event — its
    // child files get no per-file events — so walk it and (re)index each file.
    if path.is_dir() {
        return reindex_dir(pool, &canonical_root, root, path).await;
    }

    match std::fs::read_to_string(path) {
        Ok(source) => {
            let syms = SymbolExtractor::new().extract(&rel, &source);
            if syms.is_empty() {
                symbols::remove_under(pool, &rel).await?;
            } else {
                symbols::index_file(pool, &rel, &syms).await?;
            }
        }
        Err(_) => {
            // The path is gone: a deleted file OR a removed/renamed-AWAY directory
            // (indistinguishable now that it no longer exists). `remove_under`
            // purges both the exact entry and everything stored under it as a
            // directory prefix — an exact-only delete would orphan a renamed
            // directory's child symbols (their inodes move untouched, firing no
            // per-file delete events).
            symbols::remove_under(pool, &rel).await?;
        }
    }
    Ok(())
}

/// Walk a (newly-created or renamed-in) directory and index every supported file
/// under it, keyed relative to the workspace root. Best-effort per file.
async fn reindex_dir(
    pool: &SqlitePool,
    canonical_root: &Path,
    root: &Path,
    dir: &Path,
) -> Result<(), sqlx::Error> {
    let extractor = SymbolExtractor::new();
    for entry in walk(dir, &WalkOptions::default()) {
        if entry.file_type != EntryType::File {
            continue;
        }
        let rel = rel_key(canonical_root, root, &entry.path);
        let Ok(source) = std::fs::read_to_string(&entry.path) else {
            continue;
        };
        let syms = extractor.extract(&rel, &source);
        if !syms.is_empty() {
            symbols::index_file(pool, &rel, &syms).await?;
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

    /// A directory rename/delete delivers only the directory path (no per-file
    /// events). reindex_path on a now-gone directory must purge ALL symbols stored
    /// under it, not just an exact (never-matching) directory-path row.
    #[tokio::test]
    async fn reindex_path_purges_children_when_a_directory_is_removed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let dir = root.join("pkg");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.rs");
        fs::write(&file, "pub fn alpha() {}").unwrap();
        let pool = fresh_pool().await;

        reindex_path(&pool, root, &file).await.unwrap();
        assert!(!search_symbols(&pool, "alpha", 10).await.unwrap().is_empty());

        // Remove the whole directory, then deliver only the DIRECTORY path (what a
        // rename/delete event carries).
        fs::remove_dir_all(&dir).unwrap();
        reindex_path(&pool, root, &dir).await.unwrap();
        assert!(
            search_symbols(&pool, "alpha", 10).await.unwrap().is_empty(),
            "a removed directory must purge its child-file symbols"
        );
    }

    /// A created / renamed-IN directory (its children fire no per-file events) is
    /// walked and its files indexed, keyed relative to the workspace root.
    #[tokio::test]
    async fn reindex_path_indexes_children_of_a_new_directory() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let dir = root.join("pkg/sub");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("b.rs"), "pub struct Widget {}").unwrap();
        let pool = fresh_pool().await;

        // Deliver the directory path (not the file): the walk must find b.rs.
        reindex_path(&pool, root, &root.join("pkg")).await.unwrap();
        let hits = search_symbols(&pool, "Widget", 10).await.unwrap();
        let hit = hits.iter().find(|h| h.name == "Widget").unwrap();
        assert_eq!(
            hit.filepath, "pkg/sub/b.rs",
            "a newly-created directory's files are indexed at workspace-relative paths"
        );
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
