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

/// Map a file extension to a stable language id. `.h` is treated as C (a bare
/// header parses fine under the C grammar; C++-specific headers conventionally
/// use .hpp/.hh).
fn ext_to_lang(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "py" | "pyi" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "java" => "java",
        "rb" => "ruby",
        "php" | "phtml" => "php",
        _ => return None,
    })
}

/// Construct a language's grammar + compiled query. Each grammar crate exposes a
/// `LANGUAGE` (or language-specific) const; the `.scm` queries are bundled at
/// compile time via `include_str!`.
fn build_lang(lang_id: &'static str) -> Result<LangDef, tree_sitter::QueryError> {
    let (language, query_src): (Language, &str) = match lang_id {
        "rust" => (
            tree_sitter_rust::LANGUAGE.into(),
            include_str!("../queries/rust.scm"),
        ),
        "typescript" => (
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            include_str!("../queries/typescript.scm"),
        ),
        "javascript" => (
            tree_sitter_javascript::LANGUAGE.into(),
            include_str!("../queries/javascript.scm"),
        ),
        "python" => (
            tree_sitter_python::LANGUAGE.into(),
            include_str!("../queries/python.scm"),
        ),
        "go" => (
            tree_sitter_go::LANGUAGE.into(),
            include_str!("../queries/go.scm"),
        ),
        "c" => (
            tree_sitter_c::LANGUAGE.into(),
            include_str!("../queries/c.scm"),
        ),
        "cpp" => (
            tree_sitter_cpp::LANGUAGE.into(),
            include_str!("../queries/cpp.scm"),
        ),
        "java" => (
            tree_sitter_java::LANGUAGE.into(),
            include_str!("../queries/java.scm"),
        ),
        "ruby" => (
            tree_sitter_ruby::LANGUAGE.into(),
            include_str!("../queries/ruby.scm"),
        ),
        "php" => (
            tree_sitter_php::LANGUAGE_PHP.into(),
            include_str!("../queries/php.scm"),
        ),
        // Unreachable: ext_to_lang only yields ids handled here.
        other => unreachable!("ext_to_lang produced unhandled language id '{other}'"),
    };
    let query = Query::new(&language, query_src)?;
    Ok(LangDef { language, query })
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
