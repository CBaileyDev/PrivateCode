# Checkpointing & Snapshot Engine

> **Status:** Normative spec (Phase 1). Supersedes the checkpoint description in `plan.md §6` / Step 1.7.
> **Why this exists:** The original plan proposed *"create a temporary staging commit on the active branch"* and *"git reset --hard HEAD"*. That design is **both destructive and a no-op**: it rewrites the user's branch tip / reflog / index (corrupting an in-progress rebase, merge, or cherry-pick), and `reset --hard HEAD` targets `HEAD` — **not the snapshot** — so it discards uncommitted work while failing to actually revert. This spec restores OpenCode's proven design (`Reference/packages/opencode/src/snapshot/index.ts`): a **shadow git directory** that snapshots the worktree with `write-tree` (a bare tree object — no commit, no branch, no HEAD, no stash) and restores with `read-tree` + `checkout-index`.

---

## 1. Invariants (non-negotiable)

The checkpoint engine **MUST NEVER**:

1. Touch the user's repository `HEAD`, branch refs, tags, reflog, or stash.
2. Write to the user's `.git/index`, `.git/objects`, or any user-repo ref.
3. Create commits, branches, or stashes visible to `git log` / `git status`.
4. Capture ignored files, files > 2 MB, or anything excluded by the source repo's ignore rules.

All snapshot state lives in a **separate git directory** the user never sees. The user's repository is used only as a *worktree* the shadow repo reads from and writes to.

---

## 2. Storage layout

```
$DATA_DIR/snapshot/<project_id>/<fast_hash(worktree_path)>/   # the shadow git dir
```

- `$DATA_DIR` is the platform data dir (`~/.local/share/private-code` / `~/Library/Application Support/private-code` / `%APPDATA%\private-code`), **never** inside the user's workspace.
- `<project_id>` namespaces snapshots per project; `fast_hash(worktree)` namespaces per worktree (handles multiple worktrees of one repo).
- The shadow dir is a bare-style git dir whose **work-tree is the user's workspace**. Every git op is the equivalent of `git --git-dir <shadow> --work-tree <workspace> …`.

Shadow-repo config (set once at init, and re-asserted per command):

```
core.autocrlf = false      # never rewrite line endings on snapshot/restore
core.longpaths = true      # Windows long paths
core.symlinks  = true      # snapshot symlinks as symlinks
core.fsmonitor = false     # don't attach a filesystem monitor to the user's tree
core.quotepath = false     # raw UTF-8 paths (for non-ASCII filenames)
```

---

## 3. Operations

The engine is a single trait, implemented in `crates/core/src/checkpoint.rs`, with a **per-shadow-dir mutex** so concurrent turns serialize their git ops.

```rust
pub struct TreeHash(pub String); // a git tree object id; NOT a commit

#[async_trait]
pub trait Snapshot: Send + Sync {
    /// Capture the current worktree state. Returns a tree hash, or None if
    /// snapshots are disabled or the worktree is not a git repo.
    async fn track(&self) -> Result<Option<TreeHash>, SnapshotError>;

    /// Restore the ENTIRE worktree to a snapshot tree (read-tree + checkout-index -a -f).
    async fn restore(&self, snapshot: &TreeHash) -> Result<(), SnapshotError>;

    /// Revert specific files to the state recorded in a snapshot; delete files
    /// that did not exist in that snapshot. Used for partial/per-message revert.
    async fn revert(&self, patches: &[FilePatch]) -> Result<(), SnapshotError>;

    /// List the files that changed since a snapshot (for recording which files a step touched).
    async fn changed_since(&self, snapshot: &TreeHash) -> Result<Vec<PathBuf>, SnapshotError>;

    /// Produce a structured diff between two snapshots for the UI.
    async fn diff(&self, from: &TreeHash, to: &TreeHash) -> Result<Vec<FileDiff>, SnapshotError>;

    /// gc the shadow dir (prune objects older than the retention window).
    async fn gc(&self) -> Result<(), SnapshotError>;
}
```

### 3.1 `track()` — take a snapshot

1. If `config.snapshots == false` or the worktree's VCS is not git → return `None` (no-op).
2. Create the shadow git dir if absent; on first creation, init it (workdir = user workspace) and set the config flags above.
3. **Stage the changed set** (see §4): modified-tracked + untracked-not-ignored, minus ignored, minus > 2 MB.
4. `write-tree` → a tree object id. **No commit is created.**
5. Return the `TreeHash`.

git2 mapping (the in-process, no-`git`-binary path — preferred for the single-static-binary goal):

```text
init    : Repository::init_opts(shadow_dir, bare=false, workdir=workspace);
          set core.* config keys above
stage   : let mut idx = repo.index();
          idx.add_all(candidate_paths, IndexAddOption::FORCE, Some(&mut filter_cb));  // filter = §4 rules
          idx.write();
snapshot: let oid = idx.write_tree();   // TreeHash = oid.to_string()
```

> The reference shells out to the system `git` CLI. We default to **git2 (libgit2)** so the binary has **no runtime `git` dependency** (honors North Star §12 "single static binary, no runtime dependency"). gix is used for read-only ops (status/diff display) but **not** for the write path — gix's high-level write/checkout/restore APIs are still unimplemented per the gitoxide crate-status, and this is the highest-risk subsystem. *(Re-verify gitoxide's write-path status at implementation time; if it has matured, the engine can migrate. Shelling to the `git` CLI exactly as the reference does is the fallback if a libgit2 op proves insufficient.)*

### 3.2 `restore(snapshot)` — full worktree restore

```text
read-tree <snapshot>      → load the tree into the shadow index
checkout-index -a -f      → force-write every index entry to the worktree
```

git2 mapping:

```text
let tree = repo.find_tree(oid);
let mut idx = repo.index();
idx.read_tree(&tree); idx.write();
let mut co = CheckoutBuilder::new(); co.force();
repo.checkout_index(Some(&mut idx), Some(&mut co));
```

This restores the worktree to the snapshot's exact file contents. Because the shadow repo's tracked set excludes ignored/large files, restore does **not** clobber `node_modules/`, `target/`, build artifacts, or `.env` secrets.

### 3.3 `revert(patches)` — per-file revert

For each `(tree_hash, file)`: `git checkout <tree_hash> -- <file>`. If the file does not exist in that tree (`ls-tree` empty) → **delete** it from the worktree (it was created after the snapshot). Batch adjacent non-clashing files; fall back to single-file on failure. git2 mapping: read the blob at `<path>` from the tree and write it to the worktree; if absent, remove the file.

### 3.4 `changed_since` / `diff` — for the UI

`changed_since` runs `diff --cached --name-only <hash>` (after staging) to record which files a tool step touched. `diff(from, to)` produces a structured `FileDiff[]` (status added/deleted/modified, additions/deletions, unified patch text) using `diff --name-status` + `--numstat` + blob reads; binary files are tracked but shown as empty patches. **`similar` is used only to render display diffs — never to apply changes.**

---

## 4. Inclusion / exclusion rules (the part that makes restore safe)

When staging for a snapshot, the candidate set is:

```
candidates = (tracked files modified since last index)  ∪  (untracked files, --exclude-standard)
```

Then, in order:

1. **Resolve ignores against the *source* repo.** Run the equivalent of `git check-ignore --no-index` using the **user repo's** ignore rules (`.gitignore`, `.git/info/exclude`, global excludes) against the candidate set. With git2, use the worktree's ignore rules via `Repository::status_should_ignore` / the `add_all` ignore-aware path. Newly-ignored files are removed from the shadow index so they don't linger.
2. **Drop files > 2 MB.** Stat each candidate; any regular file larger than 2 MB is excluded and written into the shadow `info/exclude` so it is never re-added. (Prevents multi-GB binaries / model weights from bloating the snapshot store.)
3. **Seed `info/exclude`** from the source repo's `info/exclude` so the shadow repo inherits the user's local excludes.

Binary files that pass the size gate are tracked (so they can be restored) but rendered as empty diffs in the UI.

---

## 5. Capture cadence

The reference captures **not only when dirty** but at well-defined boundaries so every step is revertible:

| When | Why |
|---|---|
| **Turn start** (before the first provider call of a user input) | establishes the "before this turn" baseline for whole-turn revert |
| **Before each mutating tool step** | per-message / per-edit revert granularity |
| **After each mutating tool step** | records the post-step tree + which files changed (for the diff UI and the `checkpoint` row) |

A "mutating tool step" is any tool whose `mutates()` metadata is true (see the permission section: the demoted `PermissionClass` exists **only** to answer "does this tool mutate the worktree → take a checkpoint"). Read-only tools never trigger a snapshot.

---

## 6. Persistence

The `checkpoint` table records snapshots (renamed `commit_hash` → `tree_hash` to reflect that these are **tree** objects, not commits):

```sql
CREATE TABLE checkpoint (
    id          TEXT PRIMARY KEY NOT NULL,
    session_id  TEXT NOT NULL,
    message_id  TEXT NOT NULL,        -- the message/step this snapshot precedes or follows
    tree_hash   TEXT NOT NULL,        -- git tree object id from write-tree (NOT a commit)
    tool_name   TEXT NOT NULL,        -- the tool that triggered the checkpoint
    kind        TEXT NOT NULL,        -- 'turn_start' | 'pre_step' | 'post_step'
    created_at  INTEGER NOT NULL,
    FOREIGN KEY(session_id) REFERENCES session(id) ON DELETE CASCADE
);
CREATE INDEX idx_checkpoint_session ON checkpoint(session_id);
```

The `session.revert` JSON column records the active revert target (`{ message_id, tree_hash }`) so a session can show "reverted to here" and support unrevert.

---

## 7. Retention, locking, lifecycle

- **Locking:** a per-shadow-dir async mutex serializes all ops on one shadow repo (snapshots, restores, and diffs never interleave).
- **Retention:** a background task runs `gc --prune=7.days` hourly (first run after a 1-minute delay). Snapshot objects older than 7 days are pruned.
- **Disable:** `config.snapshots = false` turns the engine into a no-op (every op returns early). The `plan` agent still functions; revert simply isn't available.

---

## 8. Edge cases

| Case | Behavior |
|---|---|
| Worktree is **not** a git repo | snapshots disabled; warn once; agent runs without revert |
| Dirty user index / in-progress rebase or merge | **unaffected** — the shadow repo has its own index and never touches the user's |
| Symlinks | snapshotted as symlinks (`core.symlinks=true`) |
| CRLF files | byte-preserved (`core.autocrlf=false`) — restore never silently rewrites line endings |
| Non-ASCII paths | `core.quotepath=false` keeps raw UTF-8 |
| Submodules | tracked as gitlinks; submodule *contents* are out of scope for v1 (document the limitation) |
| File > 2 MB created mid-turn | excluded from the snapshot and added to `info/exclude`; revert will not restore it (it was never captured) — surface this in the diff UI |

---

## 9. Test plan

1. **Non-destructiveness (the headline test):** start with a dirty user index + an in-progress `git rebase -i`; take a snapshot, mutate files, restore; assert the user's `HEAD`, branch, index, and rebase state are **byte-for-byte unchanged** and `git status` is identical to before the agent ran.
2. **Restore correctness:** snapshot → edit/create/delete files → restore → assert worktree matches the snapshot exactly (including deletions of files created after the snapshot).
3. **Ignore safety:** with a 500 MB file in `target/` and a secret in a gitignored `.env`, assert neither is captured and restore never touches them.
4. **2 MB gate:** a 3 MB binary created mid-turn is excluded and `info/exclude` updated.
5. **Per-file revert:** revert one file of a multi-file step; assert other files are untouched.
6. **Concurrency:** two overlapping `track()` calls on one shadow dir serialize without index corruption.
7. **No-git worktree:** all ops are graceful no-ops.
8. **Crash resilience:** kill mid-snapshot; the next `track()` recovers (shadow index is regenerated from the worktree).
