from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("build_release", ROOT / "scripts/build-release.py")
BUILDER = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(BUILDER)


class ReleaseAssetsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.source = self.directory / "artifacts"
        self.source.mkdir()
        self.output = self.directory / "release"
        self.version = BUILDER.RELEASE.validate_contents(BUILDER.RELEASE.read_files(ROOT))
        for target, (_, _, asset) in BUILDER.TARGETS.items():
            binary = self.source / asset
            binary.write_bytes(f"fixture binary for {target}".encode())
            (self.source / f"{asset}.json").write_text(json.dumps({
                "version": self.version, "target": target, "asset": asset,
                "sha256": BUILDER.digest(binary), "size": binary.stat().st_size,
            }))

    def test_assembles_exact_platform_set_and_checksums(self) -> None:
        BUILDER.assemble(self.source, self.output)
        assets = {value[2] for value in BUILDER.TARGETS.values()}
        self.assertEqual({path.name for path in self.output.iterdir()}, assets | {"checksums.txt", "LICENSE", "notes.md"})
        for line in (self.output / "checksums.txt").read_text().splitlines():
            digest, name = line.split()
            self.assertEqual(BUILDER.digest(self.output / name), digest)
        self.assertNotIn("## [Unreleased]", (self.output / "notes.md").read_text())

    def test_missing_platform_or_corrupt_binary_stops_before_output(self) -> None:
        asset = next(iter(BUILDER.TARGETS.values()))[2]
        (self.source / asset).write_bytes(b"corrupt")
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            BUILDER.assemble(self.source, self.output)
        self.assertFalse(self.output.exists())
        (self.source / f"{asset}.json").unlink()
        with self.assertRaises(FileNotFoundError):
            BUILDER.assemble(self.source, self.output)
        self.assertFalse(self.output.exists())

    def test_foreign_target_is_rejected_before_cargo_runs(self) -> None:
        with patch.object(BUILDER.platform, "system", return_value="Darwin"), patch.object(BUILDER.platform, "machine", return_value="arm64"), patch.object(BUILDER.subprocess, "Popen") as cargo:
            with self.assertRaisesRegex(ValueError, "matching native runner"):
                BUILDER.build("x86_64-pc-windows-msvc", self.output)
            cargo.assert_not_called()


if __name__ == "__main__":
    unittest.main()
