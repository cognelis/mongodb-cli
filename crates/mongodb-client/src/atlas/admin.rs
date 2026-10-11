//! Atlas Administration API commands: the MongoDB MCP server's non-streams Atlas tools.

use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use mongodb::bson::DateTime;
use reqwest::Method;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Map, Value, json};

use super::{
    ACCEPT_TENANT_UPGRADE, AtlasClient, Outcome, Paging, compact, incompatible, rejected, results,
    seg, validation,
};
use crate::{ClientError, ConnectionUri, ErrorCode, MongoClient};

/// The comment on access list entries the CLI adds for the caller's own IP address.
pub const ACCESS_LIST_COMMENT: &str = "Added by mongodb-cli to enable tool access";
/// Instance sizes the cluster commands create or scale to directly.
pub const INSTANCE_SIZES: [&str; 7] = ["M10", "M20", "M30", "M40", "M50", "M60", "M80"];
/// Autoscaling may grow an M60 or M80 cluster past M80.
const SIZE_ORDER: [&str; 9] = [
    "M10", "M20", "M30", "M40", "M50", "M60", "M80", "M140", "M200",
];
const SHARED_TIER_METRICS: [&str; 4] = [
    "CONNECTIONS_PERCENT",
    "FLEX_CONNECTIONS_PERCENT",
    "FLEX_DATA_SIZE_TOTAL",
    "LOGICAL_SIZE",
];
const POLL_HINT: &str = "the change runs asynchronously; poll `mongodb-cli atlas clusters inspect` until the state is IDLE";

pub async fn list_orgs(client: &AtlasClient, paging: Paging) -> Result<Outcome, ClientError> {
    let response = client.get("/orgs", &paging.query()).await?;
    let organizations = results(&response)
        .iter()
        .map(|org| json!({"name": org["name"], "id": org["id"]}))
        .collect();
    Ok(counted(
        Map::new(),
        "organizations",
        organizations,
        paging,
        &response,
    ))
}

pub async fn list_projects(
    client: &AtlasClient,
    org_id: Option<&str>,
    paging: Paging,
) -> Result<Outcome, ClientError> {
    let path = match org_id {
        Some(org_id) => format!("/orgs/{}/groups", seg(org_id)),
        None => "/groups".into(),
    };
    let response = client.get(&path, &paging.query()).await?;
    let projects = results(&response)
        .iter()
        .map(|project| {
            json!({
                "name": project["name"],
                "id": project["id"],
                "orgId": project["orgId"],
                "created": project["created"].as_str().unwrap_or("N/A"),
            })
        })
        .collect();
    let mut data = Map::new();
    if let Some(org_id) = org_id {
        data.insert("orgId".into(), json!(org_id));
    }
    Ok(counted(data, "projects", projects, paging, &response))
}

pub async fn create_project(
    client: &AtlasClient,
    name: &str,
    org_id: &str,
) -> Result<Outcome, ClientError> {
    let group = client
        .post("/groups", Some(&json!({"name": name, "orgId": org_id})))
        .await?;
    let id = group["id"]
        .as_str()
        .ok_or_else(|| incompatible("Failed to create project: Atlas returned no project id"))?;
    Ok(Outcome::new(
        json!({"projectName": name, "orgId": org_id, "id": id}),
    ))
}

pub async fn list_clusters(
    client: &AtlasClient,
    project_id: Option<&str>,
) -> Result<Outcome, ClientError> {
    let Some(project_id) = project_id else {
        let response = client.get("/clusters", &[]).await?;
        let clusters: Vec<Value> = results(&response)
            .iter()
            .flat_map(|group| {
                group["clusters"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|cluster| {
                        json!({
                            "projectName": group["groupName"],
                            "projectId": group["groupId"],
                            "clusterName": cluster["name"],
                        })
                    })
            })
            .collect();
        let count = clusters.len();
        return Ok(
            Outcome::new(json!({"clusters": clusters, "totalCount": count}))
                .truncated("projects", &response),
        );
    };
    let project = client
        .get(&format!("/groups/{}", seg(project_id)), &[])
        .await?;
    let Some(id) = project["id"].as_str() else {
        return Err(ClientError::new(
            ErrorCode::NotFound,
            format!("Project with ID \"{project_id}\" not found."),
        ));
    };
    let (dedicated_path, flex_path) = (
        format!("/groups/{}/clusters", seg(id)),
        format!("/groups/{}/flexClusters", seg(id)),
    );
    let (dedicated, flex) = tokio::join!(
        client.get(&dedicated_path, &[]),
        client.get(&flex_path, &[])
    );
    let mut warnings = Vec::new();
    let mut keep = |result: Result<Value, ClientError>, kind: &str| {
        result.unwrap_or_else(|error| {
            warnings.push(format!("{kind} clusters could not be listed: {error}"));
            Value::Null
        })
    };
    let (dedicated, flex) = match (dedicated, flex) {
        (Err(error), Err(_)) => return Err(error),
        (dedicated, flex) => (keep(dedicated, "dedicated"), keep(flex, "flex")),
    };
    let clusters: Vec<Value> = results(&dedicated)
        .iter()
        .map(format_cluster)
        .chain(results(&flex).iter().map(format_flex_cluster))
        .collect();
    let count = clusters.len();
    let mut outcome = Outcome::new(json!({
        "projectId": id,
        "projectName": project["name"],
        "clusters": clusters,
        "totalCount": count,
    }))
    .truncated("dedicated clusters", &dedicated)
    .truncated("flex clusters", &flex);
    outcome.warnings.splice(0..0, warnings);
    Ok(outcome)
}

pub async fn inspect_cluster(
    client: &AtlasClient,
    project_id: &str,
    cluster: &str,
) -> Result<Outcome, ClientError> {
    let found = inspect(client, project_id, cluster).await?;
    let text = |key: &str, fallback: &'static str| {
        found[key]
            .as_str()
            .filter(|value| !value.is_empty())
            .map_or_else(|| json!(fallback), |value| json!(value))
    };
    Ok(Outcome::new(compact(json!({
        "name": text("name", "Unknown"),
        "instanceType": found["instanceType"],
        "instanceSize": text("instanceSize", "N/A"),
        "provider": found["provider"],
        "region": found["region"],
        "paused": found["paused"].as_bool().unwrap_or(false),
        "state": text("state", "UNKNOWN"),
        "mongoDBVersion": text("mongoDBVersion", "N/A"),
        "connectionStrings": if found["connectionStrings"].is_object() {
            found["connectionStrings"].clone()
        } else {
            json!({})
        },
    }))))
}

/// The inputs of `atlas clusters create`, with Atlas spellings (`AWS`, `M10`, `REPLICASET`).
#[derive(Clone, Debug, PartialEq)]
pub struct ClusterSpec {
    pub name: String,
    pub provider: String,
    pub regions: Vec<String>,
    pub cluster_type: String,
    pub instance_size: Option<String>,
    pub compute_autoscaling: bool,
    pub disk_size_gb: Option<f64>,
    pub mongodb_version: String,
    pub backup: String,
    pub termination_protection: bool,
    pub encryption_at_rest_provider: Option<String>,
}

impl ClusterSpec {
    /// The checks that need no Atlas call.
    pub fn validate(&self) -> Result<(), ClientError> {
        if !(1..=3).contains(&self.regions.len()) {
            return Err(validation("a cluster needs between one and three regions"));
        }
        if self
            .disk_size_gb
            .is_some_and(|size| !size.is_finite() || size <= 0.0)
        {
            return Err(validation("--disk-size-gb must be a positive number"));
        }
        if self.cluster_type == "SHARDED"
            && matches!(self.instance_size.as_deref(), Some("M10" | "M20"))
        {
            return Err(validation(
                "SHARDED clusters require M30 or higher instance size.",
            ));
        }
        Ok(())
    }
}

pub async fn create_cluster(
    client: &AtlasClient,
    project_id: &str,
    spec: &ClusterSpec,
) -> Result<Outcome, ClientError> {
    let base = format!("/groups/{}", seg(project_id));
    let instance_size = match (&spec.instance_size, spec.cluster_type.as_str()) {
        (Some(size), _) => size.clone(),
        (None, "SHARDED") => "M30".into(),
        (None, _) => {
            let existing = client.get(&format!("{base}/clusters"), &[]).await?;
            if results(&existing).len() < 2 {
                "M10"
            } else {
                "M30"
            }
            .into()
        }
    };
    let encryption = match &spec.encryption_at_rest_provider {
        Some(provider) => provider.clone(),
        None => match client.get(&format!("{base}/encryptionAtRest"), &[]).await {
            Ok(configuration) => {
                let key = match spec.provider.as_str() {
                    "AWS" => "awsKms",
                    "AZURE" => "azureKeyVault",
                    _ => "googleCloudKms",
                };
                let valid =
                    configuration[key]["enabled"] == true && configuration[key]["valid"] == true;
                if valid {
                    spec.provider.clone()
                } else {
                    "NONE".into()
                }
            }
            // Without permission to read the configuration, assume none exists.
            Err(error) if error.server_code() == Some(403) => "NONE".into(),
            Err(error) => return Err(error),
        },
    };
    let body = cluster_body(spec, &instance_size, &encryption);
    let access_list = ensure_current_ip(client, project_id).await;
    let created = client
        .post(&format!("{base}/clusters"), Some(&body))
        .await?;
    let data = compact(json!({
        "clusterId": created["id"],
        "provider": spec.provider,
        "regions": spec.regions,
        "instanceSize": instance_size,
        "clusterType": spec.cluster_type,
        "mongoDBVersion": spec.mongodb_version,
        "backup": spec.backup,
        "computeAutoScaling": spec.compute_autoscaling,
        "terminationProtectionEnabled": spec.termination_protection,
        "diskSizeGB": spec.disk_size_gb,
        "encryptionAtRestProvider": encryption,
        "accessList": access_list,
    }));
    Ok(with_access_list(Outcome::new(data), access_list).warn(POLL_HINT))
}

/// The cluster creation body, matching the MCP server's node layout and defaults.
fn cluster_body(spec: &ClusterSpec, instance_size: &str, encryption: &str) -> Value {
    let compute = if spec.compute_autoscaling {
        json!({
            "enabled": true,
            "scaleDownEnabled": true,
            "minInstanceSize": instance_size,
            "maxInstanceSize": max_autoscaling_size(instance_size, &spec.provider),
        })
    } else {
        json!({"enabled": false, "scaleDownEnabled": false})
    };
    let auto_scaling = json!({"compute": compute, "diskGB": {"enabled": true}});
    let nodes: &[u32] = match spec.regions.len() {
        1 => &[3],
        2 => &[2, 1],
        _ => &[2, 2, 1],
    };
    let region_configs: Vec<Value> = spec
        .regions
        .iter()
        .enumerate()
        .map(|(index, region)| {
            let mut electable = json!({
                "instanceSize": instance_size,
                "nodeCount": nodes.get(index).copied().unwrap_or(3),
            });
            if let Some(size) = spec.disk_size_gb {
                electable["diskSizeGB"] = json!(size);
            }
            json!({
                "providerName": spec.provider,
                "regionName": region,
                "priority": 7 - index as i64,
                "electableSpecs": electable,
                "autoScaling": auto_scaling,
            })
        })
        .collect();
    let mut body = json!({
        "name": spec.name,
        "clusterType": spec.cluster_type,
        "replicationSpecs": [{"regionConfigs": region_configs}],
        "terminationProtectionEnabled": spec.termination_protection,
        "backupEnabled": spec.backup != "OFF",
        "pitEnabled": spec.backup == "CONTINUOUS",
    });
    if spec.mongodb_version == "LATEST" {
        body["versionReleaseSystem"] = json!("CONTINUOUS");
    } else {
        body["versionReleaseSystem"] = json!("LTS");
        body["mongoDBMajorVersion"] = json!(spec.mongodb_version);
    }
    body["encryptionAtRestProvider"] = json!(encryption);
    body
}

/// The default autoscaling ceiling: two sizes up, with the MCP server's M60/M80 exceptions.
pub fn max_autoscaling_size(size: &str, provider: &str) -> &'static str {
    match size {
        "M80" => "M200",
        "M60" if provider == "AZURE" => "M200",
        "M60" => "M140",
        _ => INSTANCE_SIZES
            .iter()
            .position(|candidate| *candidate == size)
            .and_then(|index| INSTANCE_SIZES.get(index + 2))
            .copied()
            .unwrap_or("M80"),
    }
}

pub async fn create_free_cluster(
    client: &AtlasClient,
    project_id: &str,
    name: &str,
    region: &str,
) -> Result<Outcome, ClientError> {
    let body = json!({
        "groupId": project_id,
        "name": name,
        "clusterType": "REPLICASET",
        "replicationSpecs": [{
            "zoneName": "Zone 1",
            "regionConfigs": [{
                "providerName": "TENANT",
                "backingProviderName": "AWS",
                "regionName": region,
                "electableSpecs": {"instanceSize": "M0"},
            }],
        }],
        "terminationProtectionEnabled": false,
    });
    let access_list = ensure_current_ip(client, project_id).await;
    client
        .post(
            &format!("/groups/{}/clusters", seg(project_id)),
            Some(&body),
        )
        .await?;
    let outcome = Outcome::new(json!({"created": true, "accessList": access_list}));
    Ok(with_access_list(outcome, access_list).warn(POLL_HINT))
}

pub async fn pause_resume(
    client: &AtlasClient,
    project_id: &str,
    cluster: &str,
    pause: bool,
) -> Result<Outcome, ClientError> {
    let updated = client
        .patch(
            &format!("/groups/{}/clusters/{}", seg(project_id), seg(cluster)),
            &json!({"paused": pause}),
        )
        .await?;
    let outcome = Outcome::new(compact(json!({
        "clusterName": cluster,
        "action": if pause { "PAUSE" } else { "RESUME" },
        "clusterId": updated["id"],
    })));
    Ok(if pause {
        outcome
    } else {
        outcome.warn(POLL_HINT)
    })
}

/// The inputs of `atlas clusters upgrade`, with Atlas spellings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UpgradeRequest {
    pub target_tier: Option<String>,
    pub compute_autoscaling: Option<bool>,
    pub min_instance_size: Option<String>,
    pub max_instance_size: Option<String>,
    pub provider: Option<String>,
    pub region: Option<String>,
}

impl UpgradeRequest {
    fn autoscaling_given(&self) -> bool {
        self.compute_autoscaling.is_some()
            || self.min_instance_size.is_some()
            || self.max_instance_size.is_some()
    }
}

pub async fn upgrade_cluster(
    client: &AtlasClient,
    project_id: &str,
    cluster: &str,
    request: &UpgradeRequest,
) -> Result<Outcome, ClientError> {
    let base = format!("/groups/{}", seg(project_id));
    let cluster_path = format!("{base}/clusters/{}", seg(cluster));
    let (plan, result) = match client.get(&cluster_path, &[]).await {
        Ok(raw) => {
            let plan = plan_upgrade(cluster, &raw, request)?;
            let result = if plan.kind == "FREE" {
                // Free clusters upgrade through an endpoint of the 2023-01-01 API version.
                client
                    .send(
                        Method::POST,
                        &format!("{base}/clusters/tenantUpgrade"),
                        &[],
                        Some(&plan.body),
                        ACCEPT_TENANT_UPGRADE,
                    )
                    .await?
            } else {
                client.patch(&cluster_path, &plan.body).await?
            };
            (plan, result)
        }
        // Atlas answers 400 for Flex clusters on the cluster endpoint and 404 for unknown names.
        Err(error) if matches!(error.server_code(), Some(400 | 404)) => {
            let raw = client
                .get(&format!("{base}/flexClusters/{}", seg(cluster)), &[])
                .await?;
            let plan = plan_flex_upgrade(cluster, &raw, request)?;
            let result = client
                .post(
                    &format!("{base}/flexClusters:tenantUpgrade"),
                    Some(&plan.body),
                )
                .await?;
            (plan, result)
        }
        Err(error) => return Err(error),
    };
    let mut data = plan.data;
    if let Some(id) = result["id"].as_str() {
        data["clusterId"] = json!(id);
    }
    Ok(Outcome::new(data).warn(POLL_HINT))
}

/// What an upgrade sends and reports, decided before any write.
#[derive(Debug, PartialEq)]
pub struct UpgradePlan {
    /// `DEDICATED`, `FREE`, or `FLEX`: the source cluster's type.
    pub kind: &'static str,
    pub body: Value,
    pub data: Value,
}

/// Plan an upgrade of a cluster returned by the cluster endpoint (Free or Dedicated).
pub fn plan_upgrade(
    cluster: &str,
    raw: &Value,
    request: &UpgradeRequest,
) -> Result<UpgradePlan, ClientError> {
    let formatted = format_cluster(raw);
    let provider = request
        .provider
        .clone()
        .or_else(|| formatted["provider"].as_str().map(str::to_owned));
    let region = request
        .region
        .clone()
        .or_else(|| formatted["region"].as_str().map(str::to_owned));
    if formatted["instanceType"] == "FREE" {
        return plan_shared_upgrade(cluster, "FREE", provider, region, request);
    }
    let current = formatted["instanceSize"].as_str();
    plan_dedicated_upgrade(cluster, raw, current, provider, region, request)
}

/// Plan an upgrade of a cluster returned by the Flex cluster endpoint.
pub fn plan_flex_upgrade(
    cluster: &str,
    raw: &Value,
    request: &UpgradeRequest,
) -> Result<UpgradePlan, ClientError> {
    let settings = &raw["providerSettings"];
    let provider = request
        .provider
        .clone()
        .or_else(|| settings["backingProviderName"].as_str().map(str::to_owned));
    let region = request
        .region
        .clone()
        .or_else(|| settings["regionName"].as_str().map(str::to_owned));
    plan_shared_upgrade(cluster, "FLEX", provider, region, request)
}

fn plan_dedicated_upgrade(
    cluster: &str,
    raw: &Value,
    current: Option<&str>,
    provider: Option<String>,
    region: Option<String>,
    request: &UpgradeRequest,
) -> Result<UpgradePlan, ClientError> {
    if request.target_tier.as_deref() == Some("FLEX") {
        return Err(validation(format!(
            "Cluster \"{cluster}\" is already Dedicated. targetTier must be an instance size (M10-M80) to scale it in place, not FLEX."
        )));
    }
    if request.provider.is_some() || request.region.is_some() {
        return Err(validation(format!(
            "Invalid Arguments:provider/region. provider/region are not valid when scaling an already-Dedicated cluster \"{cluster}\"."
        )));
    }
    if request.target_tier.is_none() && !request.autoscaling_given() {
        return Err(validation(format!(
            "No changes specified for Dedicated cluster \"{cluster}\". Provide targetTier (new instance size), computeAutoScaling, minInstanceSize, and/or maxInstanceSize to scale it."
        )));
    }
    let Some(current) = current.filter(|size| INSTANCE_SIZES.contains(size)) else {
        return Err(validation(format!(
            "Cluster \"{cluster}\" has instance size \"{}\", which this tool does not support scaling. Only standard M10-M80 instance sizes are supported.",
            current.unwrap_or("unknown")
        )));
    };
    let cluster_type = raw["clusterType"].as_str();
    let specs = raw["replicationSpecs"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if !matches!(cluster_type, Some("REPLICASET" | "SHARDED")) || specs.len() > 1 {
        return Err(validation(format!(
            "Cluster \"{cluster}\" is a {} cluster, which this tool does not support scaling. Only single-shard clusters (REPLICASET, or SHARDED with one shard) are supported.",
            cluster_type.unwrap_or("unrecognized")
        )));
    }
    let region_configs = raw["replicationSpecs"][0]["regionConfigs"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let distinct = |specs: &str| {
        let mut sizes: Vec<&str> = region_configs
            .iter()
            .filter_map(|config| config[specs]["instanceSize"].as_str())
            .collect();
        sizes.sort_unstable();
        sizes.dedup();
        sizes.len()
    };
    if distinct("electableSpecs") > 1 || distinct("readOnlySpecs") > 1 {
        return Err(validation(format!(
            "Cluster \"{cluster}\" has regions with different instance sizes, which this tool does not support scaling to avoid unintended changes."
        )));
    }
    let existing = region_configs
        .first()
        .map(|config| &config["autoScaling"]["compute"])
        .unwrap_or(&Value::Null);
    let size = request.target_tier.as_deref().unwrap_or(current);
    let scaling = size != current;
    let enabled = request
        .compute_autoscaling
        .or_else(|| existing["enabled"].as_bool())
        .unwrap_or(false);
    let default_max = max_autoscaling_size(size, provider.as_deref().unwrap_or_default());
    let minimum = match &request.min_instance_size {
        Some(minimum) => Some(minimum.as_str()),
        None if !enabled => None,
        None if scaling => Some(size),
        None => Some(existing["minInstanceSize"].as_str().unwrap_or(size)),
    };
    let maximum = match &request.max_instance_size {
        Some(maximum) => Some(maximum.as_str()),
        None if !enabled => None,
        None if scaling => Some(match existing["maxInstanceSize"].as_str() {
            Some(existing) if rank(existing) > rank(default_max) => existing,
            _ => default_max,
        }),
        None => Some(existing["maxInstanceSize"].as_str().unwrap_or(default_max)),
    };
    if enabled
        && let (Some(minimum), Some(maximum)) = (minimum, maximum)
        && rank(minimum) > rank(maximum)
    {
        return Err(validation(format!(
            "minInstanceSize \"{minimum}\" cannot be higher than maxInstanceSize \"{maximum}\"."
        )));
    }
    let compute = compact(json!({
        "enabled": enabled,
        "scaleDownEnabled": enabled,
        "minInstanceSize": minimum,
        "maxInstanceSize": maximum,
    }));
    let replication_specs: Vec<Value> = specs
        .iter()
        .map(|spec| {
            let mut spec = spec.clone();
            if let Some(configs) = spec.get_mut("regionConfigs").and_then(Value::as_array_mut) {
                for config in configs {
                    let electable = config["electableSpecs"].is_object();
                    let read_only = config["readOnlySpecs"].is_object();
                    if !electable && !read_only {
                        continue;
                    }
                    if electable {
                        config["electableSpecs"]["instanceSize"] = json!(size);
                    }
                    if read_only {
                        config["readOnlySpecs"]["instanceSize"] = json!(size);
                    }
                    let disk = config
                        .get("autoScaling")
                        .and_then(|auto_scaling| auto_scaling.get("diskGB"))
                        .cloned();
                    let mut auto_scaling = json!({"compute": compute});
                    if let Some(disk) = disk {
                        auto_scaling["diskGB"] = disk;
                    }
                    config["autoScaling"] = auto_scaling;
                }
            }
            spec
        })
        .collect();
    Ok(UpgradePlan {
        kind: "DEDICATED",
        body: json!({"replicationSpecs": replication_specs}),
        data: compact(json!({
            "originalTier": current,
            "targetTier": size,
            "computeAutoScaling": enabled,
            "minInstanceSize": minimum,
            "maxInstanceSize": maximum,
            "resolvedProvider": provider,
            "resolvedRegion": region,
        })),
    })
}

/// Free clusters move to Flex or M10; Flex clusters move to M10.
fn plan_shared_upgrade(
    cluster: &str,
    kind: &'static str,
    provider: Option<String>,
    region: Option<String>,
    request: &UpgradeRequest,
) -> Result<UpgradePlan, ClientError> {
    if let Some(target) = request
        .target_tier
        .as_deref()
        .filter(|target| !matches!(*target, "FLEX" | "M10"))
    {
        return Err(validation(format!(
            "targetTier \"{target}\" is not valid when upgrading from {kind}. Choose FLEX or M10 - larger instance sizes are only valid once the cluster is already Dedicated."
        )));
    }
    let target =
        request
            .target_tier
            .as_deref()
            .unwrap_or(if kind == "FREE" { "FLEX" } else { "M10" });
    if target == "FLEX" {
        if kind == "FLEX" {
            return Err(validation(format!(
                "Cluster \"{cluster}\" is already a Flex cluster."
            )));
        }
        if request.autoscaling_given() {
            return Err(validation(
                "Invalid Arguments:computeAutoScaling/minInstanceSize/maxInstanceSize. Flex clusters do not support compute autoscaling.",
            ));
        }
        return Ok(UpgradePlan {
            kind,
            body: json!({
                "name": cluster,
                "providerSettings": compact(json!({
                    "providerName": "FLEX",
                    "instanceSizeName": "FLEX",
                    "backingProviderName": provider,
                    "regionName": region,
                })),
            }),
            data: compact(json!({
                "originalTier": kind,
                "targetTier": target,
                "resolvedProvider": provider,
                "resolvedRegion": region,
            })),
        });
    }
    if request
        .min_instance_size
        .as_deref()
        .is_some_and(|minimum| minimum != "M10")
    {
        return Err(validation(
            "minInstanceSize must be omitted or \"M10\" when upgrading to M10 Dedicated.",
        ));
    }
    let enabled = request.compute_autoscaling.unwrap_or(true);
    let (minimum, maximum) = if enabled {
        (
            Some(request.min_instance_size.as_deref().unwrap_or("M10")),
            Some(request.max_instance_size.as_deref().unwrap_or("M30")),
        )
    } else {
        (
            request.min_instance_size.as_deref(),
            request.max_instance_size.as_deref(),
        )
    };
    let body = if kind == "FREE" {
        let mut settings = compact(json!({
            "providerName": provider,
            "instanceSizeName": "M10",
            "regionName": region,
        }));
        if enabled {
            settings["autoScaling"] = json!({"compute": compact(json!({
                "minInstanceSize": minimum,
                "maxInstanceSize": maximum,
            }))});
        }
        json!({
            "name": cluster,
            "providerSettings": settings,
            "autoScaling": {
                "compute": {"enabled": enabled, "scaleDownEnabled": enabled},
                "diskGBEnabled": true,
            },
        })
    } else {
        let mut region_config = compact(json!({"providerName": provider, "regionName": region}));
        region_config["priority"] = json!(7);
        region_config["electableSpecs"] = json!({"instanceSize": "M10", "nodeCount": 3});
        region_config["autoScaling"] = json!({
            "compute": compact(json!({
                "enabled": enabled,
                "scaleDownEnabled": enabled,
                "minInstanceSize": minimum,
                "maxInstanceSize": maximum,
            })),
            "diskGB": {"enabled": true},
        });
        json!({
            "name": cluster,
            "clusterType": "REPLICASET",
            "replicationSpecs": [{"regionConfigs": [region_config]}],
        })
    };
    Ok(UpgradePlan {
        kind,
        body,
        data: compact(json!({
            "originalTier": kind,
            "targetTier": target,
            "computeAutoScaling": enabled,
            "minInstanceSize": minimum,
            "maxInstanceSize": maximum,
            "resolvedProvider": provider,
            "resolvedRegion": region,
        })),
    })
}

/// The position of an instance size in the scaling order; unknown sizes rank lowest.
fn rank(size: &str) -> i32 {
    SIZE_ORDER
        .iter()
        .position(|candidate| *candidate == size)
        .map_or(-1, |index| index as i32)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdvisorOperation {
    SuggestedIndexes,
    DropIndexSuggestions,
    SlowQueryLogs,
    SchemaSuggestions,
}

/// Milliseconds since the epoch for an RFC 3339 timestamp such as `2026-10-01T00:00:00Z`.
pub fn epoch_millis(timestamp: &str) -> Result<i64, ClientError> {
    DateTime::parse_rfc3339_str(timestamp)
        .map(DateTime::timestamp_millis)
        .map_err(|_| validation(format!("'{timestamp}' is not an RFC 3339 timestamp")))
}

pub async fn performance_advisor(
    client: &AtlasClient,
    project_id: &str,
    cluster: &str,
    operations: &[AdvisorOperation],
    since_ms: Option<i64>,
    namespaces: &[String],
) -> Result<Outcome, ClientError> {
    use AdvisorOperation::*;
    let base = format!(
        "/groups/{}/clusters/{}/performanceAdvisor",
        seg(project_id),
        seg(cluster)
    );
    let (suggested_path, drop_path, schema_path) = (
        format!("{base}/suggestedIndexes"),
        format!("{base}/dropIndexSuggestions"),
        format!("{base}/schemaAdvice"),
    );
    let wants = |operation| operations.contains(&operation);
    // Each operation fails independently, as in the MCP server.
    let (suggested, drops, slow, schema) = tokio::join!(
        optional(wants(SuggestedIndexes), client.get(&suggested_path, &[])),
        optional(wants(DropIndexSuggestions), client.get(&drop_path, &[])),
        optional(
            wants(SlowQueryLogs),
            slow_queries(client, project_id, cluster, since_ms, namespaces)
        ),
        optional(wants(SchemaSuggestions), client.get(&schema_path, &[])),
    );
    let mut warnings = Vec::new();
    let mut take = |name: &str, result: Option<Result<Value, ClientError>>| {
        result.map(|result| {
            result.unwrap_or_else(|error| {
                warnings.push(format!("{name} unavailable: {error}"));
                Value::Null
            })
        })
    };
    let list = |value: &Value| Value::Array(value.as_array().cloned().unwrap_or_default());
    let mut data = json!({"projectId": project_id, "clusterName": cluster});
    if let Some(response) = take("suggestedIndexes", suggested) {
        data["suggestedIndexes"] = list(&response["content"]["suggestedIndexes"]);
    }
    if let Some(response) = take("dropIndexSuggestions", drops) {
        let content = &response["content"];
        data["dropIndexSuggestions"] = json!({
            "hiddenIndexes": list(&content["hiddenIndexes"]),
            "redundantIndexes": list(&content["redundantIndexes"]),
            "unusedIndexes": list(&content["unusedIndexes"]),
        });
    }
    if let Some(logs) = take("slowQueryLogs", slow) {
        data["slowQueryLogs"] = list(&logs);
    }
    if let Some(response) = take("schemaSuggestions", schema) {
        data["schemaSuggestions"] = list(&response["content"]["recommendations"]);
    }
    if data["suggestedIndexes"]
        .as_array()
        .is_some_and(|items| !items.is_empty())
    {
        warnings
            .push("suggested index weight is the estimated disk bytes saved per read query".into());
    }
    if data["slowQueryLogs"]
        .as_array()
        .is_some_and(|items| !items.is_empty())
    {
        warnings.push("slow query logs are limited to the 50 most recent per host".into());
    }
    Ok(Outcome {
        warnings,
        ..Outcome::new(data)
    })
}

async fn optional(
    wanted: bool,
    call: impl Future<Output = Result<Value, ClientError>>,
) -> Option<Result<Value, ClientError>> {
    if wanted { Some(call.await) } else { None }
}

/// Slow queries of every cluster host; a host that fails is skipped.
async fn slow_queries(
    client: &AtlasClient,
    project_id: &str,
    cluster: &str,
    since_ms: Option<i64>,
    namespaces: &[String],
) -> Result<Value, ClientError> {
    let processes = inspect(client, project_id, cluster).await?["processIds"].clone();
    let mut query: Vec<(&str, String)> = Vec::new();
    if let Some(since) = since_ms {
        query.push(("since", since.to_string()));
    }
    query.extend(
        namespaces
            .iter()
            .map(|namespace| ("namespaces", namespace.clone())),
    );
    query.push(("nLogs", "50".into()));
    let mut logs = Vec::new();
    for process in processes
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let path = format!(
            "/groups/{}/processes/{}/performanceAdvisor/slowQueryLogs",
            seg(project_id),
            seg(process)
        );
        if let Ok(response) = client.get(&path, &query).await {
            logs.extend(
                response["slowQueries"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
            );
        }
    }
    Ok(Value::Array(logs))
}

pub async fn list_access_list(
    client: &AtlasClient,
    project_id: &str,
) -> Result<Outcome, ClientError> {
    let response = client
        .get(&format!("/groups/{}/accessList", seg(project_id)), &[])
        .await?;
    let entries: Vec<Value> = results(&response)
        .iter()
        .map(|entry| {
            compact(json!({
                "ipAddress": entry["ipAddress"],
                "cidrBlock": entry["cidrBlock"],
                "comment": entry["comment"],
            }))
        })
        .collect();
    let count = entries.len();
    Ok(
        Outcome::new(json!({"projectId": project_id, "entries": entries, "totalCount": count}))
            .truncated("entries", &response),
    )
}

/// Add entries in the MCP server's order: addresses, the current address, then CIDR blocks.
pub async fn add_access_list(
    client: &AtlasClient,
    project_id: &str,
    addresses: &[String],
    cidr_blocks: &[String],
    current_ip: bool,
    comment: Option<&str>,
) -> Result<Outcome, ClientError> {
    let comment = comment.unwrap_or(ACCESS_LIST_COMMENT);
    let mut entries: Vec<Value> = addresses
        .iter()
        .map(|address| json!({"groupId": project_id, "ipAddress": address, "comment": comment}))
        .collect();
    if current_ip {
        let address = client.current_ip().await?;
        entries.push(json!({"groupId": project_id, "ipAddress": address, "comment": comment}));
    }
    entries.extend(
        cidr_blocks
            .iter()
            .map(|block| json!({"groupId": project_id, "cidrBlock": block, "comment": comment})),
    );
    client
        .post(
            &format!("/groups/{}/accessList", seg(project_id)),
            Some(&Value::Array(entries)),
        )
        .await?;
    Ok(Outcome::new(json!({"projectId": project_id})))
}

pub async fn list_db_users(client: &AtlasClient, project_id: &str) -> Result<Outcome, ClientError> {
    let response = client
        .get(&format!("/groups/{}/databaseUsers", seg(project_id)), &[])
        .await?;
    let users: Vec<Value> = results(&response)
        .iter()
        .map(|user| {
            let roles: Vec<Value> = user["roles"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|role| {
                    compact(json!({
                        "roleName": role["roleName"],
                        "databaseName": role["databaseName"],
                        "collectionName": role["collectionName"],
                    }))
                })
                .collect();
            let scopes: Vec<Value> = user["scopes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|scope| json!({"type": scope["type"], "name": scope["name"]}))
                .collect();
            json!({"username": user["username"], "roles": roles, "scopes": scopes})
        })
        .collect();
    let count = users.len();
    Ok(
        Outcome::new(json!({"projectId": project_id, "users": users, "totalCount": count}))
            .truncated("users", &response),
    )
}

/// A database user role: `roleName` on a database and optionally one collection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DbRole {
    pub role_name: String,
    pub database_name: String,
    pub collection_name: Option<String>,
}

impl DbRole {
    /// Parse `ROLE[@DB[.COLLECTION]]`; the database defaults to `admin`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (role, scope) = text.split_once('@').unwrap_or((text, "admin"));
        let (database, collection) = match scope.split_once('.') {
            Some((database, collection)) => (database, Some(collection)),
            None => (scope, None),
        };
        if role.is_empty()
            || !role.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
        {
            return Err("role names may contain only letters, numbers, '-' and '_'".into());
        }
        let printable =
            |value: &str| !value.is_empty() && value.chars().all(|c| (' '..='~').contains(&c));
        if !printable(database) || collection.is_some_and(|collection| !printable(collection)) {
            return Err("database and collection names must be non-empty printable ASCII".into());
        }
        Ok(Self {
            role_name: role.into(),
            database_name: database.into(),
            collection_name: collection.map(str::to_owned),
        })
    }

    fn to_json(&self) -> Value {
        compact(json!({
            "roleName": self.role_name,
            "databaseName": self.database_name,
            "collectionName": self.collection_name,
        }))
    }
}

pub async fn create_db_user(
    client: &AtlasClient,
    project_id: &str,
    username: &str,
    password: &SecretString,
    roles: &[DbRole],
    clusters: &[String],
) -> Result<Outcome, ClientError> {
    let access_list = ensure_current_ip(client, project_id).await;
    let mut body = json!({
        "groupId": project_id,
        "awsIAMType": "NONE",
        "databaseName": "admin",
        "ldapAuthType": "NONE",
        "oidcAuthType": "NONE",
        "x509Type": "NONE",
        "username": username,
        "password": password.expose_secret(),
        "roles": roles.iter().map(DbRole::to_json).collect::<Vec<_>>(),
    });
    // Without scopes the user can reach every cluster in the project.
    if !clusters.is_empty() {
        body["scopes"] = clusters
            .iter()
            .map(|cluster| json!({"type": "CLUSTER", "name": cluster}))
            .collect();
    }
    client
        .post(
            &format!("/groups/{}/databaseUsers", seg(project_id)),
            Some(&body),
        )
        .await?;
    let outcome = Outcome::new(json!({"username": username, "accessList": access_list}));
    Ok(with_access_list(outcome, access_list))
}

pub async fn list_alerts(
    client: &AtlasClient,
    project_id: &str,
    status: &str,
    paging: Paging,
) -> Result<Outcome, ClientError> {
    let mut query = vec![("status", status.to_owned())];
    query.extend(paging.query());
    let response = client
        .get(&format!("/groups/{}/alerts", seg(project_id)), &query)
        .await?;
    let alerts: Vec<Value> = results(&response)
        .iter()
        .map(|alert| {
            json!({
                "id": alert["id"],
                "status": alert["status"],
                "created": alert["created"].as_str().unwrap_or("N/A"),
                "updated": alert["updated"].as_str().unwrap_or("N/A"),
                "eventTypeName": alert["eventTypeName"],
                "acknowledgementComment": alert["acknowledgementComment"].as_str().unwrap_or("N/A"),
            })
        })
        .collect();
    let returned = alerts.len();
    let total = response["totalCount"].as_u64();
    let mut data = json!({"projectId": project_id, "status": status, "alerts": alerts});
    // An empty page reports Atlas's count as-is; otherwise only a positive count is trusted.
    if let Some(total) = total.filter(|total| returned == 0 || *total > 0) {
        data["totalCount"] = json!(total);
    }
    Ok(Outcome::paged(data, paging, returned, total))
}

pub async fn load_sample_data(
    client: &AtlasClient,
    project_id: &str,
    cluster: &str,
) -> Result<Outcome, ClientError> {
    let path = format!(
        "/groups/{}/sampleDatasetLoad/{}",
        seg(project_id),
        seg(cluster)
    );
    let status = client.post(&path, None).await?;
    Ok(Outcome::new(sample_status(&status))
        .warn("loads take one to five minutes; poll with `mongodb-cli atlas sample-data status`"))
}

pub async fn sample_data_status(
    client: &AtlasClient,
    project_id: &str,
    job_id: &str,
) -> Result<Outcome, ClientError> {
    let path = format!(
        "/groups/{}/sampleDatasetLoad/{}",
        seg(project_id),
        seg(job_id)
    );
    Ok(Outcome::new(sample_status(&client.get(&path, &[]).await?)))
}

fn sample_status(status: &Value) -> Value {
    compact(json!({
        "jobId": status["_id"],
        "clusterName": status["clusterName"],
        "state": status["state"].as_str().filter(|state| !state.is_empty()).unwrap_or("WORKING"),
        "createDate": status["createDate"],
        "completeDate": status["completeDate"],
        "errorMessage": status["errorMessage"],
    }))
}

/// What `atlas clusters connect` asks for.
#[derive(Clone, Debug)]
pub struct ConnectRequest {
    pub project_id: String,
    pub cluster: String,
    /// The profile that will hold the connection string; named in the user's description.
    pub profile: String,
    pub read_write: bool,
    /// `standard`, `private`, or `privateEndpoint`.
    pub connection_type: String,
    pub lifetime_hours: u32,
}

/// The temporary database user behind a saved connection string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemporaryUser {
    pub project_id: String,
    pub cluster_name: String,
    pub username: String,
    pub delete_after: String,
}

/// A verified connection to a cluster through a new temporary database user.
pub struct Connected {
    /// The connection string with the temporary credentials; never displayed.
    pub uri: SecretString,
    pub user: TemporaryUser,
    pub data: Value,
    pub warnings: Vec<String>,
}

/// Create a temporary user, verify that it can connect before `deadline`, and delete it again
/// when verification fails, because its password exists nowhere else.
pub async fn connect_cluster(
    client: &AtlasClient,
    request: &ConnectRequest,
    deadline: Instant,
) -> Result<Connected, ClientError> {
    let pid = request.project_id.as_str();
    let access_list = ensure_current_ip(client, pid).await;
    let cluster = inspect(client, pid, &request.cluster).await?;
    if cluster["paused"] == true {
        return Err(
            rejected(format!("cluster \"{}\" is paused", request.cluster)).with_hint(
                "resume it with `mongodb-cli atlas clusters resume`, then connect again",
            ),
        );
    }
    let Some(cluster_id) = cluster["clusterId"].as_str() else {
        return Err(incompatible(format!(
            "Atlas did not return an id for cluster \"{}\" in project \"{pid}\"",
            request.cluster
        )));
    };
    let strings = &cluster["connectionStrings"];
    if !strings.is_object() {
        return Err(rejected("Connection strings not available")
            .with_hint("wait until `mongodb-cli atlas clusters inspect` reports the state IDLE"));
    }
    let Some(target) = connection_string(strings, &request.connection_type) else {
        return Err(rejected(format!(
            "Connection string for connection type \"{}\" is not available. Please ensure this connection type is set up in Atlas. See https://www.mongodb.com/docs/atlas/connect-to-database-deployment/#connect-to-an-atlas-cluster.",
            request.connection_type
        )));
    };
    let username = format!("mongodbCli{}", random_hex(4));
    let password = generate_password();
    let delete_after = delete_after(request.lifetime_hours)?;
    let role = if request.read_write {
        "readWriteAnyDatabase"
    } else {
        "readAnyDatabase"
    };
    let body = json!({
        "databaseName": "admin",
        "groupId": pid,
        "roles": [{"roleName": role, "databaseName": "admin"}],
        "scopes": [{"type": "CLUSTER", "name": request.cluster}],
        "username": username,
        "password": password.expose_secret(),
        "awsIAMType": "NONE",
        "ldapAuthType": "NONE",
        "oidcAuthType": "NONE",
        "x509Type": "NONE",
        "deleteAfterDate": delete_after,
        "description": format!("mongodb-cli temporary user for profile {}", request.profile),
    });
    client
        .post(&format!("/groups/{}/databaseUsers", seg(pid)), Some(&body))
        .await?;
    let user = TemporaryUser {
        project_id: pid.into(),
        cluster_name: request.cluster.clone(),
        username: username.clone(),
        delete_after: delete_after.clone(),
    };
    let verified = match with_credentials(target, &username, password.expose_secret()) {
        Ok(raw) => match ConnectionUri::parse(&raw) {
            Ok(uri) => verify(&uri, deadline)
                .await
                .map(|()| (SecretString::from(raw), uri.display().to_owned())),
            Err(error) => Err(validation(error)),
        },
        Err(error) => Err(error),
    };
    let (uri, display) = match verified {
        Ok(verified) => verified,
        Err(error) => {
            let _ = delete_temporary_user(client, &user).await;
            let message = error
                .to_string()
                .replace(password.expose_secret(), "<redacted>");
            let code = match error.code() {
                ErrorCode::Unauthorized => ErrorCode::Unauthorized,
                ErrorCode::Validation => ErrorCode::IncompatibleResponse,
                _ => ErrorCode::Timeout,
            };
            return Err(ClientError::new(
                code,
                format!("the temporary database user could not connect to cluster \"{}\": {message}", request.cluster),
            )
            .with_hint("the temporary user was deleted; new Atlas users can take minutes to become usable, so retry with a longer --timeout such as --timeout 300"));
        }
    };
    let instance_type = cluster["instanceType"].as_str().unwrap_or_default();
    let mut data = json!({
        "projectId": pid,
        "clusterName": request.cluster,
        "clusterId": cluster_id,
        "instanceType": instance_type,
        "connectionType": request.connection_type,
        "target": display,
        "accessList": access_list,
        "temporaryUser": {"username": username, "role": format!("{role}@admin"), "deleteAfterDate": delete_after},
    });
    let mut outcome = with_access_list(Outcome::new(Value::Null), access_list);
    if let Some((fields, warning)) =
        shared_tier_alerts(client, pid, &request.cluster, instance_type).await
    {
        if let (Some(data), Some(fields)) = (data.as_object_mut(), fields.as_object()) {
            data.extend(fields.clone());
        }
        outcome.warnings.extend(warning);
    }
    Ok(Connected {
        uri,
        user,
        data,
        warnings: outcome.warnings,
    })
}

/// Delete a temporary user created by `connect_cluster`.
pub async fn delete_temporary_user(
    client: &AtlasClient,
    user: &TemporaryUser,
) -> Result<Value, ClientError> {
    client
        .delete(&format!(
            "/groups/{}/databaseUsers/admin/{}",
            seg(&user.project_id),
            seg(&user.username)
        ))
        .await
}

/// Retry connect and ping until the deadline; new Atlas users take a while to propagate.
async fn verify(uri: &ConnectionUri, deadline: Instant) -> Result<(), ClientError> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let attempt = remaining
            .min(Duration::from_secs(10))
            .max(Duration::from_secs(1));
        let error = match MongoClient::connect(uri, attempt).await {
            Ok(client) => match client.ping().await {
                Ok(()) => return Ok(()),
                Err(error) => error,
            },
            Err(error) => error,
        };
        if Instant::now() + Duration::from_millis(500) >= deadline {
            return Err(error);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Insert credentials into a credential-free connection string and authenticate against
/// `admin`. The driver requires a `/` before the options, which Atlas's SRV strings omit.
pub fn with_credentials(uri: &str, username: &str, password: &str) -> Result<String, ClientError> {
    let (scheme, rest) = uri
        .split_once("://")
        .ok_or_else(|| incompatible("Atlas returned a connection string without a scheme"))?;
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let hosts = authority
        .rsplit_once('@')
        .map_or(authority, |(_, hosts)| hosts);
    let (path, query) = tail.split_once('?').unwrap_or((tail, ""));
    let path = if path.is_empty() { "/" } else { path };
    let mut options: Vec<&str> = query
        .split('&')
        .filter(|option| {
            let key = option.split('=').next().unwrap_or_default();
            !key.is_empty() && !key.eq_ignore_ascii_case("authSource")
        })
        .collect();
    options.push("authSource=admin");
    Ok(format!(
        "{scheme}://{}:{}@{hosts}{path}?{}",
        seg(username),
        seg(password),
        options.join("&")
    ))
}

/// Pick a connection string by type, preferring SRV strings like the MCP server.
fn connection_string<'a>(strings: &'a Value, connection_type: &str) -> Option<&'a str> {
    let pick = |value: &'a Value| value.as_str().filter(|text| !text.is_empty());
    match connection_type {
        "standard" => pick(&strings["standardSrv"]).or_else(|| pick(&strings["standard"])),
        "private" => pick(&strings["privateSrv"]).or_else(|| pick(&strings["private"])),
        _ => pick(&strings["privateEndpoint"][0]["srvConnectionString"])
            .or_else(|| pick(&strings["privateEndpoint"][0]["connectionString"])),
    }
}

fn delete_after(hours: u32) -> Result<String, ClientError> {
    let seconds = DateTime::now().timestamp_millis() / 1000 + i64::from(hours) * 3600;
    DateTime::from_millis(seconds * 1000)
        .try_to_rfc3339_string()
        .map_err(|error| ClientError::new(ErrorCode::Validation, error.to_string()))
}

/// Inspect a cluster, falling back to the Flex endpoint; a double failure reports the first.
async fn inspect(
    client: &AtlasClient,
    project_id: &str,
    cluster: &str,
) -> Result<Value, ClientError> {
    let path = format!("/groups/{}/clusters/{}", seg(project_id), seg(cluster));
    match client.get(&path, &[]).await {
        Ok(found) => Ok(format_cluster(&found)),
        Err(error) => {
            let flex = format!("/groups/{}/flexClusters/{}", seg(project_id), seg(cluster));
            client
                .get(&flex, &[])
                .await
                .map(|found| format_flex_cluster(&found))
                .map_err(|_| error)
        }
    }
}

/// Summarize a dedicated or free cluster like the MCP server's `formatCluster`.
pub fn format_cluster(cluster: &Value) -> Value {
    let first = cluster["replicationSpecs"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|spec| spec["regionConfigs"].as_array().into_iter().flatten())
        .next();
    let size = first
        .and_then(|config| {
            ["electableSpecs", "readOnlySpecs", "analyticsSpecs"]
                .iter()
                .find_map(|specs| {
                    config[specs]["instanceSize"]
                        .as_str()
                        .filter(|size| !size.is_empty())
                })
        })
        .unwrap_or("UNKNOWN");
    let free = size == "M0";
    let primary = &cluster["replicationSpecs"][0]["regionConfigs"][0];
    compact(json!({
        "name": cluster["name"],
        "clusterId": cluster["id"],
        "instanceType": if free { "FREE" } else { "DEDICATED" },
        "instanceSize": if free { Value::Null } else { json!(size) },
        "provider": if free { &primary["backingProviderName"] } else { &primary["providerName"] },
        "region": primary["regionName"],
        "paused": cluster["paused"].as_bool().unwrap_or(false),
        "state": cluster["stateName"],
        "mongoDBVersion": cluster["mongoDBVersion"],
        "connectionStrings": cluster["connectionStrings"],
        "processIds": process_ids(&cluster["connectionStrings"]["standard"]),
    }))
}

fn format_flex_cluster(cluster: &Value) -> Value {
    let settings = &cluster["providerSettings"];
    compact(json!({
        "name": cluster["name"],
        "clusterId": cluster["id"],
        "instanceType": "FLEX",
        "provider": settings["backingProviderName"],
        "region": settings["regionName"],
        "paused": false,
        "state": cluster["stateName"],
        "mongoDBVersion": cluster["mongoDBVersion"],
        "connectionStrings": cluster["connectionStrings"],
        "processIds": process_ids(&cluster["connectionStrings"]["standard"]),
    }))
}

/// The `host:port` entries of a non-SRV connection string.
fn process_ids(standard: &Value) -> Vec<String> {
    let Some(uri) = standard.as_str() else {
        return Vec::new();
    };
    let rest = uri.split_once("://").map_or(uri, |(_, rest)| rest);
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    let hosts = authority
        .rsplit_once('@')
        .map_or(authority, |(_, hosts)| hosts);
    hosts
        .split(',')
        .filter(|host| !host.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Add the caller's public IP to the project's access list. Never fails; the result is
/// `added`, `already-present`, or `failed`.
async fn ensure_current_ip(client: &AtlasClient, project_id: &str) -> &'static str {
    let Ok(address) = client.current_ip().await else {
        return "failed";
    };
    let entry =
        json!([{"groupId": project_id, "ipAddress": address, "comment": ACCESS_LIST_COMMENT}]);
    match client
        .post(
            &format!("/groups/{}/accessList", seg(project_id)),
            Some(&entry),
        )
        .await
    {
        Ok(_) => "added",
        Err(error) if error.server_code() == Some(409) => "already-present",
        Err(_) => "failed",
    }
}

fn with_access_list(outcome: Outcome, result: &str) -> Outcome {
    match result {
        "added" => outcome.warn("your current IP address was added to the project's IP access list"),
        "failed" => outcome.warn("your current IP address could not be added to the project's IP access list; add it with `mongodb-cli atlas access-list add --current-ip`"),
        _ => outcome,
    }
}

/// Open shared-tier threshold alerts of a Free or Flex cluster, as the MCP server reports them.
async fn shared_tier_alerts(
    client: &AtlasClient,
    project_id: &str,
    cluster: &str,
    instance_type: &str,
) -> Option<(Value, Option<String>)> {
    let tier = match instance_type {
        "FREE" => "Free",
        "FLEX" => "Flex",
        _ => return None,
    };
    let query = [
        ("status", "OPEN".to_owned()),
        ("itemsPerPage", "10".to_owned()),
        ("pageNum", "1".to_owned()),
        ("includeCount", "false".to_owned()),
    ];
    let alerts = client
        .get(&format!("/groups/{}/alerts", seg(project_id)), &query)
        .await
        .unwrap_or_default();
    let metrics = shared_tier_metrics(&alerts, cluster);
    if metrics.is_empty() {
        return Some((
            json!({"sharedTierAlertsDetected": false, "sharedTierTier": tier}),
            None,
        ));
    }
    let mut sorted = metrics.clone();
    sorted.sort_unstable();
    let warning = format!(
        "Atlas reports open shared-tier threshold alerts for cluster \"{cluster}\" affecting: {}. You may be near connection or storage limits on this {tier} tier deployment. Consider upgrading to a paid tier for more headroom — use `mongodb-cli atlas clusters upgrade` to upgrade \"{cluster}\".",
        sorted.join(", ")
    );
    Some((
        json!({"sharedTierAlertsDetected": true, "sharedTierTier": tier, "sharedTierAlerts": metrics}),
        Some(warning),
    ))
}

fn shared_tier_metrics<'a>(alerts: &'a Value, cluster: &str) -> Vec<&'a str> {
    let mut metrics = Vec::new();
    for alert in results(alerts) {
        let relevant = alert["id"].is_string()
            && alert["status"].is_string()
            && alert["clusterName"] == cluster
            && matches!(
                alert["eventTypeName"].as_str(),
                Some("OUTSIDE_METRIC_THRESHOLD" | "OUTSIDE_FLEX_METRIC_THRESHOLD")
            );
        if let Some(metric) = alert["metricName"].as_str()
            && relevant
            && SHARED_TIER_METRICS.contains(&metric)
            && !metrics.contains(&metric)
        {
            metrics.push(metric);
        }
    }
    metrics
}

/// Sixteen random bytes as unpadded base64url: 22 characters, as in the MCP server.
fn generate_password() -> SecretString {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).expect("the operating system provides random bytes");
    SecretString::from(URL_SAFE_NO_PAD.encode(bytes))
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0_u8; bytes];
    getrandom::fill(&mut buffer).expect("the operating system provides random bytes");
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A list page with the MCP server's `totalCount`: the API count when positive, else the page size.
fn counted(
    mut data: Map<String, Value>,
    key: &str,
    items: Vec<Value>,
    paging: Paging,
    response: &Value,
) -> Outcome {
    let returned = items.len();
    let api_total = response["totalCount"].as_u64();
    let total = if returned == 0 {
        0
    } else {
        api_total
            .filter(|total| *total > 0)
            .unwrap_or(returned as u64)
    };
    data.insert(key.into(), Value::Array(items));
    data.insert("totalCount".into(), json!(total));
    Outcome::paged(Value::Object(data), paging, returned, api_total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(regions: &[&str]) -> ClusterSpec {
        ClusterSpec {
            name: "c1".into(),
            provider: "AWS".into(),
            regions: regions.iter().map(|region| (*region).to_owned()).collect(),
            cluster_type: "REPLICASET".into(),
            instance_size: None,
            compute_autoscaling: true,
            disk_size_gb: None,
            mongodb_version: "LATEST".into(),
            backup: "SNAPSHOT".into(),
            termination_protection: false,
            encryption_at_rest_provider: None,
        }
    }

    #[test]
    fn autoscaling_ceilings_follow_the_mcp_table() {
        for (size, provider, expected) in [
            ("M10", "AWS", "M30"),
            ("M20", "GCP", "M40"),
            ("M30", "AWS", "M50"),
            ("M40", "AWS", "M60"),
            ("M50", "AWS", "M80"),
            ("M60", "AWS", "M140"),
            ("M60", "AZURE", "M200"),
            ("M80", "GCP", "M200"),
        ] {
            assert_eq!(
                max_autoscaling_size(size, provider),
                expected,
                "{size} {provider}"
            );
        }
    }

    #[test]
    fn cluster_bodies_distribute_nodes_and_map_options() {
        let body = cluster_body(&spec(&["US_EAST_1", "US_WEST_2"]), "M10", "NONE");
        assert_eq!(
            body,
            json!({
                "name": "c1",
                "clusterType": "REPLICASET",
                "replicationSpecs": [{"regionConfigs": [
                    {"providerName": "AWS", "regionName": "US_EAST_1", "priority": 7,
                     "electableSpecs": {"instanceSize": "M10", "nodeCount": 2},
                     "autoScaling": {"compute": {"enabled": true, "scaleDownEnabled": true, "minInstanceSize": "M10", "maxInstanceSize": "M30"}, "diskGB": {"enabled": true}}},
                    {"providerName": "AWS", "regionName": "US_WEST_2", "priority": 6,
                     "electableSpecs": {"instanceSize": "M10", "nodeCount": 1},
                     "autoScaling": {"compute": {"enabled": true, "scaleDownEnabled": true, "minInstanceSize": "M10", "maxInstanceSize": "M30"}, "diskGB": {"enabled": true}}}
                ]}],
                "terminationProtectionEnabled": false,
                "backupEnabled": true,
                "pitEnabled": false,
                "versionReleaseSystem": "CONTINUOUS",
                "encryptionAtRestProvider": "NONE"
            })
        );
        for (regions, nodes) in [
            (vec!["A"], vec![3]),
            (vec!["A", "B"], vec![2, 1]),
            (vec!["A", "B", "C"], vec![2, 2, 1]),
        ] {
            let body = cluster_body(&spec(&regions), "M30", "AWS");
            let counts: Vec<_> = body["replicationSpecs"][0]["regionConfigs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|config| config["electableSpecs"]["nodeCount"].as_u64().unwrap())
                .collect();
            assert_eq!(counts, nodes);
        }
        let mut options = spec(&["A"]);
        options.compute_autoscaling = false;
        options.disk_size_gb = Some(40.0);
        options.mongodb_version = "8.0".into();
        options.backup = "CONTINUOUS".into();
        let body = cluster_body(&options, "M30", "AWS");
        let config = &body["replicationSpecs"][0]["regionConfigs"][0];
        assert_eq!(
            config["autoScaling"]["compute"],
            json!({"enabled": false, "scaleDownEnabled": false})
        );
        assert_eq!(config["electableSpecs"]["diskSizeGB"], json!(40.0));
        assert_eq!(body["versionReleaseSystem"], "LTS");
        assert_eq!(body["mongoDBMajorVersion"], "8.0");
        assert_eq!(
            (body["backupEnabled"].clone(), body["pitEnabled"].clone()),
            (json!(true), json!(true))
        );
        options.backup = "OFF".into();
        let body = cluster_body(&options, "M30", "AWS");
        assert_eq!(
            (body["backupEnabled"].clone(), body["pitEnabled"].clone()),
            (json!(false), json!(false))
        );
    }

    #[test]
    fn cluster_specs_reject_invalid_combinations_before_any_call() {
        let mut sharded = spec(&["A"]);
        sharded.cluster_type = "SHARDED".into();
        sharded.instance_size = Some("M20".into());
        assert!(
            sharded
                .validate()
                .unwrap_err()
                .to_string()
                .contains("M30 or higher")
        );
        sharded.instance_size = Some("M30".into());
        assert!(sharded.validate().is_ok());
        assert!(spec(&["A", "B", "C", "D"]).validate().is_err());
        let mut disk = spec(&["A"]);
        disk.disk_size_gb = Some(0.0);
        assert!(disk.validate().is_err());
    }

    fn dedicated(size: &str, compute: Value) -> Value {
        json!({
            "id": "c-id",
            "clusterType": "REPLICASET",
            "replicationSpecs": [{"id": "spec-1", "regionConfigs": [
                {"providerName": "AWS", "regionName": "US_EAST_1", "priority": 7,
                 "electableSpecs": {"instanceSize": size, "nodeCount": 3},
                 "readOnlySpecs": {"instanceSize": size, "nodeCount": 0},
                 "analyticsSpecs": {"instanceSize": size, "nodeCount": 0},
                 "autoScaling": {"compute": compute, "diskGB": {"enabled": true}}}
            ]}],
        })
    }

    fn request(target: Option<&str>) -> UpgradeRequest {
        UpgradeRequest {
            target_tier: target.map(str::to_owned),
            ..UpgradeRequest::default()
        }
    }

    #[test]
    fn dedicated_upgrades_resolve_size_and_autoscaling() {
        let enabled = json!({"enabled": true, "scaleDownEnabled": true, "minInstanceSize": "M10", "maxInstanceSize": "M60"});
        // Scaling keeps a higher existing ceiling and resets the floor to the new size.
        let plan = plan_upgrade(
            "c0",
            &dedicated("M10", enabled.clone()),
            &request(Some("M30")),
        )
        .unwrap();
        assert_eq!(plan.kind, "DEDICATED");
        assert_eq!(
            plan.data,
            json!({"originalTier": "M10", "targetTier": "M30", "computeAutoScaling": true,
                   "minInstanceSize": "M30", "maxInstanceSize": "M60", "resolvedProvider": "AWS", "resolvedRegion": "US_EAST_1"})
        );
        let config = &plan.body["replicationSpecs"][0]["regionConfigs"][0];
        assert_eq!(plan.body["replicationSpecs"][0]["id"], "spec-1");
        assert_eq!(
            config["electableSpecs"],
            json!({"instanceSize": "M30", "nodeCount": 3})
        );
        assert_eq!(config["readOnlySpecs"]["instanceSize"], "M30");
        assert_eq!(
            config["analyticsSpecs"]["instanceSize"], "M10",
            "analytics nodes are untouched"
        );
        assert_eq!(config["autoScaling"]["diskGB"], json!({"enabled": true}));
        // Scaling past the existing ceiling uses the default ceiling of the new size.
        let plan = plan_upgrade(
            "c0",
            &dedicated("M10", enabled.clone()),
            &request(Some("M60")),
        )
        .unwrap();
        assert_eq!(plan.data["maxInstanceSize"], "M140");
        // Without resizing, existing bounds are kept.
        let only_autoscaling = UpgradeRequest {
            compute_autoscaling: Some(true),
            ..UpgradeRequest::default()
        };
        let plan =
            plan_upgrade("c0", &dedicated("M20", enabled.clone()), &only_autoscaling).unwrap();
        assert_eq!(
            (
                plan.data["minInstanceSize"].clone(),
                plan.data["maxInstanceSize"].clone()
            ),
            (json!("M10"), json!("M60"))
        );
        // Disabling drops both bounds.
        let disable = UpgradeRequest {
            compute_autoscaling: Some(false),
            ..UpgradeRequest::default()
        };
        let plan = plan_upgrade("c0", &dedicated("M20", enabled.clone()), &disable).unwrap();
        assert_eq!(plan.data["computeAutoScaling"], false);
        assert!(plan.data.get("minInstanceSize").is_none());
        assert_eq!(
            plan.body["replicationSpecs"][0]["regionConfigs"][0]["autoScaling"]["compute"],
            json!({"enabled": false, "scaleDownEnabled": false})
        );
        // Enabling on a cluster without autoscaling derives both bounds from the size.
        let plan = plan_upgrade(
            "c0",
            &dedicated("M20", json!({"enabled": false})),
            &only_autoscaling,
        )
        .unwrap();
        assert_eq!(
            (
                plan.data["minInstanceSize"].clone(),
                plan.data["maxInstanceSize"].clone()
            ),
            (json!("M20"), json!("M40"))
        );
    }

    #[test]
    fn dedicated_upgrades_reject_unsupported_requests_in_order() {
        let cluster = dedicated("M10", json!({"enabled": false}));
        let provider = UpgradeRequest {
            provider: Some("AWS".into()),
            target_tier: Some("M20".into()),
            ..UpgradeRequest::default()
        };
        let inverted = UpgradeRequest {
            target_tier: Some("M30".into()),
            compute_autoscaling: Some(true),
            min_instance_size: Some("M50".into()),
            max_instance_size: Some("M40".into()),
            ..UpgradeRequest::default()
        };
        let mut multi_shard = cluster.clone();
        multi_shard["replicationSpecs"] = json!([
            cluster["replicationSpecs"][0],
            cluster["replicationSpecs"][0]
        ]);
        let mut mixed = cluster.clone();
        mixed["replicationSpecs"][0]["regionConfigs"] = json!([
            {"electableSpecs": {"instanceSize": "M10"}, "regionName": "A", "providerName": "AWS"},
            {"electableSpecs": {"instanceSize": "M20"}, "regionName": "B", "providerName": "AWS"}
        ]);
        let mut odd_size = cluster.clone();
        odd_size["replicationSpecs"][0]["regionConfigs"][0]["electableSpecs"]["instanceSize"] =
            json!("R40");
        for (raw, upgrade, expected) in [
            (&cluster, request(Some("FLEX")), "already Dedicated"),
            (&cluster, provider, "provider/region are not valid"),
            (&cluster, request(None), "No changes specified"),
            (&odd_size, request(Some("M30")), "has instance size \"R40\""),
            (
                &multi_shard,
                request(Some("M30")),
                "Only single-shard clusters",
            ),
            (&mixed, request(Some("M30")), "different instance sizes"),
            (&cluster, inverted, "cannot be higher than maxInstanceSize"),
        ] {
            let error = plan_upgrade("c0", raw, &upgrade).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Validation);
            assert!(error.to_string().contains(expected), "{expected}: {error}");
        }
    }

    #[test]
    fn free_and_flex_upgrades_build_tenant_upgrade_bodies() {
        let free = json!({"replicationSpecs": [{"regionConfigs": [{
            "providerName": "TENANT", "backingProviderName": "AWS", "regionName": "US_EAST_1",
            "electableSpecs": {"instanceSize": "M0"}}]}]});
        let plan = plan_upgrade("c0", &free, &request(None)).unwrap();
        assert_eq!(plan.kind, "FREE");
        assert_eq!(
            plan.body,
            json!({"name": "c0", "providerSettings": {"providerName": "FLEX", "instanceSizeName": "FLEX", "backingProviderName": "AWS", "regionName": "US_EAST_1"}})
        );
        assert_eq!(plan.data["targetTier"], "FLEX");
        let plan = plan_upgrade("c0", &free, &request(Some("M10"))).unwrap();
        assert_eq!(
            plan.body,
            json!({"name": "c0",
                   "providerSettings": {"providerName": "AWS", "instanceSizeName": "M10", "regionName": "US_EAST_1",
                                        "autoScaling": {"compute": {"minInstanceSize": "M10", "maxInstanceSize": "M30"}}},
                   "autoScaling": {"compute": {"enabled": true, "scaleDownEnabled": true}, "diskGBEnabled": true}})
        );
        assert_eq!(
            plan.data["maxInstanceSize"], "M30",
            "M10 upgrades report the resolved autoscaling"
        );
        let flex =
            json!({"providerSettings": {"backingProviderName": "GCP", "regionName": "CENTRAL_US"}});
        let plan = plan_flex_upgrade("c0", &flex, &request(None)).unwrap();
        assert_eq!(plan.kind, "FLEX");
        assert_eq!(
            plan.body,
            json!({"name": "c0", "clusterType": "REPLICASET", "replicationSpecs": [{"regionConfigs": [{
                "providerName": "GCP", "regionName": "CENTRAL_US", "priority": 7,
                "electableSpecs": {"instanceSize": "M10", "nodeCount": 3},
                "autoScaling": {"compute": {"enabled": true, "scaleDownEnabled": true, "minInstanceSize": "M10", "maxInstanceSize": "M30"}, "diskGB": {"enabled": true}}}]}]})
        );
        let autoscaled_flex = UpgradeRequest {
            compute_autoscaling: Some(true),
            target_tier: Some("FLEX".into()),
            ..UpgradeRequest::default()
        };
        let min_m20 = UpgradeRequest {
            min_instance_size: Some("M20".into()),
            ..UpgradeRequest::default()
        };
        for (raw, upgrade, flex_source, expected) in [
            (&flex, request(Some("FLEX")), true, "already a Flex cluster"),
            (
                &flex,
                request(Some("M30")),
                true,
                "not valid when upgrading from FLEX",
            ),
            (
                &free,
                autoscaled_flex,
                false,
                "do not support compute autoscaling",
            ),
            (
                &free,
                UpgradeRequest {
                    target_tier: Some("M10".into()),
                    ..min_m20.clone()
                },
                false,
                "must be omitted or \"M10\"",
            ),
            (&flex, min_m20, true, "must be omitted or \"M10\""),
        ] {
            let result = if flex_source {
                plan_flex_upgrade("c0", raw, &upgrade)
            } else {
                plan_upgrade("c0", raw, &upgrade)
            };
            let error = result.unwrap_err();
            assert!(error.to_string().contains(expected), "{expected}: {error}");
        }
    }

    #[test]
    fn credentials_are_inserted_with_a_slash_before_the_options() {
        for (input, expected) in [
            (
                "mongodb+srv://cluster0.x.mongodb.net",
                "mongodb+srv://u:p@cluster0.x.mongodb.net/?authSource=admin",
            ),
            (
                "mongodb://a.x.net:27017,b.x.net:27017/?ssl=true&authSource=admin&replicaSet=atlas-1",
                "mongodb://u:p@a.x.net:27017,b.x.net:27017/?ssl=true&replicaSet=atlas-1&authSource=admin",
            ),
            (
                "mongodb://old:creds@host/test?AuthSource=x",
                "mongodb://u:p@host/test?authSource=admin",
            ),
        ] {
            assert_eq!(
                with_credentials(input, "u", "p").unwrap(),
                expected,
                "{input}"
            );
        }
        assert_eq!(
            with_credentials("mongodb://h", "a b", "p@ss").unwrap(),
            "mongodb://a%20b:p%40ss@h/?authSource=admin"
        );
        assert!(with_credentials("cluster0.x.net", "u", "p").is_err());
    }

    #[test]
    fn clusters_are_summarized_like_the_mcp_server() {
        let free = format_cluster(&json!({
            "name": "c0", "id": "1", "stateName": "IDLE",
            "replicationSpecs": [{"regionConfigs": [{"providerName": "TENANT", "backingProviderName": "AWS", "regionName": "US_EAST_1", "electableSpecs": {"instanceSize": "M0"}}]}],
            "connectionStrings": {"standard": "mongodb://h1:27017,h2:27017/?ssl=true", "standardSrv": "mongodb+srv://c0.x.net"}
        }));
        assert_eq!(free["instanceType"], "FREE");
        assert_eq!(free["provider"], "AWS");
        assert!(free.get("instanceSize").is_none());
        assert_eq!(free["processIds"], json!(["h1:27017", "h2:27017"]));
        assert_eq!(free["paused"], false);
        let dedicated = format_cluster(&dedicated("M30", json!({})));
        assert_eq!(
            (
                dedicated["instanceType"].clone(),
                dedicated["instanceSize"].clone()
            ),
            (json!("DEDICATED"), json!("M30"))
        );
        assert_eq!(format_cluster(&json!({}))["instanceSize"], "UNKNOWN");
        assert_eq!(
            connection_string(
                &json!({"standard": "mongodb://h", "standardSrv": ""}),
                "standard"
            ),
            Some("mongodb://h")
        );
        assert_eq!(
            connection_string(
                &json!({"privateEndpoint": [{"connectionString": "mongodb://pe"}]}),
                "privateEndpoint"
            ),
            Some("mongodb://pe")
        );
        assert_eq!(connection_string(&json!({}), "private"), None);
    }

    #[test]
    fn roles_parse_database_and_collection_scopes() {
        for (text, role, database, collection) in [
            ("readWrite", "readWrite", "admin", None),
            ("read@shop", "read", "shop", None),
            (
                "read@shop.orders.archive",
                "read",
                "shop",
                Some("orders.archive"),
            ),
            ("custom-role_1@app", "custom-role_1", "app", None),
        ] {
            assert_eq!(
                DbRole::parse(text).unwrap(),
                DbRole {
                    role_name: role.into(),
                    database_name: database.into(),
                    collection_name: collection.map(str::to_owned),
                },
                "{text}"
            );
        }
        for text in ["", "bad role", "read@", "read@shop.", "read@shöp"] {
            assert!(DbRole::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn shared_tier_alerts_match_the_mcp_filter() {
        let alerts = json!({"results": [
            {"id": "1", "status": "OPEN", "clusterName": "c0", "eventTypeName": "OUTSIDE_METRIC_THRESHOLD", "metricName": "LOGICAL_SIZE"},
            {"id": "2", "status": "OPEN", "clusterName": "c0", "eventTypeName": "OUTSIDE_FLEX_METRIC_THRESHOLD", "metricName": "CONNECTIONS_PERCENT"},
            {"id": "3", "status": "OPEN", "clusterName": "c0", "eventTypeName": "OUTSIDE_METRIC_THRESHOLD", "metricName": "LOGICAL_SIZE"},
            {"id": "4", "status": "OPEN", "clusterName": "other", "eventTypeName": "OUTSIDE_METRIC_THRESHOLD", "metricName": "FLEX_DATA_SIZE_TOTAL"},
            {"id": "5", "status": "OPEN", "clusterName": "c0", "eventTypeName": "HOST_DOWN", "metricName": "LOGICAL_SIZE"},
            {"status": "OPEN", "clusterName": "c0", "eventTypeName": "OUTSIDE_METRIC_THRESHOLD", "metricName": "FLEX_CONNECTIONS_PERCENT"}
        ]});
        assert_eq!(
            shared_tier_metrics(&alerts, "c0"),
            ["LOGICAL_SIZE", "CONNECTIONS_PERCENT"]
        );
    }

    #[test]
    fn generated_secrets_have_the_mcp_shape() {
        let password = generate_password();
        assert_eq!(password.expose_secret().len(), 22);
        assert!(
            password
                .expose_secret()
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_ne!(
            password.expose_secret(),
            generate_password().expose_secret()
        );
        let suffix = random_hex(4);
        assert_eq!(suffix.len(), 8);
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(delete_after(4).unwrap().ends_with('Z'));
        assert_eq!(
            epoch_millis("2026-10-01T00:00:00Z").unwrap(),
            1_790_812_800_000
        );
        assert!(epoch_millis("yesterday").is_err());
    }
}
