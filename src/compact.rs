use crate::llm::LlmClient;
use crate::types::*;
use colored::Colorize;

const COMPACT_PROMPT: &str = r#"Summarize the conversation so far in a way that preserves all important context for continuing the task. Include:
- What the user asked for
- What files were read/modified and their key contents
- What commands were run and their results
- Current state of the task (what's done, what's left)
- Any errors encountered and how they were resolved
- Key decisions made

Be thorough but concise. This summary replaces the conversation history."#;

/// Compact the conversation when context gets too large.
/// Keeps the system prompt and last few messages, summarizes the middle.
pub async fn compact_messages(
    client: &LlmClient,
    messages: &[Message],
    max_tokens: usize,
) -> Result<Option<Vec<Message>>, Box<dyn std::error::Error>> {
    let est_tokens: usize = messages.iter().map(|m| m.estimated_tokens()).sum();

    // Trigger at 80% of max
    let threshold = (max_tokens as f64 * 0.8) as usize;
    if est_tokens < threshold {
        return Ok(None);
    }

    eprintln!(
        "  {} ~{} tokens (threshold: {}), compacting...",
        "compact:".yellow().bold(),
        est_tokens,
        threshold
    );

    // Keep system prompt (first message) and last 6 messages
    let keep_tail = 6.min(messages.len().saturating_sub(1));
    let system_msg = &messages[0];
    let middle = &messages[1..messages.len() - keep_tail];
    let tail = &messages[messages.len() - keep_tail..];

    if middle.is_empty() {
        return Ok(None); // Nothing to compact
    }

    // Build a summary of the middle section
    let mut summary_parts = Vec::new();
    for msg in middle {
        let role_str = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool",
            Role::System => continue,
        };

        let content = msg.content.as_deref().unwrap_or("");
        // Truncate very long tool results
        let content = if content.len() > 500 && msg.role == Role::Tool {
            format!("{}... (truncated)", &content[..500])
        } else {
            content.to_string()
        };

        if let Some(tcs) = &msg.tool_calls {
            for tc in tcs {
                summary_parts.push(format!(
                    "[{} called {}({})]",
                    role_str, tc.function.name, tc.function.arguments
                ));
            }
        }

        if !content.is_empty() {
            let name = msg.name.as_deref().unwrap_or("");
            let prefix = if !name.is_empty() {
                format!("[{}/{}]", role_str, name)
            } else {
                format!("[{}]", role_str)
            };
            summary_parts.push(format!("{} {}", prefix, content));
        }
    }

    let conversation_text = summary_parts.join("\n");

    // Ask the LLM to summarize
    let summary_messages = vec![
        Message::system("You are a conversation summarizer. Be thorough and precise."),
        Message::user(&format!(
            "{}\n\n---\nConversation to summarize:\n\n{}",
            COMPACT_PROMPT, conversation_text
        )),
    ];

    let (summary_response, _stats) = client.chat(&summary_messages, &[]).await?;
    let summary_text = summary_response
        .content
        .unwrap_or_else(|| "(compaction failed)".into());

    eprintln!(
        "  {} compacted {} messages into summary ({} chars)",
        "done:".green(),
        middle.len(),
        summary_text.len()
    );

    // Rebuild: system + summary as user context + tail
    let mut new_messages = Vec::new();
    new_messages.push(system_msg.clone());
    new_messages.push(Message::user(&format!(
        "[Previous conversation summary]\n{}",
        summary_text
    )));
    new_messages.push(Message::assistant(
        "I understand the context from the summary. I'll continue from where we left off.",
    ));
    new_messages.extend_from_slice(tail);

    let new_est: usize = new_messages.iter().map(|m| m.estimated_tokens()).sum();
    eprintln!(
        "  {} {} -> {} tokens",
        "reduced:".dimmed(),
        est_tokens,
        new_est
    );

    Ok(Some(new_messages))
}
