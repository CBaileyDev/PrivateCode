//! Language server discovery based on project file types.

use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct LanguageServerSpec {
    pub id: String,
    pub command: String,
    pub args: Vec<String>,
    pub language_ids: Vec<String>,
}

/// Directories that never contain first-party source worth scanning for a
/// language server (VCS metadata, build output, vendored dependencies).
const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
];

/// Hard cap on directory entries visited, so discovery on a giant tree can never
/// stall daemon startup.
const MAX_ENTRIES: usize = 20_000;

/// Discover language servers that appear installed for files in the workspace.
pub fn discover_servers(workspace: &Path) -> Vec<LanguageServerSpec> {
    let mut specs = Vec::new();
    let exts = collect_extensions(workspace);
    let has = |group: &[&str]| group.iter().any(|e| exts.contains(*e));

    if has(&["rs"]) && which("rust-analyzer") {
        specs.push(LanguageServerSpec {
            id: "rust-analyzer".into(),
            command: "rust-analyzer".into(),
            args: vec![],
            language_ids: vec!["rust".into()],
        });
    }

    if has(&["ts", "tsx", "js", "jsx"]) && which("typescript-language-server") {
        specs.push(LanguageServerSpec {
            id: "typescript-language-server".into(),
            command: "typescript-language-server".into(),
            args: vec!["--stdio".into()],
            language_ids: vec!["typescript".into(), "javascript".into()],
        });
    }

    if has(&["py"]) && (which("pyright-langserver") || which("pyright")) {
        let cmd = if which("pyright-langserver") {
            "pyright-langserver"
        } else {
            "pyright"
        };
        specs.push(LanguageServerSpec {
            id: "pyright".into(),
            command: cmd.into(),
            args: vec!["--stdio".into()],
            language_ids: vec!["python".into()],
        });
    }

    if has(&["go"]) && which("gopls") {
        specs.push(LanguageServerSpec {
            id: "gopls".into(),
            command: "gopls".into(),
            args: vec![],
            language_ids: vec!["go".into()],
        });
    }

    specs
}

/// Collect the set of file extensions present under `workspace` in a SINGLE
/// bounded walk. Skips VCS/build/dependency and hidden directories and stops
/// after [`MAX_ENTRIES`] entries.
///
/// Previously discovery ran one full recursive walk PER language group; the
/// groups with no matching files (e.g. `.go` in a Rust repo) were exhaustive —
/// they traversed the entire tree including `target/` and `node_modules/` — which
/// delayed the daemon binding its socket. One bounded walk + O(1) set lookups
/// removes that pathology and stops counting `target/`/`node_modules/` artifacts
/// as project source.
fn collect_extensions(workspace: &Path) -> HashSet<String> {
    let mut exts = HashSet::new();
    let mut stack = vec![workspace.to_path_buf()];
    let mut seen = 0usize;

    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            seen += 1;
            if seen > MAX_ENTRIES {
                return exts;
            }
            // `file_type()` does not follow symlinks, so a symlinked directory is
            // treated as neither dir nor file (skipped) — no traversal loops.
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if ft.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with('.') || SKIP_DIRS.contains(&name) {
                    continue;
                }
                stack.push(path);
            } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                exts.insert(ext.to_string());
            }
        }
    }
    exts
}

fn which(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn discovers_rust_when_rs_files_present() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        // May or may not find rust-analyzer depending on install — just must not panic.
        let _ = discover_servers(dir.path());
    }

    #[test]
    fn collect_extensions_skips_ignored_and_hidden_dirs() {
        let dir = TempDir::new().unwrap();
        // First-party source.
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/app.ts"), "export {}").unwrap();
        // Artifacts/deps/hidden that MUST be skipped — their extensions must not
        // leak into the discovered set (else node_modules `.go`/`.py` junk would
        // spuriously start a language server, and the walk would be unbounded).
        std::fs::create_dir_all(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join("node_modules/dep.go"), "package main").unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/gen.py"), "x = 1").unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/hook.rb"), "puts 1").unwrap();

        let exts = collect_extensions(dir.path());
        assert!(exts.contains("rs"), "first-party .rs is discovered");
        assert!(exts.contains("ts"), "first-party .ts is discovered");
        assert!(!exts.contains("go"), "node_modules is skipped");
        assert!(!exts.contains("py"), "target is skipped");
        assert!(!exts.contains("rb"), ".git is skipped");
    }
}
