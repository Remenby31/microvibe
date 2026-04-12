use crate::pricing;
use crate::session::SessionStats;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

// ── Spinner frames ──
const SPINNER_BRAILLE: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_PULSE: &[&str] = &["■", "■", "■", "■", "□", "□", "□", "□"];

/// A single entry in the chat history for display
#[derive(Clone)]
pub enum ChatEntry {
    User(String),
    Assistant(String),
    ToolCall { name: String, detail: String, spinning: bool },
    ToolResult { summary: String },
    Thinking { text: String, spinning: bool },
    System(String),
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
    spinner_tick: usize,
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
            spinner_tick: 0,
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

    /// Start a new empty assistant entry for streaming
    pub fn start_assistant_entry(&mut self) {
        self.entries.push(ChatEntry::Assistant(String::new()));
        self.scroll_to_bottom();
    }

    /// Append text to the current assistant entry (streaming)
    pub fn append_assistant_text(&mut self, text: &str) {
        if let Some(ChatEntry::Assistant(ref mut content)) = self.entries.last_mut() {
            content.push_str(text);
            self.scroll_to_bottom();
        }
    }

    /// Append text to the current thinking entry
    pub fn append_thinking_text(&mut self, new_text: &str) {
        if let Some(ChatEntry::Thinking { ref mut text, .. }) = self.entries.last_mut() {
            text.push_str(new_text);
        }
    }

    /// Mark the last tool call as done
    pub fn finish_last_tool(&mut self, success: bool) {
        for entry in self.entries.iter_mut().rev() {
            if let ChatEntry::ToolCall { spinning, .. } = entry {
                *spinning = false;
                break;
            }
        }
        let _ = success;
    }

    /// Mark thinking as done
    pub fn finish_thinking(&mut self) {
        for entry in self.entries.iter_mut().rev() {
            if let ChatEntry::Thinking { spinning, .. } = entry {
                *spinning = false;
                break;
            }
        }
    }

    fn scroll_to_bottom(&mut self) {
        self.scroll = u16::MAX;
    }

    /// Render the full TUI frame
    pub fn render(&mut self, f: &mut ratatui::Frame) {
        self.spinner_tick = self.spinner_tick.wrapping_add(1);
        let size = f.area();

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),   // chat area
                Constraint::Length(1), // status bar
                Constraint::Length(3), // input box
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
                        Span::styled(
                            "  ❯ ",
                            Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(text.as_str(), Style::default().fg(Color::White)),
                    ]));
                }
                ChatEntry::Assistant(text) => {
                    lines.push(Line::from(""));
                    if text.is_empty() && self.waiting {
                        let frame = SPINNER_BRAILLE[self.spinner_tick / 2 % SPINNER_BRAILLE.len()];
                        lines.push(Line::from(Span::styled(
                            format!("  {} thinking...", frame),
                            Style::default().fg(Color::Cyan),
                        )));
                    } else {
                        for line in text.lines() {
                            lines.push(render_md_line(line));
                        }
                    }
                }
                ChatEntry::ToolCall { name, detail, spinning } => {
                    let icon = tool_icon(name);
                    let status = if *spinning {
                        let frame = SPINNER_PULSE[self.spinner_tick / 3 % SPINNER_PULSE.len()];
                        Span::styled(
                            format!(" {}", frame),
                            Style::default().fg(Color::Cyan),
                        )
                    } else {
                        Span::styled(" ✓", Style::default().fg(Color::Green))
                    };
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {} {}", icon, name),
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!(" {}", detail.chars().take(55).collect::<String>()),
                            Style::default().fg(Color::DarkGray),
                        ),
                        status,
                    ]));
                }
                ChatEntry::ToolResult { summary } => {
                    lines.push(Line::from(vec![
                        Span::styled("    → ", Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            summary.chars().take(75).collect::<String>(),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]));
                }
                ChatEntry::Thinking { text, spinning } => {
                    let indicator = if *spinning {
                        let frame = SPINNER_PULSE[self.spinner_tick / 3 % SPINNER_PULSE.len()];
                        format!("{} Thinking", frame)
                    } else {
                        "▶ Thought".to_string()
                    };
                    lines.push(Line::from(Span::styled(
                        format!("  {}", indicator),
                        Style::default().fg(Color::Magenta),
                    )));
                    if !text.is_empty() && !spinning {
                        // Show collapsed — just first line
                        let preview: String = text.lines().next().unwrap_or("").chars().take(60).collect();
                        lines.push(Line::from(Span::styled(
                            format!("    {}", preview),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }
                ChatEntry::System(text) => {
                    lines.push(Line::from(Span::styled(
                        format!("  {}", text),
                        Style::default().fg(Color::Yellow),
                    )));
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

        // Context progress
        let total = self.stats.prompt_tokens + self.stats.completion_tokens;
        let max_ctx = 128000u64; // rough
        let pct = if max_ctx > 0 {
            ((self.stats.prompt_tokens as f64 / max_ctx as f64) * 100.0).min(100.0)
        } else {
            0.0
        };

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
                format!("{:.0}% ctx ", pct),
                Style::default().fg(if pct > 80.0 {
                    Color::Red
                } else if pct > 50.0 {
                    Color::Yellow
                } else {
                    Color::DarkGray
                }),
            ),
            Span::styled(
                format!("{}tok ", total),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("${:.4} ", cost),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled("│ ", Style::default().fg(Color::DarkGray)),
            if self.waiting {
                let frame = SPINNER_BRAILLE[self.spinner_tick / 2 % SPINNER_BRAILLE.len()];
                Span::styled(
                    format!("{} working ", frame),
                    Style::default().fg(Color::Cyan),
                )
            } else {
                Span::styled("ready ", Style::default().fg(Color::Green))
            },
        ]);

        let bar =
            Paragraph::new(status).style(Style::default().bg(Color::Rgb(25, 25, 25)));
        f.render_widget(bar, area);
    }

    fn render_input(&self, f: &mut ratatui::Frame, area: Rect) {
        let prompt = if self.waiting { "⠋" } else { ">" };
        let display_text = if self.input.is_empty() && !self.waiting {
            format!("{} Type a message...", prompt)
        } else {
            format!("{} {}", prompt, &self.input)
        };

        let input_style = if self.input.is_empty() && !self.waiting {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::White)
        };

        let border_color = if self.waiting {
            Color::DarkGray
        } else {
            Color::Cyan
        };

        let input = Paragraph::new(display_text)
            .style(input_style)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color))
                    .title(Span::styled(
                        " microvibe ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )),
            );
        f.render_widget(input, area);

        // Show cursor
        if !self.waiting {
            f.set_cursor_position((
                area.x + self.cursor_pos as u16 + 3, // +3 for border + "> "
                area.y + 1,
            ));
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                return Some("/quit".to_string());
            }
            (KeyModifiers::SHIFT, KeyCode::Enter) => {
                // Multiline: insert newline
                self.input.insert(self.cursor_pos, '\n');
                self.cursor_pos += 1;
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
            (_, KeyCode::Home) => self.cursor_pos = 0,
            (_, KeyCode::End) => self.cursor_pos = self.input.len(),
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

/// Render a markdown line to a ratatui Line
fn render_md_line(text: &str) -> Line<'static> {
    let owned = text.to_string();

    if owned.starts_with("### ") {
        return Line::from(Span::styled(
            format!("  {}", &owned[4..]),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }
    if owned.starts_with("## ") {
        return Line::from(Span::styled(
            format!("  {}", &owned[3..]),
            Style::default()
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ));
    }
    if owned.starts_with("# ") {
        return Line::from(Span::styled(
            format!("  {}", &owned[2..]),
            Style::default()
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ));
    }

    if owned.starts_with("- ") || owned.starts_with("* ") {
        return Line::from(vec![
            Span::styled("  • ", Style::default().fg(Color::Cyan)),
            Span::raw(owned[2..].to_string()),
        ]);
    }

    if owned.starts_with("```") {
        let lang = owned[3..].trim();
        let label = if lang.is_empty() { "code" } else { lang };
        return Line::from(Span::styled(
            format!("  ─── {} ───", label),
            Style::default().fg(Color::DarkGray),
        ));
    }

    if owned.starts_with("> ") {
        return Line::from(vec![
            Span::styled("  ▎ ", Style::default().fg(Color::Cyan)),
            Span::styled(owned[2..].to_string(), Style::default().fg(Color::DarkGray)),
        ]);
    }

    // Inline formatting: **bold** and `code`
    let formatted = render_inline_spans(&owned);
    Line::from(formatted)
}

/// Parse inline markdown into styled spans
fn render_inline_spans(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    // Prepend indent
    current.push_str("  ");

    while i < chars.len() {
        // **bold**
        if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(end) = find_pattern(&chars, i + 2, "**") {
                if !current.is_empty() {
                    spans.push(Span::raw(std::mem::take(&mut current)));
                }
                let inner: String = chars[i + 2..end].iter().collect();
                spans.push(Span::styled(
                    inner,
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                i = end + 2;
                continue;
            }
        }

        // `code`
        if chars[i] == '`' && (i + 1 >= chars.len() || chars[i + 1] != '`') {
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == '`') {
                if !current.is_empty() {
                    spans.push(Span::raw(std::mem::take(&mut current)));
                }
                let inner: String = chars[i + 1..i + 1 + end].iter().collect();
                spans.push(Span::styled(
                    inner,
                    Style::default()
                        .fg(Color::Cyan)
                        .bg(Color::Rgb(40, 40, 40)),
                ));
                i = i + 1 + end + 1;
                continue;
            }
        }

        current.push(chars[i]);
        i += 1;
    }

    if !current.is_empty() {
        spans.push(Span::raw(current));
    }

    spans
}

fn find_pattern(chars: &[char], start: usize, pattern: &str) -> Option<usize> {
    let pat: Vec<char> = pattern.chars().collect();
    if start + pat.len() > chars.len() {
        return None;
    }
    for i in start..=chars.len() - pat.len() {
        if chars[i..i + pat.len()] == pat[..] {
            return Some(i);
        }
    }
    None
}
