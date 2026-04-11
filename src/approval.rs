use colored::Colorize;
use std::io::{self, Write};

/// Commands that are always safe to run without approval
const SAFE_COMMANDS: &[&str] = &[
    "ls", "cat", "head", "tail", "pwd", "echo", "wc", "file", "stat", "which", "whoami", "uname",
    "tree", "git status", "git log", "git diff", "git branch", "git show", "cd",
];

/// Commands that are always denied
const DENIED_COMMANDS: &[&str] = &[
    "rm -rf /",
    "rm -rf /*",
    "dd if=",
    "mkfs",
    ":(){",
    "passwd",
    "vim",
    "vi",
    "nano",
    "emacs",
    "python -i",
    "python3 -i",
    "bash -i",
    "sh -i",
];

/// Dangerous patterns that always require approval
const SENSITIVE_PATTERNS: &[&str] = &[
    "sudo",
    "rm -rf",
    "rm -r",
    "chmod",
    "chown",
    "kill",
    "pkill",
    "git push",
    "git reset",
    "git checkout --",
    "git clean",
    "docker rm",
    "docker rmi",
    "curl",
    "wget",
    "ssh",
    "scp",
];

#[derive(Debug, PartialEq)]
pub enum ApprovalResult {
    Approved,
    Denied,
    AlwaysApprove,
}

pub fn check_tool_approval(
    tool_name: &str,
    args: &serde_json::Value,
    auto_approve: bool,
    session_approved: &[String],
) -> ApprovalResult {
    // Non-bash tools are always approved (read_file, grep, glob are safe)
    if tool_name != "bash" {
        return ApprovalResult::Approved;
    }

    let command = args["command"].as_str().unwrap_or("");

    // Check deny list
    for denied in DENIED_COMMANDS {
        if command.contains(denied) {
            eprintln!(
                "  {} {}",
                "BLOCKED:".red().bold(),
                format!("'{}' matches deny pattern '{}'", command, denied).red()
            );
            return ApprovalResult::Denied;
        }
    }

    // Auto-approve mode
    if auto_approve {
        return ApprovalResult::Approved;
    }

    // Check safe commands
    for safe in SAFE_COMMANDS {
        if command.starts_with(safe) || command == *safe {
            return ApprovalResult::Approved;
        }
    }

    // Check session-approved patterns
    let cmd_prefix = extract_command_prefix(command);
    if session_approved.iter().any(|p| cmd_prefix.starts_with(p)) {
        return ApprovalResult::Approved;
    }

    // Check if it's sensitive
    let is_sensitive = SENSITIVE_PATTERNS.iter().any(|p| command.contains(p));

    if is_sensitive {
        eprintln!(
            "  {} {}",
            "SENSITIVE:".yellow().bold(),
            command.yellow()
        );
    }

    // Ask user
    prompt_approval(command)
}

fn extract_command_prefix(command: &str) -> String {
    // Extract the base command (first word, or first pipe segment)
    let first_segment = command.split('|').next().unwrap_or(command).trim();
    let parts: Vec<&str> = first_segment.split_whitespace().collect();
    if parts.is_empty() {
        return String::new();
    }
    // Return command + first arg if it looks like a subcommand
    if parts.len() >= 2 && !parts[1].starts_with('-') {
        format!("{} {}", parts[0], parts[1])
    } else {
        parts[0].to_string()
    }
}

fn prompt_approval(command: &str) -> ApprovalResult {
    eprint!(
        "  {} {} {} ",
        "Approve?".cyan().bold(),
        format!("bash: {}", command).dimmed(),
        "[y/n/a(lways)]".dimmed()
    );
    io::stderr().flush().ok();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return ApprovalResult::Denied;
    }

    match input.trim().to_lowercase().as_str() {
        "y" | "yes" => ApprovalResult::Approved,
        "a" | "always" => ApprovalResult::AlwaysApprove,
        _ => ApprovalResult::Denied,
    }
}
