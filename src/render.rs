use colored::Colorize;
use std::io::Write;

/// Render streamed text with basic markdown formatting.
/// Called character-by-character during streaming, accumulates into lines
/// and renders them with formatting when complete.
#[allow(dead_code)]
pub struct MarkdownRenderer {
    line_buffer: String,
    in_code_block: bool,
    code_lang: String,
}

#[allow(dead_code)]
impl MarkdownRenderer {
    pub fn new() -> Self {
        Self {
            line_buffer: String::new(),
            in_code_block: false,
            code_lang: String::new(),
        }
    }

    /// Push streamed text chunk. Renders complete lines immediately.
    pub fn push(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                self.render_line();
                self.line_buffer.clear();
            } else {
                self.line_buffer.push(ch);
            }
        }
        // Partial line: print inline text as-is for streaming feel
        if !self.line_buffer.is_empty() && !self.line_buffer.starts_with("```") {
            if self.in_code_block {
                print!("{}", &self.line_buffer.dimmed());
            } else {
                print!("{}", render_inline(&self.line_buffer));
            }
            std::io::stdout().flush().ok();
            // Don't clear — we'll re-render when the line is complete
            // Use carriage return to overwrite
            let len = self.line_buffer.len();
            print!("{}", "\x08".repeat(len)); // backspace
            std::io::stdout().flush().ok();
        }
    }

    /// Flush any remaining content
    pub fn flush(&mut self) {
        if !self.line_buffer.is_empty() {
            self.render_line();
            self.line_buffer.clear();
        }
        println!();
    }

    fn render_line(&mut self) {
        let line = &self.line_buffer;

        // Code block fences
        if line.starts_with("```") {
            if self.in_code_block {
                self.in_code_block = false;
                self.code_lang.clear();
                println!("{}", "```".dimmed());
            } else {
                self.in_code_block = true;
                self.code_lang = line[3..].trim().to_string();
                let label = if self.code_lang.is_empty() {
                    "```".to_string()
                } else {
                    format!("```{}", self.code_lang)
                };
                println!("{}", label.dimmed());
            }
            return;
        }

        if self.in_code_block {
            println!("{}", line.dimmed());
            return;
        }

        // Headers
        if line.starts_with("### ") {
            println!("{}", line[4..].bold());
            return;
        }
        if line.starts_with("## ") {
            println!("{}", line[3..].bold().underline());
            return;
        }
        if line.starts_with("# ") {
            println!("{}", line[2..].bold().underline());
            return;
        }

        // Horizontal rule
        if line.trim() == "---" || line.trim() == "***" {
            println!("{}", "─".repeat(40).dimmed());
            return;
        }

        // Bullet points
        if line.starts_with("- ") || line.starts_with("* ") {
            print!("{} ", "•".cyan());
            println!("{}", render_inline(&line[2..]));
            return;
        }

        // Numbered lists
        if line.len() > 2 {
            let first_dot = line.find(". ");
            if let Some(pos) = first_dot {
                if pos <= 3 && line[..pos].chars().all(|c| c.is_ascii_digit()) {
                    let num = &line[..pos];
                    print!("{} ", format!("{}.", num).cyan());
                    println!("{}", render_inline(&line[pos + 2..]));
                    return;
                }
            }
        }

        // Regular text with inline formatting
        println!("{}", render_inline(line));
    }
}

/// Render inline markdown: **bold**, `code`, *italic*
#[allow(dead_code)]
fn render_inline(text: &str) -> String {
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
                result.push_str(&format!("{}", inner.cyan()));
                i = i + 1 + end + 1;
                continue;
            }
        }

        // *italic*
        if chars[i] == '*' && (i == 0 || chars[i - 1] != '*') {
            if i + 1 < chars.len() && chars[i + 1] != '*' {
                if let Some(end) = chars[i + 1..].iter().position(|&c| c == '*') {
                    let inner: String = chars[i + 1..i + 1 + end].iter().collect();
                    result.push_str(&format!("{}", inner.italic()));
                    i = i + 1 + end + 1;
                    continue;
                }
            }
        }

        result.push(chars[i]);
        i += 1;
    }

    result
}

#[allow(dead_code)]
fn find_closing(chars: &[char], start: usize, pattern: &str) -> Option<usize> {
    let pat: Vec<char> = pattern.chars().collect();
    for i in start..chars.len() - pat.len() + 1 {
        if chars[i..i + pat.len()] == pat[..] {
            return Some(i);
        }
    }
    None
}

/// Simple spinner for API calls
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
                eprint!("\r{} {} ", frames[i % frames.len()].cyan(), msg.dimmed());
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
            // Clear the spinner line
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
