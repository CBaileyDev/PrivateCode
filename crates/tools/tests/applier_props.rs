//! Property + regression tests for the edit (exact-match + staleness) and patch
//! (Begin Patch context-seek) appliers. Exercises the multi-chunk seek,
//! line_index advancement, CRLF/BOM preservation, ambiguity/absence failures,
//! and the staleness guard — paths the 3 happy-path unit tests never reach.

use private_code_tools::{EditTool, PatchTool, ReadFileTool, Tool, ToolContext, ToolError};
use proptest::prelude::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Drive an async tool to completion on a current-thread runtime (the appliers
/// do synchronous fs work inside async fns, so no real reactor is needed).
fn run_tool(
    tool: &dyn Tool,
    ws: &Path,
    cache: &mut HashMap<PathBuf, String>,
    args: Value,
) -> Result<Value, ToolError> {
    let mut ctx = ToolContext {
        workspace_path: ws,
        active_dir: ws,
        file_read_cache: cache,
        global_data_dir: ws,
        max_lines: 100_000,
        max_bytes: 100_000_000,
    };
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(tool.run(&mut ctx, args))
}

/// A set of distinct, patch-safe lines (no leading +/-/space/@, never empty).
fn distinct_lines(min: usize, max: usize) -> impl Strategy<Value = Vec<String>> {
    prop::collection::hash_set(0u32..1_000_000, min..max).prop_map(|set| {
        let mut v: Vec<u32> = set.into_iter().collect();
        v.sort_unstable();
        v.into_iter().map(|n| format!("ln{n}")).collect()
    })
}

// ----------------------------------------------------------------------------
// EditTool properties
// ----------------------------------------------------------------------------

proptest! {
    /// P1 — replacing a unique line changes only that line.
    #[test]
    fn edit_replaces_unique_line(lines in distinct_lines(3, 12), pick in any::<prop::sample::Index>()) {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let file = ws.join("f.txt");
        let original = lines.join("\n");
        fs::write(&file, &original).unwrap();

        let i = pick.index(lines.len());
        let target = &lines[i];
        let replacement = "REPLACED_UNIQUE";

        let mut cache = HashMap::new();
        let res = run_tool(
            &EditTool,
            ws,
            &mut cache,
            json!({ "path": "f.txt", "old_content": target, "new_content": replacement }),
        );
        prop_assert!(res.is_ok(), "edit failed: {res:?}");

        let mut expected = lines.clone();
        expected[i] = replacement.to_string();
        prop_assert_eq!(fs::read_to_string(&file).unwrap(), expected.join("\n"));
    }

    /// P4 — an old_content that does not exist fails and leaves the file untouched.
    #[test]
    fn edit_absent_old_content_fails_unchanged(lines in distinct_lines(2, 10)) {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let file = ws.join("f.txt");
        let original = lines.join("\n");
        fs::write(&file, &original).unwrap();

        let mut cache = HashMap::new();
        let res = run_tool(
            &EditTool,
            ws,
            &mut cache,
            json!({ "path": "f.txt", "old_content": "THIS_LINE_IS_ABSENT_xyz", "new_content": "x" }),
        );
        prop_assert!(matches!(res, Err(ToolError::EditFailed(_))), "expected EditFailed, got {res:?}");
        prop_assert_eq!(fs::read_to_string(&file).unwrap(), original);
    }

    /// P3 — a duplicated target line makes the edit ambiguous (EditFailed).
    #[test]
    fn edit_ambiguous_line_fails(lines in distinct_lines(2, 8)) {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let file = ws.join("f.txt");
        // Duplicate the first line so it occurs twice.
        let dup = &lines[0];
        let mut content = lines.clone();
        content.push(dup.clone());
        fs::write(&file, content.join("\n")).unwrap();

        let mut cache = HashMap::new();
        let res = run_tool(
            &EditTool,
            ws,
            &mut cache,
            json!({ "path": "f.txt", "old_content": dup, "new_content": "x" }),
        );
        prop_assert!(matches!(res, Err(ToolError::EditFailed(_))), "expected EditFailed (ambiguous), got {res:?}");
    }
}

/// P2 — after a read populates the cache, an out-of-band write makes the next
/// edit return StaleFile.
#[test]
fn edit_detects_out_of_band_staleness() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    let file = ws.join("f.txt");
    fs::write(&file, "alpha\nbeta\ngamma").unwrap();

    let mut cache = HashMap::new();
    // Read populates the staleness cache.
    run_tool(&ReadFileTool, ws, &mut cache, json!({ "path": "f.txt" })).unwrap();

    // Out-of-band modification.
    fs::write(&file, "alpha\nCHANGED\ngamma").unwrap();

    let res = run_tool(
        &EditTool,
        ws,
        &mut cache,
        json!({ "path": "f.txt", "old_content": "gamma", "new_content": "delta" }),
    );
    assert!(
        matches!(res, Err(ToolError::StaleFile)),
        "expected StaleFile, got {res:?}"
    );
}

// ----------------------------------------------------------------------------
// PatchTool properties
// ----------------------------------------------------------------------------

fn begin_patch(body: &str) -> String {
    format!("*** Begin Patch\n{body}\n*** End Patch")
}

proptest! {
    /// P5 — a single-chunk Update with one -/+ swap (anchored by surrounding
    /// context) changes exactly the target line.
    #[test]
    fn patch_update_single_line(lines in distinct_lines(4, 12), pick in any::<prop::sample::Index>()) {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let file = ws.join("f.txt");
        fs::write(&file, lines.join("\n")).unwrap();

        // Interior index so we have a context line on each side.
        let i = 1 + pick.index(lines.len() - 2);
        let new_val = "PATCHED_LINE";
        let patch = begin_patch(&format!(
            "*** Update File: f.txt\n@@\n {}\n-{}\n+{}\n {}",
            lines[i - 1], lines[i], new_val, lines[i + 1]
        ));

        let mut cache = HashMap::new();
        let res = run_tool(&PatchTool, ws, &mut cache, json!({ "patchText": patch }));
        prop_assert!(res.is_ok(), "patch failed: {res:?}");

        let mut expected = lines.clone();
        expected[i] = new_val.to_string();
        prop_assert_eq!(fs::read_to_string(&file).unwrap(), expected.join("\n"));
    }

    /// P8 — two non-overlapping in-order chunks both apply.
    #[test]
    fn patch_two_chunks_apply(lines in distinct_lines(6, 14)) {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let file = ws.join("f.txt");
        fs::write(&file, lines.join("\n")).unwrap();

        // Change line 1 and a later line (len-2), both interior & distinct.
        let i = 1;
        let j = lines.len() - 2;
        prop_assume!(j > i + 1); // non-adjacent so contexts don't overlap

        let patch = begin_patch(&format!(
            "*** Update File: f.txt\n@@\n {}\n-{}\n+{}\n {}\n@@\n {}\n-{}\n+{}\n {}",
            lines[i - 1], lines[i], "FIRST", lines[i + 1],
            lines[j - 1], lines[j], "SECOND", lines[j + 1],
        ));

        let mut cache = HashMap::new();
        let res = run_tool(&PatchTool, ws, &mut cache, json!({ "patchText": patch }));
        prop_assert!(res.is_ok(), "patch failed: {res:?}");

        let mut expected = lines.clone();
        expected[i] = "FIRST".to_string();
        expected[j] = "SECOND".to_string();
        prop_assert_eq!(fs::read_to_string(&file).unwrap(), expected.join("\n"));
    }
}

/// P6 — an Add followed by a Delete of the same file leaves no file behind.
#[test]
fn patch_add_then_delete_is_noop_on_tree() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    let patch =
        begin_patch("*** Add File: scratch.txt\n+hello\n+world\n*** Delete File: scratch.txt");
    let mut cache = HashMap::new();
    let res = run_tool(&PatchTool, ws, &mut cache, json!({ "patchText": patch }));
    assert!(res.is_ok(), "patch failed: {res:?}");
    assert!(
        !ws.join("scratch.txt").exists(),
        "file should not survive add-then-delete"
    );
}

/// P7a — CRLF line endings survive an Update.
#[test]
fn patch_preserves_crlf() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    let file = ws.join("f.txt");
    fs::write(&file, "alpha\r\nbeta\r\ngamma").unwrap();

    let patch = begin_patch("*** Update File: f.txt\n@@\n alpha\n-beta\n+BETA\n gamma");
    let mut cache = HashMap::new();
    run_tool(&PatchTool, ws, &mut cache, json!({ "patchText": patch })).unwrap();

    assert_eq!(fs::read_to_string(&file).unwrap(), "alpha\r\nBETA\r\ngamma");
}

/// P7b — a UTF-8 BOM survives an Update.
#[test]
fn patch_preserves_bom() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    let file = ws.join("f.txt");
    fs::write(&file, "\u{feff}alpha\nbeta\ngamma").unwrap();

    let patch = begin_patch("*** Update File: f.txt\n@@\n alpha\n-beta\n+BETA\n gamma");
    let mut cache = HashMap::new();
    run_tool(&PatchTool, ws, &mut cache, json!({ "patchText": patch })).unwrap();

    let out = fs::read_to_string(&file).unwrap();
    assert!(
        out.starts_with('\u{feff}'),
        "BOM should be preserved: {out:?}"
    );
    assert_eq!(out, "\u{feff}alpha\nBETA\ngamma");
}

/// Regression — two chunks sharing the same `@@` context must hit DISTINCT
/// occurrences via line_index advancement (without it, both would target the
/// first occurrence and one change would be lost).
#[test]
fn patch_duplicate_context_targets_distinct_occurrences() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    let file = ws.join("f.txt");
    fs::write(&file, "ctx\ndup\nctx\ndup").unwrap();

    // Two chunks share the `@@ ctx` anchor; each swaps the following `dup`.
    let patch = begin_patch("*** Update File: f.txt\n@@ ctx\n-dup\n+A\n@@ ctx\n-dup\n+B");
    let mut cache = HashMap::new();
    let res = run_tool(&PatchTool, ws, &mut cache, json!({ "patchText": patch }));
    assert!(res.is_ok(), "patch failed: {res:?}");
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "ctx\nA\nctx\nB",
        "the two same-context chunks must update distinct dup occurrences",
    );
}

/// The Update path now mirrors EditTool's staleness guard.
#[test]
fn patch_update_detects_out_of_band_staleness() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    let file = ws.join("f.txt");
    fs::write(&file, "alpha\nbeta\ngamma").unwrap();

    let mut cache = HashMap::new();
    run_tool(&ReadFileTool, ws, &mut cache, json!({ "path": "f.txt" })).unwrap();

    // Out-of-band change after the read.
    fs::write(&file, "alpha\nDIFFERENT\ngamma").unwrap();

    let patch = begin_patch("*** Update File: f.txt\n@@\n alpha\n-DIFFERENT\n+X\n gamma");
    let res = run_tool(&PatchTool, ws, &mut cache, json!({ "patchText": patch }));
    assert!(
        matches!(res, Err(ToolError::StaleFile)),
        "expected StaleFile, got {res:?}"
    );
}
