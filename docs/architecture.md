# microvibe Rust Architecture

This project has two upstream references:

- `../openai-codex-upstream`: Rust architecture reference.
- `../mistral-vibe-upstream`: behavior, UI, wording, command, and tool parity reference.

The goal is not to clone Codex behavior. Codex is the structural reference; Mistral Vibe is the product reference.

## Upstream Revisions

- OpenAI Codex: `a0d5fd772e` on `main`.
- Mistral Vibe: `725d3a56ce`, tag `v2.17.1`, on `main`.

## Restart Decision

The old single-crate implementation has been moved to `legacy/microvibe-old-src/`.
It is not part of the workspace and must not drive new design decisions. It can be consulted only as a historical feature inventory.

## Target Shape

Codex separates protocol, config, core agent logic, tool execution, and TUI into independent crates. microvibe should move in the same direction while keeping Mistral Vibe-compatible behavior.

Target workspace:

```text
microvibe/
  crates/
    microvibe-protocol/    # message, event, tool-call, approval, session types
    microvibe-config/      # config, env, project/user config discovery
    microvibe-core/        # agent/session loop, provider calls, hooks, orchestration
    microvibe-tools/       # builtin tools and approval policy
    microvibe-tui/         # ratatui event loop and terminal rendering
    microvibe-cli/         # clap entrypoint and command dispatch
  dev/
    parity.py              # local Vibe vs microvibe terminal harness
```

## Parity Contract

Mistral Vibe is the oracle for:

- CLI flags and command names.
- Slash command names, labels, descriptions, and modal behavior.
- Tool names, tool schemas, permission wording, progress text, and result summaries.
- Hook config discovery, subprocess invocation payloads, tool rewrites, denials, result replacement, and UI events.
- TUI visible text, colors where practical, order of messages, spinners, borders, and terminal titles.
- Session, config, onboarding, and error wording.

The parity harness should be used before and after every UI-visible change. Any deliberate difference must be recorded in this document with the exact reason.

## Refactor Order

1. Add parity harness and snapshot fixtures.
2. Move shared types into `microvibe-protocol`.
3. Move config loading into `microvibe-config`.
4. Move agent loop and session state into `microvibe-core`.
5. Move builtin tools and approval rules into `microvibe-tools`.
6. Move render-only state and static UI strings into `microvibe-ui`.
7. Keep `microvibe-cli` and `microvibe-tui` as thin adapters.

This order keeps the current binary working while isolating behavior behind testable boundaries.

## Local Oracles

Useful Mistral files:

- `../mistral-vibe-upstream/vibe/cli/textual_ui/app.py`
- `../mistral-vibe-upstream/vibe/cli/textual_ui/app.tcss`
- `../mistral-vibe-upstream/vibe/cli/textual_ui/widgets/`
- `../mistral-vibe-upstream/vibe/core/agent_loop.py`
- `../mistral-vibe-upstream/vibe/core/hooks/`
- `../mistral-vibe-upstream/vibe/core/agent_loop_hooks.py`
- `../mistral-vibe-upstream/vibe/core/tools/builtins/`
- `../mistral-vibe-upstream/tests/snapshots/`
- `../mistral-vibe-upstream/tests/cli/`

Useful Codex files:

- `../openai-codex-upstream/codex-rs/Cargo.toml`
- `../openai-codex-upstream/codex-rs/core/src/session/`
- `../openai-codex-upstream/codex-rs/protocol/`
- `../openai-codex-upstream/codex-rs/config/`
- `../openai-codex-upstream/codex-rs/tools/`
- `../openai-codex-upstream/codex-rs/tui/`
