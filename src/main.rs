mod agent;
mod llm;
mod tools;
mod types;

use agent::Agent;
use clap::Parser;
use colored::Colorize;
use llm::LlmClient;
use std::io::{self, BufRead, Write};

#[derive(Parser)]
#[command(name = "microvibe", about = "Ultra-light CLI coding agent")]
struct Cli {
    /// API base URL (or set MICROVIBE_API_BASE / MISTRAL_API_BASE)
    #[arg(long, env = "MICROVIBE_API_BASE")]
    api_base: Option<String>,

    /// API key (or set MICROVIBE_API_KEY / MISTRAL_API_KEY)
    #[arg(long, env = "MICROVIBE_API_KEY")]
    api_key: Option<String>,

    /// Model name
    #[arg(long, short, default_value = "codestral-latest")]
    model: String,

    /// Run a single prompt then exit (non-interactive)
    #[arg(short, long)]
    prompt: Option<String>,

    /// Skip tool approval (auto-accept all)
    #[arg(long, default_value = "false")]
    auto_approve: bool,
}

fn get_system_prompt() -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".into());

    let git_info = std::process::Command::new("git")
        .args(["log", "--oneline", "-5"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).to_string())
            } else {
                None
            }
        })
        .unwrap_or_default();

    let git_section = if git_info.is_empty() {
        String::new()
    } else {
        format!("\n\nRecent commits:\n{}", git_info.trim())
    };

    format!(
        r#"You are microvibe, a fast CLI coding agent written in Rust.
You help users with software engineering tasks: writing code, debugging, refactoring, exploring codebases.

You have access to tools: bash, read_file, write_file, search_replace, grep, glob.
Use them to explore and modify the codebase. Always read files before editing them.
Be concise and direct. Output code changes, not explanations unless asked.

Working directory: {cwd}{git_section}
Platform: {platform}
Shell: bash"#,
        platform = std::env::consts::OS,
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let api_base = cli
        .api_base
        .or_else(|| std::env::var("MISTRAL_API_BASE").ok())
        .unwrap_or_else(|| "https://api.mistral.ai/v1".to_string());

    let api_key = cli
        .api_key
        .or_else(|| std::env::var("MISTRAL_API_KEY").ok())
        .unwrap_or_else(|| {
            eprintln!(
                "{}",
                "Error: No API key. Set MISTRAL_API_KEY or use --api-key"
                    .red()
                    .bold()
            );
            std::process::exit(1);
        });

    let client = LlmClient::new(&api_base, &api_key, &cli.model);
    let system_prompt = get_system_prompt();
    let mut agent = Agent::new(client, &system_prompt);

    // Single prompt mode
    if let Some(prompt) = cli.prompt {
        agent.run_turn(&prompt).await?;
        return Ok(());
    }

    // Interactive REPL
    print_banner();

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
                agent = Agent::new(
                    LlmClient::new(&api_base, &api_key, &cli.model),
                    &system_prompt,
                );
                eprintln!("{}", "Context cleared.".dimmed());
                continue;
            }
            "/stats" => {
                eprintln!("Messages: {}", agent.message_count());
                continue;
            }
            _ => {}
        }

        if let Err(e) = agent.run_turn(input).await {
            eprintln!("{} {}", "Error:".red().bold(), e);
        }
    }

    eprintln!("{}", "Bye!".dimmed());
    Ok(())
}

fn print_banner() {
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
        "  {} | {} | {}",
        "Ultra-light coding agent".dimmed(),
        "Rust".yellow(),
        "/quit to exit".dimmed()
    );
}
