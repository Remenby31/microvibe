use crate::types::Message;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub model: String,
    pub provider: String,
    pub created_at: String,
    pub cwd: String,
    pub messages: Vec<Message>,
    pub stats: SessionStats,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SessionStats {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub tool_calls: u64,
    pub turns: u64,
}

impl SessionStats {
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }

    pub fn estimated_cost(&self, input_price_per_m: f64, output_price_per_m: f64) -> f64 {
        (self.prompt_tokens as f64 / 1_000_000.0) * input_price_per_m
            + (self.completion_tokens as f64 / 1_000_000.0) * output_price_per_m
    }
}

impl Session {
    pub fn new(model: &str, provider: &str) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        Self {
            id,
            model: model.to_string(),
            provider: provider.to_string(),
            created_at: chrono_now(),
            cwd,
            messages: Vec::new(),
            stats: SessionStats::default(),
        }
    }

    pub fn sessions_dir() -> PathBuf {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        home.join(".config").join("microvibe").join("sessions")
    }

    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let dir = Self::sessions_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", self.id));
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn load(id: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let path = Self::sessions_dir().join(format!("{}.json", id));
        let content = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&content)?)
    }

    pub fn list_sessions() -> Vec<(String, String, String)> {
        let dir = Self::sessions_dir();
        let mut sessions = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.path().extension().is_some_and(|e| e == "json") {
                    if let Ok(content) = std::fs::read_to_string(entry.path()) {
                        if let Ok(s) = serde_json::from_str::<Session>(&content) {
                            let summary = s
                                .messages
                                .iter()
                                .find(|m| m.role == crate::types::Role::User)
                                .and_then(|m| m.content.as_deref())
                                .map(|c| c.chars().take(60).collect::<String>())
                                .unwrap_or_else(|| "(empty)".into());
                            sessions.push((s.id, s.created_at, summary));
                        }
                    }
                }
            }
        }
        sessions.sort_by(|a, b| b.1.cmp(&a.1));
        sessions
    }
}

fn chrono_now() -> String {
    // Simple ISO-ish timestamp without chrono dependency
    let output = std::process::Command::new("date")
        .arg("+%Y-%m-%dT%H:%M:%S")
        .output()
        .ok();
    output
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}
