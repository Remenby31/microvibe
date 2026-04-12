mod agent;
mod approval;
mod compact;
mod config;
mod llm;
mod memory;
mod pricing;
mod project;
mod render;
mod session;
mod task;
mod tools;
mod tui;
mod tui_runner;
mod types;

// Re-export for tui_runner
pub mod main_helpers {
    pub use super::expand_file_mentions;
}

use agent::Agent;
use clap::Parser;
use colored::Colorize;
use config::Config;
use llm::{Backend, LlmClient};
use session::Session;
use std::io::{self, BufRead, Read, Write};

#[derive(Parser)]
#[command(name = "microvibe", version, about = "Ultra-light CLI coding agent in Rust")]
struct Cli {
    /// API base URL (overrides config)
    #[arg(long, env = "MICROVIBE_API_BASE")]
    api_base: Option<String>,

    /// API key (overrides config)
    #[arg(long, env = "MICROVIBE_API_KEY")]
    api_key: Option<String>,

    /// Provider name from config (default: from config file)
    #[arg(long)]
    provider: Option<String>,

    /// Model name (overrides config)
    #[arg(long, short)]
    model: Option<String>,

    /// Run a single prompt then exit (non-interactive)
    #[arg(short, long)]
    prompt: Option<String>,

    /// Auto-approve all tool calls (dangerous)
    #[arg(long)]
    auto_approve: bool,

    /// Resume a previous session by ID
    #[arg(long)]
    resume: Option<String>,

    /// List previous sessions
    #[arg(long)]
    sessions: bool,

    /// Continue the most recent session
    #[arg(short = 'c', long = "continue")]
    continue_session: bool,

    /// Use full TUI mode (ratatui)
    #[arg(long)]
    tui: bool,

    /// Initialize default config file
    #[arg(long)]
    init: bool,
}

fn build_system_prompt() -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".into());

    let git_info = std::process::Command::new("git")
        .args(["log", "--oneline", "-5"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_default();

    let git_branch = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_default();

    let git_status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let s = String::from_utf8_lossy(&o.stdout).to_string();
                let count = s.lines().count();
                if count == 0 {
                    Some("clean".into())
                } else {
                    Some(format!("{} changes", count))
                }
            } else {
                None
            }
        })
        .unwrap_or_default();

    let git_section = if git_info.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nGit: branch={}, status={}\nRecent commits:\n{}",
            git_branch, git_status, git_info
        )
    };

    // Load AGENTS.md project instructions
    let agents_docs = config::load_agents_docs();
    let agents_section = if agents_docs.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", agents_docs)
    };

    // Project scan
    let project_info = project::scan_project();

    // Persistent memory
    let memory_content = memory::load_memory();
    let memory_section = if memory_content.is_empty() {
        String::new()
    } else {
        format!(
            "\n\n# Memory (persistent across sessions)\n{}",
            memory_content.trim()
        )
    };

    format!(
        r#"You are microvibe, a fast CLI coding agent.

# Core behavior
- Help with software engineering: writing, debugging, refactoring, exploring code.
- Be concise and direct. Lead with code, not explanations.
- Always read files before editing. Use search_replace for precise edits.
- Use dedicated tools (read_file, grep, glob) instead of bash equivalents.
- When writing code, prioritize correctness and simplicity.

# Tools
You have: bash, read_file, write_file, search_replace, grep, glob, list_dir, memory_read, memory_write.
- bash: run shell commands. Use for builds, tests, git, installs.
- read_file: read with line numbers. Prefer over `cat`.
- write_file: create new files. Only for new files or complete rewrites.
- search_replace: edit existing files with exact string matching.
- grep: search file contents with regex. Prefer over `grep` in bash.
- glob: find files by pattern. Prefer over `find` in bash.
- list_dir: list directory contents with sizes. Prefer over `ls` in bash.
- memory_read: read persistent memory (survives across sessions).
- memory_write: save a note to persistent memory. Use for user preferences, project conventions, and important context.
When you need to read multiple files, call read_file for each in the same response — they execute in parallel.
When the user tells you to remember something, use memory_write.

# Safety
- Never run destructive commands without confirming.
- Don't commit, push, or deploy without being asked.
- Don't introduce security vulnerabilities.

# Working directory
{cwd}{git_section}{project_info}
Platform: {platform}{agents_section}{memory_section}"#,
        platform = std::env::consts::OS,
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // --init: create default config
    if cli.init {
        Config::ensure_config_dir();
        eprintln!(
            "Config created at {}",
            Config::config_path().display()
        );
        return Ok(());
    }

    // --sessions: list previous sessions
    if cli.sessions {
        let sessions = Session::list_sessions();
        if sessions.is_empty() {
            eprintln!("No sessions found.");
        } else {
            eprintln!("{}", "Sessions:".bold());
            for (id, time, summary) in &sessions {
                eprintln!("  {} {} {}", &id[..8].cyan(), time.dimmed(), summary);
            }
        }
        return Ok(());
    }

    let config = Config::load();

    // Resolve provider
    let provider_name = cli
        .provider
        .unwrap_or_else(|| config.default.provider.clone());
    let provider = config.get_provider(&provider_name).cloned();

    // Resolve API base, key, model
    let api_base = cli.api_base.unwrap_or_else(|| {
        provider
            .as_ref()
            .map(|p| p.api_base.clone())
            .unwrap_or_else(|| "https://api.mistral.ai/v1".to_string())
    });

    let api_key = cli
        .api_key
        .or_else(|| {
            provider
                .as_ref()
                .and_then(|p| config.resolve_api_key(p))
        })
        .or_else(|| std::env::var("MISTRAL_API_KEY").ok())
        .unwrap_or_else(|| {
            eprintln!(
                "{}",
                "Error: No API key found. Set MISTRAL_API_KEY, use --api-key, or configure providers in ~/.config/microvibe/config.toml"
                    .red()
                    .bold()
            );
            eprintln!("  Run: {} to create default config", "microvibe --init".cyan());
            std::process::exit(1);
        });

    let mut model = cli
        .model
        .unwrap_or_else(|| config.default.model.clone());

    let auto_approve = cli.auto_approve || config.default.auto_approve;
    let backend = Backend::from_str(
        &provider
            .as_ref()
            .map(|p| p.backend.clone())
            .unwrap_or_else(|| "openai".into()),
    );

    let client = LlmClient::new(&api_base, &api_key, &model, config.default.temperature, backend);
    let system_prompt = build_system_prompt();
    let mut agent = Agent::new(client, &system_prompt, auto_approve, config.default.max_context_tokens);

    // Task store for background agents
    let task_store = task::new_task_store();

    // Pipe mode: if stdin is not a terminal, read all stdin and prepend to prompt
    let piped_input = if !atty_is_tty() {
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf).ok();
        if buf.trim().is_empty() {
            None
        } else {
            Some(buf)
        }
    } else {
        None
    };

    // Create session for persistence
    let mut current_session = Session::new(&model, &provider_name);

    // --resume: load previous session by ID
    if let Some(ref session_id) = cli.resume {
        match Session::load(session_id) {
            Ok(s) => {
                eprintln!("{} {}", "Resumed session:".green(), &session_id[..8]);
                current_session = s;
                let client = LlmClient::new(&api_base, &api_key, &model, config.default.temperature, backend);
                agent = Agent::new(client, &system_prompt, auto_approve, config.default.max_context_tokens);
            }
            Err(e) => {
                eprintln!("{} {}", "Failed to resume:".red(), e);
            }
        }
    }

    // --continue: resume the most recent session
    if cli.continue_session {
        let sessions = Session::list_sessions();
        if let Some((id, time, summary)) = sessions.first() {
            match Session::load(id) {
                Ok(s) => {
                    eprintln!(
                        "{} {} {} {}",
                        "Continuing:".green(),
                        &id[..8].cyan(),
                        time.dimmed(),
                        summary.dimmed()
                    );
                    current_session = s;
                    let client = LlmClient::new(&api_base, &api_key, &model, config.default.temperature, backend);
                    agent = Agent::new(client, &system_prompt, auto_approve, config.default.max_context_tokens);
                }
                Err(e) => {
                    eprintln!("{} {}", "Failed to continue:".red(), e);
                }
            }
        } else {
            eprintln!("{}", "No previous sessions found.".dimmed());
        }
    }

    // Single prompt mode (with optional piped stdin)
    if let Some(prompt) = cli.prompt {
        let expanded = expand_file_mentions(&prompt);
        let full_prompt = if let Some(ref piped) = piped_input {
            format!("{}\n\n---\nStdin:\n```\n{}\n```", expanded, piped.trim())
        } else {
            expanded
        };
        agent.run_turn(&full_prompt).await?;
        print_stats(&agent, &model);
        current_session.messages = agent.messages().to_vec();
        current_session.stats = agent.stats.clone();
        let _ = current_session.save();
        return Ok(());
    }

    // Pipe-only mode (stdin without -p): use piped content as the prompt
    if let Some(piped) = piped_input {
        agent.run_turn(&piped).await?;
        print_stats(&agent, &model);
        current_session.messages = agent.messages().to_vec();
        current_session.stats = agent.stats.clone();
        let _ = current_session.save();
        return Ok(());
    }

    // TUI mode
    if cli.tui {
        return tui_runner::run_tui(
            &api_base, &api_key, &model, &provider_name,
            config.default.temperature, backend, auto_approve,
            config.default.max_context_tokens, &system_prompt,
        ).await;
    }

    // Interactive REPL
    print_banner(&model, &provider_name);

    let stdin = io::stdin();
    loop {
        eprint!("{}", "\nmicrovibe> ".green().bold());
        io::stderr().flush()?;

        // Multiline input: lines ending with \ continue on the next line
        let mut full_input = String::new();
        loop {
            let mut line = String::new();
            if stdin.lock().read_line(&mut line)? == 0 {
                if full_input.is_empty() {
                    // EOF on first line = exit
                    eprintln!("{}", "Bye!".dimmed());
                    // Final save
                    current_session.messages = agent.messages().to_vec();
                    current_session.stats = agent.stats.clone();
                    let _ = current_session.save();
                    print_stats(&agent, &model);
                    return Ok(());
                }
                break;
            }
            let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
            if trimmed.ends_with('\\') {
                // Continuation line
                full_input.push_str(&trimmed[..trimmed.len() - 1]);
                full_input.push('\n');
                eprint!("{}", "       ... ".dimmed());
                io::stderr().flush()?;
                continue;
            }
            full_input.push_str(trimmed);
            break;
        }

        let input = full_input.trim();
        if input.is_empty() {
            continue;
        }

        // Handle slash commands (including those with arguments)
        let (cmd, cmd_args) = if input.starts_with('/') {
            let mut parts = input.splitn(2, ' ');
            (parts.next().unwrap_or(""), parts.next().unwrap_or(""))
        } else {
            ("", "")
        };

        let handled = match cmd {
            "/quit" | "/exit" | "/q" => break,
            "/clear" => {
                let client =
                    LlmClient::new(&api_base, &api_key, &model, config.default.temperature, backend);
                agent = Agent::new(client, &system_prompt, auto_approve, config.default.max_context_tokens);
                current_session = Session::new(&model, &provider_name);
                eprintln!("{}", "Context cleared.".dimmed());
                true
            }
            "/stats" => {
                print_stats(&agent, &model);
                true
            }
            "/save" => {
                current_session.messages = agent.messages().to_vec();
                current_session.stats = agent.stats.clone();
                match current_session.save() {
                    Ok(_) => eprintln!(
                        "{} {}",
                        "Saved:".green(),
                        &current_session.id[..8]
                    ),
                    Err(e) => eprintln!("{} {}", "Save failed:".red(), e),
                }
                true
            }
            "/sessions" => {
                let sessions = Session::list_sessions();
                if sessions.is_empty() {
                    eprintln!("{}", "No sessions.".dimmed());
                } else {
                    for (id, time, summary) in &sessions {
                        eprintln!("  {} {} {}", &id[..8].cyan(), time.dimmed(), summary);
                    }
                }
                true
            }
            "/undo" => {
                if agent.undo() {
                    eprintln!("{} ({} checkpoints left)", "Undone.".green(), agent.checkpoint_count());
                } else {
                    eprintln!("{}", "Nothing to undo.".dimmed());
                }
                true
            }
            "/compact" => {
                match agent.force_compact().await {
                    Ok(_) => eprintln!("{} ~{} tokens", "Context:".green(), agent.context_tokens()),
                    Err(e) => eprintln!("{} {}", "Compact failed:".red(), e),
                }
                true
            }
            "/context" => {
                let msgs = agent.messages();
                eprintln!("{} messages, ~{} tokens", msgs.len(), agent.context_tokens());
                for (i, m) in msgs.iter().enumerate() {
                    let role = format!("{:?}", m.role);
                    let preview: String = m.content.as_deref().unwrap_or("").chars().take(60).collect();
                    let tools = m.tool_calls.as_ref().map(|t| format!(" [{}tools]", t.len())).unwrap_or_default();
                    eprintln!("  {:>3} {} {}{}", i, role.cyan(), preview.dimmed(), tools.dimmed());
                }
                true
            }
            "/diff" => {
                show_git_diff();
                true
            }
            "/commit" => {
                auto_commit(&mut agent, cmd_args).await;
                true
            }
            "/web" => {
                if cmd_args.is_empty() {
                    eprintln!("{}", "Usage: /web <url>".dimmed());
                } else {
                    fetch_web(&mut agent, cmd_args).await;
                }
                true
            }
            "/export" => {
                export_conversation(&agent, cmd_args);
                true
            }
            "/test" => {
                run_tests(cmd_args).await;
                true
            }
            "/review" => {
                review_changes(&mut agent).await;
                true
            }
            "/branch" => {
                create_branch(cmd_args);
                true
            }
            "/run" => {
                if cmd_args.is_empty() {
                    eprintln!("{}", "Usage: /run <command>".dimmed());
                } else {
                    run_and_discuss(&mut agent, cmd_args).await;
                }
                true
            }
            "/memory" => {
                let mem = memory::load_memory();
                if mem.is_empty() {
                    eprintln!("{}", "Memory is empty. The agent can write to it with memory_write.".dimmed());
                } else {
                    eprintln!("{}", "Persistent memory:".cyan().bold());
                    eprintln!("{}", mem);
                }
                true
            }
            "/task" => {
                if cmd_args.is_empty() {
                    eprintln!("{}", "Usage: /task <prompt> — spawn a background research agent".dimmed());
                } else {
                    task::spawn_task(
                        task_store.clone(),
                        cmd_args.to_string(),
                        api_base.clone(),
                        api_key.clone(),
                        model.clone(),
                        config.default.temperature,
                        backend,
                    )
                    .await;
                }
                true
            }
            "/tasks" => {
                let tasks = task_store.lock().await;
                if tasks.is_empty() {
                    eprintln!("{}", "No background tasks.".dimmed());
                } else {
                    for t in tasks.iter() {
                        let status_str = match &t.status {
                            task::TaskStatus::Running => "running".yellow().to_string(),
                            task::TaskStatus::Completed => "done".green().to_string(),
                            task::TaskStatus::Failed(e) => format!("{}", e.red()),
                        };
                        eprintln!("  {} {} {}", t.id.cyan(), status_str, t.prompt.chars().take(50).collect::<String>().dimmed());
                        if let Some(ref result) = t.result {
                            // Show first few lines of result
                            let preview: String = result.lines().take(5).collect::<Vec<_>>().join("\n");
                            eprintln!("    {}", preview.dimmed());
                            if result.lines().count() > 5 {
                                eprintln!("    {} ({} more lines — /inject {} to add to context)", "...".dimmed(), result.lines().count() - 5, t.id);
                            }
                        }
                    }
                }
                true
            }
            "/inject" => {
                if cmd_args.is_empty() {
                    eprintln!("{}", "Usage: /inject <task-id> — inject task result into context".dimmed());
                } else {
                    let tasks = task_store.lock().await;
                    if let Some(t) = tasks.iter().find(|t| t.id == cmd_args || t.id.starts_with(cmd_args)) {
                        if let Some(ref result) = t.result {
                            let inject_msg = format!(
                                "Background research task completed. Prompt: \"{}\"\n\nResult:\n{}",
                                t.prompt, result
                            );
                            drop(tasks); // release lock before run_turn
                            if let Err(e) = agent.run_turn(&inject_msg).await {
                                eprintln!("{} {}", "Error:".red(), e);
                            }
                        } else {
                            eprintln!("{} Task {} is still {}", "Wait:".yellow(), t.id, t.status);
                        }
                    } else {
                        eprintln!("{} Task not found: {}", "?".yellow(), cmd_args);
                    }
                }
                true
            }
            "/plan" => {
                let plan_prompt = if cmd_args.is_empty() {
                    "Describe what you would do to accomplish the user's last request. List the steps as a numbered plan. Do NOT execute anything yet — just propose the plan."
                } else {
                    cmd_args
                };
                eprintln!("{}", "Plan mode: agent will propose, not execute.".cyan());
                if let Err(e) = agent.run_turn(&format!(
                    "[PLAN MODE — propose a plan, do NOT execute tools]\n{}",
                    plan_prompt
                )).await {
                    eprintln!("{} {}", "Error:".red(), e);
                }
                true
            }
            "/do" => {
                // Execute the last proposed plan
                eprintln!("{}", "Executing plan...".cyan());
                if let Err(e) = agent.run_turn(
                    "Execute the plan you just proposed. Do it step by step, using tools as needed."
                ).await {
                    eprintln!("{} {}", "Error:".red(), e);
                }
                true
            }
            "/model" => {
                if cmd_args.is_empty() {
                    eprintln!("{} {}", "Current model:".cyan(), model.yellow());
                    let p = pricing::get_pricing(&model);
                    eprintln!("  {} ${}/M in, ${}/M out", "Pricing:".dimmed(), p.input, p.output);
                } else {
                    model = cmd_args.to_string();
                    let client = LlmClient::new(&api_base, &api_key, &model, config.default.temperature, backend);
                    agent = Agent::new(client, &system_prompt, auto_approve, config.default.max_context_tokens);
                    current_session = Session::new(&model, &provider_name);
                    let p = pricing::get_pricing(&model);
                    eprintln!("{} {} (${}/M in, ${}/M out)", "Switched to:".green(), model.yellow(), p.input, p.output);
                }
                true
            }
            "/cost" => {
                let s = &agent.stats;
                let p = pricing::get_pricing(&model);
                let cost = s.estimated_cost(p.input, p.output);
                eprintln!("{}", "Cost breakdown:".cyan().bold());
                eprintln!("  Model: {}", model.yellow());
                eprintln!("  Input:  {} tokens x ${}/M = ${:.6}", s.prompt_tokens, p.input, s.prompt_tokens as f64 / 1_000_000.0 * p.input);
                eprintln!("  Output: {} tokens x ${}/M = ${:.6}", s.completion_tokens, p.output, s.completion_tokens as f64 / 1_000_000.0 * p.output);
                eprintln!("  {} ${:.4}", "Total:".bold(), cost);
                true
            }
            "/help" => {
                print_help();
                true
            }
            _ if input.starts_with('/') => {
                eprintln!("{} Unknown command: {}", "?".yellow(), cmd);
                true
            }
            _ => false,
        };

        if handled {
            continue;
        }

        // Expand @file mentions: @path/to/file gets replaced with file contents
        let expanded = expand_file_mentions(input);

        // Run turn with Ctrl+C cancellation
        tokio::select! {
            result = agent.run_turn(&expanded) => {
                if let Err(e) = result {
                    eprintln!("{} {}", "Error:".red().bold(), e);
                }
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\n{}", "Turn cancelled (Ctrl+C). Type /quit to exit.".yellow());
            }
        }

        // Auto-save after each turn
        current_session.messages = agent.messages().to_vec();
        current_session.stats = agent.stats.clone();
        let _ = current_session.save();
    }

    // Final save
    current_session.messages = agent.messages().to_vec();
    current_session.stats = agent.stats.clone();
    let _ = current_session.save();

    print_stats(&agent, &model);
    eprintln!(
        "{} session {}",
        "Saved:".dimmed(),
        &current_session.id[..8]
    );
    Ok(())
}

fn print_stats(agent: &Agent, model: &str) {
    let s = &agent.stats;
    let p = pricing::get_pricing(model);
    eprintln!(
        "\n  {} {} | {} | {} tools | {} turns | ~{} ctx",
        "Session:".dimmed(),
        format!("{}in + {}out = {} tokens", s.prompt_tokens, s.completion_tokens, s.total_tokens())
            .dimmed(),
        format!("${:.4}", s.estimated_cost(p.input, p.output)).dimmed(),
        s.tool_calls,
        s.turns,
        agent.context_tokens(),
    );
}

fn print_banner(model: &str, provider: &str) {
    eprintln!(
        "{}",
        r#"
 _ __ ___ (_) ___ _ __ _____   _(_) |__   ___
| '_ ` _ \| |/ __| '__/ _ \ \ / / | '_ \ / _ \
| | | | | | | (__| | | (_) \ V /| | |_) |  __/
|_| |_| |_|_|\___|_|  \___/ \_/ |_|_.__/ \___|
"#
        .cyan()
    );
    let cwd = std::env::current_dir()
        .map(|p| {
            let home = dirs::home_dir().unwrap_or_default();
            let display = p.display().to_string();
            let home_str = home.display().to_string();
            if display.starts_with(&home_str) {
                format!("~{}", &display[home_str.len()..])
            } else {
                display
            }
        })
        .unwrap_or_else(|_| ".".into());

    eprintln!(
        "  {} {} {}",
        model.yellow(),
        format!("({})", provider).dimmed(),
        format!("| {}", cwd).dimmed()
    );

    // Show detected project info
    let project_info = project::scan_project();
    if !project_info.is_empty() {
        for line in project_info.trim().lines() {
            eprintln!("  {}", line.dimmed());
        }
    }

    eprintln!(
        "  {} | {} | {}",
        "/help".dimmed(),
        "Ctrl+C cancel".dimmed(),
        "-c continue last".dimmed()
    );
}

/// Expand @file mentions in user input.
/// `@src/main.rs` becomes the file contents inline.
/// `@src/main.rs:10-20` reads lines 10-20 only.
pub fn expand_file_mentions(input: &str) -> String {
    let mut result = input.to_string();
    let mut expansions: Vec<(String, String)> = Vec::new();

    for word in input.split_whitespace() {
        if !word.starts_with('@') || word.len() < 2 {
            continue;
        }

        let mention = &word[1..]; // strip @

        // Parse optional line range: @file:10-20
        let (path, line_range) = if let Some(colon_pos) = mention.rfind(':') {
            let range_part = &mention[colon_pos + 1..];
            if range_part.contains('-') || range_part.chars().all(|c| c.is_ascii_digit()) {
                (&mention[..colon_pos], Some(range_part))
            } else {
                (mention, None)
            }
        } else {
            (mention, None)
        };

        let p = std::path::Path::new(path);
        if !p.exists() || !p.is_file() {
            continue;
        }

        match std::fs::read_to_string(p) {
            Ok(content) => {
                let lines: Vec<&str> = content.lines().collect();
                let selected = if let Some(range) = line_range {
                    let parts: Vec<&str> = range.split('-').collect();
                    let start = parts[0].parse::<usize>().unwrap_or(1).saturating_sub(1);
                    let end = if parts.len() > 1 {
                        parts[1].parse::<usize>().unwrap_or(lines.len())
                    } else {
                        start + 1
                    };
                    let end = end.min(lines.len());
                    lines[start..end]
                        .iter()
                        .enumerate()
                        .map(|(i, l)| format!("{:>5}| {}", start + i + 1, l))
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    // Full file, but truncate if huge
                    let max_lines = 200;
                    let selected_lines: Vec<String> = lines
                        .iter()
                        .take(max_lines)
                        .enumerate()
                        .map(|(i, l)| format!("{:>5}| {}", i + 1, l))
                        .collect();
                    let mut s = selected_lines.join("\n");
                    if lines.len() > max_lines {
                        s.push_str(&format!("\n... ({} more lines)", lines.len() - max_lines));
                    }
                    s
                };

                eprintln!(
                    "  {} {} ({} lines)",
                    "@file:".cyan().bold(),
                    path,
                    selected.lines().count()
                );

                let replacement = format!(
                    "\n\n<file path=\"{}\">\n{}\n</file>\n",
                    path, selected
                );
                expansions.push((word.to_string(), replacement));
            }
            Err(_) => continue,
        }
    }

    for (mention, content) in expansions {
        result = result.replace(&mention, &content);
    }

    result
}

/// Detect test runner and run tests
async fn run_tests(extra_args: &str) {
    let cwd = std::env::current_dir().unwrap_or_default();

    // Detect test runner from project files
    let (cmd, runner_name) = if cwd.join("Cargo.toml").exists() {
        ("cargo test", "cargo")
    } else if cwd.join("package.json").exists() {
        if cwd.join("bun.lockb").exists() {
            ("bun test", "bun")
        } else if cwd.join("pnpm-lock.yaml").exists() {
            ("pnpm test", "pnpm")
        } else {
            ("npm test", "npm")
        }
    } else if cwd.join("pyproject.toml").exists() || cwd.join("setup.py").exists() {
        if cwd.join("pytest.ini").exists()
            || cwd.join("pyproject.toml").exists()
            || cwd.join("conftest.py").exists()
        {
            ("pytest", "pytest")
        } else {
            ("python -m unittest discover", "unittest")
        }
    } else if cwd.join("go.mod").exists() {
        ("go test ./...", "go")
    } else if cwd.join("Makefile").exists() {
        ("make test", "make")
    } else {
        eprintln!("{}", "No test runner detected. Supported: cargo, npm, pytest, go, make".dimmed());
        return;
    };

    let full_cmd = if extra_args.is_empty() {
        cmd.to_string()
    } else {
        format!("{} {}", cmd, extra_args)
    };

    eprintln!("{} {} ({})", "Testing:".cyan().bold(), full_cmd, runner_name);

    match tokio::process::Command::new("bash")
        .arg("-c")
        .arg(&full_cmd)
        .env("TERM", "dumb")
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .await
    {
        Ok(status) => {
            if status.success() {
                eprintln!("{}", "Tests passed.".green().bold());
            } else {
                eprintln!(
                    "{} exit code {}",
                    "Tests failed.".red().bold(),
                    status.code().unwrap_or(-1)
                );
            }
        }
        Err(e) => eprintln!("{} {}", "Failed to run tests:".red(), e),
    }
}

async fn run_and_discuss(agent: &mut Agent, command: &str) {
    eprintln!("{} {}", "Running:".cyan().bold(), command);

    let output = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .env("TERM", "dumb")
        .output()
        .await;

    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let code = output.status.code().unwrap_or(-1);

            // Show output to user
            if !stdout.is_empty() {
                let lines: Vec<&str> = stdout.lines().collect();
                for line in lines.iter().take(30) {
                    eprintln!("  {}", line);
                }
                if lines.len() > 30 {
                    eprintln!("  {} ({} more lines)", "...".dimmed(), lines.len() - 30);
                }
            }
            if code != 0 {
                eprintln!("  {} exit code {}", "Error:".red(), code);
            }

            // Build context for discussion
            let mut result = String::new();
            if code != 0 {
                result.push_str(&format!("Exit code: {}\n", code));
            }
            let truncated_stdout = if stdout.len() > 8000 {
                format!("{}...\n(truncated)", &stdout[..8000])
            } else {
                stdout.to_string()
            };
            result.push_str(&truncated_stdout);
            if !stderr.is_empty() {
                let truncated_stderr = if stderr.len() > 2000 {
                    format!("{}...\n(truncated)", &stderr[..2000])
                } else {
                    stderr.to_string()
                };
                result.push_str(&format!("\nSTDERR:\n{}", truncated_stderr));
            }

            let prompt = format!(
                "I ran `{}` and got this output. Analyze it and tell me what's important:\n\n```\n{}\n```",
                command, result
            );

            if let Err(e) = agent.run_turn(&prompt).await {
                eprintln!("{} {}", "Error:".red(), e);
            }
        }
        Err(e) => eprintln!("{} {}", "Failed to run:".red(), e),
    }
}

async fn review_changes(agent: &mut Agent) {
    let diff = std::process::Command::new("git")
        .args(["diff"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    let staged = std::process::Command::new("git")
        .args(["diff", "--staged"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    let full_diff = format!("{}{}", staged, diff);

    if full_diff.trim().is_empty() {
        eprintln!("{}", "No changes to review.".dimmed());
        return;
    }

    let truncated = if full_diff.len() > 12_000 {
        format!("{}...\n(diff truncated)", &full_diff[..12_000])
    } else {
        full_diff
    };

    let prompt = format!(
        "Review the following git diff. Look for:\n- Bugs or logic errors\n- Security issues\n- Missing error handling\n- Code style problems\n- Potential improvements\n\nBe concise. If everything looks good, say so.\n\n```diff\n{}\n```",
        truncated
    );

    if let Err(e) = agent.run_turn(&prompt).await {
        eprintln!("{} {}", "Review failed:".red(), e);
    }
}

fn create_branch(name: &str) {
    if name.is_empty() {
        eprintln!("{}", "Usage: /branch <name>".dimmed());
        return;
    }

    match std::process::Command::new("git")
        .args(["checkout", "-b", name])
        .output()
    {
        Ok(output) => {
            if output.status.success() {
                eprintln!("{} {}", "Created branch:".green(), name);
            } else {
                let err = String::from_utf8_lossy(&output.stderr);
                eprintln!("{} {}", "Failed:".red(), err.trim());
            }
        }
        Err(e) => eprintln!("{} {}", "Error:".red(), e),
    }
}

async fn auto_commit(agent: &mut Agent, msg_override: &str) {
    // Get the diff
    let diff_output = std::process::Command::new("git")
        .args(["diff", "--staged", "--stat"])
        .output();

    let has_staged = diff_output
        .as_ref()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    if !has_staged {
        // Nothing staged, stage all changes
        eprintln!("{}", "No staged changes, staging all...".dimmed());
        let _ = std::process::Command::new("git")
            .args(["add", "-A"])
            .output();
    }

    let diff = std::process::Command::new("git")
        .args(["diff", "--staged"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    if diff.trim().is_empty() {
        eprintln!("{}", "No changes to commit.".dimmed());
        return;
    }

    let commit_msg = if !msg_override.is_empty() {
        msg_override.to_string()
    } else {
        // Ask LLM to generate commit message
        eprintln!("{}", "Generating commit message...".dimmed());
        let truncated_diff = if diff.len() > 8000 {
            format!("{}...\n(diff truncated)", &diff[..8000])
        } else {
            diff.clone()
        };

        let prompt = format!(
            "Generate a concise git commit message for this diff. Output ONLY the commit message, nothing else. Use conventional commit format (feat/fix/refactor/docs/etc). Max 72 chars for the first line.\n\n```diff\n{}\n```",
            truncated_diff
        );

        match agent.run_turn(&prompt).await {
            Ok(_) => {
                // Get the last assistant message
                agent
                    .messages()
                    .iter()
                    .rev()
                    .find(|m| m.role == crate::types::Role::Assistant)
                    .and_then(|m| m.content.clone())
                    .unwrap_or_else(|| "update code".into())
                    .trim()
                    .trim_matches('`')
                    .trim()
                    .to_string()
            }
            Err(e) => {
                eprintln!("{} {}", "Error generating message:".red(), e);
                return;
            }
        }
    };

    eprintln!("\n  {} {}", "Commit:".green().bold(), commit_msg);
    eprint!("  {} ", "Proceed? [y/n]".cyan());
    io::stderr().flush().ok();

    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_ok() && answer.trim().to_lowercase().starts_with('y')
    {
        match std::process::Command::new("git")
            .args(["commit", "-m", &commit_msg])
            .output()
        {
            Ok(output) => {
                let out = String::from_utf8_lossy(&output.stdout);
                eprintln!("{}", out.trim().green());
            }
            Err(e) => eprintln!("{} {}", "Commit failed:".red(), e),
        }
    } else {
        eprintln!("{}", "Commit cancelled.".dimmed());
    }
}

async fn fetch_web(agent: &mut Agent, url: &str) {
    eprintln!("{} {}", "Fetching:".dimmed(), url);

    // Use curl for reliability
    match tokio::process::Command::new("curl")
        .args(["-sL", "--max-time", "15", "-A", "microvibe/0.6", url])
        .output()
        .await
    {
        Ok(output) => {
            let body = String::from_utf8_lossy(&output.stdout);
            if body.is_empty() {
                eprintln!("{}", "Empty response.".dimmed());
                return;
            }

            // Strip HTML tags for readability (rough)
            let clean = strip_html_tags(&body);
            let truncated = if clean.len() > 12_000 {
                format!("{}...\n(truncated)", &clean[..12_000])
            } else {
                clean
            };

            // Inject as context
            let context_msg = format!(
                "I fetched the content from {}. Here it is:\n\n```\n{}\n```\n\nWhat would you like to know about this?",
                url, truncated
            );
            // Add as user message directly for context
            eprintln!(
                "{} {} chars injected into context",
                "Done:".green(),
                truncated.len()
            );

            if let Err(e) = agent.run_turn(&context_msg).await {
                eprintln!("{} {}", "Error:".red(), e);
            }
        }
        Err(e) => eprintln!("{} {}", "Fetch failed:".red(), e),
    }
}

fn strip_html_tags(html: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;

    for c in html.chars() {
        if c == '<' {
            in_tag = true;
            continue;
        }
        if c == '>' {
            in_tag = false;
            continue;
        }
        if !in_tag {
            result.push(c);
        }
    }

    let lines: Vec<&str> = result
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    lines.join("\n")
}

fn export_conversation(agent: &Agent, filename: &str) {
    let path = if filename.is_empty() {
        "conversation.md"
    } else {
        filename
    };

    let mut md = String::new();
    md.push_str("# Microvibe Conversation\n\n");

    for msg in agent.messages() {
        match msg.role {
            crate::types::Role::System => continue, // Skip system prompt
            crate::types::Role::User => {
                md.push_str("## User\n\n");
                md.push_str(msg.content.as_deref().unwrap_or(""));
                md.push_str("\n\n");
            }
            crate::types::Role::Assistant => {
                md.push_str("## Assistant\n\n");
                if let Some(ref content) = msg.content {
                    md.push_str(content);
                    md.push_str("\n\n");
                }
                if let Some(ref tcs) = msg.tool_calls {
                    for tc in tcs {
                        md.push_str(&format!(
                            "**Tool call:** `{}({})`\n\n",
                            tc.function.name,
                            tc.function.arguments.chars().take(100).collect::<String>()
                        ));
                    }
                }
            }
            crate::types::Role::Tool => {
                let name = msg.name.as_deref().unwrap_or("tool");
                let content = msg.content.as_deref().unwrap_or("");
                let preview = if content.len() > 200 {
                    format!("{}...", &content[..200])
                } else {
                    content.to_string()
                };
                md.push_str(&format!(
                    "<details><summary>Tool result: {}</summary>\n\n```\n{}\n```\n</details>\n\n",
                    name, preview
                ));
            }
        }
    }

    match std::fs::write(path, &md) {
        Ok(_) => eprintln!(
            "{} {} ({} bytes)",
            "Exported:".green(),
            path,
            md.len()
        ),
        Err(e) => eprintln!("{} {}", "Export failed:".red(), e),
    }
}

fn show_git_diff() {
    match std::process::Command::new("git")
        .args(["diff", "--stat", "--color=always"])
        .output()
    {
        Ok(output) => {
            let stat = String::from_utf8_lossy(&output.stdout);
            if stat.trim().is_empty() {
                eprintln!("{}", "No changes.".dimmed());
            } else {
                eprintln!("{}", stat);
                // Also show the full diff (truncated)
                if let Ok(full) = std::process::Command::new("git")
                    .args(["diff", "--color=always"])
                    .output()
                {
                    let diff = String::from_utf8_lossy(&full.stdout);
                    let lines: Vec<&str> = diff.lines().collect();
                    for line in lines.iter().take(80) {
                        eprintln!("{}", line);
                    }
                    if lines.len() > 80 {
                        eprintln!(
                            "  {} ({} more lines)",
                            "...".dimmed(),
                            lines.len() - 80
                        );
                    }
                }
            }
        }
        Err(_) => eprintln!("{}", "Not a git repository.".dimmed()),
    }
}

/// Check if stdin is a TTY (for pipe detection)
fn atty_is_tty() -> bool {
    use std::os::unix::io::AsRawFd;
    unsafe { libc::isatty(io::stdin().as_raw_fd()) != 0 }
}

fn print_help() {
    eprintln!(
        "{}",
        r#"
Commands:
  /quit, /q       Exit
  /clear          Clear conversation context
  /stats          Show token usage and cost
  /save           Save current session
  /sessions       List saved sessions
  /undo           Undo last turn (up to 10 checkpoints)
  /compact        Force context compaction now
  /context        Show conversation message list
  /diff           Show git diff of changes
  /commit [msg]   Auto-generate commit message and commit (or use provided msg)
  /review         Ask agent to review current git changes
  /branch <name>  Create a new git branch
  /run <cmd>      Run command and discuss output with agent
  /web <url>      Fetch URL content into context
  /export [file]  Export conversation as markdown (default: conversation.md)
  /test [args]    Detect test runner and run tests (cargo/npm/pytest/go)
  /task <prompt>  Spawn background research agent (read-only)
  /tasks          List background tasks and results
  /inject <id>    Inject task result into conversation
  /plan [prompt]  Agent proposes a plan without executing
  /do             Execute the last proposed plan
  /memory         Show persistent memory contents
  /model [name]   Show or switch model (clears context)
  /cost           Show detailed cost breakdown
  /help           Show this help

Shortcuts:
  Ctrl+C          Cancel current turn (not exit)
  -c              Continue most recent session

Input:
  End a line with \ to continue on the next line (multiline)
  @file.rs        Auto-read file and inject contents into prompt
  @file.rs:10-20  Read specific line range

Pipe mode:
  echo "fix the bug" | microvibe           # stdin as prompt
  git diff | microvibe -p "review this"    # pipe + prompt
  cat file.rs | microvibe -p "explain"     # pipe file contents

Providers:
  --provider mistral     Mistral API (default)
  --provider anthropic   Anthropic Claude (Messages API)
  --provider openai      OpenAI
  --provider local       Local server (llama.cpp, etc.)

Config: ~/.config/microvibe/config.toml
Project: AGENTS.md or CLAUDE.md in project root
"#
        .dimmed()
    );
}
