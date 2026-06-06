//! Code-intelligence context retrieval (Phase 4, Step 4.7): pull the symbols a
//! prompt likely refers to out of the FTS5 index, so the orchestrator can inject
//! their definitions/locations into the turn. This is the per-prompt counterpart
//! to the epoch-stable repo map.

use crate::symbols::{self, SymbolHit};
use sqlx::SqlitePool;
use std::collections::HashSet;

/// Symbols relevant to `prompt`: extract candidate identifiers, search the index
/// for each, and return de-duplicated hits (best bm25 first), capped to `limit`.
pub async fn relevant_symbols(
    pool: &SqlitePool,
    prompt: &str,
    limit: usize,
) -> Result<Vec<SymbolHit>, sqlx::Error> {
    let mut seen = HashSet::new();
    let mut hits = Vec::new();
    for term in identifier_terms(prompt) {
        for hit in symbols::search_symbols(pool, &term, limit as i64).await? {
            if seen.insert(hit.symbol_uid.clone()) {
                hits.push(hit);
            }
        }
        if hits.len() >= limit {
            break;
        }
    }
    hits.truncate(limit);
    Ok(hits)
}

/// Render retrieved symbols as a compact context block (empty when none).
pub fn format_relevant(hits: &[SymbolHit]) -> String {
    if hits.is_empty() {
        return String::new();
    }
    let mut s = String::from("Relevant code symbols (from the index):\n");
    for h in hits {
        s.push_str(&format!(
            "  {} [{}] — {}:{}\n",
            h.signature.trim(),
            h.kind,
            h.filepath,
            h.start_line
        ));
    }
    s
}

/// Identifier-like tokens worth searching: split on non-identifier chars, keep
/// tokens ≥ 3 chars that contain a letter and aren't common prose/keyword
/// stopwords. De-duplicated, order-preserving, capped so a long prompt can't fan
/// out into a huge number of index queries.
fn identifier_terms(prompt: &str) -> Vec<String> {
    const MAX_TERMS: usize = 12;
    const STOPWORDS: &[&str] = &[
        "the", "and", "for", "this", "that", "with", "you", "your", "use", "fn", "let", "pub",
        "mut", "ref", "can", "should", "would", "could", "please", "code", "file", "function",
        "struct", "trait", "enum", "impl", "add", "fix", "make", "new", "all", "any", "how",
        "what", "why", "when", "where", "into", "from", "out",
    ];
    let mut seen = HashSet::new();
    let mut terms = Vec::new();
    for raw in prompt.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if raw.len() < 3 || !raw.chars().any(|c| c.is_alphabetic()) {
            continue;
        }
        let lower = raw.to_ascii_lowercase();
        if STOPWORDS.contains(&lower.as_str()) {
            continue;
        }
        if seen.insert(lower) {
            terms.push(raw.to_string());
            if terms.len() >= MAX_TERMS {
                break;
            }
        }
    }
    terms
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect_db, run_migrations};
    use private_code_codeintel::SymbolExtractor;

    #[test]
    fn identifier_terms_keeps_identifiers_drops_stopwords() {
        let terms = identifier_terms("Please fix the AppConfig load_settings bug in x");
        assert!(terms.contains(&"AppConfig".to_string()));
        assert!(terms.contains(&"load_settings".to_string()));
        assert!(!terms.iter().any(|t| t == "the" || t == "fix" || t == "x"));
    }

    #[tokio::test]
    async fn relevant_symbols_finds_mentioned_symbols() {
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        let src =
            "pub struct AppConfig {}\nimpl AppConfig { pub fn load() {} }\npub fn unrelated() {}\n";
        let syms = SymbolExtractor::new().extract("src/config.rs", src);
        symbols::index_file(&pool, "src/config.rs", &syms)
            .await
            .unwrap();

        let hits = relevant_symbols(&pool, "how does AppConfig get loaded?", 10)
            .await
            .unwrap();
        assert!(hits.iter().any(|h| h.name == "AppConfig"));

        let block = format_relevant(&hits);
        assert!(block.contains("AppConfig"));
        assert!(block.contains("src/config.rs"));
    }

    #[tokio::test]
    async fn relevant_symbols_empty_when_no_match() {
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        let hits = relevant_symbols(&pool, "refactor the WidgetFactory", 10)
            .await
            .unwrap();
        assert!(hits.is_empty());
        assert_eq!(format_relevant(&hits), "");
    }
}
