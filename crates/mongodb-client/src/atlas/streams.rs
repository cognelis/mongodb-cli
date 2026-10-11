//! Atlas Stream Processing commands: the MongoDB MCP server's discover, build, manage, and
//! teardown tools. A workspace is called `tenantName` in API paths.

use serde_json::{Map, Value, json};

use super::{AtlasClient, Outcome, Paging, compact, rejected, results, seg, truthy, validation};
use crate::ClientError;

/// Stream processing tiers in ascending order.
pub const TIERS: [&str; 5] = ["SP2", "SP5", "SP10", "SP30", "SP50"];
/// Keys whose values are secrets in Atlas payloads, matched case-insensitively.
const SECRET_KEYS: [&str; 17] = [
    "password",
    "saslpassword",
    "apikey",
    "accesstoken",
    "token",
    "clientsecret",
    "secret",
    "privatekey",
    "sslclientauthenticationkey",
    "sslkeypassword",
    "awssecretaccesskey",
    "oidcclientsecret",
    "sharedsecret",
    "authorization",
    "proxy-authorization",
    "api-key",
    "x-api-key",
];
const BILLING: &str = "billing for stream processing is now active for this processor; stop it with `mongodb-cli atlas streams processors stop`";

fn workspace_path(project_id: &str, workspace: &str) -> String {
    format!("/groups/{}/streams/{}", seg(project_id), seg(workspace))
}

fn processor_path(project_id: &str, workspace: &str, processor: &str) -> String {
    format!(
        "{}/processor/{}",
        workspace_path(project_id, workspace),
        seg(processor)
    )
}

fn connection_path(project_id: &str, workspace: &str, connection: &str) -> String {
    format!(
        "{}/connections/{}",
        workspace_path(project_id, workspace),
        seg(connection)
    )
}

/// Streams list calls send only the page size and number.
fn page_query(limit: u32, page: u32) -> [(&'static str, String); 2] {
    [
        ("itemsPerPage", limit.to_string()),
        ("pageNum", page.to_string()),
    ]
}

fn listed(key: &str, items: Vec<Value>, response: &Value, limit: u32, page: u32) -> Outcome {
    let paging = Paging {
        limit,
        page,
        include_count: false,
    };
    let returned = items.len();
    Outcome::paged(
        json!({ key: items }),
        paging,
        returned,
        response["totalCount"].as_u64(),
    )
}

pub async fn list_workspaces(
    client: &AtlasClient,
    project_id: &str,
    limit: u32,
    page: u32,
    detailed: bool,
) -> Result<Outcome, ClientError> {
    let response = client
        .get(
            &format!("/groups/{}/streams", seg(project_id)),
            &page_query(limit, page),
        )
        .await?;
    let items = results(&response).iter().map(workspace_summary).collect();
    let mut outcome = listed("workspaces", items, &response, limit, page);
    if detailed {
        outcome.data = redact(&response);
    }
    Ok(outcome)
}

pub async fn inspect_workspace(
    client: &AtlasClient,
    project_id: &str,
    workspace: &str,
    detailed: bool,
) -> Result<Outcome, ClientError> {
    let response = client
        .get(
            &workspace_path(project_id, workspace),
            &[("includeConnections", "true".into())],
        )
        .await?;
    if detailed {
        return Ok(Outcome::new(redact(&response)));
    }
    let mut summary = workspace_summary(&response);
    summary["connectionCount"] = json!(response["connections"].as_array().map_or(0, Vec::len));
    Ok(Outcome::new(json!({"workspace": summary})))
}

fn workspace_summary(workspace: &Value) -> Value {
    let region = &workspace["dataProcessRegion"];
    let label = |key: &str| region[key].as_str().unwrap_or("unknown").to_owned();
    json!({
        "name": workspace["name"],
        "region": if region.is_object() { format!("{}/{}", label("cloudProvider"), label("region")) } else { "unknown".into() },
        "tier": workspace["streamConfig"]["tier"].as_str().unwrap_or("unknown"),
        "maxTier": workspace["streamConfig"]["maxTierSize"].as_str().unwrap_or("unknown"),
    })
}

pub async fn create_workspace(
    client: &AtlasClient,
    project_id: &str,
    workspace: &str,
    provider: &str,
    region: &str,
    tier: Option<&str>,
    sample_connections: bool,
) -> Result<Outcome, ClientError> {
    let body = json!({
        "name": workspace,
        "dataProcessRegion": {"cloudProvider": provider, "region": region},
        "streamConfig": {"tier": tier.unwrap_or("SP10")},
    });
    let path = if sample_connections {
        format!("/groups/{}/streams:withSampleConnections", seg(project_id))
    } else {
        format!("/groups/{}/streams", seg(project_id))
    };
    client.post(&path, Some(&body)).await?;
    Ok(Outcome::new(json!({"resource": "workspace"})))
}

pub async fn update_workspace(
    client: &AtlasClient,
    project_id: &str,
    workspace: &str,
    region: Option<&str>,
    tier: Option<&str>,
) -> Result<Outcome, ClientError> {
    let path = workspace_path(project_id, workspace);
    let mut body = Map::new();
    if let Some(region) = region {
        // Atlas requires the current cloud provider alongside a new region.
        let current = client.get(&path, &[]).await?;
        let provider = current["dataProcessRegion"]["cloudProvider"].as_str().ok_or_else(|| {
            rejected("Unable to update workspace region: the current workspace does not specify a cloud provider.")
        })?;
        body.insert("cloudProvider".into(), json!(provider));
        body.insert("region".into(), json!(region));
    }
    if let Some(tier) = tier {
        body.insert("streamConfig".into(), json!({"tier": tier}));
    }
    let updated = client.patch(&path, &Value::Object(body)).await?;
    let updated_region = &updated["dataProcessRegion"];
    if let (Some(region), Some(actual)) = (region, updated_region["region"].as_str())
        && actual != region
    {
        return Err(rejected(format!(
            "Failed to update workspace region to '{region}'. Current region is '{actual}'. Verify the region name is valid for the workspace's cloud provider."
        )));
    }
    let mut data = Map::new();
    if region.is_some()
        && let (Some(provider), Some(actual)) = (
            updated_region["cloudProvider"].as_str(),
            updated_region["region"].as_str(),
        )
    {
        data.insert("region".into(), json!(format!("{provider}/{actual}")));
    }
    if tier.is_some() {
        for (key, field) in [("tier", "tier"), ("maxTier", "maxTierSize")] {
            if let Some(value) = updated["streamConfig"][field].as_str() {
                data.insert(key.into(), json!(value));
            }
        }
    }
    Ok(Outcome::new(Value::Object(data)))
}

pub async fn delete_workspace(
    client: &AtlasClient,
    project_id: &str,
    workspace: &str,
) -> Result<Outcome, ClientError> {
    let path = workspace_path(project_id, workspace);
    let (connections_path, processors_path) =
        (format!("{path}/connections"), format!("{path}/processors"));
    let (connections, processors) = tokio::join!(
        client.get(&connections_path, &[]),
        client.get(&processors_path, &[])
    );
    let mut data = json!({"resource": "workspace"});
    if let Ok(connections) = &connections {
        data["connectionsRemoved"] = json!(results(connections).len());
    }
    if let Ok(processors) = &processors {
        data["processorsRemoved"] = json!(results(processors).len());
    }
    client.delete(&path).await?;
    Ok(Outcome::new(data).warn(
        "deletion runs asynchronously; confirm with `mongodb-cli atlas streams workspaces list`",
    ))
}

pub async fn list_connections(
    client: &AtlasClient,
    project_id: &str,
    workspace: &str,
    limit: u32,
    page: u32,
    detailed: bool,
) -> Result<Outcome, ClientError> {
    let path = format!("{}/connections", workspace_path(project_id, workspace));
    let response = client.get(&path, &page_query(limit, page)).await?;
    let items = results(&response).iter().map(connection_summary).collect();
    let mut outcome = listed("connections", items, &response, limit, page);
    if detailed {
        outcome.data = redact(&response);
    }
    Ok(outcome)
}

fn connection_summary(connection: &Value) -> Value {
    compact(
        json!({"name": connection["name"], "type": connection["type"], "state": connection["state"]}),
    )
}

pub async fn inspect_connection(
    client: &AtlasClient,
    project_id: &str,
    workspace: &str,
    connection: &str,
    detailed: bool,
) -> Result<Outcome, ClientError> {
    let response = client
        .get(&connection_path(project_id, workspace, connection), &[])
        .await?;
    let summary = compact(json!({
        "type": response["type"],
        "state": response["state"],
        "region": response["region"],
        "clusterName": response["clusterName"],
        "bootstrapServers": response["bootstrapServers"],
    }));
    let mut outcome = Outcome::new(if detailed {
        redact(&response)
    } else if summary.as_object().is_some_and(Map::is_empty) {
        json!({})
    } else {
        json!({"connection": summary})
    });
    if response["type"] == "Cluster"
        && response["clusterName"].is_string()
        && response["clusterName"] != response["name"]
    {
        outcome = outcome.warn("this connection's name differs from its cluster name; reference the connection name in pipeline stages");
    }
    Ok(outcome)
}

/// Normalize a connection configuration the way the MCP server's schema transforms it.
pub fn normalize_connection_config(config: &mut Map<String, Value>) {
    if let Some(servers) = config.get("bootstrapServers").and_then(Value::as_array) {
        let joined: Vec<String> = servers
            .iter()
            .map(|server| {
                server
                    .as_str()
                    .map_or_else(|| server.to_string(), str::to_owned)
            })
            .collect();
        config.insert("bootstrapServers".into(), json!(joined.join(",")));
    }
    if let Some(urls) = config.get("schemaRegistryUrls").and_then(Value::as_str) {
        let split: Vec<&str> = urls.split(',').map(str::trim).collect();
        config.insert("schemaRegistryUrls".into(), json!(split));
    }
}

/// Normalize a new connection's configuration, apply defaults, and require the fields its type
/// needs. `kind` is the Atlas type name, such as `Kafka` or `SchemaRegistry`.
pub fn prepare_connection_config(
    kind: &str,
    config: &mut Map<String, Value>,
) -> Result<(), ClientError> {
    normalize_connection_config(config);
    let present = |config: &Map<String, Value>, path: &[&str]| {
        let mut value = config.get(path[0]).unwrap_or(&Value::Null);
        for key in &path[1..] {
            value = &value[*key];
        }
        truthy(value)
    };
    let mut note = None;
    let missing: Vec<(&str, &str)> = match kind {
        "Kafka" => {
            let mechanism = config
                .get("authentication")
                .and_then(|auth| auth["mechanism"].as_str())
                .map(str::to_owned);
            if let Some(mechanism) = &mechanism
                && !matches!(
                    mechanism.as_str(),
                    "PLAIN" | "SCRAM-256" | "SCRAM-512" | "OAUTHBEARER" | "AWS_MSK_IAM"
                )
            {
                return Err(validation(format!(
                    "authentication.mechanism must be PLAIN, SCRAM-256, SCRAM-512, OAUTHBEARER, or AWS_MSK_IAM, not '{mechanism}'"
                )));
            }
            if let Some(protocol) = config
                .get("security")
                .and_then(|security| security["protocol"].as_str())
                && !matches!(protocol, "SASL_SSL" | "SASL_PLAINTEXT" | "SSL")
            {
                return Err(validation(format!(
                    "security.protocol must be SASL_SSL, SASL_PLAINTEXT, or SSL, not '{protocol}'"
                )));
            }
            let mut missing = Vec::new();
            if !present(config, &["bootstrapServers"]) {
                missing.push(KAFKA_BOOTSTRAP);
            }
            match mechanism.as_deref() {
                None => missing.push(KAFKA_MECHANISM),
                Some("AWS_MSK_IAM") => {
                    // SASL credentials do not apply to MSK IAM and must not reach Atlas.
                    if let Some(auth) = config
                        .get_mut("authentication")
                        .and_then(Value::as_object_mut)
                    {
                        auth.remove("username");
                        auth.remove("password");
                    }
                    if !present(config, &["authentication", "aws", "roleArn"]) {
                        missing.push(KAFKA_ROLE_ARN);
                    }
                }
                Some(_) => {
                    if !present(config, &["authentication", "username"]) {
                        missing.push(("Username", "SASL username for Kafka authentication"));
                    }
                    if !present(config, &["authentication", "password"]) {
                        missing.push(("Password", "SASL password for Kafka authentication"));
                    }
                }
            }
            if !present(config, &["security", "protocol"]) {
                missing.push(KAFKA_PROTOCOL);
            }
            missing
        }
        "Cluster" => {
            if !present(config, &["dbRoleToExecute"]) {
                config.insert(
                    "dbRoleToExecute".into(),
                    json!({"role": "readWriteAnyDatabase", "type": "BUILT_IN"}),
                );
            }
            if present(config, &["clusterName"]) {
                Vec::new()
            } else {
                vec![(
                    "Cluster Name",
                    "Name of an Atlas cluster in this project (see `mongodb-cli atlas clusters list`)",
                )]
            }
        }
        "S3" | "AWSKinesisDataStreams" | "AWSLambda" => {
            note = Some(
                "The IAM role ARN must first be registered in the Atlas project via Cloud Provider Access (Atlas UI → Project Settings → Cloud Provider Access).",
            );
            if present(config, &["aws", "roleArn"]) {
                Vec::new()
            } else {
                vec![(
                    "AWS IAM Role ARN",
                    "IAM role ARN registered in this Atlas project via Cloud Provider Access (e.g. 'arn:aws:iam::123456789:role/my-role')",
                )]
            }
        }
        "SchemaRegistry" => {
            prepare_schema_registry(config);
            let field = |key: &str| config.get(key).unwrap_or(&Value::Null);
            let auth = field("schemaRegistryAuthentication");
            let credentials = auth["type"] != "SASL_INHERIT";
            let mut missing = Vec::new();
            if !field("schemaRegistryUrls")
                .as_array()
                .is_some_and(|urls| !urls.is_empty())
            {
                missing.push((
                    "Schema Registry URL",
                    "Schema Registry endpoint URL (e.g. 'https://schema-registry.example.com')",
                ));
            }
            if credentials && !truthy(&auth["username"]) {
                missing.push(("Username", "Username for Schema Registry authentication"));
            }
            if credentials && !truthy(&auth["password"]) {
                missing.push(("Password", "Password for Schema Registry authentication"));
            }
            missing
        }
        "Https" if !present(config, &["url"]) => {
            vec![(
                "Endpoint URL",
                "HTTPS endpoint URL (e.g. 'https://api.example.com/webhook')",
            )]
        }
        _ => Vec::new(),
    };
    if missing.is_empty() {
        return Ok(());
    }
    let list: Vec<String> = missing
        .iter()
        .map(|(title, description)| format!("  - {title}: {description}"))
        .collect();
    let mut hint = "add the missing fields to the configuration file and retry".to_owned();
    if let Some(note) = note {
        hint = format!("{hint}. {note}");
    }
    Err(validation(format!(
        "Cannot create {kind} connection — the following required information is missing:\n{}",
        list.join("\n")
    ))
    .with_hint(hint))
}

const KAFKA_BOOTSTRAP: (&str, &str) = (
    "Bootstrap Servers",
    "Comma-separated broker addresses (e.g. 'broker1:9092,broker2:9092')",
);
const KAFKA_MECHANISM: (&str, &str) = (
    "Authentication Mechanism",
    "SASL mechanism: 'PLAIN', 'SCRAM-256', 'SCRAM-512', 'OAUTHBEARER', or 'AWS_MSK_IAM'",
);
const KAFKA_ROLE_ARN: (&str, &str) = (
    "AWS IAM Role ARN",
    "IAM role ARN registered in this Atlas project via Cloud Provider Access. Required for AWS_MSK_IAM authentication.",
);
const KAFKA_PROTOCOL: (&str, &str) = (
    "Security Protocol",
    "Security protocol: 'SASL_SSL', 'SASL_PLAINTEXT', or 'SSL'",
);

/// Accept the URL and credential spellings the MCP server accepts, and apply its defaults.
fn prepare_schema_registry(config: &mut Map<String, Value>) {
    if !truthy(config.get("schemaRegistryUrls").unwrap_or(&Value::Null)) {
        let alias = ["url", "urls", "endpoint", "schemaRegistryUrl"]
            .iter()
            .find_map(|key| config.get(*key).filter(|value| truthy(value)).cloned());
        if let Some(alias) = alias {
            let urls = if alias.is_array() {
                alias
            } else {
                json!([alias])
            };
            config.insert("schemaRegistryUrls".into(), urls);
            for key in ["url", "urls", "endpoint", "schemaRegistryUrl"] {
                config.remove(key);
            }
        }
    }
    let has = |config: &Map<String, Value>, key: &str| config.get(key).is_some_and(truthy);
    if !has(config, "schemaRegistryAuthentication")
        && (has(config, "username") || has(config, "authentication"))
    {
        let source = config.get("authentication").cloned().unwrap_or_default();
        let pick = |config: &Map<String, Value>, key: &str| {
            config
                .get(key)
                .filter(|value| truthy(value))
                .cloned()
                .unwrap_or_else(|| source[key].clone())
        };
        let auth = compact(json!({
            "type": "USER_INFO",
            "username": pick(config, "username"),
            "password": pick(config, "password"),
        }));
        config.insert("schemaRegistryAuthentication".into(), auth);
        for key in ["username", "password", "authentication"] {
            config.remove(key);
        }
    }
    if !has(config, "provider") {
        config.insert("provider".into(), json!("CONFLUENT"));
    }
    if !has(config, "schemaRegistryAuthentication") {
        config.insert("schemaRegistryAuthentication".into(), json!({}));
    }
    if let Some(auth) = config
        .get_mut("schemaRegistryAuthentication")
        .and_then(Value::as_object_mut)
        && !auth.get("type").is_some_and(truthy)
    {
        auth.insert("type".into(), json!("USER_INFO"));
    }
}

pub async fn create_connection(
    client: &AtlasClient,
    project_id: &str,
    workspace: &str,
    name: &str,
    kind: &str,
    mut config: Map<String, Value>,
) -> Result<Outcome, ClientError> {
    let private_link = config
        .get("networking")
        .is_some_and(|networking| networking["access"]["type"] == "PRIVATE_LINK");
    config.insert("name".into(), json!(name));
    config.insert("type".into(), json!(kind));
    let path = format!("{}/connections", workspace_path(project_id, workspace));
    client.post(&path, Some(&Value::Object(config))).await?;
    let outcome = Outcome::new(json!({"resource": "connection"}));
    Ok(if private_link {
        outcome.warn("this PrivateLink connection starts in the PENDING state; poll `mongodb-cli atlas streams connections inspect` until it is READY")
    } else {
        outcome
    })
}

/// Update a connection. Unlike creation, required fields are not checked, so partial updates work.
pub async fn update_connection(
    client: &AtlasClient,
    project_id: &str,
    workspace: &str,
    name: &str,
    mut config: Map<String, Value>,
) -> Result<Outcome, ClientError> {
    let path = connection_path(project_id, workspace, name);
    let existing = client.get(&path, &[]).await?;
    normalize_connection_config(&mut config);
    // The type cannot change after creation, so the existing type is always sent.
    if let Some(kind) = existing.get("type") {
        config.insert("type".into(), kind.clone());
    }
    config.insert("name".into(), json!(name));
    let updated = client.patch(&path, &Value::Object(config)).await?;
    let state = updated["state"]
        .as_str()
        .or_else(|| existing["state"].as_str())
        .filter(|state| matches!(*state, "PENDING" | "READY" | "DELETING" | "FAILED"));
    Ok(Outcome::new(match state {
        Some(state) => json!({"connectionState": state}),
        None => json!({}),
    }))
}

pub async fn delete_connection(
    client: &AtlasClient,
    project_id: &str,
    workspace: &str,
    name: &str,
) -> Result<Outcome, ClientError> {
    let processors_path = format!("{}/processors", workspace_path(project_id, workspace));
    // Like the MCP server, only the first page of processors and their pipelines are checked.
    if let Ok(processors) = client.get(&processors_path, &[]).await {
        let running: Vec<&str> = results(&processors)
            .iter()
            .filter(|processor| {
                processor["state"] == "STARTED"
                    && connection_names(&processor["pipeline"])
                        .iter()
                        .any(|used| used == name)
            })
            .filter_map(|processor| processor["name"].as_str())
            .collect();
        if !running.is_empty() {
            return Err(rejected(format!(
                "Connection '{name}' is referenced by running processor(s): {}.",
                running.join(", ")
            ))
            .with_hint("stop them with `mongodb-cli atlas streams processors stop`, then retry the deletion"));
        }
    }
    client
        .delete(&connection_path(project_id, workspace, name))
        .await?;
    Ok(Outcome::new(json!({"resource": "connection"})).warn(
        "deletion runs asynchronously; confirm with `mongodb-cli atlas streams connections list`",
    ))
}

pub async fn list_processors(
    client: &AtlasClient,
    project_id: &str,
    workspace: &str,
    limit: u32,
    page: u32,
    detailed: bool,
) -> Result<Outcome, ClientError> {
    let path = format!("{}/processors", workspace_path(project_id, workspace));
    let response = client.get(&path, &page_query(limit, page)).await?;
    let items = results(&response)
        .iter()
        .map(|processor| {
            let mut summary = compact(json!({
                "name": processor["name"],
                "state": processor["state"],
                "tier": processor["tier"],
                "effectiveTier": processor["effectiveTier"],
            }));
            if let Some(autoscaling) = autoscaling(&processor["options"]["autoscaling"]) {
                summary["autoscaling"] = autoscaling;
            }
            summary
        })
        .collect();
    let mut outcome = listed("processors", items, &response, limit, page);
    if detailed {
        outcome.data = redact(&response);
    }
    Ok(outcome)
}

pub async fn inspect_processor(
    client: &AtlasClient,
    project_id: &str,
    workspace: &str,
    name: &str,
    detailed: bool,
) -> Result<Outcome, ClientError> {
    let response = client
        .get(&processor_path(project_id, workspace, name), &[])
        .await?;
    Ok(Outcome::new(if detailed {
        redact(&response)
    } else {
        processor_details(&response, true)
    }))
}

pub async fn diagnose_processor(
    client: &AtlasClient,
    project_id: &str,
    workspace: &str,
    name: &str,
) -> Result<Outcome, ClientError> {
    let (processor_path, connections_path) = (
        processor_path(project_id, workspace, name),
        format!("{}/connections", workspace_path(project_id, workspace)),
    );
    let (processor, connections) = tokio::join!(
        client.get(&processor_path, &[]),
        client.get(&connections_path, &[])
    );
    let processor = processor?;
    let mut data = processor_details(&processor, false);
    let mut outcome = Outcome::new(Value::Null);
    match connections {
        Ok(connections) if !results(&connections).is_empty() => {
            data["connectionHealth"] = results(&connections)
                .iter()
                .map(connection_summary)
                .collect();
        }
        Ok(_) => {}
        Err(error) => outcome = outcome.warn(format!("connection health unavailable: {error}")),
    }
    let stats = &data["stats"];
    let count = |key: &str| stats[key].as_f64().unwrap_or(0.0);
    let (input, output, dlq) = (
        count("inputMessageCount"),
        count("outputMessageCount"),
        count("dlqMessageCount"),
    );
    if input > 0.0 && output == 0.0 && dlq > 0.0 {
        outcome = outcome.warn(format!(
            "all {dlq} input messages went to the DLQ (0 successful outputs); this usually means a schema mismatch, serialization error, or sink problem, so query the DLQ collection"
        ));
    } else if input > 0.0 && dlq > 0.0 && (dlq / input * 100.0).round() > 50.0 {
        outcome = outcome.warn(format!(
            "{}% of messages are going to the DLQ; check the DLQ collection for error patterns",
            (dlq / input * 100.0).round()
        ));
    }
    match processor["state"].as_str() {
        Some("FAILED") => outcome = outcome.warn("the processor failed: check the DLQ, fix the pipeline with `mongodb-cli atlas streams processors modify`, then restart with `mongodb-cli atlas streams processors start --no-resume-from-checkpoint`"),
        Some("STOPPED") => outcome = outcome.warn("the processor is stopped; resume it with `mongodb-cli atlas streams processors start`"),
        _ => {}
    }
    outcome.data = data;
    Ok(outcome)
}

/// The MCP server's processor summary; `processorState` only for the four known states.
fn processor_details(processor: &Value, include_pipeline: bool) -> Value {
    let mut details = Map::new();
    if let Some(state) = processor["state"]
        .as_str()
        .filter(|state| matches!(*state, "STARTED" | "STOPPED" | "CREATED" | "FAILED"))
    {
        details.insert("processorState".into(), json!(state));
    }
    for key in ["tier", "effectiveTier"] {
        if let Some(value) = processor.get(key) {
            details.insert(key.into(), value.clone());
        }
    }
    if let Some(autoscaling) = autoscaling(&processor["options"]["autoscaling"]) {
        details.insert("autoscaling".into(), autoscaling);
    }
    if let Some(stats) = processor["stats"]
        .as_object()
        .filter(|stats| !stats.is_empty())
    {
        let count = |key: &str| {
            stats
                .get(key)
                .filter(|value| value.is_number())
                .cloned()
                .unwrap_or(json!(0))
        };
        details.insert(
            "stats".into(),
            json!({
                "inputMessageCount": count("inputMessageCount"),
                "outputMessageCount": count("outputMessageCount"),
                "dlqMessageCount": count("dlqMessageCount"),
            }),
        );
    }
    let dlq = &processor["options"]["dlq"];
    if truthy(dlq) {
        details.insert(
            "dlq".into(),
            compact(json!({"connectionName": dlq["connectionName"], "db": dlq["db"], "coll": dlq["coll"]})),
        );
    }
    if include_pipeline && truthy(&processor["pipeline"]) {
        details.insert("pipeline".into(), processor["pipeline"].clone());
    }
    Value::Object(details)
}

/// Keep only the declared autoscaling fields; a missing or `null` setting is omitted.
fn autoscaling(value: &Value) -> Option<Value> {
    let settings = value.as_object()?;
    let mut kept = Map::new();
    if let Some(enabled) = settings.get("enabled").and_then(Value::as_bool) {
        kept.insert("enabled".into(), json!(enabled));
    }
    for key in ["minTier", "maxTier"] {
        if let Some(tier) = settings.get(key) {
            kept.insert(key.into(), tier.clone());
        }
    }
    Some(Value::Object(kept))
}

/// Check a new processor's pipeline: it must start with `$source` and must not use system
/// variables that streaming does not support.
pub fn validate_pipeline(pipeline: &[Value]) -> Result<(), ClientError> {
    if pipeline.is_empty() {
        return Err(validation(
            "pipeline is required. Provide an array of aggregation stages starting with $source and ending with a sink stage ($merge, $emit, $iceberg, $https, or $externalFunction).",
        ));
    }
    let first = pipeline[0]
        .as_object()
        .and_then(|stage| stage.keys().next());
    if first.map(String::as_str) != Some("$source") {
        return Err(validation(format!(
            "Invalid pipeline: first stage must be `$source`, but found `{}`.",
            first.map_or("undefined", String::as_str)
        ))
        .with_hint(r#"a streaming pipeline starts with $source, such as {"$source": {"connectionName": "myConnection", "topic": "myTopic"}}"#));
    }
    let text = Value::Array(pipeline.to_vec()).to_string();
    let unsupported: Vec<&str> = ["$$NOW", "$$ROOT", "$$CURRENT"]
        .into_iter()
        .filter(|variable| text.contains(variable))
        .collect();
    if !unsupported.is_empty() {
        let verb = if unsupported.len() == 1 { "is" } else { "are" };
        return Err(validation(format!(
            "pipeline contains {} which {verb} not available in Atlas Stream Processing pipelines",
            unsupported.join(", ")
        ))
        .with_hint("remove or replace them before deploying"));
    }
    Ok(())
}

/// Every string `connectionName` in a value, in first-seen order.
fn connection_names(value: &Value) -> Vec<String> {
    fn walk(value: &Value, names: &mut Vec<String>) {
        match value {
            Value::Array(items) => items.iter().for_each(|item| walk(item, names)),
            Value::Object(members) => {
                for (key, member) in members {
                    match member {
                        Value::String(name) if key == "connectionName" => {
                            if !names.contains(name) {
                                names.push(name.clone());
                            }
                        }
                        _ => walk(member, names),
                    }
                }
            }
            _ => {}
        }
    }
    let mut names = Vec::new();
    walk(value, &mut names);
    names
}

/// A new processor; `dlq` and `autoscaling` are the Atlas option objects.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcessorSpec {
    pub pipeline: Vec<Value>,
    pub dlq: Option<Value>,
    pub tier: Option<String>,
    pub autoscaling: Option<Value>,
    pub start: bool,
}

pub async fn create_processor(
    client: &AtlasClient,
    project_id: &str,
    workspace: &str,
    name: &str,
    spec: &ProcessorSpec,
) -> Result<Outcome, ClientError> {
    let base = workspace_path(project_id, workspace);
    let mut referenced = connection_names(&Value::Array(spec.pipeline.clone()));
    if let Some(dlq) = spec
        .dlq
        .as_ref()
        .and_then(|dlq| dlq["connectionName"].as_str())
        && !referenced.iter().any(|name| name == dlq)
    {
        referenced.push(dlq.to_owned());
    }
    // A soft check: when the connections cannot be listed, Atlas reports problems itself.
    let query = [
        ("itemsPerPage", "100".to_owned()),
        ("pageNum", "1".to_owned()),
    ];
    if !referenced.is_empty()
        && let Ok(listed) = client.get(&format!("{base}/connections"), &query).await
    {
        let available: Vec<String> = results(&listed)
            .iter()
            .map(|connection| {
                connection["name"]
                    .as_str()
                    .map_or_else(|| connection["name"].to_string(), str::to_owned)
            })
            .collect();
        let missing: Vec<&str> = referenced
            .iter()
            .filter(|name| !available.contains(name))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            let available = if available.is_empty() {
                "(no connections found)".to_owned()
            } else {
                available.join(", ")
            };
            return Err(rejected(format!(
                "Cannot create processor — the pipeline references connection(s) that do not exist in workspace '{workspace}'. Missing: {}. Available connections: {available}.",
                missing.join(", ")
            ))
            .with_hint("add the missing connections with `mongodb-cli atlas streams connections create`, then retry"));
        }
    }
    let mut body = json!({"name": name, "pipeline": spec.pipeline});
    if let Some(tier) = &spec.tier {
        body["tier"] = json!(tier);
    }
    let mut options = Map::new();
    if let Some(dlq) = &spec.dlq {
        options.insert("dlq".into(), dlq.clone());
    }
    if let Some(autoscaling) = &spec.autoscaling {
        options.insert("autoscaling".into(), autoscaling.clone());
    }
    if !options.is_empty() {
        body["options"] = Value::Object(options);
    }
    client
        .post(&format!("{base}/processor"), Some(&body))
        .await?;
    let mut outcome = Outcome::new(json!({"resource": "processor"}));
    if spec.start {
        client
            .post(
                &format!("{}:start", processor_path(project_id, workspace, name)),
                None,
            )
            .await?;
        outcome = outcome.warn(BILLING);
    }
    if spec.dlq.is_none() {
        outcome = outcome.warn("consider a dead letter queue (--dlq-connection, --dlq-db, --dlq-coll) for production use");
    }
    Ok(outcome)
}

/// Options for one processor start; any of them selects `:startWith`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StartOptions {
    pub tier: Option<String>,
    pub autoscaling: Option<Value>,
    pub resume_from_checkpoint: Option<bool>,
    pub start_at_operation_time: Option<String>,
}

pub async fn start_processor(
    client: &AtlasClient,
    project_id: &str,
    workspace: &str,
    name: &str,
    options: &StartOptions,
) -> Result<Outcome, ClientError> {
    let path = processor_path(project_id, workspace, name);
    let processor = client.get(&path, &[]).await?;
    if processor["state"] == "STARTED" {
        return Err(
            rejected(format!("Processor '{name}' is already running.")).with_hint(
                "stop it with `mongodb-cli atlas streams processors stop` first to restart it",
            ),
        );
    }
    if let Some(tier) = &options.tier
        && let Ok(workspace_info) = client
            .get(&workspace_path(project_id, workspace), &[])
            .await
        && let Some(maximum) = workspace_info["streamConfig"]["maxTierSize"].as_str()
        && let (Some(requested), Some(allowed)) = (tier_rank(tier), tier_rank(maximum))
        && requested > allowed
    {
        return Err(rejected(format!(
            "Cannot start processor with tier '{tier}' — workspace '{workspace}' has a maximum tier of '{maximum}'."
        ))
        .with_hint(format!(
            "use a tier up to {maximum}, or raise the workspace tier first with `mongodb-cli atlas streams workspaces update {workspace} --tier {tier}`"
        )));
    }
    let mut body = Map::new();
    if let Some(tier) = &options.tier {
        body.insert("tier".into(), json!(tier));
    }
    if let Some(autoscaling) = &options.autoscaling {
        body.insert("autoscaling".into(), autoscaling.clone());
    }
    if let Some(resume) = options.resume_from_checkpoint {
        body.insert("resumeFromCheckpoint".into(), json!(resume));
    }
    if let Some(time) = &options.start_at_operation_time {
        body.insert("startAtOperationTime".into(), json!(time));
    }
    if body.is_empty() {
        client.post(&format!("{path}:start"), None).await?;
    } else {
        client
            .post(&format!("{path}:startWith"), Some(&Value::Object(body)))
            .await?;
    }
    Ok(Outcome::new(json!({"processorState": "STARTED"})).warn(BILLING))
}

fn tier_rank(tier: &str) -> Option<usize> {
    TIERS.iter().position(|candidate| *candidate == tier)
}

pub async fn stop_processor(
    client: &AtlasClient,
    project_id: &str,
    workspace: &str,
    name: &str,
) -> Result<Outcome, ClientError> {
    let path = processor_path(project_id, workspace, name);
    // A failed lookup may mean the processor is in an error state, so stopping still proceeds.
    if let Ok(processor) = client.get(&path, &[]).await
        && let Some(state @ ("STOPPED" | "CREATED")) = processor["state"].as_str()
    {
        return Ok(Outcome::new(json!({"processorState": state})).warn(format!(
            "processor '{name}' is not running (state: {state}); nothing was changed"
        )));
    }
    client.post(&format!("{path}:stop"), None).await?;
    Ok(Outcome::new(json!({"processorState": "STOPPED"})))
}

/// Changes to a stopped processor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcessorChanges {
    pub pipeline: Option<Vec<Value>>,
    pub dlq: Option<Value>,
    pub new_name: Option<String>,
    pub tier: Option<String>,
    pub autoscaling: Option<Value>,
}

impl ProcessorChanges {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

pub async fn modify_processor(
    client: &AtlasClient,
    project_id: &str,
    workspace: &str,
    name: &str,
    changes: &ProcessorChanges,
) -> Result<Outcome, ClientError> {
    let path = processor_path(project_id, workspace, name);
    let processor = client.get(&path, &[]).await?;
    if processor["state"] == "STARTED" {
        return Err(rejected(format!(
            "Processor '{name}' must be stopped before modifying."
        ))
        .with_hint("stop it with `mongodb-cli atlas streams processors stop` first"));
    }
    let mut body = Map::new();
    if let Some(pipeline) = &changes.pipeline {
        body.insert("pipeline".into(), json!(pipeline));
    }
    if let Some(new_name) = &changes.new_name {
        body.insert("name".into(), json!(new_name));
    }
    if let Some(tier) = &changes.tier {
        body.insert("tier".into(), json!(tier));
    }
    let mut options = Map::new();
    if let Some(dlq) = &changes.dlq {
        options.insert("dlq".into(), dlq.clone());
    }
    if let Some(autoscaling) = &changes.autoscaling {
        options.insert("autoscaling".into(), autoscaling.clone());
    }
    if !options.is_empty() {
        body.insert("options".into(), Value::Object(options));
    }
    let updated = client.patch(&path, &Value::Object(body)).await?;
    // Like the MCP server, the result always reports STOPPED.
    let mut data = json!({"processorState": "STOPPED"});
    if changes.tier.is_some() && !updated["tier"].is_null() {
        data["tier"] = updated["tier"].clone();
    }
    if (changes.tier.is_some() || changes.autoscaling.is_some())
        && !updated["effectiveTier"].is_null()
    {
        data["effectiveTier"] = updated["effectiveTier"].clone();
    }
    if changes.autoscaling.is_some()
        && let Some(autoscaling) = autoscaling(&updated["options"]["autoscaling"])
    {
        data["autoscaling"] = autoscaling;
    }
    Ok(Outcome::new(data))
}

pub async fn delete_processor(
    client: &AtlasClient,
    project_id: &str,
    workspace: &str,
    name: &str,
) -> Result<Outcome, ClientError> {
    let path = processor_path(project_id, workspace, name);
    // Running processors are stopped first; lookup or stop failures do not block the deletion.
    if let Ok(processor) = client.get(&path, &[]).await
        && processor["state"] == "STARTED"
    {
        let _ = client.post(&format!("{path}:stop"), None).await;
    }
    client.delete(&path).await?;
    Ok(Outcome::new(json!({"resource": "processor"})))
}

/// PrivateLink connections and, for a provider and region, the account details for peering.
pub async fn networking(
    client: &AtlasClient,
    project_id: &str,
    account: Option<(&str, &str)>,
) -> Result<Outcome, ClientError> {
    let base = format!("/groups/{}/streams", seg(project_id));
    let mut outcome = Outcome::new(Value::Null);
    let mut data = json!({"privateLinks": []});
    match client
        .get(&format!("{base}/privateLinkConnections"), &[])
        .await
    {
        Ok(response) => {
            data["privateLinks"] = results(&response)
                .iter()
                .filter(|link| truthy(&link["_id"]))
                .map(|link| {
                    compact(json!({
                        "id": link["_id"],
                        "provider": link["provider"],
                        "region": link["region"],
                        "state": link["state"],
                        "vendor": link["vendor"],
                    }))
                })
                .collect();
        }
        Err(error) => {
            outcome = outcome.warn(format!("PrivateLink connections unavailable: {error}"))
        }
    }
    if let Some((provider, region)) = account {
        let query = [
            ("cloudProvider", provider.to_owned()),
            ("regionName", region.to_owned()),
        ];
        match client.get(&format!("{base}/accountDetails"), &query).await {
            Ok(details) => {
                let mut summary = Map::new();
                for key in [
                    "awsAccountId",
                    "azureSubscriptionId",
                    "gcpProjectId",
                    "cidrBlock",
                    "vpcId",
                    "virtualNetworkName",
                    "vpcNetworkName",
                ] {
                    if let Some(value) = details.get(key) {
                        summary.insert(key.into(), value.clone());
                    }
                }
                if !summary.is_empty() {
                    data["accountDetails"] = Value::Object(summary);
                }
            }
            Err(error) => {
                outcome = outcome.warn(format!(
                    "account details for {provider}/{region} unavailable: {error}"
                ));
            }
        }
    }
    outcome.data = data;
    Ok(outcome)
}

/// A PrivateLink configuration needs a cloud provider; Atlas checks the vendor-specific fields.
pub fn validate_private_link(config: &Map<String, Value>) -> Result<(), ClientError> {
    match config.get("provider").and_then(Value::as_str) {
        Some("AWS" | "GCP" | "AZURE") => Ok(()),
        _ => Err(validation("the PrivateLink configuration needs \"provider\": AWS, AZURE, or GCP").with_hint(
            "vendor fields: AWS CONFLUENT {vendor, region, serviceEndpointId, dnsDomain, dnsSubDomain}; AWS MSK {vendor, arn, authenticationScheme}; AWS S3 or KINESIS {vendor, region, serviceEndpointId}; AZURE EVENTHUB {vendor, region, dnsDomain, serviceEndpointId}; AZURE CONFLUENT {vendor, region, dnsDomain, azureResourceIds}; GCP CONFLUENT {vendor, region, dnsDomain, gcpServiceAttachmentUris}",
        )),
    }
}

pub async fn create_private_link(
    client: &AtlasClient,
    project_id: &str,
    config: Map<String, Value>,
) -> Result<Outcome, ClientError> {
    client
        .post(
            &format!("/groups/{}/streams/privateLinkConnections", seg(project_id)),
            Some(&Value::Object(config)),
        )
        .await?;
    Ok(Outcome::new(json!({"resource": "privatelink"})).warn(
        "the PrivateLink connection takes a few minutes to become active; check it with `mongodb-cli atlas streams networking show`",
    ))
}

pub async fn delete_private_link(
    client: &AtlasClient,
    project_id: &str,
    id: &str,
) -> Result<Outcome, ClientError> {
    client
        .delete(&format!(
            "/groups/{}/streams/privateLinkConnections/{}",
            seg(project_id),
            seg(id)
        ))
        .await?;
    Ok(Outcome::new(json!({"resource": "privatelink"})))
}

fn peering_path(project_id: &str, id: &str) -> String {
    format!(
        "/groups/{}/streams/vpcPeeringConnections/{}",
        seg(project_id),
        seg(id)
    )
}

pub async fn accept_peering(
    client: &AtlasClient,
    project_id: &str,
    id: &str,
    requester_account_id: &str,
    requester_vpc_id: &str,
) -> Result<Outcome, ClientError> {
    let body =
        json!({"requesterAccountId": requester_account_id, "requesterVpcId": requester_vpc_id});
    client
        .post(
            &format!("{}:accept", peering_path(project_id, id)),
            Some(&body),
        )
        .await?;
    Ok(Outcome::new(json!({"peeringState": "ACCEPTED"})))
}

pub async fn reject_peering(
    client: &AtlasClient,
    project_id: &str,
    id: &str,
) -> Result<Outcome, ClientError> {
    client
        .post(&format!("{}:reject", peering_path(project_id, id)), None)
        .await?;
    Ok(Outcome::new(json!({"peeringState": "REJECTED"})))
}

pub async fn delete_peering(
    client: &AtlasClient,
    project_id: &str,
    id: &str,
) -> Result<Outcome, ClientError> {
    client.delete(&peering_path(project_id, id)).await?;
    Ok(Outcome::new(json!({"resource": "peering"})))
}

/// Mask secret values recursively, keeping useful metadata such as user names.
pub fn redact(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        Value::Object(members) => Value::Object(
            members
                .iter()
                .map(|(key, member)| {
                    let secret = SECRET_KEYS.contains(&key.to_ascii_lowercase().as_str());
                    (
                        key.clone(),
                        if secret {
                            json!("<redacted>")
                        } else {
                            redact(member)
                        },
                    )
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Replace the hint of an Atlas error with the MCP server's Stream Processing advice.
pub fn with_hint(error: ClientError) -> ClientError {
    let message = error.to_string();
    let has = |parts: &[&str]| parts.iter().all(|part| message.contains(part));
    let hint = match error.server_code() {
        Some(404) => Some(
            "list the available workspaces, connections, and processors with `mongodb-cli atlas streams <group> list`",
        ),
        Some(403) if has(&["active", "processor"]) => Some(
            "the workspace may have active processors; stop them with `mongodb-cli atlas streams processors stop`, then retry",
        ),
        Some(400) if has(&["IDLUnknownField", "topic", "AtlasCollection"]) => Some(
            "The 'topic' field is not valid inside $merge. Use $emit (not $merge) to write to Kafka: {$emit: {connectionName, topic}}.",
        ),
        Some(400) if has(&["IDLUnknownField", "schemaRegistryName"]) => Some(
            "Use schemaRegistry: {connectionName, valueSchema: {type: 'avro', schema: {<avro-def>}}} instead of schemaRegistryName.",
        ),
        Some(400) if has(&["IDLFailedToParse", "valueSchema", "missing"]) => Some(
            "schemaRegistry.valueSchema is required. Include: {type: 'avro', schema: {<avro-schema-definition>}, options: {autoRegisterSchemas: true}}.",
        ),
        Some(400) if has(&["BadValue", "Enumeration", "type"]) => Some(
            "Schema type values are case-sensitive. Use lowercase 'avro' (not 'AVRO' or 'Avro').",
        ),
        Some(400) if has(&["IDLUnknownField", "MergeOperatorSpec"]) => Some(
            "Invalid field in $merge stage. $merge writes to Atlas clusters: {$merge: {into: {connectionName, db, coll}}}. For Kafka/external sinks, use $emit instead.",
        ),
        Some(400) => Some(
            "This usually indicates invalid configuration or pipeline syntax. Check the request parameters and try again.",
        ),
        Some(409) => Some(
            "The resource may already exist or be in a state that prevents this operation; check its current state with the list and inspect commands.",
        ),
        _ if message.contains("resumeFromCheckpoint") => Some(
            "a window stage interval probably changed: restart with --no-resume-from-checkpoint (dropping accumulated window state), or delete and recreate the processor",
        ),
        _ if message.contains("SASL") || message.contains("authentication failed") => Some(
            "check the Kafka credentials: passwords may need a prefix such as 'cflt/', and the mechanism (PLAIN, SCRAM-256, SCRAM-512) must match; `mongodb-cli atlas streams processors diagnose` shows details",
        ),
        _ if message.contains("INVALID_STATE") => Some(
            "check the processor's current state with `mongodb-cli atlas streams processors inspect` before retrying",
        ),
        _ => None,
    };
    match hint {
        Some(hint) => error.with_hint(hint),
        None => error,
    }
}

#[cfg(test)]
mod tests {
    use crate::ErrorCode;

    use super::*;

    fn config(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn connection_configs_report_missing_fields_per_type() {
        for (kind, input, expected) in [
            (
                "Kafka",
                json!({}),
                vec![
                    "Bootstrap Servers",
                    "Authentication Mechanism",
                    "Security Protocol",
                ],
            ),
            (
                "Kafka",
                json!({"bootstrapServers": "b:9092", "authentication": {"mechanism": "PLAIN"}, "security": {"protocol": "SASL_SSL"}}),
                vec!["Username", "Password"],
            ),
            (
                "Kafka",
                json!({"bootstrapServers": "b:9092", "authentication": {"mechanism": "AWS_MSK_IAM"}, "security": {"protocol": "SASL_SSL"}}),
                vec!["AWS IAM Role ARN"],
            ),
            ("Cluster", json!({}), vec!["Cluster Name"]),
            ("S3", json!({"aws": {}}), vec!["AWS IAM Role ARN"]),
            ("AWSLambda", json!({}), vec!["AWS IAM Role ARN"]),
            (
                "SchemaRegistry",
                json!({}),
                vec!["Schema Registry URL", "Username", "Password"],
            ),
            (
                "Https",
                json!({"headers": {"a": "b"}}),
                vec!["Endpoint URL"],
            ),
        ] {
            let mut input = config(input);
            let error = prepare_connection_config(kind, &mut input).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Validation);
            let message = error.to_string();
            assert!(
                message.starts_with(&format!("Cannot create {kind} connection")),
                "{message}"
            );
            for title in &expected {
                assert!(
                    message.contains(&format!("  - {title}:")),
                    "{kind} {title}: {message}"
                );
            }
            assert_eq!(
                message.matches("\n  - ").count(),
                expected.len(),
                "{message}"
            );
        }
        let mut sample = config(json!({}));
        assert!(prepare_connection_config("Sample", &mut sample).is_ok());
        let mut aws = config(json!({}));
        let hint = prepare_connection_config("S3", &mut aws).unwrap_err();
        assert!(hint.hint().unwrap().contains("Cloud Provider Access"));
    }

    #[test]
    fn connection_configs_are_normalized_with_defaults() {
        let mut kafka = config(json!({
            "bootstrapServers": ["a:9092", "b:9092"],
            "authentication": {"mechanism": "AWS_MSK_IAM", "username": "u", "password": "p", "aws": {"roleArn": "arn:aws:iam::1:role/r"}},
            "security": {"protocol": "SASL_SSL"}
        }));
        prepare_connection_config("Kafka", &mut kafka).unwrap();
        assert_eq!(kafka["bootstrapServers"], "a:9092,b:9092");
        assert_eq!(
            kafka["authentication"],
            json!({"mechanism": "AWS_MSK_IAM", "aws": {"roleArn": "arn:aws:iam::1:role/r"}}),
            "MSK IAM drops SASL credentials"
        );
        let mut cluster = config(json!({"clusterName": "c0"}));
        prepare_connection_config("Cluster", &mut cluster).unwrap();
        assert_eq!(
            cluster["dbRoleToExecute"],
            json!({"role": "readWriteAnyDatabase", "type": "BUILT_IN"})
        );
        let mut registry =
            config(json!({"url": "https://r.example.com", "username": "u", "password": "p"}));
        prepare_connection_config("SchemaRegistry", &mut registry).unwrap();
        assert_eq!(
            Value::Object(registry),
            json!({
                "schemaRegistryUrls": ["https://r.example.com"],
                "schemaRegistryAuthentication": {"type": "USER_INFO", "username": "u", "password": "p"},
                "provider": "CONFLUENT"
            })
        );
        let mut inherited = config(
            json!({"schemaRegistryUrls": "https://a, https://b", "schemaRegistryAuthentication": {"type": "SASL_INHERIT"}}),
        );
        prepare_connection_config("SchemaRegistry", &mut inherited).unwrap();
        assert_eq!(
            inherited["schemaRegistryUrls"],
            json!(["https://a", "https://b"])
        );
        let mut invalid = config(json!({"authentication": {"mechanism": "GSSAPI"}}));
        assert!(
            prepare_connection_config("Kafka", &mut invalid)
                .unwrap_err()
                .to_string()
                .contains("GSSAPI")
        );
    }

    #[test]
    fn pipelines_start_with_source_and_avoid_system_variables() {
        assert!(
            validate_pipeline(&[
                json!({"$source": {"connectionName": "a"}}),
                json!({"$emit": {}})
            ])
            .is_ok()
        );
        for (pipeline, expected) in [
            (vec![], "pipeline is required"),
            (vec![json!({"$match": {}})], "found `$match`"),
            (
                vec![
                    json!({"$source": {}}),
                    json!({"$addFields": {"at": "$$NOW", "r": "$$ROOT"}}),
                ],
                "$$NOW, $$ROOT which are",
            ),
        ] {
            let error = validate_pipeline(&pipeline).unwrap_err();
            assert!(error.to_string().contains(expected), "{expected}: {error}");
        }
    }

    #[test]
    fn connection_names_are_collected_recursively_once() {
        let pipeline = json!([
            {"$source": {"connectionName": "kafka"}},
            {"$lookup": {"from": {"connectionName": "cluster", "db": "d"}}},
            {"$merge": {"into": {"connectionName": "cluster"}}},
            {"$emit": {"connectionName": 5}}
        ]);
        assert_eq!(connection_names(&pipeline), ["kafka", "cluster"]);
    }

    #[test]
    fn secrets_are_redacted_case_insensitively() {
        let raw = json!({"name": "k", "authentication": {"username": "u", "Password": "p"}, "headers": {"X-Api-Key": "k", "Accept": "a"}, "list": [{"token": "t"}]});
        assert_eq!(
            redact(&raw),
            json!({"name": "k", "authentication": {"username": "u", "Password": "<redacted>"}, "headers": {"X-Api-Key": "<redacted>", "Accept": "a"}, "list": [{"token": "<redacted>"}]})
        );
    }

    #[test]
    fn processor_details_keep_known_states_and_numeric_stats() {
        let processor = json!({
            "name": "p", "state": "STARTED", "tier": "SP10", "effectiveTier": "SP30",
            "options": {"autoscaling": {"enabled": true, "minTier": null, "links": []}, "dlq": {"connectionName": "c", "db": "d", "coll": "k"}},
            "stats": {"inputMessageCount": 4, "dlqMessageCount": 1},
            "pipeline": [{"$source": {}}]
        });
        assert_eq!(
            processor_details(&processor, false),
            json!({"processorState": "STARTED", "tier": "SP10", "effectiveTier": "SP30",
                   "autoscaling": {"enabled": true, "minTier": null},
                   "stats": {"inputMessageCount": 4, "outputMessageCount": 0, "dlqMessageCount": 1},
                   "dlq": {"connectionName": "c", "db": "d", "coll": "k"}})
        );
        assert!(
            processor_details(&processor, true)
                .get("pipeline")
                .is_some()
        );
        assert_eq!(
            processor_details(&json!({"state": "PROVISIONING"}), false),
            json!({})
        );
    }

    #[test]
    fn streams_errors_get_the_mcp_hints_by_status_then_message() {
        let error = |status: u16, message: &str| {
            ClientError::api(ErrorCode::RequestRejected, status, None, message)
        };
        for (status, message, expected) in [
            (404, "x", "streams <group> list"),
            (403, "workspace has active processor", "stop them"),
            (400, "IDLUnknownField topic AtlasCollection", "Use $emit"),
            (
                400,
                "IDLUnknownField MergeOperatorSpec",
                "Invalid field in $merge",
            ),
            (400, "something", "invalid configuration or pipeline syntax"),
            (409, "exists", "may already exist"),
            (
                500,
                "resumeFromCheckpoint conflict",
                "--no-resume-from-checkpoint",
            ),
            (500, "SASL handshake", "Kafka credentials"),
            (500, "INVALID_STATE", "processors inspect"),
        ] {
            let hinted = with_hint(error(status, message));
            assert!(
                hinted.hint().unwrap().contains(expected),
                "{status} {message}"
            );
        }
        assert_eq!(
            with_hint(error(401, "denied").with_hint("keep")).hint(),
            Some("keep")
        );
    }

    #[test]
    fn private_links_need_a_provider() {
        assert!(
            validate_private_link(&config(json!({"provider": "AWS", "vendor": "MSK"}))).is_ok()
        );
        assert!(validate_private_link(&config(json!({"vendor": "MSK"}))).is_err());
        assert!(validate_private_link(&config(json!({"provider": "aws"}))).is_err());
    }
}
