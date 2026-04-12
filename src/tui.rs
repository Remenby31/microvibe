use crate::pricing;
use crate::session::SessionStats;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

const SPINNER_BRAILLE: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_PULSE: &[&str] = &["■", "■", "■", "■", "□", "□", "□", "□"];

const BORDER_V: &str = "⎢";
const BORDER_END: &str = "⎣";

/// A single entry in the chat history
#[derive(Clone)]
pub enum ChatEntry {
    User(String),
    Assistant(String),
    ToolCall { name: String, detail: String, spinning: bool },
    ToolResult { summary: String, detail: Option<String>, collapsed: bool },
    Thinking { text: String, spinning: bool, collapsed: bool },
    System(String),
    Error(String),
    Warning(String),
    Interrupt,
    Approval { tool_name: String, command: String },
}

/// What the TUI returns from handle_key
pub enum KeyAction {
    Submit(String),
    Quit,
    Cancel,            // Ctrl+C during waiting
    ApprovalYes,
    ApprovalNo,
    ApprovalAlways,
    ToggleCollapse,    // toggle last collapsible entry
    None,
}

pub struct TuiApp {
    entries: Vec<ChatEntry>,
    input: String,
    cursor_pos: usize,
    scroll: u16,
    model: String,
    pub provider: String,
    pub stats: SessionStats,
    pub waiting: bool,
    pub approval_pending: bool,
    input_history: Vec<String>,
    input_history_idx: Option<usize>,
    spinner_tick: usize,
    max_context_tokens: usize,
}

impl TuiApp {
    pub fn new(model: &str, provider: &str, max_ctx: usize) -> Self {
        Self {
            entries: Vec::new(),
            input: String::new(),
            cursor_pos: 0,
            scroll: 0,
            model: model.to_string(),
            provider: provider.to_string(),
            stats: SessionStats::default(),
            waiting: false,
            approval_pending: false,
            input_history: Vec::new(),
            input_history_idx: None,
            spinner_tick: 0,
            max_context_tokens: max_ctx,
        }
    }

    pub fn add_entry(&mut self, entry: ChatEntry) {
        self.entries.push(entry);
        self.scroll_to_bottom();
    }

    pub fn clear_entries(&mut self) {
        self.entries.clear();
        self.scroll = 0;
    }

    pub fn start_assistant_entry(&mut self) {
        self.entries.push(ChatEntry::Assistant(String::new()));
        self.scroll_to_bottom();
    }

    pub fn append_assistant_text(&mut self, text: &str) {
        if let Some(ChatEntry::Assistant(ref mut content)) = self.entries.last_mut() {
            content.push_str(text);
            self.scroll_to_bottom();
        }
    }

    pub fn append_thinking_text(&mut self, new_text: &str) {
        if let Some(ChatEntry::Thinking { ref mut text, .. }) = self.entries.last_mut() {
            text.push_str(new_text);
        }
    }

    pub fn finish_last_tool(&mut self, _success: bool) {
        for entry in self.entries.iter_mut().rev() {
            if let ChatEntry::ToolCall { spinning, .. } = entry {
                *spinning = false;
                break;
            }
        }
    }

    pub fn finish_thinking(&mut self) {
        for entry in self.entries.iter_mut().rev() {
            if let ChatEntry::Thinking { spinning, collapsed, .. } = entry {
                *spinning = false;
                *collapsed = true;
                break;
            }
        }
    }

    fn scroll_to_bottom(&mut self) {
        self.scroll = u16::MAX;
    }

    pub fn render(&mut self, f: &mut ratatui::Frame) {
        self.spinner_tick = self.spinner_tick.wrapping_add(1);
        let size = f.area();

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(size);

        self.render_chat(f, chunks[0]);
        self.render_status(f, chunks[1]);
        self.render_input(f, chunks[2]);
    }

    fn render_chat(&mut self, f: &mut ratatui::Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();
        let mut in_code_block = false;
        let mut prev_was_tool = false;

        for entry in &self.entries {
            let is_tool = matches!(entry, ChatEntry::ToolCall { .. } | ChatEntry::ToolResult { .. });

            // No-gap grouping: skip blank line between consecutive tools
            if !is_tool || !prev_was_tool {
                lines.push(Line::from(""));
            }
            prev_was_tool = is_tool;

            match entry {
                ChatEntry::User(text) => {
                    // User message with expanding border
                    for (i, line) in text.lines().enumerate() {
                        let border = if i == text.lines().count() - 1 { BORDER_END } else { BORDER_V };
                        lines.push(Line::from(vec![
                            Span::styled(format!(" {} ", border), Style::default().fg(Color::Blue)),
                            Span::styled(line.to_string(), Style::default().fg(Color::White)),
                        ]));
                    }
                }
                ChatEntry::Assistant(text) => {
                    if text.is_empty() && self.waiting {
                        let frame = SPINNER_BRAILLE[self.spinner_tick / 2 % SPINNER_BRAILLE.len()];
                        lines.push(Line::from(Span::styled(
                            format!("  {} thinking...", frame),
                            Style::default().fg(Color::Cyan),
                        )));
                    } else {
                        in_code_block = false;
                        for line in text.lines() {
                            // Track code block state
                            if line.starts_with("```") {
                                if in_code_block {
                                    in_code_block = false;
                                    lines.push(Line::from(Span::styled(
                                        "  └──────────────────────────────────────────────────┘",
                                        Style::default().fg(Color::DarkGray),
                                    )));
                                } else {
                                    in_code_block = true;
                                    let lang = line[3..].trim();
                                    let label = if lang.is_empty() { "code" } else { lang };
                                    lines.push(Line::from(Span::styled(
                                        format!("  ┌─── {} ───────────────────────────────────────────┐", label),
                                        Style::default().fg(Color::DarkGray),
                                    )));
                                }
                                continue;
                            }

                            if in_code_block {
                                lines.push(Line::from(vec![
                                    Span::styled("  │ ", Style::default().fg(Color::DarkGray)),
                                    Span::styled(
                                        line.to_string(),
                                        Style::default().fg(Color::White).bg(Color::Rgb(30, 30, 30)),
                                    ),
                                ]));
                            } else {
                                lines.push(render_md_line(line));
                            }
                        }
                        // Close unclosed code block
                        if in_code_block {
                            lines.push(Line::from(Span::styled(
                                "  └──────────────────────────────────────────────────┘",
                                Style::default().fg(Color::DarkGray),
                            )));
                            in_code_block = false;
                        }
                    }
                }
                ChatEntry::ToolCall { name, detail, spinning } => {
                    let icon = tool_icon(name);
                    let status = if *spinning {
                        let frame = SPINNER_PULSE[self.spinner_tick / 3 % SPINNER_PULSE.len()];
                        Span::styled(format!(" {}", frame), Style::default().fg(Color::Cyan))
                    } else {
                        Span::styled(" ✓", Style::default().fg(Color::Green))
                    };
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {} {} ", icon, name),
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            detail.chars().take(55).collect::<String>(),
                            Style::default().fg(Color::DarkGray),
                        ),
                        status,
                    ]));
                }
                ChatEntry::ToolResult { summary, detail, collapsed } => {
                    let toggle = if detail.is_some() {
                        if *collapsed { "▶ " } else { "▼ " }
                    } else {
                        ""
                    };
                    lines.push(Line::from(vec![
                        Span::styled(format!("    {toggle}→ "), Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            summary.chars().take(75).collect::<String>(),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]));
                    // Show expanded detail
                    if !collapsed {
                        if let Some(ref det) = detail {
                            for line in det.lines().take(20) {
                                lines.push(Line::from(vec![
                                    Span::styled("    ", Style::default()),
                                    Span::styled(format!("  {}", line), Style::default().fg(Color::DarkGray)),
                                ]));
                            }
                            let total_lines = det.lines().count();
                            if total_lines > 20 {
                                lines.push(Line::from(Span::styled(
                                    format!("      … ({} more lines)", total_lines - 20),
                                    Style::default().fg(Color::DarkGray),
                                )));
                            }
                        }
                    }
                }
                ChatEntry::Thinking { text, spinning, collapsed } => {
                    let indicator = if *spinning {
                        let frame = SPINNER_PULSE[self.spinner_tick / 3 % SPINNER_PULSE.len()];
                        format!("{} Thinking", frame)
                    } else if *collapsed {
                        "▶ Thought".to_string()
                    } else {
                        "▼ Thought".to_string()
                    };
                    lines.push(Line::from(Span::styled(
                        format!("  {}", indicator),
                        Style::default().fg(Color::Magenta),
                    )));
                    if !text.is_empty() && !collapsed {
                        for line in text.lines().take(10) {
                            lines.push(Line::from(Span::styled(
                                format!("    {}", line),
                                Style::default().fg(Color::DarkGray),
                            )));
                        }
                    }
                }
                ChatEntry::System(text) => {
                    lines.push(Line::from(Span::styled(
                        format!("  {}", text),
                        Style::default().fg(Color::Yellow),
                    )));
                }
                ChatEntry::Error(text) => {
                    lines.push(Line::from(vec![
                        Span::styled(format!(" {} ", BORDER_V), Style::default().fg(Color::Red)),
                        Span::styled(format!("Error: {}", text), Style::default().fg(Color::Red)),
                    ]));
                }
                ChatEntry::Warning(text) => {
                    lines.push(Line::from(vec![
                        Span::styled(format!(" {} ", BORDER_V), Style::default().fg(Color::Yellow)),
                        Span::styled(text.to_string(), Style::default().fg(Color::Yellow)),
                    ]));
                }
                ChatEntry::Interrupt => {
                    lines.push(Line::from(vec![
                        Span::styled(format!(" {} ", BORDER_V), Style::default().fg(Color::Yellow)),
                        Span::styled(
                            "Interrupted · What should microvibe do instead?",
                            Style::default().fg(Color::Yellow),
                        ),
                    ]));
                }
                ChatEntry::Approval { tool_name, command } => {
                    lines.push(Line::from(""));
                    lines.push(Line::from(vec![
                        Span::styled("  ⚠ Approve ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                        Span::styled(
                            format!("{}: ", tool_name),
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            command.chars().take(50).collect::<String>(),
                            Style::default().fg(Color::White),
                        ),
                    ]));
                    lines.push(Line::from(Span::styled(
                        "    [y] yes  [n] no  [a] always",
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
        }

        // Clamp scroll
        let content_height = lines.len() as u16;
        let visible_height = area.height;
        let max_scroll = content_height.saturating_sub(visible_height);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }

        let paragraph = Paragraph::new(Text::from(lines))
            .scroll((self.scroll, 0))
            .wrap(Wrap { trim: false });

        f.render_widget(paragraph, area);
    }

    fn render_status(&self, f: &mut ratatui::Frame, area: Rect) {
        let p = pricing::get_pricing(&self.model);
        let cost = self.stats.estimated_cost(p.input, p.output);
        let total = self.stats.prompt_tokens + self.stats.completion_tokens;
        let pct = if self.max_context_tokens > 0 {
            ((self.stats.prompt_tokens as f64 / self.max_context_tokens as f64) * 100.0).min(100.0)
        } else {
            0.0
        };

        let status = Line::from(vec![
            Span::styled(format!(" {} ", self.model), Style::default().fg(Color::Yellow)),
            Span::styled(format!("({}) ", self.provider), Style::default().fg(Color::DarkGray)),
            Span::styled("│ ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:.0}% ", pct),
                Style::default().fg(if pct > 80.0 { Color::Red } else if pct > 50.0 { Color::Yellow } else { Color::DarkGray }),
            ),
            Span::styled(format!("{}tok ", total), Style::default().fg(Color::DarkGray)),
            Span::styled(format!("${:.4} ", cost), Style::default().fg(Color::DarkGray)),
            Span::styled("│ ", Style::default().fg(Color::DarkGray)),
            if self.waiting {
                let frame = SPINNER_BRAILLE[self.spinner_tick / 2 % SPINNER_BRAILLE.len()];
                Span::styled(format!("{} working ", frame), Style::default().fg(Color::Cyan))
            } else {
                Span::styled("ready ", Style::default().fg(Color::Green))
            },
        ]);

        let bar = Paragraph::new(status).style(Style::default().bg(Color::Rgb(25, 25, 25)));
        f.render_widget(bar, area);
    }

    fn render_input(&self, f: &mut ratatui::Frame, area: Rect) {
        // Input mode detection
        let (prompt, prompt_color) = if self.approval_pending {
            ("?", Color::Yellow)
        } else if self.waiting {
            let frame = SPINNER_BRAILLE[self.spinner_tick / 2 % SPINNER_BRAILLE.len()];
            (frame, Color::Cyan)
        } else if self.input.starts_with('/') {
            ("/", Color::Magenta)
        } else if self.input.starts_with('!') {
            ("!", Color::Red)
        } else {
            (">", Color::Cyan)
        };

        let display_text = if self.approval_pending {
            format!("{} [y]es / [n]o / [a]lways", prompt)
        } else if self.input.is_empty() && !self.waiting {
            format!("{} Type a message... (/help for commands, Shift+Enter for newline)", prompt)
        } else {
            format!("{} {}", prompt, &self.input)
        };

        let input_style = if self.input.is_empty() && !self.waiting && !self.approval_pending {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::White)
        };

        let border_color = if self.approval_pending {
            Color::Yellow
        } else if self.waiting {
            Color::DarkGray
        } else {
            prompt_color
        };

        let title = if self.approval_pending {
            " approve? "
        } else {
            " microvibe "
        };

        let input = Paragraph::new(display_text)
            .style(input_style)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color))
                    .title(Span::styled(
                        title,
                        Style::default().fg(border_color).add_modifier(Modifier::BOLD),
                    )),
            );
        f.render_widget(input, area);

        if !self.waiting && !self.approval_pending {
            f.set_cursor_position((
                area.x + self.cursor_pos as u16 + 3,
                area.y + 1,
            ));
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> KeyAction {
        // Approval mode
        if self.approval_pending {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => KeyAction::ApprovalYes,
                KeyCode::Char('n') | KeyCode::Char('N') => KeyAction::ApprovalNo,
                KeyCode::Char('a') | KeyCode::Char('A') => KeyAction::ApprovalAlways,
                KeyCode::Esc => KeyAction::ApprovalNo,
                _ => KeyAction::None,
            };
        }

        match (key.modifiers, key.code) {
            // Ctrl+C: cancel if waiting, quit otherwise
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                if self.waiting {
                    KeyAction::Cancel
                } else {
                    KeyAction::Quit
                }
            }
            // Tab: toggle collapse on last collapsible entry
            (_, KeyCode::Tab) if !self.waiting => {
                self.toggle_last_collapsible();
                KeyAction::None
            }
            (KeyModifiers::SHIFT, KeyCode::Enter) => {
                self.input.insert(self.cursor_pos, '\n');
                self.cursor_pos += 1;
                KeyAction::None
            }
            (_, KeyCode::Enter) => {
                if self.input.is_empty() || self.waiting {
                    return KeyAction::None;
                }
                let submitted = self.input.clone();
                self.input_history.push(submitted.clone());
                self.input_history_idx = None;
                self.input.clear();
                self.cursor_pos = 0;
                KeyAction::Submit(submitted)
            }
            (_, KeyCode::Backspace) => {
                if self.cursor_pos > 0 {
                    self.cursor_pos -= 1;
                    self.input.remove(self.cursor_pos);
                }
                KeyAction::None
            }
            (_, KeyCode::Delete) => {
                if self.cursor_pos < self.input.len() {
                    self.input.remove(self.cursor_pos);
                }
                KeyAction::None
            }
            (_, KeyCode::Left) => {
                if self.cursor_pos > 0 { self.cursor_pos -= 1; }
                KeyAction::None
            }
            (_, KeyCode::Right) => {
                if self.cursor_pos < self.input.len() { self.cursor_pos += 1; }
                KeyAction::None
            }
            (_, KeyCode::Home) => { self.cursor_pos = 0; KeyAction::None }
            (_, KeyCode::End) => { self.cursor_pos = self.input.len(); KeyAction::None }
            (_, KeyCode::Up) => {
                if !self.input_history.is_empty() {
                    let idx = match self.input_history_idx {
                        Some(0) => 0,
                        Some(i) => i - 1,
                        None => self.input_history.len() - 1,
                    };
                    self.input_history_idx = Some(idx);
                    self.input = self.input_history[idx].clone();
                    self.cursor_pos = self.input.len();
                }
                KeyAction::None
            }
            (_, KeyCode::Down) => {
                if let Some(idx) = self.input_history_idx {
                    if idx + 1 < self.input_history.len() {
                        self.input_history_idx = Some(idx + 1);
                        self.input = self.input_history[idx + 1].clone();
                    } else {
                        self.input_history_idx = None;
                        self.input.clear();
                    }
                    self.cursor_pos = self.input.len();
                }
                KeyAction::None
            }
            (_, KeyCode::PageUp) => { self.scroll = self.scroll.saturating_sub(10); KeyAction::None }
            (_, KeyCode::PageDown) => { self.scroll = self.scroll.saturating_add(10); KeyAction::None }
            (_, KeyCode::Esc) => {
                self.input.clear();
                self.cursor_pos = 0;
                KeyAction::None
            }
            (_, KeyCode::Char(c)) => {
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += 1;
                KeyAction::None
            }
            _ => KeyAction::None,
        }
    }

    fn toggle_last_collapsible(&mut self) {
        for entry in self.entries.iter_mut().rev() {
            match entry {
                ChatEntry::ToolResult { collapsed, detail, .. } if detail.is_some() => {
                    *collapsed = !*collapsed;
                    break;
                }
                ChatEntry::Thinking { collapsed, spinning, .. } if !*spinning => {
                    *collapsed = !*collapsed;
                    break;
                }
                _ => continue,
            }
        }
    }
}

fn tool_icon(name: &str) -> &'static str {
    match name {
        "bash" => "⚡",
        "read_file" => "📄",
        "write_file" => "✏️",
        "search_replace" => "🔧",
        "grep" => "🔍",
        "glob" | "list_dir" => "📂",
        "memory_read" | "memory_write" => "🧠",
        _ => "🔧",
    }
}

fn render_md_line(text: &str) -> Line<'static> {
    let owned = text.to_string();

    if owned.starts_with("### ") {
        return Line::from(Span::styled(format!("  {}", &owned[4..]), Style::default().add_modifier(Modifier::BOLD)));
    }
    if owned.starts_with("## ") {
        return Line::from(Span::styled(format!("  {}", &owned[3..]), Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)));
    }
    if owned.starts_with("# ") {
        return Line::from(Span::styled(format!("  {}", &owned[2..]), Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)));
    }
    if owned.starts_with("- ") || owned.starts_with("* ") {
        return Line::from(vec![
            Span::styled("  • ", Style::default().fg(Color::Cyan)),
            Span::raw(owned[2..].to_string()),
        ]);
    }
    if owned.starts_with("  - ") || owned.starts_with("  * ") {
        return Line::from(vec![
            Span::styled("    ◦ ", Style::default().fg(Color::DarkGray)),
            Span::raw(owned[4..].to_string()),
        ]);
    }
    if owned.starts_with("> ") {
        return Line::from(vec![
            Span::styled("  ▎ ", Style::default().fg(Color::Cyan)),
            Span::styled(owned[2..].to_string(), Style::default().fg(Color::DarkGray)),
        ]);
    }
    if owned.trim() == "---" || owned.trim() == "***" {
        return Line::from(Span::styled("  ────────────────────────────────────────", Style::default().fg(Color::DarkGray)));
    }

    render_inline_line(&owned)
}

fn render_inline_line(text: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut current = String::from("  ");
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(end) = find_pat(&chars, i + 2, "**") {
                if !current.is_empty() { spans.push(Span::raw(std::mem::take(&mut current))); }
                let inner: String = chars[i + 2..end].iter().collect();
                spans.push(Span::styled(inner, Style::default().add_modifier(Modifier::BOLD)));
                i = end + 2;
                continue;
            }
        }
        if chars[i] == '`' && (i + 1 >= chars.len() || chars[i + 1] != '`') {
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == '`') {
                if !current.is_empty() { spans.push(Span::raw(std::mem::take(&mut current))); }
                let inner: String = chars[i + 1..i + 1 + end].iter().collect();
                spans.push(Span::styled(inner, Style::default().fg(Color::Cyan).bg(Color::Rgb(40, 40, 40))));
                i = i + 1 + end + 1;
                continue;
            }
        }
        current.push(chars[i]);
        i += 1;
    }
    if !current.is_empty() { spans.push(Span::raw(current)); }
    Line::from(spans)
}

fn find_pat(chars: &[char], start: usize, pattern: &str) -> Option<usize> {
    let pat: Vec<char> = pattern.chars().collect();
    if start + pat.len() > chars.len() { return None; }
    for i in start..=chars.len() - pat.len() {
        if chars[i..i + pat.len()] == pat[..] { return Some(i); }
    }
    None
}
