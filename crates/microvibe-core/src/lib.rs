mod session_store;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_util::StreamExt;
use glob::Pattern;
use microvibe_config::{
    Config, HookConfig, ToolPermissionConfig, load_hooks_from_fs, project_instructions,
};
use microvibe_protocol::{
    AgentEvent, ContentBlock, HookMessageSeverity, HookScope, ImageAttachment, ImageSource,
    Message, Role, SessionId, ToolCall, ToolResult, Usage,
};
use microvibe_tools::{
    RuntimeToolConfigs, SubagentProfile, Tool, ToolRegistry, tool_call_requires_approval,
};
use regex::RegexBuilder;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
pub use session_store::{SavedSession, SessionStore};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

pub struct ApprovalRequest {
    pub id: String,
    pub call: ToolCall,
    pub respond_to: oneshot::Sender<microvibe_protocol::ApprovalDecision>,
}

pub struct QuestionRequest {
    pub id: String,
    pub call: ToolCall,
    pub respond_to: oneshot::Sender<QuestionResponse>,
}

#[derive(Debug, Clone)]
pub struct QuestionResponse {
    pub answers: Vec<QuestionAnswer>,
    pub cancelled: bool,
}

#[derive(Debug, Clone)]
pub struct QuestionAnswer {
    pub question: String,
    pub answer: String,
    pub is_other: bool,
}

impl QuestionResponse {
    pub fn single(answer: String, question: String, is_other: bool) -> Self {
        Self {
            answers: vec![QuestionAnswer {
                question,
                answer,
                is_other,
            }],
            cancelled: false,
        }
    }

    pub fn cancelled() -> Self {
        Self {
            answers: Vec::new(),
            cancelled: true,
        }
    }
}

pub struct Agent {
    config: Config,
    http: Client,
    tools: ToolRegistry,
    messages: Vec<Message>,
    last_usage: Usage,
    cumulative_usage: Usage,
    session_approvals: HashSet<String>,
    hooks: Vec<HookConfig>,
    hook_retry_counts: BTreeMap<String, u8>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RunLimits {
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u64>,
    pub max_price: Option<f64>,
}

impl Agent {
    pub fn new(mut config: Config) -> Self {
        apply_agent_profile_overrides(&mut config);
        let messages = vec![Message::text(
            Role::System,
            system_prompt_for_config(&config),
        )];
        let tool_configs =
            RuntimeToolConfigs::from_value(serde_json::to_value(&config.tools).unwrap_or_default());
        let subagents = subagent_profiles(&config);
        let mut tools = config
            .active_provider()
            .map(|provider| {
                ToolRegistry::with_provider_configs_and_subagents(
                    provider.base_url.clone(),
                    provider.api_key_env.clone(),
                    config.model.name.clone(),
                    tool_configs.clone(),
                    subagents.clone(),
                )
            })
            .unwrap_or_else(|_| {
                ToolRegistry::with_builtins_and_configs_and_subagents(tool_configs, subagents)
            });
        tools.apply_filters(&config.enabled_tools, &config.disabled_tools);
        let hooks = load_hooks_from_fs(&config).hooks;
        Self {
            config,
            http: Client::new(),
            tools,
            messages,
            last_usage: Usage::default(),
            cumulative_usage: Usage::default(),
            session_approvals: HashSet::new(),
            hooks,
            hook_retry_counts: BTreeMap::new(),
        }
    }

    pub fn with_messages(config: Config, messages: Vec<Message>) -> Self {
        let mut agent = Self::new(config);
        agent.messages = messages;
        agent
    }

    pub fn model(&self) -> &str {
        &self.config.model.name
    }

    pub fn provider(&self) -> &str {
        &self.config.model.provider
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn messages_owned(&self) -> Vec<Message> {
        self.messages.clone()
    }

    pub fn tool_specs(&self) -> Vec<microvibe_protocol::ToolSpec> {
        self.tools.specs()
    }

    pub fn disable_tools<I, S>(&mut self, names: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.tools.disable_tools(names);
    }

    pub fn replace_tool<T: Tool + 'static>(&mut self, tool: T) {
        self.tools.replace(tool);
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn inject_user_context(&mut self, content: impl Into<String>) {
        self.messages.push(Message::text(Role::User, content));
    }

    pub fn rewindable_messages(&self) -> Vec<(usize, String)> {
        self.messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.role == Role::User)
            .map(|(index, message)| (index, text_content(message)))
            .filter(|(_, content)| !content.is_empty())
            .collect()
    }

    fn truncate_before_message(&mut self, index: usize) {
        self.messages.truncate(index);
    }

    async fn compact(&mut self, extra_instructions: &str) -> Result<String> {
        let prior_user_messages = self
            .messages
            .iter()
            .filter(|message| message.role == Role::User)
            .map(text_content)
            .filter(|content| !content.is_empty())
            .collect::<Vec<_>>();
        let mut summary_request = compaction_prompt();
        let extra_instructions = extra_instructions.trim();
        if !extra_instructions.is_empty() {
            summary_request.push_str("\n\n## Additional Instructions\n");
            summary_request.push_str(extra_instructions);
        }

        self.messages
            .push(Message::text(Role::User, summary_request));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let outcome = self.chat_once(tx, false).await;
        while rx.try_recv().is_ok() {}
        self.messages.pop();

        let outcome = outcome?;
        self.last_usage = outcome.usage;
        let summary = text_content(&outcome.message).trim().to_string();
        let summary = if summary.is_empty() {
            "(no summary available)".to_string()
        } else {
            summary
        };
        let system_message = self
            .messages
            .first()
            .cloned()
            .unwrap_or_else(|| Message::text(Role::System, system_prompt()));
        self.messages = vec![
            system_message,
            Message::text(
                Role::User,
                render_compaction_context(&prior_user_messages, &summary),
            ),
        ];
        Ok(summary)
    }

    pub fn last_usage(&self) -> &Usage {
        &self.last_usage
    }

    pub fn cumulative_usage(&self) -> &Usage {
        &self.cumulative_usage
    }

    pub async fn run_turn(
        &mut self,
        input: impl Into<String>,
        tx: mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<()> {
        self.run_turn_with_limits(input, tx, RunLimits::default())
            .await
    }

    pub async fn run_turn_with_display_content(
        &mut self,
        input: impl Into<String>,
        display_content: Option<Value>,
        tx: mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<()> {
        self.run_turn_with_options(
            input,
            tx,
            RunLimits::default(),
            true,
            None,
            None,
            None,
            display_content,
            None,
        )
        .await
    }

    pub async fn run_turn_with_display_content_and_limits(
        &mut self,
        input: impl Into<String>,
        display_content: Option<Value>,
        tx: mpsc::UnboundedSender<AgentEvent>,
        limits: RunLimits,
    ) -> Result<()> {
        self.run_turn_with_display_content_images_and_limits(
            input,
            display_content,
            Vec::new(),
            tx,
            limits,
        )
        .await
    }

    pub async fn run_turn_with_display_content_images_and_limits(
        &mut self,
        input: impl Into<String>,
        display_content: Option<Value>,
        images: Vec<ImageAttachment>,
        tx: mpsc::UnboundedSender<AgentEvent>,
        limits: RunLimits,
    ) -> Result<()> {
        self.run_turn_with_options(
            input,
            tx,
            limits,
            true,
            None,
            None,
            Some(images),
            display_content,
            None,
        )
        .await
    }

    pub async fn run_turn_with_approval(
        &mut self,
        input: impl Into<String>,
        tx: mpsc::UnboundedSender<AgentEvent>,
        approval_tx: mpsc::UnboundedSender<ApprovalRequest>,
    ) -> Result<()> {
        self.run_turn_with_options(
            input,
            tx,
            RunLimits::default(),
            true,
            Some(approval_tx),
            None,
            None,
            None,
            None,
        )
        .await
    }

    pub async fn run_turn_with_approval_and_user_message_id(
        &mut self,
        input: impl Into<String>,
        tx: mpsc::UnboundedSender<AgentEvent>,
        approval_tx: mpsc::UnboundedSender<ApprovalRequest>,
        user_message_id: String,
    ) -> Result<()> {
        self.run_turn_with_approval_display_and_user_message_id(
            input,
            tx,
            approval_tx,
            None,
            user_message_id,
        )
        .await
    }

    pub async fn run_turn_with_approval_display_and_user_message_id(
        &mut self,
        input: impl Into<String>,
        tx: mpsc::UnboundedSender<AgentEvent>,
        approval_tx: mpsc::UnboundedSender<ApprovalRequest>,
        display_content: Option<Value>,
        user_message_id: String,
    ) -> Result<()> {
        self.run_turn_with_approval_display_user_message_id_and_limits(
            input,
            tx,
            approval_tx,
            display_content,
            user_message_id,
            RunLimits::default(),
        )
        .await
    }

    pub async fn run_turn_with_approval_display_user_message_id_and_limits(
        &mut self,
        input: impl Into<String>,
        tx: mpsc::UnboundedSender<AgentEvent>,
        approval_tx: mpsc::UnboundedSender<ApprovalRequest>,
        display_content: Option<Value>,
        user_message_id: String,
        limits: RunLimits,
    ) -> Result<()> {
        self.run_turn_with_approval_display_images_user_message_id_and_limits(
            input,
            tx,
            approval_tx,
            display_content,
            Vec::new(),
            user_message_id,
            limits,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn run_turn_with_approval_display_images_user_message_id_and_limits(
        &mut self,
        input: impl Into<String>,
        tx: mpsc::UnboundedSender<AgentEvent>,
        approval_tx: mpsc::UnboundedSender<ApprovalRequest>,
        display_content: Option<Value>,
        images: Vec<ImageAttachment>,
        user_message_id: String,
        limits: RunLimits,
    ) -> Result<()> {
        self.run_turn_with_options(
            input,
            tx,
            limits,
            true,
            Some(approval_tx),
            None,
            Some(images),
            display_content,
            Some(user_message_id),
        )
        .await
    }

    pub async fn run_turn_with_interaction(
        &mut self,
        input: impl Into<String>,
        tx: mpsc::UnboundedSender<AgentEvent>,
        approval_tx: mpsc::UnboundedSender<ApprovalRequest>,
        question_tx: mpsc::UnboundedSender<QuestionRequest>,
    ) -> Result<()> {
        self.run_turn_with_options(
            input,
            tx,
            RunLimits::default(),
            true,
            Some(approval_tx),
            Some(question_tx),
            None,
            None,
            None,
        )
        .await
    }

    pub async fn run_turn_with_interaction_and_images(
        &mut self,
        input: impl Into<String>,
        display_content: Option<String>,
        images: Vec<ImageAttachment>,
        tx: mpsc::UnboundedSender<AgentEvent>,
        approval_tx: mpsc::UnboundedSender<ApprovalRequest>,
        question_tx: mpsc::UnboundedSender<QuestionRequest>,
    ) -> Result<()> {
        self.run_turn_with_options(
            input,
            tx,
            RunLimits::default(),
            true,
            Some(approval_tx),
            Some(question_tx),
            Some(images),
            display_content.map(Value::String),
            None,
        )
        .await
    }

    pub async fn run_turn_with_limits(
        &mut self,
        input: impl Into<String>,
        tx: mpsc::UnboundedSender<AgentEvent>,
        limits: RunLimits,
    ) -> Result<()> {
        self.run_turn_with_options(input, tx, limits, true, None, None, None, None, None)
            .await
    }

    pub async fn run_turn_programmatic(
        &mut self,
        input: impl Into<String>,
        tx: mpsc::UnboundedSender<AgentEvent>,
        limits: RunLimits,
    ) -> Result<()> {
        self.run_turn_with_options(input, tx, limits, false, None, None, None, None, None)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_turn_with_options(
        &mut self,
        input: impl Into<String>,
        tx: mpsc::UnboundedSender<AgentEvent>,
        limits: RunLimits,
        stream: bool,
        approval_tx: Option<mpsc::UnboundedSender<ApprovalRequest>>,
        question_tx: Option<mpsc::UnboundedSender<QuestionRequest>>,
        images: Option<Vec<ImageAttachment>>,
        display_content: Option<Value>,
        user_message_id: Option<String>,
    ) -> Result<()> {
        let input = input.into();
        let turn_id = uuid::Uuid::new_v4().to_string();
        let _ = tx.send(AgentEvent::SessionConfigured {
            model: self.model().to_string(),
            provider: self.provider().to_string(),
        });
        let _ = tx.send(AgentEvent::TurnStarted { id: turn_id });

        self.messages.push(Message::text_with_images_display_and_id(
            Role::User,
            input,
            images.unwrap_or_default(),
            display_content,
            user_message_id,
        ));
        match self
            .run_model_tool_loop(tx.clone(), limits, stream, approval_tx, question_tx)
            .await
        {
            Ok(usage) => {
                self.cumulative_usage = add_usage(&self.cumulative_usage, &usage);
                self.last_usage = self.cumulative_usage.clone();
                let _ = tx.send(AgentEvent::TurnCompleted {
                    usage: self.cumulative_usage.clone(),
                });
                Ok(())
            }
            Err(error) => {
                let _ = tx.send(AgentEvent::Error {
                    message: error.to_string(),
                });
                Err(error)
            }
        }
    }

    async fn run_model_tool_loop(
        &mut self,
        tx: mpsc::UnboundedSender<AgentEvent>,
        limits: RunLimits,
        stream: bool,
        approval_tx: Option<mpsc::UnboundedSender<ApprovalRequest>>,
        question_tx: Option<mpsc::UnboundedSender<QuestionRequest>>,
    ) -> Result<Usage> {
        self.tools
            .integrate_mcp_servers(&self.config.mcp_servers)
            .await;
        self.tools
            .apply_filters(&self.config.enabled_tools, &self.config.disabled_tools);
        let mut total_usage = Usage::default();
        let base_usage = self.cumulative_usage.clone();
        let max_model_turns = limits.max_turns.unwrap_or(8).min(8);
        for model_turn in 0..8 {
            if model_turn >= max_model_turns {
                return Ok(stop_by_limit(
                    &mut self.messages,
                    &tx,
                    total_usage,
                    format!("Turn limit of {max_model_turns} reached"),
                ));
            }
            if let Some(max_tokens) = limits.max_tokens {
                let total_tokens = total_usage.input_tokens + total_usage.output_tokens;
                if total_tokens > max_tokens {
                    return Ok(stop_by_limit(
                        &mut self.messages,
                        &tx,
                        total_usage,
                        format!(
                            "Token limit exceeded: {} > {}",
                            comma_u64(total_tokens),
                            comma_u64(max_tokens)
                        ),
                    ));
                }
            }
            if let Some(max_price) = limits.max_price {
                let cost = session_cost(
                    total_usage.input_tokens,
                    total_usage.output_tokens,
                    self.config.model.input_price,
                    self.config.model.output_price,
                );
                if cost > max_price {
                    return Ok(stop_by_limit(
                        &mut self.messages,
                        &tx,
                        total_usage,
                        format!("Price limit exceeded: ${cost:.4} > ${max_price:.2}"),
                    ));
                }
            }
            let outcome = self.chat_once(tx.clone(), stream).await?;
            total_usage.input_tokens += outcome.usage.input_tokens;
            total_usage.output_tokens += outcome.usage.output_tokens;
            let _ = tx.send(AgentEvent::UsageUpdated {
                usage: add_usage(&base_usage, &total_usage),
            });
            let tool_calls = outcome.tool_calls.clone();
            self.messages.push(outcome.message);
            if tool_calls.is_empty() {
                if let Some(retry_message) = self.run_post_agent_turn_hooks(&tx).await? {
                    self.messages
                        .push(Message::injected_text(Role::User, retry_message));
                    continue;
                }
                self.hook_retry_counts.clear();
                return Ok(total_usage);
            }
            for mut call in tool_calls {
                let before_resolution = self.run_before_tool_hooks(&tx, &call).await?;
                match before_resolution {
                    BeforeToolResolution::Allow => {}
                    BeforeToolResolution::Rewrite(arguments) => {
                        call.arguments = arguments;
                        update_assistant_tool_call_arguments(
                            &mut self.messages,
                            &call.id,
                            call.arguments.clone(),
                        );
                    }
                    BeforeToolResolution::Deny(output) => {
                        let result = ToolResult {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            output,
                            success: false,
                        };
                        let _ = tx.send(AgentEvent::ToolCallCompleted {
                            result: result.clone(),
                        });
                        self.messages.push(tool_result_message(result));
                        continue;
                    }
                }
                let approval_key = approval_key(&call);
                let permission = resolve_tool_permission(&self.config, &call);
                let approval_needed = permission == ResolvedToolPermission::Ask
                    && !self.session_approvals.contains(&approval_key)
                    && !is_persistently_allowed(&self.config, &call);
                if approval_needed || !tool_call_requires_approval(&call) {
                    let _ = tx.send(AgentEvent::ToolCallStarted { call: call.clone() });
                }
                let result = if call.name == "exit_plan_mode" && self.tools.is_enabled(&call.name) {
                    request_exit_plan_mode(&self.config, &call, question_tx.as_ref()).await
                } else if call.name == "ask_user_question" && question_tx.is_some() {
                    request_question(&call, question_tx.as_ref()).await
                } else if permission == ResolvedToolPermission::Never {
                    ToolResult {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        output: format!("Tool '{}' is permanently disabled", call.name),
                        success: false,
                    }
                } else if approval_needed {
                    match request_approval(&call, approval_tx.as_ref()).await {
                        microvibe_protocol::ApprovalDecision::AllowOnce => {
                            self.tools.run(&call).await
                        }
                        microvibe_protocol::ApprovalDecision::AllowSession => {
                            self.session_approvals.insert(approval_key);
                            self.tools.run(&call).await
                        }
                        microvibe_protocol::ApprovalDecision::AllowAlways => {
                            self.session_approvals.insert(approval_key);
                            if let Some(patterns) = approval_allowlist_patterns(&call) {
                                Config::add_tool_allowlist_patterns(&call.name, &patterns).ok();
                            } else {
                                Config::set_tool_permission(&call.name, "always").ok();
                            }
                            self.tools.run(&call).await
                        }
                        microvibe_protocol::ApprovalDecision::Deny => ToolResult {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            output: "Skipped: User cancelled the operation.".to_string(),
                            success: false,
                        },
                        microvibe_protocol::ApprovalDecision::Cancelled => ToolResult {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            output: "User cancelled the operation.".to_string(),
                            success: false,
                        },
                        microvibe_protocol::ApprovalDecision::DenyWithFeedback => ToolResult {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            output: "User rejected the tool call, provide an alternative plan"
                                .to_string(),
                            success: false,
                        },
                    }
                } else {
                    self.tools.run(&call).await
                };
                let result = self.run_after_tool_hooks(&tx, &call, result).await?;
                let _ = tx.send(AgentEvent::ToolCallCompleted {
                    result: result.clone(),
                });
                let stopped_by_user = result.name == call.name
                    && !result.success
                    && (result.output == "Skipped: User cancelled the operation."
                        || result.output == "User cancelled the operation.");
                self.messages.push(tool_result_message(result));
                if stopped_by_user {
                    return Ok(total_usage);
                }
            }
        }
        anyhow::bail!("maximum tool-call iterations reached")
    }

    async fn run_before_tool_hooks(
        &mut self,
        tx: &mpsc::UnboundedSender<AgentEvent>,
        call: &ToolCall,
    ) -> Result<BeforeToolResolution> {
        let hooks = matching_hooks(&self.hooks, HookScope::BeforeTool, &call.name);
        if hooks.is_empty() {
            return Ok(BeforeToolResolution::Allow);
        }
        let _ = tx.send(AgentEvent::HookRunStarted {
            scope: HookScope::BeforeTool,
            tool_name: Some(call.name.clone()),
            tool_call_id: Some(call.id.clone()),
        });
        let mut arguments = call.arguments.clone();
        let mut resolution = BeforeToolResolution::Allow;
        for hook in hooks {
            let _ = tx.send(AgentEvent::HookStarted {
                hook_name: hook.name.clone(),
                scope: HookScope::BeforeTool,
                tool_call_id: Some(call.id.clone()),
            });
            let invocation = hook_invocation(
                HookScope::BeforeTool,
                Some(call),
                Some(&arguments),
                None,
                None,
                None,
                0.0,
            );
            let hook_result = run_hook_command(&hook, &invocation).await;
            let action = handle_before_tool_hook(&hook, hook_result, &call.name);
            let _ = tx.send(AgentEvent::HookEnded {
                hook_name: hook.name.clone(),
                status: action.status,
                content: action.content.clone(),
                scope: HookScope::BeforeTool,
                tool_call_id: Some(call.id.clone()),
            });
            match action.decision {
                HookDecision::Allow => {}
                HookDecision::Rewrite(new_arguments) => {
                    let new_arguments = normalize_hook_tool_input(&call.name, new_arguments);
                    arguments = new_arguments.clone();
                    resolution = BeforeToolResolution::Rewrite(new_arguments);
                }
                HookDecision::Deny(output) => {
                    resolution = BeforeToolResolution::Deny(output);
                    break;
                }
                HookDecision::Retry(_) | HookDecision::ReplaceText(_) => {}
            }
        }
        let _ = tx.send(AgentEvent::HookRunCompleted {
            scope: HookScope::BeforeTool,
            tool_call_id: Some(call.id.clone()),
        });
        Ok(resolution)
    }

    async fn run_after_tool_hooks(
        &mut self,
        tx: &mpsc::UnboundedSender<AgentEvent>,
        call: &ToolCall,
        mut result: ToolResult,
    ) -> Result<ToolResult> {
        let hooks = matching_hooks(&self.hooks, HookScope::AfterTool, &call.name);
        if hooks.is_empty() {
            return Ok(result);
        }
        let _ = tx.send(AgentEvent::HookRunStarted {
            scope: HookScope::AfterTool,
            tool_name: Some(call.name.clone()),
            tool_call_id: Some(call.id.clone()),
        });
        let started = Instant::now();
        for hook in hooks {
            let _ = tx.send(AgentEvent::HookStarted {
                hook_name: hook.name.clone(),
                scope: HookScope::AfterTool,
                tool_call_id: Some(call.id.clone()),
            });
            let invocation = hook_invocation(
                HookScope::AfterTool,
                Some(call),
                Some(&call.arguments),
                Some(if result.success { "success" } else { "failure" }),
                Some(&result.output),
                (!result.success).then_some(result.output.as_str()),
                started.elapsed().as_secs_f64() * 1000.0,
            );
            let hook_result = run_hook_command(&hook, &invocation).await;
            let action = handle_after_tool_hook(&hook, hook_result, &result.output);
            let _ = tx.send(AgentEvent::HookEnded {
                hook_name: hook.name.clone(),
                status: action.status,
                content: action.content.clone(),
                scope: HookScope::AfterTool,
                tool_call_id: Some(call.id.clone()),
            });
            if let HookDecision::ReplaceText(text) = action.decision {
                result.output = text;
            }
        }
        let _ = tx.send(AgentEvent::HookRunCompleted {
            scope: HookScope::AfterTool,
            tool_call_id: Some(call.id.clone()),
        });
        Ok(result)
    }

    async fn run_post_agent_turn_hooks(
        &mut self,
        tx: &mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<Option<String>> {
        let hooks = matching_hooks(&self.hooks, HookScope::PostAgentTurn, "");
        if hooks.is_empty() {
            return Ok(None);
        }
        let _ = tx.send(AgentEvent::HookRunStarted {
            scope: HookScope::PostAgentTurn,
            tool_name: None,
            tool_call_id: None,
        });
        let mut retry = None;
        for hook in hooks {
            let _ = tx.send(AgentEvent::HookStarted {
                hook_name: hook.name.clone(),
                scope: HookScope::PostAgentTurn,
                tool_call_id: None,
            });
            let invocation =
                hook_invocation(HookScope::PostAgentTurn, None, None, None, None, None, 0.0);
            let hook_result = run_hook_command(&hook, &invocation).await;
            let action =
                handle_post_agent_turn_hook(&hook, hook_result, &mut self.hook_retry_counts);
            let _ = tx.send(AgentEvent::HookEnded {
                hook_name: hook.name.clone(),
                status: action.status,
                content: action.content.clone(),
                scope: HookScope::PostAgentTurn,
                tool_call_id: None,
            });
            if let HookDecision::Retry(message) = action.decision {
                retry = Some(message);
                break;
            }
        }
        let _ = tx.send(AgentEvent::HookRunCompleted {
            scope: HookScope::PostAgentTurn,
            tool_call_id: None,
        });
        Ok(retry)
    }

    async fn chat_once(
        &self,
        tx: mpsc::UnboundedSender<AgentEvent>,
        stream: bool,
    ) -> Result<ChatOutcome> {
        let provider = self.config.active_provider()?;
        let api_key = std::env::var(&provider.api_key_env)
            .with_context(|| format!("missing environment variable {}", provider.api_key_env))?;
        let mut request = json!({
            "model": self.config.model.name,
            "messages": to_openai_messages(&self.messages),
            "tools": to_openai_tools(self.tool_specs()),
            "tool_choice": "auto",
            "temperature": self.config.model.temperature,
            "stream": stream
        });
        if stream {
            request["stream_options"] = json!({
                "include_usage": true,
                "stream_tool_calls": true,
            });
        }

        let response = self
            .http
            .post(format!(
                "{}/chat/completions",
                provider.base_url.trim_end_matches('/')
            ))
            .bearer_auth(api_key)
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("provider returned {status}: {body}");
        }

        if !stream {
            return self.parse_non_streaming_response(response.json().await?, tx);
        }

        let mut text = String::new();
        let mut reasoning = String::new();
        let mut reasoning_message_id = None;
        let mut tool_accumulator = ToolCallAccumulator::default();
        let mut usage = Usage::default();
        let mut buffer = String::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            buffer.push_str(&String::from_utf8_lossy(&chunk?));
            while let Some(newline) = buffer.find('\n') {
                let line = buffer[..newline].trim().to_string();
                buffer = buffer[newline + 1..].to_string();
                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };
                if data == "[DONE]" {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                if let Some(u) = value.get("usage") {
                    usage.input_tokens = u
                        .get("prompt_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(usage.input_tokens);
                    usage.output_tokens = u
                        .get("completion_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(usage.output_tokens);
                }
                let delta = value
                    .pointer("/choices/0/delta/content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !delta.is_empty() {
                    text.push_str(delta);
                    let _ = tx.send(AgentEvent::AssistantDelta {
                        text: delta.to_string(),
                    });
                }
                let reasoning_delta = value
                    .pointer("/choices/0/delta/reasoning_content")
                    .or_else(|| value.pointer("/choices/0/delta/reasoning"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !reasoning_delta.is_empty() {
                    let message_id = reasoning_message_id
                        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
                        .clone();
                    reasoning.push_str(reasoning_delta);
                    let _ = tx.send(AgentEvent::ThoughtDelta {
                        text: reasoning_delta.to_string(),
                        message_id,
                    });
                }
                if let Some(tool_calls) = value.pointer("/choices/0/delta/tool_calls") {
                    tool_accumulator.push(tool_calls);
                }
            }
        }
        let tool_calls = tool_accumulator.finish()?;
        let message = assistant_message_with_reasoning(
            text,
            tool_calls.clone(),
            non_empty_string(reasoning),
            reasoning_message_id,
        );

        Ok(ChatOutcome {
            message,
            tool_calls,
            usage,
        })
    }

    fn parse_non_streaming_response(
        &self,
        value: Value,
        tx: mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<ChatOutcome> {
        let mut usage = Usage::default();
        if let Some(u) = value.get("usage") {
            usage.input_tokens = u
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            usage.output_tokens = u
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_default();
        }
        let message = value
            .pointer("/choices/0/message")
            .cloned()
            .or_else(|| value.get("message").cloned())
            .unwrap_or_else(|| json!({ "role": "assistant", "content": "" }));
        let text = message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let reasoning = message
            .get("reasoning_content")
            .or_else(|| message.get("reasoning"))
            .and_then(Value::as_str)
            .filter(|reasoning| !reasoning.is_empty())
            .map(ToString::to_string);
        let reasoning_message_id = reasoning.as_ref().map(|_| uuid::Uuid::new_v4().to_string());
        if let (Some(reasoning), Some(message_id)) = (&reasoning, &reasoning_message_id) {
            let _ = tx.send(AgentEvent::ThoughtDelta {
                text: reasoning.clone(),
                message_id: message_id.clone(),
            });
        }
        if !text.is_empty() {
            let _ = tx.send(AgentEvent::AssistantDelta { text: text.clone() });
        }
        let tool_calls = parse_full_tool_calls(message.get("tool_calls"))?;
        Ok(ChatOutcome {
            message: assistant_message_with_reasoning(
                text,
                tool_calls.clone(),
                reasoning,
                reasoning_message_id,
            ),
            tool_calls,
            usage,
        })
    }
}

#[derive(Debug)]
enum BeforeToolResolution {
    Allow,
    Rewrite(Value),
    Deny(String),
}

#[derive(Debug)]
enum HookDecision {
    Allow,
    Rewrite(Value),
    Deny(String),
    ReplaceText(String),
    Retry(String),
}

#[derive(Debug)]
struct HookAction {
    status: HookMessageSeverity,
    content: Option<String>,
    decision: HookDecision,
}

#[derive(Debug)]
struct HookExecutionResult {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

#[derive(Debug, Deserialize)]
struct HookStructuredResponse {
    #[serde(default = "hook_decision_allow")]
    decision: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    system_message: Option<String>,
    #[serde(default)]
    hook_specific_output: HookSpecificOutput,
}

#[derive(Debug, Default, Deserialize)]
struct HookSpecificOutput {
    #[serde(default)]
    tool_input: Option<Value>,
    #[serde(default)]
    additional_context: Option<String>,
}

fn hook_decision_allow() -> String {
    "allow".to_string()
}

fn matching_hooks(hooks: &[HookConfig], scope: HookScope, tool_name: &str) -> Vec<HookConfig> {
    let expected = hook_scope_name(scope);
    hooks
        .iter()
        .filter(|hook| hook.hook_type == expected)
        .filter(|hook| {
            scope == HookScope::PostAgentTurn
                || hook
                    .r#match
                    .as_deref()
                    .map(|pattern| name_matches(tool_name, &[pattern.to_string()]))
                    .unwrap_or(true)
        })
        .cloned()
        .collect()
}

fn hook_scope_name(scope: HookScope) -> &'static str {
    match scope {
        HookScope::PostAgentTurn => "post_agent_turn",
        HookScope::BeforeTool => "before_tool",
        HookScope::AfterTool => "after_tool",
    }
}

fn normalize_hook_tool_input(tool_name: &str, mut arguments: Value) -> Value {
    if tool_name == "read"
        && let Some(object) = arguments.as_object_mut()
    {
        object.entry("offset").or_insert(Value::Null);
        object.entry("limit").or_insert(json!(2000));
    }
    arguments
}

fn update_assistant_tool_call_arguments(messages: &mut [Message], call_id: &str, arguments: Value) {
    let Some(message) = messages
        .iter_mut()
        .rev()
        .find(|message| message.role == Role::Assistant)
    else {
        return;
    };
    for block in &mut message.content {
        if let ContentBlock::ToolCall(call) = block
            && call.id == call_id
        {
            call.arguments = arguments;
            return;
        }
    }
}

fn hook_invocation(
    scope: HookScope,
    call: Option<&ToolCall>,
    tool_input: Option<&Value>,
    tool_status: Option<&str>,
    tool_output_text: Option<&str>,
    tool_error: Option<&str>,
    duration_ms: f64,
) -> Value {
    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let mut invocation = json!({
        "hook_event_name": hook_scope_name(scope),
        "session_id": "microvibe-session",
        "parent_session_id": null,
        "transcript_path": "",
        "cwd": cwd,
    });
    if let Some(call) = call {
        invocation["tool_name"] = json!(call.name);
        invocation["tool_call_id"] = json!(call.id);
        invocation["tool_input"] = tool_input.cloned().unwrap_or_else(|| json!({}));
    }
    if scope == HookScope::AfterTool {
        invocation["tool_status"] = json!(tool_status.unwrap_or("success"));
        invocation["tool_output"] = Value::Null;
        invocation["tool_output_text"] = json!(tool_output_text.unwrap_or(""));
        invocation["tool_error"] = tool_error.map_or(Value::Null, |value| json!(value));
        invocation["duration_ms"] = json!(duration_ms);
    }
    invocation
}

async fn run_hook_command(hook: &HookConfig, invocation: &Value) -> HookExecutionResult {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(&hook.command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return HookExecutionResult {
                exit_code: Some(1),
                stdout: String::new(),
                stderr: format!("Failed to start: {error}"),
                timed_out: false,
            };
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(invocation.to_string().as_bytes()).await;
    }
    let timeout = std::time::Duration::from_millis(
        (hook.timeout * 1000.0).round().clamp(250.0, 300_000.0) as u64,
    );
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => HookExecutionResult {
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            timed_out: false,
        },
        Ok(Err(error)) => HookExecutionResult {
            exit_code: Some(1),
            stdout: String::new(),
            stderr: error.to_string(),
            timed_out: false,
        },
        Err(_) => HookExecutionResult {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
        },
    }
}

fn parse_hook_response(
    result: &HookExecutionResult,
) -> std::result::Result<Option<HookStructuredResponse>, String> {
    if result.timed_out || result.exit_code != Some(0) {
        return Err(hook_failure_reason(result));
    }
    if result.stdout.is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(&result.stdout)
        .map_err(|error| format!("stdout was not valid JSON: {error}"))?;
    if !value.is_object() {
        return Err(format!(
            "stdout was a JSON {}, expected an object",
            json_type_name(&value)
        ));
    }
    let response: HookStructuredResponse = serde_json::from_value(value)
        .map_err(|error| format!("stdout JSON did not match the hook response schema: {error}"))?;
    Ok(Some(response))
}

fn hook_failure_reason(result: &HookExecutionResult) -> String {
    if result.timed_out || result.exit_code.is_none() {
        "timed out".to_string()
    } else if !result.stderr.is_empty() {
        result.stderr.clone()
    } else if !result.stdout.is_empty() {
        result.stdout.clone()
    } else {
        format!("exited with code {}", result.exit_code.unwrap_or(1))
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

fn handle_before_tool_hook(
    hook: &HookConfig,
    result: HookExecutionResult,
    tool_name: &str,
) -> HookAction {
    match parse_hook_response(&result) {
        Ok(Some(response)) if response.decision == "deny" => HookAction {
            status: HookMessageSeverity::Error,
            content: Some(format!("Denied tool '{tool_name}'")),
            decision: HookDecision::Deny(response.reason.unwrap_or_default()),
        },
        Ok(Some(response)) => {
            if let Some(tool_input) = response.hook_specific_output.tool_input {
                HookAction {
                    status: HookMessageSeverity::Warning,
                    content: Some(
                        response
                            .system_message
                            .unwrap_or_else(|| format!("Rewrote tool_input for '{tool_name}'")),
                    ),
                    decision: HookDecision::Rewrite(tool_input),
                }
            } else {
                HookAction {
                    status: HookMessageSeverity::Ok,
                    content: response.system_message,
                    decision: HookDecision::Allow,
                }
            }
        }
        Ok(None) => HookAction {
            status: HookMessageSeverity::Ok,
            content: None,
            decision: HookDecision::Allow,
        },
        Err(reason) if hook.strict => HookAction {
            status: HookMessageSeverity::Error,
            content: Some(format!("Denied tool '{tool_name}' (strict)")),
            decision: HookDecision::Deny(reason),
        },
        Err(reason) => HookAction {
            status: HookMessageSeverity::Warning,
            content: Some(reason),
            decision: HookDecision::Allow,
        },
    }
}

fn handle_after_tool_hook(
    hook: &HookConfig,
    result: HookExecutionResult,
    current_output: &str,
) -> HookAction {
    match parse_hook_response(&result) {
        Ok(Some(response)) if response.decision == "deny" => {
            let reason = response.reason.unwrap_or_default();
            let final_text = append_text(
                &reason,
                response.hook_specific_output.additional_context.as_deref(),
            );
            HookAction {
                status: HookMessageSeverity::Warning,
                content: Some(response.system_message.unwrap_or_else(|| {
                    format!("Replaced tool result ({} chars)", final_text.len())
                })),
                decision: HookDecision::ReplaceText(final_text),
            }
        }
        Ok(Some(response)) => {
            if let Some(additional) = response.hook_specific_output.additional_context {
                let new_text = append_text(current_output, Some(&additional));
                HookAction {
                    status: HookMessageSeverity::Warning,
                    content: Some(response.system_message.unwrap_or_else(|| {
                        format!("Appended {} chars to tool result", additional.len())
                    })),
                    decision: HookDecision::ReplaceText(new_text),
                }
            } else {
                HookAction {
                    status: HookMessageSeverity::Ok,
                    content: response.system_message,
                    decision: HookDecision::Allow,
                }
            }
        }
        Ok(None) => HookAction {
            status: HookMessageSeverity::Ok,
            content: None,
            decision: HookDecision::Allow,
        },
        Err(_) if hook.strict => HookAction {
            status: HookMessageSeverity::Error,
            content: Some("Cleared tool result (strict)".to_string()),
            decision: HookDecision::ReplaceText(String::new()),
        },
        Err(reason) => HookAction {
            status: HookMessageSeverity::Warning,
            content: Some(reason),
            decision: HookDecision::Allow,
        },
    }
}

fn handle_post_agent_turn_hook(
    hook: &HookConfig,
    result: HookExecutionResult,
    retry_counts: &mut BTreeMap<String, u8>,
) -> HookAction {
    match parse_hook_response(&result) {
        Ok(Some(response)) if response.decision == "deny" => {
            let count = *retry_counts.get(&hook.name).unwrap_or(&0);
            if count >= 3 {
                HookAction {
                    status: HookMessageSeverity::Error,
                    content: Some("Failed, retries exhausted (3/3)".to_string()),
                    decision: HookDecision::Allow,
                }
            } else {
                retry_counts.insert(hook.name.clone(), count + 1);
                let remaining = 3 - count;
                let noun = if remaining == 1 { "retry" } else { "retries" };
                HookAction {
                    status: HookMessageSeverity::Error,
                    content: Some(format!("Failed, retrying ({remaining} {noun} remaining)")),
                    decision: HookDecision::Retry(response.reason.unwrap_or_default()),
                }
            }
        }
        Ok(Some(response)) => {
            retry_counts.remove(&hook.name);
            HookAction {
                status: HookMessageSeverity::Ok,
                content: response.system_message,
                decision: HookDecision::Allow,
            }
        }
        Ok(None) => {
            retry_counts.remove(&hook.name);
            HookAction {
                status: HookMessageSeverity::Ok,
                content: None,
                decision: HookDecision::Allow,
            }
        }
        Err(reason) => HookAction {
            status: HookMessageSeverity::Warning,
            content: Some(reason),
            decision: HookDecision::Allow,
        },
    }
}

fn append_text(base: &str, addition: Option<&str>) -> String {
    let Some(addition) = addition else {
        return base.to_string();
    };
    if base.is_empty() {
        addition.to_string()
    } else {
        format!("{base}\n{addition}")
    }
}

fn session_cost(
    input_tokens: u64,
    output_tokens: u64,
    input_price_per_million: f64,
    output_price_per_million: f64,
) -> f64 {
    (input_tokens as f64 / 1_000_000.0) * input_price_per_million
        + (output_tokens as f64 / 1_000_000.0) * output_price_per_million
}

fn add_usage(left: &Usage, right: &Usage) -> Usage {
    Usage {
        input_tokens: left.input_tokens + right.input_tokens,
        output_tokens: left.output_tokens + right.output_tokens,
    }
}

fn stop_by_limit(
    messages: &mut Vec<Message>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    usage: Usage,
    reason: String,
) -> Usage {
    let content = format!("<vibe_stop_event>{reason}</vibe_stop_event>");
    let _ = tx.send(AgentEvent::AssistantDelta {
        text: content.clone(),
    });
    messages.push(Message::text(Role::Assistant, content));
    usage
}

fn comma_u64(value: u64) -> String {
    let raw = value.to_string();
    let mut out = String::new();
    for (idx, ch) in raw.chars().rev().enumerate() {
        if idx > 0 && idx % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

pub fn validate_agent_selection(config: &Config, explicit_agent: bool) -> Result<()> {
    let selected_agent = config.default_agent.clone();
    let Some(profile) = find_agent_profile(config, &selected_agent) else {
        anyhow::bail!("Agent '{selected_agent}' not found.");
    };
    if !is_agent_available(config, &profile) {
        anyhow::bail!(
            "{}",
            excluded_agent_message(config, &profile, explicit_agent)
        );
    }
    if profile.agent_type != "agent" {
        anyhow::bail!(
            "Agent '{}' is a {} and cannot be used as the primary agent. Only agents of type 'agent' can be selected with --agent.",
            profile.name,
            profile.agent_type
        );
    }
    Ok(())
}

fn apply_agent_profile_overrides(config: &mut Config) {
    let selected_agent = config.default_agent.clone();
    if let Some(profile) = load_custom_agent_profile(config, &selected_agent)
        && is_agent_available(config, &profile)
        && profile.agent_type == "agent"
    {
        apply_agent_overrides(config, &profile.overrides);
        return;
    }

    apply_builtin_agent_overrides(config);
}

#[derive(Debug, Clone)]
struct AgentProfile {
    name: String,
    display_name: String,
    agent_type: String,
    install_required: bool,
    overrides: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSummary {
    pub name: String,
    pub display_name: String,
}

fn find_agent_profile(config: &Config, name: &str) -> Option<AgentProfile> {
    load_custom_agent_profile(config, name).or_else(|| load_builtin_agent_profile(name))
}

fn load_builtin_agent_profile(name: &str) -> Option<AgentProfile> {
    let (display_name, agent_type, install_required) = match name {
        "default" => ("Default", "agent", false),
        "plan" => ("Plan", "agent", false),
        "accept-edits" => ("Accept Edits", "agent", false),
        "auto-approve" => ("Auto Approve", "agent", false),
        "explore" => ("Explore", "subagent", false),
        "lean" => ("Lean", "agent", true),
        _ => return None,
    };
    Some(AgentProfile {
        name: name.to_string(),
        display_name: display_name.to_string(),
        agent_type: agent_type.to_string(),
        install_required,
        overrides: BTreeMap::new(),
    })
}

#[derive(Debug, Deserialize)]
struct RawAgentProfile {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    agent_type: Option<String>,
    #[serde(default)]
    install_required: Option<bool>,
    #[serde(default, rename = "description")]
    _description: Option<String>,
    #[serde(default, rename = "safety")]
    _safety: Option<String>,
    #[serde(flatten)]
    overrides: BTreeMap<String, toml::Value>,
}

fn load_custom_agent_profile(config: &Config, name: &str) -> Option<AgentProfile> {
    for base in agent_search_paths(config) {
        let path = base.join(format!("{name}.toml"));
        if !path.is_file() {
            continue;
        }
        return load_custom_agent_file(&path, name);
    }
    None
}

fn load_custom_agent_file(path: &Path, name: &str) -> Option<AgentProfile> {
    let raw = std::fs::read_to_string(path).ok()?;
    let mut profile: RawAgentProfile = toml::from_str(&raw).ok()?;
    profile.overrides.remove("display_name");
    profile.overrides.remove("description");
    profile.overrides.remove("safety");
    profile.overrides.remove("agent_type");
    profile.overrides.remove("install_required");
    Some(AgentProfile {
        name: name.to_string(),
        display_name: profile
            .display_name
            .unwrap_or_else(|| default_agent_display_name(name)),
        agent_type: profile.agent_type.unwrap_or_else(|| "agent".to_string()),
        install_required: profile.install_required.unwrap_or(false),
        overrides: profile.overrides,
    })
}

fn default_agent_display_name(name: &str) -> String {
    name.split('-')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn discover_custom_agent_profiles(config: &Config) -> Vec<AgentProfile> {
    let mut profiles = BTreeMap::new();
    for base in agent_search_paths(config) {
        let Ok(entries) = std::fs::read_dir(base) else {
            continue;
        };
        let mut paths = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("toml"))
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            let Some(name) = path.file_stem().and_then(|name| name.to_str()) else {
                continue;
            };
            if profiles.contains_key(name) {
                continue;
            }
            if let Some(profile) = load_custom_agent_file(&path, name) {
                profiles.insert(name.to_string(), profile);
            }
        }
    }
    profiles.into_values().collect()
}

fn subagent_profiles(config: &Config) -> Vec<SubagentProfile> {
    let mut profiles = BTreeMap::from([(
        "explore".to_string(),
        SubagentProfile {
            name: "explore".to_string(),
            system_prompt: "You are the explore subagent. You can inspect the codebase read-only and return a concise answer.".to_string(),
            enabled_tools: vec!["grep".to_string(), "read".to_string()],
        },
    )]);
    for profile in discover_custom_agent_profiles(config) {
        if profile.agent_type != "subagent" || !is_agent_available(config, &profile) {
            continue;
        }
        let system_prompt = profile
            .overrides
            .get("system_prompt_id")
            .and_then(toml::Value::as_str)
            .and_then(load_system_prompt)
            .unwrap_or_else(|| {
                format!(
                    "You are the {} subagent. Return a concise answer.",
                    profile.name
                )
            });
        let enabled_tools = profile
            .overrides
            .get("enabled_tools")
            .and_then(string_list)
            .unwrap_or_else(|| vec!["grep".to_string(), "read".to_string()]);
        profiles.insert(
            profile.name.clone(),
            SubagentProfile {
                name: profile.name,
                system_prompt,
                enabled_tools,
            },
        );
    }
    profiles.into_values().collect()
}

pub fn primary_agent_order(config: &Config) -> Vec<AgentSummary> {
    let mut profiles = BTreeMap::new();
    for name in [
        "default",
        "plan",
        "accept-edits",
        "auto-approve",
        "explore",
        "lean",
    ] {
        if let Some(profile) = load_builtin_agent_profile(name) {
            profiles.insert(name.to_string(), profile);
        }
    }
    for profile in discover_custom_agent_profiles(config) {
        profiles.insert(profile.name.clone(), profile);
    }
    let builtin_order = ["default", "plan", "accept-edits", "auto-approve"];
    let mut ordered = Vec::new();
    for name in builtin_order {
        if let Some(profile) = profiles.get(name)
            && profile.agent_type == "agent"
            && is_agent_available(config, profile)
        {
            ordered.push(agent_summary(profile));
        }
    }
    let mut custom = profiles
        .values()
        .filter(|profile| {
            !builtin_order.contains(&profile.name.as_str())
                && profile.agent_type == "agent"
                && is_agent_available(config, profile)
        })
        .map(agent_summary)
        .collect::<Vec<_>>();
    custom.sort_by(|left, right| left.name.cmp(&right.name));
    ordered.extend(custom);
    ordered
}

fn agent_summary(profile: &AgentProfile) -> AgentSummary {
    AgentSummary {
        name: profile.name.clone(),
        display_name: profile.display_name.clone(),
    }
}

fn agent_search_paths(config: &Config) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for path in &config.agent_paths {
        let path = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        };
        paths.push(path);
    }
    for dir in cwd.ancestors() {
        paths.push(dir.join(".vibe").join("agents"));
    }
    if let Ok(vibe_home) = std::env::var("VIBE_HOME") {
        paths.push(PathBuf::from(vibe_home).join("agents"));
    }
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".vibe").join("agents"));
    }
    dedup_paths(paths)
}

fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

fn is_agent_available(config: &Config, profile: &AgentProfile) -> bool {
    if profile.install_required
        && !config
            .installed_agents
            .iter()
            .any(|name| name == &profile.name)
    {
        return false;
    }
    if !config.enabled_agents.is_empty() {
        return name_matches(&profile.name, &config.enabled_agents);
    }
    !name_matches(&profile.name, &config.disabled_agents)
}

fn excluded_agent_message(config: &Config, profile: &AgentProfile, explicit_agent: bool) -> String {
    if profile.install_required
        && !config
            .installed_agents
            .iter()
            .any(|name| name == &profile.name)
    {
        return format!(
            "Agent '{}' requires installation. Run it once via --agent '{}', or add it to 'installed_agents'.",
            profile.name, profile.name
        );
    }
    let is_default = !explicit_agent && profile.name == config.default_agent;
    let label = if is_default { "default_agent" } else { "Agent" };
    let fix = if is_default {
        "set 'default_agent' to an enabled agent"
    } else {
        "select an enabled agent"
    };
    if !config.enabled_agents.is_empty() && !name_matches(&profile.name, &config.enabled_agents) {
        return format!(
            "{label} '{}' is not in 'enabled_agents' {}. Add '{}' to 'enabled_agents', or {fix}.",
            profile.name,
            python_list(&config.enabled_agents),
            profile.name
        );
    }
    if name_matches(&profile.name, &config.disabled_agents) {
        return format!(
            "{label} '{}' is in 'disabled_agents' {}. Remove '{}' from 'disabled_agents', or {fix}.",
            profile.name,
            python_list(&config.disabled_agents),
            profile.name
        );
    }
    format!(
        "Agent '{}' is not available. It may be disabled, not installed, or excluded by your config.",
        profile.name
    )
}

fn python_list(values: &[String]) -> String {
    let values = values
        .iter()
        .map(|value| format!("'{}'", value.replace('\'', "\\'")))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{values}]")
}

fn apply_agent_overrides(config: &mut Config, overrides: &BTreeMap<String, toml::Value>) {
    let base_disabled_tools = config.disabled_tools.clone();
    for (key, value) in overrides {
        match key.as_str() {
            "base_disabled" => merge_string_list(&mut config.disabled_tools, value),
            "enabled_tools" => {
                if let Some(mut enabled) = string_list(value) {
                    if !base_disabled_tools.is_empty() {
                        enabled.retain(|name| !name_matches(name, &base_disabled_tools));
                    }
                    config.enabled_tools = enabled;
                }
            }
            "disabled_tools" => {
                if let Some(disabled) = string_list(value) {
                    config.disabled_tools = disabled;
                }
            }
            "bypass_tool_permissions" => {
                if let Some(enabled) = value.as_bool() {
                    config.bypass_tool_permissions = enabled;
                }
            }
            "system_prompt_id" => {
                if let Some(id) = value.as_str() {
                    config.system_prompt_id = Some(id.to_string());
                }
            }
            "active_model" => {
                if let Some(alias) = value.as_str() {
                    config.active_model = Some(alias.to_string());
                }
            }
            "model" => apply_model_override(config, value),
            "models" => apply_models_override(config, value),
            "providers" => apply_providers_override(config, value),
            "tools" => apply_tools_override(config, value),
            _ => {}
        }
    }
    apply_active_model(config);
}

fn merge_string_list(target: &mut Vec<String>, value: &toml::Value) {
    if let Some(values) = string_list(value) {
        for value in values {
            if !target.contains(&value) {
                target.push(value);
            }
        }
        target.sort();
    }
}

fn string_list(value: &toml::Value) -> Option<Vec<String>> {
    value.as_array().map(|values| {
        values
            .iter()
            .filter_map(|value| value.as_str().map(ToString::to_string))
            .collect()
    })
}

fn apply_model_override(config: &mut Config, value: &toml::Value) {
    let Some(table) = value.as_table() else {
        return;
    };
    if let Some(provider) = table.get("provider").and_then(toml::Value::as_str) {
        config.model.provider = provider.to_string();
    }
    if let Some(name) = table.get("name").and_then(toml::Value::as_str) {
        config.model.name = name.to_string();
    }
    if let Some(temperature) = table.get("temperature").and_then(toml::Value::as_float) {
        config.model.temperature = temperature as f32;
    }
    if let Some(tokens) = table
        .get("max_context_tokens")
        .and_then(toml::Value::as_integer)
        && tokens >= 0
    {
        config.model.max_context_tokens = tokens as u64;
    }
    if let Some(price) = table.get("input_price").and_then(toml::Value::as_float) {
        config.model.input_price = price;
    }
    if let Some(price) = table.get("output_price").and_then(toml::Value::as_float) {
        config.model.output_price = price;
    }
}

fn apply_models_override(config: &mut Config, value: &toml::Value) {
    let Some(models) = value.as_array() else {
        return;
    };
    let mut parsed = Vec::new();
    for model in models {
        let Some(table) = model.as_table() else {
            continue;
        };
        let Some(name) = table.get("name").and_then(toml::Value::as_str) else {
            continue;
        };
        let Some(provider) = table.get("provider").and_then(toml::Value::as_str) else {
            continue;
        };
        let alias = table
            .get("alias")
            .and_then(toml::Value::as_str)
            .unwrap_or(name);
        let thinking = table
            .get("thinking")
            .and_then(toml::Value::as_str)
            .unwrap_or("off");
        let supports_images = table
            .get("supports_images")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false);
        parsed.push(microvibe_config::UiModelConfig {
            name: name.to_string(),
            provider: provider.to_string(),
            alias: alias.to_string(),
            thinking: thinking.to_string(),
            supports_images,
        });
    }
    if !parsed.is_empty() {
        config.models = parsed;
    }
}

fn apply_providers_override(config: &mut Config, value: &toml::Value) {
    if let Ok(providers) = value
        .clone()
        .try_into::<BTreeMap<String, microvibe_config::ProviderConfig>>()
    {
        for (name, provider) in providers {
            config.providers.insert(name, provider);
        }
        return;
    }

    let Some(providers) = value.as_array() else {
        return;
    };
    for provider in providers {
        let Some(table) = provider.as_table() else {
            continue;
        };
        let Some(name) = table.get("name").and_then(toml::Value::as_str) else {
            continue;
        };
        let base_url = table
            .get("base_url")
            .or_else(|| table.get("api_base"))
            .and_then(toml::Value::as_str)
            .unwrap_or("https://api.mistral.ai/v1");
        let api_key_env = table
            .get("api_key_env")
            .or_else(|| table.get("api_key_env_var"))
            .and_then(toml::Value::as_str)
            .unwrap_or("MISTRAL_API_KEY");
        let wire_format = table
            .get("wire_format")
            .or_else(|| table.get("api_style"))
            .or_else(|| table.get("backend"))
            .and_then(toml::Value::as_str)
            .unwrap_or("openai_chat");
        let backend = table
            .get("backend")
            .and_then(toml::Value::as_str)
            .unwrap_or("generic");
        let browser_auth_base_url = table
            .get("browser_auth_base_url")
            .and_then(toml::Value::as_str)
            .map(str::to_string);
        let browser_auth_api_base_url = table
            .get("browser_auth_api_base_url")
            .and_then(toml::Value::as_str)
            .map(str::to_string);
        config.providers.insert(
            name.to_string(),
            microvibe_config::ProviderConfig {
                base_url: base_url.to_string(),
                api_key_env: api_key_env.to_string(),
                backend: backend.to_string(),
                browser_auth_base_url,
                browser_auth_api_base_url,
                wire_format: wire_format.to_string(),
            },
        );
    }
}

fn apply_tools_override(config: &mut Config, value: &toml::Value) {
    let Some(tools) = value.as_table() else {
        return;
    };
    for (name, value) in tools {
        if let Ok(incoming) = value.clone().try_into::<ToolPermissionConfig>() {
            let target = config.tools.entry(name.to_string()).or_default();
            merge_tool_config(target, incoming);
        }
    }
}

fn merge_tool_config(target: &mut ToolPermissionConfig, incoming: ToolPermissionConfig) {
    if !incoming.allowlist.is_empty() {
        target.allowlist = incoming.allowlist;
    }
    if !incoming.denylist.is_empty() {
        target.denylist = incoming.denylist;
    }
    if !incoming.sensitive_patterns.is_empty() {
        target.sensitive_patterns = incoming.sensitive_patterns;
    }
    if incoming.permission.is_some() {
        target.permission = incoming.permission;
    }
    if incoming.max_output_bytes.is_some() {
        target.max_output_bytes = incoming.max_output_bytes;
    }
    if incoming.default_timeout.is_some() {
        target.default_timeout = incoming.default_timeout;
    }
    if incoming.default_max_matches.is_some() {
        target.default_max_matches = incoming.default_max_matches;
    }
    if incoming.exclude_patterns.is_some() {
        target.exclude_patterns = incoming.exclude_patterns;
    }
    if incoming.codeignore_file.is_some() {
        target.codeignore_file = incoming.codeignore_file;
    }
    if incoming.max_read_bytes.is_some() {
        target.max_read_bytes = incoming.max_read_bytes;
    }
    if incoming.max_write_bytes.is_some() {
        target.max_write_bytes = incoming.max_write_bytes;
    }
    if incoming.create_parent_dirs.is_some() {
        target.create_parent_dirs = incoming.create_parent_dirs;
    }
    if incoming.max_content_bytes.is_some() {
        target.max_content_bytes = incoming.max_content_bytes;
    }
    if incoming.max_timeout.is_some() {
        target.max_timeout = incoming.max_timeout;
    }
    if incoming.user_agent.is_some() {
        target.user_agent = incoming.user_agent;
    }
    if incoming.timeout.is_some() {
        target.timeout = incoming.timeout;
    }
    if incoming.model.is_some() {
        target.model = incoming.model;
    }
    if incoming.max_todos.is_some() {
        target.max_todos = incoming.max_todos;
    }
}

fn apply_active_model(config: &mut Config) {
    let Some(active) = config.active_model.as_deref() else {
        return;
    };
    if let Some(model) = config.models.iter().find(|model| model.alias == active) {
        config.model.name = model.name.clone();
        config.model.provider = model.provider.clone();
    }
}

fn name_matches(name: &str, patterns: &[String]) -> bool {
    let lower_name = name.to_ascii_lowercase();
    patterns.iter().any(|raw| {
        let pattern = raw.trim();
        if pattern.is_empty() {
            return false;
        }
        if let Some(regex) = pattern.strip_prefix("re:") {
            return RegexBuilder::new(regex)
                .case_insensitive(true)
                .build()
                .is_ok_and(|regex| {
                    regex
                        .find(name)
                        .is_some_and(|m| m.start() == 0 && m.end() == name.len())
                });
        }
        Pattern::new(&pattern.to_ascii_lowercase())
            .is_ok_and(|pattern| pattern.matches(&lower_name))
    })
}

fn apply_builtin_agent_overrides(config: &mut Config) {
    match config.default_agent.as_str() {
        "default" => {
            disable_agent_tools(config, &["exit_plan_mode"]);
        }
        "plan" => {
            let plan_pattern = plan_file_allowlist_pattern();
            for name in ["write_file", "edit"] {
                let tool = config.tools.entry(name.to_string()).or_default();
                tool.permission = Some("never".to_string());
                if !tool.allowlist.contains(&plan_pattern) {
                    tool.allowlist.push(plan_pattern.clone());
                    tool.allowlist.sort();
                }
            }
        }
        "accept-edits" => {
            disable_agent_tools(config, &["exit_plan_mode"]);
            for name in ["write_file", "edit"] {
                config.tools.entry(name.to_string()).or_default().permission =
                    Some("always".to_string());
            }
        }
        "auto-approve" => {
            config.bypass_tool_permissions = true;
            disable_agent_tools(config, &["exit_plan_mode"]);
        }
        "explore" => {
            config.enabled_tools = vec!["grep".to_string(), "read".to_string()];
        }
        _ => {}
    }
}

fn disable_agent_tools(config: &mut Config, tools: &[&str]) {
    for tool in tools {
        let tool = tool.to_string();
        if !config.disabled_tools.contains(&tool) {
            config.disabled_tools.push(tool);
        }
    }
}

fn plan_file_allowlist_pattern() -> String {
    let vibe_home = std::env::var("VIBE_HOME")
        .map(PathBuf::from)
        .or_else(|_| dirs::home_dir().map(|home| home.join(".vibe")).ok_or(()))
        .unwrap_or_else(|_| PathBuf::from(".vibe"));
    vibe_home
        .join("plans")
        .join("*")
        .to_string_lossy()
        .to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedToolPermission {
    Always,
    Ask,
    Never,
}

fn resolve_tool_permission(config: &Config, call: &ToolCall) -> ResolvedToolPermission {
    if config.bypass_tool_permissions {
        return ResolvedToolPermission::Always;
    }
    let tool_config = config.tools.get(&call.name);
    if let Some(permission) = resolve_special_permission(config, call, tool_config) {
        return permission;
    }
    match configured_permission(tool_config) {
        Some(permission) => permission,
        None if tool_call_requires_approval(call) => ResolvedToolPermission::Ask,
        None => ResolvedToolPermission::Always,
    }
}

fn resolve_special_permission(
    _config: &Config,
    call: &ToolCall,
    tool_config: Option<&ToolPermissionConfig>,
) -> Option<ResolvedToolPermission> {
    match call.name.as_str() {
        "bash" => Some(resolve_bash_permission(call, tool_config)),
        "edit" | "grep" | "read" | "write_file" => Some(resolve_file_permission(call, tool_config)),
        "task" => Some(resolve_task_permission(call, tool_config)),
        _ => None,
    }
}

fn configured_permission(
    tool_config: Option<&ToolPermissionConfig>,
) -> Option<ResolvedToolPermission> {
    match tool_config
        .and_then(|config| config.permission.as_deref())
        .map(|permission| permission.to_ascii_lowercase())
        .as_deref()
    {
        Some("always") => Some(ResolvedToolPermission::Always),
        Some("never") => Some(ResolvedToolPermission::Never),
        Some("ask") => Some(ResolvedToolPermission::Ask),
        _ => None,
    }
}

fn resolve_bash_permission(
    call: &ToolCall,
    tool_config: Option<&ToolPermissionConfig>,
) -> ResolvedToolPermission {
    let command = call
        .arguments
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(tool_config) = tool_config else {
        return ResolvedToolPermission::Ask;
    };
    if tool_config
        .denylist
        .iter()
        .any(|pattern| command_matches_allowlist(command, pattern))
    {
        return ResolvedToolPermission::Never;
    }
    let sensitive_patterns = if tool_config.sensitive_patterns.is_empty() {
        &["sudo".to_string()][..]
    } else {
        &tool_config.sensitive_patterns
    };
    let head = command
        .split_once(char::is_whitespace)
        .map(|(head, _)| head)
        .unwrap_or(command);
    if sensitive_patterns.iter().any(|pattern| pattern == head) {
        return ResolvedToolPermission::Ask;
    }
    if tool_config
        .allowlist
        .iter()
        .any(|pattern| command_matches_allowlist(command, pattern))
    {
        return ResolvedToolPermission::Always;
    }
    configured_permission(Some(tool_config)).unwrap_or(ResolvedToolPermission::Ask)
}

fn resolve_task_permission(
    call: &ToolCall,
    tool_config: Option<&ToolPermissionConfig>,
) -> ResolvedToolPermission {
    let agent = call
        .arguments
        .get("agent")
        .and_then(Value::as_str)
        .unwrap_or("explore");
    let default_allowlist = ["explore".to_string()];
    let allowlist = tool_config
        .map(|config| config.allowlist.as_slice())
        .filter(|allowlist| !allowlist.is_empty())
        .unwrap_or(&default_allowlist);
    if tool_config.is_some_and(|config| name_matches(agent, &config.denylist)) {
        return ResolvedToolPermission::Never;
    }
    if name_matches(agent, allowlist) {
        return ResolvedToolPermission::Always;
    }
    configured_permission(tool_config).unwrap_or(ResolvedToolPermission::Ask)
}

fn resolve_file_permission(
    call: &ToolCall,
    tool_config: Option<&ToolPermissionConfig>,
) -> ResolvedToolPermission {
    let Some(path) = file_tool_path(call) else {
        return configured_permission(tool_config).unwrap_or_else(|| {
            if tool_call_requires_approval(call) {
                ResolvedToolPermission::Ask
            } else {
                ResolvedToolPermission::Always
            }
        });
    };
    if let Some(tool_config) = tool_config {
        if path_matches_any(&path, &tool_config.denylist) {
            return ResolvedToolPermission::Never;
        }
        if path_matches_any(&path, &tool_config.allowlist) {
            return ResolvedToolPermission::Always;
        }
    }

    let default_sensitive_patterns = ["**/.env".to_string(), "**/.env.*".to_string()];
    let sensitive_patterns = tool_config
        .map(|config| config.sensitive_patterns.as_slice())
        .filter(|patterns| !patterns.is_empty())
        .unwrap_or(&default_sensitive_patterns);
    if path_matches_any(&path, sensitive_patterns) {
        return ResolvedToolPermission::Ask;
    }

    configured_permission(tool_config).unwrap_or_else(|| {
        if tool_call_requires_approval(call) {
            ResolvedToolPermission::Ask
        } else {
            ResolvedToolPermission::Always
        }
    })
}

fn file_tool_path(call: &ToolCall) -> Option<String> {
    let key = match call.name.as_str() {
        "edit" | "read" => "file_path",
        "grep" => "path",
        "write_file" => "path",
        _ => return None,
    };
    call.arguments
        .get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn path_matches_any(raw: &str, patterns: &[String]) -> bool {
    let absolute = absolutize_path(raw).to_string_lossy().to_string();
    patterns.iter().any(|pattern| {
        Pattern::new(pattern)
            .map(|glob| glob.matches(&absolute))
            .unwrap_or(false)
    })
}

fn absolutize_path(raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    let path = if path == Path::new("~") {
        dirs::home_dir().unwrap_or(path)
    } else if let Some(rest) = raw.strip_prefix("~/") {
        dirs::home_dir().map(|home| home.join(rest)).unwrap_or(path)
    } else if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    path.canonicalize().unwrap_or(path)
}

fn is_persistently_allowed(config: &Config, call: &ToolCall) -> bool {
    if call.name != "bash" {
        return false;
    }
    let command = call
        .arguments
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    config
        .tools
        .get(&call.name)
        .is_some_and(|tool_config| bash_allowlist_matches(tool_config, command))
}

fn bash_allowlist_matches(tool_config: &ToolPermissionConfig, command: &str) -> bool {
    tool_config
        .allowlist
        .iter()
        .any(|pattern| command_matches_allowlist(command, pattern))
}

fn command_matches_allowlist(command: &str, pattern: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix(" *") {
        return command == prefix || command.starts_with(&format!("{prefix} "));
    }
    command == pattern || command.starts_with(&format!("{pattern} "))
}

fn approval_key(call: &ToolCall) -> String {
    if call.name == "bash" {
        let command = call
            .arguments
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let head = command
            .split_once(char::is_whitespace)
            .map(|(head, _)| head)
            .unwrap_or(command);
        return format!("bash:{head}");
    }
    if call.name == "web_fetch" {
        let url = call
            .arguments
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return format!("web_fetch:{}", url_domain(url));
    }
    if call.name == "web_search" {
        return "web_search".to_string();
    }
    call.name.clone()
}

fn url_domain(url: &str) -> String {
    let without_scheme = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .trim_start_matches('/');
    without_scheme
        .split('/')
        .next()
        .unwrap_or(without_scheme)
        .to_string()
}

fn approval_allowlist_patterns(call: &ToolCall) -> Option<Vec<String>> {
    if call.name == "bash" {
        let command = call
            .arguments
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let tokens: Vec<&str> = command.split_whitespace().collect();
        if !tokens.is_empty() {
            return Some(vec![bash_session_permission_pattern(&tokens)]);
        }
    }
    None
}

fn bash_session_permission_pattern(tokens: &[&str]) -> String {
    for length in (1..=tokens.len()).rev() {
        let prefix = tokens[..length].join(" ");
        if let Some(arity) = bash_permission_arity(&prefix) {
            return format!("{} *", tokens[..arity.min(tokens.len())].join(" "));
        }
    }
    format!("{} *", tokens[0])
}

fn bash_permission_arity(prefix: &str) -> Option<usize> {
    match prefix {
        "cat" | "cd" | "chmod" | "chown" | "cp" | "echo" | "env" | "export" | "grep" | "kill"
        | "killall" | "ln" | "ls" | "mkdir" | "mv" | "ps" | "pwd" | "rm" | "rmdir" | "sleep"
        | "source" | "tail" | "touch" | "unset" | "which" => Some(1),
        "bazel" | "brew" | "bun" | "cargo" | "cdk" | "cf" | "cmake" | "composer" | "consul"
        | "crictl" | "deno" | "docker" | "firebase" | "flyctl" | "git" | "go" | "gradle"
        | "helm" | "heroku" | "hugo" | "ip" | "kind" | "kubectl" | "kustomize" | "make" | "mc"
        | "minikube" | "mongosh" | "mysql" | "mvn" | "ng" | "npm" | "nvm" | "nx" | "openssl"
        | "pip" | "pipenv" | "pnpm" | "poetry" | "podman" | "psql" | "pulumi" | "pyenv"
        | "python" | "rake" | "rbenv" | "redis-cli" | "rustup" | "serverless" | "skaffold"
        | "sls" | "sst" | "swift" | "systemctl" | "terraform" | "tmux" | "turbo" | "ufw" | "uv"
        | "vercel" | "volta" | "wp" | "yarn" => Some(2),
        "aws" | "az" | "doctl" | "eksctl" | "gcloud" | "gh" | "sfdx" | "vault" => Some(3),
        "bun run"
        | "bun x"
        | "cargo add"
        | "cargo run"
        | "consul kv"
        | "deno task"
        | "docker builder"
        | "docker compose"
        | "docker container"
        | "docker image"
        | "docker network"
        | "docker volume"
        | "eksctl create"
        | "git config"
        | "git remote"
        | "git stash"
        | "ip addr"
        | "ip link"
        | "ip netns"
        | "ip route"
        | "kubectl kustomize"
        | "kubectl rollout"
        | "mc admin"
        | "npm exec"
        | "npm init"
        | "npm run"
        | "npm view"
        | "openssl req"
        | "openssl x509"
        | "pnpm dlx"
        | "pnpm exec"
        | "pnpm run"
        | "podman container"
        | "podman image"
        | "pulumi stack"
        | "terraform workspace"
        | "uv run"
        | "vault auth"
        | "vault kv"
        | "yarn dlx"
        | "yarn run" => Some(3),
        _ => None,
    }
}

async fn request_approval(
    call: &ToolCall,
    approval_tx: Option<&mpsc::UnboundedSender<ApprovalRequest>>,
) -> microvibe_protocol::ApprovalDecision {
    let Some(approval_tx) = approval_tx else {
        return microvibe_protocol::ApprovalDecision::AllowOnce;
    };
    let (respond_to, response) = oneshot::channel();
    if approval_tx
        .send(ApprovalRequest {
            id: call.id.clone(),
            call: call.clone(),
            respond_to,
        })
        .is_err()
    {
        return microvibe_protocol::ApprovalDecision::Deny;
    }
    response
        .await
        .unwrap_or(microvibe_protocol::ApprovalDecision::Deny)
}

async fn request_question(
    call: &ToolCall,
    question_tx: Option<&mpsc::UnboundedSender<QuestionRequest>>,
) -> ToolResult {
    let Some(question_tx) = question_tx else {
        return ToolResult {
            call_id: call.id.clone(),
            name: call.name.clone(),
            output: "User input not available. This tool requires an interactive UI.".to_string(),
            success: false,
        };
    };
    let (respond_to, response) = oneshot::channel();
    if question_tx
        .send(QuestionRequest {
            id: call.id.clone(),
            call: call.clone(),
            respond_to,
        })
        .is_err()
    {
        return ToolResult {
            call_id: call.id.clone(),
            name: call.name.clone(),
            output: "User input not available. This tool requires an interactive UI.".to_string(),
            success: false,
        };
    }
    let response = response
        .await
        .unwrap_or_else(|_| QuestionResponse::cancelled());
    ToolResult {
        call_id: call.id.clone(),
        name: call.name.clone(),
        output: question_result_output(call, &response),
        success: !response.cancelled,
    }
}

async fn request_exit_plan_mode(
    config: &Config,
    call: &ToolCall,
    question_tx: Option<&mpsc::UnboundedSender<QuestionRequest>>,
) -> ToolResult {
    if config.default_agent != "plan" {
        return ToolResult {
            call_id: call.id.clone(),
            name: call.name.clone(),
            output: "ExitPlanMode can only be used in plan mode.".to_string(),
            success: false,
        };
    }
    let question_call = ToolCall {
        id: call.id.clone(),
        name: "exit_plan_mode".to_string(),
        arguments: exit_plan_question_args(&plan_file_path()),
    };
    let Some(question_tx) = question_tx else {
        return ToolResult {
            call_id: call.id.clone(),
            name: call.name.clone(),
            output: "ExitPlanMode requires an interactive UI.".to_string(),
            success: false,
        };
    };
    let (respond_to, response) = oneshot::channel();
    if question_tx
        .send(QuestionRequest {
            id: call.id.clone(),
            call: question_call,
            respond_to,
        })
        .is_err()
    {
        return ToolResult {
            call_id: call.id.clone(),
            name: call.name.clone(),
            output: "ExitPlanMode requires an interactive UI.".to_string(),
            success: false,
        };
    }
    let response = response
        .await
        .unwrap_or_else(|_| QuestionResponse::cancelled());
    let (switched, message) = exit_plan_message(&response);
    ToolResult {
        call_id: call.id.clone(),
        name: call.name.clone(),
        output: format!("switched: {}\nmessage: {message}", py_bool(switched)),
        success: switched,
    }
}

fn plan_file_path() -> PathBuf {
    let vibe_home = std::env::var("VIBE_HOME")
        .map(PathBuf::from)
        .or_else(|_| dirs::home_dir().map(|home| home.join(".vibe")).ok_or(()))
        .unwrap_or_else(|_| PathBuf::from(".vibe"));
    let plans_dir = vibe_home.join("plans");
    let _ = std::fs::create_dir_all(&plans_dir);
    let timestamp = chrono::Utc::now().timestamp();
    plans_dir.join(format!("{timestamp}-plan.md"))
}

fn exit_plan_question_args(plan_path: &Path) -> Value {
    json!({
        "questions": [{
            "question": "Plan is complete. Switch to accept-edits mode and start implementing?",
            "header": "Plan ready",
            "options": [
                {
                    "label": "Yes, and auto approve edits",
                    "description": "Switch to accept-edits mode with auto-approve permissions"
                },
                {
                    "label": "Yes, and request approval for edits",
                    "description": "Switch to default agent mode (manual approval for edits)"
                },
                {
                    "label": "No",
                    "description": "Stay in plan mode and continue planning"
                }
            ]
        }],
        "footer_note": format!("Plan: {} (Ctrl+G to edit)", plan_path.display())
    })
}

fn exit_plan_message(response: &QuestionResponse) -> (bool, String) {
    let Some(answer) = response.answers.first() else {
        return (false, "User cancelled. Staying in plan mode.".to_string());
    };
    if response.cancelled || answer.answer.is_empty() {
        return (false, "User cancelled. Staying in plan mode.".to_string());
    }
    match answer.answer.to_ascii_lowercase().as_str() {
        "yes, and auto approve edits" => (
            true,
            "Switched to accept-edits mode. You can now start implementing the plan.".to_string(),
        ),
        "yes, and request approval for edits" => (
            true,
            "Switched to default agent mode. Edits will require your approval.".to_string(),
        ),
        "no" => (
            false,
            "Staying in plan mode. Continue refining the plan.".to_string(),
        ),
        other if answer.is_other => (
            false,
            format!("Staying in plan mode. User feedback: {other}"),
        ),
        _ => (
            false,
            "Staying in plan mode. Continue refining the plan.".to_string(),
        ),
    }
}

fn question_result_output(call: &ToolCall, response: &QuestionResponse) -> String {
    if response.cancelled {
        return "answers: []\ncancelled: True".to_string();
    }
    let questions = call
        .arguments
        .get("questions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let answers = response
        .answers
        .iter()
        .enumerate()
        .map(|(idx, answer)| {
            let question = if answer.question.is_empty() {
                questions
                    .get(idx)
                    .and_then(|question| question.get("question"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            } else {
                &answer.question
            };
            format!(
                "{{'question': {}, 'answer': {}, 'is_other': {}}}",
                python_string_repr(question),
                python_string_repr(&answer.answer),
                if answer.is_other { "True" } else { "False" }
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("answers: [{}]\ncancelled: False", answers)
}

fn python_string_repr(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn py_bool(value: bool) -> &'static str {
    if value { "True" } else { "False" }
}

#[derive(Debug)]
struct ChatOutcome {
    message: Message,
    tool_calls: Vec<ToolCall>,
    usage: Usage,
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Default)]
struct ToolCallAccumulator {
    calls: Vec<PartialToolCall>,
}

impl ToolCallAccumulator {
    fn push(&mut self, value: &Value) {
        let Some(items) = value.as_array() else {
            return;
        };
        for (fallback_index, item) in items.iter().enumerate() {
            let index = item
                .get("index")
                .and_then(Value::as_u64)
                .map(|idx| idx as usize)
                .unwrap_or(fallback_index);
            if self.calls.len() <= index {
                self.calls.resize_with(index + 1, PartialToolCall::default);
            }
            let call = &mut self.calls[index];
            if let Some(id) = item.get("id").and_then(Value::as_str)
                && !id.is_empty()
            {
                call.id = id.to_string();
            }
            if let Some(name) = item.pointer("/function/name").and_then(Value::as_str)
                && !name.is_empty()
            {
                call.name = name.to_string();
            }
            if let Some(arguments) = item.pointer("/function/arguments") {
                match arguments {
                    Value::String(fragment) => call.arguments.push_str(fragment),
                    other if !other.is_null() => call.arguments.push_str(&other.to_string()),
                    _ => {}
                }
            }
        }
    }

    fn finish(self) -> Result<Vec<ToolCall>> {
        self.calls
            .into_iter()
            .filter(|call| !call.name.is_empty())
            .map(|call| {
                let arguments = if call.arguments.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&call.arguments).with_context(|| {
                        format!("failed to parse tool arguments for {}", call.name)
                    })?
                };
                Ok(ToolCall {
                    id: if call.id.is_empty() {
                        uuid::Uuid::new_v4().to_string()
                    } else {
                        call.id
                    },
                    name: call.name,
                    arguments,
                })
            })
            .collect()
    }
}

fn parse_full_tool_calls(value: Option<&Value>) -> Result<Vec<ToolCall>> {
    let Some(Value::Array(items)) = value else {
        return Ok(Vec::new());
    };
    items
        .iter()
        .filter_map(|item| {
            let name = item.pointer("/function/name").and_then(Value::as_str)?;
            Some((item, name))
        })
        .map(|(item, name)| {
            let raw_arguments = item
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let arguments = if raw_arguments.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(raw_arguments)
                    .with_context(|| format!("failed to parse tool arguments for {name}"))?
            };
            Ok(ToolCall {
                id: item
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(ToString::to_string)
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                name: name.to_string(),
                arguments,
            })
        })
        .collect()
}

pub struct Session {
    pub id: SessionId,
    pub agent: Agent,
    pub store: SessionStore,
}

impl Session {
    pub fn new(config: Config) -> Self {
        let id = SessionId::new();
        let store = SessionStore::new(id.clone()).expect("session store path must be valid");
        Self {
            id,
            agent: Agent::new(config),
            store,
        }
    }

    pub fn resume(config: Config, session_dir: std::path::PathBuf) -> Result<Self> {
        let (store, messages) = SessionStore::resume(session_dir)?;
        let id = store.session_id.clone();
        Ok(Self {
            id,
            agent: Agent::with_messages(config, messages),
            store,
        })
    }

    pub fn fork_from(config: Config, source: &Session) -> Result<Self> {
        Self::fork_from_messages(config, source, source.agent.messages_owned())
    }

    pub fn fork_from_message_id(
        config: Config,
        source: &Session,
        message_id: &str,
    ) -> Result<Self> {
        let source_messages = source.agent.messages();
        let non_system = source_messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.role != Role::System)
            .collect::<Vec<_>>();
        let Some((anchor_non_system_index, (_, anchor))) = non_system
            .iter()
            .enumerate()
            .find(|(_, (_, message))| message.message_id.as_deref() == Some(message_id))
        else {
            anyhow::bail!("Cannot fork from unknown message_id: {message_id}");
        };
        if anchor.role != Role::User {
            anyhow::bail!("Fork from message_id is only supported for user messages");
        }
        let end_source_index = non_system
            .iter()
            .skip(anchor_non_system_index + 1)
            .find(|(_, message)| message.role == Role::User)
            .map(|(source_index, _)| *source_index)
            .unwrap_or(source_messages.len());
        let messages = source_messages[..end_source_index].to_vec();
        Self::fork_from_messages(config, source, messages)
    }

    fn fork_from_messages(
        config: Config,
        source: &Session,
        messages: Vec<Message>,
    ) -> Result<Self> {
        let suffix = source
            .id
            .0
            .rsplit_once('-')
            .map(|(_, suffix)| suffix)
            .unwrap_or(&source.id.0);
        let id = SessionId::new_with_suffix(suffix);
        let store = SessionStore::new(id.clone())?;
        Ok(Self {
            id,
            agent: Agent::with_messages(config, messages),
            store,
        })
    }

    pub fn resume_latest_for_cwd(config: Config, cwd: &Path) -> Result<Option<Self>> {
        let Some(saved) = SessionStore::latest_for_cwd(cwd)? else {
            return Ok(None);
        };
        Self::resume(config, saved.session_dir).map(Some)
    }

    pub fn resume_by_id(config: Config, session_id: &str) -> Result<Option<Self>> {
        let Some(saved) = SessionStore::find_by_id(session_id)? else {
            return Ok(None);
        };
        Self::resume(config, saved.session_dir).map(Some)
    }

    pub fn switch_agent(&mut self, config: Config) {
        let messages = self.agent.messages_owned();
        self.agent = Agent::with_messages(config, messages);
    }

    pub fn agent_config(&self) -> Config {
        self.agent.config().clone()
    }

    pub async fn save(&mut self) -> Result<()> {
        let usage = self.agent.last_usage().clone();
        let tools = self.agent.tool_specs();
        let model = self.agent.model().to_string();
        let provider = self.agent.provider().to_string();
        self.store
            .save(self.agent.messages(), &usage, &tools, &model, &provider)
            .await
    }

    pub async fn rewind_to_message(&mut self, index: usize) -> Result<String> {
        let Some(message) = self.agent.messages().get(index) else {
            anyhow::bail!("Invalid message index: {index}");
        };
        if message.role != Role::User {
            anyhow::bail!("Message at index {index} is not a user message");
        }
        let message_content = text_content(message);
        self.save().await?;
        self.agent.truncate_before_message(index);
        self.id = SessionId::new();
        self.store = SessionStore::new(self.id.clone())?;
        Ok(message_content)
    }

    pub async fn compact(&mut self, extra_instructions: &str) -> Result<(SessionId, SessionId)> {
        let old_session_id = self.id.clone();
        self.save().await?;
        self.agent.compact(extra_instructions).await?;
        self.id = SessionId::new();
        self.store = SessionStore::new(self.id.clone())?;
        self.save().await?;
        Ok((old_session_id, self.id.clone()))
    }
}

pub(crate) fn system_prompt() -> String {
    base_system_prompt(None)
}

fn system_prompt_for_config(config: &Config) -> String {
    let prompt = config
        .system_prompt_id
        .as_deref()
        .and_then(load_system_prompt);
    base_system_prompt(prompt.as_deref())
}

fn base_system_prompt(custom_prompt: Option<&str>) -> String {
    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let instructions = project_instructions();
    let instructions = if instructions.is_empty() {
        String::new()
    } else {
        format!("\n\n{instructions}")
    };

    let prompt = custom_prompt
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .unwrap_or("You are Vibe, Mistral's coding agent, reimplemented in Rust.\nMatch Mistral Vibe behavior and UI exactly.");
    format!("{prompt}\nWork in: {cwd}.{instructions}")
}

fn load_system_prompt(id: &str) -> Option<String> {
    let mut candidates = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        for dir in cwd.ancestors() {
            candidates.push(dir.join(".vibe").join("prompts").join(format!("{id}.md")));
        }
    }
    if let Ok(vibe_home) = std::env::var("VIBE_HOME") {
        candidates.push(
            PathBuf::from(vibe_home)
                .join("prompts")
                .join(format!("{id}.md")),
        );
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".vibe").join("prompts").join(format!("{id}.md")));
    }
    dedup_paths(candidates)
        .into_iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
}

fn to_openai_messages(messages: &[Message]) -> Vec<Value> {
    messages
        .iter()
        .map(|message| {
            let role = role_name(message.role);
            let content = text_content(message);
            let mut out = json!({ "role": role, "content": content });
            if message.role == Role::User
                && let Some(images) = message.images.as_ref().filter(|images| !images.is_empty())
            {
                let mut parts = Vec::new();
                if !content.is_empty() {
                    parts.push(json!({ "type": "text", "text": content }));
                }
                for image in images {
                    parts.push(json!({
                        "type": "image_url",
                        "image_url": { "url": image_data_uri(image) }
                    }));
                }
                out["content"] = Value::Array(parts);
            }
            if message.role == Role::Assistant {
                let tool_calls = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall(call) => Some(json!({
                            "id": call.id,
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": call.arguments.to_string()
                            }
                        })),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if !tool_calls.is_empty() {
                    out["tool_calls"] = Value::Array(tool_calls);
                }
            }
            if message.role == Role::Tool
                && let Some(result) = message.content.iter().find_map(|block| match block {
                    ContentBlock::ToolResult(result) => Some(result),
                    _ => None,
                })
            {
                out["tool_call_id"] = Value::String(result.call_id.clone());
                out["name"] = Value::String(result.name.clone());
                out["content"] = Value::String(result.output.clone());
            }
            out
        })
        .collect()
}

fn image_data_uri(image: &ImageAttachment) -> String {
    let data = match &image.source {
        ImageSource::File { path } => std::fs::read(path).unwrap_or_default(),
        ImageSource::Inline { data } => BASE64.decode(data).unwrap_or_default(),
    };
    format!("data:{};base64,{}", image.mime_type, BASE64.encode(data))
}

fn text_content(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn compaction_prompt() -> String {
    "Summarize the conversation so far. Preserve user goals, constraints, decisions, file changes, pending work, and any important tool results. Return only the summary.".to_string()
}

fn render_compaction_context(previous_user_messages: &[String], summary: &str) -> String {
    let mut lines = vec![
        "You are continuing a trajectory after a context compaction.".to_string(),
        String::new(),
        "Here are some of the most recent previous user messages, preserved verbatim where possible. Treat them as prior context, not as new requests.".to_string(),
        String::new(),
        "<previous_user_messages>".to_string(),
    ];
    for (idx, message) in previous_user_messages.iter().enumerate() {
        lines.push(format!(
            "<previous_user_message_{idx}>{}</previous_user_message_{idx}>",
            escape_compaction_text(message)
        ));
    }
    lines.extend([
        "</previous_user_messages>".to_string(),
        String::new(),
        "Here is a summary of what has happened so far:".to_string(),
        String::new(),
        "<compaction_summary>".to_string(),
        escape_compaction_text(summary),
        "</compaction_summary>".to_string(),
    ]);
    lines.join("\n")
}

fn escape_compaction_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn assistant_message_with_reasoning(
    text: String,
    tool_calls: Vec<ToolCall>,
    reasoning_content: Option<String>,
    reasoning_message_id: Option<String>,
) -> Message {
    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(ContentBlock::Text { text });
    }
    content.extend(tool_calls.into_iter().map(ContentBlock::ToolCall));
    Message {
        role: Role::Assistant,
        content,
        message_id: None,
        reasoning_content,
        reasoning_message_id,
        injected: false,
        images: None,
        display_content: None,
    }
}

fn tool_result_message(result: ToolResult) -> Message {
    Message {
        role: Role::Tool,
        content: vec![ContentBlock::ToolResult(result)],
        message_id: None,
        reasoning_content: None,
        reasoning_message_id: None,
        injected: false,
        images: None,
        display_content: None,
    }
}

fn non_empty_string(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn to_openai_tools(specs: Vec<microvibe_protocol::ToolSpec>) -> Vec<Value> {
    specs
        .into_iter()
        .map(|spec| {
            json!({
                "type": "function",
                "function": {
                    "name": spec.name,
                    "description": spec.description,
                    "parameters": spec.input_schema
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use microvibe_config::{
        Config, HookConfig, ModelConfig, PermissionsConfig, ProviderConfig, ToolPermissionConfig,
    };
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::io::ErrorKind;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn run_turn_executes_streamed_tool_call_and_continues() {
        let file = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(file.path(), "alpha\nbeta\n")
            .await
            .unwrap();
        let arg = json!({ "file_path": file.path() }).to_string();
        let responses = vec![
            sse_response(vec![json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_read_1",
                            "type": "function",
                            "function": { "name": "read", "arguments": arg }
                        }]
                    }
                }]
            })]),
            sse_response(vec![
                json!({ "choices": [{ "delta": { "content": "read complete" } }] }),
                json!({ "usage": { "prompt_tokens": 10, "completion_tokens": 2 }, "choices": [{ "delta": {} }] }),
            ]),
        ];
        let requests = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_chat_server(responses, requests.clone()).await;
        unsafe {
            std::env::set_var("MICROVIBE_TEST_API_KEY", "test-key");
        }

        let mut session = Session::new(test_config(base_url));
        let (tx, mut rx) = mpsc::unbounded_channel();
        session.agent.run_turn("read the file", tx).await.unwrap();

        let mut saw_started = false;
        let mut saw_completed = false;
        let mut saw_final_delta = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::ToolCallStarted { call } => {
                    saw_started = call.name == "read";
                }
                AgentEvent::ToolCallCompleted { result } => {
                    saw_completed =
                        result.name == "read" && result.success && result.output.contains("alpha");
                }
                AgentEvent::AssistantDelta { text } => {
                    saw_final_delta |= text == "read complete";
                }
                _ => {}
            }
        }
        assert!(saw_started);
        assert!(saw_completed);
        assert!(saw_final_delta);

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let second: Value = serde_json::from_str(&requests[1]).unwrap();
        let messages = second["messages"].as_array().unwrap();
        assert!(messages.iter().any(|message| {
            message["role"] == "assistant" && message["tool_calls"][0]["id"] == "call_read_1"
        }));
        assert!(messages.iter().any(|message| {
            message["role"] == "tool"
                && message["tool_call_id"] == "call_read_1"
                && message["content"].as_str().unwrap().contains("alpha")
        }));
    }

    #[tokio::test]
    async fn run_turn_waits_for_bash_approval_before_running_tool() {
        let arg = json!({ "command": "printf approved" }).to_string();
        let responses = vec![
            sse_response(vec![json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_bash_1",
                            "type": "function",
                            "function": { "name": "bash", "arguments": arg }
                        }]
                    }
                }]
            })]),
            sse_response(vec![
                json!({ "choices": [{ "delta": { "content": "bash complete" } }] }),
                json!({ "usage": { "prompt_tokens": 10, "completion_tokens": 2 }, "choices": [{ "delta": {} }] }),
            ]),
        ];
        let requests = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_chat_server(responses, requests.clone()).await;
        unsafe {
            std::env::set_var("MICROVIBE_TEST_API_KEY", "test-key");
        }

        let mut session = Session::new(test_config(base_url));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (approval_tx, mut approval_rx) = mpsc::unbounded_channel();
        let run = tokio::spawn(async move {
            session
                .agent
                .run_turn_with_approval("run bash", event_tx, approval_tx)
                .await
                .unwrap();
            session
        });

        let approval = tokio::time::timeout(std::time::Duration::from_secs(2), approval_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(approval.call.name, "bash");
        assert_eq!(
            requests.lock().unwrap().len(),
            1,
            "second model turn must wait for approval"
        );
        approval
            .respond_to
            .send(microvibe_protocol::ApprovalDecision::AllowOnce)
            .unwrap();

        let session = run.await.unwrap();
        assert!(
            session
                .agent
                .messages()
                .iter()
                .flat_map(|message| message.content.iter())
                .any(|block| matches!(
                    block,
                    ContentBlock::ToolResult(result) if result.output.contains("approved")
                ))
        );
        assert_eq!(requests.lock().unwrap().len(), 2);

        let mut saw_bash_result = false;
        while let Ok(event) = event_rx.try_recv() {
            if let AgentEvent::ToolCallCompleted { result } = event {
                saw_bash_result = result.name == "bash" && result.output.contains("approved");
            }
        }
        assert!(saw_bash_result);
    }

    #[tokio::test]
    async fn run_turn_waits_for_task_approval_before_running_tool() {
        let arg = json!({ "task": "Inspect sample.txt", "agent": "no-such-agent" }).to_string();
        let responses = vec![sse_response(vec![json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_task_1",
                        "type": "function",
                        "function": { "name": "task", "arguments": arg }
                    }]
                }
            }]
        })])];
        let requests = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_chat_server(responses, requests.clone()).await;
        unsafe {
            std::env::set_var("MICROVIBE_TEST_API_KEY", "test-key");
        }

        let mut session = Session::new(test_config(base_url));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (approval_tx, mut approval_rx) = mpsc::unbounded_channel();
        let run = tokio::spawn(async move {
            session
                .agent
                .run_turn_with_approval("delegate", event_tx, approval_tx)
                .await
                .unwrap();
            session
        });

        let approval = tokio::time::timeout(std::time::Duration::from_secs(2), approval_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(approval.call.name, "task");
        assert_eq!(
            requests.lock().unwrap().len(),
            1,
            "task must not execute before approval"
        );
        approval
            .respond_to
            .send(microvibe_protocol::ApprovalDecision::Deny)
            .unwrap();

        let session = run.await.unwrap();
        assert!(
            session
                .agent
                .messages()
                .iter()
                .flat_map(|message| message.content.iter())
                .any(|block| matches!(
                    block,
                    ContentBlock::ToolResult(result)
                        if result.name == "task"
                            && result.output == "Skipped: User cancelled the operation."
                ))
        );
        assert_eq!(requests.lock().unwrap().len(), 1);

        let mut saw_task_denied = false;
        while let Ok(event) = event_rx.try_recv() {
            if let AgentEvent::ToolCallCompleted { result } = event {
                saw_task_denied = result.name == "task"
                    && result.output == "Skipped: User cancelled the operation.";
            }
        }
        assert!(saw_task_denied);
    }

    #[tokio::test]
    async fn run_turn_auto_allows_explore_task() {
        let arg = json!({ "task": "Inspect sample.txt", "agent": "explore" }).to_string();
        let responses = vec![
            sse_response(vec![json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_task_1",
                            "type": "function",
                            "function": { "name": "task", "arguments": arg }
                        }]
                    }
                }]
            })]),
            json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "Subagent found alpha."
                    },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 4, "completion_tokens": 3, "total_tokens": 7 }
            })
            .to_string(),
            sse_response(vec![
                json!({ "choices": [{ "delta": { "content": "task complete" } }] }),
                json!({ "usage": { "prompt_tokens": 10, "completion_tokens": 2 }, "choices": [{ "delta": {} }] }),
            ]),
        ];
        let requests = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_chat_server(responses, requests.clone()).await;
        unsafe {
            std::env::set_var("MICROVIBE_TEST_API_KEY", "test-key");
        }

        let mut session = Session::new(test_config(base_url));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (approval_tx, mut approval_rx) = mpsc::unbounded_channel();
        let run = tokio::spawn(async move {
            session
                .agent
                .run_turn_with_approval("delegate", event_tx, approval_tx)
                .await
                .unwrap();
            session
        });

        let approval =
            tokio::time::timeout(std::time::Duration::from_millis(200), approval_rx.recv()).await;
        assert!(
            !matches!(approval, Ok(Some(_))),
            "explore task should use Vibe's default allowlist"
        );

        let session = run.await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 3);
        assert!(
            session
                .agent
                .messages()
                .iter()
                .flat_map(|message| message.content.iter())
                .any(|block| matches!(
                    block,
                    ContentBlock::ToolResult(result)
                        if result.name == "task"
                            && result.output.contains("response: Subagent found alpha.")
                            && result.output.contains("completed: True")
                ))
        );

        let mut saw_task_completed = false;
        while let Ok(event) = event_rx.try_recv() {
            if let AgentEvent::ToolCallCompleted { result } = event {
                saw_task_completed = result.name == "task"
                    && result.output.contains("response: Subagent found alpha.")
                    && result.output.contains("turns_used: 1");
            }
        }
        assert!(saw_task_completed);
    }

    #[tokio::test]
    async fn run_turn_executes_custom_subagent_task_after_approval() {
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("reader.toml"),
            r#"
agent_type = "subagent"
enabled_tools = ["read"]
"#,
        )
        .unwrap();

        let arg = json!({ "task": "Inspect sample.txt", "agent": "reader" }).to_string();
        let responses = vec![
            sse_response(vec![json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_custom_task_1",
                            "type": "function",
                            "function": { "name": "task", "arguments": arg }
                        }]
                    }
                }]
            })]),
            json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "Reader subagent finished."
                    },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 4, "completion_tokens": 3, "total_tokens": 7 }
            })
            .to_string(),
            sse_response(vec![
                json!({ "choices": [{ "delta": { "content": "task complete" } }] }),
                json!({ "usage": { "prompt_tokens": 10, "completion_tokens": 2 }, "choices": [{ "delta": {} }] }),
            ]),
        ];
        let requests = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_chat_server(responses, requests.clone()).await;
        unsafe {
            std::env::set_var("MICROVIBE_TEST_API_KEY", "test-key");
        }

        let mut config = test_config(base_url);
        config.agent_paths = vec![agents_dir];
        let mut session = Session::new(config);
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let (approval_tx, mut approval_rx) = mpsc::unbounded_channel();
        let run = tokio::spawn(async move {
            session
                .agent
                .run_turn_with_approval("delegate", event_tx, approval_tx)
                .await
                .unwrap();
            session
        });

        let approval = tokio::time::timeout(std::time::Duration::from_secs(2), approval_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(approval.call.name, "task");
        approval
            .respond_to
            .send(microvibe_protocol::ApprovalDecision::AllowOnce)
            .unwrap();

        let session = run.await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 3);
        let subagent_request: Value = serde_json::from_str(&requests.lock().unwrap()[1]).unwrap();
        assert_eq!(
            subagent_request["messages"][0]["content"],
            "You are the reader subagent. Return a concise answer."
        );
        let tools = subagent_request["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "read");
        assert!(
            session
                .agent
                .messages()
                .iter()
                .flat_map(|message| message.content.iter())
                .any(|block| matches!(
                    block,
                    ContentBlock::ToolResult(result)
                        if result.name == "task"
                            && result.output.contains("response: Reader subagent finished.")
                            && result.output.contains("completed: True")
                ))
        );
    }

    #[tokio::test]
    async fn read_sensitive_file_requires_approval_even_with_default_always_permission() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        tokio::fs::write(&path, "TOKEN=secret\n").await.unwrap();
        let arg = json!({ "file_path": path }).to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_chat_server(
            vec![sse_response(vec![json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_read_sensitive",
                            "type": "function",
                            "function": { "name": "read", "arguments": arg }
                        }]
                    }
                }]
            })])],
            requests.clone(),
        )
        .await;
        unsafe {
            std::env::set_var("MICROVIBE_TEST_API_KEY", "test-key");
        }

        let mut session = Session::new(test_config(base_url));
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let (approval_tx, mut approval_rx) = mpsc::unbounded_channel();
        let run = tokio::spawn(async move {
            session
                .agent
                .run_turn_with_approval("read env", event_tx, approval_tx)
                .await
                .unwrap();
            session
        });

        let approval = tokio::time::timeout(std::time::Duration::from_secs(2), approval_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(approval.call.name, "read");
        approval
            .respond_to
            .send(microvibe_protocol::ApprovalDecision::Deny)
            .unwrap();
        let session = run.await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert!(
            session
                .agent
                .messages()
                .iter()
                .flat_map(|message| message.content.iter())
                .any(|block| matches!(
                    block,
                    ContentBlock::ToolResult(result)
                        if result.name == "read"
                            && result.output == "Skipped: User cancelled the operation."
                ))
        );
    }

    #[tokio::test]
    async fn explicit_tool_permission_always_skips_approval() {
        let arg = json!({ "command": "printf configured" }).to_string();
        let responses = vec![
            sse_response(vec![json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_bash_always",
                            "type": "function",
                            "function": { "name": "bash", "arguments": arg }
                        }]
                    }
                }]
            })]),
            sse_response(vec![
                json!({ "choices": [{ "delta": { "content": "done" } }] }),
                json!({ "usage": { "prompt_tokens": 1, "completion_tokens": 1 }, "choices": [{ "delta": {} }] }),
            ]),
        ];
        let requests = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_chat_server(responses, requests.clone()).await;
        unsafe {
            std::env::set_var("MICROVIBE_TEST_API_KEY", "test-key");
        }

        let mut config = test_config(base_url);
        config.tools.insert(
            "bash".to_string(),
            ToolPermissionConfig {
                permission: Some("always".to_string()),
                ..ToolPermissionConfig::default()
            },
        );
        let mut session = Session::new(config);
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let (approval_tx, mut approval_rx) = mpsc::unbounded_channel();
        session
            .agent
            .run_turn_with_approval("run configured bash", event_tx, approval_tx)
            .await
            .unwrap();

        let approval =
            tokio::time::timeout(std::time::Duration::from_millis(200), approval_rx.recv()).await;
        assert!(!matches!(approval, Ok(Some(_))));
        assert_eq!(requests.lock().unwrap().len(), 2);
        assert!(
            session
                .agent
                .messages()
                .iter()
                .flat_map(|message| message.content.iter())
                .any(|block| matches!(
                    block,
                    ContentBlock::ToolResult(result)
                        if result.name == "bash" && result.output.contains("configured")
                ))
        );
    }

    #[test]
    fn default_agent_disables_exit_plan_tool() {
        let session = Session::new(test_config("http://127.0.0.1:1".to_string()));
        let tool_names = session
            .agent
            .tool_specs()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert!(!tool_names.contains(&"exit_plan_mode".to_string()));
    }

    #[test]
    fn custom_agent_from_agent_path_applies_enabled_tools() {
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("custom-read.toml"),
            r#"
display_name = "Custom Read"
description = "Read-only custom agent"
safety = "safe"
enabled_tools = ["read"]
"#,
        )
        .unwrap();

        let mut config = test_config("http://127.0.0.1:1".to_string());
        config.default_agent = "custom-read".to_string();
        config.agent_paths = vec![agents_dir];
        let session = Session::new(config);
        let tool_names = session
            .agent
            .tool_specs()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();

        assert_eq!(tool_names, vec!["read".to_string()]);
    }

    #[test]
    fn custom_agent_disabled_by_config_is_not_applied() {
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("custom-read.toml"),
            r#"
enabled_tools = ["read"]
"#,
        )
        .unwrap();

        let mut config = test_config("http://127.0.0.1:1".to_string());
        config.default_agent = "custom-read".to_string();
        config.agent_paths = vec![agents_dir];
        config.disabled_agents = vec!["custom-*".to_string()];
        let session = Session::new(config);
        let tool_names = session
            .agent
            .tool_specs()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();

        assert!(tool_names.contains(&"bash".to_string()));
        assert!(tool_names.contains(&"read".to_string()));
    }

    #[test]
    fn validates_builtin_agent_filter_diagnostics_like_vibe() {
        let mut config = test_config("http://127.0.0.1:1".to_string());
        config.default_agent = "plan".to_string();
        config.disabled_agents = vec!["plan".to_string()];

        let error = validate_agent_selection(&config, false)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "default_agent 'plan' is in 'disabled_agents' ['plan']. Remove 'plan' from 'disabled_agents', or set 'default_agent' to an enabled agent."
        );
        let error = validate_agent_selection(&config, true)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "Agent 'plan' is in 'disabled_agents' ['plan']. Remove 'plan' from 'disabled_agents', or select an enabled agent."
        );

        config.disabled_agents.clear();
        config.enabled_agents = vec!["default".to_string()];
        let error = validate_agent_selection(&config, false)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "default_agent 'plan' is not in 'enabled_agents' ['default']. Add 'plan' to 'enabled_agents', or set 'default_agent' to an enabled agent."
        );
        let error = validate_agent_selection(&config, true)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "Agent 'plan' is not in 'enabled_agents' ['default']. Add 'plan' to 'enabled_agents', or select an enabled agent."
        );
    }

    #[test]
    fn validates_explicit_agent_errors_like_vibe() {
        let mut config = test_config("http://127.0.0.1:1".to_string());
        config.default_agent = "nope".to_string();
        assert_eq!(
            validate_agent_selection(&config, true)
                .unwrap_err()
                .to_string(),
            "Agent 'nope' not found."
        );

        config.default_agent = "explore".to_string();
        assert_eq!(
            validate_agent_selection(&config, true)
                .unwrap_err()
                .to_string(),
            "Agent 'explore' is a subagent and cannot be used as the primary agent. Only agents of type 'agent' can be selected with --agent."
        );

        config.default_agent = "lean".to_string();
        assert_eq!(
            validate_agent_selection(&config, true)
                .unwrap_err()
                .to_string(),
            "Agent 'lean' requires installation. Run it once via --agent 'lean', or add it to 'installed_agents'."
        );
    }

    #[test]
    fn validates_custom_subagent_rejection_like_vibe() {
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(agents_dir.join("sub.toml"), "agent_type = \"subagent\"\n").unwrap();

        let mut config = test_config("http://127.0.0.1:1".to_string());
        config.default_agent = "sub".to_string();
        config.agent_paths = vec![agents_dir];
        assert_eq!(
            validate_agent_selection(&config, true)
                .unwrap_err()
                .to_string(),
            "Agent 'sub' is a subagent and cannot be used as the primary agent. Only agents of type 'agent' can be selected with --agent."
        );
    }

    #[test]
    fn primary_agent_order_includes_sorted_custom_agents() {
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("review-bot.toml"),
            r#"
display_name = "Review Bot"
agent_type = "agent"
"#,
        )
        .unwrap();
        std::fs::write(
            agents_dir.join("reader.toml"),
            r#"
agent_type = "subagent"
"#,
        )
        .unwrap();

        let mut config = test_config("http://127.0.0.1:1".to_string());
        config.agent_paths = vec![agents_dir];
        let order = primary_agent_order(&config);
        assert_eq!(
            order
                .iter()
                .map(|agent| (agent.name.as_str(), agent.display_name.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("default", "Default"),
                ("plan", "Plan"),
                ("accept-edits", "Accept Edits"),
                ("auto-approve", "Auto Approve"),
                ("review-bot", "Review Bot"),
            ]
        );
    }

    #[tokio::test]
    async fn auto_approve_agent_skips_bash_approval() {
        let arg = json!({ "command": "printf yolo" }).to_string();
        let responses = vec![
            sse_response(vec![json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_bash_auto",
                            "type": "function",
                            "function": { "name": "bash", "arguments": arg }
                        }]
                    }
                }]
            })]),
            sse_response(vec![
                json!({ "choices": [{ "delta": { "content": "done" } }] }),
                json!({ "usage": { "prompt_tokens": 1, "completion_tokens": 1 }, "choices": [{ "delta": {} }] }),
            ]),
        ];
        let requests = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_chat_server(responses, requests.clone()).await;
        unsafe {
            std::env::set_var("MICROVIBE_TEST_API_KEY", "test-key");
        }

        let mut config = test_config(base_url);
        config.default_agent = "auto-approve".to_string();
        let mut session = Session::new(config);
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let (approval_tx, mut approval_rx) = mpsc::unbounded_channel();
        session
            .agent
            .run_turn_with_approval("run auto bash", event_tx, approval_tx)
            .await
            .unwrap();

        let approval =
            tokio::time::timeout(std::time::Duration::from_millis(200), approval_rx.recv()).await;
        assert!(!matches!(approval, Ok(Some(_))));
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn custom_agent_tool_permission_skips_approval() {
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("custom-bash.toml"),
            r#"
[tools.bash]
permission = "always"
"#,
        )
        .unwrap();

        let arg = json!({ "command": "printf custom" }).to_string();
        let responses = vec![
            sse_response(vec![json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_bash_custom_agent",
                            "type": "function",
                            "function": { "name": "bash", "arguments": arg }
                        }]
                    }
                }]
            })]),
            sse_response(vec![
                json!({ "choices": [{ "delta": { "content": "done" } }] }),
                json!({ "usage": { "prompt_tokens": 1, "completion_tokens": 1 }, "choices": [{ "delta": {} }] }),
            ]),
        ];
        let requests = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_chat_server(responses, requests.clone()).await;
        unsafe {
            std::env::set_var("MICROVIBE_TEST_API_KEY", "test-key");
        }

        let mut config = test_config(base_url);
        config.default_agent = "custom-bash".to_string();
        config.agent_paths = vec![agents_dir];
        let mut session = Session::new(config);
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let (approval_tx, mut approval_rx) = mpsc::unbounded_channel();
        session
            .agent
            .run_turn_with_approval("run custom bash", event_tx, approval_tx)
            .await
            .unwrap();

        let approval =
            tokio::time::timeout(std::time::Duration::from_millis(200), approval_rx.recv()).await;
        assert!(!matches!(approval, Ok(Some(_))));
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn accept_edits_agent_auto_approves_file_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("created.txt");
        let arg = json!({ "path": &path, "content": "created" }).to_string();
        let responses = vec![
            sse_response(vec![json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_write_accept",
                            "type": "function",
                            "function": { "name": "write_file", "arguments": arg }
                        }]
                    }
                }]
            })]),
            sse_response(vec![
                json!({ "choices": [{ "delta": { "content": "done" } }] }),
                json!({ "usage": { "prompt_tokens": 1, "completion_tokens": 1 }, "choices": [{ "delta": {} }] }),
            ]),
        ];
        let requests = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_chat_server(responses, requests.clone()).await;
        unsafe {
            std::env::set_var("MICROVIBE_TEST_API_KEY", "test-key");
        }

        let mut config = test_config(base_url);
        config.default_agent = "accept-edits".to_string();
        let mut session = Session::new(config);
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let (approval_tx, mut approval_rx) = mpsc::unbounded_channel();
        session
            .agent
            .run_turn_with_approval("write file", event_tx, approval_tx)
            .await
            .unwrap();

        let approval =
            tokio::time::timeout(std::time::Duration::from_millis(200), approval_rx.recv()).await;
        assert!(!matches!(approval, Ok(Some(_))));
        assert_eq!(requests.lock().unwrap().len(), 2);
        assert_eq!(tokio::fs::read_to_string(path).await.unwrap(), "created");
    }

    #[tokio::test]
    async fn bash_allowlist_skips_approval() {
        let arg = json!({ "command": "printf configured" }).to_string();
        let responses = vec![
            sse_response(vec![json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_bash_allowlist",
                            "type": "function",
                            "function": { "name": "bash", "arguments": arg }
                        }]
                    }
                }]
            })]),
            sse_response(vec![
                json!({ "choices": [{ "delta": { "content": "done" } }] }),
                json!({ "usage": { "prompt_tokens": 1, "completion_tokens": 1 }, "choices": [{ "delta": {} }] }),
            ]),
        ];
        let requests = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_chat_server(responses, requests.clone()).await;
        unsafe {
            std::env::set_var("MICROVIBE_TEST_API_KEY", "test-key");
        }

        let mut config = test_config(base_url);
        config.tools.insert(
            "bash".to_string(),
            ToolPermissionConfig {
                allowlist: vec!["printf".to_string()],
                ..ToolPermissionConfig::default()
            },
        );
        let mut session = Session::new(config);
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let (approval_tx, mut approval_rx) = mpsc::unbounded_channel();
        session
            .agent
            .run_turn_with_approval("run configured bash", event_tx, approval_tx)
            .await
            .unwrap();

        let approval =
            tokio::time::timeout(std::time::Duration::from_millis(200), approval_rx.recv()).await;
        assert!(!matches!(approval, Ok(Some(_))));
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn file_tool_denylist_skips_without_prompting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blocked.txt");
        tokio::fs::write(&path, "blocked\n").await.unwrap();
        let arg = json!({ "file_path": &path }).to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_chat_server(
            vec![
                sse_response(vec![json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_read_denied",
                                "type": "function",
                                "function": { "name": "read", "arguments": arg }
                            }]
                        }
                    }]
                })]),
                sse_response(vec![
                    json!({ "choices": [{ "delta": { "content": "skipped" } }] }),
                    json!({ "usage": { "prompt_tokens": 1, "completion_tokens": 1 }, "choices": [{ "delta": {} }] }),
                ]),
            ],
            requests.clone(),
        )
        .await;
        unsafe {
            std::env::set_var("MICROVIBE_TEST_API_KEY", "test-key");
        }

        let mut config = test_config(base_url);
        config.tools.insert(
            "read".to_string(),
            ToolPermissionConfig {
                denylist: vec![path.canonicalize().unwrap().to_string_lossy().to_string()],
                ..ToolPermissionConfig::default()
            },
        );
        let mut session = Session::new(config);
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let (approval_tx, mut approval_rx) = mpsc::unbounded_channel();
        session
            .agent
            .run_turn_with_approval("read blocked", event_tx, approval_tx)
            .await
            .unwrap();

        let approval =
            tokio::time::timeout(std::time::Duration::from_millis(200), approval_rx.recv()).await;
        assert!(!matches!(approval, Ok(Some(_))));
        assert_eq!(requests.lock().unwrap().len(), 2);
        let tool_outputs = session
            .agent
            .messages()
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ContentBlock::ToolResult(result) => Some(result.output.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            tool_outputs
                .iter()
                .any(|output| output == "Tool 'read' is permanently disabled"),
            "{tool_outputs:#?}"
        );
    }

    #[tokio::test]
    async fn max_turns_stops_before_next_model_turn() {
        let file = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(file.path(), "alpha\n").await.unwrap();
        let arg = json!({ "file_path": file.path() }).to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_chat_server(
            vec![sse_response(vec![json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_read_1",
                            "type": "function",
                            "function": { "name": "read", "arguments": arg }
                        }]
                    }
                }]
            })])],
            requests.clone(),
        )
        .await;
        unsafe {
            std::env::set_var("MICROVIBE_TEST_API_KEY", "test-key");
        }

        let mut session = Session::new(test_config(base_url));
        let (tx, mut rx) = mpsc::unbounded_channel();
        session
            .agent
            .run_turn_with_limits(
                "read once",
                tx,
                RunLimits {
                    max_turns: Some(1),
                    max_tokens: None,
                    max_price: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(requests.lock().unwrap().len(), 1);
        let mut saw_stop = false;
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::AssistantDelta { text } = event {
                saw_stop = text == "<vibe_stop_event>Turn limit of 1 reached</vibe_stop_event>";
            }
        }
        assert!(saw_stop);
    }

    #[tokio::test]
    async fn max_tokens_stops_when_usage_exceeds_limit() {
        let file = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(file.path(), "alpha\n").await.unwrap();
        let arg = json!({ "file_path": file.path() }).to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_chat_server(
            vec![sse_response(vec![json!({
                "usage": { "prompt_tokens": 10, "completion_tokens": 2 },
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_read_1",
                            "type": "function",
                            "function": { "name": "read", "arguments": arg }
                        }]
                    }
                }]
            })])],
            requests,
        )
        .await;
        unsafe {
            std::env::set_var("MICROVIBE_TEST_API_KEY", "test-key");
        }

        let mut session = Session::new(test_config(base_url));
        let (tx, mut rx) = mpsc::unbounded_channel();
        session
            .agent
            .run_turn_with_limits(
                "spend",
                tx,
                RunLimits {
                    max_turns: None,
                    max_tokens: Some(5),
                    max_price: None,
                },
            )
            .await
            .unwrap();
        let mut saw_stop = false;
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::AssistantDelta { text } = event {
                saw_stop =
                    text == "<vibe_stop_event>Token limit exceeded: 12 > 5</vibe_stop_event>";
            }
        }
        assert!(saw_stop);
    }

    #[tokio::test]
    async fn max_price_stops_when_estimated_cost_exceeds_limit() {
        let file = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(file.path(), "alpha\n").await.unwrap();
        let arg = json!({ "file_path": file.path() }).to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_chat_server(
            vec![sse_response(vec![json!({
                "usage": { "prompt_tokens": 1_000_000, "completion_tokens": 1_000_000 },
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_read_1",
                            "type": "function",
                            "function": { "name": "read", "arguments": arg }
                        }]
                    }
                }]
            })])],
            requests,
        )
        .await;
        unsafe {
            std::env::set_var("MICROVIBE_TEST_API_KEY", "test-key");
        }

        let mut config = test_config(base_url);
        config.model.input_price = 1.5;
        config.model.output_price = 7.5;
        let mut session = Session::new(config);
        let (tx, mut rx) = mpsc::unbounded_channel();
        session
            .agent
            .run_turn_with_limits(
                "spend",
                tx,
                RunLimits {
                    max_turns: None,
                    max_tokens: None,
                    max_price: Some(1.0),
                },
            )
            .await
            .unwrap();
        let mut saw_stop = false;
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::AssistantDelta { text } = event {
                saw_stop = text
                    == "<vibe_stop_event>Price limit exceeded: $9.0000 > $1.00</vibe_stop_event>";
            }
        }
        assert!(saw_stop);
    }

    #[tokio::test]
    async fn before_tool_hook_rewrites_tool_input() {
        let first = tempfile::NamedTempFile::new().unwrap();
        let second = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(first.path(), "wrong\n").await.unwrap();
        tokio::fs::write(second.path(), "rewritten\n")
            .await
            .unwrap();
        let arg = json!({ "file_path": first.path() }).to_string();
        let responses = vec![
            sse_response(vec![json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_read_1",
                            "type": "function",
                            "function": { "name": "read", "arguments": arg }
                        }]
                    }
                }]
            })]),
            sse_response(vec![json!({
                "choices": [{ "delta": { "content": "done" } }],
                "usage": { "prompt_tokens": 10, "completion_tokens": 2 }
            })]),
        ];
        let requests = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_chat_server(responses, requests.clone()).await;
        unsafe {
            std::env::set_var("MICROVIBE_TEST_API_KEY", "test-key");
        }

        let mut session = Session::new(test_config(base_url));
        let payload = json!({"hook_specific_output": {"tool_input": {"file_path": second.path()}}});
        session.agent.hooks = vec![HookConfig {
            name: "rewrite-read".to_string(),
            hook_type: "before_tool".to_string(),
            command: printf_json_command(&payload),
            r#match: Some("read".to_string()),
            timeout: 60.0,
            strict: false,
            description: None,
        }];
        let (tx, mut rx) = mpsc::unbounded_channel();
        session.agent.run_turn("read", tx).await.unwrap();

        let mut saw_rewrite_event = false;
        let mut saw_rewritten_output = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::HookEnded {
                    hook_name, content, ..
                } => {
                    saw_rewrite_event = hook_name == "rewrite-read"
                        && content.as_deref() == Some("Rewrote tool_input for 'read'");
                }
                AgentEvent::ToolCallCompleted { result } => {
                    saw_rewritten_output = result.output.contains("rewritten");
                }
                _ => {}
            }
        }
        assert!(saw_rewrite_event);
        assert!(saw_rewritten_output);
        let request_bodies = requests.lock().unwrap();
        assert!(request_bodies.last().unwrap().contains("rewritten"));
        assert!(!request_bodies.last().unwrap().contains("wrong"));
    }

    #[tokio::test]
    async fn after_tool_hook_appends_context_to_tool_output() {
        let file = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(file.path(), "alpha\n").await.unwrap();
        let arg = json!({ "file_path": file.path() }).to_string();
        let responses = vec![
            sse_response(vec![json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_read_1",
                            "type": "function",
                            "function": { "name": "read", "arguments": arg }
                        }]
                    }
                }]
            })]),
            sse_response(vec![json!({
                "choices": [{ "delta": { "content": "done" } }],
                "usage": { "prompt_tokens": 10, "completion_tokens": 2 }
            })]),
        ];
        let requests = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_chat_server(responses, requests.clone()).await;
        unsafe {
            std::env::set_var("MICROVIBE_TEST_API_KEY", "test-key");
        }

        let mut session = Session::new(test_config(base_url));
        let payload = json!({"hook_specific_output": {"additional_context": "hook context"}});
        session.agent.hooks = vec![HookConfig {
            name: "append-read".to_string(),
            hook_type: "after_tool".to_string(),
            command: printf_json_command(&payload),
            r#match: Some("read".to_string()),
            timeout: 60.0,
            strict: false,
            description: None,
        }];
        let (tx, mut rx) = mpsc::unbounded_channel();
        session.agent.run_turn("read", tx).await.unwrap();

        let mut saw_append_event = false;
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::HookEnded {
                hook_name, content, ..
            } = event
            {
                saw_append_event = hook_name == "append-read"
                    && content.as_deref() == Some("Appended 12 chars to tool result");
            }
        }
        assert!(saw_append_event);
        let request_bodies = requests.lock().unwrap();
        assert!(request_bodies.last().unwrap().contains("alpha"));
        assert!(request_bodies.last().unwrap().contains("hook context"));
    }

    fn test_config(base_url: String) -> Config {
        Config {
            enable_experimental_hooks: false,
            active_model: None,
            default_agent: "default".to_string(),
            theme: None,
            autocopy_to_clipboard: true,
            voice_mode_enabled: false,
            bypass_tool_permissions: false,
            system_prompt_id: None,
            model: ModelConfig {
                provider: "test".to_string(),
                name: "test-model".to_string(),
                temperature: 0.0,
                max_context_tokens: 200_000,
                input_price: 1.5,
                output_price: 7.5,
            },
            models: Vec::new(),
            providers: BTreeMap::from([(
                "test".to_string(),
                ProviderConfig {
                    base_url,
                    api_key_env: "MICROVIBE_TEST_API_KEY".to_string(),
                    backend: "generic".to_string(),
                    browser_auth_base_url: None,
                    browser_auth_api_base_url: None,
                    wire_format: "openai_chat".to_string(),
                },
            )]),
            permissions: PermissionsConfig::default(),
            tools: BTreeMap::new(),
            enabled_tools: Vec::new(),
            disabled_tools: Vec::new(),
            agent_paths: Vec::new(),
            enabled_agents: Vec::new(),
            disabled_agents: Vec::new(),
            installed_agents: Vec::new(),
            mcp_servers: Vec::new(),
        }
    }

    fn printf_json_command(value: &Value) -> String {
        let raw = value.to_string().replace('\'', "'\\''");
        format!("printf '%s' '{raw}'")
    }

    fn sse_response(events: Vec<Value>) -> String {
        let mut body = String::new();
        for event in events {
            body.push_str("data: ");
            body.push_str(&event.to_string());
            body.push_str("\n\n");
        }
        body.push_str("data: [DONE]\n\n");
        body
    }

    async fn spawn_chat_server(
        responses: Vec<String>,
        requests: Arc<Mutex<Vec<String>>>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for response_body in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let body = read_http_body(&mut socket).await.unwrap();
                requests.lock().unwrap().push(body);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        format!("http://{addr}")
    }

    async fn read_http_body(socket: &mut tokio::net::TcpStream) -> std::io::Result<String> {
        let mut raw = Vec::new();
        let header_end = loop {
            let mut buf = [0; 1024];
            let n = socket.read(&mut buf).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "connection closed before headers",
                ));
            }
            raw.extend_from_slice(&buf[..n]);
            if let Some(pos) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let headers = String::from_utf8_lossy(&raw[..header_end]).to_ascii_lowercase();
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while raw.len() < header_end + content_length {
            let mut buf = vec![0; header_end + content_length - raw.len()];
            let n = socket.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
        }
        Ok(String::from_utf8_lossy(&raw[header_end..header_end + content_length]).to_string())
    }
}
