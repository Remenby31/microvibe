#!/usr/bin/env python3
"""Assert TUI visual attributes that text parity intentionally normalizes.

The transcript parity harness compares final terminal text, but it strips or
normalizes ANSI styling. This check guards the visual contract that users notice
immediately: the Vibe banner/prompt colors must be emitted as real truecolor
SGR sequences, even when the developer shell has NO_COLOR set.
"""

from __future__ import annotations

import pathlib
import re
import subprocess

from parity import CSI_RE, Screen


ROOT = pathlib.Path(__file__).resolve().parents[1]
RAW = ROOT / "target" / "parity" / "default_tui_startup.microvibe.raw"
MODE_RAW = ROOT / "target" / "parity" / "tui_cycle_mode_shift_tab.microvibe.raw"


EXPECTED_RGB = {
    "orange": b"\x1b[38;2;255;130;5;49m",
    "foreground": b"\x1b[38;2;197;200;198;49m",
    "secondary": b"\x1b[38;2;104;160;179;49m",
    "muted": b"\x1b[38;2;134;136;135;49m",
}
MODE_SAFE_RGB = b"\x1b[38;2;63;185;80;49m"


def ensure_startup_raw() -> bytes:
    subprocess.run(["dev/parity.py", "--case", "default_tui_startup"], cwd=ROOT, check=True)
    return RAW.read_bytes()


def ensure_mode_raw() -> bytes:
    subprocess.run(["dev/parity.py", "--case", "tui_cycle_mode_shift_tab"], cwd=ROOT, check=True)
    return MODE_RAW.read_bytes()


def require(name: str, condition: bool) -> None:
    if not condition:
        raise SystemExit(f"TUI visual contract failed: {name}")


def petit_chat_frames(raw: bytes, rows: int = 36, cols: int = 120) -> list[str]:
    text = raw.decode("utf-8", "replace")
    screen = Screen(rows, cols)
    frames: list[str] = []

    def sample() -> None:
        lines = screen.text().splitlines()
        if len(lines) <= 23:
            return
        frame = "\n".join(lines[20:23])
        if not any("\u2800" <= char <= "\u28ff" for char in frame):
            return
        if not frames or frames[-1] != frame:
            frames.append(frame)

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
    return frames


def main() -> int:
    raw = ensure_startup_raw()
    mode_raw = ensure_mode_raw()
    require("modifyOtherKeys is not disabled before keyboard setup", b"\x1b[>4;0m" in raw)
    require("keyboard enhancement flags do not match Codex's Kitty contract", b"\x1b[>7u" in raw)
    require("prompt cursor is not explicitly visible", b"\x1b[?25h" in raw)

    for name, sequence in EXPECTED_RGB.items():
        require(f"missing {name} truecolor SGR", sequence in raw)

    brand = re.search(rb"\x1b\[1m\x1b\[38;2;255;130;5;49mMistral Vibe", raw)
    require("brand is not bold orange", brand is not None)

    prompt = re.search(rb"\x1b\[1m\x1b\[38;2;255;130;5;49m> ", raw)
    require("prompt marker is not bold orange", prompt is not None)

    frames = petit_chat_frames(raw)
    require("petit chat banner does not animate", len(frames) >= 3)
    require("agent mode border does not change color", MODE_SAFE_RGB in mode_raw)

    print("TUI visual contract OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
