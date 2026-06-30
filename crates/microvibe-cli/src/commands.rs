use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct CommandSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    pub handler: &'static str,
    pub exits: bool,
    pub availability: Option<&'static str>,
}

pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "clear",
        aliases: &["/clear"],
        description: "Clear conversation history",
        handler: "_clear_history",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "compact",
        aliases: &["/compact"],
        description: "Compact conversation history by summarizing. Optionally pass instructions to guide the summary",
        handler: "_compact_history",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "config",
        aliases: &["/config"],
        description: "Edit config settings",
        handler: "_show_config",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "copy",
        aliases: &["/copy"],
        description: "Copy the last agent message to the clipboard",
        handler: "_copy_last_agent_message",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "data-retention",
        aliases: &["/data-retention"],
        description: "Show data retention information",
        handler: "_show_data_retention",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "debug",
        aliases: &["/debug"],
        description: "Toggle debug console",
        handler: "action_toggle_debug_console",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "exit",
        aliases: &["/exit", "exit", "quit", ":q", ":quit"],
        description: "Exit the application",
        handler: "_exit_app",
        exits: true,
        availability: None,
    },
    CommandSpec {
        name: "help",
        aliases: &["/help"],
        description: "Show help message",
        handler: "_show_help",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "leanstall",
        aliases: &["/leanstall"],
        description: "Install the Lean 4 agent (leanstral)",
        handler: "_install_lean",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "log",
        aliases: &["/log"],
        description: "Show path to current interaction log file",
        handler: "_show_log_path",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "loop",
        aliases: &["/loop"],
        description: "Schedule a recurring prompt. Use `/loop <interval> <prompt>`, `/loop list`, or `/loop cancel <id|all>`",
        handler: "_loop_command",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "mcp",
        aliases: &["/mcp", "/connectors"],
        description: "Display available MCP servers and connectors. Pass a name to list tools; subcommands: status, login <alias>, logout <alias>",
        handler: "_show_mcp",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "model",
        aliases: &["/model"],
        description: "Select active model",
        handler: "_show_model",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "proxy-setup",
        aliases: &["/proxy-setup"],
        description: "Configure proxy and SSL certificate settings",
        handler: "_show_proxy_setup",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "reload",
        aliases: &["/reload"],
        description: "Reload configuration, agent instructions, and skills from disk",
        handler: "_reload_config",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "rename",
        aliases: &["/rename"],
        description: "Rename the current session",
        handler: "_rename_session",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "resume",
        aliases: &["/resume", "/continue"],
        description: "Browse, resume, or delete saved sessions",
        handler: "_show_session_picker",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "rewind",
        aliases: &["/rewind"],
        description: "Rewind to a previous message",
        handler: "_start_rewind_mode",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "status",
        aliases: &["/status"],
        description: "Display agent statistics",
        handler: "_show_status",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "teleport",
        aliases: &["/teleport"],
        description: "Teleport session to Vibe Code Web",
        handler: "_teleport_command",
        exits: false,
        availability: Some("lambda vibe_code_enabled: vibe_code_enabled"),
    },
    CommandSpec {
        name: "theme",
        aliases: &["/theme"],
        description: "Select theme",
        handler: "_show_theme",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "thinking",
        aliases: &["/thinking"],
        description: "Select thinking level",
        handler: "_show_thinking",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "unleanstall",
        aliases: &["/unleanstall"],
        description: "Uninstall the Lean 4 agent",
        handler: "_uninstall_lean",
        exits: false,
        availability: None,
    },
    CommandSpec {
        name: "voice",
        aliases: &["/voice"],
        description: "Configure voice settings",
        handler: "_show_voice_settings",
        exits: false,
        availability: None,
    },
];

pub fn help_text() -> String {
    let mut lines = vec![
        "### Keyboard Shortcuts".to_string(),
        String::new(),
        "- `Enter` Submit message".to_string(),
        "- `Ctrl+J` / `Shift+Enter` Insert newline".to_string(),
        "- `Escape` Interrupt agent or close dialogs".to_string(),
        "- `Ctrl+C` Quit (or clear input if text present)".to_string(),
        "- `Ctrl+G` Edit input in external editor".to_string(),
        "- `Ctrl+O` Toggle tool output view".to_string(),
        "- `Shift+Tab` Cycle through agents (default, plan, ...)".to_string(),
        format!(
            "- `{}+↑↓` / `Ctrl+P/N` Rewind to previous/next message",
            if cfg!(target_os = "macos") {
                "⌥"
            } else {
                "Alt"
            }
        ),
        String::new(),
        "### Special Features".to_string(),
        String::new(),
        "- `!<command>` Execute bash command directly".to_string(),
        "- `@path/to/file/` Autocompletes file paths".to_string(),
        String::new(),
        "### Commands".to_string(),
        String::new(),
    ];

    let mut commands = COMMANDS.to_vec();
    commands.sort_by_key(|command| command.name);
    for command in commands {
        let canonical = format!("/{}", command.name);
        let mut aliases = command.aliases.to_vec();
        aliases.sort_by_key(|alias| (*alias != canonical, *alias));
        let aliases = aliases
            .into_iter()
            .map(|alias| format!("`{alias}`"))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("- {aliases}: {}", command.description));
    }
    lines.join("\n")
}
