use std::path::PathBuf;

/// Persistent memory file that survives across sessions.
/// The agent can read and write to it via the `memory` tool.
/// Contents are injected into the system prompt.

pub fn memory_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("microvibe").join("memory.md")
}

pub fn load_memory() -> String {
    let path = memory_path();
    if path.exists() {
        std::fs::read_to_string(&path).unwrap_or_default()
    } else {
        String::new()
    }
}

pub fn save_memory(content: &str) -> Result<(), Box<dyn std::error::Error>> {
    let path = memory_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, content)?;
    Ok(())
}

pub fn append_memory(entry: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut current = load_memory();
    if !current.is_empty() && !current.ends_with('\n') {
        current.push('\n');
    }
    current.push_str(entry);
    current.push('\n');
    save_memory(&current)
}
