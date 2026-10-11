#!/usr/bin/env python3
"""Prepare and validate synchronized CLI, crate, plugin, and changelog releases."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import re
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
PACKAGES = ("mongodb-cli", "mongodb-client")
PLUGINS = (
    "plugins/mongodb-cli/.codex-plugin/plugin.json",
    "plugins/mongodb-cli/.claude-plugin/plugin.json",
)
NPM_MANIFEST = "build/npm/package.json"
FILES = ("Cargo.toml", "Cargo.lock", "CHANGELOG.md", *PLUGINS, NPM_MANIFEST)
MANIFESTS = tuple(f"crates/{name}/Cargo.toml" for name in PACKAGES)
SEMVER = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")
HEADINGS = re.compile(r"^## \[([^\]\n]+)\](?: - ([^\n]+))?$", re.MULTILINE)


class ReleaseError(ValueError):
    pass


def version_tuple(version: str) -> tuple[int, int, int]:
    match = SEMVER.fullmatch(version)
    if not match:
        raise ReleaseError(f"expected a plain MAJOR.MINOR.PATCH version, got {version!r}")
    return tuple(int(part) for part in match.groups())


def increment(version: str, kind: str) -> str:
    major, minor, patch = version_tuple(version)
    if kind == "major":
        return f"{major + 1}.0.0"
    if kind == "minor":
        return f"{major}.{minor + 1}.0"
    if kind == "patch":
        return f"{major}.{minor}.{patch + 1}"
    raise ReleaseError(f"unknown increment: {kind}")


def section(text: str, name: str) -> tuple[int, int]:
    matches = list(re.finditer(r"^\[" + re.escape(name) + r"\][ \t]*(?:#.*)?$", text, re.MULTILINE))
    if len(matches) != 1:
        raise ReleaseError(f"expected exactly one [{name}] section")
    start = matches[0].end()
    following = re.search(r"^\[", text[start:], re.MULTILINE)
    return start, start + following.start() if following else len(text)


def quoted_field(text: str, key: str, start: int, end: int) -> re.Match:
    pattern = re.compile(
        r"^[ \t]*" + re.escape(key) + r"[ \t]*=[ \t]*(?P<quote>[\"'])(?P<value>[^\"'\n]*)(?P=quote)[ \t]*(?:#.*)?$",
        re.MULTILINE,
    )
    matches = list(pattern.finditer(text, start, end))
    if len(matches) != 1:
        raise ReleaseError(f"expected exactly one quoted {key} in the TOML section")
    return matches[0]


def workspace_version(text: str) -> re.Match:
    return quoted_field(text, "version", *section(text, "workspace.package"))


def lock_versions(text: str) -> dict[str, re.Match]:
    headers = list(re.finditer(r"^\[\[package\]\][ \t]*$", text, re.MULTILINE))
    result = {}
    for index, heading in enumerate(headers):
        start = heading.end()
        end = headers[index + 1].start() if index + 1 < len(headers) else len(text)
        name = quoted_field(text, "name", start, end).group("value")
        if name in PACKAGES:
            if name in result:
                raise ReleaseError(f"duplicate lockfile package: {name}")
            result[name] = quoted_field(text, "version", start, end)
    if set(result) != set(PACKAGES):
        raise ReleaseError("Cargo.lock must contain both workspace packages")
    return result


def read_files(root: Path) -> dict[str, str]:
    return {name: (root / name).read_text(encoding="utf-8") for name in (*FILES, *MANIFESTS)}


def changelog_sections(text: str) -> list[re.Match]:
    headings = list(HEADINGS.finditer(text))
    if not headings or headings[0].group(1) != "Unreleased":
        raise ReleaseError("CHANGELOG.md must begin its entries with [Unreleased]")
    if sum(item.group(1) == "Unreleased" for item in headings) != 1:
        raise ReleaseError("CHANGELOG.md must contain exactly one [Unreleased] entry")
    if headings[0].group(2) is not None:
        raise ReleaseError("Unreleased must not have a release date")
    versions = [version_tuple(item.group(1)) for item in headings[1:]]
    if not versions or versions != sorted(set(versions), reverse=True):
        raise ReleaseError("release entries must be unique and ordered newest version first")
    for item in headings[1:]:
        if item.group(2):
            try:
                dt.date.fromisoformat(item.group(2))
            except ValueError as error:
                raise ReleaseError(f"invalid release date for {item.group(1)}") from error
    return headings


def validate_contents(contents: dict[str, str], preparing: bool = False) -> str:
    version = workspace_version(contents["Cargo.toml"]).group("value")
    version_tuple(version)
    for path in MANIFESTS:
        start, end = section(contents[path], "package")
        body = contents[path][start:end]
        if not re.search(r"^version\.workspace[ \t]*=[ \t]*true[ \t]*(?:#.*)?$", body, re.MULTILINE):
            raise ReleaseError(f"{path} must inherit workspace.package.version")
        if re.search(r"^version[ \t]*=", body, re.MULTILINE):
            raise ReleaseError(f"{path} must not override the workspace version")
    for name, match in lock_versions(contents["Cargo.lock"]).items():
        if match.group("value") != version:
            raise ReleaseError(f"Cargo.lock: {name} must be version {version}")
    plugin_versions = []
    for path in PLUGINS:
        manifest = json.loads(contents[path])
        if not isinstance(manifest, dict) or manifest.get("name") != "mongodb-cli":
            raise ReleaseError(f"{path} must describe the mongodb-cli plugin")
        actual = manifest.get("version")
        development_version = preparing and isinstance(actual, str) and re.fullmatch(
            re.escape(version) + r"\+codex\.[A-Za-z0-9.-]+", actual
        )
        if actual != version and not development_version:
            raise ReleaseError(f"{path} must use the product version {version} without a cachebuster")
        plugin_versions.append(actual)
    if len(set(plugin_versions)) != 1:
        raise ReleaseError("Codex and Claude plugin versions must match")
    package = json.loads(contents[NPM_MANIFEST])
    if not isinstance(package, dict) or package.get("name") != "@cognelis/mongodb-cli":
        raise ReleaseError("the npm package must be named @cognelis/mongodb-cli")
    if package.get("version") != version:
        raise ReleaseError(f"{NPM_MANIFEST} must use product version {version}")
    headings = changelog_sections(contents["CHANGELOG.md"])
    latest = headings[1]
    if latest.group(1) != version:
        raise ReleaseError(f"latest CHANGELOG release must match product version {version}")
    if not preparing and not latest.group(2):
        raise ReleaseError("the current release requires a YYYY-MM-DD date")
    end = headings[2].start() if len(headings) > 2 else len(contents["CHANGELOG.md"])
    if not re.search(r"^- \S", contents["CHANGELOG.md"][latest.end():end], re.MULTILINE):
        raise ReleaseError("the current release requires changelog entries")
    return version


def replace_values(text: str, matches: list[re.Match], value: str) -> str:
    for match in sorted(matches, key=lambda item: item.start(), reverse=True):
        text = text[:match.start("value")] + value + text[match.end("value"):]
    return text


def stage(path: Path, contents: str) -> Path:
    staged = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent, prefix=".release-", delete=False) as handle:
            staged = Path(handle.name)
            handle.write(contents)
            handle.flush()
        os.chmod(staged, path.stat().st_mode & 0o777)
        return staged
    except Exception:
        if staged is not None:
            staged.unlink()
        raise


def write_changes(root: Path, before: dict[str, str], after: dict[str, str]) -> None:
    staged = {}
    replaced = []
    try:
        for name in FILES:
            if after[name] == before[name]:
                continue
            if (root / name).read_text(encoding="utf-8") != before[name]:
                raise ReleaseError(f"{name} changed during release preparation")
            staged[name] = stage(root / name, after[name])
        for name, temporary in staged.items():
            if (root / name).read_text(encoding="utf-8") != before[name]:
                raise ReleaseError(f"{name} changed during release preparation")
            os.replace(temporary, root / name)
            replaced.append(name)
    except Exception as error:
        failures = []
        for name in reversed(replaced):
            try:
                temporary = stage(root / name, before[name])
                try:
                    os.replace(temporary, root / name)
                finally:
                    temporary.unlink(missing_ok=True)
            except OSError as rollback_error:
                failures.append(f"{name}: {rollback_error}")
        if failures:
            raise ReleaseError(f"release write failed ({error}); rollback also failed: {'; '.join(failures)}") from error
        raise
    finally:
        for temporary in staged.values():
            temporary.unlink(missing_ok=True)


def prepare(root: Path, kind: str, date: str) -> str:
    before = read_files(root)
    current = validate_contents(before, preparing=True)
    version = increment(current, kind)
    release_date = dt.date.fromisoformat(date)
    changelog = before["CHANGELOG.md"]
    headings = changelog_sections(changelog)
    unreleased, latest = headings[:2]
    notes = changelog[unreleased.end():latest.start()].strip()
    if not re.search(r"^- \S", notes, re.MULTILINE):
        raise ReleaseError("add change entries under Unreleased before preparing a release")
    if latest.group(2) and release_date < dt.date.fromisoformat(latest.group(2)):
        raise ReleaseError("the new release date must not precede the previous release")
    after = before.copy()
    after["Cargo.toml"] = replace_values(before["Cargo.toml"], [workspace_version(before["Cargo.toml"])], version)
    after["Cargo.lock"] = replace_values(before["Cargo.lock"], list(lock_versions(before["Cargo.lock"]).values()), version)
    for path in (*PLUGINS, NPM_MANIFEST):
        manifest = json.loads(before[path])
        # Preserve unrelated manifest formatting and metadata.
        old = re.escape(json.dumps(manifest["version"]))
        after[path], count = re.subn(r'("version"\s*:\s*)' + old, lambda match: match.group(1) + json.dumps(version), before[path])
        if count != 1:
            raise ReleaseError(f"expected one version field in {path}")
    after["CHANGELOG.md"] = (
        changelog[:unreleased.end()] + f"\n\n## [{version}] - {release_date.isoformat()}\n\n"
        + notes + "\n\n" + changelog[latest.start():]
    )
    validate_contents(after)
    write_changes(root, before, after)
    return version


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("check", help="Check release versions and changelog without writing files")
    release = commands.add_parser("prepare", help="Bump versions and archive Unreleased notes")
    release.add_argument("increment", choices=("patch", "minor", "major"))
    release.add_argument("--date", default=dt.date.today().isoformat(), help="Release date (YYYY-MM-DD; defaults to local date)")
    args = parser.parse_args()
    try:
        if args.command == "prepare":
            version = prepare(ROOT, args.increment, args.date)
            print(f"Prepared {version}. Review the diff and run the release checks before committing or publishing.")
        else:
            version = validate_contents(read_files(ROOT))
            print(f"Release metadata is consistent: {version}")
    except (OSError, ValueError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
