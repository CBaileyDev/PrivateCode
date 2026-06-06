//! Maps file extensions to a tree-sitter grammar + symbol-extraction query,
//! building (and caching) each language's compiled [`Query`] **lazily on first
//! use** so the cold-start budget isn't spent on grammars the session never
//! touches. Phase-4 C1 ships Rust only; C7 widens the `ext_to_lang` /
//! `build_lang` match to the full 8-language set.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tree_sitter::{Language, Query};

/// A loaded grammar plus its compiled extraction query.
pub struct LangDef {
    pub language: Language,
    pub query: Query,
}

/// Lazily-populated extension → grammar registry.
#[derive(Default)]
pub struct LanguageRegistry {
    cache: Mutex<HashMap<&'static str, Arc<LangDef>>>,
}

impl LanguageRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The `LangDef` for a path's extension, built on first use and cached.
    /// `None` for unsupported extensions or if the (static) query fails to
    /// compile against the grammar — that is a programming error, logged loudly.
    pub fn for_path(&self, path: &str) -> Option<Arc<LangDef>> {
        let ext = extension(path)?;
        let lang_id = ext_to_lang(&ext)?;
        let mut cache = self.cache.lock().expect("language registry mutex poisoned");
        if let Some(def) = cache.get(lang_id) {
            return Some(def.clone());
        }
        match build_lang(lang_id) {
            Ok(def) => {
                let arc = Arc::new(def);
                cache.insert(lang_id, arc.clone());
                Some(arc)
            }
            Err(e) => {
                tracing::error!("codeintel: failed to build grammar '{lang_id}': {e}");
                None
            }
        }
    }
}

/// Lowercased file extension (no dot), or `None` if there is none. A leading-dot
/// dotfile (`.gitignore`) has no extension — its stem is empty.
fn extension(path: &str) -> Option<String> {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let (stem, ext) = file.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() {
        None
    } else {
        Some(ext.to_ascii_lowercase())
    }
}

/// Map an extension to a stable language id. Extended in C7.
fn ext_to_lang(ext: &str) -> Option<&'static str> {
    match ext {
        "rs" => Some("rust"),
        _ => None,
    }
}

/// Construct a language's grammar + compiled query. Extended in C7.
fn build_lang(lang_id: &'static str) -> Result<LangDef, tree_sitter::QueryError> {
    match lang_id {
        "rust" => {
            let language: Language = tree_sitter_rust::LANGUAGE.into();
            let query = Query::new(&language, include_str!("../queries/rust.scm"))?;
            Ok(LangDef { language, query })
        }
        // Unreachable: ext_to_lang only yields ids handled here.
        other => unreachable!("ext_to_lang produced unhandled language id '{other}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_rust_extension_and_caches() {
        let reg = LanguageRegistry::new();
        assert!(reg.for_path("src/main.rs").is_some());
        // Cached on the second call (same Arc-backed def).
        assert!(reg.for_path("/abs/path/lib.rs").is_some());
        assert!(reg.for_path("README.md").is_none());
        assert!(reg.for_path("no_extension").is_none());
    }

    #[test]
    fn extension_helper_handles_paths() {
        assert_eq!(extension("a/b/c.rs").as_deref(), Some("rs"));
        assert_eq!(extension("C.RS").as_deref(), Some("rs"));
        assert_eq!(extension("Makefile"), None);
        assert_eq!(extension(".gitignore"), None);
    }
}
