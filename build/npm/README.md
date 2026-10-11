# @cognelis/mongodb-cli

MongoDB queries, aggregations, schema inspection, guarded writes, read-only
profiles, connection diagnostics, and shell completion for terminals, scripts,
and coding agents.

Derived from the official [MongoDB MCP server](https://github.com/mongodb-js/mongodb-mcp-server)
(`mongodb-mcp-server`). The MCP server does not support multiple profiles: its
read-only mode applies to the whole server. Its tools were therefore
re-implemented by hand as this CLI, whose named profiles each carry their own
access mode, so a test profile can allow writes while production stays read-only.

```sh
npm install -g @cognelis/mongodb-cli
mongodb-cli version --output json
mongodb-cli init
```

The npm package installs a prebuilt native executable. No Rust toolchain is
required. It supports macOS 12 or newer, Linux with glibc 2.35 or newer and
`libdbus-1`, and Windows on x64 and ARM64. Alpine/musl requires a source build.
Persistent Linux credentials also require an available Secret Service; CI jobs
can use `MONGODB_URI` (and `MONGODB_CLI_ACCESS=read-write` when writes are
intended) from their secret store. Installation downloads the matching release
binary and requires a valid SHA-256 checksum; network and integrity failures are
reported instead of running an unverified file. If lifecycle scripts are
disabled, the launcher retries installation on first use.

```sh
mongodb-cli --profile prod doctor --output json
# Continue after doctor succeeds.
mongodb-cli --profile prod --output json documents find shop orders --limit 5
mongodb-cli skills install
```

Profiles are read-only unless they were created with `--access read-write`, and
destructive commands also require `--yes`.

For zsh completion in the current shell:

```zsh
autoload -Uz compinit && compinit
source <(mongodb-cli completion zsh)
```

Human-readable output is the default on a terminal. Use `--output json` for scripts
and agents, and inspect the exit code and pagination metadata. The launcher passes
arguments, standard streams, and process exit status through to the Rust CLI.
