# Contributing and releasing

## Version policy

`Cargo.toml`'s `workspace.package.version` is the canonical product version.
`mongodb-cli` and `mongodb-client` inherit it. Their entries in `Cargo.lock` and both
plugin manifests and `build/npm/package.json` must carry the same release version.

Use plain `MAJOR.MINOR.PATCH` versions for releases:

| Change | Version increment |
| --- | --- |
| Compatible bug fix, security fix, or released Skill correction | Patch |
| New user-visible capability | Minor |
| Incompatible CLI, config, JSON, or public Rust API change while below 1.0 | Minor, with migration notes |
| Incompatible public behavior at 1.0 or later | Major, with migration notes |
| Internal refactor, tests, or documentation with no shipped behavior change | Record under Unreleased when useful; no standalone release is required |

Choose the highest required increment in a batch. Bump once for the completed
batch, not for each intermediate edit or test run. `+codex.<timestamp>` is a local
development cache key, not a substitute for a product version increment; never
use it in a release commit. A changed plain plugin version also selects a fresh
plugin cache during installation.

The JSON envelope's `schemaVersion` is a separate compatibility contract. Do not
change it just because the product version changes. Describe breaking behavior
and required caller changes explicitly in the changelog.

The Claude marketplace's `metadata.version` versions the marketplace descriptor,
not the plugin. Update it only when releasing a descriptor/schema change; plugin
versions belong in the plugin manifests.

## Record changes as you work

Add concise, user-facing entries under `CHANGELOG.md`'s `Unreleased` section using
`Added`, `Changed`, `Fixed`, `Removed`, or `Security` as appropriate. Include
migration notes when commands, authentication, defaults, or public APIs change.
Do not rewrite a previously published release's history to hide a later change.

Keep CLI behavior, README examples, and the bundled Skills under
`plugins/mongodb-cli/skills/` in sync; the canonical Skill is
`plugins/mongodb-cli/skills/mongodb-cli/SKILL.md`. The Skills are embedded in the
executable, so rebuild before updating installed copies.

## Prepare a release

After the batch and its changelog entries are ready, choose the increment:

```sh
python3 scripts/release.py prepare minor
# Or: prepare patch / prepare major
python3 scripts/release.py check
```

`prepare` moves Unreleased notes into a dated release entry, updates the workspace,
lockfile, both plugin manifests, and the npm package, and leaves a fresh Unreleased section. It
uses the local calendar date; `--date YYYY-MM-DD` allows an explicit release date.
It rejects an empty Unreleased section, mismatched versions, and duplicate or
out-of-order release entries before changing files. It does not commit, tag,
push, publish, or install anything.

Use Python 3.9 or newer for the repository scripts. Run the checks before finalizing:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
node --test build/npm/install.test.js
python3 -m unittest discover -s tests -p 'test_*.py'
python3 scripts/validate-agent-integration.py
python3 scripts/release.py check
git diff --check
```

`cargo test` includes an end-to-end suite against a real server. It uses the disposable
server in `MONGODB_CLI_TEST_URI` when set, otherwise starts a temporary `mongod` from
`MONGODB_CLI_TEST_MONGOD` or `PATH`, and skips itself when neither exists. CI provides a
MongoDB service container. Never point `MONGODB_CLI_TEST_URI` at shared data: the suite
creates and drops its own database.

For npm distribution, also build the native executable and run
`npm --prefix build/npm run test:package`. This checks the tarball's file allowlist,
installs it into an isolated directory, and exercises the launcher with a verified
native-binary fixture. Public download verification remains required before publishing.
See [docs/releasing.md](docs/releasing.md) for the first npm publish and CI setup.

Review the version bump and migration notes together with the implementation.
When a release is requested, commit the reviewed changes and create an annotated
`vX.Y.Z` tag on that commit, using the matching CHANGELOG section as release notes.
A request to commit changes alone does not require creating a release tag. Do not
tag an older commit while the release changes are still uncommitted. Push tags or
publish packages only when those actions are requested.

## Refresh local installations

After verification, update the executable and its embedded Skills:

```sh
cargo install --path crates/mongodb-cli --locked
mongodb-cli version --output json
mongodb-cli skills install
```

For an already configured local Codex marketplace, refresh the plugin using its
new release version:

```sh
codex plugin add mongodb-cli@mongodb-cli
```

Verify that the installed Skills and active plugin cache match the canonical
source. New Codex threads pick up the refreshed plugin instructions. Claude Code
plugin-managed installations use their own plugin update flow; standalone Skills
are updated by `mongodb-cli skills install`.
