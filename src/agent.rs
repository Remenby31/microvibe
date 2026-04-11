use crate::llm::LlmClient;
use crate::tools::{execute_tool, tool_definitions};
use crate::types::*;
use colored::Colorize;

const MAX_TOOL_ROUNDS: usize = 50;

pub struct Agent {
    client: LlmClient,
    messages: Vec<Message>,
}

impl Agent {
    pub fn new(client: LlmClient, system_prompt: &str) -> Self {
        Self {
            client,
            messages: vec![Message::system(system_prompt)],
        }
    }

    pub async fn run_turn(&mut self, user_input: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.messages.push(Message::user(user_input));
        let tools = tool_definitions();

        for _ in 0..MAX_TOOL_ROUNDS {
            let assistant_msg = self.client.chat(&self.messages, &tools).await?;

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

                // Display tool call
                print_tool_call(name, &args);

                let result = execute_tool(name, &args).await;

                // Display result summary
                print_tool_result(name, &result);

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

    pub fn message_count(&self) -> usize {
        self.messages.len()
    }
}

fn print_tool_call(name: &str, args: &serde_json::Value) {
    match name {
        "bash" => {
            let cmd = args["command"].as_str().unwrap_or("?");
            eprintln!("{} {}", "  > bash:".cyan().bold(), cmd.dimmed());
        }
        "read_file" => {
            let path = args["path"].as_str().unwrap_or("?");
            eprintln!("{} {}", "  > read:".cyan().bold(), path.dimmed());
        }
        "write_file" => {
            let path = args["path"].as_str().unwrap_or("?");
            eprintln!("{} {}", "  > write:".cyan().bold(), path.dimmed());
        }
        "search_replace" => {
            let path = args["path"].as_str().unwrap_or("?");
            eprintln!("{} {}", "  > edit:".cyan().bold(), path.dimmed());
        }
        "grep" => {
            let pattern = args["pattern"].as_str().unwrap_or("?");
            eprintln!("{} {}", "  > grep:".cyan().bold(), pattern.dimmed());
        }
        "glob" => {
            let pattern = args["pattern"].as_str().unwrap_or("?");
            eprintln!("{} {}", "  > glob:".cyan().bold(), pattern.dimmed());
        }
        _ => {
            eprintln!("{} {}", "  > tool:".cyan().bold(), name.dimmed());
        }
    }
}

fn print_tool_result(name: &str, result: &str) {
    let lines: Vec<&str> = result.lines().collect();
    let summary = if lines.len() > 3 {
        format!("{} ({} lines)", lines[0], lines.len())
    } else {
        result.chars().take(120).collect()
    };
    eprintln!("    {} {}", "=".dimmed(), summary.dimmed());
    let _ = name;
}
