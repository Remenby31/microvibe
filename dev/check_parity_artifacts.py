#!/usr/bin/env python3
"""Verify parity transcript artifacts after a parity run."""

from __future__ import annotations

import argparse
import pathlib
import sys

import parity


ROOT = pathlib.Path(__file__).resolve().parents[1]
TARGET = ROOT / "target" / "parity"
REQUIRED_SUFFIXES = (
    ".vibe.raw",
    ".microvibe.raw",
    ".vibe.txt",
    ".microvibe.txt",
)


def requested_cases(tier: str) -> list[str]:
    if tier == "fast":
        return parity.FAST_CASES
    if tier == "smoke":
        return parity.SMOKE_CASES
    return list(parity.CASES)


def artifact_case_name(path: pathlib.Path) -> str | None:
    return parity.artifact_case_name(path)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--tier", choices=["fast", "smoke", "all"], default="all")
    args = parser.parse_args()

    if not TARGET.exists():
        raise SystemExit(f"missing parity artifact directory: {TARGET}")

    valid_cases = set(parity.CASES)
    requested = requested_cases(args.tier)
    errors: list[str] = []

    diffs = sorted(path.name for path in TARGET.glob("*.diff"))
    if diffs:
        errors.append("unexpected parity diffs: " + ", ".join(diffs[:20]))

    stale = sorted(
        path.name
        for path in TARGET.iterdir()
        if (case_name := artifact_case_name(path)) is not None and case_name not in valid_cases
    )
    if stale:
        errors.append("stale parity artifacts: " + ", ".join(stale[:20]))

    missing: list[str] = []
    for case_name in requested:
        for suffix in REQUIRED_SUFFIXES:
            if not (TARGET / f"{case_name}{suffix}").exists():
                missing.append(f"{case_name}{suffix}")
    if missing:
        errors.append("missing parity artifacts: " + ", ".join(missing[:20]))

    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        return 1

    print(f"parity artifacts OK ({args.tier}: {len(requested)} cases)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
