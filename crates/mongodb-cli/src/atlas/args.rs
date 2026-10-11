//! The `atlas` command tree. Inputs are validated while parsing, before any credential or
//! network access; enum values are kebab-case here and Atlas spellings on the wire.

use std::net::Ipv4Addr;
use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};
use mongodb_client::atlas::admin::{self, DbRole};

use crate::args::{Access, PipelineArgs};

#[derive(Clone, Debug, Subcommand)]
pub enum AtlasCommand {
    /// List Atlas organizations.
    Orgs {
        #[command(subcommand)]
        command: OrgsCommand,
    },
    /// List and create Atlas projects.
    Projects {
        #[command(subcommand)]
        command: ProjectsCommand,
    },
    /// Manage Atlas clusters.
    Clusters {
        #[command(subcommand)]
        command: ClustersCommand,
    },
    /// Manage a project's IP access list.
    AccessList {
        #[command(subcommand)]
        command: AccessListCommand,
    },
    /// Manage Atlas database users.
    DbUsers {
        #[command(subcommand)]
        command: DbUsersCommand,
    },
    /// List triggered Atlas alerts.
    Alerts {
        #[command(subcommand)]
        command: AlertsCommand,
    },
    /// List Atlas cloud regions.
    Regions {
        #[command(subcommand)]
        command: RegionsCommand,
    },
    /// Load Atlas sample datasets.
    SampleData {
        #[command(subcommand)]
        command: SampleDataCommand,
    },
    /// Manage Atlas Stream Processing.
    Streams {
        #[command(subcommand)]
        command: StreamsCommand,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum OrgsCommand {
    /// List organizations.
    List(ListArgs),
}

#[derive(Clone, Debug, Subcommand)]
pub enum ProjectsCommand {
    /// List projects.
    List {
        /// Only list projects of this organization.
        #[arg(long, value_parser = object_id)]
        org_id: Option<String>,
        #[command(flatten)]
        list: ListArgs,
    },
    /// Create a project.
    Create {
        #[arg(value_parser = project_name)]
        name: String,
        /// Select the organization ID.
        #[arg(long, value_parser = object_id)]
        org_id: String,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum ClustersCommand {
    /// List clusters.
    List {
        /// Only list clusters of this project.
        #[arg(long, value_parser = object_id)]
        project_id: Option<String>,
    },
    /// Show a cluster.
    Inspect {
        #[arg(value_parser = name64)]
        cluster: String,
        #[command(flatten)]
        project: Project,
    },
    /// Create a dedicated cluster.
    Create {
        #[arg(value_parser = name64)]
        cluster: String,
        #[command(flatten)]
        project: Project,
        /// Select the cloud provider.
        #[arg(long, value_enum)]
        provider: Provider,
        /// Add a region; the first has priority.
        #[arg(long = "region", required = true, value_parser = region)]
        regions: Vec<String>,
        /// Select the cluster topology.
        #[arg(long = "type", value_enum, default_value = "replicaset")]
        cluster_type: ClusterType,
        /// Select the instance size.
        #[arg(long, value_enum)]
        instance_size: Option<InstanceSize>,
        /// Turn compute autoscaling on or off.
        #[arg(long, value_enum, default_value = "on")]
        compute_autoscaling: Switch,
        /// Set the initial disk size in GB.
        #[arg(long)]
        disk_size_gb: Option<f64>,
        /// Select the MongoDB version.
        #[arg(long, value_enum, default_value = "latest")]
        mongodb_version: MongoVersion,
        /// Select the backup mode.
        #[arg(long, value_enum, default_value = "snapshot")]
        backup: Backup,
        /// Prevent deleting the cluster.
        #[arg(long)]
        termination_protection: bool,
        /// Select the encryption-at-rest key provider.
        #[arg(long, value_enum)]
        encryption_at_rest_provider: Option<EncryptionProvider>,
    },
    /// Create a free M0 cluster.
    CreateFree {
        #[arg(value_parser = name64)]
        cluster: String,
        #[command(flatten)]
        project: Project,
        /// Select the AWS region.
        #[arg(long, default_value = "US_EAST_1", value_parser = region)]
        region: String,
    },
    /// Pause a dedicated cluster.
    Pause {
        #[arg(value_parser = name64)]
        cluster: String,
        #[command(flatten)]
        project: Project,
    },
    /// Resume a paused cluster.
    Resume {
        #[arg(value_parser = name64)]
        cluster: String,
        #[command(flatten)]
        project: Project,
    },
    /// Upgrade or scale a cluster.
    Upgrade {
        #[arg(value_parser = name64)]
        cluster: String,
        #[command(flatten)]
        project: Project,
        /// Select the target tier.
        #[arg(long, value_enum)]
        target_tier: Option<TargetTier>,
        /// Turn compute autoscaling on or off.
        #[arg(long, value_enum)]
        compute_autoscaling: Option<Switch>,
        /// Set the smallest autoscaling size.
        #[arg(long, value_enum)]
        min_instance_size: Option<InstanceSize>,
        /// Set the largest autoscaling size.
        #[arg(long, value_enum)]
        max_instance_size: Option<MaxInstanceSize>,
        /// Set the provider of a Free or Flex cluster.
        #[arg(long, value_parser = provider_name)]
        provider: Option<String>,
        /// Set the region of a Free or Flex cluster.
        #[arg(long, value_parser = region)]
        region: Option<String>,
    },
    /// Show Performance Advisor suggestions.
    Advisor {
        #[arg(value_parser = name64)]
        cluster: String,
        #[command(flatten)]
        project: Project,
        /// Select an operation; repeat for several.
        #[arg(long = "operation", value_enum)]
        operations: Vec<AdvisorKind>,
        /// Only slow queries since this RFC 3339 time.
        #[arg(long, value_parser = since)]
        since: Option<i64>,
        /// Only slow queries of this namespace.
        #[arg(long = "namespace")]
        namespaces: Vec<String>,
    },
    /// Save a profile with a temporary database user.
    Connect {
        #[arg(value_parser = name64)]
        cluster: String,
        #[command(flatten)]
        project: Project,
        /// Name the profile to create.
        #[arg(long)]
        save_profile: String,
        /// Set the new profile's access mode.
        #[arg(long, value_enum, default_value_t = Access::ReadOnly)]
        access: Access,
        /// Select the connection string type.
        #[arg(long, value_enum, default_value = "standard")]
        connection_type: ConnectionType,
        /// Set the temporary user's lifetime in hours.
        #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u32).range(1..=168))]
        user_lifetime_hours: u32,
    },
    /// Delete a connect profile and its user.
    Disconnect {
        #[arg(value_name = "SAVED_PROFILE")]
        saved_profile: String,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum AccessListCommand {
    /// List access list entries.
    List {
        #[command(flatten)]
        project: Project,
    },
    /// Allow addresses to reach the clusters.
    Add {
        #[command(flatten)]
        project: Project,
        /// Allow an IPv4 address; repeat for several.
        #[arg(long = "ip", value_parser = ipv4)]
        addresses: Vec<String>,
        /// Allow an IPv4 CIDR block; repeat for several.
        #[arg(long = "cidr", value_parser = cidr)]
        cidr_blocks: Vec<String>,
        /// Allow this machine's public IP address.
        #[arg(long)]
        current_ip: bool,
        /// Comment the new entries.
        #[arg(long, value_parser = comment)]
        comment: Option<String>,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum DbUsersCommand {
    /// List database users.
    List {
        #[command(flatten)]
        project: Project,
    },
    /// Create a database user.
    Create {
        #[arg(value_parser = username)]
        username: String,
        #[command(flatten)]
        project: Project,
        /// Grant ROLE[@DB[.COLLECTION]]; repeatable.
        #[arg(long = "role", required = true, value_parser = role)]
        roles: Vec<DbRole>,
        /// Limit access to a cluster; repeatable.
        #[arg(long = "cluster", value_parser = name64)]
        clusters: Vec<String>,
        /// Read the password from stdin.
        #[arg(long)]
        password_stdin: bool,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum AlertsCommand {
    /// List alerts.
    List {
        #[command(flatten)]
        project: Project,
        /// Select the alert status.
        #[arg(long, value_enum, default_value = "open")]
        status: AlertStatus,
        #[command(flatten)]
        list: ListArgs,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum RegionsCommand {
    /// List the regions of a cloud provider.
    List {
        /// Select the cloud provider.
        #[arg(long, value_enum)]
        provider: Provider,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum SampleDataCommand {
    /// Load sample datasets into a cluster.
    Load {
        #[arg(value_parser = name64)]
        cluster: String,
        #[command(flatten)]
        project: Project,
    },
    /// Show a sample dataset load.
    Status {
        #[arg(value_parser = object_id)]
        job_id: String,
        #[command(flatten)]
        project: Project,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum StreamsCommand {
    /// Manage Stream Processing workspaces.
    Workspaces {
        #[command(subcommand)]
        command: WorkspacesCommand,
    },
    /// Manage Stream Processing connections.
    Connections {
        #[command(subcommand)]
        command: ConnectionsCommand,
    },
    /// Manage stream processors.
    Processors {
        #[command(subcommand)]
        command: ProcessorsCommand,
    },
    /// Show Stream Processing networking.
    Networking {
        #[command(subcommand)]
        command: NetworkingCommand,
    },
    /// Manage Stream Processing PrivateLink.
    Privatelink {
        #[command(subcommand)]
        command: PrivateLinkCommand,
    },
    /// Manage Stream Processing VPC peering.
    Peering {
        #[command(subcommand)]
        command: PeeringCommand,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum WorkspacesCommand {
    /// List workspaces.
    List {
        #[command(flatten)]
        project: Project,
        #[command(flatten)]
        list: StreamsList,
    },
    /// Show a workspace.
    Inspect {
        #[arg(value_parser = name64)]
        workspace: String,
        #[command(flatten)]
        project: Project,
        #[command(flatten)]
        detail: Detail,
    },
    /// Create a workspace.
    Create {
        #[arg(value_parser = name64)]
        workspace: String,
        #[command(flatten)]
        project: Project,
        /// Select the cloud provider.
        #[arg(long, value_enum)]
        cloud_provider: Provider,
        /// Select the Atlas streams region.
        #[arg(long, value_parser = region)]
        region: String,
        /// Select the processing tier.
        #[arg(long, value_enum)]
        tier: Option<Tier>,
        /// Skip the sample connection.
        #[arg(long)]
        no_sample_data: bool,
    },
    /// Change a workspace's region or tier.
    Update {
        #[arg(value_parser = name64)]
        workspace: String,
        #[command(flatten)]
        project: Project,
        /// Move the workspace to this region.
        #[arg(long, value_parser = region)]
        region: Option<String>,
        /// Set the workspace tier.
        #[arg(long, value_enum)]
        tier: Option<Tier>,
    },
    /// Delete a workspace.
    Delete {
        #[arg(value_parser = name64)]
        workspace: String,
        #[command(flatten)]
        project: Project,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum ConnectionsCommand {
    /// List connections.
    List {
        #[arg(value_parser = name64)]
        workspace: String,
        #[command(flatten)]
        project: Project,
        #[command(flatten)]
        list: StreamsList,
    },
    /// Show a connection.
    Inspect {
        #[arg(value_parser = name64)]
        workspace: String,
        #[arg(value_parser = name64)]
        connection: String,
        #[command(flatten)]
        project: Project,
        #[command(flatten)]
        detail: Detail,
    },
    /// Create a connection.
    Create {
        #[arg(value_parser = name64)]
        workspace: String,
        #[arg(value_parser = name64)]
        connection: String,
        #[command(flatten)]
        project: Project,
        /// Select the connection type.
        #[arg(long = "type", value_enum)]
        kind: ConnectionKind,
        /// Read the JSON configuration (- for stdin).
        #[arg(long, value_hint = clap::ValueHint::FilePath)]
        config_file: Option<PathBuf>,
    },
    /// Update a connection.
    Update {
        #[arg(value_parser = name64)]
        workspace: String,
        #[arg(value_parser = name64)]
        connection: String,
        #[command(flatten)]
        project: Project,
        /// Read the JSON configuration (- for stdin).
        #[arg(long, value_hint = clap::ValueHint::FilePath)]
        config_file: PathBuf,
    },
    /// Delete a connection.
    Delete {
        #[arg(value_parser = name64)]
        workspace: String,
        #[arg(value_parser = name64)]
        connection: String,
        #[command(flatten)]
        project: Project,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum ProcessorsCommand {
    /// List processors.
    List {
        #[arg(value_parser = name64)]
        workspace: String,
        #[command(flatten)]
        project: Project,
        #[command(flatten)]
        list: StreamsList,
    },
    /// Show a processor.
    Inspect {
        #[arg(value_parser = name64)]
        workspace: String,
        #[arg(value_parser = name64)]
        processor: String,
        #[command(flatten)]
        project: Project,
        #[command(flatten)]
        detail: Detail,
    },
    /// Diagnose a processor's health.
    Diagnose {
        #[arg(value_parser = name64)]
        workspace: String,
        #[arg(value_parser = name64)]
        processor: String,
        #[command(flatten)]
        project: Project,
    },
    /// Create a processor.
    Create {
        #[arg(value_parser = name64)]
        workspace: String,
        #[arg(value_parser = name64)]
        processor: String,
        #[command(flatten)]
        project: Project,
        #[command(flatten)]
        pipeline: PipelineArgs,
        #[command(flatten)]
        dlq: Dlq,
        /// Set the baseline processing tier.
        #[arg(long, value_enum)]
        tier: Option<Tier>,
        #[command(flatten)]
        autoscaling: Autoscaling,
        /// Start the processor after creating it.
        #[arg(long)]
        start: bool,
    },
    /// Start a processor.
    Start {
        #[arg(value_parser = name64)]
        workspace: String,
        #[arg(value_parser = name64)]
        processor: String,
        #[command(flatten)]
        project: Project,
        /// Set the processing tier of this run.
        #[arg(long, value_enum)]
        tier: Option<Tier>,
        #[command(flatten)]
        autoscaling: Autoscaling,
        /// Reprocess instead of resuming.
        #[arg(long)]
        no_resume_from_checkpoint: bool,
        /// Start at this ISO 8601 operation time.
        #[arg(long)]
        start_at_operation_time: Option<String>,
    },
    /// Stop a processor.
    Stop {
        #[arg(value_parser = name64)]
        workspace: String,
        #[arg(value_parser = name64)]
        processor: String,
        #[command(flatten)]
        project: Project,
    },
    /// Change a stopped processor.
    Modify {
        #[arg(value_parser = name64)]
        workspace: String,
        #[arg(value_parser = name64)]
        processor: String,
        #[command(flatten)]
        project: Project,
        #[command(flatten)]
        pipeline: NewPipeline,
        #[command(flatten)]
        dlq: Dlq,
        /// Rename the processor.
        #[arg(long, value_parser = name64)]
        new_name: Option<String>,
        /// Set the baseline processing tier.
        #[arg(long, value_enum)]
        tier: Option<Tier>,
        #[command(flatten)]
        autoscaling: Autoscaling,
    },
    /// Delete a processor.
    Delete {
        #[arg(value_parser = name64)]
        workspace: String,
        #[arg(value_parser = name64)]
        processor: String,
        #[command(flatten)]
        project: Project,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum NetworkingCommand {
    /// Show PrivateLink and account details.
    Show {
        #[command(flatten)]
        project: Project,
        /// Select the cloud provider for account details.
        #[arg(long, value_enum, requires = "region")]
        cloud_provider: Option<Provider>,
        /// Select the region for account details.
        #[arg(long, requires = "cloud_provider", value_parser = region)]
        region: Option<String>,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum PrivateLinkCommand {
    /// Create a PrivateLink connection.
    Create {
        #[command(flatten)]
        project: Project,
        /// Read the JSON configuration (- for stdin).
        #[arg(long, value_hint = clap::ValueHint::FilePath)]
        config_file: PathBuf,
    },
    /// Delete a PrivateLink connection.
    Delete {
        #[arg(value_parser = name64)]
        connection_id: String,
        #[command(flatten)]
        project: Project,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum PeeringCommand {
    /// Accept a VPC peering request.
    Accept {
        #[arg(value_parser = name64)]
        id: String,
        #[command(flatten)]
        project: Project,
        /// Set the requester's cloud account ID.
        #[arg(long)]
        requester_account_id: String,
        /// Set the requester's VPC ID.
        #[arg(long)]
        requester_vpc_id: String,
    },
    /// Reject a VPC peering request.
    Reject {
        #[arg(value_parser = name64)]
        id: String,
        #[command(flatten)]
        project: Project,
    },
    /// Delete a VPC peering connection.
    Delete {
        #[arg(value_parser = name64)]
        id: String,
        #[command(flatten)]
        project: Project,
    },
}

#[derive(Clone, Debug, Args)]
pub struct Project {
    /// Select the Atlas project ID.
    #[arg(
        id = "project_id",
        long = "project-id",
        value_name = "PROJECT_ID",
        value_parser = object_id
    )]
    pub id: String,
}

#[derive(Clone, Debug, Args)]
pub struct ListArgs {
    /// Limit the number of results per page.
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=500))]
    pub limit: u32,
    /// Select the page number.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
    pub page: u32,
    /// Ask Atlas for the total count.
    #[arg(long)]
    pub include_count: bool,
}

#[derive(Clone, Debug, Args)]
pub struct StreamsList {
    /// Limit the number of results per page.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=100))]
    pub limit: u32,
    /// Select the page number.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
    pub page: u32,
    #[command(flatten)]
    pub detail: Detail,
}

#[derive(Clone, Debug, Args)]
pub struct Detail {
    /// Return the redacted Atlas payload.
    #[arg(long)]
    pub detailed: bool,
}

#[derive(Clone, Debug, Args)]
#[group(multiple = false)]
pub struct NewPipeline {
    /// Replace the pipeline with a JSON array.
    #[arg(long)]
    pub pipeline: Option<String>,
    /// Read the new pipeline from a file or stdin (-).
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pub file: Option<PathBuf>,
}

#[derive(Clone, Debug, Args)]
pub struct Dlq {
    /// Send failed documents to this connection.
    #[arg(long, requires_all = ["dlq_db", "dlq_coll"], value_parser = name64)]
    pub dlq_connection: Option<String>,
    /// Set the dead letter queue database.
    #[arg(long, requires_all = ["dlq_connection", "dlq_coll"])]
    pub dlq_db: Option<String>,
    /// Set the dead letter queue collection.
    #[arg(long, requires_all = ["dlq_connection", "dlq_db"])]
    pub dlq_coll: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct Autoscaling {
    /// Turn processor autoscaling on or off.
    #[arg(long, value_enum)]
    pub autoscaling: Option<Switch>,
    /// Set the autoscaling floor tier.
    #[arg(long, value_enum)]
    pub min_tier: Option<TierBound>,
    /// Set the autoscaling ceiling tier.
    #[arg(long, value_enum)]
    pub max_tier: Option<TierBound>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Provider {
    Aws,
    Gcp,
    Azure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ClusterType {
    Replicaset,
    Sharded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum InstanceSize {
    M10,
    M20,
    M30,
    M40,
    M50,
    M60,
    M80,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum MaxInstanceSize {
    M10,
    M20,
    M30,
    M40,
    M50,
    M60,
    M80,
    M140,
    M200,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum TargetTier {
    Flex,
    M10,
    M20,
    M30,
    M40,
    M50,
    M60,
    M80,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum MongoVersion {
    #[value(name = "7.0")]
    V7,
    #[value(name = "8.0")]
    V8,
    Latest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Backup {
    Off,
    Snapshot,
    Continuous,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum EncryptionProvider {
    Aws,
    Azure,
    Gcp,
    None,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Switch {
    On,
    Off,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ConnectionType {
    Standard,
    Private,
    PrivateEndpoint,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum AlertStatus {
    Open,
    Tracking,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum AdvisorKind {
    SuggestedIndexes,
    DropIndexSuggestions,
    SlowQueryLogs,
    SchemaSuggestions,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Tier {
    Sp2,
    Sp5,
    Sp10,
    Sp30,
    Sp50,
}

/// A tier, or `default` to reset an autoscaling bound to the workspace default.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum TierBound {
    Sp2,
    Sp5,
    Sp10,
    Sp30,
    Sp50,
    Default,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ConnectionKind {
    Kafka,
    Cluster,
    S3,
    Https,
    AwsKinesisDataStreams,
    AwsLambda,
    SchemaRegistry,
    Sample,
}

impl ConnectionKind {
    pub fn atlas(self) -> &'static str {
        match self {
            Self::Kafka => "Kafka",
            Self::Cluster => "Cluster",
            Self::S3 => "S3",
            Self::Https => "Https",
            Self::AwsKinesisDataStreams => "AWSKinesisDataStreams",
            Self::AwsLambda => "AWSLambda",
            Self::SchemaRegistry => "SchemaRegistry",
            Self::Sample => "Sample",
        }
    }
}

impl ConnectionType {
    pub fn atlas(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Private => "private",
            Self::PrivateEndpoint => "privateEndpoint",
        }
    }
}

/// The Atlas spelling of an enum value: `aws` is `AWS`, `m10` is `M10`, `sp10` is `SP10`.
pub fn atlas<T: ValueEnum>(value: &T) -> String {
    value
        .to_possible_value()
        .expect("Atlas values are never skipped")
        .get_name()
        .to_uppercase()
}

impl AtlasCommand {
    /// The dotted command name and, for commands that change Atlas, what they change; those
    /// commands need read-write access and the global `--yes`.
    pub fn describe(&self) -> (&'static str, Option<String>) {
        match self {
            Self::Orgs { .. } => ("atlas.orgs.list", None),
            Self::Projects { command } => match command {
                ProjectsCommand::List { .. } => ("atlas.projects.list", None),
                ProjectsCommand::Create { name, .. } => (
                    "atlas.projects.create",
                    Some(format!("creates the Atlas project '{name}'")),
                ),
            },
            Self::Clusters { command } => command.describe(),
            Self::AccessList { command } => match command {
                AccessListCommand::List { .. } => ("atlas.access-list.list", None),
                AccessListCommand::Add {
                    project,
                    addresses,
                    cidr_blocks,
                    current_ip,
                    comment,
                } => {
                    let mut entries: Vec<String> =
                        addresses.iter().chain(cidr_blocks).cloned().collect();
                    if *current_ip {
                        entries.push("this machine's public IP address".into());
                    }
                    let comment = comment.as_deref().unwrap_or(admin::ACCESS_LIST_COMMENT);
                    (
                        "atlas.access-list.add",
                        Some(format!(
                            "allows network access to the clusters of project {} from {} (comment: {comment})",
                            project.id,
                            entries.join(", ")
                        )),
                    )
                }
            },
            Self::DbUsers { command } => match command {
                DbUsersCommand::List { .. } => ("atlas.db-users.list", None),
                DbUsersCommand::Create {
                    username,
                    project,
                    roles,
                    clusters,
                    ..
                } => {
                    let roles: Vec<String> = roles
                        .iter()
                        .map(|role| match &role.collection_name {
                            Some(collection) => {
                                format!("{} on {}.{collection}", role.role_name, role.database_name)
                            }
                            None => format!("{} on {}", role.role_name, role.database_name),
                        })
                        .collect();
                    let scope = if clusters.is_empty() {
                        "all clusters in the project".to_owned()
                    } else {
                        clusters.join(", ")
                    };
                    (
                        "atlas.db-users.create",
                        Some(format!(
                            "creates database user '{username}' in project {} with {} for {scope}",
                            project.id,
                            roles.join(", ")
                        )),
                    )
                }
            },
            Self::Alerts { .. } => ("atlas.alerts.list", None),
            Self::Regions { .. } => ("atlas.regions.list", None),
            Self::SampleData { command } => match command {
                SampleDataCommand::Load { cluster, .. } => (
                    "atlas.sample-data.load",
                    Some(format!(
                        "loads the sample databases into cluster '{cluster}'"
                    )),
                ),
                SampleDataCommand::Status { .. } => ("atlas.sample-data.status", None),
            },
            Self::Streams { command } => command.describe(),
        }
    }
}

impl ClustersCommand {
    fn describe(&self) -> (&'static str, Option<String>) {
        match self {
            Self::List { .. } => ("atlas.clusters.list", None),
            Self::Inspect { .. } => ("atlas.clusters.inspect", None),
            Self::Advisor { .. } => ("atlas.clusters.advisor", None),
            Self::Create { cluster, .. } => (
                "atlas.clusters.create",
                Some(format!(
                    "creates the billable M10+ cluster '{cluster}' and may add this machine's IP to the access list"
                )),
            ),
            Self::CreateFree { cluster, .. } => (
                "atlas.clusters.create-free",
                Some(format!(
                    "creates the free cluster '{cluster}' and may add this machine's IP to the access list"
                )),
            ),
            Self::Pause { cluster, .. } => (
                "atlas.clusters.pause",
                Some(format!(
                    "pauses cluster '{cluster}'; it stops accepting connections"
                )),
            ),
            Self::Resume { cluster, .. } => (
                "atlas.clusters.resume",
                Some(format!(
                    "resumes cluster '{cluster}'; compute billing restarts"
                )),
            ),
            Self::Upgrade { cluster, .. } => (
                "atlas.clusters.upgrade",
                Some(format!(
                    "changes the tier and billing of cluster '{cluster}'"
                )),
            ),
            Self::Connect {
                cluster,
                save_profile,
                ..
            } => (
                "atlas.clusters.connect",
                Some(format!(
                    "creates a temporary database user for cluster '{cluster}', may add this machine's IP to the access list, and saves profile '{save_profile}'"
                )),
            ),
            Self::Disconnect { saved_profile } => (
                "atlas.clusters.disconnect",
                Some(format!(
                    "deletes the temporary database user behind profile '{saved_profile}' and removes the profile"
                )),
            ),
        }
    }
}

impl StreamsCommand {
    fn describe(&self) -> (&'static str, Option<String>) {
        match self {
            Self::Workspaces { command } => match command {
                WorkspacesCommand::List { .. } => ("atlas.streams.workspaces.list", None),
                WorkspacesCommand::Inspect { .. } => ("atlas.streams.workspaces.inspect", None),
                WorkspacesCommand::Create { workspace, .. } => (
                    "atlas.streams.workspaces.create",
                    Some(format!(
                        "creates the Stream Processing workspace '{workspace}'"
                    )),
                ),
                WorkspacesCommand::Update { workspace, .. } => (
                    "atlas.streams.workspaces.update",
                    Some(format!("updates workspace '{workspace}'")),
                ),
                WorkspacesCommand::Delete { workspace, .. } => (
                    "atlas.streams.workspaces.delete",
                    Some(format!(
                        "permanently deletes workspace '{workspace}' with all of its connections and processors; this cannot be undone"
                    )),
                ),
            },
            Self::Connections { command } => match command {
                ConnectionsCommand::List { .. } => ("atlas.streams.connections.list", None),
                ConnectionsCommand::Inspect { .. } => ("atlas.streams.connections.inspect", None),
                ConnectionsCommand::Create { connection, .. } => (
                    "atlas.streams.connections.create",
                    Some(format!("creates the connection '{connection}'")),
                ),
                ConnectionsCommand::Update { connection, .. } => (
                    "atlas.streams.connections.update",
                    Some(format!("updates connection '{connection}'")),
                ),
                ConnectionsCommand::Delete { connection, .. } => (
                    "atlas.streams.connections.delete",
                    Some(format!(
                        "deletes connection '{connection}'; processors that use it will fail"
                    )),
                ),
            },
            Self::Processors { command } => match command {
                ProcessorsCommand::List { .. } => ("atlas.streams.processors.list", None),
                ProcessorsCommand::Inspect { .. } => ("atlas.streams.processors.inspect", None),
                ProcessorsCommand::Diagnose { .. } => ("atlas.streams.processors.diagnose", None),
                ProcessorsCommand::Create {
                    processor, start, ..
                } => (
                    "atlas.streams.processors.create",
                    Some(if *start {
                        format!("creates and starts processor '{processor}'; billing begins")
                    } else {
                        format!("creates processor '{processor}'")
                    }),
                ),
                ProcessorsCommand::Start {
                    processor,
                    no_resume_from_checkpoint,
                    ..
                } => {
                    let mut risk = format!(
                        "starts processor '{processor}'; billing begins at the workspace tier"
                    );
                    if *no_resume_from_checkpoint {
                        risk.push_str(
                            ", and all accumulated window state will be permanently lost",
                        );
                    }
                    ("atlas.streams.processors.start", Some(risk))
                }
                ProcessorsCommand::Stop { processor, .. } => (
                    "atlas.streams.processors.stop",
                    Some(format!(
                        "stops processor '{processor}'; in-flight data completes processing"
                    )),
                ),
                ProcessorsCommand::Modify { processor, .. } => (
                    "atlas.streams.processors.modify",
                    Some(format!(
                        "modifies processor '{processor}', which may change its pipeline behavior"
                    )),
                ),
                ProcessorsCommand::Delete { processor, .. } => (
                    "atlas.streams.processors.delete",
                    Some(format!(
                        "stops processor '{processor}' if it runs and permanently deletes its state and checkpoints"
                    )),
                ),
            },
            Self::Networking { .. } => ("atlas.streams.networking.show", None),
            Self::Privatelink { command } => match command {
                PrivateLinkCommand::Create { .. } => (
                    "atlas.streams.privatelink.create",
                    Some("creates a PrivateLink connection".into()),
                ),
                PrivateLinkCommand::Delete { connection_id, .. } => (
                    "atlas.streams.privatelink.delete",
                    Some(format!(
                        "deletes PrivateLink connection '{connection_id}'; this cannot be undone"
                    )),
                ),
            },
            Self::Peering { command } => match command {
                PeeringCommand::Accept { id, .. } => (
                    "atlas.streams.peering.accept",
                    Some(format!("accepts VPC peering connection '{id}'")),
                ),
                PeeringCommand::Reject { id, .. } => (
                    "atlas.streams.peering.reject",
                    Some(format!(
                        "rejects VPC peering connection '{id}'; this cannot be undone"
                    )),
                ),
                PeeringCommand::Delete { id, .. } => (
                    "atlas.streams.peering.delete",
                    Some(format!(
                        "deletes VPC peering connection '{id}'; this cannot be undone"
                    )),
                ),
            },
        }
    }
}

fn object_id(value: &str) -> Result<String, String> {
    if value.len() == 24 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(value.into())
    } else {
        Err("expected a 24-character hexadecimal ID".into())
    }
}

/// Cluster, workspace, connection, processor, and peering names.
fn name64(value: &str) -> Result<String, String> {
    identifier(value, 64)
}

fn region(value: &str) -> Result<String, String> {
    identifier(value, 50)
}

fn identifier(value: &str, max: usize) -> Result<String, String> {
    let valid = (1..=max).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if valid {
        Ok(value.into())
    } else {
        Err(format!("use 1 to {max} letters, digits, '-' or '_'"))
    }
}

fn project_name(value: &str) -> Result<String, String> {
    let valid = (1..=64).contains(&value.chars().count())
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || character.is_ascii_whitespace()
                || "()@&+:._',-".contains(character)
        });
    if valid {
        Ok(value.into())
    } else {
        Err("use up to 64 letters, digits, spaces, and ( ) @ & + : . _ ' , -".into())
    }
}

fn username(value: &str) -> Result<String, String> {
    let valid = (1..=100).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'));
    if valid {
        Ok(value.into())
    } else {
        Err("use 1 to 100 letters, digits, '.', '-' or '_'".into())
    }
}

fn ipv4(value: &str) -> Result<String, String> {
    value
        .parse::<Ipv4Addr>()
        .map(|_| value.to_owned())
        .map_err(|_| "expected an IPv4 address".into())
}

fn cidr(value: &str) -> Result<String, String> {
    let valid = value.split_once('/').is_some_and(|(address, prefix)| {
        address.parse::<Ipv4Addr>().is_ok()
            && !prefix.is_empty()
            && prefix.bytes().all(|byte| byte.is_ascii_digit())
            && prefix.parse::<u8>().is_ok_and(|prefix| prefix <= 32)
    });
    if valid {
        Ok(value.into())
    } else {
        Err("expected an IPv4 CIDR block such as 10.0.0.0/16".into())
    }
}

fn comment(value: &str) -> Result<String, String> {
    if value.len() <= 80
        && value
            .chars()
            .all(|character| (' '..='~').contains(&character))
    {
        Ok(value.into())
    } else {
        Err("use at most 80 printable ASCII characters".into())
    }
}

fn role(value: &str) -> Result<DbRole, String> {
    DbRole::parse(value)
}

fn provider_name(value: &str) -> Result<String, String> {
    if !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphabetic() || character == '_')
    {
        Ok(value.to_ascii_uppercase())
    } else {
        Err("use letters and '_' only, such as AWS".into())
    }
}

fn since(value: &str) -> Result<i64, String> {
    admin::epoch_millis(value).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_validated_like_the_mcp_server() {
        assert!(object_id("65f1a2b3c4d5e6f708192a3b").is_ok());
        for bad in ["65f1", "65f1a2b3c4d5e6f708192a3z", ""] {
            assert!(object_id(bad).is_err(), "{bad}");
        }
        assert!(name64("cluster-0_a").is_ok());
        assert!(name64(&"x".repeat(65)).is_err());
        assert!(name64("bad.name").is_err());
        assert!(region(&"R".repeat(51)).is_err());
        assert!(project_name("Shop (prod) & co, v1.2").is_ok());
        assert!(project_name("bad/name").is_err());
        assert!(username("app.user-1").is_ok());
        assert!(username("app user").is_err());
        assert!(ipv4("10.0.0.1").is_ok());
        assert!(ipv4("10.0.0.256").is_err());
        assert!(ipv4("010.0.0.1").is_err());
        assert!(cidr("10.0.0.0/16").is_ok());
        for bad in [
            "10.0.0.0",
            "10.0.0.0/33",
            "10.0.0.0/",
            "10.0.0.0/+8",
            "::/0",
        ] {
            assert!(cidr(bad).is_err(), "{bad}");
        }
        assert!(comment("ok").is_ok());
        assert!(comment(&"c".repeat(81)).is_err());
        assert!(comment("tab\t").is_err());
        assert_eq!(provider_name("aws").unwrap(), "AWS");
        assert!(provider_name("AWS1").is_err());
    }

    #[test]
    fn enums_map_to_atlas_spellings() {
        assert_eq!(atlas(&Provider::Azure), "AZURE");
        assert_eq!(atlas(&InstanceSize::M10), "M10");
        assert_eq!(atlas(&MaxInstanceSize::M200), "M200");
        assert_eq!(atlas(&TargetTier::Flex), "FLEX");
        assert_eq!(atlas(&ClusterType::Replicaset), "REPLICASET");
        assert_eq!(atlas(&MongoVersion::V7), "7.0");
        assert_eq!(atlas(&MongoVersion::Latest), "LATEST");
        assert_eq!(atlas(&Backup::Continuous), "CONTINUOUS");
        assert_eq!(atlas(&EncryptionProvider::None), "NONE");
        assert_eq!(atlas(&AlertStatus::Tracking), "TRACKING");
        assert_eq!(atlas(&Tier::Sp10), "SP10");
        assert_eq!(
            ConnectionKind::AwsKinesisDataStreams.atlas(),
            "AWSKinesisDataStreams"
        );
        assert_eq!(ConnectionType::PrivateEndpoint.atlas(), "privateEndpoint");
    }
}
