use crate::types::*;
use colored::Colorize;
use serde_json::json;
use std::path::Path;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;

pub fn tool_definitions() -> Vec<AvailableTool> {
    vec![
        AvailableTool {
            tool_type: "function".into(),
            function: FunctionDef {
                name: "bash".into(),
                description: "Run a bash command and return stdout/stderr.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The bash command to execute"
                        },
                        "timeout": {
                            "type": "integer",
                            "description": "Timeout in seconds (default 120)"
                        }
                    },
                    "required": ["command"]
                }),
            },
        },
        AvailableTool {
            tool_type: "function".into(),
            function: FunctionDef {
                name: "read_file".into(),
                description: "Read a file's contents. Returns numbered lines.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute or relative path to the file"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Line number to start from (1-based)"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max number of lines to read"
                        }
                    },
                    "required": ["path"]
                }),
            },
        },
        AvailableTool {
            tool_type: "function".into(),
            function: FunctionDef {
                name: "write_file".into(),
                description: "Write content to a file, creating it if needed.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to write to"
                        },
                        "content": {
                            "type": "string",
                            "description": "Content to write"
                        }
                    },
                    "required": ["path", "content"]
                }),
            },
        },
        AvailableTool {
            tool_type: "function".into(),
            function: FunctionDef {
                name: "search_replace".into(),
                description:
                    "Search and replace text in a file. Provide exact match strings.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file"
                        },
                        "search": {
                            "type": "string",
                            "description": "Exact text to search for"
                        },
                        "replace": {
                            "type": "string",
                            "description": "Text to replace with"
                        }
                    },
                    "required": ["path", "search", "replace"]
                }),
            },
        },
        AvailableTool {
            tool_type: "function".into(),
            function: FunctionDef {
                name: "grep".into(),
                description:
                    "Search for a pattern in files. Respects .gitignore. Returns matching lines."
                        .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Regex pattern to search for"
                        },
                        "path": {
                            "type": "string",
                            "description": "Directory or file to search in (default: current dir)"
                        },
                        "include": {
                            "type": "string",
                            "description": "File glob pattern to include (e.g. '*.rs')"
                        }
                    },
                    "required": ["pattern"]
                }),
            },
        },
        AvailableTool {
            tool_type: "function".into(),
            function: FunctionDef {
                name: "glob".into(),
                description: "Find files matching a glob pattern. Skips .git, node_modules, target directories.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Glob pattern (e.g. 'src/**/*.rs')"
                        }
                    },
                    "required": ["pattern"]
                }),
            },
        },
        AvailableTool {
            tool_type: "function".into(),
            function: FunctionDef {
                name: "list_dir".into(),
                description: "List directory contents with file sizes and types. Shows files and subdirectories.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory path to list (default: current dir)"
                        },
                        "recursive": {
                            "type": "boolean",
                            "description": "List recursively (default: false, max 2 levels)"
                        }
                    },
                    "required": []
                }),
            },
        },
        AvailableTool {
            tool_type: "function".into(),
            function: FunctionDef {
                name: "memory_read".into(),
                description: "Read the persistent memory file. Memory persists across sessions.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
        },
        AvailableTool {
            tool_type: "function".into(),
            function: FunctionDef {
                name: "memory_write".into(),
                description: "Append a note to persistent memory. Use for user preferences, project conventions, important context that should persist across sessions.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "The note to append to memory"
                        }
                    },
                    "required": ["content"]
                }),
            },
        },
    ]
}

pub async fn execute_tool(name: &str, args: &serde_json::Value) -> String {
    match name {
        "bash" => tool_bash(args).await,
        "read_file" => tool_read_file(args),
        "write_file" => tool_write_file(args),
        "search_replace" => tool_search_replace(args),
        "grep" => tool_grep(args).await,
        "glob" => tool_glob(args),
        "list_dir" => tool_list_dir(args),
        "memory_read" => tool_memory_read(),
        "memory_write" => tool_memory_write(args),
        _ => format!("Unknown tool: {}", name),
    }
}

async fn tool_bash(args: &serde_json::Value) -> String {
    let command = args["command"].as_str().unwrap_or("");
    let timeout_secs = args["timeout"].as_u64().unwrap_or(120);

    let mut child = match Command::new("bash")
        .arg("-c")
        .arg(command)
        .env("TERM", "dumb")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("LESS", "-FX")
        .env("DEBIAN_FRONTEND", "noninteractive")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return format!("Failed to spawn command: {}", e),
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();

    // Stream stdout line by line for live feedback
    let stdout_handle = tokio::spawn(async move {
        let mut collected = String::new();
        if let Some(stdout) = stdout {
            let reader = tokio::io::BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut line_count = 0;
            while let Ok(Some(line)) = lines.next_line().await {
                // Show live output (first 20 lines, then dots)
                line_count += 1;
                if line_count <= 20 {
                    eprintln!("    {}", line.dimmed());
                } else if line_count == 21 {
                    eprintln!("    {}", "... (streaming)".dimmed());
                }
                collected.push_str(&line);
                collected.push('\n');
                if collected.len() > 16_000 {
                    collected.push_str("...\n(truncated)\n");
                    break;
                }
            }
        }
        collected
    });

    let stderr_handle = tokio::spawn(async move {
        let mut collected = String::new();
        if let Some(stderr) = stderr {
            let reader = tokio::io::BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                collected.push_str(&line);
                collected.push('\n');
                if collected.len() > 4_000 {
                    collected.push_str("...\n(truncated)\n");
                    break;
                }
            }
        }
        collected
    });

    let timeout_result = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        child.wait(),
    )
    .await;

    let code = match timeout_result {
        Ok(Ok(status)) => status.code().unwrap_or(-1),
        Ok(Err(e)) => return format!("Failed to wait for command: {}", e),
        Err(_) => {
            let _ = child.kill().await;
            return format!("Command timed out after {}s", timeout_secs);
        }
    };

    if let Ok(s) = stdout_handle.await {
        stdout_buf = s;
    }
    if let Ok(s) = stderr_handle.await {
        stderr_buf = s;
    }

    let mut result = String::new();
    if code != 0 {
        result.push_str(&format!("Exit code: {}\n", code));
    }
    if !stdout_buf.is_empty() {
        result.push_str(&stdout_buf);
    }
    if !stderr_buf.is_empty() {
        result.push_str(&format!("STDERR: {}", stderr_buf));
    }
    if result.is_empty() {
        "(no output)".to_string()
    } else {
        result
    }
}

fn tool_read_file(args: &serde_json::Value) -> String {
    let path = args["path"].as_str().unwrap_or("");
    let offset = args["offset"].as_u64().unwrap_or(1) as usize;
    let limit = args["limit"].as_u64().unwrap_or(2000) as usize;

    let p = Path::new(path);
    match std::fs::read_to_string(p) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().collect();
            let start = if offset > 0 { offset - 1 } else { 0 };
            let end = (start + limit).min(lines.len());

            let mut result = String::new();
            for (i, line) in lines[start..end].iter().enumerate() {
                result.push_str(&format!("{:>5}| {}\n", start + i + 1, line));
            }
            if result.is_empty() {
                "(empty file)".to_string()
            } else {
                result
            }
        }
        Err(e) => format!("Error reading {}: {}", path, e),
    }
}

fn tool_write_file(args: &serde_json::Value) -> String {
    let path = args["path"].as_str().unwrap_or("");
    let content = args["content"].as_str().unwrap_or("");

    let p = Path::new(path);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    match std::fs::write(p, content) {
        Ok(_) => format!("Wrote {} bytes to {}", content.len(), path),
        Err(e) => format!("Error writing {}: {}", path, e),
    }
}

fn tool_search_replace(args: &serde_json::Value) -> String {
    let path = args["path"].as_str().unwrap_or("");
    let search = args["search"].as_str().unwrap_or("");
    let replace = args["replace"].as_str().unwrap_or("");

    match std::fs::read_to_string(path) {
        Ok(content) => {
            let count = content.matches(search).count();
            if count == 0 {
                // Fuzzy hint: find the most similar line
                let search_first_line = search.lines().next().unwrap_or(search).trim();
                let mut best_match = ("", 0usize);
                for line in content.lines() {
                    let trimmed = line.trim();
                    let sim = similarity(search_first_line, trimmed);
                    if sim > best_match.1 {
                        best_match = (line, sim);
                    }
                }

                let hint = if best_match.1 > search_first_line.len() / 3 {
                    format!(
                        "\n\nClosest match in file ({}% similar):\n  \"{}\"",
                        (best_match.1 * 100) / search_first_line.len().max(1),
                        best_match.0.chars().take(120).collect::<String>()
                    )
                } else {
                    String::new()
                };

                return format!(
                    "No matches found for the search string in {}. Make sure whitespace and indentation match exactly.{}",
                    path, hint
                );
            }
            let new_content = content.replace(search, replace);
            match std::fs::write(path, &new_content) {
                Ok(_) => format!("Replaced {} occurrence(s) in {}", count, path),
                Err(e) => format!("Error writing {}: {}", path, e),
            }
        }
        Err(e) => format!("Error reading {}: {}", path, e),
    }
}

/// Simple character-level similarity (longest common subsequence length)
fn similarity(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    // Use simple matching character count (faster than LCS)
    let mut score = 0;
    let mut b_used = vec![false; b.len()];
    for ac in &a {
        for (j, bc) in b.iter().enumerate() {
            if !b_used[j] && ac == bc {
                score += 1;
                b_used[j] = true;
                break;
            }
        }
    }
    score
}

async fn tool_grep(args: &serde_json::Value) -> String {
    let pattern = args["pattern"].as_str().unwrap_or("");
    let path = args["path"].as_str().unwrap_or(".");
    let include = args["include"].as_str().unwrap_or("");

    // Prefer ripgrep (rg) if available — it respects .gitignore by default
    let has_rg = Command::new("which")
        .arg("rg")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);

    let output = if has_rg {
        let mut cmd = Command::new("rg");
        cmd.arg("-n").arg("--color=never").arg("--no-heading");

        if !include.is_empty() {
            cmd.arg("-g").arg(include);
        }

        cmd.arg(pattern).arg(path);
        cmd.output().await
    } else {
        // Fallback to grep with manual .gitignore exclusions
        let mut cmd = Command::new("grep");
        cmd.arg("-rn").arg("--color=never");

        if !include.is_empty() {
            cmd.arg("--include").arg(include);
        }

        cmd.arg("--exclude-dir=.git")
            .arg("--exclude-dir=node_modules")
            .arg("--exclude-dir=target")
            .arg("--exclude-dir=__pycache__")
            .arg("--exclude-dir=.venv")
            .arg("--exclude-dir=dist")
            .arg("--exclude-dir=build")
            .arg("--exclude=*.pyc")
            .arg("--exclude=*.lock")
            .arg(pattern)
            .arg(path);

        cmd.output().await
    };

    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.is_empty() {
                "No matches found.".to_string()
            } else if stdout.len() > 16_000 {
                format!(
                    "{}...\n(truncated, {} total bytes)",
                    &stdout[..16_000],
                    stdout.len()
                )
            } else {
                stdout.to_string()
            }
        }
        Err(e) => format!("grep failed: {}", e),
    }
}

fn tool_glob(args: &serde_json::Value) -> String {
    let pattern = args["pattern"].as_str().unwrap_or("");

    // Skip common noise directories
    const SKIP_DIRS: &[&str] = &[
        ".git",
        "node_modules",
        "target",
        "__pycache__",
        ".venv",
        "dist",
        "build",
        ".next",
    ];

    match glob::glob(pattern) {
        Ok(entries) => {
            let mut results: Vec<String> = Vec::new();
            for entry in entries.take(500) {
                match entry {
                    Ok(path) => {
                        let path_str = path.display().to_string();
                        // Skip noise directories
                        let should_skip = SKIP_DIRS
                            .iter()
                            .any(|d| path_str.contains(&format!("/{}/", d)));
                        if !should_skip {
                            results.push(path_str);
                        }
                    }
                    Err(e) => results.push(format!("(error: {})", e)),
                }
            }
            if results.is_empty() {
                "No files found.".to_string()
            } else {
                results.join("\n")
            }
        }
        Err(e) => format!("Invalid glob pattern: {}", e),
    }
}

fn tool_list_dir(args: &serde_json::Value) -> String {
    let path = args["path"].as_str().unwrap_or(".");
    let recursive = args["recursive"].as_bool().unwrap_or(false);

    list_dir_inner(Path::new(path), recursive, 0, 2)
}

fn list_dir_inner(path: &Path, recursive: bool, depth: usize, max_depth: usize) -> String {
    const SKIP: &[&str] = &[
        ".git",
        "node_modules",
        "target",
        "__pycache__",
        ".venv",
        "dist",
        ".next",
    ];

    let entries = match std::fs::read_dir(path) {
        Ok(e) => e,
        Err(e) => return format!("Error reading {}: {}", path.display(), e),
    };

    let indent = "  ".repeat(depth);
    let mut dirs = Vec::new();
    let mut files = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if SKIP.contains(&name.as_str()) {
            continue;
        }

        let meta = entry.metadata();
        if let Ok(meta) = meta {
            if meta.is_dir() {
                dirs.push(name);
            } else {
                let size = meta.len();
                let size_str = format_size(size);
                files.push(format!("{}{} ({})", indent, name, size_str));
            }
        }
    }

    dirs.sort();
    files.sort();

    let mut result = String::new();
    for dir in &dirs {
        result.push_str(&format!("{}{}/\n", indent, dir));
        if recursive && depth < max_depth {
            result.push_str(&list_dir_inner(&path.join(dir), recursive, depth + 1, max_depth));
        }
    }
    for file in &files {
        result.push_str(&format!("{}\n", file));
    }

    if result.is_empty() {
        format!("{}(empty)\n", indent)
    } else {
        result
    }
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn tool_memory_read() -> String {
    let content = crate::memory::load_memory();
    if content.is_empty() {
        "(memory is empty)".to_string()
    } else {
        content
    }
}

fn tool_memory_write(args: &serde_json::Value) -> String {
    let content = args["content"].as_str().unwrap_or("");
    if content.is_empty() {
        return "Nothing to write.".to_string();
    }
    match crate::memory::append_memory(content) {
        Ok(_) => format!(
            "Saved to memory ({} total bytes)",
            crate::memory::load_memory().len()
        ),
        Err(e) => format!("Failed to write memory: {}", e),
    }
}
