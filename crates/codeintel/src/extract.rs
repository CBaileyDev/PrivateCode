//! `SymbolExtractor::extract(path, source) -> Vec<Symbol>` — parses a source
//! file with the extension's tree-sitter grammar and runs its extraction query,
//! producing structured [`Symbol`]s for the FTS5 index and the repo map.

use crate::registry::LanguageRegistry;
use crate::symbol::Symbol;
use tree_sitter::{Node, Parser, QueryCursor, StreamingIterator};

/// Stateless across calls except for the lazily-populated grammar registry.
#[derive(Default)]
pub struct SymbolExtractor {
    registry: LanguageRegistry,
}

impl SymbolExtractor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Extract symbols from `source` (the file at `path`, used for the language
    /// pick and the symbol filepath). Returns an empty vec for unsupported
    /// extensions or unparseable input — extraction is best-effort and never
    /// fails a turn.
    pub fn extract(&self, path: &str, source: &str) -> Vec<Symbol> {
        let Some(def) = self.registry.for_path(path) else {
            return Vec::new();
        };
        let mut parser = Parser::new();
        if parser.set_language(&def.language).is_err() {
            return Vec::new();
        }
        let Some(tree) = parser.parse(source, None) else {
            return Vec::new();
        };

        let src = source.as_bytes();
        let capture_names = def.query.capture_names();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&def.query, tree.root_node(), src);
        let mut out = Vec::new();

        // tree-sitter 0.25+ yields a StreamingIterator, not a std Iterator.
        while let Some(m) = matches.next() {
            let mut name_node: Option<Node> = None;
            let mut def_node: Option<Node> = None;
            let mut kind: Option<&str> = None;

            for cap in m.captures {
                let cap_name = capture_names[cap.index as usize];
                if cap_name == "name" {
                    name_node = Some(cap.node);
                } else {
                    kind = Some(cap_name);
                    def_node = Some(cap.node);
                }
            }

            let (Some(name_node), Some(def_node), Some(kind)) = (name_node, def_node, kind) else {
                continue;
            };
            let Ok(name) = name_node.utf8_text(src) else {
                continue;
            };

            let start = def_node.start_position();
            let end = def_node.end_position();
            let start_line = start.row as u32 + 1; // 1-based for humans
            let start_column = start.column as u32; // 0-based (tree-sitter native)

            out.push(Symbol {
                symbol_uid: Symbol::make_uid(path, start_line, start_column, name),
                filepath: path.to_string(),
                name: name.to_string(),
                kind: kind.to_string(),
                start_line,
                start_column,
                end_line: end.row as u32 + 1,
                end_column: end.column as u32,
                signature: signature_of(def_node, src),
                parent_scope: enclosing_scope(def_node, src),
            });
        }

        out
    }
}

/// Pragmatic signature: text from the node start to the body (the `body` field —
/// a block / declaration list), whitespace-collapsed. Bodyless items (consts,
/// type aliases) fall back to the first line. Never a compiler-grade signature.
fn signature_of(node: Node, src: &[u8]) -> String {
    let full = node.utf8_text(src).unwrap_or("");
    let raw = match node.child_by_field_name("body") {
        Some(body) => {
            let end = body.start_byte().saturating_sub(node.start_byte());
            full.get(..end).unwrap_or(full)
        }
        None => full.lines().next().unwrap_or(full),
    };
    normalize_ws(raw)
}

/// The name of the nearest enclosing `impl`/`trait`/`mod`/`struct`/`enum`
/// ancestor (e.g. a method's `impl` type), or `None` for a top-level item.
fn enclosing_scope(node: Node, src: &[u8]) -> Option<String> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        let name_field = match n.kind() {
            // impl's identifying name is the implemented `type`, not a `name` field.
            "impl_item" => n
                .child_by_field_name("type")
                .map(|t| innermost_type_name(t, src)),
            "trait_item" | "mod_item" | "struct_item" | "enum_item" | "union_item" => n
                .child_by_field_name("name")
                .and_then(|t| t.utf8_text(src).ok())
                .map(str::to_string),
            _ => None,
        };
        if let Some(name) = name_field {
            return Some(name);
        }
        cur = n.parent();
    }
    None
}

/// Resolve an impl `type` node to a bare name: `Foo` from `Foo`, `Foo<T>`, or
/// `path::Foo` (best-effort).
fn innermost_type_name(type_node: Node, src: &[u8]) -> String {
    if let Some(inner) = type_node.child_by_field_name("type") {
        return innermost_type_name(inner, src);
    }
    type_node.utf8_text(src).unwrap_or("").to_string()
}

/// Collapse all runs of whitespace (including newlines) to single spaces.
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(src: &str) -> Vec<Symbol> {
        SymbolExtractor::new().extract("src/lib.rs", src)
    }

    fn find<'a>(syms: &'a [Symbol], name: &str) -> &'a Symbol {
        syms.iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("symbol '{name}' not found in {syms:?}"))
    }

    #[test]
    fn extracts_free_function_with_signature_and_no_parent() {
        let syms = extract("pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n");
        let f = find(&syms, "add");
        assert_eq!(f.kind, "function");
        assert_eq!(f.start_line, 1);
        assert_eq!(f.signature, "pub fn add(a: i32, b: i32) -> i32");
        assert_eq!(f.parent_scope, None);
    }

    #[test]
    fn extracts_struct_trait_enum_typealias_const() {
        let src = r#"
pub struct Config { path: String }
pub trait Loader { fn load(&self); }
pub enum Mode { A, B }
pub type Alias = Vec<u8>;
pub const MAX: usize = 10;
"#;
        let syms = extract(src);
        assert_eq!(find(&syms, "Config").kind, "struct");
        assert_eq!(find(&syms, "Loader").kind, "trait");
        assert_eq!(find(&syms, "Mode").kind, "enum");
        assert_eq!(find(&syms, "Alias").kind, "type_alias");
        assert_eq!(find(&syms, "MAX").kind, "const");
    }

    #[test]
    fn methods_carry_their_impl_type_as_parent_scope() {
        let src = r#"
struct Db;
impl Db {
    pub fn open(path: &str) -> Db { Db }
}
impl Cache<String> {
    fn get(&self) {}
}
"#;
        let syms = extract(src);
        // The impl type itself is captured.
        assert_eq!(find(&syms, "Db").kind, "struct");
        let open = find(&syms, "open");
        assert_eq!(open.kind, "function");
        assert_eq!(open.parent_scope.as_deref(), Some("Db"));
        // Generic impl: parent scope resolves to the bare type name.
        assert_eq!(find(&syms, "get").parent_scope.as_deref(), Some("Cache"));
    }

    #[test]
    fn unsupported_extension_and_garbage_yield_no_panic() {
        assert!(
            SymbolExtractor::new()
                .extract("notes.txt", "fn x(){}")
                .is_empty()
        );
        // Unparseable Rust still must not panic (best-effort).
        let _ = extract("fn (((");
    }

    #[test]
    fn end_position_spans_the_whole_item() {
        let syms = extract("fn outer() {\n    let x = 1;\n}\n");
        let f = find(&syms, "outer");
        assert_eq!(f.start_line, 1);
        assert_eq!(f.end_line, 3);
    }
}
