//! Structural repo map (Phase 4, Step 4.6): a compact, token-budgeted overview
//! of the workspace's symbols, rendered from the FTS5 index. Injected into the
//! system context as the `code/repomap` source so its updates ride the same
//! context-epoch machinery as date/env/instructions.
//!
//! ```text
//! src/config.rs
//!   pub struct AppConfig
//!   impl AppConfig
//!     pub fn load() -> AppConfig
//!     pub fn save(&self)
//! ```

use crate::symbols;
use private_code_codeintel::Symbol;
use sqlx::SqlitePool;

/// Default repo-map token budget (≈ chars/4 estimate). Keeps the map from
/// dominating the cached system prefix on large repos.
pub const DEFAULT_REPOMAP_TOKEN_BUDGET: usize = 4_000;

/// Generate the repo map from the current index, capped to `token_budget`.
pub async fn generate(pool: &SqlitePool, token_budget: usize) -> Result<String, sqlx::Error> {
    let syms = symbols::all_symbols(pool).await?;
    Ok(render(&syms, token_budget))
}

/// Render `symbols` (assumed ordered by filepath then start_line) into the map,
/// ranked by structural richness (symbol count desc, then path) and truncated to
/// the token budget. Pure, so it is unit-testable without a DB.
///
/// Ranking is pragmatic (advisor: don't rathole): symbol-count proxies
/// "importance"; the reference's recency / cross-file-reference / cwd-proximity
/// signals are a later refinement.
pub fn render(symbols: &[Symbol], token_budget: usize) -> String {
    // Group consecutive same-file symbols (input is sorted by filepath).
    let mut groups: Vec<(&str, Vec<&Symbol>)> = Vec::new();
    for s in symbols {
        match groups.last_mut() {
            Some((path, v)) if *path == s.filepath => v.push(s),
            _ => groups.push((&s.filepath, vec![s])),
        }
    }
    // Rank: richer files (more symbols) first, ties by path for determinism.
    groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));

    let mut out = String::new();
    let mut used_tokens = 0usize;
    let mut omitted = 0usize;

    for (path, syms) in &groups {
        let remaining = token_budget.saturating_sub(used_tokens);
        // A file needs at least its header plus one symbol line to be worth
        // emitting; if the remaining budget can't cover the header, omit the file
        // entirely. This (unlike the prior `!out.is_empty()` exemption) means even
        // the FIRST, richest file is bounded — a single huge file can no longer
        // blow the budget by orders of magnitude.
        let header_tokens = est_tokens(&format!("{path}\n"));
        if remaining <= header_tokens {
            omitted += 1;
            continue;
        }
        let block = render_file_block(path, syms, remaining);
        used_tokens += est_tokens(&block);
        out.push_str(&block);
    }

    if omitted > 0 {
        out.push_str(&format!("… ({omitted} more file(s) omitted for budget)\n"));
    }
    out
}

/// Render one file's block, bounded to `max_tokens`: emit the header then as many
/// symbol lines as fit, replacing the overflow tail with a per-file "… N more
/// symbol(s) omitted" marker. Bounding INTRA-block (not just whole-block) is what
/// stops a single large file from overflowing the budget.
fn render_file_block(path: &str, syms: &[&Symbol], max_tokens: usize) -> String {
    let mut block = format!("{path}\n");
    let mut omitted_in_file = 0usize;
    for (i, s) in syms.iter().enumerate() {
        // Top-level items at 2 spaces; members (with a parent scope) at 4.
        let indent = if s.parent_scope.is_some() {
            "    "
        } else {
            "  "
        };
        let line = format!("{indent}{}\n", compact_signature(s));
        if est_tokens(&block) + est_tokens(&line) > max_tokens {
            omitted_in_file = syms.len() - i;
            break;
        }
        block.push_str(&line);
    }
    if omitted_in_file > 0 {
        block.push_str(&format!(
            "  … ({omitted_in_file} more symbol(s) omitted for budget)\n"
        ));
    }
    block
}

/// A compact one-line label for the map: the extracted signature with a leading
/// visibility modifier stripped (`pub `, `pub(crate) `), falling back to
/// `{kind} {name}` if the signature is empty.
fn compact_signature(s: &Symbol) -> String {
    let sig = s.signature.trim();
    let sig = sig
        .strip_prefix("pub(crate) ")
        .or_else(|| sig.strip_prefix("pub(super) "))
        .or_else(|| sig.strip_prefix("pub "))
        .unwrap_or(sig);
    if sig.is_empty() {
        format!("{} {}", s.kind, s.name)
    } else {
        sig.to_string()
    }
}

/// Coarse token estimate (≈ 4 chars/token) — matches the provider-budgeting
/// heuristic used elsewhere; precise counting lands with the model catalog.
fn est_tokens(s: &str) -> usize {
    s.len().div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(
        file: &str,
        name: &str,
        kind: &str,
        line: u32,
        sig: &str,
        parent: Option<&str>,
    ) -> Symbol {
        Symbol {
            symbol_uid: Symbol::make_uid(file, line, 0, name),
            filepath: file.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            start_line: line,
            start_column: 0,
            end_line: line,
            end_column: 0,
            signature: sig.to_string(),
            parent_scope: parent.map(str::to_string),
        }
    }

    #[test]
    fn renders_nested_tree_with_indentation_and_strips_pub() {
        let syms = vec![
            sym(
                "src/config.rs",
                "AppConfig",
                "struct",
                1,
                "pub struct AppConfig",
                None,
            ),
            sym(
                "src/config.rs",
                "AppConfig",
                "impl",
                2,
                "impl AppConfig",
                None,
            ),
            sym(
                "src/config.rs",
                "load",
                "function",
                3,
                "pub fn load() -> AppConfig",
                Some("AppConfig"),
            ),
            sym(
                "src/config.rs",
                "save",
                "function",
                4,
                "pub fn save(&self)",
                Some("AppConfig"),
            ),
        ];
        let map = render(&syms, DEFAULT_REPOMAP_TOKEN_BUDGET);
        let expected = "src/config.rs\n  struct AppConfig\n  impl AppConfig\n    fn load() -> AppConfig\n    fn save(&self)\n";
        assert_eq!(map, expected);
    }

    #[test]
    fn ranks_richer_files_first() {
        let syms = vec![
            sym("a.rs", "one", "function", 1, "fn one()", None),
            sym("b.rs", "x", "function", 1, "fn x()", None),
            sym("b.rs", "y", "function", 2, "fn y()", None),
        ];
        let map = render(&syms, DEFAULT_REPOMAP_TOKEN_BUDGET);
        // b.rs (2 symbols) ranks before a.rs (1 symbol).
        assert!(
            map.find("b.rs").unwrap() < map.find("a.rs").unwrap(),
            "got:\n{map}"
        );
    }

    #[test]
    fn respects_token_budget() {
        let mut syms = Vec::new();
        for f in 0..50 {
            syms.push(sym(
                &format!("file_{f}.rs"),
                "f",
                "function",
                1,
                "fn f() -> SomethingFairlyLong",
                None,
            ));
        }
        // Tiny budget: only the first block fits; the rest are summarized.
        let map = render(&syms, 12);
        assert!(map.contains("more file(s) omitted"), "got:\n{map}");
        // At most one or two file blocks rendered.
        let files_rendered = map.matches(".rs\n").count();
        assert!(
            files_rendered <= 2,
            "budget should cap rendered files, got {files_rendered}"
        );
    }

    /// A SINGLE large file must not overflow the budget: intra-block truncation
    /// keeps even the first (richest) file's block bounded, with a per-file
    /// "symbol(s) omitted" marker. (Regression for the C11 finding where the first
    /// block was emitted in full, blowing the cap ~50-80x.)
    #[test]
    fn single_large_file_does_not_overflow_the_budget() {
        let mut syms = Vec::new();
        for i in 0..400 {
            syms.push(sym(
                "src/huge.rs",
                &format!("symbol_number_{i}"),
                "function",
                i as u32 + 1,
                &format!("fn symbol_number_{i}(arg: SomeReasonablyLongType) -> Result<(), Error>"),
                None,
            ));
        }
        let budget = 100;
        let map = render(&syms, budget);
        let used = map.len().div_ceil(4);
        assert!(
            used <= budget * 2,
            "a single large file must stay near the budget, used ~{used} tokens for budget {budget}"
        );
        assert!(
            map.contains("more symbol(s) omitted"),
            "the overflow tail must be summarized, got:\n{map}"
        );
    }

    #[tokio::test]
    async fn generate_reads_from_the_live_index() {
        use crate::db::{connect_db, run_migrations};
        use private_code_codeintel::SymbolExtractor;

        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        let src = "pub struct AppConfig {}\nimpl AppConfig { pub fn load() {} }\n";
        let syms = SymbolExtractor::new().extract("src/config.rs", src);
        symbols::index_file(&pool, "src/config.rs", &syms)
            .await
            .unwrap();

        let map = generate(&pool, DEFAULT_REPOMAP_TOKEN_BUDGET).await.unwrap();
        assert!(map.contains("src/config.rs"));
        assert!(map.contains("struct AppConfig"));
        assert!(map.contains("fn load()"));
    }
}
