mod agent;
mod approval;
mod compact;
mod config;
mod llm;
mod session;
mod tools;
mod types;

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

    format!(
        r#"You are microvibe, a fast CLI coding agent.

# Core behavior
- Help with software engineering: writing, debugging, refactoring, exploring code.
- Be concise and direct. Lead with code, not explanations.
- Always read files before editing. Use search_replace for precise edits.
- Use dedicated tools (read_file, grep, glob) instead of bash equivalents.
- When writing code, prioritize correctness and simplicity.

# Tools
You have: bash, read_file, write_file, search_replace, grep, glob, list_dir.
- bash: run shell commands. Use for builds, tests, git, installs.
- read_file: read with line numbers. Prefer over `cat`.
- write_file: create new files. Only for new files or complete rewrites.
- search_replace: edit existing files with exact string matching.
- grep: search file contents with regex. Prefer over `grep` in bash.
- glob: find files by pattern. Prefer over `find` in bash.
- list_dir: list directory contents with sizes. Prefer over `ls` in bash.
When you need to read multiple files, call read_file for each in the same response — they execute in parallel.

# Safety
- Never run destructive commands without confirming.
- Don't commit, push, or deploy without being asked.
- Don't introduce security vulnerabilities.

# Working directory
{cwd}{git_section}
Platform: {platform}{agents_section}"#,
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

    let model = cli
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

    // --resume: load previous session
    if let Some(ref session_id) = cli.resume {
        match Session::load(session_id) {
            Ok(s) => {
                eprintln!("{} {}", "Resumed session:".green(), &session_id[..8]);
                current_session = s;
                // Rebuild agent with loaded messages
                let client = LlmClient::new(&api_base, &api_key, &model, config.default.temperature, backend);
                agent = Agent::new(client, &system_prompt, auto_approve, config.default.max_context_tokens);
            }
            Err(e) => {
                eprintln!("{} {}", "Failed to resume:".red(), e);
            }
        }
    }

    // Single prompt mode (with optional piped stdin)
    if let Some(prompt) = cli.prompt {
        let full_prompt = if let Some(ref piped) = piped_input {
            format!("{}\n\n---\nStdin:\n```\n{}\n```", prompt, piped.trim())
        } else {
            prompt
        };
        agent.run_turn(&full_prompt).await?;
        print_stats(&agent);
        current_session.messages = agent.messages().to_vec();
        current_session.stats = agent.stats.clone();
        let _ = current_session.save();
        return Ok(());
    }

    // Pipe-only mode (stdin without -p): use piped content as the prompt
    if let Some(piped) = piped_input {
        agent.run_turn(&piped).await?;
        print_stats(&agent);
        current_session.messages = agent.messages().to_vec();
        current_session.stats = agent.stats.clone();
        let _ = current_session.save();
        return Ok(());
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
                    print_stats(&agent);
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
                print_stats(&agent);
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

        if let Err(e) = agent.run_turn(input).await {
            eprintln!("{} {}", "Error:".red().bold(), e);
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

    print_stats(&agent);
    eprintln!(
        "{} session {}",
        "Saved:".dimmed(),
        &current_session.id[..8]
    );
    Ok(())
}

fn print_stats(agent: &Agent) {
    let s = &agent.stats;
    eprintln!(
        "\n  {} {} | {} | {} tools | {} turns | ~{} ctx",
        "Session:".dimmed(),
        format!("{}in + {}out = {} tokens", s.prompt_tokens, s.completion_tokens, s.total_tokens())
            .dimmed(),
        format!("${:.4}", s.estimated_cost(2.0, 6.0)).dimmed(), // rough Mistral pricing
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
    eprintln!(
        "  {} | {} | {} | {}",
        "Ultra-light coding agent".dimmed(),
        model.yellow(),
        provider.dimmed(),
        "/help for commands".dimmed()
    );
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
    let in_script = false;

    for c in html.chars() {
        if c == '<' {
            in_tag = true;
            continue;
        }
        if c == '>' {
            in_tag = false;
            continue;
        }
        if in_tag {
            // Check for script/style tags
            continue;
        }
        if !in_script {
            result.push(c);
        }
    }

    // Collapse whitespace
    let _ = in_script; // suppress warning
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
  /web <url>      Fetch URL content into context
  /export [file]  Export conversation as markdown (default: conversation.md)
  /help           Show this help

Input:
  End a line with \ to continue on the next line (multiline)

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
