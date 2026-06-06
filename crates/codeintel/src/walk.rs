//! Workspace file walker (Step 4.2). Wraps the `ignore` crate so it respects
//! `.gitignore`, `.ignore`, and a project-specific `.private-code-ignore`, and
//! skips oversized files (binaries) by a configurable byte limit. Yields both
//! files and directories; the indexer filters to [`EntryType::File`].

use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// Default max indexable file size — files larger than this are skipped as
/// presumed-binary / not worth parsing.
pub const DEFAULT_MAX_FILE_SIZE: u64 = 1_000_000; // 1 MB

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryType {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone)]
pub struct WalkEntry {
    pub path: PathBuf,
    pub file_type: EntryType,
    /// File size in bytes (0 for directories / symlinks).
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct WalkOptions {
    pub max_file_size: u64,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            max_file_size: DEFAULT_MAX_FILE_SIZE,
        }
    }
}

/// Walk `root` recursively, honoring ignore files and the size limit.
///
/// The `ignore` crate's standard filters (on by default) skip `.git`/hidden
/// entries and apply `.gitignore` + `.ignore`; `.private-code-ignore` is added as
/// an extra ignore source. Files larger than `options.max_file_size` are omitted
/// entirely (directories and symlinks are always reported, with size 0).
/// Unreadable entries are skipped rather than failing the whole walk.
pub fn walk(root: impl AsRef<Path>, options: &WalkOptions) -> Vec<WalkEntry> {
    let mut builder = WalkBuilder::new(root);
    // Honor .gitignore even when the workspace isn't a git repo (default
    // `require_git` would otherwise ignore .gitignore outside a git tree). .ignore
    // and the custom file below are honored regardless.
    builder.require_git(false);
    builder.add_custom_ignore_filename(".private-code-ignore");

    let mut out = Vec::new();
    for result in builder.build() {
        let Ok(dent) = result else { continue };
        let Some(ft) = dent.file_type() else { continue }; // None == stdin sentinel
        let entry = if ft.is_dir() {
            WalkEntry {
                path: dent.path().to_path_buf(),
                file_type: EntryType::Dir,
                size: 0,
            }
        } else if ft.is_symlink() {
            WalkEntry {
                path: dent.path().to_path_buf(),
                file_type: EntryType::Symlink,
                size: 0,
            }
        } else {
            let size = dent.metadata().map(|m| m.len()).unwrap_or(0);
            if size > options.max_file_size {
                continue; // skip oversize files (presumed binary)
            }
            WalkEntry {
                path: dent.path().to_path_buf(),
                file_type: EntryType::File,
                size,
            }
        };
        out.push(entry);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &Path, rel: &str, contents: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, contents).unwrap();
    }

    fn file_names(entries: &[WalkEntry]) -> Vec<String> {
        entries
            .iter()
            .filter(|e| e.file_type == EntryType::File)
            .filter_map(|e| e.path.file_name().map(|n| n.to_string_lossy().to_string()))
            .collect()
    }

    #[test]
    fn walks_files_and_respects_gitignore() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "src/main.rs", "fn main() {}");
        write(root, "src/lib.rs", "pub fn x() {}");
        write(root, ".gitignore", "ignored.rs\n");
        write(root, "ignored.rs", "fn nope() {}");

        let names = file_names(&walk(root, &WalkOptions::default()));
        assert!(names.contains(&"main.rs".to_string()));
        assert!(names.contains(&"lib.rs".to_string()));
        assert!(
            !names.contains(&"ignored.rs".to_string()),
            ".gitignore'd file must be excluded, got {names:?}"
        );
    }

    #[test]
    fn respects_private_code_ignore() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "keep.rs", "fn k() {}");
        write(root, ".private-code-ignore", "secret.rs\n");
        write(root, "secret.rs", "fn s() {}");

        let names = file_names(&walk(root, &WalkOptions::default()));
        assert!(names.contains(&"keep.rs".to_string()));
        assert!(
            !names.contains(&"secret.rs".to_string()),
            ".private-code-ignore must be honored, got {names:?}"
        );
    }

    #[test]
    fn skips_oversize_files() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "small.rs", "fn s() {}");
        write(root, "big.rs", &"x".repeat(50));

        let entries = walk(root, &WalkOptions { max_file_size: 10 });
        let names = file_names(&entries);
        assert!(names.contains(&"small.rs".to_string()));
        assert!(
            !names.contains(&"big.rs".to_string()),
            "files over the size limit must be skipped, got {names:?}"
        );
    }

    #[test]
    fn reports_size_and_directories() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "src/a.rs", "abcdef");

        let entries = walk(root, &WalkOptions::default());
        let a = entries
            .iter()
            .find(|e| e.path.ends_with("a.rs"))
            .expect("a.rs present");
        assert_eq!(a.file_type, EntryType::File);
        assert_eq!(a.size, 6);
        assert!(
            entries.iter().any(|e| e.file_type == EntryType::Dir),
            "directories are reported too"
        );
    }
}
