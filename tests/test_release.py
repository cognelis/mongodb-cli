from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("release", ROOT / "scripts/release.py")
RELEASE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(RELEASE)


class ReleaseWorkflowTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        files = {
            "Cargo.toml": '[workspace.package]\nversion = "0.1.0"\nedition = "2024"\n[workspace.dependencies]\nserde = "1"\n',
            "Cargo.lock": '\n'.join(
                f'[[package]]\nname = "{name}"\nversion = "{version}"\n'
                for name, version in [("mongodb-cli", "0.1.0"), ("mongodb-client", "0.1.0"), ("serde", "1.0.9")]
            ),
            "CHANGELOG.md": '# Changelog\n\n## [Unreleased]\n\n### Fixed\n\n- Preserve 中文 and literal `\\n` in release notes.\n\n## [0.1.0] - 2026-07-23\n\n- Original notes.\n',
        }
        for name in RELEASE.PACKAGES:
            files[f"crates/{name}/Cargo.toml"] = f'[package]\nname = "{name}"\nversion.workspace = true\n'
        for path in RELEASE.PLUGINS:
            files[path] = json.dumps({"name": "mongodb-cli", "version": "0.1.0", "description": "keep me"}) + '\n'
        files[RELEASE.NPM_MANIFEST] = json.dumps({"name": "@cognelis/mongodb-cli", "version": "0.1.0"}) + '\n'
        for name, contents in files.items():
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(contents, encoding="utf-8")

    def replace(self, name: str, old: str, new: str) -> None:
        path = self.root / name
        path.write_text(path.read_text(encoding="utf-8").replace(old, new), encoding="utf-8")

    def test_prepares_all_artifacts_and_preserves_notes_and_dependencies(self) -> None:
        version = RELEASE.prepare(self.root, "minor", "2026-09-06")
        self.assertEqual(version, "0.2.0")
        contents = RELEASE.read_files(self.root)
        self.assertEqual(RELEASE.validate_contents(contents), version)
        self.assertEqual(json.loads(contents[RELEASE.NPM_MANIFEST])["version"], version)
        self.assertIn('name = "serde"\nversion = "1.0.9"', contents["Cargo.lock"])
        self.assertIn('serde = "1"', contents["Cargo.toml"])
        for name in RELEASE.PLUGINS:
            self.assertEqual(json.loads(contents[name]), {"name": "mongodb-cli", "version": "0.2.0", "description": "keep me"})
        self.assertIn('## [Unreleased]\n\n## [0.2.0] - 2026-09-06', contents["CHANGELOG.md"])
        self.assertIn('Preserve 中文 and literal `\\n`', contents["CHANGELOG.md"])
        self.assertIn('## [0.1.0] - 2026-07-23\n\n- Original notes.', contents["CHANGELOG.md"])
        before = contents.copy()
        with self.assertRaisesRegex(RELEASE.ReleaseError, "Unreleased"):
            RELEASE.prepare(self.root, "patch", "2026-09-07")
        self.assertEqual(RELEASE.read_files(self.root), before)

    def test_patch_minor_and_major_increments(self) -> None:
        for current, kind, expected in [
            ("0.1.9", "patch", "0.1.10"), ("0.1.9", "minor", "0.2.0"),
            ("0.9.2", "major", "1.0.0"), ("1.9.2", "major", "2.0.0"),
        ]:
            self.assertEqual(RELEASE.increment(current, kind), expected)
        for invalid in ["01.2.3", "1.2", "v1.2.3", "1.2.3+codex.test", "1.2.3-rc.1"]:
            with self.assertRaises(RELEASE.ReleaseError):
                RELEASE.increment(invalid, "patch")

    def test_rejects_metadata_drift_before_writing(self) -> None:
        original = RELEASE.read_files(self.root)
        mutations = [
            ("Cargo.lock", 'version = "0.1.0"', 'version = "0.9.0"'),
            (RELEASE.PLUGINS[0], '"version": "0.1.0"', '"version": "0.9.0"'),
            (RELEASE.NPM_MANIFEST, '"version": "0.1.0"', '"version": "0.9.0"'),
            (RELEASE.NPM_MANIFEST, '@cognelis/mongodb-cli', '@wrong/mongodb-cli'),
            (RELEASE.MANIFESTS[0], 'version.workspace = true', 'version = "0.1.0"'),
            ("CHANGELOG.md", '## [0.1.0]', '## [0.0.9]'),
            ("CHANGELOG.md", '## [Unreleased]', '## [Upcoming]'),
            ("CHANGELOG.md", '2026-07-23', 'not-a-date'),
            ("CHANGELOG.md", '- Original notes.', '- Original notes.\n\n## [0.1.0]\n\n- Duplicate.'),
            ("CHANGELOG.md", '- Original notes.', '- Original notes.\n\n## [0.2.0]\n\n- Out of order.'),
        ]
        for name, old, new in mutations:
            with self.subTest(name=name, new=new):
                for relative, text in original.items():
                    (self.root / relative).write_text(text, encoding="utf-8")
                self.replace(name, old, new)
                before = RELEASE.read_files(self.root)
                with self.assertRaises(RELEASE.ReleaseError):
                    RELEASE.prepare(self.root, "minor", "2026-09-06")
                self.assertEqual(RELEASE.read_files(self.root), before)

    def test_date_regression_and_empty_notes_leave_files_unchanged(self) -> None:
        before = RELEASE.read_files(self.root)
        with self.assertRaisesRegex(RELEASE.ReleaseError, "date"):
            RELEASE.prepare(self.root, "patch", "2026-01-01")
        self.assertEqual(RELEASE.read_files(self.root), before)
        self.replace("CHANGELOG.md", '- Preserve 中文 and literal `\\n` in release notes.', '')
        before = RELEASE.read_files(self.root)
        with self.assertRaisesRegex(RELEASE.ReleaseError, "Unreleased"):
            RELEASE.prepare(self.root, "minor", "2026-09-06")
        self.assertEqual(RELEASE.read_files(self.root), before)

    def test_converts_development_cache_versions_to_a_plain_release(self) -> None:
        for name in RELEASE.PLUGINS:
            self.replace(name, '"0.1.0"', '"0.1.0+codex.20260723044026"')
        self.replace("CHANGELOG.md", '## [0.1.0] - 2026-07-23', '## [0.1.0]')
        with self.assertRaises(RELEASE.ReleaseError):
            RELEASE.validate_contents(RELEASE.read_files(self.root))
        self.assertEqual(RELEASE.prepare(self.root, "minor", "2026-09-06"), "0.2.0")
        self.assertEqual(RELEASE.validate_contents(RELEASE.read_files(self.root)), "0.2.0")

    def test_write_failure_rolls_back_already_replaced_files(self) -> None:
        before = RELEASE.read_files(self.root)
        original_replace = RELEASE.os.replace
        calls = 0

        def fail_second_write(source, destination):
            nonlocal calls
            calls += 1
            if calls == 2:
                raise OSError("simulated lockfile write failure")
            return original_replace(source, destination)

        with patch.object(RELEASE.os, "replace", side_effect=fail_second_write):
            with self.assertRaisesRegex(OSError, "simulated"):
                RELEASE.prepare(self.root, "minor", "2026-09-06")
        self.assertEqual(RELEASE.read_files(self.root), before)
        self.assertEqual(list(self.root.rglob(".release-*")), [])


if __name__ == "__main__":
    unittest.main()
