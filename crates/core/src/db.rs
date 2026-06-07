use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::time::Duration;

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct ProjectRow {
    pub id: String,
    pub name: String,
    pub directory: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct SessionRow {
    pub id: String,
    pub project_id: String,
    pub parent_id: Option<String>,
    pub workspace_path: String,
    pub active_directory: String,
    pub title: String,
    pub agent_id: String,
    pub model_config: String, // JSON string
    pub seq_counter: i64,
    pub cost: f64,
    pub tokens_input: i64,
    pub tokens_output: i64,
    pub tokens_reasoning: i64,
    pub tokens_cache_read: i64,
    pub tokens_cache_write: i64,
    pub revert: Option<String>,     // JSON string
    pub permission: Option<String>, // JSON string
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct MessageRow {
    pub id: String,
    pub session_id: String,
    pub seq: i64,
    pub type_: String, // maps to db column `type`
    pub data: String,  // JSON representation of Message variant
    pub created_at: i64,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct ContextEpochRow {
    pub session_id: String,
    pub agent_id: String,
    pub baseline: String,
    pub snapshot: String, // JSON representation of SystemContextSnapshot
    pub baseline_seq: i64,
    pub replacement_seq: Option<i64>,
    pub revision: i64,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct SessionInputRow {
    pub id: String,
    pub session_id: String,
    pub prompt: String,
    pub delivery: String, // 'steer' | 'queue'
    pub admitted_seq: i64,
    pub promoted_seq: Option<i64>,
    pub created_at: i64,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct CheckpointRow {
    pub id: String,
    pub session_id: String,
    pub message_id: String,
    pub tree_hash: String,
    pub tool_name: String,
    pub kind: String, // 'turn_start' | 'pre_step' | 'post_step'
    pub created_at: i64,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct PermissionSavedRow {
    pub project_id: String,
    pub action: String,
    pub resource: String,
    pub created_at: i64,
}

pub async fn connect_db(db_url: &str) -> Result<SqlitePool, sqlx::Error> {
    let options = db_url
        .parse::<SqliteConnectOptions>()?
        // Create the database file on first run. sqlx defaults this to false, so
        // without it a fresh install fails to open its on-disk DB with
        // SQLITE_CANTOPEN (the CLI URL carries no `?mode=rwc`). Harmless for an
        // existing file and for the `sqlite::memory:` test/bench URLs.
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_millis(5000));

    SqlitePoolOptions::new()
        .max_connections(10)
        .connect_with(options)
        .await
}

pub async fn run_migrations(pool: &SqlitePool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}

// Monotonic sequence allocator (CAS/immediate lock write style)
pub async fn next_sequence(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session_id: &str,
) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        "UPDATE session SET seq_counter = seq_counter + 1, updated_at = strftime('%s','now') WHERE id = ?1 RETURNING seq_counter"
    )
    .bind(session_id)
    .fetch_one(&mut **tx)
    .await?;
    Ok(row.0)
}

pub async fn create_project(
    pool: &SqlitePool,
    id: &str,
    name: &str,
    directory: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO project (id, name, directory, created_at) VALUES (?1, ?2, ?3, strftime('%s','now'))"
    )
    .bind(id)
    .bind(name)
    .bind(directory)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_project(pool: &SqlitePool, id: &str) -> Result<Option<ProjectRow>, sqlx::Error> {
    sqlx::query_as::<_, ProjectRow>(
        "SELECT id, name, directory, created_at FROM project WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn list_projects(pool: &SqlitePool) -> Result<Vec<ProjectRow>, sqlx::Error> {
    sqlx::query_as::<_, ProjectRow>(
        "SELECT id, name, directory, created_at FROM project ORDER BY created_at DESC",
    )
    .fetch_all(pool)
    .await
}

#[allow(clippy::too_many_arguments)] // thin SQL CRUD wrapper; columns map 1:1
pub async fn create_session(
    pool: &SqlitePool,
    id: &str,
    project_id: &str,
    workspace_path: &str,
    active_directory: &str,
    title: &str,
    agent_id: &str,
    model_config: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO session (id, project_id, workspace_path, active_directory, title, agent_id, model_config, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, strftime('%s','now'), strftime('%s','now'))"
    )
    .bind(id)
    .bind(project_id)
    .bind(workspace_path)
    .bind(active_directory)
    .bind(title)
    .bind(agent_id)
    .bind(model_config)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_session(pool: &SqlitePool, id: &str) -> Result<Option<SessionRow>, sqlx::Error> {
    sqlx::query_as::<_, SessionRow>("SELECT * FROM session WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn list_sessions(
    pool: &SqlitePool,
    project_id: &str,
) -> Result<Vec<SessionRow>, sqlx::Error> {
    sqlx::query_as::<_, SessionRow>(
        "SELECT * FROM session WHERE project_id = ?1 ORDER BY updated_at DESC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

pub async fn delete_session(pool: &SqlitePool, session_id: &str) -> Result<(), sqlx::Error> {
    // Delete messages, inputs, checkpoints, context epoch, and session in order
    sqlx::query("DELETE FROM session_message WHERE session_id = ?1")
        .bind(session_id)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM session_input WHERE session_id = ?1")
        .bind(session_id)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM checkpoint WHERE session_id = ?1")
        .bind(session_id)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM session_context_epoch WHERE session_id = ?1")
        .bind(session_id)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM session WHERE id = ?1")
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)] // thin SQL UPDATE wrapper; columns map 1:1
pub async fn update_usage(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session_id: &str,
    cost: f64,
    tokens_input: i64,
    tokens_output: i64,
    tokens_reasoning: i64,
    tokens_cache_read: i64,
    tokens_cache_write: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE session SET \
         cost = cost + ?2, \
         tokens_input = tokens_input + ?3, \
         tokens_output = tokens_output + ?4, \
         tokens_reasoning = tokens_reasoning + ?5, \
         tokens_cache_read = tokens_cache_read + ?6, \
         tokens_cache_write = tokens_cache_write + ?7, \
         updated_at = strftime('%s','now') \
         WHERE id = ?1",
    )
    .bind(session_id)
    .bind(cost)
    .bind(tokens_input)
    .bind(tokens_output)
    .bind(tokens_reasoning)
    .bind(tokens_cache_read)
    .bind(tokens_cache_write)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Persist a session's model selection (`model_config` JSON: `provider_id` +
/// `model_id`). The orchestrator re-reads `model_id` per turn, but the provider
/// is pinned at session build — so a `provider_id` change requires evicting the
/// live session (see `SessionCoordinator::set_model`).
pub async fn update_session_model(
    pool: &SqlitePool,
    session_id: &str,
    model_config: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE session SET model_config = ?1, updated_at = strftime('%s','now') WHERE id = ?2",
    )
    .bind(model_config)
    .bind(session_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Persist a session's agent selection. The orchestrator re-reads `agent_id`
/// from the DB at the start of each turn, so this takes effect on the next turn
/// without evicting the live session.
pub async fn update_session_agent(
    pool: &SqlitePool,
    session_id: &str,
    agent_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE session SET agent_id = ?1, updated_at = strftime('%s','now') WHERE id = ?2",
    )
    .bind(agent_id)
    .bind(session_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn append_message(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
    session_id: &str,
    seq: i64,
    message_type: &str,
    data_json: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO session_message (id, session_id, seq, type, data, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, strftime('%s','now'))",
    )
    .bind(id)
    .bind(session_id)
    .bind(seq)
    .bind(message_type)
    .bind(data_json)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn get_messages(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<Vec<MessageRow>, sqlx::Error> {
    sqlx::query_as::<_, MessageRow>("SELECT id, session_id, seq, type AS type_, data, created_at FROM session_message WHERE session_id = ?1 ORDER BY seq ASC")
        .bind(session_id)
        .fetch_all(pool)
        .await
}

pub async fn create_checkpoint(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
    session_id: &str,
    message_id: &str,
    tree_hash: &str,
    tool_name: &str,
    kind: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO checkpoint (id, session_id, message_id, tree_hash, tool_name, kind, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%s','now'))"
    )
    .bind(id)
    .bind(session_id)
    .bind(message_id)
    .bind(tree_hash)
    .bind(tool_name)
    .bind(kind)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn list_checkpoints(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<Vec<CheckpointRow>, sqlx::Error> {
    // `created_at` is whole-second granularity, but a single turn writes several
    // checkpoints (turn_start / pre_step / post_step) within the same second. Tie-
    // break on the implicit monotonic `rowid` (insertion order) so the newest
    // checkpoint is unambiguously first — revert/undo selects the first non-backup
    // row and must land on the latest one, not an arbitrary same-second sibling.
    sqlx::query_as::<_, CheckpointRow>(
        "SELECT * FROM checkpoint WHERE session_id = ?1 ORDER BY created_at DESC, rowid DESC",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
}

pub async fn save_permission(
    pool: &SqlitePool,
    project_id: &str,
    action: &str,
    resource: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR IGNORE INTO permission_saved (project_id, action, resource, created_at) \
         VALUES (?1, ?2, ?3, strftime('%s','now'))",
    )
    .bind(project_id)
    .bind(action)
    .bind(resource)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_saved_permissions(
    pool: &SqlitePool,
    project_id: &str,
) -> Result<Vec<PermissionSavedRow>, sqlx::Error> {
    sqlx::query_as::<_, PermissionSavedRow>(
        "SELECT * FROM permission_saved WHERE project_id = ?1 ORDER BY created_at DESC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

// Context epoch operations
pub async fn insert_context_epoch(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session_id: &str,
    agent_id: &str,
    baseline: &str,
    snapshot: &str,
    baseline_seq: i64,
) -> Result<bool, sqlx::Error> {
    let affected = sqlx::query(
        "INSERT INTO session_context_epoch (session_id, agent_id, baseline, snapshot, baseline_seq, revision) \
         VALUES (?1, ?2, ?3, ?4, ?5, 0) \
         ON CONFLICT(session_id) DO NOTHING"
    )
    .bind(session_id)
    .bind(agent_id)
    .bind(baseline)
    .bind(snapshot)
    .bind(baseline_seq)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

pub async fn advance_context_epoch(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session_id: &str,
    snapshot: &str,
    expected_revision: i64,
) -> Result<bool, sqlx::Error> {
    let affected = sqlx::query(
        "UPDATE session_context_epoch SET snapshot = ?2, revision = revision + 1 \
         WHERE session_id = ?1 AND revision = ?3 AND replacement_seq IS NULL",
    )
    .bind(session_id)
    .bind(snapshot)
    .bind(expected_revision)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

pub async fn replace_context_epoch(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session_id: &str,
    agent_id: &str,
    baseline: &str,
    snapshot: &str,
    baseline_seq: i64,
    expected_revision: i64,
) -> Result<bool, sqlx::Error> {
    let affected = sqlx::query(
        "UPDATE session_context_epoch SET baseline = ?3, agent_id = ?2, snapshot = ?4, \
         baseline_seq = ?5, replacement_seq = NULL, revision = revision + 1 \
         WHERE session_id = ?1 AND revision = ?6",
    )
    .bind(session_id)
    .bind(agent_id)
    .bind(baseline)
    .bind(snapshot)
    .bind(baseline_seq)
    .bind(expected_revision)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

pub async fn request_context_epoch_replacement(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session_id: &str,
    replacement_seq: i64,
) -> Result<bool, sqlx::Error> {
    let affected = sqlx::query(
        "UPDATE session_context_epoch SET replacement_seq = ?2, revision = revision + 1 \
         WHERE session_id = ?1 AND baseline_seq < ?2 AND (replacement_seq IS NULL OR replacement_seq < ?2)"
    )
    .bind(session_id)
    .bind(replacement_seq)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

pub async fn get_context_epoch(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<Option<ContextEpochRow>, sqlx::Error> {
    sqlx::query_as::<_, ContextEpochRow>(
        "SELECT session_id, agent_id, baseline, snapshot, baseline_seq, replacement_seq, revision \
         FROM session_context_epoch WHERE session_id = ?1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
}

// Session input admission
pub async fn admit_session_input(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
    session_id: &str,
    prompt: &str,
    delivery: &str,
    admitted_seq: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO session_input (id, session_id, prompt, delivery, admitted_seq, promoted_seq, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, NULL, strftime('%s','now'))"
    )
    .bind(id)
    .bind(session_id)
    .bind(prompt)
    .bind(delivery)
    .bind(admitted_seq)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn promote_session_input(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
    promoted_seq: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE session_input SET promoted_seq = ?2 WHERE id = ?1")
        .bind(id)
        .bind(promoted_seq)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn get_pending_inputs(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<Vec<SessionInputRow>, sqlx::Error> {
    sqlx::query_as::<_, SessionInputRow>(
        "SELECT * FROM session_input WHERE session_id = ?1 AND promoted_seq IS NULL ORDER BY admitted_seq ASC"
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    async fn setup_test_db() -> SqlitePool {
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn test_connect_db_creates_missing_file() {
        // Reproduces the CLI first-run path: `sqlite://<path>` with no `?mode=rwc`.
        // Without `.create_if_missing(true)` this fails with SQLITE_CANTOPEN.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("private-code.db");
        assert!(!db_path.exists());

        let url = format!("sqlite://{}", db_path.to_string_lossy());
        let pool = connect_db(&url)
            .await
            .expect("first-run connect must create the db file");
        run_migrations(&pool).await.unwrap();

        assert!(db_path.exists(), "db file should exist after first connect");
    }

    #[tokio::test]
    async fn test_list_checkpoints_orders_newest_first_within_same_second() {
        let pool = setup_test_db().await;
        let project_id = Uuid::new_v4().to_string();
        let session_id = Uuid::new_v4().to_string();
        create_project(&pool, &project_id, "P", "/tmp/p")
            .await
            .unwrap();
        create_session(
            &pool,
            &session_id,
            &project_id,
            "/tmp/p",
            "/tmp/p",
            "S",
            "build",
            "{}",
        )
        .await
        .unwrap();

        // Three checkpoints written back-to-back — the common case where all share
        // the same whole-second `created_at`. Insertion order is turn_start →
        // pre_step → post_step.
        let mut tx = pool.begin().await.unwrap();
        for (id, kind) in [
            ("cp1", "turn_start"),
            ("cp2", "pre_step"),
            ("cp3", "post_step"),
        ] {
            create_checkpoint(&mut tx, id, &session_id, "msg", "tree", "tool", kind)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();

        // The latest checkpoint (post_step, inserted last) must sort first, so
        // revert/undo — which picks the first non-backup row — lands on it rather
        // than an arbitrary same-second sibling.
        let cps = list_checkpoints(&pool, &session_id).await.unwrap();
        let ids: Vec<&str> = cps.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["cp3", "cp2", "cp1"]);
    }

    #[tokio::test]
    async fn test_crud_operations() {
        let pool = setup_test_db().await;
        let project_id = Uuid::new_v4().to_string();
        let session_id = Uuid::new_v4().to_string();

        create_project(&pool, &project_id, "Test Project", "/tmp/test")
            .await
            .unwrap();
        let proj = get_project(&pool, &project_id).await.unwrap().unwrap();
        assert_eq!(proj.name, "Test Project");

        create_session(
            &pool,
            &session_id,
            &project_id,
            "/tmp/test",
            "/tmp/test/src",
            "Test Session",
            "build",
            "{}",
        )
        .await
        .unwrap();

        let sess = get_session(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(sess.title, "Test Session");
        assert_eq!(sess.seq_counter, 0);

        let mut tx = pool.begin().await.unwrap();
        let seq = next_sequence(&mut tx, &session_id).await.unwrap();
        assert_eq!(seq, 1);

        append_message(&mut tx, "msg-1", &session_id, seq, "user", "{}")
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let messages = get_messages(&pool, &session_id).await.unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "msg-1");
        assert_eq!(messages[0].seq, 1);
    }

    #[tokio::test]
    async fn test_concurrent_append() {
        let pool = setup_test_db().await;
        let project_id = Uuid::new_v4().to_string();
        let session_id = Uuid::new_v4().to_string();

        create_project(&pool, &project_id, "Race Project", "/tmp/race")
            .await
            .unwrap();
        create_session(
            &pool,
            &session_id,
            &project_id,
            "/tmp/race",
            "/tmp/race",
            "Race Session",
            "build",
            "{}",
        )
        .await
        .unwrap();

        let num_tasks = 20;
        let mut handles = vec![];

        for i in 0..num_tasks {
            let pool_clone = pool.clone();
            let session_id_clone = session_id.clone();
            handles.push(tokio::spawn(async move {
                // Race safety comes from `next_sequence`: its `UPDATE ... RETURNING`
                // takes SQLite's write lock atomically, and the pool's busy_timeout
                // serializes contending writers — so no two tasks can read the same
                // counter. (A nested `BEGIN IMMEDIATE` here would be a no-op error
                // inside an already-open `pool.begin()` tx, so we don't issue one.)
                let mut tx = pool_clone.begin().await.unwrap();
                let seq = next_sequence(&mut tx, &session_id_clone).await.unwrap();
                let msg_id = format!("msg-{}", i);
                append_message(&mut tx, &msg_id, &session_id_clone, seq, "user", "{}")
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
                seq
            }));
        }

        let mut seqs = vec![];
        for h in handles {
            let seq = h.await.unwrap();
            seqs.push(seq);
        }

        seqs.sort();

        // Assert we got exactly 1..=num_tasks with no gaps and no duplicates
        let expected: Vec<i64> = (1..=num_tasks).map(|x| x as i64).collect();
        assert_eq!(seqs, expected);

        let messages = get_messages(&pool, &session_id).await.unwrap();
        assert_eq!(messages.len(), num_tasks);
    }
}
