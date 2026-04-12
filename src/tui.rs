use crate::pricing;
use crate::session::SessionStats;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

/// A single entry in the chat history for display
#[derive(Clone)]
pub enum ChatEntry {
    User(String),
    Assistant(String),
    ToolCall { name: String, detail: String },
    ToolResult { summary: String },
    System(String), // status messages, errors, etc.
}

/// The TUI application state
pub struct TuiApp {
    entries: Vec<ChatEntry>,
    input: String,
    cursor_pos: usize,
    scroll: u16,
    model: String,
    pub provider: String,
    pub stats: SessionStats,
    pub waiting: bool,
    input_history: Vec<String>,
    input_history_idx: Option<usize>,
}

impl TuiApp {
    pub fn new(model: &str, provider: &str) -> Self {
        Self {
            entries: Vec::new(),
            input: String::new(),
            cursor_pos: 0,
            scroll: 0,
            model: model.to_string(),
            provider: provider.to_string(),
            stats: SessionStats::default(),
            waiting: false,
            input_history: Vec::new(),
            input_history_idx: None,
        }
    }

    pub fn add_entry(&mut self, entry: ChatEntry) {
        self.entries.push(entry);
        // Auto-scroll to bottom
        self.scroll_to_bottom();
    }

    fn scroll_to_bottom(&mut self) {
        // Will be clamped during render
        self.scroll = u16::MAX;
    }

    /// Render the full TUI frame
    pub fn render(&mut self, f: &mut ratatui::Frame) {
        let size = f.area();

        // Layout: [chat area] [status bar] [input box]
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),     // chat area
                Constraint::Length(1),   // status bar
                Constraint::Length(3),   // input box
            ])
            .split(size);

        self.render_chat(f, chunks[0]);
        self.render_status(f, chunks[1]);
        self.render_input(f, chunks[2]);
    }

    fn render_chat(&mut self, f: &mut ratatui::Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();

        for entry in &self.entries {
            match entry {
                ChatEntry::User(text) => {
                    lines.push(Line::from(""));
                    lines.push(Line::from(vec![
                        Span::styled("  You ", Style::default().fg(Color::Black).bg(Color::Blue)),
                        Span::raw(" "),
                        Span::styled(text.as_str(), Style::default().fg(Color::White)),
                    ]));
                }
                ChatEntry::Assistant(text) => {
                    lines.push(Line::from(""));
                    lines.push(Line::from(vec![
                        Span::styled("  AI ", Style::default().fg(Color::Black).bg(Color::Green)),
                    ]));
                    // Render markdown-ish text
                    for line in text.lines() {
                        lines.push(render_md_line(line));
                    }
                }
                ChatEntry::ToolCall { name, detail } => {
                    let icon = match name.as_str() {
                        "bash" => "⚡",
                        "read_file" => "📄",
                        "write_file" => "✏️",
                        "search_replace" => "🔧",
                        "grep" => "🔍",
                        "glob" => "📂",
                        "list_dir" => "📁",
                        _ => "🔧",
                    };
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {} {} ", icon, name),
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            detail.chars().take(60).collect::<String>(),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]));
                }
                ChatEntry::ToolResult { summary } => {
                    lines.push(Line::from(vec![
                        Span::styled("  → ", Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            summary.chars().take(80).collect::<String>(),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]));
                }
                ChatEntry::System(text) => {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {}", text),
                            Style::default().fg(Color::Yellow),
                        ),
                    ]));
                }
            }
        }

        // Clamp scroll
        let content_height = lines.len() as u16;
        let visible_height = area.height.saturating_sub(2);
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

        let status = Line::from(vec![
            Span::styled(
                format!(" {} ", self.model),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                format!("({}) ", self.provider),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled("│ ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!(
                    "{}in {}out ",
                    self.stats.prompt_tokens, self.stats.completion_tokens
                ),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("${:.4} ", cost),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled("│ ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} tools ", self.stats.tool_calls),
                Style::default().fg(Color::DarkGray),
            ),
            if self.waiting {
                Span::styled("│ ⠋ thinking ", Style::default().fg(Color::Cyan))
            } else {
                Span::styled("│ ready ", Style::default().fg(Color::Green))
            },
        ]);

        let bar = Paragraph::new(status)
            .style(Style::default().bg(Color::Rgb(30, 30, 30)));
        f.render_widget(bar, area);
    }

    fn render_input(&self, f: &mut ratatui::Frame, area: Rect) {
        let display_text = if self.input.is_empty() && !self.waiting {
            "Type a message... (Ctrl+C to cancel, /help for commands)"
        } else {
            &self.input
        };

        let input_style = if self.input.is_empty() && !self.waiting {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::White)
        };

        let input = Paragraph::new(display_text)
            .style(input_style)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(Span::styled(
                        " microvibe ",
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )),
            );
        f.render_widget(input, area);

        // Show cursor in input area
        if !self.waiting {
            f.set_cursor_position((
                area.x + self.cursor_pos as u16 + 1,
                area.y + 1,
            ));
        }
    }

    /// Handle a key event, returns Some(input) if user submitted
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                if self.waiting {
                    // Cancel current operation
                    return None;
                }
                // Exit
                return Some("/quit".to_string());
            }
            (_, KeyCode::Enter) => {
                if self.input.is_empty() || self.waiting {
                    return None;
                }
                let submitted = self.input.clone();
                self.input_history.push(submitted.clone());
                self.input_history_idx = None;
                self.input.clear();
                self.cursor_pos = 0;
                return Some(submitted);
            }
            (_, KeyCode::Backspace) => {
                if self.cursor_pos > 0 {
                    self.cursor_pos -= 1;
                    self.input.remove(self.cursor_pos);
                }
            }
            (_, KeyCode::Delete) => {
                if self.cursor_pos < self.input.len() {
                    self.input.remove(self.cursor_pos);
                }
            }
            (_, KeyCode::Left) => {
                if self.cursor_pos > 0 {
                    self.cursor_pos -= 1;
                }
            }
            (_, KeyCode::Right) => {
                if self.cursor_pos < self.input.len() {
                    self.cursor_pos += 1;
                }
            }
            (_, KeyCode::Home) => {
                self.cursor_pos = 0;
            }
            (_, KeyCode::End) => {
                self.cursor_pos = self.input.len();
            }
            (_, KeyCode::Up) => {
                // Input history
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
            }
            (_, KeyCode::PageUp) => {
                self.scroll = self.scroll.saturating_sub(10);
            }
            (_, KeyCode::PageDown) => {
                self.scroll = self.scroll.saturating_add(10);
            }
            (_, KeyCode::Char(c)) => {
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += 1;
            }
            _ => {}
        }
        None
    }
}

/// Render a markdown line to a ratatui Line
fn render_md_line(text: &str) -> Line<'static> {
    let owned = text.to_string();

    // Headers
    if owned.starts_with("### ") {
        return Line::from(Span::styled(
            format!("  {}", &owned[4..]),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }
    if owned.starts_with("## ") {
        return Line::from(Span::styled(
            format!("  {}", &owned[3..]),
            Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ));
    }
    if owned.starts_with("# ") {
        return Line::from(Span::styled(
            format!("  {}", &owned[2..]),
            Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ));
    }

    // Bullet points
    if owned.starts_with("- ") || owned.starts_with("* ") {
        return Line::from(vec![
            Span::styled("  • ", Style::default().fg(Color::Cyan)),
            Span::raw(owned[2..].to_string()),
        ]);
    }

    // Code block fences
    if owned.starts_with("```") {
        let lang = owned[3..].trim();
        return Line::from(Span::styled(
            format!("  ─── {} ───", if lang.is_empty() { "code" } else { lang }),
            Style::default().fg(Color::DarkGray),
        ));
    }

    // Blockquote
    if owned.starts_with("> ") {
        return Line::from(vec![
            Span::styled("  ▎ ", Style::default().fg(Color::Cyan)),
            Span::styled(owned[2..].to_string(), Style::default().fg(Color::DarkGray)),
        ]);
    }

    // Regular text with inline formatting
    Line::from(format!("  {}", owned))
}

fn entry_to_plain_text(entry: &ChatEntry) -> String {
    match entry {
        ChatEntry::User(t) => t.clone(),
        ChatEntry::Assistant(t) => t.clone(),
        ChatEntry::ToolCall { name, detail } => format!("{}: {}", name, detail),
        ChatEntry::ToolResult { summary } => summary.clone(),
        ChatEntry::System(t) => t.clone(),
    }
}
