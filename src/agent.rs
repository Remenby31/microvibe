use crate::approval::{check_tool_approval, ApprovalResult};
use crate::llm::LlmClient;
use crate::session::SessionStats;
use crate::tools::{execute_tool, tool_definitions};
use crate::types::*;
use colored::Colorize;

const MAX_TOOL_ROUNDS: usize = 50;

pub struct Agent {
    client: LlmClient,
    messages: Vec<Message>,
    pub stats: SessionStats,
    auto_approve: bool,
    session_approved_patterns: Vec<String>,
}

impl Agent {
    pub fn new(client: LlmClient, system_prompt: &str, auto_approve: bool) -> Self {
        Self {
            client,
            messages: vec![Message::system(system_prompt)],
            stats: SessionStats::default(),
            auto_approve,
            session_approved_patterns: Vec::new(),
        }
    }

    pub async fn run_turn(&mut self, user_input: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.messages.push(Message::user(user_input));
        self.stats.turns += 1;
        let tools = tool_definitions();

        for round in 0..MAX_TOOL_ROUNDS {
            let (assistant_msg, completion_stats) =
                self.client.chat(&self.messages, &tools).await?;

            // Track token usage
            self.stats.prompt_tokens += completion_stats.prompt_tokens;
            self.stats.completion_tokens += completion_stats.completion_tokens;

            // Show token info on first response of each turn
            if round == 0 && completion_stats.prompt_tokens > 0 {
                let tps = if completion_stats.duration_ms > 0 {
                    (completion_stats.completion_tokens as f64
                        / completion_stats.duration_ms as f64)
                        * 1000.0
                } else {
                    0.0
                };
                eprintln!(
                    "  {} {} | {} | {:.0} tok/s",
                    "tokens:".dimmed(),
                    format!("{}in", completion_stats.prompt_tokens).dimmed(),
                    format!("{}out", completion_stats.completion_tokens).dimmed(),
                    tps
                );
            }

            let has_tool_calls = assistant_msg
                .tool_calls
                .as_ref()
                .is_some_and(|tc| !tc.is_empty());

            self.messages.push(assistant_msg.clone());

            if !has_tool_calls {
                return Ok(());
            }

            let tool_calls = assistant_msg.tool_calls.unwrap();
            for tc in &tool_calls {
                let name = &tc.function.name;
                let args: serde_json::Value =
                    serde_json::from_str(&tc.function.arguments).unwrap_or_default();

                // Tool approval check
                let approval = check_tool_approval(
                    name,
                    &args,
                    self.auto_approve,
                    &self.session_approved_patterns,
                );

                match approval {
                    ApprovalResult::Denied => {
                        self.messages.push(Message::tool_result(
                            &tc.id,
                            name,
                            "Tool call denied by user.",
                        ));
                        continue;
                    }
                    ApprovalResult::AlwaysApprove => {
                        let prefix = extract_approval_prefix(name, &args);
                        if !prefix.is_empty() {
                            self.session_approved_patterns.push(prefix);
                        }
                    }
                    ApprovalResult::Approved => {}
                }

                // Display tool call
                print_tool_call(name, &args);

                let result = execute_tool(name, &args).await;
                self.stats.tool_calls += 1;

                // Display result summary
                print_tool_result(&result);

                self.messages
                    .push(Message::tool_result(&tc.id, name, &result));
            }

            // Check context size (rough estimate)
            let est_tokens: usize = self.messages.iter().map(|m| m.estimated_tokens()).sum();
            if est_tokens > 100_000 {
                eprintln!(
                    "  {} ~{} tokens in context",
                    "warning:".yellow().bold(),
                    est_tokens
                );
            }
        }

        eprintln!(
            "{}",
            "Warning: reached max tool call rounds".yellow().bold()
        );
        Ok(())
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    pub fn context_tokens(&self) -> usize {
        self.messages.iter().map(|m| m.estimated_tokens()).sum()
    }

    pub fn model_name(&self) -> &str {
        self.client.model_name()
    }
}

fn extract_approval_prefix(name: &str, args: &serde_json::Value) -> String {
    if name == "bash" {
        let cmd = args["command"].as_str().unwrap_or("");
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        if parts.len() >= 2 && !parts[1].starts_with('-') {
            format!("{} {}", parts[0], parts[1])
        } else if !parts.is_empty() {
            parts[0].to_string()
        } else {
            String::new()
        }
    } else {
        String::new()
    }
}

fn print_tool_call(name: &str, args: &serde_json::Value) {
    let (label, detail) = match name {
        "bash" => ("bash", args["command"].as_str().unwrap_or("?")),
        "read_file" => ("read", args["path"].as_str().unwrap_or("?")),
        "write_file" => ("write", args["path"].as_str().unwrap_or("?")),
        "search_replace" => ("edit", args["path"].as_str().unwrap_or("?")),
        "grep" => ("grep", args["pattern"].as_str().unwrap_or("?")),
        "glob" => ("glob", args["pattern"].as_str().unwrap_or("?")),
        _ => ("tool", name),
    };
    eprintln!(
        "  {} {}",
        format!("{}:", label).cyan().bold(),
        detail.dimmed()
    );

    // For search_replace, show a mini diff
    if name == "search_replace" {
        if let (Some(search), Some(replace)) = (args["search"].as_str(), args["replace"].as_str())
        {
            let search_preview: String = search.lines().take(3).collect::<Vec<_>>().join("\\n");
            let replace_preview: String = replace.lines().take(3).collect::<Vec<_>>().join("\\n");
            if search_preview.len() < 120 {
                eprintln!("    {} {}", "-".red(), search_preview.red());
                eprintln!("    {} {}", "+".green(), replace_preview.green());
            }
        }
    }
}

fn print_tool_result(result: &str) {
    let lines: Vec<&str> = result.lines().collect();
    let summary = if lines.len() > 3 {
        format!("{} ({} lines)", lines[0], lines.len())
    } else {
        result.chars().take(120).collect()
    };
    eprintln!("    {} {}", "=".dimmed(), summary.dimmed());
}
