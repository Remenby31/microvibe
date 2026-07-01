#!/usr/bin/env python3
"""Run the full pre-release parity gate."""

from __future__ import annotations

import argparse
import os
import pathlib
import subprocess


ROOT = pathlib.Path(__file__).resolve().parents[1]


def run(command: list[str], *, env: dict[str, str]) -> None:
    print("+ " + " ".join(command), flush=True)
    subprocess.run(command, cwd=ROOT, env=env, check=True)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--jobs", type=int, default=32, help="Cargo and parity worker count")
    args = parser.parse_args()

    if args.jobs < 1:
        parser.error("--jobs must be at least 1")

    env = os.environ.copy()
    env.setdefault("CARGO_INCREMENTAL", "0")

    run(["dev/quick_check.py", "--jobs", str(args.jobs), "--smoke-tier", "smoke"], env=env)
    run(["dev/parity.py", "--all", "--jobs", str(args.jobs)], env=env)
    run(["dev/check_parity_artifacts.py", "--tier", "all"], env=env)
    run(["dev/check_tui_visual_contract.py"], env=env)
    run(["dev/check_tui_input_contract.py"], env=env)
    run(["git", "diff", "--check"], env=env)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
