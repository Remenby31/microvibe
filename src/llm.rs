use crate::types::*;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::json;
use std::io::Write;

pub struct LlmClient {
    client: Client,
    api_base: String,
    api_key: String,
    model: String,
}

impl LlmClient {
    pub fn new(api_base: &str, api_key: &str, model: &str) -> Self {
        Self {
            client: Client::new(),
            api_base: api_base.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
        }
    }

    pub async fn chat(
        &self,
        messages: &[Message],
        tools: &[AvailableTool],
    ) -> Result<Message, Box<dyn std::error::Error>> {
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "temperature": 0.1,
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
        let mut stream = resp.bytes_stream();

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result?;
            let text = String::from_utf8_lossy(&chunk);

            for line in text.lines() {
                let line = line.trim();
                if !line.starts_with("data: ") {
                    continue;
                }
                let data = &line[6..];
                if data == "[DONE]" {
                    break;
                }

                let parsed: StreamChunk = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

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

        Ok(Message {
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
        })
    }

    pub fn model_name(&self) -> &str {
        &self.model
    }
}
