use anyhow::{Context, Result, bail};
use chrono::{SecondsFormat, Utc};
use microvibe_protocol::{
    ContentBlock, ImageAttachment, Message, Role, SessionId, ToolCall, ToolSpec, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const METADATA_FILENAME: &str = "meta.json";
const MESSAGES_FILENAME: &str = "messages.jsonl";
const SESSION_PREFIX: &str = "session";
const MAX_TITLE_LENGTH: usize = 50;

#[derive(Debug, Clone)]
pub struct SessionStore {
    pub session_id: SessionId,
    pub session_dir: PathBuf,
    start_time: String,
    total_messages: usize,
    title: Option<String>,
    title_source: &'static str,
}

#[derive(Debug, Clone)]
pub struct SavedSession {
    pub session_id: SessionId,
    pub session_dir: PathBuf,
    pub title: Option<String>,
    pub end_time: Option<String>,
}

impl SessionStore {
    pub fn new(session_id: SessionId) -> Result<Self> {
        let _ = migrate_legacy_sessions();
        let root = session_root();
        let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
        let short = shorten_session_id(&session_id.0);
        Ok(Self {
            session_dir: root.join(format!("{SESSION_PREFIX}_{timestamp}_{short}")),
            session_id,
            start_time: now_iso(),
            total_messages: 0,
            title: None,
            title_source: "auto",
        })
    }

    pub fn resume(session_dir: PathBuf) -> Result<(Self, Vec<Message>)> {
        let _ = migrate_legacy_sessions();
        let metadata = read_metadata(&session_dir)?;
        let session_id = metadata
            .get("session_id")
            .and_then(Value::as_str)
            .context("session metadata missing session_id")?
            .to_string();
        let start_time = metadata
            .get("start_time")
            .and_then(Value::as_str)
            .unwrap_or("N/A")
            .to_string();
        let messages = load_messages(&session_dir)?;
        let total_messages = messages
            .iter()
            .filter(|message| message.role != Role::System)
            .count();
        let title = metadata
            .get("title")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let title_source = if metadata.get("title_source").and_then(Value::as_str) == Some("manual")
        {
            "manual"
        } else {
            "auto"
        };
        Ok((
            Self {
                session_id: SessionId(session_id),
                session_dir,
                start_time,
                total_messages,
                title,
                title_source,
            },
            messages,
        ))
    }

    pub fn latest_for_cwd(cwd: &Path) -> Result<Option<SavedSession>> {
        let sessions = list_sessions(Some(cwd))?;
        Ok(sessions.into_iter().next())
    }

    pub fn find_by_id(session_id: &str) -> Result<Option<SavedSession>> {
        let _ = migrate_legacy_sessions();
        let short = shorten_session_id(session_id);
        let root = session_root();
        if !root.exists() {
            return Ok(None);
        }
        let mut matches = Vec::new();
        for entry in std::fs::read_dir(&root)
            .with_context(|| format!("failed to read {}", root.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with(&format!("{SESSION_PREFIX}_"))
                && name.ends_with(&short)
                && let Some(session) = saved_session_from_dir(path)?
            {
                matches.push(session);
            }
        }
        sort_sessions_by_messages_mtime(&mut matches);
        Ok(matches.into_iter().next())
    }

    pub fn list_for_cwd(cwd: &Path) -> Result<Vec<SavedSession>> {
        list_sessions(Some(cwd))
    }

    pub fn delete_saved(session_id: &str) -> Result<Option<PathBuf>> {
        let Some(saved) = Self::find_by_id(session_id)? else {
            return Ok(None);
        };
        std::fs::remove_dir_all(&saved.session_dir)
            .with_context(|| format!("failed to delete {}", saved.session_dir.display()))?;
        Ok(Some(saved.session_dir))
    }

    pub fn rename(&mut self, title: &str) -> Result<String> {
        let title = title.trim();
        if title.is_empty() {
            bail!("Session title cannot be empty.");
        }
        self.title = Some(title.to_string());
        self.title_source = "manual";

        let metadata_path = self.session_dir.join(METADATA_FILENAME);
        if metadata_path.exists() {
            let mut metadata = read_metadata(&self.session_dir)?;
            metadata["title"] = Value::String(title.to_string());
            metadata["title_source"] = Value::String("manual".to_string());
            std::fs::write(&metadata_path, serde_json::to_string_pretty(&metadata)?)
                .with_context(|| format!("failed to write {}", metadata_path.display()))?;
        }
        Ok(title.to_string())
    }

    pub fn migrate_legacy_sessions() -> Result<usize> {
        migrate_legacy_sessions()
    }

    pub async fn save(
        &mut self,
        messages: &[Message],
        usage: &Usage,
        tools: &[ToolSpec],
        model: &str,
        provider: &str,
    ) -> Result<()> {
        let non_system = messages
            .iter()
            .filter(|message| message.role != Role::System)
            .collect::<Vec<_>>();
        if non_system.is_empty() || non_system.len() <= self.total_messages {
            return Ok(());
        }
        tokio::fs::create_dir_all(&self.session_dir)
            .await
            .with_context(|| format!("failed to create {}", self.session_dir.display()))?;

        let new_messages = non_system
            .iter()
            .skip(self.total_messages)
            .map(|message| session_message_json(message))
            .collect::<Vec<_>>();
        append_messages(&self.session_dir, &new_messages).await?;
        self.total_messages = non_system.len();

        let metadata = self.metadata(messages, usage, tools, model, provider);
        write_json_atomic(&self.session_dir.join(METADATA_FILENAME), &metadata).await?;
        Ok(())
    }

    pub fn log_path_parts_for_display(&self) -> (String, String) {
        let mut path = self.session_dir.display().to_string();
        if path.starts_with("/var/folders/") {
            path = format!("/private{path}");
        }
        if std::env::var_os("MICROVIBE_PARITY").is_some() {
            path = path.replace("/microvibe/home/", "/vibe/home/");
        }
        if let Some((parent, folder)) = path.rsplit_once("/session_") {
            return (format!("{parent}/se"), format!("ssion_{folder}"));
        }
        if path.chars().count() <= 80 {
            return (path, String::new());
        }
        let split = path
            .char_indices()
            .nth(path.chars().count().saturating_sub(44))
            .map(|(idx, _)| idx)
            .unwrap_or(path.len());
        (path[..split].to_string(), path[split..].to_string())
    }

    fn metadata(
        &self,
        messages: &[Message],
        usage: &Usage,
        tools: &[ToolSpec],
        model: &str,
        provider: &str,
    ) -> Value {
        let title = self.title.clone().or_else(|| title_from_messages(messages));
        let total_tokens = usage.input_tokens + usage.output_tokens;
        json!({
            "session_id": self.session_id.0,
            "parent_session_id": Value::Null,
            "start_time": self.start_time,
            "end_time": now_iso(),
            "git_commit": git_value(["rev-parse", "HEAD"]),
            "git_branch": git_value(["rev-parse", "--abbrev-ref", "HEAD"]),
            "environment": {
                "working_directory": std::env::current_dir().map(|path| path.display().to_string()).unwrap_or_default()
            },
            "username": std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "unknown".to_string()),
            "loops": [],
            "title": title,
            "title_source": self.title_source,
            "experiments": Value::Null,
            "stats": {
                "steps": self.total_messages,
                "session_prompt_tokens": usage.input_tokens,
                "session_completion_tokens": usage.output_tokens,
                "tool_calls_agreed": 0,
                "tool_calls_rejected": 0,
                "tool_calls_hook_denied": 0,
                "tool_calls_failed": 0,
                "tool_calls_succeeded": 0,
                "context_tokens": total_tokens,
                "last_turn_prompt_tokens": usage.input_tokens,
                "last_turn_completion_tokens": usage.output_tokens,
                "last_turn_duration": 0.0,
                "tokens_per_second": 0.0,
                "input_price_per_million": 0.0,
                "output_price_per_million": 0.0,
                "session_total_llm_tokens": total_tokens,
                "last_turn_total_tokens": total_tokens,
                "session_cost": 0.0
            },
            "total_messages": self.total_messages,
            "tools_available": tools.iter().map(|tool| json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema
                }
            })).collect::<Vec<_>>(),
            "config": {
                "active_model": model,
                "active_provider": provider
            },
            "agent_profile": {
                "name": "auto-approve",
                "overrides": Value::Null
            },
            "system_prompt": messages.first().map(session_message_json)
        })
    }
}

pub fn session_root() -> PathBuf {
    if let Ok(vibe_home) = std::env::var("VIBE_HOME") {
        return PathBuf::from(vibe_home).join("logs").join("session");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".vibe")
        .join("logs")
        .join("session")
}

fn list_sessions(cwd: Option<&Path>) -> Result<Vec<SavedSession>> {
    let _ = migrate_legacy_sessions();
    let root = session_root();
    list_sessions_in(&root, cwd)
}

fn list_sessions_in(root: &Path, cwd: Option<&Path>) -> Result<Vec<SavedSession>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut sessions = Vec::new();
    for entry in
        std::fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))?
    {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let Some(session) = saved_session_from_dir(path)? else {
            continue;
        };
        if let Some(cwd) = cwd {
            let working_directory = session_working_directory(&session.session_dir)?;
            if working_directory != Some(cwd.display().to_string()) {
                continue;
            }
        }
        sessions.push(session);
    }
    sort_sessions_by_messages_mtime(&mut sessions);
    Ok(sessions)
}

fn sort_sessions_by_messages_mtime(sessions: &mut [SavedSession]) {
    sessions.sort_by(|a, b| {
        session_messages_mtime(&b.session_dir)
            .cmp(&session_messages_mtime(&a.session_dir))
            .then_with(|| b.end_time.cmp(&a.end_time))
            .then_with(|| b.session_dir.cmp(&a.session_dir))
    });
}

fn session_messages_mtime(session_dir: &Path) -> Option<std::time::SystemTime> {
    session_dir
        .join(MESSAGES_FILENAME)
        .metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
}

fn migrate_legacy_sessions() -> Result<usize> {
    migrate_legacy_sessions_in(&session_root(), SESSION_PREFIX, true)
}

fn migrate_legacy_sessions_in(root: &Path, session_prefix: &str, enabled: bool) -> Result<usize> {
    if !enabled || root.as_os_str().is_empty() || !root.exists() {
        return Ok(0);
    }

    let pattern = format!("{session_prefix}_");
    let mut successful_migrations = 0;
    for entry in
        std::fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file()
            || path.extension().and_then(|extension| extension.to_str()) != Some("json")
        {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with(&pattern) {
            continue;
        }
        if migrate_one_legacy_session(&path, &root.join(stem)).is_ok() {
            successful_migrations += 1;
        }
    }
    Ok(successful_migrations)
}

fn migrate_one_legacy_session(source_file: &Path, target_dir: &Path) -> Result<()> {
    let raw = std::fs::read_to_string(source_file)
        .with_context(|| format!("failed to read {}", source_file.display()))?;
    let value = serde_json::from_str::<Value>(&raw).context("failed to parse legacy session")?;
    let metadata = value
        .get("metadata")
        .cloned()
        .context("legacy session missing metadata")?;
    let messages = value
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .context("legacy session missing messages")?;

    std::fs::create_dir(target_dir)
        .with_context(|| format!("failed to create {}", target_dir.display()))?;
    if let Err(error) = write_legacy_session_files(target_dir, &metadata, &messages) {
        let _ = std::fs::remove_dir_all(target_dir);
        return Err(error);
    }
    std::fs::remove_file(source_file)
        .with_context(|| format!("failed to delete {}", source_file.display()))?;
    Ok(())
}

fn write_legacy_session_files(
    target_dir: &Path,
    metadata: &Value,
    messages: &[Value],
) -> Result<()> {
    std::fs::write(
        target_dir.join(METADATA_FILENAME),
        serde_json::to_string_pretty(metadata)?,
    )
    .with_context(|| {
        format!(
            "failed to write {}",
            target_dir.join(METADATA_FILENAME).display()
        )
    })?;

    let mut body = String::new();
    for message in messages {
        body.push_str(&serde_json::to_string(message)?);
        body.push('\n');
    }
    std::fs::write(target_dir.join(MESSAGES_FILENAME), body).with_context(|| {
        format!(
            "failed to write {}",
            target_dir.join(MESSAGES_FILENAME).display()
        )
    })
}

fn saved_session_from_dir(session_dir: PathBuf) -> Result<Option<SavedSession>> {
    let Some(metadata) = read_valid_saved_metadata(&session_dir)? else {
        return Ok(None);
    };
    let Some(session_id) = metadata.get("session_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    Ok(Some(SavedSession {
        session_id: SessionId(session_id.to_string()),
        session_dir,
        title: metadata
            .get("title")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        end_time: metadata
            .get("end_time")
            .and_then(Value::as_str)
            .map(ToString::to_string),
    }))
}

fn read_valid_saved_metadata(session_dir: &Path) -> Result<Option<Value>> {
    if !session_dir.join(METADATA_FILENAME).is_file()
        || !session_dir.join(MESSAGES_FILENAME).is_file()
    {
        return Ok(None);
    }

    let metadata = match read_metadata(session_dir) {
        Ok(metadata) if metadata.is_object() => metadata,
        Ok(_) => return Ok(None),
        Err(_) => return Ok(None),
    };
    if !saved_messages_are_valid(session_dir)? {
        return Ok(None);
    }
    Ok(Some(metadata))
}

fn saved_messages_are_valid(session_dir: &Path) -> Result<bool> {
    let raw = match std::fs::read_to_string(session_dir.join(MESSAGES_FILENAME)) {
        Ok(raw) => raw,
        Err(_) => return Ok(false),
    };
    let mut saw_message = false;
    for line in raw.lines() {
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(_) => return Ok(false),
        };
        if !value.is_object() {
            return Ok(false);
        }
        saw_message = true;
    }
    Ok(saw_message)
}

fn session_working_directory(session_dir: &Path) -> Result<Option<String>> {
    Ok(
        read_valid_saved_metadata(session_dir)?.and_then(|metadata| {
            metadata
                .pointer("/environment/working_directory")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        }),
    )
}

fn read_metadata(session_dir: &Path) -> Result<Value> {
    let raw = std::fs::read_to_string(session_dir.join(METADATA_FILENAME)).with_context(|| {
        format!(
            "failed to read {}",
            session_dir.join(METADATA_FILENAME).display()
        )
    })?;
    serde_json::from_str(&raw).context("failed to parse session metadata")
}

fn load_messages(session_dir: &Path) -> Result<Vec<Message>> {
    let raw = std::fs::read_to_string(session_dir.join(MESSAGES_FILENAME)).with_context(|| {
        format!(
            "failed to read {}",
            session_dir.join(MESSAGES_FILENAME).display()
        )
    })?;
    let mut messages = vec![Message::text(Role::System, super::system_prompt())];
    for line in raw.lines() {
        let value =
            serde_json::from_str::<Value>(line).context("failed to parse session message")?;
        messages.push(message_from_session_json(&value)?);
    }
    Ok(messages)
}

async fn append_messages(session_dir: &Path, messages: &[Value]) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let path = session_dir.join(MESSAGES_FILENAME);
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
        .with_context(|| format!("failed to open {}", path.display()))?;
    for message in messages {
        file.write_all(serde_json::to_string(message)?.as_bytes())
            .await?;
        file.write_all(b"\n").await?;
        file.flush().await?;
    }
    Ok(())
}

async fn write_json_atomic(path: &Path, value: &Value) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, serde_json::to_string_pretty(value)?)
        .await
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("failed to replace {}", path.display()))
}

fn session_message_json(message: &Message) -> Value {
    let mut object = serde_json::Map::new();
    object.insert(
        "role".to_string(),
        Value::String(role_name(message.role).to_string()),
    );
    object.insert("content".to_string(), Value::String(text_content(message)));
    if let Some(images) = message.images.as_ref().filter(|images| !images.is_empty()) {
        object.insert(
            "images".to_string(),
            serde_json::to_value(images).unwrap_or(Value::Null),
        );
    }
    object.insert("injected".to_string(), Value::Bool(message.injected));
    if message.role != Role::Tool {
        object.insert(
            "message_id".to_string(),
            Value::String(
                message
                    .message_id
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            ),
        );
    }
    if message.role == Role::User
        && let Some(display_content) = &message.display_content
    {
        object.insert("user_display_content".to_string(), display_content.clone());
    }
    if message.role == Role::Assistant {
        if let Some(reasoning) = message
            .reasoning_content
            .as_ref()
            .filter(|reasoning| !reasoning.is_empty())
        {
            object.insert(
                "reasoning_content".to_string(),
                Value::String(reasoning.clone()),
            );
            object.insert(
                "reasoning_message_id".to_string(),
                Value::String(
                    message
                        .reasoning_message_id
                        .clone()
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                ),
            );
        }
        let calls = message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall(call) => Some(json!({
                    "id": call.id,
                    "function": {
                        "name": call.name,
                        "arguments": call.arguments.to_string()
                    },
                    "type": "function"
                })),
                _ => None,
            })
            .collect::<Vec<_>>();
        if !calls.is_empty() {
            object.insert("tool_calls".to_string(), Value::Array(calls));
        }
    }
    if let Some(result) = message.content.iter().find_map(|block| match block {
        ContentBlock::ToolResult(result) => Some(result),
        _ => None,
    }) {
        object.insert("content".to_string(), Value::String(result.output.clone()));
        object.insert(
            "tool_call_id".to_string(),
            Value::String(result.call_id.clone()),
        );
        object.insert("name".to_string(), Value::String(result.name.clone()));
    }
    Value::Object(object)
}

fn message_from_session_json(value: &Value) -> Result<Message> {
    let role = match value
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        "system" => Role::System,
        other => bail!("unsupported session message role: {other}"),
    };
    if role == Role::Tool {
        return Ok(Message {
            role,
            content: vec![ContentBlock::ToolResult(microvibe_protocol::ToolResult {
                call_id: value
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                name: value
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                output: value
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                success: true,
            })],
            message_id: None,
            reasoning_content: None,
            reasoning_message_id: None,
            injected: value
                .get("injected")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            images: None,
            display_content: None,
        });
    }
    let mut content = Vec::new();
    if let Some(text) = value.get("content").and_then(Value::as_str)
        && !text.is_empty()
    {
        content.push(ContentBlock::Text {
            text: text.to_string(),
        });
    }
    for call in value
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let function = call.get("function").unwrap_or(&Value::Null);
        let arguments = function
            .get("arguments")
            .and_then(Value::as_str)
            .and_then(|raw| serde_json::from_str(raw).ok())
            .unwrap_or(Value::Null);
        content.push(ContentBlock::ToolCall(ToolCall {
            id: call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            name: function
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            arguments,
        }));
    }
    if content.is_empty() {
        content.push(ContentBlock::Text {
            text: String::new(),
        });
    }
    let images = value
        .get("images")
        .cloned()
        .and_then(|images| serde_json::from_value::<Vec<ImageAttachment>>(images).ok())
        .filter(|images| !images.is_empty());
    Ok(Message {
        role,
        content,
        message_id: value
            .get("message_id")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        reasoning_content: value
            .get("reasoning_content")
            .and_then(Value::as_str)
            .filter(|reasoning| !reasoning.is_empty())
            .map(ToString::to_string),
        reasoning_message_id: value
            .get("reasoning_message_id")
            .or_else(|| value.get("reasoningMessageId"))
            .and_then(Value::as_str)
            .filter(|message_id| !message_id.is_empty())
            .map(ToString::to_string),
        injected: value
            .get("injected")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        images,
        display_content: value.get("user_display_content").cloned(),
    })
}

fn title_from_messages(messages: &[Message]) -> Option<String> {
    let first_user = messages.iter().find(|message| message.role == Role::User)?;
    let text = first_user
        .display_content
        .as_ref()
        .and_then(display_content_title_text)
        .unwrap_or_else(|| text_content(first_user));
    let title_text = format_session_title_text(&text);
    if title_text.is_empty() {
        return Some("Untitled session".to_string());
    }
    let mut title = title_text
        .chars()
        .take(MAX_TITLE_LENGTH)
        .collect::<String>();
    if title_text.chars().count() > MAX_TITLE_LENGTH {
        title.push('…');
    }
    Some(title)
}

fn display_content_title_text(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    let content = value.get("content").and_then(Value::as_array)?;
    let text = content
        .iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn format_session_title_text(text: &str) -> String {
    collapse_title_whitespace(&render_title_mentions(text))
}

fn render_title_mentions(text: &str) -> String {
    if !text.contains('@') {
        return text.to_string();
    }
    let mut rendered = String::new();
    let mut pos = 0;
    while pos < text.len() {
        let ch = text[pos..].chars().next().expect("pos is char boundary");
        if ch == '@'
            && title_path_anchor(text, pos)
            && let Some((candidate, end)) = extract_title_path_candidate(text, pos + ch.len_utf8())
        {
            rendered.push('@');
            rendered.push_str(&title_mention_name(&candidate));
            pos = end;
            continue;
        }
        rendered.push(ch);
        pos += ch.len_utf8();
    }
    rendered
}

fn collapse_title_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn title_path_anchor(text: &str, pos: usize) -> bool {
    if pos == 0 {
        return true;
    }
    text[..pos]
        .chars()
        .next_back()
        .is_none_or(|ch| !(ch.is_alphanumeric() || ch == '_'))
}

fn extract_title_path_candidate(text: &str, start: usize) -> Option<(String, usize)> {
    if start >= text.len() {
        return None;
    }
    let first = text[start..].chars().next()?;
    if matches!(first, '\'' | '"') {
        let quote_len = first.len_utf8();
        let mut end = start + quote_len;
        while end < text.len() {
            let ch = text[end..].chars().next()?;
            if ch == first {
                return Some((
                    text[start + quote_len..end].to_string(),
                    end + ch.len_utf8(),
                ));
            }
            end += ch.len_utf8();
        }
        return None;
    }

    let mut end = start;
    while end < text.len() {
        let ch = text[end..].chars().next()?;
        if !(ch.is_alphanumeric() || "._/\\-()[]{}~".contains(ch)) {
            break;
        }
        end += ch.len_utf8();
    }
    (end > start).then(|| (text[start..end].to_string(), end))
}

fn title_mention_name(candidate: &str) -> String {
    Path::new(candidate)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(candidate)
        .to_string()
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

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn shorten_session_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn git_value<const N: usize>(args: [&str; N]) -> Value {
    std::process::Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| Value::String(text.trim().to_string()))
        .filter(|value| value.as_str().is_some_and(|text| !text.is_empty()))
        .unwrap_or(Value::Null)
}

#[derive(Debug, Serialize, Deserialize)]
struct _SessionCompatShape {
    role: String,
    content: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use microvibe_protocol::Usage;

    #[tokio::test]
    async fn saves_and_resumes_vibe_session_files() {
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("VIBE_HOME", dir.path());
        }
        let mut store = SessionStore::new(SessionId(
            "12345678-aaaa-bbbb-cccc-123456789abc".to_string(),
        ))
        .unwrap();
        let messages = vec![
            Message::text(Role::System, "system"),
            Message::text(Role::User, "hello"),
            Message::text(Role::Assistant, "world"),
        ];
        store
            .save(
                &messages,
                &Usage {
                    input_tokens: 3,
                    output_tokens: 2,
                },
                &[],
                "model",
                "provider",
            )
            .await
            .unwrap();

        assert!(store.session_dir.join("messages.jsonl").is_file());
        assert!(store.session_dir.join("meta.json").is_file());
        let raw_messages =
            std::fs::read_to_string(store.session_dir.join("messages.jsonl")).unwrap();
        assert!(raw_messages.contains("\"role\":\"user\""));
        assert!(!raw_messages.contains("\"role\":\"system\""));

        let (resumed, loaded) = SessionStore::resume(store.session_dir.clone()).unwrap();
        assert_eq!(resumed.session_id.0, "12345678-aaaa-bbbb-cccc-123456789abc");
        assert_eq!(loaded.len(), 3);
        assert_eq!(text_content(&loaded[1]), "hello");
        assert_eq!(text_content(&loaded[2]), "world");
    }

    fn legacy_session_data(session_id: &str) -> Value {
        json!({
            "metadata": {
                "session_id": session_id,
                "start_time": "2023-01-01T00:00:00",
                "end_time": "2023-01-01T01:00:00",
                "git_commit": "abc123",
                "git_branch": "main",
                "username": "testuser",
                "environment": {"working_directory": "/test"}
            },
            "messages": [
                {"role": "system", "content": "System prompt"},
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": "Hi there!"}
            ]
        })
    }

    fn write_saved_session(root: &Path, name: &str, session_id: &str, cwd: &Path) -> PathBuf {
        let session_dir = root.join(name);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join(MESSAGES_FILENAME),
            "{\"role\":\"user\",\"content\":\"Hello\"}\n",
        )
        .unwrap();
        std::fs::write(
            session_dir.join(METADATA_FILENAME),
            serde_json::to_string(&json!({
                "session_id": session_id,
                "start_time": "2024-01-01T12:00:00Z",
                "end_time": "2024-01-01T12:05:00Z",
                "environment": {"working_directory": cwd.display().to_string()},
                "title": "Valid session"
            }))
            .unwrap(),
        )
        .unwrap();
        session_dir
    }

    #[test]
    fn migrate_legacy_sessions_returns_zero_when_disabled_missing_or_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");

        assert_eq!(
            migrate_legacy_sessions_in(&missing, "test", true).unwrap(),
            0
        );
        assert_eq!(
            migrate_legacy_sessions_in(dir.path(), "test", false).unwrap(),
            0
        );
        assert_eq!(
            migrate_legacy_sessions_in(dir.path(), "test", true).unwrap(),
            0
        );
    }

    #[test]
    fn migrate_legacy_sessions_splits_json_file_like_vibe() {
        let dir = tempfile::tempdir().unwrap();
        let old_file = dir.path().join("test_session-123.json");
        let legacy = legacy_session_data("test-session-123");
        std::fs::write(&old_file, serde_json::to_string(&legacy).unwrap()).unwrap();

        assert_eq!(
            migrate_legacy_sessions_in(dir.path(), "test", true).unwrap(),
            1
        );
        assert!(!old_file.exists());

        let session_dir = dir.path().join("test_session-123");
        let metadata = std::fs::read_to_string(session_dir.join("meta.json")).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&metadata).unwrap(),
            legacy["metadata"]
        );

        let messages = std::fs::read_to_string(session_dir.join("messages.jsonl")).unwrap();
        let parsed = messages
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(parsed, legacy["messages"].as_array().unwrap().clone());
    }

    #[test]
    fn migrate_legacy_sessions_handles_multiple_files() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..3 {
            let path = dir.path().join(format!("test_session-{index:03}.json"));
            let data = legacy_session_data(&format!("test-session-{index}"));
            std::fs::write(path, serde_json::to_string(&data).unwrap()).unwrap();
        }

        assert_eq!(
            migrate_legacy_sessions_in(dir.path(), "test", true).unwrap(),
            3
        );
        for index in 0..3 {
            assert!(dir.path().join(format!("test_session-{index:03}")).is_dir());
            assert!(
                !dir.path()
                    .join(format!("test_session-{index:03}.json"))
                    .exists()
            );
        }
    }

    #[test]
    fn migrate_legacy_sessions_skips_invalid_files_and_continues() {
        let dir = tempfile::tempdir().unwrap();
        let valid_file = dir.path().join("test_session-valid.json");
        let invalid_file = dir.path().join("test_session-invalid.json");
        std::fs::write(
            &valid_file,
            serde_json::to_string(&legacy_session_data("valid-session")).unwrap(),
        )
        .unwrap();
        std::fs::write(&invalid_file, "{invalid json}").unwrap();

        assert_eq!(
            migrate_legacy_sessions_in(dir.path(), "test", true).unwrap(),
            1
        );
        assert!(dir.path().join("test_session-valid").is_dir());
        assert!(!valid_file.exists());
        assert!(invalid_file.exists());
    }

    #[test]
    fn migrate_legacy_sessions_keeps_source_when_target_exists() {
        let dir = tempfile::tempdir().unwrap();
        let old_file = dir.path().join("test_session-123.json");
        std::fs::write(
            &old_file,
            serde_json::to_string(&legacy_session_data("test-session-123")).unwrap(),
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("test_session-123")).unwrap();

        assert_eq!(
            migrate_legacy_sessions_in(dir.path(), "test", true).unwrap(),
            0
        );
        assert!(old_file.exists());
    }

    #[test]
    fn saved_session_listing_skips_invalid_newer_sessions_like_vibe() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("workspace");
        let valid = write_saved_session(
            dir.path(),
            "session_20240101_120000_valid123",
            "valid123-session",
            &cwd,
        );
        let invalid = dir.path().join("session_20240101_130000_invalid1");
        std::fs::create_dir_all(&invalid).unwrap();
        std::fs::write(
            invalid.join(MESSAGES_FILENAME),
            "{\"role\":\"user\",\"content\":\"Newer\"}\n",
        )
        .unwrap();
        std::fs::write(invalid.join(METADATA_FILENAME), "{invalid json}").unwrap();

        let sessions = list_sessions_in(dir.path(), Some(&cwd)).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_dir, valid);
        assert_eq!(sessions[0].session_id.0, "valid123-session");
    }

    #[test]
    fn saved_session_listing_rejects_empty_or_non_object_messages_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("workspace");
        write_saved_session(
            dir.path(),
            "session_20240101_120000_valid123",
            "valid123-session",
            &cwd,
        );

        let empty_messages = dir.path().join("session_20240101_130000_emptymsg");
        std::fs::create_dir_all(&empty_messages).unwrap();
        std::fs::write(empty_messages.join(MESSAGES_FILENAME), "").unwrap();
        std::fs::write(
            empty_messages.join(METADATA_FILENAME),
            "{\"session_id\":\"emptymsg-session\"}",
        )
        .unwrap();

        let list_message = dir.path().join("session_20240101_140000_listmsg");
        std::fs::create_dir_all(&list_message).unwrap();
        std::fs::write(list_message.join(MESSAGES_FILENAME), "[]\n").unwrap();
        std::fs::write(
            list_message.join(METADATA_FILENAME),
            "{\"session_id\":\"listmsg-session\"}",
        )
        .unwrap();

        let list_metadata = dir.path().join("session_20240101_150000_listmeta");
        std::fs::create_dir_all(&list_metadata).unwrap();
        std::fs::write(
            list_metadata.join(MESSAGES_FILENAME),
            "{\"role\":\"user\",\"content\":\"Hello\"}\n",
        )
        .unwrap();
        std::fs::write(list_metadata.join(METADATA_FILENAME), "[]").unwrap();

        let sessions = list_sessions_in(dir.path(), Some(&cwd)).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id.0, "valid123-session");
    }

    #[test]
    fn saved_session_listing_orders_by_messages_mtime_like_vibe() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("workspace");
        write_saved_session(
            dir.path(),
            "session_20240101_120000_older123",
            "older123-session",
            &cwd,
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_saved_session(
            dir.path(),
            "session_20240101_120001_newer123",
            "newer123-session",
            &cwd,
        );

        let sessions = list_sessions_in(dir.path(), Some(&cwd)).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id.0, "newer123-session");
        assert_eq!(sessions[1].session_id.0, "older123-session");
    }

    #[test]
    fn auto_title_renders_path_mentions_as_basenames_like_vibe() {
        let mut message = Message::text(Role::User, "look /tmp/workspace/image.png");
        message.display_content = Some(Value::String(
            "look @/tmp/workspace/image.png\nplease".to_string(),
        ));

        assert_eq!(
            title_from_messages(&[message]),
            Some("look @image.png please".to_string())
        );
    }
}
