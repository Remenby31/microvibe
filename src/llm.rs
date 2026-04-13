use crate::events::{EventSender, TuiEvent};
use crate::render::{Spinner, StreamRenderer};
use crate::types::*;
use colored::Colorize;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::json;
use std::time::{Duration, Instant};

const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 1000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Backend {
    OpenAI,
    Anthropic,
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
    event_sender: Option<EventSender>,
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
            event_sender: None,
        }
    }

    /// Set the event sender for TUI mode
    pub fn set_event_sender(&mut self, sender: EventSender) {
        self.event_sender = Some(sender);
    }

    pub fn emit(&self, event: TuiEvent) {
        if let Some(ref sender) = self.event_sender {
            sender.send(event);
        }
    }

    pub fn is_tui_mode(&self) -> bool {
        self.event_sender.is_some()
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
                if self.is_tui_mode() {
                    self.emit(TuiEvent::SystemMessage(format!(
                        "Retry {}/{} in {}ms...",
                        attempt, MAX_RETRIES, backoff
                    )));
                } else {
                    eprintln!(
                        "  {} retry {}/{} in {}ms...",
                        "retry:".yellow(),
                        attempt,
                        MAX_RETRIES,
                        backoff
                    );
                }
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
                    if !self.is_tui_mode() {
                        eprintln!(
                            "  {} {}",
                            "error:".red(),
                            last_err.chars().take(100).collect::<String>().dimmed()
                        );
                    }
                }
            }
        }

        Err(format!("All {} retries exhausted: {}", MAX_RETRIES, last_err).into())
    }

    // ── Output helpers ──

    fn output_text(&self, text: &str, renderer: &mut StreamRenderer) {
        if self.is_tui_mode() {
            self.emit(TuiEvent::TextDelta(text.to_string()));
        } else {
            renderer.push_streaming(text);
        }
    }

    fn output_done(&self, renderer: &mut StreamRenderer) {
        if self.is_tui_mode() {
            self.emit(TuiEvent::TextDone);
        } else {
            renderer.finish();
        }
    }

    fn start_spinner(&self) -> Option<Spinner> {
        if self.is_tui_mode() {
            self.emit(TuiEvent::ThinkingStart);
            None
        } else {
            Some(Spinner::start("thinking"))
        }
    }

    fn stop_spinner(&self, spinner: &mut Option<Spinner>) {
        if self.is_tui_mode() {
            self.emit(TuiEvent::ThinkingDone);
        }
        if let Some(ref mut s) = spinner {
            s.stop();
        }
    }

    // ── OpenAI backend ──

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

        let mut content = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut usage = Usage::default();
        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();
        let mut spinner = self.start_spinner();
        let mut renderer = StreamRenderer::new();
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
                                self.stop_spinner(&mut spinner);
                                first_token = false;
                            }
                            content.push_str(c);
                            self.output_text(c, &mut renderer);
                        }
                        if let Some(tcs) = &delta.tool_calls {
                            if first_token {
                                self.stop_spinner(&mut spinner);
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

        self.stop_spinner(&mut spinner);
        self.output_done(&mut renderer);

        // Emit token update
        self.emit(TuiEvent::TokenUpdate {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            duration_ms: start.elapsed().as_millis(),
        });

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

        Ok((
            msg,
            CompletionStats {
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                duration_ms: start.elapsed().as_millis(),
            },
        ))
    }

    // ── Anthropic backend ──

    async fn chat_anthropic(
        &self,
        messages: &[Message],
        tools: &[AvailableTool],
    ) -> Result<(Message, CompletionStats), Box<dyn std::error::Error>> {
        let start = Instant::now();

        let system_content = messages
            .iter()
            .find(|m| m.role == Role::System)
            .and_then(|m| m.content.as_deref())
            .unwrap_or("");

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
            .post(format!("{}/v1/messages", self.api_base))
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

        let mut content = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut usage = Usage::default();
        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();
        let mut current_tool_id = String::new();
        let mut current_tool_name = String::new();
        let mut current_tool_input = String::new();
        let mut spinner = self.start_spinner();
        let mut renderer = StreamRenderer::new();
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
                                        self.stop_spinner(&mut spinner);
                                        first_token = false;
                                    }
                                    content.push_str(text);
                                    self.output_text(text, &mut renderer);
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

        self.stop_spinner(&mut spinner);
        self.output_done(&mut renderer);

        self.emit(TuiEvent::TokenUpdate {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            duration_ms: start.elapsed().as_millis(),
        });

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

fn convert_to_anthropic_message(msg: &Message) -> serde_json::Value {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "user",
        Role::System => "user",
    };

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

    if msg.role == Role::Assistant {
        if let Some(ref tcs) = msg.tool_calls {
            let mut content_blocks: Vec<serde_json::Value> = Vec::new();
            if let Some(ref text) = msg.content {
                if !text.is_empty() {
                    content_blocks.push(json!({"type": "text", "text": text}));
                }
            }
            for tc in tcs {
                let input: serde_json::Value =
                    serde_json::from_str(&tc.function.arguments).unwrap_or(json!({}));
                content_blocks.push(json!({
                    "type": "tool_use",
                    "id": tc.id,
                    "name": tc.function.name,
                    "input": input,
                }));
            }
            return json!({"role": "assistant", "content": content_blocks});
        }
    }

    json!({
        "role": role,
        "content": msg.content.as_deref().unwrap_or(""),
    })
}
