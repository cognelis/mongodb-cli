Adapted from [MongoDB's agent-skills](https://github.com/mongodb/agent-skills) (Apache-2.0, commit c370a63); modified to use mongodb-cli commands instead of MCP tools.

Commands such as `atlas streams processors list` are mongodb-cli subcommands: run them as `mongodb-cli --profile <atlas-profile> --output json atlas streams ...` with `--project-id <project-id>`. `documents ...` commands use a profile that connects to the sink cluster. See SKILL.md for prerequisites, confirmation rules, and secret handling.

# Development Workflow Reference

## Pipeline Stage Categories

Understanding stage categories helps compose valid pipelines. Stages must appear in this order:

| Category | Stages | Rules |
|----------|--------|-------|
| **Source** (1, required) | `$source` | Must be first. One per pipeline. |
| **Stateless Processing** | `$match`, `$project`, `$addFields`, `$unset`, `$unwind`, `$replaceRoot`, `$redact` | Can appear anywhere after source. No state or memory overhead. |
| **Enrichment** | `$lookup`, `$https` | I/O-bound. Use `parallelism` setting. Place `$https` after windows to batch. |
| **Stateful/Window** | `$tumblingWindow`, `$hoppingWindow`, `$sessionWindow` | Accumulates state in memory. Monitor `memoryUsageBytes`. |
| **Validation** | `$validate` | Schema enforcement. Use `validationAction: "dlq"` (not `"error"`). Place early to catch bad data. |
| **Custom Code** | `$function` | JavaScript UDFs. Requires SP30+. |
| **Output** (1+, required for deployed) | `$merge`, `$emit` | Must be last. Required for persistent processors. Sinkless = ephemeral only. |

**Key ordering principle:** Place `$match` as early as possible (reduces volume for all downstream stages). Place `$project` after `$match` (reduces document size). Place `$https` after windows (batches API calls).

## 5-Phase Development Lifecycle

### Phase 1: Project Setup

**Goal:** Workspace and connections ready.

1. Discover existing resources:
   - `atlas streams workspaces list --project-id <project-id>` — see what already exists
   - If workspace exists, `atlas streams workspaces inspect <ws> --project-id <project-id>` to review config

2. Create workspace (if needed):
   - `atlas streams workspaces create <ws> --project-id <project-id> --cloud-provider <provider> --region <region> --yes` (after the user confirms)
   - Choose region close to your data sources
   - Start with `--tier sp10` for development
   - Sample data is included by default (no `--no-sample-data`) and gives you `sample_stream_solar` for testing

3. Verify workspace:
   - `atlas streams workspaces inspect <ws> --project-id <project-id>` — confirm state and region

### Phase 2: Connection Development

**Goal:** All data sources and sinks connected and verified.

1. Identify required connections:
   - Source connections (Kafka, Cluster change streams, Kinesis, Sample)
   - Sink connections (Cluster for `$merge`, Kafka for `$emit`, S3, Kinesis)
   - Enrichment connections (Https for `$https`, Cluster for `$lookup`)

2. Create each connection:
   - `atlas streams connections create <ws> <connection> --project-id <project-id> --type <type> --config-file <path|-> --yes` for each
   - Never collect sensitive fields (passwords, API keys, tokens) in the conversation: the user adds them to the config file and runs the command; missing required fields make it exit 2 with a list
   - See [connection-configs.md](connection-configs.md) for type-specific schemas

3. Verify connections:
   - `atlas streams connections list <ws> --project-id <project-id>` — confirm all created
   - `atlas streams connections inspect <ws> <connection> --project-id <project-id>` for each — verify state and config

### Phase 3: Processor Development

**Goal:** Working processor with validated pipeline.

#### Pre-Deployment Connection Validation (MANDATORY)

**BEFORE creating any processor**, you MUST validate all connections referenced in your pipeline. This prevents silent failures and confusion about data destinations.

**Step 1: List all connections in workspace**
```
mongodb-cli --profile <atlas-profile> --output json atlas streams connections list <your-workspace> --project-id <project-id>
```
Verify all required connections exist.

**Step 2: Inspect EACH connection referenced in pipeline**

For EVERY `connectionName` in your pipeline (source, sink, enrichment), inspect it:
```
mongodb-cli --profile <atlas-profile> --output json atlas streams connections inspect <your-workspace> <connection-name> --project-id <project-id>
```

**Verify for each connection:**
- [ ] Connection exists and state is READY
- [ ] Connection type matches intended usage:
  - Cluster: valid for `$source` (change streams), `$merge`, `$lookup`
  - Kafka: valid for `$source`, `$emit`
  - S3: valid for `$emit` only
  - Https: valid for `$https` enrichment or sink
  - Lambda: valid for `$externalFunction` only
- [ ] Connection name matches actual target (avoid confusion):
  - ⚠️ BAD: connection "atlascluster" → actual target "ClusterRestoreTest"
  - ✅ GOOD: connection "cluster-restore-test" → actual target "ClusterRestoreTest"
- [ ] For Cluster connections: verify the `clusterName` field points to the intended cluster

**Step 3: Present validation summary to user**

Always show the user what connections will be used:
```
"Before creating processor '<name>', I've verified your connections:
 - ✅ sample_stream_solar → Sample data (READY)
 - ⚠️ atlascluster → ClusterRestoreTest (READY)
      Warning: Connection name 'atlascluster' doesn't match actual cluster 'ClusterRestoreTest'
 - ✅ open-meteo-api → https://api.open-meteo.com/v1/... (READY)

Proceed with processor creation?"
```

**Step 4: Wait for user confirmation if warnings exist**

If any connection name doesn't match its target, ask the user to confirm before proceeding.

**Step 5: Only then create the processor**

This validation workflow prevents:
- Creating processors with non-existent connections (fails immediately)
- Writing data to unexpected clusters (e.g., "atlascluster" → "ClusterRestoreTest" instead of "AtlasCluster")
- Confusion when verifying output data later

#### Incremental Pipeline Development

Follow incremental pipeline development — test at each step:

**Step 1: Basic connectivity**
```json
[
  {"$source": {"connectionName": "my-source"}},
  {"$merge": {"into": {"connectionName": "my-sink", "db": "test", "coll": "step1"}}}
]
```
Create with `--start`. Verify documents flow. Stop processor.

**Step 2: Add filtering**
```json
[
  {"$source": {"connectionName": "my-source"}},
  {"$match": {"status": "active"}},
  {"$merge": {"into": {"connectionName": "my-sink", "db": "test", "coll": "step2"}}}
]
```
Modify pipeline (`processors stop` → `processors modify` → `processors start`). Verify filtered output.

**Step 3: Add transformations**
```json
[
  {"$source": {"connectionName": "my-source"}},
  {"$match": {"status": "active"}},
  {"$addFields": {"processed_at": "$$NOW_NOT_VALID"}},
  {"$project": {"userId": 1, "amount": 1, "processed_at": 1}},
  {"$merge": {"into": {"connectionName": "my-sink", "db": "test", "coll": "step3"}}}
]
```
**Remember:** `$$NOW` is NOT valid in streaming. Use a field from the source document or omit.

**Step 4: Add windowing or enrichment** (if needed)

**Step 5: Add error handling**
- Configure DLQ: `{"dlq": {"connectionName": "my-sink", "db": "streams_dlq", "coll": "failed_docs"}}` (mongodb-cli: `--dlq-connection my-sink --dlq-db streams_dlq --dlq-coll failed_docs`)
- Add `$ifNull` for optional enrichment fields
- Set `onError: "dlq"` on `$https` stages

### Phase 4: Testing & Validation

**Goal:** Processor verified working correctly.

1. Confirm processor state:
   - `atlas streams processors inspect <ws> <processor> --project-id <project-id>` — state should be STARTED

2. Run diagnostics:
   - `atlas streams processors diagnose <ws> <processor> --project-id <project-id>` — full health report

3. Verify data flow (database commands with a profile for the sink cluster; see "Checking output and DLQ collections" in SKILL.md):
   - Use `documents count <db> <output-coll>` on output collection — documents arriving?
   - Use `documents find <db> <output-coll>` on output collection — data looks correct?
   - Use `documents count <db> <dlq-coll>` on DLQ collection — any errors?
   - If DLQ has documents, use `documents find <db> <dlq-coll>` to inspect failure reasons

4. Classify output volume:
   - See [output-diagnostics.md](output-diagnostics.md) for the full decision framework
   - Alert processors: low output is expected
   - Transformation processors: low output is a red flag

### Phase 5: Production Deployment

**Goal:** Processor running at appropriate tier with monitoring.

1. Right-size the tier:
   - See [sizing-and-parallelism.md](sizing-and-parallelism.md) for tier selection
   - Review `memoryUsageBytes` from diagnostics
   - Consider parallelism needs for `$merge`, `$lookup`, `$https`
   - Upgrade tier: `atlas streams processors stop`, then `atlas streams processors start` with a `--tier` override

2. Ensure DLQ is configured (mandatory for production)

3. Use descriptive processor names (e.g., `fraud-detector`, `order-enricher`, `iot-rollup`)

## Debugging Decision Trees

### Connection Failures
1. `atlas streams connections inspect <ws> <connection> --project-id <project-id>` — check state
2. If Kafka: verify `bootstrapServers` is a comma-separated string (not array)
3. If Cluster: verify cluster exists in project (`atlas clusters list --project-id <project-id>`)
4. If AWS (S3/Kinesis/Lambda): verify IAM role ARN is registered in Cloud Provider Access
5. If Https: verify URL is reachable and auth headers are in connection config

### Processor Startup Failures
1. `atlas streams processors diagnose <ws> <processor> --project-id <project-id>` — check state and errors
2. If FAILED: read the error message in diagnostics
3. Common causes:
   - Invalid pipeline syntax (missing `$source`, missing sink)
   - `$$NOW`/`$$ROOT`/`$$CURRENT` used (not valid in streaming)
   - Kafka `$source` missing `topic` field
   - **Referenced connection doesn't exist** — validate with `atlas streams connections list` first
   - **Connection name doesn't match expected target** — inspect connection to verify actual cluster/resource
   - OOM — tier too small for pipeline complexity

### Processing Errors (Running but DLQ filling up)
1. Use `documents find <db> <dlq-coll>` on DLQ collection — inspect error messages
2. Common causes:
   - Schema mismatches in source data
   - `$https` enrichment failures (API down, auth expired)
   - Type errors in `$addFields` or `$project` expressions
3. Fix: `processors stop` → `processors modify` (fix pipeline) → `processors start`

### Performance Issues (Running but slow)
1. `atlas streams processors diagnose <ws> <processor> --project-id <project-id>` — check stats
2. Check `memoryUsageBytes` (full `stats` with `atlas streams processors inspect <ws> <processor> --project-id <project-id> --detailed`) — if near 80% of tier RAM, upgrade tier
3. Check if `$match` is early in pipeline (reduces downstream volume)
4. Check if `$https` has `parallelism` setting (increase for I/O-bound enrichment)
5. Check if windows have `partitionIdleTimeout` (idle Kafka partitions block windows)
6. Consider upgrading tier or increasing stage parallelism

## Operational Monitoring Cadence

### Daily
- Check processor states via `atlas streams processors list <ws> --project-id <project-id>`
- Verify DLQ collections aren't growing via `documents count`
- Confirm output collections are receiving data

### Weekly
- Run `atlas streams processors diagnose` for each production processor
- Review `memoryUsageBytes` trends — approaching 80%?
- Check connection health across all connections

### Monthly
- Evaluate tier appropriateness — over-provisioned or under-provisioned?
- Review DLQ patterns — recurring errors that need pipeline fixes?
- Consider parallelism adjustments based on throughput trends

## Troubleshooting

| Symptom | Likely cause | Action |
|---------|-------------|--------|
| Processor FAILED on start | Invalid pipeline syntax, missing connection, `$$NOW` used | `processors diagnose` → read error → fix pipeline |
| DLQ filling up | Schema mismatch, `$https` failures, type errors | `documents find` on DLQ → fix pipeline or connection |
| Zero output (transformation) | Connection issue, wrong topic, filter too strict | Check source health → verify connections → check `$match` |
| Zero output (alert) | Probably normal — no anomalies detected | Verify with known test event |
| Windows not closing | Idle Kafka partitions | Add `partitionIdleTimeout` to `$source` (e.g., `{"size": 30, "unit": "second"}`) |
| OOM / processor crash | Tier too small for window state | `processors diagnose` (or `processors inspect --detailed`) → check `memoryUsageBytes` → upgrade tier |
| Slow throughput | Low parallelism on I/O stages | Increase `parallelism` on `$merge`/`$lookup`/`$https` |
| 404 on workspace | Doesn't exist or misspelled | `atlas streams workspaces list` |
| 409 on create | Name already exists | Inspect existing resource or pick new name |
| 402 error on start | No billing configured | Do NOT retry. Add payment method in Atlas → Billing. Use `sp.process()` in mongosh (outside mongodb-cli) as free alternative |
| "processor must be stopped" | Tried to modify running processor | `atlas streams processors stop` first |
| bootstrapServers format | Passed as array instead of string | Use comma-separated string: `"broker1:9092,broker2:9092"` |
| "must choose at least one role" | Cluster connection without `dbRoleToExecute` | Defaults to `readWriteAnyDatabase` — or specify custom role |
| "No cluster named X" | Cluster doesn't exist in project | `atlas clusters list --project-id <project-id>` to verify |
| IAM role ARN not found | ARN not registered in project | Register via Atlas → Cloud Provider Access |
| dataProcessRegion format | Wrong region format | See region table above. If unsure, inspect an existing workspace |
| Processor PROVISIONING for minutes | Restart cycle with exponential backoff | Wait for FAILED state, or stop → restart. Check logs for repeated error |
| Parallelism exceeded | Tier too small for requested parallelism | Start with higher tier (see `sizing-and-parallelism.md`) |
| Networking change needed | Networking is immutable after creation | Delete connection and recreate with new networking config |
| 401 / 403 on API call (exit 3) | Invalid or expired Atlas API credentials | Run `mongodb-cli --profile <atlas-profile> doctor --output json`; the user re-runs `auth login --atlas` if credentials are wrong; check the service account's API Access List and project-level permissions |
| 429 rate limit (exit 6) | Too many API calls | Wait and retry; avoid tight loops of list/inspect calls |

## Pre-Deploy Quality Checklist

Before creating a processor, verify:

### Connection Validation (MANDATORY - Always do this first)
- [ ] **CRITICAL**: Run `atlas streams connections list <ws> --project-id <project-id>` to list all connections in workspace
- [ ] **CRITICAL**: Run `atlas streams connections inspect <ws> <connection> --project-id <project-id>` for EACH connection referenced in pipeline
- [ ] **CRITICAL**: Verify connection names clearly indicate their actual targets (avoid generic names like "atlascluster" pointing to "ClusterRestoreTest")
- [ ] **CRITICAL**: Present connection summary to user: "Connection 'X' → Actual target 'Y'" for each connection
- [ ] **CRITICAL**: Warn user if connection names don't match their targets and ask for confirmation
- [ ] All connections are in READY state
- [ ] Connection types match usage (Cluster for $source/$merge, Kafka for topics, etc.)

### Pipeline Validation
- [ ] `knowledge search` was run to validate sink/source field names
- [ ] Pipeline starts with `$source` and ends with `$merge`, `$emit`, `$https`, or `$externalFunction` (async)
- [ ] No `$$NOW`, `$$ROOT`, or `$$CURRENT` in the pipeline
- [ ] Kafka `$source` includes a `topic` field
- [ ] Kafka `$source` with windowed pipeline includes `partitionIdleTimeout` (prevents windows from stalling on idle partitions)
- [ ] HTTPS connections are only used in `$https` enrichment or sink stages, not in `$source`
- [ ] DLQ is configured (recommended for production)
- [ ] `$https` stages use `onError: "dlq"` (not `"fail"`)
- [ ] `$externalFunction` stages use `onError: "dlq"` and `execution` is explicitly set
- [ ] API auth is stored in connection settings, not hardcoded in the pipeline

## Post-Deploy Verification Workflow

After creating and starting a processor:
1. `atlas streams processors inspect <ws> <processor> --project-id <project-id>` — confirm state is STARTED
2. `atlas streams processors diagnose <ws> <processor> --project-id <project-id>` — check for errors in the health report
3. Use `documents count <db> <dlq-coll>` on the DLQ collection — verify no errors accumulating
4. Use `documents find <db> <output-coll> --sort '{"_id": -1}' --limit 5` on the output collection — verify documents are arriving
5. If output is low/zero, classify processor type before assuming a problem (see Debug section)
