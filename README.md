# microvibe

Ultra-light CLI coding agent in Rust. Built from scratch as a lean alternative to [Mistral Vibe](https://github.com/mistralai/mistral-vibe).

```
 _ __ ___ (_) ___ _ __ _____   _(_) |__   ___
| '_ ` _ \| |/ __| '__/ _ \ \ / / | '_ \ / _ \
| | | | | | | (__| | | (_) \ V /| | |_) |  __/
|_| |_| |_|_|\___|_|  \___/ \_/ |_|_.__/ \___|

  2.2 MB binary · 11 MB RAM · <50ms startup · 47 features
```

## Quick start

```bash
# Build
cargo build --release

# Run (interactive REPL)
MISTRAL_API_KEY=xxx ./target/release/microvibe

# Run with TUI (full-screen ratatui interface)
./target/release/microvibe --tui

# Single prompt
./target/release/microvibe -p "fix the bug in src/main.rs"

# Pipe mode
git diff | ./target/release/microvibe -p "review this"

# Continue last session
./target/release/microvibe -c
```

## Features

### Core (47 features)
- Agent loop with streaming SSE
- 9 tools: bash, read_file, write_file, search_replace, grep, glob, list_dir, memory_read, memory_write
- 4 providers: Mistral, Anthropic (Messages API), OpenAI, local
- Tool approval system (safe/deny/sensitive/always)
- Context auto-compaction (LLM-based summarization)
- Token tracking + real model pricing (15+ models)
- Session persistence (auto-save, resume, export)
- Persistent memory across sessions
- Background subagent tasks (/task, /inject)
- Plan mode (/plan, /do)
- @file mentions with line ranges
- Pipe mode, Ctrl+C cancel, --continue

### TUI (ratatui)
- Full-screen terminal UI matching Vibe's visual style
- Event-driven architecture (agent in tokio::spawn, mpsc channels)
- Animated braille cat banner (from Vibe's petit_chat)
- Streaming assistant responses in the TUI
- Markdown rendering: headers, bold, code (green), code blocks, tables, bullets, blockquotes
- Tool calls with pulse spinner ■□ → ✓, timer, bash $ prompt
- Collapsible tool results with diff coloring
- Thinking section (collapsible, italic gray)
- Slash command autocomplete + @file path completion
- Agent modes: default → plan → accept-edits → auto-approve (Shift+Tab)
- Model picker, session picker, rewind modals
- Desktop notifications (macOS)
- Easter egg loading messages

### Keyboard shortcuts
| Key | Action |
|---|---|
| Enter | Send message |
| Shift+Enter | Newline |
| Ctrl+C / Esc | Cancel turn / quit |
| Ctrl+D | Force quit |
| Tab | Accept completion / toggle collapse |
| Shift+Tab | Cycle agent mode |
| Ctrl+G | Open $EDITOR |
| Ctrl+Y | Copy last response |
| Ctrl+O | Toggle tool result |
| Ctrl+P | Undo |
| Ctrl+W | Delete word |
| Alt+←/→ | Word navigation |
| Shift+↑/↓ | Scroll chat |
| PageUp/Down | Scroll chat (fast) |

### Slash commands
`/quit` `/clear` `/stats` `/undo` `/compact` `/diff` `/commit` `/test` `/review` `/branch` `/model` `/models` `/sessions` `/rewind` `/cost` `/memory` `/export` `/config` `/log` `/reload` `/help`

## Configuration

```bash
# Create default config
./target/release/microvibe --init
```

Config file: `~/.config/microvibe/config.toml`

```toml
[default]
provider = "foundry-anthropic"
model = "claude-opus-4-6"
max_context_tokens = 900000

[providers.mistral]
api_base = "https://api.mistral.ai/v1"
api_key_env = "MISTRAL_API_KEY"

[providers.anthropic]
api_base = "https://api.anthropic.com/v1"
api_key_env = "ANTHROPIC_API_KEY"
backend = "anthropic"
```

Project instructions: `AGENTS.md` or `CLAUDE.md` in project root.

## Comparison

| | microvibe | Vibe (Python) | Claude Code (Node) |
|---|---|---|---|
| Binary | **2.2 MB** | ~50 MB | ~200 MB |
| RAM | **11.5 MB** | ~200 MB | ~300 MB |
| Startup | **<50ms** | ~3-5s | ~2s |
| LOC | **6k** | ~15k | ~50k+ |

## Docker

```bash
docker build -t microvibe .
docker run -e MISTRAL_API_KEY=xxx -v $(pwd):/workspace microvibe -p "hello"
```

## License

MIT
