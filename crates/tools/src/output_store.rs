use std::fs;
use std::path::PathBuf;
use uuid::Uuid;

pub struct OutputStore {
    global_data_dir: PathBuf,
    max_lines: usize,
    max_bytes: usize,
}

impl OutputStore {
    pub fn new(global_data_dir: PathBuf, max_lines: usize, max_bytes: usize) -> Self {
        Self {
            global_data_dir,
            max_lines,
            max_bytes,
        }
    }

    pub fn process_output(&self, output: &str) -> String {
        let bytes_len = output.len();
        let lines: Vec<&str> = output.lines().collect();
        let lines_len = lines.len();

        if bytes_len <= self.max_bytes && lines_len <= self.max_lines {
            return output.to_string();
        }

        // Output exceeds limits, store to file
        let tool_outputs_dir = self.global_data_dir.join("tool_outputs");
        if let Err(e) = fs::create_dir_all(&tool_outputs_dir) {
            eprintln!("Failed to create tool_outputs directory: {}", e);
            // Fallback: return raw truncated text without storing
            return truncate_preview(&lines, self.max_lines);
        }

        let file_name = format!("tool_{}.txt", Uuid::new_v4());
        let file_path = tool_outputs_dir.join(&file_name);

        if let Err(e) = fs::write(&file_path, output) {
            eprintln!("Failed to write tool output file: {}", e);
            return truncate_preview(&lines, self.max_lines);
        }

        let preview = truncate_preview(&lines, self.max_lines);
        format!(
            "[Truncated — full output at {}]\n{}",
            file_path.display(),
            preview
        )
    }
}

fn truncate_preview(lines: &[&str], max_lines: usize) -> String {
    if lines.len() <= max_lines {
        return lines.join("\n");
    }

    let head_count = max_lines / 2;
    let tail_count = max_lines - head_count;

    let head_lines = &lines[0..head_count];
    let tail_lines = &lines[(lines.len() - tail_count)..];

    format!(
        "{}\n\n... [{} lines truncated] ...\n\n{}",
        head_lines.join("\n"),
        lines.len() - max_lines,
        tail_lines.join("\n")
    )
}
