#!/usr/bin/env python3
"""Fast pre-flight checks before the full parity matrix.

This is intentionally conservative: it verifies formatting, Python syntax,
Rust tests, inventory parity, then runs a curated parity tier. Use
`dev/parity.py --tier smoke --jobs 32` as the broader pre-final parity gate and
`dev/parity.py --all --jobs 32` as the final gate.
"""

from __future__ import annotations

import argparse
import os
import pathlib
import subprocess
import sys


ROOT = pathlib.Path(__file__).resolve().parents[1]


def run(command: list[str], *, env: dict[str, str] | None = None) -> None:
    print("+ " + " ".join(command), flush=True)
    subprocess.run(command, cwd=ROOT, env=env, check=True)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--jobs", type=int, default=32, help="Cargo and parity worker count")
    parser.add_argument("--no-inventory", action="store_true", help="skip generated inventory parity")
    parser.add_argument("--no-smoke", action="store_true", help="skip the parity tier")
    parser.add_argument(
        "--smoke-tier",
        choices=["fast", "smoke"],
        default="fast",
        help="parity tier to run after format/tests/inventory",
    )
    args = parser.parse_args()

    if args.jobs < 1:
        parser.error("--jobs must be at least 1")

    env = os.environ.copy()
    env.setdefault("CARGO_INCREMENTAL", "0")

    run(["cargo", "fmt", "--all", "--", "--check"], env=env)
    run(
        [
            "python3",
            "-m",
            "py_compile",
            "dev/parity.py",
            "dev/sitecustomize.py",
            "dev/check_parity_inventory.py",
            "dev/extract_mistral_inventory.py",
            "dev/quick_check.py",
        ],
        env=env,
    )
    run(
        [
            "cargo",
            "test",
            "--workspace",
            "--jobs",
            str(args.jobs),
            "--",
            f"--test-threads={args.jobs}",
        ],
        env=env,
    )
    if not args.no_inventory:
        run(["dev/check_parity_inventory.py"], env=env)
    if not args.no_smoke:
        run(["dev/parity.py", "--tier", args.smoke_tier, "--jobs", str(args.jobs)], env=env)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
