use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TreeHash(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePatch {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileDiff {
    pub path: PathBuf,
    pub status: String, // "added" | "deleted" | "modified"
    pub additions: usize,
    pub deletions: usize,
    pub patch: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("Git error: {0}")]
    Git(#[from] git2::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Other error: {0}")]
    Other(String),
}

#[async_trait]
pub trait Snapshot: Send + Sync {
    async fn track(&self) -> Result<Option<TreeHash>, SnapshotError>;
    async fn restore(&self, snapshot: &TreeHash) -> Result<(), SnapshotError>;
    async fn revert(&self, patches: &[FilePatch]) -> Result<(), SnapshotError>;
    async fn changed_since(&self, snapshot: &TreeHash) -> Result<Vec<PathBuf>, SnapshotError>;
    async fn diff(&self, from: &TreeHash, to: &TreeHash) -> Result<Vec<FileDiff>, SnapshotError>;
    async fn gc(&self) -> Result<(), SnapshotError>;
}

pub struct GitSnapshotEngine {
    pub project_id: String,
    pub workspace_path: PathBuf,
    pub shadow_dir: PathBuf,
    pub enabled: bool,
    lock: Arc<Mutex<()>>,
}

impl GitSnapshotEngine {
    pub fn new(
        project_id: &str,
        workspace_path: &Path,
        global_data_dir: &Path,
        enabled: bool,
    ) -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        workspace_path.hash(&mut hasher);
        let path_hash = format!("{:x}", hasher.finish());

        let shadow_dir = global_data_dir
            .join("snapshot")
            .join(project_id)
            .join(path_hash);

        Self {
            project_id: project_id.to_string(),
            workspace_path: workspace_path.to_path_buf(),
            shadow_dir,
            enabled,
            lock: Arc::new(Mutex::new(())),
        }
    }

    fn ensure_shadow_repo(&self) -> Result<git2::Repository, git2::Error> {
        open_shadow_repo(&self.shadow_dir, &self.workspace_path)
    }

    fn check_vcs_is_git(&self) -> bool {
        self.workspace_path.join(".git").exists()
    }
}

/// Open (or init) the shadow git dir as a **bare** repo whose work-tree is the
/// user's workspace. Bare = no `.git` gitlink is ever written into the user's
/// tree. All snapshot ops point this gitdir at the worktree in memory.
fn open_shadow_repo(shadow_dir: &Path, workspace: &Path) -> Result<git2::Repository, git2::Error> {
    if !shadow_dir.exists() {
        std::fs::create_dir_all(shadow_dir).map_err(|e| {
            git2::Error::from_str(&format!("Failed to create shadow directory: {}", e))
        })?;
    }
    let repo = match git2::Repository::open(shadow_dir) {
        Ok(r) => r,
        Err(_) => {
            let mut opts = git2::RepositoryInitOptions::new();
            opts.bare(true);
            opts.no_reinit(false);
            git2::Repository::init_opts(shadow_dir, &opts)?
        }
    };
    repo.set_workdir(workspace, false)?;
    let mut config = repo.config()?;
    config.set_bool("core.autocrlf", false)?;
    config.set_bool("core.longpaths", true)?;
    config.set_bool("core.symlinks", true)?;
    config.set_bool("core.fsmonitor", false)?;
    config.set_bool("core.quotepath", false)?;
    Ok(repo)
}

const SNAPSHOT_SIZE_LIMIT: u64 = 2_000_000;

/// Should this worktree-relative path be excluded from snapshots? (.git, or > 2 MB).
/// Ignored files are filtered separately by libgit2's `add_all` ignore handling.
fn should_skip_snapshot(workspace: &Path, rel: &Path) -> bool {
    if rel.starts_with(".git") {
        return true;
    }
    if let Ok(meta) = std::fs::metadata(workspace.join(rel))
        && meta.is_file()
        && meta.len() > SNAPSHOT_SIZE_LIMIT
    {
        return true;
    }
    false
}

#[async_trait]
impl Snapshot for GitSnapshotEngine {
    async fn track(&self) -> Result<Option<TreeHash>, SnapshotError> {
        if !self.enabled || !self.check_vcs_is_git() {
            return Ok(None);
        }

        let _guard = self.lock.lock().await;
        let shadow_dir = self.shadow_dir.clone();
        let workspace = self.workspace_path.clone();

        // All blocking git2 + fs work runs off the async executor.
        let hash = tokio::task::spawn_blocking(move || -> Result<String, SnapshotError> {
            let repo = open_shadow_repo(&shadow_dir, &workspace)?;
            let mut index = repo.index()?;

            // Incremental staging: `add_all` uses the persisted index stat-cache to
            // re-hash only CHANGED files (O(changed), not O(repo)), honors the
            // worktree's .gitignore, and stages adds/modifications/deletions like
            // `git add -A`. The callback additionally skips .git and files > 2 MB.
            let ws = workspace.clone();
            let mut cb = |path: &Path, _matched: &[u8]| -> i32 {
                if should_skip_snapshot(&ws, path) {
                    1
                } else {
                    0
                }
            };
            index.add_all(["*"], git2::IndexAddOption::DEFAULT, Some(&mut cb))?;
            index.write()?;
            let oid = index.write_tree()?;
            Ok(oid.to_string())
        })
        .await
        .map_err(|e| SnapshotError::Other(format!("snapshot task join error: {e}")))??;

        Ok(Some(TreeHash(hash)))
    }

    async fn restore(&self, snapshot: &TreeHash) -> Result<(), SnapshotError> {
        if !self.enabled || !self.check_vcs_is_git() {
            return Ok(());
        }

        let _guard = self.lock.lock().await;
        let shadow_dir = self.shadow_dir.clone();
        let workspace = self.workspace_path.clone();
        let snap = snapshot.0.clone();

        tokio::task::spawn_blocking(move || -> Result<(), SnapshotError> {
            let repo = open_shadow_repo(&shadow_dir, &workspace)?;
            let oid = git2::Oid::from_str(&snap)?;
            let tree = repo.find_tree(oid)?;

            // 1. Overwrite tracked files to the snapshot state (read-tree + checkout-index -a -f).
            let mut index = repo.index()?;
            index.read_tree(&tree)?;
            index.write()?;
            let mut co = git2::build::CheckoutBuilder::new();
            co.force();
            repo.checkout_index(Some(&mut index), Some(&mut co))?;

            // 2. Delete files created AFTER the snapshot — but ONLY ones that the
            //    snapshot would have captured (non-ignored, <= 2 MB, not under .git).
            //    Crucially this does NOT use checkout's `remove_untracked`, which would
            //    also delete ignored files (node_modules/target) and > 2 MB assets the
            //    snapshot never tracked. That was a data-loss bug.
            let user_repo = git2::Repository::open(&workspace)?;
            let mut walk = ignore::WalkBuilder::new(&workspace);
            walk.standard_filters(true);
            walk.hidden(false);
            for entry in walk.build().flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let rel = match path.strip_prefix(&workspace) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if should_skip_snapshot(&workspace, rel) {
                    continue;
                }
                if user_repo.status_should_ignore(rel).unwrap_or(false) {
                    continue;
                }
                // Not present in the target tree => created after the snapshot => remove.
                if tree.get_path(rel).is_err() {
                    let _ = std::fs::remove_file(path);
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| SnapshotError::Other(format!("restore task join error: {e}")))??;

        Ok(())
    }

    async fn revert(&self, patches: &[FilePatch]) -> Result<(), SnapshotError> {
        if !self.enabled || !self.check_vcs_is_git() {
            return Ok(());
        }

        let _guard = self.lock.lock().await;
        // In this implementation, a revert is applied relative to the latest snapshot
        // or a specific tree. Let's find the current tree from the index, or just checkout from index.
        let repo = self.ensure_shadow_repo()?;
        let index = repo.index()?;

        for patch in patches {
            let rel_path = if patch.path.is_absolute() {
                match patch.path.strip_prefix(&self.workspace_path) {
                    Ok(p) => p.to_path_buf(),
                    Err(_) => continue,
                }
            } else {
                patch.path.clone()
            };

            let rel_path_str = rel_path.to_string_lossy().into_owned();

            if let Some(entry) = index.get_path(Path::new(&rel_path_str), 0) {
                // File exists in index, restore it
                let object = repo.find_object(entry.id, Some(git2::ObjectType::Blob))?;
                if let Some(blob) = object.as_blob() {
                    let target_path = self.workspace_path.join(&rel_path);
                    if let Some(parent) = target_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&target_path, blob.content())?;
                }
            } else {
                // File does not exist in index, remove from workspace
                let target_path = self.workspace_path.join(&rel_path);
                if target_path.exists() {
                    std::fs::remove_file(&target_path)?;
                }
            }
        }

        Ok(())
    }

    async fn changed_since(&self, snapshot: &TreeHash) -> Result<Vec<PathBuf>, SnapshotError> {
        if !self.enabled || !self.check_vcs_is_git() {
            return Ok(Vec::new());
        }

        let _guard = self.lock.lock().await;

        let repo = self.ensure_shadow_repo()?;
        let old_oid = git2::Oid::from_str(&snapshot.0)?;
        let old_tree = repo.find_tree(old_oid)?;

        let mut diff_opts = git2::DiffOptions::new();
        diff_opts.include_untracked(true);

        let diff =
            repo.diff_tree_to_index(Some(&old_tree), Some(&repo.index()?), Some(&mut diff_opts))?;

        let mut changed_paths = Vec::new();
        diff.foreach(
            &mut |delta, _| {
                if let Some(new_file) = delta.new_file().path() {
                    changed_paths.push(new_file.to_path_buf());
                } else if let Some(old_file) = delta.old_file().path() {
                    changed_paths.push(old_file.to_path_buf());
                }
                true
            },
            None,
            None,
            None,
        )?;

        // Deduplicate and filter out empty paths
        changed_paths.retain(|p| !p.as_os_str().is_empty());
        changed_paths.sort();
        changed_paths.dedup();

        Ok(changed_paths)
    }

    async fn diff(&self, from: &TreeHash, to: &TreeHash) -> Result<Vec<FileDiff>, SnapshotError> {
        if !self.enabled || !self.check_vcs_is_git() {
            return Ok(Vec::new());
        }

        let _guard = self.lock.lock().await;

        let repo = self.ensure_shadow_repo()?;
        let from_oid = git2::Oid::from_str(&from.0)?;
        let to_oid = git2::Oid::from_str(&to.0)?;
        let from_tree = repo.find_tree(from_oid)?;
        let to_tree = repo.find_tree(to_oid)?;

        let mut diff_opts = git2::DiffOptions::new();
        let diff =
            repo.diff_tree_to_tree(Some(&from_tree), Some(&to_tree), Some(&mut diff_opts))?;

        let mut file_diffs: std::collections::HashMap<PathBuf, FileDiff> =
            std::collections::HashMap::new();
        let mut current_patch = String::new();
        let mut last_path: Option<PathBuf> = None;

        diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
            let path = delta.new_file().path().or_else(|| delta.old_file().path());
            if let Some(p) = path {
                let p_buf = p.to_path_buf();
                if last_path.as_ref() != Some(&p_buf) {
                    if let Some(lp) = last_path.take()
                        && let Some(fd) = file_diffs.get_mut(&lp)
                    {
                        fd.patch = std::mem::take(&mut current_patch);
                    }
                    last_path = Some(p_buf.clone());

                    let status = match delta.status() {
                        git2::Delta::Added => "added",
                        git2::Delta::Deleted => "deleted",
                        _ => "modified",
                    };

                    file_diffs.entry(p_buf).or_insert(FileDiff {
                        path: p.to_path_buf(),
                        status: status.to_string(),
                        additions: 0,
                        deletions: 0,
                        patch: String::new(),
                    });
                }

                let origin = line.origin();
                match origin {
                    '+' | '-' | ' ' | 'F' | 'H' | 'B' => {
                        if let Ok(line_str) = std::str::from_utf8(line.content()) {
                            if origin == '+' {
                                if let Some(fd) = file_diffs.get_mut(last_path.as_ref().unwrap()) {
                                    fd.additions += 1;
                                }
                            } else if origin == '-'
                                && let Some(fd) = file_diffs.get_mut(last_path.as_ref().unwrap())
                            {
                                fd.deletions += 1;
                            }
                            current_patch.push(origin);
                            current_patch.push_str(line_str);
                        }
                    }
                    _ => {}
                }
            }
            true
        })?;

        if let Some(lp) = last_path
            && let Some(fd) = file_diffs.get_mut(&lp)
        {
            fd.patch = current_patch;
        }

        Ok(file_diffs.into_values().collect())
    }

    async fn gc(&self) -> Result<(), SnapshotError> {
        if !self.enabled {
            return Ok(());
        }

        let _guard = self.lock.lock().await;

        info!("Pruning shadow git ODB objects older than 7 days...");
        let _ = tokio::process::Command::new("git")
            .arg("--git-dir")
            .arg(&self.shadow_dir)
            .arg("gc")
            .arg("--prune=7.days")
            .output()
            .await;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_git_repo(path: &Path) -> git2::Repository {
        let repo = git2::Repository::init(path).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test User").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();
        repo
    }

    #[tokio::test]
    async fn test_snapshot_workflow() {
        let ws_dir = TempDir::new().unwrap();
        let ws_path = ws_dir.path();
        let data_dir = TempDir::new().unwrap();
        let data_path = data_dir.path();

        // Init workspace as git repo
        let user_repo = init_git_repo(ws_path);

        // Add an initial commit so we have a valid HEAD/index
        let file1 = ws_path.join("file1.txt");
        std::fs::write(&file1, "Hello World\n").unwrap();

        let mut index = user_repo.index().unwrap();
        index.add_path(Path::new("file1.txt")).unwrap();
        index.write().unwrap();
        let oid = index.write_tree().unwrap();
        let signature = user_repo.signature().unwrap();
        let parent_commit = user_repo.find_tree(oid).unwrap();
        user_repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Initial commit",
                &parent_commit,
                &[],
            )
            .unwrap();

        let engine = GitSnapshotEngine::new("test_project", ws_path, data_path, true);

        // 1. First track
        let hash1 = engine.track().await.unwrap().unwrap();

        // 2. Modify files and add untracked
        std::fs::write(&file1, "Hello World modified\n").unwrap();
        let file2 = ws_path.join("file2.txt");
        std::fs::write(&file2, "Uncommitted file\n").unwrap();

        // 3. Track again
        let hash2 = engine.track().await.unwrap().unwrap();
        assert_ne!(hash1, hash2);

        // 4. Verify changed since
        let changed = engine.changed_since(&hash1).await.unwrap();
        assert_eq!(changed.len(), 2);
        assert!(changed.contains(&PathBuf::from("file1.txt")));
        assert!(changed.contains(&PathBuf::from("file2.txt")));

        // 5. Verify diff
        let diffs = engine.diff(&hash1, &hash2).await.unwrap();
        assert_eq!(diffs.len(), 2);

        // 6. Restore back to hash1
        engine.restore(&hash1).await.unwrap();
        assert_eq!(std::fs::read_to_string(&file1).unwrap(), "Hello World\n");
        assert!(!file2.exists()); // file2 did not exist in hash1
    }

    /// The regression test that matters: restore must NEVER delete ignored files
    /// or files > 2 MB (which the snapshot never captured), and must never touch
    /// the user's HEAD/branch.
    #[tokio::test]
    async fn test_restore_preserves_ignored_and_large_files() {
        let ws_dir = TempDir::new().unwrap();
        let ws_path = ws_dir.path();
        let data_dir = TempDir::new().unwrap();
        let data_path = data_dir.path();

        let user_repo = init_git_repo(ws_path);
        std::fs::write(ws_path.join("src.txt"), "v1\n").unwrap();
        std::fs::write(ws_path.join(".gitignore"), "ignored/\n").unwrap();
        let mut index = user_repo.index().unwrap();
        index.add_path(Path::new("src.txt")).unwrap();
        index.add_path(Path::new(".gitignore")).unwrap();
        index.write().unwrap();
        let oid = index.write_tree().unwrap();
        let sig = user_repo.signature().unwrap();
        let tree = user_repo.find_tree(oid).unwrap();
        user_repo
            .commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        let head_before = user_repo.head().unwrap().target().unwrap();

        let engine = GitSnapshotEngine::new("p", ws_path, data_path, true);
        let snap = engine.track().await.unwrap().unwrap();

        // After the snapshot: an ignored dir, a >2MB NON-ignored binary, a modified
        // tracked file, and a small created-after file.
        std::fs::create_dir_all(ws_path.join("ignored")).unwrap();
        std::fs::write(ws_path.join("ignored/junk.txt"), "junk").unwrap();
        std::fs::write(ws_path.join("big.bin"), vec![0u8; 3_000_000]).unwrap();
        std::fs::write(ws_path.join("src.txt"), "v2-modified\n").unwrap();
        std::fs::write(ws_path.join("new.txt"), "created after\n").unwrap();

        engine.restore(&snap).await.unwrap();

        // Tracked file restored.
        assert_eq!(
            std::fs::read_to_string(ws_path.join("src.txt")).unwrap(),
            "v1\n"
        );
        // Ignored files and >2MB files MUST survive (snapshot never captured them).
        assert!(
            ws_path.join("ignored/junk.txt").exists(),
            "ignored files must survive restore"
        );
        assert!(
            ws_path.join("big.bin").exists(),
            ">2MB files must survive restore"
        );
        // Small created-after tracked-able file is reverted (removed).
        assert!(
            !ws_path.join("new.txt").exists(),
            "created-after files are reverted"
        );
        // The user's HEAD/branch is untouched.
        let head_after = user_repo.head().unwrap().target().unwrap();
        assert_eq!(
            head_before, head_after,
            "user HEAD must be unchanged by snapshot/restore"
        );
    }

    /// Proves the two properties the whole engine depends on but the other tests
    /// don't exercise: `add_all(["*"])` must (a) recurse into subdirectories and
    /// (b) stage deletions — otherwise nested files are silently never snapshotted
    /// and deleted files resurrect on restore.
    #[tokio::test]
    async fn test_snapshot_handles_nested_dirs_and_deletions() {
        let ws_dir = TempDir::new().unwrap();
        let ws_path = ws_dir.path();
        let data_dir = TempDir::new().unwrap();
        let data_path = data_dir.path();
        let _user_repo = init_git_repo(ws_path);

        std::fs::create_dir_all(ws_path.join("src/deep")).unwrap();
        std::fs::write(ws_path.join("src/deep/mod.rs"), "fn original() {}\n").unwrap();
        std::fs::write(ws_path.join("top.txt"), "top v1\n").unwrap();

        let engine = GitSnapshotEngine::new("p", ws_path, data_path, true);
        let first = engine.track().await.unwrap().unwrap();

        // Modify a NESTED file, DELETE a top-level file, ADD a new nested file.
        std::fs::write(ws_path.join("src/deep/mod.rs"), "fn changed() {}\n").unwrap();
        std::fs::remove_file(ws_path.join("top.txt")).unwrap();
        std::fs::write(ws_path.join("src/new.rs"), "new\n").unwrap();

        let second = engine.track().await.unwrap().unwrap();
        assert_ne!(
            first, second,
            "nested change/deletion must produce a different tree"
        );

        engine.restore(&first).await.unwrap();

        // (a) Nested file snapshotted + restored to its original content.
        assert_eq!(
            std::fs::read_to_string(ws_path.join("src/deep/mod.rs")).unwrap(),
            "fn original() {}\n",
            "nested file must be snapshotted and restored (add_all must recurse)"
        );
        // (b) Deleted file was captured in `first` and is restored.
        assert!(
            ws_path.join("top.txt").exists(),
            "a file deleted after the snapshot must be restored from it"
        );
        assert_eq!(
            std::fs::read_to_string(ws_path.join("top.txt")).unwrap(),
            "top v1\n"
        );
        // The nested file created after `first` is reverted.
        assert!(
            !ws_path.join("src/new.rs").exists(),
            "created-after nested file must be reverted"
        );
    }
}
