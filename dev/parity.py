#!/usr/bin/env python3
"""Capture and compare Vibe and microvibe terminal transcripts.

This is intentionally dependency-free. It runs each command in its own pseudo
terminal with isolated HOME/XDG directories and a fixed terminal size.

Examples:
  dev/parity.py --case startup
  VIBE_CMD="../mistral-vibe-upstream/.venv/bin/vibe" \
  MICROVIBE_CMD="./target/debug/microvibe" \
  dev/parity.py --case tui_help --mode tui
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
import difflib
import http.server
import json
import os
import pathlib
import pty
import queue
import re
import select
import shlex
import shutil
import signal
import socketserver
import fcntl
import struct
import subprocess
import sys
import tempfile
import termios
import textwrap
import threading
import time
import tomllib
import tty
import typing
from dataclasses import dataclass


ROOT = pathlib.Path(__file__).resolve().parents[1]
OUT_DIR = ROOT / "target" / "parity"
ANSI_RE = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07]*(?:\x07|\x1b\\)")
CSI_RE = re.compile(r"\x1b\[([0-?]*)([ -/]*)([@-~])")


@dataclass(frozen=True)
class Case:
    name: str
    mode: str
    input_text: bytes
    settle: float = 0.8
    timeout: float = 8.0


CASES: dict[str, Case] = {
    "startup": Case("startup", "default_tui", b"", settle=1.0, timeout=5.0),
    "default_tui_startup": Case("default_tui_startup", "default_tui", b"", settle=1.0, timeout=5.0),
    "cli_help": Case("cli_help", "cli_help", b"", settle=1.0, timeout=5.0),
    "cli_version": Case("cli_version", "cli_version", b"", settle=1.0, timeout=5.0),
    "cli_output_invalid": Case("cli_output_invalid", "cli_output_invalid", b"", settle=1.0, timeout=5.0),
    "cli_agent_auto_approve_conflict": Case("cli_agent_auto_approve_conflict", "cli_agent_auto_approve_conflict", b"", settle=1.0, timeout=5.0),
    "cli_agent_not_found": Case("cli_agent_not_found", "cli_agent_not_found", b"", settle=1.0, timeout=5.0),
    "cli_agent_disabled": Case("cli_agent_disabled", "cli_agent_disabled", b"", settle=1.0, timeout=5.0),
    "cli_agent_enabled_excluded": Case("cli_agent_enabled_excluded", "cli_agent_enabled_excluded", b"", settle=1.0, timeout=5.0),
    "cli_agent_subagent": Case("cli_agent_subagent", "cli_agent_subagent", b"", settle=1.0, timeout=5.0),
    "cli_agent_lean_missing": Case("cli_agent_lean_missing", "cli_agent_lean_missing", b"", settle=1.0, timeout=5.0),
    "cli_default_agent_disabled": Case("cli_default_agent_disabled", "cli_default_agent_disabled", b"", settle=1.0, timeout=5.0),
    "cli_default_agent_enabled_excluded": Case("cli_default_agent_enabled_excluded", "cli_default_agent_enabled_excluded", b"", settle=1.0, timeout=5.0),
    "cli_workdir_missing": Case("cli_workdir_missing", "cli_workdir_missing", b"", settle=1.0, timeout=5.0),
    "cli_add_dir_missing": Case("cli_add_dir_missing", "cli_add_dir_missing", b"", settle=1.0, timeout=5.0),
    "cli_check_upgrade_available": Case("cli_check_upgrade_available", "cli_check_upgrade_available", b"\x1b[C\r", settle=1.0, timeout=5.0),
    "cli_setup_welcome": Case("cli_setup_welcome", "cli_setup", b"", settle=6.0, timeout=10.0),
    "cli_setup_cancel": Case("cli_setup_cancel", "cli_setup", b"\x03", settle=1.0, timeout=5.0),
    "cli_setup_theme": Case("cli_setup_theme", "cli_setup", b"\r", settle=1.0, timeout=12.0),
    "cli_setup_auth_method": Case("cli_setup_auth_method", "cli_setup", b"\r\r", settle=1.0, timeout=12.0),
    "cli_setup_api_key": Case("cli_setup_api_key", "cli_setup", b"\r\r\x1b[B\r", settle=1.0, timeout=15.0),
    "cli_setup_save_api_key": Case("cli_setup_save_api_key", "cli_setup", b"\r\r\x1b[B\rsk-parity-key\r", settle=1.0, timeout=18.0),
    "acp_help": Case("acp_help", "acp_help", b"", settle=0.0, timeout=5.0),
    "acp_version": Case("acp_version", "acp_version", b"", settle=0.0, timeout=5.0),
    "acp_initialize": Case("acp_initialize", "acp_initialize", b"", settle=0.0, timeout=10.0),
    "acp_new_session": Case("acp_new_session", "acp_new_session", b"", settle=0.0, timeout=10.0),
    "acp_list_sessions_empty": Case("acp_list_sessions_empty", "acp_list_sessions_empty", b"", settle=0.0, timeout=10.0),
    "acp_list_sessions_seeded": Case("acp_list_sessions_seeded", "acp_list_sessions_seeded", b"", settle=0.0, timeout=10.0),
    "acp_list_sessions_cwd_filter": Case("acp_list_sessions_cwd_filter", "acp_list_sessions_cwd_filter", b"", settle=0.0, timeout=10.0),
    "acp_list_sessions_sorted": Case("acp_list_sessions_sorted", "acp_list_sessions_sorted", b"", settle=0.0, timeout=10.0),
    "acp_list_sessions_skip_invalid": Case("acp_list_sessions_skip_invalid", "acp_list_sessions_skip_invalid", b"", settle=0.0, timeout=10.0),
    "acp_list_sessions_timestamps": Case("acp_list_sessions_timestamps", "acp_list_sessions_timestamps", b"", settle=0.0, timeout=10.0),
    "acp_load_session": Case("acp_load_session", "acp_load_session", b"", settle=0.0, timeout=10.0),
    "acp_load_rich_session": Case("acp_load_rich_session", "acp_load_rich_session", b"", settle=0.0, timeout=10.0),
    "acp_load_replay_ids": Case("acp_load_replay_ids", "acp_load_replay_ids", b"", settle=0.0, timeout=10.0),
    "acp_load_missing": Case("acp_load_missing", "acp_load_missing", b"", settle=0.0, timeout=10.0),
    "acp_fork_session": Case("acp_fork_session", "acp_fork_session", b"", settle=0.0, timeout=10.0),
    "acp_fork_from_prompt_message": Case("acp_fork_from_prompt_message", "acp_fork_from_prompt_message", b"", settle=0.0, timeout=25.0),
    "acp_fork_missing": Case("acp_fork_missing", "acp_fork_missing", b"", settle=0.0, timeout=10.0),
    "acp_set_mode_fork_default": Case("acp_set_mode_fork_default", "acp_set_mode_fork_default", b"", settle=0.0, timeout=10.0),
    "acp_set_mode_fork_auto_approve": Case("acp_set_mode_fork_auto_approve", "acp_set_mode_fork_auto_approve", b"", settle=0.0, timeout=10.0),
    "acp_set_mode_fork_plan": Case("acp_set_mode_fork_plan", "acp_set_mode_fork_plan", b"", settle=0.0, timeout=10.0),
    "acp_set_mode_fork_accept_edits": Case("acp_set_mode_fork_accept_edits", "acp_set_mode_fork_accept_edits", b"", settle=0.0, timeout=10.0),
    "acp_set_mode_fork_chat": Case("acp_set_mode_fork_chat", "acp_set_mode_fork_chat", b"", settle=0.0, timeout=10.0),
    "acp_set_mode_fork_invalid": Case("acp_set_mode_fork_invalid", "acp_set_mode_fork_invalid", b"", settle=0.0, timeout=10.0),
    "acp_set_mode_fork_empty": Case("acp_set_mode_fork_empty", "acp_set_mode_fork_empty", b"", settle=0.0, timeout=10.0),
    "acp_prompt_simple": Case("acp_prompt_simple", "acp_prompt_simple", b"", settle=0.0, timeout=20.0),
    "acp_prompt_client_message_id": Case("acp_prompt_client_message_id", "acp_prompt_client_message_id", b"", settle=0.0, timeout=20.0),
    "acp_prompt_agent_thought": Case("acp_prompt_agent_thought", "acp_prompt_agent_thought", b"", settle=0.0, timeout=20.0),
    "acp_prompt_usage_accumulates": Case("acp_prompt_usage_accumulates", "acp_prompt_usage_accumulates", b"", settle=0.0, timeout=20.0),
    "acp_prompt_usage_cost": Case("acp_prompt_usage_cost", "acp_prompt_usage_cost", b"", settle=0.0, timeout=20.0),
    "acp_prompt_missing_session": Case("acp_prompt_missing_session", "acp_prompt_missing_session", b"", settle=0.0, timeout=10.0),
    "acp_prompt_image": Case("acp_prompt_image", "acp_prompt_image", b"", settle=0.0, timeout=20.0),
    "acp_prompt_image_wrong_type": Case("acp_prompt_image_wrong_type", "acp_prompt_image_wrong_type", b"", settle=0.0, timeout=10.0),
    "acp_prompt_image_invalid_base64": Case("acp_prompt_image_invalid_base64", "acp_prompt_image_invalid_base64", b"", settle=0.0, timeout=10.0),
    "acp_command_help": Case("acp_command_help", "acp_command_help", b"", settle=0.0, timeout=20.0),
    "acp_command_reload": Case("acp_command_reload", "acp_command_reload", b"", settle=0.0, timeout=20.0),
    "acp_command_compact_empty": Case("acp_command_compact_empty", "acp_command_compact_empty", b"", settle=0.0, timeout=20.0),
    "acp_command_compact_one": Case("acp_command_compact_one", "acp_command_compact_one", b"", settle=0.0, timeout=25.0),
    "acp_command_teleport_no_history": Case("acp_command_teleport_no_history", "acp_command_teleport_no_history", b"", settle=0.0, timeout=20.0),
    "acp_command_data_retention": Case("acp_command_data_retention", "acp_command_data_retention", b"", settle=0.0, timeout=20.0),
    "acp_command_proxy_help": Case("acp_command_proxy_help", "acp_command_proxy_help", b"", settle=0.0, timeout=20.0),
    "acp_command_proxy_set": Case("acp_command_proxy_set", "acp_command_proxy_set", b"", settle=0.0, timeout=20.0),
    "acp_command_proxy_unset": Case("acp_command_proxy_unset", "acp_command_proxy_unset", b"", settle=0.0, timeout=20.0),
    "acp_command_proxy_invalid": Case("acp_command_proxy_invalid", "acp_command_proxy_invalid", b"", settle=0.0, timeout=20.0),
    "acp_command_proxy_case": Case("acp_command_proxy_case", "acp_command_proxy_case", b"", settle=0.0, timeout=20.0),
    "acp_prompt_grep": Case("acp_prompt_grep", "acp_prompt_grep", b"", settle=0.0, timeout=25.0),
    "acp_permission_grep_allow": Case("acp_permission_grep_allow", "acp_permission_grep_allow", b"", settle=0.0, timeout=25.0),
    "acp_permission_grep_deny": Case("acp_permission_grep_deny", "acp_permission_grep_deny", b"", settle=0.0, timeout=25.0),
    "acp_permission_grep_cancelled": Case("acp_permission_grep_cancelled", "acp_permission_grep_cancelled", b"", settle=0.0, timeout=25.0),
    "acp_permission_grep_allow_always": Case("acp_permission_grep_allow_always", "acp_permission_grep_allow_always", b"", settle=0.0, timeout=25.0),
    "acp_permission_grep_allow_always_permanent": Case("acp_permission_grep_allow_always_permanent", "acp_permission_grep_allow_always_permanent", b"", settle=0.0, timeout=25.0),
    "acp_permission_bash_granular": Case("acp_permission_bash_granular", "acp_permission_bash_granular", b"", settle=0.0, timeout=25.0),
    "acp_permission_bash_granular_allow_always_permanent": Case("acp_permission_bash_granular_allow_always_permanent", "acp_permission_bash_granular_allow_always_permanent", b"", settle=0.0, timeout=25.0),
    "acp_fs_read": Case("acp_fs_read", "acp_fs_read", b"", settle=0.0, timeout=25.0),
    "acp_fs_read_range": Case("acp_fs_read_range", "acp_fs_read_range", b"", settle=0.0, timeout=25.0),
    "acp_fs_write": Case("acp_fs_write", "acp_fs_write", b"", settle=0.0, timeout=25.0),
    "acp_fs_edit": Case("acp_fs_edit", "acp_fs_edit", b"", settle=0.0, timeout=25.0),
    "acp_terminal_bash_allow": Case("acp_terminal_bash_allow", "acp_terminal_bash_allow", b"", settle=0.0, timeout=25.0),
    "acp_terminal_bash_nonzero": Case("acp_terminal_bash_nonzero", "acp_terminal_bash_nonzero", b"", settle=0.0, timeout=25.0),
    "acp_terminal_bash_none_exit": Case("acp_terminal_bash_none_exit", "acp_terminal_bash_none_exit", b"", settle=0.0, timeout=25.0),
    "acp_terminal_bash_timeout": Case("acp_terminal_bash_timeout", "acp_terminal_bash_timeout", b"", settle=0.0, timeout=25.0),
    "acp_tool_meta_web_fetch": Case("acp_tool_meta_web_fetch", "acp_tool_meta_web_fetch", b"", settle=0.0, timeout=25.0),
    "acp_tool_meta_web_search": Case("acp_tool_meta_web_search", "acp_tool_meta_web_search", b"", settle=0.0, timeout=25.0),
    "acp_tool_meta_skill": Case("acp_tool_meta_skill", "acp_tool_meta_skill", b"", settle=0.0, timeout=25.0),
    "acp_tool_meta_task": Case("acp_tool_meta_task", "acp_tool_meta_task", b"", settle=0.0, timeout=30.0),
    "acp_prompt_todo": Case("acp_prompt_todo", "acp_prompt_todo", b"", settle=0.0, timeout=25.0),
    "acp_prompt_todo_invalid": Case("acp_prompt_todo_invalid", "acp_prompt_todo_invalid", b"", settle=0.0, timeout=25.0),
    "acp_user_display_content": Case("acp_user_display_content", "acp_user_display_content", b"", settle=0.0, timeout=25.0),
    "acp_close_session": Case("acp_close_session", "acp_close_session", b"", settle=0.0, timeout=10.0),
    "acp_close_missing": Case("acp_close_missing", "acp_close_missing", b"", settle=0.0, timeout=10.0),
    "acp_set_title_live_unsaved": Case("acp_set_title_live_unsaved", "acp_set_title_live_unsaved", b"", settle=0.0, timeout=10.0),
    "acp_set_title_saved": Case("acp_set_title_saved", "acp_set_title_saved", b"", settle=0.0, timeout=10.0),
    "acp_delete_saved": Case("acp_delete_saved", "acp_delete_saved", b"", settle=0.0, timeout=10.0),
    "acp_delete_missing": Case("acp_delete_missing", "acp_delete_missing", b"", settle=0.0, timeout=10.0),
    "acp_delete_invalid_missing": Case("acp_delete_invalid_missing", "acp_delete_invalid_missing", b"", settle=0.0, timeout=10.0),
    "acp_delete_invalid_empty": Case("acp_delete_invalid_empty", "acp_delete_invalid_empty", b"", settle=0.0, timeout=10.0),
    "acp_delete_invalid_saved_session_id": Case("acp_delete_invalid_saved_session_id", "acp_delete_invalid_saved_session_id", b"", settle=0.0, timeout=10.0),
    "acp_delete_saved_pointer": Case("acp_delete_saved_pointer", "acp_delete_saved_pointer", b"", settle=0.0, timeout=10.0),
    "acp_delete_exact_collision": Case("acp_delete_exact_collision", "acp_delete_exact_collision", b"", settle=0.0, timeout=10.0),
    "acp_delete_live_unsaved": Case("acp_delete_live_unsaved", "acp_delete_live_unsaved", b"", settle=0.0, timeout=10.0),
    "acp_delete_loaded_saved": Case("acp_delete_loaded_saved", "acp_delete_loaded_saved", b"", settle=0.0, timeout=10.0),
    "acp_auth_status_signed_out": Case("acp_auth_status_signed_out", "acp_auth_status_signed_out", b"", settle=0.0, timeout=10.0),
    "acp_auth_status_process_env": Case("acp_auth_status_process_env", "acp_auth_status_process_env", b"", settle=0.0, timeout=10.0),
    "acp_auth_status_dotenv": Case("acp_auth_status_dotenv", "acp_auth_status_dotenv", b"", settle=0.0, timeout=10.0),
    "acp_auth_status_process_over_dotenv": Case("acp_auth_status_process_over_dotenv", "acp_auth_status_process_over_dotenv", b"", settle=0.0, timeout=10.0),
    "acp_auth_signout_dotenv": Case("acp_auth_signout_dotenv", "acp_auth_signout_dotenv", b"", settle=0.0, timeout=10.0),
    "acp_auth_signout_process_over_dotenv": Case("acp_auth_signout_process_over_dotenv", "acp_auth_signout_process_over_dotenv", b"", settle=0.0, timeout=10.0),
    "acp_authenticate_unsupported": Case("acp_authenticate_unsupported", "acp_authenticate_unsupported", b"", settle=0.0, timeout=10.0),
    "acp_initialize_unsupported_provider": Case("acp_initialize_unsupported_provider", "acp_initialize_unsupported_provider", b"", settle=0.0, timeout=10.0),
    "acp_authenticate_browser_unsupported": Case("acp_authenticate_browser_unsupported", "acp_authenticate_browser_unsupported", b"", settle=0.0, timeout=10.0),
    "acp_authenticate_browser_complete": Case("acp_authenticate_browser_complete", "acp_authenticate_browser_complete", b"", settle=0.0, timeout=10.0),
    "acp_authenticate_browser_unsupported_action": Case("acp_authenticate_browser_unsupported_action", "acp_authenticate_browser_unsupported_action", b"", settle=0.0, timeout=10.0),
    "acp_initialize_delegated_browser_auth": Case("acp_initialize_delegated_browser_auth", "acp_initialize_delegated_browser_auth", b"", settle=0.0, timeout=10.0),
    "acp_authenticate_delegated_start": Case("acp_authenticate_delegated_start", "acp_authenticate_delegated_start", b"", settle=0.0, timeout=10.0),
    "acp_authenticate_delegated_complete": Case("acp_authenticate_delegated_complete", "acp_authenticate_delegated_complete", b"", settle=0.0, timeout=10.0),
    "acp_authenticate_delegated_missing_attempt": Case("acp_authenticate_delegated_missing_attempt", "acp_authenticate_delegated_missing_attempt", b"", settle=0.0, timeout=10.0),
    "acp_authenticate_delegated_unknown_attempt": Case("acp_authenticate_delegated_unknown_attempt", "acp_authenticate_delegated_unknown_attempt", b"", settle=0.0, timeout=10.0),
    "acp_authenticate_delegated_unsupported_action": Case("acp_authenticate_delegated_unsupported_action", "acp_authenticate_delegated_unsupported_action", b"", settle=0.0, timeout=10.0),
    "acp_trust_status_untrusted": Case("acp_trust_status_untrusted", "acp_trust_status_untrusted", b"", settle=0.0, timeout=10.0),
    "acp_trust_status_repo": Case("acp_trust_status_repo", "acp_trust_status_repo", b"", settle=0.0, timeout=10.0),
    "acp_trust_decision_cwd": Case("acp_trust_decision_cwd", "acp_trust_decision_cwd", b"", settle=0.0, timeout=10.0),
    "acp_trust_decision_repo": Case("acp_trust_decision_repo", "acp_trust_decision_repo", b"", settle=0.0, timeout=10.0),
    "acp_trust_decision_invalid": Case("acp_trust_decision_invalid", "acp_trust_decision_invalid", b"", settle=0.0, timeout=10.0),
    "acp_trust_decision_missing_session": Case("acp_trust_decision_missing_session", "acp_trust_decision_missing_session", b"", settle=0.0, timeout=10.0),
    "acp_set_mode_valid": Case("acp_set_mode_valid", "acp_set_mode_valid", b"", settle=0.0, timeout=10.0),
    "acp_set_mode_invalid": Case("acp_set_mode_invalid", "acp_set_mode_invalid", b"", settle=0.0, timeout=10.0),
    "acp_set_model_valid": Case("acp_set_model_valid", "acp_set_model_valid", b"", settle=0.0, timeout=10.0),
    "acp_set_model_invalid": Case("acp_set_model_invalid", "acp_set_model_invalid", b"", settle=0.0, timeout=10.0),
    "acp_set_model_same": Case("acp_set_model_same", "acp_set_model_same", b"", settle=0.0, timeout=10.0),
    "acp_set_model_empty": Case("acp_set_model_empty", "acp_set_model_empty", b"", settle=0.0, timeout=10.0),
    "acp_set_config_mode": Case("acp_set_config_mode", "acp_set_config_mode", b"", settle=0.0, timeout=10.0),
    "acp_set_config_mode_empty": Case("acp_set_config_mode_empty", "acp_set_config_mode_empty", b"", settle=0.0, timeout=10.0),
    "acp_set_config_model": Case("acp_set_config_model", "acp_set_config_model", b"", settle=0.0, timeout=10.0),
    "acp_set_config_model_empty": Case("acp_set_config_model_empty", "acp_set_config_model_empty", b"", settle=0.0, timeout=10.0),
    "acp_set_config_thinking": Case("acp_set_config_thinking", "acp_set_config_thinking", b"", settle=0.0, timeout=10.0),
    "acp_set_config_thinking_invalid": Case("acp_set_config_thinking_invalid", "acp_set_config_thinking_invalid", b"", settle=0.0, timeout=10.0),
    "acp_set_config_thinking_empty": Case("acp_set_config_thinking_empty", "acp_set_config_thinking_empty", b"", settle=0.0, timeout=10.0),
    "acp_set_config_max_turns": Case("acp_set_config_max_turns", "acp_set_config_max_turns", b"", settle=0.0, timeout=10.0),
    "acp_set_config_max_turns_invalid": Case("acp_set_config_max_turns_invalid", "acp_set_config_max_turns_invalid", b"", settle=0.0, timeout=10.0),
    "acp_set_config_max_turns_bool": Case("acp_set_config_max_turns_bool", "acp_set_config_max_turns_bool", b"", settle=0.0, timeout=10.0),
    "acp_set_config_invalid_id": Case("acp_set_config_invalid_id", "acp_set_config_invalid_id", b"", settle=0.0, timeout=10.0),
    "acp_set_config_empty_id": Case("acp_set_config_empty_id", "acp_set_config_empty_id", b"", settle=0.0, timeout=10.0),
    "acp_telemetry_notification": Case("acp_telemetry_notification", "acp_telemetry_notification", b"", settle=0.0, timeout=10.0),
    "acp_unknown_notification": Case("acp_unknown_notification", "acp_unknown_notification", b"", settle=0.0, timeout=10.0),
    "cli_continue_missing": Case("cli_continue_missing", "cli_continue_missing", b"", settle=1.0, timeout=5.0),
    "cli_resume_missing": Case("cli_resume_missing", "cli_resume_missing", b"", settle=1.0, timeout=5.0),
    "tui_trust_prompt": Case("tui_trust_prompt", "tui_untrusted_workspace", b"", settle=1.0, timeout=5.0),
    "tui_trust_accept": Case("tui_trust_accept", "tui_untrusted_workspace", b"1", settle=1.0, timeout=5.0),
    "tui_trust_repo_prompt": Case("tui_trust_repo_prompt", "tui_untrusted_workspace", b"", settle=1.0, timeout=5.0),
    "tui_trust_repo_accept": Case("tui_trust_repo_accept", "tui_untrusted_workspace", b"1", settle=1.0, timeout=5.0),
    "tui_trust_repo_decline": Case("tui_trust_repo_decline", "tui_untrusted_workspace", b"3", settle=1.0, timeout=5.0),
    "tui_startup": Case("tui_startup", "tui", b"", settle=1.0, timeout=5.0),
    "tui_startup_agent_plan": Case("tui_startup_agent_plan", "tui_agent_plan", b"", settle=3.0, timeout=10.0),
    "tui_startup_agent_custom": Case("tui_startup_agent_custom", "tui_agent_custom", b"", settle=3.0, timeout=10.0),
    "tui_startup_auto_approve": Case("tui_startup_auto_approve", "tui_auto_approve", b"", settle=3.0, timeout=10.0),
    "tui_help": Case("tui_help", "tui", b"/help\x1b\r", settle=1.0, timeout=5.0),
    "tui_status": Case("tui_status", "tui", b"/status\x1b\r", settle=1.0, timeout=5.0),
    "tui_data_retention": Case("tui_data_retention", "tui", b"/data-retention\x1b\r", settle=1.0, timeout=5.0),
    "tui_debug_command": Case("tui_debug_command", "tui", b"/debug\x1b\r", settle=1.0, timeout=5.0),
    "tui_debug_ctrl_backslash": Case("tui_debug_ctrl_backslash", "tui", b"\x1c", settle=1.0, timeout=5.0),
    "tui_mcp": Case("tui_mcp", "tui", b"/mcp\x1b\r", settle=1.0, timeout=5.0),
    "tui_mcp_status": Case("tui_mcp_status", "tui", b"/mcp status\x1b\r", settle=1.0, timeout=5.0),
    "tui_mcp_configured": Case("tui_mcp_configured", "tui", b"/mcp\x1b\r", settle=1.0, timeout=8.0),
    "tui_mcp_status_configured": Case("tui_mcp_status_configured", "tui", b"/mcp status\x1b\r", settle=1.0, timeout=8.0),
    "tui_mcp_stdio_tools": Case("tui_mcp_stdio_tools", "tui", b"/mcp\x1b\r", settle=2.0, timeout=12.0),
    "tui_mcp_stdio_tools_detail": Case("tui_mcp_stdio_tools_detail", "tui", b"/mcp local-demo\x1b\r", settle=2.0, timeout=12.0),
    "tui_mcp_disable_server": Case("tui_mcp_disable_server", "tui", b"/mcp\x1b\rd", settle=1.0, timeout=12.0),
    "tui_mcp_enable_server": Case("tui_mcp_enable_server", "tui", b"/mcp\x1b\re", settle=1.0, timeout=8.0),
    "tui_mcp_disable_tool": Case("tui_mcp_disable_tool", "tui", b"/mcp local-demo\x1b\rd", settle=1.0, timeout=12.0),
    "tui_mcp_enable_tool": Case("tui_mcp_enable_tool", "tui", b"/mcp local-demo\x1b\re", settle=1.0, timeout=12.0),
    "tui_mcp_login_usage": Case("tui_mcp_login_usage", "tui", b"/mcp login\x1b\r", settle=1.0, timeout=5.0),
    "tui_mcp_logout_usage": Case("tui_mcp_logout_usage", "tui", b"/mcp logout\x1b\r", settle=1.0, timeout=5.0),
    "tui_connectors": Case("tui_connectors", "tui", b"/connectors\x1b\r", settle=1.0, timeout=5.0),
    "tui_connectors_status": Case("tui_connectors_status", "tui", b"/connectors status\x1b\r", settle=1.0, timeout=5.0),
    "tui_connectors_configured": Case("tui_connectors_configured", "tui", b"/connectors\x1b\r", settle=1.0, timeout=8.0),
    "tui_connectors_login_usage": Case("tui_connectors_login_usage", "tui", b"/connectors login\x1b\r", settle=1.0, timeout=5.0),
    "tui_connectors_logout_usage": Case("tui_connectors_logout_usage", "tui", b"/connectors logout\x1b\r", settle=1.0, timeout=5.0),
    "tui_resume_empty": Case("tui_resume_empty", "tui", b"/resume\x1b\r", settle=1.0, timeout=5.0),
    "tui_resume_one": Case("tui_resume_one", "tui", b"/resume\x1b\r", settle=1.0, timeout=5.0),
    "tui_resume_legacy_json": Case("tui_resume_legacy_json", "tui", b"/resume\x1b\r", settle=1.5, timeout=8.0),
    "tui_resume_skips_invalid": Case("tui_resume_skips_invalid", "tui", b"/resume\x1b\r", settle=1.0, timeout=5.0),
    "tui_resume_same_end_time_mtime": Case("tui_resume_same_end_time_mtime", "tui", b"/resume\x1b\r", settle=1.0, timeout=5.0),
    "tui_continue_empty": Case("tui_continue_empty", "tui", b"/continue\x1b\r", settle=1.0, timeout=5.0),
    "tui_continue_one": Case("tui_continue_one", "tui", b"/continue\x1b\r", settle=1.0, timeout=5.0),
    "tui_resume_select_one": Case("tui_resume_select_one", "tui", b"/resume\x1b\r\r", settle=1.0, timeout=5.0),
    "tui_resume_delete_confirm": Case("tui_resume_delete_confirm", "tui", b"/resume\x1b\rd", settle=1.0, timeout=5.0),
    "tui_resume_delete_one": Case("tui_resume_delete_one", "tui", b"/resume\x1b\rdd", settle=1.0, timeout=5.0),
    "tui_resume_rename_one": Case("tui_resume_rename_one", "tui", b"/resume\x1b\r\r/rename Renamed parity\x1b\r", settle=1.0, timeout=5.0),
    "tui_compact_empty": Case("tui_compact_empty", "tui", b"/compact\x1b\r", settle=1.0, timeout=5.0),
    "tui_compact_one": Case("tui_compact_one", "tui", b"/resume\x1b\r\r/compact\x1b\r", settle=1.0, timeout=8.0),
    "tui_loop_usage": Case("tui_loop_usage", "tui", b"/loop\x1b\r", settle=1.0, timeout=5.0),
    "tui_loop_list_empty": Case("tui_loop_list_empty", "tui", b"/loop list\x1b\r", settle=1.0, timeout=5.0),
    "tui_loop_ls_empty": Case("tui_loop_ls_empty", "tui", b"/loop ls\x1b\r", settle=1.0, timeout=5.0),
    "tui_loop_cancel_all_empty": Case("tui_loop_cancel_all_empty", "tui", b"/loop cancel all\x1b\r", settle=1.0, timeout=5.0),
    "tui_loop_create": Case("tui_loop_create", "tui", b"/loop 30s check status\x1b\r", settle=1.0, timeout=5.0),
    "tui_loop_create_list": Case("tui_loop_create_list", "tui", b"/loop 30s check status\x1b\r/loop list\x1b\r", settle=1.0, timeout=5.0),
    "tui_loop_create_cancel_all": Case("tui_loop_create_cancel_all", "tui", b"/loop 30s check status\x1b\r/loop cancel all\x1b\r", settle=1.0, timeout=5.0),
    "tui_loop_invalid_interval": Case("tui_loop_invalid_interval", "tui", b"/loop wat check status\x1b\r", settle=1.0, timeout=5.0),
    "tui_loop_too_short": Case("tui_loop_too_short", "tui", b"/loop 1s check status\x1b\r", settle=1.0, timeout=5.0),
    "tui_loop_missing_prompt": Case("tui_loop_missing_prompt", "tui", b"/loop 30s\x1b\r", settle=1.0, timeout=5.0),
    "tui_loop_prompt_slash": Case("tui_loop_prompt_slash", "tui", b"/loop 30s /status\x1b\r", settle=1.0, timeout=5.0),
    "tui_loop_cancel_missing": Case("tui_loop_cancel_missing", "tui", b"/loop cancel\x1b\r", settle=1.0, timeout=5.0),
    "tui_loop_cancel_unknown": Case("tui_loop_cancel_unknown", "tui", b"/loop cancel deadbeef\x1b\r", settle=1.0, timeout=5.0),
    "tui_rename_usage": Case("tui_rename_usage", "tui", b"/rename\x1b\r", settle=1.0, timeout=5.0),
    "tui_rename_title": Case("tui_rename_title", "tui", b"/rename Parity title\x1b\r", settle=1.0, timeout=5.0),
    "tui_clear": Case("tui_clear", "tui", b"/clear\x1b\r", settle=1.0, timeout=5.0),
    "tui_reload": Case("tui_reload", "tui", b"/reload\x1b\r", settle=1.0, timeout=5.0),
    "tui_log": Case("tui_log", "tui", b"/log\x1b\r", settle=1.0, timeout=5.0),
    "tui_copy_empty": Case("tui_copy_empty", "tui", b"/copy\x1b\r", settle=1.0, timeout=5.0),
    "tui_copy_last_agent": Case("tui_copy_last_agent", "tui", b"", settle=1.0, timeout=10.0),
    "tui_copy_last_agent_xclip": Case("tui_copy_last_agent_xclip", "tui", b"", settle=1.0, timeout=10.0),
    "tui_leanstall": Case("tui_leanstall", "tui", b"/leanstall\x1b\r", settle=1.0, timeout=5.0),
    "tui_unleanstall": Case("tui_unleanstall", "tui", b"/unleanstall\x1b\r", settle=1.0, timeout=5.0),
    "tui_model_picker": Case("tui_model_picker", "tui", b"/model\x1b\r", settle=1.0, timeout=5.0),
    "tui_model_select_next": Case("tui_model_select_next", "tui", b"/model\x1b\r\x1b[B\r", settle=1.0, timeout=5.0),
    "tui_theme_picker": Case("tui_theme_picker", "tui", b"/theme\x1b\r", settle=1.0, timeout=5.0),
    "tui_theme_select_next": Case("tui_theme_select_next", "tui", b"/theme\x1b\r\x1b[B\r", settle=1.0, timeout=5.0),
    "tui_thinking_picker": Case("tui_thinking_picker", "tui", b"/thinking\x1b\r", settle=1.0, timeout=5.0),
    "tui_thinking_select_next": Case("tui_thinking_select_next", "tui", b"/thinking\x1b\r\x1b[B\r", settle=1.0, timeout=5.0),
    "tui_config": Case("tui_config", "tui", b"/config\x1b\r", settle=1.0, timeout=5.0),
    "tui_config_toggle_autocopy": Case("tui_config_toggle_autocopy", "tui", b"/config\x1b\r\x1b[B\x1b[B\r", settle=1.0, timeout=5.0),
    "tui_config_toggle_autocopy_exit": Case("tui_config_toggle_autocopy_exit", "tui", b"/config\x1b\r\x1b[B\x1b[B\r\x1b", settle=1.0, timeout=5.0),
    "tui_proxy_setup": Case("tui_proxy_setup", "tui", b"/proxy-setup\x1b\r", settle=1.0, timeout=5.0),
    "tui_proxy_setup_save_http": Case("tui_proxy_setup_save_http", "tui", b"", settle=1.0, timeout=10.0),
    "tui_proxy_setup_preserve_env": Case("tui_proxy_setup_preserve_env", "tui", b"", settle=1.0, timeout=10.0),
    "tui_proxy_setup_unset_http": Case("tui_proxy_setup_unset_http", "tui", b"", settle=1.0, timeout=10.0),
    "tui_voice": Case("tui_voice", "tui", b"/voice\x1b\r", settle=1.0, timeout=5.0),
    "tui_voice_toggle": Case("tui_voice_toggle", "tui", b"/voice\x1b\r ", settle=1.0, timeout=5.0),
    "tui_voice_toggle_exit": Case("tui_voice_toggle_exit", "tui", b"/voice\x1b\r \x1b", settle=1.0, timeout=5.0),
    "tui_rewind_empty": Case("tui_rewind_empty", "tui", b"/rewind\x1b\r", settle=1.0, timeout=5.0),
    "tui_rewind_one": Case("tui_rewind_one", "tui", b"/resume\x1b\r\r/rewind\x1b\r", settle=1.0, timeout=5.0),
    "tui_rewind_select_one": Case("tui_rewind_select_one", "tui", b"/resume\x1b\r\r/rewind\x1b\r\r", settle=1.0, timeout=5.0),
    "tui_rewind_global_ctrl_p": Case("tui_rewind_global_ctrl_p", "tui", b"", settle=1.0, timeout=7.0),
    "tui_rewind_global_ctrl_p_prev": Case("tui_rewind_global_ctrl_p_prev", "tui", b"", settle=1.0, timeout=7.0),
    "tui_rewind_global_ctrl_n": Case("tui_rewind_global_ctrl_n", "tui", b"", settle=1.0, timeout=7.0),
    "tui_rewind_global_alt_up": Case("tui_rewind_global_alt_up", "tui", b"", settle=1.0, timeout=7.0),
    "tui_rewind_global_alt_down": Case("tui_rewind_global_alt_down", "tui", b"", settle=1.0, timeout=7.0),
    "tui_cycle_mode_shift_tab": Case("tui_cycle_mode_shift_tab", "tui", b"\x1b[Z", settle=1.5, timeout=6.0),
    "tui_cycle_mode_shift_tab_twice": Case("tui_cycle_mode_shift_tab_twice", "tui", b"\x1b[Z\x1b[Z", settle=1.5, timeout=6.0),
    "tui_cycle_mode_shift_tab_thrice": Case("tui_cycle_mode_shift_tab_thrice", "tui", b"\x1b[Z\x1b[Z\x1b[Z", settle=1.5, timeout=6.0),
    "tui_cycle_mode_shift_tab_custom": Case("tui_cycle_mode_shift_tab_custom", "tui", b"", settle=1.5, timeout=8.0),
    "tui_ctrl_c_confirm": Case("tui_ctrl_c_confirm", "tui", b"\x03", settle=0.4, timeout=5.0),
    "tui_ctrl_c_clear_input": Case("tui_ctrl_c_clear_input", "tui", b"draft input\x03", settle=1.0, timeout=5.0),
    "tui_ctrl_d_confirm": Case("tui_ctrl_d_confirm", "tui", b"\x04", settle=0.4, timeout=5.0),
    "tui_ctrl_d_nonempty_no_quit": Case("tui_ctrl_d_nonempty_no_quit", "tui", b"abc\x1b[D\x04", settle=1.0, timeout=5.0),
    "tui_ctrl_r_no_insert": Case("tui_ctrl_r_no_insert", "tui", b"\x12", settle=1.0, timeout=5.0),
    "tui_ctrl_r_voice_enabled_no_insert": Case("tui_ctrl_r_voice_enabled_no_insert", "tui", b"\x12", settle=1.0, timeout=5.0),
    "tui_ctrl_y_no_insert": Case("tui_ctrl_y_no_insert", "tui", b"\x19", settle=1.0, timeout=5.0),
    "tui_ctrl_y_draft_no_insert": Case("tui_ctrl_y_draft_no_insert", "tui", b"draft\x19", settle=1.0, timeout=5.0),
    "tui_malformed_mouse_ignored": Case("tui_malformed_mouse_ignored", "tui", b"hello\x1b[<32;NaN;NaNMworld", settle=1.0, timeout=5.0),
    "tui_malformed_mouse_release_ignored": Case("tui_malformed_mouse_release_ignored", "tui", b"hello\x1b[<35;NaN;NaNmworld", settle=1.0, timeout=5.0),
    "tui_shift_delete_right": Case("tui_shift_delete_right", "tui", b"abc\x1b[D\x1b[3;2~", settle=1.0, timeout=5.0),
    "tui_initial_prompt": Case("tui_initial_prompt", "tui_initial_prompt", b"", settle=1.0, timeout=8.0),
    "tui_prompt_simple": Case("tui_prompt_simple", "tui", b"hello tui\x1b\r", settle=1.0, timeout=8.0),
    "tui_prompt_history_up": Case("tui_prompt_history_up", "tui", b"", settle=1.0, timeout=8.0),
    "tui_prompt_history_up_down": Case("tui_prompt_history_up_down", "tui", b"", settle=1.0, timeout=8.0),
    "tui_prompt_history_persisted": Case("tui_prompt_history_persisted", "tui", b"\x1b[A", settle=1.0, timeout=8.0),
    "tui_prompt_multiline_ctrl_j": Case("tui_prompt_multiline_ctrl_j", "tui", b"hello\x0aworld\x1b\r", settle=1.0, timeout=8.0),
    "tui_prompt_at_file": Case("tui_prompt_at_file", "tui", b"use @sample.txt\x1b\r", settle=1.0, timeout=10.0),
    "tui_completion_slash": Case("tui_completion_slash", "tui", b"/he\r", settle=1.0, timeout=10.0),
    "tui_completion_slash_nav_enter": Case("tui_completion_slash_nav_enter", "tui", b"/co\x1b[B\r", settle=1.0, timeout=10.0),
    "tui_completion_path_popup_list": Case("tui_completion_path_popup_list", "tui", b"@s", settle=1.0, timeout=10.0),
    "tui_completion_path_popup_ten": Case("tui_completion_path_popup_ten", "tui", b"@src/core/extra/", settle=1.0, timeout=10.0),
    "tui_completion_path_dir_tab": Case("tui_completion_path_dir_tab", "tui", b"@sr\t", settle=1.0, timeout=10.0),
    "tui_completion_path_file": Case("tui_completion_path_file", "tui", b"use @samp\t\r", settle=1.0, timeout=12.0),
    "tui_prompt_at_folder": Case("tui_prompt_at_folder", "tui", b"use @notes\x1b\r", settle=1.0, timeout=10.0),
    "tui_prompt_at_image": Case("tui_prompt_at_image", "tui", b"use @image.png\x1b\r", settle=1.0, timeout=10.0),
    "tui_prompt_at_image_no_vision": Case("tui_prompt_at_image_no_vision", "tui", b"use @image.png\x1b\r", settle=1.0, timeout=10.0),
    "tui_external_editor_input": Case("tui_external_editor_input", "tui", b"", settle=1.0, timeout=8.0),
    "tui_external_editor_empty": Case("tui_external_editor_empty", "tui", b"", settle=1.0, timeout=8.0),
    "tui_scroll_shift_up": Case("tui_scroll_shift_up", "tui", b"", settle=1.0, timeout=18.0),
    "tui_scroll_shift_up_down": Case("tui_scroll_shift_up_down", "tui", b"", settle=1.0, timeout=18.0),
    "tui_prompt_read": Case("tui_prompt_read", "tui", b"read sample\x1b\r", settle=1.0, timeout=10.0),
    "tui_prompt_read_expand_tool": Case("tui_prompt_read_expand_tool", "tui", b"read sample\x1b\r\x0f", settle=1.0, timeout=10.0),
    "tui_prompt_read_expand_collapse_tool": Case("tui_prompt_read_expand_collapse_tool", "tui", b"read sample\x1b\r\x0f\x0f", settle=1.0, timeout=10.0),
    "tui_bang_empty": Case("tui_bang_empty", "tui", b"!\x1b\r", settle=1.0, timeout=5.0),
    "tui_bang_bash": Case("tui_bang_bash", "tui", b"!printf manual-bash\x1b\r", settle=1.0, timeout=8.0),
    "tui_prompt_bash": Case("tui_prompt_bash", "tui", b"run bash\x1b\r", settle=1.0, timeout=10.0),
    "tui_animation_bash_spinner": Case("tui_animation_bash_spinner", "animation_tui", b"run bash\x1b\r", settle=1.0, timeout=10.0),
    "tui_prompt_bash_allow": Case("tui_prompt_bash_allow", "tui", b"run bash\x1b\r\r", settle=1.0, timeout=12.0),
    "tui_prompt_bash_allow_y": Case("tui_prompt_bash_allow_y", "tui", b"run bash\x1b\ry", settle=1.0, timeout=12.0),
    "tui_prompt_bash_allow_expand_tool": Case("tui_prompt_bash_allow_expand_tool", "tui", b"run bash\x1b\r\r\x0f", settle=1.0, timeout=12.0),
    "tui_prompt_bash_allow_expand_collapse_tool": Case("tui_prompt_bash_allow_expand_collapse_tool", "tui", b"run bash\x1b\r\r\x0f\x0f", settle=1.0, timeout=12.0),
    "tui_prompt_bash_allow_session": Case("tui_prompt_bash_allow_session", "tui", b"run bash twice\x1b\r2\r", settle=1.0, timeout=16.0),
    "tui_prompt_bash_always": Case("tui_prompt_bash_always", "tui", b"run bash always\x1b\r3\r", settle=1.0, timeout=16.0),
    "tui_prompt_bash_persisted_allow": Case("tui_prompt_bash_persisted_allow", "tui", b"run persisted bash\x1b\r", settle=1.0, timeout=12.0),
    "tui_prompt_bash_deny": Case("tui_prompt_bash_deny", "tui", b"deny bash\x1b\r4\r", settle=1.0, timeout=14.0),
    "tui_prompt_bash_deny_n": Case("tui_prompt_bash_deny_n", "tui", b"deny bash\x1b\rn", settle=1.0, timeout=14.0),
    "tui_prompt_file_tools": Case("tui_prompt_file_tools", "tui", b"file tools\x1b\r", settle=1.0, timeout=12.0),
    "tui_animation_write_file_spinner": Case("tui_animation_write_file_spinner", "animation_tui", b"file tools\x1b\r", settle=1.0, timeout=10.0),
    "tui_animation_edit_spinner": Case("tui_animation_edit_spinner", "animation_tui", b"", settle=1.0, timeout=14.0),
    "tui_prompt_file_tools_allow_write": Case("tui_prompt_file_tools_allow_write", "tui", b"file tools\x1b\r\r", settle=1.0, timeout=14.0),
    "tui_prompt_file_tools_allow_edit": Case("tui_prompt_file_tools_allow_edit", "tui", b"file tools\x1b\r\r\r", settle=1.0, timeout=18.0),
    "tui_prompt_file_tools_expand_tool": Case("tui_prompt_file_tools_expand_tool", "tui", b"file tools\x1b\r\r\r\x0f", settle=1.0, timeout=18.0),
    "tui_prompt_todo": Case("tui_prompt_todo", "tui", b"todo update\x1b\r", settle=1.0, timeout=12.0),
    "tui_prompt_todo_empty": Case("tui_prompt_todo_empty", "tui", b"todo read empty\x1b\r", settle=1.0, timeout=12.0),
    "tui_slash_skill": Case("tui_slash_skill", "tui", b"/parity-skill extra instructions\x1b\r", settle=1.0, timeout=10.0),
    "tui_prompt_skill": Case("tui_prompt_skill", "tui", b"load skill\x1b\r", settle=1.0, timeout=12.0),
    "tui_prompt_skill_expand_tool": Case("tui_prompt_skill_expand_tool", "tui", b"load skill\x1b\r\x0f", settle=1.0, timeout=12.0),
    "tui_prompt_task": Case("tui_prompt_task", "tui", b"delegate task\x1b\r", settle=1.0, timeout=14.0),
    "tui_animation_task_spinner": Case("tui_animation_task_spinner", "animation_tui", b"delegate task\x1b\r", settle=1.0, timeout=10.0),
    "tui_prompt_task_allow_explore": Case("tui_prompt_task_allow_explore", "tui", b"delegate explore task\x1b\r", settle=1.0, timeout=18.0),
    "tui_prompt_task_allow_unknown": Case("tui_prompt_task_allow_unknown", "tui", b"delegate task\x1b\r\r", settle=1.0, timeout=18.0),
    "tui_prompt_task_deny": Case("tui_prompt_task_deny", "tui", b"delegate task\x1b\r4", settle=1.0, timeout=16.0),
    "tui_prompt_web_fetch": Case("tui_prompt_web_fetch", "tui", b"fetch web\x1b\r", settle=1.0, timeout=14.0),
    "tui_prompt_web_fetch_expand_tool": Case("tui_prompt_web_fetch_expand_tool", "tui", b"fetch web\x1b\r\r\x0f", settle=1.0, timeout=14.0),
    "tui_animation_web_fetch_spinner": Case("tui_animation_web_fetch_spinner", "animation_tui", b"fetch web\x1b\r", settle=1.0, timeout=10.0),
    "tui_prompt_web_search": Case("tui_prompt_web_search", "tui", b"search web\x1b\r", settle=1.0, timeout=14.0),
    "tui_animation_web_search_spinner": Case("tui_animation_web_search_spinner", "animation_tui", b"search web\x1b\r", settle=1.0, timeout=10.0),
    "tui_prompt_web_search_expand_tool": Case("tui_prompt_web_search_expand_tool", "tui", b"search web\x1b\r\r\x0f", settle=1.0, timeout=14.0),
    "tui_prompt_question": Case("tui_prompt_question", "tui", b"ask question\x1b\r1", settle=1.0, timeout=16.0),
    "tui_animation_question_spinner": Case("tui_animation_question_spinner", "animation_tui", b"ask question\x1b\r", settle=1.0, timeout=10.0),
    "tui_prompt_question_expand_tool": Case("tui_prompt_question_expand_tool", "tui", b"ask question\x1b\r1\x0f", settle=1.0, timeout=16.0),
    "tui_prompt_question_other": Case("tui_prompt_question_other", "tui", b"ask custom question\x1b\r\x1b[B\x1b[BCustom parity\r", settle=1.0, timeout=18.0),
    "tui_prompt_question_multi": Case("tui_prompt_question_multi", "tui", b"ask multi question\x1b\r1\r", settle=1.0, timeout=18.0),
    "tui_prompt_question_multiselect": Case("tui_prompt_question_multiselect", "tui", b"ask multi select\x1b\r\r\x1b[B\r\x1b[B\r", settle=1.0, timeout=18.0),
    "tui_prompt_question_multiselect_other": Case("tui_prompt_question_multiselect_other", "tui", b"ask multi select custom\x1b\r\r\x1b[B\x1b[BGreen\r\r", settle=1.0, timeout=18.0),
    "tui_prompt_exit_plan_auto": Case("tui_prompt_exit_plan_auto", "tui", b"finish plan\x1b\r\r", settle=1.0, timeout=16.0),
    "tui_animation_exit_plan_spinner": Case("tui_animation_exit_plan_spinner", "animation_tui", b"finish plan\x1b\r", settle=1.0, timeout=10.0),
    "tui_prompt_exit_plan_default": Case("tui_prompt_exit_plan_default", "tui", b"finish plan\x1b\r\x1b[B\r", settle=1.0, timeout=16.0),
    "tui_prompt_exit_plan_no": Case("tui_prompt_exit_plan_no", "tui", b"finish plan\x1b\r\x1b[B\x1b[B\r", settle=1.0, timeout=16.0),
    "tui_prompt_exit_plan_editor": Case("tui_prompt_exit_plan_editor", "tui", b"", settle=1.0, timeout=16.0),
    "tui_teleport_unavailable": Case("tui_teleport_unavailable", "tui", b"/teleport\x1b\r", settle=1.0, timeout=5.0),
    "tui_teleport_ampersand_unavailable": Case("tui_teleport_ampersand_unavailable", "tui", b"&open web target\x1b\r", settle=1.0, timeout=5.0),
    "programmatic_text": Case("programmatic_text", "programmatic_text", b"", settle=0.0, timeout=15.0),
    "programmatic_empty_prompt_text": Case("programmatic_empty_prompt_text", "programmatic_empty_prompt_text", b"", settle=0.0, timeout=15.0),
    "programmatic_json": Case("programmatic_json", "programmatic_json", b"", settle=0.0, timeout=15.0),
    "programmatic_streaming": Case("programmatic_streaming", "programmatic_streaming", b"", settle=0.0, timeout=15.0),
    "programmatic_read_text": Case("programmatic_read_text", "programmatic_read_text", b"", settle=0.0, timeout=15.0),
    "programmatic_read_json": Case("programmatic_read_json", "programmatic_read_json", b"", settle=0.0, timeout=15.0),
    "programmatic_read_streaming": Case("programmatic_read_streaming", "programmatic_read_streaming", b"", settle=0.0, timeout=15.0),
    "programmatic_tools_text": Case("programmatic_tools_text", "programmatic_tools_text", b"", settle=0.0, timeout=20.0),
    "programmatic_tools_json": Case("programmatic_tools_json", "programmatic_tools_json", b"", settle=0.0, timeout=20.0),
    "programmatic_tools_streaming": Case("programmatic_tools_streaming", "programmatic_tools_streaming", b"", settle=0.0, timeout=20.0),
    "programmatic_hooks_before_json": Case("programmatic_hooks_before_json", "programmatic_hooks_before_json", b"", settle=0.0, timeout=20.0),
    "programmatic_hooks_after_json": Case("programmatic_hooks_after_json", "programmatic_hooks_after_json", b"", settle=0.0, timeout=20.0),
    "programmatic_hooks_post_json": Case("programmatic_hooks_post_json", "programmatic_hooks_post_json", b"", settle=0.0, timeout=20.0),
    "programmatic_mcp_stdio_text": Case("programmatic_mcp_stdio_text", "programmatic_mcp_stdio_text", b"", settle=0.0, timeout=25.0),
    "programmatic_mcp_stdio_json": Case("programmatic_mcp_stdio_json", "programmatic_mcp_stdio_json", b"", settle=0.0, timeout=25.0),
    "programmatic_mcp_stdio_streaming": Case("programmatic_mcp_stdio_streaming", "programmatic_mcp_stdio_streaming", b"", settle=0.0, timeout=25.0),
    "programmatic_enabled_tools_text": Case("programmatic_enabled_tools_text", "programmatic_enabled_tools_text", b"", settle=0.0, timeout=20.0),
    "programmatic_enabled_tools_json": Case("programmatic_enabled_tools_json", "programmatic_enabled_tools_json", b"", settle=0.0, timeout=20.0),
    "programmatic_enabled_tools_streaming": Case("programmatic_enabled_tools_streaming", "programmatic_enabled_tools_streaming", b"", settle=0.0, timeout=20.0),
    "programmatic_agent_custom_text": Case("programmatic_agent_custom_text", "programmatic_agent_custom_text", b"", settle=0.0, timeout=20.0),
    "programmatic_agent_custom_json": Case("programmatic_agent_custom_json", "programmatic_agent_custom_json", b"", settle=0.0, timeout=20.0),
    "programmatic_agent_custom_streaming": Case("programmatic_agent_custom_streaming", "programmatic_agent_custom_streaming", b"", settle=0.0, timeout=20.0),
    "programmatic_max_turns_text": Case("programmatic_max_turns_text", "programmatic_max_turns_text", b"", settle=0.0, timeout=20.0),
    "programmatic_max_turns_json": Case("programmatic_max_turns_json", "programmatic_max_turns_json", b"", settle=0.0, timeout=20.0),
    "programmatic_max_turns_streaming": Case("programmatic_max_turns_streaming", "programmatic_max_turns_streaming", b"", settle=0.0, timeout=20.0),
    "programmatic_max_tokens_text": Case("programmatic_max_tokens_text", "programmatic_max_tokens_text", b"", settle=0.0, timeout=20.0),
    "programmatic_max_tokens_json": Case("programmatic_max_tokens_json", "programmatic_max_tokens_json", b"", settle=0.0, timeout=20.0),
    "programmatic_max_tokens_streaming": Case("programmatic_max_tokens_streaming", "programmatic_max_tokens_streaming", b"", settle=0.0, timeout=20.0),
    "programmatic_max_price_text": Case("programmatic_max_price_text", "programmatic_max_price_text", b"", settle=0.0, timeout=20.0),
    "programmatic_max_price_json": Case("programmatic_max_price_json", "programmatic_max_price_json", b"", settle=0.0, timeout=20.0),
    "programmatic_max_price_streaming": Case("programmatic_max_price_streaming", "programmatic_max_price_streaming", b"", settle=0.0, timeout=20.0),
    "programmatic_state_text": Case("programmatic_state_text", "programmatic_state_text", b"", settle=0.0, timeout=20.0),
    "programmatic_state_json": Case("programmatic_state_json", "programmatic_state_json", b"", settle=0.0, timeout=20.0),
    "programmatic_state_streaming": Case("programmatic_state_streaming", "programmatic_state_streaming", b"", settle=0.0, timeout=20.0),
    "programmatic_web_fetch_text": Case("programmatic_web_fetch_text", "programmatic_web_fetch_text", b"", settle=0.0, timeout=20.0),
    "programmatic_web_fetch_json": Case("programmatic_web_fetch_json", "programmatic_web_fetch_json", b"", settle=0.0, timeout=20.0),
    "programmatic_web_fetch_streaming": Case("programmatic_web_fetch_streaming", "programmatic_web_fetch_streaming", b"", settle=0.0, timeout=20.0),
    "programmatic_web_search_text": Case("programmatic_web_search_text", "programmatic_web_search_text", b"", settle=0.0, timeout=20.0),
    "programmatic_web_search_json": Case("programmatic_web_search_json", "programmatic_web_search_json", b"", settle=0.0, timeout=20.0),
    "programmatic_web_search_streaming": Case("programmatic_web_search_streaming", "programmatic_web_search_streaming", b"", settle=0.0, timeout=20.0),
    "programmatic_skill_text": Case("programmatic_skill_text", "programmatic_skill_text", b"", settle=0.0, timeout=20.0),
    "programmatic_skill_json": Case("programmatic_skill_json", "programmatic_skill_json", b"", settle=0.0, timeout=20.0),
    "programmatic_skill_streaming": Case("programmatic_skill_streaming", "programmatic_skill_streaming", b"", settle=0.0, timeout=20.0),
    "programmatic_question_text": Case("programmatic_question_text", "programmatic_question_text", b"", settle=0.0, timeout=20.0),
    "programmatic_question_json": Case("programmatic_question_json", "programmatic_question_json", b"", settle=0.0, timeout=20.0),
    "programmatic_question_streaming": Case("programmatic_question_streaming", "programmatic_question_streaming", b"", settle=0.0, timeout=20.0),
    "programmatic_exit_plan_text": Case("programmatic_exit_plan_text", "programmatic_exit_plan_text", b"", settle=0.0, timeout=20.0),
    "programmatic_exit_plan_json": Case("programmatic_exit_plan_json", "programmatic_exit_plan_json", b"", settle=0.0, timeout=20.0),
    "programmatic_exit_plan_streaming": Case("programmatic_exit_plan_streaming", "programmatic_exit_plan_streaming", b"", settle=0.0, timeout=20.0),
    "programmatic_task_unknown_text": Case("programmatic_task_unknown_text", "programmatic_task_unknown_text", b"", settle=0.0, timeout=20.0),
    "programmatic_task_unknown_json": Case("programmatic_task_unknown_json", "programmatic_task_unknown_json", b"", settle=0.0, timeout=20.0),
    "programmatic_task_unknown_streaming": Case("programmatic_task_unknown_streaming", "programmatic_task_unknown_streaming", b"", settle=0.0, timeout=20.0),
    "programmatic_task_text": Case("programmatic_task_text", "programmatic_task_text", b"", settle=0.0, timeout=30.0),
    "programmatic_task_json": Case("programmatic_task_json", "programmatic_task_json", b"", settle=0.0, timeout=30.0),
    "programmatic_task_streaming": Case("programmatic_task_streaming", "programmatic_task_streaming", b"", settle=0.0, timeout=30.0),
    "programmatic_task_custom_text": Case("programmatic_task_custom_text", "programmatic_task_custom_text", b"", settle=0.0, timeout=30.0),
    "programmatic_task_custom_json": Case("programmatic_task_custom_json", "programmatic_task_custom_json", b"", settle=0.0, timeout=30.0),
    "programmatic_task_custom_streaming": Case("programmatic_task_custom_streaming", "programmatic_task_custom_streaming", b"", settle=0.0, timeout=30.0),
    "programmatic_task_read_text": Case("programmatic_task_read_text", "programmatic_task_read_text", b"", settle=0.0, timeout=30.0),
    "programmatic_task_read_json": Case("programmatic_task_read_json", "programmatic_task_read_json", b"", settle=0.0, timeout=30.0),
    "programmatic_task_read_streaming": Case("programmatic_task_read_streaming", "programmatic_task_read_streaming", b"", settle=0.0, timeout=30.0),
    "programmatic_continue_json": Case("programmatic_continue_json", "programmatic_continue_json", b"", settle=0.0, timeout=30.0),
    "programmatic_resume_id_json": Case("programmatic_resume_id_json", "programmatic_resume_id_json", b"", settle=0.0, timeout=30.0),
}

SERIAL_ALL_CASES = {
    "cli_workdir_missing",
    "cli_add_dir_missing",
    "cli_help",
    "cli_version",
    "cli_output_invalid",
    "cli_agent_auto_approve_conflict",
    "cli_agent_not_found",
    "cli_agent_disabled",
    "cli_agent_enabled_excluded",
    "cli_agent_subagent",
    "cli_agent_lean_missing",
    "cli_default_agent_disabled",
    "cli_default_agent_enabled_excluded",
    "cli_check_upgrade_available",
    "cli_setup_welcome",
    "cli_setup_cancel",
    "cli_setup_theme",
    "cli_setup_auth_method",
    "cli_setup_api_key",
    "cli_setup_save_api_key",
    "cli_continue_missing",
    "cli_resume_missing",
    "tui_trust_prompt",
    "tui_trust_accept",
    "tui_trust_repo_prompt",
    "tui_trust_repo_accept",
    "tui_trust_repo_decline",
    "startup",
    "default_tui_startup",
    "tui_startup",
    "tui_startup_agent_plan",
    "tui_startup_agent_custom",
    "tui_startup_auto_approve",
    "tui_help",
    "tui_status",
    "tui_data_retention",
    "tui_debug_command",
    "tui_debug_ctrl_backslash",
    "tui_mcp",
    "tui_mcp_status",
    "tui_mcp_configured",
    "tui_mcp_status_configured",
    "tui_mcp_stdio_tools",
    "tui_mcp_stdio_tools_detail",
    "tui_mcp_disable_server",
    "tui_mcp_enable_server",
    "tui_mcp_disable_tool",
    "tui_mcp_enable_tool",
    "tui_mcp_login_usage",
    "tui_mcp_logout_usage",
    "tui_resume_empty",
    "tui_resume_one",
    "tui_resume_legacy_json",
    "tui_resume_skips_invalid",
    "tui_resume_same_end_time_mtime",
    "tui_resume_select_one",
    "tui_resume_delete_confirm",
    "tui_resume_delete_one",
    "tui_resume_rename_one",
    "tui_compact_empty",
    "tui_compact_one",
    "tui_loop_usage",
    "tui_loop_list_empty",
    "tui_loop_ls_empty",
    "tui_loop_cancel_all_empty",
    "tui_loop_create",
    "tui_loop_create_list",
    "tui_loop_create_cancel_all",
    "tui_loop_invalid_interval",
    "tui_loop_too_short",
    "tui_loop_missing_prompt",
    "tui_loop_prompt_slash",
    "tui_loop_cancel_missing",
    "tui_loop_cancel_unknown",
    "tui_rename_usage",
    "tui_rename_title",
    "tui_clear",
    "tui_reload",
    "tui_log",
    "tui_copy_empty",
    "tui_leanstall",
    "tui_unleanstall",
    "tui_model_picker",
    "tui_model_select_next",
    "tui_theme_picker",
    "tui_theme_select_next",
    "tui_thinking_picker",
    "tui_thinking_select_next",
    "tui_config",
    "tui_config_toggle_autocopy",
    "tui_config_toggle_autocopy_exit",
    "tui_proxy_setup",
    "tui_voice",
    "tui_voice_toggle",
    "tui_voice_toggle_exit",
    "tui_rewind_empty",
    "tui_rewind_one",
    "tui_rewind_select_one",
    "tui_rewind_global_ctrl_p",
    "tui_rewind_global_ctrl_p_prev",
    "tui_rewind_global_ctrl_n",
    "tui_rewind_global_alt_up",
    "tui_rewind_global_alt_down",
    "tui_cycle_mode_shift_tab",
    "tui_cycle_mode_shift_tab_twice",
    "tui_cycle_mode_shift_tab_thrice",
    "tui_cycle_mode_shift_tab_custom",
    "tui_ctrl_c_confirm",
    "tui_ctrl_c_clear_input",
    "tui_ctrl_d_confirm",
    "tui_ctrl_d_nonempty_no_quit",
    "tui_ctrl_r_no_insert",
    "tui_ctrl_r_voice_enabled_no_insert",
    "tui_ctrl_y_no_insert",
    "tui_ctrl_y_draft_no_insert",
    "tui_malformed_mouse_ignored",
    "tui_malformed_mouse_release_ignored",
    "tui_shift_delete_right",
    "tui_prompt_at_file",
    "tui_prompt_at_folder",
    "tui_prompt_at_image",
    "tui_prompt_at_image_no_vision",
}

SMOKE_CASES = [
    "startup",
    "cli_help",
    "cli_version",
    "cli_output_invalid",
    "cli_agent_auto_approve_conflict",
    "cli_agent_not_found",
    "cli_agent_disabled",
    "cli_agent_enabled_excluded",
    "cli_agent_subagent",
    "cli_agent_lean_missing",
    "cli_default_agent_disabled",
    "cli_default_agent_enabled_excluded",
    "cli_workdir_missing",
    "cli_add_dir_missing",
    "cli_check_upgrade_available",
    "cli_setup_welcome",
    "cli_setup_cancel",
    "cli_setup_theme",
    "cli_setup_auth_method",
    "cli_setup_api_key",
    "cli_setup_save_api_key",
    "cli_continue_missing",
    "cli_resume_missing",
    "default_tui_startup",
    "tui_trust_prompt",
    "tui_trust_accept",
    "tui_trust_repo_prompt",
    "tui_trust_repo_accept",
    "tui_trust_repo_decline",
    "tui_startup",
    "tui_startup_agent_plan",
    "tui_startup_agent_custom",
    "tui_startup_auto_approve",
    "tui_help",
    "tui_status",
    "tui_data_retention",
    "tui_debug_command",
    "tui_debug_ctrl_backslash",
    "tui_mcp",
    "tui_mcp_status",
    "tui_mcp_configured",
    "tui_mcp_status_configured",
    "tui_mcp_stdio_tools",
    "tui_mcp_stdio_tools_detail",
    "tui_mcp_disable_server",
    "tui_mcp_enable_server",
    "tui_mcp_disable_tool",
    "tui_mcp_enable_tool",
    "tui_mcp_login_usage",
    "tui_mcp_logout_usage",
    "tui_connectors",
    "tui_connectors_status",
    "tui_connectors_configured",
    "tui_connectors_login_usage",
    "tui_connectors_logout_usage",
    "tui_resume_empty",
    "tui_resume_one",
    "tui_resume_legacy_json",
    "tui_resume_skips_invalid",
    "tui_resume_same_end_time_mtime",
    "tui_continue_empty",
    "tui_continue_one",
    "tui_resume_select_one",
    "tui_resume_delete_confirm",
    "tui_resume_delete_one",
    "tui_resume_rename_one",
    "tui_compact_empty",
    "tui_compact_one",
    "tui_loop_usage",
    "tui_loop_list_empty",
    "tui_loop_ls_empty",
    "tui_loop_cancel_all_empty",
    "tui_loop_create",
    "tui_loop_create_list",
    "tui_loop_create_cancel_all",
    "tui_loop_invalid_interval",
    "tui_loop_too_short",
    "tui_loop_missing_prompt",
    "tui_loop_prompt_slash",
    "tui_loop_cancel_missing",
    "tui_loop_cancel_unknown",
    "tui_rename_usage",
    "tui_rename_title",
    "tui_clear",
    "tui_reload",
    "tui_log",
    "tui_copy_empty",
    "tui_copy_last_agent",
    "tui_copy_last_agent_xclip",
    "tui_leanstall",
    "tui_unleanstall",
    "tui_model_picker",
    "tui_model_select_next",
    "tui_theme_picker",
    "tui_theme_select_next",
    "tui_thinking_picker",
    "tui_thinking_select_next",
    "tui_config",
    "tui_config_toggle_autocopy",
    "tui_config_toggle_autocopy_exit",
    "tui_proxy_setup",
    "tui_proxy_setup_save_http",
    "tui_proxy_setup_preserve_env",
    "tui_proxy_setup_unset_http",
    "tui_voice",
    "tui_voice_toggle",
    "tui_voice_toggle_exit",
    "tui_rewind_empty",
    "tui_rewind_one",
    "tui_rewind_select_one",
    "tui_rewind_global_ctrl_p",
    "tui_rewind_global_ctrl_p_prev",
    "tui_rewind_global_ctrl_n",
    "tui_rewind_global_alt_up",
    "tui_rewind_global_alt_down",
    "tui_cycle_mode_shift_tab",
    "tui_cycle_mode_shift_tab_twice",
    "tui_cycle_mode_shift_tab_thrice",
    "tui_cycle_mode_shift_tab_custom",
    "tui_ctrl_c_confirm",
    "tui_ctrl_c_clear_input",
    "tui_ctrl_d_confirm",
    "tui_ctrl_d_nonempty_no_quit",
    "tui_ctrl_r_no_insert",
    "tui_ctrl_r_voice_enabled_no_insert",
    "tui_ctrl_y_no_insert",
    "tui_ctrl_y_draft_no_insert",
    "tui_malformed_mouse_ignored",
    "tui_malformed_mouse_release_ignored",
    "tui_shift_delete_right",
    "tui_initial_prompt",
    "tui_prompt_history_up",
    "tui_prompt_history_up_down",
    "tui_prompt_history_persisted",
    "tui_prompt_multiline_ctrl_j",
    "tui_prompt_at_file",
    "tui_completion_slash",
    "tui_completion_slash_nav_enter",
    "tui_completion_path_popup_list",
    "tui_completion_path_popup_ten",
    "tui_completion_path_dir_tab",
    "tui_completion_path_file",
    "tui_prompt_at_folder",
    "tui_prompt_at_image",
    "tui_prompt_at_image_no_vision",
    "tui_external_editor_input",
    "tui_external_editor_empty",
    "tui_scroll_shift_up",
    "tui_scroll_shift_up_down",
    "tui_prompt_simple",
    "tui_prompt_read",
    "tui_prompt_read_expand_tool",
    "tui_prompt_read_expand_collapse_tool",
    "tui_bang_empty",
    "tui_bang_bash",
    "tui_prompt_bash",
    "tui_prompt_bash_allow",
    "tui_prompt_bash_allow_y",
    "tui_prompt_bash_allow_expand_tool",
    "tui_prompt_bash_allow_expand_collapse_tool",
    "tui_prompt_bash_allow_session",
    "tui_prompt_bash_always",
    "tui_prompt_bash_persisted_allow",
    "tui_prompt_bash_deny",
    "tui_prompt_bash_deny_n",
    "tui_prompt_file_tools",
    "tui_animation_write_file_spinner",
    "tui_animation_edit_spinner",
    "tui_prompt_file_tools_allow_write",
    "tui_prompt_file_tools_allow_edit",
    "tui_prompt_file_tools_expand_tool",
    "tui_prompt_todo",
    "tui_prompt_todo_empty",
    "tui_slash_skill",
    "tui_prompt_skill",
    "tui_prompt_skill_expand_tool",
    "tui_prompt_task",
    "tui_animation_task_spinner",
    "tui_prompt_task_allow_explore",
    "tui_prompt_task_allow_unknown",
    "tui_prompt_task_deny",
    "tui_prompt_web_fetch",
    "tui_prompt_web_fetch_expand_tool",
    "tui_animation_web_fetch_spinner",
    "tui_prompt_web_search",
    "tui_animation_web_search_spinner",
    "tui_prompt_web_search_expand_tool",
    "tui_prompt_question",
    "tui_animation_question_spinner",
    "tui_prompt_question_expand_tool",
    "tui_prompt_question_other",
    "tui_prompt_question_multi",
    "tui_prompt_question_multiselect",
    "tui_prompt_question_multiselect_other",
    "tui_prompt_exit_plan_auto",
    "tui_animation_exit_plan_spinner",
    "tui_prompt_exit_plan_default",
    "tui_prompt_exit_plan_no",
    "tui_prompt_exit_plan_editor",
    "tui_teleport_unavailable",
    "tui_teleport_ampersand_unavailable",
    "tui_animation_bash_spinner",
    "acp_help",
    "acp_version",
    "acp_initialize",
    "acp_new_session",
    "acp_prompt_simple",
    "acp_prompt_client_message_id",
    "acp_prompt_agent_thought",
    "acp_prompt_usage_accumulates",
    "acp_prompt_usage_cost",
    "acp_prompt_missing_session",
    "acp_prompt_image",
    "acp_prompt_image_wrong_type",
    "acp_prompt_image_invalid_base64",
    "acp_command_help",
    "acp_command_reload",
    "acp_command_compact_empty",
    "acp_command_compact_one",
    "acp_command_teleport_no_history",
    "acp_command_data_retention",
    "acp_command_proxy_help",
    "acp_command_proxy_set",
    "acp_command_proxy_unset",
    "acp_command_proxy_invalid",
    "acp_command_proxy_case",
    "acp_list_sessions_empty",
    "acp_list_sessions_seeded",
    "acp_load_rich_session",
    "acp_load_session",
    "acp_load_missing",
    "acp_load_replay_ids",
    "acp_list_sessions_cwd_filter",
    "acp_list_sessions_sorted",
    "acp_list_sessions_skip_invalid",
    "acp_list_sessions_timestamps",
    "acp_fork_session",
    "acp_fork_from_prompt_message",
    "acp_fork_missing",
    "acp_set_title_live_unsaved",
    "acp_set_title_saved",
    "acp_delete_saved",
    "acp_delete_missing",
    "acp_delete_saved_pointer",
    "acp_delete_exact_collision",
    "acp_delete_live_unsaved",
    "acp_delete_loaded_saved",
    "acp_delete_invalid_missing",
    "acp_delete_invalid_empty",
    "acp_delete_invalid_saved_session_id",
    "acp_set_mode_fork_default",
    "acp_set_mode_fork_auto_approve",
    "acp_set_mode_fork_plan",
    "acp_set_mode_fork_accept_edits",
    "acp_set_mode_fork_chat",
    "acp_set_mode_fork_invalid",
    "acp_set_mode_fork_empty",
    "acp_set_mode_valid",
    "acp_set_mode_invalid",
    "acp_set_model_valid",
    "acp_set_model_invalid",
    "acp_set_model_same",
    "acp_set_model_empty",
    "acp_set_config_thinking",
    "acp_set_config_model",
    "acp_set_config_model_empty",
    "acp_set_config_mode",
    "acp_set_config_mode_empty",
    "acp_set_config_thinking_invalid",
    "acp_set_config_thinking_empty",
    "acp_set_config_max_turns",
    "acp_set_config_max_turns_invalid",
    "acp_set_config_max_turns_bool",
    "acp_set_config_invalid_id",
    "acp_set_config_empty_id",
    "acp_permission_bash_granular",
    "acp_prompt_grep",
    "acp_permission_grep_allow",
    "acp_permission_grep_deny",
    "acp_permission_grep_cancelled",
    "acp_permission_grep_allow_always",
    "acp_permission_grep_allow_always_permanent",
    "acp_permission_bash_granular_allow_always_permanent",
    "acp_fs_read",
    "acp_fs_read_range",
    "acp_fs_write",
    "acp_fs_edit",
    "acp_terminal_bash_allow",
    "acp_terminal_bash_nonzero",
    "acp_terminal_bash_none_exit",
    "acp_terminal_bash_timeout",
    "acp_tool_meta_web_fetch",
    "acp_tool_meta_web_search",
    "acp_tool_meta_skill",
    "acp_tool_meta_task",
    "acp_prompt_todo",
    "acp_prompt_todo_invalid",
    "acp_user_display_content",
    "acp_close_session",
    "acp_close_missing",
    "acp_auth_status_signed_out",
    "acp_auth_status_process_env",
    "acp_auth_status_dotenv",
    "acp_auth_status_process_over_dotenv",
    "acp_auth_signout_dotenv",
    "acp_auth_signout_process_over_dotenv",
    "acp_authenticate_unsupported",
    "acp_initialize_unsupported_provider",
    "acp_authenticate_browser_unsupported",
    "acp_authenticate_browser_complete",
    "acp_authenticate_browser_unsupported_action",
    "acp_initialize_delegated_browser_auth",
    "acp_authenticate_delegated_start",
    "acp_authenticate_delegated_complete",
    "acp_authenticate_delegated_missing_attempt",
    "acp_authenticate_delegated_unknown_attempt",
    "acp_authenticate_delegated_unsupported_action",
    "acp_telemetry_notification",
    "acp_unknown_notification",
    "acp_trust_status_untrusted",
    "acp_trust_status_repo",
    "acp_trust_decision_cwd",
    "acp_trust_decision_repo",
    "acp_trust_decision_invalid",
    "acp_trust_decision_missing_session",
    "programmatic_text",
    "programmatic_empty_prompt_text",
    "programmatic_read_text",
    "programmatic_read_json",
    "programmatic_read_streaming",
    "programmatic_tools_text",
    "programmatic_tools_streaming",
    "programmatic_hooks_after_json",
    "programmatic_hooks_post_json",
    "programmatic_mcp_stdio_text",
    "programmatic_mcp_stdio_streaming",
    "programmatic_enabled_tools_text",
    "programmatic_enabled_tools_json",
    "programmatic_enabled_tools_streaming",
    "programmatic_agent_custom_text",
    "programmatic_agent_custom_json",
    "programmatic_agent_custom_streaming",
    "programmatic_max_turns_text",
    "programmatic_max_turns_json",
    "programmatic_max_turns_streaming",
    "programmatic_max_tokens_text",
    "programmatic_max_tokens_json",
    "programmatic_max_tokens_streaming",
    "programmatic_max_price_text",
    "programmatic_max_price_json",
    "programmatic_max_price_streaming",
    "programmatic_state_text",
    "programmatic_state_json",
    "programmatic_state_streaming",
    "programmatic_web_fetch_text",
    "programmatic_web_fetch_json",
    "programmatic_web_fetch_streaming",
    "programmatic_web_search_text",
    "programmatic_web_search_json",
    "programmatic_web_search_streaming",
    "programmatic_skill_text",
    "programmatic_skill_json",
    "programmatic_skill_streaming",
    "programmatic_question_text",
    "programmatic_question_json",
    "programmatic_question_streaming",
    "programmatic_exit_plan_text",
    "programmatic_exit_plan_json",
    "programmatic_exit_plan_streaming",
    "programmatic_task_unknown_text",
    "programmatic_task_unknown_json",
    "programmatic_task_unknown_streaming",
    "programmatic_task_text",
    "programmatic_task_streaming",
    "programmatic_task_custom_text",
    "programmatic_task_custom_json",
    "programmatic_task_custom_streaming",
    "programmatic_task_read_text",
    "programmatic_task_read_json",
    "programmatic_task_read_streaming",
    "programmatic_continue_json",
    "programmatic_resume_id_json",
    "programmatic_json",
    "programmatic_streaming",
    "programmatic_tools_json",
    "programmatic_hooks_before_json",
    "programmatic_mcp_stdio_json",
    "programmatic_task_json",
]

FAST_CASES = [
    "startup",
    "cli_help",
    "cli_output_invalid",
    "cli_agent_not_found",
    "cli_check_upgrade_available",
    "cli_setup_cancel",
    "default_tui_startup",
    "tui_help",
    "tui_prompt_simple",
    "tui_ctrl_y_no_insert",
    "tui_ctrl_y_draft_no_insert",
    "tui_malformed_mouse_ignored",
    "tui_malformed_mouse_release_ignored",
    "tui_prompt_bash_allow",
    "tui_animation_bash_spinner",
    "acp_initialize",
    "acp_prompt_simple",
    "acp_prompt_usage_cost",
    "acp_command_help",
    "acp_permission_grep_allow",
    "acp_permission_grep_allow_always_permanent",
    "acp_fs_read",
    "acp_terminal_bash_allow",
    "acp_tool_meta_web_fetch",
    "acp_auth_status_dotenv",
    "acp_trust_status_untrusted",
    "programmatic_text",
    "programmatic_json",
    "programmatic_streaming",
    "programmatic_tools_json",
    "programmatic_web_fetch_json",
    "programmatic_question_json",
    "programmatic_task_json",
]


def command_from_env(name: str, default: str) -> list[str]:
    return shlex.split(os.environ.get(name, default))


def default_vibe_command() -> str:
    direct = (ROOT / "../mistral-vibe-upstream/.venv/bin/vibe").resolve()
    if direct.exists():
        return str(direct)
    return "uv run --project ../mistral-vibe-upstream vibe"


def default_vibe_acp_command() -> str:
    direct = (ROOT / "../mistral-vibe-upstream/.venv/bin/vibe-acp").resolve()
    if direct.exists():
        return str(direct)
    return "uv run --project ../mistral-vibe-upstream vibe-acp"


def build_command(binary: list[str], mode: str, *, microvibe: bool) -> list[str]:
    if mode == "acp_help":
        return [*binary, "--help"]
    if mode == "acp_version":
        return [*binary, "--version"]
    if mode.startswith("programmatic_"):
        output = programmatic_output_mode(mode)
        command = [*binary, "--trust", "--auto-approve"]
        if "_agent_custom_" in mode:
            command = [*binary, "--trust", "--agent", "review-bot"]
        if "_enabled_tools_" in mode:
            command.extend(["--enabled-tools", "read"])
        if "_max_turns_" in mode:
            command.extend(["--max-turns", "1"])
        if "_max_tokens_" in mode:
            command.extend(["--max-tokens", "5"])
        if "_max_price_" in mode:
            command.extend(["--max-price", "0.000001"])
        if mode.startswith("programmatic_empty_prompt_"):
            command.extend(["-p", "--output", output])
        else:
            command.extend(["-p", "hi", "--output", output])
        return command
    if mode == "default_tui":
        return [*binary, "--trust"]
    if mode == "cli_help":
        return [*binary, "--help"]
    if mode == "cli_version":
        return [*binary, "--version"]
    if mode == "cli_output_invalid":
        return [*binary, "--output", "xml", "-p", "hi"]
    if mode == "cli_agent_auto_approve_conflict":
        return [*binary, "--agent", "plan", "--auto-approve", "-p", "hi"]
    if mode == "cli_agent_not_found":
        return [*binary, "--trust", "--agent", "nope", "-p", "hi"]
    if mode in {"cli_agent_disabled", "cli_agent_enabled_excluded"}:
        return [*binary, "--trust", "--agent", "plan", "-p", "hi"]
    if mode == "cli_agent_subagent":
        return [*binary, "--trust", "--agent", "sub", "-p", "hi"]
    if mode == "cli_agent_lean_missing":
        return [*binary, "--trust", "--agent", "lean", "-p", "hi"]
    if mode in {"cli_default_agent_disabled", "cli_default_agent_enabled_excluded"}:
        return [*binary, "--trust", "-p", "hi"]
    if mode == "cli_workdir_missing":
        return [*binary, "--workdir", str(OUT_DIR / "missing-workdir"), "--trust"]
    if mode == "cli_add_dir_missing":
        return [*binary, "--add-dir", str(OUT_DIR / "missing-add-dir"), "--trust"]
    if mode == "cli_check_upgrade_available":
        return [*binary, "--check-upgrade"]
    if mode == "cli_setup":
        return [*binary, "--setup"]
    if mode == "cli_continue_missing":
        return [*binary, "--trust", "--continue", "-p", "hi"]
    if mode == "cli_resume_missing":
        return [*binary, "--trust", "--resume", "deadbeef", "-p", "hi"]
    if mode == "tui_untrusted_workspace" and microvibe:
        return [*binary, "--tui"]
    if mode == "tui_untrusted_workspace":
        return [*binary]
    if mode == "tui_agent_plan" and microvibe:
        return [*binary, "--trust", "--agent", "plan", "--tui"]
    if mode == "tui_agent_plan":
        return [*binary, "--trust", "--agent", "plan"]
    if mode == "tui_agent_custom" and microvibe:
        return [*binary, "--trust", "--agent", "review-bot", "--tui"]
    if mode == "tui_agent_custom":
        return [*binary, "--trust", "--agent", "review-bot"]
    if mode == "tui_auto_approve" and microvibe:
        return [*binary, "--trust", "--auto-approve", "--tui"]
    if mode == "tui_auto_approve":
        return [*binary, "--trust", "--auto-approve"]
    if mode == "tui_initial_prompt" and microvibe:
        return [*binary, "--trust", "--tui", "hello tui"]
    if mode == "tui_initial_prompt":
        return [*binary, "--trust", "hello tui"]
    if mode == "animation_tui" and microvibe:
        return [*binary, "--trust", "--tui"]
    if mode == "animation_tui":
        return [*binary, "--trust"]
    if mode == "tui" and microvibe:
        return [*binary, "--trust", "--tui"]
    if mode == "tui":
        return [*binary, "--trust"]
    return binary


def is_tui_mode(mode: str) -> bool:
    return mode in {"tui", "default_tui", "animation_tui", "tui_agent_plan", "tui_agent_custom", "tui_auto_approve", "tui_initial_prompt", "tui_untrusted_workspace"}


def programmatic_output_mode(mode: str) -> str:
    return next(
        suffix
        for suffix in ("text", "json", "streaming")
        if mode == f"programmatic_{suffix}" or mode.endswith(f"_{suffix}")
    )


def resolve_command(cmd: list[str]) -> list[str]:
    if not cmd:
        return cmd
    first = cmd[0]
    if "/" in first:
        path = pathlib.Path(first)
        if not path.is_absolute():
            return [str((ROOT / path).resolve()), *cmd[1:]]
    return cmd


def resolve_uv_project_args(cmd: list[str]) -> list[str]:
    resolved = list(cmd)
    for idx, part in enumerate(resolved[:-1]):
        if part == "--project":
            path = pathlib.Path(resolved[idx + 1])
            if not path.is_absolute():
                resolved[idx + 1] = str((ROOT / path).resolve())
    return resolved


def isolated_env(label: str, base: pathlib.Path) -> dict[str, str]:
    home = base / label / "home"
    xdg_config = base / label / "xdg-config"
    xdg_data = base / label / "xdg-data"
    xdg_cache = base / label / "xdg-cache"
    for path in (home, xdg_config, xdg_data, xdg_cache):
        path.mkdir(parents=True, exist_ok=True)

    env = os.environ.copy()
    env.update(
        {
            "HOME": str(home),
            "XDG_CONFIG_HOME": str(xdg_config),
            "XDG_DATA_HOME": str(xdg_data),
            "XDG_CACHE_HOME": str(xdg_cache),
            "TERM": "xterm-256color",
            "COLORTERM": "truecolor",
            "COLUMNS": "120",
            "LINES": "36",
            "MICROVIBE_PARITY": "1",
            "VIBE_PARITY": "1",
            "MISTRAL_API_KEY": env.get("MISTRAL_API_KEY", "microvibe-parity-key"),
            "TEST_API_KEY": env.get("TEST_API_KEY", "microvibe-parity-key"),
            "PYTHONPATH": f"{ROOT / 'dev'}{os.pathsep}{env.get('PYTHONPATH', '')}",
            "UV_CACHE_DIR": str(OUT_DIR / "uv-cache"),
            "UV_PYTHON_INSTALL_DIR": str(OUT_DIR / "uv-python"),
        }
    )
    env.pop("NO_COLOR", None)
    return env


class FakeChatHandler(http.server.BaseHTTPRequestHandler):
    responses: list[dict[str, object]] = []
    requests: list[dict[str, object]] = []
    next_response = 0
    lock = threading.Lock()

    def do_POST(self) -> None:
        request_body = self.rfile.read(int(self.headers.get("content-length", "0") or 0))
        if self.path.endswith("/v1/conversations"):
            response = {
                "conversation_id": "test",
                "outputs": [
                    {
                        "content": [
                            {"text": "Search answer", "type": "text"},
                            {
                                "tool": "web_search",
                                "title": "Source A",
                                "type": "tool_reference",
                                "url": "https://a.example",
                            },
                            {
                                "tool": "web_search",
                                "title": "Source B",
                                "type": "tool_reference",
                                "url": "https://b.example",
                            },
                        ],
                        "object": "entry",
                        "type": "message.output",
                        "role": "assistant",
                    }
                ],
                "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30},
                "object": "conversation.response",
            }
            raw = json.dumps(response).encode("utf-8")
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)
            return
        handler = type(self)
        with handler.lock:
            index = min(handler.next_response, len(handler.responses) - 1)
            response = handler.responses[index]
            handler.next_response += 1
        try:
            request = json.loads(request_body.decode("utf-8"))
        except Exception:
            request = {}
        with handler.lock:
            handler.requests.append(request if isinstance(request, dict) else {})
        if isinstance(response, dict) and response.get("__dynamic_image_echo") is True:
            image_count = count_request_image_urls(request if isinstance(request, dict) else {})
            response = simple_chat_response(f"image-count:{image_count}", prompt_tokens=4, completion_tokens=2)
        if isinstance(request, dict) and request.get("stream") is True:
            raw = streaming_response_from_completion(response)
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("cache-control", "no-cache")
            self.send_header("content-length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)
            return
        raw = json.dumps(response).encode("utf-8")
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self) -> None:
        body = b"fetch parity\n"
        self.send_response(200)
        self.send_header("content-type", "text/plain; charset=utf-8")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args: object) -> None:
        return


class FakeBrowserAuthHandler(http.server.BaseHTTPRequestHandler):
    requests: list[dict[str, object]] = []
    poll_status: str = "completed"
    lock = threading.Lock()

    def do_POST(self) -> None:
        request_body = self.rfile.read(int(self.headers.get("content-length", "0") or 0))
        try:
            request = json.loads(request_body.decode("utf-8"))
        except Exception:
            request = {}
        with type(self).lock:
            type(self).requests.append({"path": self.path, "body": request if isinstance(request, dict) else {}})
        if self.path == "/api/vibe/sign-in":
            port = int(self.server.server_address[1])
            body = {
                "process_id": "process-123",
                "sign_in_url": f"http://127.0.0.1:{port}/codestral/cli/authenticate#process_id=process-123&complete_token=complete-token&state=state",
                "poll_url": f"http://127.0.0.1:{port}/api/vibe/sign-in/poll/poll-token-1",
                "expires_at": "2027-04-23T12:00:00Z",
            }
            self._json(200, body)
            return
        if self.path == "/api/vibe/sign-in/process-123/exchange":
            self._json(200, {"api_key": "sk-browser-key"})
            return
        self._json(404, {"error": "not found"})

    def do_GET(self) -> None:
        if self.path == "/api/vibe/sign-in/poll/poll-token-1":
            if type(self).poll_status == "pending":
                self._json(200, {"status": "pending"})
            else:
                self._json(200, {"status": "completed", "exchange_token": "exchange-1"})
            return
        self._json(404, {"error": "not found"})

    def _json(self, status: int, body: dict[str, object]) -> None:
        raw = json.dumps(body).encode("utf-8")
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def log_message(self, *args: object) -> None:
        return


class ThreadingTCPServer(socketserver.ThreadingMixIn, socketserver.TCPServer):
    allow_reuse_address = True
    daemon_threads = True


def count_request_image_urls(request: dict[str, object]) -> int:
    raw_messages = request.get("messages")
    if not isinstance(raw_messages, list):
        return 0
    count = 0
    for message in raw_messages:
        if not isinstance(message, dict) or message.get("role") != "user":
            continue
        content = message.get("content")
        if not isinstance(content, list):
            continue
        for block in content:
            if not isinstance(block, dict):
                continue
            if block.get("type") == "image_url" and isinstance(block.get("image_url"), dict):
                count += 1
            elif block.get("type") == "input_image" and isinstance(block.get("image_url"), str):
                count += 1
    return count


def simple_chat_response(content: str, *, prompt_tokens: int = 3, completion_tokens: int = 2) -> dict[str, object]:
    return {
        "id": "chatcmpl_parity",
        "object": "chat.completion",
        "created": 0,
        "model": "test-model",
        "choices": [
            {
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop",
            }
        ],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
        },
    }


def streaming_response_from_completion(response: dict[str, object]) -> bytes:
    choice = (response.get("choices") or [{}])[0]
    if not isinstance(choice, dict):
        choice = {}
    message = choice.get("message") if isinstance(choice.get("message"), dict) else {}
    if not isinstance(message, dict):
        message = {}
    model = str(response.get("model") or "test-model")
    response_id = str(response.get("id") or "chatcmpl_parity_stream")
    chunks: list[dict[str, object]] = []
    content = str(message.get("content") or "")
    reasoning = str(message.get("reasoning_content") or message.get("reasoning") or "")
    tool_calls = message.get("tool_calls")
    if isinstance(tool_calls, list) and tool_calls:
        stream_tool_calls = []
        for idx, call in enumerate(tool_calls):
            if isinstance(call, dict):
                item = dict(call)
                item["index"] = idx
                stream_tool_calls.append(item)
            else:
                stream_tool_calls.append(call)
        chunks.append({
            "id": response_id,
            "object": "chat.completion.chunk",
            "created": 0,
            "model": model,
            "choices": [{"index": 0, "delta": {"tool_calls": stream_tool_calls}, "finish_reason": None}],
        })
        finish_reason = "tool_calls"
    else:
        if reasoning:
            chunks.append({
                "id": response_id,
                "object": "chat.completion.chunk",
                "created": 0,
                "model": model,
                "choices": [{"index": 0, "delta": {"reasoning_content": reasoning}, "finish_reason": None}],
            })
        chunks.append({
            "id": response_id,
            "object": "chat.completion.chunk",
            "created": 0,
            "model": model,
            "choices": [{"index": 0, "delta": {"content": content}, "finish_reason": None}],
        })
        finish_reason = "stop"
    chunks.append({
        "id": response_id,
        "object": "chat.completion.chunk",
        "created": 0,
        "model": model,
        "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}],
    })
    chunks.append({
        "id": response_id,
        "object": "chat.completion.chunk",
        "created": 0,
        "model": model,
        "choices": [],
        "usage": response.get("usage") or {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
    })
    body = bytearray()
    for chunk in chunks:
        body.extend(f"data: {json.dumps(chunk, separators=(',', ':'))}\n\n".encode("utf-8"))
    body.extend(b"data: [DONE]\n\n")
    return bytes(body)


def programmatic_responses(case: Case, workspace: pathlib.Path, port: int) -> list[dict[str, object]]:
    read_path = workspace / "sample.txt"
    tool_file = workspace / "tool-output.txt"
    final_response = {
        "id": "chatcmpl_parity_final",
        "object": "chat.completion",
        "created": 0,
        "model": "test-model",
        "choices": [
            {
                "index": 0,
                "message": {"role": "assistant", "content": "hello parity"},
                "finish_reason": "stop",
            }
        ],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
    }
    if "_hooks_before_" in case.mode:
        return [
            tool_response(
                "call_hook_before_read_1",
                "read",
                {"file_path": str(read_path)},
            ),
            {
                **final_response,
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": "hook before complete"},
                        "finish_reason": "stop",
                    }
                ],
            },
        ]
    if "_hooks_after_" in case.mode:
        return [
            tool_response(
                "call_hook_after_read_1",
                "read",
                {"file_path": str(read_path)},
            ),
            {
                **final_response,
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": "hook after complete"},
                        "finish_reason": "stop",
                    }
                ],
            },
        ]
    if "_hooks_post_" in case.mode:
        return [
            {
                **final_response,
                "id": "chatcmpl_hook_post_first",
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": "first post response"},
                        "finish_reason": "stop",
                    }
                ],
            },
            {
                **final_response,
                "id": "chatcmpl_hook_post_second",
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": "post retry complete"},
                        "finish_reason": "stop",
                    }
                ],
            },
        ]
    if "_read_" not in case.mode or "_task_read_" in case.mode:
        if "_max_turns_" in case.mode or "_max_tokens_" in case.mode or "_max_price_" in case.mode:
            tool_call = tool_response(
                "call_limit_read_1",
                "read",
                {"file_path": str(read_path)},
            )
            if "_max_tokens_" in case.mode:
                tool_call["usage"] = {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}
            if "_max_price_" in case.mode:
                tool_call["usage"] = {"prompt_tokens": 1_000_000, "completion_tokens": 1_000_000, "total_tokens": 2_000_000}
            return [
                tool_call,
                {
                    **final_response,
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "limit should stop before this",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                },
            ]
        if "_web_fetch_" in case.mode:
            return [
                tool_response(
                    "call_web_fetch_1",
                    "web_fetch",
                    {"url": f"http://127.0.0.1:{port}/fetch.txt"},
                ),
                {
                    **final_response,
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "web fetch complete",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                },
            ]
        if "_web_search_" in case.mode:
            return [
                tool_response(
                    "call_web_search_1",
                    "web_search",
                    {"query": "parity search query"},
                ),
                {
                    **final_response,
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "web search complete",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                },
            ]
        if "_mcp_stdio_" in case.mode:
            return [
                tool_response(
                    "call_mcp_stdio_1",
                    "local-demo_lookup",
                    {"query": "alpha"},
                ),
                {
                    **final_response,
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "mcp complete",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                },
            ]
        if "_skill_" in case.mode:
            return [
                tool_response(
                    "call_skill_1",
                    "skill",
                    {"name": "parity-skill"},
                ),
                {
                    **final_response,
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "skill complete",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                },
            ]
        if "_question_" in case.mode:
            return [
                tool_response(
                    "call_question_1",
                    "ask_user_question",
                    {
                        "questions": [
                            {
                                "question": "Choose parity mode?",
                                "header": "Parity",
                                "options": [
                                    {
                                        "label": "Strict",
                                        "description": "Require exact parity",
                                    },
                                    {
                                        "label": "Loose",
                                        "description": "Allow differences",
                                    },
                                ],
                            }
                        ],
                    },
                ),
                {
                    **final_response,
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "question complete",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                },
            ]
        if "_exit_plan_" in case.mode:
            return [
                tool_response("call_exit_plan_1", "exit_plan_mode", {}),
                {
                    **final_response,
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "exit plan complete",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                },
            ]
        if "_enabled_tools_" in case.mode:
            return [
                tool_response(
                    "call_enabled_tools_bash_1",
                    "bash",
                    {"command": "printf disabled"},
                ),
                {
                    **final_response,
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "enabled tools complete",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                },
            ]
        if "_agent_custom_" in case.mode:
            return [
                tool_response(
                    "call_custom_agent_bash_1",
                    "bash",
                    {"command": "printf custom-agent-disabled"},
                ),
                {
                    **final_response,
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "custom agent complete",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                },
            ]
        if "_task_unknown_" in case.mode:
            return [
                tool_response(
                    "call_task_unknown_1",
                    "task",
                    {"task": "Check unknown agent handling", "agent": "no-such-agent"},
                ),
                {
                    **final_response,
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "task unknown complete",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                },
            ]
        if "_task_custom_" in case.mode:
            return [
                tool_response(
                    "call_task_custom_1",
                    "task",
                    {"task": "Read sample.txt and report the first word", "agent": "reader"},
                ),
                tool_response(
                    "call_custom_subagent_read_1",
                    "read",
                    {"file_path": str(read_path)},
                ),
                {
                    **final_response,
                    "id": "chatcmpl_parity_custom_subagent_final",
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "Custom subagent read alpha.",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
                },
                {
                    **final_response,
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "task custom complete",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                },
            ]
        if "_task_read_" in case.mode:
            return [
                tool_response(
                    "call_task_read_1",
                    "task",
                    {"task": "Read sample.txt and report the first word", "agent": "explore"},
                ),
                tool_response(
                    "call_subagent_read_1",
                    "read",
                    {"file_path": str(read_path)},
                ),
                {
                    **final_response,
                    "id": "chatcmpl_parity_subagent_read_final",
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "Subagent read alpha.",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
                },
                {
                    **final_response,
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "task read complete",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                },
            ]
        if "_task_" in case.mode:
            return [
                tool_response(
                    "call_task_1",
                    "task",
                    {"task": "Inspect sample.txt and report the first word", "agent": "explore"},
                ),
                {
                    **final_response,
                    "id": "chatcmpl_parity_subagent",
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "Subagent found alpha.",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {"prompt_tokens": 4, "completion_tokens": 3, "total_tokens": 7},
                },
                {
                    **final_response,
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "task complete",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                },
            ]
        if "_state_" in case.mode:
            return [
                tool_response(
                    "call_todo_write_1",
                    "todo",
                    {
                        "action": "write",
                        "todos": [
                            {
                                "id": "1",
                                "content": "Check parity",
                                "status": "in_progress",
                                "priority": "high",
                            }
                        ],
                    },
                ),
                tool_response("call_todo_read_1", "todo", {"action": "read"}),
                {
                    **final_response,
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "state tools complete",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                },
            ]
        if "_tools_" not in case.mode:
            return [final_response]
        return [
            tool_response(
                "call_bash_1",
                "bash",
                {"command": "printf bash-parity"},
            ),
            tool_response(
                "call_write_1",
                "write_file",
                {"path": str(tool_file), "content": "needle\nold\n"},
            ),
            tool_response(
                "call_edit_1",
                "edit",
                {
                    "file_path": str(tool_file),
                    "old_string": "old",
                    "new_string": "new",
                },
            ),
            tool_response(
                "call_grep_1",
                "grep",
                {"pattern": "needle", "path": str(workspace), "max_matches": 10},
            ),
            {
                **final_response,
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": "tools complete"},
                        "finish_reason": "stop",
                    }
                ],
            },
        ]

    args = json.dumps({"file_path": str(read_path)}, separators=(",", ":"))
    return [
        tool_response("call_read_1", "read", json.loads(args)),
        {
            **final_response,
            "choices": [
                {
                    "index": 0,
                    "message": {"role": "assistant", "content": "read complete"},
                    "finish_reason": "stop",
                }
            ],
        },
    ]


def tool_response(call_id: str, name: str, arguments: dict[str, object]) -> dict[str, object]:
    return {
        "id": f"chatcmpl_parity_{call_id}",
        "object": "chat.completion",
        "created": 0,
        "model": "test-model",
        "choices": [
            {
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [
                        {
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": json.dumps(arguments, separators=(",", ":")),
                            },
                        }
                    ],
                },
                "finish_reason": "tool_calls",
            }
        ],
        "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4},
    }


def write_programmatic_configs(base: pathlib.Path, port: int) -> None:
    vibe_home = base / "vibe" / "home" / ".vibe"
    vibe_home.mkdir(parents=True, exist_ok=True)
    (vibe_home / "config.toml").write_text(
        textwrap.dedent(
            f"""
            active_model = "test"
            default_agent = "default"
            disabled_tools = ["exit_plan_mode"]
            enable_telemetry = false
            enable_update_checks = false
            enable_notifications = false
            include_commit_signature = false
            include_model_info = false
            include_project_context = false
            include_prompt_detail = false

                [session_logging]
                enabled = false

            [[providers]]
            name = "test-provider"
            api_base = "http://127.0.0.1:{port}/v1"
            api_key_env_var = "TEST_API_KEY"
            api_style = "openai"

            [[providers]]
            name = "mistral"
            api_base = "http://127.0.0.1:{port}/v1"
            api_key_env_var = "TEST_API_KEY"
            backend = "mistral"

            [[providers]]
            name = "mistral"
            api_base = "http://127.0.0.1:{port}/v1"
            api_key_env_var = "TEST_API_KEY"
            backend = "mistral"

            [[models]]
            name = "test-model"
            provider = "test-provider"
            alias = "test"
            temperature = 0.1
            input_price = 0.0
            output_price = 0.0
            """
        ).strip()
        + "\n",
        encoding="utf-8",
    )
    write_parity_skill(vibe_home)

    micro_home = base / "microvibe" / "home"
    for config_dir in [
        micro_home / "Library" / "Application Support" / "microvibe",
        base / "microvibe" / "xdg-config" / "microvibe",
    ]:
        config_dir.mkdir(parents=True, exist_ok=True)
        (config_dir / "config.toml").write_text(
            textwrap.dedent(
                f"""
                [model]
                provider = "test-provider"
                name = "test-model"
                temperature = 0.1
                max_context_tokens = 200000
                input_price = 0.0
                output_price = 0.0

                [providers.test-provider]
                base_url = "http://127.0.0.1:{port}/v1"
                api_key_env = "TEST_API_KEY"
                wire_format = "openai_chat"

                [permissions]
                mode = "ask"
                """
            ).strip()
            + "\n",
            encoding="utf-8",
        )


def write_acp_unsupported_provider_configs(base: pathlib.Path) -> None:
    vibe_home = base / "vibe" / "home" / ".vibe"
    vibe_home.mkdir(parents=True, exist_ok=True)
    (vibe_home / "config.toml").write_text(
        textwrap.dedent(
            """
            active_model = "local"
            default_agent = "default"
            enable_telemetry = false
            enable_update_checks = false
            enable_notifications = false

            [[providers]]
            name = "llamacpp"
            api_base = "http://127.0.0.1:8080/v1"
            api_key_env_var = "LLAMACPP_API_KEY"
            backend = "generic"

            [[models]]
            name = "local-model"
            provider = "llamacpp"
            alias = "local"
            temperature = 0.1
            input_price = 0.0
            output_price = 0.0
            """
        ).strip()
        + "\n",
        encoding="utf-8",
    )

    micro_home = base / "microvibe" / "home"
    for config_dir in [
        micro_home / "Library" / "Application Support" / "microvibe",
        base / "microvibe" / "xdg-config" / "microvibe",
    ]:
        config_dir.mkdir(parents=True, exist_ok=True)
        (config_dir / "config.toml").write_text(
            textwrap.dedent(
                """
                [model]
                provider = "llamacpp"
                name = "local-model"
                temperature = 0.1
                max_context_tokens = 200000
                input_price = 0.0
                output_price = 0.0

                [providers.llamacpp]
                base_url = "http://127.0.0.1:8080/v1"
                api_key_env = "LLAMACPP_API_KEY"
                backend = "generic"
                wire_format = "openai_chat"

                [permissions]
                mode = "ask"
                """
            ).strip()
            + "\n",
            encoding="utf-8",
        )


def write_acp_browser_auth_configs(base: pathlib.Path, auth_base_url: str) -> None:
    vibe_home = base / "vibe" / "home" / ".vibe"
    vibe_home.mkdir(parents=True, exist_ok=True)
    (vibe_home / "config.toml").write_text(
        textwrap.dedent(
            f"""
            active_model = "mistral"
            default_agent = "default"
            enable_telemetry = false
            enable_update_checks = false
            enable_notifications = false

            [[providers]]
            name = "mistral"
            api_base = "http://127.0.0.1:8080/v1"
            api_key_env_var = "MISTRAL_API_KEY"
            backend = "mistral"
            browser_auth_base_url = "{auth_base_url}"
            browser_auth_api_base_url = "{auth_base_url}/api"

            [[models]]
            name = "mistral-medium"
            provider = "mistral"
            alias = "mistral"
            temperature = 0.1
            input_price = 0.0
            output_price = 0.0
            """
        ).strip()
        + "\n",
        encoding="utf-8",
    )

    micro_home = base / "microvibe" / "home"
    for config_dir in [
        micro_home / "Library" / "Application Support" / "microvibe",
        base / "microvibe" / "xdg-config" / "microvibe",
    ]:
        config_dir.mkdir(parents=True, exist_ok=True)
        (config_dir / "config.toml").write_text(
            textwrap.dedent(
                f"""
                [model]
                provider = "mistral"
                name = "mistral-medium"
                temperature = 0.1
                max_context_tokens = 200000
                input_price = 0.0
                output_price = 0.0

                [providers.mistral]
                base_url = "http://127.0.0.1:8080/v1"
                api_key_env = "MISTRAL_API_KEY"
                backend = "mistral"
                browser_auth_base_url = "{auth_base_url}"
                browser_auth_api_base_url = "{auth_base_url}/api"
                wire_format = "openai_chat"

                [permissions]
                mode = "ask"
                """
            ).strip()
            + "\n",
            encoding="utf-8",
        )


def write_session_configs(
    base: pathlib.Path,
    port: int,
    *,
    supports_images: bool = False,
    extra_model: bool = False,
    input_price: float = 0.0,
    output_price: float = 0.0,
) -> None:
    supports_images_line = "supports_images = true" if supports_images else ""
    vibe_extra_model = (
        textwrap.dedent(
            """

            [[models]]
            name = "alt-model"
            provider = "test-provider"
            alias = "alt"
            temperature = 0.1
            input_price = 0.0
            output_price = 0.0
            """
        ).rstrip()
        if extra_model
        else ""
    )
    micro_extra_model = (
        textwrap.dedent(
            """

                    [[models]]
                    name = "alt-model"
                    provider = "test-provider"
                    alias = "alt"
                    thinking = "off"
                    """
        ).rstrip()
        if extra_model
        else ""
    )
    vibe_home = base / "vibe" / "home" / ".vibe"
    vibe_home.mkdir(parents=True, exist_ok=True)
    (vibe_home / "config.toml").write_text(
        textwrap.dedent(
            f"""
            active_model = "test"
            default_agent = "default"
            disabled_tools = ["exit_plan_mode"]
            enable_telemetry = false
            enable_update_checks = false
            enable_notifications = false
            include_commit_signature = false
            include_model_info = false
            include_project_context = false
            include_prompt_detail = false

            [session_logging]
            enabled = true

            [[providers]]
            name = "test-provider"
            api_base = "http://127.0.0.1:{port}/v1"
            api_key_env_var = "TEST_API_KEY"
            api_style = "openai"

            [[models]]
            name = "test-model"
            provider = "test-provider"
            alias = "test"
            temperature = 0.1
            input_price = {input_price}
            output_price = {output_price}
            {supports_images_line}
            {vibe_extra_model}
            """
        ).strip()
        + "\n",
        encoding="utf-8",
    )
    write_parity_skill(vibe_home)

    for label in ["microvibe"]:
        micro_home = base / label / "home"
        micro_vibe_home = micro_home / ".vibe"
        micro_vibe_home.mkdir(parents=True, exist_ok=True)
        write_parity_skill(micro_vibe_home)
        for config_dir in [
            micro_home / "Library" / "Application Support" / "microvibe",
            base / label / "xdg-config" / "microvibe",
        ]:
            config_dir.mkdir(parents=True, exist_ok=True)
            (config_dir / "config.toml").write_text(
                textwrap.dedent(
                    f"""
                    active_model = "test"

                    [model]
                    provider = "test-provider"
                    name = "test[off]"
                    temperature = 0.1
                    max_context_tokens = 200000
                    input_price = {input_price}
                    output_price = {output_price}

                    [[models]]
                    name = "test-model"
                    provider = "test-provider"
                    alias = "test"
                    thinking = "off"
                    {supports_images_line}
                    {micro_extra_model}

                    [providers.test-provider]
                    base_url = "http://127.0.0.1:{port}/v1"
                    api_key_env = "TEST_API_KEY"
                    wire_format = "openai_chat"

                    [permissions]
                    mode = "ask"
                    """
                ).strip()
                + "\n",
                encoding="utf-8",
            )


def seed_bash_allowlist(base: pathlib.Path) -> None:
    for path in [
        base / "vibe" / "home" / ".vibe" / "config.toml",
        base
        / "microvibe"
        / "home"
        / "Library"
        / "Application Support"
        / "microvibe"
        / "config.toml",
        base / "microvibe" / "xdg-config" / "microvibe" / "config.toml",
    ]:
        path.write_text(
            path.read_text(encoding="utf-8")
            + "\n[tools.bash]\nallowlist = [\"printf\"]\n",
            encoding="utf-8",
        )


def seed_grep_ask(base: pathlib.Path) -> None:
    for path in [
        base / "vibe" / "home" / ".vibe" / "config.toml",
        base
        / "microvibe"
        / "home"
        / "Library"
        / "Application Support"
        / "microvibe"
        / "config.toml",
        base / "microvibe" / "xdg-config" / "microvibe" / "config.toml",
    ]:
        path.write_text(
            path.read_text(encoding="utf-8")
            + "\n[tools.grep]\npermission = \"ask\"\n",
            encoding="utf-8",
        )


def seed_programmatic_hooks(base: pathlib.Path, case_name: str, workspace: pathlib.Path) -> None:
    rewritten = workspace / "rewritten.txt"
    rewritten.write_text("rewritten hook file\n", encoding="utf-8")
    hook_dir = base / "hooks"
    hook_dir.mkdir(parents=True, exist_ok=True)
    counter = hook_dir / "post_hook_count.txt"
    if case_name == "programmatic_hooks_before_json":
        script = hook_dir / "before_hook.py"
        script.write_text(
            "import json\n"
            f"print(json.dumps({{'hook_specific_output': {{'tool_input': {{'file_path': {str(rewritten)!r}}}}}}}))\n",
            encoding="utf-8",
        )
        hook_type = "before_tool"
        hook_name = "rewrite-read"
    elif case_name == "programmatic_hooks_after_json":
        script = hook_dir / "after_hook.py"
        script.write_text(
            "import json\n"
            "print(json.dumps({'hook_specific_output': {'additional_context': 'hook context'}}))\n",
            encoding="utf-8",
        )
        hook_type = "after_tool"
        hook_name = "append-read"
    elif case_name == "programmatic_hooks_post_json":
        script = hook_dir / "post_hook.py"
        script.write_text(
            "import json\n"
            "from pathlib import Path\n"
            f"p = Path({str(counter)!r})\n"
            "count = int(p.read_text()) if p.exists() else 0\n"
            "p.write_text(str(count + 1))\n"
            "if count == 0:\n"
            "    print(json.dumps({'decision': 'deny', 'reason': 'fix this'}))\n",
            encoding="utf-8",
        )
        hook_type = "post_agent_turn"
        hook_name = "retry-turn"
    else:
        raise AssertionError(f"unknown hook case {case_name}")
    script.chmod(0o755)
    match_line = 'match = "read"\n' if hook_type != "post_agent_turn" else ""
    hooks_toml = (
        textwrap.dedent(
            f"""
            [[hooks]]
            name = "{hook_name}"
            type = "{hook_type}"
            command = "{shlex.quote(sys.executable)} {shlex.quote(str(script))}"
            """
        ).strip()
        + "\n"
        + match_line
    )
    for vibe_home in [
        base / "vibe" / "home" / ".vibe",
        base / "microvibe" / "home" / ".vibe",
    ]:
        vibe_home.mkdir(parents=True, exist_ok=True)
        (vibe_home / "hooks.toml").write_text(hooks_toml, encoding="utf-8")
    for config_path in [
        base / "vibe" / "home" / ".vibe" / "config.toml",
        base / "microvibe" / "home" / "Library" / "Application Support" / "microvibe" / "config.toml",
        base / "microvibe" / "xdg-config" / "microvibe" / "config.toml",
    ]:
        raw = config_path.read_text(encoding="utf-8")
        raw = re.sub(r"(?m)^enable_experimental_hooks = true\n", "", raw)
        config_path.write_text(
            "enable_experimental_hooks = true\n" + raw,
            encoding="utf-8",
        )


def seed_plan_agent(base: pathlib.Path) -> None:
    (base / "vibe" / "home" / ".vibe" / "plans").mkdir(parents=True, exist_ok=True)
    (base / "microvibe" / "home" / ".vibe" / "plans").mkdir(parents=True, exist_ok=True)
    for path in [
        base / "vibe" / "home" / ".vibe" / "config.toml",
        base
        / "microvibe"
        / "home"
        / "Library"
        / "Application Support"
        / "microvibe"
        / "config.toml",
        base / "microvibe" / "xdg-config" / "microvibe" / "config.toml",
    ]:
        raw = path.read_text(encoding="utf-8")
        raw = raw.replace('default_agent = "default"\n', 'default_agent = "plan"\n')
        raw = raw.replace('disabled_tools = ["exit_plan_mode"]\n', "")
        if 'default_agent = "plan"' not in raw:
            raw = 'default_agent = "plan"\n' + raw
        path.write_text(raw, encoding="utf-8")


def seed_mcp_config(base: pathlib.Path) -> None:
    vibe_home = base / "vibe" / "home" / ".vibe"
    vibe_home.mkdir(parents=True, exist_ok=True)
    vibe_config = vibe_home / "config.toml"
    vibe_config.write_text(
        vibe_config.read_text(encoding="utf-8") if vibe_config.exists() else "",
        encoding="utf-8",
    )
    vibe_config.write_text(
        vibe_config.read_text(encoding="utf-8")
        + textwrap.dedent(
            """

            enable_telemetry = false
            enable_update_checks = false
            enable_notifications = false
            include_commit_signature = false
            include_model_info = false
            include_project_context = false
            include_prompt_detail = false

            [[mcp_servers]]
            name = "local-demo"
            transport = "stdio"
            command = "definitely-missing-mcp-command"
            disabled = true
            """
        ).strip()
        + "\n",
        encoding="utf-8",
    )

    for path in [
        base
        / "microvibe"
        / "home"
        / "Library"
        / "Application Support"
        / "microvibe"
        / "config.toml",
        base / "microvibe" / "xdg-config" / "microvibe" / "config.toml",
    ]:
        path.parent.mkdir(parents=True, exist_ok=True)
        if not path.exists():
            path.write_text(
                textwrap.dedent(
                    """
                    [model]
                    provider = "mistral"
                    name = "mistral-medium-3.5[high]"
                    temperature = 0.1
                    max_context_tokens = 200000
                    input_price = 1.5
                    output_price = 7.5

                    [providers.mistral]
                    base_url = "https://api.mistral.ai/v1"
                    api_key_env = "MISTRAL_API_KEY"
                    wire_format = "openai_chat"

                    [permissions]
                    mode = "ask"
                    """
                ).strip()
                + "\n",
                encoding="utf-8",
            )
        path.write_text(
            path.read_text(encoding="utf-8")
            + textwrap.dedent(
                """

                [[mcp_servers]]
                name = "local-demo"
                transport = "stdio"
                command = "definitely-missing-mcp-command"
                disabled = true
                """
            ).strip()
            + "\n",
            encoding="utf-8",
        )


def write_stdio_mcp_fixture(base: pathlib.Path) -> pathlib.Path:
    server = base / "stdio_mcp_server.py"
    server.write_text(
        textwrap.dedent(
            """
            import json
            import sys

            def send(message):
                sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\\n")
                sys.stdout.flush()

            for line in sys.stdin:
                if not line.strip():
                    continue
                message = json.loads(line)
                method = message.get("method")
                if method == "initialize":
                    send({
                        "jsonrpc": "2.0",
                        "id": message.get("id"),
                        "result": {
                            "protocolVersion": "2024-11-05",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "parity-mcp", "version": "1.0.0"},
                        },
                    })
                elif method == "tools/list":
                    send({
                        "jsonrpc": "2.0",
                        "id": message.get("id"),
                        "result": {
                            "tools": [
                                {
                                    "name": "lookup",
                                    "description": "Look up parity facts",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": {
                                            "query": {
                                                "type": "string",
                                                "description": "Query text",
                                            }
                                        },
                                        "required": ["query"],
                                    },
                                },
                                {
                                    "name": "disabled_tool",
                                    "description": "Disabled parity tool",
                                    "inputSchema": {"type": "object", "properties": {}},
                                },
                            ]
                        },
                    })
                elif method == "tools/call":
                    args = message.get("params", {}).get("arguments", {})
                    query = args.get("query", "")
                    send({
                        "jsonrpc": "2.0",
                        "id": message.get("id"),
                        "result": {
                            "content": [
                                {
                                    "type": "text",
                                    "text": "lookup result: " + query,
                                }
                            ],
                            "isError": False,
                        },
                    })
            """
        ).strip()
        + "\n",
        encoding="utf-8",
    )
    return server


def seed_mcp_stdio_tools_config(base: pathlib.Path) -> None:
    seed_mcp_stdio_tools_config_with_disabled_tools(base, ["disabled_tool"])


def seed_mcp_stdio_tools_enabled_config(base: pathlib.Path) -> None:
    seed_mcp_stdio_tools_config_with_disabled_tools(base, [])


def seed_mcp_stdio_tools_config_with_disabled_tools(base: pathlib.Path, disabled_tools: list[str]) -> None:
    server = write_stdio_mcp_fixture(base)
    disabled_tools_line = ""
    if disabled_tools:
        disabled_tools_json = json.dumps(disabled_tools)
        disabled_tools_line = f"\n            disabled_tools = {disabled_tools_json}"
    vibe_home = base / "vibe" / "home" / ".vibe"
    vibe_home.mkdir(parents=True, exist_ok=True)
    (vibe_home / "config.toml").write_text(
        textwrap.dedent(
            f"""
            enable_telemetry = false
            enable_update_checks = false
            enable_notifications = false
            include_commit_signature = false
            include_model_info = false
            include_project_context = false
            include_prompt_detail = false

            [[mcp_servers]]
            name = "local-demo"
            transport = "stdio"
            command = "{sys.executable}"
            args = ["{server}"]{disabled_tools_line}
            """
        ).strip()
        + "\n",
        encoding="utf-8",
    )

    for path in [
        base
        / "microvibe"
        / "home"
        / "Library"
        / "Application Support"
        / "microvibe"
        / "config.toml",
        base / "microvibe" / "xdg-config" / "microvibe" / "config.toml",
    ]:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            textwrap.dedent(
                f"""
                [model]
                provider = "mistral"
                name = "mistral-medium-3.5[high]"
                temperature = 0.1
                max_context_tokens = 200000
                input_price = 1.5
                output_price = 7.5

                [providers.mistral]
                base_url = "https://api.mistral.ai/v1"
                api_key_env = "MISTRAL_API_KEY"
                wire_format = "openai_chat"

                [permissions]
                mode = "ask"

                [[mcp_servers]]
                name = "local-demo"
                transport = "stdio"
                command = "{sys.executable}"
                args = ["{server}"]{disabled_tools_line}
                """
            ).strip()
            + "\n",
            encoding="utf-8",
        )


def seed_voice_enabled_config(base: pathlib.Path) -> None:
    vibe_home = base / "vibe" / "home" / ".vibe"
    vibe_home.mkdir(parents=True, exist_ok=True)
    (vibe_home / "config.toml").write_text(
        "voice_mode_enabled = true\n",
        encoding="utf-8",
    )

    for path in [
        base
        / "microvibe"
        / "home"
        / "Library"
        / "Application Support"
        / "microvibe"
        / "config.toml",
        base / "microvibe" / "xdg-config" / "microvibe" / "config.toml",
    ]:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            textwrap.dedent(
                """
                voice_mode_enabled = true

                [model]
                provider = "mistral"
                name = "mistral-medium-3.5[high]"
                temperature = 0.1
                max_context_tokens = 200000
                input_price = 1.5
                output_price = 7.5

                [providers.mistral]
                base_url = "https://api.mistral.ai/v1"
                api_key_env = "MISTRAL_API_KEY"
                wire_format = "openai_chat"

                [permissions]
                mode = "ask"
                """
            ).strip()
            + "\n",
            encoding="utf-8",
        )


def seed_programmatic_mcp_stdio_config(base: pathlib.Path) -> None:
    server = write_stdio_mcp_fixture(base)
    vibe_config = base / "vibe" / "home" / ".vibe" / "config.toml"
    vibe_config.write_text(
        vibe_config.read_text(encoding="utf-8")
        + textwrap.dedent(
            f"""

            [[mcp_servers]]
            name = "local-demo"
            transport = "stdio"
            command = "{sys.executable}"
            args = ["{server}"]
            """
        ).strip()
        + "\n",
        encoding="utf-8",
    )
    for path in [
        base
        / "microvibe"
        / "home"
        / "Library"
        / "Application Support"
        / "microvibe"
        / "config.toml",
        base / "microvibe" / "xdg-config" / "microvibe" / "config.toml",
    ]:
        path.write_text(
            path.read_text(encoding="utf-8")
            + textwrap.dedent(
                f"""

                [[mcp_servers]]
                name = "local-demo"
                transport = "stdio"
                command = "{sys.executable}"
                args = ["{server}"]
                """
            ).strip()
            + "\n",
            encoding="utf-8",
        )


def seed_proxy_preserve_env(base: pathlib.Path, label: str) -> None:
    env_path = base / label / "home" / ".vibe" / ".env"
    env_path.parent.mkdir(parents=True, exist_ok=True)
    env_path.write_text("MISTRAL_API_KEY='sk-existing'\n", encoding="utf-8")


def seed_proxy_unset_env(base: pathlib.Path, label: str) -> None:
    env_path = base / label / "home" / ".vibe" / ".env"
    env_path.parent.mkdir(parents=True, exist_ok=True)
    env_path.write_text(
        "MISTRAL_API_KEY='sk-existing'\nHTTP_PROXY='http://old.proxy:8080'\n",
        encoding="utf-8",
    )


def write_parity_skill(vibe_home: pathlib.Path) -> None:
    skill_dir = vibe_home / "skills" / "parity-skill"
    (skill_dir / "references").mkdir(parents=True, exist_ok=True)
    (skill_dir / "SKILL.md").write_text(
        textwrap.dedent(
            """
            ---
            name: parity-skill
            description: Skill used by the microvibe parity harness.
            ---
            Follow the parity workflow exactly.

            Use references/checklist.md when more detail is needed.
            """
        ).strip()
        + "\n",
        encoding="utf-8",
    )
    (skill_dir / "references" / "checklist.md").write_text(
        "Check skill parity output.\n",
        encoding="utf-8",
    )


def write_custom_subagent(vibe_home: pathlib.Path) -> None:
    agents_dir = vibe_home / "agents"
    agents_dir.mkdir(parents=True, exist_ok=True)
    (agents_dir / "reader.toml").write_text(
        textwrap.dedent(
            """
            display_name = "Reader"
            description = "Read-only custom subagent for parity"
            safety = "safe"
            agent_type = "subagent"
            enabled_tools = ["read"]
            """
        ).strip()
        + "\n",
        encoding="utf-8",
    )


def write_custom_primary_agent(vibe_home: pathlib.Path) -> None:
    agents_dir = vibe_home / "agents"
    agents_dir.mkdir(parents=True, exist_ok=True)
    (agents_dir / "review-bot.toml").write_text(
        textwrap.dedent(
            """
            display_name = "Review Bot"
            description = "Custom primary agent for parity"
            safety = "neutral"
            agent_type = "agent"
            enabled_tools = ["read"]
            """
        ).strip()
        + "\n",
        encoding="utf-8",
    )


AGENT_DIAGNOSTIC_MODES = {
    "cli_agent_not_found",
    "cli_agent_disabled",
    "cli_agent_enabled_excluded",
    "cli_agent_subagent",
    "cli_agent_lean_missing",
    "cli_default_agent_disabled",
    "cli_default_agent_enabled_excluded",
}


def write_agent_diagnostic_configs(base: pathlib.Path, mode: str) -> None:
    default_agent = "default"
    extra = ""
    if mode == "cli_agent_disabled":
        extra = 'disabled_agents = ["plan"]\n'
    elif mode == "cli_agent_enabled_excluded":
        extra = 'enabled_agents = ["default"]\n'
    elif mode == "cli_default_agent_disabled":
        extra = 'disabled_agents = ["default"]\n'
    elif mode == "cli_default_agent_enabled_excluded":
        extra = 'enabled_agents = ["plan"]\n'

    vibe_home = base / "vibe" / "home" / ".vibe"
    vibe_home.mkdir(parents=True, exist_ok=True)
    (vibe_home / "config.toml").write_text(
        textwrap.dedent(
            f"""
            active_model = "test"
            default_agent = "{default_agent}"
            {extra}enable_telemetry = false
            enable_update_checks = false
            enable_notifications = false
            include_commit_signature = false
            include_model_info = false
            include_project_context = false
            include_prompt_detail = false

            [session_logging]
            enabled = false

            [[providers]]
            name = "test-provider"
            api_base = "http://127.0.0.1:9/v1"
            api_key_env_var = "TEST_API_KEY"
            backend = "mistral"

            [[models]]
            name = "test-model"
            provider = "test-provider"
            alias = "test"
            """
        ).strip()
        + "\n",
        encoding="utf-8",
    )
    (vibe_home / "agents").mkdir(parents=True, exist_ok=True)
    (vibe_home / "agents" / "sub.toml").write_text(
        'agent_type = "subagent"\n',
        encoding="utf-8",
    )

    micro_home = base / "microvibe" / "home"
    for config_dir in [
        micro_home / "Library" / "Application Support" / "microvibe",
        base / "microvibe" / "xdg-config" / "microvibe",
    ]:
        config_dir.mkdir(parents=True, exist_ok=True)
        (config_dir / "config.toml").write_text(
            textwrap.dedent(
                f"""
                default_agent = "{default_agent}"
                {extra}
                [model]
                provider = "test-provider"
                name = "test-model"
                temperature = 0.1
                max_context_tokens = 200000
                input_price = 0.0
                output_price = 0.0

                [providers.test-provider]
                base_url = "http://127.0.0.1:9/v1"
                api_key_env = "TEST_API_KEY"
                wire_format = "openai_chat"

                [permissions]
                mode = "ask"
                """
            ).strip()
            + "\n",
            encoding="utf-8",
        )
    micro_vibe_home = base / "microvibe" / "home" / ".vibe"
    (micro_vibe_home / "agents").mkdir(parents=True, exist_ok=True)
    (micro_vibe_home / "agents" / "sub.toml").write_text(
        'agent_type = "subagent"\n',
        encoding="utf-8",
    )


def run_programmatic(
    cmd: list[str],
    env: dict[str, str],
    case: Case,
    cwd: pathlib.Path,
) -> bytes:
    try:
        result = subprocess.run(
            cmd,
            cwd=cwd,
            env=env,
            text=False,
            capture_output=True,
            timeout=case.timeout,
        )
    except subprocess.TimeoutExpired as exc:
        stdout = exc.stdout or b""
        stderr = exc.stderr or b""
        return stdout + stderr + f"\n<timeout after {case.timeout}s>\n".encode()
    return result.stdout + result.stderr


def continue_command(
    binary: list[str],
    prompt: str,
    *,
    continue_session: bool = False,
    resume_id: str | None = None,
) -> list[str]:
    cmd = [*binary, "--trust", "--auto-approve"]
    if continue_session:
        cmd.append("--continue")
    if resume_id is not None:
        cmd.extend(["--resume", resume_id])
    cmd.extend(["-p", prompt, "--output", "json"])
    return cmd


def run_continue_programmatic(
    binary: list[str],
    env: dict[str, str],
    case: Case,
    cwd: pathlib.Path,
) -> bytes:
    first = subprocess.run(
        continue_command(binary, "first", continue_session=False),
        cwd=cwd,
        env=env,
        text=False,
        capture_output=True,
        timeout=case.timeout,
    )
    if first.returncode != 0:
        return first.stdout + first.stderr
    second = subprocess.run(
        continue_command(binary, "second", continue_session=True),
        cwd=cwd,
        env=env,
        text=False,
        capture_output=True,
        timeout=case.timeout,
    )
    return second.stdout + second.stderr


def run_seed_programmatic(
    binary: list[str],
    env: dict[str, str],
    case: Case,
    cwd: pathlib.Path,
) -> bytes:
    first = run_seed_subprocess(continue_command(binary, "first"), env, case, cwd)
    return first.stdout + first.stderr


def run_seed_two_programmatic(
    binary: list[str],
    env: dict[str, str],
    case: Case,
    cwd: pathlib.Path,
) -> bytes:
    first = run_seed_subprocess(continue_command(binary, "first"), env, case, cwd)
    if first.returncode != 0:
        return first.stdout + first.stderr
    second = run_seed_subprocess(continue_command(binary, "second", continue_session=True), env, case, cwd)
    return first.stdout + first.stderr + second.stdout + second.stderr


def run_seed_subprocess(
    command: list[str],
    env: dict[str, str],
    case: Case,
    cwd: pathlib.Path,
) -> subprocess.CompletedProcess[bytes]:
    timeout = max(case.timeout, 120.0)
    try:
        return subprocess.run(
            command,
            cwd=cwd,
            env=env,
            text=False,
            capture_output=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired as exc:
        stdout = exc.stdout or b""
        stderr = exc.stderr or b""
        stderr += f"\nseed command timed out after {timeout:.1f}s: {command!r}\n".encode()
        return subprocess.CompletedProcess(command, 124, stdout, stderr)


def latest_session_id(env: dict[str, str]) -> str:
    root = pathlib.Path(env["VIBE_HOME"]) / "logs" / "session"
    sessions = sorted(root.glob("session_*"))
    if not sessions:
        return ""
    metadata = json.loads((sessions[-1] / "meta.json").read_text(encoding="utf-8"))
    return str(metadata.get("session_id") or "")


def run_resume_id_programmatic(
    binary: list[str],
    env: dict[str, str],
    case: Case,
    cwd: pathlib.Path,
) -> bytes:
    first = subprocess.run(
        continue_command(binary, "first"),
        cwd=cwd,
        env=env,
        text=False,
        capture_output=True,
        timeout=case.timeout,
    )
    if first.returncode != 0:
        return first.stdout + first.stderr
    session_id = latest_session_id(env)
    second = subprocess.run(
        continue_command(binary, "second", resume_id=session_id),
        cwd=cwd,
        env=env,
        text=False,
        capture_output=True,
        timeout=case.timeout,
    )
    return second.stdout + second.stderr


def normalize_programmatic(raw: bytes, output: str) -> str:
    if output == "text":
        return normalize(raw)
    text = raw.decode("utf-8", "replace").strip()
    if text.startswith("<vibe_stop_event>"):
        return normalize(raw)
    if output == "json":
        data = json.loads(text)
        return json.dumps(scrub_programmatic_messages(data), indent=2, ensure_ascii=False) + "\n"
    if output == "streaming":
        lines = []
        for line in text.splitlines():
            if not line.strip():
                continue
            if line.startswith("<vibe_stop_event>"):
                lines.append(line)
            else:
                lines.append(json.dumps(scrub_programmatic_message(json.loads(line)), ensure_ascii=False))
        return "\n".join(lines) + "\n"
    raise ValueError(output)


def scrub_programmatic_messages(messages: list[dict[str, object]]) -> list[dict[str, object]]:
    return [scrub_programmatic_message(message) for message in messages]


def scrub_programmatic_message(message: dict[str, object]) -> dict[str, object]:
    scrubbed = dict(message)
    if scrubbed.get("role") == "system":
        scrubbed["content"] = "<system>"
    for key in ["message_id", "reasoning_message_id"]:
        value = scrubbed.get(key)
        if isinstance(value, str) and re.fullmatch(r"[0-9a-f]{8}-[0-9a-f-]{27,}", value, flags=re.I):
            scrubbed[key] = "<uuid>"
    return scrubbed


def read_available(fd: int, deadline: float, quiet_gap: float) -> bytes:
    chunks: list[bytes] = []
    last = time.monotonic()
    while time.monotonic() < deadline:
        timeout = min(0.05, max(0.0, deadline - time.monotonic()))
        ready, _, _ = select.select([fd], [], [], timeout)
        if ready:
            try:
                data = os.read(fd, 65536)
            except OSError:
                break
            if not data:
                break
            chunks.append(data)
            last = time.monotonic()
        elif time.monotonic() - last >= quiet_gap:
            break
    return b"".join(chunks)


def read_for(fd: int, duration: float) -> bytes:
    return read_available(fd, time.monotonic() + duration, duration + 1.0)


def read_until(fd: int, needles: list[bytes], timeout: float) -> bytes:
    chunks: list[bytes] = []
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        chunk = read_available(fd, time.monotonic() + 0.1, 0.2)
        if chunk:
            chunks.append(chunk)
            haystack = b"".join(chunks)
            if all(needle in haystack for needle in needles):
                break
    return b"".join(chunks)


def read_until_plain(fd: int, needles: list[bytes], timeout: float) -> bytes:
    chunks: list[bytes] = []
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        chunk = read_available(fd, time.monotonic() + 0.1, 0.2)
        if chunk:
            chunks.append(chunk)
            plain = ANSI_RE.sub(b"", b"".join(chunks))
            if all(needle in plain for needle in needles):
                break
    return b"".join(chunks)


def setup_staged_needles(case_name: str) -> list[tuple[bytes, list[bytes]]]:
    if case_name == "cli_setup_theme":
        return [(b"\r", [b"Select your preferred theme"])]
    if case_name == "cli_setup_auth_method":
        return [
            (b"\r", [b"Select your preferred theme"]),
            (b"\r", [b"Choose your sign in method", b"Use an API key"]),
        ]
    if case_name == "cli_setup_api_key":
        return [
            (b"\r", [b"Select your preferred theme"]),
            (b"\r", [b"Choose your sign in method", b"Use an API key", b"Use arrows"]),
            (b"\x1b[B", []),
            (b"\r", [b"Paste API key"]),
        ]
    if case_name == "cli_setup_save_api_key":
        return [
            (b"\r", [b"Select your preferred theme"]),
            (b"\r", [b"Choose your sign in method", b"Use an API key", b"Use arrows"]),
            (b"\x1b[B", []),
            (b"\r", [b"Paste API key"]),
            (b"sk-parity-key\r", [b"Setup complete"]),
        ]
    return []


def parity_timeout_scale() -> float:
    return float(os.environ.get("MICROVIBE_PARITY_TIMEOUT_SCALE", "1"))


def run_pty(cmd: list[str], env: dict[str, str], case: Case, cwd: pathlib.Path) -> bytes:
    pid, fd = pty.fork()
    if pid == 0:
        try:
            os.chdir(cwd)
            tty.setraw(0)
            os.execvpe(cmd[0], cmd, env)
        except Exception as exc:
            print(f"exec failed: {exc}", file=sys.stderr)
            os._exit(127)

    winsize = struct.pack("HHHH", 36, 120, 0, 0)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, winsize)
    transcript = bytearray()
    try:
        if is_tui_mode(case.mode):
            transcript.extend(read_until(fd, [b"default", b"tokens"], case.timeout))
            transcript.extend(read_for(fd, min(2.0, 0.2 * parity_timeout_scale())))
        elif case.mode == "cli_setup":
            needles = [b"Mistral Vibe"]
            if case.name not in {"cli_setup_cancel"}:
                needles.append(b"Press Enter")
            transcript.extend(read_until_plain(fd, needles, case.timeout))
            transcript.extend(read_for(fd, min(case.settle, case.timeout)))
        else:
            transcript.extend(read_for(fd, min(case.settle, case.timeout)))
        staged_inputs = {
            "tui_model_select_next": (b"/model\x1b\r", b"Select Model", b"\x1b[B\r"),
            "tui_thinking_select_next": (b"/thinking\x1b\r", b"Select Thinking Level", b"\x1b[B\r"),
            "tui_theme_select_next": (b"/theme\x1b\r", b"Select Theme", b"\x1b[B\r"),
            "tui_voice_toggle": (b"/voice\x1b\r", b"Voice Settings", b" "),
            "tui_config_toggle_autocopy": (b"/config\x1b\r", b"Settings", b"\x1b[B\x1b[B\r"),
            "tui_voice_toggle_exit": (b"/voice\x1b\r", b"Voice Settings", b" \x1b"),
            "tui_config_toggle_autocopy_exit": (b"/config\x1b\r", b"Settings", b"\x1b[B\x1b[B\r\x1b"),
            "tui_resume_select_one": (b"/resume\x1b\r", b"Enter Select", b"\r"),
            "tui_resume_delete_confirm": (b"/resume\x1b\r", b"Enter Select", b"d"),
            "tui_resume_delete_one": (b"/resume\x1b\r", b"Enter Select", b"dd"),
            "tui_resume_rename_one": (b"/resume\x1b\r", b"Enter Select", b"\r/rename Renamed parity\x1b\r"),
            "tui_mcp_disable_server": (b"/mcp\x1b\r", b"2 tools", b"d"),
            "tui_mcp_enable_server": (b"/mcp\x1b\r", b"local-demo", b"e"),
            "tui_mcp_disable_tool": (b"/mcp local-demo\x1b\r", b"Disabled parity tool", b"d"),
            "tui_mcp_enable_tool": (b"/mcp local-demo\x1b\r", b"Disabled parity tool", b"e"),
            "tui_loop_create_list": (b"/loop 30s check status\x1b\r", b"Scheduled loop", b"/loop list\x1b\r"),
            "tui_loop_create_cancel_all": (b"/loop 30s check status\x1b\r", b"Scheduled loop", b"/loop cancel all\x1b\r"),
        }
        if case.mode == "cli_setup" and case.name == "cli_setup_cancel":
            try:
                os.write(fd, b"\x03")
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.mode == "cli_setup" and setup_staged_needles(case.name):
            try:
                for keys, needles in setup_staged_needles(case.name):
                    if case.name == "cli_setup_save_api_key" and keys == b"sk-parity-key\r":
                        os.write(fd, b"sk-parity-key")
                        transcript.extend(read_until_plain(fd, [b"Press Enter to submit"], min(8.0, case.timeout)))
                        os.write(fd, b"\r")
                        final_chunk = read_until_plain(fd, needles, min(4.0, case.timeout))
                        transcript.extend(final_chunk)
                        if needles and not all(needle in ANSI_RE.sub(b"", final_chunk) for needle in needles):
                            os.write(fd, b"\r")
                            transcript.extend(read_until_plain(fd, needles, case.timeout))
                    else:
                        os.write(fd, keys)
                        if needles:
                            transcript.extend(read_until_plain(fd, needles, case.timeout))
                        else:
                            transcript.extend(read_for(fd, 0.8))
                    time.sleep(0.8 if case.name.startswith("cli_setup_") else 0.3)
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_resume_rename_one":
            try:
                os.write(fd, b"/resume\x1b\r")
                transcript.extend(read_until(fd, [b"Enter Select"], case.timeout))
                os.write(fd, b"\r")
                transcript.extend(read_until(fd, [b"Resumed session"], case.timeout))
                os.write(fd, b"/rename Renamed parity\x1b\r")
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in {"tui_proxy_setup_save_http", "tui_proxy_setup_preserve_env", "tui_proxy_setup_unset_http"}:
            try:
                os.write(fd, b"/proxy-setup\x1b\r")
                transcript.extend(read_until_plain(fd, [b"Proxy Configuration", b"Proxy URL for HTTP requests"], case.timeout))
                if case.name == "tui_proxy_setup_unset_http":
                    os.write(fd, b"\x7f" * len("http://old.proxy:8080"))
                    transcript.extend(read_until_plain(fd, [b"Proxy URL for HTTP requests"], case.timeout))
                else:
                    os.write(fd, b"http://proxy.local:8080")
                    transcript.extend(read_until_plain(fd, [b"http://proxy.local:8080"], case.timeout))
                os.write(fd, b"\r")
                transcript.extend(read_until_plain(fd, [b"Proxy settings saved"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_initial_prompt":
            transcript.extend(read_until(fd, [b"hello from tui"], case.timeout))
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_resume_delete_one":
            try:
                os.write(fd, b"/resume\x1b\r")
                transcript.extend(read_until(fd, [b"Enter Select"], case.timeout))
                os.write(fd, b"d")
                transcript.extend(read_until(fd, [b"Press D again"], case.timeout))
                os.write(fd, b"d")
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_help":
            try:
                os.write(fd, b"/help\r")
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in {"tui_prompt_read_expand_tool", "tui_prompt_read_expand_collapse_tool"}:
            try:
                os.write(fd, b"read sample\x1b\r")
                transcript.extend(read_until(fd, [b"read done"], case.timeout))
                time.sleep(0.3)
                os.write(fd, b"\x0f")
                if case.name == "tui_prompt_read_expand_collapse_tool":
                    transcript.extend(read_until(fd, [b"show less"], case.timeout))
                    time.sleep(0.3)
                    os.write(fd, b"\x0f")
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_prompt_skill_expand_tool":
            try:
                os.write(fd, b"load skill\x1b\r")
                transcript.extend(read_until(fd, [b"skill done"], case.timeout))
                time.sleep(0.3)
                os.write(fd, b"\x0f")
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in {"tui_prompt_history_up", "tui_prompt_history_up_down"}:
            try:
                os.write(fd, b"hello tui\x1b\r")
                transcript.extend(read_until(fd, [b"hello from tui"], case.timeout))
                time.sleep(0.3)
                os.write(fd, b"\x1b[A")
                if case.name == "tui_prompt_history_up_down":
                    transcript.extend(read_until(fd, [b"> hello tui"], case.timeout))
                    time.sleep(0.3)
                    os.write(fd, b"\x1b[B")
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_prompt_multiline_ctrl_j":
            try:
                os.write(fd, b"hello")
                transcript.extend(read_until(fd, [b"> hello"], case.timeout))
                time.sleep(0.2)
                os.write(fd, b"\x0a")
                time.sleep(0.2)
                os.write(fd, b"world\x1b\r")
                transcript.extend(read_until(fd, [b"hello from tui"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in {"tui_external_editor_input", "tui_external_editor_empty"}:
            try:
                if case.name == "tui_external_editor_input":
                    os.write(fd, b"original")
                    transcript.extend(read_until(fd, [b"> original"], case.timeout))
                    time.sleep(0.2)
                os.write(fd, b"\x07")
                transcript.extend(read_until(fd, [b"edited from editor"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in {"tui_copy_last_agent", "tui_copy_last_agent_xclip"}:
            try:
                os.write(fd, b"hello tui\x1b\r")
                transcript.extend(read_until(fd, [b"hello from tui"], case.timeout))
                time.sleep(0.3)
                os.write(fd, b"/copy\x1b\r")
                clipboard_path = env.get("MICROVIBE_FAKE_CLIPBOARD")
                if clipboard_path:
                    deadline = time.monotonic() + 1.0
                    while time.monotonic() < deadline:
                        if pathlib.Path(clipboard_path).exists():
                            break
                        time.sleep(0.05)
                    if not pathlib.Path(clipboard_path).exists():
                        os.write(fd, b"\n")
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in {"tui_scroll_shift_up", "tui_scroll_shift_up_down"}:
            try:
                step_timeout = min(case.timeout, 20.0)
                for idx in range(1, 7):
                    prompt = f"scroll prompt {idx:02d}".encode()
                    reply = f"scroll reply {idx:02d}".encode()
                    os.write(fd, prompt + b"\x1b\r")
                    transcript.extend(read_until(fd, [reply], step_timeout))
                    time.sleep(0.1)
                time.sleep(0.3)
                os.write(fd, b"\x1b[1;2A")
                if case.name == "tui_scroll_shift_up_down":
                    time.sleep(0.3)
                    os.write(fd, b"\x1b[1;2B")
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_completion_slash_nav_enter":
            try:
                os.write(fd, b"/co")
                transcript.extend(read_until(fd, [b"/compact"], case.timeout))
                os.write(fd, b"\x1b[B\r")
                transcript.extend(read_until(fd, [b"No conversation history to compact yet"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in {"tui_completion_path_popup_list", "tui_completion_path_popup_ten", "tui_completion_path_dir_tab"}:
            try:
                if case.name == "tui_completion_path_dir_tab":
                    os.write(fd, b"@sr")
                    transcript.extend(read_until(fd, [b"src/"], case.timeout))
                    os.write(fd, b"\t")
                    transcript.extend(read_until(fd, [b"src/main.py"], case.timeout))
                elif case.name == "tui_completion_path_popup_ten":
                    os.write(fd, case.input_text)
                    transcript.extend(read_until(fd, [b"extra_file_10.py"], case.timeout))
                else:
                    os.write(fd, case.input_text)
                    transcript.extend(read_until(fd, [b"src/"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in {"tui_prompt_at_file", "tui_completion_path_file", "tui_prompt_at_folder", "tui_prompt_at_image", "tui_prompt_at_image_no_vision"}:
            try:
                typed = {
                    "tui_prompt_at_file": b"use @sample.txt",
                    "tui_completion_path_file": b"use @samp\t",
                    "tui_prompt_at_folder": b"use @notes",
                    "tui_prompt_at_image": b"use @image.png",
                    "tui_prompt_at_image_no_vision": b"use @image.png",
                }[case.name]
                done = {
                    "tui_prompt_at_file": b"at file done",
                    "tui_completion_path_file": b"completion file done",
                    "tui_prompt_at_folder": b"at folder done",
                    "tui_prompt_at_image": b"at image done",
                    "tui_prompt_at_image_no_vision": b"does not support images",
                }[case.name]
                os.write(fd, typed)
                wait_for = b"sample.txt" if case.name == "tui_completion_path_file" else typed.split()[-1]
                transcript.extend(read_until(fd, [wait_for], case.timeout))
                time.sleep(0.2)
                if case.name != "tui_completion_path_file":
                    os.write(fd, b"\x1b")
                    transcript.extend(read_for(fd, 0.3))
                os.write(fd, b"\r")
                if case.name == "tui_completion_path_file":
                    time.sleep(0.2)
                    os.write(fd, b"\r")
                transcript.extend(read_until(fd, [done], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in {"tui_rewind_one", "tui_rewind_select_one", "tui_compact_one"}:
            try:
                os.write(fd, b"/resume\x1b\r")
                transcript.extend(read_until(fd, [b"Enter Select"], case.timeout))
                os.write(fd, b"\r")
                transcript.extend(read_until(fd, [b"Resumed session"], case.timeout))
                if case.name == "tui_compact_one":
                    os.write(fd, b"/compact\x1b\r")
                    transcript.extend(read_until(fd, [b"Compaction completed"], case.timeout))
                else:
                    os.write(fd, b"/rewind\x1b\r")
                if case.name == "tui_rewind_select_one":
                    transcript.extend(read_until(fd, [b"Enter confirm"], case.timeout))
                    os.write(fd, b"\r")
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in {
            "tui_rewind_global_ctrl_p",
            "tui_rewind_global_ctrl_p_prev",
            "tui_rewind_global_ctrl_n",
            "tui_rewind_global_alt_up",
            "tui_rewind_global_alt_down",
        }:
            try:
                os.write(fd, b"/resume\x1b\r")
                transcript.extend(read_until(fd, [b"Enter Select"], case.timeout))
                os.write(fd, b"\r")
                transcript.extend(read_until(fd, [b"Resumed session"], case.timeout))
                up_key = b"\x1b[1;3A" if case.name.startswith("tui_rewind_global_alt") else b"\x10"
                down_key = b"\x1b[1;3B" if case.name.startswith("tui_rewind_global_alt") else b"\x0e"
                os.write(fd, up_key)
                transcript.extend(read_until(fd, [b"Rewind to: second"], case.timeout))
                if case.name in {"tui_rewind_global_ctrl_p_prev", "tui_rewind_global_ctrl_n", "tui_rewind_global_alt_down"}:
                    os.write(fd, up_key)
                    transcript.extend(read_until(fd, [b"Rewind to: first"], case.timeout))
                if case.name in {"tui_rewind_global_ctrl_n", "tui_rewind_global_alt_down"}:
                    os.write(fd, down_key)
                    transcript.extend(read_until(fd, [b"Rewind to: second"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_prompt_bash":
            try:
                os.write(fd, b"run bash\x1b\r")
                transcript.extend(read_until(fd, [b"Permission for the bash tool"], case.timeout))
                time.sleep(0.6)
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_prompt_bash_allow":
            try:
                os.write(fd, b"run bash\x1b\r")
                transcript.extend(read_until(fd, [b"Permission for the bash tool"], case.timeout))
                time.sleep(0.6)
                os.write(fd, b"\r")
                transcript.extend(read_until(fd, [b"bash done"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_prompt_bash_allow_y":
            try:
                os.write(fd, b"run bash\x1b\r")
                transcript.extend(read_until(fd, [b"Permission for the bash tool"], case.timeout))
                time.sleep(0.6)
                os.write(fd, b"y")
                transcript.extend(read_until(fd, [b"bash done"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in {"tui_prompt_bash_allow_expand_tool", "tui_prompt_bash_allow_expand_collapse_tool"}:
            try:
                os.write(fd, b"run bash\x1b\r")
                transcript.extend(read_until(fd, [b"Permission for the bash tool"], case.timeout))
                time.sleep(0.6)
                os.write(fd, b"\r")
                transcript.extend(read_until(fd, [b"bash done"], case.timeout))
                time.sleep(0.3)
                os.write(fd, b"\x0f")
                if case.name == "tui_prompt_bash_allow_expand_collapse_tool":
                    transcript.extend(read_until(fd, [b"show less"], case.timeout))
                    time.sleep(0.3)
                    os.write(fd, b"\x0f")
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_prompt_bash_allow_session":
            try:
                os.write(fd, b"run bash twice\x1b\r")
                transcript.extend(read_until(fd, [b"Permission for the bash tool"], case.timeout))
                time.sleep(0.6)
                os.write(fd, b"2\r")
                transcript.extend(read_until(fd, [b"bash session done"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_prompt_bash_always":
            try:
                os.write(fd, b"run bash always\x1b\r")
                transcript.extend(read_until(fd, [b"Permission for the bash tool"], case.timeout))
                time.sleep(0.6)
                os.write(fd, b"3\r")
                transcript.extend(read_until(fd, [b"bash always done"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_prompt_bash_persisted_allow":
            try:
                os.write(fd, b"run persisted bash\x1b\r")
                transcript.extend(read_until(fd, [b"bash persisted done"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_prompt_bash_deny":
            try:
                os.write(fd, b"deny bash\x1b\r")
                transcript.extend(read_until(fd, [b"Permission for the bash tool"], case.timeout))
                time.sleep(0.6)
                os.write(fd, b"4")
                transcript.extend(read_until(fd, [b"bash denied done"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_prompt_bash_deny_n":
            try:
                os.write(fd, b"deny bash\x1b\r")
                transcript.extend(read_until(fd, [b"Permission for the bash tool"], case.timeout))
                time.sleep(0.6)
                os.write(fd, b"n")
                transcript.extend(read_until(fd, [b"bash denied done"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in {"tui_prompt_todo", "tui_prompt_todo_empty"}:
            try:
                prompt = b"todo update\x1b\r" if case.name == "tui_prompt_todo" else b"todo read empty\x1b\r"
                done = b"todo done" if case.name == "tui_prompt_todo" else b"todo empty done"
                os.write(fd, prompt)
                transcript.extend(read_until(fd, [done], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_prompt_file_tools_allow_write":
            try:
                os.write(fd, b"file tools\x1b\r")
                transcript.extend(read_until(fd, [b"Permission for the write_file tool"], case.timeout))
                time.sleep(0.6)
                os.write(fd, b"\r")
                transcript.extend(read_until(fd, [b"Permission for the edit tool"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_animation_edit_spinner":
            try:
                os.write(fd, b"file tools\x1b\r")
                transcript.extend(read_until(fd, [b"Permission for the write_file tool"], case.timeout))
                time.sleep(0.6)
                os.write(fd, b"\r")
                transcript.extend(read_until(fd, [b"Permission for the edit tool"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in {"tui_prompt_file_tools_allow_edit", "tui_prompt_file_tools_expand_tool"}:
            try:
                os.write(fd, b"file tools\x1b\r")
                transcript.extend(read_until(fd, [b"Permission for the write_file tool"], case.timeout))
                time.sleep(0.6)
                os.write(fd, b"\r")
                transcript.extend(read_until(fd, [b"Permission for the edit tool"], case.timeout))
                time.sleep(0.6)
                os.write(fd, b"\r")
                transcript.extend(read_until(fd, [b"file tools done"], case.timeout))
                if case.name == "tui_prompt_file_tools_expand_tool":
                    time.sleep(0.3)
                    os.write(fd, b"\x0f")
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in {"tui_prompt_question", "tui_prompt_question_expand_tool"}:
            try:
                os.write(fd, b"ask question\x1b\r")
                transcript.extend(read_until(fd, [b"Choose parity mode?"], case.timeout))
                time.sleep(1.5)
                os.write(fd, b"\r")
                transcript.extend(read_until(fd, [b"question answer done"], case.timeout))
                if case.name == "tui_prompt_question_expand_tool":
                    time.sleep(0.3)
                    os.write(fd, b"\x0f")
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in {"tui_prompt_web_fetch", "tui_prompt_web_fetch_expand_tool"}:
            try:
                os.write(fd, b"fetch web\x1b\r")
                transcript.extend(read_until(fd, [b"Permission for the web_fetch tool"], case.timeout))
                time.sleep(0.6)
                os.write(fd, b"\r")
                transcript.extend(read_until(fd, [b"web fetch done"], case.timeout))
                if case.name == "tui_prompt_web_fetch_expand_tool":
                    time.sleep(0.3)
                    os.write(fd, b"\x0f")
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_prompt_task_allow_explore":
            try:
                os.write(fd, b"delegate explore task\x1b\r")
                transcript.extend(read_until(fd, [b"task explore done"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in {"tui_prompt_task", "tui_animation_task_spinner", "tui_prompt_task_allow_unknown", "tui_prompt_task_deny"}:
            try:
                os.write(fd, b"delegate task\x1b\r")
                transcript.extend(read_until(fd, [b"Permission for the task tool"], case.timeout))
                time.sleep(0.6)
                if case.name == "tui_prompt_task_allow_unknown":
                    os.write(fd, b"\r")
                    transcript.extend(read_until(fd, [b"task unknown done"], case.timeout))
                elif case.name == "tui_prompt_task_deny":
                    os.write(fd, b"4")
                    transcript.extend(read_until(fd, [b"task denied done"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in {"tui_prompt_web_search", "tui_prompt_web_search_expand_tool"}:
            try:
                os.write(fd, b"search web\x1b\r")
                transcript.extend(read_until(fd, [b"Permission for the web_search tool"], case.timeout))
                time.sleep(0.6)
                os.write(fd, b"\r")
                transcript.extend(read_until(fd, [b"web search done"], case.timeout))
                if case.name == "tui_prompt_web_search_expand_tool":
                    time.sleep(0.3)
                    os.write(fd, b"\x0f")
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_prompt_question_other":
            try:
                os.write(fd, b"ask custom question\x1b\r")
                transcript.extend(read_until(fd, [b"Strict"], case.timeout))
                time.sleep(1.5)
                os.write(fd, b"\x1b[B\x1b[B")
                transcript.extend(read_until(fd, [b"Type your answer"], case.timeout))
                os.write(fd, b"Custom parity\r")
                transcript.extend(read_until(fd, [b"question other done"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_prompt_question_multi":
            try:
                os.write(fd, b"ask multi question\x1b\r")
                transcript.extend(read_until(fd, [b"Alpha"], case.timeout))
                time.sleep(1.5)
                os.write(fd, b"\r")
                transcript.extend(read_until(fd, [b"Gamma"], case.timeout))
                time.sleep(1.5)
                os.write(fd, b"\r")
                transcript.extend(read_until(fd, [b"multi question done"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_prompt_question_multiselect":
            try:
                os.write(fd, b"ask multi select\x1b\r")
                transcript.extend(read_until(fd, [b"Red"], case.timeout))
                time.sleep(1.5)
                os.write(fd, b"\r")
                time.sleep(0.2)
                os.write(fd, b"\x1b[B\r")
                time.sleep(0.2)
                os.write(fd, b"\x1b[B\r")
                transcript.extend(read_until(fd, [b"multi select done"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name == "tui_prompt_question_multiselect_other":
            try:
                os.write(fd, b"ask multi select custom\x1b\r")
                transcript.extend(read_until(fd, [b"Red"], case.timeout))
                time.sleep(1.5)
                os.write(fd, b"\r")
                time.sleep(0.2)
                os.write(fd, b"\x1b[B\x1b[B")
                transcript.extend(read_until(fd, [b"Type your answer"], case.timeout))
                os.write(fd, b"Green\r")
                time.sleep(0.3)
                os.write(fd, b"\r")
                transcript.extend(read_until(fd, [b"multi select other done"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in {"tui_prompt_exit_plan_no", "tui_prompt_exit_plan_editor"}:
            try:
                os.write(fd, b"finish plan\x1b\r")
                transcript.extend(read_until(fd, [b"Plan is complete."], case.timeout))
                time.sleep(1.5)
                if case.name == "tui_prompt_exit_plan_editor":
                    os.write(fd, b"\x07")
                    time.sleep(0.5)
                os.write(fd, b"\x1b[B\x1b[B\r")
                transcript.extend(read_until(fd, [b"exit plan done"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in {"tui_prompt_exit_plan_auto", "tui_prompt_exit_plan_default"}:
            try:
                os.write(fd, b"finish plan\x1b\r")
                transcript.extend(read_until(fd, [b"Plan is complete."], case.timeout))
                time.sleep(1.5)
                if case.name == "tui_prompt_exit_plan_default":
                    os.write(fd, b"\x1b[B")
                os.write(fd, b"\r")
                transcript.extend(read_until(fd, [b"exit plan done"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in {"tui_cycle_mode_shift_tab_twice", "tui_cycle_mode_shift_tab_thrice", "tui_cycle_mode_shift_tab_custom"}:
            try:
                os.write(fd, b"\x1b[Z")
                transcript.extend(read_until(fd, [b"plan"], case.timeout))
                time.sleep(1.0)
                os.write(fd, b"\x1b[Z")
                transcript.extend(read_until(fd, [b"accept edits"], case.timeout))
                if case.name in {"tui_cycle_mode_shift_tab_thrice", "tui_cycle_mode_shift_tab_custom"}:
                    time.sleep(1.0)
                    os.write(fd, b"\x1b[Z")
                    transcript.extend(read_until(fd, [b"auto approve"], case.timeout))
                if case.name == "tui_cycle_mode_shift_tab_custom":
                    time.sleep(1.0)
                    os.write(fd, b"\x1b[Z")
                    transcript.extend(read_until(fd, [b"review bot"], case.timeout))
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.name in staged_inputs:
            first, needle, second = staged_inputs[case.name]
            try:
                os.write(fd, first)
                transcript.extend(read_until(fd, [needle], case.timeout))
                os.write(fd, second)
            except OSError:
                pass
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
        elif case.input_text:
            input_text = case.input_text
            interrupt = b""
            if is_tui_mode(case.mode) and input_text.endswith(b"\x03"):
                input_text = input_text[:-1]
                interrupt = b"\x03"
            if (
                is_tui_mode(case.mode)
                and input_text.startswith(b"/")
                and input_text.endswith(b"\x1b\r")
                and b"\x1b" not in input_text[:-2]
                and b"\r" not in input_text[:-2]
            ):
                command = input_text[:-2]
                try:
                    os.write(fd, command)
                    transcript.extend(read_for(fd, 0.1))
                    os.write(fd, b"\x1b")
                    transcript.extend(read_for(fd, 0.05))
                    os.write(fd, b"\r")
                    slash_needles = {
                        "tui_mcp": [b"No MCP servers or connectors configured."],
                        "tui_mcp_configured": [b"local-demo"],
                        "tui_mcp_stdio_tools": [b"1/2 tools"],
                        "tui_mcp_stdio_tools_detail": [b"Disabled parity tool"],
                        "tui_connectors_configured": [b"local-demo"],
                    }.get(case.name)
                    if slash_needles:
                        transcript.extend(read_until(fd, slash_needles, case.timeout))
                except OSError:
                    pass
            else:
                try:
                    if case.name == "tui_ctrl_d_confirm":
                        transcript.extend(read_for(fd, 1.0))
                    os.write(fd, input_text)
                except OSError:
                    input_text = b""
            transcript.extend(
                read_available(
                    fd,
                    time.monotonic() + case.timeout,
                    case.settle,
                )
            )
            if interrupt:
                transcript.extend(read_for(fd, 1.0))
                try:
                    os.write(fd, interrupt)
                except OSError:
                    interrupt = b""
                transcript.extend(read_for(fd, 0.2))
    finally:
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            os.close(fd)
        except OSError:
            pass
        deadline = time.monotonic() + 1.0
        while True:
            try:
                waited, _ = os.waitpid(pid, os.WNOHANG)
            except ChildProcessError:
                break
            if waited == pid:
                break
            if time.monotonic() >= deadline:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                try:
                    os.waitpid(pid, 0)
                except ChildProcessError:
                    pass
                break
            time.sleep(0.05)
    return bytes(transcript)


def normalize(raw: bytes) -> str:
    text = ANSI_RE.sub(b"", raw).decode("utf-8", "replace")
    text = text.replace("\r\n", "\n").replace("\r", "\n")
    text = text.replace("/microvibe/home/", "/vibe/home/")
    text = re.sub(r"\d+(?:\.\d+)?s\b", "<duration>", text)
    text = re.sub(r"`[0-9a-f]{8}`", "`<loop_id>`", text, flags=re.I)
    text = re.sub(r"(?<=Scheduled loop )[0-9a-f]{8}", "<loop_id>", text, flags=re.I)
    text = re.sub(r"(?<=Cancelled loop )[0-9a-f]{8}", "<loop_id>", text, flags=re.I)
    text = re.sub(r"(?<=│ )[0-9a-f]{8}(?= +│)", "<loop_id>", text, flags=re.I)
    text = re.sub(r"[0-9a-f]{8}-[0-9a-f-]{27,}", "<uuid>", text, flags=re.I)
    text = re.sub(r"\n{3,}", "\n\n", text)
    return text.strip() + "\n"


def normalize_json_line(raw: bytes) -> str:
    text = raw.decode("utf-8", "replace").strip()
    data = json.loads(text)
    return json.dumps(data, indent=2, sort_keys=True, ensure_ascii=False) + "\n"


def normalize_acp_json_line(raw: bytes) -> str:
    text = raw.decode("utf-8", "replace").strip()
    data = json.loads(text)
    preserved_message_ids = {
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        "11111111-2222-3333-4444-555555555555",
        "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
        "client-message-id-parity",
    }
    if data.get("method") in {
        "session/request_permission",
        "fs/read_text_file",
        "fs/write_text_file",
        "terminal/create",
        "terminal/wait_for_exit",
        "terminal/output",
        "terminal/release",
        "terminal/kill",
    } and "id" in data:
        data["id"] = f"<{data.get('method').replace('/', '_')}_id>"

    def scrub(value):
        if isinstance(value, dict):
            out = {}
            for key, child in value.items():
                if key in {"sessionId", "session_id"} and isinstance(child, str):
                    out[key] = "<session_id>"
                elif key in {"terminalId", "terminal_id"} and isinstance(child, str):
                    out[key] = "<terminal_id>"
                elif key in {"toolCallId", "tool_call_id"} and isinstance(child, str):
                    out[key] = "<tool_call_id>"
                elif key in {"messageId", "message_id", "userMessageId"} and child in preserved_message_ids:
                    out[key] = child
                elif key in {"messageId", "message_id", "userMessageId"} and isinstance(child, str):
                    out[key] = "<message_id>"
                else:
                    out[key] = scrub(child)
            return out
        if isinstance(value, list):
            return [scrub(child) for child in value]
        if isinstance(value, str):
            value = value.replace("/microvibe/home/", "/vibe/home/")
            return re.sub(
                r"session: [0-9a-f]{8} \(before compaction\) → [0-9a-f]{8} \(after compaction\)",
                "session: <session> (before compaction) → <session> (after compaction)",
                value,
                flags=re.I,
            )
        return value

    return json.dumps(scrub(data), indent=2, sort_keys=True, ensure_ascii=False) + "\n"


def normalize_acp_transcript(raw: bytes) -> str:
    return "".join(normalize_acp_json_line(line + b"\n") for line in raw.splitlines() if line.strip())


def run_acp_request(
    binary: list[str],
    env: dict[str, str],
    cwd: pathlib.Path,
    requests: list[dict],
) -> list[bytes]:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None
    lines: list[bytes] = []
    try:
        for request in requests:
            process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
            process.stdin.flush()
            line = process.stdout.readline()
            if not line:
                break
            lines.append(line.encode("utf-8"))
        process.terminate()
        try:
            _, stderr = process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            _, stderr = process.communicate(timeout=2.0)
    except Exception:
        process.kill()
        raise
    if not lines:
        return [stderr.encode("utf-8", "replace")]
    return lines


def run_acp_until_response(
    binary: list[str],
    env: dict[str, str],
    cwd: pathlib.Path,
    request: dict,
    response_id: int | str,
) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None
    lines: list[str] = []
    done = threading.Event()
    errors: queue.Queue[BaseException] = queue.Queue()

    def reader() -> None:
        try:
            for line in process.stdout:
                lines.append(line)
                try:
                    data = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if data.get("id") == response_id:
                    done.set()
                    break
        except BaseException as exc:
            errors.put(exc)
            done.set()

    thread = threading.Thread(target=reader, daemon=True)
    thread.start()
    try:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()
        done.wait(timeout=10.0)
        process.terminate()
        try:
            _, stderr = process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            _, stderr = process.communicate(timeout=2.0)
    except Exception:
        process.kill()
        raise
    thread.join(timeout=1.0)
    if not errors.empty():
        raise errors.get()
    if not lines:
        return stderr.encode("utf-8", "replace")
    return "".join(lines).encode("utf-8")


def acp_initialize_request(
    *,
    fs_read: bool = False,
    fs_write: bool = False,
    terminal: bool = False,
    field_meta: dict[str, object] | None = None,
) -> dict:
    client_capabilities = {}
    if fs_read or fs_write:
        client_capabilities["fs"] = {}
        if fs_read:
            client_capabilities["fs"]["readTextFile"] = True
        if fs_write:
            client_capabilities["fs"]["writeTextFile"] = True
    if terminal:
        client_capabilities["terminal"] = True
    if field_meta:
        client_capabilities["_meta"] = field_meta
    return {
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientCapabilities": client_capabilities,
            "clientInfo": {"name": "smoke-test", "title": "Smoke Test", "version": "0.0.0"},
        },
    }


def acp_new_session_request(cwd: pathlib.Path, request_id: int | str = 1) -> dict:
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/new",
        "params": {"cwd": str(cwd), "mcpServers": []},
    }


def acp_list_sessions_request(request_id: int | str = 1, cwd: pathlib.Path | None = None) -> dict:
    params = {}
    if cwd is not None:
        params["cwd"] = str(cwd)
    return {"jsonrpc": "2.0", "id": request_id, "method": "session/list", "params": params}


def acp_close_session_request(session_id: str, request_id: int | str = 2) -> dict:
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/close",
        "params": {"sessionId": session_id},
    }


def acp_set_title_request(session_id: str, title: str, request_id: int | str = 1) -> dict:
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "_session/set_title",
        "params": {"sessionId": session_id, "title": title},
    }


def acp_delete_session_request(session_id: str, request_id: int | str = 1) -> dict:
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "_session/delete",
        "params": {"sessionId": session_id},
    }


def acp_auth_status_request(request_id: int | str = 1) -> dict:
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "_auth/status",
        "params": {},
    }


def acp_auth_signout_request(request_id: int | str = 1) -> dict:
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "_auth/signOut",
        "params": {},
    }


def acp_authenticate_request(method_id: str, request_id: int | str = 2, **params: object) -> dict:
    request_params = {"methodId": method_id}
    if params:
        request_params["_meta"] = params
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "authenticate",
        "params": request_params,
    }


def acp_telemetry_notification(session_id: str) -> dict:
    return {
        "jsonrpc": "2.0",
        "method": "_telemetry/send",
        "params": {
            "event": "vibe.unsupported_event",
            "session_id": session_id,
            "properties": {"context_type": "file"},
        },
    }


def acp_unknown_notification() -> dict:
    return {
        "jsonrpc": "2.0",
        "method": "unknown/notification",
        "params": {"ignored": True},
    }


def acp_trust_status_request(cwd: pathlib.Path, request_id: int | str = 1) -> dict:
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "_trust/status",
        "params": {"cwd": str(cwd)},
    }


def acp_trust_decision_request(
    cwd: pathlib.Path,
    decision: str,
    request_id: int | str = 1,
    session_id: str | None = None,
) -> dict:
    params: dict[str, object] = {"cwd": str(cwd), "decision": decision}
    if session_id is not None:
        params["session_id"] = session_id
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "_trust/decision",
        "params": params,
    }


def acp_set_mode_request(session_id: str, mode_id: str, request_id: int | str = 2) -> dict:
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/set_mode",
        "params": {"sessionId": session_id, "modeId": mode_id},
    }


def acp_set_model_request(session_id: str, model_id: str, request_id: int | str = 2) -> dict:
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/set_model",
        "params": {"sessionId": session_id, "modelId": model_id},
    }


def acp_set_config_option_request(
    session_id: str,
    config_id: str,
    value: object,
    request_id: int | str = 2,
) -> dict:
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/set_config_option",
        "params": {"sessionId": session_id, "configId": config_id, "value": value},
    }


def acp_load_session_request(cwd: pathlib.Path, session_id: str, request_id: int | str = 1) -> dict:
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/load",
        "params": {"cwd": str(cwd), "sessionId": session_id, "mcpServers": []},
    }


def acp_fork_session_request(cwd: pathlib.Path, session_id: str, request_id: int | str = 3) -> dict:
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/fork",
        "params": {"cwd": str(cwd), "sessionId": session_id, "mcpServers": []},
    }


def acp_fork_session_from_message_request(
    cwd: pathlib.Path,
    session_id: str,
    message_id: str,
    request_id: int | str = 4,
) -> dict:
    request = acp_fork_session_request(cwd, session_id, request_id)
    request["params"]["messageId"] = message_id
    return request


def acp_prompt_request(session_id: str, prompt: str, request_id: int | str = 3) -> dict:
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": prompt}],
        },
    }


def acp_prompt_client_message_id_request(session_id: str) -> dict:
    request = acp_prompt_request(session_id, "Just say hi")
    request["params"]["messageId"] = "client-message-id-parity"
    return request


def acp_prompt_agent_thought_request(session_id: str) -> dict:
    return acp_prompt_request(session_id, "Think then answer")


def acp_prompt_image_request(
    session_id: str,
    *,
    data: str | None = None,
    mime_type: str = "image/png",
    request_id: int | str = 3,
) -> dict:
    png_bytes = b"\x89PNG\r\n\x1a\n" + b"\x00" * 16
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [
                {"type": "text", "text": "Describe this image"},
                {
                    "type": "image",
                    "data": data if data is not None else base64.b64encode(png_bytes).decode("ascii"),
                    "mime_type": mime_type,
                    "uri": "file:///workspace/cat.png",
                },
            ],
        },
    }


def acp_prompt_user_display_request(session_id: str, request_id: int | str = 3) -> dict:
    request = acp_prompt_request(session_id, "Look at app.ts", request_id=request_id)
    request["params"]["_meta"] = {
        "user_display_content": {
            "version": "1.0.0",
            "host": "mistral-vscode",
            "content": [
                {"type": "text", "text": "Look at "},
                {
                    "type": "workspace_mention",
                    "kind": "file",
                    "uri": "file:///repo/src/app.ts",
                    "name": "app.ts",
                },
            ],
        }
    }
    return request


def run_acp_initialize(binary: list[str], env: dict[str, str], case: Case, cwd: pathlib.Path) -> bytes:
    return run_acp_request(binary, env, cwd, [acp_initialize_request()])[0]


def run_acp_new_session(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_request(binary, env, cwd, [acp_new_session_request(workspace)])[0]


def run_acp_list_sessions(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    return run_acp_request(binary, env, cwd, [acp_list_sessions_request()])[0]


def run_acp_list_sessions_cwd(binary: list[str], env: dict[str, str], cwd: pathlib.Path, filter_cwd: pathlib.Path) -> bytes:
    return run_acp_request(binary, env, cwd, [acp_list_sessions_request(cwd=filter_cwd)])[0]


def run_acp_close_missing(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    return run_acp_request(binary, env, cwd, [acp_close_session_request("missing-session", 1)])[0]


def run_acp_set_title_live_unsaved(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_until_id(expected_id: int, *, keep: bool) -> list[str]:
        lines: list[str] = []
        deadline = time.monotonic() + 10.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            if keep:
                lines.append(line)
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if data.get("id") == expected_id:
                return lines
        raise TimeoutError(f"ACP response id={expected_id} not received")

    try:
        send(acp_new_session_request(workspace, request_id=1))
        new_lines = read_until_id(1, keep=True)
        new_response = next(json.loads(line) for line in new_lines if json.loads(line).get("id") == 1)
        session_id = new_response["result"]["sessionId"]
        send(acp_set_title_request(session_id, "Manual title", request_id=2))
        title_lines = read_until_id(2, keep=True)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return "".join(title_lines).encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_set_title_saved(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    return run_acp_until_response(
        binary,
        env,
        cwd,
        acp_set_title_request("titlesaved-12345678", "Renamed ACP title"),
        1,
    ) + run_acp_request(binary, env, cwd, [acp_list_sessions_request(request_id=2)])[0]


def run_acp_delete_saved(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    return b"".join(
        run_acp_request(
            binary,
            env,
            cwd,
            [
                acp_delete_session_request("deletesaved-12345678", request_id=1),
                acp_list_sessions_request(request_id=2),
            ],
        )
    )


def run_acp_delete_missing(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    return b"".join(
        run_acp_request(
            binary,
            env,
            cwd,
            [
                acp_delete_session_request("missing-session", request_id=1),
                acp_list_sessions_request(request_id=2),
            ],
        )
    )


def run_acp_delete_invalid_missing(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    return run_acp_request(
        binary,
        env,
        cwd,
        [{"jsonrpc": "2.0", "id": 1, "method": "_session/delete", "params": {}}],
    )[0]


def run_acp_delete_invalid_empty(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    return run_acp_request(binary, env, cwd, [acp_delete_session_request("   ", request_id=1)])[0]


def run_acp_delete_invalid_saved_session_id(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    return run_acp_request(
        binary,
        env,
        cwd,
        [
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "_session/delete",
                "params": {"savedSessionId": "unsupported-session"},
            }
        ],
    )[0]


def acp_pointer_projection(env: dict[str, str]) -> bytes:
    pointer_dir = pathlib.Path(env["VIBE_HOME"]) / "logs" / "session" / ".last_session"
    projection: dict[str, str] = {}
    if pointer_dir.exists():
        for path in sorted(pointer_dir.iterdir()):
            if path.is_file():
                projection[path.name] = path.read_text(encoding="utf-8")
    return (json.dumps({"lastSessionPointers": projection}, separators=(",", ":"), sort_keys=True) + "\n").encode("utf-8")


def run_acp_delete_saved_pointer(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    raw = b"".join(
        run_acp_request(
            binary,
            env,
            cwd,
            [
                acp_delete_session_request("pointer-session-12345678", request_id=1),
                acp_list_sessions_request(request_id=2),
            ],
        )
    )
    return raw + acp_pointer_projection(env)


def run_acp_delete_exact_collision(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    return b"".join(
        run_acp_request(
            binary,
            env,
            cwd,
            [
                acp_delete_session_request("aaaaaaaa-2222", request_id=1),
                acp_list_sessions_request(request_id=2),
            ],
        )
    )


def run_acp_delete_live_unsaved(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_until_id(expected_id: int) -> list[str]:
        lines: list[str] = []
        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            lines.append(line)
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if data.get("id") == expected_id:
                return lines
        raise TimeoutError(f"ACP response id={expected_id} not received")

    try:
        send(acp_initialize_request())
        init_lines = read_until_id(0)
        send(acp_new_session_request(workspace, request_id=1))
        new_lines = read_until_id(1)
        new_response = next(json.loads(line) for line in new_lines if json.loads(line).get("id") == 1)
        send(acp_delete_session_request(new_response["result"]["sessionId"], request_id=2))
        delete_lines = read_until_id(2)
        send(acp_list_sessions_request(request_id=3))
        list_lines = read_until_id(3)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return "".join(init_lines + new_lines + delete_lines + list_lines).encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_delete_loaded_saved(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_until_id(expected_id: int) -> list[str]:
        lines: list[str] = []
        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            lines.append(line)
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if data.get("id") == expected_id:
                return lines
        raise TimeoutError(f"ACP response id={expected_id} not received")

    try:
        send(acp_load_session_request(workspace, "loaddelete", request_id=1))
        load_lines = read_until_id(1)
        send(acp_delete_session_request("loaddelete-12345678", request_id=2))
        delete_lines = read_until_id(2)
        send(acp_list_sessions_request(request_id=3))
        list_lines = read_until_id(3)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return "".join(load_lines + delete_lines + list_lines).encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_auth_status(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    return run_acp_request(binary, env, cwd, [acp_auth_status_request()])[0]


def run_acp_auth_signout_dotenv(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    return b"".join(
        run_acp_request(
            binary,
            env,
            cwd,
            [
                acp_auth_signout_request(request_id=1),
                acp_auth_status_request(request_id=2),
            ],
        )
    )


def run_acp_auth_signout_process_over_dotenv(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    return b"".join(
        run_acp_request(
            binary,
            env,
            cwd,
            [
                acp_auth_signout_request(request_id=1),
                acp_auth_status_request(request_id=2),
            ],
        )
    )


def run_acp_authenticate_unsupported(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    initialize = acp_initialize_request()
    initialize["id"] = 1
    return b"".join(
        run_acp_request(
            binary,
            env,
            cwd,
            [
                initialize,
                acp_authenticate_request("vibe-setup", request_id=2),
            ],
        )
    )


def run_acp_initialize_unsupported_provider(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    return run_acp_request(binary, env, cwd, [acp_initialize_request()])[0]


def run_acp_authenticate_browser_unsupported(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    initialize = acp_initialize_request()
    initialize["id"] = 1
    return b"".join(
        run_acp_request(
            binary,
            env,
            cwd,
            [
                initialize,
                acp_authenticate_request("browser-auth", request_id=2),
            ],
        )
    )


def acp_dotenv_projection(env: dict[str, str]) -> bytes:
    dotenv_path = pathlib.Path(env["VIBE_HOME"]) / ".env"
    value = ""
    if dotenv_path.exists():
        for line in dotenv_path.read_text(encoding="utf-8").splitlines():
            if line.startswith("MISTRAL_API_KEY="):
                value = "present"
    return (json.dumps({"dotenv": {"MISTRAL_API_KEY": value}}, separators=(",", ":"), sort_keys=True) + "\n").encode("utf-8")


def install_browser_opener(env: dict[str, str], base: pathlib.Path, label: str) -> None:
    script = base / f"{label}-browser-opener.py"
    log = base / f"{label}-browser-open.jsonl"
    script.write_text(
        textwrap.dedent(
            """\
            #!/usr/bin/env python3
            import json
            import os
            import sys

            with open(os.environ["BROWSER_OPEN_LOG"], "a", encoding="utf-8") as handle:
                handle.write(json.dumps({"args": sys.argv[1:]}, sort_keys=True) + "\\n")
            raise SystemExit(0)
            """
        ),
        encoding="utf-8",
    )
    script.chmod(0o755)
    env["BROWSER"] = str(script)
    env["BROWSER_OPEN_LOG"] = str(log)


def browser_open_projection(env: dict[str, str]) -> bytes:
    log = pathlib.Path(env["BROWSER_OPEN_LOG"])
    opened: list[list[str]] = []
    if log.exists():
        for line in log.read_text(encoding="utf-8").splitlines():
            if line.strip():
                payload = json.loads(line)
                args = payload.get("args", [])
                opened.append(args if isinstance(args, list) else [])
    return (json.dumps({"browserOpen": opened}, separators=(",", ":"), sort_keys=True) + "\n").encode("utf-8")


def run_acp_authenticate_browser_complete(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    raw = run_acp_request(
        binary,
        env,
        cwd,
        [acp_authenticate_request("browser-auth", request_id=1)],
    )[0]
    return raw + acp_dotenv_projection(env) + browser_open_projection(env)


def run_acp_authenticate_browser_unsupported_action(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    return run_acp_request(
        binary,
        env,
        cwd,
        [
            acp_authenticate_request(
                "browser-auth",
                request_id=1,
                action="complete",
            )
        ],
    )[0]


def run_acp_initialize_delegated_browser_auth(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    return run_acp_request(
        binary,
        env,
        cwd,
        [acp_initialize_request(field_meta={"browser-auth-delegated": True})],
    )[0]


def run_acp_authenticate_delegated_start(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    initialize = acp_initialize_request(field_meta={"browser-auth-delegated": True})
    initialize["id"] = 1
    return b"".join(
        run_acp_request(
            binary,
            env,
            cwd,
            [
                initialize,
                acp_authenticate_request("browser-auth-delegated", request_id=2),
            ],
        )
    )


def run_acp_authenticate_delegated_complete(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    raw = b"".join(
        run_acp_request(
            binary,
            env,
            cwd,
            [
                acp_authenticate_request("browser-auth-delegated", request_id=1),
                acp_authenticate_request(
                    "browser-auth-delegated",
                    request_id=2,
                    action="complete",
                    attemptId="process-123",
                ),
            ],
        )
    )
    return raw + acp_dotenv_projection(env)


def run_acp_authenticate_delegated_missing_attempt(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    return run_acp_request(
        binary,
        env,
        cwd,
        [
            acp_authenticate_request(
                "browser-auth-delegated",
                request_id=1,
                action="complete",
            )
        ],
    )[0]


def run_acp_authenticate_delegated_unknown_attempt(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    return run_acp_request(
        binary,
        env,
        cwd,
        [
            acp_authenticate_request(
                "browser-auth-delegated",
                request_id=1,
                action="complete",
                attemptId="process-404",
            )
        ],
    )[0]


def run_acp_authenticate_delegated_unsupported_action(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    return run_acp_request(
        binary,
        env,
        cwd,
        [
            acp_authenticate_request(
                "browser-auth-delegated",
                request_id=1,
                action="cancel",
            )
        ],
    )[0]


def run_acp_trust_status(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_request(binary, env, cwd, [acp_trust_status_request(workspace)])[0]


def run_acp_trust_decision(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path, decision: str) -> bytes:
    return b"".join(
        run_acp_request(
            binary,
            env,
            cwd,
            [
                acp_trust_decision_request(workspace, decision, request_id=1),
                acp_trust_status_request(workspace, request_id=2),
            ],
        )
    )


def run_acp_trust_decision_missing_session(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_request(
        binary,
        env,
        cwd,
        [acp_trust_decision_request(workspace, "trust_cwd", request_id=1, session_id="missing-session")],
    )[0]


def run_acp_telemetry_notification(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_until_id(expected_id: int) -> list[str]:
        lines: list[str] = []
        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            lines.append(line)
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if data.get("id") == expected_id:
                return lines
        raise TimeoutError(f"ACP response id={expected_id} not received")

    try:
        send(acp_initialize_request())
        init_lines = read_until_id(0)
        send(acp_new_session_request(workspace, request_id=1))
        new_lines = read_until_id(1)
        new_response = next(json.loads(line) for line in new_lines if json.loads(line).get("id") == 1)
        send(acp_telemetry_notification(new_response["result"]["sessionId"]))
        send(acp_auth_status_request(request_id=2))
        status_lines = read_until_id(2)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return "".join(init_lines + new_lines + status_lines).encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_unknown_notification(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_until_id(expected_id: int) -> list[str]:
        lines: list[str] = []
        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            lines.append(line)
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if data.get("id") == expected_id:
                return lines
        raise TimeoutError(f"ACP response id={expected_id} not received")

    try:
        send(acp_unknown_notification())
        send(acp_auth_status_request(request_id=1))
        lines = read_until_id(1)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return "".join(lines).encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_session_mutation(
    binary: list[str],
    env: dict[str, str],
    cwd: pathlib.Path,
    workspace: pathlib.Path,
    mutation,
) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_response(expected_id: int) -> dict:
        deadline = time.monotonic() + 10.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if data.get("id") == expected_id:
                return data
        raise TimeoutError(f"ACP response id={expected_id} not received")

    try:
        send(acp_new_session_request(workspace, request_id=1))
        new_response = read_response(1)
        session_id = new_response["result"]["sessionId"]
        send(mutation(session_id))
        mutation_response = read_response(2)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return (json.dumps(mutation_response, separators=(",", ":")) + "\n").encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_set_mode(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path, mode_id: str) -> bytes:
    return run_acp_session_mutation(
        binary,
        env,
        cwd,
        workspace,
        lambda session_id: acp_set_mode_request(session_id, mode_id),
    )


def run_acp_set_model(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path, model_id: str) -> bytes:
    return run_acp_session_mutation(
        binary,
        env,
        cwd,
        workspace,
        lambda session_id: acp_set_model_request(session_id, model_id),
    )


def run_acp_set_config(
    binary: list[str],
    env: dict[str, str],
    cwd: pathlib.Path,
    workspace: pathlib.Path,
    config_id: str,
    value: str,
) -> bytes:
    return run_acp_session_mutation(
        binary,
        env,
        cwd,
        workspace,
        lambda session_id: acp_set_config_option_request(session_id, config_id, value),
    )


def run_acp_set_config_value(
    binary: list[str],
    env: dict[str, str],
    cwd: pathlib.Path,
    workspace: pathlib.Path,
    config_id: str,
    value: object,
) -> bytes:
    return run_acp_session_mutation(
        binary,
        env,
        cwd,
        workspace,
        lambda session_id: acp_set_config_option_request(session_id, config_id, value),
    )


def run_acp_fork_session(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_set_mode_then_fork(binary, env, cwd, workspace, "plan")


def run_acp_set_mode_then_fork(
    binary: list[str],
    env: dict[str, str],
    cwd: pathlib.Path,
    workspace: pathlib.Path,
    mode_id: str,
) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_response(expected_id: int) -> dict:
        deadline = time.monotonic() + 10.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if data.get("id") == expected_id:
                return data
        raise TimeoutError(f"ACP response id={expected_id} not received")

    try:
        send(acp_new_session_request(workspace, request_id=1))
        new_response = read_response(1)
        session_id = new_response["result"]["sessionId"]
        send(acp_set_mode_request(session_id, mode_id, request_id=2))
        read_response(2)
        send(acp_fork_session_request(workspace, session_id, request_id=3))
        fork_response = read_response(3)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return (json.dumps(fork_response, separators=(",", ":")) + "\n").encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_fork_missing(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_request(binary, env, cwd, [acp_fork_session_request(workspace, "missing-session", request_id=1)])[0]


def run_acp_fork_from_prompt_message(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_until(predicate) -> dict:
        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if predicate(data):
                return data
        raise TimeoutError("ACP expected response not received")

    try:
        send(acp_initialize_request())
        read_until(lambda data: data.get("id") == 0)
        send(acp_new_session_request(workspace, request_id=1))
        new_response = read_until(lambda data: data.get("id") == 1)
        session_id = new_response["result"]["sessionId"]
        send(acp_prompt_request(session_id, "Fork from this prompt", request_id=3))
        prompt_response = read_until(lambda data: data.get("id") == 3)
        user_message_id = prompt_response["result"]["userMessageId"]
        send(acp_fork_session_from_message_request(workspace, session_id, user_message_id, request_id=4))
        fork_response = read_until(lambda data: data.get("id") == 4)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return (json.dumps(fork_response, separators=(",", ":")) + "\n").encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_load_session(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_until_response(
        binary,
        env,
        cwd,
        acp_load_session_request(workspace, "loadtest-12345678"),
        1,
    )


def run_acp_load_missing(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_until_response(
        binary,
        env,
        cwd,
        acp_load_session_request(workspace, "missing-session"),
        1,
    )


def run_acp_load_rich_session(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_until_response(
        binary,
        env,
        cwd,
        acp_load_session_request(workspace, "richload-12345678"),
        1,
    )


def run_acp_load_replay_ids(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_until_response(
        binary,
        env,
        cwd,
        acp_load_session_request(workspace, "replayids-1234567"),
        1,
    )


def run_acp_prompt(
    binary: list[str],
    env: dict[str, str],
    cwd: pathlib.Path,
    workspace: pathlib.Path,
    prompt: str,
) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_until_id(expected_id: int, *, keep: bool) -> list[str]:
        lines: list[str] = []
        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            if keep:
                lines.append(line)
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if data.get("id") == expected_id:
                return lines
        raise TimeoutError(f"ACP response id={expected_id} not received")

    try:
        send(acp_initialize_request())
        read_until_id(0, keep=False)
        send(acp_new_session_request(workspace, request_id=1))
        new_lines = read_until_id(1, keep=True)
        new_response = next(json.loads(line) for line in new_lines if json.loads(line).get("id") == 1)
        session_id = new_response["result"]["sessionId"]
        send(acp_prompt_request(session_id, prompt))
        prompt_lines = read_until_id(3, keep=True)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return "".join(prompt_lines).encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_prompt_request(
    binary: list[str],
    env: dict[str, str],
    cwd: pathlib.Path,
    workspace: pathlib.Path,
    request_builder: typing.Callable[[str], dict],
) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_until_id(expected_id: int, *, keep: bool) -> list[str]:
        lines: list[str] = []
        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            if keep:
                lines.append(line)
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if data.get("id") == expected_id:
                return lines
        raise TimeoutError(f"ACP response id={expected_id} not received")

    try:
        send(acp_initialize_request())
        read_until_id(0, keep=False)
        send(acp_new_session_request(workspace, request_id=1))
        new_lines = read_until_id(1, keep=True)
        new_response = next(json.loads(line) for line in new_lines if json.loads(line).get("id") == 1)
        session_id = new_response["result"]["sessionId"]
        send(request_builder(session_id))
        prompt_lines = read_until_id(3, keep=True)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return "".join(prompt_lines).encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_prompt_simple(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt(binary, env, cwd, workspace, "Just say hi")


def run_acp_prompt_missing_session(binary: list[str], env: dict[str, str], cwd: pathlib.Path) -> bytes:
    return run_acp_request(
        binary,
        env,
        cwd,
        [acp_prompt_request("missing-session", "Hello, world!", request_id=1)],
    )[0]


def run_acp_prompt_client_message_id(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt_request(binary, env, cwd, workspace, acp_prompt_client_message_id_request)


def run_acp_prompt_agent_thought(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt_request(binary, env, cwd, workspace, acp_prompt_agent_thought_request)


def run_acp_prompt_usage_accumulates(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_until_id(expected_id: int, *, keep: bool) -> list[str]:
        lines: list[str] = []
        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            if keep:
                lines.append(line)
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if data.get("id") == expected_id:
                return lines
        raise TimeoutError(f"ACP response id={expected_id} not received")

    try:
        send(acp_initialize_request())
        read_until_id(0, keep=False)
        send(acp_new_session_request(workspace, request_id=1))
        new_lines = read_until_id(1, keep=True)
        new_response = next(json.loads(line) for line in new_lines if json.loads(line).get("id") == 1)
        session_id = new_response["result"]["sessionId"]
        send(acp_prompt_request(session_id, "First usage prompt", request_id=3))
        first_lines = read_until_id(3, keep=True)
        send(acp_prompt_request(session_id, "Second usage prompt", request_id=4))
        second_lines = read_until_id(4, keep=True)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return "".join(first_lines + second_lines).encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_prompt_usage_cost(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_until_id(expected_id: int, *, keep: bool) -> list[str]:
        lines: list[str] = []
        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            if keep:
                lines.append(line)
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if data.get("id") == expected_id:
                return lines
        raise TimeoutError(f"ACP response id={expected_id} not received")

    try:
        send(acp_initialize_request())
        read_until_id(0, keep=False)
        send(acp_new_session_request(workspace, request_id=1))
        new_lines = read_until_id(1, keep=True)
        new_response = next(json.loads(line) for line in new_lines if json.loads(line).get("id") == 1)
        session_id = new_response["result"]["sessionId"]
        send(acp_prompt_request(session_id, "First usage cost prompt", request_id=3))
        first_lines = read_until_id(3, keep=True)
        send(acp_prompt_request(session_id, "Second usage cost prompt", request_id=4))
        second_lines = read_until_id(4, keep=True)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return "".join(first_lines + second_lines).encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_prompt_image(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt_request(binary, env, cwd, workspace, acp_prompt_image_request)


def run_acp_prompt_image_wrong_type(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt_request(
        binary,
        env,
        cwd,
        workspace,
        lambda session_id: acp_prompt_image_request(session_id, mime_type="image/tiff"),
    )


def run_acp_prompt_image_invalid_base64(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt_request(
        binary,
        env,
        cwd,
        workspace,
        lambda session_id: acp_prompt_image_request(session_id, data="not base64!!!"),
    )


def run_acp_command_help(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt(binary, env, cwd, workspace, "/help")


def run_acp_command_reload(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt(binary, env, cwd, workspace, "/reload")


def run_acp_command_compact_empty(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt(binary, env, cwd, workspace, "/compact")


def run_acp_command_compact_one(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_until_id(expected_id: int, *, keep: bool) -> list[str]:
        lines: list[str] = []
        deadline = time.monotonic() + 25.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            if keep:
                lines.append(line)
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if data.get("id") == expected_id:
                return lines
        raise TimeoutError(f"ACP response id={expected_id} not received")

    try:
        send(acp_initialize_request())
        read_until_id(0, keep=False)
        send(acp_new_session_request(workspace, request_id=1))
        new_lines = read_until_id(1, keep=True)
        new_response = next(json.loads(line) for line in new_lines if json.loads(line).get("id") == 1)
        session_id = new_response["result"]["sessionId"]
        send(acp_prompt_request(session_id, "Hello, tell me something", request_id=3))
        first_lines = read_until_id(3, keep=True)
        send(acp_prompt_request(session_id, "/compact", request_id=4))
        compact_lines = read_until_id(4, keep=True)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return "".join(first_lines + compact_lines).encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_command_teleport_no_history(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt(binary, env, cwd, workspace, "/teleport")


def acp_proxy_env_json(env: dict[str, str]) -> bytes:
    env_path = pathlib.Path(env["VIBE_HOME"]) / ".env"
    text = env_path.read_text(encoding="utf-8") if env_path.exists() else ""
    return (json.dumps({"proxyEnv": text}, separators=(",", ":"), sort_keys=True) + "\n").encode("utf-8")


def run_acp_command_data_retention(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt(binary, env, cwd, workspace, "/data-retention")


def run_acp_command_proxy_help(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt(binary, env, cwd, workspace, "/proxy-setup")


def run_acp_command_proxy_set(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    raw = run_acp_prompt(binary, env, cwd, workspace, "/proxy-setup HTTP_PROXY http://localhost:8080")
    return raw + acp_proxy_env_json(env)


def run_acp_command_proxy_unset(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    env_path = pathlib.Path(env["VIBE_HOME"]) / ".env"
    env_path.parent.mkdir(parents=True, exist_ok=True)
    env_path.write_text("HTTP_PROXY=http://old-proxy.com\n", encoding="utf-8")
    raw = run_acp_prompt(binary, env, cwd, workspace, "/proxy-setup HTTP_PROXY")
    return raw + acp_proxy_env_json(env)


def run_acp_command_proxy_invalid(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt(binary, env, cwd, workspace, "/proxy-setup INVALID_KEY value")


def run_acp_command_proxy_case(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    raw = run_acp_prompt(binary, env, cwd, workspace, "/PROXY-SETUP http_proxy http://localhost:8080")
    return raw + acp_proxy_env_json(env)


def run_acp_tool_meta_web_fetch(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt_permission(binary, env, cwd, workspace, "allow_once", "Fetch metadata parity")


def run_acp_tool_meta_web_search(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt_permission(binary, env, cwd, workspace, "allow_once", "Search metadata parity")


def run_acp_tool_meta_skill(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt(binary, env, cwd, workspace, "Skill metadata parity")


def run_acp_tool_meta_task(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt(binary, env, cwd, workspace, "Task metadata parity")


def run_acp_prompt_todo(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt(binary, env, cwd, workspace, "Todo ACP parity")


def run_acp_prompt_todo_invalid(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt(binary, env, cwd, workspace, "Todo ACP invalid parity")


def run_acp_user_display_content(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_until_id(expected_id: int) -> dict:
        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if data.get("id") == expected_id:
                return data
        raise TimeoutError(f"ACP response id={expected_id} not received")

    try:
        send(acp_initialize_request())
        read_until_id(0)
        send(acp_new_session_request(workspace, request_id=1))
        session_id = read_until_id(1)["result"]["sessionId"]
        send(acp_prompt_user_display_request(session_id, request_id=3))
        read_until_id(3)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)

        session_root = pathlib.Path(env["VIBE_HOME"]) / "logs" / "session"
        messages_file = next(session_root.glob("session_*/messages.jsonl"))
        messages = [json.loads(line) for line in messages_file.read_text(encoding="utf-8").splitlines()]
        user = next(message for message in messages if message.get("role") == "user")
        return (json.dumps({"user_display_content": user.get("user_display_content")}, sort_keys=True) + "\n").encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_prompt_grep(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt(binary, env, cwd, workspace, "Search auth")


def run_acp_prompt_permission(
    binary: list[str],
    env: dict[str, str],
    cwd: pathlib.Path,
    workspace: pathlib.Path,
    selected_option_id: str,
    prompt: str = "Search auth",
    *,
    outcome: str = "selected",
) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_until(predicate, *, keep: bool) -> tuple[list[str], dict]:
        lines: list[str] = []
        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            if keep:
                lines.append(line)
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if predicate(data):
                return lines, data
        raise TimeoutError("ACP expected response not received")

    try:
        send(acp_initialize_request())
        read_until(lambda data: data.get("id") == 0, keep=False)
        send(acp_new_session_request(workspace, request_id=1))
        _, new_response = read_until(lambda data: data.get("id") == 1, keep=False)
        session_id = new_response["result"]["sessionId"]
        send(acp_prompt_request(session_id, prompt))
        permission_lines, permission_request = read_until(
            lambda data: data.get("method") == "session/request_permission",
            keep=True,
        )
        if outcome == "selected":
            permission_response_outcome = {
                "outcome": "selected",
                "optionId": selected_option_id,
            }
        else:
            permission_response_outcome = {"outcome": outcome}
        send(
            {
                "jsonrpc": "2.0",
                "id": permission_request["id"],
                "result": {"outcome": permission_response_outcome},
            }
        )
        prompt_lines, _ = read_until(lambda data: data.get("id") == 3, keep=True)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return "".join(permission_lines + prompt_lines).encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_prompt_permission_cancelled(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt_permission(
        binary,
        env,
        cwd,
        workspace,
        "reject_once",
        outcome="cancelled",
    )


def run_acp_prompt_permission_allow_always(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_until(predicate, *, keep: bool) -> tuple[list[str], dict]:
        lines: list[str] = []
        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            if keep:
                lines.append(line)
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if predicate(data):
                return lines, data
        raise TimeoutError("ACP expected response not received")

    try:
        send(acp_initialize_request())
        read_until(lambda data: data.get("id") == 0, keep=False)
        send(acp_new_session_request(workspace, request_id=1))
        _, new_response = read_until(lambda data: data.get("id") == 1, keep=False)
        session_id = new_response["result"]["sessionId"]

        send(acp_prompt_request(session_id, "Search auth", request_id=3))
        permission_lines, permission_request = read_until(
            lambda data: data.get("method") == "session/request_permission",
            keep=True,
        )
        send(
            {
                "jsonrpc": "2.0",
                "id": permission_request["id"],
                "result": {
                    "outcome": {
                        "outcome": "selected",
                        "optionId": "allow_always",
                    }
                },
            }
        )
        first_prompt_lines, _ = read_until(lambda data: data.get("id") == 3, keep=True)

        send(acp_prompt_request(session_id, "Search auth again", request_id=4))
        second_prompt_lines, _ = read_until(lambda data: data.get("id") == 4, keep=True)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return "".join(permission_lines + first_prompt_lines + second_prompt_lines).encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_prompt_permission_allow_always_permanent(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_prompt_permission(
        binary,
        env,
        cwd,
        workspace,
        "allow_always_permanent",
    )


def run_acp_first_permission_request(
    binary: list[str],
    env: dict[str, str],
    cwd: pathlib.Path,
    workspace: pathlib.Path,
    prompt: str,
    *,
    terminal: bool = False,
) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_until(predicate) -> str:
        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if predicate(data):
                return line
        raise TimeoutError("ACP expected permission request not received")

    try:
        send(acp_initialize_request(terminal=terminal))
        read_until(lambda data: data.get("id") == 0)
        send(acp_new_session_request(workspace, request_id=1))
        new_line = read_until(lambda data: data.get("id") == 1)
        session_id = json.loads(new_line)["result"]["sessionId"]
        send(acp_prompt_request(session_id, prompt))
        permission_line = read_until(lambda data: data.get("method") == "session/request_permission")
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return permission_line.encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_permission_bash_granular(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    return run_acp_first_permission_request(
        binary,
        env,
        cwd,
        workspace,
        "Run npm install foo",
        terminal=True,
    )


def run_acp_permission_bash_granular_allow_always_permanent(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_until(predicate, *, keep: bool) -> tuple[list[str], dict]:
        lines: list[str] = []
        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            if keep:
                lines.append(line)
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if predicate(data):
                return lines, data
        raise TimeoutError("ACP expected response not received")

    try:
        send(acp_initialize_request())
        read_until(lambda data: data.get("id") == 0, keep=False)
        send(acp_new_session_request(workspace, request_id=1))
        _, new_response = read_until(lambda data: data.get("id") == 1, keep=False)
        session_id = new_response["result"]["sessionId"]
        send(acp_prompt_request(session_id, "Run npm install help"))
        _, permission_request = read_until(
            lambda data: data.get("method") == "session/request_permission",
            keep=False,
        )
        send(
            {
                "jsonrpc": "2.0",
                "id": permission_request["id"],
                "result": {
                    "outcome": {
                        "outcome": "selected",
                        "optionId": "allow_always_permanent",
                    }
                },
            }
        )
        read_until(lambda data: data.get("id") == 3, keep=False)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return (json.dumps(permission_request, separators=(",", ":")) + "\n").encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_prompt_fs_read(
    binary: list[str],
    env: dict[str, str],
    cwd: pathlib.Path,
    workspace: pathlib.Path,
    client_content: str,
) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_until(predicate, *, keep: bool) -> tuple[list[str], dict]:
        lines: list[str] = []
        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            if keep:
                lines.append(line)
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if predicate(data):
                return lines, data
        raise TimeoutError("ACP expected response not received")

    try:
        send(acp_initialize_request(fs_read=True))
        read_until(lambda data: data.get("id") == 0, keep=False)
        send(acp_new_session_request(workspace, request_id=1))
        _, new_response = read_until(lambda data: data.get("id") == 1, keep=False)
        session_id = new_response["result"]["sessionId"]
        send(acp_prompt_request(session_id, "Read client file"))
        read_lines, read_request = read_until(
            lambda data: data.get("method") == "fs/read_text_file",
            keep=True,
        )
        send(
            {
                "jsonrpc": "2.0",
                "id": read_request["id"],
                "result": {"content": client_content},
            }
        )
        prompt_lines, _ = read_until(lambda data: data.get("id") == 3, keep=True)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return "".join(read_lines + prompt_lines).encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_prompt_fs_write(
    binary: list[str],
    env: dict[str, str],
    cwd: pathlib.Path,
    workspace: pathlib.Path,
) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_until(predicate, *, keep: bool) -> tuple[list[str], dict]:
        lines: list[str] = []
        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            if keep:
                lines.append(line)
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if predicate(data):
                return lines, data
        raise TimeoutError("ACP expected response not received")

    try:
        send(acp_initialize_request(fs_write=True))
        read_until(lambda data: data.get("id") == 0, keep=False)
        send(acp_new_session_request(workspace, request_id=1))
        _, new_response = read_until(lambda data: data.get("id") == 1, keep=False)
        session_id = new_response["result"]["sessionId"]
        send(acp_prompt_request(session_id, "Write client file"))
        permission_lines, permission_request = read_until(
            lambda data: data.get("method") == "session/request_permission",
            keep=True,
        )
        send(
            {
                "jsonrpc": "2.0",
                "id": permission_request["id"],
                "result": {
                    "outcome": {
                        "outcome": "selected",
                        "optionId": "allow_once",
                    }
                },
            }
        )
        write_lines, write_request = read_until(
            lambda data: data.get("method") == "fs/write_text_file",
            keep=True,
        )
        send({"jsonrpc": "2.0", "id": write_request["id"], "result": {}})
        prompt_lines, _ = read_until(lambda data: data.get("id") == 3, keep=True)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return "".join(permission_lines + write_lines + prompt_lines).encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_prompt_fs_edit(
    binary: list[str],
    env: dict[str, str],
    cwd: pathlib.Path,
    workspace: pathlib.Path,
    client_content: str,
) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_until(predicate, *, keep: bool) -> tuple[list[str], dict]:
        lines: list[str] = []
        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            if keep:
                lines.append(line)
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if predicate(data):
                return lines, data
        raise TimeoutError("ACP expected response not received")

    try:
        send(acp_initialize_request(fs_read=True, fs_write=True))
        read_until(lambda data: data.get("id") == 0, keep=False)
        send(acp_new_session_request(workspace, request_id=1))
        _, new_response = read_until(lambda data: data.get("id") == 1, keep=False)
        session_id = new_response["result"]["sessionId"]
        send(acp_prompt_request(session_id, "Edit client file"))
        permission_lines, permission_request = read_until(
            lambda data: data.get("method") == "session/request_permission",
            keep=True,
        )
        send(
            {
                "jsonrpc": "2.0",
                "id": permission_request["id"],
                "result": {
                    "outcome": {
                        "outcome": "selected",
                        "optionId": "allow_once",
                    }
                },
            }
        )
        read_lines, read_request = read_until(
            lambda data: data.get("method") == "fs/read_text_file",
            keep=True,
        )
        send(
            {
                "jsonrpc": "2.0",
                "id": read_request["id"],
                "result": {"content": client_content},
            }
        )
        write_lines, write_request = read_until(
            lambda data: data.get("method") == "fs/write_text_file",
            keep=True,
        )
        send({"jsonrpc": "2.0", "id": write_request["id"], "result": {}})
        prompt_lines, _ = read_until(lambda data: data.get("id") == 3, keep=True)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return "".join(permission_lines + read_lines + write_lines + prompt_lines).encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_prompt_terminal_bash(
    binary: list[str],
    env: dict[str, str],
    cwd: pathlib.Path,
    workspace: pathlib.Path,
    *,
    prompt: str = "Run terminal bash",
    terminal_output: str = "bash-parity",
    exit_code: int | None = 0,
    wait_timeout: bool = False,
) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def send(request: dict) -> None:
        process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def read_until(predicate, *, keep: bool) -> tuple[list[str], dict]:
        lines: list[str] = []
        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            if keep:
                lines.append(line)
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if predicate(data):
                return lines, data
        raise TimeoutError("ACP expected response not received")

    try:
        send(acp_initialize_request(terminal=True))
        read_until(lambda data: data.get("id") == 0, keep=False)
        send(acp_new_session_request(workspace, request_id=1))
        _, new_response = read_until(lambda data: data.get("id") == 1, keep=False)
        session_id = new_response["result"]["sessionId"]
        send(acp_prompt_request(session_id, prompt))
        permission_lines, permission_request = read_until(
            lambda data: data.get("method") == "session/request_permission",
            keep=True,
        )
        send(
            {
                "jsonrpc": "2.0",
                "id": permission_request["id"],
                "result": {
                    "outcome": {
                        "outcome": "selected",
                        "optionId": "allow_once",
                    }
                },
            }
        )
        create_lines, create_request = read_until(
            lambda data: data.get("method") == "terminal/create",
            keep=True,
        )
        send(
            {
                "jsonrpc": "2.0",
                "id": create_request["id"],
                "result": {"terminalId": "terminal_parity_123"},
            }
        )
        wait_lines, wait_request = read_until(
            lambda data: data.get("method") == "terminal/wait_for_exit",
            keep=True,
        )
        output_lines: list[str] = []
        kill_lines: list[str] = []
        if wait_timeout:
            kill_lines, kill_request = read_until(
                lambda data: data.get("method") == "terminal/kill",
                keep=True,
            )
            send({"jsonrpc": "2.0", "id": kill_request["id"], "result": {}})
        else:
            send(
                {
                    "jsonrpc": "2.0",
                    "id": wait_request["id"],
                    "result": {"exitCode": exit_code},
                }
            )
            output_lines, output_request = read_until(
                lambda data: data.get("method") == "terminal/output",
                keep=True,
            )
            send(
                {
                    "jsonrpc": "2.0",
                    "id": output_request["id"],
                    "result": {
                        "output": terminal_output,
                        "truncated": False,
                        "exitStatus": {"exitCode": exit_code},
                    },
                }
            )
        release_lines, release_request = read_until(
            lambda data: data.get("method") == "terminal/release",
            keep=True,
        )
        send({"jsonrpc": "2.0", "id": release_request["id"], "result": {}})
        prompt_lines, _ = read_until(lambda data: data.get("id") == 3, keep=True)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return "".join(
            permission_lines
            + create_lines
            + wait_lines
            + kill_lines
            + output_lines
            + release_lines
            + prompt_lines
        ).encode("utf-8")
    except Exception:
        process.kill()
        raise


def run_acp_close_session(binary: list[str], env: dict[str, str], cwd: pathlib.Path, workspace: pathlib.Path) -> bytes:
    process = subprocess.Popen(
        binary,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    def read_response(expected_id: int) -> dict:
        deadline = time.monotonic() + 10.0
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                break
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if data.get("id") == expected_id:
                return data
        raise TimeoutError(f"ACP response id={expected_id} not received")

    try:
        process.stdin.write(json.dumps(acp_new_session_request(workspace), separators=(",", ":")) + "\n")
        process.stdin.flush()
        first_response = read_response(1)
        session_id = first_response["result"]["sessionId"]
        process.stdin.write(json.dumps(acp_close_session_request(session_id), separators=(",", ":")) + "\n")
        process.stdin.flush()
        close_response = read_response(2)
        process.terminate()
        try:
            process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2.0)
        return (json.dumps(close_response, separators=(",", ":")) + "\n").encode("utf-8")
    except Exception:
        process.kill()
        raise


def seed_acp_saved_sessions(vibe_home: pathlib.Path) -> None:
    sessions = [
        ("aaaaaaaa-1111", "/home/user/project1", "First session", "2024-01-01T12:00:00Z"),
        ("bbbbbbbb-2222", "/home/user/project2", "Second session", "2024-01-01T13:00:00Z"),
    ]
    root = vibe_home / "logs" / "session"
    for index, (session_id, cwd, title, end_time) in enumerate(sessions):
        session_dir = root / f"session_20240101_12000{index}_{session_id[:8]}"
        session_dir.mkdir(parents=True, exist_ok=True)
        (session_dir / "messages.jsonl").write_text(
            json.dumps({"role": "user", "content": "Hello"}) + "\n",
            encoding="utf-8",
        )
        (session_dir / "meta.json").write_text(
            json.dumps(
                {
                    "session_id": session_id,
                    "start_time": "2024-01-01T12:00:00Z",
                    "end_time": end_time,
                    "environment": {"working_directory": cwd},
                    "title": title,
                }
            ),
            encoding="utf-8",
        )


def seed_acp_cwd_filter_sessions(vibe_home: pathlib.Path, project1: pathlib.Path, project2: pathlib.Path) -> None:
    sessions = [
        ("aaaaaaaa-proj1", project1, "Project 1 session", "2024-01-01T12:00:00Z"),
        ("bbbbbbbb-proj2", project2, "Project 2 session", "2024-01-01T13:00:00Z"),
        ("cccccccc-proj1", project1, "Another Project 1 session", "2024-01-01T14:00:00Z"),
    ]
    root = vibe_home / "logs" / "session"
    for index, (session_id, cwd, title, end_time) in enumerate(sessions):
        session_dir = root / f"session_20240101_13000{index}_{session_id[:8]}"
        session_dir.mkdir(parents=True, exist_ok=True)
        (session_dir / "messages.jsonl").write_text(
            json.dumps({"role": "user", "content": title}) + "\n",
            encoding="utf-8",
        )
        (session_dir / "meta.json").write_text(
            json.dumps(
                {
                    "session_id": session_id,
                    "start_time": "2024-01-01T12:00:00Z",
                    "end_time": end_time,
                    "environment": {"working_directory": str(cwd)},
                    "title": title,
                }
            ),
            encoding="utf-8",
        )


def seed_acp_sorted_sessions(vibe_home: pathlib.Path, cwd: pathlib.Path) -> None:
    sessions = [
        ("oldest-s", "Oldest", "2024-01-01T10:00:00Z"),
        ("newest-s", "Newest", "2024-01-01T14:00:00Z"),
        ("middle-s", "Middle", "2024-01-01T12:00:00Z"),
    ]
    root = vibe_home / "logs" / "session"
    for index, (session_id, title, end_time) in enumerate(sessions):
        session_dir = root / f"session_20240101_14000{index}_{session_id[:8]}"
        session_dir.mkdir(parents=True, exist_ok=True)
        (session_dir / "messages.jsonl").write_text(
            json.dumps({"role": "user", "content": title}) + "\n",
            encoding="utf-8",
        )
        (session_dir / "meta.json").write_text(
            json.dumps(
                {
                    "session_id": session_id,
                    "start_time": "2024-01-01T09:00:00Z",
                    "end_time": end_time,
                    "environment": {"working_directory": str(cwd)},
                    "title": title,
                }
            ),
            encoding="utf-8",
        )


def seed_acp_invalid_list_sessions(vibe_home: pathlib.Path, cwd: pathlib.Path) -> None:
    root = vibe_home / "logs" / "session"
    valid_dir = root / "session_20240101_150000_valid-se"
    valid_dir.mkdir(parents=True, exist_ok=True)
    (valid_dir / "messages.jsonl").write_text(
        json.dumps({"role": "user", "content": "valid"}) + "\n",
        encoding="utf-8",
    )
    (valid_dir / "meta.json").write_text(
        json.dumps(
            {
                "session_id": "valid-se",
                "start_time": "2024-01-01T10:00:00Z",
                "end_time": "2024-01-01T10:00:00Z",
                "environment": {"working_directory": str(cwd)},
                "title": "Valid Session",
            }
        ),
        encoding="utf-8",
    )

    missing_messages_dir = root / "session_20240101_150001_missingm"
    missing_messages_dir.mkdir(parents=True, exist_ok=True)
    (missing_messages_dir / "meta.json").write_text(
        json.dumps(
            {
                "session_id": "missing-messages",
                "end_time": "2024-01-01T16:00:00Z",
                "environment": {"working_directory": str(cwd)},
                "title": "Missing messages",
            }
        ),
        encoding="utf-8",
    )

    no_id_dir = root / "session_20240101_150002_noid0000"
    no_id_dir.mkdir(parents=True, exist_ok=True)
    (no_id_dir / "messages.jsonl").write_text(
        json.dumps({"role": "user", "content": "no id"}) + "\n",
        encoding="utf-8",
    )
    (no_id_dir / "meta.json").write_text(
        json.dumps({"environment": {"working_directory": str(cwd)}}),
        encoding="utf-8",
    )

    empty_messages_dir = root / "session_20240101_150003_emptymsg"
    empty_messages_dir.mkdir(parents=True, exist_ok=True)
    (empty_messages_dir / "messages.jsonl").write_text("", encoding="utf-8")
    (empty_messages_dir / "meta.json").write_text(
        json.dumps(
            {
                "session_id": "empty-messages",
                "end_time": "2024-01-01T17:00:00Z",
                "environment": {"working_directory": str(cwd)},
                "title": "Empty messages",
            }
        ),
        encoding="utf-8",
    )

    non_object_messages_dir = root / "session_20240101_150004_nonobject"
    non_object_messages_dir.mkdir(parents=True, exist_ok=True)
    (non_object_messages_dir / "messages.jsonl").write_text(
        json.dumps(["not", "an", "object"]) + "\n",
        encoding="utf-8",
    )
    (non_object_messages_dir / "meta.json").write_text(
        json.dumps(
            {
                "session_id": "non-object-message",
                "end_time": "2024-01-01T18:00:00Z",
                "environment": {"working_directory": str(cwd)},
                "title": "Non-object message",
            }
        ),
        encoding="utf-8",
    )


def seed_acp_timestamp_sessions(vibe_home: pathlib.Path, cwd: pathlib.Path) -> None:
    sessions = [
        ("offset-s", "Offset time", "2024-01-01T12:00:00+02:00"),
        ("zulu-s", "Zulu time", "2024-01-01T11:00:00Z"),
        ("invalid-s", "Invalid time", "not-a-timestamp"),
    ]
    root = vibe_home / "logs" / "session"
    for index, (session_id, title, end_time) in enumerate(sessions):
        session_dir = root / f"session_20240101_16000{index}_{session_id[:8]}"
        session_dir.mkdir(parents=True, exist_ok=True)
        (session_dir / "messages.jsonl").write_text(
            json.dumps({"role": "user", "content": title}) + "\n",
            encoding="utf-8",
        )
        (session_dir / "meta.json").write_text(
            json.dumps(
                {
                    "session_id": session_id,
                    "start_time": "2024-01-01T09:00:00Z",
                    "end_time": end_time,
                    "environment": {"working_directory": str(cwd)},
                    "title": title,
                }
            ),
            encoding="utf-8",
        )


def seed_acp_single_saved_session(
    vibe_home: pathlib.Path,
    session_id: str,
    cwd: pathlib.Path,
    *,
    title: str = "Saved ACP title",
) -> None:
    session_dir = vibe_home / "logs" / "session" / f"session_20240101_120000_{session_id[:8]}"
    session_dir.mkdir(parents=True, exist_ok=True)
    (session_dir / "messages.jsonl").write_text(
        json.dumps({"role": "user", "content": "Hello"}) + "\n",
        encoding="utf-8",
    )
    (session_dir / "meta.json").write_text(
        json.dumps(
            {
                "session_id": session_id,
                "start_time": "2024-01-01T12:00:00Z",
                "end_time": "2024-01-01T12:05:00Z",
                "git_commit": None,
                "git_branch": None,
                "username": "test-user",
                "environment": {"working_directory": str(cwd)},
                "title": title,
                "title_source": "auto",
            },
            separators=(",", ":"),
        ),
        encoding="utf-8",
    )


def seed_legacy_json_session(vibe_home: pathlib.Path, cwd: pathlib.Path) -> None:
    session_root = vibe_home / "logs" / "session"
    session_root.mkdir(parents=True, exist_ok=True)
    legacy_file = session_root / "session_20240101_120000_legacy1.json"
    legacy_file.write_text(
        json.dumps(
            {
                "metadata": {
                    "session_id": "legacy12-session",
                    "start_time": "2024-01-01T12:00:00Z",
                    "end_time": "2024-01-01T12:05:00Z",
                    "git_commit": None,
                    "git_branch": None,
                    "username": "test-user",
                    "environment": {"working_directory": str(cwd)},
                    "title": "Legacy JSON title",
                    "title_source": "auto",
                    "total_messages": 2,
                },
                "messages": [
                    {"role": "system", "content": "System prompt"},
                    {"role": "user", "content": "Legacy hello"},
                    {"role": "assistant", "content": "Legacy response"},
                ],
            },
            separators=(",", ":"),
        ),
        encoding="utf-8",
    )


def seed_invalid_newer_saved_session(vibe_home: pathlib.Path, cwd: pathlib.Path) -> None:
    seed_acp_single_saved_session(vibe_home, "validbad-12345678", cwd, title="Valid before corrupt")
    invalid_dir = vibe_home / "logs" / "session" / "session_20240101_130000_invalid1"
    invalid_dir.mkdir(parents=True, exist_ok=True)
    (invalid_dir / "messages.jsonl").write_text(
        json.dumps({"role": "user", "content": "Corrupt newer"}) + "\n",
        encoding="utf-8",
    )
    (invalid_dir / "meta.json").write_text("{invalid json}", encoding="utf-8")


def seed_same_end_time_sessions(vibe_home: pathlib.Path, cwd: pathlib.Path) -> None:
    seed_acp_single_saved_session(vibe_home, "olderabc-12345678", cwd, title="Older mtime")
    seed_acp_single_saved_session(vibe_home, "newerabc-12345678", cwd, title="Newer mtime")
    root = vibe_home / "logs" / "session"
    older_messages = root / "session_20240101_120000_olderabc" / "messages.jsonl"
    newer_messages = root / "session_20240101_120000_newerabc" / "messages.jsonl"
    os.utime(older_messages, (1704110400, 1704110400))
    os.utime(newer_messages, (1704114000, 1704114000))


def seed_acp_pointer_session(vibe_home: pathlib.Path, cwd: pathlib.Path) -> None:
    seed_acp_single_saved_session(vibe_home, "pointer-session-12345678", cwd, title="Pointer session")
    pointer_dir = vibe_home / "logs" / "session" / ".last_session"
    pointer_dir.mkdir(parents=True, exist_ok=True)
    (pointer_dir / "ttys001").write_text("pointer-session-12345678\n", encoding="utf-8")
    (pointer_dir / "ttys002").write_text("other-session\n", encoding="utf-8")


def seed_acp_collision_sessions(vibe_home: pathlib.Path, cwd: pathlib.Path) -> None:
    seed_acp_single_saved_session(vibe_home, "aaaaaaaa-1111", cwd, title="Collision survivor")
    target_dir = vibe_home / "logs" / "session" / "session_20240101_120500_aaaaaaaa"
    target_dir.mkdir(parents=True, exist_ok=True)
    (target_dir / "messages.jsonl").write_text(
        json.dumps({"role": "user", "content": "Hello target"}) + "\n",
        encoding="utf-8",
    )
    (target_dir / "meta.json").write_text(
        json.dumps(
            {
                "session_id": "aaaaaaaa-2222",
                "start_time": "2024-01-01T12:00:00Z",
                "end_time": "2024-01-01T12:05:00Z",
                "git_commit": None,
                "git_branch": None,
                "username": "test-user",
                "environment": {"working_directory": str(cwd)},
                "title": "Collision target",
                "title_source": "auto",
            },
            separators=(",", ":"),
        ),
        encoding="utf-8",
    )


def seed_acp_dotenv(vibe_home: pathlib.Path) -> None:
    vibe_home.mkdir(parents=True, exist_ok=True)
    (vibe_home / ".env").write_text("MISTRAL_API_KEY='sk-acp-dotenv'\n", encoding="utf-8")


def seed_acp_load_session(vibe_home: pathlib.Path, workspace: pathlib.Path) -> None:
    session_id = "loadtest-12345678"
    session_dir = vibe_home / "logs" / "session" / f"session_20240101_120000_{session_id[:8]}"
    session_dir.mkdir(parents=True, exist_ok=True)
    messages = [
        {"role": "user", "content": "Hello world"},
        {"role": "assistant", "content": "Hi there"},
    ]
    (session_dir / "messages.jsonl").write_text(
        "".join(json.dumps(message, separators=(",", ":")) + "\n" for message in messages),
        encoding="utf-8",
    )


def seed_acp_rich_load_session(vibe_home: pathlib.Path, workspace: pathlib.Path) -> None:
    session_id = "richload-12345678"
    session_dir = vibe_home / "logs" / "session" / f"session_20240101_120000_{session_id[:8]}"
    session_dir.mkdir(parents=True, exist_ok=True)
    user_display_content = {
        "version": "1.0.0",
        "host": "mistral-vscode",
        "content": [{"type": "text", "text": "Read "}],
    }
    messages = [
        {"role": "user", "content": "Read the file", "user_display_content": user_display_content},
        {
            "role": "assistant",
            "content": "",
            "tool_calls": [
                {
                    "id": "call_123",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": "{\"file_path\":\"/tmp/test.txt\"}",
                    },
                }
            ],
        },
        {"role": "tool", "tool_call_id": "call_123", "name": "read", "content": "file contents"},
        {"role": "assistant", "content": "Answer", "reasoning_content": "Thinking"},
    ]
    (session_dir / "messages.jsonl").write_text(
        "".join(json.dumps(message, separators=(",", ":")) + "\n" for message in messages),
        encoding="utf-8",
    )
    (session_dir / "meta.json").write_text(
        json.dumps(
            {
                "session_id": session_id,
                "start_time": "2024-01-01T12:00:00Z",
                "end_time": "2024-01-01T12:05:00Z",
                "git_commit": None,
                "git_branch": None,
                "username": "test-user",
                "environment": {"working_directory": str(workspace)},
                "title": "Rich loaded title",
            },
            separators=(",", ":"),
        ),
        encoding="utf-8",
    )
    (session_dir / "meta.json").write_text(
        json.dumps(
            {
                "session_id": session_id,
                "start_time": "2024-01-01T12:00:00Z",
                "end_time": "2024-01-01T12:05:00Z",
                "git_commit": None,
                "git_branch": None,
                "username": "test-user",
                "environment": {"working_directory": str(workspace)},
                "title": "Loaded title",
            },
            separators=(",", ":"),
        ),
        encoding="utf-8",
    )


def seed_acp_load_replay_ids(vibe_home: pathlib.Path, workspace: pathlib.Path) -> None:
    session_id = "replayids-1234567"
    session_dir = vibe_home / "logs" / "session" / f"session_20240101_120000_{session_id[:8]}"
    session_dir.mkdir(parents=True, exist_ok=True)
    user_display_content = {
        "version": "1.0.0",
        "host": "mistral-vscode",
        "content": [
            {"type": "text", "text": "Look at "},
            {
                "type": "workspace_mention",
                "kind": "file",
                "uri": "file:///repo/src/app.ts",
                "name": "app.ts",
            },
        ],
    }
    messages = [
        {
            "role": "user",
            "content": "Look at app.ts",
            "message_id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "user_display_content": user_display_content,
        },
        {
            "role": "assistant",
            "content": "",
            "tool_calls": [
                {
                    "id": "call_replay_123",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": "{\"file_path\":\"/tmp/replay.txt\",\"offset\":0,\"limit\":20}",
                    },
                }
            ],
        },
        {"role": "tool", "tool_call_id": "call_replay_123", "name": "read", "content": "file contents"},
        {"role": "tool", "tool_call_id": "call_orphan", "name": "read", "content": "orphan contents"},
        {
            "role": "assistant",
            "content": "Here is my answer",
            "message_id": "11111111-2222-3333-4444-555555555555",
            "reasoning_content": "Let me think...",
            "reasoning_message_id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
        },
    ]
    (session_dir / "messages.jsonl").write_text(
        "".join(json.dumps(message, separators=(",", ":")) + "\n" for message in messages),
        encoding="utf-8",
    )
    (session_dir / "meta.json").write_text(
        json.dumps(
            {
                "session_id": session_id,
                "start_time": "2024-01-01T12:00:00Z",
                "end_time": "2024-01-01T12:05:00Z",
                "git_commit": None,
                "git_branch": None,
                "username": "test-user",
                "environment": {"working_directory": str(workspace)},
                "title": "Replay ids title",
            },
            separators=(",", ":"),
        ),
        encoding="utf-8",
    )


def char_width(ch: str) -> int:
    import unicodedata

    if unicodedata.combining(ch):
        return 0
    if unicodedata.east_asian_width(ch) in {"F", "W"}:
        return 2
    return 1


class Screen:
    def __init__(self, rows: int = 36, cols: int = 120) -> None:
        self.rows = rows
        self.cols = cols
        self.grid = [[" "] * cols for _ in range(rows)]
        self.row = 0
        self.col = 0

    def put(self, ch: str) -> None:
        if ch == "\n":
            self.row = min(self.rows - 1, self.row + 1)
            return
        if ch == "\r":
            self.col = 0
            return
        if ch == "\b":
            self.col = max(0, self.col - 1)
            return
        if ch < " ":
            return

        width = max(1, char_width(ch))
        if self.col >= self.cols:
            self.col = 0
            self.row = min(self.rows - 1, self.row + 1)
        if self.row < self.rows and self.col < self.cols:
            self.grid[self.row][self.col] = ch
            for idx in range(1, width):
                if self.col + idx < self.cols:
                    self.grid[self.row][self.col + idx] = ""
        self.col += width

    def csi(self, private: str, params: str, final: str) -> None:
        if private.startswith("?"):
            return
        nums = []
        for part in params.split(";"):
            if part == "":
                continue
            if part.isdigit():
                nums.append(int(part))
            else:
                nums.append(0)
        n = nums[0] if nums else 0
        if final in {"H", "f"}:
            row = (nums[0] if len(nums) >= 1 and nums[0] else 1) - 1
            col = (nums[1] if len(nums) >= 2 and nums[1] else 1) - 1
            self.row = min(max(row, 0), self.rows - 1)
            self.col = min(max(col, 0), self.cols - 1)
        elif final == "A":
            self.row = max(0, self.row - (n or 1))
        elif final == "B":
            self.row = min(self.rows - 1, self.row + (n or 1))
        elif final == "C":
            self.col = min(self.cols - 1, self.col + (n or 1))
        elif final == "D":
            self.col = max(0, self.col - (n or 1))
        elif final == "G":
            self.col = min(max((n or 1) - 1, 0), self.cols - 1)
        elif final == "J":
            mode = n
            if mode in {0, 2, 3}:
                start = 0 if mode in {2, 3} else self.row
                for row in range(start, self.rows):
                    col_start = 0
                    if mode == 0 and row == self.row:
                        col_start = self.col
                    for col in range(col_start, self.cols):
                        self.grid[row][col] = " "
        elif final == "K":
            mode = n
            if mode == 0:
                rng = range(self.col, self.cols)
            elif mode == 1:
                rng = range(0, self.col + 1)
            else:
                rng = range(0, self.cols)
            for col in rng:
                self.grid[self.row][col] = " "
        elif final in {"m", "h", "l", "r", "s", "u"}:
            return

    def text(self) -> str:
        return "\n".join("".join(row).rstrip() for row in self.grid).rstrip() + "\n"


def render_screen(raw: bytes, rows: int = 36, cols: int = 120) -> str:
    text = raw.decode("utf-8", "replace")
    screen = Screen(rows, cols)
    idx = 0
    while idx < len(text):
        if text[idx] == "\x1b":
            csi = CSI_RE.match(text, idx)
            if csi:
                raw_params = csi.group(1)
                private = raw_params[0] if raw_params[:1] in {"?", ">", "=", "<"} else ""
                params = raw_params[1:] if private else raw_params
                screen.csi(private, params, csi.group(3))
                idx = csi.end()
                continue
            if text.startswith("\x1b]", idx):
                end_bel = text.find("\x07", idx)
                end_st = text.find("\x1b\\", idx)
                ends = [pos for pos in [end_bel, end_st] if pos != -1]
                if ends:
                    end = min(ends)
                    idx = end + (1 if end == end_bel else 2)
                    continue
            idx += 1
            continue
        screen.put(text[idx])
        idx += 1
    rendered = screen.text()
    rendered = re.sub(r"\d+(?:\.\d+)?s\b", "<duration>", rendered)
    rendered = re.sub(r"(?<=Scheduled loop )[0-9a-f]{8}", "<loop_id>", rendered, flags=re.I)
    rendered = re.sub(r"(?<=Cancelled loop )[0-9a-f]{8}", "<loop_id>", rendered, flags=re.I)
    rendered = re.sub(r"(?<=│ )[0-9a-f]{8}(?= +│)", "<loop_id>", rendered, flags=re.I)
    rendered = re.sub(r"[0-9a-f]{8}-[0-9a-f-]{27,}", "<uuid>", rendered, flags=re.I)
    rendered = re.sub(r"session_\d{8}_\d{6}_[0-9a-f]{8}", "session_<id>", rendered, flags=re.I)
    rendered = re.sub(r"ssion_\d{8}_\d{6}_[0-9a-f]{8}", "ssion_<id>", rendered, flags=re.I)
    rendered = re.sub(r"(?<=just now    )[0-9a-f]{8}(?=  )", "<session>", rendered, flags=re.I)
    rendered = re.sub(r"(?<=Deleted session )[0-9a-f]{8}(?=\.)", "<session>", rendered, flags=re.I)
    rendered = re.sub(r"(?<=Resumed session )[0-9a-f]{8}", "<session>", rendered, flags=re.I)
    rendered = re.sub(
        r"session: [0-9a-f]{8} \(before compaction\) → [0-9a-f]{8} \(after compaction\)",
        "session: <session> (before compaction) → <session> (after compaction)",
        rendered,
        flags=re.I,
    )
    rendered = re.sub(r"^\s*\S+\s+Initializing…$", "", rendered, flags=re.MULTILINE)
    rendered = re.sub(
        r"^.*… \(<duration> Esc/Ctrl\+C to interrupt\)$",
        "<spinner> Loading… (<duration> Esc/Ctrl+C to interrupt)",
        rendered,
        flags=re.MULTILINE,
    )
    rendered = re.sub(
        r"(?m)^ *.*[⡠⢠⢔].*\n *.*⢸.*\n *.*[⠉⠈].*(?=\n\nMistral Vibe)",
        " <petit-chat>\n <petit-chat>\n <petit-chat>",
        rendered,
    )
    rendered = re.sub(
        r"(?m)^ *.*⢸.*\n *.*[⠉⠈].*(?:\s+[▃▄▅])?(?=\n\nMistral Vibe)",
        " <petit-chat>\n <petit-chat>",
        rendered,
    )
    rendered = re.sub(r"\A\n*(?= <petit-chat>)", "\n" * 14, rendered)
    rendered = re.sub(r"(?m)^(  ⎢) +▁$", r"\1", rendered)
    rendered = re.sub(r"(?m)^ *[⠀-⣿]+(?:  +[⠀-⣿]+)+ *$", "", rendered)
    rendered = re.sub(r" *[⠀-⣿▁▂▃▄▅▆▇█]$", "", rendered, flags=re.MULTILINE)
    rendered = re.sub(r"(?m)^─{100,}$", "─" * 120, rendered)
    rendered = re.sub(r"\n{3,}(?=─+ (?:default|plan|auto approve) ─)", "\n\n", rendered)
    rendered = re.sub(r"\n{3,}(?=─{20,} .+ ─)", "\n\n", rendered)
    rendered = re.sub(r">\n\n(?=─{20,})", ">\n\n\n", rendered)
    rendered = re.sub(r"\A\n+(?=> )", "", rendered)
    lines = rendered.splitlines()
    normalized_lines: list[str] = []
    for line in lines:
        if (
            line == ""
            and normalized_lines
            and normalized_lines[-1].endswith("Configure voice settings")
        ):
            continue
        normalized_lines.append(line)
    rendered = "\n".join(normalized_lines).rstrip() + "\n"
    return rendered


def spinner_animation_projection(raw: bytes, status: str, rows: int = 36, cols: int = 120) -> str:
    text = raw.decode("utf-8", "replace")
    screen = Screen(rows, cols)
    frames: list[str] = []
    primary_status = status if status.endswith("…") else f"{status}…"
    statuses = [
        primary_status,
        status,
        "Running command",
        "Writing file",
        "Editing files",
        "Fetching URL",
        "Searching the web",
        "Running subagent",
        "Waiting for user input",
        "Waiting for user confirmation",
        "Réflexion",
        "Analyse",
        "Contemplation",
        "Synthèse",
        "Reading Proust",
        "Oui oui baguette",
        "Counting Rs in strawberry",
        "Vibing",
        "Eating a chocolatine",
        "Eating a pain au chocolat",
        "Petting le chat",
        "Seeding Mistral weights",
        "Sending good vibes",
    ]

    def sample() -> None:
        lines = screen.text().splitlines()
        target_lines = [line for line in lines if primary_status in line]
        if not target_lines:
            target_lines = lines
        for line in target_lines:
            matched = next((candidate for candidate in statuses if candidate in line), None)
            if matched is not None:
                indicator = line.split(matched, 1)[0].strip()
                if indicator and (not frames or frames[-1] != indicator):
                    frames.append(indicator)
                return

    idx = 0
    while idx < len(text):
        if text[idx] == "\x1b":
            csi = CSI_RE.match(text, idx)
            if csi:
                raw_params = csi.group(1)
                private = raw_params[0] if raw_params[:1] in {"?", ">", "=", "<"} else ""
                params = raw_params[1:] if private else raw_params
                screen.csi(private, params, csi.group(3))
                idx = csi.end()
                sample()
                continue
            if text.startswith("\x1b]", idx):
                end_bel = text.find("\x07", idx)
                end_st = text.find("\x1b\\", idx)
                ends = [pos for pos in [end_bel, end_st] if pos != -1]
                if ends:
                    end = min(ends)
                    idx = end + (1 if end == end_bel else 2)
                    sample()
                    continue
            idx += 1
            continue
        screen.put(text[idx])
        idx += 1
        sample()

    distinct: list[str] = []
    for frame in frames:
        if frame not in distinct:
            distinct.append(frame)
    snake_like = any(any("\u2800" <= char <= "\u28ff" for char in frame) for frame in distinct)
    rich_sequence = len(distinct) >= 8
    wide_frames = any(len(frame) >= 2 for frame in distinct)
    snake_grid_like = all(
        1 <= len(frame) <= 2
        and all(char == " " or "\u2800" <= char <= "\u28ff" for char in frame)
        for frame in distinct
    )
    return (
        f"status: {status}\n"
        f"animated: {str(len(distinct) >= 2).lower()}\n"
        f"snake_like: {str(snake_like).lower()}\n"
        f"rich_sequence: {str(rich_sequence).lower()}\n"
        f"wide_frames: {str(wide_frames).lower()}\n"
        f"snake_grid_like: {str(snake_grid_like).lower()}\n"
        f"distinct_frame_count: {min(len(distinct), 8)}\n"
    )


CONFIG_PROJECTION_CASES = {
    "tui_model_select_next",
    "tui_theme_select_next",
    "tui_thinking_select_next",
    "tui_config_toggle_autocopy",
    "tui_config_toggle_autocopy_exit",
    "tui_voice_toggle",
    "tui_voice_toggle_exit",
    "tui_prompt_bash_always",
    "tui_mcp_disable_server",
    "tui_mcp_enable_server",
    "tui_mcp_disable_tool",
    "tui_mcp_enable_tool",
    "acp_set_model_valid",
    "acp_set_model_invalid",
    "acp_set_model_same",
    "acp_set_model_empty",
    "acp_set_config_model",
    "acp_set_config_model_empty",
    "acp_set_config_thinking",
    "acp_set_config_thinking_invalid",
    "acp_set_config_thinking_empty",
    "acp_permission_grep_allow_always_permanent",
    "acp_permission_bash_granular_allow_always_permanent",
}

HISTORY_PROJECTION_CASES = {
    "tui_prompt_history_up",
    "tui_prompt_history_up_down",
    "tui_prompt_history_persisted",
}

EDITOR_PROJECTION_CASES = {
    "tui_external_editor_input",
    "tui_external_editor_empty",
}

PLAN_EDITOR_PROJECTION_CASES = {
    "tui_prompt_exit_plan_editor",
}

CLIPBOARD_PROJECTION_CASES = {
    "tui_copy_last_agent",
    "tui_copy_last_agent_xclip",
}


def config_projection(case_name: str, config: dict[str, object]) -> dict[str, object]:
    if case_name == "tui_model_select_next":
        return {"active_model": config.get("active_model")}
    if case_name in {
        "acp_set_model_valid",
        "acp_set_model_invalid",
        "acp_set_model_same",
        "acp_set_model_empty",
        "acp_set_config_model",
        "acp_set_config_model_empty",
    }:
        return {"active_model": config.get("active_model")}
    if case_name == "tui_theme_select_next":
        return {"theme": config.get("theme")}
    if case_name == "tui_config_toggle_autocopy":
        return {"autocopy_to_clipboard": config.get("autocopy_to_clipboard", True)}
    if case_name == "tui_config_toggle_autocopy_exit":
        return {"autocopy_to_clipboard": config.get("autocopy_to_clipboard", True)}
    if case_name == "tui_voice_toggle":
        return {"voice_mode_enabled": config.get("voice_mode_enabled", False)}
    if case_name == "tui_voice_toggle_exit":
        return {"voice_mode_enabled": config.get("voice_mode_enabled", False)}
    if case_name == "tui_thinking_select_next":
        active = str(config.get("active_model") or "mistral-medium-3.5")
        models = config.get("models")
        if isinstance(models, list):
            for model in models:
                if isinstance(model, dict) and model.get("alias", model.get("name")) == active:
                    return {"thinking": model.get("thinking", "off")}
        return {"thinking": "off"}
    if case_name in {
        "acp_set_config_thinking",
        "acp_set_config_thinking_invalid",
        "acp_set_config_thinking_empty",
    }:
        active = str(config.get("active_model") or "test")
        models = config.get("models")
        if isinstance(models, list):
            for model in models:
                if isinstance(model, dict) and model.get("alias", model.get("name")) == active:
                    return {"thinking": model.get("thinking", "off")}
        return {"thinking": "off"}
    if case_name == "tui_prompt_bash_always":
        return {"tools": config.get("tools", {})}
    if case_name == "acp_permission_grep_allow_always_permanent":
        return {"tools": config.get("tools", {})}
    if case_name == "acp_permission_bash_granular_allow_always_permanent":
        return {"tools": config.get("tools", {})}
    if case_name in {
        "tui_mcp_disable_server",
        "tui_mcp_enable_server",
        "tui_mcp_disable_tool",
        "tui_mcp_enable_tool",
    }:
        servers = config.get("mcp_servers", [])
        if not isinstance(servers, list):
            return {"mcp_servers": []}
        projection = []
        for server in servers:
            if not isinstance(server, dict):
                continue
            disabled_tools = server.get("disabled_tools", [])
            if not isinstance(disabled_tools, list):
                disabled_tools = []
            projection.append(
                {
                    "name": server.get("name"),
                    "disabled": bool(server.get("disabled", False)),
                    "disabled_tools": sorted(str(tool) for tool in disabled_tools),
                }
            )
        return {"mcp_servers": projection}
    return {}


def read_toml(path: pathlib.Path) -> dict[str, object]:
    if not path.exists():
        return {}
    with path.open("rb") as file:
        data = tomllib.load(file)
    return data if isinstance(data, dict) else {}


def config_projection_text(case_name: str, base: pathlib.Path, label: str) -> str:
    if case_name not in CONFIG_PROJECTION_CASES:
        return ""
    if label == "vibe":
        path = base / "vibe" / "home" / ".vibe" / "config.toml"
    else:
        path = base / "microvibe" / "home" / "Library" / "Application Support" / "microvibe" / "config.toml"
        if not path.exists():
            path = base / "microvibe" / "xdg-config" / "microvibe" / "config.toml"
    projection = config_projection(case_name, read_toml(path))
    return "\n<config_projection>" + json.dumps(projection, sort_keys=True) + "</config_projection>\n"


def history_projection_text(case_name: str, base: pathlib.Path, label: str) -> str:
    if case_name not in HISTORY_PROJECTION_CASES:
        return ""
    path = base / label / "home" / ".vibe" / "vibehistory"
    contents = path.read_text(encoding="utf-8") if path.exists() else ""
    projection = {"exists": path.exists(), "contents": contents}
    return "\n<history_projection>" + json.dumps(projection, sort_keys=True) + "</history_projection>\n"


def editor_result_for_case(case_name: str) -> str:
    if case_name in EDITOR_PROJECTION_CASES:
        return "edited from editor"
    return ""


def setup_fake_editor(case_name: str, base: pathlib.Path, label: str, env: dict[str, str]) -> None:
    if case_name not in EDITOR_PROJECTION_CASES | PLAN_EDITOR_PROJECTION_CASES:
        return
    script = base / "fake-editor.sh"
    if not script.exists():
        script.write_text(
            textwrap.dedent(
                """\
                #!/bin/sh
                file="$1"
                {
                  printf 'path=%s\\n' "$file"
                  printf 'initial<<EOF\\n'
                  if [ -f "$file" ]; then
                    cat "$file"
                  fi
                  printf '\\nEOF\\n'
                } >> "$EDITOR_LOG"
                printf '%s\\n' "$EDITOR_RESULT" > "$file"
                """
            ),
            encoding="utf-8",
        )
        script.chmod(0o755)
    env["VISUAL"] = str(script)
    env["EDITOR"] = "should-not-be-used"
    env["EDITOR_LOG"] = str(base / label / "editor.log")
    env["EDITOR_RESULT"] = editor_result_for_case(case_name)


def setup_fake_clipboard(case_name: str, base: pathlib.Path, label: str, env: dict[str, str]) -> None:
    if case_name not in CLIPBOARD_PROJECTION_CASES:
        return
    bin_dir = base / label / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    store = base / label / "clipboard.txt"
    pbcopy = bin_dir / "pbcopy"
    pbcopy.write_text(
        "#!/bin/sh\ncat > \"$MICROVIBE_FAKE_CLIPBOARD\"\n",
        encoding="utf-8",
    )
    pbcopy.chmod(0o755)
    pbpaste = bin_dir / "pbpaste"
    pbpaste.write_text(
        "#!/bin/sh\nif [ -f \"$MICROVIBE_FAKE_CLIPBOARD\" ]; then cat \"$MICROVIBE_FAKE_CLIPBOARD\"; fi\n",
        encoding="utf-8",
    )
    pbpaste.chmod(0o755)
    if case_name == "tui_copy_last_agent_xclip":
        xclip = bin_dir / "xclip"
        xclip.write_text(
            textwrap.dedent(
                """\
                #!/bin/sh
                if [ "$*" = "-selection clipboard -o" ]; then
                  if [ -f "$MICROVIBE_FAKE_CLIPBOARD" ]; then cat "$MICROVIBE_FAKE_CLIPBOARD"; fi
                else
                  cat > "$MICROVIBE_FAKE_CLIPBOARD"
                fi
                """
            ),
            encoding="utf-8",
        )
        xclip.chmod(0o755)
    env["MICROVIBE_FAKE_CLIPBOARD"] = str(store)
    env["PATH"] = f"{bin_dir}{os.pathsep}{env.get('PATH', '')}"


def editor_projection_text(case_name: str, base: pathlib.Path, label: str) -> str:
    if case_name not in EDITOR_PROJECTION_CASES | PLAN_EDITOR_PROJECTION_CASES:
        return ""
    path = base / label / "editor.log"
    raw = path.read_text(encoding="utf-8") if path.exists() else ""
    temp_path = ""
    initial = ""
    if raw.startswith("path="):
        first, _, rest = raw.partition("\n")
        temp_path = first.removeprefix("path=")
        start = "initial<<EOF\n"
        end = "\nEOF\n"
        if rest.startswith(start) and rest.endswith(end):
            initial = rest[len(start) : -len(end)]
    if case_name in PLAN_EDITOR_PROJECTION_CASES:
        projection = {
            "called": path.exists(),
            "initial": initial,
            "plan_path_shape": "/.vibe/plans/" in temp_path
            and pathlib.Path(temp_path).suffix == ".md",
        }
    else:
        projection = {
            "called": path.exists(),
            "initial": initial,
            "temp_name_shape": pathlib.Path(temp_path).name.startswith("vibe_")
            and pathlib.Path(temp_path).suffix == ".md",
        }
    return "\n<editor_projection>" + json.dumps(projection, sort_keys=True) + "</editor_projection>\n"


def clipboard_projection_text(case_name: str, base: pathlib.Path, label: str) -> str:
    if case_name not in CLIPBOARD_PROJECTION_CASES:
        return ""
    path = base / label / "clipboard.txt"
    contents = path.read_text(encoding="utf-8") if path.exists() else ""
    projection = {"exists": path.exists(), "contents": contents}
    return "\n<clipboard_projection>" + json.dumps(projection, sort_keys=True) + "</clipboard_projection>\n"


def raw_projection_text(case_name: str, raw: bytes) -> str:
    if case_name in {"tui_ctrl_c_confirm", "tui_ctrl_d_confirm"}:
        key = "Ctrl+C" if case_name == "tui_ctrl_c_confirm" else "Ctrl+D"
        text = raw.decode("utf-8", "replace")
        projection = {"quit_prompt_seen": f"Press {key} again to quit" in text}
        return "\n<raw_projection>" + json.dumps(projection, sort_keys=True) + "</raw_projection>\n"
    return ""


def debug_console_projection(case_name: str, raw: bytes) -> str:
    text = normalize(raw)
    compact = re.sub(r"\s+", "", text)
    projection = {
        "debug_header": "DebugConsole(ctrl+\\toclose)" in compact,
        "slash_debug": "/debug" in compact,
    }
    if case_name == "tui_debug_ctrl_backslash":
        projection["literal_ctrl_code_leaked"] = "> 4" in text
    return json.dumps(projection, sort_keys=True) + "\n"


def update_prompt_projection(raw: bytes) -> str:
    text = normalize(raw)
    projection = {
        "title": "A new Vibe release is available" in text,
        "version": bool(re.search(r"2\.17\.1\s*→\s*9\.9\.9", text)),
        "update_now": "Update now" in text,
        "cancel_upgrade": "Cancel upgrade" in text,
        "help": "navigate" in text and "Enter select" in text,
    }
    return json.dumps(projection, sort_keys=True) + "\n"


def setup_projection(case_name: str, raw: bytes, base: pathlib.Path, label: str) -> str:
    text = normalize(raw)
    screen = render_screen(raw)
    combined = text + "\n" + screen
    env_path = base / label / "home" / ".vibe" / ".env"
    env_text = env_path.read_text(encoding="utf-8") if env_path.exists() else ""
    projection = {
        "welcome": "Welcome to Mistral Vibe" in combined,
        "enter_hint": "Press Enter" in combined,
        "theme_title": "Select your preferred theme" in combined,
        "theme_nav": "Navigate" in combined and "Press Enter" in combined,
        "preview": "Preview" in combined and "Heading" in combined,
        "auth_title": "Welcome to Mistral Vibe" in combined and "Choose your sign in method" in combined,
        "browser_option": "Launch browser" in combined,
        "manual_option": "Use an API key" in combined,
        "api_key_title": "Get your Mistral API key" in combined,
        "api_key_input": "Paste API key" in combined,
        "cancelled": "Setup cancelled. See you next time!" in combined,
        "complete": "Setup complete" in combined,
        "env_file_exists": env_path.exists(),
        "env_text": env_text,
        "saved_key": "sk-parity-key" in env_text,
        "saved_env_var": "MISTRAL_API_KEY" in env_text,
    }
    expected_keys = {
        "cli_setup_welcome": ["welcome", "enter_hint"],
        "cli_setup_cancel": ["cancelled"],
        "cli_setup_theme": ["theme_title", "theme_nav", "preview"],
        "cli_setup_auth_method": ["auth_title", "browser_option", "manual_option"],
        "cli_setup_api_key": ["api_key_title", "api_key_input"],
        "cli_setup_save_api_key": ["env_file_exists", "env_text"],
    }[case_name]
    filtered = {key: projection[key] for key in expected_keys}
    return json.dumps(filtered, sort_keys=True) + "\n"


def proxy_env_projection(case_name: str, base: pathlib.Path, label: str) -> str:
    env_path = base / label / "home" / ".vibe" / ".env"
    env_text = env_path.read_text(encoding="utf-8") if env_path.exists() else ""
    expected_keys = {
        "tui_proxy_setup_save_http": ["env_file_exists", "env_text"],
        "tui_proxy_setup_preserve_env": ["env_file_exists", "env_text"],
        "tui_proxy_setup_unset_http": ["env_file_exists", "env_text"],
    }[case_name]
    projection = {
        "env_file_exists": env_path.exists(),
        "env_text": env_text,
    }
    filtered = {key: projection[key] for key in expected_keys}
    return "\n<proxy_env_projection>" + json.dumps(filtered, sort_keys=True) + "</proxy_env_projection>\n"


def trust_prompt_projection(raw: bytes) -> str:
    text = normalize(raw)
    projection = {
        "title": "Trust this folder?" in text or "Trust folder or repository?" in text,
        "path": "trust-workspace" in text or "trust-repo" in text,
        "repo_title": "Trust folder or repository?" in text,
        "repo_root": "git repository:" in text,
        "warning": "Only trust folders you fully control" in text,
        "detected": ("Detected in current folder:" in text or "Detected in repository context:" in text) and "AGENTS.md" in text,
        "repo_context": "Detected in repository context:" in text,
        "trust_repo": "Trust full repo" in text,
        "trust_folder": "Trust folder" in text,
        "decline": "Don't trust" in text,
        "help": "navigate" in text and "Enter select" in text,
        "save_info": "trusted_folders.toml" in text,
    }
    return json.dumps(projection, sort_keys=True) + "\n"


def trust_file_projection(case_name: str, base: pathlib.Path, label: str) -> str:
    if case_name not in {"tui_trust_accept", "tui_trust_repo_accept", "tui_trust_repo_decline"}:
        return ""
    path = base / label / "home" / ".vibe" / "trusted_folders.toml"
    raw = path.read_text(encoding="utf-8") if path.exists() else ""
    projection = {
        "file_exists": path.exists(),
        "trusted_workspace": "trust-workspace" in raw,
        "trusted_repo": "trust-repo" in raw and "nested" not in raw,
        "untrusted_nested": "nested" in raw and "untrusted" in raw,
        "has_trusted": "trusted" in raw,
        "has_untrusted": "untrusted" in raw,
    }
    return "\n<trust_projection>" + json.dumps(projection, sort_keys=True) + "</trust_projection>\n"


def request_projection_text(case_name: str, requests: list[dict[str, object]]) -> str:
    if case_name == "tui_prompt_at_image_no_vision":
        return "\n<request_projection>" + json.dumps({"request_count": len(requests)}, sort_keys=True) + "</request_projection>\n"
    if case_name in {"programmatic_hooks_before_json", "programmatic_hooks_after_json", "programmatic_hooks_post_json"}:
        tool_messages: list[str] = []
        tool_call_arguments: list[object] = []
        user_messages_by_request: list[list[dict[str, object]]] = []
        for request in requests:
            raw_messages = request.get("messages")
            if not isinstance(raw_messages, list):
                continue
            request_user_messages: list[dict[str, object]] = []
            for message in raw_messages:
                if not isinstance(message, dict):
                    continue
                if message.get("role") == "user":
                    request_user_messages.append(
                        {
                            "content": message.get("content"),
                            "injected": message.get("injected", False),
                        }
                    )
                if message.get("role") == "assistant":
                    for call in message.get("tool_calls", []) if isinstance(message.get("tool_calls"), list) else []:
                        if not isinstance(call, dict):
                            continue
                        function = call.get("function")
                        if isinstance(function, dict):
                            arguments = function.get("arguments")
                            if isinstance(arguments, str):
                                try:
                                    tool_call_arguments.append(json.loads(arguments))
                                except json.JSONDecodeError:
                                    tool_call_arguments.append(arguments)
                if message.get("role") == "tool" and isinstance(message.get("content"), str):
                    tool_messages.append(
                        re.sub(r"/[^\s\"]*/workspace/(sample|rewritten)\.txt", r"<workspace>/\1.txt", message["content"])
                    )
            user_messages_by_request.append(request_user_messages)
        projection = {
            "tool_call_arguments": tool_call_arguments,
            "tool_messages": tool_messages,
            "user_messages_by_request": user_messages_by_request,
        }
        return "\n<request_projection>" + json.dumps(projection, sort_keys=True, ensure_ascii=False) + "</request_projection>\n"
    if case_name not in {"tui_slash_skill", "tui_prompt_at_file", "tui_completion_path_file", "tui_prompt_at_folder", "tui_prompt_at_image"}:
        return ""
    user_messages: list[str] = []
    multimodal_user_messages: list[dict[str, object]] = []
    for request in requests:
        raw_messages = request.get("messages")
        if not isinstance(raw_messages, list):
            continue
        for message in raw_messages:
            if not isinstance(message, dict) or message.get("role") != "user":
                continue
            content = message.get("content")
            if isinstance(content, str):
                user_messages.append(content)
                multimodal_user_messages.append({"text": content, "image_urls": []})
            elif isinstance(content, list):
                parts = []
                image_urls = []
                for block in content:
                    if isinstance(block, dict):
                        if isinstance(block.get("text"), str):
                            parts.append(block["text"])
                        elif isinstance(block.get("content"), str):
                            parts.append(block["content"])
                        elif block.get("type") == "image_url" and isinstance(block.get("image_url"), dict):
                            url = block["image_url"].get("url")
                            if isinstance(url, str):
                                image_urls.append(url)
                    elif isinstance(block, str):
                        parts.append(block)
                user_messages.append("\n".join(parts))
                multimodal_user_messages.append({"text": "\n".join(parts), "image_urls": image_urls})
    if case_name == "tui_prompt_at_image":
        projection = {"user_messages": multimodal_user_messages}
    if case_name in {"tui_prompt_at_file", "tui_completion_path_file", "tui_prompt_at_folder"}:
        projection = {
            "user_messages": [
                re.sub(
                    r"file://.*/workspace/(sample\.txt|notes)",
                    r"file://<workspace>/\1",
                    message,
                )
                for message in user_messages
            ]
        }
    elif case_name != "tui_prompt_at_image":
        projection = {"user_messages": user_messages}
    return "\n<request_projection>" + json.dumps(projection, sort_keys=True, ensure_ascii=False) + "</request_projection>\n"


def session_projection_text(case_name: str, base: pathlib.Path, label: str) -> str:
    if case_name not in {
        "programmatic_continue_json",
        "programmatic_resume_id_json",
        "tui_bang_bash",
        "tui_compact_one",
        "tui_loop_create",
        "tui_prompt_at_image",
        "tui_resume_delete_one",
        "tui_resume_legacy_json",
        "tui_resume_skips_invalid",
        "tui_resume_same_end_time_mtime",
        "tui_resume_rename_one",
    }:
        return ""
    if label == "vibe":
        root = base / "vibe" / "home" / ".vibe" / "logs" / "session"
    else:
        root = base / "microvibe" / "home" / ".vibe" / "logs" / "session"
    sessions = sorted(root.glob("session_*"))
    if not sessions:
        projection = {"sessions": []}
    else:
        session = None
        metadata = {}
        messages = []
        for candidate in reversed(sessions):
            try:
                candidate_metadata = json.loads((candidate / "meta.json").read_text(encoding="utf-8"))
                messages_path = candidate / "messages.jsonl"
                candidate_messages = [
                    json.loads(line)
                    for line in messages_path.read_text(encoding="utf-8").splitlines()
                    if line.strip()
                ]
            except (OSError, json.JSONDecodeError):
                continue
            if not isinstance(candidate_metadata, dict) or not all(
                isinstance(message, dict) for message in candidate_messages
            ):
                continue
            session = candidate
            metadata = candidate_metadata
            messages = candidate_messages
            break
        if session is None:
            return "\n<session_projection>" + json.dumps({"sessions": []}, sort_keys=True, ensure_ascii=False) + "</session_projection>\n"
        attachment_files = sorted(path.name for path in session.joinpath("attachments").glob("*"))

        def normalized_images(message: dict[str, object]) -> list[dict[str, object]]:
            raw_images = message.get("images")
            if not isinstance(raw_images, list):
                return []
            normalized = []
            for image in raw_images:
                if not isinstance(image, dict):
                    continue
                source = image.get("source")
                if not isinstance(source, dict):
                    source = {}
                raw_path = source.get("path")
                path_name = pathlib.Path(raw_path).name if isinstance(raw_path, str) else None
                normalized.append(
                    {
                        "alias": image.get("alias"),
                        "mime_type": image.get("mime_type"),
                        "source_kind": source.get("kind"),
                        "path_name": path_name,
                    }
                )
            return normalized

        projection = {
            "sessions": [
                {
                    "title": metadata.get("title"),
                    "total_messages": metadata.get("total_messages"),
                    "attachment_files": attachment_files if case_name == "tui_prompt_at_image" else [],
                    "loops": [
                        {
                            "id": "<loop_id>" if loop.get("id") else None,
                            "interval_seconds": loop.get("interval_seconds"),
                            "prompt": loop.get("prompt"),
                            "next_fire_at": "<time>" if loop.get("next_fire_at") else None,
                            "created_at": "<time>" if loop.get("created_at") else None,
                        }
                        for loop in metadata.get("loops", [])
                        if isinstance(loop, dict)
                    ],
                    "messages": [
                        {
                            "role": message.get("role"),
                            "content": message.get("content"),
                            "name": message.get("name"),
                            "tool_call_id": "<tool_call_id>" if message.get("tool_call_id") else None,
                            "images": normalized_images(message) if case_name == "tui_prompt_at_image" else [],
                        }
                        for message in messages
                    ],
                }
            ]
        }
    return "\n<session_projection>" + json.dumps(projection, sort_keys=True, ensure_ascii=False) + "</session_projection>\n"


def ensure_microvibe_built(cmd: list[str]) -> None:
    if os.environ.get("MICROVIBE_PARITY_SKIP_BUILD") == "1":
        return
    exe = shutil.which(cmd[0]) if len(cmd) == 1 else None
    if cmd in (["./target/debug/microvibe"], ["./target/debug/microvibe-acp"]) or pathlib.Path(
        cmd[0]
    ) in {
        ROOT / "target" / "debug" / "microvibe",
        ROOT / "target" / "debug" / "microvibe-acp",
    }:
        subprocess.run(["cargo", "build"], cwd=ROOT, check=True)
        return
    if exe or pathlib.Path(cmd[0]).exists():
        return
    subprocess.run(["cargo", "build"], cwd=ROOT, check=True)


def case_subprocess_timeout(name: str, timeout_scale: float) -> float:
    case = CASES[name]
    if case.mode.startswith("programmatic_"):
        return max(60.0, case.timeout * timeout_scale * 4.0 + 30.0)
    if is_tui_mode(case.mode):
        return max(90.0, case.timeout * timeout_scale * 8.0 + 45.0)
    return max(30.0, case.timeout * timeout_scale * 4.0 + 15.0)


def run_all_case(name: str, update: bool, timeout_scale: float = 1.0) -> tuple[str, int, str]:
    command = [sys.executable, __file__, "--case", name]
    if update:
        command.append("--update")
    env = os.environ.copy()
    if timeout_scale != 1.0:
        env["MICROVIBE_PARITY_TIMEOUT_SCALE"] = str(timeout_scale)
    env["MICROVIBE_PARITY_SKIP_BUILD"] = "1"
    timeout = case_subprocess_timeout(name, timeout_scale)
    process = subprocess.Popen(
        command,
        cwd=ROOT,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        start_new_session=True,
    )
    try:
        stdout, _ = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            stdout, _ = process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            stdout, _ = process.communicate()
        stdout += f"\n<case subprocess timeout after {timeout:.1f}s>\n"
        return name, 124, stdout
    return name, process.returncode, stdout


def run_all_case_with_retry(
    name: str,
    update: bool,
    timeout_scale: float = 1.0,
    attempts: int = 3,
) -> tuple[str, int, str]:
    last_name = name
    last_returncode = 1
    outputs: list[str] = []
    for attempt in range(1, attempts + 1):
        last_name, last_returncode, output = run_all_case(name, update, timeout_scale)
        if attempt > 1:
            outputs.append(f"retry {attempt}/{attempts} for {name}\n")
        outputs.append(output)
        if last_returncode == 0:
            return last_name, last_returncode, "".join(outputs)
        time.sleep(0.5)
    return last_name, last_returncode, "".join(outputs)


def run_case_collection(
    names: list[str],
    *,
    update: bool,
    jobs: int,
    label: str,
) -> int:
    if jobs < 1:
        raise ValueError("jobs must be at least 1")
    subprocess.run(["cargo", "build"], cwd=ROOT, check=True)
    timeout_scale = min(2.0, max(1.0, jobs / 16))
    failed: list[str] = []
    if jobs == 1:
        for name in names:
            print(f"== {name}", flush=True)
            _, returncode, output = run_all_case_with_retry(name, update, timeout_scale)
            print(output, end="")
            if returncode != 0:
                failed.append(name)
    else:
        serial_names = [name for name in names if name in SERIAL_ALL_CASES]
        parallel_names = [name for name in names if name not in SERIAL_ALL_CASES]
        for name in serial_names:
            print(f"== {name}", flush=True)
            _, returncode, output = run_all_case_with_retry(name, update, 1.0)
            print(output, end="")
            if returncode != 0:
                failed.append(name)
        if parallel_names:
            print(
                f"running {len(parallel_names)} parallel {label} parity cases "
                f"with {jobs} workers "
                f"(timeout scale {timeout_scale:g})",
                flush=True,
            )
            with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as executor:
                futures = {
                    executor.submit(run_all_case, name, update, timeout_scale): name
                    for name in parallel_names
                }
                for future in concurrent.futures.as_completed(futures):
                    name, returncode, output = future.result()
                    print(f"== {name}", flush=True)
                    print(output, end="")
                    if returncode != 0:
                        failed.append(name)
        if failed:
            retrying = failed
            failed = []
            print("retrying failed parity cases serially", flush=True)
            for name in retrying:
                print(f"== retry {name}", flush=True)
                _, returncode, output = run_all_case_with_retry(name, update, 1.0)
                print(output, end="")
                if returncode != 0:
                    failed.append(name)
    if failed:
        print("failed parity cases: " + ", ".join(failed), file=sys.stderr)
        return 1
    print(f"{label} {len(names)} parity cases OK")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--case", choices=sorted(CASES))
    parser.add_argument("--mode", choices=["repl", "tui", "default_tui"])
    parser.add_argument("--update", action="store_true")
    parser.add_argument("--all", action="store_true", help="run every parity case")
    parser.add_argument("--tier", choices=["fast", "smoke"], help="run a curated parity subset")
    parser.add_argument("--jobs", type=int, default=32, help="parallel workers for --all/--tier")
    args = parser.parse_args()

    if args.all and args.tier:
        parser.error("--all cannot be combined with --tier")
    if args.mode and (args.all or args.tier):
        parser.error("--mode can only be used with --case")
    if args.case and (args.all or args.tier):
        parser.error("--case cannot be combined with --all or --tier")

    if args.all:
        return run_case_collection(list(CASES), update=args.update, jobs=args.jobs, label="all")
    if args.tier == "fast":
        return run_case_collection(FAST_CASES, update=args.update, jobs=args.jobs, label="fast")
    if args.tier == "smoke":
        return run_case_collection(SMOKE_CASES, update=args.update, jobs=args.jobs, label="smoke")

    case = CASES[args.case or "startup"]
    if args.mode:
        case = Case(case.name, args.mode, case.input_text, case.settle, case.timeout)
    timeout_scale = float(os.environ.get("MICROVIBE_PARITY_TIMEOUT_SCALE", "1"))
    if timeout_scale != 1:
        case = Case(
            case.name,
            case.mode,
            case.input_text,
            case.settle,
            case.timeout * timeout_scale,
        )

    vibe = resolve_uv_project_args(command_from_env("VIBE_CMD", default_vibe_command()))
    microvibe = command_from_env("MICROVIBE_CMD", "./target/debug/microvibe")
    vibe_acp = resolve_uv_project_args(command_from_env("VIBE_ACP_CMD", default_vibe_acp_command()))
    microvibe_acp = command_from_env("MICROVIBE_ACP_CMD", "./target/debug/microvibe-acp")
    ensure_microvibe_built(microvibe)
    ensure_microvibe_built(microvibe_acp)
    microvibe = resolve_command(microvibe)
    microvibe_acp = resolve_command(microvibe_acp)

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    vibe_config_text = ""
    micro_config_text = ""
    vibe_history_text = ""
    micro_history_text = ""
    vibe_editor_text = ""
    micro_editor_text = ""
    vibe_clipboard_text = ""
    micro_clipboard_text = ""
    vibe_side_effect_text = ""
    micro_side_effect_text = ""
    setup_vibe_text: str | None = None
    setup_micro_text: str | None = None
    proxy_vibe_text: str | None = None
    proxy_micro_text: str | None = None
    with tempfile.TemporaryDirectory(prefix="microvibe-parity-") as tmp:
        tmp_path = pathlib.Path(tmp)
        if case.mode.startswith("acp_"):
            vibe_env = isolated_env("vibe", tmp_path)
            micro_env = isolated_env("microvibe", tmp_path)
            vibe_env["VIBE_HOME"] = str(tmp_path / "vibe" / "home" / ".vibe")
            micro_env["VIBE_HOME"] = str(tmp_path / "microvibe" / "home" / ".vibe")
            workspace = tmp_path / "workspace"
            workspace.mkdir(parents=True, exist_ok=True)
            if case.mode.startswith("acp_trust_"):
                if case.mode in {"acp_trust_status_repo", "acp_trust_decision_repo"}:
                    repo_root = tmp_path / "trust-repo"
                    workspace = repo_root / "nested"
                    (repo_root / ".git").mkdir(parents=True, exist_ok=True)
                    (repo_root / ".git" / "HEAD").write_text("ref: refs/heads/main\n", encoding="utf-8")
                    workspace.mkdir(parents=True, exist_ok=True)
                    (repo_root / "AGENTS.md").write_text("repo trust instructions\n", encoding="utf-8")
                (workspace / "AGENTS.md").write_text("trust parity instructions\n", encoding="utf-8")
            if case.mode.startswith("acp_auth_"):
                vibe_env["PYTHON_KEYRING_BACKEND"] = "keyring.backends.fail.Keyring"
                micro_env["PYTHON_KEYRING_BACKEND"] = "keyring.backends.fail.Keyring"
                if case.mode in {"acp_auth_status_signed_out", "acp_auth_status_dotenv", "acp_auth_signout_dotenv"}:
                    vibe_env.pop("MISTRAL_API_KEY", None)
                    micro_env.pop("MISTRAL_API_KEY", None)
                if case.mode in {"acp_auth_status_process_env", "acp_auth_status_process_over_dotenv", "acp_auth_signout_process_over_dotenv"}:
                    vibe_env["MISTRAL_API_KEY"] = "sk-process-env"
                    micro_env["MISTRAL_API_KEY"] = "sk-process-env"
                if case.mode in {"acp_auth_status_dotenv", "acp_auth_signout_dotenv", "acp_auth_status_process_over_dotenv", "acp_auth_signout_process_over_dotenv"}:
                    seed_acp_dotenv(pathlib.Path(vibe_env["VIBE_HOME"]))
                    seed_acp_dotenv(pathlib.Path(micro_env["VIBE_HOME"]))
            if case.mode in {"acp_initialize_unsupported_provider", "acp_authenticate_browser_unsupported"}:
                vibe_env.pop("MISTRAL_API_KEY", None)
                micro_env.pop("MISTRAL_API_KEY", None)
                write_acp_unsupported_provider_configs(tmp_path)
            if case.mode in {
                "acp_authenticate_browser_complete",
                "acp_authenticate_browser_unsupported_action",
                "acp_initialize_delegated_browser_auth",
                "acp_authenticate_delegated_start",
                "acp_authenticate_delegated_complete",
                "acp_authenticate_delegated_missing_attempt",
                "acp_authenticate_delegated_unknown_attempt",
                "acp_authenticate_delegated_unsupported_action",
            }:
                vibe_env["PYTHON_KEYRING_BACKEND"] = "keyring.backends.fail.Keyring"
                micro_env["PYTHON_KEYRING_BACKEND"] = "keyring.backends.fail.Keyring"
                vibe_env.pop("MISTRAL_API_KEY", None)
                micro_env.pop("MISTRAL_API_KEY", None)
                if case.mode == "acp_authenticate_browser_complete":
                    install_browser_opener(vibe_env, tmp_path, "vibe")
                    install_browser_opener(micro_env, tmp_path, "microvibe")
                with ThreadingTCPServer(("127.0.0.1", 0), FakeBrowserAuthHandler) as server:
                    port = int(server.server_address[1])
                    FakeBrowserAuthHandler.requests = []
                    FakeBrowserAuthHandler.poll_status = "completed"
                    write_acp_browser_auth_configs(tmp_path, f"http://127.0.0.1:{port}")
                    thread = threading.Thread(target=server.serve_forever, daemon=True)
                    thread.start()
                    if case.mode == "acp_authenticate_browser_complete":
                        vibe_raw = run_acp_authenticate_browser_complete(vibe_acp, vibe_env, ROOT)
                    elif case.mode == "acp_authenticate_browser_unsupported_action":
                        vibe_raw = run_acp_authenticate_browser_unsupported_action(vibe_acp, vibe_env, ROOT)
                    elif case.mode == "acp_initialize_delegated_browser_auth":
                        vibe_raw = run_acp_initialize_delegated_browser_auth(vibe_acp, vibe_env, ROOT)
                    elif case.mode == "acp_authenticate_delegated_start":
                        vibe_raw = run_acp_authenticate_delegated_start(vibe_acp, vibe_env, ROOT)
                    elif case.mode == "acp_authenticate_delegated_complete":
                        vibe_raw = run_acp_authenticate_delegated_complete(vibe_acp, vibe_env, ROOT)
                    elif case.mode == "acp_authenticate_delegated_missing_attempt":
                        vibe_raw = run_acp_authenticate_delegated_missing_attempt(vibe_acp, vibe_env, ROOT)
                    elif case.mode == "acp_authenticate_delegated_unknown_attempt":
                        vibe_raw = run_acp_authenticate_delegated_unknown_attempt(vibe_acp, vibe_env, ROOT)
                    else:
                        vibe_raw = run_acp_authenticate_delegated_unsupported_action(vibe_acp, vibe_env, ROOT)
                    FakeBrowserAuthHandler.requests = []
                    if case.mode == "acp_authenticate_browser_complete":
                        micro_raw = run_acp_authenticate_browser_complete(microvibe_acp, micro_env, ROOT)
                    elif case.mode == "acp_authenticate_browser_unsupported_action":
                        micro_raw = run_acp_authenticate_browser_unsupported_action(microvibe_acp, micro_env, ROOT)
                    elif case.mode == "acp_initialize_delegated_browser_auth":
                        micro_raw = run_acp_initialize_delegated_browser_auth(microvibe_acp, micro_env, ROOT)
                    elif case.mode == "acp_authenticate_delegated_start":
                        micro_raw = run_acp_authenticate_delegated_start(microvibe_acp, micro_env, ROOT)
                    elif case.mode == "acp_authenticate_delegated_complete":
                        micro_raw = run_acp_authenticate_delegated_complete(microvibe_acp, micro_env, ROOT)
                    elif case.mode == "acp_authenticate_delegated_missing_attempt":
                        micro_raw = run_acp_authenticate_delegated_missing_attempt(microvibe_acp, micro_env, ROOT)
                    elif case.mode == "acp_authenticate_delegated_unknown_attempt":
                        micro_raw = run_acp_authenticate_delegated_unknown_attempt(microvibe_acp, micro_env, ROOT)
                    else:
                        micro_raw = run_acp_authenticate_delegated_unsupported_action(microvibe_acp, micro_env, ROOT)
                    server.shutdown()
            if case.mode in {
                "acp_set_mode_valid",
                "acp_set_mode_invalid",
                "acp_set_model_valid",
                "acp_set_model_invalid",
                "acp_set_model_same",
                "acp_set_model_empty",
                "acp_set_mode_fork_default",
                "acp_set_mode_fork_auto_approve",
                "acp_set_mode_fork_plan",
                "acp_set_mode_fork_accept_edits",
                "acp_set_mode_fork_chat",
                "acp_set_mode_fork_invalid",
                "acp_set_mode_fork_empty",
                "acp_set_config_mode",
                "acp_set_config_mode_empty",
                "acp_set_config_model",
                "acp_set_config_model_empty",
                "acp_set_config_thinking",
                "acp_set_config_thinking_invalid",
                "acp_set_config_thinking_empty",
                "acp_set_config_max_turns",
                "acp_set_config_max_turns_invalid",
                "acp_set_config_max_turns_bool",
                "acp_set_config_invalid_id",
                "acp_set_config_empty_id",
            }:
                write_session_configs(tmp_path, 9, extra_model=True)
            if case.mode in {"acp_prompt_simple", "acp_prompt_client_message_id", "acp_prompt_agent_thought", "acp_prompt_usage_accumulates", "acp_prompt_usage_cost", "acp_prompt_image", "acp_prompt_image_wrong_type", "acp_prompt_image_invalid_base64", "acp_command_help", "acp_command_reload", "acp_command_compact_empty", "acp_command_compact_one", "acp_command_teleport_no_history", "acp_command_data_retention", "acp_command_proxy_help", "acp_command_proxy_set", "acp_command_proxy_unset", "acp_command_proxy_invalid", "acp_command_proxy_case", "acp_prompt_grep", "acp_permission_grep_allow", "acp_permission_grep_deny", "acp_permission_grep_cancelled", "acp_permission_grep_allow_always", "acp_permission_grep_allow_always_permanent", "acp_permission_bash_granular", "acp_permission_bash_granular_allow_always_permanent", "acp_fs_read", "acp_fs_read_range", "acp_fs_write", "acp_fs_edit", "acp_terminal_bash_allow", "acp_terminal_bash_nonzero", "acp_terminal_bash_none_exit", "acp_terminal_bash_timeout", "acp_tool_meta_web_fetch", "acp_tool_meta_web_search", "acp_tool_meta_skill", "acp_tool_meta_task", "acp_prompt_todo", "acp_prompt_todo_invalid", "acp_fork_from_prompt_message", "acp_user_display_content"}:
                with ThreadingTCPServer(("127.0.0.1", 0), FakeChatHandler) as server:
                    port = int(server.server_address[1])
                    vibe_env["VIBE_PARITY_MISTRAL_SERVER_URL"] = f"http://127.0.0.1:{port}"
                    if case.mode in {"acp_prompt_grep", "acp_permission_grep_allow", "acp_permission_grep_deny", "acp_permission_grep_cancelled", "acp_permission_grep_allow_always", "acp_permission_grep_allow_always_permanent"}:
                        (workspace / "auth.txt").write_text("auth token\nnope\n", encoding="utf-8")
                        final_content = {
                            "acp_permission_grep_deny": "The search for 'auth' has not been performed, because you rejected the permission request",
                            "acp_permission_grep_cancelled": "The search for 'auth' has not been performed, because you cancelled the permission request",
                        }.get(case.mode, "Found auth")
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_grep_123",
                                "grep",
                                {
                                    "pattern": "auth",
                                    "path": ".",
                                    "max_matches": None,
                                    "use_default_ignore": True,
                                },
                            ),
                            {
                                "id": "chatcmpl_acp_prompt_final",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": final_content},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6},
                            },
                        ]
                        if case.mode == "acp_permission_grep_allow_always":
                            FakeChatHandler.responses.extend(
                                [
                                    tool_response(
                                        "call_grep_again_123",
                                        "grep",
                                        {
                                            "pattern": "auth",
                                            "path": ".",
                                            "max_matches": None,
                                            "use_default_ignore": True,
                                        },
                                    ),
                                    {
                                        "id": "chatcmpl_acp_prompt_final_again",
                                        "object": "chat.completion",
                                        "created": 0,
                                        "model": "test-model",
                                        "choices": [
                                            {
                                                "index": 0,
                                                "message": {"role": "assistant", "content": "Found auth again"},
                                                "finish_reason": "stop",
                                            }
                                        ],
                                        "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6},
                                    },
                                ]
                            )
                    elif case.mode in {"acp_terminal_bash_allow", "acp_terminal_bash_nonzero", "acp_terminal_bash_none_exit", "acp_terminal_bash_timeout"}:
                        final_content = {
                            "acp_terminal_bash_allow": "Ran terminal bash",
                            "acp_terminal_bash_nonzero": "Terminal bash failed",
                            "acp_terminal_bash_none_exit": "Terminal bash none exit",
                            "acp_terminal_bash_timeout": "Terminal bash timed out",
                        }[case.mode]
                        bash_args = {"command": "printf bash-parity"}
                        if case.mode == "acp_terminal_bash_timeout":
                            bash_args["timeout"] = 1
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_bash_123",
                                "bash",
                                bash_args,
                            ),
                            {
                                "id": "chatcmpl_acp_bash_final",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": final_content},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
                            },
                        ]
                    elif case.mode in {"acp_permission_bash_granular", "acp_permission_bash_granular_allow_always_permanent"}:
                        bash_command = "npm install --help" if case.mode == "acp_permission_bash_granular_allow_always_permanent" else "npm install foo"
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_bash_granular_123",
                                "bash",
                                {"command": bash_command},
                            ),
                            {
                                "id": "chatcmpl_acp_bash_granular_final",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "Installed"},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
                            },
                        ]
                    elif case.mode == "acp_tool_meta_web_fetch":
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_web_fetch_meta_123",
                                "web_fetch",
                                {"url": f"http://127.0.0.1:{port}/fetch.txt"},
                            ),
                            {
                                "id": "chatcmpl_acp_web_fetch_meta_final",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "Fetched metadata"},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
                            },
                        ]
                    elif case.mode == "acp_tool_meta_web_search":
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_web_search_meta_123",
                                "web_search",
                                {"query": "parity search query"},
                            ),
                            {
                                "id": "chatcmpl_acp_web_search_meta_final",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "Searched metadata"},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
                            },
                        ]
                    elif case.mode == "acp_tool_meta_skill":
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_skill_meta_123",
                                "skill",
                                {"name": "parity-skill"},
                            ),
                            {
                                "id": "chatcmpl_acp_skill_meta_final",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "Skill metadata"},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
                            },
                        ]
                    elif case.mode == "acp_tool_meta_task":
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_task_meta_123",
                                "task",
                                {"task": "Inspect metadata parity", "agent": "explore"},
                            ),
                            {
                                "id": "chatcmpl_acp_task_meta_subagent",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "Subagent inspected metadata."},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 4, "completion_tokens": 3, "total_tokens": 7},
                            },
                            {
                                "id": "chatcmpl_acp_task_meta_final",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "Task metadata"},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
                            },
                        ]
                    elif case.mode == "acp_prompt_todo":
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_todo_acp_123",
                                "todo",
                                {
                                    "action": "write",
                                    "todos": [
                                        {
                                            "id": "1",
                                            "content": "First",
                                            "status": "in_progress",
                                            "priority": "high",
                                        },
                                        {
                                            "id": "2",
                                            "content": "Second",
                                            "status": "pending",
                                            "priority": "medium",
                                        },
                                    ],
                                },
                            ),
                            {
                                "id": "chatcmpl_acp_todo_final",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "Todo updated"},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
                            },
                        ]
                    elif case.mode == "acp_prompt_todo_invalid":
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_todo_acp_invalid_123",
                                "todo",
                                {
                                    "action": "write",
                                    "todos": [
                                        {"id": "1", "content": "First", "status": "pending"},
                                        {"id": "1", "content": "Duplicate", "status": "pending"},
                                    ],
                                },
                            ),
                            {
                                "id": "chatcmpl_acp_todo_invalid_final",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "Todo invalid"},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
                            },
                        ]
                    elif case.mode in {"acp_fs_read", "acp_fs_read_range"}:
                        read_file = workspace / "client-read.txt"
                        read_file.write_text("", encoding="utf-8")
                        read_args = {
                            "file_path": str(read_file),
                            "offset": None,
                            "limit": 2_000,
                        }
                        if case.mode == "acp_fs_read_range":
                            read_args["offset"] = 10
                            read_args["limit"] = 20
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_read_123",
                                "read",
                                read_args,
                            ),
                            {
                                "id": "chatcmpl_acp_read_final",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "Read client file"},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
                            },
                        ]
                    elif case.mode == "acp_fs_write":
                        write_file = workspace / "client-write.txt"
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_write_123",
                                "write_file",
                                {
                                    "path": str(write_file),
                                    "content": "client alpha\nclient beta\n",
                                },
                            ),
                            {
                                "id": "chatcmpl_acp_write_final",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "Wrote client file"},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
                            },
                        ]
                    elif case.mode == "acp_fs_edit":
                        edit_file = workspace / "client-edit.txt"
                        edit_file.write_text("", encoding="utf-8")
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_edit_123",
                                "edit",
                                {
                                    "file_path": str(edit_file),
                                    "old_string": "client beta",
                                    "new_string": "client gamma",
                                    "replace_all": False,
                                },
                            ),
                            {
                                "id": "chatcmpl_acp_edit_final",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "Edited client file"},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
                            },
                        ]
                    elif case.mode == "acp_fork_from_prompt_message":
                        FakeChatHandler.responses = [
                            {
                                "id": "chatcmpl_acp_fork_prompt",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "Forkable reply"},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
                            }
                        ]
                    elif case.mode == "acp_prompt_agent_thought":
                        FakeChatHandler.responses = [
                            {
                                "id": "chatcmpl_acp_agent_thought",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {
                                            "role": "assistant",
                                            "reasoning_content": "Let me think about this...",
                                            "content": "Hi",
                                        },
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
                            }
                        ]
                    elif case.mode == "acp_prompt_usage_accumulates":
                        FakeChatHandler.responses = [
                            {
                                "id": "chatcmpl_acp_usage_first",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "First usage reply"},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
                            },
                            {
                                "id": "chatcmpl_acp_usage_second",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "Second usage reply"},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
                            },
                        ]
                    elif case.mode == "acp_prompt_usage_cost":
                        FakeChatHandler.responses = [
                            {
                                "id": "chatcmpl_acp_usage_cost_first",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "First cost usage reply"},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 1_000, "completion_tokens": 500, "total_tokens": 1_500},
                            },
                            {
                                "id": "chatcmpl_acp_usage_cost_second",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "Second cost usage reply"},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
                            },
                        ]
                    elif case.mode == "acp_command_compact_one":
                        FakeChatHandler.responses = [
                            {
                                "id": "chatcmpl_acp_compact_seed",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "Compactable reply"},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
                            },
                            {
                                "id": "chatcmpl_acp_compact_summary",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "compact summary"},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
                            },
                        ]
                    elif case.mode == "acp_prompt_image":
                        FakeChatHandler.responses = [{"__dynamic_image_echo": True}]
                    else:
                        FakeChatHandler.responses = [
                            {
                                "id": "chatcmpl_acp_prompt",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "test-model",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "Hi"},
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
                            }
                        ]
                    FakeChatHandler.next_response = 0
                    FakeChatHandler.requests = []
                    thread = threading.Thread(target=server.serve_forever, daemon=True)
                    thread.start()
                    write_session_configs(
                        tmp_path,
                        port,
                        supports_images=case.mode == "acp_prompt_image",
                        input_price=400.0 if case.mode == "acp_prompt_usage_cost" else 0.0,
                        output_price=2000.0 if case.mode == "acp_prompt_usage_cost" else 0.0,
                    )
                    if case.mode in {"acp_permission_grep_allow", "acp_permission_grep_deny", "acp_permission_grep_cancelled", "acp_permission_grep_allow_always", "acp_permission_grep_allow_always_permanent"}:
                        seed_grep_ask(tmp_path)
                    if case.mode == "acp_prompt_grep":
                        vibe_raw = run_acp_prompt_grep(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_permission_grep_allow":
                        vibe_raw = run_acp_prompt_permission(vibe_acp, vibe_env, ROOT, workspace, "allow_once")
                    elif case.mode == "acp_permission_grep_deny":
                        vibe_raw = run_acp_prompt_permission(vibe_acp, vibe_env, ROOT, workspace, "reject_once")
                    elif case.mode == "acp_permission_grep_cancelled":
                        vibe_raw = run_acp_prompt_permission_cancelled(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_permission_grep_allow_always":
                        vibe_raw = run_acp_prompt_permission_allow_always(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_permission_grep_allow_always_permanent":
                        vibe_raw = run_acp_prompt_permission_allow_always_permanent(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_permission_bash_granular":
                        vibe_raw = run_acp_permission_bash_granular(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_permission_bash_granular_allow_always_permanent":
                        vibe_raw = run_acp_permission_bash_granular_allow_always_permanent(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode in {"acp_fs_read", "acp_fs_read_range"}:
                        vibe_raw = run_acp_prompt_fs_read(vibe_acp, vibe_env, ROOT, workspace, "client alpha\nclient beta\n")
                    elif case.mode == "acp_fs_write":
                        vibe_raw = run_acp_prompt_fs_write(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_fs_edit":
                        vibe_raw = run_acp_prompt_fs_edit(vibe_acp, vibe_env, ROOT, workspace, "client alpha\nclient beta\n")
                    elif case.mode == "acp_terminal_bash_allow":
                        vibe_raw = run_acp_prompt_terminal_bash(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_terminal_bash_nonzero":
                        vibe_raw = run_acp_prompt_terminal_bash(
                            vibe_acp,
                            vibe_env,
                            ROOT,
                            workspace,
                            prompt="Run terminal bash nonzero",
                            terminal_output="error: command failed",
                            exit_code=1,
                        )
                    elif case.mode == "acp_terminal_bash_none_exit":
                        vibe_raw = run_acp_prompt_terminal_bash(
                            vibe_acp,
                            vibe_env,
                            ROOT,
                            workspace,
                            prompt="Run terminal bash none exit",
                            terminal_output="bash-parity",
                            exit_code=None,
                        )
                    elif case.mode == "acp_terminal_bash_timeout":
                        vibe_raw = run_acp_prompt_terminal_bash(
                            vibe_acp,
                            vibe_env,
                            ROOT,
                            workspace,
                            prompt="Run terminal bash timeout",
                            wait_timeout=True,
                        )
                    elif case.mode == "acp_fork_from_prompt_message":
                        vibe_raw = run_acp_fork_from_prompt_message(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_user_display_content":
                        vibe_raw = run_acp_user_display_content(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_prompt_image":
                        vibe_raw = run_acp_prompt_image(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_prompt_image_wrong_type":
                        vibe_raw = run_acp_prompt_image_wrong_type(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_prompt_image_invalid_base64":
                        vibe_raw = run_acp_prompt_image_invalid_base64(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_prompt_client_message_id":
                        vibe_raw = run_acp_prompt_client_message_id(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_prompt_agent_thought":
                        vibe_raw = run_acp_prompt_agent_thought(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_prompt_usage_accumulates":
                        vibe_raw = run_acp_prompt_usage_accumulates(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_prompt_usage_cost":
                        vibe_raw = run_acp_prompt_usage_cost(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_command_help":
                        vibe_raw = run_acp_command_help(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_command_reload":
                        vibe_raw = run_acp_command_reload(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_command_compact_empty":
                        vibe_raw = run_acp_command_compact_empty(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_command_compact_one":
                        vibe_raw = run_acp_command_compact_one(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_command_teleport_no_history":
                        vibe_raw = run_acp_command_teleport_no_history(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_command_data_retention":
                        vibe_raw = run_acp_command_data_retention(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_command_proxy_help":
                        vibe_raw = run_acp_command_proxy_help(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_command_proxy_set":
                        vibe_raw = run_acp_command_proxy_set(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_command_proxy_unset":
                        vibe_raw = run_acp_command_proxy_unset(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_command_proxy_invalid":
                        vibe_raw = run_acp_command_proxy_invalid(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_command_proxy_case":
                        vibe_raw = run_acp_command_proxy_case(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_tool_meta_web_fetch":
                        vibe_raw = run_acp_tool_meta_web_fetch(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_tool_meta_web_search":
                        vibe_raw = run_acp_tool_meta_web_search(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_tool_meta_skill":
                        vibe_raw = run_acp_tool_meta_skill(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_tool_meta_task":
                        vibe_raw = run_acp_tool_meta_task(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_prompt_todo":
                        vibe_raw = run_acp_prompt_todo(vibe_acp, vibe_env, ROOT, workspace)
                    elif case.mode == "acp_prompt_todo_invalid":
                        vibe_raw = run_acp_prompt_todo_invalid(vibe_acp, vibe_env, ROOT, workspace)
                    else:
                        vibe_raw = run_acp_prompt_simple(vibe_acp, vibe_env, ROOT, workspace)
                    FakeChatHandler.next_response = 0
                    FakeChatHandler.requests = []
                    if case.mode == "acp_prompt_grep":
                        micro_raw = run_acp_prompt_grep(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_permission_grep_allow":
                        micro_raw = run_acp_prompt_permission(microvibe_acp, micro_env, ROOT, workspace, "allow_once")
                    elif case.mode == "acp_permission_grep_deny":
                        micro_raw = run_acp_prompt_permission(microvibe_acp, micro_env, ROOT, workspace, "reject_once")
                    elif case.mode == "acp_permission_grep_cancelled":
                        micro_raw = run_acp_prompt_permission_cancelled(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_permission_grep_allow_always":
                        micro_raw = run_acp_prompt_permission_allow_always(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_permission_grep_allow_always_permanent":
                        micro_raw = run_acp_prompt_permission_allow_always_permanent(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_permission_bash_granular":
                        micro_raw = run_acp_permission_bash_granular(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_permission_bash_granular_allow_always_permanent":
                        micro_raw = run_acp_permission_bash_granular_allow_always_permanent(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode in {"acp_fs_read", "acp_fs_read_range"}:
                        micro_raw = run_acp_prompt_fs_read(microvibe_acp, micro_env, ROOT, workspace, "client alpha\nclient beta\n")
                    elif case.mode == "acp_fs_write":
                        micro_raw = run_acp_prompt_fs_write(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_fs_edit":
                        micro_raw = run_acp_prompt_fs_edit(microvibe_acp, micro_env, ROOT, workspace, "client alpha\nclient beta\n")
                    elif case.mode == "acp_terminal_bash_allow":
                        micro_raw = run_acp_prompt_terminal_bash(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_terminal_bash_nonzero":
                        micro_raw = run_acp_prompt_terminal_bash(
                            microvibe_acp,
                            micro_env,
                            ROOT,
                            workspace,
                            prompt="Run terminal bash nonzero",
                            terminal_output="error: command failed",
                            exit_code=1,
                        )
                    elif case.mode == "acp_terminal_bash_none_exit":
                        micro_raw = run_acp_prompt_terminal_bash(
                            microvibe_acp,
                            micro_env,
                            ROOT,
                            workspace,
                            prompt="Run terminal bash none exit",
                            terminal_output="bash-parity",
                            exit_code=None,
                        )
                    elif case.mode == "acp_terminal_bash_timeout":
                        micro_raw = run_acp_prompt_terminal_bash(
                            microvibe_acp,
                            micro_env,
                            ROOT,
                            workspace,
                            prompt="Run terminal bash timeout",
                            wait_timeout=True,
                        )
                    elif case.mode == "acp_fork_from_prompt_message":
                        micro_raw = run_acp_fork_from_prompt_message(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_user_display_content":
                        micro_raw = run_acp_user_display_content(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_prompt_image":
                        micro_raw = run_acp_prompt_image(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_prompt_image_wrong_type":
                        micro_raw = run_acp_prompt_image_wrong_type(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_prompt_image_invalid_base64":
                        micro_raw = run_acp_prompt_image_invalid_base64(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_prompt_client_message_id":
                        micro_raw = run_acp_prompt_client_message_id(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_prompt_agent_thought":
                        micro_raw = run_acp_prompt_agent_thought(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_prompt_usage_accumulates":
                        micro_raw = run_acp_prompt_usage_accumulates(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_prompt_usage_cost":
                        micro_raw = run_acp_prompt_usage_cost(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_command_help":
                        micro_raw = run_acp_command_help(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_command_reload":
                        micro_raw = run_acp_command_reload(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_command_compact_empty":
                        micro_raw = run_acp_command_compact_empty(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_command_compact_one":
                        micro_raw = run_acp_command_compact_one(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_command_teleport_no_history":
                        micro_raw = run_acp_command_teleport_no_history(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_command_data_retention":
                        micro_raw = run_acp_command_data_retention(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_command_proxy_help":
                        micro_raw = run_acp_command_proxy_help(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_command_proxy_set":
                        micro_raw = run_acp_command_proxy_set(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_command_proxy_unset":
                        micro_raw = run_acp_command_proxy_unset(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_command_proxy_invalid":
                        micro_raw = run_acp_command_proxy_invalid(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_command_proxy_case":
                        micro_raw = run_acp_command_proxy_case(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_tool_meta_web_fetch":
                        micro_raw = run_acp_tool_meta_web_fetch(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_tool_meta_web_search":
                        micro_raw = run_acp_tool_meta_web_search(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_tool_meta_skill":
                        micro_raw = run_acp_tool_meta_skill(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_tool_meta_task":
                        micro_raw = run_acp_tool_meta_task(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_prompt_todo":
                        micro_raw = run_acp_prompt_todo(microvibe_acp, micro_env, ROOT, workspace)
                    elif case.mode == "acp_prompt_todo_invalid":
                        micro_raw = run_acp_prompt_todo_invalid(microvibe_acp, micro_env, ROOT, workspace)
                    else:
                        micro_raw = run_acp_prompt_simple(microvibe_acp, micro_env, ROOT, workspace)
                    server.shutdown()
            if case.mode == "acp_list_sessions_seeded":
                seed_acp_saved_sessions(pathlib.Path(vibe_env["VIBE_HOME"]))
                seed_acp_saved_sessions(pathlib.Path(micro_env["VIBE_HOME"]))
            if case.mode == "acp_list_sessions_cwd_filter":
                project1 = tmp_path / "project1"
                project2 = tmp_path / "project2"
                project1.mkdir(parents=True, exist_ok=True)
                project2.mkdir(parents=True, exist_ok=True)
                seed_acp_cwd_filter_sessions(pathlib.Path(vibe_env["VIBE_HOME"]), project1, project2)
                seed_acp_cwd_filter_sessions(pathlib.Path(micro_env["VIBE_HOME"]), project1, project2)
            if case.mode == "acp_list_sessions_sorted":
                seed_acp_sorted_sessions(pathlib.Path(vibe_env["VIBE_HOME"]), workspace)
                seed_acp_sorted_sessions(pathlib.Path(micro_env["VIBE_HOME"]), workspace)
            if case.mode == "acp_list_sessions_skip_invalid":
                seed_acp_invalid_list_sessions(pathlib.Path(vibe_env["VIBE_HOME"]), workspace)
                seed_acp_invalid_list_sessions(pathlib.Path(micro_env["VIBE_HOME"]), workspace)
            if case.mode == "acp_list_sessions_timestamps":
                seed_acp_timestamp_sessions(pathlib.Path(vibe_env["VIBE_HOME"]), workspace)
                seed_acp_timestamp_sessions(pathlib.Path(micro_env["VIBE_HOME"]), workspace)
            if case.mode == "acp_load_session":
                seed_acp_load_session(pathlib.Path(vibe_env["VIBE_HOME"]), workspace)
                seed_acp_load_session(pathlib.Path(micro_env["VIBE_HOME"]), workspace)
            if case.mode == "acp_load_rich_session":
                seed_acp_rich_load_session(pathlib.Path(vibe_env["VIBE_HOME"]), workspace)
                seed_acp_rich_load_session(pathlib.Path(micro_env["VIBE_HOME"]), workspace)
            if case.mode == "acp_load_replay_ids":
                seed_acp_load_replay_ids(pathlib.Path(vibe_env["VIBE_HOME"]), workspace)
                seed_acp_load_replay_ids(pathlib.Path(micro_env["VIBE_HOME"]), workspace)
            if case.mode == "acp_set_title_saved":
                seed_acp_single_saved_session(pathlib.Path(vibe_env["VIBE_HOME"]), "titlesaved-12345678", workspace)
                seed_acp_single_saved_session(pathlib.Path(micro_env["VIBE_HOME"]), "titlesaved-12345678", workspace)
            if case.mode == "acp_delete_saved":
                seed_acp_single_saved_session(pathlib.Path(vibe_env["VIBE_HOME"]), "deletesaved-12345678", workspace)
                seed_acp_single_saved_session(pathlib.Path(micro_env["VIBE_HOME"]), "deletesaved-12345678", workspace)
            if case.mode == "acp_delete_saved_pointer":
                seed_acp_pointer_session(pathlib.Path(vibe_env["VIBE_HOME"]), workspace)
                seed_acp_pointer_session(pathlib.Path(micro_env["VIBE_HOME"]), workspace)
            if case.mode == "acp_delete_exact_collision":
                seed_acp_collision_sessions(pathlib.Path(vibe_env["VIBE_HOME"]), workspace)
                seed_acp_collision_sessions(pathlib.Path(micro_env["VIBE_HOME"]), workspace)
            if case.mode == "acp_delete_loaded_saved":
                seed_acp_single_saved_session(pathlib.Path(vibe_env["VIBE_HOME"]), "loaddelete-12345678", workspace)
                seed_acp_single_saved_session(pathlib.Path(micro_env["VIBE_HOME"]), "loaddelete-12345678", workspace)
            if case.mode in {"acp_prompt_simple", "acp_prompt_client_message_id", "acp_prompt_agent_thought", "acp_prompt_usage_accumulates", "acp_prompt_usage_cost", "acp_prompt_image", "acp_prompt_image_wrong_type", "acp_prompt_image_invalid_base64", "acp_command_help", "acp_command_reload", "acp_command_compact_empty", "acp_command_compact_one", "acp_command_teleport_no_history", "acp_command_data_retention", "acp_command_proxy_help", "acp_command_proxy_set", "acp_command_proxy_unset", "acp_command_proxy_invalid", "acp_command_proxy_case", "acp_prompt_grep", "acp_permission_grep_allow", "acp_permission_grep_deny", "acp_permission_grep_cancelled", "acp_permission_grep_allow_always", "acp_permission_grep_allow_always_permanent", "acp_permission_bash_granular", "acp_permission_bash_granular_allow_always_permanent", "acp_fs_read", "acp_fs_read_range", "acp_fs_write", "acp_fs_edit", "acp_terminal_bash_allow", "acp_terminal_bash_nonzero", "acp_terminal_bash_none_exit", "acp_terminal_bash_timeout", "acp_tool_meta_web_fetch", "acp_tool_meta_web_search", "acp_tool_meta_skill", "acp_tool_meta_task", "acp_prompt_todo", "acp_prompt_todo_invalid", "acp_fork_from_prompt_message", "acp_user_display_content", "acp_authenticate_browser_complete", "acp_authenticate_browser_unsupported_action", "acp_initialize_delegated_browser_auth", "acp_authenticate_delegated_start", "acp_authenticate_delegated_complete", "acp_authenticate_delegated_missing_attempt", "acp_authenticate_delegated_unknown_attempt", "acp_authenticate_delegated_unsupported_action"}:
                pass
            elif case.mode == "acp_initialize":
                vibe_env.pop("MISTRAL_API_KEY", None)
                micro_env.pop("MISTRAL_API_KEY", None)
                vibe_raw = run_acp_initialize(vibe_acp, vibe_env, case, ROOT)
                micro_raw = run_acp_initialize(microvibe_acp, micro_env, case, ROOT)
            elif case.mode == "acp_new_session":
                vibe_raw = run_acp_new_session(vibe_acp, vibe_env, ROOT, workspace)
                micro_raw = run_acp_new_session(microvibe_acp, micro_env, ROOT, workspace)
            elif case.mode in {
                "acp_list_sessions_empty",
                "acp_list_sessions_seeded",
                "acp_list_sessions_sorted",
                "acp_list_sessions_skip_invalid",
                "acp_list_sessions_timestamps",
            }:
                vibe_raw = run_acp_list_sessions(vibe_acp, vibe_env, ROOT)
                micro_raw = run_acp_list_sessions(microvibe_acp, micro_env, ROOT)
            elif case.mode == "acp_list_sessions_cwd_filter":
                project1 = tmp_path / "project1"
                vibe_raw = run_acp_list_sessions_cwd(vibe_acp, vibe_env, ROOT, project1)
                micro_raw = run_acp_list_sessions_cwd(microvibe_acp, micro_env, ROOT, project1)
            elif case.mode == "acp_load_session":
                vibe_raw = run_acp_load_session(vibe_acp, vibe_env, ROOT, workspace)
                micro_raw = run_acp_load_session(microvibe_acp, micro_env, ROOT, workspace)
            elif case.mode == "acp_load_rich_session":
                vibe_raw = run_acp_load_rich_session(vibe_acp, vibe_env, ROOT, workspace)
                micro_raw = run_acp_load_rich_session(microvibe_acp, micro_env, ROOT, workspace)
            elif case.mode == "acp_load_replay_ids":
                vibe_raw = run_acp_load_replay_ids(vibe_acp, vibe_env, ROOT, workspace)
                micro_raw = run_acp_load_replay_ids(microvibe_acp, micro_env, ROOT, workspace)
            elif case.mode == "acp_load_missing":
                vibe_raw = run_acp_load_missing(vibe_acp, vibe_env, ROOT, workspace)
                micro_raw = run_acp_load_missing(microvibe_acp, micro_env, ROOT, workspace)
            elif case.mode == "acp_fork_session":
                write_session_configs(tmp_path, 9)
                vibe_raw = run_acp_fork_session(vibe_acp, vibe_env, ROOT, workspace)
                micro_raw = run_acp_fork_session(microvibe_acp, micro_env, ROOT, workspace)
            elif case.mode.startswith("acp_set_mode_fork_"):
                write_session_configs(tmp_path, 9)
                mode_id = {
                    "acp_set_mode_fork_default": "default",
                    "acp_set_mode_fork_auto_approve": "auto-approve",
                    "acp_set_mode_fork_plan": "plan",
                    "acp_set_mode_fork_accept_edits": "accept-edits",
                    "acp_set_mode_fork_chat": "chat",
                    "acp_set_mode_fork_invalid": "invalid-mode",
                    "acp_set_mode_fork_empty": "",
                }[case.mode]
                vibe_raw = run_acp_set_mode_then_fork(vibe_acp, vibe_env, ROOT, workspace, mode_id)
                micro_raw = run_acp_set_mode_then_fork(microvibe_acp, micro_env, ROOT, workspace, mode_id)
            elif case.mode == "acp_fork_missing":
                vibe_raw = run_acp_fork_missing(vibe_acp, vibe_env, ROOT, workspace)
                micro_raw = run_acp_fork_missing(microvibe_acp, micro_env, ROOT, workspace)
            elif case.mode == "acp_prompt_missing_session":
                vibe_raw = run_acp_prompt_missing_session(vibe_acp, vibe_env, ROOT)
                micro_raw = run_acp_prompt_missing_session(microvibe_acp, micro_env, ROOT)
            elif case.mode == "acp_close_session":
                vibe_raw = run_acp_close_session(vibe_acp, vibe_env, ROOT, workspace)
                micro_raw = run_acp_close_session(microvibe_acp, micro_env, ROOT, workspace)
            elif case.mode == "acp_close_missing":
                vibe_raw = run_acp_close_missing(vibe_acp, vibe_env, ROOT)
                micro_raw = run_acp_close_missing(microvibe_acp, micro_env, ROOT)
            elif case.mode == "acp_set_title_live_unsaved":
                vibe_raw = run_acp_set_title_live_unsaved(vibe_acp, vibe_env, ROOT, workspace)
                micro_raw = run_acp_set_title_live_unsaved(microvibe_acp, micro_env, ROOT, workspace)
            elif case.mode == "acp_set_title_saved":
                vibe_raw = run_acp_set_title_saved(vibe_acp, vibe_env, ROOT)
                micro_raw = run_acp_set_title_saved(microvibe_acp, micro_env, ROOT)
            elif case.mode == "acp_delete_saved":
                vibe_raw = run_acp_delete_saved(vibe_acp, vibe_env, ROOT)
                micro_raw = run_acp_delete_saved(microvibe_acp, micro_env, ROOT)
            elif case.mode == "acp_delete_missing":
                vibe_raw = run_acp_delete_missing(vibe_acp, vibe_env, ROOT)
                micro_raw = run_acp_delete_missing(microvibe_acp, micro_env, ROOT)
            elif case.mode == "acp_delete_invalid_missing":
                vibe_raw = run_acp_delete_invalid_missing(vibe_acp, vibe_env, ROOT)
                micro_raw = run_acp_delete_invalid_missing(microvibe_acp, micro_env, ROOT)
            elif case.mode == "acp_delete_invalid_empty":
                vibe_raw = run_acp_delete_invalid_empty(vibe_acp, vibe_env, ROOT)
                micro_raw = run_acp_delete_invalid_empty(microvibe_acp, micro_env, ROOT)
            elif case.mode == "acp_delete_invalid_saved_session_id":
                vibe_raw = run_acp_delete_invalid_saved_session_id(vibe_acp, vibe_env, ROOT)
                micro_raw = run_acp_delete_invalid_saved_session_id(microvibe_acp, micro_env, ROOT)
            elif case.mode == "acp_delete_saved_pointer":
                vibe_raw = run_acp_delete_saved_pointer(vibe_acp, vibe_env, ROOT)
                micro_raw = run_acp_delete_saved_pointer(microvibe_acp, micro_env, ROOT)
            elif case.mode == "acp_delete_exact_collision":
                vibe_raw = run_acp_delete_exact_collision(vibe_acp, vibe_env, ROOT)
                micro_raw = run_acp_delete_exact_collision(microvibe_acp, micro_env, ROOT)
            elif case.mode == "acp_delete_live_unsaved":
                vibe_raw = run_acp_delete_live_unsaved(vibe_acp, vibe_env, ROOT, workspace)
                micro_raw = run_acp_delete_live_unsaved(microvibe_acp, micro_env, ROOT, workspace)
            elif case.mode == "acp_delete_loaded_saved":
                vibe_raw = run_acp_delete_loaded_saved(vibe_acp, vibe_env, ROOT, workspace)
                micro_raw = run_acp_delete_loaded_saved(microvibe_acp, micro_env, ROOT, workspace)
            elif case.mode in {
                "acp_auth_status_signed_out",
                "acp_auth_status_process_env",
                "acp_auth_status_dotenv",
                "acp_auth_status_process_over_dotenv",
            }:
                vibe_raw = run_acp_auth_status(vibe_acp, vibe_env, ROOT)
                micro_raw = run_acp_auth_status(microvibe_acp, micro_env, ROOT)
            elif case.mode == "acp_auth_signout_dotenv":
                vibe_raw = run_acp_auth_signout_dotenv(vibe_acp, vibe_env, ROOT)
                micro_raw = run_acp_auth_signout_dotenv(microvibe_acp, micro_env, ROOT)
            elif case.mode == "acp_auth_signout_process_over_dotenv":
                vibe_raw = run_acp_auth_signout_process_over_dotenv(vibe_acp, vibe_env, ROOT)
                micro_raw = run_acp_auth_signout_process_over_dotenv(microvibe_acp, micro_env, ROOT)
            elif case.mode == "acp_authenticate_unsupported":
                vibe_raw = run_acp_authenticate_unsupported(vibe_acp, vibe_env, ROOT)
                micro_raw = run_acp_authenticate_unsupported(microvibe_acp, micro_env, ROOT)
            elif case.mode == "acp_initialize_unsupported_provider":
                vibe_raw = run_acp_initialize_unsupported_provider(vibe_acp, vibe_env, ROOT)
                micro_raw = run_acp_initialize_unsupported_provider(microvibe_acp, micro_env, ROOT)
            elif case.mode == "acp_authenticate_browser_unsupported":
                vibe_raw = run_acp_authenticate_browser_unsupported(vibe_acp, vibe_env, ROOT)
                micro_raw = run_acp_authenticate_browser_unsupported(microvibe_acp, micro_env, ROOT)
            elif case.mode == "acp_telemetry_notification":
                vibe_raw = run_acp_telemetry_notification(vibe_acp, vibe_env, ROOT, workspace)
                micro_raw = run_acp_telemetry_notification(microvibe_acp, micro_env, ROOT, workspace)
            elif case.mode == "acp_unknown_notification":
                vibe_raw = run_acp_unknown_notification(vibe_acp, vibe_env, ROOT)
                micro_raw = run_acp_unknown_notification(microvibe_acp, micro_env, ROOT)
            elif case.mode == "acp_trust_status_untrusted":
                vibe_raw = run_acp_trust_status(vibe_acp, vibe_env, ROOT, workspace)
                micro_raw = run_acp_trust_status(microvibe_acp, micro_env, ROOT, workspace)
            elif case.mode == "acp_trust_status_repo":
                vibe_raw = run_acp_trust_status(vibe_acp, vibe_env, ROOT, workspace)
                micro_raw = run_acp_trust_status(microvibe_acp, micro_env, ROOT, workspace)
            elif case.mode == "acp_trust_decision_cwd":
                vibe_raw = run_acp_trust_decision(vibe_acp, vibe_env, ROOT, workspace, "trust_cwd")
                micro_raw = run_acp_trust_decision(microvibe_acp, micro_env, ROOT, workspace, "trust_cwd")
            elif case.mode == "acp_trust_decision_repo":
                vibe_raw = run_acp_trust_decision(vibe_acp, vibe_env, ROOT, workspace, "trust_repo")
                micro_raw = run_acp_trust_decision(microvibe_acp, micro_env, ROOT, workspace, "trust_repo")
            elif case.mode == "acp_trust_decision_invalid":
                vibe_raw = run_acp_trust_decision(vibe_acp, vibe_env, ROOT, workspace, "trust_repo")
                micro_raw = run_acp_trust_decision(microvibe_acp, micro_env, ROOT, workspace, "trust_repo")
            elif case.mode == "acp_trust_decision_missing_session":
                vibe_raw = run_acp_trust_decision_missing_session(vibe_acp, vibe_env, ROOT, workspace)
                micro_raw = run_acp_trust_decision_missing_session(microvibe_acp, micro_env, ROOT, workspace)
            elif case.mode == "acp_set_mode_valid":
                vibe_raw = run_acp_set_mode(vibe_acp, vibe_env, ROOT, workspace, "auto-approve")
                micro_raw = run_acp_set_mode(microvibe_acp, micro_env, ROOT, workspace, "auto-approve")
            elif case.mode == "acp_set_mode_invalid":
                vibe_raw = run_acp_set_mode(vibe_acp, vibe_env, ROOT, workspace, "invalid-mode")
                micro_raw = run_acp_set_mode(microvibe_acp, micro_env, ROOT, workspace, "invalid-mode")
            elif case.mode == "acp_set_model_valid":
                vibe_raw = run_acp_set_model(vibe_acp, vibe_env, ROOT, workspace, "alt")
                micro_raw = run_acp_set_model(microvibe_acp, micro_env, ROOT, workspace, "alt")
            elif case.mode == "acp_set_model_invalid":
                vibe_raw = run_acp_set_model(vibe_acp, vibe_env, ROOT, workspace, "missing-model")
                micro_raw = run_acp_set_model(microvibe_acp, micro_env, ROOT, workspace, "missing-model")
            elif case.mode == "acp_set_model_same":
                vibe_raw = run_acp_set_model(vibe_acp, vibe_env, ROOT, workspace, "test")
                micro_raw = run_acp_set_model(microvibe_acp, micro_env, ROOT, workspace, "test")
            elif case.mode == "acp_set_model_empty":
                vibe_raw = run_acp_set_model(vibe_acp, vibe_env, ROOT, workspace, "")
                micro_raw = run_acp_set_model(microvibe_acp, micro_env, ROOT, workspace, "")
            elif case.mode == "acp_set_config_mode":
                vibe_raw = run_acp_set_config(vibe_acp, vibe_env, ROOT, workspace, "mode", "plan")
                micro_raw = run_acp_set_config(microvibe_acp, micro_env, ROOT, workspace, "mode", "plan")
            elif case.mode == "acp_set_config_mode_empty":
                vibe_raw = run_acp_set_config(vibe_acp, vibe_env, ROOT, workspace, "mode", "")
                micro_raw = run_acp_set_config(microvibe_acp, micro_env, ROOT, workspace, "mode", "")
            elif case.mode == "acp_set_config_model":
                vibe_raw = run_acp_set_config(vibe_acp, vibe_env, ROOT, workspace, "model", "alt")
                micro_raw = run_acp_set_config(microvibe_acp, micro_env, ROOT, workspace, "model", "alt")
            elif case.mode == "acp_set_config_model_empty":
                vibe_raw = run_acp_set_config(vibe_acp, vibe_env, ROOT, workspace, "model", "")
                micro_raw = run_acp_set_config(microvibe_acp, micro_env, ROOT, workspace, "model", "")
            elif case.mode == "acp_set_config_thinking":
                vibe_raw = run_acp_set_config(vibe_acp, vibe_env, ROOT, workspace, "thinking", "high")
                micro_raw = run_acp_set_config(microvibe_acp, micro_env, ROOT, workspace, "thinking", "high")
            elif case.mode == "acp_set_config_thinking_invalid":
                vibe_raw = run_acp_set_config(vibe_acp, vibe_env, ROOT, workspace, "thinking", "ultra")
                micro_raw = run_acp_set_config(microvibe_acp, micro_env, ROOT, workspace, "thinking", "ultra")
            elif case.mode == "acp_set_config_thinking_empty":
                vibe_raw = run_acp_set_config(vibe_acp, vibe_env, ROOT, workspace, "thinking", "")
                micro_raw = run_acp_set_config(microvibe_acp, micro_env, ROOT, workspace, "thinking", "")
            elif case.mode == "acp_set_config_max_turns":
                vibe_raw = run_acp_set_config(vibe_acp, vibe_env, ROOT, workspace, "max_turns", "2")
                micro_raw = run_acp_set_config(microvibe_acp, micro_env, ROOT, workspace, "max_turns", "2")
            elif case.mode == "acp_set_config_max_turns_invalid":
                vibe_raw = run_acp_set_config(vibe_acp, vibe_env, ROOT, workspace, "max_turns", "abc")
                micro_raw = run_acp_set_config(microvibe_acp, micro_env, ROOT, workspace, "max_turns", "abc")
            elif case.mode == "acp_set_config_max_turns_bool":
                vibe_raw = run_acp_set_config_value(vibe_acp, vibe_env, ROOT, workspace, "max_turns", True)
                micro_raw = run_acp_set_config_value(microvibe_acp, micro_env, ROOT, workspace, "max_turns", True)
            elif case.mode == "acp_set_config_invalid_id":
                vibe_raw = run_acp_set_config(vibe_acp, vibe_env, ROOT, workspace, "invalid_config", "some_value")
                micro_raw = run_acp_set_config(microvibe_acp, micro_env, ROOT, workspace, "invalid_config", "some_value")
            elif case.mode == "acp_set_config_empty_id":
                vibe_raw = run_acp_set_config(vibe_acp, vibe_env, ROOT, workspace, "", "some_value")
                micro_raw = run_acp_set_config(microvibe_acp, micro_env, ROOT, workspace, "", "some_value")
            else:
                vibe_raw = run_programmatic(
                    build_command(vibe_acp, case.mode, microvibe=False),
                    vibe_env,
                    case,
                    ROOT,
                )
                micro_raw = run_programmatic(
                    build_command(microvibe_acp, case.mode, microvibe=True),
                    micro_env,
                    case,
                    ROOT,
                )
        elif case.name in {"programmatic_continue_json", "programmatic_resume_id_json"}:
            with ThreadingTCPServer(("127.0.0.1", 0), FakeChatHandler) as server:
                port = int(server.server_address[1])
                workspace = tmp_path / "workspace"
                workspace.mkdir(parents=True, exist_ok=True)
                first_response = {
                    "id": "chatcmpl_parity_first",
                    "object": "chat.completion",
                    "created": 0,
                    "model": "test-model",
                    "choices": [
                        {
                            "index": 0,
                            "message": {"role": "assistant", "content": "first saved"},
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
                }
                second_response = {
                    **first_response,
                    "id": "chatcmpl_parity_second",
                    "choices": [
                        {
                            "index": 0,
                            "message": {"role": "assistant", "content": "second resumed"},
                            "finish_reason": "stop",
                        }
                    ],
                }
                FakeChatHandler.responses = [first_response, second_response]
                FakeChatHandler.next_response = 0
                thread = threading.Thread(target=server.serve_forever, daemon=True)
                thread.start()
                write_session_configs(tmp_path, port)
                vibe_env = isolated_env("vibe", tmp_path)
                vibe_env["VIBE_HOME"] = str(tmp_path / "vibe" / "home" / ".vibe")
                micro_env = isolated_env("microvibe", tmp_path)
                micro_env["VIBE_HOME"] = str(tmp_path / "microvibe" / "home" / ".vibe")
                if case.name == "programmatic_resume_id_json":
                    vibe_raw = run_resume_id_programmatic(vibe, vibe_env, case, workspace)
                else:
                    vibe_raw = run_continue_programmatic(vibe, vibe_env, case, workspace)
                FakeChatHandler.next_response = 0
                if case.name == "programmatic_resume_id_json":
                    micro_raw = run_resume_id_programmatic(microvibe, micro_env, case, workspace)
                else:
                    micro_raw = run_continue_programmatic(microvibe, micro_env, case, workspace)
                server.shutdown()
        elif case.mode.startswith("programmatic_"):
            with ThreadingTCPServer(("127.0.0.1", 0), FakeChatHandler) as server:
                port = int(server.server_address[1])
                workspace = tmp_path / "workspace"
                read_path = workspace / "sample.txt"
                workspace.mkdir(parents=True, exist_ok=True)
                read_path.write_text("alpha\nbeta\n", encoding="utf-8")
                FakeChatHandler.responses = programmatic_responses(case, workspace, port)
                FakeChatHandler.next_response = 0
                thread = threading.Thread(target=server.serve_forever, daemon=True)
                thread.start()
                write_programmatic_configs(tmp_path, port)
                if "_hooks_" in case.mode:
                    seed_programmatic_hooks(tmp_path, case.name, workspace)
                if "_mcp_stdio_" in case.mode:
                    seed_programmatic_mcp_stdio_config(tmp_path)
                if "_task_custom_" in case.mode:
                    write_custom_subagent(tmp_path / "vibe" / "home" / ".vibe")
                if "_agent_custom_" in case.mode:
                    write_custom_primary_agent(tmp_path / "vibe" / "home" / ".vibe")
                vibe_env = isolated_env("vibe", tmp_path)
                vibe_env["VIBE_HOME"] = str(tmp_path / "vibe" / "home" / ".vibe")
                micro_env = isolated_env("microvibe", tmp_path)
                micro_env["VIBE_HOME"] = vibe_env["VIBE_HOME"]
                FakeChatHandler.requests = []
                vibe_raw = run_programmatic(
                    build_command(vibe, case.mode, microvibe=False),
                    vibe_env,
                    case,
                    workspace,
                )
                vibe_requests = list(FakeChatHandler.requests)
                FakeChatHandler.next_response = 0
                FakeChatHandler.requests = []
                if case.name == "programmatic_hooks_post_json":
                    counter = tmp_path / "hooks" / "post_hook_count.txt"
                    if counter.exists():
                        counter.unlink()
                tool_output = workspace / "tool-output.txt"
                if tool_output.exists():
                    tool_output.unlink()
                micro_raw = run_programmatic(
                    build_command(microvibe, case.mode, microvibe=True),
                    micro_env,
                    case,
                    workspace,
                )
                micro_requests = list(FakeChatHandler.requests)
                vibe_side_effect_text = request_projection_text(case.name, vibe_requests)
                micro_side_effect_text = request_projection_text(case.name, micro_requests)
                server.shutdown()
        else:
            if case.mode.startswith("cli_"):
                vibe_env = isolated_env("vibe", tmp_path)
                micro_env = isolated_env("microvibe", tmp_path)
                if case.mode == "cli_setup":
                    vibe_env["VIBE_HOME"] = str(tmp_path / "vibe" / "home" / ".vibe")
                    micro_env["VIBE_HOME"] = str(tmp_path / "microvibe" / "home" / ".vibe")
                    vibe_env["PYTHON_KEYRING_BACKEND"] = "keyring.backends.fail.Keyring"
                    micro_env["PYTHON_KEYRING_BACKEND"] = "keyring.backends.fail.Keyring"
                    vibe_env["VIBE_ENABLE_TELEMETRY"] = "false"
                    micro_env["VIBE_ENABLE_TELEMETRY"] = "false"
                if case.name == "cli_check_upgrade_available":
                    vibe_env["VIBE_PARITY_UPDATE_LATEST"] = "9.9.9"
                    micro_env["VIBE_PARITY_UPDATE_LATEST"] = "9.9.9"
                if case.mode in AGENT_DIAGNOSTIC_MODES:
                    write_agent_diagnostic_configs(tmp_path, case.mode)
                    vibe_env["VIBE_HOME"] = str(tmp_path / "vibe" / "home" / ".vibe")
                    micro_env["VIBE_HOME"] = str(tmp_path / "microvibe" / "home" / ".vibe")
                if case.name == "tui_startup_agent_custom":
                    write_custom_primary_agent(pathlib.Path(vibe_env["HOME"]) / ".vibe")
                    write_custom_primary_agent(pathlib.Path(micro_env["HOME"]) / ".vibe")
                if case.name == "cli_check_upgrade_available" or case.mode == "cli_setup":
                    vibe_raw = run_pty(
                        build_command(vibe, case.mode, microvibe=False),
                        vibe_env,
                        case,
                        ROOT,
                    )
                    micro_raw = run_pty(
                        build_command(microvibe, case.mode, microvibe=True),
                        micro_env,
                        case,
                        ROOT,
                    )
                else:
                    vibe_raw = run_programmatic(
                        build_command(vibe, case.mode, microvibe=False),
                        vibe_env,
                        case,
                        ROOT,
                    )
                    micro_raw = run_programmatic(
                        build_command(microvibe, case.mode, microvibe=True),
                        micro_env,
                        case,
                        ROOT,
                    )
            elif case.name in {"tui_initial_prompt", "tui_prompt_simple", "tui_copy_last_agent", "tui_copy_last_agent_xclip", "tui_prompt_history_up", "tui_prompt_history_up_down", "tui_prompt_history_persisted", "tui_prompt_multiline_ctrl_j", "tui_prompt_at_file", "tui_completion_slash", "tui_completion_slash_nav_enter", "tui_completion_path_popup_list", "tui_completion_path_popup_ten", "tui_completion_path_dir_tab", "tui_completion_path_file", "tui_prompt_at_folder", "tui_prompt_at_image", "tui_prompt_at_image_no_vision", "tui_external_editor_input", "tui_external_editor_empty", "tui_scroll_shift_up", "tui_scroll_shift_up_down", "tui_prompt_read", "tui_prompt_read_expand_tool", "tui_prompt_read_expand_collapse_tool", "tui_prompt_bash", "tui_animation_bash_spinner", "tui_prompt_bash_allow", "tui_prompt_bash_allow_y", "tui_prompt_bash_allow_expand_tool", "tui_prompt_bash_allow_expand_collapse_tool", "tui_prompt_bash_allow_session", "tui_prompt_bash_always", "tui_prompt_bash_persisted_allow", "tui_prompt_bash_deny", "tui_prompt_bash_deny_n", "tui_prompt_file_tools", "tui_animation_write_file_spinner", "tui_animation_edit_spinner", "tui_prompt_file_tools_allow_write", "tui_prompt_file_tools_allow_edit", "tui_prompt_file_tools_expand_tool", "tui_prompt_todo", "tui_prompt_todo_empty", "tui_slash_skill", "tui_prompt_skill", "tui_prompt_skill_expand_tool", "tui_prompt_task", "tui_animation_task_spinner", "tui_prompt_task_allow_explore", "tui_prompt_task_allow_unknown", "tui_prompt_task_deny", "tui_prompt_web_fetch", "tui_prompt_web_fetch_expand_tool", "tui_animation_web_fetch_spinner", "tui_prompt_web_search", "tui_animation_web_search_spinner", "tui_prompt_web_search_expand_tool", "tui_prompt_question", "tui_animation_question_spinner", "tui_prompt_question_expand_tool", "tui_prompt_question_other", "tui_prompt_question_multi", "tui_prompt_question_multiselect", "tui_prompt_question_multiselect_other", "tui_prompt_exit_plan_auto", "tui_animation_exit_plan_spinner", "tui_prompt_exit_plan_default", "tui_prompt_exit_plan_no", "tui_prompt_exit_plan_editor"}:
                with ThreadingTCPServer(("127.0.0.1", 0), FakeChatHandler) as server:
                    port = int(server.server_address[1])
                    workspace = tmp_path / "workspace"
                    workspace.mkdir(parents=True, exist_ok=True)
                    read_path = workspace / "sample.txt"
                    read_path.write_text("alpha\nbeta\n", encoding="utf-8")
                    notes_dir = workspace / "notes"
                    notes_dir.mkdir(exist_ok=True)
                    (notes_dir / "note.md").write_text("folder note\n", encoding="utf-8")
                    (workspace / "image.png").write_bytes(
                        base64.b64decode(
                            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/p9sAAAAASUVORK5CYII="
                        )
                    )
                    if case.name in {"tui_completion_path_popup_list", "tui_completion_path_popup_ten", "tui_completion_path_dir_tab"}:
                        (workspace / "src" / "utils").mkdir(parents=True, exist_ok=True)
                        (workspace / "src" / "main.py").write_text("", encoding="utf-8")
                        (workspace / "src" / "utils" / "config.py").write_text("", encoding="utf-8")
                        if case.name == "tui_completion_path_popup_ten":
                            extra_dir = workspace / "src" / "core" / "extra"
                            extra_dir.mkdir(parents=True, exist_ok=True)
                            for index in range(1, 13):
                                (extra_dir / f"extra_file_{index}.py").write_text("", encoding="utf-8")
                    response = {
                        "id": "chatcmpl_tui_prompt_simple",
                        "object": "chat.completion",
                        "created": 0,
                        "model": "test-model",
                        "choices": [
                            {
                                "index": 0,
                                "message": {"role": "assistant", "content": "hello from tui"},
                                "finish_reason": "stop",
                            }
                        ],
                        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
                    }
                    if case.name in {"tui_scroll_shift_up", "tui_scroll_shift_up_down"}:
                        FakeChatHandler.responses = [
                            {
                                **response,
                                "id": f"chatcmpl_tui_scroll_{idx:02d}",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {
                                            "role": "assistant",
                                            "content": f"scroll reply {idx:02d}",
                                        },
                                        "finish_reason": "stop",
                                    }
                                ],
                            }
                            for idx in range(1, 7)
                        ]
                    elif case.name in {"tui_prompt_read", "tui_prompt_read_expand_tool", "tui_prompt_read_expand_collapse_tool"}:
                        FakeChatHandler.responses = [
                            tool_response("call_tui_read_1", "read", {"file_path": str(read_path)}),
                            {
                                **response,
                                "id": "chatcmpl_tui_prompt_read_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "read done"},
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    elif case.name in {"tui_prompt_at_file", "tui_completion_path_file", "tui_prompt_at_folder", "tui_prompt_at_image"}:
                        content = {
                            "tui_prompt_at_file": "at file done",
                            "tui_completion_path_file": "completion file done",
                            "tui_prompt_at_folder": "at folder done",
                            "tui_prompt_at_image": "at image done",
                        }[case.name]
                        FakeChatHandler.responses = [
                            {
                                **response,
                                "id": f"chatcmpl_{case.name}_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": content},
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    elif case.name in {"tui_prompt_bash", "tui_animation_bash_spinner", "tui_prompt_bash_allow", "tui_prompt_bash_allow_y", "tui_prompt_bash_allow_expand_tool", "tui_prompt_bash_allow_expand_collapse_tool"}:
                        FakeChatHandler.responses = [
                            tool_response("call_tui_bash_1", "bash", {"command": "printf bash-parity"}),
                            {
                                **response,
                                "id": "chatcmpl_tui_prompt_bash_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "bash done"},
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    elif case.name in {"tui_prompt_bash_deny", "tui_prompt_bash_deny_n"}:
                        denied_marker = workspace / "denied-side-effect.txt"
                        FakeChatHandler.responses = [
                            tool_response("call_tui_bash_denied", "bash", {"command": f"touch {denied_marker}"}),
                            {
                                **response,
                                "id": "chatcmpl_tui_prompt_bash_denied_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "bash denied done"},
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    elif case.name == "tui_prompt_bash_allow_session":
                        FakeChatHandler.responses = [
                            tool_response("call_tui_bash_1", "bash", {"command": "printf bash-parity"}),
                            tool_response("call_tui_bash_2", "bash", {"command": "printf bash-parity"}),
                            {
                                **response,
                                "id": "chatcmpl_tui_prompt_bash_session_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "bash session done"},
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    elif case.name == "tui_prompt_bash_always":
                        FakeChatHandler.responses = [
                            tool_response("call_tui_bash_1", "bash", {"command": "printf bash-parity"}),
                            tool_response("call_tui_bash_2", "bash", {"command": "printf bash-parity"}),
                            {
                                **response,
                                "id": "chatcmpl_tui_prompt_bash_always_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "bash always done"},
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    elif case.name == "tui_prompt_bash_persisted_allow":
                        FakeChatHandler.responses = [
                            tool_response("call_tui_bash_persisted", "bash", {"command": "printf bash-parity"}),
                            {
                                **response,
                                "id": "chatcmpl_tui_prompt_bash_persisted_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "bash persisted done"},
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    elif case.name in {"tui_prompt_file_tools", "tui_animation_write_file_spinner", "tui_animation_edit_spinner", "tui_prompt_file_tools_allow_write", "tui_prompt_file_tools_allow_edit", "tui_prompt_file_tools_expand_tool"}:
                        tool_file = workspace / "tool-output.txt"
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_tui_write_1",
                                "write_file",
                                {"path": str(tool_file), "content": "needle\nold\n"},
                            ),
                            tool_response(
                                "call_tui_edit_1",
                                "edit",
                                {
                                    "file_path": str(tool_file),
                                    "old_string": "old",
                                    "new_string": "new",
                                },
                            ),
                            tool_response(
                                "call_tui_grep_1",
                                "grep",
                                {
                                    "pattern": "needle",
                                    "path": str(workspace),
                                    "max_matches": 10,
                                },
                            ),
                            {
                                **response,
                                "id": "chatcmpl_tui_prompt_file_tools_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "file tools done"},
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    elif case.name == "tui_prompt_todo":
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_tui_todo_1",
                                "todo",
                                {
                                    "action": "write",
                                    "todos": [
                                        {
                                            "id": "1",
                                            "content": "Check parity",
                                            "status": "in_progress",
                                            "priority": "high",
                                        },
                                        {
                                            "id": "2",
                                            "content": "Document result",
                                            "status": "pending",
                                            "priority": "medium",
                                        },
                                        {
                                            "id": "3",
                                            "content": "Ship Rust version",
                                            "status": "completed",
                                            "priority": "high",
                                        },
                                    ],
                                },
                            ),
                            {
                                **response,
                                "id": "chatcmpl_tui_prompt_todo_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "todo done"},
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    elif case.name == "tui_prompt_todo_empty":
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_tui_todo_empty_1",
                                "todo",
                                {"action": "read"},
                            ),
                            {
                                **response,
                                "id": "chatcmpl_tui_prompt_todo_empty_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "todo empty done"},
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    elif case.name in {"tui_prompt_skill", "tui_prompt_skill_expand_tool"}:
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_tui_skill_1",
                                "skill",
                                {"name": "parity-skill"},
                            ),
                            {
                                **response,
                                "id": "chatcmpl_tui_prompt_skill_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "skill done"},
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    elif case.name == "tui_prompt_task_allow_explore":
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_tui_task_explore_1",
                                "task",
                                {
                                    "task": "Inspect sample.txt and report the first word",
                                    "agent": "explore",
                                },
                            ),
                            {
                                **response,
                                "id": "chatcmpl_tui_prompt_task_explore_subagent",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {
                                            "role": "assistant",
                                            "content": "Subagent found alpha.",
                                        },
                                        "finish_reason": "stop",
                                    }
                                ],
                                "usage": {"prompt_tokens": 4, "completion_tokens": 3, "total_tokens": 7},
                            },
                            {
                                **response,
                                "id": "chatcmpl_tui_prompt_task_explore_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {
                                            "role": "assistant",
                                            "content": "task explore done",
                                        },
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    elif case.name in {"tui_prompt_task", "tui_animation_task_spinner", "tui_prompt_task_allow_unknown", "tui_prompt_task_deny"}:
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_tui_task_1",
                                "task",
                                {
                                    "task": "Inspect sample.txt and report the first word",
                                    "agent": "no-such-agent",
                                },
                            ),
                            {
                                **response,
                                "id": f"chatcmpl_{case.name}_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {
                                            "role": "assistant",
                                            "content": "task denied done"
                                            if case.name == "tui_prompt_task_deny"
                                            else "task unknown done"
                                            if case.name == "tui_prompt_task_allow_unknown"
                                            else "task done",
                                        },
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    elif case.name in {"tui_prompt_web_fetch", "tui_prompt_web_fetch_expand_tool", "tui_animation_web_fetch_spinner"}:
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_tui_web_fetch_1",
                                "web_fetch",
                                {"url": f"http://127.0.0.1:{port}/fetch.txt"},
                            ),
                            {
                                **response,
                                "id": "chatcmpl_tui_prompt_web_fetch_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "web fetch done"},
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    elif case.name in {"tui_prompt_web_search", "tui_animation_web_search_spinner", "tui_prompt_web_search_expand_tool"}:
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_tui_web_search_1",
                                "web_search",
                                {"query": "parity search query"},
                            ),
                            {
                                **response,
                                "id": "chatcmpl_tui_prompt_web_search_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "web search done"},
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    elif case.name in {"tui_prompt_question", "tui_animation_question_spinner", "tui_prompt_question_expand_tool"}:
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_tui_question_1",
                                "ask_user_question",
                                {
                                    "questions": [
                                        {
                                            "question": "Choose parity mode?",
                                            "header": "Parity",
                                            "options": [
                                                {
                                                    "label": "Strict",
                                                    "description": "Require exact parity",
                                                },
                                                {
                                                    "label": "Loose",
                                                    "description": "Allow differences",
                                                },
                                            ],
                                        }
                                    ],
                                },
                            ),
                            {
                                **response,
                                "id": "chatcmpl_tui_prompt_question_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "question answer done"},
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    elif case.name == "tui_prompt_question_other":
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_tui_question_other_1",
                                "ask_user_question",
                                {
                                    "questions": [
                                        {
                                            "question": "Choose custom mode?",
                                            "header": "Custom",
                                            "options": [
                                                {
                                                    "label": "Strict",
                                                    "description": "Require exact parity",
                                                },
                                                {
                                                    "label": "Loose",
                                                    "description": "Allow differences",
                                                },
                                            ],
                                        }
                                    ],
                                },
                            ),
                            {
                                **response,
                                "id": "chatcmpl_tui_prompt_question_other_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "question other done"},
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    elif case.name == "tui_prompt_question_multi":
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_tui_question_multi_1",
                                "ask_user_question",
                                {
                                    "questions": [
                                        {
                                            "question": "Choose first?",
                                            "header": "First",
                                            "options": [
                                                {"label": "Alpha", "description": "First answer"},
                                                {"label": "Beta", "description": "Other first answer"},
                                            ],
                                        },
                                        {
                                            "question": "Choose second?",
                                            "header": "Second",
                                            "options": [
                                                {"label": "Gamma", "description": "Second answer"},
                                                {"label": "Delta", "description": "Other second answer"},
                                            ],
                                        },
                                    ],
                                },
                            ),
                            {
                                **response,
                                "id": "chatcmpl_tui_prompt_question_multi_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "multi question done"},
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    elif case.name == "tui_prompt_question_multiselect":
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_tui_question_multiselect_1",
                                "ask_user_question",
                                {
                                    "questions": [
                                        {
                                            "question": "Pick colors?",
                                            "header": "Colors",
                                            "multi_select": True,
                                            "hide_other": True,
                                            "options": [
                                                {"label": "Red", "description": "Warm"},
                                                {"label": "Blue", "description": "Cool"},
                                            ],
                                        }
                                    ],
                                },
                            ),
                            {
                                **response,
                                "id": "chatcmpl_tui_prompt_question_multiselect_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "multi select done"},
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    elif case.name == "tui_prompt_question_multiselect_other":
                        FakeChatHandler.responses = [
                            tool_response(
                                "call_tui_question_multiselect_other_1",
                                "ask_user_question",
                                {
                                    "questions": [
                                        {
                                            "question": "Pick colors?",
                                            "header": "Colors",
                                            "multi_select": True,
                                            "options": [
                                                {"label": "Red", "description": "Warm"},
                                                {"label": "Blue", "description": "Cool"},
                                            ],
                                        }
                                    ],
                                },
                            ),
                            {
                                **response,
                                "id": "chatcmpl_tui_prompt_question_multiselect_other_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "multi select other done"},
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    elif case.name in {"tui_prompt_exit_plan_auto", "tui_animation_exit_plan_spinner", "tui_prompt_exit_plan_default", "tui_prompt_exit_plan_no", "tui_prompt_exit_plan_editor"}:
                        FakeChatHandler.responses = [
                            tool_response("call_tui_exit_plan_1", "exit_plan_mode", {}),
                            {
                                **response,
                                "id": f"chatcmpl_{case.name}_final",
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {"role": "assistant", "content": "exit plan done"},
                                        "finish_reason": "stop",
                                    }
                                ],
                            },
                        ]
                    else:
                        FakeChatHandler.responses = [response]
                    FakeChatHandler.next_response = 0
                    FakeChatHandler.requests = []
                    thread = threading.Thread(target=server.serve_forever, daemon=True)
                    thread.start()
                    write_session_configs(
                        tmp_path,
                        port,
                        supports_images=case.name == "tui_prompt_at_image",
                    )
                    if case.name == "tui_prompt_bash_persisted_allow":
                        seed_bash_allowlist(tmp_path)
                    if case.name in {"tui_prompt_exit_plan_auto", "tui_animation_exit_plan_spinner", "tui_prompt_exit_plan_default", "tui_prompt_exit_plan_no", "tui_prompt_exit_plan_editor"}:
                        seed_plan_agent(tmp_path)
                    vibe_env = isolated_env("vibe", tmp_path)
                    vibe_env["VIBE_HOME"] = str(tmp_path / "vibe" / "home" / ".vibe")
                    vibe_env["VIBE_PARITY_MISTRAL_SERVER_URL"] = f"http://127.0.0.1:{port}"
                    micro_env = isolated_env("microvibe", tmp_path)
                    micro_env["VIBE_HOME"] = str(tmp_path / "microvibe" / "home" / ".vibe")
                    setup_fake_editor(case.name, tmp_path, "vibe", vibe_env)
                    setup_fake_editor(case.name, tmp_path, "microvibe", micro_env)
                    setup_fake_clipboard(case.name, tmp_path, "vibe", vibe_env)
                    setup_fake_clipboard(case.name, tmp_path, "microvibe", micro_env)
                    if case.name == "tui_prompt_history_persisted":
                        seed_case = Case(
                            "tui_prompt_simple",
                            case.mode,
                            b"hello tui\x1b\r",
                            case.settle,
                            case.timeout,
                        )
                        run_pty(
                            build_command(vibe, case.mode, microvibe=False),
                            vibe_env,
                            seed_case,
                            workspace,
                        )
                        FakeChatHandler.next_response = 0
                    vibe_raw = run_pty(
                        build_command(vibe, case.mode, microvibe=False),
                        vibe_env,
                        case,
                        workspace,
                    )
                    if case.name in {"tui_slash_skill", "tui_prompt_at_file", "tui_completion_path_file", "tui_prompt_at_folder", "tui_prompt_at_image", "tui_prompt_at_image_no_vision"}:
                        vibe_side_effect_text = request_projection_text(case.name, FakeChatHandler.requests)
                    if case.name in {"tui_prompt_bash_deny", "tui_prompt_bash_deny_n"}:
                        denied_marker = workspace / "denied-side-effect.txt"
                        vibe_side_effect_text = (
                            "\n<side_effect>" + json.dumps(
                                {"denied_marker_exists": denied_marker.exists()},
                                sort_keys=True,
                            ) + "</side_effect>\n"
                        )
                    if case.name in {"tui_prompt_file_tools", "tui_animation_write_file_spinner", "tui_animation_edit_spinner", "tui_prompt_file_tools_allow_write", "tui_prompt_file_tools_allow_edit", "tui_prompt_file_tools_expand_tool"}:
                        tool_output = workspace / "tool-output.txt"
                        if tool_output.exists():
                            tool_output.unlink()
                    if case.name in {"tui_prompt_bash_deny", "tui_prompt_bash_deny_n"}:
                        denied_marker = workspace / "denied-side-effect.txt"
                        if denied_marker.exists():
                            denied_marker.unlink()
                    FakeChatHandler.next_response = 0
                    FakeChatHandler.requests = []
                    if case.name == "tui_prompt_history_persisted":
                        seed_case = Case(
                            "tui_prompt_simple",
                            case.mode,
                            b"hello tui\x1b\r",
                            case.settle,
                            case.timeout,
                        )
                        run_pty(
                            build_command(microvibe, case.mode, microvibe=True),
                            micro_env,
                            seed_case,
                            workspace,
                        )
                        FakeChatHandler.next_response = 0
                    micro_raw = run_pty(
                        build_command(microvibe, case.mode, microvibe=True),
                        micro_env,
                        case,
                        workspace,
                    )
                    if case.name in {"tui_slash_skill", "tui_prompt_at_file", "tui_completion_path_file", "tui_prompt_at_folder", "tui_prompt_at_image", "tui_prompt_at_image_no_vision"}:
                        micro_side_effect_text = request_projection_text(case.name, FakeChatHandler.requests)
                    if case.name in {"tui_prompt_bash_deny", "tui_prompt_bash_deny_n"}:
                        denied_marker = workspace / "denied-side-effect.txt"
                        micro_side_effect_text = (
                            "\n<side_effect>" + json.dumps(
                                {"denied_marker_exists": denied_marker.exists()},
                                sort_keys=True,
                            ) + "</side_effect>\n"
                        )
                    if case.name == "tui_prompt_at_image":
                        vibe_session_text = session_projection_text(case.name, tmp_path, "vibe")
                        micro_session_text = session_projection_text(case.name, tmp_path, "microvibe")
                    server.shutdown()
            if case.name in {
                "tui_resume_one",
                "tui_resume_legacy_json",
                "tui_resume_skips_invalid",
                "tui_resume_same_end_time_mtime",
                "tui_continue_one",
                "tui_resume_select_one",
                "tui_resume_delete_confirm",
                "tui_resume_delete_one",
                "tui_resume_rename_one",
                "tui_compact_one",
                "tui_rewind_one",
                "tui_rewind_select_one",
                "tui_rewind_global_ctrl_p",
                "tui_rewind_global_ctrl_p_prev",
                "tui_rewind_global_ctrl_n",
                "tui_rewind_global_alt_up",
                "tui_rewind_global_alt_down",
            }:
                with ThreadingTCPServer(("127.0.0.1", 0), FakeChatHandler) as server:
                    port = int(server.server_address[1])
                    workspace = tmp_path / "workspace"
                    workspace.mkdir(parents=True, exist_ok=True)
                    response = {
                        "id": "chatcmpl_resume_one",
                        "object": "chat.completion",
                        "created": 0,
                        "model": "test-model",
                        "choices": [
                            {
                                "index": 0,
                                "message": {"role": "assistant", "content": "first saved"},
                                "finish_reason": "stop",
                            }
                        ],
                        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
                    }
                    compact_response = {
                        **response,
                        "id": "chatcmpl_compact_one",
                        "choices": [
                            {
                                "index": 0,
                                "message": {"role": "assistant", "content": "compact summary"},
                                "finish_reason": "stop",
                            }
                        ],
                    }
                    second_response = {
                        **response,
                        "id": "chatcmpl_resume_two",
                        "choices": [
                            {
                                "index": 0,
                                "message": {"role": "assistant", "content": "second saved"},
                                "finish_reason": "stop",
                            }
                        ],
                    }
                    FakeChatHandler.responses = [response, compact_response]
                    FakeChatHandler.next_response = 0
                    thread = threading.Thread(target=server.serve_forever, daemon=True)
                    thread.start()
                    write_session_configs(tmp_path, port)
                    vibe_env = isolated_env("vibe", tmp_path)
                    vibe_env["VIBE_HOME"] = str(tmp_path / "vibe" / "home" / ".vibe")
                    micro_env = isolated_env("microvibe", tmp_path)
                    micro_env["VIBE_HOME"] = str(tmp_path / "microvibe" / "home" / ".vibe")
                    if case.name in {
                        "tui_rewind_global_ctrl_p",
                        "tui_rewind_global_ctrl_p_prev",
                        "tui_rewind_global_ctrl_n",
                        "tui_rewind_global_alt_up",
                        "tui_rewind_global_alt_down",
                    }:
                        FakeChatHandler.responses = [response, second_response]
                        run_seed_two_programmatic(vibe, vibe_env, case, workspace)
                    elif case.name == "tui_resume_legacy_json":
                        seed_legacy_json_session(pathlib.Path(vibe_env["VIBE_HOME"]), workspace)
                    elif case.name == "tui_resume_skips_invalid":
                        seed_invalid_newer_saved_session(pathlib.Path(vibe_env["VIBE_HOME"]), workspace)
                    elif case.name == "tui_resume_same_end_time_mtime":
                        seed_same_end_time_sessions(pathlib.Path(vibe_env["VIBE_HOME"]), workspace)
                    else:
                        FakeChatHandler.responses = [response, compact_response]
                        run_seed_programmatic(vibe, vibe_env, case, workspace)
                    FakeChatHandler.next_response = 0
                    if case.name in {
                        "tui_rewind_global_ctrl_p",
                        "tui_rewind_global_ctrl_p_prev",
                        "tui_rewind_global_ctrl_n",
                        "tui_rewind_global_alt_up",
                        "tui_rewind_global_alt_down",
                    }:
                        FakeChatHandler.responses = [response, second_response]
                        run_seed_two_programmatic(microvibe, micro_env, case, workspace)
                    elif case.name == "tui_resume_legacy_json":
                        seed_legacy_json_session(pathlib.Path(micro_env["VIBE_HOME"]), workspace)
                    elif case.name == "tui_resume_skips_invalid":
                        seed_invalid_newer_saved_session(pathlib.Path(micro_env["VIBE_HOME"]), workspace)
                    elif case.name == "tui_resume_same_end_time_mtime":
                        seed_same_end_time_sessions(pathlib.Path(micro_env["VIBE_HOME"]), workspace)
                    else:
                        FakeChatHandler.responses = [response, compact_response]
                        run_seed_programmatic(microvibe, micro_env, case, workspace)
                    vibe_raw = run_pty(
                        build_command(vibe, case.mode, microvibe=False),
                        vibe_env,
                        case,
                        workspace,
                    )
                    micro_raw = run_pty(
                        build_command(microvibe, case.mode, microvibe=True),
                        micro_env,
                        case,
                        workspace,
                    )
                    server.shutdown()
                vibe_config_text = config_projection_text(case.name, tmp_path, "vibe")
                micro_config_text = config_projection_text(case.name, tmp_path, "microvibe")
                vibe_history_text = history_projection_text(case.name, tmp_path, "vibe")
                micro_history_text = history_projection_text(case.name, tmp_path, "microvibe")
                vibe_editor_text = editor_projection_text(case.name, tmp_path, "vibe")
                micro_editor_text = editor_projection_text(case.name, tmp_path, "microvibe")
                vibe_clipboard_text = clipboard_projection_text(case.name, tmp_path, "vibe")
                micro_clipboard_text = clipboard_projection_text(case.name, tmp_path, "microvibe")
                vibe_session_text = session_projection_text(case.name, tmp_path, "vibe")
                micro_session_text = session_projection_text(case.name, tmp_path, "microvibe")
                vibe_side_effect_text = trust_file_projection(case.name, tmp_path, "vibe")
                micro_side_effect_text = trust_file_projection(case.name, tmp_path, "microvibe")
                # Skip the generic TUI runner below; this branch already produced raw output.
                pass
            elif case.name not in {"tui_initial_prompt", "tui_prompt_simple", "tui_copy_last_agent", "tui_copy_last_agent_xclip", "tui_prompt_history_up", "tui_prompt_history_up_down", "tui_prompt_history_persisted", "tui_prompt_multiline_ctrl_j", "tui_prompt_at_file", "tui_completion_slash", "tui_completion_slash_nav_enter", "tui_completion_path_popup_list", "tui_completion_path_popup_ten", "tui_completion_path_dir_tab", "tui_completion_path_file", "tui_prompt_at_folder", "tui_prompt_at_image", "tui_prompt_at_image_no_vision", "tui_external_editor_input", "tui_external_editor_empty", "tui_scroll_shift_up", "tui_scroll_shift_up_down", "tui_prompt_read", "tui_prompt_read_expand_tool", "tui_prompt_read_expand_collapse_tool", "tui_prompt_bash", "tui_animation_bash_spinner", "tui_prompt_bash_allow", "tui_prompt_bash_allow_y", "tui_prompt_bash_allow_expand_tool", "tui_prompt_bash_allow_expand_collapse_tool", "tui_prompt_bash_allow_session", "tui_prompt_bash_always", "tui_prompt_bash_persisted_allow", "tui_prompt_bash_deny", "tui_prompt_bash_deny_n", "tui_prompt_file_tools", "tui_animation_write_file_spinner", "tui_animation_edit_spinner", "tui_prompt_file_tools_allow_write", "tui_prompt_file_tools_allow_edit", "tui_prompt_file_tools_expand_tool", "tui_prompt_todo", "tui_prompt_todo_empty", "tui_slash_skill", "tui_prompt_skill", "tui_prompt_skill_expand_tool", "tui_prompt_task", "tui_animation_task_spinner", "tui_prompt_task_allow_explore", "tui_prompt_task_allow_unknown", "tui_prompt_task_deny", "tui_prompt_web_fetch", "tui_prompt_web_fetch_expand_tool", "tui_animation_web_fetch_spinner", "tui_prompt_web_search", "tui_animation_web_search_spinner", "tui_prompt_web_search_expand_tool", "tui_prompt_question", "tui_animation_question_spinner", "tui_prompt_question_expand_tool", "tui_prompt_question_other", "tui_prompt_question_multi", "tui_prompt_question_multiselect", "tui_prompt_question_multiselect_other", "tui_prompt_exit_plan_auto", "tui_animation_exit_plan_spinner", "tui_prompt_exit_plan_default", "tui_prompt_exit_plan_no", "tui_prompt_exit_plan_editor"}:
                vibe_env = isolated_env("vibe", tmp_path)
                micro_env = isolated_env("microvibe", tmp_path)
                workspace = ROOT
                if case.name in {"tui_trust_prompt", "tui_trust_accept"}:
                    workspace = tmp_path / "trust-workspace"
                    workspace.mkdir(parents=True, exist_ok=True)
                    (workspace / "AGENTS.md").write_text("trust parity instructions\n", encoding="utf-8")
                if case.name in {"tui_trust_repo_prompt", "tui_trust_repo_accept", "tui_trust_repo_decline"}:
                    repo_root = tmp_path / "trust-repo"
                    workspace = repo_root / "packages" / "nested"
                    workspace.mkdir(parents=True, exist_ok=True)
                    (repo_root / ".git").mkdir(parents=True, exist_ok=True)
                    (repo_root / ".git" / "HEAD").write_text("ref: refs/heads/main\n", encoding="utf-8")
                    (repo_root / "AGENTS.md").write_text("repo trust parity instructions\n", encoding="utf-8")
                if case.name in {"tui_startup_agent_custom", "tui_cycle_mode_shift_tab_custom"}:
                    write_custom_primary_agent(pathlib.Path(vibe_env["HOME"]) / ".vibe")
                    write_custom_primary_agent(pathlib.Path(micro_env["HOME"]) / ".vibe")
                if case.name == "tui_proxy_setup_preserve_env":
                    seed_proxy_preserve_env(tmp_path, "vibe")
                    seed_proxy_preserve_env(tmp_path, "microvibe")
                if case.name == "tui_proxy_setup_unset_http":
                    seed_proxy_unset_env(tmp_path, "vibe")
                    seed_proxy_unset_env(tmp_path, "microvibe")
                if case.name in {"tui_mcp_configured", "tui_mcp_status_configured", "tui_connectors_configured"}:
                    seed_mcp_config(tmp_path)
                if case.name in {"tui_mcp_stdio_tools", "tui_mcp_stdio_tools_detail", "tui_mcp_enable_tool"}:
                    seed_mcp_stdio_tools_config(tmp_path)
                if case.name in {"tui_mcp_disable_server", "tui_mcp_disable_tool"}:
                    seed_mcp_stdio_tools_enabled_config(tmp_path)
                if case.name == "tui_mcp_enable_server":
                    seed_mcp_config(tmp_path)
                if case.name == "tui_ctrl_r_voice_enabled_no_insert":
                    seed_voice_enabled_config(tmp_path)
                vibe_raw = run_pty(
                    build_command(vibe, case.mode, microvibe=False),
                    vibe_env,
                    case,
                    workspace,
                )
                micro_raw = run_pty(
                    build_command(microvibe, case.mode, microvibe=True),
                    micro_env,
                    case,
                    workspace,
                )
        if case.name not in {
            "tui_resume_one",
            "tui_resume_legacy_json",
            "tui_resume_skips_invalid",
            "tui_resume_same_end_time_mtime",
            "tui_continue_one",
            "tui_resume_select_one",
            "tui_resume_delete_confirm",
            "tui_resume_delete_one",
            "tui_resume_rename_one",
            "tui_compact_one",
            "tui_rewind_one",
            "tui_rewind_select_one",
            "tui_rewind_global_ctrl_p",
            "tui_rewind_global_ctrl_p_prev",
            "tui_rewind_global_ctrl_n",
            "tui_rewind_global_alt_up",
            "tui_rewind_global_alt_down",
        }:
            vibe_config_text = config_projection_text(case.name, tmp_path, "vibe")
            micro_config_text = config_projection_text(case.name, tmp_path, "microvibe")
            vibe_history_text = history_projection_text(case.name, tmp_path, "vibe")
            micro_history_text = history_projection_text(case.name, tmp_path, "microvibe")
            vibe_editor_text = editor_projection_text(case.name, tmp_path, "vibe")
            micro_editor_text = editor_projection_text(case.name, tmp_path, "microvibe")
            vibe_clipboard_text = clipboard_projection_text(case.name, tmp_path, "vibe")
            micro_clipboard_text = clipboard_projection_text(case.name, tmp_path, "microvibe")
            vibe_session_text = session_projection_text(case.name, tmp_path, "vibe")
            micro_session_text = session_projection_text(case.name, tmp_path, "microvibe")
            if not case.mode.startswith("programmatic_"):
                vibe_side_effect_text = trust_file_projection(case.name, tmp_path, "vibe")
                micro_side_effect_text = trust_file_projection(case.name, tmp_path, "microvibe")
            if case.mode == "cli_setup":
                setup_vibe_text = setup_projection(case.name, vibe_raw, tmp_path, "vibe")
                setup_micro_text = setup_projection(case.name, micro_raw, tmp_path, "microvibe")
            if case.name in {"tui_proxy_setup_save_http", "tui_proxy_setup_preserve_env", "tui_proxy_setup_unset_http"}:
                proxy_vibe_text = proxy_env_projection(case.name, tmp_path, "vibe")
                proxy_micro_text = proxy_env_projection(case.name, tmp_path, "microvibe")

    animation_statuses = {
        "tui_animation_bash_spinner": "Running command",
        "tui_animation_write_file_spinner": "Writing file",
        "tui_animation_edit_spinner": "Editing files",
        "tui_animation_web_fetch_spinner": "Fetching URL",
        "tui_animation_web_search_spinner": "Searching the web",
        "tui_animation_task_spinner": "Running subagent",
        "tui_animation_question_spinner": "Waiting for user input",
        "tui_animation_exit_plan_spinner": "Waiting for user confirmation",
    }
    if case.name in animation_statuses:
        status = animation_statuses[case.name]
        vibe_text = spinner_animation_projection(vibe_raw, status) + vibe_config_text + vibe_history_text + vibe_editor_text + vibe_clipboard_text + vibe_session_text + vibe_side_effect_text
        micro_text = spinner_animation_projection(micro_raw, status) + micro_config_text + micro_history_text + micro_editor_text + micro_clipboard_text + micro_session_text + micro_side_effect_text
    elif case.name == "cli_check_upgrade_available":
        vibe_text = update_prompt_projection(vibe_raw)
        micro_text = update_prompt_projection(micro_raw)
    elif case.mode == "cli_setup":
        vibe_text = setup_vibe_text if setup_vibe_text is not None else setup_projection(case.name, vibe_raw, tmp_path, "vibe")
        micro_text = setup_micro_text if setup_micro_text is not None else setup_projection(case.name, micro_raw, tmp_path, "microvibe")
    elif case.name in {"tui_proxy_setup_save_http", "tui_proxy_setup_preserve_env", "tui_proxy_setup_unset_http"}:
        vibe_text = proxy_vibe_text if proxy_vibe_text is not None else proxy_env_projection(case.name, tmp_path, "vibe")
        micro_text = proxy_micro_text if proxy_micro_text is not None else proxy_env_projection(case.name, tmp_path, "microvibe")
    elif case.name in {"tui_trust_prompt", "tui_trust_repo_prompt"}:
        vibe_text = trust_prompt_projection(vibe_raw)
        micro_text = trust_prompt_projection(micro_raw)
    elif case.name in {"tui_trust_accept", "tui_trust_repo_accept", "tui_trust_repo_decline"}:
        vibe_text = trust_file_projection(case.name, tmp_path, "vibe")
        micro_text = trust_file_projection(case.name, tmp_path, "microvibe")
    elif case.name in {"tui_debug_command", "tui_debug_ctrl_backslash"}:
        vibe_text = debug_console_projection(case.name, vibe_raw) + vibe_config_text + vibe_history_text + vibe_editor_text + vibe_clipboard_text + vibe_session_text + vibe_side_effect_text
        micro_text = debug_console_projection(case.name, micro_raw) + micro_config_text + micro_history_text + micro_editor_text + micro_clipboard_text + micro_session_text + micro_side_effect_text
    elif case.mode in {"acp_help", "acp_version"}:
        vibe_text = normalize(vibe_raw)
        micro_text = normalize(micro_raw)
    elif case.mode == "acp_initialize":
        vibe_text = normalize_json_line(vibe_raw) + vibe_config_text + vibe_history_text + vibe_session_text + vibe_side_effect_text
        micro_text = normalize_json_line(micro_raw) + micro_config_text + micro_history_text + micro_session_text + micro_side_effect_text
    elif case.mode in {
        "acp_load_session",
        "acp_load_rich_session",
        "acp_load_replay_ids",
        "acp_prompt_simple",
        "acp_prompt_client_message_id",
        "acp_prompt_agent_thought",
        "acp_prompt_usage_accumulates",
        "acp_prompt_usage_cost",
        "acp_prompt_image",
        "acp_command_help",
        "acp_command_reload",
        "acp_command_compact_empty",
        "acp_command_compact_one",
        "acp_command_teleport_no_history",
        "acp_command_data_retention",
        "acp_command_proxy_help",
        "acp_command_proxy_set",
        "acp_command_proxy_unset",
        "acp_command_proxy_invalid",
        "acp_command_proxy_case",
        "acp_prompt_grep",
        "acp_permission_grep_allow",
        "acp_permission_grep_deny",
        "acp_permission_grep_cancelled",
        "acp_permission_grep_allow_always",
        "acp_permission_grep_allow_always_permanent",
        "acp_permission_bash_granular",
        "acp_permission_bash_granular_allow_always_permanent",
        "acp_fs_read",
        "acp_fs_read_range",
        "acp_fs_write",
        "acp_fs_edit",
        "acp_terminal_bash_allow",
        "acp_terminal_bash_nonzero",
        "acp_terminal_bash_none_exit",
        "acp_terminal_bash_timeout",
        "acp_tool_meta_web_fetch",
        "acp_tool_meta_web_search",
        "acp_tool_meta_skill",
        "acp_tool_meta_task",
        "acp_prompt_todo",
        "acp_prompt_todo_invalid",
        "acp_set_title_live_unsaved",
        "acp_set_title_saved",
        "acp_delete_saved",
        "acp_delete_missing",
        "acp_delete_saved_pointer",
        "acp_delete_exact_collision",
        "acp_delete_live_unsaved",
        "acp_delete_loaded_saved",
        "acp_auth_signout_dotenv",
        "acp_auth_signout_process_over_dotenv",
        "acp_authenticate_unsupported",
        "acp_authenticate_browser_unsupported",
        "acp_authenticate_browser_complete",
        "acp_authenticate_browser_unsupported_action",
        "acp_authenticate_delegated_start",
        "acp_authenticate_delegated_complete",
        "acp_authenticate_delegated_missing_attempt",
        "acp_authenticate_delegated_unknown_attempt",
        "acp_authenticate_delegated_unsupported_action",
        "acp_telemetry_notification",
        "acp_unknown_notification",
        "acp_trust_decision_cwd",
        "acp_trust_decision_repo",
        "acp_trust_decision_invalid",
        "acp_trust_decision_missing_session",
    }:
        vibe_text = normalize_acp_transcript(vibe_raw) + vibe_config_text + vibe_history_text + vibe_session_text + vibe_side_effect_text
        micro_text = normalize_acp_transcript(micro_raw) + micro_config_text + micro_history_text + micro_session_text + micro_side_effect_text
    elif case.mode.startswith("acp_"):
        vibe_text = normalize_acp_json_line(vibe_raw) + vibe_config_text + vibe_history_text + vibe_session_text + vibe_side_effect_text
        micro_text = normalize_acp_json_line(micro_raw) + micro_config_text + micro_history_text + micro_session_text + micro_side_effect_text
    elif is_tui_mode(case.mode):
        vibe_text = render_screen(vibe_raw) + raw_projection_text(case.name, vibe_raw) + vibe_config_text + vibe_history_text + vibe_editor_text + vibe_clipboard_text + vibe_session_text + vibe_side_effect_text
        micro_text = render_screen(micro_raw) + raw_projection_text(case.name, micro_raw) + micro_config_text + micro_history_text + micro_editor_text + micro_clipboard_text + micro_session_text + micro_side_effect_text
    elif case.mode.startswith("programmatic_"):
        output = programmatic_output_mode(case.mode)
        vibe_text = normalize_programmatic(vibe_raw, output) + vibe_session_text + vibe_side_effect_text
        micro_text = normalize_programmatic(micro_raw, output) + micro_session_text + micro_side_effect_text
    else:
        vibe_text = normalize(vibe_raw)
        micro_text = normalize(micro_raw)
    (OUT_DIR / f"{case.name}.vibe.raw").write_bytes(vibe_raw)
    (OUT_DIR / f"{case.name}.microvibe.raw").write_bytes(micro_raw)
    (OUT_DIR / f"{case.name}.vibe.txt").write_text(vibe_text)
    (OUT_DIR / f"{case.name}.microvibe.txt").write_text(micro_text)

    diff = "".join(
        difflib.unified_diff(
            vibe_text.splitlines(True),
            micro_text.splitlines(True),
            fromfile="vibe",
            tofile="microvibe",
        )
    )
    if bool(vibe_raw) != bool(micro_raw):
        diff = (
            f"raw output presence mismatch: vibe={len(vibe_raw)} bytes, "
            f"microvibe={len(micro_raw)} bytes\n"
            + diff
        )
    diff_path = OUT_DIR / f"{case.name}.diff"
    diff_path.write_text(diff)
    if diff:
        print(diff)
        print(f"wrote {diff_path}", file=sys.stderr)
        return 1
    print(f"{case.name}: parity OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
