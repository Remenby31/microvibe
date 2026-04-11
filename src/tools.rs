use crate::types::*;
use serde_json::json;
use std::path::Path;
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
                description: "Search and replace text in a file. Provide exact match strings.".into(),
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
                description: "Search for a pattern in files. Returns matching lines with file paths.".into(),
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
                description: "Find files matching a glob pattern.".into(),
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
        _ => format!("Unknown tool: {}", name),
    }
}

async fn tool_bash(args: &serde_json::Value) -> String {
    let command = args["command"].as_str().unwrap_or("");
    let timeout_secs = args["timeout"].as_u64().unwrap_or(120);

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        Command::new("bash")
            .arg("-c")
            .arg(command)
            .env("TERM", "dumb")
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let code = output.status.code().unwrap_or(-1);

            let mut result = String::new();
            if code != 0 {
                result.push_str(&format!("Exit code: {}\n", code));
            }
            if !stdout.is_empty() {
                // Truncate to 16KB
                let s = if stdout.len() > 16_000 {
                    format!("{}...\n(truncated)", &stdout[..16_000])
                } else {
                    stdout.to_string()
                };
                result.push_str(&s);
            }
            if !stderr.is_empty() {
                let s = if stderr.len() > 4_000 {
                    format!("{}...\n(truncated)", &stderr[..4_000])
                } else {
                    stderr.to_string()
                };
                result.push_str(&format!("STDERR: {}", s));
            }
            if result.is_empty() {
                "(no output)".to_string()
            } else {
                result
            }
        }
        Ok(Err(e)) => format!("Failed to run command: {}", e),
        Err(_) => format!("Command timed out after {}s", timeout_secs),
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
                return format!("No matches found for the search string in {}", path);
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

async fn tool_grep(args: &serde_json::Value) -> String {
    let pattern = args["pattern"].as_str().unwrap_or("");
    let path = args["path"].as_str().unwrap_or(".");
    let include = args["include"].as_str().unwrap_or("");

    let mut cmd = Command::new("grep");
    cmd.arg("-rn").arg("--color=never");

    if !include.is_empty() {
        cmd.arg("--include").arg(include);
    }

    // Exclude common noise
    cmd.arg("--exclude-dir=.git")
        .arg("--exclude-dir=node_modules")
        .arg("--exclude-dir=target")
        .arg("--exclude-dir=__pycache__")
        .arg(pattern)
        .arg(path);

    match cmd.output().await {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.is_empty() {
                "No matches found.".to_string()
            } else if stdout.len() > 16_000 {
                format!("{}...\n(truncated, {} total bytes)", &stdout[..16_000], stdout.len())
            } else {
                stdout.to_string()
            }
        }
        Err(e) => format!("grep failed: {}", e),
    }
}

fn tool_glob(args: &serde_json::Value) -> String {
    let pattern = args["pattern"].as_str().unwrap_or("");

    match glob::glob(pattern) {
        Ok(entries) => {
            let mut results: Vec<String> = Vec::new();
            for entry in entries.take(500) {
                match entry {
                    Ok(path) => results.push(path.display().to_string()),
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
