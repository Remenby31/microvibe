use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_message_id: Option<String>,
    #[serde(default)]
    pub injected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImageAttachment>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_content: Option<Value>,
}

impl Message {
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
            message_id: None,
            reasoning_content: None,
            reasoning_message_id: None,
            injected: false,
            images: None,
            display_content: None,
        }
    }

    pub fn text_with_id(role: Role, text: impl Into<String>, message_id: Option<String>) -> Self {
        Self {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
            message_id,
            reasoning_content: None,
            reasoning_message_id: None,
            injected: false,
            images: None,
            display_content: None,
        }
    }

    pub fn injected_text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
            message_id: None,
            reasoning_content: None,
            reasoning_message_id: None,
            injected: true,
            images: None,
            display_content: None,
        }
    }

    pub fn text_with_images_and_display(
        role: Role,
        text: impl Into<String>,
        images: Vec<ImageAttachment>,
        display_content: Option<Value>,
    ) -> Self {
        Self {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
            message_id: None,
            reasoning_content: None,
            reasoning_message_id: None,
            injected: false,
            images: (!images.is_empty()).then_some(images),
            display_content,
        }
    }

    pub fn text_with_images_display_and_id(
        role: Role,
        text: impl Into<String>,
        images: Vec<ImageAttachment>,
        display_content: Option<Value>,
        message_id: Option<String>,
    ) -> Self {
        Self {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
            message_id,
            reasoning_content: None,
            reasoning_message_id: None,
            injected: false,
            images: (!images.is_empty()).then_some(images),
            display_content,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub source: ImageSource,
    pub alias: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageSource {
    File { path: PathBuf },
    Inline { data: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

impl ToolCall {
    pub fn new(name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            arguments,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    pub output: String,
    pub success: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookScope {
    PostAgentTurn,
    BeforeTool,
    AfterTool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookMessageSeverity {
    Ok,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::now_v7().to_string())
    }

    pub fn new_with_suffix(suffix: &str) -> Self {
        let head = Uuid::now_v7().to_string();
        let Some((prefix, _)) = head.rsplit_once('-') else {
            return Self::new();
        };
        Self(format!("{prefix}-{suffix}"))
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentTurn {
    pub session_id: SessionId,
    pub input: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    SessionConfigured {
        model: String,
        provider: String,
    },
    TurnStarted {
        id: String,
    },
    AssistantDelta {
        text: String,
    },
    ThoughtDelta {
        text: String,
        message_id: String,
    },
    ToolCallStarted {
        call: ToolCall,
    },
    ToolCallCompleted {
        result: ToolResult,
    },
    UsageUpdated {
        usage: Usage,
    },
    HookRunStarted {
        scope: HookScope,
        tool_name: Option<String>,
        tool_call_id: Option<String>,
    },
    HookStarted {
        hook_name: String,
        scope: HookScope,
        tool_call_id: Option<String>,
    },
    HookEnded {
        hook_name: String,
        status: HookMessageSeverity,
        content: Option<String>,
        scope: HookScope,
        tool_call_id: Option<String>,
    },
    HookRunCompleted {
        scope: HookScope,
        tool_call_id: Option<String>,
    },
    TurnCompleted {
        usage: Usage,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    AllowOnce,
    AllowSession,
    AllowAlways,
    Deny,
    Cancelled,
    DenyWithFeedback,
}
