#!/usr/bin/env python3
"""Compare generated Mistral and microvibe parity inventories."""

from __future__ import annotations

import argparse
import json
import pathlib
import subprocess
import sys


ROOT = pathlib.Path(__file__).resolve().parents[1]
TARGET = ROOT / "target" / "parity"


def run(command: list[str], cwd: pathlib.Path = ROOT) -> str:
    return subprocess.check_output(command, cwd=cwd, text=True)


def load_mistral(upstream: pathlib.Path) -> dict:
    output = TARGET / "mistral_inventory.json"
    subprocess.check_call(
        [
            sys.executable,
            str(ROOT / "dev" / "extract_mistral_inventory.py"),
            "--upstream",
            str(upstream),
            "--output",
            str(output),
        ],
        cwd=ROOT,
    )
    return json.loads(output.read_text())


def load_microvibe() -> dict:
    subprocess.check_call(["cargo", "build"], cwd=ROOT)
    raw = run([str(ROOT / "target" / "debug" / "microvibe"), "--dump-parity-inventory"])
    return json.loads(raw)


def command_projection(commands: list[dict]) -> dict[str, dict]:
    return {
        command["name"]: {
            "aliases": sorted(command["aliases"]),
            "description": command["description"],
            "handler": command["handler"],
            "exits": command["exits"],
            "availability": command.get("availability"),
        }
        for command in commands
    }


def flag_projection(flags: list[dict]) -> list[dict]:
    return sorted(
        [
            {
                "names": flag["names"],
                "help": flag.get("help") or "",
                "action": flag.get("action"),
                "dest": flag.get("dest"),
                "metavar": flag.get("metavar"),
                "choices": flag.get("choices"),
                "nargs": flag.get("nargs"),
            }
            for flag in flags
        ],
        key=lambda flag: tuple(flag["names"]),
    )


def tool_projection(tools: list[dict]) -> dict[str, str]:
    out = {}
    for tool in tools:
        name = tool.get("tool_name") or tool.get("name")
        out[name] = tool.get("description", "")
    return out


def mistral_tool_permission_projection(tools: list[dict]) -> dict[str, str | None]:
    return {
        tool.get("tool_name") or tool.get("name"): tool.get("permission")
        for tool in tools
    }


def microvibe_tool_permission_projection(permissions: list[dict]) -> dict[str, str | None]:
    return {
        permission["name"]: permission.get("permission")
        for permission in permissions
    }


def mistral_tool_arg_projection(tools: list[dict]) -> dict[str, dict[str, dict]]:
    projection: dict[str, dict[str, dict]] = {}
    for tool in tools:
        name = tool.get("tool_name") or tool.get("name")
        fields: dict[str, dict] = {}
        for arg in tool.get("args", []):
            field = {
                "type": arg.get("json_type"),
                "description": arg.get("description") or "",
                "required": bool(arg.get("required")),
            }
            if arg.get("default") is not None:
                field["default"] = arg.get("default")
            fields[arg["name"]] = field
        projection[name] = fields
    return projection


def microvibe_tool_arg_projection(tools: list[dict]) -> dict[str, dict[str, dict]]:
    projection: dict[str, dict[str, dict]] = {}
    for tool in tools:
        fields: dict[str, dict] = {}
        schema = tool.get("input_schema", {})
        properties = schema.get("properties", {})
        required = set(schema.get("required", []))
        for name, schema in properties.items():
            field = {
                "type": schema.get("type"),
                "description": schema.get("description") or "",
                "required": name in required,
            }
            if "default" in schema:
                field["default"] = schema["default"]
            fields[name] = field
        projection[tool["name"]] = fields
    return projection


def mistral_tool_result_projection(tools: list[dict]) -> dict[str, dict[str, dict]]:
    projection: dict[str, dict[str, dict]] = {}
    for tool in tools:
        name = tool.get("tool_name") or tool.get("name")
        fields: dict[str, dict] = {}
        for field in tool.get("result", []):
            item = {
                "type": field.get("json_type"),
                "description": field.get("description") or "",
                "required": bool(field.get("required")),
            }
            if field.get("default") is not None:
                item["default"] = field.get("default")
            fields[field["name"]] = item
        projection[name] = fields
    return projection


def microvibe_tool_result_projection(results: list[dict]) -> dict[str, dict[str, dict]]:
    projection: dict[str, dict[str, dict]] = {}
    for result in results:
        fields: dict[str, dict] = {}
        for field in result.get("fields", []):
            item = {
                "type": field.get("json_type"),
                "description": field.get("description") or "",
                "required": bool(field.get("required")),
            }
            if "default" in field:
                item["default"] = field["default"]
            fields[field["name"]] = item
        projection[result["name"]] = fields
    return projection


def mistral_tool_config_projection(tools: list[dict]) -> dict[str, dict[str, dict]]:
    projection: dict[str, dict[str, dict]] = {}
    for tool in tools:
        name = tool.get("tool_name") or tool.get("name")
        fields: dict[str, dict] = {}
        for field in tool.get("config", []):
            item = {
                "type": field.get("json_type"),
                "description": field.get("description") or "",
                "required": bool(field.get("required")),
            }
            if field.get("default") is not None:
                item["default"] = field.get("default")
            if field.get("default_factory") is not None:
                item["default_factory"] = field.get("default_factory")
            fields[field["name"]] = item
        projection[name] = fields
    return projection


def microvibe_tool_config_projection(configs: list[dict]) -> dict[str, dict[str, dict]]:
    projection: dict[str, dict[str, dict]] = {}
    for config in configs:
        fields: dict[str, dict] = {}
        for field in config.get("fields", []):
            item = {
                "type": field.get("json_type"),
                "description": field.get("description") or "",
                "required": bool(field.get("required")),
            }
            if "default" in field:
                item["default"] = field["default"]
            if "default_factory" in field:
                item["default_factory"] = field["default_factory"]
            fields[field["name"]] = item
        projection[config["name"]] = fields
    return projection


def binding_projection(bindings: list[dict]) -> list[dict]:
    return sorted(
        [
            {
                "file": binding.get("file"),
                "class": binding.get("class"),
                "key": binding.get("key"),
                "action": binding.get("action"),
                "description": binding.get("description"),
            }
            for binding in bindings
        ],
        key=lambda item: (
            str(item["file"]),
            str(item["class"]),
            str(item["key"]),
            str(item["action"]),
            str(item["description"]),
        ),
    )


def agent_projection(agents: list[dict]) -> dict[str, dict]:
    return {
        agent["name"]: {
            "display_name": agent.get("display_name", ""),
            "description": agent.get("description", ""),
            "safety": agent.get("safety", "neutral"),
            "agent_type": agent.get("agent_type", "agent"),
            "overrides": agent.get("overrides", {}),
            "install_required": bool(agent.get("install_required", False)),
        }
        for agent in agents
    }


def compare(label: str, expected, actual) -> list[str]:
    if expected == actual:
        return []
    return [
        f"{label} mismatch",
        f"expected: {json.dumps(expected, indent=2, sort_keys=True)}",
        f"actual:   {json.dumps(actual, indent=2, sort_keys=True)}",
    ]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--upstream", type=pathlib.Path, default=ROOT.parent / "mistral-vibe-upstream")
    args = parser.parse_args()

    mistral = load_mistral(args.upstream)
    microvibe = load_microvibe()

    errors: list[str] = []
    errors += compare(
        "commands",
        command_projection(mistral["commands"]),
        command_projection(microvibe["commands"]),
    )
    errors += compare(
        "cli flags",
        flag_projection(mistral["cli_flags"]),
        flag_projection(microvibe["cli_flags"]),
    )
    errors += compare(
        "tool names/descriptions",
        tool_projection(mistral["tools"]),
        tool_projection(microvibe["tools"]),
    )
    errors += compare(
        "tool permissions",
        mistral_tool_permission_projection(mistral["tools"]),
        microvibe_tool_permission_projection(microvibe["tool_permissions"]),
    )
    errors += compare(
        "tool arguments",
        mistral_tool_arg_projection(mistral["tools"]),
        microvibe_tool_arg_projection(microvibe["tools"]),
    )
    errors += compare(
        "tool results",
        mistral_tool_result_projection(mistral["tools"]),
        microvibe_tool_result_projection(microvibe["tool_results"]),
    )
    errors += compare(
        "tool configs",
        mistral_tool_config_projection(mistral["tools"]),
        microvibe_tool_config_projection(microvibe["tool_configs"]),
    )
    errors += compare(
        "agents",
        agent_projection(mistral["agents"]),
        agent_projection(microvibe["agents"]),
    )
    errors += compare(
        "TUI bindings",
        binding_projection(mistral["bindings"]),
        binding_projection(microvibe["bindings"]),
    )

    if errors:
        print("\n\n".join(errors))
        return 1

    print("inventory parity OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
