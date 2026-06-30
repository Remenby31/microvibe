use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use glob::Pattern;
use microvibe_config::McpServerConfig;
use microvibe_protocol::{ToolCall, ToolResult, ToolSpec};
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn run(&self, call: &ToolCall) -> ToolResult;
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RuntimeToolConfig {
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(default)]
    pub denylist: Vec<String>,
    #[serde(default)]
    pub sensitive_patterns: Vec<String>,
    pub max_output_bytes: Option<usize>,
    pub default_timeout: Option<u64>,
    pub default_max_matches: Option<u64>,
    pub exclude_patterns: Option<Vec<String>>,
    pub codeignore_file: Option<String>,
    pub max_read_bytes: Option<usize>,
    pub max_write_bytes: Option<usize>,
    pub create_parent_dirs: Option<bool>,
    pub max_content_bytes: Option<usize>,
    pub max_timeout: Option<u64>,
    pub user_agent: Option<String>,
    pub timeout: Option<u64>,
    pub model: Option<String>,
    pub max_todos: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct RuntimeToolConfigs {
    by_name: BTreeMap<String, RuntimeToolConfig>,
}

impl RuntimeToolConfigs {
    pub fn from_value(value: Value) -> Self {
        let by_name = serde_json::from_value(value).unwrap_or_default();
        Self { by_name }
    }

    fn get(&self, name: &str) -> RuntimeToolConfig {
        self.by_name.get(name).cloned().unwrap_or_default()
    }
}

fn default_subagents() -> Vec<SubagentProfile> {
    vec![SubagentProfile::explore()]
}

fn subagent_map(subagents: Vec<SubagentProfile>) -> BTreeMap<String, SubagentProfile> {
    subagents
        .into_iter()
        .map(|profile| (profile.name.clone(), profile))
        .collect()
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
    disabled: BTreeSet<String>,
    mcp_integrated: bool,
}

#[derive(Debug, Clone)]
pub struct SubagentProfile {
    pub name: String,
    pub system_prompt: String,
    pub enabled_tools: Vec<String>,
}

impl SubagentProfile {
    fn explore() -> Self {
        Self {
            name: "explore".to_string(),
            system_prompt: "You are the explore subagent. You can inspect the codebase read-only and return a concise answer.".to_string(),
            enabled_tools: vec!["grep".to_string(), "read".to_string()],
        }
    }
}

impl ToolRegistry {
    pub fn with_builtins() -> Self {
        Self::with_builtins_and_configs(RuntimeToolConfigs::default())
    }

    pub fn with_builtins_and_configs(configs: RuntimeToolConfigs) -> Self {
        Self::with_builtins_and_configs_and_subagents(configs, default_subagents())
    }

    pub fn with_builtins_and_configs_and_subagents(
        configs: RuntimeToolConfigs,
        subagents: Vec<SubagentProfile>,
    ) -> Self {
        let mut registry = Self::default();
        let todo_state = Arc::new(Mutex::new(Vec::new()));
        registry.insert(AskUserQuestionTool::new(configs.get("ask_user_question")));
        registry.insert(BashTool::new(configs.get("bash")));
        registry.insert(EditTool::new(configs.get("edit")));
        registry.insert(ExitPlanModeTool::new(configs.get("exit_plan_mode")));
        registry.insert(GrepTool::new(configs.get("grep")));
        registry.insert(ReadTool::new(configs.get("read")));
        registry.insert(SkillTool::new(configs.get("skill")));
        registry.insert(TaskTool::new(configs.get("task"), subagents));
        registry.insert(TodoTool {
            state: todo_state,
            config: configs.get("todo"),
        });
        registry.insert(WebFetchTool::new(configs.get("web_fetch")));
        registry.insert(WebSearchTool::default());
        registry.insert(WriteFileTool::new(configs.get("write_file")));
        registry
    }

    pub fn with_provider(provider_base_url: String, api_key_env: String, model: String) -> Self {
        Self::with_provider_and_configs(
            provider_base_url,
            api_key_env,
            model,
            RuntimeToolConfigs::default(),
        )
    }

    pub fn with_provider_and_configs(
        provider_base_url: String,
        api_key_env: String,
        model: String,
        configs: RuntimeToolConfigs,
    ) -> Self {
        Self::with_provider_configs_and_subagents(
            provider_base_url,
            api_key_env,
            model,
            configs,
            default_subagents(),
        )
    }

    pub fn with_provider_configs_and_subagents(
        provider_base_url: String,
        api_key_env: String,
        model: String,
        configs: RuntimeToolConfigs,
        subagents: Vec<SubagentProfile>,
    ) -> Self {
        let mut registry =
            Self::with_builtins_and_configs_and_subagents(configs.clone(), subagents.clone());
        registry.replace_provider_tools(provider_base_url, api_key_env, model, &configs, subagents);
        registry
    }

    fn replace_provider_tools(
        &mut self,
        provider_base_url: String,
        api_key_env: String,
        model: String,
        configs: &RuntimeToolConfigs,
        subagents: Vec<SubagentProfile>,
    ) {
        self.tools
            .retain(|tool| !matches!(tool.spec().name.as_str(), "task" | "web_search"));
        self.insert(TaskTool {
            provider_base_url: Some(provider_base_url.clone()),
            api_key_env: Some(api_key_env.clone()),
            model: Some(model),
            subagents: subagent_map(subagents),
        });
        self.insert(WebSearchTool {
            provider_base_url: Some(provider_base_url),
            api_key_env: Some(api_key_env),
            config: configs.get("web_search"),
        });
    }

    pub fn insert<T: Tool + 'static>(&mut self, tool: T) {
        self.tools.push(Box::new(tool));
        self.tools.sort_by_key(|tool| tool.spec().name);
    }

    pub fn replace<T: Tool + 'static>(&mut self, tool: T) {
        let name = tool.spec().name;
        self.tools.retain(|existing| existing.spec().name != name);
        self.insert(tool);
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .map(|tool| tool.spec())
            .filter(|spec| !self.disabled.contains(&spec.name))
            .collect()
    }

    pub fn disable_tools<I, S>(&mut self, names: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.disabled
            .extend(names.into_iter().map(|name| name.as_ref().to_string()));
    }

    pub fn apply_filters(&mut self, enabled_patterns: &[String], disabled_patterns: &[String]) {
        if !enabled_patterns.is_empty() {
            let disabled = self
                .tools
                .iter()
                .map(|tool| tool.spec().name)
                .filter(|name| !name_matches(name, enabled_patterns))
                .collect::<Vec<_>>();
            self.disable_tools(disabled);
        } else if !disabled_patterns.is_empty() {
            let disabled = self
                .tools
                .iter()
                .map(|tool| tool.spec().name)
                .filter(|name| name_matches(name, disabled_patterns))
                .collect::<Vec<_>>();
            self.disable_tools(disabled);
        }
    }

    pub fn is_enabled(&self, name: &str) -> bool {
        !self.disabled.contains(name) && self.tools.iter().any(|tool| tool.spec().name == name)
    }

    pub async fn run(&self, call: &ToolCall) -> ToolResult {
        if self.disabled.contains(&call.name) {
            return unknown_tool_result(call);
        }
        match self.tools.iter().find(|tool| tool.spec().name == call.name) {
            Some(tool) => tool.run(call).await,
            None => unknown_tool_result(call),
        }
    }

    pub async fn integrate_mcp_servers(&mut self, servers: &[McpServerConfig]) {
        if self.mcp_integrated {
            return;
        }
        self.mcp_integrated = true;
        for server in servers {
            if server.transport != "stdio" {
                continue;
            }
            let Ok(remote_tools) = discover_stdio_mcp_tools(server).await else {
                continue;
            };
            for remote in remote_tools {
                let published_name = format!("{}_{}", server.name, remote.name);
                let disabled = server.disabled
                    || server
                        .disabled_tools
                        .iter()
                        .any(|name| name == &remote.name);
                self.insert(McpStdioTool {
                    name: published_name.clone(),
                    description: format!("[{}] {}", server.name, remote.description),
                    input_schema: remote.input_schema,
                    server_name: server.name.clone(),
                    remote_name: remote.name,
                    command: mcp_argv(server).unwrap_or_default(),
                    env: server.env.clone(),
                    cwd: server.cwd.clone(),
                    startup_timeout_sec: server.startup_timeout_sec,
                    tool_timeout_sec: server.tool_timeout_sec,
                });
                if disabled {
                    self.disabled.insert(published_name);
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
struct RemoteMcpTool {
    name: String,
    description: String,
    input_schema: Value,
}

struct McpStdioTool {
    name: String,
    description: String,
    input_schema: Value,
    server_name: String,
    remote_name: String,
    command: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<String>,
    startup_timeout_sec: Option<f64>,
    tool_timeout_sec: Option<f64>,
}

#[async_trait]
impl Tool for McpStdioTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
        }
    }

    async fn run(&self, call: &ToolCall) -> ToolResult {
        let output = call_stdio_mcp_tool(self, &call.arguments).await;
        tool_result(call, output)
    }
}

async fn discover_stdio_mcp_tools(server: &McpServerConfig) -> Result<Vec<RemoteMcpTool>> {
    let Some(argv) = mcp_argv(server) else {
        return Ok(Vec::new());
    };
    let timeout = mcp_timeout(server.startup_timeout_sec, 1500);
    tokio::time::timeout(timeout, discover_stdio_mcp_tools_inner(server, argv)).await?
}

async fn discover_stdio_mcp_tools_inner(
    server: &McpServerConfig,
    argv: Vec<String>,
) -> Result<Vec<RemoteMcpTool>> {
    let (_child, mut stdin, mut stdout) = start_stdio_mcp(server, &argv).await?;
    mcp_initialize(&mut stdin, &mut stdout).await?;
    write_mcp_message(
        &mut stdin,
        json!({"method": "tools/list", "jsonrpc": "2.0", "id": 1}),
    )
    .await?;
    let response = read_mcp_response(&mut stdout, 1).await?;
    Ok(parse_remote_mcp_tools(response))
}

async fn call_stdio_mcp_tool(tool: &McpStdioTool, arguments: &Value) -> Result<String> {
    let server = McpServerConfig {
        name: tool.server_name.clone(),
        transport: "stdio".to_string(),
        disabled: false,
        disabled_tools: Vec::new(),
        command: Some(toml::Value::Array(
            tool.command
                .iter()
                .cloned()
                .map(toml::Value::String)
                .collect(),
        )),
        args: Vec::new(),
        env: tool.env.clone(),
        cwd: tool.cwd.clone(),
        startup_timeout_sec: tool.startup_timeout_sec,
        tool_timeout_sec: tool.tool_timeout_sec,
        url: None,
    };
    let timeout = mcp_timeout(tool.tool_timeout_sec.or(tool.startup_timeout_sec), 60_000);
    tokio::time::timeout(timeout, call_stdio_mcp_tool_inner(&server, tool, arguments)).await?
}

async fn call_stdio_mcp_tool_inner(
    server: &McpServerConfig,
    tool: &McpStdioTool,
    arguments: &Value,
) -> Result<String> {
    let (_child, mut stdin, mut stdout) = start_stdio_mcp(server, &tool.command).await?;
    mcp_initialize(&mut stdin, &mut stdout).await?;
    write_mcp_message(
        &mut stdin,
        json!({
            "method": "tools/call",
            "params": {
                "name": tool.remote_name,
                "arguments": arguments,
            },
            "jsonrpc": "2.0",
            "id": 1
        }),
    )
    .await?;
    let response = read_mcp_response(&mut stdout, 1).await?;
    let (ok, text, structured) = parse_mcp_call_result(&response);
    let server_text = format!("stdio:{}", tool.command.join(" "));
    Ok(format!(
        "ok: {}\nserver: {}\ntool: {}\ntext: {}\nstructured: {}",
        if ok { "True" } else { "False" },
        server_text,
        tool.remote_name,
        text.unwrap_or_else(|| "None".to_string()),
        structured.unwrap_or_else(|| "None".to_string())
    ))
}

async fn start_stdio_mcp(
    server: &McpServerConfig,
    argv: &[String],
) -> Result<(
    tokio::process::Child,
    tokio::process::ChildStdin,
    tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
)> {
    if argv.is_empty() {
        bail!("MCP stdio command is empty");
    }
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    if let Some(cwd) = &server.cwd {
        command.current_dir(cwd);
    }
    command.envs(&server.env);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn()?;
    let stdin = child.stdin.take().context("MCP stdin is not piped")?;
    let stdout = child.stdout.take().context("MCP stdout is not piped")?;
    Ok((child, stdin, BufReader::new(stdout).lines()))
}

async fn mcp_initialize(
    stdin: &mut tokio::process::ChildStdin,
    stdout: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
) -> Result<()> {
    write_mcp_message(
        stdin,
        json!({
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "mcp", "version": "0.1.0"}
            },
            "jsonrpc": "2.0",
            "id": 0
        }),
    )
    .await?;
    read_mcp_response(stdout, 0).await?;
    write_mcp_message(
        stdin,
        json!({"method": "notifications/initialized", "jsonrpc": "2.0"}),
    )
    .await
}

fn mcp_argv(server: &McpServerConfig) -> Option<Vec<String>> {
    let mut argv = match server.command.as_ref()? {
        toml::Value::String(command) => shlex::split(command).unwrap_or_default(),
        toml::Value::Array(items) => items
            .iter()
            .filter_map(|item| item.as_str().map(ToString::to_string))
            .collect(),
        _ => Vec::new(),
    };
    argv.extend(server.args.iter().cloned());
    Some(argv)
}

fn mcp_timeout(seconds: Option<f64>, default_ms: u64) -> Duration {
    Duration::from_millis(
        seconds
            .map(|seconds| (seconds * 1000.0).round().clamp(250.0, 300_000.0) as u64)
            .unwrap_or(default_ms),
    )
}

async fn write_mcp_message(stdin: &mut tokio::process::ChildStdin, message: Value) -> Result<()> {
    stdin.write_all(message.to_string().as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_mcp_response(
    stdout: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    expected_id: i64,
) -> Result<Value> {
    while let Some(line) = stdout.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line)?;
        if value.get("id").and_then(Value::as_i64) == Some(expected_id) {
            return Ok(value);
        }
    }
    bail!("MCP server closed before response {expected_id}");
}

fn parse_remote_mcp_tools(response: Value) -> Vec<RemoteMcpTool> {
    response
        .get("result")
        .and_then(|result| result.get("tools"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            Some(RemoteMcpTool {
                name: tool.get("name")?.as_str()?.to_string(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                input_schema: tool
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            })
        })
        .collect()
}

fn parse_mcp_call_result(response: &Value) -> (bool, Option<String>, Option<String>) {
    let result = response.get("result").unwrap_or(&Value::Null);
    let ok = !result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let structured = result
        .get("structuredContent")
        .map(|value| value.to_string());
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|text| !text.is_empty());
    (ok, text, structured)
}

pub fn builtin_result_schemas() -> Vec<Value> {
    vec![
        result_schema(
            "ask_user_question",
            &[result_field(
                "answers",
                "array",
                "List of answers",
                true,
                None,
            )],
        ),
        result_schema(
            "bash",
            &[
                result_field("command", "string", "", true, None),
                result_field("stdout", "string", "", true, None),
                result_field("stderr", "string", "", true, None),
                result_field("returncode", "integer", "", true, None),
            ],
        ),
        result_schema(
            "edit",
            &[
                result_field("file", "string", "", true, None),
                result_field("message", "string", "", true, None),
                result_field("old_string", "string", "", true, None),
                result_field("new_string", "string", "", true, None),
            ],
        ),
        result_schema(
            "exit_plan_mode",
            &[
                result_field("switched", "boolean", "", true, None),
                result_field("message", "string", "", true, None),
            ],
        ),
        result_schema(
            "grep",
            &[
                result_field("matches", "string", "", true, None),
                result_field("match_count", "integer", "", true, None),
                result_field(
                    "was_truncated",
                    "boolean",
                    "True if output was cut short by max_matches or max_output_bytes.",
                    true,
                    None,
                ),
            ],
        ),
        result_schema(
            "read",
            &[
                result_field("file_path", "string", "", true, None),
                result_field("content", "string", "", true, None),
                result_field("num_lines", "integer", "", true, None),
                result_field("start_line", "integer", "", true, None),
                result_field("requested_offset", "integer|null", "", false, None),
                result_field("requested_limit", "integer", "", false, Some(json!(2000))),
                result_field("total_lines", "integer|null", "", false, None),
                result_field("was_truncated", "boolean", "", false, Some(json!(false))),
            ],
        ),
        result_schema(
            "skill",
            &[
                result_field("name", "string", "The name of the loaded skill", true, None),
                result_field(
                    "content",
                    "string",
                    "The full skill content block",
                    true,
                    None,
                ),
                result_field(
                    "skill_dir",
                    "string|null",
                    "Absolute path to the skill directory when available",
                    false,
                    None,
                ),
            ],
        ),
        result_schema(
            "task",
            &[
                result_field(
                    "response",
                    "string",
                    "The accumulated response from the subagent",
                    true,
                    None,
                ),
                result_field(
                    "turns_used",
                    "integer",
                    "Number of turns the subagent used",
                    true,
                    None,
                ),
                result_field(
                    "completed",
                    "boolean",
                    "Whether the task completed normally",
                    true,
                    None,
                ),
            ],
        ),
        result_schema(
            "todo",
            &[
                result_field("message", "string", "", true, None),
                result_field("todos", "array", "", true, None),
                result_field("total_count", "integer", "", true, None),
            ],
        ),
        result_schema(
            "web_fetch",
            &[
                result_field("url", "string", "", true, None),
                result_field("content", "string", "", true, None),
                result_field("content_type", "string", "", true, None),
                result_field("was_truncated", "boolean", "", false, Some(json!(false))),
            ],
        ),
        result_schema(
            "web_search",
            &[
                result_field("query", "string", "", true, None),
                result_field("answer", "string", "", true, None),
                result_field("sources", "array", "", false, Some(json!([]))),
            ],
        ),
        result_schema(
            "write_file",
            &[
                result_field("path", "string", "", true, None),
                result_field("bytes_written", "integer", "", true, None),
                result_field("content", "string", "", true, None),
            ],
        ),
    ]
}

pub fn builtin_tool_permissions() -> Vec<Value> {
    [
        ("ask_user_question", "ALWAYS"),
        ("bash", "ASK"),
        ("edit", "ASK"),
        ("exit_plan_mode", "ALWAYS"),
        ("grep", "ALWAYS"),
        ("read", "ALWAYS"),
        ("skill", "ALWAYS"),
        ("task", "ASK"),
        ("todo", "ALWAYS"),
        ("web_fetch", "ASK"),
        ("web_search", "ASK"),
        ("write_file", "ASK"),
    ]
    .into_iter()
    .map(|(name, permission)| {
        json!({
            "name": name,
            "permission": permission,
        })
    })
    .collect()
}

pub fn builtin_tool_config_schemas() -> Vec<Value> {
    vec![
        config_schema("ask_user_question", &[]),
        config_schema(
            "bash",
            &[
                config_field(
                    "max_output_bytes",
                    "integer",
                    "Maximum bytes to capture from stdout and stderr.",
                    false,
                    Some(json!(16_000)),
                    None,
                ),
                config_field(
                    "default_timeout",
                    "integer",
                    "Default timeout for commands in seconds.",
                    false,
                    Some(json!(300)),
                    None,
                ),
                config_field(
                    "allowlist",
                    "array",
                    "Command prefixes that are automatically allowed",
                    false,
                    None,
                    Some("_get_default_allowlist"),
                ),
                config_field(
                    "denylist",
                    "array",
                    "Command prefixes that are automatically denied",
                    false,
                    None,
                    Some("_get_default_denylist"),
                ),
                config_field(
                    "denylist_standalone",
                    "array",
                    "Commands that are denied only when run without arguments",
                    false,
                    None,
                    Some("_get_default_denylist_standalone"),
                ),
                config_field(
                    "sensitive_patterns",
                    "array",
                    "Command prefixes that always ASK regardless of arity approval.",
                    false,
                    Some(json!(["sudo"])),
                    None,
                ),
            ],
        ),
        config_schema(
            "edit",
            &[sensitive_patterns_config_field(
                "File patterns that trigger ASK even when permission is ALWAYS.",
            )],
        ),
        config_schema("exit_plan_mode", &[]),
        config_schema(
            "grep",
            &[
                sensitive_patterns_config_field(
                    "File patterns that trigger ASK even when permission is ALWAYS.",
                ),
                config_field(
                    "max_output_bytes",
                    "integer",
                    "Hard cap for the total size of matched lines.",
                    false,
                    Some(json!(64_000)),
                    None,
                ),
                config_field(
                    "default_max_matches",
                    "integer",
                    "Default maximum number of matches to return.",
                    false,
                    Some(json!(100)),
                    None,
                ),
                config_field(
                    "default_timeout",
                    "integer",
                    "Default timeout for the search command in seconds.",
                    false,
                    Some(json!(60)),
                    None,
                ),
                config_field(
                    "exclude_patterns",
                    "array",
                    "List of glob patterns to exclude from search (dirs should end with /).",
                    false,
                    Some(json!([
                        ".venv/",
                        "venv/",
                        ".env/",
                        "env/",
                        "node_modules/",
                        ".git/",
                        "__pycache__/",
                        ".pytest_cache/",
                        ".mypy_cache/",
                        ".tox/",
                        ".nox/",
                        ".coverage/",
                        "htmlcov/",
                        "dist/",
                        "build/",
                        ".idea/",
                        ".vscode/",
                        "*.egg-info",
                        "*.pyc",
                        "*.pyo",
                        "*.pyd",
                        ".DS_Store",
                        "Thumbs.db"
                    ])),
                    None,
                ),
                config_field(
                    "codeignore_file",
                    "string",
                    "Name of the file to read for additional exclusion patterns.",
                    false,
                    Some(json!(".vibeignore")),
                    None,
                ),
            ],
        ),
        config_schema(
            "read",
            &[
                sensitive_patterns_config_field(
                    "File patterns that trigger ASK even when permission is ALWAYS.",
                ),
                config_field(
                    "max_read_bytes",
                    "integer",
                    "Maximum selected/output bytes to return in one call.",
                    false,
                    Some(json!(51_200)),
                    None,
                ),
            ],
        ),
        config_schema("skill", &[]),
        config_schema(
            "task",
            &[config_field(
                "allowlist",
                "array",
                "",
                false,
                Some(json!(["explore"])),
                None,
            )],
        ),
        config_schema(
            "todo",
            &[config_field(
                "max_todos",
                "integer",
                "",
                false,
                Some(json!(100)),
                None,
            )],
        ),
        config_schema(
            "web_fetch",
            &[
                config_field(
                    "default_timeout",
                    "integer",
                    "Default timeout in seconds.",
                    false,
                    Some(json!(30)),
                    None,
                ),
                config_field(
                    "max_timeout",
                    "integer",
                    "Maximum allowed timeout.",
                    false,
                    Some(json!(120)),
                    None,
                ),
                config_field(
                    "max_content_bytes",
                    "integer",
                    "Maximum content size in bytes returned to the model.",
                    false,
                    Some(json!(120_000)),
                    None,
                ),
                config_field(
                    "user_agent",
                    "string",
                    "User agent string for requests.",
                    false,
                    Some(json!(
                        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                         (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
                    )),
                    None,
                ),
            ],
        ),
        config_schema(
            "web_search",
            &[
                config_field(
                    "timeout",
                    "integer",
                    "HTTP timeout in seconds.",
                    false,
                    Some(json!(120)),
                    None,
                ),
                config_field(
                    "model",
                    "string",
                    "Mistral model to use for web search.",
                    false,
                    Some(json!("mistral-vibe-cli-with-tools")),
                    None,
                ),
            ],
        ),
        config_schema(
            "write_file",
            &[
                sensitive_patterns_config_field(
                    "File patterns that trigger ASK even when permission is ALWAYS.",
                ),
                config_field(
                    "max_write_bytes",
                    "integer",
                    "",
                    false,
                    Some(json!(64_000)),
                    None,
                ),
                config_field(
                    "create_parent_dirs",
                    "boolean",
                    "",
                    false,
                    Some(json!(true)),
                    None,
                ),
            ],
        ),
    ]
}

pub fn tool_requires_approval(name: &str) -> bool {
    matches!(
        name,
        "bash" | "edit" | "task" | "web_fetch" | "web_search" | "write_file"
    )
}

pub fn tool_call_requires_approval(call: &ToolCall) -> bool {
    if call.name == "task" {
        let agent = call
            .arguments
            .get("agent")
            .and_then(Value::as_str)
            .unwrap_or("explore");
        return agent != "explore";
    }
    tool_requires_approval(&call.name)
}

fn config_schema(name: &str, fields: &[Value]) -> Value {
    json!({
        "name": name,
        "fields": fields,
    })
}

fn sensitive_patterns_config_field(description: &str) -> Value {
    config_field(
        "sensitive_patterns",
        "array",
        description,
        false,
        Some(json!(["**/.env", "**/.env.*"])),
        None,
    )
}

fn config_field(
    name: &str,
    json_type: &str,
    description: &str,
    required: bool,
    default: Option<Value>,
    default_factory: Option<&str>,
) -> Value {
    let mut field = json!({
        "name": name,
        "json_type": parse_result_type(json_type),
        "description": description,
        "required": required,
    });
    if let Some(default) = default {
        field["default"] = default;
    }
    if let Some(default_factory) = default_factory {
        field["default_factory"] = json!(default_factory);
    }
    field
}

fn result_schema(name: &str, fields: &[Value]) -> Value {
    json!({
        "name": name,
        "fields": fields,
    })
}

fn result_field(
    name: &str,
    json_type: &str,
    description: &str,
    required: bool,
    default: Option<Value>,
) -> Value {
    let mut field = json!({
        "name": name,
        "json_type": parse_result_type(json_type),
        "description": description,
        "required": required,
    });
    if let Some(default) = default {
        field["default"] = default;
    }
    field
}

fn parse_result_type(raw: &str) -> Value {
    if let Some(base) = raw.strip_suffix("|null") {
        json!([base, "null"])
    } else {
        json!(raw)
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

fn unknown_tool_result(call: &ToolCall) -> ToolResult {
    ToolResult {
        call_id: call.id.clone(),
        name: call.name.clone(),
        output: format!(
            "<tool_error>{}: Unknown tool '{}'</tool_error>",
            call.name, call.name
        ),
        success: false,
    }
}

macro_rules! simple_tool {
    ($type_name:ident, $name:literal, $description:literal, $schema:expr, $run_fn:ident) => {
        struct $type_name {
            config: RuntimeToolConfig,
        }

        impl $type_name {
            fn new(config: RuntimeToolConfig) -> Self {
                Self { config }
            }
        }

        impl Default for $type_name {
            fn default() -> Self {
                Self::new(RuntimeToolConfig::default())
            }
        }

        #[async_trait]
        impl Tool for $type_name {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: $name.to_string(),
                    description: $description.to_string(),
                    input_schema: $schema,
                }
            }

            async fn run(&self, call: &ToolCall) -> ToolResult {
                tool_result(call, $run_fn(call, &self.config).await)
            }
        }
    };
}

simple_tool!(
    AskUserQuestionTool,
    "ask_user_question",
    "Ask the user one or more questions and wait for their responses. Each question has 2-4 choices plus an automatic 'Other' option for free text. Use this to gather preferences, clarify requirements, or get decisions.",
    json!({
        "type": "object",
        "properties": {
            "questions": { "type": "array", "description": "Questions to ask (1-4). Displayed as tabs if multiple." },
            "footer_note": { "type": ["string", "null"], "description": "Optional subtle note displayed at the bottom of the question widget." }
        },
        "required": ["questions"]
    }),
    ask_user_question
);

simple_tool!(
    BashTool,
    "bash",
    "Run a one-off bash command and capture its output.",
    json!({
        "type": "object",
        "properties": {
            "command": { "type": "string" },
            "timeout": { "type": ["integer", "null"], "description": "Override the default command timeout." }
        },
        "required": ["command"]
    }),
    bash
);

simple_tool!(
    EditTool,
    "edit",
    "Perform exact string replacements in files. Supports single or bulk (replace_all) substitutions with atomic, concurrent-safe writes.",
    json!({
        "type": "object",
        "properties": {
            "file_path": { "type": "string", "description": "The absolute path to the file to modify" },
            "old_string": { "type": "string", "description": "The text to replace" },
            "new_string": { "type": "string", "description": "The text to replace it with (must be different from old_string)" },
            "replace_all": { "type": "boolean", "default": false, "description": "Replace all occurrences of old_string (default false)" }
        },
        "required": ["file_path", "old_string", "new_string"]
    }),
    edit
);

simple_tool!(
    ExitPlanModeTool,
    "exit_plan_mode",
    "Signal that your plan is complete and you are ready to start implementing. This will ask the user to confirm switching from plan mode to accept-edits mode. Only use this tool when you have finished writing your plan to the plan file and are ready for user approval to begin implementation.",
    json!({ "type": "object", "properties": {}, "required": [] }),
    exit_plan_mode
);

simple_tool!(
    GrepTool,
    "grep",
    "Recursively search files for a regex pattern using ripgrep (rg) or grep. Respects .gitignore and .codeignore files by default when using ripgrep.",
    json!({
        "type": "object",
        "properties": {
            "pattern": { "type": "string" },
            "path": { "type": "string", "default": "." },
            "max_matches": { "type": ["integer", "null"], "description": "Override the default maximum number of matches." },
            "use_default_ignore": { "type": "boolean", "default": true, "description": "Whether to respect .gitignore and .ignore files." }
        },
        "required": ["pattern"]
    }),
    grep
);

simple_tool!(
    ReadTool,
    "read",
    "Read a text file with line numbers. Results are formatted with line number prefixes for easy reference.",
    json!({
        "type": "object",
        "properties": {
            "file_path": { "type": "string", "description": "The absolute path to the file to read." },
            "offset": { "type": ["integer", "null"], "description": "The line number to start reading from (1-indexed). Only provide if the file is too large to read at once." },
            "limit": { "type": "integer", "default": 2000, "description": "The number of lines to read. Lower it to read a smaller portion of a large file." }
        },
        "required": ["file_path"]
    }),
    read
);

simple_tool!(
    SkillTool,
    "skill",
    "Load a specialized skill that provides domain-specific instructions and workflows. When you recognize that a task matches one of the available skills listed in your system prompt, use this tool to load the full skill instructions. The skill will inject detailed instructions, workflows, and access to bundled resources (scripts, references, templates) into the conversation context.",
    json!({
        "type": "object",
        "properties": { "name": { "type": "string", "description": "The name of the skill to load from available_skills" } },
        "required": ["name"]
    }),
    skill
);

struct TaskTool {
    provider_base_url: Option<String>,
    api_key_env: Option<String>,
    model: Option<String>,
    subagents: BTreeMap<String, SubagentProfile>,
}

impl Default for TaskTool {
    fn default() -> Self {
        Self::new(RuntimeToolConfig::default(), default_subagents())
    }
}

impl TaskTool {
    fn new(_config: RuntimeToolConfig, subagents: Vec<SubagentProfile>) -> Self {
        Self {
            provider_base_url: None,
            api_key_env: None,
            model: None,
            subagents: subagent_map(subagents),
        }
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task".to_string(),
            description: "Delegate a task to a subagent for independent execution. Useful for exploration, research, or parallel work that doesn't require user interaction. The subagent runs in-memory and saves interaction logs.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "The task to delegate to the subagent" },
                    "agent": { "type": "string", "default": "explore", "description": "Name of the agent profile to use (must be a subagent)" }
                },
                "required": ["task"]
            }),
        }
    }

    async fn run(&self, call: &ToolCall) -> ToolResult {
        tool_result(call, self.run_task(call).await)
    }
}

simple_tool!(
    WebFetchTool,
    "web_fetch",
    "Fetch content from a URL. Converts HTML to markdown for readability.",
    json!({
        "type": "object",
        "properties": {
            "url": { "type": "string", "description": "URL to fetch (http/https)" },
            "timeout": { "type": ["integer", "null"], "description": "Timeout in seconds (max 120)" }
        },
        "required": ["url"]
    }),
    webfetch
);

simple_tool!(
    WriteFileTool,
    "write_file",
    "Create a UTF-8 file. Fails if the file already exists; use edit to modify.",
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "content": { "type": "string" }
        },
        "required": ["path", "content"]
    }),
    write_file
);

async fn ask_user_question(call: &ToolCall, _config: &RuntimeToolConfig) -> Result<String> {
    let questions = required(&call.arguments, "questions")?;
    Ok(json!({
        "cancelled": true,
        "answers": [],
        "message": "ask_user_question requires an interactive TUI callback",
        "questions": questions,
    })
    .to_string())
}

async fn bash(call: &ToolCall, config: &RuntimeToolConfig) -> Result<String> {
    let command = required_str(&call.arguments, "command")?;
    let timeout_secs = call
        .arguments
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| config.default_timeout.unwrap_or(300));
    let max_output_bytes = config.max_output_bytes.unwrap_or(16_000);
    let child = Command::new("bash")
        .arg("-lc")
        .arg(command)
        .env("TERM", "dumb")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to execute {command}"))?;
    let output = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
        .await
        .context("command timed out")??;
    let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();
    truncate_string_bytes(&mut stdout, max_output_bytes);
    truncate_string_bytes(&mut stderr, max_output_bytes.saturating_sub(stdout.len()));
    Ok(format!(
        "command: {command}\nstdout: {}\nstderr: {}\nreturncode: {}",
        stdout,
        stderr,
        output.status.code().unwrap_or(-1),
    ))
}

async fn edit(call: &ToolCall, _config: &RuntimeToolConfig) -> Result<String> {
    let path = resolve_edit_path(required_str(&call.arguments, "file_path")?)?;
    let old = required_str(&call.arguments, "old_string")?;
    let new = required_str(&call.arguments, "new_string")?;
    if old.is_empty() {
        bail!("old_string cannot be empty. Use write_file to create new files.");
    }
    if old == new {
        bail!("No changes to make — old_string and new_string are identical");
    }
    let replace_all = call
        .arguments
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|error| map_read_error(&path, error, "edit"))?;
    let count = content.matches(old).count();
    if count == 0 {
        bail!("String to replace not found in file.\nString: {old}");
    }
    if count > 1 && !replace_all {
        bail!(
            "Found {count} matches of the string to replace, but replace_all is false. To replace all occurrences, set replace_all to true. To replace only one occurrence, please provide more context to uniquely identify the instance.\nString: {old}"
        );
    }
    let updated = if replace_all {
        content.replace(old, new)
    } else {
        content.replacen(old, new, 1)
    };
    atomic_write(&path, updated).await?;
    let message = if replace_all {
        "The file has been updated. All occurrences were successfully replaced"
    } else {
        "The file has been updated successfully."
    };
    Ok(format!(
        "file: {}\nmessage: {}\nold_string: {}\nnew_string: {}",
        path.display(),
        message,
        old,
        new,
    ))
}

async fn exit_plan_mode(_call: &ToolCall, _config: &RuntimeToolConfig) -> Result<String> {
    Ok("Plan complete. Waiting for user confirmation to switch modes.".to_string())
}

async fn grep(call: &ToolCall, config: &RuntimeToolConfig) -> Result<String> {
    let pattern = required_str(&call.arguments, "pattern")?;
    if pattern.trim().is_empty() {
        bail!("Empty search pattern provided.");
    }
    let path = call
        .arguments
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or(".");
    let max_matches = call
        .arguments
        .get("max_matches")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| config.default_max_matches.unwrap_or(100))
        .max(1);
    let use_default_ignore = call
        .arguments
        .get("use_default_ignore")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let search_path = resolve_grep_path(path)?;

    let mut command = if command_exists("rg").await {
        let mut cmd = Command::new("rg");
        cmd.arg("--line-number")
            .arg("--no-heading")
            .arg("--with-filename")
            .arg("--smart-case")
            .arg("--no-binary")
            .arg("--max-count")
            .arg((max_matches + 1).to_string());
        if !use_default_ignore {
            cmd.arg("--no-ignore");
        }
        for pattern in grep_exclude_patterns(config) {
            cmd.arg("--glob").arg(format!("!{pattern}"));
        }
        cmd.arg("-e").arg(pattern).arg(search_path);
        cmd
    } else {
        let mut cmd = Command::new("grep");
        cmd.arg("-r")
            .arg("-n")
            .arg("-H")
            .arg("-I")
            .arg("-E")
            .arg(format!("--max-count={}", max_matches + 1));
        if pattern
            .chars()
            .all(|ch| !ch.is_alphabetic() || ch.is_lowercase())
        {
            cmd.arg("-i");
        }
        for pattern in grep_exclude_patterns(config) {
            if let Some(dir) = pattern.strip_suffix('/') {
                cmd.arg(format!("--exclude-dir={dir}"));
            } else {
                cmd.arg(format!("--exclude={pattern}"));
            }
        }
        cmd.arg("-e").arg(pattern).arg(search_path);
        cmd
    };
    let timeout_secs = config.default_timeout.unwrap_or(60);
    let output = tokio::time::timeout(Duration::from_secs(timeout_secs), command.output())
        .await
        .with_context(|| format!("Search timed out after {timeout_secs}s"))?
        .context("failed to run search")?;
    if !matches!(output.status.code(), Some(0 | 1)) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = if stderr.trim().is_empty() {
            format!("Process exited with code {}", output.status)
        } else {
            stderr.to_string()
        };
        bail!("grep error: {message}");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines = stdout.lines().collect::<Vec<_>>();
    let truncated = lines.len() as u64 > max_matches;
    let mut matches = lines
        .into_iter()
        .take(max_matches as usize)
        .collect::<Vec<_>>()
        .join("\n");
    let byte_limit = config.max_output_bytes.unwrap_or(64_000);
    if matches.len() > byte_limit {
        matches.truncate(byte_limit);
    }
    Ok(format!(
        "matches: {}\nmatch_count: {}\nwas_truncated: {}",
        matches,
        if matches.is_empty() {
            0
        } else {
            matches.lines().count()
        },
        py_bool(truncated || matches.len() == byte_limit),
    ))
}

async fn read(call: &ToolCall, config: &RuntimeToolConfig) -> Result<String> {
    let path = resolve_read_path(required_str(&call.arguments, "file_path")?)?;
    let offset = call
        .arguments
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(1) as usize;
    let limit = call
        .arguments
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(2_000) as usize;
    if limit == 0 {
        bail!("limit must be greater than 0");
    }
    let raw = tokio::fs::read_to_string(&path)
        .await
        .map_err(|error| map_read_error(&path, error, "read"))?;
    let total_lines = raw.lines().count();
    let selected = raw
        .lines()
        .enumerate()
        .skip(offset.saturating_sub(1))
        .take(limit)
        .map(|(idx, line)| format!("{:>9}→{}", idx + 1, line))
        .collect::<Vec<_>>();
    let content = if !selected.is_empty() {
        selected.join("\n")
    } else if total_lines == 0 {
        "<vibe_warning>Warning: the file exists but the contents are empty.</vibe_warning>"
            .to_string()
    } else {
        format!(
            "<vibe_warning>Warning: the file exists but is shorter than the provided offset ({offset}). The file has {total_lines} lines.</vibe_warning>"
        )
    };
    let max_read_bytes = config.max_read_bytes.unwrap_or(51_200);
    if content.len() > max_read_bytes {
        bail!(
            "Output ({} bytes) exceeds maximum allowed size ({} bytes). Use offset and limit to read a smaller portion of the file.",
            content.len(),
            max_read_bytes,
        );
    }
    let requested_offset = call
        .arguments
        .get("offset")
        .and_then(Value::as_u64)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "None".to_string());
    let was_truncated = offset.saturating_sub(1) + selected.len() < total_lines;
    Ok(format!(
        "file_path: {}\ncontent: {}\nnum_lines: {}\nstart_line: {}\nrequested_offset: {}\nrequested_limit: {}\ntotal_lines: {}\nwas_truncated: {}",
        path.display(),
        content,
        selected.len(),
        offset,
        requested_offset,
        limit,
        total_lines,
        py_bool(was_truncated),
    ))
}

async fn skill(call: &ToolCall, _config: &RuntimeToolConfig) -> Result<String> {
    let name = required_str(&call.arguments, "name")?;
    let candidates = skill_search_paths(name);
    for path in candidates {
        if path.exists() {
            let raw = tokio::fs::read_to_string(&path).await?;
            let prompt = parse_skill_prompt(&raw)?;
            let skill_dir = path
                .parent()
                .ok_or_else(|| anyhow!("skill path has no parent"))?
                .canonicalize()
                .with_context(|| format!("failed to resolve skill directory {}", path.display()))?;
            let files = list_skill_files(&skill_dir)?;
            let file_lines = files
                .iter()
                .map(|file| format!("<file>{file}</file>"))
                .collect::<Vec<_>>()
                .join("\n");
            let content = format!(
                "<skill_content name=\"{name}\">\n# Skill: {name}\n\n{}\n\nBase directory for this skill: {}\nRelative paths in this skill are relative to this base directory.\nNote: file list is sampled.\n\n<skill_files>\n{}\n</skill_files>\n</skill_content>",
                prompt.trim(),
                skill_dir.display(),
                file_lines
            );
            return Ok(format!(
                "name: {name}\ncontent: {content}\nskill_dir: {}",
                skill_dir.display()
            ));
        }
    }
    bail!("Skill \"{name}\" not found. Available skills: none");
}

fn skill_search_paths(name: &str) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(vibe_home) = std::env::var("VIBE_HOME") {
        candidates.push(
            PathBuf::from(vibe_home)
                .join("skills")
                .join(name)
                .join("SKILL.md"),
        );
    }
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(
            PathBuf::from(&home)
                .join(".vibe")
                .join("skills")
                .join(name)
                .join("SKILL.md"),
        );
        candidates.push(
            PathBuf::from(home)
                .join(".agents")
                .join("skills")
                .join(name)
                .join("SKILL.md"),
        );
    }
    candidates.push(
        PathBuf::from(".vibe")
            .join("skills")
            .join(name)
            .join("SKILL.md"),
    );
    candidates.push(
        PathBuf::from(".agents")
            .join("skills")
            .join(name)
            .join("SKILL.md"),
    );
    candidates
}

fn parse_skill_prompt(content: &str) -> Result<String> {
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        bail!("Missing or invalid YAML frontmatter (metadata section must start and end with ---)");
    }
    for line in &mut lines {
        if line.trim() == "---" {
            return Ok(lines.collect::<Vec<_>>().join("\n"));
        }
    }
    bail!("Missing or invalid YAML frontmatter (metadata section must start and end with ---)");
}

fn list_skill_files(skill_dir: &Path) -> Result<Vec<String>> {
    fn visit(base: &Path, dir: &Path, files: &mut Vec<String>) -> Result<()> {
        let mut entries = std::fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                visit(base, &path, files)?;
            } else if path.is_file()
                && path.file_name().and_then(|name| name.to_str()) != Some("SKILL.md")
            {
                files.push(
                    path.strip_prefix(base)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
                if files.len() >= 10 {
                    return Ok(());
                }
            }
            if files.len() >= 10 {
                return Ok(());
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    visit(skill_dir, skill_dir, &mut files)?;
    Ok(files)
}

impl TaskTool {
    async fn run_task(&self, call: &ToolCall) -> Result<String> {
        let task = required_str(&call.arguments, "task")?;
        let agent = call
            .arguments
            .get("agent")
            .and_then(Value::as_str)
            .unwrap_or("explore");
        let Some(profile) = self.subagents.get(agent) else {
            return Ok(format!(
                "<tool_error>task failed: Unknown agent: {agent}</tool_error>"
            ));
        };
        let Some(base_url) = self.provider_base_url.as_deref() else {
            return Ok(task_result_output(
                "Subagent error: Task tool requires agent_manager in context",
                0,
                false,
            ));
        };
        let Some(api_key_env) = self.api_key_env.as_deref() else {
            return Ok(task_result_output(
                "Subagent error: Task tool requires agent_manager in context",
                0,
                false,
            ));
        };
        let model = self.model.as_deref().unwrap_or("mistral-medium-3.5");
        match run_subagent(base_url, api_key_env, model, profile, task).await {
            Ok((response, turns_used)) => Ok(task_result_output(&response, turns_used, true)),
            Err(error) => Ok(task_result_output(
                &format!("\n[Subagent error: {error}]"),
                0,
                false,
            )),
        }
    }
}

async fn run_subagent(
    base_url: &str,
    api_key_env: &str,
    model: &str,
    profile: &SubagentProfile,
    task: &str,
) -> Result<(String, u64)> {
    let api_key = std::env::var(api_key_env)
        .with_context(|| format!("missing environment variable {api_key_env}"))?;
    let mut messages = vec![
        json!({
                "role": "system",
                "content": profile.system_prompt
        }),
        json!({
                "role": "user",
                "content": task
        }),
    ];
    let client = reqwest::Client::new();
    let mut turns_used = 0;
    for _ in 0..8 {
        let request = json!({
            "model": model,
            "messages": messages,
            "tools": subagent_tool_specs(&profile.enabled_tools),
            "tool_choice": "auto",
            "temperature": 0.1,
            "stream": false
        });
        let response = client
            .post(format!(
                "{}/chat/completions",
                base_url.trim_end_matches('/')
            ))
            .bearer_auth(&api_key)
            .json(&request)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("provider returned {status}: {body}");
        }
        let value: Value = response.json().await?;
        let message = value
            .pointer("/choices/0/message")
            .cloned()
            .or_else(|| value.get("message").cloned())
            .unwrap_or_else(|| json!({ "role": "assistant", "content": "" }));
        turns_used += 1;
        let text = message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let tool_calls = parse_openai_tool_calls(message.get("tool_calls"))?;
        if tool_calls.is_empty() {
            return Ok((text, turns_used));
        }
        messages.push(json!({
            "role": "assistant",
            "content": text,
            "tool_calls": tool_calls.iter().map(openai_tool_call_json).collect::<Vec<_>>()
        }));
        for call in tool_calls {
            let result = match call.name.as_str() {
                "grep" => tool_result(&call, grep(&call, &RuntimeToolConfig::default()).await),
                "read" => tool_result(&call, read(&call, &RuntimeToolConfig::default()).await),
                other => ToolResult {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    output: format!("<tool_error>{other}: Unknown tool '{other}'</tool_error>"),
                    success: false,
                },
            };
            messages.push(json!({
                "role": "tool",
                "tool_call_id": result.call_id,
                "name": result.name,
                "content": result.output
            }));
        }
    }
    bail!("maximum subagent tool-call iterations reached")
}

fn subagent_tool_specs(enabled_tools: &[String]) -> Vec<Value> {
    let enabled = if enabled_tools.is_empty() {
        vec!["grep".to_string(), "read".to_string()]
    } else {
        enabled_tools.to_vec()
    };
    let mut specs = Vec::new();
    if name_matches("grep", &enabled) {
        specs.push(json!({
            "type": "function",
            "function": {
                "name": "grep",
                "description": "Search files for text patterns.",
                "parameters": GrepTool::default().spec().input_schema
            }
        }));
    }
    if name_matches("read", &enabled) {
        specs.push(json!({
            "type": "function",
            "function": {
                "name": "read",
                "description": "Read a file from disk.",
                "parameters": ReadTool::default().spec().input_schema
            }
        }));
    }
    specs
}

fn parse_openai_tool_calls(value: Option<&Value>) -> Result<Vec<ToolCall>> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    items
        .iter()
        .map(|item| {
            let function = item.get("function").unwrap_or(&Value::Null);
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let raw_arguments = function
                .get("arguments")
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
                name,
                arguments,
            })
        })
        .collect()
}

fn openai_tool_call_json(call: &ToolCall) -> Value {
    json!({
        "id": call.id,
        "type": "function",
        "function": {
            "name": call.name,
            "arguments": call.arguments.to_string()
        }
    })
}

fn task_result_output(response: &str, turns_used: u64, completed: bool) -> String {
    format!(
        "response: {response}\nturns_used: {turns_used}\ncompleted: {}",
        py_bool(completed)
    )
}

async fn webfetch(call: &ToolCall, config: &RuntimeToolConfig) -> Result<String> {
    let url = required_str(&call.arguments, "url")?;
    let normalized_url = normalize_fetch_url(url);
    let timeout = call
        .arguments
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| config.default_timeout.unwrap_or(30));
    if timeout == 0 {
        bail!("Timeout must be a positive number");
    }
    let max_timeout = config.max_timeout.unwrap_or(120);
    if timeout > max_timeout {
        bail!("Timeout cannot exceed {max_timeout} seconds");
    }
    let user_agent = config.user_agent.as_deref().unwrap_or(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
    );
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout))
        .build()?
        .get(&normalized_url)
        .header("User-Agent", user_agent)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
        )
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await?
        .error_for_status()?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("text/plain")
        .to_string();
    let mut content = response.text().await?;
    if content_type.contains("text/html") {
        content = strip_html(&content);
    }
    let mut was_truncated = false;
    let max_content_bytes = config.max_content_bytes.unwrap_or(120_000);
    if content.len() > max_content_bytes {
        was_truncated = true;
        let truncate_at = content
            .char_indices()
            .map(|(idx, _)| idx)
            .take_while(|idx| *idx <= max_content_bytes)
            .last()
            .unwrap_or(0);
        content.truncate(truncate_at);
        content.push_str("\n\n[Content truncated due to size limit]");
    }
    Ok(format!(
        "url: {}\ncontent: {}\ncontent_type: {}\nwas_truncated: {}",
        normalized_url,
        content,
        content_type,
        py_bool(was_truncated)
    ))
}

#[derive(Default)]
struct WebSearchTool {
    provider_base_url: Option<String>,
    api_key_env: Option<String>,
    config: RuntimeToolConfig,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_search".to_string(),
            description: "Search the web for current information using Mistral's web search."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
        }
    }

    async fn run(&self, call: &ToolCall) -> ToolResult {
        tool_result(call, self.websearch(call).await)
    }
}

impl WebSearchTool {
    async fn websearch(&self, call: &ToolCall) -> Result<String> {
        let query = required_str(&call.arguments, "query")?;
        let api_key_env = self.api_key_env.as_deref().unwrap_or("MISTRAL_API_KEY");
        let api_key = std::env::var(api_key_env)
            .with_context(|| format!("{api_key_env} environment variable not set."))?;
        let base_url = self
            .provider_base_url
            .as_deref()
            .map(conversations_base_url)
            .unwrap_or_else(|| "https://api.mistral.ai/v1/conversations".to_string());
        let response = reqwest::Client::builder()
            .timeout(Duration::from_secs(self.config.timeout.unwrap_or(120)))
            .build()?
            .post(base_url)
            .bearer_auth(api_key)
            .json(&json!({
                "model": self.config.model.as_deref().unwrap_or("mistral-vibe-cli-with-tools"),
                "instructions": "Always use the web_search tool to answer queries. Never answer from memory alone.",
                "tools": [{"type": "web_search"}],
                "inputs": query,
                "store": false
            }))
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("Mistral API error: {status} {body}");
        }
        parse_websearch_response(response.json().await?, query)
    }
}

fn conversations_base_url(api_base: &str) -> String {
    let trimmed = api_base.trim_end_matches('/');
    if let Some(index) = trimmed.rfind("/v") {
        let suffix = &trimmed[index + 2..];
        if suffix.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
            return format!("{}/v1/conversations", &trimmed[..index]);
        }
    }
    format!("{trimmed}/v1/conversations")
}

fn parse_websearch_response(value: Value, query: &str) -> Result<String> {
    let mut answer = String::new();
    let mut sources = Vec::<(String, String)>::new();
    let mut seen = BTreeSet::<String>::new();
    for output in value
        .get("outputs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(content) = output.get("content") else {
            continue;
        };
        if let Some(text) = content.as_str() {
            answer.push_str(text);
            continue;
        }
        for chunk in content.as_array().into_iter().flatten() {
            if chunk.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(text) = chunk.get("text").and_then(Value::as_str) {
                    answer.push_str(text);
                }
            } else if chunk.get("type").and_then(Value::as_str) == Some("tool_reference") {
                let Some(url) = chunk.get("url").and_then(Value::as_str) else {
                    continue;
                };
                if seen.insert(url.to_string()) {
                    let title = chunk
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    sources.push((title.to_string(), url.to_string()));
                }
            }
        }
    }
    let answer = answer.trim().to_string();
    if answer.is_empty() {
        bail!("No text in agent response.");
    }
    Ok(format!(
        "query: {}\nanswer: {}\nsources: {}",
        query,
        answer,
        format_websearch_sources(&sources)
    ))
}

fn format_websearch_sources(sources: &[(String, String)]) -> String {
    let rendered = sources
        .iter()
        .map(|(title, url)| {
            format!(
                "{{'title': {}, 'url': {}}}",
                python_string_repr(title),
                python_string_repr(url)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{rendered}]")
}

fn normalize_fetch_url(url: &str) -> String {
    if let Some(stripped) = url.strip_prefix("//") {
        return format!("https://{stripped}");
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!("https://{url}")
    }
}

async fn write_file(call: &ToolCall, config: &RuntimeToolConfig) -> Result<String> {
    let path = resolve_write_path(required_str(&call.arguments, "path")?)?;
    let content = required_str(&call.arguments, "content")?;
    let max_write_bytes = config.max_write_bytes.unwrap_or(64_000);
    if content.len() > max_write_bytes {
        bail!("Content exceeds {max_write_bytes} bytes limit");
    }
    if path.exists() {
        bail!(
            "File '{}' already exists. Use edit to modify it.",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        if config.create_parent_dirs == Some(false) && !parent.exists() {
            bail!("Parent directory does not exist: {}", parent.display());
        }
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&path, content).await?;
    let display_path = std::fs::canonicalize(&path).unwrap_or(path);
    Ok(format!(
        "path: {}\nbytes_written: {}\ncontent: {}",
        display_path.display(),
        content.len(),
        content,
    ))
}

struct TodoTool {
    state: Arc<Mutex<Vec<Value>>>,
    config: RuntimeToolConfig,
}

#[async_trait]
impl Tool for TodoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "todo".to_string(),
            description: "Manage todos. Use action='read' to view, action='write' with complete list to update.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "description": "Either 'read' or 'write'" },
                    "todos": { "type": ["array", "null"], "description": "Complete list of todos when writing." }
                },
                "required": ["action"]
            }),
        }
    }

    async fn run(&self, call: &ToolCall) -> ToolResult {
        tool_result(call, todo_with_state(call, &self.state, &self.config).await)
    }
}

async fn todo_with_state(
    call: &ToolCall,
    state: &Arc<Mutex<Vec<Value>>>,
    config: &RuntimeToolConfig,
) -> Result<String> {
    let action = required_str(&call.arguments, "action")?;
    match action {
        "read" => {
            let todos = state
                .lock()
                .map_err(|_| anyhow!("todo state poisoned"))?
                .clone();
            Ok(todo_result(
                format!("Retrieved {} todos", todos.len()),
                todos,
            ))
        }
        "write" => {
            let todos = required(&call.arguments, "todos")?
                .as_array()
                .ok_or_else(|| anyhow!("todos must be an array"))?
                .clone();
            let max_todos = config.max_todos.unwrap_or(100);
            if todos.len() > max_todos {
                bail!("Cannot store more than {max_todos} todos");
            }
            let mut ids = std::collections::BTreeSet::new();
            for todo in &todos {
                let id = todo
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("todo id must be a string"))?;
                if !ids.insert(id.to_string()) {
                    bail!("Todo IDs must be unique");
                }
            }
            *state.lock().map_err(|_| anyhow!("todo state poisoned"))? = todos.clone();
            Ok(todo_result(format!("Updated {} todos", todos.len()), todos))
        }
        _ => bail!("Invalid action '{action}'. Use 'read' or 'write'."),
    }
}

fn todo_result(message: String, todos: Vec<Value>) -> String {
    format!(
        "message: {}\ntodos: {}\ntotal_count: {}",
        message,
        format_todo_list(&todos),
        todos.len()
    )
}

fn format_todo_list(todos: &[Value]) -> String {
    let rendered = todos
        .iter()
        .map(format_todo_item)
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{rendered}]")
}

fn format_todo_item(todo: &Value) -> String {
    let id = todo.get("id").and_then(Value::as_str).unwrap_or_default();
    let content = todo
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let status = todo
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("pending");
    let priority = todo
        .get("priority")
        .and_then(Value::as_str)
        .unwrap_or("medium");

    format!(
        "{{'id': {}, 'content': {}, 'status': {}, 'priority': {}}}",
        python_string_repr(id),
        python_string_repr(content),
        todo_status_repr(status),
        todo_priority_repr(priority)
    )
}

fn python_string_repr(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn todo_status_repr(status: &str) -> String {
    let variant = status.to_ascii_uppercase();
    format!("<TodoStatus.{}: {}>", variant, python_string_repr(status))
}

fn todo_priority_repr(priority: &str) -> String {
    let variant = priority.to_ascii_uppercase();
    format!(
        "<TodoPriority.{}: {}>",
        variant,
        python_string_repr(priority)
    )
}

fn required<'a>(value: &'a Value, key: &str) -> Result<&'a Value> {
    value.get(key).ok_or_else(|| anyhow!("missing {key}"))
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    required(value, key)?
        .as_str()
        .ok_or_else(|| anyhow!("{key} must be a string"))
}

async fn atomic_write(path: &Path, content: String) -> Result<()> {
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("microvibe")
    ));
    tokio::fs::write(&tmp, content).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

fn resolve_read_path(raw: &str) -> Result<PathBuf> {
    if raw.trim().is_empty() {
        bail!("file_path cannot be empty");
    }
    let path = absolutize(raw);
    if !path.exists() {
        bail!("File not found at: {}", path.display());
    }
    if path.is_dir() {
        bail!("Path is a directory, not a file: {}", path.display());
    }
    Ok(path)
}

fn resolve_edit_path(raw: &str) -> Result<PathBuf> {
    if raw.trim().is_empty() {
        bail!("File path cannot be empty");
    }
    let path = absolutize(raw);
    if !path.exists() {
        bail!("File does not exist: {}", path.display());
    }
    if !path.is_file() {
        bail!("Path is not a file: {}", path.display());
    }
    Ok(path)
}

fn resolve_write_path(raw: &str) -> Result<PathBuf> {
    if raw.trim().is_empty() {
        bail!("Path cannot be empty");
    }
    Ok(absolutize(raw))
}

fn resolve_grep_path(raw: &str) -> Result<PathBuf> {
    let path = PathBuf::from(raw).expanduser();
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    if !path.exists() {
        bail!("Path does not exist: {raw}");
    }
    Ok(path)
}

fn absolutize(raw: &str) -> PathBuf {
    let path = PathBuf::from(raw).expanduser();
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
    .canonicalize()
    .unwrap_or_else(|_| {
        let path = PathBuf::from(raw).expanduser();
        if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        }
    })
}

fn map_read_error(path: &Path, error: std::io::Error, action: &str) -> anyhow::Error {
    if action == "edit" {
        match error.kind() {
            std::io::ErrorKind::PermissionDenied => {
                anyhow!("Permission denied accessing file: {}", path.display())
            }
            _ => anyhow!("OS error accessing {}: {}", path.display(), error),
        }
    } else {
        anyhow!("Error reading {}: {}", path.display(), error)
    }
}

trait ExpandUser {
    fn expanduser(self) -> PathBuf;
}

impl ExpandUser for PathBuf {
    fn expanduser(self) -> PathBuf {
        let Some(raw) = self.to_str() else {
            return self;
        };
        if raw == "~" {
            return dirs::home_dir().unwrap_or(self);
        }
        if let Some(rest) = raw.strip_prefix("~/")
            && let Some(home) = dirs::home_dir()
        {
            return home.join(rest);
        }
        self
    }
}

async fn command_exists(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .await
        .map(|status| status.success())
        .unwrap_or(false)
}

fn default_exclude_patterns() -> &'static [&'static str] {
    &[
        ".venv/",
        "venv/",
        ".env/",
        "env/",
        "node_modules/",
        ".git/",
        "__pycache__/",
        ".pytest_cache/",
        ".mypy_cache/",
        ".tox/",
        ".nox/",
        ".coverage/",
        "htmlcov/",
        "dist/",
        "build/",
        ".idea/",
        ".vscode/",
        "*.egg-info",
        "*.pyc",
        "*.pyo",
        "*.pyd",
        ".DS_Store",
        "Thumbs.db",
    ]
}

fn grep_exclude_patterns(config: &RuntimeToolConfig) -> Vec<String> {
    let mut patterns = config.exclude_patterns.clone().unwrap_or_else(|| {
        default_exclude_patterns()
            .iter()
            .map(|item| item.to_string())
            .collect()
    });
    let codeignore_file = config
        .codeignore_file
        .as_deref()
        .unwrap_or(".vibeignore")
        .trim();
    if !codeignore_file.is_empty() {
        let path = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(codeignore_file);
        if let Ok(raw) = std::fs::read_to_string(path) {
            patterns.extend(
                raw.lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty() && !line.starts_with('#'))
                    .map(ToString::to_string),
            );
        }
    }
    patterns
}

fn truncate_string_bytes(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let truncate_at = value
        .char_indices()
        .map(|(idx, _)| idx)
        .take_while(|idx| *idx <= max_bytes)
        .last()
        .unwrap_or(0);
    value.truncate(truncate_at);
}

fn strip_html(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                output.push(' ');
            }
            _ if !in_tag => output.push(ch),
            _ => {}
        }
    }
    output.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn py_bool(value: bool) -> &'static str {
    if value { "True" } else { "False" }
}

fn tool_result(call: &ToolCall, result: Result<String>) -> ToolResult {
    match result {
        Ok(output) => ToolResult {
            call_id: call.id.clone(),
            name: call.name.clone(),
            output,
            success: true,
        },
        Err(error) => ToolResult {
            call_id: call.id.clone(),
            name: call.name.clone(),
            output: error.to_string(),
            success: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn configured_registry(configs: Value) -> ToolRegistry {
        ToolRegistry::with_builtins_and_configs(RuntimeToolConfigs::from_value(configs))
    }

    #[test]
    fn builtin_specs_match_mistral_tool_names() {
        let names = ToolRegistry::with_builtins()
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "ask_user_question",
                "bash",
                "edit",
                "exit_plan_mode",
                "grep",
                "read",
                "skill",
                "task",
                "todo",
                "web_fetch",
                "web_search",
                "write_file",
            ]
        );
    }

    #[tokio::test]
    async fn read_write_and_edit_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.txt");
        let registry = ToolRegistry::with_builtins();

        let write = registry
            .run(&ToolCall::new(
                "write_file",
                json!({ "path": &path, "content": "alpha\nbeta\n" }),
            ))
            .await;
        assert!(write.success, "{}", write.output);
        assert!(write.output.contains("bytes_written: 11"));
        assert!(write.output.contains("content: alpha\nbeta\n"));

        let read = registry
            .run(&ToolCall::new(
                "read",
                json!({ "file_path": &path, "offset": 2, "limit": 1 }),
            ))
            .await;
        assert!(read.success, "{}", read.output);
        assert!(read.output.contains("content:         2→beta"));
        assert!(read.output.contains("start_line: 2"));
        assert!(read.output.contains("requested_offset: 2"));
        assert!(read.output.contains("was_truncated: False"));

        let edit = registry
            .run(&ToolCall::new(
                "edit",
                json!({
                    "file_path": &path,
                    "old_string": "beta",
                    "new_string": "gamma"
                }),
            ))
            .await;
        assert!(edit.success, "{}", edit.output);
        assert!(
            edit.output
                .contains("message: The file has been updated successfully.")
        );
        assert_eq!(
            tokio::fs::read_to_string(path).await.unwrap(),
            "alpha\ngamma\n"
        );
    }

    #[tokio::test]
    async fn bash_captures_stdout() {
        let result = ToolRegistry::with_builtins()
            .run(&ToolCall::new(
                "bash",
                json!({ "command": "printf microvibe" }),
            ))
            .await;
        assert!(result.success, "{}", result.output);
        assert_eq!(
            result.output,
            "command: printf microvibe\nstdout: microvibe\nstderr: \nreturncode: 0"
        );
    }

    #[tokio::test]
    async fn bash_runtime_config_controls_output_limit() {
        let result = configured_registry(json!({
            "bash": { "max_output_bytes": 5 }
        }))
        .run(&ToolCall::new(
            "bash",
            json!({ "command": "printf 123456789" }),
        ))
        .await;
        assert!(result.success, "{}", result.output);
        assert_eq!(
            result.output,
            "command: printf 123456789\nstdout: 12345\nstderr: \nreturncode: 0"
        );
    }

    #[tokio::test]
    async fn grep_finds_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.txt");
        tokio::fs::write(&path, "alpha\nneedle\n").await.unwrap();

        let result = ToolRegistry::with_builtins()
            .run(&ToolCall::new(
                "grep",
                json!({ "pattern": "needle", "path": dir.path() }),
            ))
            .await;
        assert!(result.success, "{}", result.output);
        assert!(result.output.contains("matches: "));
        assert!(result.output.contains("needle"));
        assert!(result.output.contains("match_count: 1"));
    }

    #[tokio::test]
    async fn grep_runtime_config_controls_defaults_and_excludes() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("keep.txt"), "needle\nneedle\n")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("skip.log"), "needle\n")
            .await
            .unwrap();

        let result = configured_registry(json!({
            "grep": {
                "default_max_matches": 1,
                "exclude_patterns": ["*.log"],
                "max_output_bytes": 64_000
            }
        }))
        .run(&ToolCall::new(
            "grep",
            json!({ "pattern": "needle", "path": dir.path() }),
        ))
        .await;
        assert!(result.success, "{}", result.output);
        assert!(result.output.contains("keep.txt"));
        assert!(!result.output.contains("skip.log"));
        assert!(result.output.contains("match_count: 1"));
        assert!(result.output.contains("was_truncated: True"));
    }

    #[tokio::test]
    async fn file_tool_errors_match_vibe_wording() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.txt");
        tokio::fs::write(&path, "same\nsame\n").await.unwrap();
        let registry = ToolRegistry::with_builtins();

        let duplicate_write = registry
            .run(&ToolCall::new(
                "write_file",
                json!({ "path": &path, "content": "new" }),
            ))
            .await;
        assert!(!duplicate_write.success);
        assert!(
            duplicate_write
                .output
                .contains("already exists. Use edit to modify it.")
        );

        let ambiguous_edit = registry
            .run(&ToolCall::new(
                "edit",
                json!({ "file_path": &path, "old_string": "same", "new_string": "other" }),
            ))
            .await;
        assert!(!ambiguous_edit.success);
        assert!(ambiguous_edit.output.contains("Found 2 matches"));

        let empty_read = registry
            .run(&ToolCall::new("read", json!({ "file_path": "" })))
            .await;
        assert!(!empty_read.success);
        assert_eq!(empty_read.output, "file_path cannot be empty");
    }

    #[tokio::test]
    async fn write_file_runtime_config_controls_limits_and_parent_creation() {
        let dir = tempfile::tempdir().unwrap();
        let missing_parent = dir.path().join("missing").join("sample.txt");
        let registry = configured_registry(json!({
            "write_file": {
                "max_write_bytes": 4,
                "create_parent_dirs": false
            }
        }));

        let too_large = registry
            .run(&ToolCall::new(
                "write_file",
                json!({ "path": dir.path().join("large.txt"), "content": "12345" }),
            ))
            .await;
        assert!(!too_large.success);
        assert_eq!(too_large.output, "Content exceeds 4 bytes limit");

        let missing_parent_result = registry
            .run(&ToolCall::new(
                "write_file",
                json!({ "path": missing_parent, "content": "1234" }),
            ))
            .await;
        assert!(!missing_parent_result.success);
        assert!(
            missing_parent_result
                .output
                .contains("Parent directory does not exist:")
        );
    }

    #[tokio::test]
    async fn todos_are_session_scoped_and_structured() {
        let registry = ToolRegistry::with_builtins();
        let initial = registry
            .run(&ToolCall::new("todo", json!({ "action": "read" })))
            .await;
        assert!(initial.success, "{}", initial.output);
        assert_eq!(
            initial.output,
            "message: Retrieved 0 todos\ntodos: []\ntotal_count: 0"
        );

        let updated = registry
            .run(&ToolCall::new(
                "todo",
                json!({
                    "action": "write",
                    "todos": [{ "id": "1", "content": "ship parity", "status": "pending", "priority": "high" }]
                }),
        ))
        .await;
        assert!(updated.success, "{}", updated.output);
        assert!(updated.output.contains("message: Updated 1 todos"));
        assert!(updated.output.contains("'content': 'ship parity'"));
        assert!(
            updated
                .output
                .contains("'status': <TodoStatus.PENDING: 'pending'>")
        );
        assert!(
            updated
                .output
                .contains("'priority': <TodoPriority.HIGH: 'high'>")
        );

        let read = registry
            .run(&ToolCall::new("todo", json!({ "action": "read" })))
            .await;
        assert!(read.success, "{}", read.output);
        assert!(read.output.contains("message: Retrieved 1 todos"));
        assert!(read.output.contains("'content': 'ship parity'"));
        assert!(read.output.contains("total_count: 1"));
    }

    #[tokio::test]
    async fn todo_runtime_config_controls_max_todos() {
        let result = configured_registry(json!({
            "todo": { "max_todos": 1 }
        }))
        .run(&ToolCall::new(
            "todo",
            json!({
                "action": "write",
                "todos": [
                    { "id": "1", "content": "one" },
                    { "id": "2", "content": "two" }
                ]
            }),
        ))
        .await;
        assert!(!result.success);
        assert_eq!(result.output, "Cannot store more than 1 todos");
    }

    #[test]
    fn websearch_parses_mistral_conversation_response() {
        let output = parse_websearch_response(
            json!({
                "conversation_id": "test",
                "outputs": [{
                    "content": [
                        { "type": "text", "text": "Answer " },
                        { "type": "text", "text": "body" },
                        { "type": "tool_reference", "tool": "web_search", "title": "Source A", "url": "https://a.example" },
                        { "type": "tool_reference", "tool": "web_search", "title": "Duplicate", "url": "https://a.example" },
                        { "type": "tool_reference", "tool": "web_search", "title": "Source B", "url": "https://b.example" }
                    ],
                    "object": "entry",
                    "type": "message.output",
                    "role": "assistant"
                }]
            }),
            "test query",
        )
        .unwrap();
        assert_eq!(
            output,
            "query: test query\nanswer: Answer body\nsources: [{'title': 'Source A', 'url': 'https://a.example'}, {'title': 'Source B', 'url': 'https://b.example'}]"
        );
    }

    #[test]
    fn websearch_conversations_url_strips_versioned_api_base() {
        assert_eq!(
            conversations_base_url("http://127.0.0.1:1234/v1"),
            "http://127.0.0.1:1234/v1/conversations"
        );
        assert_eq!(
            conversations_base_url("https://api.example.test/custom/v2"),
            "https://api.example.test/custom/v1/conversations"
        );
    }

    #[test]
    fn enabled_tool_filters_match_exact_glob_and_regex_names() {
        let mut registry = ToolRegistry::with_builtins();
        registry.apply_filters(&["read".to_string(), "web_*".to_string()], &[]);
        assert!(registry.is_enabled("read"));
        assert!(registry.is_enabled("web_fetch"));
        assert!(registry.is_enabled("web_search"));
        assert!(!registry.is_enabled("bash"));

        let mut registry = ToolRegistry::with_builtins();
        registry.apply_filters(&["re:^(read|grep)$".to_string()], &[]);
        assert!(registry.is_enabled("read"));
        assert!(registry.is_enabled("grep"));
        assert!(!registry.is_enabled("write_file"));
    }

    #[test]
    fn enabled_tool_filters_take_precedence_over_disabled_patterns() {
        let mut registry = ToolRegistry::with_builtins();
        registry.apply_filters(&["bash".to_string()], &["bash".to_string()]);
        assert!(registry.is_enabled("bash"));
        assert!(!registry.is_enabled("read"));
    }
}
