mod commands;

use anyhow::Result;
use clap::Parser;
use microvibe_config::Config;
use microvibe_core::{RunLimits, Session, validate_agent_selection};
use microvibe_protocol::{AgentEvent, ContentBlock, Message, Role, ToolCall};
use serde::Serialize;
use serde_json::{Value, json};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

const VIBE_VERSION: &str = "2.17.1";
const VIBE_USAGE: &str = r#"usage: vibe [-h] [-v] [-p [TEXT]] [--max-turns N] [--max-price DOLLARS] [--max-tokens N] [--enabled-tools TOOL]
            [--output {text,json,streaming}] [--agent NAME | --auto-approve] [--setup] [--check-upgrade]
            [--workdir DIR] [--add-dir DIR] [--trust] [-c | --resume [SESSION_ID]]
            [PROMPT]
"#;
const VIBE_HELP: &str = r#"usage: vibe [-h] [-v] [-p [TEXT]] [--max-turns N] [--max-price DOLLARS] [--max-tokens N] [--enabled-tools TOOL]
            [--output {text,json,streaming}] [--agent NAME | --auto-approve] [--setup] [--check-upgrade]
            [--workdir DIR] [--add-dir DIR] [--trust] [-c | --resume [SESSION_ID]]
            [PROMPT]

Run the Mistral Vibe interactive CLI

positional arguments:
  PROMPT                Initial prompt to start the interactive session with.

options:
  -h, --help            show this help message and exit
  -v, --version         show program's version number and exit
  -p [TEXT], --prompt [TEXT]
                        Run in programmatic mode: send prompt, output response, and exit. Tool approval follows the
                        selected --agent (or 'default_agent' config); pass --auto-approve or --yolo to allow all tool
                        calls.
  --max-turns N         Maximum number of assistant turns (only applies in programmatic mode with -p).
  --max-price DOLLARS   Maximum cost in dollars (only applies in programmatic mode with -p). Session will be
                        interrupted if cost exceeds this limit.
  --max-tokens N        Maximum total prompt + completion tokens across the session (only applies in programmatic mode
                        with -p). Session will be interrupted if usage exceeds this limit.
  --enabled-tools TOOL  Enable specific tools. In programmatic mode (-p), this disables all other tools. Can use exact
                        names, glob patterns (e.g., 'bash*'), or regex with 're:' prefix. Can be specified multiple
                        times.
  --output {text,json,streaming}
                        Output format for programmatic mode (-p): 'text' for human-readable (default), 'json' for all
                        messages at end, 'streaming' for newline-delimited JSON per message.
  --agent NAME          Agent to use (builtin: default, plan, accept-edits, auto-approve, or custom from
                        ~/.vibe/agents/NAME.toml). Defaults to the 'default_agent' config setting in both interactive
                        and programmatic (-p/--prompt) mode.
  --auto-approve, --yolo
                        Shortcut for --agent auto-approve. Approves all tool calls without prompting.
  --setup               Setup API key and exit
  --check-upgrade       Check for a Vibe update now, prompt to install it, and exit
  --workdir DIR         Change to this directory before running
  --add-dir DIR         Additional working directory for file access and context. Implicitly trusted for the session
                        (same semantics as --trust). Can be specified multiple times.
  --trust               Trust the working directory for this invocation only (not persisted to trusted_folders.toml).
                        Skips the trust prompt. Use this for non-interactive automation.
  -c, --continue        Continue from the most recent saved session
  --resume [SESSION_ID]
                        Resume a session. Without SESSION_ID, shows an interactive picker.

Environment variables:
  VIBE_HOME       Override the Vibe home directory (default: ~/.vibe)
  LOG_LEVEL       Logging level: DEBUG, INFO, WARNING (default), ERROR, CRITICAL.
                  Logs are written to $VIBE_HOME/logs/vibe.log.
  LOG_MAX_BYTES   Max size of vibe.log before rotation (default: 10485760).
  VIBE_*          Override any config field (e.g. VIBE_ACTIVE_MODEL=local).
"#;

#[derive(Debug, Parser)]
#[command(
    name = "microvibe",
    version,
    about = "Run the Mistral Vibe interactive CLI"
)]
struct Cli {
    #[arg(value_name = "PROMPT")]
    initial_prompt: Option<String>,

    #[arg(short, long, value_name = "TEXT", num_args = 0..=1, default_missing_value = "")]
    prompt: Option<String>,

    #[arg(long, value_name = "N")]
    max_turns: Option<u32>,

    #[arg(long, value_name = "DOLLARS")]
    max_price: Option<f64>,

    #[arg(long, value_name = "N")]
    max_tokens: Option<u64>,

    #[arg(long, value_name = "TOOL")]
    enabled_tools: Vec<String>,

    #[arg(long, value_parser = ["text", "json", "streaming"], default_value = "text")]
    output: String,

    #[arg(long, value_name = "NAME", conflicts_with = "auto_approve")]
    agent: Option<String>,

    #[arg(long, visible_alias = "yolo", conflicts_with = "agent")]
    auto_approve: bool,

    #[arg(long)]
    setup: bool,

    #[arg(long)]
    check_upgrade: bool,

    #[arg(long, value_name = "DIR")]
    workdir: Option<PathBuf>,

    #[arg(long, value_name = "DIR")]
    add_dir: Vec<PathBuf>,

    #[arg(long)]
    trust: bool,

    #[arg(long, hide = true)]
    teleport: bool,

    #[arg(short = 'c', long = "continue", conflicts_with = "resume")]
    continue_session: bool,

    #[arg(long, value_name = "SESSION_ID", num_args = 0..=1, conflicts_with = "continue_session")]
    resume: Option<Option<String>>,

    #[arg(long, hide = true)]
    tui: bool,

    #[arg(long, hide = true)]
    init: bool,

    #[arg(long, hide = true)]
    dump_parity_inventory: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    if let Some(exit_code) = maybe_print_vibe_builtin_output() {
        std::process::exit(exit_code);
    }
    if let Some(exit_code) = maybe_print_vibe_arg_error() {
        std::process::exit(exit_code);
    }
    let cli = Cli::parse();
    if cli.dump_parity_inventory {
        print_microvibe_inventory()?;
        return Ok(());
    }
    if cli.init {
        let path = Config::init()?;
        println!("Config created at {}", path.display());
        return Ok(());
    }
    if cli.setup {
        run_setup()?;
        return Ok(());
    }
    if cli.check_upgrade {
        run_check_upgrade().await?;
        return Ok(());
    }
    if let Some(workdir) = &cli.workdir {
        let resolved = resolve_cli_path(workdir)?;
        if !resolved.is_dir() {
            print_path_error(
                "Error: --workdir does not exist or is not a directory: ",
                &resolved,
            );
            std::process::exit(1);
        }
        std::env::set_current_dir(&resolved)?;
    }
    for add_dir in &cli.add_dir {
        let resolved = resolve_cli_path(add_dir)?;
        if !resolved.is_dir() {
            print_path_error(
                "Error: --add-dir path does not exist or is not a directory: ",
                add_dir,
            );
            std::process::exit(1);
        }
    }
    if should_resolve_workspace_trust(&cli) {
        maybe_prompt_workspace_trust()?;
    }

    let mut config = Config::load()?;
    if !cli.enabled_tools.is_empty() {
        config.enabled_tools = cli.enabled_tools.clone();
    }
    if cli.auto_approve {
        config.default_agent = "auto-approve".to_string();
    } else if let Some(agent) = &cli.agent {
        config.default_agent = agent.clone();
    }
    if let Err(error) = validate_agent_selection(&config, cli.agent.is_some() || cli.auto_approve) {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }

    if cli.tui {
        return microvibe_tui::run_with_initial_prompt(config, cli.initial_prompt).await;
    }

    if cli.prompt.as_deref() == Some("") {
        eprintln!("Error: No prompt provided for programmatic mode");
        std::process::exit(1);
    }

    if let Some(prompt) = cli.prompt {
        let limits = RunLimits {
            max_turns: cli.max_turns,
            max_tokens: cli.max_tokens,
            max_price: cli.max_price,
        };
        let session = prompt_session(config, cli.continue_session, cli.resume.as_ref())?;
        run_prompt(session, prompt, OutputMode::parse(&cli.output), limits).await?;
        return Ok(());
    }

    if cli.initial_prompt.is_some() {
        return microvibe_tui::run_with_initial_prompt(config, cli.initial_prompt).await;
    }

    microvibe_tui::run(config).await
}

fn maybe_print_vibe_builtin_output() -> Option<i32> {
    let mut args = std::env::args().skip(1);
    if let Some(arg) = args.next()
        && args.next().is_none()
    {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{VIBE_HELP}");
                return Some(0);
            }
            "-v" | "--version" => {
                println!("vibe {VIBE_VERSION}");
                return Some(0);
            }
            _ => {}
        }
    }
    None
}

fn maybe_print_vibe_arg_error() -> Option<i32> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let has_agent = args.iter().any(|arg| arg == "--agent");
    let has_auto_approve = args
        .iter()
        .any(|arg| arg == "--auto-approve" || arg == "--yolo");
    if has_agent && has_auto_approve {
        print_vibe_arg_error("argument --auto-approve/--yolo: not allowed with argument --agent");
        return Some(2);
    }

    for (index, arg) in args.iter().enumerate() {
        let value = if arg == "--output" {
            args.get(index + 1)
                .map(String::as_str)
                .filter(|next| !next.starts_with('-'))
        } else {
            arg.strip_prefix("--output=")
        };
        if let Some(value) = value
            && !matches!(value, "text" | "json" | "streaming")
        {
            print_vibe_arg_error(&format!(
                "argument --output: invalid choice: '{value}' (choose from text, json, streaming)"
            ));
            return Some(2);
        }
    }

    None
}

fn print_vibe_arg_error(message: &str) {
    eprint!("{VIBE_USAGE}");
    eprintln!("vibe: error: {message}");
}

fn print_path_error(prefix: &str, path: &Path) {
    let path = path.display().to_string();
    let columns = std::env::var("COLUMNS")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(120);
    if prefix.chars().count() + path.chars().count() > columns {
        eprintln!("{prefix}");
        eprintln!("{path}");
    } else {
        eprintln!("{prefix}{path}");
    }
}

async fn run_check_upgrade() -> Result<()> {
    match latest_mistral_vibe_version().await {
        Ok(Some(latest)) if is_newer_version(&latest, VIBE_VERSION) => {
            print_update_prompt(&latest);
        }
        Ok(_) => {
            println!("Vibe is already up to date ({VIBE_VERSION}).");
        }
        Err(error) => {
            eprintln!("✗ Update check failed: {error}");
            std::process::exit(1);
        }
    }
    Ok(())
}

async fn latest_mistral_vibe_version() -> Result<Option<String>> {
    if let Ok(version) = std::env::var("VIBE_PARITY_UPDATE_LATEST")
        && !version.trim().is_empty()
    {
        return Ok(Some(version));
    }

    let response: Value = reqwest::Client::new()
        .get("https://pypi.org/pypi/mistral-vibe/json")
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(response
        .get("info")
        .and_then(|info| info.get("version"))
        .and_then(Value::as_str)
        .map(str::to_string))
}

fn print_update_prompt(latest: &str) {
    println!("A new Vibe release is available");
    println!("{VIBE_VERSION} → {latest}");
    println!("› Update now    Cancel upgrade");
    println!("← → navigate  Enter select");
}

fn is_newer_version(candidate: &str, current: &str) -> bool {
    version_parts(candidate) > version_parts(current)
}

fn run_setup() -> Result<()> {
    Config::init()?;
    print_setup_welcome();
    let mut input = SetupInput::new();
    match input.read_key()? {
        SetupKey::Cancel => {
            print_setup_cancelled();
            return Ok(());
        }
        SetupKey::Enter => {}
        _ => {}
    }

    let mut theme_index = 0usize;
    let themes = setup_themes();
    loop {
        print_setup_theme(themes[theme_index]);
        match input.read_key()? {
            SetupKey::Cancel => {
                print_setup_cancelled();
                return Ok(());
            }
            SetupKey::Up => {
                theme_index = theme_index.checked_sub(1).unwrap_or(themes.len() - 1);
            }
            SetupKey::Down => {
                theme_index = (theme_index + 1) % themes.len();
            }
            SetupKey::Enter => {
                let _ = Config::save_theme(themes[theme_index]);
                break;
            }
            _ => {}
        }
    }

    let mut selected_auth = 0usize;
    loop {
        print_setup_auth_method(selected_auth);
        match input.read_key()? {
            SetupKey::Cancel => {
                print_setup_cancelled();
                return Ok(());
            }
            SetupKey::Up | SetupKey::Down => {
                selected_auth = 1 - selected_auth;
            }
            SetupKey::Enter if selected_auth == 0 => {
                print_setup_browser_sign_in();
            }
            SetupKey::Enter => break,
            _ => {}
        }
    }

    print_setup_api_key();
    let api_key = input.read_line_masked()?;
    if api_key.trim().is_empty() {
        print_setup_cancelled();
        return Ok(());
    }
    persist_setup_api_key(api_key.trim())?;
    println!();
    println!("Setup complete 🎉. Run \"vibe\" to start using the Mistral Vibe CLI.");
    println!();
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupKey {
    Enter,
    Up,
    Down,
    Cancel,
    Other,
}

struct SetupInput {
    stdin: io::Stdin,
}

impl SetupInput {
    fn new() -> Self {
        Self { stdin: io::stdin() }
    }

    fn read_key(&mut self) -> Result<SetupKey> {
        let mut stdin = self.stdin.lock();
        let mut byte = [0u8; 1];
        stdin.read_exact(&mut byte)?;
        let key = match byte[0] {
            b'\r' | b'\n' => SetupKey::Enter,
            3 => SetupKey::Cancel,
            27 => {
                let mut seq = [0u8; 2];
                if stdin.read_exact(&mut seq).is_ok() {
                    match seq {
                        [b'[', b'A'] => SetupKey::Up,
                        [b'[', b'B'] => SetupKey::Down,
                        _ => SetupKey::Cancel,
                    }
                } else {
                    SetupKey::Cancel
                }
            }
            _ => SetupKey::Other,
        };
        Ok(key)
    }

    fn read_line_masked(&mut self) -> Result<String> {
        let mut stdin = self.stdin.lock();
        let mut bytes = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stdin.read_exact(&mut byte)?;
            match byte[0] {
                b'\r' | b'\n' => break,
                3 | 27 => return Ok(String::new()),
                b => bytes.push(b),
            }
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn print_setup_welcome() {
    println!("╭──────────────────────────────────────────────────────╮");
    println!("│                                                      │");
    println!("│   Welcome to Mistral Vibe - Let's get you started!   │");
    println!("│                                                      │");
    println!("╰──────────────────────────────────────────────────────╯");
    println!();
    println!("Press Enter ↵");
    let _ = io::stdout().flush();
}

fn setup_themes() -> &'static [&'static str] {
    &[
        "ansi-dark",
        "catppuccin-mocha",
        "dracula",
        "gruvbox",
        "monokai",
        "nord",
        "solarized-dark",
    ]
}

fn print_setup_theme(selected: &str) {
    println!();
    println!("Select your preferred theme");
    println!("Navigate ↑ ↓");
    println!("  ansi-dark");
    println!("  catppuccin-mocha");
    println!("> {selected}");
    println!("  dracula");
    println!("  gruvbox");
    println!("Press Enter ↵");
    println!("╭─ Preview ────────────────────────────────────────────╮");
    println!("│ ### Heading                                          │");
    println!("│ **Bold**, *italic*, and `inline code`.               │");
    println!("│ - Bullet point                                       │");
    println!("╰──────────────────────────────────────────────────────╯");
    let _ = io::stdout().flush();
}

fn print_setup_auth_method(selected: usize) {
    println!();
    println!("Welcome to Mistral Vibe");
    println!("Choose your sign in method");
    if selected == 0 {
        println!("> Launch browser");
    } else {
        println!("  Launch browser");
    }
    println!("  Sign in to Mistral AI Studio and finish setup automatically.");
    println!("or");
    if selected == 1 {
        println!("> Use an API key");
    } else {
        println!("  Use an API key");
    }
    println!("  Already have a key? Paste it manually instead.");
    println!("Use arrows to navigate - Enter Select - Esc Cancel");
    let _ = io::stdout().flush();
}

fn print_setup_browser_sign_in() {
    println!();
    println!("Open browser sign-in is unavailable in this terminal harness.");
    println!("Use arrows to navigate - Enter Select - Esc Cancel");
    let _ = io::stdout().flush();
}

fn print_setup_api_key() {
    println!();
    println!("Get your Mistral API key");
    println!("Visit Mistral Vibe to generate or copy your Vibe key");
    println!("https://chat.mistral.ai/code/extensions?focus=key");
    println!("Paste API key");
    println!("Learn more about Vibe configurations");
    println!("https://github.com/mistralai/mistral-vibe?tab=readme-ov-file#configuration");
    let _ = io::stdout().flush();
}

fn print_setup_cancelled() {
    println!();
    println!("Setup cancelled. See you next time!");
}

fn persist_setup_api_key(api_key: &str) -> Result<()> {
    let env_path = vibe_home_dir().join(".env");
    if let Some(parent) = env_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        env_path,
        format!("MISTRAL_API_KEY='{}'\n", dotenv_single_quote(api_key)),
    )?;
    Ok(())
}

fn dotenv_single_quote(value: &str) -> String {
    value.replace('\n', "").replace('\'', "\\'")
}

fn version_parts(version: &str) -> Vec<u64> {
    version
        .split(['.', '-', '+'])
        .map(|part| {
            part.chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>()
        })
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect()
}

fn should_resolve_workspace_trust(cli: &Cli) -> bool {
    !cli.trust && !cli.check_upgrade && cli.prompt.is_none()
}

fn maybe_prompt_workspace_trust() -> Result<()> {
    let cwd = std::env::current_dir()?;
    if cwd == dirs::home_dir().unwrap_or_else(|| PathBuf::from("~")) {
        return Ok(());
    }
    if workspace_trust_status(&cwd)? == Some(true) || workspace_explicitly_untrusted(&cwd)? {
        return Ok(());
    }
    let repo_root = find_git_repo_ancestor(&cwd)?;
    let detected = trustable_files(&cwd);
    let repo_detected = repo_trustable_files(&cwd, repo_root.as_deref())?;
    if detected.is_empty() && repo_detected.is_empty() {
        return Ok(());
    }
    let offer_repo_trust = repo_root.as_ref().is_some_and(|root| {
        root != &cwd && workspace_trust_status(root).ok().flatten() != Some(true)
    });

    print_workspace_trust_prompt(
        &cwd,
        repo_root.as_deref(),
        &detected,
        &repo_detected,
        offer_repo_trust,
    );
    match read_workspace_trust_decision(offer_repo_trust) {
        WorkspaceTrustChoice::TrustRepo => {
            if let Some(repo_root) = repo_root {
                save_workspace_trust_path(&repo_root, true)?;
            }
        }
        WorkspaceTrustChoice::TrustFolder => save_workspace_trust_path(&cwd, true)?,
        WorkspaceTrustChoice::Decline => save_workspace_trust_path(&cwd, false)?,
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceTrustChoice {
    TrustRepo,
    TrustFolder,
    Decline,
}

fn trustable_files(cwd: &Path) -> Vec<String> {
    let mut files = Vec::new();
    if cwd.join("AGENTS.md").is_file() {
        files.push("AGENTS.md".to_string());
    }
    if cwd.join(".vibe").is_dir() {
        files.push(".vibe/".to_string());
    }
    files
}

fn find_git_repo_ancestor(cwd: &Path) -> Result<Option<PathBuf>> {
    let home = dirs::home_dir()
        .map(|path| path.canonicalize().unwrap_or(path))
        .unwrap_or_else(|| PathBuf::from("~"));
    let mut current = cwd.canonicalize()?;
    loop {
        if current != home && current.join(".git").join("HEAD").is_file() {
            return Ok(Some(current));
        }
        let Some(parent) = current.parent() else {
            return Ok(None);
        };
        if parent == current {
            return Ok(None);
        }
        current = parent.to_path_buf();
    }
}

fn repo_trustable_files(cwd: &Path, repo_root: Option<&Path>) -> Result<Vec<String>> {
    let Some(repo_root) = repo_root else {
        return Ok(Vec::new());
    };
    let cwd = cwd.canonicalize()?;
    let repo_root = repo_root.canonicalize()?;
    if !cwd.starts_with(&repo_root) || cwd == repo_root {
        return Ok(Vec::new());
    }
    let mut files = trustable_files(&repo_root);
    let mut current = cwd
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo_root.clone());
    while current != repo_root {
        if current.join("AGENTS.md").is_file()
            && let Ok(relative) = current.join("AGENTS.md").strip_prefix(&repo_root)
        {
            files.push(relative.display().to_string());
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.to_path_buf();
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn print_workspace_trust_prompt(
    cwd: &Path,
    repo_root: Option<&Path>,
    detected: &[String],
    repo_detected: &[String],
    offer_repo_trust: bool,
) {
    if offer_repo_trust {
        println!("Trust folder or repository?");
    } else {
        println!("Trust this folder?");
    }
    println!("{}", cwd.display());
    if let Some(repo_root) = repo_root.filter(|root| *root != cwd) {
        println!("↳ git repository: {}", repo_root.display());
    }
    println!();
    println!(
        "Files here can modify AI behavior. Malicious configs may exfiltrate data, run destructive commands, or silently alter your code."
    );
    println!();
    if !detected.is_empty() {
        println!("Detected in current folder:");
        for file in detected {
            println!("• {file}");
        }
        println!();
    }
    if !repo_detected.is_empty() {
        println!("Detected in repository context:");
        for file in repo_detected {
            println!("• {file}");
        }
        println!();
    }
    println!("Only trust folders you fully control");
    if offer_repo_trust {
        println!("  1. Trust full repo    2. Trust folder    › 3. Don't trust");
    } else {
        println!("  1. Trust folder    › 2. Don't trust");
    }
    println!("← → navigate  Enter select");
    println!(
        "Setting will be saved in: {}",
        trusted_folders_file().display()
    );
}

fn read_workspace_trust_decision(offer_repo_trust: bool) -> WorkspaceTrustChoice {
    let options: &[WorkspaceTrustChoice] = if offer_repo_trust {
        &[
            WorkspaceTrustChoice::TrustRepo,
            WorkspaceTrustChoice::TrustFolder,
            WorkspaceTrustChoice::Decline,
        ]
    } else {
        &[
            WorkspaceTrustChoice::TrustFolder,
            WorkspaceTrustChoice::Decline,
        ]
    };
    let mut selected = options.len() - 1;
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let mut byte = [0u8; 1];
    while stdin.read_exact(&mut byte).is_ok() {
        match byte {
            [b'1'..=b'3'] => {
                let idx = usize::from(byte[0] - b'1');
                if let Some(choice) = options.get(idx) {
                    return *choice;
                }
            }
            [b'\r' | b'\n'] => return options[selected],
            [b'\x1b'] => {
                let mut seq = [0u8; 2];
                if stdin.read_exact(&mut seq).is_ok() {
                    match seq {
                        [b'[', b'D'] => selected = selected.saturating_sub(1),
                        [b'[', b'C'] => selected = (selected + 1).min(options.len() - 1),
                        _ => {}
                    }
                }
            }
            [_] => {}
        }
    }
    options[selected]
}

fn workspace_trust_status(cwd: &Path) -> Result<Option<bool>> {
    let file = trusted_folders_file();
    let Ok(raw) = fs::read_to_string(file) else {
        return Ok(None);
    };
    let mut current = cwd.canonicalize()?;
    loop {
        let normalized = current.display().to_string();
        if trust_section_contains(&raw, "trusted", &normalized) {
            return Ok(Some(true));
        }
        if trust_section_contains(&raw, "untrusted", &normalized) {
            return Ok(Some(false));
        }
        let Some(parent) = current.parent() else {
            return Ok(None);
        };
        if parent == current {
            return Ok(None);
        }
        current = parent.to_path_buf();
    }
}

fn workspace_explicitly_untrusted(cwd: &Path) -> Result<bool> {
    let file = trusted_folders_file();
    let Ok(raw) = fs::read_to_string(file) else {
        return Ok(false);
    };
    let normalized = normalize_trust_path(cwd)?;
    Ok(trust_section_contains(&raw, "untrusted", &normalized))
}

fn trust_section_contains(raw: &str, section: &str, path: &str) -> bool {
    raw.lines()
        .find(|line| line.trim_start().starts_with(&format!("{section} = [")))
        .is_some_and(|line| line.contains(&format!("\"{}\"", escape_toml_string(path))))
}

fn save_workspace_trust_path(path: &Path, trusted: bool) -> Result<()> {
    let path = normalize_trust_path(path)?;
    let file = trusted_folders_file();
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
    }
    let (trusted_entries, untrusted_entries) = if trusted {
        (vec![path], Vec::new())
    } else {
        (Vec::new(), vec![path])
    };
    fs::write(
        file,
        format!(
            "trusted = [{}]\nuntrusted = [{}]\n",
            toml_array(&trusted_entries),
            toml_array(&untrusted_entries)
        ),
    )?;
    Ok(())
}

fn trusted_folders_file() -> PathBuf {
    vibe_home_dir().join("trusted_folders.toml")
}

fn vibe_home_dir() -> PathBuf {
    if let Ok(vibe_home) = std::env::var("VIBE_HOME") {
        return PathBuf::from(vibe_home);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".vibe")
}

fn normalize_trust_path(path: &Path) -> Result<String> {
    Ok(path.canonicalize()?.display().to_string())
}

fn toml_array(entries: &[String]) -> String {
    entries
        .iter()
        .map(|entry| format!("\"{}\"", escape_toml_string(entry)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn escape_toml_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn resolve_cli_path(path: &Path) -> Result<PathBuf> {
    let expanded = if let Ok(stripped) = path.strip_prefix("~") {
        dirs::home_dir()
            .map(|home| home.join(stripped))
            .unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    };
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(std::env::current_dir()?.join(expanded))
    }
}

fn prompt_session(
    config: Config,
    continue_session: bool,
    resume: Option<&Option<String>>,
) -> Result<Session> {
    if continue_session {
        let cwd = std::env::current_dir()?;
        if let Some(session) = Session::resume_latest_for_cwd(config.clone(), &cwd)? {
            return Ok(session);
        }
        eprintln!("No previous sessions found in ");
        eprintln!("{} for ", session_save_dir_for_display().display());
        eprintln!("cwd=PosixPath('{}')", cwd.display());
        std::process::exit(1);
    }
    if let Some(resume) = resume {
        if let Some(session_id) = resume.as_deref() {
            if let Some(session) = Session::resume_by_id(config.clone(), session_id)? {
                return Ok(session);
            }
            eprintln!("Session '{session_id}' not found in ");
            eprintln!("{}", session_save_dir_for_display().display());
            std::process::exit(1);
        } else {
            let cwd = std::env::current_dir()?;
            if let Some(session) = Session::resume_latest_for_cwd(config.clone(), &cwd)? {
                return Ok(session);
            }
        }
    }
    Ok(Session::new(config))
}

fn session_save_dir_for_display() -> PathBuf {
    if let Ok(vibe_home) = std::env::var("VIBE_HOME") {
        return normalize_private_var_path(PathBuf::from(vibe_home).join("logs").join("session"));
    }
    normalize_private_var_path(
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".vibe")
            .join("logs")
            .join("session"),
    )
}

fn normalize_private_var_path(path: PathBuf) -> PathBuf {
    let rendered = path.display().to_string();
    if rendered.starts_with("/var/folders/") {
        PathBuf::from(format!("/private{rendered}"))
    } else {
        path
    }
}

#[derive(Debug, Serialize)]
struct MicrovibeInventory {
    commands: &'static [commands::CommandSpec],
    help_text: String,
    cli_flags: Vec<CliFlagSpec>,
    tools: Vec<serde_json::Value>,
    tool_permissions: Vec<serde_json::Value>,
    tool_configs: Vec<serde_json::Value>,
    tool_results: Vec<serde_json::Value>,
    agents: Vec<serde_json::Value>,
    bindings: &'static [BindingSpec],
}

#[derive(Debug, Serialize)]
struct CliFlagSpec {
    names: &'static [&'static str],
    help: &'static str,
    action: Option<&'static str>,
    dest: Option<&'static str>,
    metavar: Option<&'static str>,
    choices: Option<&'static [&'static str]>,
    nargs: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct BindingSpec {
    file: &'static str,
    #[serde(rename = "class")]
    class_name: &'static str,
    key: &'static str,
    action: &'static str,
    description: &'static str,
}

const BINDINGS: &[BindingSpec] = &[
    BindingSpec {
        file: "vibe/cli/textual_ui/app.py",
        class_name: "VibeApp",
        key: "ctrl+c",
        action: "interrupt_or_quit",
        description: "Quit",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/app.py",
        class_name: "VibeApp",
        key: "ctrl+d",
        action: "delete_right_or_quit",
        description: "Quit",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/app.py",
        class_name: "VibeApp",
        key: "ctrl+z",
        action: "suspend_with_message",
        description: "Suspend",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/app.py",
        class_name: "VibeApp",
        key: "escape",
        action: "interrupt",
        description: "Interrupt",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/app.py",
        class_name: "VibeApp",
        key: "ctrl+o",
        action: "toggle_tool",
        description: "Toggle Tool",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/app.py",
        class_name: "VibeApp",
        key: "ctrl+y",
        action: "copy_selection",
        description: "Copy",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/app.py",
        class_name: "VibeApp",
        key: "ctrl+shift+c",
        action: "copy_selection",
        description: "Copy",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/app.py",
        class_name: "VibeApp",
        key: "shift+tab",
        action: "cycle_mode",
        description: "Cycle Mode",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/app.py",
        class_name: "VibeApp",
        key: "shift+up",
        action: "scroll_chat_up",
        description: "Scroll Up",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/app.py",
        class_name: "VibeApp",
        key: "shift+down",
        action: "scroll_chat_down",
        description: "Scroll Down",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/app.py",
        class_name: "VibeApp",
        key: "ctrl+g",
        action: "open_plan_in_editor",
        description: "Edit Plan",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/app.py",
        class_name: "VibeApp",
        key: "ctrl+backslash",
        action: "toggle_debug_console",
        description: "Debug Console",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/app.py",
        class_name: "VibeApp",
        key: "alt+up",
        action: "rewind_prev",
        description: "Rewind Previous",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/app.py",
        class_name: "VibeApp",
        key: "ctrl+p",
        action: "rewind_prev",
        description: "Rewind Previous",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/app.py",
        class_name: "VibeApp",
        key: "alt+down",
        action: "rewind_next",
        description: "Rewind Next",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/app.py",
        class_name: "VibeApp",
        key: "ctrl+n",
        action: "rewind_next",
        description: "Rewind Next",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/approval_app.py",
        class_name: "ApprovalApp",
        key: "up",
        action: "move_up",
        description: "Up",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/approval_app.py",
        class_name: "ApprovalApp",
        key: "down",
        action: "move_down",
        description: "Down",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/approval_app.py",
        class_name: "ApprovalApp",
        key: "enter",
        action: "select",
        description: "Select",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/approval_app.py",
        class_name: "ApprovalApp",
        key: "1",
        action: "select_1",
        description: "Yes",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/approval_app.py",
        class_name: "ApprovalApp",
        key: "y",
        action: "select_1",
        description: "Yes",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/approval_app.py",
        class_name: "ApprovalApp",
        key: "2",
        action: "select_2",
        description: "Always Tool Session",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/approval_app.py",
        class_name: "ApprovalApp",
        key: "3",
        action: "select_3",
        description: "Always Permanent",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/approval_app.py",
        class_name: "ApprovalApp",
        key: "4",
        action: "select_4",
        description: "No",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/approval_app.py",
        class_name: "ApprovalApp",
        key: "n",
        action: "select_4",
        description: "No",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/chat_input/text_area.py",
        class_name: "ChatTextArea",
        key: "shift+enter,ctrl+j",
        action: "insert_newline",
        description: "New Line",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/chat_input/text_area.py",
        class_name: "ChatTextArea",
        key: "shift+backspace",
        action: "delete_left",
        description: "Delete character left",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/chat_input/text_area.py",
        class_name: "ChatTextArea",
        key: "shift+delete",
        action: "delete_right",
        description: "Delete character right",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/chat_input/text_area.py",
        class_name: "ChatTextArea",
        key: "ctrl+g",
        action: "open_external_editor",
        description: "External Editor",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/config_app.py",
        class_name: "ConfigApp",
        key: "escape",
        action: "close",
        description: "Close",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/connector_auth_app.py",
        class_name: "ConnectorAuthApp",
        key: "escape",
        action: "close",
        description: "Close",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/connector_auth_app.py",
        class_name: "ConnectorAuthApp",
        key: "backspace",
        action: "close",
        description: "Back",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/connector_auth_app.py",
        class_name: "ConnectorAuthApp",
        key: "r",
        action: "refresh",
        description: "Refresh",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/mcp_app.py",
        class_name: "MCPApp",
        key: "escape",
        action: "close",
        description: "Close",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/mcp_app.py",
        class_name: "MCPApp",
        key: "backspace",
        action: "back",
        description: "Back",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/mcp_app.py",
        class_name: "MCPApp",
        key: "d",
        action: "disable",
        description: "Disable",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/mcp_app.py",
        class_name: "MCPApp",
        key: "e",
        action: "enable",
        description: "Enable",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/mcp_app.py",
        class_name: "MCPApp",
        key: "r",
        action: "refresh",
        description: "Refresh",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/model_picker.py",
        class_name: "ModelPickerApp",
        key: "escape",
        action: "cancel",
        description: "Cancel",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/proxy_setup_app.py",
        class_name: "ProxySetupApp",
        key: "up",
        action: "focus_previous",
        description: "Up",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/proxy_setup_app.py",
        class_name: "ProxySetupApp",
        key: "down",
        action: "focus_next",
        description: "Down",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/question_app.py",
        class_name: "QuestionApp",
        key: "up",
        action: "move_up",
        description: "Up",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/question_app.py",
        class_name: "QuestionApp",
        key: "down",
        action: "move_down",
        description: "Down",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/question_app.py",
        class_name: "QuestionApp",
        key: "enter",
        action: "select",
        description: "Select",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/question_app.py",
        class_name: "QuestionApp",
        key: "escape",
        action: "cancel",
        description: "Cancel",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/rewind_app.py",
        class_name: "RewindApp",
        key: "up",
        action: "move_up",
        description: "Up",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/rewind_app.py",
        class_name: "RewindApp",
        key: "down",
        action: "move_down",
        description: "Down",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/rewind_app.py",
        class_name: "RewindApp",
        key: "enter",
        action: "select",
        description: "Select",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/rewind_app.py",
        class_name: "RewindApp",
        key: "1",
        action: "select_1",
        description: "Option 1",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/rewind_app.py",
        class_name: "RewindApp",
        key: "2",
        action: "select_2",
        description: "Option 2",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/session_picker.py",
        class_name: "SessionPickerApp",
        key: "escape",
        action: "cancel",
        description: "Cancel",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/session_picker.py",
        class_name: "SessionPickerApp",
        key: "d,D",
        action: "request_delete",
        description: "Delete",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/theme_picker.py",
        class_name: "ThemePickerApp",
        key: "escape",
        action: "cancel",
        description: "Cancel",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/thinking_picker.py",
        class_name: "ThinkingPickerApp",
        key: "escape",
        action: "cancel",
        description: "Cancel",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/voice_app.py",
        class_name: "VoiceApp",
        key: "up",
        action: "move_up",
        description: "Up",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/voice_app.py",
        class_name: "VoiceApp",
        key: "down",
        action: "move_down",
        description: "Down",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/voice_app.py",
        class_name: "VoiceApp",
        key: "space",
        action: "toggle_setting",
        description: "Toggle",
    },
    BindingSpec {
        file: "vibe/cli/textual_ui/widgets/voice_app.py",
        class_name: "VoiceApp",
        key: "enter",
        action: "cycle",
        description: "Next",
    },
];

fn print_microvibe_inventory() -> Result<()> {
    let _config = match Config::load() {
        Ok(config) => config,
        Err(_) => {
            Config::init()?;
            Config::load()?
        }
    };
    let tools = microvibe_tools::ToolRegistry::with_builtins()
        .specs()
        .into_iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()?;
    let inventory = MicrovibeInventory {
        commands: commands::COMMANDS,
        help_text: commands::help_text(),
        cli_flags: vec![
            CliFlagSpec {
                names: &["-v", "--version"],
                help: "",
                action: Some("version"),
                dest: None,
                metavar: None,
                choices: None,
                nargs: None,
            },
            CliFlagSpec {
                names: &["initial_prompt"],
                help: "Initial prompt to start the interactive session with.",
                action: None,
                dest: None,
                metavar: Some("PROMPT"),
                choices: None,
                nargs: Some("?"),
            },
            CliFlagSpec {
                names: &["-p", "--prompt"],
                help: "Run in programmatic mode: send prompt, output response, and exit. Tool approval follows the selected --agent (or 'default_agent' config); pass --auto-approve or --yolo to allow all tool calls.",
                action: None,
                dest: None,
                metavar: Some("TEXT"),
                choices: None,
                nargs: Some("?"),
            },
            CliFlagSpec {
                names: &["--max-turns"],
                help: "Maximum number of assistant turns (only applies in programmatic mode with -p).",
                action: None,
                dest: None,
                metavar: Some("N"),
                choices: None,
                nargs: None,
            },
            CliFlagSpec {
                names: &["--max-price"],
                help: "Maximum cost in dollars (only applies in programmatic mode with -p). Session will be interrupted if cost exceeds this limit.",
                action: None,
                dest: None,
                metavar: Some("DOLLARS"),
                choices: None,
                nargs: None,
            },
            CliFlagSpec {
                names: &["--max-tokens"],
                help: "Maximum total prompt + completion tokens across the session (only applies in programmatic mode with -p). Session will be interrupted if usage exceeds this limit.",
                action: None,
                dest: None,
                metavar: Some("N"),
                choices: None,
                nargs: None,
            },
            CliFlagSpec {
                names: &["--enabled-tools"],
                help: "Enable specific tools. In programmatic mode (-p), this disables all other tools. Can use exact names, glob patterns (e.g., 'bash*'), or regex with 're:' prefix. Can be specified multiple times.",
                action: Some("append"),
                dest: None,
                metavar: Some("TOOL"),
                choices: None,
                nargs: None,
            },
            CliFlagSpec {
                names: &["--output"],
                help: "Output format for programmatic mode (-p): 'text' for human-readable (default), 'json' for all messages at end, 'streaming' for newline-delimited JSON per message.",
                action: None,
                dest: None,
                metavar: None,
                choices: Some(&["text", "json", "streaming"]),
                nargs: None,
            },
            CliFlagSpec {
                names: &["--agent"],
                help: "Agent to use (builtin: default, plan, accept-edits, auto-approve, or custom from ~/.vibe/agents/NAME.toml). Defaults to the 'default_agent' config setting in both interactive and programmatic (-p/--prompt) mode.",
                action: None,
                dest: None,
                metavar: Some("NAME"),
                choices: None,
                nargs: None,
            },
            CliFlagSpec {
                names: &["--auto-approve", "--yolo"],
                help: "Shortcut for --agent auto-approve. Approves all tool calls without prompting.",
                action: Some("store_true"),
                dest: None,
                metavar: None,
                choices: None,
                nargs: None,
            },
            CliFlagSpec {
                names: &["--setup"],
                help: "Setup API key and exit",
                action: Some("store_true"),
                dest: None,
                metavar: None,
                choices: None,
                nargs: None,
            },
            CliFlagSpec {
                names: &["--check-upgrade"],
                help: "Check for a Vibe update now, prompt to install it, and exit",
                action: Some("store_true"),
                dest: None,
                metavar: None,
                choices: None,
                nargs: None,
            },
            CliFlagSpec {
                names: &["--workdir"],
                help: "Change to this directory before running",
                action: None,
                dest: None,
                metavar: Some("DIR"),
                choices: None,
                nargs: None,
            },
            CliFlagSpec {
                names: &["--add-dir"],
                help: "Additional working directory for file access and context. Implicitly trusted for the session (same semantics as --trust). Can be specified multiple times.",
                action: Some("append"),
                dest: None,
                metavar: Some("DIR"),
                choices: None,
                nargs: None,
            },
            CliFlagSpec {
                names: &["--trust"],
                help: "Trust the working directory for this invocation only (not persisted to trusted_folders.toml). Skips the trust prompt. Use this for non-interactive automation.",
                action: Some("store_true"),
                dest: None,
                metavar: None,
                choices: None,
                nargs: None,
            },
            CliFlagSpec {
                names: &["--teleport"],
                help: "",
                action: Some("store_true"),
                dest: None,
                metavar: None,
                choices: None,
                nargs: None,
            },
            CliFlagSpec {
                names: &["-c", "--continue"],
                help: "Continue from the most recent saved session",
                action: Some("store_true"),
                dest: Some("continue_session"),
                metavar: None,
                choices: None,
                nargs: None,
            },
            CliFlagSpec {
                names: &["--resume"],
                help: "Resume a session. Without SESSION_ID, shows an interactive picker.",
                action: None,
                dest: None,
                metavar: Some("SESSION_ID"),
                choices: None,
                nargs: Some("?"),
            },
        ],
        tools,
        tool_permissions: microvibe_tools::builtin_tool_permissions(),
        tool_configs: microvibe_tools::builtin_tool_config_schemas(),
        tool_results: microvibe_tools::builtin_result_schemas(),
        agents: builtin_agent_inventory(),
        bindings: BINDINGS,
    };
    println!("{}", serde_json::to_string_pretty(&inventory)?);
    Ok(())
}

fn builtin_agent_inventory() -> Vec<serde_json::Value> {
    vec![
        json!({
            "name": "accept-edits",
            "display_name": "Accept Edits",
            "description": "Auto-approves file edits only",
            "safety": "destructive",
            "agent_type": "agent",
            "overrides": {
                "base_disabled": ["exit_plan_mode"],
                "tools": {
                    "write_file": {"permission": "always"},
                    "edit": {"permission": "always"}
                }
            },
            "install_required": false
        }),
        json!({
            "name": "auto-approve",
            "display_name": "Auto Approve",
            "description": "Auto-approves all tool executions",
            "safety": "yolo",
            "agent_type": "agent",
            "overrides": {
                "bypass_tool_permissions": true,
                "base_disabled": ["exit_plan_mode"]
            },
            "install_required": false
        }),
        json!({
            "name": "default",
            "display_name": "Default",
            "description": "Requires approval for tool executions",
            "safety": "neutral",
            "agent_type": "agent",
            "overrides": {
                "base_disabled": ["exit_plan_mode"]
            },
            "install_required": false
        }),
        json!({
            "name": "explore",
            "display_name": "Explore",
            "description": "Read-only subagent for codebase exploration",
            "safety": "safe",
            "agent_type": "subagent",
            "overrides": {
                "enabled_tools": ["grep", "read"],
                "system_prompt_id": "explore"
            },
            "install_required": false
        }),
        json!({
            "name": "lean",
            "display_name": "Lean",
            "description": "Specialized mode for Lean 4 code analysis, proof assistance, and theorem proving",
            "safety": "neutral",
            "agent_type": "agent",
            "overrides": {
                "system_prompt_id": "lean",
                "active_model": "leanstral",
                "providers": [{
                    "name": "mistral-testing",
                    "api_base": "https://api.mistral.ai/v1",
                    "api_key_env_var": "MISTRAL_API_KEY",
                    "backend": "mistral"
                }],
                "models": [{
                    "name": "labs-leanstral-2603",
                    "provider": "mistral-testing",
                    "alias": "leanstral",
                    "thinking": "high",
                    "temperature": 1.0,
                    "auto_compact_threshold": 168000
                }],
                "compaction_model": {
                    "name": "mistral-small-latest",
                    "provider": "mistral-testing",
                    "alias": "devstral-compact",
                    "temperature": 0.2,
                    "thinking": "off"
                },
                "tools": {"bash": {"default_timeout": 1200}},
                "base_disabled": ["exit_plan_mode"]
            },
            "install_required": true
        }),
        json!({
            "name": "plan",
            "display_name": "Plan",
            "description": "Read-only agent for exploration and planning",
            "safety": "safe",
            "agent_type": "agent",
            "overrides": "_plan_overrides()",
            "install_required": false
        }),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Text,
    Json,
    Streaming,
}

impl OutputMode {
    fn parse(raw: &str) -> Self {
        match raw {
            "json" => Self::Json,
            "streaming" => Self::Streaming,
            _ => Self::Text,
        }
    }
}

async fn run_prompt(
    mut session: Session,
    prompt: String,
    output: OutputMode,
    limits: RunLimits,
) -> Result<()> {
    session
        .agent
        .disable_tools(["ask_user_question", "exit_plan_mode"]);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        let mut final_text = String::new();
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::AssistantDelta { text } => {
                    final_text.push_str(&text);
                }
                AgentEvent::Error { message } => eprintln!("error: {message}"),
                _ => {}
            }
        }
        final_text
    });
    session
        .agent
        .run_turn_programmatic(prompt, tx, limits)
        .await?;
    session.save().await?;
    let final_text = handle.await?;
    let stopped_by_limit = final_text.starts_with("<vibe_stop_event>");
    match output {
        OutputMode::Text => {
            if !final_text.is_empty() {
                println!("{final_text}");
            }
        }
        OutputMode::Json => {
            if stopped_by_limit {
                println!("{final_text}");
                return Ok(());
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&programmatic_messages(session.agent.messages()))?
            );
        }
        OutputMode::Streaming => {
            let mut messages = programmatic_messages(session.agent.messages());
            if stopped_by_limit {
                messages.pop();
            }
            for message in messages {
                println!("{}", serde_json::to_string(&message)?);
            }
            if stopped_by_limit {
                println!("{final_text}");
            }
        }
    }
    Ok(())
}

fn programmatic_messages(messages: &[Message]) -> Vec<Value> {
    messages.iter().map(programmatic_message).collect()
}

fn programmatic_message(message: &Message) -> Value {
    let mut out = json!({
        "role": role_name(message.role),
        "content": text_content(message),
        "images": Value::Null,
        "injected": message.injected,
        "reasoning_content": message
            .reasoning_content
            .as_ref()
            .map(|reasoning| Value::String(reasoning.clone()))
            .unwrap_or(Value::Null),
        "reasoning_state": Value::Null,
        "reasoning_signature": Value::Null,
        "reasoning_message_id": message
            .reasoning_message_id
            .as_ref()
            .map(|message_id| Value::String(message_id.clone()))
            .unwrap_or(Value::Null),
        "tool_calls": Value::Null,
        "name": Value::Null,
        "tool_call_id": Value::Null,
        "message_id": if message.role == Role::Tool {
            Value::Null
        } else {
            Value::String(uuid::Uuid::new_v4().to_string())
        },
        "user_display_content": Value::Null,
    });
    match message.role {
        Role::Assistant => {
            let tool_calls = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall(call) => Some(json!({
                        "id": call.id,
                        "index": Value::Null,
                        "function": {
                            "name": call.name,
                            "arguments": programmatic_tool_arguments(call),
                        },
                        "type": "function",
                    })),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if !tool_calls.is_empty() {
                out["tool_calls"] = Value::Array(tool_calls);
            }
            out
        }
        Role::Tool => {
            if let Some(result) = message.content.iter().find_map(|block| match block {
                ContentBlock::ToolResult(result) => Some(result),
                _ => None,
            }) {
                out["content"] = Value::String(result.output.clone());
                out["tool_call_id"] = Value::String(result.call_id.clone());
                out["name"] = Value::String(result.name.clone());
            }
            out
        }
        Role::User | Role::System => out,
    }
}

fn programmatic_tool_arguments(call: &ToolCall) -> String {
    if call.name == "read"
        && let Some(object) = call.arguments.as_object()
        && object.contains_key("offset")
        && object.contains_key("limit")
    {
        let mut parts = Vec::new();
        if let Some(path) = object.get("file_path").and_then(Value::as_str) {
            parts.push(format!("\"file_path\": {}", json!(path)));
        }
        if let Some(offset) = object.get("offset") {
            parts.push(format!("\"offset\": {}", python_json_value(offset)));
        }
        if let Some(limit) = object.get("limit") {
            parts.push(format!("\"limit\": {}", python_json_value(limit)));
        }
        return format!("{{{}}}", parts.join(", "));
    }
    call.arguments.to_string()
}

fn python_json_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => {
            if *value {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        Value::Number(_) | Value::String(_) | Value::Array(_) | Value::Object(_) => {
            value.to_string()
        }
    }
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn text_content(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use microvibe_protocol::{ToolCall, ToolResult};

    #[test]
    fn programmatic_json_messages_match_vibe_shape_and_include_tools() {
        let call = ToolCall {
            id: "call_1".to_string(),
            name: "read".to_string(),
            arguments: json!({ "file_path": "a.txt" }),
        };
        let result = ToolResult {
            call_id: "call_1".to_string(),
            name: "read".to_string(),
            output: "content".to_string(),
            success: true,
        };
        let messages = vec![
            Message::text(Role::System, "system"),
            Message::text(Role::User, "hello"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolCall(call)],
                message_id: None,
                reasoning_content: None,
                reasoning_message_id: None,
                injected: false,
                images: None,
                display_content: None,
            },
            Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult(result)],
                message_id: None,
                reasoning_content: None,
                reasoning_message_id: None,
                injected: false,
                images: None,
                display_content: None,
            },
            Message::text(Role::Assistant, "done"),
        ];

        let out = programmatic_messages(&messages);
        assert_eq!(out.len(), 5);
        assert_eq!(out[0]["role"], "system");
        assert_eq!(out[0]["content"], "system");
        assert_eq!(out[0]["injected"], false);
        assert!(out[0]["message_id"].as_str().is_some());
        assert_eq!(out[1]["role"], "user");
        assert_eq!(out[1]["content"], "hello");
        assert_eq!(out[2]["role"], "assistant");
        assert_eq!(out[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(out[2]["tool_calls"][0]["index"], Value::Null);
        assert_eq!(out[3]["role"], "tool");
        assert_eq!(out[3]["tool_call_id"], "call_1");
        assert_eq!(out[3]["message_id"], Value::Null);
        assert_eq!(out[4]["role"], "assistant");
        assert_eq!(out[4]["content"], "done");
    }
}
