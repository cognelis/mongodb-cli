# Changelog

User-visible changes are recorded here. Versions follow the project policy in
[CONTRIBUTING.md](CONTRIBUTING.md).

## [Unreleased]

### Added

- A choice of credential store: `config set credential-store keychain|security|file`,
  or `MONGODB_CLI_CREDENTIAL_STORE`. `security` uses macOS `/usr/bin/security`, so
  keychain approvals survive CLI upgrades. `file` keeps credentials in an owner-only
  file and needs confirmation in an interactive terminal. `--migrate` moves existing
  credentials to the new store.
- Connection strings without secrets, such as local servers and Atlas Local
  deployments, are kept in the config file and never touch a credential store.

### Changed

- A partial page adds a `meta.warnings` line with the next `--offset`, and `--raw`
  output writes warnings to stderr instead of dropping them.
- JSON usage errors carry clap's first line and tips instead of the whole usage
  text, and a positional argument that looks like JSON points to `--pipeline` or
  `--filter`.
- `doctor` reports where the connection string came from.
- The bundled skills trigger for MongoDB data questions unless the MongoDB MCP
  server is configured, with descriptions short enough for agents' skill lists.
  The core skill uses a production profile only when the request names production,
  answers questions about a whole environment from each of its profiles, never
  substitutes another environment's data for a refused command, runs `doctor` once
  per profile in a session, and adds a quick reference of common commands and their
  output shapes.

## [0.2.0] - 2026-10-12

### Added

- Profile descriptions: `profile add|update --description` records what an
  environment is for, and `profile list` and `profile show` return it. The bundled
  skills choose a profile by its name, description, and access, and ask the user
  when several profiles or none could match the request.

## [0.1.2] - 2026-10-12

### Changed

- Release binaries no longer include a symbol table, which makes them about a
  quarter smaller. Panic messages still name the source file and line; only
  `RUST_BACKTRACE` call stacks lose their function names.

### Fixed

- The npm installer no longer fails on slow connections. A binary download now
  fails only after 30 seconds without receiving data or 10 minutes in total,
  instead of having to finish within 30 seconds.

## [0.1.1] - 2026-10-12

### Changed

- npm releases are published from GitHub Actions through npm trusted publishing,
  so each version carries an npm provenance attestation.

## [0.1.0] - 2026-10-12

First release.

### Added

- Database commands covering the MongoDB MCP server 3.0.5 database tools:
  `databases list|stats|aggregate|drop`, `collections list|schema|stats|create|rename|drop`,
  `indexes list|create|drop` (classic, Atlas Search, and Vector Search),
  `documents find|count|export|aggregate|insert|update|delete`,
  `explain find|aggregate|count`, and `server info|logs`.
- `documents export`, which writes every matching document of a find or an
  aggregation as a relaxed or canonical Extended JSON array to stdout, or atomically
  to a file, without page or byte limits.
- Offset pagination with `meta.hasMore` and `meta.nextOffset`. Pages are capped at
  100 documents and 1 MiB by default (`--response-bytes-limit` and profile caps),
  and `meta.appliedLimits` names the cap that shortened a page. Finds and
  aggregations report a best-effort `meta.total`, and `$out`/`$merge` pipelines
  report `meta.writeTargets`.
- Named profiles whose connection strings are verified and stored in the
  operating-system keyring, and `MONGODB_URI` for CI. Profiles and environment
  connections are read-only by default, and single-host connection strings connect
  directly, as in mongosh.
- Profile safety settings: access mode, server-side JavaScript, index checks,
  document and byte caps, count time limits, and disabled and confirmation-required
  tools, with `MONGODB_CLI_*` equivalents for `MONGODB_URI` connections. Loosening a
  profile requires confirmation in an interactive terminal.
- Checks before any connection: read-only refusals, server-side JavaScript
  (`$where`, `$function`, `$accumulator`) refused by default, disabled and
  confirmation-required tools matched by MCP tool name, command, operation type,
  or category, and a required `--filter` for updates and deletes.
- A global `--yes` that confirms destructive or billable operations, and a global
  `--require-index` that refuses queries that would scan the whole collection.
- Guided `init`, ordered `doctor` diagnostics, `auth login|check|logout`, and a
  warning when a read-only connection's database user has write-capable roles.
- Atlas tools gating: the `atlas`, `local`, and `knowledge` groups and the `--atlas`
  flag of `auth login|logout` are disabled by default, hidden from help and
  completion, and enabled with `config set atlas-tools enabled` or
  `MONGODB_CLI_ATLAS_TOOLS`.
- `atlas` commands covering the MCP server's 22 Atlas tools: organizations,
  projects, clusters (including Performance Advisor and temporary-user `connect`),
  the IP access list, database users, alerts, regions, sample data, and Atlas Stream
  Processing. Every change requires read-write access and `--yes`.
- Atlas service-account credentials stored with `auth login --atlas` in the keyring,
  or read from `MONGODB_ATLAS_CLIENT_ID` and `MONGODB_ATLAS_CLIENT_SECRET`. Secrets
  are read only from stdin, files, or masked prompts and are never printed, and
  `doctor` checks Atlas credentials while Atlas tools are enabled.
- `local deployments list|create|delete|connect` for Atlas Local in Docker, using
  the MCP server's `atlas-local` library; `connect` saves the deployment as a
  profile without printing its connection string.
- `knowledge sources|search` for the MongoDB Assistant knowledge base.
- Nine Agent Skills, installed for Codex and Claude Code by `skills install` and
  `init`: the core `mongodb-cli` skill and eight topic skills adapted from MongoDB's
  agent-skills (Apache-2.0) to drive the CLI. Codex and Claude Code plugin manifests
  package the same skills.
- A stable JSON envelope with relaxed Extended JSON documents, human-readable
  tables, stable exit codes, shell completion, and npm and native release tooling.
