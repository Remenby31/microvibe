use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const DEFAULT_CONFIG: &str = r#"
[default]
provider = "foundry-anthropic"
model = "claude-opus-4-6"
auto_approve = false
max_context_tokens = 900000
temperature = 0.1

[providers.foundry-anthropic]
api_base = "https://foundry-proxy.cheetah-koi.ts.net/anthropic"
api_key_env = "ANTHROPIC_FOUNDRY_API_KEY"
backend = "anthropic"

[providers.mistral]
api_base = "https://api.mistral.ai/v1"
api_key_env = "MISTRAL_API_KEY"

[providers.anthropic]
api_base = "https://api.anthropic.com/v1"
api_key_env = "ANTHROPIC_API_KEY"
backend = "anthropic"

[providers.openai]
api_base = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

[providers.local]
api_base = "http://localhost:8080/v1"
api_key_env = "LOCAL_API_KEY"
"#;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Config {
    pub default: DefaultConfig,
    pub providers: HashMap<String, ProviderConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct DefaultConfig {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub auto_approve: bool,
    #[serde(default = "default_max_context")]
    pub max_context_tokens: usize,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
}

fn default_max_context() -> usize {
    128_000
}
fn default_temperature() -> f32 {
    0.1
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ProviderConfig {
    pub api_base: String,
    pub api_key_env: String,
    #[serde(default = "default_backend")]
    pub backend: String,
}

fn default_backend() -> String {
    "openai".into()
}

impl Config {
    pub fn load() -> Self {
        let config_path = Self::config_path();
        if config_path.exists() {
            match std::fs::read_to_string(&config_path) {
                Ok(content) => match toml::from_str(&content) {
                    Ok(config) => return config,
                    Err(e) => {
                        eprintln!("Warning: invalid config {}: {}", config_path.display(), e);
                    }
                },
                Err(e) => {
                    eprintln!("Warning: can't read {}: {}", config_path.display(), e);
                }
            }
        }
        // Return default config
        toml::from_str(DEFAULT_CONFIG).expect("default config should parse")
    }

    pub fn config_path() -> PathBuf {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        home.join(".config").join("microvibe").join("config.toml")
    }

    pub fn ensure_config_dir() {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if !path.exists() {
            let _ = std::fs::write(&path, DEFAULT_CONFIG.trim_start());
        }
    }

    pub fn get_provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.get(name)
    }

    pub fn resolve_api_key(&self, provider: &ProviderConfig) -> Option<String> {
        std::env::var(&provider.api_key_env).ok()
    }
}

/// Load AGENTS.md from the project root and parent directories
pub fn load_agents_docs() -> String {
    let mut docs = Vec::new();
    let cwd = std::env::current_dir().unwrap_or_default();

    // Walk up from cwd looking for AGENTS.md or .microvibe/AGENTS.md
    let mut dir = Some(cwd.as_path());
    while let Some(d) = dir {
        for name in &["AGENTS.md", ".microvibe/AGENTS.md", "CLAUDE.md"] {
            let p = d.join(name);
            if p.exists() {
                if let Ok(content) = std::fs::read_to_string(&p) {
                    docs.push(format!(
                        "# Instructions from {}\n\n{}",
                        p.display(),
                        content.trim()
                    ));
                }
            }
        }
        dir = d.parent();
        // Don't go above home
        if dir.map(|d| d == Path::new("/")).unwrap_or(true) {
            break;
        }
    }

    // Also check ~/.config/microvibe/AGENTS.md for global instructions
    let global = dirs::home_dir()
        .map(|h| h.join(".config/microvibe/AGENTS.md"))
        .unwrap_or_default();
    if global.exists() {
        if let Ok(content) = std::fs::read_to_string(&global) {
            docs.push(format!(
                "# Global user instructions\n\n{}",
                content.trim()
            ));
        }
    }

    docs.join("\n\n---\n\n")
}
