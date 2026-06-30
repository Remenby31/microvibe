# microvibe

Rust reimplementation of Mistral Vibe, restarted from a clean Codex-style architecture.

References checked out next to this repo:

- `../openai-codex-upstream`: Rust workspace and agent architecture reference.
- `../mistral-vibe-upstream`: behavior, UI, wording, and tool parity oracle.

The previous single-crate prototype is archived in `legacy/microvibe-old-src/` and is not compiled.

## Current Shape

```text
crates/
  microvibe-protocol/  # messages, content blocks, tool calls, events
  microvibe-config/    # config loading and project instructions
  microvibe-core/      # session and agent loop
  microvibe-tools/     # builtin tool registry
  microvibe-tui/       # terminal UI adapter
  microvibe-cli/       # binary entrypoint
```

## Build

```bash
cargo check
cargo run -- --help
```

## Run

```bash
cargo run -- --init
MISTRAL_API_KEY=... cargo run -- -p "hello"
cargo run -- --tui
```

## Parity Harness

```bash
dev/parity.py --case startup
dev/parity.py --case tui_help
dev/check_parity_inventory.py
```

Captured raw transcripts and diffs are written to `target/parity/`.
For TUI cases, the harness compares the rendered terminal screen, not the raw ANSI stream.

See `docs/architecture.md` for the restart plan and parity contract.
