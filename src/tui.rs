//! TUI rendering and input handling for microvibe.
//! Matches Vibe's (Textual) visual style as closely as possible using ratatui.

use crate::pricing;
use crate::session::SessionStats;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

// ── Vibe color palette (from app.tcss) ──

const MISTRAL_ORANGE: Color = Color::Rgb(255, 130, 5);
const ANSI_GREEN: Color = Color::Green;
const ANSI_YELLOW: Color = Color::Yellow;
const ANSI_RED: Color = Color::Red;
const ANSI_CYAN: Color = Color::Cyan;
const ANSI_BRIGHT_BLACK: Color = Color::DarkGray;
const ANSI_DEFAULT: Color = Color::White;
const ANSI_BLUE: Color = Color::Blue;

// ── Spinners ──

const SPINNER_PULSE: &[&str] = &["■", "■", "■", "■", "□", "□", "□", "□"];
const SPINNER_BRAILLE: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// B2: Easter egg loading messages (from Vibe)
const LOADING_MESSAGES: &[&str] = &[
    "Thinking", "Vibing", "Petting le chat", "Eating a chocolatine",
    "Reading Proust", "Contemplation", "Sending good vibes",
    "Counting Rs in strawberry", "Seeding Mistral weights",
    "Wibbling", "Réflexion", "Analyse", "Synthèse",
];

fn format_elapsed(secs: u64) -> String {
    if secs < 60 { return format!("{}s", secs); }
    let (m, s) = (secs / 60, secs % 60);
    if m < 60 { return format!("{}m{}s", m, s); }
    let (h, m) = (m / 60, m % 60);
    format!("{}h{}m{}s", h, m, s)
}

// ── Vibe's petit_chat animation frames ──

const CAT_FRAMES: &[&[&str]] = &[
    &["  ⡠⣒⠄  ⡔⢄⠔⡄", " ⢸⠸⣀⡔⢉⠱⣃⡢⣂⡣", "  ⠉⠒⠣⠤⠵⠤⠬⠮⠆"],
    &["  ⡠⣒⠄  ⡔⢄⠔⡄", " ⢸⠸⣀⡔⢉⠱⣃⡠⣀⡣", "  ⠉⠒⠣⠤⠵⠤⠬⠮⠆"], // blink
    &["  ⡠⣒⠄  ⡔⢄⠔⡄", " ⢸⠸⣀⡔⢉⠱⣃⡢⣂⡣", "  ⠉⠒⠣⠤⠵⠤⠬⠮⠆"],
    &[" ⢠⢢    ⡔⢄⠔⡄", " ⢸⢸⣀⡔⢉⠱⣃⡢⣂⡣", " ⠈⠒⠒⠣⠤⠵⠤⠬⠮⠆"], // tail wag
    &["  ⡠⣒⠄  ⡔⢄⠔⡄", " ⢸⠸⣀⡔⢉⠱⣃⡢⣂⡣", "  ⠉⠒⠣⠤⠵⠤⠬⠮⠆"],
];

// ── Agent modes (matches Vibe's agent profiles) ──

#[derive(Clone, Copy, PartialEq)]
pub enum AgentMode {
    Default,     // neutral — gray border, approval required
    Plan,        // safe — green border, read-only
    AcceptEdits, // warning — yellow border, auto-approve files
    AutoApprove, // yolo — red border, auto-approve all
}

impl AgentMode {
    pub fn next(self) -> Self {
        match self {
            Self::Default => Self::Plan,
            Self::Plan => Self::AcceptEdits,
            Self::AcceptEdits => Self::AutoApprove,
            Self::AutoApprove => Self::Default,
        }
    }

    fn border_color(self) -> Color {
        match self {
            Self::Default => ANSI_BRIGHT_BLACK,
            Self::Plan => ANSI_GREEN,
            Self::AcceptEdits => ANSI_YELLOW,
            Self::AutoApprove => ANSI_RED,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Default => "",
            Self::Plan => " plan ",
            Self::AcceptEdits => " accept edits ",
            Self::AutoApprove => " auto approve ",
        }
    }

    pub fn is_auto_approve(self) -> bool {
        matches!(self, Self::AutoApprove)
    }
}

// ── Chat entries ──

#[derive(Clone)]
pub enum ChatEntry {
    User(String),
    Assistant(String),
    ToolCall { name: String, detail: String, spinning: bool, started_at: std::time::Instant },
    ToolResult { tool_name: String, summary: String, detail: Option<String>, collapsed: bool },
    Thinking { text: String, spinning: bool, collapsed: bool },
    System(String),
    Error(String),
    Warning(String),
    Interrupt,
    Approval { tool_name: String, command: String },
    Compact { old_tokens: usize, new_tokens: usize },
}

// ── Key actions ──

pub enum KeyAction {
    Submit(String),
    Quit,
    Cancel,
    ApprovalYes,
    ApprovalNo,
    ApprovalAlways,
    CopyLast,
    None,
}

// ── Slash commands ──

struct SlashCmd { name: &'static str, desc: &'static str }

const SLASH_COMMANDS: &[SlashCmd] = &[
    SlashCmd { name: "/quit", desc: "Exit microvibe" },
    SlashCmd { name: "/clear", desc: "Clear context" },
    SlashCmd { name: "/stats", desc: "Token usage & cost" },
    SlashCmd { name: "/undo", desc: "Undo last turn" },
    SlashCmd { name: "/compact", desc: "Force compaction" },
    SlashCmd { name: "/diff", desc: "Git diff" },
    SlashCmd { name: "/commit", desc: "Auto-commit" },
    SlashCmd { name: "/test", desc: "Run tests" },
    SlashCmd { name: "/review", desc: "Review changes" },
    SlashCmd { name: "/branch", desc: "Create branch" },
    SlashCmd { name: "/export", desc: "Export markdown" },
    SlashCmd { name: "/model", desc: "Switch model" },
    SlashCmd { name: "/models", desc: "Model picker" },
    SlashCmd { name: "/sessions", desc: "Session picker" },
    SlashCmd { name: "/rewind", desc: "Rewind checkpoints" },
    SlashCmd { name: "/cost", desc: "Cost breakdown" },
    SlashCmd { name: "/memory", desc: "Persistent memory" },
    SlashCmd { name: "/help", desc: "Show help" },
];

// ── Modal ──

#[derive(Clone, PartialEq)]
pub enum Modal {
    None,
    ModelPicker { items: Vec<String>, selected: usize },
    SessionPicker { items: Vec<(String, String, String)>, selected: usize },
    RewindPicker { items: Vec<String>, selected: usize },
}

// ── TUI App ──

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
    pub agent_mode: AgentMode,
    input_history: Vec<String>,
    input_history_idx: Option<usize>,
    spinner_tick: usize,
    max_context_tokens: usize,
    completions: Vec<(String, String)>,
    completion_idx: Option<usize>,
    pub modal: Modal,
    show_banner: bool,
    at_bottom: bool,
    turn_started_at: Option<std::time::Instant>,
    loading_msg_idx: usize,
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
            agent_mode: AgentMode::Default,
            input_history: Vec::new(),
            input_history_idx: None,
            spinner_tick: 0,
            max_context_tokens: max_ctx,
            completions: Vec::new(),
            completion_idx: None,
            modal: Modal::None,
            show_banner: true,
            at_bottom: true,
            turn_started_at: None,
            loading_msg_idx: 0,
        }
    }

    // ── Entry management ──

    pub fn add_entry(&mut self, entry: ChatEntry) {
        if matches!(entry, ChatEntry::User(_)) {
            self.show_banner = false;
        }
        self.entries.push(entry);
        if self.at_bottom { self.scroll = u16::MAX; }
    }

    pub fn clear_entries(&mut self) {
        self.entries.clear();
        self.scroll = 0;
    }

    pub fn set_waiting(&mut self, waiting: bool) {
        self.waiting = waiting;
        if waiting {
            self.turn_started_at = Some(std::time::Instant::now());
            self.loading_msg_idx = (self.loading_msg_idx + 1) % LOADING_MESSAGES.len();
        } else {
            self.turn_started_at = None;
        }
    }

    pub fn start_assistant_entry(&mut self) {
        self.entries.push(ChatEntry::Assistant(String::new()));
        if self.at_bottom { self.scroll = u16::MAX; }
    }

    pub fn append_assistant_text(&mut self, text: &str) {
        let found = self.entries.iter_mut().rev().any(|e| {
            if let ChatEntry::Assistant(ref mut content) = e {
                content.push_str(text);
                true
            } else { false }
        });
        if !found {
            self.entries.push(ChatEntry::Assistant(text.to_string()));
        }
        if self.at_bottom { self.scroll = u16::MAX; }
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

    pub fn get_last_assistant_text(&self) -> Option<String> {
        self.entries.iter().rev().find_map(|e| {
            if let ChatEntry::Assistant(t) = e { if !t.is_empty() { return Some(t.clone()); } }
            None
        })
    }

    // ── Rendering ──

    pub fn render(&mut self, f: &mut ratatui::Frame) {
        self.spinner_tick = self.spinner_tick.wrapping_add(1);
        let size = f.area();

        let has_completions = !self.completions.is_empty();
        let popup_h = if has_completions { (self.completions.len() as u16 + 2).min(10) } else { 0 };

        let constraints: Vec<Constraint> = if has_completions {
            vec![Constraint::Min(3), Constraint::Length(popup_h), Constraint::Length(4), Constraint::Length(1)]
        } else {
            vec![Constraint::Min(3), Constraint::Length(4), Constraint::Length(1)]
        };

        let chunks = Layout::default().direction(Direction::Vertical).constraints(constraints).split(size);

        if has_completions {
            self.render_chat(f, chunks[0]);
            self.render_completions(f, chunks[1]);
            self.render_input(f, chunks[2]);
            self.render_status(f, chunks[3]);
        } else {
            self.render_chat(f, chunks[0]);
            self.render_input(f, chunks[1]);
            self.render_status(f, chunks[2]);
        }

        if self.modal != Modal::None {
            self.render_modal(f, size);
        }
    }

    fn render_chat(&mut self, f: &mut ratatui::Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();
        let mut in_code_block = false;
        let mut prev_was_tool = false;

        // Animated banner
        if self.show_banner {
            let frame_idx = (self.spinner_tick / 5) % CAT_FRAMES.len();
            let cat = CAT_FRAMES[frame_idx];
            let dim = style(ANSI_DEFAULT);

            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(format!("  {}", cat[0]), dim),
                Span::styled("  microvibe", style(MISTRAL_ORANGE).add_modifier(Modifier::BOLD)),
                Span::styled(format!("  v{} · ", env!("CARGO_PKG_VERSION")), style(ANSI_BRIGHT_BLACK)),
                Span::styled(&self.model, style(ANSI_CYAN)),
            ]));
            lines.push(Line::from(vec![
                Span::styled(format!("  {}", cat[1]), dim),
                Span::styled(format!("  {} · 4 providers", self.provider), style(ANSI_BRIGHT_BLACK)),
            ]));
            lines.push(Line::from(vec![
                Span::styled(format!("  {}", cat[2]), dim),
                Span::styled("  Type ", style(ANSI_BRIGHT_BLACK)),
                Span::styled("/help", style(ANSI_CYAN)),
                Span::styled(" for more information", style(ANSI_BRIGHT_BLACK)),
            ]));
            lines.push(Line::from(""));
        }

        for entry in &self.entries {
            let is_tool = matches!(entry, ChatEntry::ToolCall { .. } | ChatEntry::ToolResult { .. });
            if !is_tool || !prev_was_tool { lines.push(Line::from("")); }
            prev_was_tool = is_tool;

            match entry {
                // User: orange bold text with heavy orange left border
                ChatEntry::User(text) => {
                    for line in text.lines() {
                        lines.push(Line::from(vec![
                            Span::styled("  ┃ ", style(MISTRAL_ORANGE)),
                            Span::styled(line.to_string(), style(MISTRAL_ORANGE).add_modifier(Modifier::BOLD)),
                        ]));
                    }
                }

                // Assistant: markdown rendered
                ChatEntry::Assistant(text) => {
                    if text.is_empty() && self.waiting {
                        let frame = SPINNER_BRAILLE[self.spinner_tick / 2 % SPINNER_BRAILLE.len()];
                        let msg = LOADING_MESSAGES[self.loading_msg_idx % LOADING_MESSAGES.len()];
                        let elapsed = self.turn_started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                        let timer = if elapsed > 0 { format!(" ({})", format_elapsed(elapsed)) } else { String::new() };
                        lines.push(Line::from(Span::styled(
                            format!("  {} {}…{}", frame, msg, timer), style(ANSI_BRIGHT_BLACK),
                        )));
                    } else {
                        in_code_block = false;
                        for line in text.lines() {
                            if line.starts_with("```") {
                                in_code_block = !in_code_block;
                                if in_code_block {
                                    let lang = line[3..].trim();
                                    let label = if lang.is_empty() { "code" } else { lang };
                                    lines.push(Line::from(Span::styled(
                                        format!("  ┌─ {} ─", label), style(ANSI_BRIGHT_BLACK),
                                    )));
                                } else {
                                    lines.push(Line::from(Span::styled("  └─", style(ANSI_BRIGHT_BLACK))));
                                }
                                continue;
                            }
                            if in_code_block {
                                lines.push(Line::from(vec![
                                    Span::styled("  │ ", style(ANSI_BRIGHT_BLACK)),
                                    Span::styled(line.to_string(), style(ANSI_DEFAULT)),
                                ]));
                            } else {
                                lines.push(render_md_line(line));
                            }
                        }
                        if in_code_block {
                            lines.push(Line::from(Span::styled("  └─", style(ANSI_BRIGHT_BLACK))));
                            in_code_block = false;
                        }
                    }
                }

                // Tool call: pulse spinner + name + detail + timer (like Vibe)
                ChatEntry::ToolCall { name, detail, spinning, started_at } => {
                    let elapsed = started_at.elapsed().as_secs();
                    let timer = if *spinning && elapsed > 0 {
                        format!(" ({}s esc to interrupt)", elapsed)
                    } else { String::new() };

                    let status = if *spinning {
                        Span::styled(
                            format!("{} ", SPINNER_PULSE[self.spinner_tick / 3 % SPINNER_PULSE.len()]),
                            style(ANSI_DEFAULT),
                        )
                    } else {
                        Span::styled("✓ ", style(ANSI_GREEN))
                    };

                    // Bash: show "$ command" like Vibe
                    let display = if name == "bash" {
                        format!("$ {}", detail.chars().take(55).collect::<String>())
                    } else {
                        format!("{} {} {}", tool_icon(name), name, detail.chars().take(50).collect::<String>())
                    };

                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        status,
                        Span::styled(display, style(ANSI_DEFAULT)),
                        Span::styled(timer, style(ANSI_BRIGHT_BLACK)),
                    ]));
                }

                // Tool result: collapsible with border
                ChatEntry::ToolResult { tool_name, summary, detail, collapsed } => {
                    let toggle = if detail.is_some() {
                        if *collapsed { "▶ " } else { "▼ " }
                    } else { "" };
                    lines.push(Line::from(vec![
                        Span::styled(format!("    {}→ ", toggle), style(ANSI_BRIGHT_BLACK)),
                        Span::styled(summary.chars().take(75).collect::<String>(), style(ANSI_BRIGHT_BLACK)),
                    ]));
                    if !collapsed {
                        if let Some(ref det) = detail {
                            let max = if tool_name == "bash" { 30 } else { 20 };
                            for line in det.lines().take(max) {
                                let s = if line.starts_with('+') && !line.starts_with("+++") { style(ANSI_GREEN) }
                                    else if line.starts_with('-') && !line.starts_with("---") { style(ANSI_RED) }
                                    else if line.starts_with("@@") { style(ANSI_BLUE) }
                                    else { style(ANSI_BRIGHT_BLACK) };
                                lines.push(Line::from(vec![
                                    Span::styled("    ⎢ ", style(ANSI_BRIGHT_BLACK)),
                                    Span::styled(line.to_string(), s),
                                ]));
                            }
                            if det.lines().count() > max {
                                lines.push(Line::from(Span::styled(
                                    format!("    ⎣ … ({} more lines)", det.lines().count() - max),
                                    style(ANSI_BRIGHT_BLACK),
                                )));
                            }
                        }
                    }
                }

                // Thinking: pulse spinner, italic gray, collapsible
                ChatEntry::Thinking { text, spinning, collapsed } => {
                    let indicator = if *spinning {
                        format!("{} Thinking", SPINNER_PULSE[self.spinner_tick / 3 % SPINNER_PULSE.len()])
                    } else if *collapsed {
                        "▶ Thought".to_string()
                    } else {
                        "▼ Thought".to_string()
                    };
                    lines.push(Line::from(Span::styled(
                        format!("  {}", indicator),
                        style(ANSI_BRIGHT_BLACK).add_modifier(Modifier::ITALIC),
                    )));
                    if !text.is_empty() && !collapsed {
                        for line in text.lines().take(10) {
                            lines.push(Line::from(Span::styled(
                                format!("    {}", line),
                                style(ANSI_BRIGHT_BLACK).add_modifier(Modifier::ITALIC),
                            )));
                        }
                    }
                }

                ChatEntry::System(text) => {
                    lines.push(Line::from(Span::styled(format!("  {}", text), style(ANSI_BRIGHT_BLACK))));
                }
                ChatEntry::Error(text) => {
                    lines.push(Line::from(vec![
                        Span::styled("  ⎢ ", style(ANSI_BRIGHT_BLACK)),
                        Span::styled(format!("Error: {}", text), style(ANSI_RED).add_modifier(Modifier::BOLD)),
                    ]));
                }
                ChatEntry::Warning(text) => {
                    lines.push(Line::from(vec![
                        Span::styled("  ⎢ ", style(ANSI_BRIGHT_BLACK)),
                        Span::styled(text.to_string(), style(ANSI_YELLOW)),
                    ]));
                }
                ChatEntry::Interrupt => {
                    lines.push(Line::from(vec![
                        Span::styled("  ⎢ ", style(ANSI_BRIGHT_BLACK)),
                        Span::styled("Interrupted · What should microvibe do instead?", style(ANSI_YELLOW)),
                    ]));
                }
                ChatEntry::Approval { tool_name, command } => {
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        format!("  ⚠ Approve {} ?", tool_name),
                        style(ANSI_YELLOW).add_modifier(Modifier::BOLD),
                    )));
                    if tool_name == "bash" {
                        lines.push(Line::from(Span::styled("  ┌─ bash ─", style(ANSI_BRIGHT_BLACK))));
                        for cmd_line in command.lines().take(5) {
                            lines.push(Line::from(vec![
                                Span::styled("  │ ", style(ANSI_BRIGHT_BLACK)),
                                Span::styled(cmd_line.to_string(), style(ANSI_DEFAULT)),
                            ]));
                        }
                        lines.push(Line::from(Span::styled("  └─", style(ANSI_BRIGHT_BLACK))));
                    } else {
                        lines.push(Line::from(Span::styled(
                            format!("    {}", command.chars().take(70).collect::<String>()),
                            style(ANSI_DEFAULT),
                        )));
                    }
                    lines.push(Line::from(Span::styled(
                        "    [y] yes  [n] no  [a] always",
                        style(ANSI_BRIGHT_BLACK),
                    )));
                }
                ChatEntry::Compact { old_tokens, new_tokens } => {
                    lines.push(Line::from(Span::styled(
                        format!("  ◆ Context compacted: {} → {} tokens", old_tokens, new_tokens),
                        style(ANSI_BRIGHT_BLACK),
                    )));
                }
            }
        }

        // Clamp scroll
        let content_h = lines.len() as u16;
        let visible_h = area.height;
        let max_scroll = content_h.saturating_sub(visible_h);
        if self.scroll > max_scroll { self.scroll = max_scroll; }
        self.at_bottom = self.scroll >= max_scroll;

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
        } else { 0.0 };

        let cwd = std::env::current_dir()
            .map(|p| {
                let s = p.display().to_string();
                let home = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
                if s.starts_with(&home) { format!("~{}", &s[home.len()..]) } else { s }
            })
            .unwrap_or_else(|_| ".".into());

        let status = Line::from(vec![
            Span::styled(format!(" {} ", cwd), style(ANSI_BRIGHT_BLACK)),
            Span::styled("│ ", style(ANSI_BRIGHT_BLACK)),
            Span::styled(
                format!("{:.0}% of {}k tokens ", pct, self.max_context_tokens / 1000),
                style(if pct > 80.0 { ANSI_RED } else if pct > 50.0 { ANSI_YELLOW } else { ANSI_BRIGHT_BLACK }),
            ),
            Span::styled(format!("${:.2} ", cost), style(ANSI_BRIGHT_BLACK)),
            Span::styled("│ ", style(ANSI_BRIGHT_BLACK)),
            if self.waiting {
                let frame = SPINNER_BRAILLE[self.spinner_tick / 2 % SPINNER_BRAILLE.len()];
                Span::styled(format!("{} working ", frame), style(ANSI_CYAN))
            } else {
                Span::styled("ready ", style(ANSI_GREEN))
            },
        ]);
        f.render_widget(Paragraph::new(status).style(Style::default().bg(Color::Rgb(25, 25, 25))), area);
    }

    fn render_input(&self, f: &mut ratatui::Frame, area: Rect) {
        // Prompt: always orange (like Vibe's $mistral_orange)
        let prompt_color = if self.approval_pending { ANSI_YELLOW } else { MISTRAL_ORANGE };
        let prompt = if self.approval_pending { "?" } else { ">" };

        let input_content = if self.approval_pending {
            "[y]es / [n]o / [a]lways".to_string()
        } else {
            self.input.clone()
        };

        let input_line = Line::from(vec![
            Span::styled(format!("{} ", prompt), style(prompt_color).add_modifier(Modifier::BOLD)),
            Span::styled(input_content, style(ANSI_DEFAULT)),
        ]);

        // Border color from agent mode (default = gray, not green!)
        let border_color = if self.approval_pending {
            ANSI_YELLOW
        } else if self.waiting {
            ANSI_BRIGHT_BLACK
        } else {
            self.agent_mode.border_color()
        };

        let label = self.agent_mode.label();

        let cwd = std::env::current_dir()
            .map(|p| {
                let s = p.display().to_string();
                let home = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
                if s.starts_with(&home) { format!(" ~{} ", &s[home.len()..]) } else { format!(" {} ", s) }
            })
            .unwrap_or_else(|_| " . ".into());

        let mode_name = match self.agent_mode {
            AgentMode::Default => "microvibe",
            AgentMode::Plan => "plan",
            AgentMode::AcceptEdits => "accept edits",
            AgentMode::AutoApprove => "auto approve",
        };
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_style(style(border_color))
            .title_bottom(Line::from(Span::styled(
                format!(" {} ", mode_name), style(border_color),
            )).right_aligned());

        if !label.is_empty() {
            block = block.title_top(
                Line::from(Span::styled(label, style(border_color))).right_aligned()
            );
        }

        f.render_widget(Paragraph::new(input_line).block(block), area);

        if !self.waiting && !self.approval_pending {
            f.set_cursor_position((area.x + self.cursor_pos as u16 + 3, area.y + 1));
        }
    }

    fn render_completions(&self, f: &mut ratatui::Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();
        for (i, (name, desc)) in self.completions.iter().enumerate() {
            let sel = self.completion_idx == Some(i);
            let s = if sel { Style::default().fg(Color::Black).bg(ANSI_CYAN) } else { style(ANSI_DEFAULT) };
            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", name), s.add_modifier(Modifier::BOLD)),
                Span::styled(desc.to_string(), if sel { s } else { style(ANSI_BRIGHT_BLACK) }),
            ]));
        }
        let popup = Paragraph::new(Text::from(lines))
            .block(Block::default().borders(Borders::ALL).border_style(style(ANSI_BRIGHT_BLACK)))
            .style(Style::default().bg(Color::Rgb(30, 30, 30)));
        f.render_widget(popup, area);
    }

    fn render_modal(&self, f: &mut ratatui::Frame, area: Rect) {
        let w = 50.min(area.width.saturating_sub(4));
        let h = 15.min(area.height.saturating_sub(4));
        let r = Rect::new((area.width - w) / 2, (area.height - h) / 2, w, h);

        let (title, title_color, items_lines) = match &self.modal {
            Modal::ModelPicker { items, selected } => {
                let lines: Vec<Line> = items.iter().enumerate().map(|(i, item)| {
                    let m = if i == *selected { "▸ " } else { "  " };
                    let s = if i == *selected { style(ANSI_CYAN).add_modifier(Modifier::BOLD) } else { style(ANSI_DEFAULT) };
                    Line::from(Span::styled(format!("{}{}", m, item), s))
                }).collect();
                (" Select Model ", ANSI_BLUE, lines)
            }
            Modal::SessionPicker { items, selected } => {
                let lines: Vec<Line> = items.iter().enumerate().flat_map(|(i, (id, time, summary))| {
                    let m = if i == *selected { "▸ " } else { "  " };
                    let s = if i == *selected { style(ANSI_CYAN) } else { style(ANSI_DEFAULT) };
                    vec![
                        Line::from(vec![
                            Span::styled(format!("{}{} ", m, &id[..8.min(id.len())]), s.add_modifier(Modifier::BOLD)),
                            Span::styled(time.to_string(), style(ANSI_BRIGHT_BLACK)),
                        ]),
                        Line::from(Span::styled(format!("    {}", summary), style(ANSI_BRIGHT_BLACK))),
                    ]
                }).collect();
                (" Sessions ", ANSI_BLUE, lines)
            }
            Modal::RewindPicker { items, selected } => {
                let lines: Vec<Line> = items.iter().enumerate().map(|(i, item)| {
                    let m = if i == *selected { "▸ " } else { "  " };
                    let s = if i == *selected { style(ANSI_CYAN).add_modifier(Modifier::BOLD) } else { style(ANSI_DEFAULT) };
                    Line::from(Span::styled(format!("{}{}", m, item), s))
                }).collect();
                (" Rewind ", ANSI_BLUE, lines)
            }
            Modal::None => return,
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(style(title_color))
            .title(Span::styled(title, style(title_color).add_modifier(Modifier::BOLD)));
        let p = Paragraph::new(Text::from(items_lines))
            .block(block)
            .style(Style::default().bg(Color::Rgb(20, 20, 20)));
        f.render_widget(ratatui::widgets::Clear, r);
        f.render_widget(p, r);
    }

    // ── Input handling ──

    pub fn handle_key(&mut self, key: KeyEvent) -> KeyAction {
        if self.modal != Modal::None { return self.handle_modal_key(key); }

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
            // A1: Esc = interrupt during turn (like Vibe)
            (_, KeyCode::Esc) if self.waiting => KeyAction::Cancel,
            // Ctrl+C: cancel if waiting, quit otherwise
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                if self.waiting { KeyAction::Cancel } else { KeyAction::Quit }
            }
            // A2: Ctrl+D = force quit
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => KeyAction::Quit,
            // Ctrl+Y: copy last
            (KeyModifiers::CONTROL, KeyCode::Char('y')) => KeyAction::CopyLast,
            // Ctrl+G: external editor
            (KeyModifiers::CONTROL, KeyCode::Char('g')) if !self.waiting => {
                KeyAction::Submit("/editor".to_string())
            }
            // A5: Ctrl+P = rewind previous
            (KeyModifiers::CONTROL, KeyCode::Char('p')) if !self.waiting => {
                KeyAction::Submit("/undo".to_string())
            }
            // A7: Ctrl+O = toggle last tool result
            (KeyModifiers::CONTROL, KeyCode::Char('o')) => {
                self.toggle_last_collapsible();
                KeyAction::None
            }
            // D2: Ctrl+A = select all (go to start)
            (KeyModifiers::CONTROL, KeyCode::Char('a')) => {
                self.cursor_pos = 0;
                KeyAction::None
            }
            // D3: Ctrl+W = delete word backward
            (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
                if self.cursor_pos > 0 {
                    let before = &self.input[..self.cursor_pos];
                    let word_start = before.rfind(' ').map(|p| p + 1).unwrap_or(0);
                    self.input = format!("{}{}", &self.input[..word_start], &self.input[self.cursor_pos..]);
                    self.cursor_pos = word_start;
                    self.update_completions();
                }
                KeyAction::None
            }
            // D4: Alt+Left = word left
            (KeyModifiers::ALT, KeyCode::Left) => {
                if self.cursor_pos > 0 {
                    let before = &self.input[..self.cursor_pos];
                    self.cursor_pos = before.rfind(' ').map(|p| p).unwrap_or(0);
                }
                KeyAction::None
            }
            // D4: Alt+Right = word right
            (KeyModifiers::ALT, KeyCode::Right) => {
                if self.cursor_pos < self.input.len() {
                    let after = &self.input[self.cursor_pos + 1..];
                    self.cursor_pos = after.find(' ').map(|p| self.cursor_pos + 1 + p + 1).unwrap_or(self.input.len());
                }
                KeyAction::None
            }
            // Shift+Tab: cycle agent mode
            (_, KeyCode::BackTab) if !self.waiting => {
                self.agent_mode = self.agent_mode.next();
                KeyAction::None
            }
            // Tab: accept completion or toggle collapse
            (_, KeyCode::Tab) if !self.waiting => {
                if !self.completions.is_empty() { self.accept_completion(); }
                else { self.toggle_last_collapsible(); }
                KeyAction::None
            }
            // Up/Down in completion popup
            (_, KeyCode::Up) if !self.completions.is_empty() => {
                if let Some(ref mut idx) = self.completion_idx {
                    if *idx > 0 { *idx -= 1; }
                }
                KeyAction::None
            }
            (_, KeyCode::Down) if !self.completions.is_empty() => {
                if let Some(ref mut idx) = self.completion_idx {
                    if *idx + 1 < self.completions.len() { *idx += 1; }
                } else if !self.completions.is_empty() {
                    self.completion_idx = Some(0);
                }
                KeyAction::None
            }
            // A4: Shift+Up/Down = scroll chat
            (KeyModifiers::SHIFT, KeyCode::Up) => {
                self.scroll = self.scroll.saturating_sub(3);
                self.at_bottom = false;
                KeyAction::None
            }
            (KeyModifiers::SHIFT, KeyCode::Down) => {
                self.scroll = self.scroll.saturating_add(3);
                KeyAction::None
            }
            (KeyModifiers::SHIFT, KeyCode::Enter) => {
                self.input.insert(self.cursor_pos, '\n');
                self.cursor_pos += 1;
                KeyAction::None
            }
            (_, KeyCode::Enter) => {
                if self.input.is_empty() || self.waiting { return KeyAction::None; }
                let submitted = self.input.clone();
                self.input_history.push(submitted.clone());
                self.input_history_idx = None;
                self.input.clear();
                self.cursor_pos = 0;
                self.completions.clear();
                self.completion_idx = None;
                self.show_banner = false;
                KeyAction::Submit(submitted)
            }
            (_, KeyCode::Backspace) => {
                if self.cursor_pos > 0 { self.cursor_pos -= 1; self.input.remove(self.cursor_pos); self.update_completions(); }
                KeyAction::None
            }
            (_, KeyCode::Delete) => {
                if self.cursor_pos < self.input.len() { self.input.remove(self.cursor_pos); }
                KeyAction::None
            }
            (_, KeyCode::Left) => { if self.cursor_pos > 0 { self.cursor_pos -= 1; } KeyAction::None }
            (_, KeyCode::Right) => { if self.cursor_pos < self.input.len() { self.cursor_pos += 1; } KeyAction::None }
            (_, KeyCode::Home) => { self.cursor_pos = 0; KeyAction::None }
            (_, KeyCode::End) => { self.cursor_pos = self.input.len(); KeyAction::None }
            (_, KeyCode::Up) => {
                if !self.input_history.is_empty() {
                    let idx = match self.input_history_idx {
                        Some(0) => 0, Some(i) => i - 1,
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
                    } else { self.input_history_idx = None; self.input.clear(); }
                    self.cursor_pos = self.input.len();
                }
                KeyAction::None
            }
            (_, KeyCode::PageUp) => { self.scroll = self.scroll.saturating_sub(10); self.at_bottom = false; KeyAction::None }
            (_, KeyCode::PageDown) => { self.scroll = self.scroll.saturating_add(10); KeyAction::None }
            (_, KeyCode::Esc) => {
                if !self.completions.is_empty() { self.completions.clear(); self.completion_idx = None; }
                else { self.input.clear(); self.cursor_pos = 0; }
                KeyAction::None
            }
            (_, KeyCode::Char(c)) => {
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += 1;
                self.update_completions();
                KeyAction::None
            }
            _ => KeyAction::None,
        }
    }

    fn handle_modal_key(&mut self, key: KeyEvent) -> KeyAction {
        match key.code {
            KeyCode::Esc => { self.modal = Modal::None; KeyAction::None }
            KeyCode::Up => {
                match &mut self.modal {
                    Modal::ModelPicker { selected, .. } |
                    Modal::SessionPicker { selected, .. } |
                    Modal::RewindPicker { selected, .. } => { if *selected > 0 { *selected -= 1; } }
                    _ => {}
                }
                KeyAction::None
            }
            KeyCode::Down => {
                match &mut self.modal {
                    Modal::ModelPicker { selected, items } => { if *selected + 1 < items.len() { *selected += 1; } }
                    Modal::SessionPicker { selected, items } => { if *selected + 1 < items.len() { *selected += 1; } }
                    Modal::RewindPicker { selected, items } => { if *selected + 1 < items.len() { *selected += 1; } }
                    _ => {}
                }
                KeyAction::None
            }
            KeyCode::Enter => {
                let cmd = match &self.modal {
                    Modal::ModelPicker { selected, items } => items.get(*selected).map(|i| format!("/model {}", i)),
                    Modal::SessionPicker { selected, items } => items.get(*selected).map(|(id, _, _)| format!("/resume {}", id)),
                    Modal::RewindPicker { selected, .. } => Some(format!("/rewind {}", selected)),
                    _ => None,
                };
                self.modal = Modal::None;
                cmd.map(KeyAction::Submit).unwrap_or(KeyAction::None)
            }
            _ => KeyAction::None,
        }
    }

    // ── Completions ──

    fn update_completions(&mut self) {
        if self.input.starts_with('/') && !self.input.contains(' ') {
            let prefix = &self.input;
            self.completions = SLASH_COMMANDS.iter()
                .filter(|c| c.name.starts_with(prefix))
                .map(|c| (c.name.to_string(), c.desc.to_string()))
                .collect();
            if self.completions.len() == 1 && self.completions[0].0 == self.input { self.completions.clear(); }
            self.completion_idx = if self.completions.is_empty() { None } else { Some(0) };
            return;
        }

        // @file path completion
        if let Some(at_pos) = self.input[..self.cursor_pos].rfind('@') {
            let partial = &self.input[at_pos + 1..self.cursor_pos];
            if !partial.contains(' ') && (at_pos == 0 || self.input.as_bytes()[at_pos - 1] == b' ') {
                let (dir, prefix) = if let Some(slash) = partial.rfind('/') {
                    (&partial[..slash + 1], &partial[slash + 1..])
                } else { ("", partial) };

                let search_dir = if dir.is_empty() { ".".to_string() } else { dir.to_string() };
                if let Ok(entries) = std::fs::read_dir(&search_dir) {
                    self.completions = entries.flatten().filter_map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        if name.starts_with('.') || !name.to_lowercase().starts_with(&prefix.to_lowercase()) { return None; }
                        let is_dir = e.metadata().map(|m| m.is_dir()).unwrap_or(false);
                        let display = format!("@{}{}{}", dir, name, if is_dir { "/" } else { "" });
                        let desc = if is_dir { "dir".into() } else {
                            let sz = e.metadata().map(|m| m.len()).unwrap_or(0);
                            if sz < 1024 { format!("{}B", sz) } else { format!("{:.1}KB", sz as f64 / 1024.0) }
                        };
                        Some((display, desc))
                    }).take(10).collect();
                    self.completion_idx = if self.completions.is_empty() { None } else { Some(0) };
                    return;
                }
            }
        }

        self.completions.clear();
        self.completion_idx = None;
    }

    fn accept_completion(&mut self) {
        if let Some(idx) = self.completion_idx {
            if idx < self.completions.len() {
                let completion = self.completions[idx].0.clone();
                if completion.starts_with('@') {
                    if let Some(at_pos) = self.input[..self.cursor_pos].rfind('@') {
                        let suffix = self.input[self.cursor_pos..].to_string();
                        let sep = if completion.ends_with('/') { "" } else { " " };
                        self.input = format!("{}{}{}", &self.input[..at_pos], completion, sep);
                        self.cursor_pos = self.input.len();
                        self.input.push_str(&suffix);
                    }
                } else {
                    self.input = completion;
                    self.cursor_pos = self.input.len();
                }
                self.completions.clear();
                self.completion_idx = None;
            }
        }
    }

    fn toggle_last_collapsible(&mut self) {
        for entry in self.entries.iter_mut().rev() {
            match entry {
                ChatEntry::ToolResult { collapsed, detail, .. } if detail.is_some() => { *collapsed = !*collapsed; return; }
                ChatEntry::Thinking { collapsed, spinning, .. } if !*spinning => { *collapsed = !*collapsed; return; }
                _ => {}
            }
        }
    }
}

// ── Helpers ──

fn style(color: Color) -> Style { Style::default().fg(color) }

fn tool_icon(name: &str) -> &'static str {
    match name {
        "bash" => "⚡", "read_file" => "📄", "write_file" => "✏️", "search_replace" => "🔧",
        "grep" => "🔍", "glob" | "list_dir" => "📂", "memory_read" | "memory_write" => "🧠",
        _ => "🔧",
    }
}

/// Render a markdown line with inline formatting
fn render_md_line(text: &str) -> Line<'static> {
    let owned = text.to_string();

    // E2: Empty lines — no indent
    if owned.trim().is_empty() { return Line::from(""); }

    // E3: Headers with margin
    if owned.starts_with("### ") { return Line::from(Span::styled(format!("  {}", &owned[4..]), style(ANSI_DEFAULT).add_modifier(Modifier::BOLD))); }
    if owned.starts_with("## ") { return Line::from(Span::styled(format!("  {}", &owned[3..]), style(ANSI_DEFAULT).add_modifier(Modifier::BOLD | Modifier::UNDERLINED))); }
    if owned.starts_with("# ") { return Line::from(Span::styled(format!("  {}", &owned[2..]), style(ANSI_DEFAULT).add_modifier(Modifier::BOLD | Modifier::UNDERLINED))); }

    // Bullets — content goes through inline parser for **bold** and `code`
    if owned.starts_with("- ") || owned.starts_with("* ") {
        let mut spans = vec![Span::raw("  • ")];
        spans.extend(parse_inline(&owned[2..]));
        return Line::from(spans);
    }
    if owned.starts_with("  - ") || owned.starts_with("  * ") {
        let mut spans = vec![Span::raw("    ◦ ")];
        spans.extend(parse_inline(&owned[4..]));
        return Line::from(spans);
    }

    // Blockquote
    if owned.starts_with("> ") {
        return Line::from(vec![
            Span::styled("  ▎ ", style(ANSI_BRIGHT_BLACK)),
            Span::styled(owned[2..].to_string(), style(ANSI_BRIGHT_BLACK)),
        ]);
    }

    // Numbered lists: 1. text, 2. text, etc.
    if let Some(dot_pos) = owned.find(". ") {
        let num_part = owned[..dot_pos].trim();
        if dot_pos <= 4 && num_part.chars().all(|c| c.is_ascii_digit()) {
            let mut spans = vec![Span::raw(format!("  {}. ", num_part))];
            spans.extend(parse_inline(&owned[dot_pos + 2..]));
            return Line::from(spans);
        }
    }

    // Table rows: | col | col |
    if owned.starts_with('|') && owned.ends_with('|') {
        let is_separator = owned.chars().all(|c| c == '|' || c == '-' || c == ':' || c == ' ');
        if is_separator {
            // Render as thin horizontal rule between header and body
            let col_count = owned.matches('|').count().saturating_sub(1);
            let sep = (0..col_count).map(|_| "──────────").collect::<Vec<_>>().join("┼");
            return Line::from(Span::styled(format!("  {}", sep), style(ANSI_BRIGHT_BLACK)));
        }
        let cells: Vec<&str> = owned.split('|').filter(|s| !s.is_empty()).collect();
        let mut spans = vec![Span::raw("  ")];
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 { spans.push(Span::styled(" │ ", style(ANSI_BRIGHT_BLACK))); }
            spans.extend(parse_inline(cell.trim()));
        }
        return Line::from(spans);
    }

    // Horizontal rule
    if owned.trim() == "---" || owned.trim() == "***" {
        return Line::from(Span::styled("  ────────────────────────────────────────", style(ANSI_BRIGHT_BLACK)));
    }

    // Inline formatting
    render_inline_spans(&owned)
}

/// Parse inline markdown (**bold**, `code`) into spans
fn parse_inline(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // **bold**
        if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(end) = find_pat(&chars, i + 2, "**") {
                if !current.is_empty() { spans.push(Span::raw(std::mem::take(&mut current))); }
                let inner: String = chars[i + 2..end].iter().collect();
                spans.push(Span::styled(inner, style(ANSI_DEFAULT).add_modifier(Modifier::BOLD)));
                i = end + 2;
                continue;
            }
        }
        // `code` — green bold on transparent (like Vibe)
        if chars[i] == '`' && (i + 1 >= chars.len() || chars[i + 1] != '`') {
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == '`') {
                if !current.is_empty() { spans.push(Span::raw(std::mem::take(&mut current))); }
                let inner: String = chars[i + 1..i + 1 + end].iter().collect();
                spans.push(Span::styled(inner, style(ANSI_GREEN).add_modifier(Modifier::BOLD)));
                i = i + 1 + end + 1;
                continue;
            }
        }
        current.push(chars[i]);
        i += 1;
    }
    if !current.is_empty() { spans.push(Span::raw(current)); }
    spans
}

/// Render a full line with inline formatting (adds indent)
fn render_inline_spans(text: &str) -> Line<'static> {
    let mut spans = vec![Span::raw("  ".to_string())];
    spans.extend(parse_inline(text));
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
