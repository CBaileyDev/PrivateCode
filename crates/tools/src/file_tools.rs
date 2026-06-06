use globset::Glob;
use regex::Regex;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

use crate::output_store::OutputStore;
use crate::tool::{Tool, ToolContext, ToolError};
use async_trait::async_trait;

// Helper to validate and canonicalize paths within the workspace boundary
pub fn validate_path(workspace_path: &Path, user_path: &str) -> Result<PathBuf, ToolError> {
    let path = Path::new(user_path);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace_path.join(path)
    };

    let canonical = if resolved.exists() {
        resolved.canonicalize().map_err(|e| {
            ToolError::PathOutOfBounds(format!("Failed to canonicalize {}: {}", user_path, e))
        })?
    } else {
        let mut ancestor = resolved.clone();
        while !ancestor.exists() {
            if let Some(parent) = ancestor.parent() {
                ancestor = parent.to_path_buf();
            } else {
                break;
            }
        }
        let canonical_ancestor = ancestor.canonicalize().map_err(|e| {
            ToolError::PathOutOfBounds(format!("Failed to canonicalize ancestor: {}", e))
        })?;
        if let Ok(rel) = resolved.strip_prefix(&ancestor) {
            canonical_ancestor.join(rel)
        } else {
            canonical_ancestor
        }
    };

    let canonical_workspace = workspace_path.canonicalize().map_err(|e| {
        ToolError::PathOutOfBounds(format!("Failed to canonicalize workspace root: {}", e))
    })?;

    if canonical.starts_with(&canonical_workspace) {
        Ok(canonical)
    } else {
        Err(ToolError::PathOutOfBounds(format!(
            "Path {} is outside of workspace root {}",
            user_path,
            workspace_path.display()
        )))
    }
}

// 1. ReadFile Tool
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file within the workspace, optionally between line ranges."
    }

    fn schema(&self) -> Value {
        json!({
            "name": "read_file",
            "description": self.description(),
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "The path to the file to read (absolute or relative to workspace)" },
                    "start_line": { "type": "integer", "description": "1-indexed start line (inclusive)" },
                    "end_line": { "type": "integer", "description": "1-indexed end line (inclusive)" }
                },
                "required": ["path"]
            }
        })
    }

    fn mutates(&self) -> bool {
        false
    }

    fn permission_class(&self) -> &str {
        "read_file"
    }

    async fn run(
        &self,
        context: &mut ToolContext<'_>,
        arguments: Value,
    ) -> Result<Value, ToolError> {
        let path_str = arguments["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'path' argument".to_string()))?;
        let canonical_path = validate_path(context.workspace_path, path_str)?;

        let content = fs::read_to_string(&canonical_path)?;

        // Cache the read content for the staleness guard
        context
            .file_read_cache
            .insert(canonical_path.clone(), content.clone());

        let start_line = arguments["start_line"].as_u64().map(|x| x as usize);
        let end_line = arguments["end_line"].as_u64().map(|x| x as usize);

        let result_text = if start_line.is_some() || end_line.is_some() {
            let lines: Vec<&str> = content.lines().collect();
            let start = start_line.unwrap_or(1).saturating_sub(1);
            let end = end_line.unwrap_or(lines.len()).min(lines.len());
            if start >= lines.len() || start > end {
                String::new()
            } else {
                lines[start..end].join("\n")
            }
        } else {
            content
        };

        // Truncate if exceeds bounds
        let output_store = OutputStore::new(
            context.global_data_dir.to_path_buf(),
            context.max_lines,
            context.max_bytes,
        );
        let processed = output_store.process_output(&result_text);

        Ok(Value::String(processed))
    }
}

// 2. WriteFile Tool
pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write a new file or completely overwrite an existing file with the specified content."
    }

    fn schema(&self) -> Value {
        json!({
            "name": "write_file",
            "description": self.description(),
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "The target path to write (absolute or relative to workspace)" },
                    "content": { "type": "string", "description": "The file content to write" }
                },
                "required": ["path", "content"]
            }
        })
    }

    fn mutates(&self) -> bool {
        true
    }

    fn permission_class(&self) -> &str {
        "write_file"
    }

    async fn run(
        &self,
        context: &mut ToolContext<'_>,
        arguments: Value,
    ) -> Result<Value, ToolError> {
        let path_str = arguments["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'path' argument".to_string()))?;
        let content = arguments["content"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'content' argument".to_string()))?;
        let canonical_path = validate_path(context.workspace_path, path_str)?;

        // Staleness guard check: if file already exists, make sure it matches cached read
        if canonical_path.exists() {
            let current = fs::read_to_string(&canonical_path)?;
            if let Some(cached) = context.file_read_cache.get(&canonical_path)
                && current != *cached
            {
                return Err(ToolError::StaleFile);
            }
        }

        if let Some(parent) = canonical_path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::write(&canonical_path, content)?;
        context
            .file_read_cache
            .insert(canonical_path.clone(), content.to_string());

        Ok(json!({ "status": "success", "path": path_str }))
    }
}

// 3. Glob Tool
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern relative to the workspace, respecting gitignore."
    }

    fn schema(&self) -> Value {
        json!({
            "name": "glob",
            "description": self.description(),
            "input_schema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "The glob pattern relative to workspace (e.g., 'src/**/*.rs')" }
                },
                "required": ["pattern"]
            }
        })
    }

    fn mutates(&self) -> bool {
        false
    }

    fn permission_class(&self) -> &str {
        "glob"
    }

    async fn run(
        &self,
        context: &mut ToolContext<'_>,
        arguments: Value,
    ) -> Result<Value, ToolError> {
        let pattern_str = arguments["pattern"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'pattern' argument".to_string()))?;
        let matcher = Glob::new(pattern_str)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?
            .compile_matcher();

        let mut matches = Vec::new();
        let walker = ignore::WalkBuilder::new(context.workspace_path).build();

        for entry_res in walker {
            if let Ok(entry) = entry_res
                && entry.file_type().is_some_and(|t| t.is_file())
            {
                let full_path = entry.path();
                if let Ok(rel_path) = full_path.strip_prefix(context.workspace_path)
                    && matcher.is_match(rel_path)
                {
                    matches.push(rel_path.to_string_lossy().to_string());
                }
            }
        }

        Ok(json!(matches))
    }
}

// 4. Grep Tool
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search for regex pattern matches in files within the workspace, respecting gitignore."
    }

    fn schema(&self) -> Value {
        json!({
            "name": "grep",
            "description": self.description(),
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The regex pattern to search for" }
                },
                "required": ["query"]
            }
        })
    }

    fn mutates(&self) -> bool {
        false
    }

    fn permission_class(&self) -> &str {
        "grep"
    }

    async fn run(
        &self,
        context: &mut ToolContext<'_>,
        arguments: Value,
    ) -> Result<Value, ToolError> {
        let query_str = arguments["query"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'query' argument".to_string()))?;
        let regex = Regex::new(query_str)
            .map_err(|e| ToolError::InvalidArguments(format!("Invalid regex: {}", e)))?;

        let mut results = Vec::new();
        let walker = ignore::WalkBuilder::new(context.workspace_path).build();

        for entry_res in walker {
            if let Ok(entry) = entry_res
                && entry.file_type().is_some_and(|t| t.is_file())
            {
                let path = entry.path();
                if let Ok(content) = fs::read_to_string(path) {
                    let rel_path = path
                        .strip_prefix(context.workspace_path)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .to_string();
                    for (i, line) in content.lines().enumerate() {
                        if regex.is_match(line) {
                            results.push(json!({
                                "path": rel_path,
                                "line": i + 1,
                                "content": line
                            }));
                            if results.len() >= 500 {
                                break; // cap results to avoid flooding
                            }
                        }
                    }
                }
            }
        }

        Ok(json!(results))
    }
}

// Helper for UTF-8 BOM splitting/joining
struct SplitBom {
    text: String,
    bom: bool,
}

fn split_bom(s: &str) -> SplitBom {
    if s.starts_with('\u{feff}') {
        SplitBom {
            text: s.chars().skip(1).collect(),
            bom: true,
        }
    } else {
        SplitBom {
            text: s.to_string(),
            bom: false,
        }
    }
}

fn join_bom(text: &str, bom: bool) -> String {
    if bom {
        format!("\u{feff}{}", text)
    } else {
        text.to_string()
    }
}

// 5. Edit Tool (Exact String Replace)
pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Apply an exact-string replace in a file. Returns success or fails if target string is not found or is ambiguous."
    }

    fn schema(&self) -> Value {
        json!({
            "name": "edit",
            "description": self.description(),
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "The path to the file to modify" },
                    "old_content": { "type": "string", "description": "The exact string block to replace" },
                    "new_content": { "type": "string", "description": "The replacement string block" }
                },
                "required": ["path", "old_content", "new_content"]
            }
        })
    }

    fn mutates(&self) -> bool {
        true
    }

    fn permission_class(&self) -> &str {
        "write_file"
    }

    async fn run(
        &self,
        context: &mut ToolContext<'_>,
        arguments: Value,
    ) -> Result<Value, ToolError> {
        let path_str = arguments["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'path' argument".to_string()))?;
        let old_content = arguments["old_content"].as_str().ok_or_else(|| {
            ToolError::InvalidArguments("Missing 'old_content' argument".to_string())
        })?;
        let new_content = arguments["new_content"].as_str().ok_or_else(|| {
            ToolError::InvalidArguments("Missing 'new_content' argument".to_string())
        })?;

        if old_content.is_empty() {
            return Err(ToolError::EditFailed(
                "old_content cannot be empty".to_string(),
            ));
        }
        if old_content == new_content {
            return Err(ToolError::EditFailed(
                "old_content and new_content are identical".to_string(),
            ));
        }

        let canonical_path = validate_path(context.workspace_path, path_str)?;
        let file_text = fs::read_to_string(&canonical_path)?;

        // Staleness guard check
        if let Some(cached) = context.file_read_cache.get(&canonical_path)
            && file_text != *cached
        {
            return Err(ToolError::StaleFile);
        }

        // BOM & line endings detection
        let SplitBom {
            text: content_body,
            bom,
        } = split_bom(&file_text);
        let has_crlf = content_body.contains("\r\n");

        // Normalize inputs for searching
        let norm_old = old_content.replace("\r\n", "\n");
        let norm_new = new_content.replace("\r\n", "\n");
        let search_body = content_body.replace("\r\n", "\n");

        let matches: Vec<_> = search_body.match_indices(&norm_old).collect();
        if matches.is_empty() {
            return Err(ToolError::EditFailed(
                "old_content not found in file".to_string(),
            ));
        }
        if matches.len() > 1 {
            return Err(ToolError::EditFailed(
                "Multiple occurrences of old_content found. Please provide more lines of context."
                    .to_string(),
            ));
        }

        // Perform replacement in normalized body
        let replaced_body = search_body.replacen(&norm_old, &norm_new, 1);

        // Re-apply original CRLF style if detected
        let final_body = if has_crlf {
            replaced_body.replace('\n', "\r\n")
        } else {
            replaced_body
        };

        let final_text = join_bom(&final_body, bom);
        fs::write(&canonical_path, &final_text)?;

        // Update read cache
        context.file_read_cache.insert(canonical_path, final_text);

        Ok(json!({ "status": "success", "path": path_str }))
    }
}

// 6. Patch Tool (*** Begin Patch envelope)
pub struct PatchTool;

#[derive(Debug)]
pub enum Hunk {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_path: Option<String>,
        chunks: Vec<UpdateChunk>,
    },
}

#[derive(Debug, Clone)]
pub struct UpdateChunk {
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    change_context: Option<String>,
    is_end_of_file: bool,
}

#[async_trait]
impl Tool for PatchTool {
    fn name(&self) -> &str {
        "patch"
    }

    fn description(&self) -> &str {
        "Apply context-anchored block edits using a *** Begin Patch envelope. Supports Add, Delete, and Update File (with optional Move to)."
    }

    fn schema(&self) -> Value {
        json!({
            "name": "patch",
            "description": self.description(),
            "input_schema": {
                "type": "object",
                "properties": {
                    "patchText": { "type": "string", "description": "The patch content containing *** Begin Patch / *** End Patch markers" }
                },
                "required": ["patchText"]
            }
        })
    }

    fn mutates(&self) -> bool {
        true
    }

    fn permission_class(&self) -> &str {
        "write_file"
    }

    async fn run(
        &self,
        context: &mut ToolContext<'_>,
        arguments: Value,
    ) -> Result<Value, ToolError> {
        let patch_text = arguments["patchText"].as_str().ok_or_else(|| {
            ToolError::InvalidArguments("Missing 'patchText' argument".to_string())
        })?;
        let hunks = parse_patch(patch_text)?;

        if hunks.is_empty() {
            return Err(ToolError::EditFailed("No hunks found in patch".to_string()));
        }

        // Validate all target paths first before applying any edits (atomicity / preview boundary)
        for hunk in &hunks {
            match hunk {
                Hunk::Add { path, .. } => {
                    validate_path(context.workspace_path, path)?;
                }
                Hunk::Delete { path } => {
                    validate_path(context.workspace_path, path)?;
                }
                Hunk::Update {
                    path, move_path, ..
                } => {
                    validate_path(context.workspace_path, path)?;
                    if let Some(mv) = move_path {
                        validate_path(context.workspace_path, mv)?;
                    }
                }
            }
        }

        let mut applied_ops = Vec::new();
        for hunk in hunks {
            match hunk {
                Hunk::Add { path, contents } => {
                    let canonical = validate_path(context.workspace_path, &path)?;
                    if let Some(parent) = canonical.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&canonical, &contents)?;
                    context.file_read_cache.insert(canonical, contents);
                    applied_ops.push(format!("Add File: {}", path));
                }
                Hunk::Delete { path } => {
                    let canonical = validate_path(context.workspace_path, &path)?;
                    if canonical.exists() {
                        fs::remove_file(&canonical)?;
                        context.file_read_cache.remove(&canonical);
                    }
                    applied_ops.push(format!("Delete File: {}", path));
                }
                Hunk::Update {
                    path,
                    move_path,
                    chunks,
                } => {
                    let canonical = validate_path(context.workspace_path, &path)?;
                    let original_text = fs::read_to_string(&canonical)?;

                    // Staleness guard: mirror EditTool/WriteFileTool so a patch
                    // cannot silently clobber a file that changed out-of-band
                    // since it was read this turn.
                    if let Some(cached) = context.file_read_cache.get(&canonical)
                        && original_text != *cached
                    {
                        return Err(ToolError::StaleFile);
                    }

                    let SplitBom {
                        text: original_body,
                        bom,
                    } = split_bom(&original_text);

                    let has_crlf = original_body.contains("\r\n");
                    let ends_with_newline = original_body.ends_with('\n');
                    let lines: Vec<String> = original_body.lines().map(|s| s.to_string()).collect();

                    let replacements = compute_replacements(&lines, &path, &chunks)?;
                    let new_lines = apply_replacements(lines, replacements);

                    let mut final_body = new_lines.join(if has_crlf { "\r\n" } else { "\n" });
                    if ends_with_newline && !final_body.is_empty() {
                        if has_crlf {
                            final_body.push_str("\r\n");
                        } else {
                            final_body.push('\n');
                        }
                    }
                    let final_text = join_bom(&final_body, bom);

                    if let Some(mv) = move_path {
                        let canonical_mv = validate_path(context.workspace_path, &mv)?;
                        if let Some(parent) = canonical_mv.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::write(&canonical_mv, &final_text)?;
                        fs::remove_file(&canonical)?;
                        context.file_read_cache.remove(&canonical);
                        context.file_read_cache.insert(canonical_mv, final_text);
                        applied_ops.push(format!("Update File (moved): {} -> {}", path, mv));
                    } else {
                        fs::write(&canonical, &final_text)?;
                        context.file_read_cache.insert(canonical, final_text);
                        applied_ops.push(format!("Update File: {}", path));
                    }
                }
            }
        }

        Ok(json!({ "status": "success", "applied": applied_ops }))
    }
}

// Hunk parser for *** Begin Patch
pub fn parse_patch(patch_text: &str) -> Result<Vec<Hunk>, ToolError> {
    let cleaned = strip_heredoc(patch_text.trim());
    let lines: Vec<&str> = cleaned.lines().collect();

    let begin_idx = lines.iter().position(|l| l.trim() == "*** Begin Patch");
    let end_idx = lines.iter().position(|l| l.trim() == "*** End Patch");

    let (begin, end) = match (begin_idx, end_idx) {
        (Some(b), Some(e)) if b < e => (b, e),
        _ => {
            return Err(ToolError::InvalidArguments(
                "Invalid patch format: missing *** Begin Patch / *** End Patch markers".to_string(),
            ));
        }
    };

    let mut hunks = Vec::new();
    let mut i = begin + 1;

    while i < end {
        let line = lines[i].trim();
        if line.is_empty() {
            i += 1;
            continue;
        }

        if line.starts_with("*** Add File:") {
            let path = line
                .strip_prefix("*** Add File:")
                .unwrap()
                .trim()
                .to_string();
            i += 1;
            let mut contents = String::new();
            while i < end && !lines[i].starts_with("***") {
                if lines[i].starts_with('+') {
                    contents.push_str(lines[i].strip_prefix('+').unwrap());
                    contents.push('\n');
                }
                i += 1;
            }
            if contents.ends_with('\n') {
                contents.pop();
            }
            hunks.push(Hunk::Add { path, contents });
        } else if line.starts_with("*** Delete File:") {
            let path = line
                .strip_prefix("*** Delete File:")
                .unwrap()
                .trim()
                .to_string();
            hunks.push(Hunk::Delete { path });
            i += 1;
        } else if line.starts_with("*** Update File:") {
            let path = line
                .strip_prefix("*** Update File:")
                .unwrap()
                .trim()
                .to_string();
            i += 1;
            let mut move_path = None;
            if i < end && lines[i].trim().starts_with("*** Move to:") {
                move_path = Some(
                    lines[i]
                        .trim()
                        .strip_prefix("*** Move to:")
                        .unwrap()
                        .trim()
                        .to_string(),
                );
                i += 1;
            }

            let mut chunks = Vec::new();
            while i < end && !lines[i].starts_with("***") {
                let chunk_line = lines[i].trim();
                if chunk_line.starts_with("@@") {
                    let change_context = chunk_line.strip_prefix("@@").unwrap().trim();
                    let change_ctx = if change_context.is_empty() {
                        None
                    } else {
                        Some(change_context.to_string())
                    };
                    i += 1;

                    let mut old_lines = Vec::new();
                    let mut new_lines = Vec::new();
                    let mut is_end_of_file = false;

                    while i < end && !lines[i].starts_with("@@") && !lines[i].starts_with("***") {
                        let cl = lines[i];
                        if cl == "*** End of File" {
                            is_end_of_file = true;
                            i += 1;
                            break;
                        }

                        if cl.starts_with(' ') {
                            let content = cl.strip_prefix(' ').unwrap().to_string();
                            old_lines.push(content.clone());
                            new_lines.push(content);
                        } else if cl.starts_with('-') {
                            old_lines.push(cl.strip_prefix('-').unwrap().to_string());
                        } else if cl.starts_with('+') {
                            new_lines.push(cl.strip_prefix('+').unwrap().to_string());
                        }
                        i += 1;
                    }

                    chunks.push(UpdateChunk {
                        old_lines,
                        new_lines,
                        change_context: change_ctx,
                        is_end_of_file,
                    });
                } else {
                    i += 1;
                }
            }

            hunks.push(Hunk::Update {
                path,
                move_path,
                chunks,
            });
        } else {
            i += 1;
        }
    }

    Ok(hunks)
}

fn strip_heredoc(input: &str) -> String {
    let mut s = input.trim();
    if s.starts_with("cat") {
        s = &s["cat".len()..];
        s = s.trim_start();
    }
    if !s.starts_with("<<") {
        return input.to_string();
    }
    s = &s["<<".len()..];

    // Parse delimiter
    let mut quote_char = None;
    if s.starts_with('\'') || s.starts_with('"') || s.starts_with('&') {
        let first = s.chars().next().unwrap();
        quote_char = Some(first);
        s = &s[first.len_utf8()..];
    }

    let delim_end = s
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .unwrap_or(s.len());
    if delim_end == 0 {
        return input.to_string();
    }
    let delimiter = &s[..delim_end];
    s = &s[delim_end..];

    if let Some(qc) = quote_char
        && s.starts_with(qc)
    {
        s = &s[qc.len_utf8()..];
    }

    s = s.trim_start_matches([' ', '\t']);
    if !s.starts_with('\n') && !s.starts_with("\r\n") {
        return input.to_string();
    }

    if s.starts_with("\r\n") {
        s = &s[2..];
    } else {
        s = &s[1..];
    }

    let mut temp = s.trim_end_matches([' ', '\t']);
    if temp.ends_with(delimiter) {
        temp = &temp[..temp.len() - delimiter.len()];
        if temp.ends_with('\n') {
            temp = &temp[..temp.len() - 1];
            if temp.ends_with('\r') {
                temp = &temp[..temp.len() - 1];
            }
            return temp.to_string();
        }
    }

    input.to_string()
}

// Compute replacements for context seek patch applier
fn compute_replacements(
    original_lines: &[String],
    file_path: &str,
    chunks: &[UpdateChunk],
) -> Result<Vec<(usize, usize, Vec<String>)>, ToolError> {
    let mut replacements = Vec::new();
    let mut line_index = 0;

    for chunk in chunks {
        if let Some(ctx) = &chunk.change_context {
            let context_idx =
                seek_sequence(original_lines, std::slice::from_ref(ctx), line_index, false);
            if context_idx == -1 {
                return Err(ToolError::EditFailed(format!(
                    "Failed to find context '{}' in {}",
                    ctx, file_path
                )));
            }
            line_index = (context_idx as usize) + 1;
        }

        if chunk.old_lines.is_empty() {
            // Pure addition at end of file or cursor
            let insertion_idx = original_lines.len();
            replacements.push((insertion_idx, 0, chunk.new_lines.clone()));
            continue;
        }

        let mut pattern = chunk.old_lines.clone();
        let mut new_slice = chunk.new_lines.clone();
        let mut found = seek_sequence(original_lines, &pattern, line_index, chunk.is_end_of_file);

        if found == -1
            && !pattern.is_empty()
            && pattern.last().map(|s| s.is_empty()).unwrap_or(false)
        {
            pattern.pop();
            if !new_slice.is_empty() && new_slice.last().map(|s| s.is_empty()).unwrap_or(false) {
                new_slice.pop();
            }
            found = seek_sequence(original_lines, &pattern, line_index, chunk.is_end_of_file);
        }

        if found != -1 {
            replacements.push((found as usize, pattern.len(), new_slice));
            line_index = (found as usize) + pattern.len();
        } else {
            return Err(ToolError::EditFailed(format!(
                "Failed to find expected lines in {}:\n{}",
                file_path,
                chunk.old_lines.join("\n")
            )));
        }
    }

    replacements.sort_by_key(|r| r.0);
    Ok(replacements)
}

fn apply_replacements(
    lines: Vec<String>,
    replacements: Vec<(usize, usize, Vec<String>)>,
) -> Vec<String> {
    let mut result = lines;
    for (start_idx, old_len, new_segment) in replacements.into_iter().rev() {
        if start_idx <= result.len() {
            let end_idx = (start_idx + old_len).min(result.len());
            result.drain(start_idx..end_idx);
            for (offset, new_line) in new_segment.into_iter().enumerate() {
                result.insert(start_idx + offset, new_line);
            }
        }
    }
    result
}

fn normalize_unicode(s: &str) -> String {
    s.replace(&['‘', '’', '‚', '‛'][..], "'")
        .replace(&['“', '”', '„', '‟'][..], "\"")
        .replace(&['‐', '‑', '‒', '–', '—', '―'][..], "-")
        .replace('…', "...")
        .replace('\u{a0}', " ")
}

fn try_match<F>(
    lines: &[String],
    pattern: &[String],
    start_index: usize,
    compare: F,
    eof: bool,
) -> i32
where
    F: Fn(&str, &str) -> bool,
{
    if eof {
        let from_end = lines.len().saturating_sub(pattern.len());
        if from_end >= start_index {
            let mut matches = true;
            for j in 0..pattern.len() {
                if !compare(&lines[from_end + j], &pattern[j]) {
                    matches = false;
                    break;
                }
            }
            if matches {
                return from_end as i32;
            }
        }
    }

    for i in start_index..=lines.len().saturating_sub(pattern.len()) {
        let mut matches = true;
        for j in 0..pattern.len() {
            if !compare(&lines[i + j], &pattern[j]) {
                matches = false;
                break;
            }
        }
        if matches {
            return i as i32;
        }
    }

    -1
}

fn seek_sequence(lines: &[String], pattern: &[String], start_index: usize, eof: bool) -> i32 {
    if pattern.is_empty() {
        return -1;
    }

    // Pass 1: exact
    let res = try_match(lines, pattern, start_index, |a, b| a == b, eof);
    if res != -1 {
        return res;
    }

    // Pass 2: trimEnd
    let res = try_match(
        lines,
        pattern,
        start_index,
        |a, b| a.trim_end() == b.trim_end(),
        eof,
    );
    if res != -1 {
        return res;
    }

    // Pass 3: trim
    let res = try_match(
        lines,
        pattern,
        start_index,
        |a, b| a.trim() == b.trim(),
        eof,
    );
    if res != -1 {
        return res;
    }

    // Pass 4: unicode normalized
    try_match(
        lines,
        pattern,
        start_index,
        |a, b| normalize_unicode(a.trim()) == normalize_unicode(b.trim()),
        eof,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_read_write_file() {
        let dir = tempdir().unwrap();
        let ws = dir.path();

        let mut cache = HashMap::new();
        let mut ctx = ToolContext {
            workspace_path: ws,
            active_dir: ws,
            file_read_cache: &mut cache,
            global_data_dir: ws,
            max_lines: 100,
            max_bytes: 1000,
        };

        let write = WriteFileTool;
        let res = write
            .run(
                &mut ctx,
                json!({
                    "path": "test.txt",
                    "content": "hello world\nline 2\nline 3"
                }),
            )
            .await
            .unwrap();
        assert_eq!(res["status"], "success");

        let read = ReadFileTool;
        let read_res = read
            .run(
                &mut ctx,
                json!({
                    "path": "test.txt"
                }),
            )
            .await
            .unwrap();
        assert_eq!(read_res.as_str().unwrap(), "hello world\nline 2\nline 3");

        // Range read
        let range_res = read
            .run(
                &mut ctx,
                json!({
                    "path": "test.txt",
                    "start_line": 2,
                    "end_line": 2
                }),
            )
            .await
            .unwrap();
        assert_eq!(range_res.as_str().unwrap(), "line 2");
    }

    #[tokio::test]
    async fn test_edit_tool() {
        let dir = tempdir().unwrap();
        let ws = dir.path();

        let mut cache = HashMap::new();
        let mut ctx = ToolContext {
            workspace_path: ws,
            active_dir: ws,
            file_read_cache: &mut cache,
            global_data_dir: ws,
            max_lines: 100,
            max_bytes: 1000,
        };

        let file_path = ws.join("edit_test.txt");
        fs::write(&file_path, "original content\nline to change\nend content").unwrap();

        // 1. Initial read to populate cache
        let read = ReadFileTool;
        read.run(&mut ctx, json!({"path": "edit_test.txt"}))
            .await
            .unwrap();

        let edit = EditTool;

        // 2. Successful replacement
        let edit_res = edit
            .run(
                &mut ctx,
                json!({
                    "path": "edit_test.txt",
                    "old_content": "line to change",
                    "new_content": "replaced line"
                }),
            )
            .await
            .unwrap();
        assert_eq!(edit_res["status"], "success");

        let updated = fs::read_to_string(&file_path).unwrap();
        assert_eq!(updated, "original content\nreplaced line\nend content");

        // 3. Ambiguity check (fails if not unique)
        fs::write(&file_path, "dup\ndup\nother").unwrap();
        ctx.file_read_cache.clear();
        read.run(&mut ctx, json!({"path": "edit_test.txt"}))
            .await
            .unwrap();

        let err_res = edit
            .run(
                &mut ctx,
                json!({
                    "path": "edit_test.txt",
                    "old_content": "dup",
                    "new_content": "unique"
                }),
            )
            .await;
        assert!(err_res.is_err());
    }

    #[tokio::test]
    async fn test_patch_tool() {
        let dir = tempdir().unwrap();
        let ws = dir.path();

        let mut cache = HashMap::new();
        let mut ctx = ToolContext {
            workspace_path: ws,
            active_dir: ws,
            file_read_cache: &mut cache,
            global_data_dir: ws,
            max_lines: 100,
            max_bytes: 1000,
        };

        // Write initial file to modify
        let file_path = ws.join("patch_test.txt");
        fs::write(&file_path, "row 1\nrow 2\nrow 3\n").unwrap();

        let patch = PatchTool;
        let patch_text = "*** Begin Patch\n*** Update File: patch_test.txt\n@@\n-row 2\n+row 2 changed\n*** End Patch";

        let res = patch
            .run(
                &mut ctx,
                json!({
                    "patchText": patch_text
                }),
            )
            .await
            .unwrap();
        assert!(!res["applied"].as_array().unwrap().is_empty());

        let final_content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(final_content, "row 1\nrow 2 changed\nrow 3\n");
    }
}
