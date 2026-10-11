Adapted from [MongoDB's agent-skills](https://github.com/mongodb/agent-skills) (Apache-2.0, commit c370a63), file mcp-troubleshooting.md; modified to troubleshoot mongodb-cli instead of the MongoDB MCP server.

# mongodb-cli Troubleshooting

This skill requires mongodb-cli with Atlas tools enabled and Atlas service-account credentials. If the `atlas streams` commands are unavailable or failing, follow the diagnostic steps below.

## Step 1: Verify mongodb-cli Is Installed

```bash
mongodb-cli version --output json
```

**If it is missing:**
- Ask the user to install mongodb-cli (from a checkout: `cargo install --path crates/mongodb-cli --locked`)

## Step 2: Verify Atlas Tools Are Enabled

```bash
mongodb-cli config show --output json
```

`atlas_tools` shows whether the `atlas`, `local`, and `knowledge` command groups are enabled; `MONGODB_CLI_ATLAS_TOOLS=enabled|disabled` overrides the config file. While they are disabled, the groups and the `--atlas` flag of `auth login` and `auth logout` are hidden from `--help`, and using any of them exits 3 with error code `config` and "Atlas tools are disabled".

**If disabled:**
- Enabling changes the user's configuration, so run it only with their agreement:
  ```bash
  mongodb-cli config set atlas-tools enabled
  ```

## Step 3: Verify Atlas Credentials

```bash
mongodb-cli profile show <atlas-profile> --output json
mongodb-cli --profile <atlas-profile> doctor --output json
```

`profile show` reports whether Atlas credentials are stored (never their values). With Atlas tools enabled, `doctor` checks the credentials, requests a token, and calls the Atlas Admin API; read its check array from stdout even when it exits nonzero.

**If credentials are missing or rejected:**
- The user stores them in their own terminal: `mongodb-cli --profile <atlas-profile> auth login --atlas` (client ID prompt and masked secret prompt). Automation can set `MONGODB_ATLAS_CLIENT_ID` and `MONGODB_ATLAS_CLIENT_SECRET`; both are required. Never ask for the client secret in the conversation.
- HTTP 401 (exit 3): invalid or expired credentials. HTTP 403 (exit 3): the service account lacks a role (Project Stream Processing Owner for managing streams) or the caller's IP is not in the service account's API Access List.

## Step 4: Verify Command Availability

Check that the streams command groups are available:

```bash
mongodb-cli atlas streams --help
```

- `atlas streams workspaces`
- `atlas streams connections`
- `atlas streams processors`
- `atlas streams networking`, `atlas streams privatelink`, `atlas streams peering`

A read-only profile can run `list`, `inspect`, `diagnose`, and `networking show`; every other streams command needs a read-write profile and `--yes`. A read-only refusal exits 3 with `forbidden`: report it, and never change the profile's access or switch profiles to get around it.

## Fallback Options (Limited Functionality)

If mongodb-cli cannot be configured immediately, you have limited alternatives. Both run outside mongodb-cli:

### Option 1: Atlas CLI (Read-Only)
Use Atlas CLI API commands for exploration only:
```bash
atlas api streams listStreamWorkspaces --projectId <project-id>
atlas api streams getStreamWorkspace --workspaceName <workspace-name> --projectId <project-id>
```

**Limitations:**
- Read-only operations only
- Cannot create or modify processors
- No automated validation or diagnostics

### Option 2: mongosh with sp.process() (Prototyping Only)
Use `sp.process()` in mongosh for ephemeral pipeline testing:
```javascript
sp.process([
  { $source: { connectionName: "sample_stream_solar" } },
  { $match: { temperature: { $gt: 50 } } },
  { $limit: 10 }
])
```

**Limitations:**
- Ephemeral only (no deployed processors)
- No billing (runs locally)
- Cannot test production connections
- Limited to simple pipeline validation

## Recommended Action

**For full Atlas Stream Processing capabilities, enable mongodb-cli Atlas tools and store Atlas service-account credentials for the profile.** The fallback options above provide minimal functionality and are not suitable for production workflows.
