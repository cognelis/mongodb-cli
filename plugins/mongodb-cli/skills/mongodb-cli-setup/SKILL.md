---
name: mongodb-cli-setup
description: Guide users through configuring the mongodb-cli executable for MongoDB and Atlas - named profiles whose connection strings live in the operating-system keyring, read-only or read-write access, Atlas service-account credentials, Atlas Local deployments in Docker, and the doctor check. Use this skill when a user has mongodb-cli installed but no working profile or credentials, or asks how to connect mongodb-cli to MongoDB or Atlas. When the user is configuring the MongoDB MCP server, or the MCP server is the active integration and the user did not ask for mongodb-cli, use the official mongodb-mcp-setup skill instead.
license: Apache-2.0
metadata:
  version: "1.0.0"
---

Adapted from [MongoDB's agent-skills](https://github.com/mongodb/agent-skills) (Apache-2.0, commit c370a63), skill mongodb-mcp-setup, for the mongodb-cli executable; modified to configure mongodb-cli profiles and commands instead of the MongoDB MCP server.

# mongodb-cli Setup

This skill guides users through configuring mongodb-cli for use from a terminal, CI, or an agentic client.

If the user is configuring the MongoDB MCP server, or the MCP server is the active integration and the user did not ask for mongodb-cli, use the official MongoDB plugin skill `mongodb-mcp-setup` instead of this one.

## Overview

mongodb-cli requires credentials for the deployment it talks to. Users have three options:

1. **Connection String** (Option A): Direct connection to a specific cluster
   - Quick setup for single cluster
   - Stored per named profile in the operating-system keyring, or provided as `MONGODB_URI` in CI and short-lived shells

2. **Service Account Credentials** (Option B): MongoDB Atlas Admin API access
   - **Recommended for Atlas users** who also want Atlas administration (clusters, Performance Advisor, Stream Processing) from mongodb-cli
   - Access to Atlas Admin API commands (`mongodb-cli atlas ...`) and dynamic cluster connection via `atlas clusters connect`
   - No manual DB user credential management
   - Requires Atlas tools to be enabled and a client ID and secret stored with `auth login --atlas` (or `MONGODB_ATLAS_CLIENT_ID` and `MONGODB_ATLAS_CLIENT_SECRET`)

3. **Atlas Local** (Option C): Local development with Docker
   - **Best for local testing** - no cloud account or database credentials to manage
   - Runs Atlas locally in Docker, requires Docker installed and Atlas tools enabled
   - `local deployments connect` stores the deployment's connection string in a profile for you

This is an interactive step-by-step guide. The agent detects the user's environment and provides tailored instructions, but **never asks for or handles credentials** — users enter them in their own terminal at a masked prompt, or pipe them from a secret store, in Step 5. Make this clear to the user whenever credentials come up in Steps 3a and 3b.

No agent-specific configuration is needed: profiles live in the mongodb-cli config file and secrets in the operating-system keyring, so every terminal and agent on the machine sees the same setup. Environment variables matter only for CI and short-lived shells; note that an agent app launched from Finder, Launchpad, or the Windows Start menu does not inherit shell environment variables.

## Step 1: Check Existing Configuration

Check whether mongodb-cli is installed and what is already configured. None of these commands print credentials:

```bash
mongodb-cli version --output json
mongodb-cli profile list --output json
mongodb-cli config show --output json
```

For CI or a short-lived shell, also check the environment with values redacted:

```bash
env | grep -E '^(MONGODB_URI|MONGODB_ATLAS_CLIENT_ID|MONGODB_ATLAS_CLIENT_SECRET|MONGODB_CLI_ACCESS|MONGODB_CLI_ATLAS_TOOLS)=' | sed -E '/^MONGODB_CLI_(ACCESS|ATLAS_TOOLS)=/!s/=.*/=[set]/'
```

Then inspect and diagnose the profile the user wants to use (for an environment connection, run `doctor` without `--profile`):

```bash
mongodb-cli profile show <profile> --output json
mongodb-cli --profile <profile> doctor --output json
```

**Interpretation:**

- `profile list` shows the saved profiles and the default one. `profile show` reports a profile's access mode, its credential-free connection `target`, and whether a connection string or Atlas credentials are stored (never their values).
- `doctor` checks the CLI version, configuration, credential availability, server connectivity, and authentication in order. A completed run always prints its check array to stdout, even when it exits nonzero: read `data[].name`, `data[].status` (`pass`, `fail`, or `skipped`), and `data[].message`. `meta.access` shows the access mode. Never discard its stdout.
- If `MONGODB_URI` appears → an environment connection is configured. It is used only when a command passes no `--profile`, and it is read-only unless `MONGODB_CLI_ACCESS=read-write` is also set.
- If both `MONGODB_ATLAS_CLIENT_ID` and `MONGODB_ATLAS_CLIENT_SECRET` appear → environment Atlas credentials are configured. If only one is present, treat it as incomplete.
- `config show` reports whether Atlas tools are enabled (`atlas_tools`); `MONGODB_CLI_ATLAS_TOOLS=enabled|disabled` overrides the config file.

**Partial Configuration Handling:**

- User wants read-only access on an existing read-write profile → `mongodb-cli profile update <profile> --access read-only` (always allowed), then skip to Step 6
- User wants an existing profile to become read-write → only the user can do this, by confirming `mongodb-cli profile update <profile> --access read-write` in an interactive terminal. Never run it for them and never set `MONGODB_CLI_ACCESS`
- User wants to switch authentication methods → `mongodb-cli --profile <profile> auth logout` removes the stored connection string (`auth logout --atlas` removes Atlas credentials and, like `auth login --atlas`, needs Atlas tools enabled); for environment variables, remove them from the shell profile or CI settings. Then proceed with Steps 2–5
- User wants to update credentials → skip to Step 5 (`auth login` again replaces the stored value)

**Important**: If the user wants an Atlas Admin API action (managing clusters, creating users, performance advisor, stream processing) but only has a connection string, explain they need service account credentials and Atlas tools enabled, and offer to walk through Option B.

## Step 2: Present Configuration Options

If no valid configuration exists, present the options:

**Connection String (Option A)** — Best for:

- Single cluster access
- Existing database credentials
- Self-hosted MongoDB or no Atlas Admin API needs

**Service Account Credentials (Option B)** — Best for:

- MongoDB Atlas users who want Atlas administration from mongodb-cli
- Multi-cluster switching (one saved profile per cluster through `atlas clusters connect`)
- Atlas Admin API access (cluster management, user creation, performance monitoring, stream processing)

**Atlas Local (Option C)** — Best for:

- Local development/testing without cloud setup
- Fastest setup with Docker, no credentials required

Ask the user which option they'd like to proceed with. Options can be combined: one profile may hold a connection string, Atlas credentials, or both.

## Step 3a: Connection String Setup

If the user chooses Option A:

### 3a.1: Explain How to Find the Connection String

Explain where and how to obtain their connection string:

**For MongoDB Atlas:**

1. Go to [cloud.mongodb.com](https://cloud.mongodb.com)
2. Select your cluster → click **Connect**
3. Choose **Drivers** or **Shell** → copy the connection string
4. Replace `<username>` and `<password>` with your database user credentials

**For self-hosted MongoDB:**

- The connection string is typically configured by your DBA or in your application config
- Format: `mongodb://<credentials>@<host>:<port>/<database>`, where `<credentials>` stands for the database username and password joined by a colon (percent-encode special characters)

**Expected formats:**

- `mongodb://<credentials>@<host>:<port>/<database>`
- `mongodb+srv://<credentials>@<cluster-host>/<database>`
- `mongodb://localhost:27017` (local, no auth)

Proceed to Step 4 (Determine Read-Only Access).

## Step 3b: Service Account Setup

If the user chooses Option B:

### 3b.1: Guide Through Atlas Service Account Creation

Direct the user to create a MongoDB Atlas Service Account:

**Full documentation**: https://www.mongodb.com/docs/atlas/api/service-accounts-overview/

Walk them through the key steps:

1. **Navigate to MongoDB Atlas** — [cloud.mongodb.com](https://cloud.mongodb.com)
2. **Select your organization** from the ORGANIZATION section near the top of the page
3. **Go to "Project Identity and Access"** on the left sidebar → **Applications** → **Create Service Account**
4. **Set Permissions** — Grant Organization Member or Project Owner (see docs for exact permission mappings). For least privilege: Organization Read Only lists organizations and projects, Project Read Only reads clusters and Performance Advisor, Project Cluster Manager creates and changes clusters, and Project Stream Processing Owner manages Stream Processing
5. **Generate Credentials** — Create Client ID and Secret
   - ⚠️ The **Client Secret is shown only once** — save it immediately (for example in a password manager) before leaving the page
6. **Note both values** — the user enters them in Step 5; they never go into the conversation

### 3b.2: API Access List Configuration

⚠️ **CRITICAL**: The user MUST add their IP address to the service account's API Access List, or all Atlas Admin API operations will fail.

Steps:

1. On the service account details page, find **API Access List**
2. Click **Add Access List Entry**
3. Add your current IP address. Use a specific IP or CIDR range whenever possible.
   - ⚠️ **`0.0.0.0/0` allows access from any IP — this is a significant security risk.** Only use it as a last resort for temporary testing and remove it immediately afterward. It should never be used in production.
4. Save changes

This is more secure than global Network Access settings as it only affects API access, not database connections.

### 3b.3: Enable Atlas Tools

The `atlas`, `local`, and `knowledge` command groups are disabled by default. Enabling them changes the user's configuration, so run this only with their agreement (CI can set `MONGODB_CLI_ATLAS_TOOLS=enabled` instead):

```bash
mongodb-cli config set atlas-tools enabled
```

Proceed to Step 4 (Determine Read-Only Access).

## Step 3c: Atlas Local Setup

If the user chooses Option C:

### 3c.1: Check Docker Installation

Verify Docker is installed and running:

```bash
docker info
```

If not installed, direct them to: https://www.docker.com/get-started

### 3c.2: Enable Atlas Tools and Create a Deployment

Atlas Local commands are part of the Atlas tools, which are disabled by default; enable them only with the user's agreement. The `local deployments list`, `create`, and `delete` commands talk to Docker and do not use a profile.

```bash
mongodb-cli config set atlas-tools enabled
mongodb-cli --output json local deployments list
mongodb-cli --output json local deployments create <deployment> --wait-timeout 600
```

- `local deployments create` pulls the Atlas Local image and waits up to `--wait-timeout` seconds (default 600) for the deployment to become healthy. `--load-sample-data` loads the sample datasets; `--image-tag` selects another image tag (default `preview`).
- To use Automated Embedding locally, the user sets `MONGODB_CLI_VOYAGE_API_KEY` in the environment before `create`; there is no flag for it.
- If `create` times out, the deployment may still be starting: check `local deployments list` before retrying.
- `local deployments delete <deployment> --yes` removes a deployment irreversibly; add `--yes` only after the user confirms that deployment by name.

### 3c.3: Save the Deployment as a Profile

```bash
mongodb-cli --profile <deployment> local deployments connect <deployment>
mongodb-cli --profile <deployment> doctor --output json
```

- `connect` verifies the deployment and stores its connection string in the keyring under the profile named by `--profile` (default: the deployment name); the string is never printed.
- New profiles are read-only. Add `--access read-write` when creating the profile if the user wants writes on this local deployment. Replacing the connection string of an existing profile needs `--yes`, after the user confirms.
- If `connect` reports that the deployment is still starting, wait a few seconds and run it again.

**Skip Steps 4 and 5** (the connection string is already stored) and proceed to Step 6 (Next Steps).

## Step 4: Determine Read-Only vs Read-Write Access

**Only applies to Options A and B. For Option C, choose with `--access` in Step 3c.3.**

Ask whether they want read-only or read-write access:

- **Read-Only** (default for new mongodb-cli profiles): Data reads only, no modifications
  - Best for: Production data safety, reporting, compliance

- **Read-Write**: Full data access, modifications allowed; destructive operations still require `--yes` on each command
  - Best for: Development, testing, administrative tasks

The access mode belongs to the profile and also gates Atlas commands that create, change, or delete resources. To keep a database read-only while managing Atlas, use two profiles.

**If read-only**: create the profile without `--access` in Step 5.
**If read-write**: create it with `--access read-write` in Step 5. Raising an existing profile later requires the user to confirm `mongodb-cli profile update <profile> --access read-write` in an interactive terminal; an agent cannot do it and must not set `MONGODB_CLI_ACCESS`. For environment connections, read-write requires the user to set `MONGODB_CLI_ACCESS=read-write` themselves.

The CLI's read-only mode is a safeguard, not a substitute for database privileges: prefer a database user with only the `read` role for production.

Proceed to Step 5 (Configure Credentials).

## Step 5: Configure Credentials

**Do not ask for or handle credentials** — provide exact instructions so the user can enter them directly. mongodb-cli has no flag that accepts a connection string or secret, so credentials never enter shell history or process listings.

### 5.1: Workstation (interactive terminal)

**Option A, guided:** the user runs the guided setup in their own terminal. It creates or repairs a profile, masks and verifies the connection string before storing it in the keyring, asks for the access mode, activates the profile, and installs the bundled skills. It needs a real interactive terminal; with redirected input or output it exits without changing configuration and prints the equivalent non-interactive commands.

```bash
mongodb-cli init
```

**Option A, step by step:**

```bash
mongodb-cli profile add <profile> --description "<what it is for>"                       # read-only
mongodb-cli profile add <profile> --access read-write --description "<what it is for>"   # or read-write
mongodb-cli --profile <profile> auth login              # masked prompt; verifies before storing
```

Ask the user what each environment is for and record it with `--description`, such as "Production orders cluster, reporting only". Agents choose between profiles by name, description, and access, so a description matters most when names alone are ambiguous. `profile update <profile> --description "<text>"` adds or changes it later, and `--description ""` clears it.

**Option B:**

```bash
mongodb-cli profile add <atlas-profile>
mongodb-cli --profile <atlas-profile> auth login --atlas
```

`auth login --atlas` prompts for the client ID and, masked, for the client secret, then verifies them by requesting a token before storing them in the keyring. It is refused with "Atlas tools are disabled" until Atlas tools are enabled (Step 3b.3). An agent cannot answer these prompts, so ask the user to run the commands.

### 5.2: Scripts and Secret Stores (no prompt)

Pipe the secret from a secret manager or an environment variable the user controls, never from a literal typed into the command:

```bash
printf '%s\n' "$PROD_MONGODB_URI" | mongodb-cli --profile <profile> auth login --uri-stdin
printf '%s\n%s\n' "$ATLAS_CLIENT_ID" "$ATLAS_CLIENT_SECRET" | mongodb-cli --profile <atlas-profile> auth login --atlas --credentials-stdin
```

Use `--no-verify` only for an intentional offline setup, never to get past an authentication or network error.

### 5.3: CI and Short-Lived Shells

Provide environment variables from the CI provider's secret store and disable shell tracing:

```bash
set +x
export MONGODB_URI="...from a secret store..."            # read-only unless MONGODB_CLI_ACCESS=read-write
export MONGODB_ATLAS_CLIENT_ID="...from a secret store..."
export MONGODB_ATLAS_CLIENT_SECRET="...from a secret store..."
```

Connections resolve in this order: `--profile`, then `MONGODB_URI`, then the default profile; Atlas credentials resolve the same way. An explicit `--profile` wins, and `meta.warnings` notes an ignored `MONGODB_URI`.

⚠️ Never put connection strings or secrets in repository files, command-line arguments, or the conversation.

### 5.4: Verify

```bash
mongodb-cli --profile <profile> doctor --output json
```

A healthy run exits 0. With Atlas tools enabled, doctor also checks the profile's Atlas credentials and API access; checks for credentials the profile does not hold are reported as `skipped`. If doctor exits nonzero, read the failed checks from stdout and see Troubleshooting.

Proceed to Step 6 (Next Steps).

## Step 6: Next Steps

### For Options A & B (Connection String / Service Account):

1. **No restart needed**: mongodb-cli reads the profile and keyring on every command. Only environment-variable setups need a new shell, or a restarted agent app, to pick up changes.

2. **Select the profile explicitly**: in agent workflows, run the doctor preflight and pass the same `--profile <profile>` and `--output json` to every command. `mongodb-cli profile use <profile>` changes the default profile.

3. **Using the Commands**:
   - Option A: database commands (`databases`, `collections`, `indexes`, `documents`, `explain`, `server`)
   - Option B: Atlas commands, for example:
     ```bash
     mongodb-cli --profile <atlas-profile> --output json atlas projects list
     mongodb-cli --profile <atlas-profile> --output json atlas clusters list --project-id <project-id>
     ```
     To work with a cluster's data, `mongodb-cli --profile <atlas-profile> --output json atlas clusters connect <cluster> --project-id <project-id> --save-profile <new-profile> --yes` creates a temporary database user (4 hours by default, `--user-lifetime-hours`), adds the current IP to the project's IP access list if needed (that entry is not removed later), and saves the connection as a new read-only profile (`--access read-write` for writes). It needs a read-write Atlas profile; add `--yes` only after the user confirms. `atlas clusters disconnect <new-profile> --yes` deletes the temporary user and the saved profile.
   - **Important (Option B)**: Ensure your IP is in the service account's API Access List or all API calls will fail

### For Option C (Atlas Local):

1. **Ready to use**: the profile saved in Step 3c.3 works like any other profile.

2. **Next steps**:
   - List deployments: `mongodb-cli --output json local deployments list`
   - Use standard database commands with `--profile <deployment>`

## Troubleshooting

- **`doctor` fails**: read each failed check's `message`; the exit code gives the category: 3 configuration or authentication (missing profile or credentials, wrong password, keyring problems), 4 network or timeout (DNS, firewall, IP access list), 5 server rejection
- **No usable keyring** (for example a headless Linux host without a Secret Service): use environment variables (Step 5.3) instead of stored profiles
- **"Atlas tools are disabled"** (exit 3, also from `auth login --atlas` and `auth logout --atlas`): enable them with `mongodb-cli config set atlas-tools enabled` (with the user's agreement), and check that `MONGODB_CLI_ATLAS_TOOLS` is not set to `disabled`
- **Atlas Admin API errors (Option B)**: exit 3; HTTP 401 means invalid or expired credentials (the user runs `auth login --atlas` again), 403 means missing roles or an IP that is not in the service account's API Access List
- **Docker errors (Option C)**: "Docker is not running" (exit 4) → start Docker; `DOCKER_HOST` must point to a supported endpoint
- **Invalid connection string format**: Re-check the format; must start with `mongodb://` or `mongodb+srv://`
- **Unreachable replica set whose member names differ from the configured host**: report it instead of editing the user's connection string; a single-host `mongodb://` string without `replicaSet`, `directConnection`, or `loadBalanced` connects directly to that host
- **Read-only refusal** (exit 3, `forbidden`): expected on read-only profiles; report it and never change the profile's access or switch profiles to get around it
- **`MONGODB_URI` seems ignored**: an explicit `--profile` takes precedence (`meta.warnings` says so)
- **fish/PowerShell**: Syntax differs — use `set -x` (fish) or `$env:` (PowerShell) instead of `export`
