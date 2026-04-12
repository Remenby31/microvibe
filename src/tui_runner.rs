use crate::agent::Agent;
use crate::expand_file_mentions;
use crate::llm::{Backend, LlmClient};
use crate::session::Session;
use crate::tui::{ChatEntry, TuiApp};
use crossterm::event::{self, Event};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::execute;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::time::Duration;

/// Run the full TUI application
pub async fn run_tui(
    api_base: &str,
    api_key: &str,
    model: &str,
    provider_name: &str,
    temperature: f32,
    backend: Backend,
    auto_approve: bool,
    max_context_tokens: usize,
    system_prompt: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Setup terminal
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let term_backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(term_backend)?;

    let mut app = TuiApp::new(model, provider_name);
    let client = LlmClient::new(api_base, api_key, model, temperature, backend);
    let mut agent = Agent::new(client, system_prompt, auto_approve, max_context_tokens);
    let mut session = Session::new(model, provider_name);

    app.add_entry(ChatEntry::System(format!(
        "microvibe v{} | {} ({}) | /help for commands",
        env!("CARGO_PKG_VERSION"),
        model,
        provider_name
    )));

    // Main loop
    let result = run_loop(&mut terminal, &mut app, &mut agent, &mut session, api_base, api_key, model, temperature, backend, auto_approve, max_context_tokens, system_prompt).await;

    // Restore terminal
    terminal::disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut TuiApp,
    agent: &mut Agent,
    session: &mut Session,
    api_base: &str,
    api_key: &str,
    model: &str,
    temperature: f32,
    backend: Backend,
    auto_approve: bool,
    max_context_tokens: usize,
    system_prompt: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        // Draw
        terminal.draw(|f| app.render(f))?;

        // Poll for events with timeout (non-blocking so we can update spinner)
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if let Some(input) = app.handle_key(key) {
                    // Handle the submitted input
                    if input == "/quit" || input == "/q" || input == "/exit" {
                        break;
                    }

                    if input == "/clear" {
                        let client = LlmClient::new(api_base, api_key, model, temperature, backend);
                        *agent = Agent::new(client, system_prompt, auto_approve, max_context_tokens);
                        *session = Session::new(model, &app.provider);
                        app.add_entry(ChatEntry::System("Context cleared.".into()));
                        continue;
                    }

                    if input == "/help" {
                        app.add_entry(ChatEntry::System(
                            "Commands: /quit /clear /stats /undo /diff /commit /test /review /help".into(),
                        ));
                        continue;
                    }

                    if input == "/stats" {
                        let s = &agent.stats;
                        let p = crate::pricing::get_pricing(model);
                        app.add_entry(ChatEntry::System(format!(
                            "{}in + {}out = {} tokens | ${:.4} | {} tools | {} turns",
                            s.prompt_tokens,
                            s.completion_tokens,
                            s.total_tokens(),
                            s.estimated_cost(p.input, p.output),
                            s.tool_calls,
                            s.turns
                        )));
                        continue;
                    }

                    if input == "/undo" {
                        if agent.undo() {
                            app.add_entry(ChatEntry::System("Undone.".into()));
                        } else {
                            app.add_entry(ChatEntry::System("Nothing to undo.".into()));
                        }
                        continue;
                    }

                    // Regular message — run the agent
                    app.add_entry(ChatEntry::User(input.clone()));
                    app.waiting = true;
                    terminal.draw(|f| app.render(f))?;

                    // Temporarily leave raw mode so agent can print to stdout
                    terminal::disable_raw_mode()?;
                    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

                    // Run agent turn
                    let expanded = expand_file_mentions(&input);
                    let result = agent.run_turn(&expanded).await;

                    // Get the last assistant response
                    if let Some(msg) = agent.messages().iter().rev().find(|m| m.role == crate::types::Role::Assistant) {
                        if let Some(ref content) = msg.content {
                            app.add_entry(ChatEntry::Assistant(content.clone()));
                        }
                    }

                    app.stats = agent.stats.clone();
                    app.waiting = false;

                    if let Err(e) = result {
                        app.add_entry(ChatEntry::System(format!("Error: {}", e)));
                    }

                    // Re-enter alternate screen
                    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
                    terminal::enable_raw_mode()?;

                    // Auto-save
                    session.messages = agent.messages().to_vec();
                    session.stats = agent.stats.clone();
                    let _ = session.save();
                }
            }
        }
    }

    // Final save
    session.messages = agent.messages().to_vec();
    session.stats = agent.stats.clone();
    let _ = session.save();

    Ok(())
}
