//! Interactive fuzzy symbol search (Phase 4, Step 4.4) over the `nucleo` matcher
//! (Helix's). Complements the FTS5 full-text search in [`crate::symbols`]: FTS5
//! is for prefix/token lookup, nucleo for forgiving interactive matching.
//!
//! The matcher is built **per query** over a caller-supplied candidate slice — we
//! deliberately never hold a resident matcher of all symbols (that would blow the
//! <30MB idle budget). The interactive flow is "narrow with FTS5, fuzzy-rank the
//! candidates".

use nucleo::{Config, Matcher, Utf32Str};
use private_code_codeintel::Symbol;

/// Fuzzy-match `query` against each symbol's name, returning the matches sorted
/// by descending score (best first), capped to `limit`. An empty query matches
/// nothing. Non-matching symbols are dropped.
pub fn fuzzy_search(query: &str, symbols: &[Symbol], limit: usize) -> Vec<(Symbol, u16)> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut needle_buf = Vec::new();
    let needle = Utf32Str::new(query, &mut needle_buf);

    let mut hay_buf = Vec::new();
    let mut scored: Vec<(Symbol, u16)> = Vec::new();
    for sym in symbols {
        let haystack = Utf32Str::new(&sym.name, &mut hay_buf);
        if let Some(score) = matcher.fuzzy_match(haystack, needle) {
            scored.push((sym.clone(), score));
        }
    }
    // Stable-ish ordering: score desc, then name for determinism on ties.
    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.name.cmp(&b.0.name)));
    scored.truncate(limit);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(name: &str) -> Symbol {
        Symbol {
            symbol_uid: format!("f|1|0|{name}"),
            filepath: "src/lib.rs".into(),
            name: name.into(),
            kind: "function".into(),
            start_line: 1,
            start_column: 0,
            end_line: 1,
            end_column: 0,
            signature: format!("fn {name}()"),
            parent_scope: None,
        }
    }

    #[test]
    fn fuzzy_ranks_matches_and_drops_non_matches() {
        let syms = vec![
            sym("AppConfig"),
            sym("ApplicationContext"),
            sym("totally_unrelated"),
        ];
        let hits = fuzzy_search("appcfg", &syms, 10);
        assert!(
            hits.iter().any(|(s, _)| s.name == "AppConfig"),
            "subsequence 'appcfg' should match AppConfig, got {hits:?}"
        );
        assert!(
            !hits.iter().any(|(s, _)| s.name == "totally_unrelated"),
            "a non-subsequence must not match"
        );
    }

    #[test]
    fn closer_match_outranks_looser_one() {
        let syms = vec![sym("load"), sym("lottery_odds_assigned_daily")];
        let hits = fuzzy_search("load", &syms, 10);
        assert_eq!(
            hits.first().unwrap().0.name,
            "load",
            "exact-ish match ranks first"
        );
    }

    #[test]
    fn empty_query_and_limit() {
        let syms = vec![sym("a"), sym("ab"), sym("abc")];
        assert!(fuzzy_search("", &syms, 10).is_empty());
        assert!(fuzzy_search("a", &syms, 2).len() <= 2);
    }
}
