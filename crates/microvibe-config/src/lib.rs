use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const DEFAULT_CONFIG: &str = r#"
[model]
provider = "mistral"
name = "mistral-medium-3.5[high]"
temperature = 0.1
max_context_tokens = 200000
input_price = 1.5
output_price = 7.5

[providers.mistral]
base_url = "https://api.mistral.ai/v1"
api_key_env = "MISTRAL_API_KEY"
wire_format = "openai_chat"

[permissions]
mode = "ask"
"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub enable_experimental_hooks: bool,
    #[serde(default)]
    pub active_model: Option<String>,
    #[serde(default = "default_agent")]
    pub default_agent: String,
    #[serde(default)]
    pub theme: Option<String>,
    #[serde(default = "default_autocopy_to_clipboard")]
    pub autocopy_to_clipboard: bool,
    #[serde(default)]
    pub voice_mode_enabled: bool,
    #[serde(default)]
    pub bypass_tool_permissions: bool,
    #[serde(default)]
    pub system_prompt_id: Option<String>,
    pub model: ModelConfig,
    #[serde(default)]
    pub models: Vec<UiModelConfig>,
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub permissions: PermissionsConfig,
    #[serde(default)]
    pub tools: BTreeMap<String, ToolPermissionConfig>,
    #[serde(default)]
    pub enabled_tools: Vec<String>,
    #[serde(default)]
    pub disabled_tools: Vec<String>,
    #[serde(default)]
    pub agent_paths: Vec<PathBuf>,
    #[serde(default)]
    pub enabled_agents: Vec<String>,
    #[serde(default)]
    pub disabled_agents: Vec<String>,
    #[serde(default)]
    pub installed_agents: Vec<String>,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HookConfig {
    pub name: String,
    #[serde(rename = "type")]
    pub hook_type: String,
    pub command: String,
    #[serde(default)]
    pub r#match: Option<String>,
    #[serde(default = "default_hook_timeout")]
    pub timeout: f64,
    #[serde(default)]
    pub strict: bool,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookConfigIssue {
    pub file: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookConfigResult {
    pub hooks: Vec<HookConfig>,
    pub issues: Vec<HookConfigIssue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    pub name: String,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_max_context_tokens")]
    pub max_context_tokens: u64,
    #[serde(default = "default_input_price")]
    pub input_price: f64,
    #[serde(default = "default_output_price")]
    pub output_price: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiModelConfig {
    pub name: String,
    pub provider: String,
    pub alias: String,
    #[serde(default = "default_thinking")]
    pub thinking: String,
    #[serde(default)]
    pub supports_images: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key_env: String,
    #[serde(default = "default_backend")]
    pub backend: String,
    #[serde(default)]
    pub browser_auth_base_url: Option<String>,
    #[serde(default)]
    pub browser_auth_api_base_url: Option<String>,
    #[serde(default = "default_wire_format")]
    pub wire_format: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionsConfig {
    #[serde(default = "default_permission_mode")]
    pub mode: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolPermissionConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowlist: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denylist: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sensitive_patterns: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_max_matches: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_patterns: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codeignore_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_read_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_write_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_parent_dirs: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_content_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_todos: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: String,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub disabled_tools: Vec<String>,
    #[serde(default)]
    pub command: Option<toml::Value>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub startup_timeout_sec: Option<f64>,
    #[serde(default)]
    pub tool_timeout_sec: Option<f64>,
    #[serde(default)]
    pub url: Option<String>,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            mode: default_permission_mode(),
        }
    }
}

fn default_temperature() -> f32 {
    0.1
}

fn default_max_context_tokens() -> u64 {
    200_000
}

fn default_input_price() -> f64 {
    1.5
}

fn default_output_price() -> f64 {
    7.5
}

fn default_wire_format() -> String {
    "openai_chat".to_string()
}

fn default_backend() -> String {
    "generic".to_string()
}

fn default_agent() -> String {
    "default".to_string()
}

fn default_permission_mode() -> String {
    "ask".to_string()
}

fn default_autocopy_to_clipboard() -> bool {
    true
}

fn default_thinking() -> String {
    "off".to_string()
}

fn default_hook_timeout() -> f64 {
    60.0
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            return toml::from_str(&raw)
                .with_context(|| format!("failed to parse {}", path.display()));
        }
        toml::from_str(DEFAULT_CONFIG).context("default config must parse")
    }

    pub fn init() -> Result<PathBuf> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        if !path.exists() {
            std::fs::write(&path, DEFAULT_CONFIG.trim_start())
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
        Ok(path)
    }

    pub fn active_provider(&self) -> Result<&ProviderConfig> {
        self.providers
            .get(&self.model.provider)
            .with_context(|| format!("unknown provider '{}'", self.model.provider))
    }

    pub fn save_active_model(alias: &str) -> Result<()> {
        update_config_file(|config| {
            config.active_model = Some(alias.to_string());
            ensure_ui_models(config);
            if let Some(model) = config.models.iter().find(|model| model.alias == alias) {
                config.model.name = model.name.clone();
                config.model.provider = model.provider.clone();
            } else {
                config.model.name = match alias {
                    "devstral-small" => "devstral-small[off]".to_string(),
                    "local" => "local[off]".to_string(),
                    _ => "mistral-medium-3.5[high]".to_string(),
                };
            }
        })
    }

    pub fn save_theme(theme: &str) -> Result<()> {
        update_config_file(|config| {
            config.theme = Some(theme.to_string());
        })
    }

    pub fn save_thinking(level: &str) -> Result<()> {
        update_config_file(|config| {
            ensure_ui_models(config);
            let active = config
                .active_model
                .clone()
                .unwrap_or_else(|| "mistral-medium-3.5".to_string());
            for model in &mut config.models {
                if model.alias == active {
                    model.thinking = level.to_ascii_lowercase();
                }
            }
            if active == "mistral-medium-3.5" {
                config.model.name = format!("mistral-medium-3.5[{}]", level.to_ascii_lowercase());
            }
        })
    }

    pub fn save_autocopy_to_clipboard(enabled: bool) -> Result<()> {
        update_config_file(|config| {
            config.autocopy_to_clipboard = enabled;
        })
    }

    pub fn save_voice_mode_enabled(enabled: bool) -> Result<()> {
        update_config_file(|config| {
            config.voice_mode_enabled = enabled;
        })
    }

    pub fn save_mcp_server_disabled(name: &str, disabled: bool) -> Result<()> {
        update_config_file(|config| {
            if let Some(server) = config
                .mcp_servers
                .iter_mut()
                .find(|server| server.name == name)
            {
                server.disabled = disabled;
            }
        })
    }

    pub fn save_mcp_tool_disabled(
        server_name: &str,
        tool_name: &str,
        disabled: bool,
    ) -> Result<()> {
        update_config_file(|config| {
            let Some(server) = config
                .mcp_servers
                .iter_mut()
                .find(|server| server.name == server_name)
            else {
                return;
            };
            if disabled {
                if !server.disabled_tools.iter().any(|tool| tool == tool_name) {
                    server.disabled_tools.push(tool_name.to_string());
                    server.disabled_tools.sort();
                }
            } else {
                server.disabled_tools.retain(|tool| tool != tool_name);
            }
        })
    }

    pub fn add_tool_allowlist_patterns(tool_name: &str, patterns: &[String]) -> Result<()> {
        update_config_file(|config| {
            let tool = config.tools.entry(tool_name.to_string()).or_default();
            for pattern in patterns {
                let pattern = if tool_name == "bash" {
                    strip_bash_pattern_wildcard(pattern).to_string()
                } else {
                    pattern.clone()
                };
                if !tool.allowlist.contains(&pattern) {
                    tool.allowlist.push(pattern);
                }
            }
            tool.allowlist.sort();
        })
    }

    pub fn set_tool_permission(tool_name: &str, permission: &str) -> Result<()> {
        update_config_file(|config| {
            config
                .tools
                .entry(tool_name.to_string())
                .or_default()
                .permission = Some(permission.to_string());
        })
    }
}

fn strip_bash_pattern_wildcard(pattern: &str) -> &str {
    pattern.strip_suffix(" *").unwrap_or(pattern)
}

fn update_config_file(mut apply: impl FnMut(&mut Config)) -> Result<()> {
    let mut config = Config::load()?;
    apply(&mut config);
    write_config(&config)
}

fn write_config(config: &Config) -> Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let raw = toml::to_string_pretty(config).context("failed to serialize config")?;
    std::fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))
}

fn ensure_ui_models(config: &mut Config) {
    if config.models.is_empty() {
        config.models = vec![
            UiModelConfig {
                name: "mistral-vibe-cli-latest".to_string(),
                provider: "mistral".to_string(),
                alias: "mistral-medium-3.5".to_string(),
                thinking: "high".to_string(),
                supports_images: true,
            },
            UiModelConfig {
                name: "devstral-small-latest".to_string(),
                provider: "mistral".to_string(),
                alias: "devstral-small".to_string(),
                thinking: "off".to_string(),
                supports_images: false,
            },
            UiModelConfig {
                name: "devstral".to_string(),
                provider: "llamacpp".to_string(),
                alias: "local".to_string(),
                thinking: "off".to_string(),
                supports_images: false,
            },
        ];
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| Path::new(".").to_path_buf())
        .join("microvibe")
        .join("config.toml")
}

pub fn load_hooks_from_fs(config: &Config) -> HookConfigResult {
    if !config.enable_experimental_hooks {
        return HookConfigResult::default();
    }
    let mut result = HookConfigResult::default();
    let mut seen_names = std::collections::HashSet::new();
    for path in hook_files() {
        let file_result = load_hooks_file(&path);
        result.issues.extend(file_result.issues);
        for hook in file_result.hooks {
            if !seen_names.insert(hook.name.clone()) {
                result.issues.push(HookConfigIssue {
                    file: path.clone(),
                    message: format!("Duplicate hook name: {:?}", hook.name),
                });
                continue;
            }
            result.hooks.push(hook);
        }
    }
    result
}

fn hook_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        for dir in cwd.ancestors() {
            files.push(dir.join(".vibe").join("hooks.toml"));
        }
    }
    if let Ok(vibe_home) = std::env::var("VIBE_HOME") {
        files.push(PathBuf::from(vibe_home).join("hooks.toml"));
    }
    if let Some(home) = dirs::home_dir() {
        files.push(home.join(".vibe").join("hooks.toml"));
    }
    dedup_config_paths(files)
}

fn dedup_config_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

#[derive(Debug, Deserialize)]
struct HooksTomlRoot {
    #[serde(default)]
    hooks: Vec<HookConfig>,
}

fn load_hooks_file(path: &Path) -> HookConfigResult {
    if !path.is_file() {
        return HookConfigResult::default();
    }
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => {
            return HookConfigResult {
                hooks: Vec::new(),
                issues: vec![HookConfigIssue {
                    file: path.to_path_buf(),
                    message: format!("Failed to parse: {error}"),
                }],
            };
        }
    };
    let root = match toml::from_str::<HooksTomlRoot>(&raw) {
        Ok(root) => root,
        Err(error) => {
            return HookConfigResult {
                hooks: Vec::new(),
                issues: vec![HookConfigIssue {
                    file: path.to_path_buf(),
                    message: format!("Failed to parse: {error}"),
                }],
            };
        }
    };
    let mut hooks = Vec::new();
    let mut issues = Vec::new();
    for (idx, hook) in root.hooks.into_iter().enumerate() {
        let label = if hook.name.is_empty() {
            format!("hooks[{idx}]")
        } else {
            hook.name.clone()
        };
        if hook.name.trim().is_empty() {
            issues.push(HookConfigIssue {
                file: path.to_path_buf(),
                message: format!("{label} - name: Field required"),
            });
            continue;
        }
        if !matches!(
            hook.hook_type.as_str(),
            "post_agent_turn" | "before_tool" | "after_tool"
        ) {
            issues.push(HookConfigIssue {
                file: path.to_path_buf(),
                message: format!("{label} - type: Input should be 'post_agent_turn', 'before_tool' or 'after_tool'"),
            });
            continue;
        }
        if hook.command.trim().is_empty() {
            issues.push(HookConfigIssue {
                file: path.to_path_buf(),
                message: format!("{label} - command: Value error, command must not be empty"),
            });
            continue;
        }
        if hook
            .r#match
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            issues.push(HookConfigIssue {
                file: path.to_path_buf(),
                message: format!("{label} - match: Value error, match must not be empty"),
            });
            continue;
        }
        if hook.r#match.is_some() && hook.hook_type == "post_agent_turn" {
            issues.push(HookConfigIssue {
                file: path.to_path_buf(),
                message: format!("{label} - Value error, match is only valid for tool hooks (before_tool / after_tool)"),
            });
            continue;
        }
        if hook.strict && hook.hook_type == "post_agent_turn" {
            issues.push(HookConfigIssue {
                file: path.to_path_buf(),
                message: format!("{label} - Value error, strict is only valid for tool hooks (before_tool / after_tool)"),
            });
            continue;
        }
        hooks.push(hook);
    }
    HookConfigResult { hooks, issues }
}

pub fn project_instructions() -> String {
    let mut out = Vec::new();
    let Ok(cwd) = std::env::current_dir() else {
        return String::new();
    };
    for dir in cwd.ancestors() {
        for name in ["AGENTS.md", "VIBE.md", "CLAUDE.md"] {
            let path = dir.join(name);
            if let Ok(contents) = std::fs::read_to_string(&path) {
                out.push(format!(
                    "# Instructions from {}\n\n{}",
                    path.display(),
                    contents
                ));
            }
        }
    }
    out.join("\n\n---\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_prices_default_for_legacy_configs() {
        let raw = r#"
[model]
provider = "mistral"
name = "mistral-medium-3.5[high]"

[providers.mistral]
base_url = "https://api.mistral.ai/v1"
api_key_env = "MISTRAL_API_KEY"
"#;
        let config: Config = toml::from_str(raw).unwrap();
        assert_eq!(config.model.input_price, 1.5);
        assert_eq!(config.model.output_price, 7.5);
    }

    #[test]
    fn tool_runtime_configs_parse_from_toml() {
        let raw = r#"
[model]
provider = "mistral"
name = "mistral-medium-3.5[high]"

[providers.mistral]
base_url = "https://api.mistral.ai/v1"
api_key_env = "MISTRAL_API_KEY"

[tools.bash]
allowlist = ["git status"]
max_output_bytes = 42
default_timeout = 7

[tools.write_file]
max_write_bytes = 8
create_parent_dirs = false

[tools.grep]
exclude_patterns = ["target/"]
codeignore_file = ".microvibeignore"
"#;
        let config: Config = toml::from_str(raw).unwrap();
        let bash = config.tools.get("bash").unwrap();
        assert_eq!(bash.allowlist, vec!["git status"]);
        assert_eq!(bash.max_output_bytes, Some(42));
        assert_eq!(bash.default_timeout, Some(7));

        let write_file = config.tools.get("write_file").unwrap();
        assert_eq!(write_file.max_write_bytes, Some(8));
        assert_eq!(write_file.create_parent_dirs, Some(false));

        let grep = config.tools.get("grep").unwrap();
        assert_eq!(
            grep.exclude_patterns.as_ref().unwrap(),
            &vec!["target/".to_string()]
        );
        assert_eq!(grep.codeignore_file.as_deref(), Some(".microvibeignore"));
    }
}
