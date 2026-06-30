#!/usr/bin/env python3
"""Extract a parity inventory from a local Mistral Vibe checkout.

The output is intentionally mechanical: it is the contract microvibe must
implement before a feature can be considered parity-complete.
"""

from __future__ import annotations

import argparse
import ast
import json
import pathlib
import re
import operator
from typing import Any


DEFAULT_UPSTREAM = pathlib.Path("../mistral-vibe-upstream")


def literal(node: ast.AST) -> Any:
    try:
        return ast.literal_eval(node)
    except Exception:
        return None


def static_value(node: ast.AST | None, constants: dict[str, Any]) -> Any:
    if node is None:
        return None
    value = literal(node)
    if value is not None:
        return value
    if isinstance(node, ast.Name):
        return constants.get(node.id)
    if isinstance(node, ast.Attribute):
        if isinstance(node.value, ast.Name) and node.value.id in {"BuiltinAgentName", "AgentSafety", "AgentType"}:
            return node.attr.lower().replace("_", "-")
        return ast.unparse(node)
    if isinstance(node, ast.List):
        return [static_value(item, constants) for item in node.elts]
    if isinstance(node, ast.Tuple):
        return [static_value(item, constants) for item in node.elts]
    if isinstance(node, ast.Set):
        return sorted(static_value(item, constants) for item in node.elts)
    if isinstance(node, ast.Dict):
        return {
            static_value(key, constants): static_value(value, constants)
            for key, value in zip(node.keys, node.values, strict=False)
            if key is not None
        }
    if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
        return f"{node.func.id}()"
    if isinstance(node, ast.BinOp):
        left = static_value(node.left, constants)
        right = static_value(node.right, constants)
        ops = {
            ast.Add: operator.add,
            ast.Mult: operator.mul,
        }
        op = ops.get(type(node.op))
        if op is not None:
            try:
                return op(left, right)
            except Exception:
                return None
    return None


def string_value(node: ast.AST) -> str | None:
    value = literal(node)
    return value if isinstance(value, str) else None


def find_keyword(call: ast.Call, name: str) -> ast.AST | None:
    for keyword in call.keywords:
        if keyword.arg == name:
            return keyword.value
    return None


def parse_commands(root: pathlib.Path) -> list[dict[str, Any]]:
    path = root / "vibe/cli/commands.py"
    module = ast.parse(path.read_text())
    commands: list[dict[str, Any]] = []

    for node in ast.walk(module):
        if not isinstance(node, ast.Dict):
            continue
        for key, value in zip(node.keys, node.values, strict=False):
            command_name = string_value(key) if key is not None else None
            if not command_name or not isinstance(value, ast.Call):
                continue
            if not isinstance(value.func, ast.Name) or value.func.id != "Command":
                continue

            aliases_node = find_keyword(value, "aliases")
            aliases: list[str] = []
            if isinstance(aliases_node, ast.Call) and isinstance(aliases_node.func, ast.Name):
                if aliases_node.args:
                    aliases_literal = literal(aliases_node.args[0])
                    if isinstance(aliases_literal, (set, list, tuple)):
                        aliases = sorted(str(alias) for alias in aliases_literal)

            commands.append(
                {
                    "name": command_name,
                    "aliases": aliases,
                    "description": string_value(find_keyword(value, "description")) or "",
                    "handler": string_value(find_keyword(value, "handler")) or "",
                    "exits": bool(literal(find_keyword(value, "exits"))),
                    "availability": ast.unparse(find_keyword(value, "is_available"))
                    if find_keyword(value, "is_available") is not None
                    else None,
                }
            )

    return sorted(commands, key=lambda item: item["name"])


def parse_cli_flags(root: pathlib.Path) -> list[dict[str, Any]]:
    path = root / "vibe/cli/entrypoint.py"
    module = ast.parse(path.read_text())
    flags: list[dict[str, Any]] = []

    for node in ast.walk(module):
        if not isinstance(node, ast.Call):
            continue
        if not isinstance(node.func, ast.Attribute) or node.func.attr != "add_argument":
            continue
        names = [literal(arg) for arg in node.args]
        names = [name for name in names if isinstance(name, str)]
        if not names:
            continue
        help_node = find_keyword(node, "help")
        flags.append(
            {
                "names": names,
                "help": string_value(help_node) or "",
                "action": string_value(find_keyword(node, "action")),
                "dest": string_value(find_keyword(node, "dest")),
                "metavar": string_value(find_keyword(node, "metavar")),
                "choices": literal(find_keyword(node, "choices")),
                "nargs": literal(find_keyword(node, "nargs")),
            }
        )

    return flags


def class_attr_string(class_node: ast.ClassDef, attr: str) -> str | None:
    for stmt in class_node.body:
        if isinstance(stmt, ast.AnnAssign) and isinstance(stmt.target, ast.Name):
            if stmt.target.id == attr and stmt.value is not None:
                return string_value(stmt.value)
        if isinstance(stmt, ast.Assign):
            if any(isinstance(target, ast.Name) and target.id == attr for target in stmt.targets):
                return string_value(stmt.value)
    return None


def class_permission(class_node: ast.ClassDef) -> str | None:
    for stmt in class_node.body:
        if not isinstance(stmt, ast.AnnAssign) or not isinstance(stmt.target, ast.Name):
            continue
        if stmt.target.id != "permission" or stmt.value is None:
            continue
        value = ast.unparse(stmt.value)
        if value.startswith("ToolPermission."):
            return value.rsplit(".", 1)[1]
        return value
    return None


def matching_model_name(models: dict[str, Any], class_name: str, suffix: str) -> str | None:
    normalized_class = class_name.lower()
    for name in models:
        stem = name.removesuffix(suffix).removesuffix("Tool").lower()
        if stem and (stem in normalized_class or normalized_class in stem):
            return name
    return None


def module_constants(module: ast.Module) -> dict[str, Any]:
    constants: dict[str, Any] = {}
    for stmt in module.body:
        if not isinstance(stmt, (ast.Assign, ast.AnnAssign)):
            continue
        value = static_value(stmt.value, constants) if stmt.value is not None else None
        if value is None:
            continue
        targets = [stmt.target] if isinstance(stmt, ast.AnnAssign) else stmt.targets
        for target in targets:
            if isinstance(target, ast.Name):
                constants[target.id] = value
    return constants


def field_default(value: ast.AST | None, constants: dict[str, Any]) -> tuple[bool, Any, str | None]:
    if value is None:
        return False, None, None
    if isinstance(value, ast.Call):
        default_node = find_keyword(value, "default")
        if default_node is None:
            default_factory = find_keyword(value, "default_factory")
            if isinstance(default_factory, ast.Name):
                if default_factory.id == "list":
                    return True, [], None
                if default_factory.id == "dict":
                    return True, {}, None
                if default_factory.id == "set":
                    return True, [], None
                return True, None, default_factory.id
            return False, None, None
        return True, static_value(default_node, constants), None
    return True, static_value(value, constants), None


def json_type_for_annotation(annotation: str) -> str | list[str]:
    optional = "None" in [part.strip() for part in annotation.split("|")]
    base = annotation.replace(" | None", "").replace("None | ", "").strip()
    if base.startswith("list["):
        json_type: str | list[str] = "array"
    elif base == "str" or base.startswith("Literal["):
        json_type = "string"
    elif base == "int":
        json_type = "integer"
    elif base == "float":
        json_type = "number"
    elif base == "bool":
        json_type = "boolean"
    else:
        json_type = "object"
    return [json_type, "null"] if optional else json_type


def pydantic_fields(class_node: ast.ClassDef, constants: dict[str, Any]) -> list[dict[str, Any]]:
    fields = []
    for stmt in class_node.body:
        if not isinstance(stmt, ast.AnnAssign) or not isinstance(stmt.target, ast.Name):
            continue
        name = stmt.target.id
        if name.startswith("_"):
            continue
        annotation = ast.unparse(stmt.annotation)
        description = None
        has_default, default, default_factory = field_default(stmt.value, constants)
        if isinstance(stmt.value, ast.Call):
            description = string_value(find_keyword(stmt.value, "description"))
        field = {
            "name": name,
            "annotation": annotation,
            "json_type": json_type_for_annotation(annotation),
            "description": description,
            "required": not has_default,
            "default": default,
        }
        if default_factory is not None:
            field["default_factory"] = default_factory
        fields.append(field)
    return fields


def parse_tools(root: pathlib.Path) -> list[dict[str, Any]]:
    tool_dir = root / "vibe/core/tools/builtins"
    tools: list[dict[str, Any]] = []
    base_tool_names = {"BaseTool", "CancellableTool"}

    for path in sorted(tool_dir.glob("*.py")):
        if path.name == "__init__.py":
            continue
        module = ast.parse(path.read_text())
        constants = module_constants(module)
        class_defs = [node for node in module.body if isinstance(node, ast.ClassDef)]
        arg_models: dict[str, list[dict[str, Any]]] = {}
        result_models: dict[str, list[dict[str, Any]]] = {}
        config_models: dict[str, list[dict[str, Any]]] = {}
        permissions: dict[str, str] = {}
        for cls in class_defs:
            if cls.name.endswith("Args"):
                arg_models[cls.name] = pydantic_fields(cls, constants)
            elif cls.name.endswith("Result"):
                result_models[cls.name] = pydantic_fields(cls, constants)
            elif cls.name.endswith("Config"):
                config_models[cls.name] = [
                    field
                    for field in pydantic_fields(cls, constants)
                    if field["name"] != "permission"
                ]
                permission = class_permission(cls)
                if permission is not None:
                    permissions[cls.name] = permission

        for cls in class_defs:
            base_names = {ast.unparse(base) for base in cls.bases}
            has_run = any(
                isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef))
                and stmt.name == "run"
                for stmt in cls.body
            )
            inherits_tool = any(
                base == "BaseTool"
                or base.startswith("BaseTool[")
                or base == "CancellableTool"
                or base.startswith("CancellableTool[")
                for base in base_names
            )
            if not has_run or not inherits_tool:
                continue
            description = class_attr_string(cls, "description") or ""
            args_name = matching_model_name(arg_models, cls.name, "Args")
            result_name = matching_model_name(result_models, cls.name, "Result")
            config_name = matching_model_name(permissions, cls.name, "Config")
            tools.append(
                {
                    "module": path.stem,
                    "class": cls.name,
                    "tool_name": camel_to_tool_name(cls.name),
                    "description": description,
                    "permission": permissions.get(config_name or ""),
                    "args_model": args_name,
                    "args": arg_models.get(args_name or "", []),
                    "result_model": result_name,
                    "result": result_models.get(result_name or "", []),
                    "config_model": config_name,
                    "config": config_models.get(config_name or "", []),
                    "has_run": has_run,
                }
            )

    return tools


def camel_to_tool_name(class_name: str) -> str:
    name = re.sub(r"(Tool|CancellableTool)$", "", class_name)
    name = re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()
    aliases = {
        "read": "read",
        "write_file": "write_file",
        "ask_user_question": "ask_user_question",
        "exit_plan_mode": "exit_plan_mode",
    }
    return aliases.get(name, name)


def parse_bindings(root: pathlib.Path) -> list[dict[str, Any]]:
    bindings = []
    for path in sorted((root / "vibe/cli/textual_ui").rglob("*.py")):
        module = ast.parse(path.read_text())
        for cls in [node for node in ast.walk(module) if isinstance(node, ast.ClassDef)]:
            for node in ast.walk(cls):
                if not isinstance(node, ast.Call):
                    continue
                if not isinstance(node.func, ast.Name) or node.func.id != "Binding":
                    continue
                args = [literal(arg) for arg in node.args]
                bindings.append(
                    {
                        "file": str(path.relative_to(root)),
                        "class": cls.name,
                        "key": args[0] if len(args) > 0 else None,
                        "action": args[1] if len(args) > 1 else None,
                        "description": args[2] if len(args) > 2 else None,
                    }
                )
    return bindings


def parse_agents(root: pathlib.Path) -> list[dict[str, Any]]:
    path = root / "vibe/core/agents/models.py"
    module = ast.parse(path.read_text())
    constants = module_constants(module)
    agents: dict[str, dict[str, Any]] = {}

    def agent_from_call(call: ast.Call) -> dict[str, Any] | None:
        if not isinstance(call.func, ast.Name) or call.func.id != "AgentProfile":
            return None
        positional = [static_value(arg, constants) for arg in call.args]
        keywords = {keyword.arg: static_value(keyword.value, constants) for keyword in call.keywords}

        def value(name: str, index: int, default: Any = None) -> Any:
            if name in keywords:
                return keywords[name]
            return positional[index] if len(positional) > index else default

        name = value("name", 0)
        if not isinstance(name, str):
            return None
        return {
            "name": name,
            "display_name": value("display_name", 1, ""),
            "description": value("description", 2, ""),
            "safety": value("safety", 3, "neutral"),
            "agent_type": value("agent_type", 4, "agent"),
            "overrides": value("overrides", 5, {}),
            "install_required": bool(value("install_required", 6, False)),
        }

    for node in module.body:
        if not isinstance(node, ast.Assign):
            continue
        if not isinstance(node.value, ast.Call):
            continue
        agent = agent_from_call(node.value)
        if agent is not None:
            agents[agent["name"]] = agent

    builtin_names: list[str] = []
    for node in module.body:
        if not isinstance(node, (ast.Assign, ast.AnnAssign)):
            continue
        targets = [node.target] if isinstance(node, ast.AnnAssign) else node.targets
        if not any(isinstance(target, ast.Name) and target.id == "BUILTIN_AGENTS" for target in targets):
            continue
        if not isinstance(node.value, ast.Dict):
            continue
        for key in node.value.keys:
            name = static_value(key, constants) if key is not None else None
            if isinstance(name, str):
                builtin_names.append(name)

    if builtin_names:
        return sorted(
            [agents[name] for name in builtin_names if name in agents],
            key=lambda agent: agent["name"],
        )
    return sorted(agents.values(), key=lambda agent: agent["name"])


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--upstream", type=pathlib.Path, default=DEFAULT_UPSTREAM)
    parser.add_argument("--output", type=pathlib.Path, default=pathlib.Path("target/parity/mistral_inventory.json"))
    args = parser.parse_args()

    root = args.upstream.resolve()
    inventory = {
        "upstream": str(root),
        "commands": parse_commands(root),
        "cli_flags": parse_cli_flags(root),
        "tools": parse_tools(root),
        "agents": parse_agents(root),
        "bindings": parse_bindings(root),
    }

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(inventory, indent=2, sort_keys=True) + "\n")
    print(args.output)
    print(
        f"{len(inventory['commands'])} commands, "
        f"{len(inventory['cli_flags'])} CLI flags, "
        f"{len(inventory['tools'])} tools, "
        f"{sum(1 for tool in inventory['tools'] if tool.get('permission'))} tool permissions, "
        f"{sum(1 for tool in inventory['tools'] if tool.get('config'))} tool configs, "
        f"{sum(1 for tool in inventory['tools'] if tool.get('result_model'))} tool result models, "
        f"{len(inventory['agents'])} agents, "
        f"{len(inventory['bindings'])} bindings"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
