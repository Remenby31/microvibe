use crate::types::*;
use colored::Colorize;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::json;
use std::io::Write;
use std::time::{Duration, Instant};

const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 1000;

pub struct LlmClient {
    client: Client,
    api_base: String,
    api_key: String,
    model: String,
    temperature: f32,
}

#[derive(Debug, Default)]
pub struct CompletionStats {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub duration_ms: u128,
}

impl LlmClient {
    pub fn new(api_base: &str, api_key: &str, model: &str, temperature: f32) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(300))
                .build()
                .unwrap_or_else(|_| Client::new()),
            api_base: api_base.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            temperature,
        }
    }

    pub async fn chat(
        &self,
        messages: &[Message],
        tools: &[AvailableTool],
    ) -> Result<(Message, CompletionStats), Box<dyn std::error::Error>> {
        let mut last_err = String::new();

        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                let backoff = INITIAL_BACKOFF_MS * 2u64.pow(attempt - 1);
                eprintln!(
                    "  {} retry {}/{} in {}ms...",
                    "retry:".yellow(),
                    attempt,
                    MAX_RETRIES,
                    backoff
                );
                tokio::time::sleep(Duration::from_millis(backoff)).await;
            }

            match self.chat_attempt(messages, tools).await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    let err_str = e.to_string();
                    let is_retryable = err_str.contains("429")
                        || err_str.contains("500")
                        || err_str.contains("502")
                        || err_str.contains("503")
                        || err_str.contains("timeout")
                        || err_str.contains("connection");

                    if !is_retryable || attempt == MAX_RETRIES {
                        return Err(e);
                    }

                    last_err = err_str;
                    eprintln!(
                        "  {} {}",
                        "error:".red(),
                        last_err.chars().take(100).collect::<String>().dimmed()
                    );
                }
            }
        }

        Err(format!("All {} retries exhausted: {}", MAX_RETRIES, last_err).into())
    }

    async fn chat_attempt(
        &self,
        messages: &[Message],
        tools: &[AvailableTool],
    ) -> Result<(Message, CompletionStats), Box<dyn std::error::Error>> {
        let start = Instant::now();
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "temperature": self.temperature,
            "stream": true,
        });

        if !tools.is_empty() {
            body["tools"] = serde_json::to_value(tools)?;
            body["tool_choice"] = json!("auto");
        }

        let resp = self
            .client
            .post(format!("{}/chat/completions", self.api_base))
            .bearer_auth(&self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("API error {}: {}", status, text).into());
        }

        let mut content = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut usage = Usage::default();
        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(newline_pos) = buffer.find('\n') {
                let line = buffer[..newline_pos].trim().to_string();
                buffer = buffer[newline_pos + 1..].to_string();

                if !line.starts_with("data: ") {
                    continue;
                }
                let data = &line[6..];
                if data == "[DONE]" {
                    continue;
                }

                let parsed: StreamChunk = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                if let Some(u) = &parsed.usage {
                    usage.prompt_tokens = u.prompt_tokens;
                    usage.completion_tokens = u.completion_tokens;
                }

                for choice in &parsed.choices {
                    if let Some(delta) = &choice.delta {
                        if let Some(c) = &delta.content {
                            content.push_str(c);
                            print!("{}", c);
                            std::io::stdout().flush().ok();
                        }
                        if let Some(tcs) = &delta.tool_calls {
                            for tc in tcs {
                                let idx = tc.index.unwrap_or(0);
                                while tool_calls.len() <= idx {
                                    tool_calls.push(ToolCall {
                                        id: String::new(),
                                        call_type: "function".to_string(),
                                        function: FunctionCall {
                                            name: String::new(),
                                            arguments: String::new(),
                                        },
                                        index: Some(tool_calls.len()),
                                    });
                                }
                                if !tc.id.is_empty() {
                                    tool_calls[idx].id = tc.id.clone();
                                }
                                if !tc.function.name.is_empty() {
                                    tool_calls[idx].function.name = tc.function.name.clone();
                                }
                                tool_calls[idx]
                                    .function
                                    .arguments
                                    .push_str(&tc.function.arguments);
                            }
                        }
                    }
                }
            }
        }

        if !content.is_empty() {
            println!();
        }

        let stats = CompletionStats {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            duration_ms: start.elapsed().as_millis(),
        };

        let msg = Message {
            role: Role::Assistant,
            content: if content.is_empty() {
                None
            } else {
                Some(content)
            },
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            tool_call_id: None,
            name: None,
        };

        Ok((msg, stats))
    }

    pub fn model_name(&self) -> &str {
        &self.model
    }
}
