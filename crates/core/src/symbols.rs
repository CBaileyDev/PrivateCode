//! DB-backed code-symbol index & search (Phase 4, Step 4.3).
//!
//! The schema (`symbols` + the `symbols_fts` external-content FTS5 table + sync
//! triggers) ships in the `0002_symbols` migration. Symbols come from
//! `private_code_codeintel::SymbolExtractor`. All SQLite access is centralized in
//! core (per `specs/database.md`), so the code-intel crate stays pure-CPU.

use private_code_codeintel::Symbol;
use sqlx::SqlitePool;

/// One FTS5 search hit: the symbol plus its bm25 `rank` (smaller = better match).
#[derive(Debug, Clone, PartialEq)]
pub struct SymbolHit {
    pub symbol_uid: String,
    pub filepath: String,
    pub name: String,
    pub kind: String,
    pub start_line: i64,
    pub end_line: i64,
    pub signature: String,
    pub rank: f64,
}

/// Replace **all** symbols for `filepath` with `symbols` in one transaction —
/// the atomic re-index unit. Delete-then-insert keeps the FTS5 external-content
/// index in sync through the `symbols_ad`/`symbols_ai` triggers. Idempotent per
/// file, so re-indexing an unchanged file is a no-op-equivalent (same rows back).
pub async fn index_file(
    pool: &SqlitePool,
    filepath: &str,
    symbols: &[Symbol],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM symbols WHERE filepath = ?1")
        .bind(filepath)
        .execute(&mut *tx)
        .await?;
    for s in symbols {
        sqlx::query(
            "INSERT INTO symbols \
             (symbol_uid, filepath, name, kind, start_line, start_column, end_line, end_column, signature, parent_scope) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .bind(&s.symbol_uid)
        .bind(&s.filepath)
        .bind(&s.name)
        .bind(&s.kind)
        .bind(s.start_line as i64)
        .bind(s.start_column as i64)
        .bind(s.end_line as i64)
        .bind(s.end_column as i64)
        .bind(&s.signature)
        .bind(&s.parent_scope)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Remove all symbols for a deleted or moved file.
pub async fn remove_file(pool: &SqlitePool, filepath: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM symbols WHERE filepath = ?1")
        .bind(filepath)
        .execute(pool)
        .await?;
    Ok(())
}

/// Full-text symbol search via FTS5 `MATCH`, bm25-ranked (best first), capped to
/// `limit`. The raw `query` is sanitized into a safe prefix query (see
/// [`fts5_prefix_query`]) so caller/model input can't inject FTS5 operators or
/// trigger a syntax error. An all-punctuation query yields no results.
pub async fn search_symbols(
    pool: &SqlitePool,
    query: &str,
    limit: i64,
) -> Result<Vec<SymbolHit>, sqlx::Error> {
    let match_query = fts5_prefix_query(query);
    if match_query.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as::<_, (String, String, String, String, i64, i64, String, f64)>(
        "SELECT s.symbol_uid, s.filepath, s.name, s.kind, s.start_line, s.end_line, s.signature, \
                bm25(symbols_fts) AS rank \
         FROM symbols_fts JOIN symbols s ON symbols_fts.rowid = s.id \
         WHERE symbols_fts MATCH ?1 ORDER BY rank LIMIT ?2",
    )
    .bind(&match_query)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(symbol_uid, filepath, name, kind, start_line, end_line, signature, rank)| SymbolHit {
                symbol_uid,
                filepath,
                name,
                kind,
                start_line,
                end_line,
                signature,
                rank,
            },
        )
        .collect())
}

/// Rebuild the FTS5 external-content index from the base table — the documented
/// recovery path if it ever desyncs (e.g. a VACUUM run without the rowid alias).
pub async fn rebuild_fts(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO symbols_fts(symbols_fts) VALUES('rebuild')")
        .execute(pool)
        .await?;
    Ok(())
}

/// All indexed symbols, ordered by `(filepath, start_line)` — the natural order
/// for the structural repo map (parents precede their methods within a file).
pub async fn all_symbols(pool: &SqlitePool) -> Result<Vec<Symbol>, sqlx::Error> {
    let rows = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            String,
            i64,
            i64,
            i64,
            i64,
            String,
            Option<String>,
        ),
    >(
        "SELECT symbol_uid, filepath, name, kind, start_line, start_column, end_line, end_column, \
                signature, parent_scope \
         FROM symbols ORDER BY filepath, start_line",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(symbol_uid, filepath, name, kind, sl, sc, el, ec, signature, parent_scope)| Symbol {
                symbol_uid,
                filepath,
                name,
                kind,
                start_line: sl as u32,
                start_column: sc as u32,
                end_line: el as u32,
                end_column: ec as u32,
                signature,
                parent_scope,
            },
        )
        .collect())
}

/// Turn an arbitrary query string into a safe FTS5 prefix query: split on
/// non-alphanumerics and emit `term*` per token (AND-joined). Stripping to
/// alphanumerics means no token can contain an FTS5 operator/quote, so the
/// result is always syntactically valid. Empty when the query has no alnum runs.
fn fts5_prefix_query(query: &str) -> String {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| format!("{t}*"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect_db, run_migrations};
    use private_code_codeintel::SymbolExtractor;

    async fn fresh_pool() -> SqlitePool {
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        pool
    }

    const SAMPLE: &str = r#"
pub struct AppConfig { path: String }
impl AppConfig {
    pub fn load() -> AppConfig { AppConfig { path: String::new() } }
    pub fn save(&self) {}
}
pub fn unrelated_helper() {}
"#;

    #[tokio::test]
    async fn migration_applies_and_fts5_is_available() {
        // run_migrations succeeding here proves 0002 (incl. CREATE VIRTUAL TABLE
        // … USING fts5) applies — i.e. FTS5 is compiled into sqlx's SQLite.
        let _pool = fresh_pool().await;
    }

    #[tokio::test]
    async fn index_and_search_finds_symbols_by_prefix() {
        let pool = fresh_pool().await;
        let syms = SymbolExtractor::new().extract("src/config.rs", SAMPLE);
        index_file(&pool, "src/config.rs", &syms).await.unwrap();

        let hits = search_symbols(&pool, "AppConfig", 10).await.unwrap();
        assert!(
            hits.iter()
                .any(|h| h.name == "AppConfig" && h.kind == "struct"),
            "AppConfig struct must be found, got {hits:?}"
        );
        // Prefix match: "App" finds AppConfig (single-token prefix).
        assert!(
            search_symbols(&pool, "App", 10)
                .await
                .unwrap()
                .iter()
                .any(|h| h.name == "AppConfig")
        );
        // A bare-name method is searchable.
        assert!(
            search_symbols(&pool, "load", 10)
                .await
                .unwrap()
                .iter()
                .any(|h| h.name == "load")
        );
    }

    #[tokio::test]
    async fn reindex_replaces_old_symbols_keeping_fts_in_sync() {
        let pool = fresh_pool().await;
        let v1 = SymbolExtractor::new().extract("src/a.rs", "pub fn alpha() {}");
        index_file(&pool, "src/a.rs", &v1).await.unwrap();
        assert!(!search_symbols(&pool, "alpha", 10).await.unwrap().is_empty());

        // Re-index the same file with a different symbol set.
        let v2 = SymbolExtractor::new().extract("src/a.rs", "pub fn beta() {}");
        index_file(&pool, "src/a.rs", &v2).await.unwrap();
        assert!(
            search_symbols(&pool, "alpha", 10).await.unwrap().is_empty(),
            "the AFTER DELETE trigger must purge the old symbol from FTS"
        );
        assert!(!search_symbols(&pool, "beta", 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn remove_file_clears_symbols() {
        let pool = fresh_pool().await;
        let syms = SymbolExtractor::new().extract("src/config.rs", SAMPLE);
        index_file(&pool, "src/config.rs", &syms).await.unwrap();
        remove_file(&pool, "src/config.rs").await.unwrap();
        assert!(
            search_symbols(&pool, "AppConfig", 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn rebuild_then_search_still_works() {
        let pool = fresh_pool().await;
        let syms = SymbolExtractor::new().extract("src/config.rs", SAMPLE);
        index_file(&pool, "src/config.rs", &syms).await.unwrap();
        rebuild_fts(&pool).await.unwrap();
        assert!(
            !search_symbols(&pool, "AppConfig", 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn punctuation_only_query_is_safe_and_empty() {
        let pool = fresh_pool().await;
        assert!(
            search_symbols(&pool, "  *(){} ", 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(fts5_prefix_query("AppConfig::load"), "AppConfig* load*");
        assert_eq!(fts5_prefix_query("!!!"), "");
    }
}
