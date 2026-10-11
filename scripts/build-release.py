#!/usr/bin/env python3
"""Build native release assets and assemble a complete, checksummed release."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import platform
import shutil
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("release", ROOT / "scripts/release.py")
RELEASE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(RELEASE)

TARGETS = {
    "aarch64-apple-darwin": ("darwin", "arm64", "mongodb-cli-darwin-arm64"),
    "x86_64-apple-darwin": ("darwin", "x64", "mongodb-cli-darwin-amd64"),
    "aarch64-unknown-linux-gnu": ("linux", "arm64", "mongodb-cli-linux-arm64"),
    "x86_64-unknown-linux-gnu": ("linux", "x64", "mongodb-cli-linux-amd64"),
    "aarch64-pc-windows-msvc": ("win32", "arm64", "mongodb-cli-windows-arm64.exe"),
    "x86_64-pc-windows-msvc": ("win32", "x64", "mongodb-cli-windows-amd64.exe"),
}


def digest(path: Path) -> str:
    checksum = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            checksum.update(block)
    return checksum.hexdigest()


def build(target: str, output: Path) -> None:
    version = RELEASE.validate_contents(RELEASE.read_files(ROOT))
    expected_os, expected_arch, asset = TARGETS[target]
    host_os = {"Darwin": "darwin", "Linux": "linux", "Windows": "win32"}.get(platform.system())
    host_arch = {"arm64": "arm64", "aarch64": "arm64", "x86_64": "x64", "amd64": "x64"}.get(platform.machine().lower())
    if (host_os, host_arch) != (expected_os, expected_arch):
        raise ValueError(f"{target} requires a matching native runner so the binary can be smoke-tested")
    command = ["cargo", "build", "--release", "--locked", "--target", target, "--message-format=json"]
    process = subprocess.Popen(command, cwd=ROOT, stdout=subprocess.PIPE, text=True)
    executable = None
    assert process.stdout is not None
    for line in process.stdout:
        event = json.loads(line)
        if event.get("reason") == "compiler-artifact" and event.get("target", {}).get("name") == "mongodb-cli" and event.get("executable"):
            executable = Path(event["executable"])
    if process.wait() != 0 or executable is None:
        raise RuntimeError(f"build failed for {target}")
    with tempfile.TemporaryDirectory(prefix="mongodb-release-smoke-") as temporary:
        environment = os.environ.copy()
        environment.pop("MONGODB_URI", None)
        environment.pop("MONGODB_CLI_ACCESS", None)
        environment["MONGODB_CLI_CONFIG"] = str(Path(temporary) / "empty.toml")
        actual = subprocess.check_output([str(executable), "--version"], env=environment, text=True).strip()
        if actual != f"mongodb-cli {version}":
            raise ValueError(f"binary version mismatch: {actual}")
        result = subprocess.run([str(executable), "--output", "json", "documents", "find"], env=environment, capture_output=True, text=True)
    if result.returncode != 2 or result.stdout or json.loads(result.stderr)["error"]["code"] != "validation":
        raise ValueError("native CLI failed the structured error smoke test")
    output.mkdir(parents=True, exist_ok=True)
    destination = output / asset
    shutil.copy2(executable, destination)
    destination.chmod(0o755)
    manifest = {"version": version, "target": target, "asset": asset, "sha256": digest(destination), "size": destination.stat().st_size}
    (output / f"{asset}.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(f"Built and verified {asset} ({version})")


def assemble(source: Path, output: Path) -> None:
    version = RELEASE.validate_contents(RELEASE.read_files(ROOT))
    assets = []
    for target, (_, _, asset) in TARGETS.items():
        binary = source / asset
        manifest = json.loads((source / f"{asset}.json").read_text(encoding="utf-8"))
        if manifest.get("version") != version or manifest.get("target") != target or manifest.get("asset") != asset:
            raise ValueError(f"release metadata mismatch for {asset}")
        if not 0 < binary.stat().st_size <= 64 * 1024 * 1024 or binary.stat().st_size != manifest.get("size") or digest(binary) != manifest.get("sha256"):
            raise ValueError(f"release checksum mismatch for {asset}")
        assets.append((asset, manifest["sha256"]))
    output.mkdir(parents=True, exist_ok=True)
    for asset, _ in assets:
        shutil.copy2(source / asset, output / asset)
    (output / "checksums.txt").write_text("".join(f"{sha}  {asset}\n" for asset, sha in sorted(assets)), encoding="utf-8")
    shutil.copy2(ROOT / "LICENSE", output / "LICENSE")
    changelog = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
    headings = RELEASE.changelog_sections(changelog)
    current = headings[1]
    end = headings[2].start() if len(headings) > 2 else len(changelog)
    (output / "notes.md").write_text(changelog[current.end():end].strip() + "\n", encoding="utf-8")
    print(f"Assembled all {len(assets)} verified platforms for {version}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    native = commands.add_parser("build")
    native.add_argument("--target", required=True, choices=TARGETS)
    native.add_argument("--output", type=Path, default=ROOT / "dist/artifacts")
    bundle = commands.add_parser("assemble")
    bundle.add_argument("--input", type=Path, default=ROOT / "dist/artifacts")
    bundle.add_argument("--output", type=Path, default=ROOT / "dist/release")
    args = parser.parse_args()
    if args.command == "build":
        build(args.target, args.output)
    else:
        assemble(args.input, args.output)


if __name__ == "__main__":
    main()
