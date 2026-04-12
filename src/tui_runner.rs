use crate::agent::Agent;
use crate::events::{EventSender, TuiEvent};
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
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

/// Run the full TUI application with event-driven architecture
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

    // Create event channel
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TuiEvent>();
    let event_sender = EventSender::new(event_tx);

    // Create agent with event sender
    let mut client = LlmClient::new(api_base, api_key, model, temperature, backend);
    client.set_event_sender(event_sender.clone());
    let agent = Arc::new(Mutex::new(Agent::new(
        client,
        system_prompt,
        auto_approve,
        max_context_tokens,
    )));

    let mut app = TuiApp::new(model, provider_name);
    let mut session = Session::new(model, provider_name);

    app.add_entry(ChatEntry::System(format!(
        "microvibe v{} | {} ({}) | /help for commands",
        env!("CARGO_PKG_VERSION"),
        model,
        provider_name
    )));

    // User input channel
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<String>();

    // Agent task handle
    let mut agent_handle: Option<tokio::task::JoinHandle<()>> = None;

    loop {
        // Draw
        terminal.draw(|f| app.render(f))?;

        // Process all pending TUI events (non-blocking)
        while let Ok(tui_event) = event_rx.try_recv() {
            match tui_event {
                TuiEvent::TextDelta(text) => {
                    // Append to the current assistant entry
                    app.append_assistant_text(&text);
                }
                TuiEvent::TextDone => {
                    // Nothing specific needed
                }
                TuiEvent::ToolCallStart { name, detail } => {
                    app.add_entry(ChatEntry::ToolCall {
                        name: name.clone(),
                        detail,
                        spinning: true,
                    });
                }
                TuiEvent::ToolCallDone {
                    name: _,
                    success,
                    summary,
                } => {
                    app.finish_last_tool(success);
                    app.add_entry(ChatEntry::ToolResult { summary });
                }
                TuiEvent::ThinkingStart => {
                    app.add_entry(ChatEntry::Thinking {
                        text: String::new(),
                        spinning: true,
                    });
                }
                TuiEvent::ThinkingDelta(text) => {
                    app.append_thinking_text(&text);
                }
                TuiEvent::ThinkingDone => {
                    app.finish_thinking();
                }
                TuiEvent::TokenUpdate {
                    prompt_tokens,
                    completion_tokens,
                    ..
                } => {
                    app.stats.prompt_tokens += prompt_tokens;
                    app.stats.completion_tokens += completion_tokens;
                }
                TuiEvent::TurnDone => {
                    app.waiting = false;
                    // Save session
                    let agent_lock = agent.lock().await;
                    session.messages = agent_lock.messages().to_vec();
                    session.stats = agent_lock.stats.clone();
                    app.stats = agent_lock.stats.clone();
                    drop(agent_lock);
                    let _ = session.save();
                }
                TuiEvent::Error(e) => {
                    app.add_entry(ChatEntry::System(format!("Error: {}", e)));
                    app.waiting = false;
                }
                TuiEvent::SystemMessage(msg) => {
                    app.add_entry(ChatEntry::System(msg));
                }
                TuiEvent::ApprovalRequest { .. } => {
                    // TODO: inline approval in TUI
                }
                TuiEvent::StatsUpdate(stats) => {
                    app.stats = stats;
                }
            }
        }

        // Poll for keyboard events
        if event::poll(Duration::from_millis(33))? {
            // ~30fps
            if let Event::Key(key) = event::read()? {
                if let Some(input) = app.handle_key(key) {
                    if input == "/quit" || input == "/q" || input == "/exit" {
                        break;
                    }

                    if input == "/clear" {
                        let mut client =
                            LlmClient::new(api_base, api_key, model, temperature, backend);
                        client.set_event_sender(event_sender.clone());
                        let mut agent_lock = agent.lock().await;
                        *agent_lock = Agent::new(
                            client,
                            system_prompt,
                            auto_approve,
                            max_context_tokens,
                        );
                        drop(agent_lock);
                        session = Session::new(model, provider_name);
                        app.clear_entries();
                        app.add_entry(ChatEntry::System("Context cleared.".into()));
                        continue;
                    }

                    if input == "/help" {
                        app.add_entry(ChatEntry::System(
                            "Commands: /quit /clear /stats /undo /diff /commit /test /review /help"
                                .into(),
                        ));
                        continue;
                    }

                    if input == "/stats" {
                        let agent_lock = agent.lock().await;
                        let s = &agent_lock.stats;
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
                        let mut agent_lock = agent.lock().await;
                        if agent_lock.undo() {
                            app.add_entry(ChatEntry::System("Undone.".into()));
                        } else {
                            app.add_entry(ChatEntry::System("Nothing to undo.".into()));
                        }
                        continue;
                    }

                    // Regular message — spawn agent task
                    app.add_entry(ChatEntry::User(input.clone()));
                    app.start_assistant_entry();
                    app.waiting = true;

                    let agent_clone = agent.clone();
                    let expanded = expand_file_mentions(&input);
                    let event_sender_clone = event_sender.clone();

                    agent_handle = Some(tokio::spawn(async move {
                        let mut agent_lock = agent_clone.lock().await;
                        let result = agent_lock.run_turn(&expanded).await;
                        if let Err(e) = result {
                            event_sender_clone.send(TuiEvent::Error(e.to_string()));
                        }
                        event_sender_clone.send(TuiEvent::TurnDone);
                    }));
                }
            }
        }
    }

    // Cleanup
    if let Some(handle) = agent_handle {
        handle.abort();
    }

    // Final save
    let agent_lock = agent.lock().await;
    session.messages = agent_lock.messages().to_vec();
    session.stats = agent_lock.stats.clone();
    drop(agent_lock);
    let _ = session.save();

    // Restore terminal
    terminal::disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}
