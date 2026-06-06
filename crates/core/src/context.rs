use crate::db::get_context_epoch;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceSnapshotValue {
    pub value: serde_json::Value,
    pub removed: bool,
}

pub type SystemContextSnapshot = HashMap<String, SourceSnapshotValue>;

pub enum SourceLoad {
    Loaded(serde_json::Value),
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceCompare {
    Unchanged,
    Updated,
    Incompatible,
}

#[async_trait]
pub trait ContextSource: Send + Sync {
    fn key(&self) -> &str;
    async fn load(&self, workspace_path: &Path, active_dir: &Path) -> SourceLoad;
    fn compare(&self, previous: &serde_json::Value, current: &serde_json::Value) -> SourceCompare;
    fn encode(&self, value: &serde_json::Value) -> serde_json::Value;
    fn render_baseline(&self, current: &serde_json::Value) -> String;
    fn render_update(&self, previous: &serde_json::Value, current: &serde_json::Value) -> String;
    fn render_removal(&self, previous: &serde_json::Value) -> Option<String>;
}

// 1. core/date
pub struct DateSource;

#[async_trait]
impl ContextSource for DateSource {
    fn key(&self) -> &str {
        "core/date"
    }

    async fn load(&self, _workspace_path: &Path, _active_dir: &Path) -> SourceLoad {
        let now = chrono::Local::now();
        SourceLoad::Loaded(serde_json::Value::String(
            now.format("%Y-%m-%d").to_string(),
        ))
    }

    fn compare(&self, previous: &serde_json::Value, current: &serde_json::Value) -> SourceCompare {
        if previous == current {
            SourceCompare::Unchanged
        } else {
            SourceCompare::Updated
        }
    }

    fn encode(&self, value: &serde_json::Value) -> serde_json::Value {
        value.clone()
    }

    fn render_baseline(&self, current: &serde_json::Value) -> String {
        format!("Today's date is: {}.", current.as_str().unwrap_or_default())
    }

    fn render_update(&self, _previous: &serde_json::Value, current: &serde_json::Value) -> String {
        format!(
            "Today's date is now: {}.",
            current.as_str().unwrap_or_default()
        )
    }

    fn render_removal(&self, _previous: &serde_json::Value) -> Option<String> {
        None
    }
}

// 2. core/environment
pub struct EnvironmentSource;

#[derive(Serialize, Deserialize, PartialEq)]
struct EnvValue {
    working_directory: String,
    workspace_root: String,
    vcs: String,
    platform: String,
}

#[async_trait]
impl ContextSource for EnvironmentSource {
    fn key(&self) -> &str {
        "core/environment"
    }

    async fn load(&self, workspace_path: &Path, active_dir: &Path) -> SourceLoad {
        let is_git = workspace_path.join(".git").exists() || active_dir.join(".git").exists();
        let vcs = if is_git { "Git" } else { "None" };
        let platform = match std::env::consts::OS {
            "macos" => "macOS",
            "windows" => "Windows",
            "linux" => "Linux",
            other => other,
        };

        let val = EnvValue {
            working_directory: active_dir.to_string_lossy().to_string(),
            workspace_root: workspace_path.to_string_lossy().to_string(),
            vcs: vcs.to_string(),
            platform: platform.to_string(),
        };

        SourceLoad::Loaded(serde_json::to_value(val).unwrap_or_default())
    }

    fn compare(&self, previous: &serde_json::Value, current: &serde_json::Value) -> SourceCompare {
        if previous == current {
            SourceCompare::Unchanged
        } else {
            SourceCompare::Updated
        }
    }

    fn encode(&self, value: &serde_json::Value) -> serde_json::Value {
        value.clone()
    }

    fn render_baseline(&self, current: &serde_json::Value) -> String {
        let val: EnvValue = serde_json::from_value(current.clone()).unwrap_or_else(|_| EnvValue {
            working_directory: String::new(),
            workspace_root: String::new(),
            vcs: "None".to_string(),
            platform: "Unknown".to_string(),
        });
        format!(
            "Useful info about the running environment:\n  Working directory: {}\n  Workspace root: {}\n  VCS: {}\n  Platform: {}\n",
            val.working_directory, val.workspace_root, val.vcs, val.platform
        )
    }

    fn render_update(&self, previous: &serde_json::Value, current: &serde_json::Value) -> String {
        let prev_val: EnvValue =
            serde_json::from_value(previous.clone()).unwrap_or_else(|_| EnvValue {
                working_directory: String::new(),
                workspace_root: String::new(),
                vcs: "None".to_string(),
                platform: "Unknown".to_string(),
            });
        let curr_val: EnvValue =
            serde_json::from_value(current.clone()).unwrap_or_else(|_| EnvValue {
                working_directory: String::new(),
                workspace_root: String::new(),
                vcs: "None".to_string(),
                platform: "Unknown".to_string(),
            });

        if prev_val.working_directory != curr_val.working_directory {
            format!(
                "Working directory changed to: {}.",
                curr_val.working_directory
            )
        } else {
            format!(
                "Environment updated: Working directory is: {}.",
                curr_val.working_directory
            )
        }
    }

    fn render_removal(&self, _previous: &serde_json::Value) -> Option<String> {
        None
    }
}

// 3. core/instructions (AGENTS.md)
pub struct InstructionsSource;

#[async_trait]
impl ContextSource for InstructionsSource {
    fn key(&self) -> &str {
        "core/instructions"
    }

    async fn load(&self, _workspace_path: &Path, active_dir: &Path) -> SourceLoad {
        // Scan upward for AGENTS.md starting from active_dir
        let mut curr = active_dir.to_path_buf();
        loop {
            let path = curr.join("AGENTS.md");
            if path.exists()
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                return SourceLoad::Loaded(serde_json::Value::String(content));
            }
            if let Some(parent) = curr.parent() {
                curr = parent.to_path_buf();
            } else {
                break;
            }
        }
        SourceLoad::Loaded(serde_json::Value::Null)
    }

    fn compare(&self, previous: &serde_json::Value, current: &serde_json::Value) -> SourceCompare {
        if previous == current {
            SourceCompare::Unchanged
        } else {
            SourceCompare::Updated
        }
    }

    fn encode(&self, value: &serde_json::Value) -> serde_json::Value {
        value.clone()
    }

    fn render_baseline(&self, current: &serde_json::Value) -> String {
        if let Some(content) = current.as_str() {
            format!("Instructions from AGENTS.md:\n{}\n", content)
        } else {
            String::new()
        }
    }

    fn render_update(&self, _previous: &serde_json::Value, current: &serde_json::Value) -> String {
        if let Some(content) = current.as_str() {
            format!(
                "These instructions replace all previously loaded ambient instructions:\n{}\n",
                content
            )
        } else {
            "Previously loaded instructions no longer apply.".to_string()
        }
    }

    fn render_removal(&self, _previous: &serde_json::Value) -> Option<String> {
        Some("Previously loaded instructions no longer apply.".to_string())
    }
}

#[derive(Debug)]
pub enum Reconcile {
    Unchanged,
    Updated {
        text: String,
        snapshot: SystemContextSnapshot,
    },
    ReplacementReady {
        baseline: String,
        snapshot: SystemContextSnapshot,
    },
    ReplacementBlocked,
}

// 4. code/repomap (Phase 4) — a structural overview from the symbol index,
// injected so its updates ride the same context-epoch machinery as the rest.
pub struct RepoMapSource {
    pool: SqlitePool,
    token_budget: usize,
}

impl RepoMapSource {
    pub fn new(pool: SqlitePool, token_budget: usize) -> Self {
        Self { pool, token_budget }
    }
}

#[async_trait]
impl ContextSource for RepoMapSource {
    fn key(&self) -> &str {
        "code/repomap"
    }

    async fn load(&self, _workspace_path: &Path, _active_dir: &Path) -> SourceLoad {
        // The index stores workspace-relative paths, so no root is needed here.
        match crate::repomap::generate(&self.pool, self.token_budget).await {
            Ok(map) => SourceLoad::Loaded(serde_json::Value::String(map)),
            Err(e) => {
                tracing::warn!("code/repomap: failed to generate from index: {e}");
                SourceLoad::Unavailable
            }
        }
    }

    fn compare(&self, previous: &serde_json::Value, current: &serde_json::Value) -> SourceCompare {
        if previous == current {
            SourceCompare::Unchanged
        } else {
            SourceCompare::Updated
        }
    }

    fn encode(&self, value: &serde_json::Value) -> serde_json::Value {
        value.clone()
    }

    fn render_baseline(&self, current: &serde_json::Value) -> String {
        let map = current.as_str().unwrap_or_default();
        if map.trim().is_empty() {
            String::new()
        } else {
            format!("Repository structure (symbols):\n{map}")
        }
    }

    fn render_update(&self, _previous: &serde_json::Value, current: &serde_json::Value) -> String {
        let map = current.as_str().unwrap_or_default();
        if map.trim().is_empty() {
            "The repository structure index is now empty.".to_string()
        } else {
            format!("Repository structure updated:\n{map}")
        }
    }

    fn render_removal(&self, _previous: &serde_json::Value) -> Option<String> {
        None
    }
}

pub struct SystemContextRegistry {
    sources: Vec<Box<dyn ContextSource>>,
}

impl Default for SystemContextRegistry {
    fn default() -> Self {
        Self {
            sources: vec![
                Box::new(DateSource),
                Box::new(EnvironmentSource),
                Box::new(InstructionsSource),
            ],
        }
    }
}

impl SystemContextRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an extra context source (e.g. [`RepoMapSource`]) beyond the
    /// built-in date/environment/instructions set.
    pub fn register(&mut self, source: Box<dyn ContextSource>) {
        self.sources.push(source);
    }

    pub async fn reconcile(
        &self,
        pool: &SqlitePool,
        session_id: &str,
        workspace_path: &Path,
        active_dir: &Path,
    ) -> Result<Reconcile, sqlx::Error> {
        // 1. Load current values of all sources
        let mut current_values = HashMap::new();
        for src in &self.sources {
            match src.load(workspace_path, active_dir).await {
                SourceLoad::Loaded(val) => {
                    current_values.insert(src.key().to_string(), (val, false));
                }
                SourceLoad::Unavailable => {
                    current_values.insert(src.key().to_string(), (serde_json::Value::Null, true));
                }
            }
        }

        // 2. Fetch active context epoch from DB
        let active_epoch = get_context_epoch(pool, session_id).await?;

        let previous_snapshot: SystemContextSnapshot = match &active_epoch {
            Some(epoch) => serde_json::from_str(&epoch.snapshot).unwrap_or_default(),
            None => HashMap::new(),
        };

        if active_epoch.is_none() {
            // No stored epoch -> initialize replacement
            let mut snapshot = HashMap::new();
            let mut baseline = String::new();
            for src in &self.sources {
                if let Some((val, unavailable)) = current_values.get(src.key())
                    && !unavailable
                {
                    snapshot.insert(
                        src.key().to_string(),
                        SourceSnapshotValue {
                            value: src.encode(val),
                            removed: val.is_null(),
                        },
                    );
                    let rendered = src.render_baseline(val);
                    if !rendered.is_empty() {
                        baseline.push_str(&rendered);
                        baseline.push('\n');
                    }
                }
            }
            return Ok(Reconcile::ReplacementReady { baseline, snapshot });
        }

        // Evaluate diff
        let mut updates = Vec::new();
        let mut snapshot_updated = previous_snapshot.clone();
        let mut replacement_blocked = false;
        let mut has_changes = false;

        for src in &self.sources {
            let key = src.key();
            let prev = previous_snapshot.get(key);
            let curr = current_values.get(key);

            match (prev, curr) {
                (Some(p_val), Some((c_val, false))) => {
                    // Check if it was removed previously and is back, or if it changed
                    if p_val.removed && !c_val.is_null() {
                        // Re-added
                        let update_text = src.render_baseline(c_val);
                        if !update_text.is_empty() {
                            updates.push(update_text);
                        }
                        snapshot_updated.insert(
                            key.to_string(),
                            SourceSnapshotValue {
                                value: src.encode(c_val),
                                removed: false,
                            },
                        );
                        has_changes = true;
                    } else if !p_val.removed && c_val.is_null() {
                        // Removed
                        if let Some(rem_text) = src.render_removal(&p_val.value) {
                            updates.push(rem_text);
                        }
                        snapshot_updated.insert(
                            key.to_string(),
                            SourceSnapshotValue {
                                value: serde_json::Value::Null,
                                removed: true,
                            },
                        );
                        has_changes = true;
                    } else if !p_val.removed && !c_val.is_null() {
                        // Compare values
                        match src.compare(&p_val.value, c_val) {
                            SourceCompare::Unchanged => {}
                            SourceCompare::Updated => {
                                let update_text = src.render_update(&p_val.value, c_val);
                                if !update_text.is_empty() {
                                    updates.push(update_text);
                                }
                                snapshot_updated.insert(
                                    key.to_string(),
                                    SourceSnapshotValue {
                                        value: src.encode(c_val),
                                        removed: false,
                                    },
                                );
                                has_changes = true;
                            }
                            SourceCompare::Incompatible => {
                                // Forces replacement
                                // We can just trigger rebuild immediately
                                let mut full_baseline = String::new();
                                let mut full_snapshot = HashMap::new();
                                for s in &self.sources {
                                    if let Some((val, unavail)) = current_values.get(s.key())
                                        && !unavail
                                    {
                                        full_snapshot.insert(
                                            s.key().to_string(),
                                            SourceSnapshotValue {
                                                value: s.encode(val),
                                                removed: val.is_null(),
                                            },
                                        );
                                        let rendered = s.render_baseline(val);
                                        if !rendered.is_empty() {
                                            full_baseline.push_str(&rendered);
                                            full_baseline.push('\n');
                                        }
                                    }
                                }
                                return Ok(Reconcile::ReplacementReady {
                                    baseline: full_baseline,
                                    snapshot: full_snapshot,
                                });
                            }
                        }
                    }
                }
                (Some(_p_val), Some((_, true))) => {
                    // Current is unavailable. Block replacement until it is available.
                    replacement_blocked = true;
                }
                (None, Some((c_val, false))) => {
                    // New source added
                    let update_text = src.render_baseline(c_val);
                    if !update_text.is_empty() {
                        updates.push(update_text);
                    }
                    snapshot_updated.insert(
                        key.to_string(),
                        SourceSnapshotValue {
                            value: src.encode(c_val),
                            removed: c_val.is_null(),
                        },
                    );
                    has_changes = true;
                }
                _ => {}
            }
        }

        if replacement_blocked {
            return Ok(Reconcile::ReplacementBlocked);
        }

        if has_changes {
            let text = updates.join("\n");
            Ok(Reconcile::Updated {
                text,
                snapshot: snapshot_updated,
            })
        } else {
            Ok(Reconcile::Unchanged)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{
        connect_db, create_project, create_session, insert_context_epoch, run_migrations,
    };
    use uuid::Uuid;

    /// The full Phase-4 vertical slice: index a workspace (walk→extract→FTS5),
    /// register the `code/repomap` source, and confirm the rendered structure
    /// lands in the epoch baseline via the same reconcile machinery as the
    /// built-in sources.
    #[tokio::test]
    async fn repomap_source_injects_structure_into_the_baseline() {
        use crate::indexer::index_workspace;
        use tempfile::TempDir;

        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();

        let ws = TempDir::new().unwrap();
        let root = ws.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/config.rs"),
            "pub struct AppConfig {}\nimpl AppConfig { pub fn load() {} }\n",
        )
        .unwrap();

        let project_id = Uuid::new_v4().to_string();
        let session_id = Uuid::new_v4().to_string();
        create_project(&pool, &project_id, "t", root.to_string_lossy().as_ref())
            .await
            .unwrap();
        create_session(
            &pool,
            &session_id,
            &project_id,
            root.to_string_lossy().as_ref(),
            root.to_string_lossy().as_ref(),
            "s",
            "build",
            "{}",
        )
        .await
        .unwrap();

        index_workspace(&pool, root).await.unwrap();

        let mut registry = SystemContextRegistry::new();
        registry.register(Box::new(RepoMapSource::new(
            pool.clone(),
            crate::repomap::DEFAULT_REPOMAP_TOKEN_BUDGET,
        )));

        let baseline = match registry
            .reconcile(&pool, &session_id, root, root)
            .await
            .unwrap()
        {
            Reconcile::ReplacementReady { baseline, .. } => baseline,
            other => panic!("expected ReplacementReady, got {other:?}"),
        };
        assert!(
            baseline.contains("Repository structure (symbols):"),
            "repo map must be in the baseline:\n{baseline}"
        );
        assert!(baseline.contains("src/config.rs"));
        assert!(baseline.contains("struct AppConfig"));
        assert!(baseline.contains("fn load()"));
    }

    #[tokio::test]
    async fn test_context_reconciliation() {
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();

        let project_id = Uuid::new_v4().to_string();
        let session_id = Uuid::new_v4().to_string();

        let temp_dir = std::env::temp_dir();
        let workspace_path = temp_dir.join("pc_workspace");
        let active_dir = workspace_path.join("src");
        std::fs::create_dir_all(&active_dir).ok();

        create_project(
            &pool,
            &project_id,
            "Test context project",
            workspace_path.to_string_lossy().as_ref(),
        )
        .await
        .unwrap();
        create_session(
            &pool,
            &session_id,
            &project_id,
            workspace_path.to_string_lossy().as_ref(),
            active_dir.to_string_lossy().as_ref(),
            "Context Session",
            "build",
            "{}",
        )
        .await
        .unwrap();

        let registry = SystemContextRegistry::new();

        // 1. Initial reconcile: should be ReplacementReady
        let reconcile_res = registry
            .reconcile(&pool, &session_id, &workspace_path, &active_dir)
            .await
            .unwrap();

        let (baseline, snapshot) = match reconcile_res {
            Reconcile::ReplacementReady { baseline, snapshot } => (baseline, snapshot),
            other => panic!("Expected ReplacementReady, got {:?}", other),
        };
        assert!(baseline.contains("Today's date is:"));
        assert!(baseline.contains("Useful info about the running environment:"));

        // Persist epoch
        let mut tx = pool.begin().await.unwrap();
        let snapshot_json = serde_json::to_string(&snapshot).unwrap();
        let inserted =
            insert_context_epoch(&mut tx, &session_id, "build", &baseline, &snapshot_json, 1)
                .await
                .unwrap();
        assert!(inserted);
        tx.commit().await.unwrap();

        // 2. Second reconcile without changes: should be Unchanged
        let reconcile_res2 = registry
            .reconcile(&pool, &session_id, &workspace_path, &active_dir)
            .await
            .unwrap();
        assert!(matches!(reconcile_res2, Reconcile::Unchanged));

        // 3. Create AGENTS.md: should be Updated
        let agents_path = workspace_path.join("AGENTS.md");
        std::fs::write(&agents_path, "Custom Agent System Instructions").unwrap();

        let reconcile_res3 = registry
            .reconcile(&pool, &session_id, &workspace_path, &active_dir)
            .await
            .unwrap();
        match reconcile_res3 {
            Reconcile::Updated {
                text,
                snapshot: snap,
            } => {
                assert!(text.contains("Custom Agent System Instructions"));
                assert!(snap.contains_key("core/instructions"));
            }
            other => panic!("Expected Updated, got {:?}", other),
        }

        // Cleanup
        std::fs::remove_file(&agents_path).ok();
        std::fs::remove_dir_all(&workspace_path).ok();
    }

    use std::sync::{Arc, Mutex};

    /// A test-only source with interior-mutable flags so one registry can be
    /// driven through every reconcile state across successive calls — including
    /// the two arms unreachable with the built-in sources (ReplacementBlocked via
    /// Unavailable, and Incompatible-forced ReplacementReady).
    struct ToggleSource {
        state: Arc<Mutex<ToggleState>>,
    }
    struct ToggleState {
        available: bool,
        value: serde_json::Value,
        force_incompatible: bool,
    }

    #[async_trait]
    impl ContextSource for ToggleSource {
        fn key(&self) -> &str {
            "test/toggle"
        }
        async fn load(&self, _ws: &Path, _active: &Path) -> SourceLoad {
            let s = self.state.lock().unwrap();
            if s.available {
                SourceLoad::Loaded(s.value.clone())
            } else {
                SourceLoad::Unavailable
            }
        }
        fn compare(
            &self,
            previous: &serde_json::Value,
            current: &serde_json::Value,
        ) -> SourceCompare {
            if self.state.lock().unwrap().force_incompatible {
                SourceCompare::Incompatible
            } else if previous == current {
                SourceCompare::Unchanged
            } else {
                SourceCompare::Updated
            }
        }
        fn encode(&self, value: &serde_json::Value) -> serde_json::Value {
            value.clone()
        }
        fn render_baseline(&self, current: &serde_json::Value) -> String {
            format!("toggle baseline: {current}")
        }
        fn render_update(
            &self,
            _previous: &serde_json::Value,
            current: &serde_json::Value,
        ) -> String {
            format!("toggle updated: {current}")
        }
        fn render_removal(&self, _previous: &serde_json::Value) -> Option<String> {
            Some("toggle removed".to_string())
        }
    }

    #[tokio::test]
    async fn reconcile_produces_all_four_arms() {
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        let pid = Uuid::new_v4().to_string();
        let sid = Uuid::new_v4().to_string();
        let ws = tempfile::tempdir().unwrap();
        let wp = ws.path();
        create_project(&pool, &pid, "t", wp.to_str().unwrap())
            .await
            .unwrap();
        create_session(
            &pool,
            &sid,
            &pid,
            wp.to_str().unwrap(),
            wp.to_str().unwrap(),
            "t",
            "build",
            "{}",
        )
        .await
        .unwrap();

        let state = Arc::new(Mutex::new(ToggleState {
            available: true,
            value: serde_json::json!("v1"),
            force_incompatible: false,
        }));
        let reg = SystemContextRegistry {
            sources: vec![Box::new(ToggleSource {
                state: state.clone(),
            })],
        };

        // T1: no epoch yet -> ReplacementReady; persist it.
        let r1 = reg.reconcile(&pool, &sid, wp, wp).await.unwrap();
        let (baseline, snapshot) = match r1 {
            Reconcile::ReplacementReady { baseline, snapshot } => (baseline, snapshot),
            other => panic!("T1 expected ReplacementReady, got {other:?}"),
        };
        let mut tx = pool.begin().await.unwrap();
        let snap_json = serde_json::to_string(&snapshot).unwrap();
        insert_context_epoch(&mut tx, &sid, "build", &baseline, &snap_json, 1)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        // T2: source becomes Unavailable -> ReplacementBlocked.
        state.lock().unwrap().available = false;
        let r2 = reg.reconcile(&pool, &sid, wp, wp).await.unwrap();
        assert!(
            matches!(r2, Reconcile::ReplacementBlocked),
            "T2 expected ReplacementBlocked, got {r2:?}"
        );

        // T3: available again but compare() forced Incompatible -> ReplacementReady.
        {
            let mut s = state.lock().unwrap();
            s.available = true;
            s.force_incompatible = true;
        }
        let r3 = reg.reconcile(&pool, &sid, wp, wp).await.unwrap();
        assert!(
            matches!(r3, Reconcile::ReplacementReady { .. }),
            "T3 expected ReplacementReady (incompatible), got {r3:?}"
        );

        // T4: compatible value change -> Updated.
        {
            let mut s = state.lock().unwrap();
            s.force_incompatible = false;
            s.value = serde_json::json!("v2");
        }
        let r4 = reg.reconcile(&pool, &sid, wp, wp).await.unwrap();
        assert!(
            matches!(r4, Reconcile::Updated { .. }),
            "T4 expected Updated, got {r4:?}"
        );

        // T5: back to the stored value, no incompatibility -> Unchanged.
        {
            let mut s = state.lock().unwrap();
            s.value = serde_json::json!("v1");
        }
        let r5 = reg.reconcile(&pool, &sid, wp, wp).await.unwrap();
        assert!(
            matches!(r5, Reconcile::Unchanged),
            "T5 expected Unchanged, got {r5:?}"
        );
    }
}
