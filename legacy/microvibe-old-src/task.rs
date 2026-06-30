use crate::llm::{Backend, LlmClient};
use crate::tools::{execute_tool, tool_definitions};
use crate::types::*;
use colored::Colorize;
use std::sync::Arc;
use tokio::sync::Mutex;

/// A background task that runs an agent independently
#[derive(Debug)]
pub struct BackgroundTask {
    pub id: String,
    pub prompt: String,
    pub status: TaskStatus,
    pub result: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TaskStatus {
    Running,
    Completed,
    Failed(String),
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskStatus::Running => write!(f, "running"),
            TaskStatus::Completed => write!(f, "completed"),
            TaskStatus::Failed(e) => write!(f, "failed: {}", e),
        }
    }
}

pub type TaskStore = Arc<Mutex<Vec<BackgroundTask>>>;

pub fn new_task_store() -> TaskStore {
    Arc::new(Mutex::new(Vec::new()))
}

/// Spawn a background agent task
pub async fn spawn_task(
    store: TaskStore,
    prompt: String,
    api_base: String,
    api_key: String,
    model: String,
    temperature: f32,
    backend: Backend,
) {
    let task_id = uuid::Uuid::new_v4().to_string()[..8].to_string();

    // Register the task
    {
        let mut tasks = store.lock().await;
        tasks.push(BackgroundTask {
            id: task_id.clone(),
            prompt: prompt.clone(),
            status: TaskStatus::Running,
            result: None,
        });
    }

    eprintln!(
        "  {} {} {}",
        "task:".cyan().bold(),
        task_id.cyan(),
        prompt.chars().take(60).collect::<String>().dimmed()
    );

    let store_clone = store.clone();
    let task_id_clone = task_id.clone();

    tokio::spawn(async move {
        let result = run_task_agent(&api_base, &api_key, &model, temperature, backend, &prompt).await;

        let mut tasks = store_clone.lock().await;
        if let Some(task) = tasks.iter_mut().find(|t| t.id == task_id_clone) {
            match result {
                Ok(response) => {
                    task.status = TaskStatus::Completed;
                    task.result = Some(response);
                    eprintln!(
                        "\n  {} {} {}",
                        "task done:".green().bold(),
                        task_id_clone.cyan(),
                        "Use /tasks to see results".dimmed()
                    );
                }
                Err(e) => {
                    task.status = TaskStatus::Failed(e.to_string());
                    eprintln!(
                        "\n  {} {} {}",
                        "task failed:".red().bold(),
                        task_id_clone.cyan(),
                        e.to_string().chars().take(60).collect::<String>().dimmed()
                    );
                }
            }
        }
    });
}

/// Run a lightweight agent for the background task (no approval, limited tools)
async fn run_task_agent(
    api_base: &str,
    api_key: &str,
    model: &str,
    temperature: f32,
    backend: Backend,
    prompt: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let client = LlmClient::new(api_base, api_key, model, temperature, backend);
    let tools = tool_definitions();

    // Only allow read-only tools for background tasks
    let safe_tools: Vec<AvailableTool> = tools
        .into_iter()
        .filter(|t| {
            matches!(
                t.function.name.as_str(),
                "read_file" | "grep" | "glob" | "list_dir" | "memory_read"
            )
        })
        .collect();

    let system = Message::system(
        "You are a research subagent. Answer the user's question by exploring the codebase. \
         You have read-only tools. Be thorough but concise. Return a clear answer.",
    );

    let mut messages = vec![system, Message::user(prompt)];
    let mut final_response = String::new();

    for _ in 0..10 {
        let (msg, _stats) = client.chat(&messages, &safe_tools).await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.to_string().into() })?;

        let has_tools = msg.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty());
        messages.push(msg.clone());

        if !has_tools {
            final_response = msg.content.unwrap_or_default();
            break;
        }

        let tool_calls = msg.tool_calls.unwrap();
        for tc in &tool_calls {
            let args: serde_json::Value =
                serde_json::from_str(&tc.function.arguments).unwrap_or_default();
            let result = execute_tool(&tc.function.name, &args).await;
            messages.push(Message::tool_result(&tc.id, &tc.function.name, &result));
        }
    }

    Ok(final_response)
}
