#!/usr/bin/env python3
"""Assert microvibe's Codex-style TUI line editing contract.

Some modern line-editing shortcuts are intentionally stronger than upstream
Vibe's Textual input widget in the PTY parity harness. This check verifies the
microvibe-only contract the user expects from a native CLI while strict Vibe
parity remains covered by dev/parity.py.
"""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import tempfile
import threading

from parity import (
    Case,
    FakeChatHandler,
    ROOT,
    ThreadingTCPServer,
    build_command,
    command_from_env,
    isolated_env,
    run_pty,
    write_session_configs,
)


OUT_DIR = ROOT / "target" / "input-contract"

RESPONSE = {
    "id": "chatcmpl_tui_input_contract",
    "object": "chat.completion",
    "created": 0,
    "model": "test-model",
    "choices": [
        {
            "index": 0,
            "message": {"role": "assistant", "content": "input contract ok"},
            "finish_reason": "stop",
        }
    ],
    "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
}

CASES = [
    (
        Case("ctrl_left_word", "tui", b"alpha beta gamma\x1b[1;5DX\x1b\r", settle=1.0, timeout=8.0),
        "alpha beta Xgamma",
    ),
    (
        Case("ctrl_right_word", "tui", b"alpha beta gamma\x01\x1b[1;5CX\x1b\r", settle=1.0, timeout=8.0),
        "alphaX beta gamma",
    ),
    (
        Case("alt_left_word", "tui", b"alpha beta gamma\x1b[1;3DX\x1b\r", settle=1.0, timeout=8.0),
        "alpha beta Xgamma",
    ),
    (
        Case("alt_right_word", "tui", b"alpha beta gamma\x01\x1b[1;3CX\x1b\r", settle=1.0, timeout=8.0),
        "alphaX beta gamma",
    ),
    (
        Case("ctrl_u_delete_line_start", "tui", b"alpha beta gamma\x1b[D\x1b[D\x15X\x1b\r", settle=1.0, timeout=8.0),
        "Xma",
    ),
    (
        Case("ctrl_k_delete_line_end", "tui", b"alpha beta gamma\x01\x1b[1;5C\x0bX\x1b\r", settle=1.0, timeout=8.0),
        "alphaX",
    ),
]


def extract_user_messages(requests: list[dict[str, object]]) -> list[str]:
    user_messages: list[str] = []
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
    return user_messages


def resolve_command(command: list[str]) -> list[str]:
    if not command:
        return command
    first = pathlib.Path(command[0])
    if first.is_absolute() or "/" not in command[0]:
        return command
    return [str((ROOT / first).resolve()), *command[1:]]


def run_case(microvibe: list[str], case: Case, expected: str) -> None:
    with tempfile.TemporaryDirectory(prefix="microvibe-input-contract-") as temp:
        tmp_path = pathlib.Path(temp)
        workspace = tmp_path / "workspace"
        workspace.mkdir(parents=True, exist_ok=True)

        with ThreadingTCPServer(("127.0.0.1", 0), FakeChatHandler) as server:
            port = int(server.server_address[1])
            FakeChatHandler.responses = [RESPONSE]
            FakeChatHandler.requests = []
            FakeChatHandler.next_response = 0

            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            write_session_configs(tmp_path, port)

            env = isolated_env("microvibe", tmp_path)
            env["VIBE_HOME"] = str(tmp_path / "microvibe" / "home" / ".vibe")
            raw = run_pty(build_command(microvibe, case.mode, microvibe=True), env, case, workspace)
            server.shutdown()

    messages = extract_user_messages(FakeChatHandler.requests)
    actual = messages[-1] if messages else None
    if actual != expected:
        OUT_DIR.mkdir(parents=True, exist_ok=True)
        (OUT_DIR / f"{case.name}.raw").write_bytes(raw)
        (OUT_DIR / f"{case.name}.requests.json").write_text(
            json.dumps(FakeChatHandler.requests, indent=2, sort_keys=True),
            encoding="utf-8",
        )
        raise SystemExit(
            f"TUI input contract failed for {case.name}: expected {expected!r}, got {actual!r}"
        )

    print(f"{case.name}: input contract OK")


def main() -> int:
    env = os.environ.copy()
    env.setdefault("CARGO_INCREMENTAL", "0")
    subprocess.run(["cargo", "build"], cwd=ROOT, env=env, check=True)

    microvibe = resolve_command(command_from_env("MICROVIBE_CMD", "./target/debug/microvibe"))
    for case, expected in CASES:
        run_case(microvibe, case, expected)
    print("TUI input contract OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
