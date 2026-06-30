use colored::Colorize;
use std::io::Write;

// ── Box drawing characters ──
const BOX_TL: &str = "╭";
const BOX_TR: &str = "╮";
const BOX_BL: &str = "╰";
const BOX_BR: &str = "╯";
const BOX_H: &str = "─";
const BOX_V: &str = "│";

/// Streaming markdown renderer.
/// Accumulates characters, renders complete lines with formatting,
/// and prints partial lines raw for real-time streaming feel.
pub struct StreamRenderer {
    /// Buffer for the current incomplete line
    partial: String,
    /// Whether we're inside a ``` code block
    in_code_block: bool,
    /// Language tag of the current code block
    code_lang: String,
    /// Number of characters printed for the current partial line (for erasure)
    partial_printed: usize,
}

impl StreamRenderer {
    pub fn new() -> Self {
        Self {
            partial: String::new(),
            in_code_block: false,
            code_lang: String::new(),
            partial_printed: 0,
        }
    }

    /// Print a single character as part of the streaming partial line
    fn print_partial_char(&mut self, ch: char) {
        if self.in_code_block {
            print!("{}", format!("{}", ch).dimmed());
        } else {
            print!("{}", ch);
        }
        std::io::stdout().flush().ok();
        self.partial_printed += 1;
    }

    /// Erase the partially printed line so we can re-render it formatted
    fn erase_partial(&mut self) {
        if self.partial_printed > 0 {
            // Move cursor to beginning of partial output and clear line
            print!("\r");
            print!("{}", " ".repeat(self.partial_printed + 2));
            print!("\r");
            std::io::stdout().flush().ok();
            self.partial_printed = 0;
        }
    }

    /// Render a complete line with full markdown formatting
    fn render_complete_line(&self) {
        let line = &self.partial;

        // Code block fences
        if line.starts_with("```") {
            if self.in_code_block {
                // Closing fence
                eprintln!("  {}", BOX_BL.to_string() + &BOX_H.repeat(50) + BOX_BR);
            } else {
                // Opening fence
                let lang = line[3..].trim();
                let label = if lang.is_empty() {
                    " code ".to_string()
                } else {
                    format!(" {} ", lang)
                };
                let pad = 50_usize.saturating_sub(label.len());
                eprintln!(
                    "  {}{}{}{}",
                    BOX_TL,
                    BOX_H.repeat(2),
                    label.dimmed(),
                    BOX_H.repeat(pad.saturating_sub(2)) .to_owned() + BOX_TR
                );
            }
            return;
        }

        if self.in_code_block {
            // Code line inside block — with left border
            eprintln!("  {} {}", BOX_V.dimmed(), line.dimmed());
            return;
        }

        // Empty line
        if line.is_empty() {
            eprintln!();
            return;
        }

        // Headers
        if line.starts_with("### ") {
            eprintln!("  {}", line[4..].bold());
            return;
        }
        if line.starts_with("## ") {
            eprintln!("  {}", line[3..].bold().underline());
            return;
        }
        if line.starts_with("# ") {
            eprintln!("  {}", line[2..].bold().underline());
            return;
        }

        // Horizontal rule
        if line.trim() == "---" || line.trim() == "***" || line.trim() == "___" {
            eprintln!("  {}", "─".repeat(50).dimmed());
            return;
        }

        // Bullet points
        if line.starts_with("- ") || line.starts_with("* ") {
            eprintln!("  {} {}", "•".cyan(), render_inline(&line[2..]));
            return;
        }

        // Indented bullet points
        if line.starts_with("  - ") || line.starts_with("  * ") {
            eprintln!("    {} {}", "◦".dimmed(), render_inline(&line[4..]));
            return;
        }

        // Numbered lists
        if let Some(rest) = try_numbered_list(line) {
            eprintln!("  {}", rest);
            return;
        }

        // Blockquote
        if line.starts_with("> ") {
            eprintln!("  {} {}", "▎".cyan(), render_inline(&line[2..]).dimmed());
            return;
        }

        // Regular text with inline formatting
        eprintln!("  {}", render_inline(line));
    }

    /// Flush remaining partial content (called at end of response)
    pub fn finish(&mut self) {
        if !self.partial.is_empty() {
            self.erase_partial();
            self.render_complete_line();
            self.partial.clear();
        }
        // Close any unclosed code block
        if self.in_code_block {
            eprintln!("  {}", BOX_BL.to_string() + &BOX_H.repeat(50) + BOX_BR);
            self.in_code_block = false;
        }
    }

    /// Must be called after render_complete_line to update code block state
    /// This is separate because render_complete_line borrows &self
    pub fn update_state_after_line(&mut self, line: &str) {
        if line.starts_with("```") {
            self.in_code_block = !self.in_code_block;
            if self.in_code_block {
                self.code_lang = line[3..].trim().to_string();
            } else {
                self.code_lang.clear();
            }
        }
    }

    /// Redesigned push that properly tracks code block state
    pub fn push_streaming(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                self.erase_partial();
                self.render_complete_line();
                let line = self.partial.clone();
                self.update_state_after_line(&line);
                self.partial.clear();
            } else {
                self.partial.push(ch);
                self.print_partial_char(ch);
            }
        }
    }
}

/// Try to parse a numbered list line: "1. text" -> formatted
fn try_numbered_list(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    if let Some(dot_pos) = trimmed.find(". ") {
        if dot_pos <= 3 && trimmed[..dot_pos].chars().all(|c| c.is_ascii_digit()) {
            let num = &trimmed[..dot_pos];
            let rest = &trimmed[dot_pos + 2..];
            let pad = " ".repeat(indent);
            return Some(format!(
                "{}{} {}",
                pad,
                format!("{}.", num).cyan(),
                render_inline(rest)
            ));
        }
    }
    None
}

/// Render inline markdown: **bold**, `code`, *italic*
pub fn render_inline(text: &str) -> String {
    let mut result = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // **bold**
        if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(end) = find_closing(&chars, i + 2, "**") {
                let inner: String = chars[i + 2..end].iter().collect();
                result.push_str(&format!("{}", inner.bold()));
                i = end + 2;
                continue;
            }
        }

        // `code`
        if chars[i] == '`' && (i + 1 >= chars.len() || chars[i + 1] != '`') {
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == '`') {
                let inner: String = chars[i + 1..i + 1 + end].iter().collect();
                result.push_str(&format!("{}", inner.on_bright_black().white()));
                i = i + 1 + end + 1;
                continue;
            }
        }

        // *italic* (but not **)
        if chars[i] == '*' && (i == 0 || chars[i - 1] != '*') {
            if i + 1 < chars.len() && chars[i + 1] != '*' && chars[i + 1] != ' ' {
                if let Some(end) = chars[i + 1..].iter().position(|&c| c == '*') {
                    if i + 1 + end < chars.len()
                        && (i + 1 + end + 1 >= chars.len() || chars[i + 1 + end + 1] != '*')
                    {
                        let inner: String = chars[i + 1..i + 1 + end].iter().collect();
                        result.push_str(&format!("{}", inner.italic()));
                        i = i + 1 + end + 1;
                        continue;
                    }
                }
            }
        }

        result.push(chars[i]);
        i += 1;
    }

    result
}

fn find_closing(chars: &[char], start: usize, pattern: &str) -> Option<usize> {
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

// ── Tool call display ──

/// Print a tool call with a nice box
pub fn print_tool_box(name: &str, detail: &str) {
    let icon = match name {
        "bash" => "⚡",
        "read_file" | "read" => "📄",
        "write_file" | "write" => "✏️",
        "search_replace" | "edit" => "🔧",
        "grep" => "🔍",
        "glob" => "📂",
        "list_dir" => "📁",
        "memory_read" => "🧠",
        "memory_write" => "🧠",
        _ => "🔧",
    };

    let label = match name {
        "bash" => "bash",
        "read_file" => "read",
        "write_file" => "write",
        "search_replace" => "edit",
        _ => name,
    };

    let detail_truncated: String = detail.chars().take(70).collect();
    let content = format!("{} {} {}", icon, label.bold(), detail_truncated.dimmed());
    let width = 76;

    eprintln!(
        "  {}{}{}",
        BOX_TL,
        BOX_H.repeat(width),
        BOX_TR
    );
    eprintln!("  {} {:<width$}{}", BOX_V, content, BOX_V);
    eprintln!(
        "  {}{}{}",
        BOX_BL,
        BOX_H.repeat(width),
        BOX_BR
    );
}

/// Print a tool result summary
pub fn print_tool_result_box(result: &str) {
    let lines: Vec<&str> = result.lines().collect();
    let summary = if lines.len() > 3 {
        format!("{} ({} lines)", lines[0].chars().take(80).collect::<String>(), lines.len())
    } else {
        result.chars().take(100).collect()
    };
    eprintln!("  {} {}", "→".dimmed(), summary.dimmed());
}

/// Print a diff preview for search_replace
pub fn print_diff_preview(search: &str, replace: &str) {
    let search_lines: Vec<&str> = search.lines().take(3).collect();
    let replace_lines: Vec<&str> = replace.lines().take(3).collect();

    for line in &search_lines {
        if line.len() < 100 {
            eprintln!("    {} {}", "−".red(), line.red());
        }
    }
    for line in &replace_lines {
        if line.len() < 100 {
            eprintln!("    {} {}", "+".green(), line.green());
        }
    }
}

// ── Spinner ──

pub struct Spinner {
    active: bool,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl Spinner {
    pub fn start(message: &str) -> Self {
        let msg = message.to_string();
        let handle = tokio::spawn(async move {
            let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let mut i = 0;
            loop {
                eprint!("\r  {} {} ", frames[i % frames.len()].cyan(), msg.dimmed());
                std::io::stderr().flush().ok();
                tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                i += 1;
            }
        });
        Self {
            active: true,
            handle: Some(handle),
        }
    }

    pub fn stop(&mut self) {
        if self.active {
            if let Some(handle) = self.handle.take() {
                handle.abort();
            }
            eprint!("\r{}\r", " ".repeat(60));
            std::io::stderr().flush().ok();
            self.active = false;
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop();
    }
}
