mod autocomplete;

use anyhow::Result;
use autocomplete::{
    CommandEntry, CompletionSet, FileIndexer, command_completions, path_completions_with_indexer,
    replace_completion,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use crossterm::Command as CrosstermCommand;
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, ModifierKeyCode,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::style::force_color_output;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use microvibe_config::{Config, McpServerConfig};
use microvibe_core::{
    AgentSummary, ApprovalRequest, QuestionAnswer, QuestionRequest, QuestionResponse, SavedSession,
    Session, SessionStore, primary_agent_order,
};
use microvibe_protocol::{
    AgentEvent, ApprovalDecision, ContentBlock, ImageAttachment, ImageSource, Message, Role,
    ToolCall, ToolResult,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use sha1::Digest;
use std::collections::{HashSet, VecDeque};
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command as TokioCommand;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const INPUT_GRACE_PERIOD: Duration = Duration::from_millis(500);

pub async fn run(config: Config) -> Result<()> {
    run_with_initial_prompt(config, None).await
}

pub async fn run_with_initial_prompt(config: Config, initial_prompt: Option<String>) -> Result<()> {
    force_color_output(true);
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        DisableModifyOtherKeys,
        PushKeyboardEnhancementFlags(keyboard_enhancement_flags())
    )?;
    if tmux_should_enable_modify_other_keys() {
        execute!(stdout, EnableModifyOtherKeys)?;
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_inner(config, initial_prompt, &mut terminal).await;

    terminal::disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        PopKeyboardEnhancementFlags,
        ResetKeyboardEnhancementFlags,
        DisableModifyOtherKeys,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    result
}

fn keyboard_enhancement_flags() -> KeyboardEnhancementFlags {
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResetKeyboardEnhancementFlags;

impl CrosstermCommand for ResetKeyboardEnhancementFlags {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[<u")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "keyboard enhancement reset is not implemented for the legacy Windows API",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisableModifyOtherKeys;

impl CrosstermCommand for DisableModifyOtherKeys {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[>4;0m")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "modifyOtherKeys reset is not implemented for the legacy Windows API",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnableModifyOtherKeys;

impl CrosstermCommand for EnableModifyOtherKeys {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[>4;2m")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "modifyOtherKeys enable is not implemented for the legacy Windows API",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

fn tmux_should_enable_modify_other_keys() -> bool {
    tmux_should_enable_modify_other_keys_for(
        tmux_session_detected(
            std::env::var("TMUX").ok().as_deref(),
            std::env::var("TMUX_PANE").ok().as_deref(),
        ),
        read_tmux_extended_keys_format().as_deref(),
    )
}

fn tmux_session_detected(tmux: Option<&str>, tmux_pane: Option<&str>) -> bool {
    tmux.is_some() || tmux_pane.is_some()
}

fn tmux_should_enable_modify_other_keys_for(
    running_in_tmux_session: bool,
    extended_keys_format: Option<&str>,
) -> bool {
    running_in_tmux_session && matches!(extended_keys_format, Some("csi-u"))
}

fn read_tmux_extended_keys_format() -> Option<String> {
    for args in [
        ["display-message", "-p", "#{extended-keys-format}"],
        ["show-options", "-gqv", "extended-keys-format"],
    ] {
        let output = Command::new("tmux")
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;

        if !output.status.success() {
            continue;
        }

        if let Some(value) = String::from_utf8(output.stdout)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            return Some(value);
        }
    }

    None
}

fn modifier_key_modifier(code: &KeyCode) -> Option<KeyModifiers> {
    match code {
        KeyCode::Modifier(ModifierKeyCode::LeftControl | ModifierKeyCode::RightControl) => {
            Some(KeyModifiers::CONTROL)
        }
        KeyCode::Modifier(ModifierKeyCode::LeftAlt | ModifierKeyCode::RightAlt) => {
            Some(KeyModifiers::ALT)
        }
        KeyCode::Modifier(ModifierKeyCode::LeftShift | ModifierKeyCode::RightShift) => {
            Some(KeyModifiers::SHIFT)
        }
        KeyCode::Modifier(ModifierKeyCode::LeftSuper | ModifierKeyCode::RightSuper) => {
            Some(KeyModifiers::SUPER)
        }
        KeyCode::Modifier(ModifierKeyCode::LeftHyper | ModifierKeyCode::RightHyper) => {
            Some(KeyModifiers::HYPER)
        }
        KeyCode::Modifier(ModifierKeyCode::LeftMeta | ModifierKeyCode::RightMeta) => {
            Some(KeyModifiers::META)
        }
        _ => None,
    }
}

fn normalize_key_event(
    mut key: event::KeyEvent,
    active_key_modifiers: &mut KeyModifiers,
) -> Option<event::KeyEvent> {
    if let Some(modifier) = modifier_key_modifier(&key.code) {
        match key.kind {
            KeyEventKind::Press | KeyEventKind::Repeat => active_key_modifiers.insert(modifier),
            KeyEventKind::Release => active_key_modifiers.remove(modifier),
        }
        return None;
    }
    if key.kind == KeyEventKind::Release {
        return None;
    }
    key.modifiers |= *active_key_modifiers;
    Some(key)
}

fn is_copy_selection_shortcut(key: &event::KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && (matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'))
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::SHIFT))
            || key.code == KeyCode::Char('C'))
}

async fn run_inner(
    mut config: Config,
    initial_prompt: Option<String>,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    let model_count = if config.models.is_empty() {
        3
    } else {
        config.models.len()
    };
    let mut mcp_index = discover_mcp_index(&config).await;
    let skill_count = available_skill_count();
    let agent_order = primary_agent_order(&config);
    let initial_session = Session::new(config.clone());
    let mut current_agent_name = config.default_agent.clone();
    let mut current_agent = display_agent_name(&current_agent_name, &agent_order);
    let mut input = String::new();
    let mut input_cursor = 0usize;
    let mut completion: Option<CompletionSet> = None;
    let completion_entries = completion_command_entries();
    let mut completion_file_indexer = FileIndexer::new();
    let mut input_history: Vec<String> = load_input_history();
    let mut input_history_index: Option<usize> = None;
    let mut input_history_draft = String::new();
    let show_initializing = config.default_agent == "plan";
    let mut transcript = startup_lines(
        initial_session.agent.model(),
        model_count,
        mcp_server_summary(&config),
        skill_count,
        show_initializing,
    );
    if !is_builtin_primary_agent(&current_agent_name) && !show_initializing {
        while transcript.len() >= 2
            && transcript.last().is_some_and(String::is_empty)
            && transcript
                .get(transcript.len() - 2)
                .is_some_and(String::is_empty)
        {
            transcript.pop();
        }
    }
    let mut session = Some(initial_session);
    let mut bottom_panel: Option<BottomPanel> = None;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (approval_tx, mut approval_rx) = mpsc::unbounded_channel::<ApprovalRequest>();
    let (question_tx, mut question_rx) = mpsc::unbounded_channel::<QuestionRequest>();
    let mut running_turn: Option<RunningTurn> = None;
    let mut frame_tick: usize = 0;
    let mut quit_confirmation: Option<QuitConfirmation> = None;
    let mut tools_collapsed = true;
    let mut tool_render_records: Vec<ToolRenderRecord> = Vec::new();
    let mut debug_console = false;
    let mut scroll_offset: usize = 0;
    let mut current_assistant_text = String::new();
    let mut last_assistant_text: Option<String> = None;
    let mut scheduled_loops: Vec<ScheduledLoop> = Vec::new();
    let mut active_key_modifiers = KeyModifiers::empty();
    let mut queued_inputs: VecDeque<String> = VecDeque::new();

    if let Some(submitted) = initial_prompt.filter(|prompt| !prompt.trim().is_empty()) {
        add_input_history(&mut input_history, &submitted);
        clear_initializing(&mut transcript);
        if let Some(active_session) = session.take() {
            match build_turn_payload(&submitted, &config, &active_session.store.session_dir) {
                Ok(payload) => {
                    transcript.extend(format_user_prompt_lines_with_images(
                        &submitted,
                        &payload.images,
                    ));
                    transcript.push("─".repeat(120));
                    transcript.push(String::new());
                    let tx_turn = tx.clone();
                    let approval_tx_turn = approval_tx.clone();
                    let question_tx_turn = question_tx.clone();
                    running_turn = Some(spawn_agent_turn(
                        active_session,
                        submitted,
                        payload,
                        tx_turn,
                        approval_tx_turn,
                        question_tx_turn,
                    ));
                }
                Err(lines) => {
                    set_single_trailing_blank(&mut transcript);
                    transcript.extend(lines);
                    transcript.push(String::new());
                    session = Some(active_session);
                }
            }
        }
    }

    loop {
        if quit_confirmation
            .as_ref()
            .is_some_and(|confirmation| confirmation.started.elapsed() >= QUIT_CONFIRM_DELAY)
        {
            quit_confirmation = None;
        }
        while let Ok(event) = rx.try_recv() {
            match &event {
                AgentEvent::AssistantDelta { text } => current_assistant_text.push_str(text),
                AgentEvent::TurnCompleted { .. } => {
                    let trimmed = current_assistant_text.trim().to_string();
                    if !trimmed.is_empty() {
                        last_assistant_text = Some(trimmed);
                    }
                    current_assistant_text.clear();
                }
                _ => {}
            }
            handle_agent_event(
                &mut transcript,
                &mut bottom_panel,
                &mut current_agent,
                &mut tool_render_records,
                tools_collapsed,
                event,
            );
        }
        while let Ok(request) = approval_rx.try_recv() {
            if let Some(running) = running_turn.as_mut() {
                if let Some(decision) = running.queued_decision.take() {
                    let _ = request.respond_to.send(decision);
                } else {
                    running.approval = Some(request);
                }
            }
        }
        while let Ok(request) = question_rx.try_recv() {
            if let Some(running) = running_turn.as_mut() {
                if let Some(response) = running.queued_question.take() {
                    let _ = request.respond_to.send(response);
                } else {
                    let panel_already_visible = bottom_panel.as_ref().is_some_and(|panel| {
                        panel.command == "/question"
                            && panel
                                .question_call
                                .as_ref()
                                .is_some_and(|call| call.id == request.call.id)
                    });
                    if !panel_already_visible {
                        bottom_panel = Some(question_panel(&request.call));
                    }
                    running.question = Some(request);
                }
            }
        }
        if running_turn
            .as_ref()
            .is_some_and(|running| running.handle.is_finished())
            && let Some(running) = running_turn.take()
        {
            match running.handle.await {
                Ok((mut finished_session, result)) => {
                    if result.is_ok() {
                        finished_session.save().await.ok();
                    }
                    session = Some(finished_session);
                }
                Err(error) => {
                    transcript.push(format!("error: agent task failed: {error}"));
                }
            }
        }
        if running_turn.is_none()
            && bottom_panel.is_none()
            && let Some(submitted) = queued_inputs.pop_front()
        {
            remove_queued_input_lines(&mut transcript, &submitted, queued_inputs.is_empty());
            if let Some(shell_command) = submitted.strip_prefix('!') {
                clear_initializing(&mut transcript);
                set_single_trailing_blank(&mut transcript);
                if shell_command.is_empty() {
                    transcript.extend(manual_bash_empty_lines());
                } else {
                    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                    let result = run_manual_bash_command(shell_command).await;
                    if let Some(active_session) = session.as_mut() {
                        active_session
                            .agent
                            .inject_user_context(manual_bash_context(
                                shell_command,
                                &cwd,
                                &result,
                                bash_max_output_bytes(&config),
                            ));
                        let _ = active_session.save().await;
                    }
                    transcript.extend(manual_bash_display_lines(shell_command, &result));
                }
            } else {
                let visible_input =
                    expand_skill_prompt(&submitted).unwrap_or_else(|| submitted.clone());
                clear_initializing(&mut transcript);
                if let Some(active_session) = session.take() {
                    match build_turn_payload(
                        &visible_input,
                        &config,
                        &active_session.store.session_dir,
                    ) {
                        Ok(payload) => {
                            transcript.extend(format_user_prompt_lines_with_images(
                                &visible_input,
                                &payload.images,
                            ));
                            transcript.push("─".repeat(120));
                            transcript.push(String::new());
                            running_turn = Some(spawn_agent_turn(
                                active_session,
                                visible_input,
                                payload,
                                tx.clone(),
                                approval_tx.clone(),
                                question_tx.clone(),
                            ));
                        }
                        Err(lines) => {
                            set_single_trailing_blank(&mut transcript);
                            transcript.extend(lines);
                            transcript.push(String::new());
                            session = Some(active_session);
                        }
                    }
                }
            }
        }
        terminal.draw(|frame| {
            let area = frame.area();
            let debug_width = debug_console.then(|| debug_console_width(area.width));
            let (main_area, debug_area) = if let Some(debug_width) = debug_width {
                let chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Min(1), Constraint::Length(debug_width)])
                    .split(area);
                (chunks[0], Some(chunks[1]))
            } else {
                (area, None)
            };
            let bottom_height = bottom_panel
                .as_ref()
                .map(BottomPanel::height)
                .unwrap_or_else(|| input_panel_height(completion.as_ref()).min(main_area.height));
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(bottom_height)])
                .split(main_area);
            let body_height = chunks[0].height as usize;
            let is_fixed_help = transcript.first().map(|line| line.as_str()) == Some("  ⎢");
            let display_rows = transcript_display_rows(&transcript, frame_tick);
            let visible_transcript = if !is_fixed_help && display_rows.len() > body_height {
                let max_offset = display_rows.len().saturating_sub(body_height);
                scroll_offset = scroll_offset.min(max_offset);
                let end = display_rows.len().saturating_sub(scroll_offset);
                let start = end.saturating_sub(body_height);
                let mut visible_rows = display_rows[start..end].to_vec();
                if scroll_offset > 0
                    && visible_rows
                        .first()
                        .is_some_and(|row| row.starts_with("> "))
                    && visible_rows.get(1).is_some_and(|row| row.starts_with('─'))
                {
                    visible_rows.remove(0);
                }
                visible_rows.join("\n")
            } else {
                scroll_offset = 0;
                display_rows.join("\n")
            };
            let body = Paragraph::new(styled_transcript_rows(&visible_transcript))
                .wrap(Wrap { trim: false });
            let separator = "─".repeat(chunks[1].width as usize);
            let mode_label = current_agent.as_str();
            let mode_line = format!(
                "{} {mode_label} ─",
                "─".repeat(chunks[1].width.saturating_sub(mode_label.len() as u16 + 3) as usize)
            );
            let cwd = std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| ".".to_string());
            let footer_left = quit_confirmation
                .as_ref()
                .map(|confirmation| confirmation.prompt.clone())
                .unwrap_or(cwd);
            let status = "0% of 200k tokens";
            let gap = chunks[1]
                .width
                .saturating_sub(footer_left.chars().count() as u16)
                .saturating_sub(status.len() as u16) as usize;
            let prompt_lines = match &bottom_panel {
                Some(panel) => panel_lines(panel, chunks[1].width, footer_left, status, mode_label),
                None => input_panel_lines(
                    &mode_line,
                    &input,
                    completion.as_ref(),
                    &separator,
                    footer_left,
                    status,
                    gap,
                ),
            };
            let prompt = Paragraph::new(prompt_lines);
            frame.render_widget(body, chunks[0]);
            if transcript.first().map(|line| line.as_str()) == Some("  ⎢") {
                let marker = Rect {
                    x: chunks[0].x + chunks[0].width.saturating_sub(1),
                    y: chunks[0].y + 13,
                    width: 1,
                    height: 1,
                };
                frame.render_widget(Paragraph::new("▅"), marker);
            }
            frame.render_widget(prompt, chunks[1]);
            if bottom_panel.is_none() {
                frame.set_cursor_position(input_cursor_position(
                    chunks[1],
                    completion.as_ref(),
                    &input,
                    input_cursor,
                ));
            }
            if let Some(debug_area) = debug_area {
                frame.render_widget(
                    Paragraph::new(debug_console_text(debug_area.height)),
                    debug_area,
                );
            }
        })?;
        frame_tick = frame_tick.wrapping_add(1);

        if event::poll(Duration::from_millis(50))? {
            let mut event = event::read()?;
            if let Event::Key(key) = event {
                if let Some(key) = normalize_key_event(key, &mut active_key_modifiers) {
                    event = Event::Key(key);
                } else {
                    continue;
                }
            }
            match event {
                Event::Key(key) if key.code == KeyCode::Esc => {
                    if completion.is_some() {
                        completion = None;
                        continue;
                    }
                    if bottom_panel
                        .as_ref()
                        .is_some_and(BottomPanel::guards_initial_submit)
                    {
                        continue;
                    }
                    if let Some(panel) = bottom_panel.take() {
                        if panel.command == "/approval" {
                            deny_pending_turn(&mut running_turn);
                        } else if panel.command == "/question" {
                            respond_pending_question(
                                &mut running_turn,
                                cancelled_question_response(),
                            );
                        }
                        apply_panel_exit(&mut transcript, &panel);
                        continue;
                    }
                    if input.is_empty() {
                        break;
                    }
                }
                Event::Key(key) if is_copy_selection_shortcut(&key) => {
                    // Vibe reserves Ctrl+Y and Ctrl+Shift+C for copying the terminal selection.
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(event::KeyModifiers::CONTROL) =>
                {
                    if !input.is_empty() {
                        input.clear();
                        input_cursor = 0;
                        input_history_index = None;
                        input_history_draft.clear();
                        quit_confirmation = None;
                        continue;
                    }
                    if let Some(removed) = queued_inputs.pop_back() {
                        remove_queued_input_lines(
                            &mut transcript,
                            &removed,
                            queued_inputs.is_empty(),
                        );
                        quit_confirmation = None;
                        continue;
                    }
                    if quit_confirmation
                        .as_ref()
                        .is_some_and(|confirmation| confirmation.key == "Ctrl+C")
                    {
                        break;
                    }
                    quit_confirmation = Some(QuitConfirmation::new("Ctrl+C"));
                }
                Event::Key(key)
                    if key.code == KeyCode::Char('d')
                        && key.modifiers.contains(event::KeyModifiers::CONTROL) =>
                {
                    if !input.is_empty() {
                        quit_confirmation = None;
                        continue;
                    }
                    if quit_confirmation
                        .as_ref()
                        .is_some_and(|confirmation| confirmation.key == "Ctrl+D")
                    {
                        break;
                    }
                    quit_confirmation = Some(QuitConfirmation::new("Ctrl+D"));
                }
                Event::Key(key)
                    if matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D'))
                        && bottom_panel
                            .as_ref()
                            .is_some_and(|panel| panel.command == "/mcp") =>
                {
                    toggle_mcp_panel(
                        &mut config,
                        &mut mcp_index,
                        &mut bottom_panel,
                        &mut transcript,
                        true,
                    );
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if matches!(key.code, KeyCode::Char('e') | KeyCode::Char('E'))
                        && bottom_panel
                            .as_ref()
                            .is_some_and(|panel| panel.command == "/mcp") =>
                {
                    toggle_mcp_panel(
                        &mut config,
                        &mut mcp_index,
                        &mut bottom_panel,
                        &mut transcript,
                        false,
                    );
                    quit_confirmation = None;
                }
                Event::Key(key) if key.code == KeyCode::BackTab => {
                    if bottom_panel.is_none() {
                        current_agent_name = next_agent_name(&current_agent_name, &agent_order);
                        current_agent = display_agent_name(&current_agent_name, &agent_order);
                        config.default_agent = current_agent_name.clone();
                        if let Some(active_session) = session.as_mut() {
                            active_session.switch_agent(config.clone());
                        }
                    }
                    quit_confirmation = None;
                }
                Event::Key(key) if key.code == KeyCode::Tab && bottom_panel.is_none() => {
                    if let Some(active_completion) = completion.take() {
                        let should_refresh_after_completion = active_completion
                            .items
                            .get(active_completion.selected)
                            .is_some_and(|item| {
                                item.label.starts_with('@') && item.label.ends_with('/')
                            });
                        replace_completion(&mut input, &mut input_cursor, &active_completion);
                        reset_input_history_navigation(
                            &mut input_history_index,
                            &mut input_history_draft,
                        );
                        if should_refresh_after_completion {
                            completion = refresh_completion(
                                true,
                                &input,
                                input_cursor,
                                &completion_entries,
                                &mut completion_file_indexer,
                            );
                        }
                    }
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if key.code == KeyCode::Char('o')
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    tools_collapsed = !tools_collapsed;
                    apply_tool_collapse_state(
                        &mut transcript,
                        &tool_render_records,
                        tools_collapsed,
                    );
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if key.code == KeyCode::Char('\\')
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    debug_console = !debug_console;
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if key.code == KeyCode::Char('4')
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    debug_console = !debug_console;
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if key.code == KeyCode::Char('r')
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    // Vibe consumes the voice-recording shortcut even when voice mode is off.
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if key.code == KeyCode::Char('a')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                        && bottom_panel.is_none() =>
                {
                    input_cursor = 0;
                    completion = refresh_completion(
                        true,
                        &input,
                        input_cursor,
                        &completion_entries,
                        &mut completion_file_indexer,
                    );
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if key.code == KeyCode::Char('e')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                        && bottom_panel.is_none() =>
                {
                    input_cursor = input.len();
                    completion = refresh_completion(
                        true,
                        &input,
                        input_cursor,
                        &completion_entries,
                        &mut completion_file_indexer,
                    );
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if matches!(key.code, KeyCode::Char('b') | KeyCode::Char('B'))
                        && key.modifiers.contains(KeyModifiers::ALT)
                        && bottom_panel.is_none() =>
                {
                    input_cursor = previous_word_boundary(&input, input_cursor);
                    completion = refresh_completion(
                        true,
                        &input,
                        input_cursor,
                        &completion_entries,
                        &mut completion_file_indexer,
                    );
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if matches!(key.code, KeyCode::Char('f') | KeyCode::Char('F'))
                        && key.modifiers.contains(KeyModifiers::ALT)
                        && bottom_panel.is_none() =>
                {
                    input_cursor = next_word_boundary(&input, input_cursor);
                    completion = refresh_completion(
                        true,
                        &input,
                        input_cursor,
                        &completion_entries,
                        &mut completion_file_indexer,
                    );
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if matches!(key.code, KeyCode::Char('b') | KeyCode::Char('B'))
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                        && bottom_panel.is_none() =>
                {
                    input_cursor = previous_input_boundary(&input, input_cursor);
                    completion = refresh_completion(
                        true,
                        &input,
                        input_cursor,
                        &completion_entries,
                        &mut completion_file_indexer,
                    );
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if matches!(key.code, KeyCode::Char('f') | KeyCode::Char('F'))
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                        && bottom_panel.is_none() =>
                {
                    input_cursor = next_input_boundary(&input, input_cursor);
                    completion = refresh_completion(
                        true,
                        &input,
                        input_cursor,
                        &completion_entries,
                        &mut completion_file_indexer,
                    );
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D'))
                        && key.modifiers.contains(KeyModifiers::ALT)
                        && bottom_panel.is_none() =>
                {
                    delete_word_right(&mut input, &mut input_cursor);
                    reset_input_history_navigation(
                        &mut input_history_index,
                        &mut input_history_draft,
                    );
                    completion = refresh_completion(
                        true,
                        &input,
                        input_cursor,
                        &completion_entries,
                        &mut completion_file_indexer,
                    );
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if matches!(key.code, KeyCode::Char('w') | KeyCode::Char('W'))
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                        && bottom_panel.is_none() =>
                {
                    delete_word_left(&mut input, &mut input_cursor);
                    reset_input_history_navigation(
                        &mut input_history_index,
                        &mut input_history_draft,
                    );
                    completion = refresh_completion(
                        true,
                        &input,
                        input_cursor,
                        &completion_entries,
                        &mut completion_file_indexer,
                    );
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if key.code == KeyCode::Char('u')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                        && bottom_panel.is_none() =>
                {
                    delete_to_line_start(&mut input, &mut input_cursor);
                    reset_input_history_navigation(
                        &mut input_history_index,
                        &mut input_history_draft,
                    );
                    completion = refresh_completion(
                        true,
                        &input,
                        input_cursor,
                        &completion_entries,
                        &mut completion_file_indexer,
                    );
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if key.code == KeyCode::Char('k')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                        && bottom_panel.is_none() =>
                {
                    delete_to_line_end(&mut input, &mut input_cursor);
                    reset_input_history_navigation(
                        &mut input_history_index,
                        &mut input_history_draft,
                    );
                    completion = refresh_completion(
                        true,
                        &input,
                        input_cursor,
                        &completion_entries,
                        &mut completion_file_indexer,
                    );
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if key.code == KeyCode::Left
                        && key.modifiers.contains(KeyModifiers::SUPER)
                        && bottom_panel.is_none() =>
                {
                    input_cursor = 0;
                    completion = refresh_completion(
                        true,
                        &input,
                        input_cursor,
                        &completion_entries,
                        &mut completion_file_indexer,
                    );
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if key.code == KeyCode::Right
                        && key.modifiers.contains(KeyModifiers::SUPER)
                        && bottom_panel.is_none() =>
                {
                    input_cursor = input.len();
                    completion = refresh_completion(
                        true,
                        &input,
                        input_cursor,
                        &completion_entries,
                        &mut completion_file_indexer,
                    );
                    quit_confirmation = None;
                }
                Event::Key(key) if key.code == KeyCode::Home && bottom_panel.is_none() => {
                    input_cursor = 0;
                    completion = refresh_completion(
                        true,
                        &input,
                        input_cursor,
                        &completion_entries,
                        &mut completion_file_indexer,
                    );
                    quit_confirmation = None;
                }
                Event::Key(key) if key.code == KeyCode::End && bottom_panel.is_none() => {
                    input_cursor = input.len();
                    completion = refresh_completion(
                        true,
                        &input,
                        input_cursor,
                        &completion_entries,
                        &mut completion_file_indexer,
                    );
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if key.code == KeyCode::Left
                        && key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && bottom_panel.is_none() =>
                {
                    input_cursor = previous_word_boundary(&input, input_cursor);
                    completion = refresh_completion(
                        true,
                        &input,
                        input_cursor,
                        &completion_entries,
                        &mut completion_file_indexer,
                    );
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if key.code == KeyCode::Right
                        && key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && bottom_panel.is_none() =>
                {
                    input_cursor = next_word_boundary(&input, input_cursor);
                    completion = refresh_completion(
                        true,
                        &input,
                        input_cursor,
                        &completion_entries,
                        &mut completion_file_indexer,
                    );
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if (key.code == KeyCode::Char('p')
                        && key.modifiers.contains(KeyModifiers::CONTROL))
                        || (key.code == KeyCode::Up
                            && key.modifiers.contains(KeyModifiers::ALT)) =>
                {
                    if let Some(active_session) = session.as_ref() {
                        let current = bottom_panel.as_ref().and_then(|panel| {
                            if panel.command == "/rewind" {
                                panel.rewind_message_index
                            } else {
                                None
                            }
                        });
                        if let Some(panel) =
                            rewind_previous_panel(active_session.agent.messages(), current)
                        {
                            bottom_panel = Some(panel);
                            clear_initializing(&mut transcript);
                        }
                    }
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if (key.code == KeyCode::Char('n')
                        && key.modifiers.contains(KeyModifiers::CONTROL))
                        || (key.code == KeyCode::Down
                            && key.modifiers.contains(KeyModifiers::ALT)) =>
                {
                    if let Some(active_session) = session.as_ref()
                        && let Some(current) = bottom_panel.as_ref().and_then(|panel| {
                            if panel.command == "/rewind" {
                                panel.rewind_message_index
                            } else {
                                None
                            }
                        })
                        && let Some(panel) =
                            rewind_next_panel(active_session.agent.messages(), current)
                    {
                        bottom_panel = Some(panel);
                        clear_initializing(&mut transcript);
                    }
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if key.code == KeyCode::Char('j')
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    if bottom_panel.is_none() {
                        insert_input_char(&mut input, &mut input_cursor, '\n');
                        reset_input_history_navigation(
                            &mut input_history_index,
                            &mut input_history_draft,
                        );
                        completion = None;
                    }
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if key.code == KeyCode::Char('g')
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    if let Some(path) = bottom_panel.as_ref().and_then(exit_plan_panel_file_path) {
                        open_file_in_external_editor(terminal, &path)?;
                    } else if bottom_panel.is_none()
                        && let Some(edited) = open_external_editor(terminal, &input)?
                    {
                        input = edited;
                        input_cursor = input.len();
                        reset_input_history_navigation(
                            &mut input_history_index,
                            &mut input_history_draft,
                        );
                        completion = refresh_completion(
                            true,
                            &input,
                            input_cursor,
                            &completion_entries,
                            &mut completion_file_indexer,
                        );
                    }
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if key.code == KeyCode::Up && key.modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    scroll_offset = scroll_offset.saturating_add(6);
                    quit_confirmation = None;
                }
                Event::Key(key)
                    if key.code == KeyCode::Down && key.modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    scroll_offset = scroll_offset.saturating_sub(6);
                    quit_confirmation = None;
                }
                Event::Key(key) if key.code == KeyCode::Enter => {
                    quit_confirmation = None;
                    if let Some(active_completion) = completion.take() {
                        let should_submit_after_completion = active_completion.replacement.0 == 0
                            && active_completion
                                .items
                                .get(active_completion.selected)
                                .is_some_and(|item| item.label.starts_with('/'));
                        replace_completion(&mut input, &mut input_cursor, &active_completion);
                        reset_input_history_navigation(
                            &mut input_history_index,
                            &mut input_history_draft,
                        );
                        if !should_submit_after_completion {
                            completion = refresh_completion(
                                bottom_panel.is_none(),
                                &input,
                                input_cursor,
                                &completion_entries,
                                &mut completion_file_indexer,
                            );
                            continue;
                        }
                    }
                    if bottom_panel
                        .as_ref()
                        .is_some_and(|panel| panel.command == "/approval")
                    {
                        if bottom_panel
                            .as_ref()
                            .is_some_and(BottomPanel::guards_initial_submit)
                        {
                            continue;
                        }
                        let decision = bottom_panel
                            .as_ref()
                            .map(approval_decision)
                            .unwrap_or(ApprovalDecision::AllowOnce);
                        respond_pending_turn(&mut running_turn, &mut transcript, decision);
                        bottom_panel = None;
                        continue;
                    }
                    if bottom_panel
                        .as_ref()
                        .is_some_and(|panel| panel.command == "/question")
                    {
                        if bottom_panel
                            .as_ref()
                            .is_some_and(BottomPanel::guards_initial_submit)
                        {
                            continue;
                        }
                        if let Some(panel) = bottom_panel.as_mut()
                            && let Some(response) = advance_question_panel(panel)
                        {
                            respond_pending_question(&mut running_turn, response);
                            bottom_panel = None;
                        }
                        continue;
                    }
                    if let Some(panel) = bottom_panel.as_mut()
                        && (panel.command == "/voice"
                            || (panel.command == "/config" && panel.selected == 2))
                    {
                        panel.toggle();
                        continue;
                    }
                    if let Some(panel) = bottom_panel.take() {
                        if panel.command == "/resume" {
                            if let Some(saved) = panel.selected_session() {
                                match Session::resume(config.clone(), saved.session_dir.clone()) {
                                    Ok(resumed) => {
                                        session = Some(resumed);
                                        let session_ref = session.as_ref().expect("session set");
                                        transcript = startup_lines(
                                            session_ref.agent.model(),
                                            model_count,
                                            mcp_server_summary(&config),
                                            skill_count,
                                            show_initializing,
                                        );
                                        clear_initializing(&mut transcript);
                                        transcript.extend(resumed_session_lines(
                                            session_ref.agent.messages(),
                                        ));
                                        transcript.push(format!(
                                            "  ⎣ Resumed session {}",
                                            short_session_id(&saved.session_id.0)
                                        ));
                                        transcript.extend([String::new(), String::new()]);
                                    }
                                    Err(error) => {
                                        transcript.push(format!(
                                            "  ⎣ Error: Failed to load session: {error}"
                                        ));
                                        transcript.extend([String::new(), String::new()]);
                                    }
                                }
                            }
                        } else if panel.command == "/rewind" {
                            if let Some(index) = panel.rewind_message_index {
                                let Some(active_session) = session.as_mut() else {
                                    continue;
                                };
                                match active_session.rewind_to_message(index).await {
                                    Ok(message_content) => {
                                        input = message_content;
                                        input_cursor = input.len();
                                        transcript = startup_lines(
                                            active_session.agent.model(),
                                            model_count,
                                            mcp_server_summary(&config),
                                            skill_count,
                                            show_initializing,
                                        );
                                        clear_initializing(&mut transcript);
                                    }
                                    Err(error) => {
                                        transcript.push(format!(
                                            "  ⎣ Error: Failed to rewind session: {error}"
                                        ));
                                        transcript.extend([String::new(), String::new()]);
                                    }
                                }
                            }
                        } else {
                            apply_panel_selection(&mut transcript, &panel);
                        }
                        continue;
                    }
                    if running_turn.is_some() || session.is_none() {
                        let command = input.trim();
                        if command.is_empty() {
                            continue;
                        }
                        if command.starts_with('/') || command.starts_with('&') {
                            continue;
                        }
                        let submitted = std::mem::take(&mut input);
                        input_cursor = 0;
                        add_input_history(&mut input_history, &submitted);
                        reset_input_history_navigation(
                            &mut input_history_index,
                            &mut input_history_draft,
                        );
                        append_queued_input_lines(&mut transcript, &mut queued_inputs, submitted);
                        completion = None;
                        continue;
                    }
                    let submitted = std::mem::take(&mut input);
                    input_cursor = 0;
                    let command = submitted.trim();
                    add_input_history(&mut input_history, &submitted);
                    reset_input_history_navigation(
                        &mut input_history_index,
                        &mut input_history_draft,
                    );
                    if matches!(
                        command,
                        "/quit" | "/exit" | "quit" | "exit" | ":q" | ":quit"
                    ) {
                        break;
                    }
                    if command == "/help" {
                        bottom_panel = None;
                        transcript = help_lines();
                        continue;
                    }
                    if command == "/debug" {
                        clear_initializing(&mut transcript);
                        transcript.push(slash_command_line(command));
                        debug_console = !debug_console;
                        continue;
                    }
                    if command
                        .strip_prefix("/loop")
                        .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
                    {
                        bottom_panel = None;
                        clear_initializing(&mut transcript);
                        transcript.extend(loop_command_lines(command, &mut scheduled_loops));
                        continue;
                    }
                    if matches!(command, "/resume" | "/continue") {
                        let sessions = std::env::current_dir()
                            .ok()
                            .and_then(|cwd| SessionStore::list_for_cwd(&cwd).ok())
                            .unwrap_or_default();
                        if let Some(panel) = resume_panel(&sessions) {
                            clear_initializing(&mut transcript);
                            transcript.push(slash_command_line(command));
                            transcript.extend([String::new(), String::new()]);
                            bottom_panel = Some(panel);
                            continue;
                        }
                    }
                    if let Some(extra) = command
                        .strip_prefix("/compact")
                        .filter(|rest| rest.is_empty() || rest.starts_with(' '))
                    {
                        let Some(active_session) = session.as_mut() else {
                            continue;
                        };
                        if active_session
                            .agent
                            .messages()
                            .iter()
                            .filter(|message| message.role != Role::System)
                            .count()
                            > 0
                        {
                            match active_session.compact(extra).await {
                                Ok((old_session, new_session)) => {
                                    transcript = startup_lines(
                                        active_session.agent.model(),
                                        model_count,
                                        mcp_server_summary(&config),
                                        skill_count,
                                        show_initializing,
                                    );
                                    clear_initializing(&mut transcript);
                                    if transcript.last().is_some_and(String::is_empty) {
                                        transcript.pop();
                                    }
                                    transcript.push("✓ Compaction completed.".to_string());
                                    transcript.push(format!(
                                        "  session: {} (before compaction) → {} (after compaction)",
                                        short_session_id(&old_session.0),
                                        short_session_id(&new_session.0)
                                    ));
                                    transcript.extend([String::new(), String::new()]);
                                }
                                Err(error) => {
                                    clear_initializing(&mut transcript);
                                    transcript.push(slash_command_line(command));
                                    transcript.push(format!("  ⎣ Error: {error}"));
                                    transcript.extend([String::new(), String::new()]);
                                }
                            }
                            continue;
                        }
                    }
                    if command == "/rewind" {
                        let Some(active_session) = session.as_ref() else {
                            continue;
                        };
                        if let Some(panel) = rewind_panel(active_session.agent.messages()) {
                            clear_initializing(&mut transcript);
                            transcript.push(slash_command_line(command));
                            transcript.extend([String::new(), String::new()]);
                            bottom_panel = Some(panel);
                            continue;
                        }
                    }
                    if let Some(panel) = bottom_panel_for_command(command, &config, &mcp_index) {
                        clear_initializing(&mut transcript);
                        transcript.push(slash_command_line(command));
                        if command == "/config" {
                            transcript.push("  ⎣ Configuration opened...".to_string());
                        } else if command == "/proxy-setup" {
                            transcript.push("  ⎣ Proxy setup opened...".to_string());
                        } else if command == "/voice" {
                            transcript.push("  ⎣ Voice settings opened...".to_string());
                        } else if command == "/mcp"
                            || command == "/connectors"
                            || command.starts_with("/mcp ")
                            || command.starts_with("/connectors ")
                        {
                            transcript.push("  ⎣ MCP servers opened...".to_string());
                        }
                        transcript.extend([String::new(), String::new()]);
                        bottom_panel = Some(panel);
                        continue;
                    }
                    if command == "/log" {
                        let Some(active_session) = session.as_ref() else {
                            continue;
                        };
                        bottom_panel = None;
                        clear_initializing(&mut transcript);
                        transcript.extend(log_lines(&active_session.store));
                        continue;
                    }
                    if command == "/copy" {
                        bottom_panel = None;
                        clear_initializing(&mut transcript);
                        transcript.push(slash_command_line(command));
                        if let Some(text) = last_assistant_text.as_deref() {
                            let _ = copy_text_to_clipboard(text);
                        }
                        transcript.extend([String::new(), String::new()]);
                        continue;
                    }
                    if let Some(shell_command) = submitted.strip_prefix('!') {
                        bottom_panel = None;
                        clear_initializing(&mut transcript);
                        set_single_trailing_blank(&mut transcript);
                        if shell_command.is_empty() {
                            transcript.extend(manual_bash_empty_lines());
                            continue;
                        }
                        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                        let result = run_manual_bash_command(shell_command).await;
                        if let Some(active_session) = session.as_mut() {
                            active_session
                                .agent
                                .inject_user_context(manual_bash_context(
                                    shell_command,
                                    &cwd,
                                    &result,
                                    bash_max_output_bytes(&config),
                                ));
                            let _ = active_session.save().await;
                        }
                        transcript.extend(manual_bash_display_lines(shell_command, &result));
                        continue;
                    }
                    if let Some(target) = submitted.strip_prefix('&') {
                        bottom_panel = None;
                        clear_initializing(&mut transcript);
                        transcript.extend(teleport_unavailable_lines(target));
                        continue;
                    }
                    let visible_input =
                        expand_skill_prompt(&submitted).unwrap_or_else(|| submitted.clone());
                    if let Some(title) = command.strip_prefix("/rename ") {
                        let Some(active_session) = session.as_mut() else {
                            continue;
                        };
                        bottom_panel = None;
                        clear_initializing(&mut transcript);
                        transcript.push(slash_command_line(command));
                        match active_session.store.rename(title) {
                            Ok(renamed) => {
                                transcript.push(format!("  ⎣ Session renamed to \"{renamed}\"."));
                            }
                            Err(error) => {
                                transcript
                                    .push(format!("  ⎣ Error: Failed to rename session: {error}"));
                            }
                        }
                        transcript.extend([String::new(), String::new()]);
                        continue;
                    }
                    if let Some(lines) = static_command_lines(command, &config) {
                        bottom_panel = None;
                        clear_initializing(&mut transcript);
                        transcript.extend(lines);
                        continue;
                    }
                    clear_initializing(&mut transcript);
                    if let Some(active_session) = session.take() {
                        match build_turn_payload(
                            &visible_input,
                            &config,
                            &active_session.store.session_dir,
                        ) {
                            Ok(payload) => {
                                transcript.extend(format_user_prompt_lines_with_images(
                                    &visible_input,
                                    &payload.images,
                                ));
                                transcript.push("─".repeat(120));
                                transcript.push(String::new());
                                let tx_turn = tx.clone();
                                let approval_tx_turn = approval_tx.clone();
                                let question_tx_turn = question_tx.clone();
                                running_turn = Some(spawn_agent_turn(
                                    active_session,
                                    visible_input,
                                    payload,
                                    tx_turn,
                                    approval_tx_turn,
                                    question_tx_turn,
                                ));
                            }
                            Err(lines) => {
                                set_single_trailing_blank(&mut transcript);
                                transcript.extend(lines);
                                transcript.push(String::new());
                                input = submitted;
                                input_cursor = input.len();
                                session = Some(active_session);
                            }
                        }
                    }
                }
                Event::Key(key) if key.code == KeyCode::Backspace => {
                    if let Some(panel) = bottom_panel.as_mut() {
                        if panel.command == "/proxy-setup" {
                            panel.pop_proxy_char();
                            continue;
                        }
                        if panel.command == "/question" && panel.question_accepts_other_text() {
                            panel.pop_question_other_char();
                            continue;
                        }
                    }
                    if bottom_panel.is_none() && key.modifiers.contains(KeyModifiers::SUPER) {
                        delete_to_line_start(&mut input, &mut input_cursor);
                    } else if bottom_panel.is_none()
                        && key
                            .modifiers
                            .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
                    {
                        delete_word_left(&mut input, &mut input_cursor);
                    } else {
                        backspace_input(&mut input, &mut input_cursor);
                    }
                    reset_input_history_navigation(
                        &mut input_history_index,
                        &mut input_history_draft,
                    );
                    completion = refresh_completion(
                        bottom_panel.is_none(),
                        &input,
                        input_cursor,
                        &completion_entries,
                        &mut completion_file_indexer,
                    );
                }
                Event::Key(key) if key.code == KeyCode::Delete && bottom_panel.is_none() => {
                    if key.modifiers.contains(KeyModifiers::SUPER) {
                        delete_to_line_end(&mut input, &mut input_cursor);
                    } else if key
                        .modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
                    {
                        delete_word_right(&mut input, &mut input_cursor);
                    } else {
                        delete_input_right(&mut input, &mut input_cursor);
                    }
                    reset_input_history_navigation(
                        &mut input_history_index,
                        &mut input_history_draft,
                    );
                    completion = refresh_completion(
                        true,
                        &input,
                        input_cursor,
                        &completion_entries,
                        &mut completion_file_indexer,
                    );
                }
                Event::Key(key) if key.code == KeyCode::Left && bottom_panel.is_none() => {
                    input_cursor = previous_input_boundary(&input, input_cursor);
                    completion = refresh_completion(
                        true,
                        &input,
                        input_cursor,
                        &completion_entries,
                        &mut completion_file_indexer,
                    );
                }
                Event::Key(key) if key.code == KeyCode::Right && bottom_panel.is_none() => {
                    input_cursor = next_input_boundary(&input, input_cursor);
                    completion = refresh_completion(
                        true,
                        &input,
                        input_cursor,
                        &completion_entries,
                        &mut completion_file_indexer,
                    );
                }
                Event::Key(key) if key.code == KeyCode::Down => {
                    if key.modifiers.contains(KeyModifiers::ALT) {
                        if let Some(active_session) = session.as_ref()
                            && let Some(current) = bottom_panel.as_ref().and_then(|panel| {
                                if panel.command == "/rewind" {
                                    panel.rewind_message_index
                                } else {
                                    None
                                }
                            })
                            && let Some(panel) =
                                rewind_next_panel(active_session.agent.messages(), current)
                        {
                            bottom_panel = Some(panel);
                            clear_initializing(&mut transcript);
                        }
                    } else if let Some(active_completion) = completion.as_mut()
                        && bottom_panel.is_none()
                    {
                        if !active_completion.items.is_empty() {
                            active_completion.selected =
                                (active_completion.selected + 1) % active_completion.items.len();
                        }
                    } else if let Some(panel) = bottom_panel.as_mut() {
                        panel.select_next();
                    } else {
                        input_history_next(
                            &input_history,
                            &mut input,
                            &mut input_history_index,
                            &mut input_history_draft,
                        );
                        input_cursor = input.len();
                        completion = refresh_completion(
                            true,
                            &input,
                            input_cursor,
                            &completion_entries,
                            &mut completion_file_indexer,
                        );
                    }
                }
                Event::Key(key) if key.code == KeyCode::Up => {
                    if key.modifiers.contains(KeyModifiers::ALT) {
                        if let Some(active_session) = session.as_ref() {
                            let current = bottom_panel.as_ref().and_then(|panel| {
                                if panel.command == "/rewind" {
                                    panel.rewind_message_index
                                } else {
                                    None
                                }
                            });
                            if let Some(panel) =
                                rewind_previous_panel(active_session.agent.messages(), current)
                            {
                                bottom_panel = Some(panel);
                                clear_initializing(&mut transcript);
                            }
                        }
                    } else if let Some(active_completion) = completion.as_mut()
                        && bottom_panel.is_none()
                    {
                        if !active_completion.items.is_empty() {
                            active_completion.selected = active_completion
                                .selected
                                .checked_sub(1)
                                .unwrap_or(active_completion.items.len() - 1);
                        }
                    } else if let Some(panel) = bottom_panel.as_mut() {
                        panel.select_previous();
                    } else {
                        input_history_previous(
                            &input_history,
                            &mut input,
                            &mut input_history_index,
                            &mut input_history_draft,
                        );
                        input_cursor = input.len();
                        completion = refresh_completion(
                            true,
                            &input,
                            input_cursor,
                            &completion_entries,
                            &mut completion_file_indexer,
                        );
                    }
                }
                Event::Key(key) if key.code == KeyCode::Char(' ') && bottom_panel.is_some() => {
                    if let Some(panel) = bottom_panel.as_mut() {
                        if panel.command == "/question" && panel.question_accepts_other_text() {
                            panel.push_question_other_char(' ');
                            continue;
                        }
                        panel.toggle();
                    }
                }
                Event::Key(key)
                    if matches!(key.code, KeyCode::Char('1') | KeyCode::Char('y'))
                        && bottom_panel
                            .as_ref()
                            .is_some_and(|panel| panel.command == "/approval") =>
                {
                    if bottom_panel
                        .as_ref()
                        .is_some_and(BottomPanel::guards_initial_submit)
                    {
                        continue;
                    }
                    respond_pending_turn(
                        &mut running_turn,
                        &mut transcript,
                        ApprovalDecision::AllowOnce,
                    );
                    bottom_panel = None;
                }
                Event::Key(key)
                    if matches!(key.code, KeyCode::Char('2'))
                        && bottom_panel
                            .as_ref()
                            .is_some_and(|panel| panel.command == "/approval") =>
                {
                    if bottom_panel
                        .as_ref()
                        .is_some_and(BottomPanel::guards_initial_submit)
                    {
                        continue;
                    }
                    if let Some(panel) = bottom_panel.as_mut() {
                        select_approval_option(panel, 1);
                    }
                    mark_pending_tool_selected(&mut transcript);
                }
                Event::Key(key)
                    if matches!(key.code, KeyCode::Char('3'))
                        && bottom_panel
                            .as_ref()
                            .is_some_and(|panel| panel.command == "/approval") =>
                {
                    if bottom_panel
                        .as_ref()
                        .is_some_and(BottomPanel::guards_initial_submit)
                    {
                        continue;
                    }
                    if let Some(panel) = bottom_panel.as_mut() {
                        select_approval_option(panel, 2);
                    }
                    mark_pending_tool_selected(&mut transcript);
                }
                Event::Key(key)
                    if matches!(key.code, KeyCode::Char('4') | KeyCode::Char('n'))
                        && bottom_panel
                            .as_ref()
                            .is_some_and(|panel| panel.command == "/approval") =>
                {
                    if bottom_panel
                        .as_ref()
                        .is_some_and(BottomPanel::guards_initial_submit)
                    {
                        continue;
                    }
                    respond_pending_turn(
                        &mut running_turn,
                        &mut transcript,
                        ApprovalDecision::Deny,
                    );
                    bottom_panel = None;
                }
                Event::Key(key)
                    if matches!(key.code, KeyCode::Char('1'))
                        && bottom_panel
                            .as_ref()
                            .is_some_and(|panel| panel.command == "/question") =>
                {
                    if bottom_panel
                        .as_ref()
                        .is_some_and(BottomPanel::guards_initial_submit)
                    {
                        continue;
                    }
                    if let Some(panel) = bottom_panel.as_mut() {
                        panel.selected = 0;
                        if let Some(response) = advance_question_panel(panel) {
                            respond_pending_question(&mut running_turn, response);
                            bottom_panel = None;
                        }
                    }
                }
                Event::Key(key)
                    if matches!(key.code, KeyCode::Char('2'))
                        && bottom_panel
                            .as_ref()
                            .is_some_and(|panel| panel.command == "/question") =>
                {
                    if bottom_panel
                        .as_ref()
                        .is_some_and(BottomPanel::guards_initial_submit)
                    {
                        continue;
                    }
                    if let Some(panel) = bottom_panel.as_mut() {
                        panel.selected = 1;
                        if let Some(response) = advance_question_panel(panel) {
                            respond_pending_question(&mut running_turn, response);
                            bottom_panel = None;
                        }
                    }
                }
                Event::Key(key)
                    if matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D'))
                        && bottom_panel
                            .as_ref()
                            .is_some_and(|panel| panel.command == "/resume") =>
                {
                    if let Some(panel) = bottom_panel.as_mut()
                        && let Some(deleted) = panel.request_delete()
                    {
                        let short = short_session_id(&deleted.session_id.0);
                        match SessionStore::delete_saved(&deleted.session_id.0) {
                            Ok(_) => {
                                while transcript
                                    .last()
                                    .map(|line| line.is_empty())
                                    .unwrap_or(false)
                                {
                                    transcript.pop();
                                }
                                transcript.push(format!("  ⎣ Deleted session {short}."));
                                if panel.resume_sessions.is_empty() {
                                    bottom_panel = None;
                                    transcript.push(
                                        "  ⎣ No saved sessions left for this directory."
                                            .to_string(),
                                    );
                                    transcript.extend([String::new(), String::new()]);
                                }
                            }
                            Err(error) => {
                                transcript
                                    .push(format!("  ⎣ Error: Failed to delete session: {error}"));
                            }
                        }
                    }
                }
                Event::Key(key)
                    if matches!(key.code, KeyCode::Char('\n') | KeyCode::Char('\r'))
                        && input.trim() == "/copy" =>
                {
                    input.clear();
                    input_cursor = 0;
                    bottom_panel = None;
                    clear_initializing(&mut transcript);
                    transcript.push(slash_command_line("/copy"));
                    if let Some(text) = last_assistant_text.as_deref() {
                        let _ = copy_text_to_clipboard(text);
                    }
                    transcript.extend([String::new(), String::new()]);
                }
                Event::Key(key) => {
                    if let KeyCode::Char(ch) = key.code {
                        if let Some(panel) = bottom_panel.as_mut() {
                            if panel.command == "/proxy-setup" {
                                panel.push_proxy_char(ch);
                                continue;
                            }
                            if panel.command == "/question" && panel.question_accepts_other_text() {
                                panel.push_question_other_char(ch);
                                continue;
                            }
                        }
                        quit_confirmation = None;
                        insert_input_char(&mut input, &mut input_cursor, ch);
                        reset_input_history_navigation(
                            &mut input_history_index,
                            &mut input_history_draft,
                        );
                        completion = refresh_completion(
                            bottom_panel.is_none(),
                            &input,
                            input_cursor,
                            &completion_entries,
                            &mut completion_file_indexer,
                        );
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

const QUIT_CONFIRM_DELAY: Duration = Duration::from_secs(1);
const MAX_VISIBLE_COMPLETIONS: usize = 10;
const VIBE_ORANGE: Color = Color::Rgb(255, 130, 5);
const VIBE_FOREGROUND: Color = Color::Rgb(197, 200, 198);
const VIBE_SECONDARY: Color = Color::Rgb(104, 160, 179);
const VIBE_MUTED: Color = Color::Rgb(134, 136, 135);

fn input_panel_height(completion: Option<&CompletionSet>) -> u16 {
    let visible = completion
        .map(|completion| completion.items.len().min(MAX_VISIBLE_COMPLETIONS))
        .unwrap_or(0);
    if visible == 0 { 6 } else { visible as u16 + 8 }
}

fn input_panel_lines<'a>(
    mode_line: &'a str,
    input: &'a str,
    completion: Option<&'a CompletionSet>,
    separator: &'a str,
    footer_left: String,
    status: &'a str,
    gap: usize,
) -> Vec<Line<'a>> {
    let mut lines = Vec::new();
    if let Some(completion) = completion {
        let visible = completion.items.len().min(MAX_VISIBLE_COMPLETIONS);
        let start = completion
            .selected
            .saturating_add(1)
            .saturating_sub(visible);
        let width = separator.chars().count();
        let content_width = width.saturating_sub(2);
        lines.push(Line::styled(
            format!("┌{}┐", "─".repeat(content_width)),
            Style::default().fg(VIBE_MUTED),
        ));
        for (idx, item) in completion
            .items
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
        {
            let display_label = item.label.strip_prefix('@').unwrap_or(&item.label);
            let mut text = display_label.to_string();
            if !item.description.is_empty() {
                text.push_str("  ");
                text.push_str(&item.description);
            }
            let row_width = content_width.saturating_sub(1);
            let row = if text.chars().count() > row_width {
                text.chars().take(row_width).collect::<String>()
            } else {
                format!("{text:<row_width$}")
            };
            let style = if idx == completion.selected {
                Style::default()
                    .fg(VIBE_SECONDARY)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(VIBE_FOREGROUND)
            };
            lines.push(Line::styled(format!("│ {row}│"), style));
        }
        lines.push(Line::styled(
            format!("└{}┘", "─".repeat(content_width)),
            Style::default().fg(VIBE_MUTED),
        ));
    }
    lines.push(Line::styled(
        mode_line.to_string(),
        Style::default().fg(VIBE_MUTED),
    ));
    lines.push(Line::from(vec![
        Span::styled(
            "> ",
            Style::default()
                .fg(VIBE_ORANGE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(input.to_string(), Style::default().fg(VIBE_FOREGROUND)),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(" "));
    lines.push(Line::styled(
        separator.to_string(),
        Style::default().fg(VIBE_MUTED),
    ));
    lines.push(Line::from(vec![
        Span::styled(footer_left, Style::default().fg(VIBE_MUTED)),
        Span::raw(" ".repeat(gap)),
        Span::styled(status.to_string(), Style::default().fg(VIBE_MUTED)),
    ]));
    lines
}

fn styled_transcript_rows(text: &str) -> Vec<Line<'static>> {
    text.lines().map(styled_transcript_row).collect()
}

fn styled_transcript_row(row: &str) -> Line<'static> {
    if row.starts_with("Mistral Vibe") {
        return styled_banner_title_row(row);
    }
    if row == "Type /help for more information" {
        return Line::from(vec![
            Span::styled("Type ".to_string(), Style::default().fg(VIBE_FOREGROUND)),
            Span::styled("/help".to_string(), Style::default().fg(VIBE_SECONDARY)),
            Span::styled(
                " for more information".to_string(),
                Style::default().fg(VIBE_FOREGROUND),
            ),
        ]);
    }
    if let Some(prompt) = row.strip_prefix("> ") {
        return Line::from(vec![
            Span::styled(
                "> ".to_string(),
                Style::default()
                    .fg(VIBE_ORANGE)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(prompt.to_string(), Style::default().fg(VIBE_FOREGROUND)),
        ]);
    }

    let style = if row.chars().any(is_braille_char) {
        Style::default().fg(VIBE_FOREGROUND)
    } else if row.starts_with('─') || row.starts_with("  ⎣") {
        Style::default().fg(VIBE_MUTED)
    } else if row.contains("Initializing") || row.contains("Running ") {
        Style::default().fg(VIBE_ORANGE)
    } else if row.contains("Error") || row.contains("error:") {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(VIBE_FOREGROUND)
    };
    Line::styled(row.to_string(), style)
}

fn styled_banner_title_row(row: &str) -> Line<'static> {
    let Some((prefix, model)) = row.split_once(" · ") else {
        return Line::styled(
            row.to_string(),
            Style::default()
                .fg(VIBE_ORANGE)
                .add_modifier(Modifier::BOLD),
        );
    };
    let Some(version) = prefix.strip_prefix("Mistral Vibe ") else {
        return Line::styled(row.to_string(), Style::default().fg(VIBE_FOREGROUND));
    };
    Line::from(vec![
        Span::styled(
            "Mistral Vibe".to_string(),
            Style::default()
                .fg(VIBE_ORANGE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {version} · "),
            Style::default().fg(VIBE_FOREGROUND),
        ),
        Span::styled(model.to_string(), Style::default().fg(VIBE_SECONDARY)),
    ])
}

fn is_braille_char(ch: char) -> bool {
    ('\u{2800}'..='\u{28ff}').contains(&ch)
}

fn input_cursor_position(
    area: Rect,
    completion: Option<&CompletionSet>,
    input: &str,
    cursor: usize,
) -> Position {
    let visible_completions = completion
        .map(|completion| completion.items.len().min(MAX_VISIBLE_COMPLETIONS))
        .unwrap_or(0);
    let input_line_offset = if visible_completions == 0 {
        1
    } else {
        visible_completions as u16 + 3
    };
    let cursor = cursor.min(input.len());
    let before_cursor = if input.is_char_boundary(cursor) {
        &input[..cursor]
    } else {
        &input[..previous_input_boundary(input, cursor)]
    };
    let line_offset = before_cursor.chars().filter(|ch| *ch == '\n').count() as u16;
    let column = before_cursor
        .rsplit('\n')
        .next()
        .unwrap_or_default()
        .chars()
        .count() as u16;
    let x = area.x + 2 + column.min(area.width.saturating_sub(3));
    let y = area
        .y
        .saturating_add(input_line_offset)
        .saturating_add(line_offset)
        .min(area.y + area.height.saturating_sub(1));
    Position { x, y }
}

fn completion_command_entries() -> Vec<CommandEntry> {
    let mut entries = builtin_tui_command_specs()
        .into_iter()
        .map(|(alias, description)| CommandEntry {
            alias: alias.to_string(),
            description: description.to_string(),
        })
        .collect::<Vec<_>>();
    entries.extend(
        available_skill_commands()
            .into_iter()
            .map(|(alias, description)| CommandEntry { alias, description }),
    );
    entries.sort_by(|left, right| left.alias.cmp(&right.alias));
    entries
}

fn builtin_tui_command_specs() -> Vec<(&'static str, &'static str)> {
    vec![
        ("/compact", "Compact conversation history by summarizing"),
        ("/config", "Open configuration"),
        ("/connectors", "Show MCP connectors"),
        ("/copy", "Copy the last assistant response"),
        ("/data-retention", "Show data retention information"),
        ("/debug", "Toggle debug console"),
        ("/help", "Show available commands and keyboard shortcuts"),
        ("/log", "Show path to current interaction log file"),
        ("/loop", "Schedule repeated prompts"),
        ("/mcp", "Show MCP servers"),
        ("/model", "Switch model"),
        (
            "/proxy-setup",
            "Configure proxy and SSL certificate settings",
        ),
        (
            "/reload",
            "Reload configuration, agent instructions, and skills",
        ),
        ("/rename", "Rename the current session"),
        ("/resume", "Resume a saved session"),
        ("/rewind", "Rewind the conversation"),
        ("/teleport", "Teleport session to Vibe Code Web"),
        ("/theme", "Switch theme"),
        ("/thinking", "Switch thinking mode"),
        ("/voice", "Configure voice mode"),
    ]
}

fn available_skill_commands() -> Vec<(String, String)> {
    let skills_dir = vibe_home_dir().join("skills");
    let Ok(entries) = fs::read_dir(skills_dir) else {
        return Vec::new();
    };
    let mut skills = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let skill_md = entry.path().join("SKILL.md");
            let raw = fs::read_to_string(skill_md).ok()?;
            let name = entry.file_name().to_string_lossy().to_string();
            let description = raw
                .lines()
                .find_map(|line| line.trim().strip_prefix("description: "))
                .unwrap_or("Load skill")
                .trim()
                .to_string();
            Some((format!("/{name}"), description))
        })
        .collect::<Vec<_>>();
    skills.sort_by(|left, right| left.0.cmp(&right.0));
    skills
}

fn refresh_completion(
    enabled: bool,
    input: &str,
    cursor: usize,
    command_entries: &[CommandEntry],
    file_indexer: &mut FileIndexer,
) -> Option<CompletionSet> {
    if !enabled || input.contains('\n') {
        return None;
    }
    command_completions(input, cursor, command_entries).or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|cwd| path_completions_with_indexer(file_indexer, &cwd, input, cursor))
    })
}

struct QuitConfirmation {
    key: &'static str,
    prompt: String,
    started: Instant,
}

impl QuitConfirmation {
    fn new(key: &'static str) -> Self {
        Self {
            key,
            prompt: format!("Press {key} again to quit"),
            started: Instant::now(),
        }
    }
}

#[derive(Clone, Debug)]
struct ToolRenderRecord {
    collapsed: Vec<String>,
    expanded: Vec<String>,
}

fn apply_tool_collapse_state(
    transcript: &mut Vec<String>,
    records: &[ToolRenderRecord],
    collapsed: bool,
) {
    for record in records {
        let current = if collapsed {
            &record.expanded
        } else {
            &record.collapsed
        };
        let replacement = if collapsed {
            &record.collapsed
        } else {
            &record.expanded
        };
        replace_first_line_sequence(transcript, current, replacement);
    }
}

fn replace_first_line_sequence(
    transcript: &mut Vec<String>,
    current: &[String],
    replacement: &[String],
) -> bool {
    if current.is_empty() || current.len() > transcript.len() {
        return false;
    }
    for start in 0..=transcript.len() - current.len() {
        if transcript[start..start + current.len()] == *current {
            transcript.splice(start..start + current.len(), replacement.iter().cloned());
            return true;
        }
    }
    false
}

struct RunningTurn {
    handle: JoinHandle<(Session, Result<()>)>,
    approval: Option<ApprovalRequest>,
    question: Option<QuestionRequest>,
    queued_decision: Option<ApprovalDecision>,
    queued_question: Option<QuestionResponse>,
}

fn spawn_agent_turn(
    mut active_session: Session,
    visible_input: String,
    payload: TurnPayload,
    tx_turn: mpsc::UnboundedSender<AgentEvent>,
    approval_tx_turn: mpsc::UnboundedSender<ApprovalRequest>,
    question_tx_turn: mpsc::UnboundedSender<QuestionRequest>,
) -> RunningTurn {
    RunningTurn {
        approval: None,
        question: None,
        queued_decision: None,
        queued_question: None,
        handle: tokio::spawn(async move {
            let result = active_session
                .agent
                .run_turn_with_interaction_and_images(
                    payload.model_input,
                    Some(visible_input),
                    payload.images,
                    tx_turn,
                    approval_tx_turn,
                    question_tx_turn,
                )
                .await;
            (active_session, result)
        }),
    }
}

fn append_queued_input_lines(
    transcript: &mut Vec<String>,
    queued_inputs: &mut VecDeque<String>,
    submitted: String,
) {
    if queued_inputs.is_empty() {
        set_single_trailing_blank(transcript);
        transcript.push("» Queued".to_string());
    }
    transcript.extend(format_user_prompt_lines(&submitted));
    queued_inputs.push_back(submitted);
}

fn remove_queued_input_lines(transcript: &mut Vec<String>, submitted: &str, remove_header: bool) {
    let lines = format_user_prompt_lines(submitted);
    replace_first_line_sequence(transcript, &lines, &[]);
    if remove_header && let Some(index) = transcript.iter().position(|line| line == "» Queued") {
        transcript.remove(index);
        while transcript.get(index).is_some_and(|line| line.is_empty())
            && transcript
                .get(index.saturating_sub(1))
                .is_some_and(|line| line.is_empty())
        {
            transcript.remove(index);
        }
    }
}

const PROXY_VARS: [(&str, &str); 6] = [
    ("HTTP_PROXY", "Proxy URL for HTTP requests"),
    ("HTTPS_PROXY", "Proxy URL for HTTPS requests"),
    ("ALL_PROXY", "Proxy URL for all requests (fallback)"),
    ("NO_PROXY", "Comma-separated list of hosts to bypass proxy"),
    ("SSL_CERT_FILE", "Path to custom SSL certificate file"),
    (
        "SSL_CERT_DIR",
        "Path to directory containing SSL certificates",
    ),
];

#[derive(Clone, Debug)]
struct BottomPanel {
    command: String,
    title: String,
    options: Vec<String>,
    selected: usize,
    help: String,
    scroll_marker: Option<usize>,
    raw_rows: Option<Vec<String>>,
    toggled: bool,
    auto_copy_on: bool,
    resume_sessions: Vec<SavedSession>,
    delete_confirm: Option<String>,
    rewind_message_index: Option<usize>,
    question_call: Option<ToolCall>,
    question_index: usize,
    question_answers: Vec<QuestionAnswer>,
    question_selected_options: Vec<usize>,
    question_other_texts: Vec<String>,
    proxy_values: Vec<String>,
    mounted_at: Instant,
}

#[derive(Clone, Debug, Default)]
struct McpIndex {
    servers: Vec<McpServerTools>,
}

#[derive(Clone, Debug)]
struct McpServerTools {
    name: String,
    transport: String,
    disabled: bool,
    tools: Vec<McpToolInfo>,
}

#[derive(Clone, Debug)]
struct McpToolInfo {
    name: String,
    description: String,
    enabled: bool,
}

impl BottomPanel {
    fn height(&self) -> u16 {
        let content_rows = if self.command == "/config" {
            6
        } else if self.command == "/resume" {
            self.resume_sessions.len().min(5) + 2
        } else if self.command == "/rewind" {
            return (self.options.len() + 7) as u16;
        } else if self.command == "/question" {
            self.options.len() + 2
        } else if self.command == "/proxy-setup" {
            PROXY_VARS.len() * 2
        } else {
            self.raw_rows
                .as_ref()
                .map(|rows| rows.len())
                .unwrap_or_else(|| self.options.len() + 1)
        };
        (content_rows + 5) as u16
    }

    fn select_next(&mut self) {
        if let Some(max) = self
            .selectable_count()
            .and_then(|count| count.checked_sub(1))
        {
            self.selected = (self.selected + 1).min(max);
        }
    }

    fn select_previous(&mut self) {
        if self.selectable_count().is_some() {
            self.selected = self.selected.saturating_sub(1);
        }
    }

    fn toggle(&mut self) {
        if self.command == "/voice" {
            self.toggled = !self.toggled;
        } else if self.command == "/config" && self.selected == 2 {
            self.auto_copy_on = !self.auto_copy_on;
        }
    }

    fn selectable_count(&self) -> Option<usize> {
        if self.command == "/question" {
            Some(self.options.len() + usize::from(self.question_is_multi_select()))
        } else if !self.options.is_empty() {
            Some(self.options.len())
        } else if self.command == "/resume" {
            Some(self.resume_sessions.len())
        } else {
            match self.command.as_str() {
                "/config" => Some(4),
                "/voice" => Some(2),
                "/proxy-setup" => Some(PROXY_VARS.len()),
                "/mcp" if self.title == "MCP Servers" => self
                    .raw_rows
                    .as_ref()
                    .map(|rows| rows.len().saturating_sub(2)),
                "/mcp" if self.title.starts_with("MCP Server: ") => {
                    self.raw_rows.as_ref().map(|rows| {
                        rows.iter()
                            .skip(1)
                            .filter(|row| !row.trim_start().starts_with("No tools discovered"))
                            .count()
                    })
                }
                _ => None,
            }
        }
    }

    fn proxy_value_mut(&mut self) -> &mut String {
        if self.proxy_values.len() < PROXY_VARS.len() {
            self.proxy_values.resize_with(PROXY_VARS.len(), String::new);
        }
        &mut self.proxy_values[self.selected.min(PROXY_VARS.len() - 1)]
    }

    fn push_proxy_char(&mut self, ch: char) {
        self.proxy_value_mut().push(ch);
    }

    fn pop_proxy_char(&mut self) {
        self.proxy_value_mut().pop();
    }

    fn question_is_multi_select(&self) -> bool {
        self.question_call
            .as_ref()
            .is_some_and(|call| question_is_multi_select_at(call, self.question_index))
    }

    fn question_submit_index(&self) -> usize {
        self.options.len()
    }

    fn question_other_index(&self) -> Option<usize> {
        if question_hide_other(self.question_call.as_ref()?, self.question_index) {
            None
        } else {
            self.options.len().checked_sub(1)
        }
    }

    fn question_accepts_other_text(&self) -> bool {
        self.question_other_index() == Some(self.selected)
    }

    fn question_other_text(&self) -> &str {
        self.question_other_texts
            .get(self.question_index)
            .map(String::as_str)
            .unwrap_or_default()
    }

    fn question_other_text_mut(&mut self) -> &mut String {
        if self.question_other_texts.len() <= self.question_index {
            self.question_other_texts
                .resize_with(self.question_index + 1, String::new);
        }
        &mut self.question_other_texts[self.question_index]
    }

    fn push_question_other_char(&mut self, ch: char) {
        self.question_other_text_mut().push(ch);
        if self.question_is_multi_select()
            && let Some(other_idx) = self.question_other_index()
            && !self.question_selected_options.contains(&other_idx)
        {
            self.question_selected_options.push(other_idx);
            self.question_selected_options.sort_unstable();
        }
    }

    fn pop_question_other_char(&mut self) {
        self.question_other_text_mut().pop();
        if self.question_is_multi_select()
            && self.question_other_text().trim().is_empty()
            && let Some(other_idx) = self.question_other_index()
        {
            self.question_selected_options
                .retain(|idx| *idx != other_idx);
        }
    }

    fn guards_initial_submit(&self) -> bool {
        matches!(self.command.as_str(), "/approval" | "/question")
            && self.mounted_at.elapsed() < INPUT_GRACE_PERIOD
    }

    fn selected_session(&self) -> Option<&SavedSession> {
        self.resume_sessions.get(self.selected)
    }

    fn request_delete(&mut self) -> Option<SavedSession> {
        let session = self.selected_session()?.clone();
        if self.delete_confirm.as_deref() == Some(&session.session_id.0) {
            self.resume_sessions
                .retain(|item| item.session_id.0 != session.session_id.0);
            if self.selected >= self.resume_sessions.len() {
                self.selected = self.resume_sessions.len().saturating_sub(1);
            }
            self.delete_confirm = None;
            Some(session)
        } else {
            self.delete_confirm = Some(session.session_id.0);
            None
        }
    }
}

fn bottom_panel_for_command(
    command: &str,
    config: &Config,
    mcp_index: &McpIndex,
) -> Option<BottomPanel> {
    match command {
        "/mcp" | "/connectors" if !config.mcp_servers.is_empty() => Some(mcp_panel(mcp_index)),
        _ if command.starts_with("/mcp ") || command.starts_with("/connectors ") => {
            let name = command.split_once(' ')?.1.trim();
            mcp_detail_panel(mcp_index, name)
        }
        "/model" => Some(BottomPanel {
            command: "/model".to_string(),
            title: "Select Model".to_string(),
            options: strings(&["mistral-medium-3.5", "devstral-small", "local"]),
            selected: 0,
            help: "↑↓ Navigate  Enter Select  Esc Cancel".to_string(),
            scroll_marker: None,
            raw_rows: None,
            toggled: false,
            auto_copy_on: false,
            resume_sessions: Vec::new(),
            delete_confirm: None,
            rewind_message_index: None,
            question_call: None,
            question_index: 0,
            question_answers: Vec::new(),
            question_selected_options: Vec::new(),
            question_other_texts: Vec::new(),
            proxy_values: Vec::new(),
            mounted_at: Instant::now(),
        }),
        "/thinking" => Some(BottomPanel {
            command: "/thinking".to_string(),
            title: "Select Thinking Level".to_string(),
            options: strings(&["Off", "Low", "Medium", "High", "Max"]),
            selected: 3,
            help: "↑↓ Navigate  Enter Select  Esc Cancel".to_string(),
            scroll_marker: None,
            raw_rows: None,
            toggled: false,
            auto_copy_on: false,
            resume_sessions: Vec::new(),
            delete_confirm: None,
            rewind_message_index: None,
            question_call: None,
            question_index: 0,
            question_answers: Vec::new(),
            question_selected_options: Vec::new(),
            question_other_texts: Vec::new(),
            proxy_values: Vec::new(),
            mounted_at: Instant::now(),
        }),
        "/theme" => Some(BottomPanel {
            command: "/theme".to_string(),
            title: "Select Theme".to_string(),
            options: strings(&[
                "ansi-light",
                "atom-one-light",
                "catppuccin-latte",
                "rose-pine-dawn",
                "solarized-light",
                "textual-light",
                "ansi-dark",
                "atom-one-dark",
                "catppuccin-frappe",
                "catppuccin-macchiato",
                "catppuccin-mocha",
                "dracula",
                "flexoki",
                "gruvbox",
                "monokai",
                "nord",
                "rose-pine",
                "rose-pine-moon",
            ]),
            selected: 6,
            help: "↑↓ Preview  Enter Select  Esc Cancel".to_string(),
            scroll_marker: Some(15),
            raw_rows: None,
            toggled: false,
            auto_copy_on: false,
            resume_sessions: Vec::new(),
            delete_confirm: None,
            rewind_message_index: None,
            question_call: None,
            question_index: 0,
            question_answers: Vec::new(),
            question_selected_options: Vec::new(),
            question_other_texts: Vec::new(),
            proxy_values: Vec::new(),
            mounted_at: Instant::now(),
        }),
        "/config" => Some(BottomPanel {
            command: "/config".to_string(),
            title: "Settings".to_string(),
            options: Vec::new(),
            selected: 0,
            help: "↑↓ Navigate  Enter Select/Toggle  Esc Exit".to_string(),
            scroll_marker: None,
            raw_rows: None,
            toggled: false,
            auto_copy_on: true,
            resume_sessions: Vec::new(),
            delete_confirm: None,
            rewind_message_index: None,
            question_call: None,
            question_index: 0,
            question_answers: Vec::new(),
            question_selected_options: Vec::new(),
            question_other_texts: Vec::new(),
            proxy_values: Vec::new(),
            mounted_at: Instant::now(),
        }),
        "/proxy-setup" => Some(BottomPanel {
            command: "/proxy-setup".to_string(),
            title: "Proxy Configuration".to_string(),
            options: Vec::new(),
            selected: 0,
            help: "↑↓ navigate  Enter save & exit  ESC cancel".to_string(),
            scroll_marker: None,
            raw_rows: None,
            toggled: false,
            auto_copy_on: false,
            resume_sessions: Vec::new(),
            delete_confirm: None,
            rewind_message_index: None,
            question_call: None,
            question_index: 0,
            question_answers: Vec::new(),
            question_selected_options: Vec::new(),
            question_other_texts: Vec::new(),
            proxy_values: current_proxy_values(),
            mounted_at: Instant::now(),
        }),
        "/voice" => Some(BottomPanel {
            command: "/voice".to_string(),
            title: "Voice Settings".to_string(),
            options: Vec::new(),
            selected: 0,
            help: "↑↓ navigate  Space/Enter toggle  ESC exit".to_string(),
            scroll_marker: None,
            raw_rows: Some(strings(&[
                "",
                "› Voice mode: Off",
                "  Narrator (experimental): Off",
                "",
            ])),
            toggled: false,
            auto_copy_on: false,
            resume_sessions: Vec::new(),
            delete_confirm: None,
            rewind_message_index: None,
            question_call: None,
            question_index: 0,
            question_answers: Vec::new(),
            question_selected_options: Vec::new(),
            question_other_texts: Vec::new(),
            proxy_values: Vec::new(),
            mounted_at: Instant::now(),
        }),
        _ => None,
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn mcp_panel(mcp_index: &McpIndex) -> BottomPanel {
    BottomPanel {
        command: "/mcp".to_string(),
        title: "MCP Servers".to_string(),
        options: Vec::new(),
        selected: 0,
        help: "↑↓ Navigate  Enter Show tools  D Disable  E Enable  R Refresh  Esc Close"
            .to_string(),
        scroll_marker: None,
        raw_rows: Some(mcp_panel_rows(mcp_index)),
        toggled: false,
        auto_copy_on: false,
        resume_sessions: Vec::new(),
        delete_confirm: None,
        rewind_message_index: None,
        question_call: None,
        question_index: 0,
        question_answers: Vec::new(),
        question_selected_options: Vec::new(),
        question_other_texts: Vec::new(),
        proxy_values: Vec::new(),
        mounted_at: Instant::now(),
    }
}

fn mcp_detail_panel(mcp_index: &McpIndex, name: &str) -> Option<BottomPanel> {
    let server = mcp_index
        .servers
        .iter()
        .find(|server| server.name == name)?;
    let has_tools = !server.tools.is_empty();
    Some(BottomPanel {
        command: "/mcp".to_string(),
        title: format!("MCP Server: {}", server.name),
        options: Vec::new(),
        selected: 0,
        help: if has_tools {
            "↑↓ Navigate  D Disable  E Enable  Backspace Back  R Refresh  Esc Close".to_string()
        } else {
            "↑↓ Navigate  Backspace Back  R Refresh  Esc Close".to_string()
        },
        scroll_marker: None,
        raw_rows: Some(mcp_detail_rows(server)),
        toggled: false,
        auto_copy_on: false,
        resume_sessions: Vec::new(),
        delete_confirm: None,
        rewind_message_index: None,
        question_call: None,
        question_index: 0,
        question_answers: Vec::new(),
        question_selected_options: Vec::new(),
        question_other_texts: Vec::new(),
        proxy_values: Vec::new(),
        mounted_at: Instant::now(),
    })
}

fn mcp_detail_rows(server: &McpServerTools) -> Vec<String> {
    let mut rows = vec![String::new()];
    if server.tools.is_empty() {
        rows.push(" No tools discovered for this server".to_string());
        return rows;
    }
    let mut tools = server.tools.clone();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    for tool in tools {
        let mut row = format!(" {}", tool.name);
        if !tool.description.is_empty() {
            row.push_str(&format!("  -  {}", tool.description));
        }
        if !tool.enabled {
            row.push_str("  (disabled)");
        }
        rows.push(row);
    }
    rows
}

fn mcp_panel_rows(mcp_index: &McpIndex) -> Vec<String> {
    let max_name = mcp_index
        .servers
        .iter()
        .map(|server| server.name.chars().count())
        .max()
        .unwrap_or(0);
    let max_type = mcp_index
        .servers
        .iter()
        .map(|server| server.transport.chars().count() + 2)
        .max()
        .unwrap_or(0);
    let mut rows = vec![String::new(), " Local MCP Servers".to_string()];
    for server in &mcp_index.servers {
        let type_tag = format!("[{}]", server.transport);
        let enabled = server.tools.iter().filter(|tool| tool.enabled).count();
        let total = server.tools.len();
        let mut row = format!(
            "   {:<name_width$}  {:<type_width$}  {}",
            server.name,
            type_tag,
            mcp_tool_count_text(enabled, total),
            name_width = max_name,
            type_width = max_type
        );
        if server.disabled {
            row.push_str("  ○ disabled");
        }
        rows.push(row);
    }
    rows
}

fn mcp_tool_count_text(enabled: usize, total: usize) -> String {
    if enabled < total {
        let noun = if total == 1 { "tool" } else { "tools" };
        return format!("{enabled}/{total} {noun}");
    }
    if enabled == 0 {
        return "no tools".to_string();
    }
    let noun = if enabled == 1 { "tool" } else { "tools" };
    format!("{enabled} {noun}")
}

fn toggle_mcp_panel(
    config: &mut Config,
    mcp_index: &mut McpIndex,
    bottom_panel: &mut Option<BottomPanel>,
    transcript: &mut [String],
    disabled: bool,
) {
    let Some(panel) = bottom_panel.as_mut() else {
        return;
    };
    if panel.command != "/mcp" {
        return;
    }

    if panel.title == "MCP Servers" {
        let Some(server) = mcp_index.servers.get_mut(panel.selected) else {
            return;
        };
        server.disabled = disabled;
        if let Some(config_server) = config
            .mcp_servers
            .iter_mut()
            .find(|config_server| config_server.name == server.name)
        {
            config_server.disabled = disabled;
            for tool in &mut server.tools {
                tool.enabled = !disabled && !config_server.disabled_tools.contains(&tool.name);
            }
        } else {
            for tool in &mut server.tools {
                tool.enabled = !disabled;
            }
        }
        let _ = Config::save_mcp_server_disabled(&server.name, disabled);
        update_banner_mcp_summary(transcript, &mcp_server_summary(config));
        panel.raw_rows = Some(mcp_panel_rows(mcp_index));
        return;
    }

    let Some(server_name) = panel.title.strip_prefix("MCP Server: ") else {
        return;
    };
    let Some(server) = mcp_index
        .servers
        .iter_mut()
        .find(|server| server.name == server_name)
    else {
        return;
    };
    if server.tools.is_empty() {
        return;
    }

    let mut sorted_tool_names = server
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    sorted_tool_names.sort();
    let Some(tool_name) = sorted_tool_names.get(panel.selected).cloned() else {
        return;
    };

    if let Some(config_server) = config
        .mcp_servers
        .iter_mut()
        .find(|config_server| config_server.name == server.name)
    {
        if disabled {
            if !config_server
                .disabled_tools
                .iter()
                .any(|tool| tool == &tool_name)
            {
                config_server.disabled_tools.push(tool_name.clone());
                config_server.disabled_tools.sort();
            }
        } else {
            config_server
                .disabled_tools
                .retain(|tool| tool != &tool_name);
        }
        let tool_disabled = config_server.disabled_tools.contains(&tool_name);
        if let Some(tool) = server.tools.iter_mut().find(|tool| tool.name == tool_name) {
            tool.enabled = !config_server.disabled && !tool_disabled;
        }
    } else if let Some(tool) = server.tools.iter_mut().find(|tool| tool.name == tool_name) {
        tool.enabled = !disabled;
    }

    let _ = Config::save_mcp_tool_disabled(&server.name, &tool_name, disabled);
    panel.raw_rows = Some(mcp_detail_rows(server));
}

async fn discover_mcp_index(config: &Config) -> McpIndex {
    let mut servers = Vec::new();
    for server in &config.mcp_servers {
        let mut tools = if server.transport == "stdio" {
            discover_stdio_mcp_tools(server).await.unwrap_or_default()
        } else {
            Vec::new()
        };
        for tool in &mut tools {
            tool.enabled = !server.disabled && !server.disabled_tools.contains(&tool.name);
        }
        servers.push(McpServerTools {
            name: server.name.clone(),
            transport: server.transport.clone(),
            disabled: server.disabled,
            tools,
        });
    }
    McpIndex { servers }
}

async fn discover_stdio_mcp_tools(server: &McpServerConfig) -> Result<Vec<McpToolInfo>> {
    let Some(argv) = mcp_argv(server) else {
        return Ok(Vec::new());
    };
    if argv.is_empty() {
        return Ok(Vec::new());
    }
    let timeout = Duration::from_millis(
        server
            .startup_timeout_sec
            .map(|seconds| (seconds * 1000.0).round().clamp(250.0, 10_000.0) as u64)
            .unwrap_or(1500),
    );
    tokio::time::timeout(timeout, discover_stdio_mcp_tools_inner(server, argv)).await?
}

async fn discover_stdio_mcp_tools_inner(
    server: &McpServerConfig,
    argv: Vec<String>,
) -> Result<Vec<McpToolInfo>> {
    let mut command = TokioCommand::new(&argv[0]);
    command.args(&argv[1..]);
    if let Some(cwd) = &server.cwd {
        command.current_dir(cwd);
    }
    command.envs(&server.env);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().expect("stdio mcp stdin is piped");
    let stdout = child.stdout.take().expect("stdio mcp stdout is piped");
    let mut stdout = BufReader::new(stdout).lines();

    write_mcp_message(
        &mut stdin,
        serde_json::json!({
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "mcp", "version": "0.1.0"}
            },
            "jsonrpc": "2.0",
            "id": 0
        }),
    )
    .await?;
    read_mcp_response(&mut stdout, 0).await?;
    write_mcp_message(
        &mut stdin,
        serde_json::json!({"method": "notifications/initialized", "jsonrpc": "2.0"}),
    )
    .await?;
    write_mcp_message(
        &mut stdin,
        serde_json::json!({"method": "tools/list", "jsonrpc": "2.0", "id": 1}),
    )
    .await?;
    let response = read_mcp_response(&mut stdout, 1).await?;
    let _ = child.kill().await;
    Ok(parse_mcp_tools(response))
}

fn mcp_argv(server: &McpServerConfig) -> Option<Vec<String>> {
    let mut argv = match server.command.as_ref()? {
        toml::Value::String(command) => shlex::split(command).unwrap_or_default(),
        toml::Value::Array(items) => items
            .iter()
            .filter_map(|item| item.as_str().map(ToString::to_string))
            .collect(),
        _ => Vec::new(),
    };
    argv.extend(server.args.iter().cloned());
    Some(argv)
}

async fn write_mcp_message(
    stdin: &mut tokio::process::ChildStdin,
    message: serde_json::Value,
) -> Result<()> {
    stdin.write_all(message.to_string().as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_mcp_response(
    stdout: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    expected_id: i64,
) -> Result<serde_json::Value> {
    while let Some(line) = stdout.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line)?;
        if value.get("id").and_then(serde_json::Value::as_i64) == Some(expected_id) {
            return Ok(value);
        }
    }
    Ok(serde_json::Value::Null)
}

fn parse_mcp_tools(response: serde_json::Value) -> Vec<McpToolInfo> {
    response
        .get("result")
        .and_then(|result| result.get("tools"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            let name = tool.get("name")?.as_str()?.to_string();
            let description = tool
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            Some(McpToolInfo {
                name,
                description,
                enabled: true,
            })
        })
        .collect()
}

fn current_proxy_values() -> Vec<String> {
    let env_path = vibe_home_dir().join(".env");
    let contents = fs::read_to_string(env_path).unwrap_or_default();
    PROXY_VARS
        .iter()
        .map(|(key, _)| dotenv_value(&contents, key).unwrap_or_default())
        .collect()
}

fn save_proxy_env(values: &[String]) -> Result<()> {
    let env_path = vibe_home_dir().join(".env");
    let existing = fs::read_to_string(&env_path).unwrap_or_default();
    let mut handled = vec![false; PROXY_VARS.len()];
    let mut lines = String::new();
    for line in existing.lines() {
        if let Some(proxy_index) = proxy_var_index(line) {
            handled[proxy_index] = true;
            let value = values
                .get(proxy_index)
                .map(|value| value.trim())
                .unwrap_or("");
            if !value.is_empty() {
                let key = PROXY_VARS[proxy_index].0;
                lines.push_str(&format!("{key}='{}'\n", dotenv_single_quote(value)));
            }
        } else {
            lines.push_str(line);
            lines.push('\n');
        }
    }
    for (idx, (key, _)) in PROXY_VARS.iter().enumerate() {
        if handled[idx] {
            continue;
        }
        let value = values.get(idx).map(|value| value.trim()).unwrap_or("");
        if !value.is_empty() {
            lines.push_str(&format!("{key}='{}'\n", dotenv_single_quote(value)));
        }
    }
    if lines.is_empty() && !env_path.exists() {
        return Ok(());
    }
    if let Some(parent) = env_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(env_path, lines)?;
    Ok(())
}

fn proxy_var_index(line: &str) -> Option<usize> {
    PROXY_VARS.iter().position(|(key, _)| {
        line.strip_prefix(key)
            .is_some_and(|rest| rest.starts_with('='))
    })
}

fn dotenv_value(contents: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    if let Some(line) = contents.lines().next() {
        let raw = line.strip_prefix(&prefix)?;
        if raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'') {
            return Some(raw[1..raw.len() - 1].replace("\\'", "'"));
        }
        return Some(raw.to_string());
    }
    None
}

fn dotenv_single_quote(value: &str) -> String {
    value.replace('\n', "").replace('\'', "\\'")
}

fn vibe_home_dir() -> PathBuf {
    if let Ok(vibe_home) = std::env::var("VIBE_HOME") {
        return PathBuf::from(vibe_home);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".vibe")
}

fn resume_panel(sessions: &[microvibe_core::SavedSession]) -> Option<BottomPanel> {
    sessions.first()?;
    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    Some(BottomPanel {
        command: "/resume".to_string(),
        title: format!("local {cwd}"),
        options: Vec::new(),
        selected: 0,
        help: "↑↓ Navigate  Enter Select  D Delete  Esc Cancel".to_string(),
        scroll_marker: None,
        raw_rows: None,
        toggled: false,
        auto_copy_on: false,
        resume_sessions: sessions.to_vec(),
        delete_confirm: None,
        rewind_message_index: None,
        question_call: None,
        question_index: 0,
        question_answers: Vec::new(),
        question_selected_options: Vec::new(),
        question_other_texts: Vec::new(),
        proxy_values: Vec::new(),
        mounted_at: Instant::now(),
    })
}

fn rewind_panel(messages: &[Message]) -> Option<BottomPanel> {
    let index = rewindable_message_entries(messages).last()?.0;
    rewind_panel_at(messages, index)
}

fn rewind_previous_panel(messages: &[Message], current: Option<usize>) -> Option<BottomPanel> {
    let rewindable = rewindable_message_entries(messages);
    let index = if let Some(current) = current {
        let position = rewindable
            .iter()
            .position(|(index, _)| *index == current)
            .unwrap_or(rewindable.len());
        rewindable
            .get(position.saturating_sub(1))
            .or_else(|| rewindable.first())?
            .0
    } else {
        rewindable.last()?.0
    };
    rewind_panel_at(messages, index)
}

fn rewind_next_panel(messages: &[Message], current: usize) -> Option<BottomPanel> {
    let rewindable = rewindable_message_entries(messages);
    let position = rewindable.iter().position(|(index, _)| *index == current)?;
    let index = rewindable
        .get(position + 1)
        .or_else(|| rewindable.get(position))?
        .0;
    rewind_panel_at(messages, index)
}

fn rewindable_message_entries(messages: &[Message]) -> Vec<(usize, String)> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role == Role::User)
        .map(|(index, message)| (index, text_content(message)))
        .filter(|(_, content)| !content.is_empty())
        .collect()
}

fn rewind_panel_at(messages: &[Message], index: usize) -> Option<BottomPanel> {
    let message = messages.get(index)?;
    if message.role != Role::User {
        return None;
    }
    let preview = text_content(message);
    if preview.is_empty() {
        return None;
    }
    let preview = preview.chars().take(80).collect::<String>();
    Some(BottomPanel {
        command: "/rewind".to_string(),
        title: format!("Rewind to: {preview}"),
        options: strings(&["Edit message from here"]),
        selected: 0,
        help: "⌥+↑↓ or Ctrl+P/N browse messages  ↑↓ pick option  Enter confirm  ESC cancel"
            .to_string(),
        scroll_marker: None,
        raw_rows: None,
        toggled: false,
        auto_copy_on: false,
        resume_sessions: Vec::new(),
        delete_confirm: None,
        rewind_message_index: Some(index),
        question_call: None,
        question_index: 0,
        question_answers: Vec::new(),
        question_selected_options: Vec::new(),
        question_other_texts: Vec::new(),
        proxy_values: Vec::new(),
        mounted_at: Instant::now(),
    })
}

fn apply_panel_selection(transcript: &mut Vec<String>, panel: &BottomPanel) {
    while transcript
        .last()
        .map(|line| line.is_empty())
        .unwrap_or(false)
    {
        transcript.pop();
    }
    match panel.command.as_str() {
        "/model" => {
            if let Some(model) = panel.options.get(panel.selected) {
                let _ = Config::save_active_model(model);
                let banner = match model.as_str() {
                    "devstral-small" => "devstral-small[off]",
                    "local" => "local[off]",
                    _ => "mistral-medium-3.5[high]",
                };
                update_banner_model(transcript, banner);
            }
            transcript.push(
                "  ⎣ Configuration reloaded (includes agent instructions and skills).".to_string(),
            );
            transcript.extend([String::new(), String::new()]);
        }
        "/thinking" => {
            if let Some(level) = panel.options.get(panel.selected) {
                let _ = Config::save_thinking(&level.to_ascii_lowercase());
                let banner = format!("mistral-medium-3.5[{}]", level.to_ascii_lowercase());
                update_banner_model(transcript, &banner);
            }
            transcript.push(
                "  ⎣ Configuration reloaded (includes agent instructions and skills).".to_string(),
            );
            transcript.extend([String::new(), String::new()]);
        }
        "/theme" => {
            if let Some(theme) = panel.options.get(panel.selected) {
                let _ = Config::save_theme(theme);
            }
            transcript.extend([String::new(), String::new()]);
        }
        "/proxy-setup" => {
            let _ = save_proxy_env(&panel.proxy_values);
            transcript.push(
                "  ⎣ Proxy settings saved. Restart the CLI for changes to take effect.".to_string(),
            );
            transcript.extend([String::new(), String::new()]);
        }
        _ => {
            transcript.extend([String::new(), String::new()]);
        }
    }
}

fn apply_panel_exit(transcript: &mut Vec<String>, panel: &BottomPanel) {
    match panel.command.as_str() {
        "/config" => {
            while transcript
                .last()
                .map(|line| line.is_empty())
                .unwrap_or(false)
            {
                transcript.pop();
            }
            let _ = Config::save_autocopy_to_clipboard(panel.auto_copy_on);
            transcript.push(
                "  ⎣ Configuration reloaded (includes agent instructions and skills).".to_string(),
            );
            transcript.extend([String::new(), String::new()]);
        }
        "/voice" => {
            while transcript
                .last()
                .map(|line| line.is_empty())
                .unwrap_or(false)
            {
                transcript.pop();
            }
            let _ = Config::save_voice_mode_enabled(panel.toggled);
            if panel.toggled {
                transcript
                    .push("  ⎣ Voice mode enabled. Press ctrl+r to start recording.".to_string());
            }
            transcript.extend([String::new(), String::new()]);
        }
        _ => {}
    }
}

fn update_banner_model(transcript: &mut [String], model: &str) {
    for line in transcript.iter_mut() {
        if line.starts_with("Mistral Vibe v2.17.1 · ") {
            *line = format!("Mistral Vibe v2.17.1 · {model}");
            break;
        }
    }
}

fn update_banner_mcp_summary(transcript: &mut [String], summary: &str) {
    for line in transcript.iter_mut() {
        if !line.contains(" · ") || !line.contains("MCP ") || !line.contains("skill") {
            continue;
        }
        let parts = line.split(" · ").collect::<Vec<_>>();
        if parts.len() == 3 {
            *line = format!("{} · {summary} · {}", parts[0], parts[2]);
            break;
        }
    }
}

fn panel_lines(
    panel: &BottomPanel,
    width: u16,
    cwd: String,
    status: &str,
    mode_label: &str,
) -> Vec<Line<'static>> {
    let width = width as usize;
    let inner = width.saturating_sub(2);
    let mut rows = Vec::new();
    rows.push(format!("┌{}┐", "─".repeat(inner)));
    rows.push(panel_row(&panel.title, inner));
    if panel.command == "/resume" {
        rows.push(panel_row("", inner));
        for (idx, session) in panel.resume_sessions.iter().enumerate() {
            let short = short_session_id(&session.session_id.0);
            let title = session.title.as_deref().unwrap_or("Untitled session");
            let body = if panel.delete_confirm.as_deref() == Some(&session.session_id.0) {
                "Press D again to delete".to_string()
            } else {
                title.to_string()
            };
            let row = format!(" just now    {short}  {body}");
            rows.push(panel_row(&row, inner));
            if idx + 1 >= 5 {
                break;
            }
        }
        rows.push(panel_row("", inner));
    } else if panel.command == "/rewind" {
        rows.push(panel_row("", inner));
        for (idx, option) in panel.options.iter().enumerate() {
            let marker = if idx == panel.selected { "›" } else { " " };
            rows.push(panel_row(&format!("{marker} {}. {option}", idx + 1), inner));
        }
        rows.push(panel_row("", inner));
    } else if panel.command == "/config" {
        let auto_copy = if panel.auto_copy_on { "On" } else { "Off" };
        for row in [
            "",
            " Model: mistral-medium-3.5",
            " Thinking: High",
            &format!(" Auto-copy: {auto_copy}"),
            " Autocomplete watcher (may delay first autocompletion): Off",
            "",
        ] {
            rows.push(panel_row(row, inner));
        }
    } else if panel.command == "/voice" {
        let voice = if panel.toggled { "On" } else { "Off" };
        for row in [
            "",
            &format!("› Voice mode: {voice}"),
            "  Narrator (experimental): Off",
            "",
        ] {
            rows.push(panel_row(row, inner));
        }
    } else if panel.command == "/question" {
        rows.push(panel_row("", inner));
        if panel.question_is_multi_select() {
            for (idx, option) in panel.options.iter().enumerate() {
                let marker = if idx == panel.selected { "›" } else { " " };
                let check = if panel.question_selected_options.contains(&idx) {
                    "[x]"
                } else {
                    "[ ]"
                };
                let option = if Some(idx) == panel.question_other_index()
                    && !panel.question_other_text().is_empty()
                {
                    panel.question_other_text()
                } else {
                    option.as_str()
                };
                rows.push(panel_row(
                    &format!("{marker} {}. {check} {option}", idx + 1),
                    inner,
                ));
            }
            rows.push(panel_row("", inner));
            let marker = if panel.selected == panel.question_submit_index() {
                "›"
            } else {
                " "
            };
            rows.push(panel_row(&format!("{marker}    Submit →"), inner));
        } else {
            for (idx, option) in panel.options.iter().enumerate() {
                let marker = if idx == panel.selected { "›" } else { " " };
                let option = if Some(idx) == panel.question_other_index()
                    && !panel.question_other_text().is_empty()
                {
                    panel.question_other_text()
                } else {
                    option.as_str()
                };
                rows.push(panel_row(&format!("{marker} {}. {option}", idx + 1), inner));
            }
        }
        rows.push(panel_row("", inner));
    } else if panel.command == "/proxy-setup" {
        for (idx, (key, description)) in PROXY_VARS.iter().enumerate() {
            rows.push(panel_row(key, inner));
            let value = panel
                .proxy_values
                .get(idx)
                .map(String::as_str)
                .unwrap_or("");
            let display = if value.is_empty() { description } else { value };
            rows.push(panel_row(&format!("▎ {display}"), inner));
        }
    } else if let Some(raw_rows) = panel.raw_rows.as_ref() {
        for row in raw_rows {
            rows.push(panel_row(row, inner));
        }
    } else {
        for (idx, option) in panel.options.iter().enumerate() {
            let marker = if idx == panel.selected { "›" } else { " " };
            let content = format!(" {marker} {option}");
            rows.push(if panel.scroll_marker == Some(idx) {
                panel_row_with_scroll_marker(&content, inner)
            } else {
                panel_row(&content, inner)
            });
        }
        rows.push(panel_row("", inner));
    }
    rows.push(panel_row(&panel.help, inner));
    let _ = mode_label;
    rows.push(format!("└{}┘", "─".repeat(inner)));
    let gap = width
        .saturating_sub(cwd.chars().count())
        .saturating_sub(status.len());
    rows.push(format!("{cwd}{}{status}", " ".repeat(gap)));
    let row_count = rows.len();
    rows.into_iter()
        .enumerate()
        .map(|(idx, row)| {
            let style = if idx == 0 || row.starts_with('└') {
                Style::default().fg(Color::DarkGray)
            } else if idx == 1 {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else if row.contains('›') {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if row.contains("Error") {
                Style::default().fg(Color::Red)
            } else if idx + 1 == row_count {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Gray)
            };
            Line::styled(row, style)
        })
        .collect()
}

fn panel_row(content: &str, inner: usize) -> String {
    let used = content.chars().count();
    let padding = inner.saturating_sub(used + 1);
    format!("│ {content}{}│", " ".repeat(padding))
}

fn panel_row_with_scroll_marker(content: &str, inner: usize) -> String {
    let used = content.chars().count();
    let padding = inner.saturating_sub(used + 4);
    format!("│ {content}{}▄  │", " ".repeat(padding))
}

fn clear_initializing(transcript: &mut [String]) {
    if let Some(last) = transcript.last_mut()
        && last.contains("Initializing")
    {
        last.clear();
    }
}

fn set_single_trailing_blank(transcript: &mut Vec<String>) {
    while transcript.last().is_some_and(String::is_empty) {
        transcript.pop();
    }
    transcript.push(String::new());
}

fn animated_transcript(transcript: &[String], frame_tick: usize) -> Vec<String> {
    let chat = petit_chat_lines(frame_tick);
    transcript
        .iter()
        .enumerate()
        .map(|(index, line)| {
            if (STARTUP_TOP_PADDING..STARTUP_TOP_PADDING + PETIT_CHAT_RENDERED_HEIGHT)
                .contains(&index)
                && line == PETIT_CHAT_INITIAL_LINES[index - STARTUP_TOP_PADDING]
            {
                return chat[index - STARTUP_TOP_PADDING].clone();
            }
            animate_spinner_line(line, frame_tick)
        })
        .collect()
}

const STARTUP_TOP_PADDING: usize = 20;
const PETIT_CHAT_WIDTH: usize = 22;
const PETIT_CHAT_HEIGHT: usize = 12;
const PETIT_CHAT_RENDERED_HEIGHT: usize = 3;
const PETIT_CHAT_TICKS_PER_TRANSITION: usize = 3;
const PETIT_CHAT_INITIAL_LINES: [&str; PETIT_CHAT_RENDERED_HEIGHT] =
    ["  ⡠⣒⠄  ⡔⢄⠔⡄", " ⢸⠸⣀⡔⢉⠱⣃⡢⣂⡣", "  ⠉⠒⠣⠤⠵⠤⠬⠮⠆"];

const PETIT_CHAT_STARTING_DOTS: &[&[usize]] = &[
    &[],
    &[6, 7, 15, 19],
    &[5, 8, 14, 16, 18, 20],
    &[4, 6, 7, 14, 17, 20],
    &[3, 5, 10, 11, 12, 14, 20],
    &[3, 5, 9, 13, 14, 16, 18, 20],
    &[3, 5, 8, 13, 17, 21],
    &[3, 6, 7, 8, 11, 14, 15, 16, 18, 19, 20],
    &[4, 5, 8, 12, 17, 19],
    &[6, 7, 8, 13, 18, 20],
    &[9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20],
    &[],
];

const EMPTY: &[(usize, usize)] = &[];
const BLINK_EYES_HEAD_HIGH_0_REMOVE: &[(usize, usize)] = &[(16, 5), (18, 5)];
const BLINK_EYES_HEAD_HIGH_0_ADD: &[(usize, usize)] = EMPTY;
const BLINK_EYES_HEAD_HIGH_1_REMOVE: &[(usize, usize)] = EMPTY;
const BLINK_EYES_HEAD_HIGH_1_ADD: &[(usize, usize)] = &[(16, 5), (18, 5)];
const QUEUE_RIGHT_TO_MID_REMOVE: &[(usize, usize)] = &[
    (6, 1),
    (7, 1),
    (8, 2),
    (4, 3),
    (6, 3),
    (7, 3),
    (4, 8),
    (5, 8),
];
const QUEUE_RIGHT_TO_MID_ADD: &[(usize, usize)] = &[
    (4, 1),
    (3, 2),
    (3, 3),
    (5, 3),
    (5, 7),
    (3, 8),
    (4, 9),
    (5, 9),
];
const QUEUE_MID_TO_LEFT_REMOVE: &[(usize, usize)] = &[
    (4, 1),
    (5, 2),
    (3, 3),
    (5, 3),
    (5, 7),
    (3, 8),
    (4, 9),
    (5, 9),
];
const QUEUE_MID_TO_LEFT_ADD: &[(usize, usize)] = &[
    (1, 1),
    (2, 1),
    (0, 2),
    (1, 3),
    (2, 3),
    (4, 3),
    (4, 8),
    (5, 8),
];
const HEAD_RIGHT_REMOVE: &[(usize, usize)] = &[(16, 5), (18, 5), (17, 6)];
const HEAD_RIGHT_ADD: &[(usize, usize)] = &[(17, 5), (19, 5), (18, 6)];
const HEAD_DOWN_REMOVE: &[(usize, usize)] = &[
    (15, 1),
    (19, 1),
    (14, 2),
    (16, 2),
    (18, 2),
    (20, 2),
    (17, 3),
    (17, 5),
    (19, 5),
    (13, 6),
    (18, 6),
    (21, 6),
    (14, 7),
    (15, 7),
    (16, 7),
    (19, 7),
    (20, 7),
];
const HEAD_DOWN_ADD: &[(usize, usize)] = &[
    (15, 2),
    (19, 2),
    (16, 3),
    (18, 3),
    (17, 4),
    (14, 6),
    (17, 6),
    (19, 6),
    (20, 6),
    (13, 7),
    (18, 7),
    (21, 7),
    (14, 8),
    (15, 8),
    (16, 8),
    (18, 8),
    (20, 8),
];
const BLINK_EYES_HEAD_LOW_0_REMOVE: &[(usize, usize)] = &[(17, 6), (19, 6)];
const BLINK_EYES_HEAD_LOW_0_ADD: &[(usize, usize)] = EMPTY;
const BLINK_EYES_HEAD_LOW_1_REMOVE: &[(usize, usize)] = EMPTY;
const BLINK_EYES_HEAD_LOW_1_ADD: &[(usize, usize)] = &[(17, 6), (19, 6)];
const HEAD_UP_REMOVE: &[(usize, usize)] = HEAD_DOWN_ADD;
const HEAD_UP_ADD: &[(usize, usize)] = &[
    (15, 1),
    (19, 1),
    (14, 2),
    (16, 2),
    (18, 2),
    (20, 2),
    (17, 3),
    (17, 5),
    (19, 5),
    (13, 6),
    (18, 6),
    (21, 6),
    (14, 7),
    (15, 7),
    (16, 7),
    (18, 7),
    (19, 7),
    (20, 7),
];
const HEAD_LEFT_REMOVE: &[(usize, usize)] = &[(17, 5), (19, 5), (18, 6)];
const HEAD_LEFT_ADD: &[(usize, usize)] = &[(16, 5), (18, 5), (17, 6)];

#[derive(Clone, Copy)]
struct PetitChatTransition {
    remove: &'static [(usize, usize)],
    add: &'static [(usize, usize)],
}

const fn petit_chat_transition(
    remove: &'static [(usize, usize)],
    add: &'static [(usize, usize)],
) -> PetitChatTransition {
    PetitChatTransition { remove, add }
}

const PETIT_CHAT_TRANSITIONS: &[PetitChatTransition] = &[
    petit_chat_transition(BLINK_EYES_HEAD_HIGH_0_REMOVE, BLINK_EYES_HEAD_HIGH_0_ADD),
    petit_chat_transition(BLINK_EYES_HEAD_HIGH_1_REMOVE, BLINK_EYES_HEAD_HIGH_1_ADD),
    petit_chat_transition(EMPTY, EMPTY),
    petit_chat_transition(QUEUE_RIGHT_TO_MID_REMOVE, QUEUE_RIGHT_TO_MID_ADD),
    petit_chat_transition(HEAD_RIGHT_REMOVE, HEAD_RIGHT_ADD),
    petit_chat_transition(EMPTY, EMPTY),
    petit_chat_transition(QUEUE_MID_TO_LEFT_REMOVE, QUEUE_MID_TO_LEFT_ADD),
    petit_chat_transition(EMPTY, EMPTY),
    petit_chat_transition(QUEUE_MID_TO_LEFT_ADD, QUEUE_MID_TO_LEFT_REMOVE),
    petit_chat_transition(EMPTY, EMPTY),
    petit_chat_transition(HEAD_DOWN_REMOVE, HEAD_DOWN_ADD),
    petit_chat_transition(EMPTY, EMPTY),
    petit_chat_transition(QUEUE_RIGHT_TO_MID_ADD, QUEUE_RIGHT_TO_MID_REMOVE),
    petit_chat_transition(BLINK_EYES_HEAD_LOW_0_REMOVE, BLINK_EYES_HEAD_LOW_0_ADD),
    petit_chat_transition(BLINK_EYES_HEAD_LOW_1_REMOVE, BLINK_EYES_HEAD_LOW_1_ADD),
    petit_chat_transition(EMPTY, EMPTY),
    petit_chat_transition(QUEUE_RIGHT_TO_MID_REMOVE, QUEUE_RIGHT_TO_MID_ADD),
    petit_chat_transition(EMPTY, EMPTY),
    petit_chat_transition(QUEUE_MID_TO_LEFT_REMOVE, QUEUE_MID_TO_LEFT_ADD),
    petit_chat_transition(EMPTY, EMPTY),
    petit_chat_transition(HEAD_UP_REMOVE, HEAD_UP_ADD),
    petit_chat_transition(EMPTY, EMPTY),
    petit_chat_transition(QUEUE_MID_TO_LEFT_ADD, QUEUE_MID_TO_LEFT_REMOVE),
    petit_chat_transition(HEAD_LEFT_REMOVE, HEAD_LEFT_ADD),
    petit_chat_transition(EMPTY, EMPTY),
    petit_chat_transition(QUEUE_RIGHT_TO_MID_ADD, QUEUE_RIGHT_TO_MID_REMOVE),
];

fn petit_chat_lines(frame_tick: usize) -> Vec<String> {
    let mut dots: HashSet<(usize, usize)> = PETIT_CHAT_STARTING_DOTS
        .iter()
        .enumerate()
        .flat_map(|(y, row)| row.iter().map(move |x| (*x, y)))
        .collect();
    let transition_count = frame_tick / PETIT_CHAT_TICKS_PER_TRANSITION;
    for transition in PETIT_CHAT_TRANSITIONS
        .iter()
        .cycle()
        .take(transition_count % PETIT_CHAT_TRANSITIONS.len())
    {
        for coord in transition.remove {
            dots.remove(coord);
        }
        for coord in transition.add {
            dots.insert(*coord);
        }
    }
    render_braille(&dots, PETIT_CHAT_WIDTH, PETIT_CHAT_HEIGHT)
}

fn render_braille(dots: &HashSet<(usize, usize)>, width: usize, height: usize) -> Vec<String> {
    let rendered_width = width.div_ceil(2);
    let rendered_height = height.div_ceil(4);
    let mut lines = Vec::with_capacity(rendered_height);
    for cell_y in 0..rendered_height {
        let mut line = String::with_capacity(rendered_width);
        for cell_x in 0..rendered_width {
            let mut mask = 0u32;
            for sub_y in 0..4 {
                for sub_x in 0..2 {
                    let x = cell_x * 2 + sub_x;
                    let y = cell_y * 4 + sub_y;
                    if dots.contains(&(x, y)) {
                        let dot_index = braille_dot_index(sub_x, sub_y);
                        mask += 1 << (dot_index - 1);
                    }
                }
            }
            line.push(char::from_u32(0x2800 + mask).unwrap_or(' '));
        }
        lines.push(line);
    }
    lines
}

fn braille_dot_index(x: usize, y: usize) -> u32 {
    if y < 3 {
        (y + 1 + 3 * x) as u32
    } else {
        (7 + x) as u32
    }
}

fn animate_spinner_line(line: &str, frame_tick: usize) -> String {
    if !is_pending_spinner_line(line) {
        return line.to_string();
    }
    let frame = snake_spinner_frame(frame_tick);
    let rest = line
        .char_indices()
        .nth(1)
        .map(|(idx, _)| &line[idx..])
        .unwrap_or("");
    format!("{frame}{rest}")
}

fn is_pending_spinner_line(line: &str) -> bool {
    line.contains("Running command…")
        || line.contains("Writing file…")
        || line.contains("Editing files…")
        || line.contains("Fetching URL…")
        || line.contains("Searching the web…")
        || line.contains("Running subagent…")
        || line.contains("Waiting for user input…")
        || line.contains("Waiting for user confirmation…")
}

fn snake_spinner_frame(frame_tick: usize) -> &'static str {
    const FRAMES: [&str; 32] = [
        "⠉⠁", "⠈⠁", "⠈⠉", "⠈⠙", "⠙ ", "⠸ ", "⢰ ", "⣰ ", "⣠ ", "⢀⣠", "⢀⣀", "⣀⣀", "⣀⡀", "⡀ ", "⣄⡀",
        "⣄ ", "⡆ ", "⠇ ", "⠏ ", "⠋ ", "⠉ ", "⠉⠃", "⠈⠃", " ⠃", " ⠇", "⠠⠇", "⠠⠆", "⠤⠆", "⠤⠄", "⠤⠤",
        "⠄⠤", "⠄ ",
    ];
    FRAMES[frame_tick % FRAMES.len()]
}

fn resumed_session_lines(messages: &[Message]) -> Vec<String> {
    let mut lines = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
        match message.role {
            Role::User => {
                lines.extend(format_user_prompt_lines_with_images(
                    &text_content(message),
                    message.images.as_deref().unwrap_or(&[]),
                ));
                lines.push("─".repeat(120));
                lines.push(String::new());
            }
            Role::Assistant => {
                let text = text_content(message);
                if !text.is_empty() {
                    lines.push(format!("  {text}"));
                    if messages.get(message_index + 1).is_some() {
                        lines.extend([String::new(), String::new()]);
                    }
                }
            }
            Role::Tool => {
                if let Some(result) = message.content.iter().find_map(|block| match block {
                    ContentBlock::ToolResult(result) => Some(result),
                    _ => None,
                }) {
                    lines.push(format!("tool result: {}", result.output));
                }
            }
            Role::System => {}
        }
    }
    lines
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

fn render_path_mentions_for_model_with_options(input: &str, skip_images: bool) -> String {
    if !input.contains('@') {
        return input.to_string();
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut rendered = String::new();
    let mut resources = Vec::new();
    let mut pos = 0;
    while pos < input.len() {
        let ch = input[pos..].chars().next().expect("pos is char boundary");
        if ch == '@'
            && is_path_anchor(input, pos)
            && let Some((candidate, end)) = extract_path_candidate(input, pos + ch.len_utf8())
            && let Some(resource) = path_resource(&cwd, &candidate)
        {
            rendered.push_str(&candidate);
            resources.push(resource);
            pos = end;
            continue;
        }
        rendered.push(ch);
        pos += ch.len_utf8();
    }

    let mut seen = HashSet::new();
    for resource in resources {
        if !seen.insert(resource.path.clone()) {
            continue;
        }
        match resource.kind {
            PathResourceKind::File => {
                if let Some(block) = render_text_resource(&resource) {
                    rendered.push_str("\n\n");
                    rendered.push_str(&block);
                } else {
                    rendered.push_str("\n\n");
                    rendered.push_str(&render_resource_link(&resource));
                }
            }
            PathResourceKind::Folder => {
                rendered.push_str("\n\n");
                rendered.push_str(&render_resource_link(&resource));
            }
            PathResourceKind::Image => {
                if !skip_images {
                    rendered.push_str("\n\n");
                    rendered.push_str(&render_resource_link(&resource));
                }
            }
        }
    }
    rendered
}

fn is_path_anchor(input: &str, pos: usize) -> bool {
    if pos == 0 {
        return true;
    }
    input[..pos]
        .chars()
        .next_back()
        .is_none_or(|ch| !(ch.is_alphanumeric() || ch == '_'))
}

fn extract_path_candidate(input: &str, start: usize) -> Option<(String, usize)> {
    if start >= input.len() {
        return None;
    }
    let first = input[start..].chars().next()?;
    if matches!(first, '\'' | '"') {
        let quote_len = first.len_utf8();
        let mut end = start + quote_len;
        while end < input.len() {
            let ch = input[end..].chars().next()?;
            if ch == first {
                return Some((
                    input[start + quote_len..end].to_string(),
                    end + ch.len_utf8(),
                ));
            }
            end += ch.len_utf8();
        }
        return None;
    }

    let mut end = start;
    while end < input.len() {
        let ch = input[end..].chars().next()?;
        if !(ch.is_alphanumeric() || "._/\\-()[]{}~".contains(ch)) {
            break;
        }
        end += ch.len_utf8();
    }
    (end > start).then(|| (input[start..end].to_string(), end))
}

#[derive(Clone)]
struct PathResource {
    path: PathBuf,
    alias: String,
    kind: PathResourceKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PathResourceKind {
    File,
    Folder,
    Image,
}

fn path_resource(cwd: &Path, candidate: &str) -> Option<PathResource> {
    if candidate.is_empty() {
        return None;
    }
    let expanded = if let Some(rest) = candidate.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(rest))?
    } else {
        PathBuf::from(candidate)
    };
    let raw = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    let path = raw.canonicalize().ok()?;
    let kind = if path.is_dir() {
        PathResourceKind::Folder
    } else if is_image_path(&path) {
        PathResourceKind::Image
    } else {
        PathResourceKind::File
    };
    Some(PathResource {
        path,
        alias: candidate.to_string(),
        kind,
    })
}

fn is_image_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp")
    )
}

fn render_text_resource(resource: &PathResource) -> Option<String> {
    const MAX_EMBED_BYTES: u64 = 256 * 1024;
    let metadata = fs::metadata(&resource.path).ok()?;
    if metadata.len() > MAX_EMBED_BYTES {
        return None;
    }
    let data = fs::read(&resource.path).ok()?;
    if !is_probably_text(&resource.path, &data) {
        return None;
    }
    let text = String::from_utf8_lossy(&data);
    Some(format!("{}\n```\n{text}\n```", file_uri(&resource.path)))
}

fn is_probably_text(path: &Path, data: &[u8]) -> bool {
    if data.is_empty() {
        return true;
    }
    if data.contains(&0) || is_image_path(path) {
        return false;
    }
    let non_text = data
        .iter()
        .filter(|byte| ((**byte <= 31) && !matches!(**byte, 9..=12)) || **byte == 127)
        .count();
    (non_text as f64 / data.len() as f64) < 0.1
}

fn render_resource_link(resource: &PathResource) -> String {
    format!(
        "uri: {}\nname: {}",
        file_uri(&resource.path),
        resource.alias
    )
}

fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

struct TurnPayload {
    model_input: String,
    images: Vec<ImageAttachment>,
}

fn build_turn_payload(
    input: &str,
    config: &Config,
    session_dir: &Path,
) -> std::result::Result<TurnPayload, Vec<String>> {
    let images = prepare_image_attachments(input, config, session_dir)?;
    Ok(TurnPayload {
        model_input: render_path_mentions_for_model_with_options(input, true),
        images,
    })
}

fn prepare_image_attachments(
    input: &str,
    config: &Config,
    session_dir: &Path,
) -> std::result::Result<Vec<ImageAttachment>, Vec<String>> {
    const MAX_IMAGES_PER_MESSAGE: usize = 8;
    const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

    let resources = image_resources(input);
    if resources.is_empty() {
        return Ok(Vec::new());
    }
    if resources.len() > MAX_IMAGES_PER_MESSAGE {
        return Err(vec![format!(
            "Too many image attachments (got {}, max {MAX_IMAGES_PER_MESSAGE}).",
            resources.len()
        )]);
    }
    if !active_model_supports_images(config) {
        let alias = config
            .active_model
            .as_deref()
            .unwrap_or("mistral-medium-3.5");
        return Err(vec![
            format!(
                "Model `{alias}` does not support images. Switch with /model, remove the attachment, or ask me to enable the support for",
            ),
            "this model.".to_string(),
        ]);
    }

    let mut attachments = Vec::new();
    for resource in resources {
        let metadata = fs::metadata(&resource.path)
            .map_err(|error| vec![format!("Cannot read image {}: {error}", resource.alias)])?;
        if metadata.len() > MAX_IMAGE_BYTES {
            return Err(vec![format!(
                "Image `{}` is {:.1} MB; max is {} MB.",
                resource.alias,
                metadata.len() as f64 / (1024.0 * 1024.0),
                MAX_IMAGE_BYTES / (1024 * 1024)
            )]);
        }
        let data = fs::read(&resource.path).map_err(|error| {
            vec![format!(
                "Failed to attach image {}: {error}",
                resource.alias
            )]
        })?;
        let ext = resource
            .path
            .extension()
            .and_then(OsStr::to_str)
            .map(|ext| format!(".{}", ext.to_ascii_lowercase()))
            .unwrap_or_default();
        let digest = {
            let mut hasher = sha1::Sha1::new();
            hasher.update(&data);
            format!("{:x}", hasher.finalize())
        };
        let attachments_dir = session_dir.join("attachments");
        fs::create_dir_all(&attachments_dir).map_err(|error| {
            vec![format!(
                "Failed to attach image {}: {error}",
                resource.alias
            )]
        })?;
        let dest = attachments_dir.join(format!("{digest}{ext}"));
        if !dest.exists() {
            fs::write(&dest, &data).map_err(|error| {
                vec![format!(
                    "Failed to attach image {}: {error}",
                    resource.alias
                )]
            })?;
        }
        attachments.push(ImageAttachment {
            source: ImageSource::File {
                path: dest.canonicalize().unwrap_or(dest),
            },
            alias: resource.alias,
            mime_type: image_mime_type(&resource.path),
        });
    }
    Ok(attachments)
}

fn image_resources(input: &str) -> Vec<PathResource> {
    if !input.contains('@') {
        return Vec::new();
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut resources = Vec::new();
    let mut seen = HashSet::new();
    let mut pos = 0;
    while pos < input.len() {
        let ch = input[pos..].chars().next().expect("pos is char boundary");
        if ch == '@'
            && is_path_anchor(input, pos)
            && let Some((candidate, end)) = extract_path_candidate(input, pos + ch.len_utf8())
            && let Some(resource) = path_resource(&cwd, &candidate)
        {
            if resource.kind == PathResourceKind::Image && seen.insert(resource.path.clone()) {
                resources.push(resource);
            }
            pos = end;
            continue;
        }
        pos += ch.len_utf8();
    }
    resources
}

fn active_model_supports_images(config: &Config) -> bool {
    let active = config
        .active_model
        .as_deref()
        .unwrap_or("mistral-medium-3.5");
    config
        .models
        .iter()
        .find(|model| model.alias == active || model.name == config.model.name)
        .map(|model| model.supports_images)
        .unwrap_or_else(|| config.model.name.starts_with("mistral-medium-3.5"))
}

fn image_mime_type(path: &Path) -> String {
    match path
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn short_session_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

fn static_command_lines(command: &str, config: &Config) -> Option<Vec<String>> {
    let lines = match command {
        "/status" => status_lines(),
        "/data-retention" => data_retention_lines(),
        "/mcp" | "/connectors" => command_message_lines(
            command,
            &["  ⎣ No MCP servers or connectors configured."],
            2,
        ),
        "/mcp status" | "/connectors status" => mcp_status_lines(command, config),
        "/mcp login" | "/connectors login" => {
            command_message_lines(command, &["  ⎣ Error: Usage: /mcp login <alias>"], 2)
        }
        "/mcp logout" | "/connectors logout" => {
            command_message_lines(command, &["  ⎣ Error: Usage: /mcp logout <alias>"], 2)
        }
        "/resume" | "/continue" => {
            command_message_lines(command, &["  ⎣ No sessions found for this directory."], 2)
        }
        "/compact" => command_message_lines(
            command,
            &["  ⎣ Error: No conversation history to compact yet."],
            2,
        ),
        "/clear" => command_message_lines(command, &["  ⎣ Conversation history cleared!"], 2),
        "/reload" | "/leanstall" => command_message_lines(
            command,
            &["  ⎣ Configuration reloaded (includes agent instructions and skills)."],
            2,
        ),
        "/unleanstall" => command_message_lines(command, &["  ⎣ Lean agent is not installed."], 2),
        "/rewind" => command_message_lines(command, &[], 2),
        "/teleport" => command_message_lines(
            command,
            &[
                "  ⎢ Error: Teleport requires a Vibe Pro subscription. Your current API key isn't eligible. Upgrade to Vibe Pro:",
                "  ⎣ https://chat.mistral.ai/code/extensions?focus=key",
            ],
            2,
        ),
        "/rename" => command_message_lines(command, &["  ⎣ Error: Usage: /rename <title>"], 2),
        _ => return None,
    };
    Some(lines)
}

fn mcp_status_lines(command: &str, config: &Config) -> Vec<String> {
    if config.mcp_servers.is_empty() {
        return command_message_lines(command, &["  ⎣ No MCP servers configured."], 2);
    }
    let mut body = vec!["  ⎢ MCP auth status".to_string()];
    for server in &config.mcp_servers {
        body.push(format!("  ⎣ • {}: {}", server.name, server.transport));
    }
    command_message_lines_owned(command, body, 2)
}

fn debug_console_width(width: u16) -> u16 {
    ((width as usize * 40 / 100).clamp(40, 80) as u16).min(width.saturating_sub(1))
}

fn debug_console_text(height: u16) -> String {
    let mut lines = Vec::with_capacity(height as usize);
    lines.push("│ Debug Console  (ctrl+\\ to close)".to_string());
    for _ in 1..height {
        lines.push("│".to_string());
    }
    lines.join("\n")
}

fn transcript_display_rows(transcript: &[String], frame_tick: usize) -> Vec<String> {
    animated_transcript(transcript, frame_tick)
        .into_iter()
        .flat_map(|line| line.split('\n').map(ToOwned::to_owned).collect::<Vec<_>>())
        .collect()
}

fn external_editor_command() -> String {
    std::env::var("VISUAL")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| "nano".to_string())
}

fn open_external_editor(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    initial_content: &str,
) -> io::Result<Option<String>> {
    let mut file = tempfile::Builder::new()
        .prefix("vibe_")
        .suffix(".md")
        .tempfile()?;
    file.write_all(initial_content.as_bytes())?;
    file.flush()?;
    let path = file.path().to_path_buf();
    if !open_file_in_external_editor(terminal, &path)? {
        return Ok(None);
    }
    let content = fs::read_to_string(path)?.trim_end().to_string();
    if content == initial_content {
        Ok(None)
    } else {
        Ok(Some(content))
    }
}

fn open_file_in_external_editor(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    path: &PathBuf,
) -> io::Result<bool> {
    let editor = external_editor_command();
    let parts = shlex::split(&editor).unwrap_or_else(|| vec![editor]);
    let Some((program, args)) = parts.split_first() else {
        return Ok(false);
    };

    terminal::disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    let status = Command::new(program).args(args).arg(path).status();
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        PushKeyboardEnhancementFlags(keyboard_enhancement_flags())
    )?;
    terminal::enable_raw_mode()?;

    Ok(status.map(|status| status.success()).unwrap_or(false))
}

fn exit_plan_panel_file_path(panel: &BottomPanel) -> Option<PathBuf> {
    if panel.command != "/question" {
        return None;
    }
    let call = panel.question_call.as_ref()?;
    if call.name != "exit_plan_mode" {
        return None;
    }
    let footer = call.arguments.get("footer_note")?.as_str()?;
    let path = footer
        .strip_prefix("Plan: ")?
        .strip_suffix(" (Ctrl+G to edit)")?
        .trim();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

fn copy_text_to_clipboard(text: &str) -> io::Result<bool> {
    if text.is_empty() {
        return Ok(false);
    }

    let mut any_strategy_succeeded = false;
    for strategy in copy_strategies() {
        if copy_with_strategy(strategy, text).is_err() {
            continue;
        }
        any_strategy_succeeded = true;
        if read_clipboard().as_deref() == Some(text) {
            return Ok(true);
        }
    }

    if any_strategy_succeeded {
        Ok(true)
    } else {
        Err(io::Error::other("All clipboard strategies failed"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CopyStrategy {
    Osc52,
    Pbcopy,
    Xclip,
    WlCopy,
}

fn copy_strategies() -> Vec<CopyStrategy> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut strategies = vec![CopyStrategy::Osc52];
    for (command, strategy) in [
        ("pbcopy", CopyStrategy::Pbcopy),
        ("xclip", CopyStrategy::Xclip),
        ("wl-copy", CopyStrategy::WlCopy),
    ] {
        if command_exists_in_path(command, &path) {
            strategies.push(strategy);
        }
    }
    strategies
}

fn copy_with_strategy(strategy: CopyStrategy, text: &str) -> io::Result<()> {
    match strategy {
        CopyStrategy::Osc52 => copy_osc52(text),
        CopyStrategy::Pbcopy => run_clipboard_writer("pbcopy", &[], text),
        CopyStrategy::Xclip => run_clipboard_writer("xclip", &["-selection", "clipboard"], text),
        CopyStrategy::WlCopy => run_clipboard_writer("wl-copy", &[], text),
    }
}

fn copy_osc52(text: &str) -> io::Result<()> {
    let mut tty = fs::OpenOptions::new().write(true).open("/dev/tty")?;
    tty.write_all(osc52_sequence(text).as_bytes())?;
    tty.flush()
}

fn osc52_sequence(text: &str) -> String {
    osc52_sequence_with_tmux(text, std::env::var_os("TMUX").is_some())
}

fn osc52_sequence_with_tmux(text: &str, in_tmux: bool) -> String {
    let encoded = BASE64.encode(text.as_bytes());
    let sequence = format!("\x1b]52;c;{encoded}\x07");
    if in_tmux {
        format!("\x1bPtmux;\x1b{sequence}\x1b\\")
    } else {
        sequence
    }
}

fn run_clipboard_writer(command: &str, args: &[&str], text: &str) -> io::Result<()> {
    let mut child = Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(text.as_bytes())?;
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{command} failed")))
    }
}

fn read_clipboard() -> Option<String> {
    for reader in [
        ClipboardReader::Pbpaste,
        ClipboardReader::Xclip,
        ClipboardReader::WlPaste,
    ] {
        if let Ok(text) = read_clipboard_with(reader) {
            return Some(text);
        }
    }
    None
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClipboardReader {
    Pbpaste,
    Xclip,
    WlPaste,
}

fn read_clipboard_with(reader: ClipboardReader) -> io::Result<String> {
    let (command, args): (&str, &[&str]) = match reader {
        ClipboardReader::Pbpaste => ("pbpaste", &[]),
        ClipboardReader::Xclip => ("xclip", &["-selection", "clipboard", "-o"]),
        ClipboardReader::WlPaste => ("wl-paste", &[]),
    };
    let output = Command::new(command).args(args).output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(io::Error::other(format!("{command} failed")))
    }
}

fn command_exists_in_path(command: &str, path: &OsStr) -> bool {
    std::env::split_paths(path).any(|dir| is_executable_file(&dir.join(command)))
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn command_message_lines(command: &str, body: &[&str], trailing_blanks: usize) -> Vec<String> {
    let mut lines = vec![slash_command_line(command)];
    lines.extend(body.iter().map(|line| (*line).to_string()));
    lines.extend(std::iter::repeat_n(String::new(), trailing_blanks));
    lines
}

fn command_message_lines_owned(
    command: &str,
    body: impl IntoIterator<Item = String>,
    trailing_blanks: usize,
) -> Vec<String> {
    let mut lines = vec![slash_command_line(command)];
    lines.extend(body);
    lines.extend(std::iter::repeat_n(String::new(), trailing_blanks));
    lines
}

#[derive(Debug, Clone)]
struct ScheduledLoop {
    id: String,
    interval_seconds: u64,
    prompt: String,
    next_fire_at: Instant,
}

fn loop_command_lines(command: &str, loops: &mut Vec<ScheduledLoop>) -> Vec<String> {
    let args = command.strip_prefix("/loop").unwrap_or("").trim();
    if args.is_empty() || matches!(args, "list" | "ls") {
        return loop_list_lines(command, loops);
    }

    let (verb, rest) = args.split_once(' ').unwrap_or((args, ""));
    if matches!(verb, "cancel" | "rm" | "stop" | "delete") {
        return loop_cancel_lines(command, loops, rest.trim());
    }

    match add_scheduled_loop(loops, verb, rest) {
        Ok(loop_) => command_message_lines_owned(
            command,
            [format!(
                "  ⎣ Scheduled loop {} every {}: {}",
                loop_.id,
                format_duration(loop_.interval_seconds, false),
                loop_.prompt
            )],
            2,
        ),
        Err(message) => loop_error_lines(command, &message),
    }
}

fn add_scheduled_loop(
    loops: &mut Vec<ScheduledLoop>,
    interval_text: &str,
    prompt: &str,
) -> Result<ScheduledLoop, String> {
    let interval_seconds = parse_loop_interval(interval_text)?;
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err("Missing prompt.".to_string());
    }
    if prompt.starts_with('/') {
        return Err("Prompt cannot start with '/'.".to_string());
    }
    if loops.len() >= 50 {
        return Err("Loop limit reached (50 per session).".to_string());
    }

    let loop_ = ScheduledLoop {
        id: uuid::Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(8)
            .collect(),
        interval_seconds,
        prompt: prompt.to_string(),
        next_fire_at: Instant::now() + Duration::from_secs(interval_seconds),
    };
    loops.push(loop_.clone());
    Ok(loop_)
}

fn parse_loop_interval(text: &str) -> Result<u64, String> {
    if text.is_empty() {
        return Err("Missing interval.".to_string());
    }
    let text = text.trim().to_ascii_lowercase();
    let Some(unit) = text.chars().last() else {
        return Err("Missing interval.".to_string());
    };
    let number_text = &text[..text.len().saturating_sub(unit.len_utf8())];
    if number_text.is_empty() || !number_text.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(format!(
            "Invalid interval `{text}`. Expected: <number><unit> (e.g., 30s, 5m, 2h, 1d)."
        ));
    }
    let value = number_text.parse::<u64>().unwrap_or(0);
    let unit_seconds = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        _ => {
            return Err(format!(
                "Invalid interval `{text}`. Expected: <number><unit> (e.g., 30s, 5m, 2h, 1d)."
            ));
        }
    };
    let seconds = value.saturating_mul(unit_seconds);
    if seconds < 30 {
        return Err("Interval must be at least 30s.".to_string());
    }
    Ok(seconds)
}

fn loop_list_lines(command: &str, loops: &[ScheduledLoop]) -> Vec<String> {
    if loops.is_empty() {
        return command_message_lines(command, &["  ⎣ No scheduled loops."], 2);
    }

    let now = Instant::now();
    let mut body = vec![
        format!(
            "  ⎢ ┌{}┬{}┬{}┬{}┐",
            "─".repeat(38),
            "─".repeat(24),
            "─".repeat(19),
            "─".repeat(28)
        ),
        format!(
            "  ⎢ │{}│{}│{}│{}│",
            table_cell("Prompt", 38),
            table_cell("Next in", 24),
            table_cell("Every", 19),
            table_cell("ID", 28)
        ),
        format!(
            "  ⎢ ├{}┼{}┼{}┼{}┤",
            "─".repeat(38),
            "─".repeat(24),
            "─".repeat(19),
            "─".repeat(28)
        ),
    ];
    for loop_ in loops {
        let remaining = loop_
            .next_fire_at
            .checked_duration_since(now)
            .unwrap_or_default()
            .as_secs();
        let prompt = loop_.prompt.replace('|', "\\|").replace('\n', " ");
        body.push(format!(
            "  ⎢ │{}│{}│{}│{}│",
            table_cell(&prompt, 38),
            table_cell(&format_duration(remaining, true), 24),
            table_cell(&format_duration(loop_.interval_seconds, false), 19),
            table_cell(&loop_.id, 28)
        ));
    }
    body.push(format!(
        "  ⎣ └{}┴{}┴{}┴{}┘",
        "─".repeat(38),
        "─".repeat(24),
        "─".repeat(19),
        "─".repeat(28)
    ));
    command_message_lines_owned(command, body, 2)
}

fn table_cell(value: &str, width: usize) -> String {
    let clipped = value
        .chars()
        .take(width.saturating_sub(1))
        .collect::<String>();
    format!(" {clipped:<pad$}", pad = width - 1)
}

fn loop_cancel_lines(command: &str, loops: &mut Vec<ScheduledLoop>, target: &str) -> Vec<String> {
    if target.is_empty() {
        return loop_error_lines(command, "Missing loop id.");
    }
    if target.eq_ignore_ascii_case("all") {
        let count = loops.len();
        loops.clear();
        return command_message_lines_owned(
            command,
            [format!("  ⎣ Cancelled {count} scheduled loop(s).")],
            2,
        );
    }
    let Some(index) = loops.iter().position(|loop_| loop_.id == target) else {
        return loop_error_lines(command, &format!("No scheduled loop with id `{target}`."));
    };
    let loop_ = loops.remove(index);
    command_message_lines_owned(
        command,
        [format!("  ⎣ Cancelled loop {}: {}", loop_.id, loop_.prompt)],
        2,
    )
}

fn loop_error_lines(command: &str, message: &str) -> Vec<String> {
    command_message_lines_owned(
        command,
        [
            format!("  ⎢ Error: {message}"),
            "  ⎢ Usage:".to_string(),
            "  ⎢   /loop <interval> <prompt>".to_string(),
            "  ⎢   /loop list".to_string(),
            "  ⎢   /loop cancel <id|all>".to_string(),
            "  ⎣".to_string(),
        ],
        2,
    )
}

fn format_duration(mut seconds: u64, short: bool) -> String {
    let units = [('d', 86400), ('h', 3600), ('m', 60), ('s', 1)];
    let mut parts = Vec::new();
    for (suffix, unit_seconds) in units {
        let value = seconds / unit_seconds;
        if value > 0 {
            parts.push(format!("{value}{suffix}"));
            seconds %= unit_seconds;
            if short {
                break;
            }
        }
    }
    if parts.is_empty() {
        "0s".to_string()
    } else {
        parts.join("")
    }
}

fn teleport_unavailable_lines(target: &str) -> Vec<String> {
    let mut lines = vec![format!("& {target}")];
    lines.push("─".repeat(120));
    lines.extend([
        "  ⎢ Error: Teleport requires a Vibe Pro subscription. Your current API key isn't eligible. Upgrade to Vibe Pro:".to_string(),
        "  ⎣ https://chat.mistral.ai/code/extensions?focus=key".to_string(),
        String::new(),
        String::new(),
    ]);
    lines
}

#[derive(Debug, Clone)]
struct ManualBashResult {
    stdout: String,
    stderr: String,
    exit_code: i32,
    status: Option<String>,
}

async fn run_manual_bash_command(command: &str) -> ManualBashResult {
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        TokioCommand::new("sh")
            .arg("-c")
            .arg(command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await;

    match output {
        Ok(Ok(output)) => ManualBashResult {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code().unwrap_or(1),
            status: None,
        },
        Ok(Err(error)) => ManualBashResult {
            stdout: String::new(),
            stderr: format!("Command failed: {error}"),
            exit_code: 1,
            status: None,
        },
        Err(_) => ManualBashResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 1,
            status: Some("timed out after 30 seconds".to_string()),
        },
    }
}

fn manual_bash_empty_lines() -> Vec<String> {
    vec![
        "  ⎣ Error: No command provided after '!'".to_string(),
        String::new(),
        String::new(),
    ]
}

fn manual_bash_display_lines(command: &str, result: &ManualBashResult) -> Vec<String> {
    let mut lines = vec![format!("$ {command}")];
    let mut output = String::new();
    output.push_str(&result.stdout);
    output.push_str(&result.stderr);
    if output.is_empty() {
        output.push_str("(no output)");
    }
    let output_lines = output.trim_end_matches('\n').lines().collect::<Vec<_>>();
    for (idx, line) in output_lines.iter().enumerate() {
        let marker = if idx + 1 == output_lines.len() {
            "⎣"
        } else {
            "⎢"
        };
        lines.push(format!("  {marker} {line}"));
    }
    lines.extend([String::new(), String::new()]);
    lines
}

fn bash_max_output_bytes(config: &Config) -> usize {
    config
        .tools
        .get("bash")
        .and_then(|tool| tool.max_output_bytes)
        .unwrap_or(16_000)
}

fn cap_manual_bash_output(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let capped = text.chars().take(limit).collect::<String>();
    format!("{capped}\n... [truncated]")
}

fn manual_bash_context(
    command: &str,
    cwd: &Path,
    result: &ManualBashResult,
    max_output_bytes: usize,
) -> String {
    let mut sections = vec![
        "Manual `!` command result from the user. Use this as context only.".to_string(),
        format!("Command: `{command}`"),
        format!("Working directory: `{}`", cwd.display()),
    ];
    if let Some(status) = &result.status {
        sections.push(format!("Status: {status}"));
    }
    sections.push(format!("Exit code: {}", result.exit_code));
    if !result.stdout.is_empty() {
        let stdout = cap_manual_bash_output(&result.stdout, max_output_bytes);
        sections.push(format!("Stdout:\n```text\n{}\n```", stdout.trim_end()));
    }
    if !result.stderr.is_empty() {
        let stderr = cap_manual_bash_output(&result.stderr, max_output_bytes);
        sections.push(format!("Stderr:\n```text\n{}\n```", stderr.trim_end()));
    }
    if result.stdout.is_empty() && result.stderr.is_empty() {
        sections.push("Output:\n```text\n(no output)\n```".to_string());
    }
    sections.join("\n\n")
}

fn format_user_prompt_lines(prompt: &str) -> Vec<String> {
    let mut lines = prompt.lines();
    let first = lines.next().unwrap_or_default();
    let mut formatted = vec![format!("> {first}")];
    formatted.extend(lines.map(|line| {
        if line.is_empty() {
            String::new()
        } else {
            format!("  {line}")
        }
    }));
    formatted
}

fn format_user_prompt_lines_with_images(prompt: &str, images: &[ImageAttachment]) -> Vec<String> {
    let mut formatted = format_user_prompt_lines(prompt);
    if !images.is_empty() {
        let label = if images.len() == 1 {
            "attached image"
        } else {
            "attached images"
        };
        let aliases = images
            .iter()
            .map(|image| image.alias.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        formatted.push(format!("  └ {label}: {aliases}"));
    }
    formatted
}

fn add_input_history(history: &mut Vec<String>, input: &str) {
    let entry = input.trim();
    if entry.is_empty() {
        return;
    }
    if history.last().is_some_and(|last| last == entry) {
        return;
    }
    history.push(entry.to_string());
    const MAX_INPUT_HISTORY: usize = 100;
    if history.len() > MAX_INPUT_HISTORY {
        history.drain(..history.len() - MAX_INPUT_HISTORY);
    }
    save_input_history(history);
}

fn reset_input_history_navigation(index: &mut Option<usize>, draft: &mut String) {
    *index = None;
    draft.clear();
}

fn insert_input_char(input: &mut String, cursor: &mut usize, ch: char) {
    *cursor = (*cursor).min(input.len());
    if !input.is_char_boundary(*cursor) {
        *cursor = previous_input_boundary(input, *cursor);
    }
    input.insert(*cursor, ch);
    *cursor += ch.len_utf8();
}

fn backspace_input(input: &mut String, cursor: &mut usize) {
    if input.is_empty() || *cursor == 0 {
        return;
    }
    *cursor = (*cursor).min(input.len());
    let previous = previous_input_boundary(input, *cursor);
    input.drain(previous..*cursor);
    *cursor = previous;
}

fn delete_input_right(input: &mut String, cursor: &mut usize) {
    if input.is_empty() {
        *cursor = 0;
        return;
    }
    *cursor = (*cursor).min(input.len());
    if *cursor >= input.len() {
        return;
    }
    if !input.is_char_boundary(*cursor) {
        *cursor = previous_input_boundary(input, *cursor);
    }
    let next = next_input_boundary(input, *cursor);
    input.drain(*cursor..next);
}

fn delete_to_line_start(input: &mut String, cursor: &mut usize) {
    if input.is_empty() || *cursor == 0 {
        return;
    }
    *cursor = (*cursor).min(input.len());
    if !input.is_char_boundary(*cursor) {
        *cursor = previous_input_boundary(input, *cursor);
    }
    let start = current_line_start(input, *cursor);
    input.drain(start..*cursor);
    *cursor = start;
}

fn delete_to_line_end(input: &mut String, cursor: &mut usize) {
    if input.is_empty() {
        *cursor = 0;
        return;
    }
    *cursor = (*cursor).min(input.len());
    if !input.is_char_boundary(*cursor) {
        *cursor = previous_input_boundary(input, *cursor);
    }
    let end = current_line_end(input, *cursor);
    input.drain(*cursor..end);
}

fn delete_word_left(input: &mut String, cursor: &mut usize) {
    if input.is_empty() || *cursor == 0 {
        return;
    }
    *cursor = (*cursor).min(input.len());
    if !input.is_char_boundary(*cursor) {
        *cursor = previous_input_boundary(input, *cursor);
    }
    let start = previous_word_boundary(input, *cursor);
    input.drain(start..*cursor);
    *cursor = start;
}

fn delete_word_right(input: &mut String, cursor: &mut usize) {
    if input.is_empty() {
        *cursor = 0;
        return;
    }
    *cursor = (*cursor).min(input.len());
    if *cursor >= input.len() {
        return;
    }
    if !input.is_char_boundary(*cursor) {
        *cursor = previous_input_boundary(input, *cursor);
    }
    let end = next_word_boundary(input, *cursor);
    input.drain(*cursor..end);
}

fn current_line_start(input: &str, cursor: usize) -> usize {
    input[..cursor.min(input.len())]
        .rfind('\n')
        .map(|index| index + '\n'.len_utf8())
        .unwrap_or(0)
}

fn current_line_end(input: &str, cursor: usize) -> usize {
    let cursor = cursor.min(input.len());
    input[cursor..]
        .find('\n')
        .map(|offset| cursor + offset)
        .unwrap_or(input.len())
}

fn previous_input_boundary(input: &str, cursor: usize) -> usize {
    let cursor = cursor.min(input.len());
    let mut previous = 0;
    for (index, _) in input.char_indices() {
        if index >= cursor {
            break;
        }
        previous = index;
    }
    previous
}

fn next_input_boundary(input: &str, cursor: usize) -> usize {
    let cursor = cursor.min(input.len());
    if cursor >= input.len() {
        return input.len();
    }
    let mut indices = input[cursor..].char_indices();
    let _ = indices.next();
    indices
        .next()
        .map(|(offset, _)| cursor + offset)
        .unwrap_or(input.len())
}

fn previous_word_boundary(input: &str, cursor: usize) -> usize {
    let mut index = previous_input_boundary(input, cursor);
    while index > 0 {
        let Some(ch) = input[..index].chars().next_back() else {
            break;
        };
        if !ch.is_whitespace() {
            break;
        }
        index = previous_input_boundary(input, index);
    }
    while index > 0 {
        let Some(ch) = input[..index].chars().next_back() else {
            break;
        };
        if ch.is_whitespace() {
            break;
        }
        index = previous_input_boundary(input, index);
    }
    index
}

fn next_word_boundary(input: &str, cursor: usize) -> usize {
    let mut index = next_input_boundary(input, cursor);
    while index < input.len() {
        let Some(ch) = input[index..].chars().next() else {
            break;
        };
        if !ch.is_whitespace() {
            break;
        }
        index = next_input_boundary(input, index);
    }
    while index < input.len() {
        let Some(ch) = input[index..].chars().next() else {
            break;
        };
        if ch.is_whitespace() {
            break;
        }
        index = next_input_boundary(input, index);
    }
    index
}

fn input_history_path() -> Option<PathBuf> {
    if let Ok(vibe_home) = std::env::var("VIBE_HOME") {
        return Some(PathBuf::from(vibe_home).join("vibehistory"));
    }
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".vibe").join("vibehistory"))
}

fn load_input_history() -> Vec<String> {
    let Some(path) = input_history_path() else {
        return Vec::new();
    };
    if !path.exists() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&path, "Hello Vibe!\n");
    }
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for line in contents.lines().filter(|line| !line.is_empty()) {
        let entry = serde_json::from_str::<String>(line).unwrap_or_else(|_| line.to_string());
        entries.push(entry);
    }
    const MAX_INPUT_HISTORY: usize = 100;
    if entries.len() > MAX_INPUT_HISTORY {
        entries.drain(..entries.len() - MAX_INPUT_HISTORY);
    }
    entries
}

fn save_input_history(history: &[String]) {
    let Some(path) = input_history_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut contents = String::new();
    for entry in history {
        if let Ok(line) = serde_json::to_string(entry) {
            contents.push_str(&line);
            contents.push('\n');
        }
    }
    let _ = fs::write(path, contents);
}

fn input_history_previous(
    history: &[String],
    input: &mut String,
    index: &mut Option<usize>,
    draft: &mut String,
) {
    if history.is_empty() {
        return;
    }
    let next_index = if let Some(current) = *index {
        current.saturating_sub(1)
    } else {
        *draft = input.clone();
        history.len() - 1
    };
    *index = Some(next_index);
    *input = history[next_index].clone();
}

fn input_history_next(
    history: &[String],
    input: &mut String,
    index: &mut Option<usize>,
    draft: &mut String,
) {
    let Some(current) = *index else {
        return;
    };
    if current + 1 < history.len() {
        let next_index = current + 1;
        *index = Some(next_index);
        *input = history[next_index].clone();
    } else {
        *index = None;
        *input = std::mem::take(draft);
    }
}

fn slash_command_line(command: &str) -> String {
    command
        .strip_prefix('/')
        .map(|rest| format!("/ {rest}"))
        .unwrap_or_else(|| command.to_string())
}

fn startup_lines(
    model: &str,
    model_count: usize,
    mcp_server_summary: String,
    skill_count: usize,
    show_initializing: bool,
) -> Vec<String> {
    let model_word = if model_count == 1 { "model" } else { "models" };
    let skill_word = if skill_count == 1 { "skill" } else { "skills" };
    let mut lines = vec![String::new(); 20];
    lines.extend([
        PETIT_CHAT_INITIAL_LINES[0].to_string(),
        PETIT_CHAT_INITIAL_LINES[1].to_string(),
        PETIT_CHAT_INITIAL_LINES[2].to_string(),
        String::new(),
        format!("Mistral Vibe v2.17.1 · {model}"),
        format!("{model_count} {model_word} · {mcp_server_summary} · {skill_count} {skill_word}"),
        "Type /help for more information".to_string(),
        String::new(),
        String::new(),
        if show_initializing {
            "⠋ Initializing…".to_string()
        } else {
            String::new()
        },
    ]);
    lines
}

fn mcp_server_summary(config: &Config) -> String {
    let total = config.mcp_servers.len();
    if total == 0 {
        return "0 MCP servers".to_string();
    }
    let loaded = config
        .mcp_servers
        .iter()
        .filter(|server| !server.disabled)
        .count();
    if loaded == total {
        let noun = if loaded == 1 { "server" } else { "servers" };
        return format!("{loaded} MCP {noun}");
    }
    let noun = if total == 1 { "server" } else { "servers" };
    format!("{loaded}/{total} MCP {noun}")
}

fn available_skill_count() -> usize {
    skill_roots()
        .into_iter()
        .filter_map(|root| std::fs::read_dir(root).ok())
        .flat_map(|entries| entries.flatten())
        .filter(|entry| entry.path().join("SKILL.md").is_file())
        .count()
}

fn expand_skill_prompt(user_input: &str) -> Option<String> {
    let stripped = user_input.trim();
    let command = stripped.strip_prefix('/')?;
    let split_at = command
        .char_indices()
        .find_map(|(idx, ch)| ch.is_whitespace().then_some(idx))
        .unwrap_or(command.len());
    if split_at == 0 {
        return None;
    }
    let name = command[..split_at].to_ascii_lowercase();
    let extra = command[split_at..].trim_start();
    let prompt = load_skill_prompt(&name)?;
    if extra.is_empty() {
        Some(prompt)
    } else {
        Some(format!("{user_input}\n\n{prompt}"))
    }
}

fn load_skill_prompt(name: &str) -> Option<String> {
    for root in skill_roots() {
        let skill_file = root.join(name).join("SKILL.md");
        if !skill_file.is_file() {
            continue;
        }
        let raw = fs::read_to_string(skill_file).ok()?;
        let prompt = skill_body_from_markdown(&raw);
        if !prompt.is_empty() {
            return Some(prompt);
        }
    }
    None
}

fn skill_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd.join(".vibe").join("skills"));
    }
    if let Ok(vibe_home) = std::env::var("VIBE_HOME") {
        roots.push(PathBuf::from(vibe_home).join("skills"));
    }
    if let Ok(home) = std::env::var("HOME") {
        roots.push(PathBuf::from(home).join(".vibe").join("skills"));
    }
    let mut unique = Vec::new();
    for root in roots {
        if !unique.iter().any(|seen| seen == &root) {
            unique.push(root);
        }
    }
    unique
}

fn skill_body_from_markdown(raw: &str) -> String {
    let raw = raw.trim_start();
    if let Some(rest) = raw.strip_prefix("---")
        && let Some((_, body)) = rest.split_once("---")
    {
        return body.trim().to_string();
    }
    raw.trim().to_string()
}

fn display_agent_name(agent: &str, order: &[AgentSummary]) -> String {
    order
        .iter()
        .find(|summary| summary.name == agent)
        .map(|summary| summary.display_name.to_ascii_lowercase())
        .unwrap_or_else(|| match agent {
            "accept-edits" => "accept edits".to_string(),
            "auto-approve" => "auto approve".to_string(),
            other => other.to_string(),
        })
}

fn next_agent_name(current: &str, order: &[AgentSummary]) -> String {
    if order.is_empty() {
        return "default".to_string();
    }
    let idx = order
        .iter()
        .position(|summary| summary.name == current)
        .map(|idx| idx + 1)
        .unwrap_or(0);
    order[idx % order.len()].name.clone()
}

fn is_builtin_primary_agent(agent: &str) -> bool {
    matches!(agent, "default" | "plan" | "accept-edits" | "auto-approve")
}

fn help_lines() -> Vec<String> {
    [
        "  ⎢".to_string(),
        "  ⎢ Commands".to_string(),
        "  ⎢".to_string(),
        "  ⎢ • /clear: Clear conversation history".to_string(),
        "  ⎢ • /compact: Compact conversation history by summarizing. Optionally pass instructions to guide the summary".to_string(),
        "  ⎢ • /config: Edit config settings".to_string(),
        "  ⎢ • /copy: Copy the last agent message to the clipboard".to_string(),
        "  ⎢ • /data-retention: Show data retention information".to_string(),
        "  ⎢ • /debug: Toggle debug console".to_string(),
        "  ⎢ • /exit, :q, :quit, exit, quit: Exit the application".to_string(),
        "  ⎢ • /help: Show help message".to_string(),
        "  ⎢ • /leanstall: Install the Lean 4 agent (leanstral)".to_string(),
        "  ⎢ • /log: Show path to current interaction log file".to_string(),
        "  ⎢ • /loop: Schedule a recurring prompt. Use /loop <interval> <prompt>, /loop list, or /loop cancel <id|all>".to_string(),
        "  ⎢ • /mcp, /connectors: Display available MCP servers and connectors. Pass a name to list tools; subcommands:".to_string(),
        "  ⎢   status, login , logout".to_string(),
        "  ⎢ • /model: Select active model".to_string(),
        "  ⎢ • /proxy-setup: Configure proxy and SSL certificate settings".to_string(),
        "  ⎢ • /reload: Reload configuration, agent instructions, and skills from disk".to_string(),
        "  ⎢ • /rename: Rename the current session".to_string(),
        "  ⎢ • /resume, /continue: Browse, resume, or delete saved sessions".to_string(),
        "  ⎢ • /rewind: Rewind to a previous message".to_string(),
        "  ⎢ • /status: Display agent statistics".to_string(),
        "  ⎢ • /teleport: Teleport session to Vibe Code Web".to_string(),
        "  ⎢ • /theme: Select theme".to_string(),
        "  ⎢ • /thinking: Select thinking level".to_string(),
        "  ⎢ • /unleanstall: Uninstall the Lean 4 agent".to_string(),
        "  ⎣ • /voice: Configure voice settings".to_string(),
        String::new(),
        String::new(),
        String::new(),
    ]
    .into_iter()
    .collect()
}

fn status_lines() -> Vec<String> {
    [
        slash_command_line("/status"),
        "  ⎢ Agent Statistics".to_string(),
        "  ⎢ • Steps: 0".to_string(),
        "  ⎢ • Session Prompt Tokens: 0".to_string(),
        "  ⎢ • Session Completion Tokens: 0".to_string(),
        "  ⎢ • Session Total LLM Tokens: 0".to_string(),
        "  ⎢ • Last Turn Tokens: 0".to_string(),
        "  ⎣ • Cost: $0.0000".to_string(),
        String::new(),
        String::new(),
    ]
    .into_iter()
    .collect()
}

fn data_retention_lines() -> Vec<String> {
    [
        slash_command_line("/data-retention"),
        "  ⎢ Your Data Helps Improve Mistral AI".to_string(),
        "  ⎢ At Mistral AI, we're committed to delivering the best possible experience. When you use Mistral models on our API,".to_string(),
        "  ⎢ your interactions may be collected to improve our models, ensuring they stay cutting-edge, accurate, and helpful.".to_string(),
        "  ⎢".to_string(),
        "  ⎣ Manage your data settings here".to_string(),
        String::new(),
        String::new(),
    ]
    .into_iter()
    .collect()
}

fn log_lines(store: &microvibe_core::SessionStore) -> Vec<String> {
    let (path_prefix, session_suffix) = store.log_path_parts_for_display();
    [
        slash_command_line("/log"),
        "  ⎢ Current Log Directory".to_string(),
        format!("  ⎢ {path_prefix}"),
        if session_suffix.is_empty() {
            "  ⎢".to_string()
        } else {
            format!("  ⎢ {session_suffix}")
        },
        "  ⎢".to_string(),
        "  ⎣ You can send this directory to share your interaction.".to_string(),
        String::new(),
        String::new(),
    ]
    .into_iter()
    .collect()
}

fn handle_agent_event(
    transcript: &mut Vec<String>,
    bottom_panel: &mut Option<BottomPanel>,
    current_agent: &mut String,
    tool_render_records: &mut Vec<ToolRenderRecord>,
    tools_collapsed: bool,
    event: AgentEvent,
) {
    match event {
        AgentEvent::SessionConfigured { .. }
        | AgentEvent::TurnStarted { .. }
        | AgentEvent::UsageUpdated { .. }
        | AgentEvent::ThoughtDelta { .. } => {}
        AgentEvent::AssistantDelta { text } => {
            if let Some(last) = transcript.last_mut() {
                if last.starts_with("  ") && !last.starts_with("  ⎣") && !last.starts_with("  ⎢")
                {
                    last.push_str(&text);
                    return;
                }
                if last.starts_with("  ⎣") || last.starts_with("  ⎢") {
                    transcript.push(String::new());
                }
            }
            transcript.push(format!("  {text}"));
        }
        AgentEvent::TurnCompleted { .. } => {
            if transcript.last().is_some_and(|line| !line.is_empty()) {
                transcript.extend([String::new(), String::new()]);
            }
            if transcript.iter().rev().take(8).any(|line| {
                line.starts_with("✕ Staying in plan mode.") || line.starts_with("✓ Switched to ")
            }) {
                transcript.push(String::new());
            }
        }
        AgentEvent::Error { message } => transcript.push(format!("error: {message}")),
        AgentEvent::ToolCallStarted { call } => {
            if call.name == "bash" {
                transcript.push(format!("■ bash: {}", tool_call_arg(&call, "command")));
                transcript.push(String::new());
                transcript
                    .push("⠋  Running command… (<duration> Esc/Ctrl+C to interrupt)".to_string());
                *bottom_panel = Some(approval_panel(&call));
            } else if call.name == "write_file" {
                transcript.push(format!(
                    "□ Writing {}",
                    display_tool_path(&tool_call_arg(&call, "path"))
                ));
                transcript.push(String::new());
                transcript
                    .push("⠋  Writing file… (<duration> Esc/Ctrl+C to interrupt)".to_string());
                *bottom_panel = Some(approval_panel(&call));
            } else if call.name == "edit" {
                transcript.push(format!(
                    "■ Editing {}",
                    display_tool_filename(&tool_call_arg(&call, "file_path"))
                ));
                transcript.push(String::new());
                transcript
                    .push("⠋  Editing files… (<duration> Esc/Ctrl+C to interrupt)".to_string());
                *bottom_panel = Some(approval_panel(&call));
            } else if call.name == "web_fetch" {
                transcript.push(format!(
                    "□ Fetching: {}",
                    display_url_domain(&tool_call_arg(&call, "url"))
                ));
                transcript.push(String::new());
                transcript
                    .push("⢰  Fetching URL… (<duration> Esc/Ctrl+C to interrupt)".to_string());
                *bottom_panel = Some(approval_panel(&call));
            } else if call.name == "web_search" {
                transcript.push(format!(
                    "□ Searching the web: '{}'",
                    tool_call_arg(&call, "query")
                ));
                transcript.push(String::new());
                transcript
                    .push("⠏  Searching the web… (<duration> Esc/Ctrl+C to interrupt)".to_string());
                *bottom_panel = Some(approval_panel(&call));
            } else if call.name == "task" {
                transcript.push(format!(
                    "■ Running {} agent: {}",
                    tool_call_arg(&call, "agent"),
                    tool_call_arg(&call, "task")
                ));
                transcript.push(String::new());
                transcript
                    .push("⠋  Running subagent… (<duration> Esc/Ctrl+C to interrupt)".to_string());
                if task_call_needs_approval(&call) {
                    *bottom_panel = Some(approval_panel(&call));
                }
            } else if call.name == "ask_user_question" {
                transcript.push(format!("■ Asking: {}", first_question_text(&call)));
                transcript.push(String::new());
                transcript.push(
                    "⠋  Waiting for user input… (<duration> Esc/Ctrl+C to interrupt)".to_string(),
                );
                *bottom_panel = Some(question_panel(&call));
            } else if call.name == "exit_plan_mode" {
                transcript.push("■ Ready to exit plan mode".to_string());
                transcript.push(String::new());
                transcript.push(
                    "⠋  Waiting for user confirmation… (<duration> Esc/Ctrl+C to interrupt)"
                        .to_string(),
                );
            }
        }
        AgentEvent::ToolCallCompleted { result } => {
            if result.name == "bash" {
                remove_pending_tool_block(transcript, &["□ bash:", "■ bash:"]);
            } else if result.name == "write_file" && result.success {
                remove_pending_tool_block(transcript, &["□ Writing ", "■ Writing "]);
            } else if result.name == "edit" && result.success {
                remove_pending_tool_block(transcript, &["□ Editing ", "■ Editing "]);
                transcript.retain(|line| !line.contains("Editing files…"));
            } else if result.name == "web_fetch" {
                remove_pending_tool_block(transcript, &["□ Fetching: ", "■ Fetching: "]);
            } else if result.name == "web_search" {
                remove_pending_tool_block(
                    transcript,
                    &["□ Searching the web: ", "■ Searching the web: "],
                );
            } else if result.name == "task" {
                remove_pending_tool_block(transcript, &["□ Running ", "■ Running "]);
            } else if result.name == "ask_user_question" {
                remove_pending_tool_block(transcript, &["■ Asking: "]);
            } else if result.name == "exit_plan_mode" {
                remove_pending_tool_block(transcript, &["■ Ready to exit plan mode"]);
                if result.success {
                    if result.output.contains("accept-edits mode") {
                        *current_agent = "accept edits".to_string();
                    } else if result.output.contains("default agent mode") {
                        *current_agent = "default".to_string();
                    }
                }
            }
            let record = ToolRenderRecord {
                collapsed: tool_result_display_lines(&result, true),
                expanded: tool_result_display_lines(&result, false),
            };
            transcript.extend(if tools_collapsed {
                record.collapsed.clone()
            } else {
                record.expanded.clone()
            });
            tool_render_records.push(record);
        }
        AgentEvent::HookRunStarted {
            scope, tool_name, ..
        } => {
            let label = match (scope, tool_name) {
                (microvibe_protocol::HookScope::BeforeTool, Some(name)) => {
                    format!("before_tool hooks for {name}")
                }
                (microvibe_protocol::HookScope::AfterTool, Some(name)) => {
                    format!("after_tool hooks for {name}")
                }
                (microvibe_protocol::HookScope::PostAgentTurn, _) => {
                    "post_agent_turn hooks".to_string()
                }
                _ => "hooks".to_string(),
            };
            transcript.push(format!("■ Running {label}"));
        }
        AgentEvent::HookStarted { hook_name, .. } => {
            transcript.push(format!("  ⎢ ◦ {hook_name}"));
        }
        AgentEvent::HookEnded {
            hook_name,
            status,
            content,
            ..
        } => {
            let icon = match status {
                microvibe_protocol::HookMessageSeverity::Ok => "✓",
                microvibe_protocol::HookMessageSeverity::Warning => "⚠",
                microvibe_protocol::HookMessageSeverity::Error => "✗",
            };
            if let Some(content) = content.filter(|content| !content.is_empty()) {
                transcript.push(format!("  ⎣ {icon} {hook_name}: {content}"));
            } else {
                transcript.push(format!("  ⎣ {icon} {hook_name}"));
            }
        }
        AgentEvent::HookRunCompleted { .. } => {}
    }
}

fn respond_pending_turn(
    running_turn: &mut Option<RunningTurn>,
    transcript: &mut [String],
    decision: ApprovalDecision,
) {
    if let Some(running) = running_turn.as_mut() {
        if let Some(request) = running.approval.take() {
            let _ = request.respond_to.send(decision);
        } else {
            running.queued_decision = Some(decision);
        }
        if !matches!(decision, ApprovalDecision::Deny) {
            mark_pending_tool_selected(transcript);
        }
    }
}

fn deny_pending_turn(running_turn: &mut Option<RunningTurn>) {
    if let Some(running) = running_turn.as_mut() {
        if let Some(request) = running.approval.take() {
            let _ = request.respond_to.send(ApprovalDecision::Deny);
        } else {
            running.queued_decision = Some(ApprovalDecision::Deny);
        }
    }
}

fn respond_pending_question(running_turn: &mut Option<RunningTurn>, response: QuestionResponse) {
    if let Some(running) = running_turn.as_mut() {
        if let Some(request) = running.question.take() {
            let _ = request.respond_to.send(response);
        } else {
            running.queued_question = Some(response);
        }
    }
}

fn advance_question_panel(panel: &mut BottomPanel) -> Option<QuestionResponse> {
    if panel.question_is_multi_select() && panel.selected != panel.question_submit_index() {
        if panel.question_other_index() == Some(panel.selected) {
            if !panel.question_other_text().trim().is_empty() {
                panel.selected = panel.question_submit_index();
            }
            return None;
        }
        if let Some(pos) = panel
            .question_selected_options
            .iter()
            .position(|idx| *idx == panel.selected)
        {
            panel.question_selected_options.remove(pos);
        } else {
            panel.question_selected_options.push(panel.selected);
            panel.question_selected_options.sort_unstable();
        }
        return None;
    }
    let answer = selected_question_answer(panel)?;
    panel.question_answers.push(answer);
    let next_index = panel.question_index + 1;
    let question_count = panel
        .question_call
        .as_ref()
        .map(question_count)
        .unwrap_or(0);
    if next_index >= question_count {
        return Some(QuestionResponse {
            answers: panel.question_answers.clone(),
            cancelled: false,
        });
    }
    panel.question_index = next_index;
    panel.selected = 0;
    panel.question_selected_options.clear();
    if let Some(call) = panel.question_call.as_ref() {
        panel.title = question_text_at(call, panel.question_index);
        panel.options = question_options_at(call, panel.question_index);
        panel.help = question_help(call);
    }
    None
}

fn selected_question_answer(panel: &BottomPanel) -> Option<QuestionAnswer> {
    if panel.question_is_multi_select() {
        let other_idx = panel.question_other_index();
        let mut is_other = false;
        let labels = panel
            .question_selected_options
            .iter()
            .filter_map(|idx| {
                if Some(*idx) == other_idx {
                    let other_text = panel.question_other_text().trim();
                    if other_text.is_empty() {
                        None
                    } else {
                        is_other = true;
                        Some(other_text.to_string())
                    }
                } else {
                    panel.options.get(*idx).map(|raw| {
                        raw.split_once(" - ")
                            .map(|(label, _)| label)
                            .unwrap_or(raw)
                            .to_string()
                    })
                }
            })
            .collect::<Vec<_>>();
        if labels.is_empty() {
            return None;
        }
        let question = panel
            .question_call
            .as_ref()
            .map(|call| question_text_at(call, panel.question_index))
            .unwrap_or_default();
        return Some(QuestionAnswer {
            question,
            answer: labels.join(", "),
            is_other,
        });
    }
    if panel.question_other_index() == Some(panel.selected) {
        let answer = panel.question_other_text().trim();
        if answer.is_empty() {
            return None;
        }
        let question = panel
            .question_call
            .as_ref()
            .map(|call| question_text_at(call, panel.question_index))
            .unwrap_or_default();
        return Some(QuestionAnswer {
            question,
            answer: answer.to_string(),
            is_other: true,
        });
    }
    let raw = panel
        .options
        .get(panel.selected)
        .cloned()
        .unwrap_or_default();
    let answer = raw
        .split_once(" - ")
        .map(|(label, _)| label)
        .unwrap_or(&raw)
        .to_string();
    let question = panel
        .question_call
        .as_ref()
        .map(|call| question_text_at(call, panel.question_index))
        .unwrap_or_default();
    Some(QuestionAnswer {
        question,
        answer,
        is_other: false,
    })
}

fn cancelled_question_response() -> QuestionResponse {
    QuestionResponse::cancelled()
}

fn approval_decision(panel: &BottomPanel) -> ApprovalDecision {
    match panel.selected {
        1 => ApprovalDecision::AllowSession,
        2 => ApprovalDecision::AllowAlways,
        3 => ApprovalDecision::Deny,
        _ => ApprovalDecision::AllowOnce,
    }
}

fn select_approval_option(panel: &mut BottomPanel, selected: usize) {
    panel.selected = selected.min(3);
    if let Some(rows) = panel.raw_rows.as_mut() {
        for row in &mut *rows {
            if let Some(rest) = row.strip_prefix("› ") {
                *row = format!("  {rest}");
            }
        }
        let needle = format!("{}. ", panel.selected + 1);
        if let Some(row) = rows
            .iter_mut()
            .find(|row| row.trim_start().starts_with(&needle))
        {
            let trimmed = row.trim_start().to_string();
            *row = format!("› {trimmed}");
        }
    }
}

fn tool_call_arg(call: &ToolCall, name: &str) -> String {
    call.arguments
        .get(name)
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

fn task_call_needs_approval(call: &ToolCall) -> bool {
    let agent = call
        .arguments
        .get("agent")
        .and_then(|value| value.as_str())
        .unwrap_or("explore");
    agent != "explore"
}

fn approval_panel(call: &ToolCall) -> BottomPanel {
    let title = approval_title(call);
    let raw_rows = approval_rows(call);
    BottomPanel {
        command: "/approval".to_string(),
        title,
        options: Vec::new(),
        selected: 0,
        help: "↑↓ navigate  Enter select  ESC reject".to_string(),
        scroll_marker: None,
        raw_rows: Some(raw_rows),
        toggled: false,
        auto_copy_on: false,
        resume_sessions: Vec::new(),
        delete_confirm: None,
        rewind_message_index: None,
        question_call: None,
        question_index: 0,
        question_answers: Vec::new(),
        question_selected_options: Vec::new(),
        question_other_texts: vec![String::new(); question_count(call)],
        proxy_values: Vec::new(),
        mounted_at: Instant::now(),
    }
}

fn question_panel(call: &ToolCall) -> BottomPanel {
    let options = question_options_at(call, 0);
    BottomPanel {
        command: "/question".to_string(),
        title: question_text_at(call, 0),
        options,
        selected: 0,
        help: question_help(call),
        scroll_marker: None,
        raw_rows: None,
        toggled: false,
        auto_copy_on: false,
        resume_sessions: Vec::new(),
        delete_confirm: None,
        rewind_message_index: None,
        question_call: Some(call.clone()),
        question_index: 0,
        question_answers: Vec::new(),
        question_selected_options: Vec::new(),
        question_other_texts: Vec::new(),
        proxy_values: Vec::new(),
        mounted_at: Instant::now(),
    }
}

fn first_question_text(call: &ToolCall) -> String {
    question_text_at(call, 0)
}

fn question_text_at(call: &ToolCall, index: usize) -> String {
    question_at(call, index)
        .and_then(|question| question.get("question"))
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

fn question_options_at(call: &ToolCall, index: usize) -> Vec<String> {
    let mut options = question_at(call, index)
        .and_then(|question| question.get("options"))
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .map(|option| {
            let label = option
                .get("label")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            let description = option
                .get("description")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            if description.is_empty() {
                label.to_string()
            } else {
                format!("{label} - {description}")
            }
        })
        .collect::<Vec<_>>();
    if !question_hide_other(call, index) {
        options.push("Type your answer...".to_string());
    }
    options
}

fn question_hide_other(call: &ToolCall, index: usize) -> bool {
    question_at(call, index)
        .and_then(|question| question.get("hide_other"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn question_at(call: &ToolCall, index: usize) -> Option<&serde_json::Value> {
    call.arguments
        .get("questions")
        .and_then(|value| value.as_array())
        .and_then(|questions| questions.get(index))
}

fn question_count(call: &ToolCall) -> usize {
    call.arguments
        .get("questions")
        .and_then(|value| value.as_array())
        .map(Vec::len)
        .unwrap_or(0)
}

fn question_help(call: &ToolCall) -> String {
    if question_is_multi_select_at(call, 0) {
        "↑↓ navigate  Enter toggle  Esc cancel".to_string()
    } else if question_count(call) > 1 {
        "←→ questions  ↑↓ navigate  Enter select  Esc cancel".to_string()
    } else {
        "↑↓ navigate  Enter select  Esc cancel".to_string()
    }
}

fn question_is_multi_select_at(call: &ToolCall, index: usize) -> bool {
    question_at(call, index)
        .and_then(|question| question.get("multi_select"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keyboard_enhancement_flags_request_event_types() {
        let flags = keyboard_enhancement_flags();
        assert!(flags.contains(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
        assert!(flags.contains(KeyboardEnhancementFlags::REPORT_EVENT_TYPES));
        assert!(flags.contains(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS));
        assert!(!flags.contains(KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES));
    }

    fn ansi_for(command: impl CrosstermCommand) -> String {
        let mut out = String::new();
        command.write_ansi(&mut out).unwrap();
        out
    }

    #[test]
    fn keyboard_reset_commands_match_codex_terminal_contract() {
        assert_eq!(ansi_for(DisableModifyOtherKeys), "\x1b[>4;0m");
        assert_eq!(ansi_for(EnableModifyOtherKeys), "\x1b[>4;2m");
        assert_eq!(ansi_for(ResetKeyboardEnhancementFlags), "\x1b[<u");
    }

    #[test]
    fn tmux_modify_other_keys_matches_codex_gate() {
        assert!(!tmux_session_detected(None, None));
        assert!(tmux_session_detected(Some("/tmp/tmux"), None));
        assert!(tmux_session_detected(None, Some("%1")));

        assert!(tmux_should_enable_modify_other_keys_for(
            true,
            Some("csi-u")
        ));
        assert!(!tmux_should_enable_modify_other_keys_for(
            true,
            Some("xterm")
        ));
        assert!(!tmux_should_enable_modify_other_keys_for(true, None));
        assert!(!tmux_should_enable_modify_other_keys_for(
            false,
            Some("csi-u")
        ));
    }

    #[test]
    fn key_normalization_applies_active_control_to_arrows() {
        let mut active = KeyModifiers::empty();
        let ctrl_press = event::KeyEvent::new_with_kind(
            KeyCode::Modifier(ModifierKeyCode::LeftControl),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        );
        let left = event::KeyEvent::new(KeyCode::Left, KeyModifiers::empty());
        let ctrl_release = event::KeyEvent::new_with_kind(
            KeyCode::Modifier(ModifierKeyCode::LeftControl),
            KeyModifiers::CONTROL,
            KeyEventKind::Release,
        );

        assert!(normalize_key_event(ctrl_press, &mut active).is_none());
        let normalized = normalize_key_event(left, &mut active).unwrap();
        assert_eq!(normalized.code, KeyCode::Left);
        assert!(normalized.modifiers.contains(KeyModifiers::CONTROL));
        assert!(normalize_key_event(ctrl_release, &mut active).is_none());
        assert!(!active.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn copy_selection_shortcuts_match_vibe_bindings() {
        assert!(is_copy_selection_shortcut(&event::KeyEvent::new(
            KeyCode::Char('y'),
            KeyModifiers::CONTROL,
        )));
        assert!(is_copy_selection_shortcut(&event::KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )));
        assert!(is_copy_selection_shortcut(&event::KeyEvent::new(
            KeyCode::Char('C'),
            KeyModifiers::CONTROL,
        )));
        assert!(!is_copy_selection_shortcut(&event::KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
    }

    #[test]
    fn approval_and_question_panels_guard_initial_submit_like_vibe() {
        let approval_call = ToolCall::new("bash", json!({"command": "printf parity"}));
        let mut approval = approval_panel(&approval_call);
        assert!(approval.guards_initial_submit());
        approval.mounted_at = Instant::now() - INPUT_GRACE_PERIOD - Duration::from_millis(1);
        assert!(!approval.guards_initial_submit());

        let question_call = ToolCall::new(
            "ask_user_question",
            json!({
                "questions": [{
                    "question": "Choose?",
                    "options": [{"label": "A"}, {"label": "B"}]
                }]
            }),
        );
        let mut question = question_panel(&question_call);
        assert!(question.guards_initial_submit());
        question.mounted_at = Instant::now() - INPUT_GRACE_PERIOD - Duration::from_millis(1);
        assert!(!question.guards_initial_submit());

        let mcp = mcp_panel(&McpIndex::default());
        assert!(!mcp.guards_initial_submit());
    }

    #[test]
    fn counts_multi_question_tool_calls() {
        let call = ToolCall::new(
            "ask_user_question",
            json!({
                "questions": [
                    {"question": "Choose first?", "options": [{"label": "Alpha"}]},
                    {"question": "Choose second?", "options": [{"label": "Gamma"}]}
                ]
            }),
        );
        assert_eq!(question_count(&call), 2);
        let mut panel = question_panel(&call);
        assert!(advance_question_panel(&mut panel).is_none());
        assert_eq!(panel.title, "Choose second?");
        let response = advance_question_panel(&mut panel).unwrap();
        assert_eq!(response.answers.len(), 2);
    }

    #[test]
    fn multi_select_toggles_until_submit() {
        let call = ToolCall::new(
            "ask_user_question",
            json!({
                "questions": [{
                    "question": "Pick colors?",
                    "multi_select": true,
                    "hide_other": true,
                    "options": [
                        {"label": "Red", "description": "Warm"},
                        {"label": "Blue", "description": "Cool"}
                    ]
                }]
            }),
        );
        let mut panel = question_panel(&call);
        assert!(advance_question_panel(&mut panel).is_none());
        panel.selected = 1;
        assert!(advance_question_panel(&mut panel).is_none());
        panel.selected = panel.question_submit_index();
        let response = advance_question_panel(&mut panel).unwrap();
        assert_eq!(response.answers[0].answer, "Red, Blue");
    }

    #[test]
    fn single_select_other_submits_custom_answer() {
        let call = ToolCall::new(
            "ask_user_question",
            json!({
                "questions": [{
                    "question": "Choose custom mode?",
                    "options": [
                        {"label": "Strict", "description": "Require exact parity"},
                        {"label": "Loose", "description": "Allow differences"}
                    ]
                }]
            }),
        );
        let mut panel = question_panel(&call);
        panel.selected = panel.question_other_index().unwrap();
        panel.push_question_other_char('C');
        panel.push_question_other_char('u');
        panel.push_question_other_char('s');
        let response = advance_question_panel(&mut panel).unwrap();
        assert_eq!(response.answers[0].answer, "Cus");
        assert!(response.answers[0].is_other);
    }

    #[test]
    fn multi_select_other_auto_selects_with_text() {
        let call = ToolCall::new(
            "ask_user_question",
            json!({
                "questions": [{
                    "question": "Pick colors?",
                    "multi_select": true,
                    "options": [
                        {"label": "Red"},
                        {"label": "Blue"}
                    ]
                }]
            }),
        );
        let mut panel = question_panel(&call);
        assert!(advance_question_panel(&mut panel).is_none());
        panel.selected = panel.question_other_index().unwrap();
        panel.push_question_other_char('G');
        panel.push_question_other_char('r');
        panel.push_question_other_char('e');
        panel.push_question_other_char('e');
        panel.push_question_other_char('n');
        panel.selected = panel.question_submit_index();
        let response = advance_question_panel(&mut panel).unwrap();
        assert_eq!(response.answers[0].answer, "Red, Green");
        assert!(response.answers[0].is_other);
    }

    #[test]
    fn pending_status_lines_animate_spinner_frames() {
        assert_eq!(snake_spinner_frame(0), "⠉⠁");
        assert_eq!(snake_spinner_frame(1), "⠈⠁");
        assert_eq!(snake_spinner_frame(9), "⢀⣠");
        assert_eq!(snake_spinner_frame(32), "⠉⠁");

        let line = "⠋  Running command… (<duration> Esc/Ctrl+C to interrupt)";
        assert_eq!(
            animate_spinner_line(line, 1),
            "⠈⠁  Running command… (<duration> Esc/Ctrl+C to interrupt)"
        );
        assert_eq!(
            animate_spinner_line(
                "⠋  Running subagent… (<duration> Esc/Ctrl+C to interrupt)",
                1
            ),
            "⠈⠁  Running subagent… (<duration> Esc/Ctrl+C to interrupt)"
        );
        assert_eq!(animate_spinner_line("✓ Ran command", 1), "✓ Ran command");
    }

    #[test]
    fn path_mentions_embed_small_text_files_for_model() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sample.txt");
        fs::write(&file, "alpha\nbeta\n").unwrap();
        let input = format!("use @{} please", file.display());

        let rendered = render_path_mentions_for_model_with_options(&input, false);

        assert!(rendered.starts_with(&format!("use {} please\n\nfile://", file.display())));
        assert!(rendered.contains("/sample.txt\n```\nalpha\nbeta\n\n```"));
    }

    #[test]
    fn path_mentions_link_folders_for_model() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("notes");
        fs::create_dir(&folder).unwrap();
        let input = format!("use @{}", folder.display());

        let rendered = render_path_mentions_for_model_with_options(&input, false);

        assert!(rendered.starts_with(&format!("use {}\n\nuri: file://", folder.display())));
        assert!(rendered.contains("/notes\nname: "));
        assert!(rendered.ends_with(&folder.display().to_string()));
    }

    #[test]
    fn path_mentions_skip_images_when_carried_as_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("shot.png");
        fs::write(&image, b"not-really-a-png").unwrap();
        let input = format!("inspect @{}", image.display());

        let rendered = render_path_mentions_for_model_with_options(&input, true);
        let resources = image_resources(&input);

        assert_eq!(rendered, format!("inspect {}", image.display()));
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].alias, image.display().to_string());
    }

    #[test]
    fn todo_items_parse_for_display() {
        let output = concat!(
            "message: Updated 2 todos\n",
            "todos: [",
            "{'id': '1', 'content': 'Check parity', 'status': <TodoStatus.IN_PROGRESS: 'in_progress'>, 'priority': <TodoPriority.HIGH: 'high'>}, ",
            "{'id': '2', 'content': 'Ship Rust version', 'status': <TodoStatus.COMPLETED: 'completed'>, 'priority': <TodoPriority.HIGH: 'high'>}",
            "]\n",
            "total_count: 2"
        );
        let todos = todo_items_from_output(output);
        assert_eq!(
            todos,
            vec![
                TodoDisplayItem {
                    content: "Check parity".to_string(),
                    status: "in_progress".to_string(),
                },
                TodoDisplayItem {
                    content: "Ship Rust version".to_string(),
                    status: "completed".to_string(),
                },
            ]
        );
    }

    #[test]
    fn prompt_cursor_insert_backspace_and_delete_are_utf8_safe() {
        let mut input = "aéz".to_string();
        let mut cursor = input.len();

        cursor = previous_input_boundary(&input, cursor);
        assert_eq!(&input[cursor..], "z");
        delete_input_right(&mut input, &mut cursor);
        assert_eq!(input, "aé");

        cursor = previous_input_boundary(&input, cursor);
        backspace_input(&mut input, &mut cursor);
        assert_eq!(input, "é");
        assert_eq!(cursor, 0);

        insert_input_char(&mut input, &mut cursor, 'x');
        assert_eq!(input, "xé");
        assert_eq!(cursor, 1);
    }

    #[test]
    fn prompt_word_boundaries_support_modified_arrows() {
        let input = "alpha  beta gamma";

        assert_eq!(
            previous_word_boundary(input, input.len()),
            "alpha  beta ".len()
        );
        assert_eq!(
            previous_word_boundary(input, "alpha  beta ".len()),
            "alpha  ".len()
        );
        assert_eq!(previous_word_boundary(input, "alpha  ".len()), 0);

        assert_eq!(next_word_boundary(input, 0), "alpha".len());
        assert_eq!(
            next_word_boundary(input, "alpha".len()),
            "alpha  beta".len()
        );
        assert_eq!(next_word_boundary(input, "alpha  beta".len()), input.len());
    }

    #[test]
    fn prompt_line_editing_shortcuts_are_utf8_and_multiline_safe() {
        let mut input = "first line\nalpha  béta gamma\nlast".to_string();
        let mut cursor = "first line\nalpha  béta".len();

        delete_word_left(&mut input, &mut cursor);
        assert_eq!(input, "first line\nalpha   gamma\nlast");
        assert_eq!(cursor, "first line\nalpha  ".len());

        delete_to_line_start(&mut input, &mut cursor);
        assert_eq!(input, "first line\n gamma\nlast");
        assert_eq!(cursor, "first line\n".len());

        delete_word_right(&mut input, &mut cursor);
        assert_eq!(input, "first line\n\nlast");
        assert_eq!(cursor, "first line\n".len());

        delete_to_line_end(&mut input, &mut cursor);
        assert_eq!(input, "first line\n\nlast");

        cursor = input.len();
        delete_to_line_start(&mut input, &mut cursor);
        assert_eq!(input, "first line\n\n");
        assert_eq!(cursor, "first line\n\n".len());
    }

    #[test]
    fn manual_bash_context_caps_stdout_and_stderr_like_vibe() {
        let result = ManualBashResult {
            stdout: "abcdefghij".to_string(),
            stderr: "1234567890".to_string(),
            exit_code: 1,
            status: None,
        };

        let context = manual_bash_context("demo", Path::new("/tmp/workspace"), &result, 5);

        assert!(context.contains("Stdout:\n```text\nabcde\n... [truncated]\n```"));
        assert!(context.contains("Stderr:\n```text\n12345\n... [truncated]\n```"));
        assert!(!context.contains("abcdefghij"));
        assert!(!context.contains("1234567890"));
    }

    #[test]
    fn manual_bash_context_leaves_short_output_unchanged() {
        let result = ManualBashResult {
            stdout: "short\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            status: None,
        };

        let context = manual_bash_context("echo short", Path::new("/tmp/workspace"), &result, 10);

        assert!(context.contains("Stdout:\n```text\nshort\n```"));
        assert!(!context.contains("[truncated]"));
    }

    #[test]
    fn queued_input_lines_keep_header_until_queue_drains() {
        let mut transcript = vec!["start".to_string(), String::new()];
        let mut queue = VecDeque::new();

        append_queued_input_lines(&mut transcript, &mut queue, "first".to_string());
        append_queued_input_lines(&mut transcript, &mut queue, "second line".to_string());

        assert_eq!(queue.len(), 2);
        assert_eq!(
            transcript.iter().filter(|line| *line == "» Queued").count(),
            1
        );
        assert!(transcript.contains(&"> first".to_string()));
        assert!(transcript.contains(&"> second line".to_string()));

        remove_queued_input_lines(&mut transcript, "first", false);
        assert!(transcript.contains(&"» Queued".to_string()));
        assert!(!transcript.contains(&"> first".to_string()));
        assert!(transcript.contains(&"> second line".to_string()));

        remove_queued_input_lines(&mut transcript, "second line", true);
        assert!(!transcript.contains(&"» Queued".to_string()));
        assert!(!transcript.contains(&"> second line".to_string()));
    }

    #[test]
    fn input_cursor_position_tracks_prompt_text() {
        let area = Rect::new(10, 20, 80, 6);
        let position = input_cursor_position(area, None, "abc", 2);
        assert_eq!(position, Position::new(14, 21));

        let multiline = input_cursor_position(area, None, "abc\nde", "abc\nd".len());
        assert_eq!(multiline, Position::new(13, 22));
    }

    #[test]
    fn osc52_sequence_matches_vibe_plain_and_tmux_shapes() {
        assert_eq!(
            osc52_sequence_with_tmux("hello world", false),
            "\x1b]52;c;aGVsbG8gd29ybGQ=\x07"
        );
        assert_eq!(
            osc52_sequence_with_tmux("test text", true),
            "\x1bPtmux;\x1b\x1b]52;c;dGVzdCB0ZXh0\x07\x1b\\"
        );
    }

    #[test]
    fn command_exists_in_path_requires_executable_file() {
        let dir = tempfile::tempdir().unwrap();
        let command = dir.path().join("pbcopy");
        fs::write(&command, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&command, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(!command_exists_in_path("pbcopy", dir.path().as_os_str()));
            fs::set_permissions(&command, fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(command_exists_in_path("pbcopy", dir.path().as_os_str()));
        assert!(!command_exists_in_path("xclip", dir.path().as_os_str()));
    }

    #[test]
    fn clipboard_writer_sends_stdin_to_command() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("copy.sh");
        let store = dir.path().join("clipboard.txt");
        fs::write(&script, "#!/bin/sh\ncat > \"$1\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let command = script.to_string_lossy().into_owned();
        let output = store.to_string_lossy().into_owned();
        run_clipboard_writer(&command, &[output.as_str()], "assistant text").unwrap();
        assert_eq!(fs::read_to_string(store).unwrap(), "assistant text");
    }
}

fn approval_title(call: &ToolCall) -> String {
    if call.name == "bash" {
        let command = tool_call_arg(call, "command");
        let pattern = command
            .split_once(char::is_whitespace)
            .map(|(head, _)| format!("{head} *"))
            .unwrap_or(command);
        return format!("Permission for the bash tool ({pattern})");
    }
    if call.name == "web_fetch" {
        return format!(
            "Permission for the web_fetch tool (fetching from {})",
            display_url_domain(&tool_call_arg(call, "url"))
        );
    }
    format!("Permission for the {} tool", call.name)
}

fn approval_rows(call: &ToolCall) -> Vec<String> {
    let mut rows = Vec::new();
    if call.name == "write_file" {
        rows.push(format!(
            "File: {}",
            display_tool_path(&tool_call_arg(call, "path"))
        ));
        rows.push(String::new());
        rows.extend(
            tool_call_arg(call, "content")
                .lines()
                .map(ToString::to_string),
        );
        rows.push(String::new());
    } else if call.name == "edit" {
        rows.push(format!(
            "File: {}",
            display_tool_path(&tool_call_arg(call, "file_path"))
        ));
        rows.push(String::new());
        rows.extend(edit_diff_preview_rows(call));
        rows.push(String::new());
    } else if call.name == "web_fetch" {
        rows.push(format!("url: {}", tool_call_arg(call, "url")));
        rows.push(String::new());
    } else if call.name == "web_search" {
        rows.push(format!("query: {}", tool_call_arg(call, "query")));
        rows.push(String::new());
    } else if call.name == "task" {
        rows.push(format!("task: {}", tool_call_arg(call, "task")));
        rows.push(format!("agent: {}", tool_call_arg(call, "agent")));
        rows.push(String::new());
    } else {
        rows.push(tool_call_arg(call, "command"));
        rows.push(String::new());
    }
    rows.extend(strings(&[
        "› 1. Allow once",
        "",
        "  2. Allow for remainder of this session",
        "",
        "  3. Always allow",
        "",
        "  4. Deny",
        "",
    ]));
    rows
}

fn mark_pending_tool_selected(transcript: &mut [String]) {
    if let Some(line) = transcript.iter_mut().rev().find(|line| {
        line.starts_with("□ bash:")
            || line.starts_with("□ Writing ")
            || line.starts_with("□ Editing ")
            || line.starts_with("□ Fetching: ")
            || line.starts_with("□ Searching the web: ")
            || line.starts_with("□ Running ")
    }) {
        *line = line.replacen('□', "■", 1);
    }
}

fn remove_pending_tool_block(transcript: &mut Vec<String>, prefixes: &[&str]) {
    let Some(index) = transcript
        .iter()
        .rposition(|line| prefixes.iter().any(|prefix| line.starts_with(prefix)))
    else {
        return;
    };
    let mut end = index + 1;
    if transcript.get(end).is_some_and(String::is_empty) {
        end += 1;
    }
    if transcript.get(end).is_some_and(|line| {
        line.contains("Running command…")
            || line.contains("Writing file…")
            || line.contains("Editing files…")
            || line.contains("Fetching URL…")
            || line.contains("Searching the web…")
            || line.contains("Running subagent…")
            || line.contains("Waiting for user input…")
            || line.contains("Waiting for user confirmation…")
    }) {
        end += 1;
    }
    transcript.drain(index..end);
}

fn display_tool_path(path: &str) -> String {
    path.strip_prefix("/private/var/")
        .map(|stripped| format!("/var/{stripped}"))
        .unwrap_or_else(|| path.to_string())
}

fn display_tool_filename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
        .to_string()
}

fn display_url_domain(url: &str) -> String {
    let without_scheme = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .trim_start_matches('/');
    without_scheme
        .split('/')
        .next()
        .unwrap_or(without_scheme)
        .to_string()
}

fn edit_diff_preview_rows(call: &ToolCall) -> Vec<String> {
    let path = tool_call_arg(call, "file_path");
    let old = tool_call_arg(call, "old_string");
    let new = tool_call_arg(call, "new_string");
    let line_number = std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| {
            content
                .lines()
                .position(|line| line.contains(&old))
                .map(|idx| idx + 1)
        })
        .unwrap_or(0);
    if line_number == 0 {
        return vec![format!("-{old}"), format!("+{new}")];
    }
    vec![
        format!("{line_number:>4} - {old}"),
        format!("{line_number:>4} + {new}"),
    ]
}

fn tool_result_display_lines(result: &ToolResult, collapsed: bool) -> Vec<String> {
    if result.name == "read" && result.success {
        let file_path = result
            .output
            .lines()
            .find_map(|line| line.strip_prefix("file_path: "))
            .unwrap_or_default();
        let filename = std::path::Path::new(file_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(file_path);
        let lines = result
            .output
            .lines()
            .find_map(|line| line.strip_prefix("num_lines: "))
            .or_else(|| {
                result
                    .output
                    .lines()
                    .find_map(|line| line.strip_prefix("total_lines: "))
            })
            .unwrap_or("0");
        if !collapsed {
            let content = result
                .output
                .split_once("\ncontent: ")
                .and_then(|(_, rest)| rest.split_once("\nnum_lines: "))
                .map(|(content, _)| content)
                .unwrap_or_default();
            return expanded_text_tool_lines(
                format!("✓ Read from {filename}"),
                content.lines().map(strip_read_line_number).collect(),
            );
        }
        return vec![
            format!("✓ Read from {filename}"),
            format!("  ⎣ ▶ {lines} lines"),
            String::new(),
        ];
    }
    if result.name == "bash" && result.success {
        let command = result
            .output
            .lines()
            .find_map(|line| line.strip_prefix("command: "))
            .unwrap_or_default();
        let stdout = result
            .output
            .split_once("\nstdout: ")
            .and_then(|(_, rest)| rest.split_once("\nstderr: "))
            .map(|(stdout, _)| stdout)
            .unwrap_or_default();
        let line_count = stdout.lines().count().max(usize::from(!stdout.is_empty()));
        let line_noun = if line_count == 1 { "line" } else { "lines" };
        if !collapsed {
            let mut lines = vec![format!("✓ Ran {command}")];
            let stdout_lines = stdout.lines().collect::<Vec<_>>();
            if stdout_lines.is_empty() {
                lines.push("  ⎣ ▼ show less".to_string());
            } else {
                for line in stdout_lines {
                    lines.push(format!("  ⎢ {line}"));
                }
                lines.push("  ⎣ ▼ show less".to_string());
            }
            return lines;
        }
        return vec![
            format!("✓ Ran {command}"),
            format!("  ⎣ ▶ {line_count} {line_noun}"),
        ];
    }
    if !result.success && result.output == "Skipped: User cancelled the operation." {
        return vec![
            format!("✕ {}: skipped", result.name),
            format!("  ⎣ {}", result.output),
            String::new(),
        ];
    }
    if result.name == "write_file" && result.success {
        let path = result
            .output
            .lines()
            .find_map(|line| line.strip_prefix("path: "))
            .unwrap_or_default();
        let bytes_written = result
            .output
            .lines()
            .find_map(|line| line.strip_prefix("bytes_written: "))
            .unwrap_or_default();
        let content_prefix = format!("path: {path}\nbytes_written: {bytes_written}\ncontent: ");
        let content = result
            .output
            .strip_prefix(&content_prefix)
            .unwrap_or_default();
        let mut lines = vec![format!("✓ Created {}", display_tool_filename(path))];
        let content_lines = content.lines().collect::<Vec<_>>();
        if content_lines.is_empty() {
            lines.push("  ⎣".to_string());
        } else {
            for (idx, line) in content_lines.iter().enumerate() {
                let marker = if idx + 1 == content_lines.len() {
                    "⎣"
                } else {
                    "⎢"
                };
                lines.push(format!("  {marker} {line}"));
            }
        }
        return lines;
    }
    if result.name == "edit" && result.success {
        let file = result
            .output
            .lines()
            .find_map(|line| line.strip_prefix("file: "))
            .unwrap_or_default();
        let old = result
            .output
            .lines()
            .find_map(|line| line.strip_prefix("old_string: "))
            .unwrap_or_default();
        let new = result
            .output
            .lines()
            .find_map(|line| line.strip_prefix("new_string: "))
            .unwrap_or_default();
        let line_number = std::fs::read_to_string(file)
            .ok()
            .and_then(|content| {
                content
                    .lines()
                    .position(|line| line.contains(new))
                    .map(|idx| idx + 1)
            })
            .unwrap_or(0);
        let mut lines = vec![format!("✓ Edited {}", display_tool_filename(file))];
        if line_number == 0 {
            lines.push(format!("  ⎢ - {old}"));
            lines.push(format!("  ⎣ + {new}"));
        } else {
            lines.push(format!("  ⎢ {line_number:>4} - {old}"));
            lines.push(format!("  ⎣ {line_number:>4} + {new}"));
        }
        return lines;
    }
    if result.name == "grep" && result.success {
        let count = result
            .output
            .lines()
            .find_map(|line| line.strip_prefix("match_count: "))
            .unwrap_or("0");
        let line_noun = if count == "1" { "line" } else { "lines" };
        if !collapsed {
            let matches = result
                .output
                .split_once("\nmatches: ")
                .map(|(_, rest)| rest)
                .or_else(|| result.output.strip_prefix("matches: "))
                .and_then(|rest| rest.split_once("\nmatch_count: "))
                .map(|(matches, _)| matches)
                .unwrap_or_default();
            return expanded_text_tool_lines(
                format!("✓ Found {count} matches"),
                matches.lines().map(normalize_tool_display_path).collect(),
            );
        }
        return vec![
            format!("✓ Found {count} matches"),
            format!("  ⎣ ▶ {count} {line_noun}"),
            String::new(),
        ];
    }
    if result.name == "todo" && result.success {
        let message = result
            .output
            .lines()
            .find_map(|line| line.strip_prefix("message: "))
            .unwrap_or("Success");
        let todos = todo_items_from_output(&result.output);
        let mut lines = vec![format!("✓ {message}")];
        if todos.is_empty() {
            lines.push("  ⎣ No todos".to_string());
            lines.push(String::new());
            return lines;
        }
        let ordered_statuses = ["in_progress", "pending", "completed", "cancelled"];
        let ordered_todos = ordered_statuses
            .iter()
            .flat_map(|status| todos.iter().filter(move |todo| todo.status == *status))
            .collect::<Vec<_>>();
        for (idx, todo) in ordered_todos.iter().enumerate() {
            let marker = if idx + 1 == ordered_todos.len() {
                "⎣"
            } else {
                "⎢"
            };
            lines.push(format!(
                "  {marker} {} {}",
                todo_status_icon(&todo.status),
                todo.content
            ));
        }
        lines.push(String::new());
        return lines;
    }
    if result.name == "skill" && result.success {
        let name = result
            .output
            .lines()
            .find_map(|line| line.strip_prefix("name: "))
            .unwrap_or("skill");
        let line_count = result
            .output
            .strip_suffix('\n')
            .unwrap_or(&result.output)
            .lines()
            .count();
        if !collapsed {
            return expanded_text_tool_lines(
                format!("✓ Loaded skill: {name}"),
                skill_output_lines(&result.output),
            );
        }
        return vec![
            format!("✓ Loaded skill: {name}"),
            format!("  ⎣ ▶ {line_count} lines"),
            String::new(),
        ];
    }
    if result.name == "web_fetch" && result.success {
        let url = result
            .output
            .lines()
            .find_map(|line| line.strip_prefix("url: "))
            .unwrap_or_default();
        let content = result
            .output
            .split_once("\ncontent: ")
            .and_then(|(_, rest)| rest.split_once("\ncontent_type: "))
            .map(|(content, _)| content)
            .unwrap_or_default();
        let content_type = result
            .output
            .lines()
            .find_map(|line| line.strip_prefix("content_type: "))
            .unwrap_or("text/plain")
            .split_once(';')
            .map(|(head, _)| head)
            .unwrap_or("text/plain");
        let was_truncated = result.output.contains("was_truncated: True");
        let suffix = if was_truncated { " [truncated]" } else { "" };
        let line_count = result
            .output
            .strip_suffix('\n')
            .unwrap_or(&result.output)
            .lines()
            .count();
        if !collapsed {
            return expanded_text_tool_lines(
                format!(
                    "✓ Fetched {url} ({} chars, {content_type}){suffix}",
                    content.chars().count()
                ),
                generic_output_lines(&result.output),
            );
        }
        return vec![
            format!(
                "✓ Fetched {url} ({} chars, {content_type}){suffix}",
                content.chars().count()
            ),
            format!("  ⎣ ▶ {line_count} lines"),
            String::new(),
        ];
    }
    if result.name == "web_search" && result.success {
        let query = result
            .output
            .lines()
            .find_map(|line| line.strip_prefix("query: "))
            .unwrap_or_default();
        let source_count = websearch_source_count(&result.output);
        let source_noun = if source_count == 1 {
            "source"
        } else {
            "sources"
        };
        let line_count = result
            .output
            .strip_suffix('\n')
            .unwrap_or(&result.output)
            .lines()
            .count();
        if !collapsed {
            return expanded_text_tool_lines(
                format!("✓ Searched '{query}' ({source_count} {source_noun})"),
                generic_output_lines(&result.output)
                    .into_iter()
                    .flat_map(format_websearch_sources_lines)
                    .collect(),
            );
        }
        return vec![
            format!("✓ Searched '{query}' ({source_count} {source_noun})"),
            format!("  ⎣ ▶ {line_count} lines"),
            String::new(),
        ];
    }
    if result.name == "ask_user_question" && result.success {
        let answer_count = result.output.matches("'question':").count();
        if answer_count > 1 {
            return vec![
                format!("✓ {answer_count} answers received"),
                format!("  ⎣ ▶ {} lines", answer_count * 2),
                String::new(),
            ];
        }
        let answer = question_answer_from_output(&result.output).unwrap_or("Questions answered");
        let answer = if question_output_is_other(&result.output) {
            format!("(Other) {answer}")
        } else {
            answer.to_string()
        };
        if !collapsed {
            return expanded_text_tool_lines(format!("✓ {answer}"), vec![answer]);
        }
        return vec![
            format!("✓ {}", answer),
            "  ⎣ ▶ 1 line".to_string(),
            String::new(),
        ];
    }
    if result.name == "task" && !result.output.starts_with("<tool_error>") {
        let turns_used = result
            .output
            .lines()
            .find_map(|line| line.strip_prefix("turns_used: "))
            .unwrap_or("0");
        let completed = result.output.contains("completed: True");
        let turn_word = if turns_used == "1" { "turn" } else { "turns" };
        let message = if completed {
            format!("Agent completed in {turns_used} {turn_word}")
        } else {
            format!("Agent interrupted after {turns_used} {turn_word}")
        };
        let mark = if completed { "✓" } else { "✕" };
        let line_count = result
            .output
            .strip_suffix('\n')
            .unwrap_or(&result.output)
            .lines()
            .count();
        if !collapsed {
            return expanded_text_tool_lines(
                format!("{mark} {message}"),
                generic_output_lines(&result.output),
            );
        }
        return vec![
            format!("{mark} {message}"),
            format!("  ⎣ ▶ {line_count} lines"),
            String::new(),
        ];
    }
    if result.name == "task" && result.output.starts_with("<tool_error>") {
        let line_count = result.output.lines().count().max(1);
        let line_noun = if line_count == 1 { "line" } else { "lines" };
        if !collapsed {
            return expanded_text_tool_lines(
                "✕ task: error".to_string(),
                generic_output_lines(&result.output),
            );
        }
        return vec![
            "✕ task: error".to_string(),
            format!("  ⎣ ▶ {line_count} {line_noun}"),
            String::new(),
        ];
    }
    if result.name == "exit_plan_mode" {
        let mark = if result.success { "✓" } else { "✕" };
        let message = result
            .output
            .lines()
            .find_map(|line| line.strip_prefix("message: "))
            .unwrap_or(&result.output);
        return vec![
            format!("{mark} {message}"),
            "  ⎣ ▶ 2 lines".to_string(),
            String::new(),
            format!("┌{}┐", "─".repeat(118)),
            format!("└{}┘", "─".repeat(118)),
            String::new(),
        ];
    }
    if result.success {
        return vec![format!("✓ {}", result.name), String::new()];
    }
    vec![
        format!("✕ {}", result.name),
        format!("  ⎣ Error: {}", result.output),
        String::new(),
    ]
}

fn question_answer_from_output(output: &str) -> Option<&str> {
    let marker = "'answer': '";
    let rest = output.split_once(marker)?.1;
    rest.split_once('\'').map(|(answer, _)| answer)
}

fn expanded_text_tool_lines(header: String, body_lines: Vec<String>) -> Vec<String> {
    let mut lines = vec![header];
    for line in body_lines {
        if line.is_empty() {
            lines.push("  ⎢".to_string());
            continue;
        }
        for chunk in wrap_tool_body_line(&line) {
            lines.push(format!("  ⎢ {chunk}"));
        }
    }
    lines.push("  ⎣ ▼ show less".to_string());
    lines.push(String::new());
    lines
}

fn wrap_tool_body_line(line: &str) -> Vec<String> {
    const MAX_CONTENT_WIDTH: usize = 113;
    let chars = line.chars().collect::<Vec<_>>();
    if chars.len() <= MAX_CONTENT_WIDTH {
        return vec![line.to_string()];
    }
    chars
        .chunks(MAX_CONTENT_WIDTH)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

fn strip_read_line_number(line: &str) -> String {
    line.split_once('→')
        .map(|(_, content)| content)
        .unwrap_or(line)
        .to_string()
}

fn normalize_tool_display_path(line: &str) -> String {
    line.replace("/private/var/", "/var/")
}

fn generic_output_lines(output: &str) -> Vec<String> {
    output
        .strip_suffix('\n')
        .unwrap_or(output)
        .lines()
        .map(normalize_skill_home_display)
        .collect()
}

fn skill_output_lines(output: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for line in generic_output_lines(output) {
        if let Some(path) = line.strip_prefix("Base directory for this skill: ") {
            lines.push("Base directory for this skill:".to_string());
            lines.push(path.to_string());
        } else if let Some(path) = line.strip_prefix("skill_dir: ") {
            lines.push("skill_dir:".to_string());
            lines.push(path.to_string());
        } else {
            lines.push(line);
        }
    }
    lines
}

fn normalize_skill_home_display(line: &str) -> String {
    line.replace("/microvibe/home/.vibe/skills/", "/vibe/home/.vibe/skills/")
}

fn format_websearch_sources_lines(line: String) -> Vec<String> {
    let Some(raw) = line.strip_prefix("sources: ") else {
        return vec![line];
    };
    let mut sources = Vec::new();
    for part in raw.split("{'title': ").skip(1) {
        let Some((title_part, rest)) = part.split_once(", 'url': ") else {
            continue;
        };
        let title = title_part.trim_matches(|ch| ch == '\'' || ch == ' ');
        let url = rest
            .split_once('}')
            .map(|(url, _)| url)
            .unwrap_or(rest)
            .trim_matches(|ch| ch == '\'' || ch == ' ');
        sources.push(format!("WebSearchSource(title='{title}', url='{url}')"));
    }
    if sources.is_empty() {
        vec![line]
    } else if sources.len() == 2 {
        vec![
            format!(
                "sources: [{}, {},",
                sources[0],
                source_without_url(&sources[1])
            ),
            format!("url='{}')]", source_url(&sources[1])),
        ]
    } else {
        vec![format!("sources: [{}]", sources.join(", "))]
    }
}

fn source_without_url(source: &str) -> String {
    source
        .split_once(", url=")
        .map(|(head, _)| head.to_string())
        .unwrap_or_else(|| source.to_string())
}

fn source_url(source: &str) -> String {
    source
        .split_once("url='")
        .and_then(|(_, rest)| rest.split_once("'"))
        .map(|(url, _)| url.to_string())
        .unwrap_or_default()
}

fn question_output_is_other(output: &str) -> bool {
    output.contains("'is_other': True")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TodoDisplayItem {
    content: String,
    status: String,
}

fn todo_items_from_output(output: &str) -> Vec<TodoDisplayItem> {
    let Some(todos) = output.lines().find_map(|line| line.strip_prefix("todos: ")) else {
        return Vec::new();
    };
    todos
        .split("}, ")
        .filter_map(|raw| {
            let item = raw
                .trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim_start_matches('{')
                .trim_end_matches('}');
            let content = python_repr_field(item, "'content': ")?;
            let status = todo_enum_value(item, "'status': ")?;
            Some(TodoDisplayItem { content, status })
        })
        .collect()
}

fn python_repr_field(item: &str, marker: &str) -> Option<String> {
    let rest = item.split_once(marker)?.1;
    let quoted = rest.strip_prefix('\'')?;
    let mut value = String::new();
    let mut escaped = false;
    for ch in quoted.chars() {
        if escaped {
            value.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '\'' {
            return Some(value);
        } else {
            value.push(ch);
        }
    }
    None
}

fn todo_enum_value(item: &str, marker: &str) -> Option<String> {
    let rest = item.split_once(marker)?.1;
    let enum_body = rest.strip_prefix('<')?.split_once('>')?.0;
    let (_, quoted) = enum_body.split_once(": ")?;
    Some(quoted.trim_matches('\'').to_string())
}

fn todo_status_icon(status: &str) -> &'static str {
    match status {
        "completed" => "☑",
        "cancelled" => "☒",
        _ => "☐",
    }
}

fn websearch_source_count(output: &str) -> usize {
    output
        .lines()
        .find_map(|line| line.strip_prefix("sources: "))
        .map(|sources| sources.matches("'url':").count())
        .unwrap_or(0)
}
