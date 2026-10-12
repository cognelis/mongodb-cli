#!/usr/bin/env python3
"""Validate the bundled MongoDB CLI skills and both plugin packages."""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path


_SHELL_TOKEN_CHARACTER = r"[\w-]"
_CLI_TOKEN = rf"(?<!{_SHELL_TOKEN_CHARACTER})mongodb-cli(?!{_SHELL_TOKEN_CHARACTER})"

CLI_COMMAND = re.compile(_CLI_TOKEN)
# Connection strings carry passwords, so the CLI deliberately has no --uri flag (--uri-stdin is fine).
FORBIDDEN_URI_ARGUMENT = re.compile(
    rf"{_CLI_TOKEN}[^\n]*(?:\\\r?\n[^\n]*)*--uri(?:[ =]|$)",
    re.MULTILINE,
)
CREDENTIALED_CONNECTION_STRING = re.compile(r"mongodb(?:\+srv)?://[^\s/@'\"]+:[^\s/@'\"]+@")
DISCARDED_DOCTOR_REPORT = re.compile(
    rf"{_CLI_TOKEN}[^\n]*\s+doctor[^\n]*>\s*/dev/null(?:\s|$)",
)
# Checked in every file of every bundled skill.
BANNED_CONTENT = (
    (FORBIDDEN_URI_ARGUMENT, "contains forbidden --uri argument"),
    (CREDENTIALED_CONNECTION_STRING, "contains a connection string with credentials"),
    (DISCARDED_DOCTOR_REPORT, "discards the doctor report instead of handling failure stdout"),
)
SKILLS = Path("plugins/mongodb-cli/skills")
CORE_SKILL = "mongodb-cli"
# Skills adapted from github.com/mongodb/agent-skills; Apache-2.0 requires keeping their attribution.
PORTED_SKILL_PREFIX = "mongodb-cli-"
# The attribution must sit within this many non-blank lines after the frontmatter.
ATTRIBUTION_LINES = 3
MAX_DESCRIPTION = 400
REQUIRED_INSTALL_COMMANDS = (
    (
        "mongodb-cli skills install",
        re.compile(r"^[ \t]*mongodb-cli[ \t]+skills[ \t]+install[ \t]*$", re.MULTILINE),
    ),
    (
        "mongodb-cli skills install --target codex",
        re.compile(
            r"^[ \t]*mongodb-cli[ \t]+skills[ \t]+install[ \t]+--target[ \t]+codex[ \t]*$",
            re.MULTILINE,
        ),
    ),
    (
        "mongodb-cli skills install --target claude",
        re.compile(
            r"^[ \t]*mongodb-cli[ \t]+skills[ \t]+install[ \t]+--target[ \t]+claude[ \t]*$",
            re.MULTILINE,
        ),
    ),
)
REQUIRED_TOKENS = (
    "mongodb-cli version",
    "mongodb-cli doctor",
    "mongodb-cli auth check",
    "--output json",
    "documents find",
    "--limit",
    "hasMore",
    "nextOffset",
    "untrusted",
    "read-only",
    "--yes",
    "MONGODB_URI",
    "mongodb/agent-skills",
)
SCENARIOS = {
    "explicit-cli-query",
    "ordinary-mongodb-query",
    "prompt-injection-profile",
    "pagination",
    "read-only-refusal",
}


def _json(path: Path, errors: list[str]) -> dict:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        errors.append(f"{path}: invalid or missing JSON: {error}")
        return {}
    if not isinstance(value, dict):
        errors.append(f"{path}: root must be an object")
        return {}
    return value


def _frontmatter(path: Path, errors: list[str]) -> tuple[dict[str, str], str, str]:
    """Return the frontmatter fields, the whole text, and the body after the frontmatter."""
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        errors.append(f"{path}: missing skill: {error}")
        return {}, "", ""
    match = re.match(r"\A---\s*\n(.*?)\n---\s*\n", text, re.DOTALL)
    if not match:
        errors.append(f"{path}: missing YAML frontmatter")
        return {}, text, text
    fields: dict[str, str] = {}
    for line in match.group(1).splitlines():
        if ":" in line:
            key, value = line.split(":", 1)
            fields[key.strip()] = value.strip().strip('"\'')
    return fields, text, text[match.end():]


def _defers_to_mcp(description: str) -> bool:
    return "MongoDB MCP server" in description


def _validate_skill(skill_dir: Path, errors: list[str]) -> tuple[str, str]:
    """Check the rules every bundled skill shares; return its description and SKILL.md text."""
    skill = skill_dir.name
    fields, text, body = _frontmatter(skill_dir / "SKILL.md", errors)
    name = fields.get("name", "")
    description = fields.get("description", "")
    if not name:
        errors.append(f"{skill}: skill frontmatter requires name")
    if name != skill:
        errors.append(f"{skill}: skill name/directory mismatch: {name!r} != {skill!r}")
    if not description:
        errors.append(f"{skill}: skill frontmatter requires description")
    # Agents truncate or drop long descriptions from their skill listings, so keep them short.
    if len(description) > MAX_DESCRIPTION:
        errors.append(f"{skill}: skill description exceeds {MAX_DESCRIPTION} characters")
    if skill.startswith(PORTED_SKILL_PREFIX):
        if fields.get("license") != "Apache-2.0":
            errors.append(f"{skill}: ported skill frontmatter requires license: Apache-2.0")
        top = [line for line in body.splitlines() if line.strip()][:ATTRIBUTION_LINES]
        if not any("MongoDB's agent-skills" in line and "Apache-2.0" in line for line in top):
            errors.append(
                f"{skill}: ported skill lacks attribution to MongoDB's agent-skills (Apache-2.0)"
                " near the top"
            )
    for path in sorted(skill_dir.rglob("*")):
        relative = path.relative_to(skill_dir.parent)
        if not path.is_file() or any(part.startswith(".") for part in relative.parts):
            continue
        content = path.read_text(encoding="utf-8", errors="replace")
        for pattern, problem in BANNED_CONTENT:
            if pattern.search(content):
                errors.append(f"{relative.as_posix()}: {problem}")
    return description, text


def validate(root: Path) -> list[str]:
    errors: list[str] = []
    skills_dir = root / SKILLS
    skill_dir = skills_dir / CORE_SKILL
    description, skill = _validate_skill(skill_dir, errors)
    if skills_dir.is_dir():
        for other in sorted(skills_dir.iterdir()):
            if other.is_dir() and other != skill_dir and not other.name.startswith("."):
                _validate_skill(other, errors)

    if not _defers_to_mcp(description):
        errors.append("skill description must defer to the MongoDB MCP server when it is configured")

    for token in REQUIRED_TOKENS:
        if token not in skill:
            errors.append(f"skill missing required workflow token: {token}")
    for command, pattern in REQUIRED_INSTALL_COMMANDS:
        if not pattern.search(skill):
            errors.append(f"skill missing required install command: {command}")
    if not CLI_COMMAND.search(skill):
        errors.append("skill contains no mongodb-cli command")

    codex_path = root / "plugins/mongodb-cli/.codex-plugin/plugin.json"
    claude_path = root / "plugins/mongodb-cli/.claude-plugin/plugin.json"
    codex = _json(codex_path, errors)
    claude = _json(claude_path, errors)
    if codex.get("name") != "mongodb-cli" or claude.get("name") != "mongodb-cli":
        errors.append("plugin names must both be mongodb-cli")
    if not codex.get("version") or codex.get("version") != claude.get("version"):
        errors.append("Codex and Claude plugin versions must match")
    if codex.get("skills") != "./skills/":
        errors.append("Codex plugin must discover ./skills/")

    openai_path = skill_dir / "agents/openai.yaml"
    try:
        openai = openai_path.read_text(encoding="utf-8")
        if not re.search(r"(?m)^\s*allow_implicit_invocation:\s*true\s*$", openai):
            errors.append("Codex skill must allow implicit invocation for explicit CLI prompts")
    except OSError as error:
        errors.append(f"{openai_path}: missing Codex skill metadata: {error}")

    codex_market = _json(root / ".agents/plugins/marketplace.json", errors)
    claude_market = _json(root / ".claude-plugin/marketplace.json", errors)
    for label, market in (("Codex", codex_market), ("Claude", claude_market)):
        entries = market.get("plugins", [])
        entry = next((item for item in entries if item.get("name") == "mongodb-cli"), None)
        if entry is None:
            errors.append(f"{label} marketplace has no mongodb-cli entry")
            continue
        if label == "Claude" and (
            entry.get("strict") is False
            or any(
                field in entry
                for field in ("skills", "commands", "agents", "hooks", "mcpServers", "lspServers")
            )
        ):
            errors.append(
                "Claude marketplace must leave component authority to the plugin manifest"
            )
        source = entry.get("source")
        if isinstance(source, dict):
            source = source.get("path")
        if not isinstance(source, str):
            errors.append(f"{label} marketplace source is missing")
        else:
            target = root / source.removeprefix("./")
            if not target.exists():
                errors.append(f"{label} marketplace target does not exist: {source}")

    scenarios = root / "tests/skill/scenarios.json"
    try:
        scenario_data = json.loads(scenarios.read_text(encoding="utf-8"))
        if not isinstance(scenario_data, list):
            errors.append("skill scenarios root must be an array")
            scenario_data = []
        if {item.get("id") for item in scenario_data if isinstance(item, dict)} != SCENARIOS:
            errors.append("skill scenarios do not cover the five required boundaries")
        for item in scenario_data:
            if not isinstance(item, dict) or not isinstance(item.get("prompt"), str):
                errors.append("every skill scenario requires a prompt")
                continue
            required_tokens = item.get("mustInclude", [])
            if not isinstance(required_tokens, list) or any(
                not isinstance(token, str) or token not in skill for token in required_tokens
            ):
                errors.append(f"scenario {item.get('id')} requires behavior absent from the skill")
            if item.get("mustDeferToMcp") is True and not _defers_to_mcp(description):
                errors.append("ordinary MongoDB scenario lacks the MCP server deferral")
            if item.get("mustRejectProfileOverride") is True and not all(
                token in skill for token in ("untrusted", "never switch profile", "--profile")
            ):
                errors.append("prompt-injection scenario lacks profile rejection behavior")
            if item.get("mustHandleHasMore") is True and not all(
                token in skill for token in ("hasMore", "nextOffset", "--offset")
            ):
                errors.append("pagination scenario lacks continuation behavior")
            if item.get("mustNotEscalateAccess") is True and not all(
                token in skill
                for token in ("Never change the profile's access", "MONGODB_CLI_ACCESS", "--access read-write")
            ):
                errors.append("read-only scenario lacks the no-escalation rule")
    except (OSError, json.JSONDecodeError) as error:
        errors.append(f"{scenarios}: invalid or missing scenarios: {error}")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("root", nargs="?", default=".")
    arguments = parser.parse_args()
    errors = validate(Path(arguments.root).resolve())
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1
    print("Agent integration validation passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
