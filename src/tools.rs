///
/// tools.rs - Tool definitions and handlers
///
/// Defines OpenAI-format tool schemas and their execution handlers.
/// Tools: bash, read_file, write_file, edit_file, list_directory, grep_search
///
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

// ---------------------------------------------------------------------------
// Path safety
// ---------------------------------------------------------------------------

fn safe_path(workdir: &Path, p: &str) -> Result<PathBuf, String> {
    let path = workdir.join(p).canonicalize().unwrap_or_else(|_| workdir.join(p));
    // For new files that don't exist yet, check the parent
    let check_path = if path.exists() {
        path.clone()
    } else {
        path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| workdir.to_path_buf())
    };
    let check_resolved = check_path
        .canonicalize()
        .unwrap_or_else(|_| check_path.clone());
    let workdir_resolved = workdir.canonicalize().unwrap_or_else(|_| workdir.to_path_buf());
    if !check_resolved.starts_with(&workdir_resolved) {
        return Err(format!("Path escapes workspace: {}", p));
    }
    Ok(path)
}

// ---------------------------------------------------------------------------
// Tool handlers
// ---------------------------------------------------------------------------

pub fn run_bash(workdir: &Path, command: &str) -> String {
    let dangerous = [
        "rm -rf /",
        "rm -rf ~",
        "sudo rm",
        "shutdown",
        "reboot",
        "> /dev/",
        "mkfs",
    ];
    if dangerous.iter().any(|d| command.contains(d)) {
        return "Error: Dangerous command blocked".into();
    }
    match Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(workdir)
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let out = format!("{}{}", stdout, stderr).trim().to_string();
            if out.is_empty() {
                "(no output)".into()
            } else if out.len() > 50000 {
                out[..50000].to_string()
            } else {
                out
            }
        }
        Err(e) => format!("Error: {}", e),
    }
}

pub fn run_read_file(workdir: &Path, path: &str, offset: usize, limit: usize) -> String {
    let fp = match safe_path(workdir, path) {
        Ok(p) => p,
        Err(e) => return format!("Error: {}", e),
    };
    match fs::read_to_string(&fp) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().collect();
            let total = lines.len();
            let start = if offset > 0 { offset - 1 } else { 0 };
            let selected: Vec<&str> = if limit > 0 {
                lines.iter().skip(start).take(limit).copied().collect()
            } else {
                lines.iter().skip(start).copied().collect()
            };
            let line_start = if offset > 0 { offset } else { 1 };
            let numbered: Vec<String> = selected
                .iter()
                .enumerate()
                .map(|(i, line)| format!("{:4}  {}", i + line_start, line))
                .collect();
            let mut result = numbered.join("\n");
            if result.len() > 50000 {
                result.truncate(50000);
                result.push_str("\n... (truncated)");
            }
            if total > selected.len() {
                result.push_str(&format!("\n[{} lines total]", total));
            }
            result
        }
        Err(e) => format!("Error: {}", e),
    }
}

pub fn run_write_file(workdir: &Path, path: &str, content: &str) -> String {
    let fp = match safe_path(workdir, path) {
        Ok(p) => p,
        Err(e) => return format!("Error: {}", e),
    };
    if let Some(parent) = fp.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return format!("Error: {}", e);
        }
    }
    match fs::write(&fp, content) {
        Ok(()) => format!("Wrote {} bytes to {}", content.len(), path),
        Err(e) => format!("Error: {}", e),
    }
}

pub fn run_edit_file(workdir: &Path, path: &str, old_text: &str, new_text: &str) -> String {
    let fp = match safe_path(workdir, path) {
        Ok(p) => p,
        Err(e) => return format!("Error: {}", e),
    };
    match fs::read_to_string(&fp) {
        Ok(c) => {
            if !c.contains(old_text) {
                return format!("Error: Text not found in {}", path);
            }
            let count = c.matches(old_text).count();
            let new_content = c.replacen(old_text, new_text, 1);
            match fs::write(&fp, new_content) {
                Ok(()) => {
                    if count > 1 {
                        format!("Edited {} (1 of {} occurrences replaced)", path, count)
                    } else {
                        format!("Edited {}", path)
                    }
                }
                Err(e) => format!("Error: {}", e),
            }
        }
        Err(e) => format!("Error: {}", e),
    }
}

pub fn run_list_directory(workdir: &Path, path: &str, max_depth: usize) -> String {
    let target = match safe_path(workdir, path) {
        Ok(p) => p,
        Err(e) => return format!("Error: {}", e),
    };
    if !target.is_dir() {
        return format!("Error: {} is not a directory", path);
    }
    let mut lines = Vec::new();
    walk_dir(&target, &target, &mut lines, max_depth, 0);
    let result = lines.join("\n");
    if result.is_empty() {
        "(empty directory)".into()
    } else if result.len() > 50000 {
        result[..50000].to_string()
    } else {
        result
    }
}

fn walk_dir(base: &Path, current: &Path, lines: &mut Vec<String>, max_depth: usize, depth: usize) {
    if depth > max_depth {
        return;
    }
    let mut entries: Vec<_> = match fs::read_dir(current) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return,
    };
    entries.sort_by(|a, b| {
        let a_dir = a.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let b_dir = b.file_type().map(|t| t.is_dir()).unwrap_or(false);
        b_dir
            .cmp(&a_dir)
            .then_with(|| a.file_name().to_ascii_lowercase().cmp(&b.file_name().to_ascii_lowercase()))
    });
    let indent = "  ".repeat(depth);
    for entry in entries {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(base).unwrap_or(&path);
        if path.is_dir() {
            lines.push(format!("{}{}/", indent, rel.display()));
            walk_dir(base, &path, lines, max_depth, depth + 1);
        } else {
            let size = path.metadata().map(|m| m.len()).unwrap_or(0);
            lines.push(format!("{}{}  ({} bytes)", indent, rel.display(), size));
        }
    }
}

pub fn run_grep_search(workdir: &Path, pattern: &str, path: &str, include: &str) -> String {
    let search_path = match safe_path(workdir, path) {
        Ok(p) => p,
        Err(e) => return format!("Error: {}", e),
    };

    // Try rg first
    let mut cmd = Command::new("rg");
    cmd.args(["--no-heading", "--line-number", "--color=never", "-m", "50"]);
    if !include.is_empty() {
        cmd.args(["-g", include]);
    }
    cmd.arg(pattern).arg(search_path.to_string_lossy().as_ref());
    cmd.current_dir(workdir);

    match cmd.output() {
        Ok(output) => {
            let out = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
            .trim()
            .to_string();
            if out.is_empty() {
                "(no matches)".into()
            } else if out.len() > 50000 {
                out[..50000].to_string()
            } else {
                out
            }
        }
        Err(_) => {
            // Fallback to grep
            let mut cmd = Command::new("grep");
            cmd.args(["-rn", "--color=never"]);
            if !include.is_empty() {
                cmd.args(["--include", include]);
            }
            cmd.arg(pattern).arg(search_path.to_string_lossy().as_ref());
            cmd.current_dir(workdir);
            match cmd.output() {
                Ok(output) => {
                    let out = format!(
                        "{}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    )
                    .trim()
                    .to_string();
                    let lines: Vec<&str> = out.lines().take(50).collect();
                    if lines.is_empty() {
                        "(no matches)".into()
                    } else {
                        lines.join("\n")
                    }
                }
                Err(e) => format!("Error: {}", e),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tool dispatch
// ---------------------------------------------------------------------------

pub fn dispatch_tool(workdir: &Path, name: &str, args: &Value) -> String {
    match name {
        "bash" => {
            let command = args["command"].as_str().unwrap_or("");
            run_bash(workdir, command)
        }
        "read_file" => {
            let path = args["path"].as_str().unwrap_or("");
            let offset = args["offset"].as_u64().unwrap_or(0) as usize;
            let limit = args["limit"].as_u64().unwrap_or(0) as usize;
            run_read_file(workdir, path, offset, limit)
        }
        "write_file" => {
            let path = args["path"].as_str().unwrap_or("");
            let content = args["content"].as_str().unwrap_or("");
            run_write_file(workdir, path, content)
        }
        "edit_file" => {
            let path = args["path"].as_str().unwrap_or("");
            let old_text = args["old_text"].as_str().unwrap_or("");
            let new_text = args["new_text"].as_str().unwrap_or("");
            run_edit_file(workdir, path, old_text, new_text)
        }
        "list_directory" => {
            let path = args["path"].as_str().unwrap_or(".");
            let max_depth = args["max_depth"].as_u64().unwrap_or(2) as usize;
            run_list_directory(workdir, path, max_depth)
        }
        "grep_search" => {
            let pattern = args["pattern"].as_str().unwrap_or("");
            let path = args["path"].as_str().unwrap_or(".");
            let include = args["include"].as_str().unwrap_or("");
            run_grep_search(workdir, pattern, path, include)
        }
        _ => format!("Error: Unknown tool '{}'", name),
    }
}

// ---------------------------------------------------------------------------
// OpenAI-format tool definitions
// ---------------------------------------------------------------------------

pub fn tool_definitions() -> Vec<Value> {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Run a shell command and return stdout+stderr.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "The shell command to execute."}
                    },
                    "required": ["command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read file contents with line numbers. Use offset/limit for large files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path":   {"type": "string", "description": "File path (relative to workspace root)."},
                        "offset": {"type": "integer", "description": "1-indexed start line (optional)."},
                        "limit":  {"type": "integer", "description": "Max lines to read (optional)."}
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Create or overwrite a file with the given content.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path":    {"type": "string", "description": "File path (relative to workspace root)."},
                        "content": {"type": "string", "description": "Full file content to write."}
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "edit_file",
                "description": "Replace the first occurrence of old_text with new_text in a file.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path":     {"type": "string", "description": "File path."},
                        "old_text": {"type": "string", "description": "Exact text to find (must match)."},
                        "new_text": {"type": "string", "description": "Replacement text."}
                    },
                    "required": ["path", "old_text", "new_text"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_directory",
                "description": "List files and directories in a tree structure.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path":      {"type": "string", "description": "Directory path (default: workspace root)."},
                        "max_depth": {"type": "integer", "description": "Max depth to traverse (default: 2)."}
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "grep_search",
                "description": "Search for a regex pattern in files using ripgrep. Returns matching lines with file paths and line numbers.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "Regex pattern to search for."},
                        "path":    {"type": "string", "description": "Directory or file to search in (default: workspace root)."},
                        "include": {"type": "string", "description": "Glob filter, e.g. '*.py' (optional)."}
                    },
                    "required": ["pattern"]
                }
            }
        }
    ])
    .as_array()
    .unwrap()
    .clone()
}
