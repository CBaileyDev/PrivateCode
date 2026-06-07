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
    if let Ok(content) = serde_json::from_str::<Value>(&msg.data) {
        if let Some(text) = content.as_str() {
            md.push_str(text);
            md.push('\n');
        } else if let Some(arr) = content.as_array() {
            for block in arr {
                if let Some(t) = block["text"].as_str() {
                    md.push_str(t);
                    md.push('\n');
                } else if let Some(name) = block["name"].as_str() {
                    md.push_str(&format!("**Tool:** `{name}`\n\n"));
                    if let Ok(pretty) = serde_json::to_string_pretty(&block["input"]) {
                        md.push_str("```json\n");
                        md.push_str(&pretty);
                        md.push_str("\n```\n");
                    }
                }
            }
        } else {
            md.push_str("```json\n");
            md.push_str(&msg.data);
            md.push_str("\n```\n");
        }
    } else {
        md.push_str(&msg.data);
        md.push('\n');
    }
    md.push('\n');
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

    #[tokio::test]
    async fn export_markdown_includes_title() {
        let pool = setup_pool().await;
        let pid = Uuid::new_v4().to_string();
        db::create_project(&pool, &pid, "p", "/tmp").await.unwrap();
        let sid = Uuid::new_v4().to_string();
        db::create_session(
            &pool,
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
        let md = export_session_markdown(&pool, &sid).await.unwrap();
        assert!(md.contains("Test Session"));
    }
}
