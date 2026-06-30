# Mistral Vibe Parity

microvibe is complete only when every item in the generated Mistral inventory has an implementation and a passing parity check.

Generate the current inventory:

```bash
dev/extract_mistral_inventory.py
```

The generated JSON is written to `target/parity/mistral_inventory.json`.

Compare Mistral's generated inventory with microvibe's current inventory:

```bash
dev/check_parity_inventory.py
```

Run the full terminal/programmatic parity matrix:

```bash
dev/parity.py --all
```

Run the fast pre-flight gate while iterating:

```bash
dev/quick_check.py --jobs 32
```

That command runs Rust formatting, Python syntax checks, the Rust workspace
tests, inventory parity, and the short parity fast tier. The fast tier samples
CLI, setup, TUI rendering, ACP, tool approval, file/terminal tools, and
programmatic text/JSON/streaming paths. To run the broader pre-final smoke tier:

```bash
dev/parity.py --tier smoke --jobs 32
```

Or run that broader tier through the pre-flight wrapper:

```bash
dev/quick_check.py --jobs 32 --smoke-tier smoke
```

The smoke tier includes ACP prompt/usage/cost, client file/terminal tools,
programmatic JSON, streaming, hooks, MCP, and task coverage.

The default full run uses 32 workers. Override it when local iteration needs a different worker count:

```bash
dev/parity.py --all --jobs 8
```

The harness still serializes startup-only, missing-path CLI, and local slash-command TUI gates. Those cases are sensitive to first-render/input timing under heavy Textual PTY load; the remaining conversation, tool, and programmatic matrix runs with the configured worker count.
Failed full-run cases are retried serially before the run is marked failed, so transient PTY startup/render races do not hide real parity results.
For `--all`, the harness builds the local Rust CLI once before spawning child cases, then disables child rebuilds. Each child case also has a hard subprocess deadline so continuous TUI animation cannot leave a worker running indefinitely.

Required parity gates:

- CLI argument parity with `vibe.cli.entrypoint.parse_arguments`, including flag names, help text, actions, destinations, metavars, choices, and optional-argument arity.
- Slash command parity with `vibe.cli.commands.CommandRegistry`.
- Builtin tool schema, permission, config, display, and result parity with `vibe.core.tools.builtins`; the generated inventory compares tool names, descriptions, permission mode, argument descriptions, config defaults, result fields, required fields, JSON types, and defaults.
- Experimental hook loading and hook behavior parity with `vibe.core.hooks`, including `before_tool` rewrites, `after_tool` result augmentation, and `post_agent_turn` retry injection.
- TUI binding and visible widget state parity with `vibe.cli.textual_ui`.
- `dev/check_parity_inventory.py` compares extracted TUI bindings, not only command and tool names, so new upstream shortcuts must be mirrored or explicitly handled.
- Snapshot parity for startup, basic conversation, streaming, tool approval, tool result, config, model picker, session picker, rewind, MCP/connectors, voice/narrator, onboarding, and errors.
- Programmatic mode parity for text, JSON, and streaming output.
- Session persistence and resume parity.

`dev/parity.py` now renders TUI byte streams into a deterministic terminal screen before diffing. This avoids false diffs from cursor movement, redraw order, and ANSI styling while still comparing the visible terminal content.

Programmatic gates run both CLIs against the same local OpenAI-compatible fake chat server with isolated home/config directories. `programmatic_text` compares the emitted text exactly. `programmatic_json` and `programmatic_streaming` parse the JSON output and normalize only volatile message IDs and the system prompt body while preserving the message shape and fields.

Current TUI gates:

- `default_tui_startup`: default no-argument launch starts the interactive TUI.
- `cli_help`, `cli_version`: argparse-compatible `--help` and `--version` output.
- `cli_output_invalid`, `cli_agent_auto_approve_conflict`: argparse-compatible CLI validation errors for invalid `--output` choices and mutually exclusive `--agent`/`--auto-approve`.
- `cli_agent_not_found`, `cli_agent_disabled`, `cli_agent_enabled_excluded`, `cli_agent_subagent`, `cli_agent_lean_missing`, `cli_default_agent_disabled`, `cli_default_agent_enabled_excluded`: agent selection diagnostics match Vibe for unknown agents, filtered agents, subagents selected as primary agents, install-required agents, and invalid `default_agent` config.
- `cli_workdir_missing`, `cli_add_dir_missing`: missing path validation for `--workdir` and `--add-dir` matches Vibe's error wording and terminal wrapping.
- `cli_check_upgrade_available`: `--check-upgrade` detects a controlled newer version and renders the Vibe upgrade prompt.
- `cli_setup_welcome`, `cli_setup_cancel`, `cli_setup_theme`, `cli_setup_auth_method`, `cli_setup_api_key`: `--setup` onboarding renders Vibe's welcome screen, cancellation message, theme selection preview, browser/manual auth method picker, and manual API-key entry screen.
- `cli_setup_save_api_key`: `--setup` manual API-key entry persists `VIBE_HOME/.env` with Vibe's `python-dotenv` quoting.
- `cli_continue_missing`, `cli_resume_missing`: programmatic `--continue` and `--resume <id>` fail before model execution when no matching session exists, with Vibe's diagnostics.
- `tui_trust_prompt`, `tui_trust_accept`: interactive startup without `--trust` in a folder containing `AGENTS.md` renders Vibe's trust dialog and persists the accepted folder in `trusted_folders.toml`.
- `tui_trust_repo_prompt`, `tui_trust_repo_accept`, `tui_trust_repo_decline`: interactive startup from a nested Git working tree with repo-level `AGENTS.md` renders Vibe's folder-or-repository trust dialog, persists "Trust full repo" at the repo root, and persists decline at the nested cwd.
- `tui_startup`: startup screen after initialization.
- `tui_startup_agent_plan`, `tui_startup_agent_custom`, `tui_startup_auto_approve`: `--agent plan`, a custom primary TOML agent, and `--auto-approve` select the same initial visible agent mode as Vibe.
- `tui_animation_bash_spinner`, `tui_animation_write_file_spinner`, `tui_animation_edit_spinner`, `tui_animation_web_fetch_spinner`, `tui_animation_web_search_spinner`, `tui_animation_task_spinner`, `tui_animation_question_spinner`, `tui_animation_exit_plan_spinner`: forced pending tool or confirmation states capture the raw terminal stream and verify both Vibe and microvibe animate the matching visible status with a rich two-cell braille snake sequence instead of a static or trivial spinner.
- `tui_help`: `/help` command screen.
- `tui_status`: `/status` command screen.
- `tui_data_retention`: `/data-retention` command message.
- `tui_debug_command`, `tui_debug_ctrl_backslash`: debug console toggle through `/debug` and global `Ctrl+\`; these use a raw stream projection because current upstream Vibe renders the dock and then emits a Textual traceback in the PTY harness.
- `tui_mcp`, `tui_connectors`: `/mcp` and `/connectors` with no MCP servers or connectors configured.
- `tui_mcp_status`, `tui_connectors_status`: `/mcp status` and `/connectors status` with no MCP servers configured.
- `tui_mcp_configured`, `tui_connectors_configured`: configured disabled stdio MCP server list rendering, including startup MCP count and zero-tool disabled row.
- `tui_mcp_status_configured`: configured stdio MCP server auth/status rendering.
- `tui_mcp_stdio_tools`, `tui_mcp_stdio_tools_detail`: stdio MCP discovery through a fixture JSON-RPC server, including enabled/disabled tool counts and `/mcp <alias>` detail rows.
- `tui_mcp_login_usage`, `tui_connectors_login_usage`: `/mcp login` and `/connectors login` without an alias.
- `tui_mcp_logout_usage`, `tui_connectors_logout_usage`: `/mcp logout` and `/connectors logout` without an alias.
- `tui_resume_empty`, `tui_continue_empty`: `/resume` and `/continue` with no saved sessions for the directory.
- `tui_resume_one`, `tui_continue_one`: `/resume` and `/continue` with one saved local session render the session picker.
- `tui_resume_select_one`: selecting a saved session from `/resume` restores visible history and emits the resumed message.
- `tui_resume_delete_confirm`: pressing `D` once in `/resume` shows Vibe's delete confirmation row.
- `tui_resume_delete_one`: pressing `D` twice deletes the saved session and verifies both screen output and session-file projection.
- `tui_resume_rename_one`: resume a saved session, run `/rename`, and verify both screen output and persisted title projection.
- `tui_compact_empty`: `/compact` before any conversation history exists.
- `tui_loop_usage`: `/loop` with no scheduled loops.
- `tui_loop_list_empty`: `/loop list` with no scheduled loops.
- `tui_loop_ls_empty`: `/loop ls` with no scheduled loops.
- `tui_loop_cancel_all_empty`: `/loop cancel all` with no scheduled loops.
- `tui_loop_create`, `tui_loop_create_list`, `tui_loop_create_cancel_all`: scheduled-loop creation, list rendering, and cancel-all state changes.
- `tui_loop_invalid_interval`, `tui_loop_too_short`, `tui_loop_missing_prompt`, `tui_loop_prompt_slash`, `tui_loop_cancel_missing`, `tui_loop_cancel_unknown`: scheduled-loop validation errors and usage rendering.
- `tui_rename_usage`: `/rename` without a title.
- `tui_rename_title`: `/rename <title>` on the active session.
- `tui_clear`: `/clear` command message.
- `tui_reload`: `/reload` command message.
- `tui_log`: `/log` command message with normalized session path.
- `tui_copy_empty`: `/copy` before any agent message exists.
- `tui_copy_last_agent`: `/copy` after a completed agent response copies the exact last assistant message.
- `tui_copy_last_agent_xclip`: `/copy` uses Vibe's command-backend fallback path when `xclip` is available and `pbcopy`/`pbpaste` are not.
- `tui_leanstall`: `/leanstall` command message.
- `tui_unleanstall`: `/unleanstall` when Lean is not installed.
- `tui_model_picker`, `tui_model_select_next`: model picker rendering, selection, and persisted active model projection.
- `tui_theme_picker`, `tui_theme_select_next`: theme picker rendering, preview navigation, and persisted theme projection.
- `tui_thinking_picker`, `tui_thinking_select_next`: thinking picker rendering, selection, and persisted thinking projection.
- `tui_config`, `tui_config_toggle_autocopy`, `tui_config_toggle_autocopy_exit`: settings panel rendering and delayed persistence semantics.
- `tui_proxy_setup`: proxy settings panel rendering.
- `tui_proxy_setup_save_http`: proxy settings input saves `HTTP_PROXY` to `VIBE_HOME/.env` with Vibe's `python-dotenv` quoting.
- `tui_proxy_setup_preserve_env`: proxy settings save preserves existing non-proxy `.env` entries while appending the changed proxy variable.
- `tui_proxy_setup_unset_http`: clearing an existing proxy field removes only that proxy variable from `.env` and preserves unrelated entries.
- `tui_voice`, `tui_voice_toggle`, `tui_voice_toggle_exit`: voice settings panel rendering and delayed persistence semantics.
- `tui_rewind_empty`: `/rewind` before any rewindable message exists.
- `tui_rewind_one`, `tui_rewind_select_one`, `tui_rewind_global_ctrl_p`, `tui_rewind_global_ctrl_p_prev`, `tui_rewind_global_ctrl_n`, `tui_rewind_global_alt_up`, `tui_rewind_global_alt_down`: rewind panel rendering, confirm behavior, and global `Ctrl+P`/`Ctrl+N` plus `Alt+Up`/`Alt+Down` browsing across saved user messages.
- `tui_cycle_mode_shift_tab`, `tui_cycle_mode_shift_tab_twice`, `tui_cycle_mode_shift_tab_thrice`, `tui_cycle_mode_shift_tab_custom`: global `Shift+Tab` cycles visible and active agent mode through Vibe's primary-agent order (`default`, `plan`, `accept edits`, `auto approve`, then sorted custom primary agents), including Vibe's async switch timing.
- `tui_ctrl_c_confirm`, `tui_ctrl_d_confirm`: empty-input quit confirmation prompts appear in the raw terminal stream and expire back to the normal footer.
- `tui_ctrl_c_clear_input`: `Ctrl+C` with draft input clears the prompt instead of quitting.
- `tui_ctrl_d_nonempty_no_quit`: `Ctrl+D` with draft input matches Vibe's PTY behavior: it does not trigger quit.
- `tui_ctrl_r_no_insert`, `tui_ctrl_r_voice_enabled_no_insert`: `Ctrl+R` is consumed as Vibe's voice-recording shortcut and never inserts a literal `r`, both with voice mode disabled and enabled.
- `tui_ctrl_y_no_insert`, `tui_ctrl_y_draft_no_insert`: `Ctrl+Y` is consumed as Vibe's copy-selection shortcut and never inserts a literal `y`.
- `tui_ctrl_shift_c_draft_no_clear`: `Ctrl+Shift+C` is consumed as Vibe's copy-selection shortcut and does not clear draft input like `Ctrl+C`.
- `tui_malformed_mouse_ignored`, `tui_malformed_mouse_release_ignored`: malformed SGR mouse reports such as VS Code's `ESC[<32;NaN;NaNM` / `ESC[<35;NaN;NaNm` focus-change noise are ignored instead of leaking characters into the prompt.
- `tui_shift_backspace_left`: prompt editing treats `Shift+Backspace` like Backspace, matching upstream ChatTextArea keybinding coverage.
- `tui_shift_delete_right`: prompt editing deletes the character to the right of the cursor.
- `tui_initial_prompt`: positional `PROMPT` starts the interactive TUI, submits that prompt after startup, and renders the same user/assistant transcript instead of using programmatic mode.
- `tui_prompt_bash_allow_expand_tool`, `tui_prompt_bash_allow_expand_collapse_tool`: global `Ctrl+O` expands and re-collapses Vibe-style bash tool output sections (`▶ N lines` / `▼ show less`).
- `tui_approval_grace_enter`: approval panels ignore an immediate accidental Enter during Vibe's initial input grace period.
- `tui_prompt_read_expand_tool`, `tui_prompt_read_expand_collapse_tool`: global `Ctrl+O` expands and re-collapses Vibe-style `read` output, including stripped line numbers.
- `tui_prompt_file_tools_expand_tool`: global `Ctrl+O` expands Vibe-style `grep` output after the write/edit/grep tool chain.
- `tui_prompt_skill_expand_tool`, `tui_prompt_web_fetch_expand_tool`, `tui_prompt_web_search_expand_tool`, `tui_prompt_question_expand_tool`: global `Ctrl+O` expands generic tool result widgets for skill loading, web fetch, web search, and user-question answers.
- `tui_prompt_bash_allow_y`, `tui_prompt_bash_deny_n`: approval `y`/`n` bindings immediately accept or reject, matching Vibe's approval app shortcuts.
- `tui_prompt_history_up`, `tui_prompt_history_up_down`, `tui_prompt_history_persisted`: prompt history navigation reloads the latest submitted prompt with `Up`, returns to the draft with `Down`, and persists entries across TUI launches via `VIBE_HOME/vibehistory`; the parity harness also compares the exact `vibehistory` file contents.
- `tui_prompt_multiline_ctrl_j`: `Ctrl+J` is handled as Vibe's multiline-input binding and does not leak a literal `j` into the prompt display.
- `tui_prompt_at_file`: TUI `@path` mentions keep the raw visible user prompt while sending Vibe's model-facing resource embedding for small text files.
- `tui_prompt_at_folder`: TUI `@path` mentions for folders keep the raw visible user prompt while sending Vibe's model-facing resource link.
- `tui_prompt_at_image`: TUI `@path` mentions for images keep the raw visible user prompt, render Vibe's attached-image footer, snapshot the image, and send native `image_url` multimodal content to the model.
- `tui_prompt_at_image_no_vision`: TUI image mentions against a non-vision model render Vibe's clear model-support error and do not send a model request.
- `tui_bang_empty`, `tui_bang_bash`, `tui_bang_large_context`: manual `!` shell input matches Vibe's empty-command error, command-output rendering, saved session context injection, and `bash.max_output_bytes` truncation for large injected stdout/stderr.
- `tui_external_editor_input`, `tui_external_editor_empty`: `Ctrl+G` opens the configured external editor for filled and empty input, passes the current buffer through a `vibe_*.md` temp file, strips trailing whitespace, and replaces the prompt with edited content.
- `tui_scroll_shift_up`, `tui_scroll_shift_up_down`: global `Shift+Up` and `Shift+Down` scroll the chat viewport by Vibe's five-line step and restore the bottom view.
- `tui_prompt_todo`, `tui_prompt_todo_empty`: forced `todo.write`/`todo.read` render Vibe's todo result widget, including empty state, status ordering, and icons.
- `tui_slash_skill`: slash skill input expands to the same skill-enriched prompt sent to the model while rendering the same visible user message as Vibe; the harness compares the fake-server request body, not only the screen.
- `tui_prompt_skill`: forced `skill` call renders Vibe's generic skill result and startup skill count.
- `tui_prompt_task`, `tui_prompt_task_allow_explore`, `tui_prompt_task_allow_unknown`, `tui_prompt_task_deny`: forced `task` calls cover Vibe's approval panel for non-allowlisted agents, skipped results, unknown-agent errors, and the allowlisted `explore` subagent success rendering.
- `tui_prompt_web_fetch`: forced `web_fetch` renders Vibe's URL approval flow and fetch result summary.
- `tui_prompt_web_search`: forced `web_search` renders Vibe's search approval flow and search result summary against a deterministic fake Mistral Conversations endpoint.
- `tui_prompt_question`, `tui_prompt_question_other`, `tui_prompt_question_multi`, `tui_prompt_question_multiselect`, `tui_prompt_question_multiselect_other`: `ask_user_question` pauses the turn, renders Vibe's question panel, returns one or multiple selected answers including custom "Other" answers and multi-select answers, and continues the model loop.
- `tui_question_grace_enter`: question panels ignore an immediate accidental Enter during Vibe's initial input grace period.
- `tui_prompt_exit_plan_auto`, `tui_prompt_exit_plan_default`, `tui_prompt_exit_plan_no`, `tui_prompt_exit_plan_editor`: in plan mode, `exit_plan_mode` opens Vibe's plan-review confirmation, handles both "Yes" paths and "No", updates the displayed agent mode when switching, renders the review result, supports `Ctrl+G` opening the plan file in the configured editor, and continues the model loop.
- `tui_teleport_unavailable`, `tui_teleport_ampersand_unavailable`: `/teleport` and `&target` Teleport entrypoints render Vibe's unavailable-subscription error without starting a model turn.

Current programmatic gates:

- `programmatic_text`: `vibe -p hi --output text` and `microvibe -p hi --output text` emit the same final assistant response.
- `programmatic_empty_prompt_text`: `-p` without a value runs programmatic mode with an empty prompt, matching Vibe's `argparse` `const=""` behavior.
- `programmatic_json`: `--output json` emits the same normalized conversation-message sequence and field shape.
- `programmatic_streaming`: `--output streaming` emits the same normalized newline-delimited message sequence and field shape.
- `programmatic_agent_custom_*`: custom primary TOML agents are discovered from `$VIBE_HOME/agents`, selected through `--agent`, and apply their tool profile in text, JSON, and streaming output modes.
- `programmatic_read_text`: a forced model `read` tool call followed by a final assistant response emits the same text output.
- `programmatic_read_json`: the same forced `read` call emits the same normalized message sequence, including the assistant tool call and tool result.
- `programmatic_read_streaming`: the forced `read` call emits the same normalized newline-delimited message sequence.
- `programmatic_tools_text`: forced `bash`, `write_file`, `edit`, and `grep` tool calls followed by a final assistant response emit the same text output.
- `programmatic_tools_json`: the same multi-tool chain emits the same normalized message sequence, including tool result text.
- `programmatic_tools_streaming`: the multi-tool chain emits the same normalized newline-delimited message sequence.
- `programmatic_hooks_before_json`: `enable_experimental_hooks` plus a `before_tool` hook rewrites a `read` call's `tool_input`; the harness compares both normalized JSON output and the next model request's rewritten tool result.
- `programmatic_hooks_after_json`: an `after_tool` hook appends `additional_context` to a `read` result; the harness compares both normalized JSON output and the next model request's augmented tool result.
- `programmatic_hooks_post_json`: a `post_agent_turn` hook denies the first assistant turn, injects Vibe's retry user message with `injected=true`, reruns the model turn, and then allows completion; the harness compares JSON output and the fake-server request sequence.
- `programmatic_mcp_stdio_*`: configured stdio MCP tools are discovered, exposed to the model as `alias_tool`, executed via `tools/call`, and serialized with Vibe's MCP result text in text, JSON, and streaming modes.
- `programmatic_enabled_tools_*`: `--enabled-tools` allowlists exact, glob, and regex-compatible tool names and hides non-matching tool calls with Vibe's unknown-tool result.
- `programmatic_max_turns_*`, `programmatic_max_tokens_*`, `programmatic_max_price_*`: programmatic conversation limits stop the model loop with Vibe's `<vibe_stop_event>` output in text, JSON, and streaming modes, including streaming's mixed JSON-lines-plus-stop-event shape.
- `programmatic_state_*`: forced `todo` write/read calls preserve Vibe's Pydantic-style output.
- `programmatic_web_fetch_*`: forced `web_fetch` call against the local HTTP fixture preserves URL/content/type/truncation output.
- `programmatic_web_search_*`: forced `web_search` call against the local fake Mistral Conversations endpoint preserves answer and source serialization.
- `programmatic_skill_*`: forced `skill` call loads the fixture skill and preserves injected content/file-list output.
- `programmatic_question_*`, `programmatic_exit_plan_*`: non-interactive programmatic mode disables interactive-only tools with Vibe-compatible unknown-tool output.
- `programmatic_task_unknown_*`: unknown subagent errors match Vibe's task tool output.
- `programmatic_task_custom_*`: custom TOML subagents are discovered from `$VIBE_HOME/agents`, validated by the `task` tool, and run through the same text/JSON/streaming output paths as Vibe.
- `programmatic_continue_json`: two-run session flow where the second run uses `--continue`; compares JSON output and a projected `meta.json`/`messages.jsonl` shape.
- `programmatic_resume_id_json`: two-run session flow where the second run uses `--resume <session_id>`; compares JSON output and the same session-file projection.

The harness normalizes animated petit-chat frames, randomized loading labels/glyphs, and startup `Initializing...` races for static screen gates. Dedicated animation gates verify that every current pending approval, tool, question, and plan-confirmation status class actually animates with Vibe-style braille snake frame richness and shape. Future gates still need to tighten timing parity; Vibe's upstream snake direction includes randomness, so exact frame order is not a stable oracle without controlling that RNG.

The old Rust prototype is not a parity source. Mistral Vibe upstream is the oracle.
