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
use llm::LlmClient;
use session::Session;
use std::io::{self, BufRead, Write};

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
You have: bash, read_file, write_file, search_replace, grep, glob.
- bash: run shell commands. Use for builds, tests, git, installs.
- read_file: read with line numbers. Prefer over `cat`.
- write_file: create new files. Only for new files or complete rewrites.
- search_replace: edit existing files with exact string matching.
- grep: search file contents with regex. Prefer over `grep` in bash.
- glob: find files by pattern. Prefer over `find` in bash.

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

    let client = LlmClient::new(&api_base, &api_key, &model, config.default.temperature);
    let system_prompt = build_system_prompt();
    let mut agent = Agent::new(client, &system_prompt, auto_approve, config.default.max_context_tokens);

    // Create session for persistence
    let mut current_session = Session::new(&model, &provider_name);

    // --resume: load previous session
    if let Some(ref session_id) = cli.resume {
        match Session::load(session_id) {
            Ok(s) => {
                eprintln!("{} {}", "Resumed session:".green(), &session_id[..8]);
                current_session = s;
                // Rebuild agent with loaded messages
                let client = LlmClient::new(&api_base, &api_key, &model, config.default.temperature);
                agent = Agent::new(client, &system_prompt, auto_approve, config.default.max_context_tokens);
            }
            Err(e) => {
                eprintln!("{} {}", "Failed to resume:".red(), e);
            }
        }
    }

    // Single prompt mode
    if let Some(prompt) = cli.prompt {
        agent.run_turn(&prompt).await?;
        print_stats(&agent);
        // Save session
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

        let mut input = String::new();
        if stdin.lock().read_line(&mut input)? == 0 {
            break; // EOF
        }

        let input = input.trim();
        if input.is_empty() {
            continue;
        }

        match input {
            "/quit" | "/exit" | "/q" => break,
            "/clear" => {
                let client =
                    LlmClient::new(&api_base, &api_key, &model, config.default.temperature);
                agent = Agent::new(client, &system_prompt, auto_approve, config.default.max_context_tokens);
                current_session = Session::new(&model, &provider_name);
                eprintln!("{}", "Context cleared.".dimmed());

                continue;
            }
            "/stats" => {
                print_stats(&agent);
                continue;
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
                continue;
            }
            "/sessions" => {
                let sessions = Session::list_sessions();
                for (id, time, summary) in &sessions {
                    eprintln!("  {} {} {}", &id[..8].cyan(), time.dimmed(), summary);
                }
                continue;
            }
            "/help" => {
                print_help();
                continue;
            }
            _ => {}
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

fn print_help() {
    eprintln!(
        "{}",
        r#"
Commands:
  /quit, /q     Exit
  /clear        Clear conversation context
  /stats        Show token usage and cost
  /save         Save current session
  /sessions     List saved sessions
  /help         Show this help

Flags:
  --auto-approve   Skip tool approval prompts
  --provider X     Use provider X from config
  --model X        Override model name
  --resume ID      Resume a saved session
  --init           Create default config file
  -p "prompt"      Run single prompt and exit

Config: ~/.config/microvibe/config.toml
Project: AGENTS.md or CLAUDE.md in project root
"#
        .dimmed()
    );
}
