from __future__ import annotations

import importlib.util
import json
import re
import shutil
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "validate_agent_integration", ROOT / "scripts/validate-agent-integration.py"
)
VALIDATOR = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(VALIDATOR)


class AgentIntegrationValidationTest(unittest.TestCase):
    def fixture(self) -> tuple[tempfile.TemporaryDirectory, Path]:
        temporary = tempfile.TemporaryDirectory()
        root = Path(temporary.name)
        for relative in ["plugins", ".agents", ".claude-plugin", "tests/skill"]:
            source = ROOT / relative
            destination = root / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copytree(source, destination)
        return temporary, root

    def test_live_tree_is_valid(self) -> None:
        self.assertEqual(VALIDATOR.validate(ROOT), [])

    def test_rejects_each_missing_exact_install_command(self) -> None:
        commands = [
            "mongodb-cli skills install",
            "mongodb-cli skills install --target codex",
            "mongodb-cli skills install --target claude",
        ]
        for command in commands:
            with self.subTest(command=command):
                temporary, root = self.fixture()
                try:
                    replacement = (
                        "mongodb-cli skills installer"
                        if command == "mongodb-cli skills install"
                        else f"{command} extra"
                    )
                    self.replace(root, command, replacement)
                    self.assertIn(
                        f"skill missing required install command: {command}",
                        VALIDATOR.validate(root),
                    )
                finally:
                    temporary.cleanup()

    def test_rejects_broken_skill_and_manifest_contracts(self) -> None:
        mutations = [
            ("missing name", lambda root: self.replace(root, "name: mongodb-cli", "name:")),
            ("name mismatch", lambda root: self.replace(root, "name: mongodb-cli", "name: other")),
            ("long description", self.long_description),
            ("missing command", lambda root: self.replace(root, "mongodb-cli ", "observe ")),
            (
                "missing doctor workflow",
                lambda root: self.replace(root, "mongodb-cli doctor", "mongodb-cli inspect"),
            ),
            (
                "missing installer workflow",
                lambda root: self.replace(
                    root, "mongodb-cli skills install", "mongodb-cli plugin install"
                ),
            ),
            (
                "forbidden uri flag",
                lambda root: self.append_skill(
                    root, "\nmongodb-cli --uri mongodb://db.example.com documents find a b\n"
                ),
            ),
            (
                "multiline forbidden uri flag",
                lambda root: self.append_skill(
                    root, "\nmongodb-cli documents find a b \\\n  --uri=mongodb://db.example.com\n"
                ),
            ),
            (
                "credentialed connection string",
                lambda root: self.append_skill(
                    root, "\nexport MONGODB_URI='mongodb://admin:hunter2@db.example.com/app'\n"
                ),
            ),
            (
                "missing escalation rule",
                lambda root: self.replace(
                    root, "Never change the profile's access", "Change the profile's access"
                ),
            ),
            (
                "discarded doctor report",
                lambda root: self.append_skill(
                    root, "\nmongodb-cli doctor --output json >/dev/null\n"
                ),
            ),
            ("version mismatch", self.version_mismatch),
            ("missing target", self.missing_target),
            ("duplicate Claude components", self.duplicate_claude_components),
            ("missing MCP deferral", self.missing_mcp_deferral),
            ("implicit invocation disabled", self.implicit_invocation_disabled),
            ("unimplemented scenario expectation", self.unimplemented_scenario),
        ]
        for label, mutate in mutations:
            with self.subTest(label=label):
                temporary, root = self.fixture()
                try:
                    mutate(root)
                    self.assertTrue(VALIDATOR.validate(root), label)
                finally:
                    temporary.cleanup()

    def test_rejects_broken_ported_skills(self) -> None:
        credentialed = "\nexport MONGODB_URI='mongodb://admin:hunter2@db.example.com/app'\n"
        mutations = [
            (
                "mongodb-cli-connection/SKILL.md",
                lambda text: "\n".join(
                    line for line in text.splitlines() if "MongoDB's agent-skills" not in line
                ),
                "mongodb-cli-connection: ported skill lacks attribution to MongoDB's agent-skills"
                " (Apache-2.0) near the top",
            ),
            (
                "mongodb-cli-connection/SKILL.md",
                lambda text: text.replace("license: Apache-2.0", "license: MIT"),
                "mongodb-cli-connection: ported skill frontmatter requires license: Apache-2.0",
            ),
            (
                "mongodb-cli-schema-design/SKILL.md",
                lambda text: text.replace(
                    "name: mongodb-cli-schema-design", "name: mongodb-schema-design"
                ),
                "mongodb-cli-schema-design: skill name/directory mismatch:"
                " 'mongodb-schema-design' != 'mongodb-cli-schema-design'",
            ),
            (
                "mongodb-cli-setup/SKILL.md",
                lambda text: re.sub(r"(?m)^description: .*$", "description:", text),
                "mongodb-cli-setup: skill frontmatter requires description",
            ),
            (
                "mongodb-cli-setup/SKILL.md",
                lambda text: text.replace("description: ", "description: " + "x" * 1100, 1),
                "mongodb-cli-setup: skill description exceeds 400 characters",
            ),
            (
                "mongodb-cli-connection/references/monitoring-guide.md",
                lambda text: text + credentialed,
                "mongodb-cli-connection/references/monitoring-guide.md:"
                " contains a connection string with credentials",
            ),
            (
                "mongodb-cli-query-optimizer/SKILL.md",
                lambda text: text
                + "\nmongodb-cli --uri mongodb://db.example.com explain find a b\n",
                "mongodb-cli-query-optimizer/SKILL.md: contains forbidden --uri argument",
            ),
            (
                "mongodb-cli-search-and-ai/references/vector-search.md",
                lambda text: text + "\nmongodb-cli doctor --output json >/dev/null\n",
                "mongodb-cli-search-and-ai/references/vector-search.md:"
                " discards the doctor report instead of handling failure stdout",
            ),
        ]
        for relative, change, expected in mutations:
            with self.subTest(expected=expected):
                temporary, root = self.fixture()
                try:
                    path = root / "plugins/mongodb-cli/skills" / relative
                    path.write_text(change(path.read_text(encoding="utf-8")), encoding="utf-8")
                    self.assertEqual(VALIDATOR.validate(root), [expected])
                finally:
                    temporary.cleanup()

    @staticmethod
    def skill(root: Path) -> Path:
        return root / "plugins/mongodb-cli/skills/mongodb-cli/SKILL.md"

    def replace(self, root: Path, old: str, new: str) -> None:
        path = self.skill(root)
        path.write_text(path.read_text().replace(old, new), encoding="utf-8")

    def append_skill(self, root: Path, value: str) -> None:
        path = self.skill(root)
        path.write_text(path.read_text() + value, encoding="utf-8")

    def long_description(self, root: Path) -> None:
        self.replace(root, "description: ", "description: " + "x" * 1100)

    @staticmethod
    def version_mismatch(root: Path) -> None:
        path = root / "plugins/mongodb-cli/.claude-plugin/plugin.json"
        value = json.loads(path.read_text())
        value["version"] = "9.9.9"
        path.write_text(json.dumps(value), encoding="utf-8")

    @staticmethod
    def missing_target(root: Path) -> None:
        path = root / ".claude-plugin/marketplace.json"
        value = json.loads(path.read_text())
        value["plugins"][0]["source"] = "./missing"
        path.write_text(json.dumps(value), encoding="utf-8")

    @staticmethod
    def duplicate_claude_components(root: Path) -> None:
        path = root / ".claude-plugin/marketplace.json"
        value = json.loads(path.read_text())
        value["plugins"][0]["strict"] = False
        value["plugins"][0]["skills"] = [
            "./plugins/mongodb-cli/skills/mongodb-cli"
        ]
        path.write_text(json.dumps(value), encoding="utf-8")

    def missing_mcp_deferral(self, root: Path) -> None:
        self.replace(root, "when the MongoDB MCP server is configured, prefer its tools", "always")

    @staticmethod
    def implicit_invocation_disabled(root: Path) -> None:
        path = root / "plugins/mongodb-cli/skills/mongodb-cli/agents/openai.yaml"
        path.write_text(
            path.read_text().replace(
                "allow_implicit_invocation: true", "allow_implicit_invocation: false"
            ),
            encoding="utf-8",
        )

    @staticmethod
    def unimplemented_scenario(root: Path) -> None:
        path = root / "tests/skill/scenarios.json"
        value = json.loads(path.read_text())
        value[0]["mustInclude"].append("invented-command")
        path.write_text(json.dumps(value), encoding="utf-8")


if __name__ == "__main__":
    unittest.main()
