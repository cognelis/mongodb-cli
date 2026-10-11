# Changelog

User-visible changes are recorded here. Versions follow the project policy in
[CONTRIBUTING.md](CONTRIBUTING.md).

## [Unreleased]

### Fixed

- On a terminal, usage errors and the help shown for a missing subcommand, such as
  `mongodb-cli` alone, keep their line breaks instead of printing as one line with
  escaped newlines.

## [0.1.0] - 2026-10-11

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
