use crate::render::{MarkdownRenderer, Spinner};
use crate::types::*;
use colored::Colorize;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::json;
use std::io::Write;
use std::time::{Duration, Instant};

const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 1000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Backend {
    OpenAI,    // OpenAI-compatible (Mistral, OpenAI, local)
    Anthropic, // Anthropic Messages API
}

impl Backend {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "anthropic" | "claude" => Backend::Anthropic,
            _ => Backend::OpenAI,
        }
    }
}

pub struct LlmClient {
    client: Client,
    api_base: String,
    api_key: String,
    model: String,
    temperature: f32,
    backend: Backend,
}

#[derive(Debug, Default)]
pub struct CompletionStats {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub duration_ms: u128,
}

impl LlmClient {
    pub fn new(
        api_base: &str,
        api_key: &str,
        model: &str,
        temperature: f32,
        backend: Backend,
    ) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(300))
                .build()
                .unwrap_or_else(|_| Client::new()),
            api_base: api_base.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            temperature,
            backend,
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

            let result = match self.backend {
                Backend::OpenAI => self.chat_openai(messages, tools).await,
                Backend::Anthropic => self.chat_anthropic(messages, tools).await,
            };

            match result {
                Ok(result) => return Ok(result),
                Err(e) => {
                    let err_str = e.to_string();
                    let is_retryable = err_str.contains("429")
                        || err_str.contains("500")
                        || err_str.contains("502")
                        || err_str.contains("503")
                        || err_str.contains("529")
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

    // ── OpenAI-compatible backend ──

    async fn chat_openai(
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

        let (msg, usage) = parse_openai_stream(resp).await?;

        Ok((
            msg,
            CompletionStats {
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                duration_ms: start.elapsed().as_millis(),
            },
        ))
    }

    // ── Anthropic Messages API backend ──

    async fn chat_anthropic(
        &self,
        messages: &[Message],
        tools: &[AvailableTool],
    ) -> Result<(Message, CompletionStats), Box<dyn std::error::Error>> {
        let start = Instant::now();

        // Anthropic separates system from messages
        let system_content = messages
            .iter()
            .find(|m| m.role == Role::System)
            .and_then(|m| m.content.as_deref())
            .unwrap_or("");

        // Convert messages to Anthropic format (skip system)
        let anthropic_messages: Vec<serde_json::Value> = messages
            .iter()
            .filter(|m| m.role != Role::System)
            .map(|m| convert_to_anthropic_message(m))
            .collect();

        let mut body = json!({
            "model": self.model,
            "max_tokens": 8192,
            "system": system_content,
            "messages": anthropic_messages,
            "stream": true,
        });

        if !tools.is_empty() {
            let anthropic_tools: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.function.name,
                        "description": t.function.description,
                        "input_schema": t.function.parameters,
                    })
                })
                .collect();
            body["tools"] = json!(anthropic_tools);
        }

        let resp = self
            .client
            .post(format!("{}/messages", self.api_base))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("Anthropic API error {}: {}", status, text).into());
        }

        let (msg, usage) = parse_anthropic_stream(resp).await?;

        Ok((
            msg,
            CompletionStats {
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                duration_ms: start.elapsed().as_millis(),
            },
        ))
    }

}

/// Convert our Message to Anthropic format
fn convert_to_anthropic_message(msg: &Message) -> serde_json::Value {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "user", // Tool results go as user messages in Anthropic
        Role::System => "user",
    };

    // Tool result messages become tool_result content blocks
    if msg.role == Role::Tool {
        return json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": msg.tool_call_id.as_deref().unwrap_or(""),
                "content": msg.content.as_deref().unwrap_or(""),
            }]
        });
    }

    // Assistant messages with tool calls
    if msg.role == Role::Assistant {
        if let Some(ref tcs) = msg.tool_calls {
            let mut content: Vec<serde_json::Value> = Vec::new();
            if let Some(ref text) = msg.content {
                if !text.is_empty() {
                    content.push(json!({"type": "text", "text": text}));
                }
            }
            for tc in tcs {
                let input: serde_json::Value =
                    serde_json::from_str(&tc.function.arguments).unwrap_or(json!({}));
                content.push(json!({
                    "type": "tool_use",
                    "id": tc.id,
                    "name": tc.function.name,
                    "input": input,
                }));
            }
            return json!({"role": "assistant", "content": content});
        }
    }

    json!({
        "role": role,
        "content": msg.content.as_deref().unwrap_or(""),
    })
}

/// Parse OpenAI SSE stream with markdown rendering and spinner
async fn parse_openai_stream(
    resp: reqwest::Response,
) -> Result<(Message, Usage), Box<dyn std::error::Error>> {
    let mut content = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut usage = Usage::default();
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut spinner = Spinner::start("thinking");
    let _renderer = MarkdownRenderer::new();
    let mut first_token = true;

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
                        if first_token {
                            spinner.stop();
                            first_token = false;
                        }
                        content.push_str(c);
                        // Use simple print for now — markdown renderer
                        // has issues with partial lines during streaming
                        print!("{}", c);
                        std::io::stdout().flush().ok();
                    }
                    if let Some(tcs) = &delta.tool_calls {
                        if first_token {
                            spinner.stop();
                            first_token = false;
                        }
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

    spinner.stop();
    drop(_renderer);

    if !content.is_empty() {
        println!();
    }

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

    Ok((msg, usage))
}

/// Parse Anthropic SSE stream
async fn parse_anthropic_stream(
    resp: reqwest::Response,
) -> Result<(Message, Usage), Box<dyn std::error::Error>> {
    let mut content = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut usage = Usage::default();
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut current_tool_id = String::new();
    let mut current_tool_name = String::new();
    let mut current_tool_input = String::new();
    let mut spinner = Spinner::start("thinking");
    let mut first_token = true;

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
            let event: serde_json::Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let event_type = event["type"].as_str().unwrap_or("");

            match event_type {
                "message_start" => {
                    if let Some(u) = event["message"]["usage"].as_object() {
                        usage.prompt_tokens =
                            u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    }
                }
                "content_block_start" => {
                    let block = &event["content_block"];
                    if block["type"].as_str() == Some("tool_use") {
                        current_tool_id =
                            block["id"].as_str().unwrap_or("").to_string();
                        current_tool_name =
                            block["name"].as_str().unwrap_or("").to_string();
                        current_tool_input.clear();
                    }
                }
                "content_block_delta" => {
                    let delta = &event["delta"];
                    match delta["type"].as_str() {
                        Some("text_delta") => {
                            if let Some(text) = delta["text"].as_str() {
                                if first_token {
                                    spinner.stop();
                                    first_token = false;
                                }
                                content.push_str(text);
                                print!("{}", text);
                                std::io::stdout().flush().ok();
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(partial) = delta["partial_json"].as_str() {
                                current_tool_input.push_str(partial);
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    if !current_tool_name.is_empty() {
                        tool_calls.push(ToolCall {
                            id: current_tool_id.clone(),
                            call_type: "function".to_string(),
                            function: FunctionCall {
                                name: current_tool_name.clone(),
                                arguments: current_tool_input.clone(),
                            },
                            index: Some(tool_calls.len()),
                        });
                        current_tool_name.clear();
                        current_tool_id.clear();
                        current_tool_input.clear();
                    }
                }
                "message_delta" => {
                    if let Some(u) = event["usage"].as_object() {
                        usage.completion_tokens = u
                            .get("output_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                    }
                }
                _ => {}
            }
        }
    }

    spinner.stop();

    if !content.is_empty() {
        println!();
    }

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

    Ok((msg, usage))
}
