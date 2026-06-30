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


ROOT = pathlib.Path(__file__).resolve().parents[1]
RAW = ROOT / "target" / "parity" / "default_tui_startup.microvibe.raw"


EXPECTED_RGB = {
    "orange": b"\x1b[38;2;255;130;5;49m",
    "foreground": b"\x1b[38;2;197;200;198;49m",
    "secondary": b"\x1b[38;2;104;160;179;49m",
    "muted": b"\x1b[38;2;134;136;135;49m",
}


def ensure_startup_raw() -> bytes:
    if not RAW.exists():
        subprocess.run(["dev/parity.py", "--case", "default_tui_startup"], cwd=ROOT, check=True)
    return RAW.read_bytes()


def require(name: str, condition: bool) -> None:
    if not condition:
        raise SystemExit(f"TUI visual contract failed: {name}")


def main() -> int:
    raw = ensure_startup_raw()
    for name, sequence in EXPECTED_RGB.items():
        require(f"missing {name} truecolor SGR", sequence in raw)

    brand = re.search(rb"\x1b\[1m\x1b\[38;2;255;130;5;49mMistral Vibe", raw)
    require("brand is not bold orange", brand is not None)

    prompt = re.search(rb"\x1b\[1m\x1b\[38;2;255;130;5;49m> ", raw)
    require("prompt marker is not bold orange", prompt is not None)

    print("TUI visual contract OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
