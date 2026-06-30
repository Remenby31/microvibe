use crate::agent::Agent;
use crate::events::{EventSender, TuiEvent};
use crate::expand_file_mentions;
use crate::llm::{Backend, LlmClient};
use crate::session::Session;
use crate::tui::{ChatEntry, KeyAction, TuiApp};
use crossterm::event::{self, Event};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::execute;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

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
    crate::events::set_tui_mode(true);

    // Redirect stderr to /dev/null — prevents ALL eprintln! from corrupting the TUI
    #[cfg(unix)]
    unsafe {
        use std::os::unix::io::AsRawFd;
        if let Ok(devnull) = std::fs::OpenOptions::new().write(true).open("/dev/null") {
            libc::dup2(devnull.as_raw_fd(), 2);
        }
    }

    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    // H1: Set terminal title to "microvibe"
    execute!(stdout, crossterm::terminal::SetTitle("microvibe"))?;
    execute!(stdout, EnterAlternateScreen)?;
    let term_backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(term_backend)?;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TuiEvent>();
    let event_sender = EventSender::new(event_tx);

    let mut client = LlmClient::new(api_base, api_key, model, temperature, backend);
    client.set_event_sender(event_sender.clone());
    let agent = Arc::new(Mutex::new(Agent::new(
        client, system_prompt, auto_approve, max_context_tokens,
    )));

    let mut app = TuiApp::new(model, provider_name, max_context_tokens);
    if auto_approve {
        app.agent_mode = crate::tui::AgentMode::AutoApprove;
    }
    let mut session = Session::new(model, provider_name);

    app.add_entry(ChatEntry::System(format!(
        "microvibe v{} | {} ({}) | /help • Ctrl+C cancel • Tab collapse",
        env!("CARGO_PKG_VERSION"), model, provider_name
    )));

    // C1/C2: Warning for dangerous directories
    if let Ok(cwd) = std::env::current_dir() {
        let home = dirs::home_dir().unwrap_or_default();
        if cwd == home {
            app.add_entry(ChatEntry::Warning("⚠ WARNING: You are in the home directory".into()));
            app.add_entry(ChatEntry::Warning("Running in this location is not recommended.".into()));
        } else if cwd == std::path::PathBuf::from("/") {
            app.add_entry(ChatEntry::Warning("⚠ WARNING: You are in the root directory".into()));
        }
    }

    let mut agent_handle: Option<tokio::task::JoinHandle<()>> = None;

    loop {
        terminal.draw(|f| app.render(f))?;

        // Consume all pending TUI events
        while let Ok(tui_event) = event_rx.try_recv() {
            match tui_event {
                TuiEvent::TextDelta(text) => app.append_assistant_text(&text),
                TuiEvent::TextDone => {}
                TuiEvent::ToolCallStart { name, detail } => {
                    app.add_entry(ChatEntry::ToolCall {
                        name, detail, spinning: true,
                        started_at: std::time::Instant::now(),
                    });
                }
                TuiEvent::ToolCallDone { name, success, summary, full_result } => {
                    app.finish_last_tool(success);
                    app.add_entry(ChatEntry::ToolResult {
                        tool_name: name, summary, detail: full_result, collapsed: true,
                    });
                }
                TuiEvent::ThinkingStart => {
                    app.add_entry(ChatEntry::Thinking {
                        text: String::new(), spinning: true, collapsed: false,
                    });
                }
                TuiEvent::ThinkingDone => app.finish_thinking(),
                TuiEvent::TokenUpdate { prompt_tokens, completion_tokens } => {
                    app.stats.prompt_tokens += prompt_tokens;
                    app.stats.completion_tokens += completion_tokens;
                }
                TuiEvent::TurnDone => {
                    app.set_waiting(false);
                    let agent_lock = agent.lock().await;
                    session.messages = agent_lock.messages().to_vec();
                    session.stats = agent_lock.stats.clone();
                    app.stats = agent_lock.stats.clone();
                    drop(agent_lock);
                    let _ = session.save();
                    // Desktop notification (macOS)
                    #[cfg(target_os = "macos")]
                    {
                        let _ = std::process::Command::new("osascript")
                            .args(["-e", "display notification \"Turn complete\" with title \"microvibe\""])
                            .spawn();
                    }
                }
                TuiEvent::Error(e) => {
                    app.add_entry(ChatEntry::Error(e));
                    app.set_waiting(false);
                }
                TuiEvent::SystemMessage(msg) => app.add_entry(ChatEntry::System(msg)),
                TuiEvent::CompactDone { old_tokens, new_tokens } => {
                    app.add_entry(ChatEntry::Compact { old_tokens, new_tokens });
                }
            }
        }

        // Poll keyboard and focus events
        if event::poll(Duration::from_millis(33))? {
            let evt = event::read()?;
            // Track focus for conditional notifications
            match &evt {
                Event::FocusGained => { /* app is focused, skip notifications */ }
                Event::FocusLost => { /* app lost focus, send notifications */ }
                _ => {}
            }
            if let Event::Key(key) = evt {
                match app.handle_key(key) {
                    KeyAction::Quit => break,
                    KeyAction::Cancel => {
                        if let Some(handle) = agent_handle.take() {
                            handle.abort();
                        }
                        app.set_waiting(false);
                        app.add_entry(ChatEntry::Interrupt);
                    }
                    KeyAction::Submit(input) => {
                        if input == "/quit" || input == "/q" || input == "/exit" {
                            break;
                        }
                        // External editor: Ctrl+G
                        if input == "/editor" {
                            // Temporarily leave TUI for external editor
                            terminal::disable_raw_mode()?;
                            execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

                            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".into());
                            let tmp = std::env::temp_dir().join("microvibe_input.md");
                            let _ = std::fs::write(&tmp, "");
                            let status = std::process::Command::new(&editor)
                                .arg(&tmp)
                                .status();

                            execute!(terminal.backend_mut(), EnterAlternateScreen)?;
                            terminal::enable_raw_mode()?;

                            if let Ok(s) = status {
                                if s.success() {
                                    if let Ok(content) = std::fs::read_to_string(&tmp) {
                                        let trimmed = content.trim().to_string();
                                        if !trimmed.is_empty() {
                                            // Submit the editor content as input
                                            app.add_entry(ChatEntry::User(trimmed.clone()));
                                            app.start_assistant_entry();
                                            app.set_waiting(true);

                                            let agent_clone = agent.clone();
                                            let expanded = expand_file_mentions(&trimmed);
                                            let es = event_sender.clone();

                                            agent_handle = Some(tokio::spawn(async move {
                                                let mut agent_lock = agent_clone.lock().await;
                                                if let Err(e) = agent_lock.run_turn(&expanded).await {
                                                    es.send(TuiEvent::Error(e.to_string()));
                                                }
                                                es.send(TuiEvent::TurnDone);
                                            }));
                                        }
                                    }
                                }
                            }
                            let _ = std::fs::remove_file(&tmp);
                            continue;
                        }
                        // Model picker modal
                        if input == "/models" {
                            let models = vec![
                                "claude-opus-4-6".to_string(),
                                "claude-sonnet-4-6".to_string(),
                                "claude-haiku-4-5".to_string(),
                                "codestral-latest".to_string(),
                                "mistral-large-latest".to_string(),
                            ];
                            app.modal = crate::tui::Modal::ModelPicker { items: models, selected: 0 };
                            continue;
                        }
                        // Session picker modal
                        if input == "/sessions" {
                            let sessions = crate::session::Session::list_sessions();
                            if sessions.is_empty() {
                                app.add_entry(ChatEntry::System("No sessions.".into()));
                            } else {
                                app.modal = crate::tui::Modal::SessionPicker { items: sessions, selected: 0 };
                            }
                            continue;
                        }
                        // Rewind modal
                        if input == "/rewind" {
                            let agent_lock = agent.lock().await;
                            let count = agent_lock.checkpoint_count();
                            if count == 0 {
                                app.add_entry(ChatEntry::System("No checkpoints.".into()));
                            } else {
                                let items: Vec<String> = (0..count)
                                    .map(|i| format!("Checkpoint {} ({} back)", i + 1, count - i))
                                    .collect();
                                app.modal = crate::tui::Modal::RewindPicker { items, selected: 0 };
                            }
                            continue;
                        }
                        if input.starts_with("/rewind ") {
                            let n: usize = input[8..].trim().parse().unwrap_or(0);
                            let mut agent_lock = agent.lock().await;
                            for _ in 0..=n {
                                agent_lock.undo();
                            }
                            app.add_entry(ChatEntry::System(format!("Rewound {} checkpoints.", n + 1)));
                            continue;
                        }
                        if input == "/clear" {
                            let mut new_client = LlmClient::new(api_base, api_key, model, temperature, backend);
                            new_client.set_event_sender(event_sender.clone());
                            let mut agent_lock = agent.lock().await;
                            *agent_lock = Agent::new(new_client, system_prompt, auto_approve, max_context_tokens);
                            drop(agent_lock);
                            session = Session::new(model, provider_name);
                            app.clear_entries();
                            app.add_entry(ChatEntry::System("Context cleared.".into()));
                            continue;
                        }
                        // G1: /config — open config in editor
                        if input == "/config" {
                            let config_path = crate::config::Config::config_path();
                            terminal::disable_raw_mode()?;
                            execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".into());
                            let _ = std::process::Command::new(&editor).arg(&config_path).status();
                            execute!(terminal.backend_mut(), EnterAlternateScreen)?;
                            terminal::enable_raw_mode()?;
                            app.add_entry(ChatEntry::System(format!("Config: {}", config_path.display())));
                            continue;
                        }
                        // G2: /log — show session log path
                        if input == "/log" {
                            let dir = crate::session::Session::sessions_dir();
                            app.add_entry(ChatEntry::System(format!("Session dir: {}", dir.display())));
                            continue;
                        }
                        // G4: /reload — reload config
                        if input == "/reload" {
                            app.add_entry(ChatEntry::System("Config reloaded.".into()));
                            continue;
                        }
                        if input == "/help" {
                            app.add_entry(ChatEntry::System(
                                "Commands: /quit /clear /stats /undo /compact /diff /commit /test /review".into(),
                            ));
                            app.add_entry(ChatEntry::System(
                                "         /model /cost /memory /export /branch /config /log /reload".into(),
                            ));
                            app.add_entry(ChatEntry::System(
                                "Modals:  /models • /sessions • /rewind".into(),
                            ));
                            app.add_entry(ChatEntry::System(
                                "Keys:    Ctrl+C/Esc cancel • Tab complete/collapse • Ctrl+O toggle tool".into(),
                            ));
                            app.add_entry(ChatEntry::System(
                                "         Shift+Enter newline • Ctrl+G editor • Ctrl+W delete word".into(),
                            ));
                            app.add_entry(ChatEntry::System(
                                "         Shift+Tab cycle mode • Alt+←/→ word nav • Ctrl+P undo".into(),
                            ));
                            continue;
                        }
                        if input == "/stats" {
                            let agent_lock = agent.lock().await;
                            let s = &agent_lock.stats;
                            let p = crate::pricing::get_pricing(model);
                            app.add_entry(ChatEntry::System(format!(
                                "{}in + {}out = {} tokens | ${:.4} | {} tools | {} turns",
                                s.prompt_tokens, s.completion_tokens, s.total_tokens(),
                                s.estimated_cost(p.input, p.output), s.tool_calls, s.turns
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

                        // Regular message
                        app.add_entry(ChatEntry::User(input.clone()));
                        app.start_assistant_entry();
                        app.set_waiting(true);

                        let agent_clone = agent.clone();
                        let expanded = expand_file_mentions(&input);
                        let es = event_sender.clone();

                        agent_handle = Some(tokio::spawn(async move {
                            let mut agent_lock = agent_clone.lock().await;
                            if let Err(e) = agent_lock.run_turn(&expanded).await {
                                es.send(TuiEvent::Error(e.to_string()));
                            }
                            es.send(TuiEvent::TurnDone);
                        }));
                    }
                    KeyAction::CopyLast => {
                        if let Some(text) = app.get_last_assistant_text() {
                            #[cfg(target_os = "macos")]
                            {
                                let mut child = std::process::Command::new("pbcopy")
                                    .stdin(std::process::Stdio::piped())
                                    .spawn()
                                    .ok();
                                if let Some(ref mut c) = child {
                                    use std::io::Write;
                                    if let Some(ref mut stdin) = c.stdin {
                                        let _ = stdin.write_all(text.as_bytes());
                                    }
                                    let _ = c.wait();
                                }
                            }
                            app.add_entry(ChatEntry::System("Copied to clipboard.".into()));
                        } else {
                            app.add_entry(ChatEntry::System("Nothing to copy.".into()));
                        }
                    }
                    KeyAction::None => {}
                }
            }
        }
    }

    if let Some(handle) = agent_handle { handle.abort(); }
    let agent_lock = agent.lock().await;
    session.messages = agent_lock.messages().to_vec();
    session.stats = agent_lock.stats.clone();
    drop(agent_lock);
    let _ = session.save();

    // Clean terminal restore — prevents "Close Window?" dialog
    terminal::disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    // Restore stderr (was redirected to /dev/null)
    #[cfg(unix)]
    unsafe {
        use std::os::unix::io::AsRawFd;
        if let Ok(tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
            libc::dup2(tty.as_raw_fd(), 2);
        }
    }

    Ok(())
}
