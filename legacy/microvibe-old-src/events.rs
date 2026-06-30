use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

/// Global flag: when true, all eprintln! should be suppressed
pub static TUI_MODE: AtomicBool = AtomicBool::new(false);

pub fn set_tui_mode(v: bool) {
    TUI_MODE.store(v, Ordering::Relaxed);
}

/// Events emitted by the agent/LLM for the TUI to consume
#[derive(Debug, Clone)]
pub enum TuiEvent {
    /// Streaming text delta from the LLM
    TextDelta(String),
    /// LLM finished generating text
    TextDone,
    /// A tool call is starting
    ToolCallStart {
        name: String,
        detail: String,
    },
    /// A tool call completed
    ToolCallDone {
        name: String,
        success: bool,
        summary: String,
        full_result: Option<String>,
    },
    /// Thinking/reasoning started
    ThinkingStart,
    /// Thinking done
    ThinkingDone,
    /// Token usage update
    TokenUpdate {
        prompt_tokens: u64,
        completion_tokens: u64,
    },
    /// Agent turn completed
    TurnDone,
    /// Error occurred
    Error(String),
    /// Informational system message
    SystemMessage(String),
    /// Context was compacted
    CompactDone { old_tokens: usize, new_tokens: usize },
}

/// Optional event sender — when Some, events go to TUI; when None, direct print
#[derive(Clone)]
pub struct EventSender {
    tx: mpsc::UnboundedSender<TuiEvent>,
}

impl EventSender {
    pub fn new(tx: mpsc::UnboundedSender<TuiEvent>) -> Self {
        Self { tx }
    }

    pub fn send(&self, event: TuiEvent) {
        let _ = self.tx.send(event);
    }
}
