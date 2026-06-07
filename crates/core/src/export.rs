//! Session export (Phase 5, Step 5.10).

use crate::db::{self, MessageRow};
use serde_json::Value;
use sqlx::SqlitePool;

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

/// Export a session conversation as Markdown.
pub async fn export_session_markdown(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<String, ExportError> {
    let session = db::get_session(pool, session_id)
        .await?
        .ok_or_else(|| ExportError::Other(format!("session not found: {session_id}")))?;
    let messages = db::get_messages(pool, session_id).await?;

    let mut md = String::new();
    md.push_str(&format!("# Session: {}\n\n", session.title));
    md.push_str(&format!("- **Agent:** {}\n", session.agent_id));
    md.push_str(&format!("- **Model:** {}\n", session.model_config));
    md.push_str(&format!("- **Cost:** ${:.4}\n\n", session.cost));
    md.push_str("---\n\n");

    for msg in messages {
        append_message_md(&mut md, &msg);
    }
    Ok(md)
}

fn append_message_md(md: &mut String, msg: &MessageRow) {
    let role = msg.type_.as_str();
    md.push_str(&format!("## {role} (seq {})\n\n", msg.seq));

    // Rows persist a FULL `ChatMessage` JSON object (`{id, role, content:[...]}`),
    // so the content blocks live under `.content` — render those, rather than
    // dumping the whole object as raw JSON (the old code parsed `data` and tested
    // `as_array()`/`as_str()` on the *object*, which never matched, so every
    // message was emitted as a raw JSON code block).
    let Ok(parsed) = serde_json::from_str::<Value>(&msg.data) else {
        md.push_str(&msg.data);
        md.push_str("\n\n");
        return;
    };

    if let Some(blocks) = parsed.get("content").and_then(|c| c.as_array()) {
        for block in blocks {
            append_block_md(md, block);
        }
    } else if parsed.get("summary").is_some() {
        // Compaction marker ({compacted_through_seq, summary}).
        md.push_str("_— history compacted —_\n");
    } else if let Some(text) = parsed.as_str() {
        md.push_str(text);
        md.push('\n');
    } else if let Some(arr) = parsed.as_array() {
        // Legacy/bare content array.
        for block in arr {
            append_block_md(md, block);
        }
    } else {
        md.push_str("```json\n");
        md.push_str(&msg.data);
        md.push_str("\n```\n");
    }
    md.push('\n');
}

/// Render a single content block (text / reasoning / tool_use / tool_result).
fn append_block_md(md: &mut String, block: &Value) {
    if let Some(t) = block["text"].as_str() {
        md.push_str(t);
        md.push('\n');
    } else if let Some(r) = block["reasoning"].as_str() {
        md.push_str(r);
        md.push('\n');
    } else if let Some(name) = block["name"].as_str() {
        // tool_use block.
        md.push_str(&format!("**Tool:** `{name}`\n\n"));
        if !block["input"].is_null()
            && let Ok(pretty) = serde_json::to_string_pretty(&block["input"])
        {
            md.push_str("```json\n");
            md.push_str(&pretty);
            md.push_str("\n```\n");
        }
    } else if let Some(id) = block["tool_use_id"].as_str() {
        // tool_result block.
        let content = &block["content"];
        let text = content
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| serde_json::to_string(content).unwrap_or_default());
        md.push_str(&format!("**Tool result** (`{id}`):\n\n```\n{text}\n```\n"));
    }
}

/// Export raw session data as JSON.
pub async fn export_session_json(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<String, ExportError> {
    let session = db::get_session(pool, session_id)
        .await?
        .ok_or_else(|| ExportError::Other(format!("session not found: {session_id}")))?;
    let messages = db::get_messages(pool, session_id).await?;
    let body = serde_json::json!({
        "session": session,
        "messages": messages,
    });
    Ok(serde_json::to_string_pretty(&body)?)
}

/// Write export to a file path.
pub async fn export_session_to_file(
    pool: &SqlitePool,
    session_id: &str,
    path: &std::path::Path,
    format: &str,
) -> Result<(), ExportError> {
    let content = match format {
        "json" => export_session_json(pool, session_id).await?,
        _ => export_session_markdown(pool, session_id).await?,
    };
    std::fs::write(path, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use uuid::Uuid;

    async fn setup_pool() -> sqlx::SqlitePool {
        let pool = db::connect_db("sqlite::memory:").await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        pool
    }

    async fn seed_session(pool: &sqlx::SqlitePool) -> String {
        let pid = Uuid::new_v4().to_string();
        db::create_project(pool, &pid, "p", "/tmp").await.unwrap();
        let sid = Uuid::new_v4().to_string();
        db::create_session(
            pool,
            &sid,
            &pid,
            "/tmp",
            "/tmp",
            "Test Session",
            "build",
            r#"{"provider_id":"anthropic","model_id":"claude-opus-4-8"}"#,
        )
        .await
        .unwrap();
        sid
    }

    async fn append(pool: &sqlx::SqlitePool, sid: &str, seq: i64, mtype: &str, data: &str) {
        let mut tx = pool.begin().await.unwrap();
        db::append_message(&mut tx, &Uuid::new_v4().to_string(), sid, seq, mtype, data)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    #[tokio::test]
    async fn export_markdown_includes_title() {
        let pool = setup_pool().await;
        let sid = seed_session(&pool).await;
        let md = export_session_markdown(&pool, &sid).await.unwrap();
        assert!(md.contains("Test Session"));
    }

    /// Markdown renders ChatMessage CONTENT cleanly (text + tool blocks), not the
    /// raw `{id,role,content,created_at}` JSON the old renderer dumped.
    #[tokio::test]
    async fn export_markdown_renders_content_not_raw_json() {
        let pool = setup_pool().await;
        let sid = seed_session(&pool).await;
        append(
            &pool,
            &sid,
            1,
            "user",
            r#"{"id":"u1","role":"user","content":[{"type":"text","text":"hello there"}],"created_at":1}"#,
        )
        .await;
        append(
            &pool,
            &sid,
            2,
            "assistant",
            r#"{"id":"a1","role":"assistant","content":[{"type":"text","text":"using a tool"},{"type":"tool_use","id":"t1","name":"read_file","input":{"path":"x.rs"}}],"created_at":2}"#,
        )
        .await;

        let md = export_session_markdown(&pool, &sid).await.unwrap();
        assert!(md.contains("hello there"), "user text rendered");
        assert!(md.contains("using a tool"), "assistant text rendered");
        assert!(md.contains("**Tool:** `read_file`"), "tool block rendered");
        // The raw ChatMessage wrapper keys must NOT leak into the output.
        assert!(
            !md.contains("\"created_at\""),
            "must not dump raw ChatMessage JSON"
        );
    }

    /// A compaction marker row renders as a divider, not raw JSON.
    #[tokio::test]
    async fn export_markdown_renders_compaction_divider() {
        let pool = setup_pool().await;
        let sid = seed_session(&pool).await;
        append(
            &pool,
            &sid,
            1,
            "compaction",
            r#"{"compacted_through_seq":5,"summary":"older turns folded"}"#,
        )
        .await;
        let md = export_session_markdown(&pool, &sid).await.unwrap();
        assert!(md.contains("history compacted"));
        assert!(!md.contains("compacted_through_seq"), "no raw marker JSON");
    }

    /// JSON export round-trips into a structured object with session + messages.
    #[tokio::test]
    async fn export_json_round_trips() {
        let pool = setup_pool().await;
        let sid = seed_session(&pool).await;
        append(
            &pool,
            &sid,
            1,
            "user",
            r#"{"id":"u1","role":"user","content":[{"type":"text","text":"hi"}],"created_at":1}"#,
        )
        .await;
        let json = export_session_json(&pool, &sid).await.unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["session"]["title"], "Test Session");
        assert_eq!(v["messages"].as_array().unwrap().len(), 1);
    }

    /// to_file writes the chosen format to disk.
    #[tokio::test]
    async fn export_to_file_writes_content() {
        let pool = setup_pool().await;
        let sid = seed_session(&pool).await;
        let dir = tempfile::tempdir().unwrap();
        let md_path = dir.path().join("out.md");
        export_session_to_file(&pool, &sid, &md_path, "markdown")
            .await
            .unwrap();
        let written = std::fs::read_to_string(&md_path).unwrap();
        assert!(written.contains("# Session: Test Session"));

        let json_path = dir.path().join("out.json");
        export_session_to_file(&pool, &sid, &json_path, "json")
            .await
            .unwrap();
        let written = std::fs::read_to_string(&json_path).unwrap();
        assert!(serde_json::from_str::<Value>(&written).is_ok());
    }

    /// session-not-found surfaces an error rather than panicking.
    #[tokio::test]
    async fn export_missing_session_errors() {
        let pool = setup_pool().await;
        assert!(export_session_markdown(&pool, "nope").await.is_err());
        assert!(export_session_json(&pool, "nope").await.is_err());
    }
}
