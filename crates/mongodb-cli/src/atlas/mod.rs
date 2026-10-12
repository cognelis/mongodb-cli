//! Atlas Administration and Stream Processing commands, `auth login|logout --atlas`, and the
//! Atlas doctor checks. Safety settings, access checks, `--yes`, and input validation run before
//! any credential lookup or network request.

pub mod args;

use std::io::{self, IsTerminal, Read};
use std::path::Path;
use std::time::{Duration, Instant};

use mongodb_client::atlas::admin::{
    self, AdvisorOperation, ClusterSpec, ConnectRequest, TemporaryUser, UpgradeRequest,
};
use mongodb_client::atlas::streams::{self, ProcessorChanges, ProcessorSpec, StartOptions};
use mongodb_client::atlas::{
    AtlasClient, AtlasCredentials, DEFAULT_BASE_URL, Outcome, Paging, regions,
};
use mongodb_client::{ClientError, ErrorCode};
use secrecy::SecretString;
use serde_json::{Map, Value, json};

use self::args::*;
use crate::args::{Access, Cli, PipelineArgs};
use crate::config::{
    ACCESS_ENV, AtlasTempUser, ConfigFile, CredentialStore, Settings, atlas_account,
    environment_settings, parse_access, validate_profile_name,
};
use crate::dispatch::{AppError, DispatchResult, read_input};
use crate::doctor::{self, DoctorStatus};
use crate::output::Meta;

/// Service-account credentials for automation; both must be set (the Atlas CLI's names).
pub const CLIENT_ID_ENV: &str = "MONGODB_ATLAS_CLIENT_ID";
pub const CLIENT_SECRET_ENV: &str = "MONGODB_ATLAS_CLIENT_SECRET";
/// Another Atlas origin, such as Atlas for Government or a test server.
pub const BASE_URL_ENV: &str = "MONGODB_CLI_ATLAS_BASE_URL";
const INSECURE_WARNING: &str =
    "MONGODB_CLI_ATLAS_BASE_URL uses plain HTTP; Atlas credentials and tokens are sent unencrypted";

pub async fn run(
    cli: &Cli,
    command: &AtlasCommand,
    config: &mut ConfigFile,
    path: &Path,
    store: &dyn CredentialStore,
) -> Result<DispatchResult, AppError> {
    let (name, _) = command.describe();
    let resolved = resolve_target(config, cli.profile.as_deref());
    if let AtlasCommand::Regions {
        command: RegionsCommand::List { provider },
    } = command
    {
        // Static data needs no credentials, but the selected settings still apply.
        if let Ok(target) = &resolved {
            authorize(command, target, cli.yes)?;
        }
        let provider = atlas(provider);
        let regions: Vec<Value> = regions::regions(&provider)
            .iter()
            .map(|(name, location)| json!({"name": name, "location": location}))
            .collect();
        return Ok(result(
            name,
            json!({"provider": provider, "regions": regions}),
            Meta::default(),
        ));
    }
    let mut target = resolved.map_err(AppError::config)?;
    authorize(command, &target, cli.yes)?;
    let inputs = prepare(command, config)?;
    let client = target.client(store, cli.timeout)?;
    let started = Instant::now();
    let timeout = Duration::from_secs(cli.timeout);
    let outcome = match command {
        AtlasCommand::Clusters {
            command: ClustersCommand::Connect {
                cluster,
                project,
                save_profile,
                access,
                connection_type,
                user_lifetime_hours,
                ..
            },
        } => {
            let request = ConnectRequest {
                project_id: project.id.clone(),
                cluster: cluster.clone(),
                profile: save_profile.clone(),
                read_write: *access == Access::ReadWrite,
                connection_type: connection_type.atlas().into(),
                lifetime_hours: *user_lifetime_hours,
            };
            // Connect manages its own deadline so that a failed user is always deleted.
            connect(&client, &request, *access, started + timeout, config, path, store).await
        }
        AtlasCommand::Clusters {
            command: ClustersCommand::Disconnect { saved_profile, .. },
        } => match inputs.temporary_user {
            Some(user) => disconnect(&client, saved_profile, user, config, path, store).await,
            None => unreachable!("prepare loads the temporary user of a disconnect"),
        },
        command => match tokio::time::timeout(timeout, execute(command, inputs, &client)).await {
            Ok(outcome) => outcome.map_err(AppError::from),
            Err(_) => Err(AppError::new(
                ErrorCode::Timeout,
                format!("Atlas did not respond within the {}-second timeout", cli.timeout),
            )
            .with_hint("raise --timeout; a timed-out change may still complete in Atlas, so check its state before retrying")),
        },
    };
    client.revoke().await;
    let outcome = outcome?;
    let mut warnings = target.warnings;
    if client.insecure() {
        warnings.push(INSECURE_WARNING.into());
    }
    warnings.extend(outcome.warnings);
    let meta = Meta {
        profile: target.profile,
        access: Some(target.access),
        elapsed_ms: Some(started.elapsed().as_millis()),
        limit: outcome.limit,
        total: outcome.total,
        has_more: outcome.has_more,
        warnings,
        page: outcome.page,
        next_page: outcome
            .page
            .filter(|_| outcome.has_more == Some(true))
            .map(|page| page.saturating_add(1)),
        ..Meta::default()
    };
    Ok(result(name, outcome.data, meta))
}

/// Refuse what the credentials' settings and access do not allow, in the order database commands
/// use: disabled tools, read-only access, then `--yes` for changes and confirmation-required tools.
fn authorize(command: &AtlasCommand, target: &Target, yes: bool) -> Result<(), AppError> {
    let (name, risk) = command.describe();
    let (tool, operation_type) = mcp_tool(name);
    let selects = |entry: &&String| ["atlas", tool, name, operation_type].contains(&entry.as_str());
    let settings = &target.settings;
    if let Some(entry) = settings.disabled_tools.iter().find(selects) {
        return Err(AppError::new(
            ErrorCode::Forbidden,
            format!("{name} is disabled: \"{entry}\" is listed in the disabled tools"),
        )
        .with_hint(
            "the user disabled this operation for the connection; do not change its settings",
        ));
    }
    if risk.is_some() && target.access == Access::ReadOnly {
        return Err(refusal(name, target.profile.as_deref()));
    }
    if yes {
        return Ok(());
    }
    if let Some(risk) = risk {
        return Err(AppError::validation(format!(
            "{name} changes Atlas resources and requires --yes"
        ))
        .with_hint(format!(
            "it {risk}; confirm with the user, then rerun the same command with --yes"
        )));
    }
    match settings.confirmation_required_tools.iter().find(selects) {
        Some(entry) => Err(AppError::validation(format!(
            "{name} requires --yes: \"{entry}\" is listed in the confirmation-required tools"
        ))
        .with_hint("check the profile and target, then rerun the same command with --yes")),
        None => Ok(()),
    }
}

/// The MongoDB MCP server tool a command replicates and the command's operation type (read,
/// create, update, delete, or connect); safety-settings entries may name either.
fn mcp_tool(name: &'static str) -> (&'static str, &'static str) {
    match name {
        "atlas.orgs.list" => ("atlas-list-orgs", "read"),
        "atlas.projects.list" => ("atlas-list-projects", "read"),
        "atlas.projects.create" => ("atlas-create-project", "create"),
        "atlas.clusters.list" => ("atlas-list-clusters", "read"),
        "atlas.clusters.inspect" => ("atlas-inspect-cluster", "read"),
        "atlas.clusters.create" => ("atlas-create-cluster", "create"),
        "atlas.clusters.create-free" => ("atlas-create-free-cluster", "create"),
        "atlas.clusters.pause" | "atlas.clusters.resume" => {
            ("atlas-pause-resume-cluster", "update")
        }
        "atlas.clusters.upgrade" => ("atlas-upgrade-cluster", "update"),
        "atlas.clusters.advisor" => ("atlas-get-performance-advisor", "read"),
        "atlas.clusters.connect" => ("atlas-connect-cluster", "connect"),
        // The MCP server has no tool for this cleanup, which deletes a database user.
        "atlas.clusters.disconnect" => (name, "delete"),
        "atlas.access-list.list" => ("atlas-inspect-access-list", "read"),
        "atlas.access-list.add" => ("atlas-create-access-list", "create"),
        "atlas.db-users.list" => ("atlas-list-db-users", "read"),
        "atlas.db-users.create" => ("atlas-create-db-user", "create"),
        "atlas.alerts.list" => ("atlas-list-alerts", "read"),
        "atlas.regions.list" => ("atlas-get-regions", "read"),
        "atlas.sample-data.load" => ("atlas-load-sample-dataset", "create"),
        "atlas.sample-data.status" => ("atlas-load-sample-dataset", "read"),
        // The Stream Processing commands, named atlas.streams.<noun>.<verb>.
        streams => match streams.rsplit('.').next() {
            Some("list" | "inspect" | "diagnose" | "show") => ("atlas-streams-discover", "read"),
            Some("create") => ("atlas-streams-build", "create"),
            Some("delete") => ("atlas-streams-teardown", "delete"),
            _ => ("atlas-streams-manage", "update"),
        },
    }
}

fn refusal(command: &str, profile: Option<&str>) -> AppError {
    match profile {
        Some(name) => AppError::new(
            ErrorCode::Forbidden,
            format!("profile '{name}' is read-only; {command} would change Atlas resources"),
        )
        .with_hint("use a profile the user configured with --access read-write; do not change this profile's access"),
        None => AppError::new(
            ErrorCode::Forbidden,
            format!("the {CLIENT_ID_ENV} credentials are read-only; {command} would change Atlas resources"),
        )
        .with_hint("environment credentials allow changes only when MONGODB_CLI_ACCESS=read-write is set by the user"),
    }
}

/// The selected Atlas credentials, access mode, and safety settings, resolved without reading
/// the keyring.
#[derive(Debug)]
struct Target {
    profile: Option<String>,
    access: Access,
    settings: Settings,
    warnings: Vec<String>,
    environment: Option<AtlasCredentials>,
}

impl Target {
    /// Build a client from the environment credentials or the profile's keyring entry.
    fn client(
        &mut self,
        store: &dyn CredentialStore,
        timeout: u64,
    ) -> Result<AtlasClient, AppError> {
        let credentials = match self.environment.take() {
            Some(credentials) => credentials,
            None => {
                let name = self
                    .profile
                    .as_deref()
                    .expect("targets without environment credentials name a profile");
                stored_credentials(name, store).map_err(AppError::config)?
            }
        };
        Ok(AtlasClient::new(
            &base_url(),
            credentials,
            Duration::from_secs(timeout),
        )?)
    }
}

/// Read `{"clientId", "clientSecret"}` from the keyring entry `<profile>/atlas`.
fn stored_credentials(name: &str, store: &dyn CredentialStore) -> Result<AtlasCredentials, String> {
    let missing = || {
        format!(
            "profile '{name}' has no Atlas credentials; run `mongodb-cli --profile {name} auth login --atlas`"
        )
    };
    let stored = store
        .get(&atlas_account(name))?
        .filter(|value| !value.is_empty())
        .ok_or_else(missing)?;
    let value: Value = serde_json::from_str(&stored).map_err(|_| {
        format!("profile '{name}' has unreadable Atlas credentials; run `mongodb-cli --profile {name} auth login --atlas` again")
    })?;
    match (value["clientId"].as_str(), value["clientSecret"].as_str()) {
        (Some(id), Some(secret)) if !id.is_empty() && !secret.is_empty() => Ok(AtlasCredentials {
            client_id: SecretString::from(id.to_owned()),
            client_secret: SecretString::from(secret.to_owned()),
        }),
        _ => Err(missing()),
    }
}

fn base_url() -> String {
    environment(BASE_URL_ENV).unwrap_or_else(|| DEFAULT_BASE_URL.into())
}

fn environment(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Select credentials: `--profile`, then the environment, then the default profile.
fn resolve_target(config: &ConfigFile, selected: Option<&str>) -> Result<Target, String> {
    target_from(
        config,
        selected,
        environment(CLIENT_ID_ENV),
        environment(CLIENT_SECRET_ENV),
        environment(ACCESS_ENV).as_deref(),
    )
}

fn target_from(
    config: &ConfigFile,
    selected: Option<&str>,
    client_id: Option<String>,
    client_secret: Option<String>,
    access: Option<&str>,
) -> Result<Target, String> {
    let profile_target = |name: &str, warnings: Vec<String>| {
        let profile = config
            .profiles
            .get(name)
            .ok_or_else(|| format!("profile '{name}' does not exist"))?;
        Ok(Target {
            profile: Some(name.to_owned()),
            access: profile.access,
            settings: profile.settings(),
            warnings,
            environment: None,
        })
    };
    if let Some(name) = selected {
        let mut warnings = Vec::new();
        if client_id.is_some() || client_secret.is_some() {
            warnings.push(format!(
                "{CLIENT_ID_ENV} and {CLIENT_SECRET_ENV} are ignored because --profile selected '{name}'"
            ));
        }
        return profile_target(name, warnings);
    }
    match (client_id, client_secret) {
        (Some(id), Some(secret)) => {
            return Ok(Target {
                profile: None,
                access: parse_access(access)?,
                // Like MONGODB_URI connections, environment credentials take the
                // MONGODB_CLI_* settings variables.
                settings: environment_settings()?,
                warnings: Vec::new(),
                environment: Some(AtlasCredentials {
                    client_id: SecretString::from(id),
                    client_secret: SecretString::from(secret),
                }),
            });
        }
        (None, None) => {}
        _ => {
            return Err(format!(
                "set both {CLIENT_ID_ENV} and {CLIENT_SECRET_ENV}, or neither"
            ));
        }
    }
    let name = config.current_profile.as_deref().ok_or_else(|| {
        format!("no Atlas credentials: run `mongodb-cli auth login --atlas`, or set {CLIENT_ID_ENV} and {CLIENT_SECRET_ENV}")
    })?;
    profile_target(name, Vec::new())
}

/// Inputs read and checked before any credential lookup or network request.
#[derive(Default)]
struct Inputs {
    password: Option<SecretString>,
    config: Option<Map<String, Value>>,
    pipeline: Option<Vec<Value>>,
    temporary_user: Option<AtlasTempUser>,
}

fn prepare(command: &AtlasCommand, config: &ConfigFile) -> Result<Inputs, AppError> {
    let mut inputs = Inputs::default();
    match command {
        AtlasCommand::Clusters { command } => match command {
            ClustersCommand::Create { .. } => cluster_spec(command).validate()?,
            ClustersCommand::Connect { save_profile, .. } => {
                validate_profile_name(save_profile).map_err(AppError::validation)?;
                if config.profiles.contains_key(save_profile) {
                    return Err(AppError::validation(format!(
                        "profile '{save_profile}' already exists"
                    ))
                    .with_hint("choose a new --save-profile name"));
                }
            }
            ClustersCommand::Disconnect { saved_profile, .. } => {
                let profile = config.profiles.get(saved_profile).ok_or_else(|| {
                    AppError::config(format!("profile '{saved_profile}' does not exist"))
                })?;
                inputs.temporary_user = Some(profile.atlas_temp_user.clone().ok_or_else(|| {
                    AppError::config(format!(
                        "profile '{saved_profile}' was not created by `mongodb-cli atlas clusters connect`"
                    ))
                    .with_hint("remove other profiles with `mongodb-cli profile remove`")
                })?);
            }
            _ => {}
        },
        AtlasCommand::AccessList {
            command:
                AccessListCommand::Add {
                    addresses,
                    cidr_blocks,
                    current_ip,
                    ..
                },
        } if addresses.is_empty() && cidr_blocks.is_empty() && !current_ip => {
            return Err(AppError::validation(
                "One of --ip, --cidr, or --current-ip must be provided.",
            ));
        }
        AtlasCommand::DbUsers {
            command: DbUsersCommand::Create { password_stdin, .. },
        } => inputs.password = Some(read_password(*password_stdin)?),
        AtlasCommand::Streams { command } => match command {
            StreamsCommand::Connections { command } => match command {
                ConnectionsCommand::Create {
                    kind, config_file, ..
                } => {
                    let mut settings = match config_file {
                        Some(path) => read_object(path)?,
                        None => Map::new(),
                    };
                    streams::prepare_connection_config(kind.atlas(), &mut settings)?;
                    inputs.config = Some(settings);
                }
                ConnectionsCommand::Update { config_file, .. } => {
                    inputs.config = Some(read_object(config_file)?);
                }
                _ => {}
            },
            StreamsCommand::Processors { command } => match command {
                ProcessorsCommand::Create { pipeline, .. } => {
                    let pipeline = read_stages(pipeline)?;
                    streams::validate_pipeline(&pipeline)?;
                    inputs.pipeline = Some(pipeline);
                }
                ProcessorsCommand::Modify {
                    pipeline,
                    dlq,
                    new_name,
                    tier,
                    autoscaling,
                    ..
                } => {
                    let stages = match (&pipeline.pipeline, &pipeline.file) {
                        (None, None) => None,
                        (text, file) => Some(read_stages(&PipelineArgs {
                            pipeline: text.clone(),
                            file: file.clone(),
                        })?),
                    };
                    let unchanged = stages.is_none()
                        && dlq_option(dlq).is_none()
                        && new_name.is_none()
                        && tier.is_none()
                        && autoscaling_option(autoscaling).is_none();
                    if unchanged {
                        return Err(AppError::validation(
                            "No modifications specified. Provide at least one of: --pipeline or --file, --dlq-*, --new-name, --tier, or the autoscaling flags.",
                        ));
                    }
                    inputs.pipeline = stages;
                }
                _ => {}
            },
            StreamsCommand::Workspaces {
                command:
                    WorkspacesCommand::Update {
                        region: None,
                        tier: None,
                        ..
                    },
            } => {
                return Err(AppError::validation(
                    "No updates specified. Provide at least one of: --region or --tier.",
                ));
            }
            StreamsCommand::Privatelink {
                command: PrivateLinkCommand::Create { config_file, .. },
            } => {
                let settings = read_object(config_file)?;
                streams::validate_private_link(&settings)?;
                inputs.config = Some(settings);
            }
            _ => {}
        },
        _ => {}
    }
    Ok(inputs)
}

async fn execute(
    command: &AtlasCommand,
    mut inputs: Inputs,
    client: &AtlasClient,
) -> Result<Outcome, ClientError> {
    match command {
        AtlasCommand::Orgs {
            command: OrgsCommand::List(list),
        } => admin::list_orgs(client, paging(list)).await,
        AtlasCommand::Projects { command } => match command {
            ProjectsCommand::List { org_id, list } => {
                admin::list_projects(client, org_id.as_deref(), paging(list)).await
            }
            ProjectsCommand::Create { name, org_id, .. } => {
                admin::create_project(client, name, org_id).await
            }
        },
        AtlasCommand::Clusters { command } => clusters(command, client).await,
        AtlasCommand::AccessList { command } => match command {
            AccessListCommand::List { project } => {
                admin::list_access_list(client, &project.id).await
            }
            AccessListCommand::Add {
                project,
                addresses,
                cidr_blocks,
                current_ip,
                comment,
                ..
            } => {
                admin::add_access_list(
                    client,
                    &project.id,
                    addresses,
                    cidr_blocks,
                    *current_ip,
                    comment.as_deref(),
                )
                .await
            }
        },
        AtlasCommand::DbUsers { command } => match command {
            DbUsersCommand::List { project } => admin::list_db_users(client, &project.id).await,
            DbUsersCommand::Create {
                username,
                project,
                roles,
                clusters,
                ..
            } => {
                let password = inputs.password.take().expect("prepare reads the password");
                admin::create_db_user(client, &project.id, username, &password, roles, clusters)
                    .await
            }
        },
        AtlasCommand::Alerts {
            command:
                AlertsCommand::List {
                    project,
                    status,
                    list,
                },
        } => admin::list_alerts(client, &project.id, &atlas(status), paging(list)).await,
        AtlasCommand::SampleData { command } => match command {
            SampleDataCommand::Load {
                cluster, project, ..
            } => admin::load_sample_data(client, &project.id, cluster).await,
            SampleDataCommand::Status { job_id, project } => {
                admin::sample_data_status(client, &project.id, job_id).await
            }
        },
        AtlasCommand::Streams { command } => stream(command, inputs, client)
            .await
            .map_err(streams::with_hint),
        AtlasCommand::Regions { .. } => unreachable!("regions are listed without credentials"),
    }
}

async fn clusters(command: &ClustersCommand, client: &AtlasClient) -> Result<Outcome, ClientError> {
    match command {
        ClustersCommand::List { project_id } => {
            admin::list_clusters(client, project_id.as_deref()).await
        }
        ClustersCommand::Inspect { cluster, project } => {
            admin::inspect_cluster(client, &project.id, cluster).await
        }
        ClustersCommand::Create { project, .. } => {
            admin::create_cluster(client, &project.id, &cluster_spec(command)).await
        }
        ClustersCommand::CreateFree {
            cluster,
            project,
            region,
            ..
        } => admin::create_free_cluster(client, &project.id, cluster, region).await,
        ClustersCommand::Pause {
            cluster, project, ..
        } => admin::pause_resume(client, &project.id, cluster, true).await,
        ClustersCommand::Resume {
            cluster, project, ..
        } => admin::pause_resume(client, &project.id, cluster, false).await,
        ClustersCommand::Upgrade {
            cluster,
            project,
            target_tier,
            compute_autoscaling,
            min_instance_size,
            max_instance_size,
            provider,
            region,
            ..
        } => {
            let request = UpgradeRequest {
                target_tier: target_tier.as_ref().map(atlas),
                compute_autoscaling: compute_autoscaling.map(|switch| switch == Switch::On),
                min_instance_size: min_instance_size.as_ref().map(atlas),
                max_instance_size: max_instance_size.as_ref().map(atlas),
                provider: provider.clone(),
                region: region.clone(),
            };
            admin::upgrade_cluster(client, &project.id, cluster, &request).await
        }
        ClustersCommand::Advisor {
            cluster,
            project,
            operations,
            since,
            namespaces,
        } => {
            let all = [
                AdvisorKind::SuggestedIndexes,
                AdvisorKind::DropIndexSuggestions,
                AdvisorKind::SlowQueryLogs,
                AdvisorKind::SchemaSuggestions,
            ];
            let selected = if operations.is_empty() {
                &all[..]
            } else {
                operations
            };
            let operations: Vec<AdvisorOperation> = selected
                .iter()
                .map(|operation| match operation {
                    AdvisorKind::SuggestedIndexes => AdvisorOperation::SuggestedIndexes,
                    AdvisorKind::DropIndexSuggestions => AdvisorOperation::DropIndexSuggestions,
                    AdvisorKind::SlowQueryLogs => AdvisorOperation::SlowQueryLogs,
                    AdvisorKind::SchemaSuggestions => AdvisorOperation::SchemaSuggestions,
                })
                .collect();
            admin::performance_advisor(
                client,
                &project.id,
                cluster,
                &operations,
                *since,
                namespaces,
            )
            .await
        }
        ClustersCommand::Connect { .. } | ClustersCommand::Disconnect { .. } => {
            unreachable!("connect and disconnect run with the configuration")
        }
    }
}

async fn stream(
    command: &StreamsCommand,
    mut inputs: Inputs,
    client: &AtlasClient,
) -> Result<Outcome, ClientError> {
    let mut config = || {
        inputs
            .config
            .take()
            .expect("prepare reads the configuration")
    };
    match command {
        StreamsCommand::Workspaces { command } => match command {
            WorkspacesCommand::List { project, list } => {
                streams::list_workspaces(
                    client,
                    &project.id,
                    list.limit,
                    list.page,
                    list.detail.detailed,
                )
                .await
            }
            WorkspacesCommand::Inspect {
                workspace,
                project,
                detail,
            } => streams::inspect_workspace(client, &project.id, workspace, detail.detailed).await,
            WorkspacesCommand::Create {
                workspace,
                project,
                cloud_provider,
                region,
                tier,
                no_sample_data,
                ..
            } => {
                let tier = tier.as_ref().map(atlas);
                streams::create_workspace(
                    client,
                    &project.id,
                    workspace,
                    &atlas(cloud_provider),
                    region,
                    tier.as_deref(),
                    !no_sample_data,
                )
                .await
            }
            WorkspacesCommand::Update {
                workspace,
                project,
                region,
                tier,
                ..
            } => {
                let tier = tier.as_ref().map(atlas);
                streams::update_workspace(
                    client,
                    &project.id,
                    workspace,
                    region.as_deref(),
                    tier.as_deref(),
                )
                .await
            }
            WorkspacesCommand::Delete {
                workspace, project, ..
            } => streams::delete_workspace(client, &project.id, workspace).await,
        },
        StreamsCommand::Connections { command } => match command {
            ConnectionsCommand::List {
                workspace,
                project,
                list,
            } => {
                streams::list_connections(
                    client,
                    &project.id,
                    workspace,
                    list.limit,
                    list.page,
                    list.detail.detailed,
                )
                .await
            }
            ConnectionsCommand::Inspect {
                workspace,
                connection,
                project,
                detail,
            } => {
                streams::inspect_connection(
                    client,
                    &project.id,
                    workspace,
                    connection,
                    detail.detailed,
                )
                .await
            }
            ConnectionsCommand::Create {
                workspace,
                connection,
                project,
                kind,
                ..
            } => {
                streams::create_connection(
                    client,
                    &project.id,
                    workspace,
                    connection,
                    kind.atlas(),
                    config(),
                )
                .await
            }
            ConnectionsCommand::Update {
                workspace,
                connection,
                project,
                ..
            } => {
                streams::update_connection(client, &project.id, workspace, connection, config())
                    .await
            }
            ConnectionsCommand::Delete {
                workspace,
                connection,
                project,
                ..
            } => streams::delete_connection(client, &project.id, workspace, connection).await,
        },
        StreamsCommand::Processors { command } => match command {
            ProcessorsCommand::List {
                workspace,
                project,
                list,
            } => {
                streams::list_processors(
                    client,
                    &project.id,
                    workspace,
                    list.limit,
                    list.page,
                    list.detail.detailed,
                )
                .await
            }
            ProcessorsCommand::Inspect {
                workspace,
                processor,
                project,
                detail,
            } => {
                streams::inspect_processor(
                    client,
                    &project.id,
                    workspace,
                    processor,
                    detail.detailed,
                )
                .await
            }
            ProcessorsCommand::Diagnose {
                workspace,
                processor,
                project,
            } => streams::diagnose_processor(client, &project.id, workspace, processor).await,
            ProcessorsCommand::Create {
                workspace,
                processor,
                project,
                dlq,
                tier,
                autoscaling,
                start,
                ..
            } => {
                let spec = ProcessorSpec {
                    pipeline: inputs.pipeline.take().expect("prepare reads the pipeline"),
                    dlq: dlq_option(dlq),
                    tier: tier.as_ref().map(atlas),
                    autoscaling: autoscaling_option(autoscaling),
                    start: *start,
                };
                streams::create_processor(client, &project.id, workspace, processor, &spec).await
            }
            ProcessorsCommand::Start {
                workspace,
                processor,
                project,
                tier,
                autoscaling,
                no_resume_from_checkpoint,
                start_at_operation_time,
                ..
            } => {
                let options = StartOptions {
                    tier: tier.as_ref().map(atlas),
                    autoscaling: autoscaling_option(autoscaling),
                    resume_from_checkpoint: no_resume_from_checkpoint.then_some(false),
                    start_at_operation_time: start_at_operation_time.clone(),
                };
                streams::start_processor(client, &project.id, workspace, processor, &options).await
            }
            ProcessorsCommand::Stop {
                workspace,
                processor,
                project,
                ..
            } => streams::stop_processor(client, &project.id, workspace, processor).await,
            ProcessorsCommand::Modify {
                workspace,
                processor,
                project,
                dlq,
                new_name,
                tier,
                autoscaling,
                ..
            } => {
                let changes = ProcessorChanges {
                    pipeline: inputs.pipeline.take(),
                    dlq: dlq_option(dlq),
                    new_name: new_name.clone(),
                    tier: tier.as_ref().map(atlas),
                    autoscaling: autoscaling_option(autoscaling),
                };
                streams::modify_processor(client, &project.id, workspace, processor, &changes).await
            }
            ProcessorsCommand::Delete {
                workspace,
                processor,
                project,
                ..
            } => streams::delete_processor(client, &project.id, workspace, processor).await,
        },
        StreamsCommand::Networking {
            command:
                NetworkingCommand::Show {
                    project,
                    cloud_provider,
                    region,
                },
        } => {
            let provider = cloud_provider.as_ref().map(atlas);
            let account = provider.as_deref().zip(region.as_deref());
            streams::networking(client, &project.id, account).await
        }
        StreamsCommand::Privatelink { command } => match command {
            PrivateLinkCommand::Create { project, .. } => {
                streams::create_private_link(client, &project.id, config()).await
            }
            PrivateLinkCommand::Delete {
                connection_id,
                project,
                ..
            } => streams::delete_private_link(client, &project.id, connection_id).await,
        },
        StreamsCommand::Peering { command } => match command {
            PeeringCommand::Accept {
                id,
                project,
                requester_account_id,
                requester_vpc_id,
                ..
            } => {
                streams::accept_peering(
                    client,
                    &project.id,
                    id,
                    requester_account_id,
                    requester_vpc_id,
                )
                .await
            }
            PeeringCommand::Reject { id, project, .. } => {
                streams::reject_peering(client, &project.id, id).await
            }
            PeeringCommand::Delete { id, project, .. } => {
                streams::delete_peering(client, &project.id, id).await
            }
        },
    }
}

/// Create a temporary user, verify it, and save its connection string in a new profile.
async fn connect(
    client: &AtlasClient,
    request: &ConnectRequest,
    access: Access,
    deadline: Instant,
    config: &mut ConfigFile,
    path: &Path,
    store: &dyn CredentialStore,
) -> Result<Outcome, AppError> {
    let connected = admin::connect_cluster(client, request, deadline).await?;
    let user = connected.user.clone();
    let target = connected.data["target"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let profile = request.profile.as_str();
    let saved = crate::onboarding::persist_credential(
        config,
        path,
        store,
        profile,
        &connected.uri,
        |config| {
            config.add_profile(profile, access)?;
            config.set_target(profile, &target)?;
            if let Some(saved) = config.profiles.get_mut(profile) {
                saved.atlas_temp_user = Some(AtlasTempUser {
                    project_id: user.project_id.clone(),
                    cluster_name: user.cluster_name.clone(),
                    username: user.username.clone(),
                    delete_after: user.delete_after.clone(),
                });
            }
            Ok(())
        },
    );
    if let Err(error) = saved {
        // The password exists nowhere else, so the user is useless without the profile.
        let _ = admin::delete_temporary_user(client, &user).await;
        return Err(error
            .with_hint("the temporary user was deleted because the profile could not be saved"));
    }
    let mut data = Map::new();
    data.insert("profile".into(), json!(profile));
    data.insert("access".into(), json!(access));
    if let Value::Object(fields) = connected.data {
        data.extend(fields);
    }
    let mut outcome = Outcome::new(Value::Object(data));
    outcome.warnings = connected.warnings;
    Ok(outcome)
}

/// Delete the temporary user behind a profile saved by `connect`, then the profile itself.
async fn disconnect(
    client: &AtlasClient,
    profile: &str,
    user: AtlasTempUser,
    config: &mut ConfigFile,
    path: &Path,
    store: &dyn CredentialStore,
) -> Result<Outcome, AppError> {
    let temporary = TemporaryUser {
        project_id: user.project_id,
        cluster_name: user.cluster_name,
        username: user.username,
        delete_after: user.delete_after,
    };
    let mut outcome = Outcome::new(json!({
        "profile": profile,
        "projectId": temporary.project_id,
        "clusterName": temporary.cluster_name,
        "username": temporary.username,
        "removed": true,
    }));
    match admin::delete_temporary_user(client, &temporary).await {
        Ok(_) => {}
        Err(error) if error.code() == ErrorCode::NotFound => outcome
            .warnings
            .push("the temporary user no longer exists in Atlas; it probably expired".into()),
        Err(error) => return Err(error.into()),
    }
    config
        .remove_profile(profile, store)
        .map_err(AppError::config)?;
    config.save(path).map_err(AppError::config)?;
    Ok(outcome)
}

fn cluster_spec(command: &ClustersCommand) -> ClusterSpec {
    let ClustersCommand::Create {
        cluster,
        provider,
        regions,
        cluster_type,
        instance_size,
        compute_autoscaling,
        disk_size_gb,
        mongodb_version,
        backup,
        termination_protection,
        encryption_at_rest_provider,
        ..
    } = command
    else {
        unreachable!("only clusters create has a cluster specification")
    };
    ClusterSpec {
        name: cluster.clone(),
        provider: atlas(provider),
        regions: regions.clone(),
        cluster_type: atlas(cluster_type),
        instance_size: instance_size.as_ref().map(atlas),
        compute_autoscaling: *compute_autoscaling == Switch::On,
        disk_size_gb: *disk_size_gb,
        mongodb_version: atlas(mongodb_version),
        backup: atlas(backup),
        termination_protection: *termination_protection,
        encryption_at_rest_provider: encryption_at_rest_provider.as_ref().map(atlas),
    }
}

fn paging(list: &ListArgs) -> Paging {
    Paging {
        limit: list.limit,
        page: list.page,
        include_count: list.include_count,
    }
}

fn dlq_option(dlq: &Dlq) -> Option<Value> {
    match (&dlq.dlq_connection, &dlq.dlq_db, &dlq.dlq_coll) {
        (Some(connection), Some(db), Some(coll)) => {
            Some(json!({"connectionName": connection, "db": db, "coll": coll}))
        }
        _ => None,
    }
}

/// Any autoscaling flag sends the object; `default` resets a bound with `null`.
fn autoscaling_option(autoscaling: &Autoscaling) -> Option<Value> {
    let bound = |tier: &TierBound| match tier {
        TierBound::Default => Value::Null,
        tier => json!(atlas(tier)),
    };
    let mut settings = Map::new();
    if let Some(switch) = autoscaling.autoscaling {
        settings.insert("enabled".into(), json!(switch == Switch::On));
    }
    if let Some(tier) = &autoscaling.min_tier {
        settings.insert("minTier".into(), bound(tier));
    }
    if let Some(tier) = &autoscaling.max_tier {
        settings.insert("maxTier".into(), bound(tier));
    }
    (!settings.is_empty()).then_some(Value::Object(settings))
}

fn read_object(path: &Path) -> Result<Map<String, Value>, AppError> {
    let label = path.display();
    match serde_json::from_str(&read_input(path)?) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => Err(AppError::validation(format!(
            "{label}: expected a JSON object"
        ))),
        Err(error) => Err(AppError::validation(format!(
            "{label}: invalid JSON: {error}"
        ))),
    }
}

fn read_stages(arguments: &PipelineArgs) -> Result<Vec<Value>, AppError> {
    let (text, label) = match (&arguments.pipeline, &arguments.file) {
        (Some(text), _) => (text.clone(), "--pipeline"),
        (None, Some(path)) => (read_input(path)?, "--file"),
        (None, None) => unreachable!("clap requires --pipeline or --file"),
    };
    match serde_json::from_str(&text) {
        Ok(Value::Array(stages)) if stages.iter().all(Value::is_object) => Ok(stages),
        Ok(_) => Err(AppError::validation(format!(
            "{label}: expected a JSON array of stage objects"
        ))),
        Err(error) => Err(AppError::validation(format!(
            "{label}: invalid JSON: {error}"
        ))),
    }
}

/// A database user's password comes from stdin or a masked prompt, never from a flag, and the
/// CLI never generates one because it would have to print it.
fn read_password(from_stdin: bool) -> Result<SecretString, AppError> {
    let password = if from_stdin {
        if io::stdin().is_terminal() {
            return Err(AppError::validation(
                "--password-stdin expects a pipe or redirected standard input",
            ));
        }
        read_lines(&mut io::stdin(), 1, "the password")?.remove(0)
    } else if crate::onboarding::interactive_terminal() {
        crate::onboarding::prompt_io(cliclack::password("Database user password").interact())?
    } else {
        return Err(AppError::validation("a database user password is required")
            .with_hint("pipe it to `mongodb-cli atlas db-users create ... --password-stdin`"));
    };
    if !(1..=100).contains(&password.chars().count()) {
        return Err(AppError::validation(
            "the password must have 1 to 100 characters",
        ));
    }
    Ok(SecretString::from(password))
}

/// Read exactly `count` non-empty lines (trailing empty lines allowed) of at most 64 KiB.
fn read_lines(reader: &mut impl Read, count: usize, label: &str) -> Result<Vec<String>, AppError> {
    const MAX_BYTES: u64 = 64 * 1024;
    let mut text = String::new();
    reader
        .take(MAX_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|error| {
            AppError::validation(format!("cannot read {label} from stdin: {error}"))
        })?;
    if text.len() as u64 > MAX_BYTES {
        return Err(AppError::validation(format!(
            "{label} on stdin exceeds {MAX_BYTES} bytes"
        )));
    }
    let mut lines: Vec<&str> = text.lines().collect();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    if lines.len() != count
        || lines
            .iter()
            .any(|line| line.is_empty() || line.contains('\0'))
    {
        return Err(AppError::validation(format!(
            "stdin must contain {label} as exactly {count} non-empty line(s)"
        )));
    }
    Ok(lines.into_iter().map(str::to_owned).collect())
}

/// `auth login --atlas`: verify service-account credentials and store them in the keyring.
pub async fn login(
    cli: &Cli,
    no_verify: bool,
    credentials_stdin: bool,
    config: &mut ConfigFile,
    path: &Path,
    store: &dyn CredentialStore,
) -> Result<DispatchResult, AppError> {
    let interactive = crate::onboarding::interactive_terminal();
    let profile =
        crate::onboarding::select_login_profile(config, cli.profile.as_deref(), interactive)?;
    let access = config
        .profiles
        .get(&profile)
        .ok_or_else(|| AppError::config(format!("profile '{profile}' does not exist")))?
        .access;
    let (client_id, client_secret) = if credentials_stdin {
        if io::stdin().is_terminal() {
            return Err(AppError::validation(
                "--credentials-stdin expects a pipe or redirected standard input",
            )
            .with_hint("pipe the client ID and the client secret as two lines"));
        }
        let mut lines = read_lines(&mut io::stdin(), 2, "the client ID and secret")?;
        let secret = lines.pop().expect("two lines");
        (lines.pop().expect("two lines"), secret)
    } else {
        if !interactive {
            return Err(AppError::validation("interactive Atlas credential entry requires an interactive terminal")
                .with_hint("pipe the client ID and secret as two lines to `mongodb-cli auth login --atlas --credentials-stdin`"));
        }
        let prompt = format!("Atlas service-account client ID for profile '{profile}'");
        let id = crate::onboarding::prompt_io(cliclack::input(prompt).interact())?;
        let secret = crate::onboarding::prompt_io(
            cliclack::password("Atlas service-account client secret").interact(),
        )?;
        (id, secret)
    };
    if client_id.contains(':') || client_id.chars().any(char::is_whitespace) {
        return Err(AppError::validation(
            "the client ID must not contain ':' or whitespace",
        ));
    }
    let stored = SecretString::from(
        json!({"clientId": client_id, "clientSecret": client_secret}).to_string(),
    );
    let credentials = AtlasCredentials {
        client_id: SecretString::from(client_id),
        client_secret: SecretString::from(client_secret),
    };
    let client = AtlasClient::new(&base_url(), credentials, Duration::from_secs(cli.timeout))?;
    let mut meta = Meta {
        profile: Some(profile.clone()),
        access: Some(access),
        ..Meta::default()
    };
    if client.insecure() {
        meta.warnings.push(INSECURE_WARNING.into());
    }
    if no_verify {
        meta.warnings.push("Atlas credentials were stored without verification because --no-verify was explicitly set".into());
    } else {
        let started = Instant::now();
        let verified = client.verify().await;
        client.revoke().await;
        verified?;
        meta.elapsed_ms = Some(started.elapsed().as_millis());
    }
    crate::onboarding::persist_credential(
        config,
        path,
        store,
        &atlas_account(&profile),
        &stored,
        |config| {
            config
                .profiles
                .get_mut(&profile)
                .ok_or_else(|| format!("profile '{profile}' does not exist"))?
                .atlas = true;
            Ok(())
        },
    )?;
    Ok(result(
        "auth.login",
        json!({"profile": profile, "stored": true, "verified": !no_verify, "atlas": true}),
        meta,
    ))
}

/// `auth logout --atlas`: remove a profile's Atlas credentials.
pub fn logout(
    cli: &Cli,
    config: &mut ConfigFile,
    path: &Path,
    store: &dyn CredentialStore,
) -> Result<DispatchResult, AppError> {
    let profile = cli
        .profile
        .clone()
        .or_else(|| config.current_profile.clone())
        .ok_or_else(|| AppError::config("select a profile before auth logout"))?;
    let saved = config
        .profiles
        .get_mut(&profile)
        .ok_or_else(|| AppError::config(format!("profile '{profile}' does not exist")))?;
    store
        .delete(&atlas_account(&profile))
        .map_err(AppError::config)?;
    saved.atlas = false;
    config.save(path).map_err(AppError::config)?;
    Ok(result(
        "auth.logout",
        json!({"profile": profile, "removed": true, "atlas": true}),
        Meta::default(),
    ))
}

/// Append the `atlas-credentials`, `atlas-token`, and `atlas-api` checks to a finished doctor
/// report. `enabled` is the Atlas tools switch; when it is off the report is returned unchanged.
pub async fn doctor(cli: &Cli, mut report: DispatchResult, enabled: bool) -> DispatchResult {
    if !enabled {
        return report;
    }
    let checks = atlas_checks(cli, &mut report).await;
    if let Some(entries) = report.data.as_array_mut() {
        entries.extend(checks);
    }
    report
}

async fn atlas_checks(cli: &Cli, report: &mut DispatchResult) -> Vec<Value> {
    let skipped = |message: &str| {
        ["atlas-credentials", "atlas-token", "atlas-api"]
            .into_iter()
            .map(|name| doctor::check(name, DoctorStatus::Skipped, message))
            .collect()
    };
    let Ok((config, path)) = crate::dispatch::config_path().and_then(|path| {
        ConfigFile::load(&path)
            .map(|config| (config, path))
            .map_err(AppError::config)
    }) else {
        return skipped("not checked because configuration failed");
    };
    let mut target = match resolve_target(&config, cli.profile.as_deref()) {
        Ok(target) => target,
        // A missing --profile already failed the configuration check.
        Err(_) if cli.profile.is_some() => {
            return skipped("not checked because configuration failed");
        }
        Err(_)
            if environment(CLIENT_ID_ENV).is_none() && environment(CLIENT_SECRET_ENV).is_none() =>
        {
            return skipped("Atlas credentials are not configured");
        }
        Err(_) => {
            report.failure_code.get_or_insert(ErrorCode::Config);
            let mut checks: Vec<Value> =
                skipped("not checked because the Atlas credentials failed");
            checks[0] = doctor::check(
                "atlas-credentials",
                DoctorStatus::Fail,
                "the Atlas credential environment variables are incomplete or invalid",
            );
            return checks;
        }
    };
    // Profiles that never stored Atlas credentials are not configured for Atlas.
    if let Some(name) = &target.profile
        && !config
            .profiles
            .get(name)
            .is_some_and(|profile| profile.atlas)
    {
        return skipped("Atlas credentials are not configured");
    }
    let source = match &target.profile {
        Some(name) => format!("profile '{name}'"),
        None => "the environment".to_owned(),
    };
    let atlas_only = mongodb_unconfigured(&config, cli.profile.as_deref());
    report.meta.access.get_or_insert(target.access);
    report.meta.warnings.extend(target.warnings.iter().cloned());
    let client = match crate::dispatch::credential_store_for(&config, &path)
        .and_then(|store| target.client(&store, cli.timeout))
    {
        Ok(client) => client,
        Err(_) => {
            report.failure_code.get_or_insert(ErrorCode::Config);
            let mut checks: Vec<Value> =
                skipped("not checked because the Atlas credentials failed");
            checks[0] = doctor::check(
                "atlas-credentials",
                DoctorStatus::Fail,
                format!("the Atlas credentials of {source} are missing or unavailable"),
            );
            return checks;
        }
    };
    if atlas_only && report.failure_code == Some(ErrorCode::Config) {
        skip_mongodb_checks(report);
    }
    if client.insecure() {
        report.meta.warnings.push(INSECURE_WARNING.into());
    }
    let mut checks = vec![doctor::check(
        "atlas-credentials",
        DoctorStatus::Pass,
        format!("Atlas service-account credentials are available from {source}"),
    )];
    let timeout = Duration::from_secs(cli.timeout);
    let token = tokio::time::timeout(timeout, client.verify())
        .await
        .unwrap_or_else(|_| {
            Err(ClientError::new(
                ErrorCode::Timeout,
                "Atlas did not respond before the timeout",
            ))
        });
    match token {
        Ok(()) => {
            checks.push(doctor::check(
                "atlas-token",
                DoctorStatus::Pass,
                "Atlas issued an access token",
            ));
            let query = [
                ("itemsPerPage", "1".to_owned()),
                ("includeCount", "false".to_owned()),
            ];
            let probe = client.get("/orgs", &query);
            match tokio::time::timeout(timeout, probe).await {
                Ok(Ok(_)) => checks.push(doctor::check(
                    "atlas-api",
                    DoctorStatus::Pass,
                    "the Atlas Administration API accepted a request",
                )),
                Ok(Err(error)) => {
                    report.failure_code.get_or_insert(error.code());
                    checks.push(doctor::check(
                        "atlas-api",
                        DoctorStatus::Fail,
                        api_failure_message(error.code()),
                    ));
                }
                Err(_) => {
                    report.failure_code.get_or_insert(ErrorCode::Timeout);
                    checks.push(doctor::check(
                        "atlas-api",
                        DoctorStatus::Fail,
                        "Atlas did not respond before the timeout",
                    ));
                }
            }
        }
        Err(error) => {
            report.failure_code.get_or_insert(error.code());
            checks.push(doctor::check(
                "atlas-token",
                DoctorStatus::Fail,
                token_failure_message(error.code()),
            ));
            checks.push(doctor::check(
                "atlas-api",
                DoctorStatus::Skipped,
                "not checked because no Atlas token was issued",
            ));
        }
    }
    client.revoke().await;
    checks
}

fn token_failure_message(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::Unauthorized => "Atlas rejected the service-account credentials",
        ErrorCode::Network => "Atlas is unreachable",
        ErrorCode::Timeout => "Atlas did not respond before the timeout",
        _ => "Atlas could not issue an access token",
    }
}

fn api_failure_message(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::Forbidden => {
            "Atlas refused the request; check the service account's roles and API access list"
        }
        ErrorCode::Unauthorized => "Atlas rejected the access token",
        ErrorCode::Network => "Atlas is unreachable",
        _ => "the Atlas Administration API failed the diagnostic request",
    }
}

/// Whether no MongoDB connection string is configured for the doctor's MongoDB checks.
fn mongodb_unconfigured(config: &ConfigFile, selected: Option<&str>) -> bool {
    if selected.is_none() && environment(crate::config::URI_ENV).is_some() {
        return false;
    }
    match selected.or(config.current_profile.as_deref()) {
        Some(name) => config
            .profiles
            .get(name)
            .is_some_and(|profile| profile.target.is_none()),
        None => true,
    }
}

/// A target with only Atlas credentials skips the MongoDB checks instead of failing them.
fn skip_mongodb_checks(report: &mut DispatchResult) {
    for check in report.data.as_array_mut().into_iter().flatten() {
        let (status, message) = match check["name"].as_str() {
            Some("configuration") if check["status"] == json!(DoctorStatus::Fail) => (
                DoctorStatus::Pass,
                "no MongoDB connection is configured; Atlas credentials are",
            ),
            Some("credentials" | "server" | "authentication") => (
                DoctorStatus::Skipped,
                "no MongoDB connection string is configured",
            ),
            _ => continue,
        };
        check["status"] = json!(status);
        check["message"] = json!(message);
    }
    report.failure_code = None;
}

fn result(command: &str, data: Value, meta: Meta) -> DispatchResult {
    DispatchResult {
        command: command.to_owned(),
        data,
        meta,
        failure_code: None,
        emitted: false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use clap::Parser;
    use secrecy::ExposeSecret;

    use super::*;

    #[derive(Default)]
    struct MemoryStore(Mutex<HashMap<String, String>>);

    impl CredentialStore for MemoryStore {
        fn get(&self, name: &str) -> Result<Option<String>, String> {
            Ok(self.0.lock().unwrap().get(name).cloned())
        }
        fn set(&self, name: &str, value: &str) -> Result<(), String> {
            self.0.lock().unwrap().insert(name.into(), value.into());
            Ok(())
        }
        fn delete(&self, name: &str) -> Result<(), String> {
            self.0.lock().unwrap().remove(name);
            Ok(())
        }
    }

    fn parse(arguments: &[&str]) -> Cli {
        Cli::try_parse_from(
            ["mongodb-cli", "atlas"]
                .into_iter()
                .chain(arguments.iter().copied()),
        )
        .unwrap_or_else(|error| panic!("{arguments:?}: {error}"))
    }

    fn command(arguments: &[&str]) -> AtlasCommand {
        match parse(arguments).command {
            crate::args::Command::Atlas { command } => command,
            _ => unreachable!(),
        }
    }

    /// The checks `run` makes before reading credentials, for the profile `admin`.
    fn authorized(arguments: &[&str], access: Access, settings: Settings) -> Result<(), AppError> {
        let cli = parse(arguments);
        let crate::args::Command::Atlas { command } = &cli.command else {
            unreachable!()
        };
        let target = Target {
            profile: Some("admin".into()),
            access,
            settings,
            warnings: Vec::new(),
            environment: None,
        };
        authorize(command, &target, cli.yes)
    }

    const PROJECT: &str = "65f1a2b3c4d5e6f708192a3b";

    #[test]
    fn changes_need_read_write_access_and_the_global_yes() {
        let mut tools = std::collections::BTreeSet::new();
        for arguments in [
            vec!["orgs", "list"],
            vec!["projects", "list"],
            vec!["clusters", "list"],
            vec!["clusters", "inspect", "c0", "--project-id", PROJECT],
            vec!["clusters", "advisor", "c0", "--project-id", PROJECT],
            vec!["access-list", "list", "--project-id", PROJECT],
            vec!["db-users", "list", "--project-id", PROJECT],
            vec!["alerts", "list", "--project-id", PROJECT],
            vec!["regions", "list", "--provider", "aws"],
            vec!["sample-data", "status", PROJECT, "--project-id", PROJECT],
            vec!["streams", "workspaces", "list", "--project-id", PROJECT],
            vec![
                "streams",
                "processors",
                "diagnose",
                "w",
                "p",
                "--project-id",
                PROJECT,
            ],
            vec!["streams", "networking", "show", "--project-id", PROJECT],
        ] {
            let (name, risk) = command(&arguments).describe();
            assert!(risk.is_none(), "{arguments:?}");
            let (tool, operation_type) = mcp_tool(name);
            assert_eq!(operation_type, "read", "{name}");
            tools.insert(tool);
            assert!(
                authorized(&arguments, Access::ReadOnly, Settings::default()).is_ok(),
                "{name}"
            );
        }
        for arguments in [
            vec!["projects", "create", "Shop", "--org-id", PROJECT],
            vec![
                "clusters",
                "create",
                "c0",
                "--project-id",
                PROJECT,
                "--provider",
                "aws",
                "--region",
                "US_EAST_1",
            ],
            vec!["clusters", "create-free", "c0", "--project-id", PROJECT],
            vec!["clusters", "pause", "c0", "--project-id", PROJECT],
            vec!["clusters", "resume", "c0", "--project-id", PROJECT],
            vec!["clusters", "upgrade", "c0", "--project-id", PROJECT],
            vec![
                "clusters",
                "connect",
                "c0",
                "--project-id",
                PROJECT,
                "--save-profile",
                "shop",
            ],
            vec!["clusters", "disconnect", "shop"],
            vec![
                "access-list",
                "add",
                "--project-id",
                PROJECT,
                "--current-ip",
            ],
            vec![
                "db-users",
                "create",
                "app",
                "--project-id",
                PROJECT,
                "--role",
                "read@shop",
            ],
            vec!["sample-data", "load", "c0", "--project-id", PROJECT],
            vec![
                "streams",
                "workspaces",
                "create",
                "w",
                "--project-id",
                PROJECT,
                "--cloud-provider",
                "aws",
                "--region",
                "VIRGINIA_USA",
            ],
            vec![
                "streams",
                "workspaces",
                "update",
                "w",
                "--project-id",
                PROJECT,
                "--tier",
                "sp30",
            ],
            vec![
                "streams",
                "workspaces",
                "delete",
                "w",
                "--project-id",
                PROJECT,
            ],
            vec![
                "streams",
                "connections",
                "create",
                "w",
                "c",
                "--project-id",
                PROJECT,
                "--type",
                "sample",
            ],
            vec![
                "streams",
                "connections",
                "update",
                "w",
                "c",
                "--project-id",
                PROJECT,
                "--config-file",
                "-",
            ],
            vec![
                "streams",
                "connections",
                "delete",
                "w",
                "c",
                "--project-id",
                PROJECT,
            ],
            vec![
                "streams",
                "processors",
                "create",
                "w",
                "p",
                "--project-id",
                PROJECT,
                "--pipeline",
                "[]",
            ],
            vec![
                "streams",
                "processors",
                "start",
                "w",
                "p",
                "--project-id",
                PROJECT,
            ],
            vec![
                "streams",
                "processors",
                "stop",
                "w",
                "p",
                "--project-id",
                PROJECT,
            ],
            vec![
                "streams",
                "processors",
                "modify",
                "w",
                "p",
                "--project-id",
                PROJECT,
                "--new-name",
                "q",
            ],
            vec![
                "streams",
                "processors",
                "delete",
                "w",
                "p",
                "--project-id",
                PROJECT,
            ],
            vec![
                "streams",
                "privatelink",
                "create",
                "--project-id",
                PROJECT,
                "--config-file",
                "-",
            ],
            vec![
                "streams",
                "privatelink",
                "delete",
                "pl1",
                "--project-id",
                PROJECT,
            ],
            vec![
                "streams",
                "peering",
                "accept",
                "pc1",
                "--project-id",
                PROJECT,
                "--requester-account-id",
                "1",
                "--requester-vpc-id",
                "v",
            ],
            vec![
                "streams",
                "peering",
                "reject",
                "pc1",
                "--project-id",
                PROJECT,
            ],
            vec![
                "streams",
                "peering",
                "delete",
                "pc1",
                "--project-id",
                PROJECT,
            ],
        ] {
            let (name, risk) = command(&arguments).describe();
            assert!(risk.is_some(), "{arguments:?} changes nothing");
            assert!(name.starts_with("atlas."), "{name}");
            let (tool, operation_type) = mcp_tool(name);
            assert_ne!(operation_type, "read", "{name}");
            tools.insert(tool);
            let error = authorized(&arguments, Access::ReadWrite, Settings::default()).unwrap_err();
            assert_eq!(error.code, ErrorCode::Validation, "{name}");
            assert!(error.hint.unwrap().contains("--yes"), "{name}");
            let error = authorized(&arguments, Access::ReadOnly, Settings::default()).unwrap_err();
            assert_eq!(error.code, ErrorCode::Forbidden, "{name}");
            let confirmed = [&arguments[..], &["--yes"]].concat();
            assert!(
                authorized(&confirmed, Access::ReadWrite, Settings::default()).is_ok(),
                "{name}"
            );
        }
        // Every MCP Atlas tool is covered; disconnect is the CLI's own cleanup command.
        tools.remove("atlas.clusters.disconnect");
        assert_eq!(tools.len(), 22, "{tools:?}");
        let (_, risk) = command(&[
            "streams",
            "processors",
            "start",
            "w",
            "p",
            "--project-id",
            PROJECT,
            "--no-resume-from-checkpoint",
        ])
        .describe();
        assert!(risk.unwrap().contains("permanently lost"));
    }

    #[test]
    fn safety_settings_select_atlas_commands_by_tool_name_type_or_category() {
        let orgs = ["orgs", "list"];
        let disabling = |entry: &str| Settings {
            disabled_tools: vec![entry.into()],
            ..Settings::default()
        };
        for entry in ["atlas-list-orgs", "atlas.orgs.list", "read", "atlas"] {
            let error = authorized(&orgs, Access::ReadWrite, disabling(entry)).unwrap_err();
            assert_eq!(error.code, ErrorCode::Forbidden, "{entry}");
            assert!(error.message.contains(entry), "{entry}: {}", error.message);
            assert!(!error.hint.unwrap().contains("--profile"));
        }
        for entry in [
            "mongodb",
            "list-databases",
            "create",
            "atlas-list-projects",
            "atlas.orgs",
        ] {
            assert!(
                authorized(&orgs, Access::ReadWrite, disabling(entry)).is_ok(),
                "{entry}"
            );
        }
        // Disabled tools are refused before the read-only check.
        let pause = ["clusters", "pause", "c0", "--project-id", PROJECT, "--yes"];
        let error = authorized(&pause, Access::ReadOnly, disabling("update")).unwrap_err();
        assert!(error.message.contains("is disabled"), "{}", error.message);

        let requiring = |entry: &str| Settings {
            confirmation_required_tools: vec![entry.into()],
            ..Settings::default()
        };
        for entry in ["atlas-list-orgs", "atlas.orgs.list", "read", "atlas"] {
            let error = authorized(&orgs, Access::ReadOnly, requiring(entry)).unwrap_err();
            assert_eq!(error.code, ErrorCode::Validation, "{entry}");
            assert!(error.message.contains(entry), "{entry}: {}", error.message);
            let confirmed = ["orgs", "list", "--yes"];
            assert!(authorized(&confirmed, Access::ReadOnly, requiring(entry)).is_ok());
        }
        assert!(authorized(&orgs, Access::ReadOnly, requiring("create")).is_ok());

        for (arguments, tool, operation_type) in [
            (
                vec!["clusters", "resume", "c0", "--project-id", PROJECT],
                "atlas-pause-resume-cluster",
                "update",
            ),
            (
                vec!["sample-data", "status", PROJECT, "--project-id", PROJECT],
                "atlas-load-sample-dataset",
                "read",
            ),
            (
                vec![
                    "clusters",
                    "connect",
                    "c0",
                    "--project-id",
                    PROJECT,
                    "--save-profile",
                    "s",
                ],
                "atlas-connect-cluster",
                "connect",
            ),
            (
                vec![
                    "streams",
                    "workspaces",
                    "update",
                    "w",
                    "--project-id",
                    PROJECT,
                ],
                "atlas-streams-manage",
                "update",
            ),
            (
                vec![
                    "streams",
                    "peering",
                    "delete",
                    "pc1",
                    "--project-id",
                    PROJECT,
                ],
                "atlas-streams-teardown",
                "delete",
            ),
            (
                vec![
                    "streams",
                    "privatelink",
                    "create",
                    "--project-id",
                    PROJECT,
                    "--config-file",
                    "-",
                ],
                "atlas-streams-build",
                "create",
            ),
        ] {
            let (name, _) = command(&arguments).describe();
            assert_eq!(mcp_tool(name), (tool, operation_type), "{name}");
        }
    }

    #[test]
    fn invalid_values_are_rejected_while_parsing() {
        for arguments in [
            vec!["clusters", "inspect", "c0", "--project-id", "nothex"],
            vec!["clusters", "inspect", "bad.name", "--project-id", PROJECT],
            vec![
                "access-list",
                "add",
                "--project-id",
                PROJECT,
                "--ip",
                "300.1.1.1",
            ],
            vec![
                "access-list",
                "add",
                "--project-id",
                PROJECT,
                "--cidr",
                "10.0.0.0/40",
            ],
            vec![
                "db-users",
                "create",
                "app",
                "--project-id",
                PROJECT,
                "--role",
                "read@",
            ],
            vec!["db-users", "create", "app", "--project-id", PROJECT],
            vec![
                "clusters",
                "advisor",
                "c0",
                "--project-id",
                PROJECT,
                "--since",
                "yesterday",
            ],
            vec![
                "clusters",
                "connect",
                "c0",
                "--project-id",
                PROJECT,
                "--save-profile",
                "s",
                "--user-lifetime-hours",
                "169",
            ],
            vec![
                "streams",
                "networking",
                "show",
                "--project-id",
                PROJECT,
                "--region",
                "VIRGINIA_USA",
            ],
            vec![
                "streams",
                "processors",
                "create",
                "w",
                "p",
                "--project-id",
                PROJECT,
                "--pipeline",
                "[]",
                "--dlq-db",
                "d",
            ],
        ] {
            let parsed = Cli::try_parse_from(
                ["mongodb-cli", "atlas"]
                    .into_iter()
                    .chain(arguments.iter().copied()),
            );
            assert!(parsed.is_err(), "{arguments:?} parsed");
        }
    }

    #[test]
    fn inputs_are_checked_before_credentials() {
        let config = ConfigFile::default();
        for (arguments, expected) in [
            (
                vec!["access-list", "add", "--project-id", PROJECT],
                "One of --ip",
            ),
            (
                vec![
                    "clusters",
                    "create",
                    "c0",
                    "--project-id",
                    PROJECT,
                    "--provider",
                    "aws",
                    "--region",
                    "A",
                    "--type",
                    "sharded",
                    "--instance-size",
                    "m10",
                ],
                "M30 or higher",
            ),
            (
                vec![
                    "streams",
                    "connections",
                    "create",
                    "w",
                    "c",
                    "--project-id",
                    PROJECT,
                    "--type",
                    "kafka",
                ],
                "Bootstrap Servers",
            ),
            (
                vec![
                    "streams",
                    "processors",
                    "create",
                    "w",
                    "p",
                    "--project-id",
                    PROJECT,
                    "--pipeline",
                    r#"[{"$match": {}}]"#,
                ],
                "first stage must be `$source`",
            ),
            (
                vec![
                    "streams",
                    "processors",
                    "create",
                    "w",
                    "p",
                    "--project-id",
                    PROJECT,
                    "--pipeline",
                    "{}",
                ],
                "expected a JSON array",
            ),
            (
                vec![
                    "streams",
                    "processors",
                    "modify",
                    "w",
                    "p",
                    "--project-id",
                    PROJECT,
                ],
                "No modifications",
            ),
            (
                vec![
                    "streams",
                    "workspaces",
                    "update",
                    "w",
                    "--project-id",
                    PROJECT,
                ],
                "No updates",
            ),
            (
                vec![
                    "clusters",
                    "connect",
                    "c0",
                    "--project-id",
                    PROJECT,
                    "--save-profile",
                    "bad name",
                ],
                "profile names",
            ),
        ] {
            let error = prepare(&command(&arguments), &config)
                .err()
                .unwrap_or_else(|| panic!("{arguments:?} passed"));
            assert_eq!(error.code, ErrorCode::Validation, "{arguments:?}");
            assert!(
                error.message.contains(expected),
                "{arguments:?}: {}",
                error.message
            );
        }
        let error = prepare(&command(&["clusters", "disconnect", "missing"]), &config)
            .err()
            .unwrap();
        assert_eq!(error.code, ErrorCode::Config);
    }

    #[test]
    fn credentials_resolve_from_profile_then_environment_then_default() {
        let store = MemoryStore::default();
        let mut config = ConfigFile::default();
        config.add_profile("admin", Access::ReadWrite).unwrap();
        config.add_profile("viewer", Access::ReadOnly).unwrap();
        config.use_profile("viewer").unwrap();
        store
            .set(
                "admin/atlas",
                r#"{"clientId":"id-1","clientSecret":"secret-1"}"#,
            )
            .unwrap();
        let env = |id: Option<&str>, secret: Option<&str>| {
            (id.map(str::to_owned), secret.map(str::to_owned))
        };

        let (id, secret) = env(Some("env-id"), Some("env-secret"));
        let target = target_from(&config, Some("admin"), id, secret, Some("read-write")).unwrap();
        assert_eq!(
            (target.profile.as_deref(), target.access),
            (Some("admin"), Access::ReadWrite)
        );
        assert!(target.warnings[0].contains("ignored"));
        assert!(target.environment.is_none());
        let credentials = stored_credentials("admin", &store).unwrap();
        assert_eq!(credentials.client_secret.expose_secret(), "secret-1");
        assert!(!format!("{credentials:?}").contains("secret-1"));

        let (id, secret) = env(Some("env-id"), Some("env-secret"));
        let target = target_from(&config, None, id, secret, None).unwrap();
        assert_eq!((target.profile, target.access), (None, Access::ReadOnly));
        let (id, secret) = env(Some("env-id"), Some("env-secret"));
        assert_eq!(
            target_from(&config, None, id, secret, Some("read-write"))
                .unwrap()
                .access,
            Access::ReadWrite
        );
        let (id, secret) = env(Some("env-id"), Some("env-secret"));
        assert!(target_from(&config, None, id, secret, Some("yes")).is_err());
        let (id, secret) = env(Some("env-id"), None);
        assert!(
            target_from(&config, None, id, secret, None)
                .unwrap_err()
                .contains("both")
        );

        let target = target_from(&config, None, None, None, None).unwrap();
        assert_eq!(target.profile.as_deref(), Some("viewer"));
        let error = stored_credentials("viewer", &store).unwrap_err();
        assert!(error.contains("auth login --atlas"), "{error}");
        assert!(
            target_from(&ConfigFile::default(), None, None, None, None)
                .unwrap_err()
                .contains("auth login --atlas")
        );
        store.set("viewer/atlas", "not json").unwrap();
        assert!(
            stored_credentials("viewer", &store)
                .unwrap_err()
                .contains("unreadable")
        );
    }

    #[test]
    fn piped_lines_must_match_the_expected_count() {
        let read = |text: &str, count| {
            read_lines(
                &mut io::Cursor::new(text.as_bytes().to_vec()),
                count,
                "value",
            )
        };
        assert_eq!(read("id\nsecret\n", 2).unwrap(), ["id", "secret"]);
        assert_eq!(read("id\r\nsecret\r\n\n", 2).unwrap(), ["id", "secret"]);
        for text in ["", "only\n", "a\n\nb\n", "a\nb\nc\n"] {
            assert!(read(text, 2).is_err(), "{text:?}");
        }
        assert!(read(&"x".repeat(64 * 1024 + 1), 1).is_err());
    }

    #[test]
    fn autoscaling_flags_build_the_atlas_object() {
        let parsed = command(&[
            "streams",
            "processors",
            "start",
            "w",
            "p",
            "--project-id",
            PROJECT,
            "--autoscaling",
            "on",
            "--max-tier",
            "default",
            "--min-tier",
            "sp5",
        ]);
        let AtlasCommand::Streams {
            command:
                StreamsCommand::Processors {
                    command: ProcessorsCommand::Start { autoscaling, .. },
                },
        } = parsed
        else {
            unreachable!()
        };
        assert_eq!(
            autoscaling_option(&autoscaling),
            Some(json!({"enabled": true, "minTier": "SP5", "maxTier": null}))
        );
        assert_eq!(
            autoscaling_option(&Autoscaling {
                autoscaling: None,
                min_tier: None,
                max_tier: None
            }),
            None
        );
    }

    #[test]
    fn doctor_skips_mongodb_checks_only_without_a_connection_string() {
        let mut config = ConfigFile::default();
        config.add_profile("atlas-only", Access::ReadOnly).unwrap();
        config.add_profile("db", Access::ReadOnly).unwrap();
        config.set_target("db", "mongodb://db.example.com").unwrap();
        assert!(mongodb_unconfigured(&config, Some("atlas-only")));
        assert!(!mongodb_unconfigured(&config, Some("db")));
        assert!(!mongodb_unconfigured(&config, Some("missing")));
        let mut report = DispatchResult {
            command: "doctor".into(),
            data: json!([
                {"name": "version", "status": "pass", "message": "v"},
                {"name": "configuration", "status": "pass", "message": "ok"},
                {"name": "credentials", "status": "fail", "message": "missing"},
                {"name": "server", "status": "skipped", "message": "x"},
                {"name": "authentication", "status": "skipped", "message": "x"}
            ]),
            meta: Meta::default(),
            failure_code: Some(ErrorCode::Config),
            emitted: false,
        };
        skip_mongodb_checks(&mut report);
        let statuses: Vec<_> = report
            .data
            .as_array()
            .unwrap()
            .iter()
            .map(|check| check["status"].as_str().unwrap())
            .collect();
        assert_eq!(statuses, ["pass", "pass", "skipped", "skipped", "skipped"]);
        assert_eq!(report.failure_code, None);
    }
}
