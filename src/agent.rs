use crate::approval::{check_tool_approval, ApprovalResult};
use crate::compact::compact_messages;
use crate::llm::LlmClient;
use crate::session::SessionStats;
use crate::tools::{execute_tool, tool_definitions};
use crate::types::*;
use colored::Colorize;
use std::time::Instant;

const MAX_TOOL_ROUNDS: usize = 50;

pub struct Agent {
    client: LlmClient,
    messages: Vec<Message>,
    pub stats: SessionStats,
    auto_approve: bool,
    session_approved_patterns: Vec<String>,
    max_context_tokens: usize,
    checkpoints: Vec<Vec<Message>>,
}

impl Agent {
    pub fn new(
        client: LlmClient,
        system_prompt: &str,
        auto_approve: bool,
        max_context_tokens: usize,
    ) -> Self {
        Self {
            client,
            messages: vec![Message::system(system_prompt)],
            stats: SessionStats::default(),
            auto_approve,
            session_approved_patterns: Vec::new(),
            max_context_tokens,
            checkpoints: Vec::new(),
        }
    }

    /// Save a checkpoint of the current message state (for undo)
    fn save_checkpoint(&mut self) {
        // Keep max 10 checkpoints
        if self.checkpoints.len() >= 10 {
            self.checkpoints.remove(0);
        }
        self.checkpoints.push(self.messages.clone());
    }

    /// Undo to the last checkpoint
    pub fn undo(&mut self) -> bool {
        if let Some(prev) = self.checkpoints.pop() {
            self.messages = prev;
            true
        } else {
            false
        }
    }

    pub fn checkpoint_count(&self) -> usize {
        self.checkpoints.len()
    }

    pub async fn run_turn(&mut self, user_input: &str) -> Result<(), Box<dyn std::error::Error>> {
        // Save checkpoint before each turn
        self.save_checkpoint();

        self.messages.push(Message::user(user_input));
        self.stats.turns += 1;
        let tools = tool_definitions();

        for round in 0..MAX_TOOL_ROUNDS {
            // Context compaction check before each LLM call
            if let Some(compacted) =
                compact_messages(&self.client, &self.messages, self.max_context_tokens).await?
            {
                self.messages = compacted;
            }

            let turn_start = Instant::now();
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
                    "  {} {} | {} | {:.0} tok/s | ~{} ctx | {:.1}s",
                    "tokens:".dimmed(),
                    format!("{}in", completion_stats.prompt_tokens).dimmed(),
                    format!("{}out", completion_stats.completion_tokens).dimmed(),
                    tps,
                    self.context_tokens(),
                    turn_start.elapsed().as_secs_f64()
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

            // Phase 1: Check approval for all tool calls first
            let mut approved_calls: Vec<(&ToolCall, serde_json::Value)> = Vec::new();
            for tc in &tool_calls {
                let name = &tc.function.name;
                let args: serde_json::Value =
                    serde_json::from_str(&tc.function.arguments).unwrap_or_default();

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
                    }
                    ApprovalResult::AlwaysApprove => {
                        let prefix = extract_approval_prefix(name, &args);
                        if !prefix.is_empty() {
                            self.session_approved_patterns.push(prefix);
                        }
                        approved_calls.push((tc, args));
                    }
                    ApprovalResult::Approved => {
                        approved_calls.push((tc, args));
                    }
                }
            }

            // Phase 2: Execute approved tool calls
            // Parallel for read-only tools, sequential for mutating tools
            let (readonly, mutating): (Vec<_>, Vec<_>) = approved_calls
                .iter()
                .partition(|(tc, _)| is_readonly_tool(&tc.function.name));

            // Execute read-only tools in parallel
            if readonly.len() > 1 {
                eprintln!(
                    "  {} {} tools in parallel",
                    "parallel:".dimmed(),
                    readonly.len()
                );
            }

            let mut parallel_results: Vec<(String, String, String)> = Vec::new(); // (id, name, result)

            if !readonly.is_empty() {
                let handles: Vec<_> = readonly
                    .into_iter()
                    .map(|(tc, args)| {
                        let id = tc.id.clone();
                        let name = tc.function.name.clone();
                        let args = args.clone();
                        tokio::spawn(async move {
                            print_tool_call(&name, &args);
                            let result = execute_tool(&name, &args).await;
                            print_tool_result(&result);
                            (id, name, result)
                        })
                    })
                    .collect();

                for handle in handles {
                    if let Ok(result) = handle.await {
                        parallel_results.push(result);
                    }
                }
            }

            // Add parallel results to messages
            for (id, name, result) in &parallel_results {
                self.stats.tool_calls += 1;
                self.messages
                    .push(Message::tool_result(id, name, result));
            }

            // Execute mutating tools sequentially
            for (tc, args) in &mutating {
                let name = &tc.function.name;
                print_tool_call(name, args);

                let result = execute_tool(name, args).await;
                self.stats.tool_calls += 1;
                print_tool_result(&result);

                self.messages
                    .push(Message::tool_result(&tc.id, name, &result));
            }
        }

        eprintln!(
            "{}",
            "Warning: reached max tool call rounds".yellow().bold()
        );
        Ok(())
    }

    /// Force context compaction now
    pub async fn force_compact(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Use a very low threshold to force compaction
        if let Some(compacted) = compact_messages(&self.client, &self.messages, 1).await? {
            self.messages = compacted;
        } else {
            eprintln!("{}", "Nothing to compact.".dimmed());
        }
        Ok(())
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn context_tokens(&self) -> usize {
        self.messages.iter().map(|m| m.estimated_tokens()).sum()
    }
}

/// Tools that only read data and can be safely parallelized
fn is_readonly_tool(name: &str) -> bool {
    matches!(name, "read_file" | "grep" | "glob")
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
        if let (Some(search), Some(replace)) =
            (args["search"].as_str(), args["replace"].as_str())
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
