use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mongodb_client::{
    Bson, ClientError, Document, ErrorCode, ExplainTarget, ExportSource, FindQuery,
    IndexCheckTarget, LogType, MongoClient, NewIndex, Page, PageRequest, Verbosity, ejson, guard,
};
use serde_json::{Map, Value, json};

use crate::args::*;
use crate::config::{
    ConfigFile, CredentialStore, ProfileChanges, ProfileConfig, Settings, SystemKeyring, URI_ENV,
    default_config_path,
};
use crate::doctor::{self, DoctorReport};
use crate::output::Meta;

pub struct DispatchResult {
    pub command: String,
    pub data: Value,
    pub meta: Meta,
    pub failure_code: Option<ErrorCode>,
    /// The command already wrote its output to stdout, as `documents export --out -` does.
    pub emitted: bool,
}

#[derive(Debug)]
pub struct AppError {
    pub code: ErrorCode,
    pub message: String,
    pub hint: Option<String>,
    pub server_code: Option<i32>,
    pub code_name: Option<String>,
}

impl AppError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
            server_code: None,
            code_name: None,
        }
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Config, message)
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Validation, message)
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl From<ClientError> for AppError {
    fn from(error: ClientError) -> Self {
        Self {
            code: error.code(),
            message: error.to_string(),
            hint: error.hint().map(str::to_owned),
            server_code: error.server_code(),
            code_name: error.code_name().map(str::to_owned),
        }
    }
}

pub async fn run(cli: &Cli) -> Result<DispatchResult, AppError> {
    match &cli.command {
        Command::Completion { shell } => {
            return Ok(local(
                "completion",
                Value::String(crate::completion::generate(*shell)),
            ));
        }
        Command::Version => {
            return Ok(local(
                "version",
                json!({"version": env!("CARGO_PKG_VERSION")}),
            ));
        }
        Command::Doctor => {
            // Atlas checks follow the MongoDB checks only while Atlas tools are enabled.
            let report = run_doctor(cli).await;
            return Ok(crate::atlas::doctor(cli, report, crate::gate::enabled()).await);
        }
        Command::Config {
            command: ConfigCommand::Path,
        } => {
            return Ok(local(
                "config.path",
                json!({"path": config_path()?.to_string_lossy()}),
            ));
        }
        Command::Init => {
            return crate::onboarding::run_init(
                cli.profile.as_deref(),
                cli.timeout,
                &config_path()?,
                &SystemKeyring,
            )
            .await;
        }
        Command::Skills {
            command: SkillsCommand::Install { target },
        } => {
            let results = crate::skill_install::install(*target).map_err(AppError::config)?;
            return Ok(local(
                "skills.install",
                serde_json::to_value(results)
                    .expect("Skill installer result paths are guaranteed strings"),
            ));
        }
        // Reaching these means Atlas tools are enabled; the parser leaves them out otherwise.
        Command::Local { command } => return crate::local::run(cli, command).await,
        Command::Knowledge { command } => return crate::knowledge::run(command, cli.timeout).await,
        // These flags are only hidden while Atlas tools are disabled, so refuse them here.
        Command::Auth {
            command: AuthCommand::Login { atlas: true, .. } | AuthCommand::Logout { atlas: true },
        } => crate::gate::require_enabled()?,
        _ => {}
    }
    let path = config_path()?;
    let mut config = ConfigFile::load(&path).map_err(AppError::config)?;
    let store = SystemKeyring;
    match &cli.command {
        Command::Config {
            command: ConfigCommand::Show,
        } => Ok(local(
            "config.show",
            serde_json::to_value(&config).map_err(|error| AppError::config(error.to_string()))?,
        )),
        Command::Auth {
            command:
                AuthCommand::Login {
                    uri_stdin,
                    no_verify,
                    atlas: false,
                    ..
                },
        } => {
            crate::onboarding::run_login(
                crate::onboarding::LoginRequest {
                    selected_profile: cli.profile.as_deref(),
                    uri_stdin: *uri_stdin,
                    no_verify: *no_verify,
                    timeout: cli.timeout,
                    output: cli.output,
                },
                &mut config,
                &path,
                &store,
            )
            .await
        }
        Command::Auth {
            command: AuthCommand::Logout { atlas: false },
        } => logout(cli.profile.as_deref(), &mut config, &path, &store),
        Command::Profile { command } => {
            profile(command, cli.profile.as_deref(), &path, &mut config, &store)
        }
        Command::Config {
            command:
                ConfigCommand::Set {
                    key: ConfigKey::AtlasTools,
                    value,
                },
        } => {
            config.atlas_tools = *value == Toggle::Enabled;
            config.save(&path).map_err(AppError::config)?;
            let mut result = local(
                "config.set",
                json!({"key": ConfigKey::AtlasTools, "value": value}),
            );
            if config.atlas_tools_enabled() != Ok(config.atlas_tools) {
                result.meta.warnings.push(format!(
                    "{} is set and takes precedence over this setting",
                    crate::config::ATLAS_TOOLS_ENV
                ));
            }
            Ok(result)
        }
        Command::Auth {
            command:
                AuthCommand::Login {
                    no_verify,
                    atlas: true,
                    credentials_stdin,
                    ..
                },
        } => {
            crate::atlas::login(
                cli,
                *no_verify,
                *credentials_stdin,
                &mut config,
                &path,
                &store,
            )
            .await
        }
        Command::Auth {
            command: AuthCommand::Logout { atlas: true },
        } => crate::atlas::logout(cli, &mut config, &path, &store),
        Command::Atlas { command } => {
            crate::atlas::run(cli, command, &mut config, &path, &store).await
        }
        command => remote(cli, command, &config, &store).await,
    }
}

async fn remote(
    cli: &Cli,
    command: &Command,
    config: &ConfigFile,
    store: &dyn CredentialStore,
) -> Result<DispatchResult, AppError> {
    // Inputs are validated before any connection, so malformed JSON never reaches a server.
    let operation = Operation::parse(command)?;
    let name = operation.name();
    let target = config
        .resolve_target(cli.profile.as_deref())
        .map_err(AppError::config)?;
    authorize(
        &operation,
        target.profile.as_deref(),
        target.access,
        &target.settings,
        cli.yes,
    )?;
    let access = target.access;
    let connection = config
        .resolve_credentials(target, store)
        .map_err(AppError::config)?;
    let started = Instant::now();
    let client = MongoClient::connect(&connection.uri, Duration::from_secs(cli.timeout)).await?;
    let mut warnings = connection.warnings;
    if (cli.require_index || connection.settings.index_check)
        && let Some((database, collection, target)) = operation.index_check(&connection.settings)
    {
        warnings.extend(
            client
                .check_index_usage(database, collection, target)
                .await?,
        );
    }
    let auth_check = matches!(operation, Operation::AuthCheck);
    let emitted = matches!(operation, Operation::DocumentsExport { out: None, .. });
    let (data, mut meta) = operation.execute(&client, &connection.settings).await?;
    if auth_check && access == Access::ReadOnly {
        meta.warnings.extend(doctor::read_only_role_warning(&data));
    }
    warnings.append(&mut meta.warnings);
    meta.warnings = warnings;
    meta.profile = connection.profile;
    meta.access = Some(access);
    meta.elapsed_ms = Some(started.elapsed().as_millis());
    Ok(DispatchResult {
        command: name.to_owned(),
        data,
        meta,
        failure_code: None,
        emitted,
    })
}

/// Refuse what the connection does not allow, before credentials are read or a server is contacted.
fn authorize(
    operation: &Operation,
    profile: Option<&str>,
    access: Access,
    settings: &Settings,
    yes: bool,
) -> Result<(), AppError> {
    let name = operation.name();
    if let Some(message) = disabled(operation, &settings.disabled_tools) {
        return Err(AppError::new(ErrorCode::Forbidden, message).with_hint(
            "the user disabled this operation for the connection; do not change its settings",
        ));
    }
    if operation.writes() && access == Access::ReadOnly {
        return Err(read_only_refusal(name, profile));
    }
    if !settings.server_side_js
        && let Some(operator) = operation.server_side_js()
    {
        return Err(AppError::new(
            ErrorCode::Forbidden,
            format!(
                "server-side JavaScript is disabled: the \"{operator}\" operator is not allowed"
            ),
        )
        .with_hint("rewrite the query with $expr or aggregation operators"));
    }
    if !yes && let Some(reason) = confirmation(operation, &settings.confirmation_required_tools) {
        return Err(AppError::validation(format!("{name} {reason}"))
            .with_hint("check the profile and target, then rerun the same command with --yes"));
    }
    Ok(())
}

/// Why the disabled tools refuse the operation, including the MCP server's linked refusals.
fn disabled(operation: &Operation, disabled_tools: &[String]) -> Option<String> {
    let name = operation.name();
    let listed = |entry: &str| disabled_tools.iter().any(|value| value == entry);
    if let Some(entry) = disabled_tools.iter().find(|entry| operation.matches(entry)) {
        return Some(format!(
            "{name} is disabled: \"{entry}\" is listed in the disabled tools"
        ));
    }
    if operation.has_write_stage()
        && let Some(kind) = ["create", "update", "delete"]
            .into_iter()
            .find(|kind| listed(kind))
    {
        return Some(format!(
            "{name} cannot run pipelines with $out or $merge while \"{kind}\" operations are disabled"
        ));
    }
    if matches!(
        operation,
        Operation::CollectionsRename {
            drop_target: true,
            ..
        }
    ) && listed("delete")
    {
        return Some(format!(
            "{name} cannot use --drop-target while \"delete\" operations are disabled"
        ));
    }
    None
}

/// Why the operation needs --yes: it destroys data, or the settings require confirmation for it.
fn confirmation(operation: &Operation, required: &[String]) -> Option<String> {
    if operation.destructive() {
        let targets = operation.write_targets();
        return Some(if targets.is_empty() {
            "is destructive and requires --yes".into()
        } else {
            format!("writes data and requires --yes: {}", targets.join("; "))
        });
    }
    required
        .iter()
        .find(|entry| operation.matches(entry))
        .map(|entry| {
            format!("requires --yes: \"{entry}\" is listed in the confirmation-required tools")
        })
}

fn read_only_refusal(command: &str, profile: Option<&str>) -> AppError {
    match profile {
        Some(name) => AppError::new(
            ErrorCode::Forbidden,
            format!("profile '{name}' is read-only; {command} would modify data"),
        )
        .with_hint("use a profile the user configured with --access read-write; do not change this profile's access"),
        None => AppError::new(
            ErrorCode::Forbidden,
            format!("the {URI_ENV} connection is read-only; {command} would modify data"),
        )
        .with_hint("environment connections allow writes only when MONGODB_CLI_ACCESS=read-write is set by the user"),
    }
}

async fn run_doctor(cli: &Cli) -> DispatchResult {
    let mut report = DoctorReport::new(env!("CARGO_PKG_VERSION"));
    let mut meta = Meta::default();
    let config =
        match config_path().and_then(|path| ConfigFile::load(&path).map_err(AppError::config)) {
            Ok(config) => config,
            Err(_) => {
                report.configuration_failed();
                return doctor_result(report, meta, Some(ErrorCode::Config));
            }
        };
    let target = match config.resolve_target(cli.profile.as_deref()) {
        Ok(target) => target,
        Err(_) => {
            report.configuration_failed();
            return doctor_result(report, meta, Some(ErrorCode::Config));
        }
    };
    report.configuration_passed();
    meta.profile = target.profile.clone();
    meta.access = Some(target.access);
    meta.warnings.clone_from(&target.warnings);
    let access = target.access;
    let connection = match config.resolve_credentials(target, &SystemKeyring) {
        Ok(connection) => connection,
        Err(_) => {
            report.credentials_failed();
            return doctor_result(report, meta, Some(ErrorCode::Config));
        }
    };
    report.credentials_passed();

    let started = Instant::now();
    let client = match MongoClient::connect(&connection.uri, Duration::from_secs(cli.timeout)).await
    {
        Ok(client) => client,
        Err(error) => {
            report.server_failed(server_failure_message(error.code()));
            meta.elapsed_ms = Some(started.elapsed().as_millis());
            return doctor_result(report, meta, Some(error.code()));
        }
    };
    let failure = match client.ping().await {
        Ok(()) => {
            report.server_passed(match client.server_info().await {
                Ok(info) => doctor::server_message(&info),
                Err(_) => "MongoDB server is reachable".into(),
            });
            match client.connection_status().await {
                Ok(status) => {
                    report.authentication_passed(doctor::identity(&status));
                    if access == Access::ReadOnly {
                        meta.warnings
                            .extend(doctor::read_only_role_warning(&status));
                    }
                    None
                }
                Err(error) => {
                    report
                        .authentication_failed("the server did not report the authenticated user");
                    Some(error.code())
                }
            }
        }
        Err(error) => {
            match error.code() {
                ErrorCode::Unauthorized => {
                    report.server_passed("MongoDB server is reachable".into());
                    report
                        .authentication_failed("the connection string's credentials were rejected");
                }
                ErrorCode::Forbidden => {
                    report.server_passed("MongoDB server is reachable".into());
                    report
                        .authentication_failed("the database user may not run the diagnostic ping");
                }
                code => report.server_failed(server_failure_message(code)),
            }
            Some(error.code())
        }
    };
    meta.elapsed_ms = Some(started.elapsed().as_millis());
    doctor_result(report, meta, failure)
}

fn doctor_result(
    report: DoctorReport,
    meta: Meta,
    failure_code: Option<ErrorCode>,
) -> DispatchResult {
    DispatchResult {
        command: "doctor".into(),
        data: report.into_value(),
        meta,
        failure_code,
        emitted: false,
    }
}

fn server_failure_message(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::Network => "MongoDB server is unreachable",
        ErrorCode::Timeout => "MongoDB server did not respond before the timeout",
        ErrorCode::Config => "the connection could not be configured; check the TLS settings",
        ErrorCode::Validation => "the connection string options are invalid",
        ErrorCode::FeatureUnsupported => "the MongoDB server version is not supported",
        _ => "MongoDB server rejected the diagnostic request",
    }
}

fn profile(
    command: &ProfileCommand,
    selected_profile: Option<&str>,
    path: &Path,
    config: &mut ConfigFile,
    store: &dyn CredentialStore,
) -> Result<DispatchResult, AppError> {
    match command {
        ProfileCommand::Add { name, settings } => {
            let changes = profile_changes(settings);
            config
                .add_profile(name, changes.access.unwrap_or_default())
                .map_err(AppError::validation)?;
            let profile = config
                .profiles
                .get_mut(name)
                .expect("the profile was just added");
            profile.apply(&changes);
            let data = profile_value(name, profile);
            config.save(path).map_err(AppError::config)?;
            Ok(local("profile.add", data))
        }
        ProfileCommand::Update { name, settings } => {
            let changes = profile_changes(settings);
            if changes.is_empty() {
                return Err(
                    AppError::validation("profile update needs a setting to change")
                        .with_hint("run `mongodb-cli profile update --help` to list the settings"),
                );
            }
            let profile = config
                .profiles
                .get_mut(name)
                .ok_or_else(|| AppError::validation(format!("profile '{name}' does not exist")))?;
            let loosened = profile.loosened_by(&changes);
            if !loosened.is_empty() {
                crate::onboarding::confirm_loosening(name, &loosened)?;
            }
            let raised =
                profile.access == Access::ReadOnly && changes.access == Some(Access::ReadWrite);
            let changed = profile.apply(&changes);
            let mut data = profile_value(name, profile);
            data["changed"] = json!(changed);
            config.save(path).map_err(AppError::config)?;
            let mut result = local("profile.update", data);
            result.meta.profile = Some(name.clone());
            if raised {
                result.meta.warnings.push(format!(
                    "profile '{name}' now allows write and destructive operations"
                ));
            }
            Ok(result)
        }
        ProfileCommand::List => {
            let mut result = local(
                "profile.list",
                serde_json::to_value(&config.profiles)
                    .map_err(|error| AppError::config(error.to_string()))?,
            );
            result.meta.profile.clone_from(&config.current_profile);
            Ok(result)
        }
        ProfileCommand::Show { name } => {
            let name = name
                .clone()
                .or_else(|| selected_profile.map(str::to_owned))
                .or_else(|| config.current_profile.clone())
                .ok_or_else(|| AppError::config("no current profile"))?;
            let profile = config
                .profiles
                .get(&name)
                .ok_or_else(|| AppError::config(format!("profile '{name}' does not exist")))?;
            let mut data = profile_value(&name, profile);
            // Which credentials the keyring holds, as booleans; never their values.
            data["credentials"] =
                json!({"connectionString": profile.target.is_some(), "atlas": profile.atlas});
            Ok(local("profile.show", data))
        }
        ProfileCommand::Use { name } => {
            config.use_profile(name).map_err(AppError::config)?;
            config.save(path).map_err(AppError::config)?;
            Ok(local("profile.use", json!({"currentProfile": name})))
        }
        ProfileCommand::Remove { name } => {
            config
                .remove_profile(name, store)
                .map_err(AppError::config)?;
            config.save(path).map_err(AppError::config)?;
            Ok(local(
                "profile.remove",
                json!({"name": name, "removed": true}),
            ))
        }
    }
}

/// Omitted flags keep the current value; empty list entries are dropped, so `""` clears a list.
fn profile_changes(settings: &ProfileSettingsArgs) -> ProfileChanges {
    let list = |values: &Option<Vec<String>>| {
        values.as_ref().map(|values| {
            values
                .iter()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect()
        })
    };
    ProfileChanges {
        access: settings.access,
        server_side_js: settings.server_side_js,
        index_check: settings.index_check.map(|switch| switch == Switch::On),
        max_documents_per_query: settings.max_documents_per_query,
        max_bytes_per_query: settings.max_bytes_per_query,
        query_count_max_time_ms: settings.query_count_max_time_ms,
        aggregation_count_max_time_ms: settings.aggregation_count_max_time_ms,
        disabled_tools: list(&settings.disabled_tools),
        confirmation_required_tools: list(&settings.confirmation_required_tools),
    }
}

/// The profile's name and every stored field; connection strings never live in a profile.
fn profile_value(name: &str, profile: &ProfileConfig) -> Value {
    let mut fields = Map::new();
    fields.insert("name".into(), json!(name));
    if let Ok(Value::Object(stored)) = serde_json::to_value(profile) {
        fields.extend(stored);
    }
    Value::Object(fields)
}

fn logout(
    selected_profile: Option<&str>,
    config: &mut ConfigFile,
    path: &Path,
    store: &dyn CredentialStore,
) -> Result<DispatchResult, AppError> {
    let profile = selected_profile
        .map(str::to_owned)
        .or_else(|| config.current_profile.clone())
        .ok_or_else(|| AppError::config("select a profile before auth logout"))?;
    if !config.profiles.contains_key(&profile) {
        return Err(AppError::config(format!(
            "profile '{profile}' does not exist"
        )));
    }
    store.delete(&profile).map_err(AppError::config)?;
    // Without a stored connection string the profile has no target.
    if let Some(saved) = config.profiles.get_mut(&profile) {
        saved.target = None;
    }
    config.save(path).map_err(AppError::config)?;
    Ok(local(
        "auth.logout",
        json!({"profile": profile, "removed": true}),
    ))
}

/// A remote operation with every input already parsed and validated.
enum Operation {
    ServerInfo,
    ServerLogs {
        log_type: LogType,
        limit: u32,
    },
    AuthCheck,
    DatabasesList,
    DatabasesStats {
        database: String,
    },
    DatabasesAggregate {
        database: String,
        pipeline: Vec<Document>,
        page: PageArgs,
    },
    DatabasesDrop {
        database: String,
    },
    CollectionsList {
        database: String,
    },
    CollectionsSchema {
        database: String,
        collection: String,
        sample_size: u32,
        response_bytes_limit: u64,
    },
    CollectionsStats {
        database: String,
        collection: String,
    },
    CollectionsCreate {
        database: String,
        collection: String,
        options: Document,
    },
    CollectionsRename {
        database: String,
        collection: String,
        new_name: String,
        drop_target: bool,
    },
    CollectionsDrop {
        database: String,
        collection: String,
    },
    IndexesList {
        database: String,
        collection: String,
        index_type: IndexListType,
    },
    IndexesCreate {
        database: String,
        collection: String,
        index: NewIndex,
    },
    IndexesDrop {
        database: String,
        collection: String,
        name: String,
        search: bool,
    },
    DocumentsFind {
        database: String,
        collection: String,
        query: FindQuery,
        page: PageArgs,
    },
    DocumentsCount {
        database: String,
        collection: String,
        filter: Document,
    },
    DocumentsExport {
        database: String,
        collection: String,
        source: ExportSource,
        canonical: bool,
        /// `None` streams the array to stdout.
        out: Option<PathBuf>,
    },
    DocumentsAggregate {
        database: String,
        collection: String,
        pipeline: Vec<Document>,
        page: PageArgs,
    },
    DocumentsInsert {
        database: String,
        collection: String,
        documents: Vec<Document>,
    },
    DocumentsUpdate {
        database: String,
        collection: String,
        filter: Document,
        update: Bson,
        upsert: bool,
    },
    DocumentsDelete {
        database: String,
        collection: String,
        filter: Document,
    },
    ExplainFind {
        database: String,
        collection: String,
        query: FindQuery,
        limit: Option<u32>,
        verbosity: Verbosity,
    },
    ExplainAggregate {
        database: String,
        collection: String,
        pipeline: Vec<Document>,
        verbosity: Verbosity,
    },
    ExplainCount {
        database: String,
        collection: String,
        filter: Document,
        verbosity: Verbosity,
    },
}

impl Operation {
    fn parse(command: &Command) -> Result<Self, AppError> {
        Ok(match command {
            Command::Server { command } => match command {
                ServerCommand::Info => Self::ServerInfo,
                ServerCommand::Logs { log_type, limit } => Self::ServerLogs {
                    log_type: match log_type {
                        LogKind::Global => LogType::Global,
                        LogKind::StartupWarnings => LogType::StartupWarnings,
                    },
                    limit: *limit,
                },
            },
            Command::Auth {
                command: AuthCommand::Check,
            } => Self::AuthCheck,
            Command::Databases { command } => match command {
                DatabasesCommand::List => Self::DatabasesList,
                DatabasesCommand::Stats { database } => Self::DatabasesStats {
                    database: non_empty(database, "database")?,
                },
                DatabasesCommand::Aggregate {
                    database,
                    pipeline,
                    page,
                } => {
                    let pipeline = read_pipeline(pipeline)?;
                    guard::validate_database_pipeline(&pipeline)?;
                    Self::DatabasesAggregate {
                        database: non_empty(database, "database")?,
                        pipeline,
                        page: page.clone(),
                    }
                }
                DatabasesCommand::Drop { database } => Self::DatabasesDrop {
                    database: non_empty(database, "database")?,
                },
            },
            Command::Collections { command } => match command {
                CollectionsCommand::List { database } => Self::CollectionsList {
                    database: non_empty(database, "database")?,
                },
                CollectionsCommand::Schema {
                    database,
                    collection,
                    sample_size,
                    response_bytes_limit,
                } => Self::CollectionsSchema {
                    database: non_empty(database, "database")?,
                    collection: non_empty(collection, "collection")?,
                    sample_size: *sample_size,
                    response_bytes_limit: *response_bytes_limit,
                },
                CollectionsCommand::Stats {
                    database,
                    collection,
                } => Self::CollectionsStats {
                    database: non_empty(database, "database")?,
                    collection: non_empty(collection, "collection")?,
                },
                CollectionsCommand::Create {
                    database,
                    collection,
                    options,
                } => Self::CollectionsCreate {
                    database: non_empty(database, "database")?,
                    collection: non_empty(collection, "collection")?,
                    options: optional_document(options.as_deref(), "--options")?
                        .unwrap_or_default(),
                },
                CollectionsCommand::Rename {
                    database,
                    collection,
                    new_name,
                    drop_target,
                } => Self::CollectionsRename {
                    database: non_empty(database, "database")?,
                    collection: non_empty(collection, "collection")?,
                    new_name: non_empty(new_name, "new collection name")?,
                    drop_target: *drop_target,
                },
                CollectionsCommand::Drop {
                    database,
                    collection,
                } => Self::CollectionsDrop {
                    database: non_empty(database, "database")?,
                    collection: non_empty(collection, "collection")?,
                },
            },
            Command::Indexes { command } => match command {
                IndexesCommand::List {
                    database,
                    collection,
                    index_type,
                } => Self::IndexesList {
                    database: non_empty(database, "database")?,
                    collection: non_empty(collection, "collection")?,
                    index_type: *index_type,
                },
                IndexesCommand::Create {
                    database,
                    collection,
                    index_type,
                    keys,
                    options,
                    definition,
                    name,
                } => Self::IndexesCreate {
                    database: non_empty(database, "database")?,
                    collection: non_empty(collection, "collection")?,
                    index: new_index(*index_type, keys, options, definition, name)?,
                },
                IndexesCommand::Drop {
                    database,
                    collection,
                    name,
                    index_type,
                } => Self::IndexesDrop {
                    database: non_empty(database, "database")?,
                    collection: non_empty(collection, "collection")?,
                    name: non_empty(name, "index name")?,
                    search: *index_type == IndexType::Search,
                },
            },
            Command::Documents { command } => match command {
                DocumentsCommand::Find {
                    database,
                    collection,
                    query,
                    page,
                } => Self::DocumentsFind {
                    database: non_empty(database, "database")?,
                    collection: non_empty(collection, "collection")?,
                    query: find_query(query)?,
                    page: page.clone(),
                },
                DocumentsCommand::Count {
                    database,
                    collection,
                    filter,
                } => Self::DocumentsCount {
                    database: non_empty(database, "database")?,
                    collection: non_empty(collection, "collection")?,
                    filter: optional_document(filter.as_deref(), "--filter")?.unwrap_or_default(),
                },
                DocumentsCommand::Export {
                    database,
                    collection,
                    query,
                    limit,
                    pipeline,
                    file,
                    format,
                    out,
                } => {
                    let source = if pipeline.is_none() && file.is_none() {
                        ExportSource::Find {
                            query: Box::new(find_query(query)?),
                            limit: *limit,
                        }
                    } else {
                        let pipeline = read_pipeline(&PipelineArgs {
                            pipeline: pipeline.clone(),
                            file: file.clone(),
                        })?;
                        if guard::write_stage(&pipeline).is_some() {
                            return Err(AppError::validation(
                                "export cannot run pipelines with $out or $merge; use documents aggregate",
                            ));
                        }
                        ExportSource::Aggregate { pipeline }
                    };
                    Self::DocumentsExport {
                        database: non_empty(database, "database")?,
                        collection: non_empty(collection, "collection")?,
                        source,
                        canonical: *format == ExportFormat::Canonical,
                        out: (out.as_path() != Path::new("-")).then(|| out.clone()),
                    }
                }
                DocumentsCommand::Aggregate {
                    database,
                    collection,
                    pipeline,
                    page,
                } => Self::DocumentsAggregate {
                    database: non_empty(database, "database")?,
                    collection: non_empty(collection, "collection")?,
                    pipeline: read_pipeline(pipeline)?,
                    page: page.clone(),
                },
                DocumentsCommand::Insert {
                    database,
                    collection,
                    input,
                } => {
                    let documents = match (&input.documents, &input.file) {
                        (Some(text), _) => ejson::parse_documents(text, "--documents")?,
                        (None, Some(path)) => ejson::parse_documents(&read_input(path)?, "--file")?,
                        (None, None) => unreachable!("clap requires --documents or --file"),
                    };
                    if documents.is_empty() {
                        return Err(AppError::validation("there are no documents to insert"));
                    }
                    Self::DocumentsInsert {
                        database: non_empty(database, "database")?,
                        collection: non_empty(collection, "collection")?,
                        documents,
                    }
                }
                DocumentsCommand::Update {
                    database,
                    collection,
                    filter,
                    update,
                    upsert,
                } => Self::DocumentsUpdate {
                    database: non_empty(database, "database")?,
                    collection: non_empty(collection, "collection")?,
                    filter: ejson::parse_document(filter, "--filter")?,
                    update: ejson::parse_value(update, "--update")?,
                    upsert: *upsert,
                },
                DocumentsCommand::Delete {
                    database,
                    collection,
                    filter,
                } => Self::DocumentsDelete {
                    database: non_empty(database, "database")?,
                    collection: non_empty(collection, "collection")?,
                    filter: ejson::parse_document(filter, "--filter")?,
                },
            },
            Command::Explain { command } => match command {
                ExplainCommand::Find {
                    database,
                    collection,
                    query,
                    limit,
                    verbosity,
                } => Self::ExplainFind {
                    database: non_empty(database, "database")?,
                    collection: non_empty(collection, "collection")?,
                    query: find_query(query)?,
                    // 0 explains the query without a limit.
                    limit: (*limit > 0).then_some(*limit),
                    verbosity: explain_verbosity(verbosity),
                },
                ExplainCommand::Aggregate {
                    database,
                    collection,
                    pipeline,
                    verbosity,
                } => Self::ExplainAggregate {
                    database: non_empty(database, "database")?,
                    collection: non_empty(collection, "collection")?,
                    pipeline: read_pipeline(pipeline)?,
                    verbosity: explain_verbosity(verbosity),
                },
                ExplainCommand::Count {
                    database,
                    collection,
                    filter,
                    verbosity,
                } => Self::ExplainCount {
                    database: non_empty(database, "database")?,
                    collection: non_empty(collection, "collection")?,
                    filter: optional_document(filter.as_deref(), "--filter")?.unwrap_or_default(),
                    verbosity: explain_verbosity(verbosity),
                },
            },
            _ => unreachable!("local commands are dispatched before remote operations"),
        })
    }

    fn name(&self) -> &'static str {
        match self {
            Self::ServerInfo => "server.info",
            Self::ServerLogs { .. } => "server.logs",
            Self::AuthCheck => "auth.check",
            Self::DatabasesList => "databases.list",
            Self::DatabasesStats { .. } => "databases.stats",
            Self::DatabasesAggregate { .. } => "databases.aggregate",
            Self::DatabasesDrop { .. } => "databases.drop",
            Self::CollectionsList { .. } => "collections.list",
            Self::CollectionsSchema { .. } => "collections.schema",
            Self::CollectionsStats { .. } => "collections.stats",
            Self::CollectionsCreate { .. } => "collections.create",
            Self::CollectionsRename { .. } => "collections.rename",
            Self::CollectionsDrop { .. } => "collections.drop",
            Self::IndexesList { .. } => "indexes.list",
            Self::IndexesCreate { .. } => "indexes.create",
            Self::IndexesDrop { .. } => "indexes.drop",
            Self::DocumentsFind { .. } => "documents.find",
            Self::DocumentsCount { .. } => "documents.count",
            Self::DocumentsExport { .. } => "documents.export",
            Self::DocumentsAggregate { .. } => "documents.aggregate",
            Self::DocumentsInsert { .. } => "documents.insert",
            Self::DocumentsUpdate { .. } => "documents.update",
            Self::DocumentsDelete { .. } => "documents.delete",
            Self::ExplainFind { .. } => "explain.find",
            Self::ExplainAggregate { .. } => "explain.aggregate",
            Self::ExplainCount { .. } => "explain.count",
        }
    }

    /// The MongoDB MCP server tool this operation replicates; commands it lacks keep their name.
    fn tool(&self) -> &'static str {
        match self {
            Self::ServerInfo | Self::AuthCheck => self.name(),
            Self::ServerLogs { .. } => "mongodb-logs",
            Self::DatabasesList => "list-databases",
            Self::DatabasesStats { .. } => "db-stats",
            Self::DatabasesAggregate { .. } => "aggregate-db",
            Self::DatabasesDrop { .. } => "drop-database",
            Self::CollectionsList { .. } => "list-collections",
            Self::CollectionsSchema { .. } => "collection-schema",
            Self::CollectionsStats { .. } => "collection-storage-size",
            Self::CollectionsCreate { .. } => "create-collection",
            Self::CollectionsRename { .. } => "rename-collection",
            Self::CollectionsDrop { .. } => "drop-collection",
            Self::IndexesList { .. } => "collection-indexes",
            Self::IndexesCreate { .. } => "create-index",
            Self::IndexesDrop { .. } => "drop-index",
            Self::DocumentsFind { .. } => "find",
            Self::DocumentsCount { .. } => "count",
            Self::DocumentsExport { .. } => "export",
            Self::DocumentsAggregate { .. } => "aggregate",
            Self::DocumentsInsert { .. } => "insert-many",
            Self::DocumentsUpdate { .. } => "update-many",
            Self::DocumentsDelete { .. } => "delete-many",
            Self::ExplainFind { .. }
            | Self::ExplainAggregate { .. }
            | Self::ExplainCount { .. } => "explain",
        }
    }

    /// The MCP server's operation type: read, metadata, create, update, or delete.
    fn operation_type(&self) -> &'static str {
        match self.tool() {
            "find" | "count" | "aggregate" | "aggregate-db" | "export" => "read",
            "create-collection" | "create-index" | "insert-many" => "create",
            "rename-collection" | "update-many" => "update",
            "delete-many" | "drop-database" | "drop-collection" | "drop-index" => "delete",
            _ => "metadata",
        }
    }

    /// Whether a disabled-tools or confirmation-required-tools entry selects this operation:
    /// its tool name, command name, operation type, or the category `mongodb`.
    fn matches(&self, entry: &str) -> bool {
        ["mongodb", self.tool(), self.name(), self.operation_type()].contains(&entry)
    }

    /// The aggregation pipeline this operation runs, exports, or explains.
    fn pipeline(&self) -> Option<&[Document]> {
        match self {
            Self::DatabasesAggregate { pipeline, .. }
            | Self::DocumentsAggregate { pipeline, .. }
            | Self::ExplainAggregate { pipeline, .. }
            | Self::DocumentsExport {
                source: ExportSource::Aggregate { pipeline },
                ..
            } => Some(pipeline),
            _ => None,
        }
    }

    fn has_write_stage(&self) -> bool {
        self.pipeline()
            .is_some_and(|pipeline| guard::write_stage(pipeline).is_some())
    }

    /// Whether the operation can modify data; read-only connections refuse these.
    fn writes(&self) -> bool {
        matches!(self.operation_type(), "create" | "update" | "delete") || self.has_write_stage()
    }

    /// Whether the operation can destroy data and therefore requires --yes.
    fn destructive(&self) -> bool {
        match self {
            Self::DatabasesDrop { .. }
            | Self::CollectionsDrop { .. }
            | Self::IndexesDrop { .. }
            | Self::DocumentsDelete { .. } => true,
            Self::CollectionsRename { drop_target, .. } => *drop_target,
            Self::DatabasesAggregate { .. } | Self::DocumentsAggregate { .. } => {
                self.has_write_stage()
            }
            _ => false,
        }
    }

    /// What the `$out`/`$merge` stages of an aggregation would write.
    fn write_targets(&self) -> Vec<String> {
        match self {
            Self::DatabasesAggregate {
                database, pipeline, ..
            }
            | Self::DocumentsAggregate {
                database, pipeline, ..
            } => guard::write_targets(pipeline, database),
            _ => Vec::new(),
        }
    }

    /// The first server-side JavaScript operator in the inputs the MCP server scans; update
    /// documents are scanned too because pipeline updates can run `$function`.
    fn server_side_js(&self) -> Option<&'static str> {
        let find = |query: &FindQuery| {
            let mut scanned = vec![Bson::Document(query.filter.clone())];
            scanned.extend(query.projection.clone().map(Bson::Document));
            scanned
        };
        let scanned = match self {
            Self::DocumentsFind { query, .. } | Self::ExplainFind { query, .. } => find(query),
            Self::DocumentsExport {
                source: ExportSource::Find { query, .. },
                ..
            } => find(query),
            Self::DocumentsCount { filter, .. }
            | Self::ExplainCount { filter, .. }
            | Self::DocumentsDelete { filter, .. } => vec![Bson::Document(filter.clone())],
            Self::DocumentsUpdate { filter, update, .. } => {
                vec![Bson::Document(filter.clone()), update.clone()]
            }
            operation => operation.pipeline().map_or_else(Vec::new, |pipeline| {
                pipeline.iter().cloned().map(Bson::Document).collect()
            }),
        };
        guard::server_side_js(&Bson::Array(scanned))
    }

    /// The plan `--require-index` inspects, for the operations the MCP server's indexCheck covers.
    fn index_check(&self, settings: &Settings) -> Option<(&str, &str, IndexCheckTarget)> {
        let (database, collection, target) = match self {
            Self::DocumentsFind {
                database,
                collection,
                query,
                page,
            } => (
                database,
                collection,
                IndexCheckTarget::Find {
                    query: query.clone(),
                    page: page_request(page, settings, settings.query_count_max_time).0,
                },
            ),
            Self::DocumentsCount {
                database,
                collection,
                filter,
            } => (
                database,
                collection,
                IndexCheckTarget::Count {
                    filter: filter.clone(),
                },
            ),
            Self::DocumentsAggregate {
                database,
                collection,
                pipeline,
                ..
            } => (
                database,
                collection,
                IndexCheckTarget::Aggregate {
                    pipeline: pipeline.clone(),
                },
            ),
            Self::DocumentsUpdate {
                database,
                collection,
                filter,
                update,
                upsert,
            } => (
                database,
                collection,
                IndexCheckTarget::Update {
                    filter: filter.clone(),
                    update: update.clone(),
                    upsert: *upsert,
                },
            ),
            Self::DocumentsDelete {
                database,
                collection,
                filter,
            } => (
                database,
                collection,
                IndexCheckTarget::Delete {
                    filter: filter.clone(),
                },
            ),
            _ => return None,
        };
        Some((database.as_str(), collection.as_str(), target))
    }

    async fn execute(
        self,
        client: &MongoClient,
        settings: &Settings,
    ) -> Result<(Value, Meta), AppError> {
        match self {
            Self::ServerInfo => plain(client.server_info().await?),
            Self::ServerLogs { log_type, limit } => {
                plain(client.server_logs(log_type, limit).await?)
            }
            Self::AuthCheck => plain(client.connection_status().await?),
            Self::DatabasesList => listed(client.list_databases().await?),
            Self::DatabasesStats { database } => plain(client.database_stats(&database).await?),
            Self::DatabasesAggregate {
                database,
                pipeline,
                page,
            } => aggregated(client, &database, None, pipeline, &page, settings).await,
            Self::DatabasesDrop { database } => {
                client.drop_database(&database).await?;
                plain(json!({"database": database, "dropped": true}))
            }
            Self::CollectionsList { database } => listed(client.list_collections(&database).await?),
            Self::CollectionsSchema {
                database,
                collection,
                sample_size,
                response_bytes_limit,
            } => {
                let (max_bytes, _) = byte_limit(response_bytes_limit, settings.max_bytes_per_query);
                plain(
                    client
                        .sample_schema(&database, &collection, sample_size, max_bytes)
                        .await?,
                )
            }
            Self::CollectionsStats {
                database,
                collection,
            } => plain(client.collection_stats(&database, &collection).await?),
            Self::CollectionsCreate {
                database,
                collection,
                options,
            } => {
                client
                    .create_collection(&database, &collection, options)
                    .await?;
                plain(json!({"database": database, "collection": collection, "created": true}))
            }
            Self::CollectionsRename {
                database,
                collection,
                new_name,
                drop_target,
            } => {
                client
                    .rename_collection(&database, &collection, &new_name, drop_target)
                    .await?;
                plain(json!({
                    "database": database,
                    "collection": collection,
                    "newName": new_name,
                    "renamed": true,
                }))
            }
            Self::CollectionsDrop {
                database,
                collection,
            } => {
                client.drop_collection(&database, &collection).await?;
                plain(json!({"database": database, "collection": collection, "dropped": true}))
            }
            Self::IndexesList {
                database,
                collection,
                index_type,
            } => match index_type {
                IndexListType::Classic => {
                    listed(client.list_indexes(&database, &collection).await?)
                }
                IndexListType::Search => {
                    listed(client.list_search_indexes(&database, &collection).await?)
                }
                IndexListType::All => {
                    let classic = client.list_indexes(&database, &collection).await?;
                    let mut meta = Meta::default();
                    let search = match client.list_search_indexes(&database, &collection).await {
                        Err(error) if error.code() == ErrorCode::FeatureUnsupported => {
                            meta.warnings
                                .push(format!("search indexes are unavailable: {error}"));
                            Vec::new()
                        }
                        search => search?,
                    };
                    Ok((json!({"classic": classic, "search": search}), meta))
                }
            },
            Self::IndexesCreate {
                database,
                collection,
                index,
            } => plain(client.create_index(&database, &collection, index).await?),
            Self::IndexesDrop {
                database,
                collection,
                name,
                search,
            } => {
                client
                    .drop_index(&database, &collection, &name, search)
                    .await?;
                plain(json!({
                    "database": database,
                    "collection": collection,
                    "name": name,
                    "type": if search { "search" } else { "classic" },
                    "dropped": true,
                }))
            }
            Self::DocumentsFind {
                database,
                collection,
                query,
                page,
            } => {
                let (page, limits) = page_request(&page, settings, settings.query_count_max_time);
                Ok(paged(
                    page,
                    limits,
                    client.find(&database, &collection, query, page).await?,
                ))
            }
            Self::DocumentsCount {
                database,
                collection,
                filter,
            } => plain(json!({"count": client.count(&database, &collection, filter).await?})),
            Self::DocumentsExport {
                database,
                collection,
                source,
                canonical,
                out,
            } => match out {
                // The array itself is the output, like a completion script.
                None => {
                    let mut stdout = io::BufWriter::new(io::stdout());
                    client
                        .export(&database, &collection, source, canonical, &mut stdout)
                        .await?;
                    plain(Value::Null)
                }
                Some(path) => {
                    let documents =
                        export_to_file(client, &database, &collection, source, canonical, &path)
                            .await?;
                    plain(json!({
                        "path": path.to_string_lossy(),
                        "documents": documents,
                        "format": if canonical { "canonical" } else { "relaxed" },
                    }))
                }
            },
            Self::DocumentsAggregate {
                database,
                collection,
                pipeline,
                page,
            } => {
                aggregated(
                    client,
                    &database,
                    Some(&collection),
                    pipeline,
                    &page,
                    settings,
                )
                .await
            }
            Self::DocumentsInsert {
                database,
                collection,
                documents,
            } => plain(
                client
                    .insert_many(&database, &collection, documents)
                    .await?,
            ),
            Self::DocumentsUpdate {
                database,
                collection,
                filter,
                update,
                upsert,
            } => plain(
                client
                    .update_many(&database, &collection, filter, update, upsert)
                    .await?,
            ),
            Self::DocumentsDelete {
                database,
                collection,
                filter,
            } => plain(client.delete_many(&database, &collection, filter).await?),
            Self::ExplainFind {
                database,
                collection,
                query,
                limit,
                verbosity,
            } => plain(
                client
                    .explain(
                        &database,
                        &collection,
                        ExplainTarget::Find { query, limit },
                        verbosity,
                    )
                    .await?,
            ),
            Self::ExplainAggregate {
                database,
                collection,
                pipeline,
                verbosity,
            } => plain(
                client
                    .explain(
                        &database,
                        &collection,
                        ExplainTarget::Aggregate { pipeline },
                        verbosity,
                    )
                    .await?,
            ),
            Self::ExplainCount {
                database,
                collection,
                filter,
                verbosity,
            } => plain(
                client
                    .explain(
                        &database,
                        &collection,
                        ExplainTarget::Count { filter },
                        verbosity,
                    )
                    .await?,
            ),
        }
    }
}

fn plain(data: Value) -> Result<(Value, Meta), AppError> {
    Ok((data, Meta::default()))
}

fn listed(items: Vec<Value>) -> Result<(Value, Meta), AppError> {
    let total = items.len() as u64;
    Ok((
        Value::Array(items),
        Meta {
            total: Some(total),
            ..Meta::default()
        },
    ))
}

async fn aggregated(
    client: &MongoClient,
    database: &str,
    collection: Option<&str>,
    pipeline: Vec<Document>,
    page: &PageArgs,
    settings: &Settings,
) -> Result<(Value, Meta), AppError> {
    // $out and $merge must stay last, so write pipelines run unpaged.
    if guard::write_stage(&pipeline).is_some() {
        let write_targets = guard::write_targets(&pipeline, database);
        let result = client
            .aggregate(database, collection, pipeline, None)
            .await?;
        return Ok((
            Value::Array(result.documents),
            Meta {
                write_targets,
                warnings: result.warnings,
                ..Meta::default()
            },
        ));
    }
    let (page, limits) = page_request(page, settings, settings.aggregation_count_max_time);
    let result = client
        .aggregate(database, collection, pipeline, Some(page))
        .await?;
    Ok(paged(page, limits, result))
}

/// The settings that can shorten a page, named as `meta.appliedLimits` reports them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PageLimits {
    /// Set when maxDocumentsPerQuery lowered --limit.
    documents: Option<&'static str>,
    /// The source of the byte limit; reported only when the page was truncated.
    bytes: Option<&'static str>,
}

/// Cap the requested page by the connection's settings, as the MCP server caps its tools.
fn page_request(
    page: &PageArgs,
    settings: &Settings,
    count_time_cap: Duration,
) -> (PageRequest, PageLimits) {
    let cap = settings.max_documents_per_query;
    let limit = if cap > 0 {
        page.limit.min(cap)
    } else {
        page.limit
    };
    let (max_bytes, bytes) = byte_limit(page.response_bytes_limit, settings.max_bytes_per_query);
    (
        PageRequest {
            limit,
            offset: page.offset,
            max_bytes,
            count_time_cap,
        },
        PageLimits {
            documents: (limit < page.limit).then_some("maxDocumentsPerQuery"),
            bytes,
        },
    )
}

/// The smaller positive byte limit and its source; when both are 0 nothing limits the bytes.
fn byte_limit(
    response_bytes_limit: u64,
    max_bytes_per_query: u64,
) -> (usize, Option<&'static str>) {
    [
        (response_bytes_limit, "responseBytesLimit"),
        (max_bytes_per_query, "maxBytesPerQuery"),
    ]
    .into_iter()
    .filter(|(bytes, _)| *bytes > 0)
    .min_by_key(|(bytes, _)| *bytes)
    .map_or((usize::MAX, None), |(bytes, source)| {
        (usize::try_from(bytes).unwrap_or(usize::MAX), Some(source))
    })
}

fn paged(page: PageRequest, limits: PageLimits, result: Page) -> (Value, Meta) {
    let returned = u32::try_from(result.documents.len()).unwrap_or(u32::MAX);
    let mut warnings = result.warnings;
    if limits.documents.is_some() {
        warnings.push(format!(
            "the page was capped at {} documents by maxDocumentsPerQuery",
            page.limit
        ));
    }
    let meta = Meta {
        limit: Some(page.limit),
        offset: Some(page.offset),
        total: result.total,
        has_more: Some(result.has_more),
        next_offset: result
            .has_more
            .then(|| page.offset.saturating_add(returned)),
        applied_limits: limits
            .documents
            .into_iter()
            .chain(limits.bytes.filter(|_| result.truncated))
            .map(str::to_owned)
            .collect(),
        warnings,
        ..Meta::default()
    };
    (Value::Array(result.documents), meta)
}

/// Export beside `path` and rename into place, so a failed export leaves no partial file.
async fn export_to_file(
    client: &MongoClient,
    database: &str,
    collection: &str,
    source: ExportSource,
    canonical: bool,
    path: &Path,
) -> Result<u64, AppError> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(".tmp-{}", std::process::id()));
    let temporary = PathBuf::from(temporary);
    let unwritable = |path: &Path, error: io::Error| {
        AppError::validation(format!("cannot write {}: {error}", path.display()))
    };
    let mut file = io::BufWriter::new(
        fs::File::create(&temporary).map_err(|error| unwritable(&temporary, error))?,
    );
    let exported = client
        .export(database, collection, source, canonical, &mut file)
        .await;
    // Close the file before renaming or removing it.
    drop(file);
    let result = exported.map_err(AppError::from).and_then(|documents| {
        fs::rename(&temporary, path)
            .map(|()| documents)
            .map_err(|error| unwritable(path, error))
    });
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn new_index(
    index_type: NewIndexType,
    keys: &Option<String>,
    options: &Option<String>,
    definition: &Option<String>,
    name: &Option<String>,
) -> Result<NewIndex, AppError> {
    match index_type {
        NewIndexType::Classic => {
            if definition.is_some() {
                return Err(AppError::validation(
                    "--definition applies only to --type search or vector-search",
                ));
            }
            let keys = keys
                .as_deref()
                .ok_or_else(|| AppError::validation("--keys is required for a classic index"))?;
            Ok(NewIndex::Classic {
                keys: ejson::parse_document(keys, "--keys")?,
                name: name.clone(),
                options: optional_document(options.as_deref(), "--options")?.unwrap_or_default(),
            })
        }
        NewIndexType::Search | NewIndexType::VectorSearch => {
            if keys.is_some() || options.is_some() {
                return Err(AppError::validation(
                    "--keys and --options apply only to classic indexes",
                ));
            }
            let definition = definition.as_deref().ok_or_else(|| {
                AppError::validation("--definition is required for a search index")
            })?;
            Ok(NewIndex::Search {
                name: name.clone(),
                definition: ejson::parse_document(definition, "--definition")?,
                vector: index_type == NewIndexType::VectorSearch,
            })
        }
    }
}

fn find_query(query: &QueryArgs) -> Result<FindQuery, AppError> {
    Ok(FindQuery {
        filter: optional_document(query.filter.as_deref(), "--filter")?.unwrap_or_default(),
        projection: optional_document(query.projection.as_deref(), "--projection")?,
        sort: optional_document(query.sort.as_deref(), "--sort")?,
    })
}

fn optional_document(text: Option<&str>, label: &str) -> Result<Option<Document>, AppError> {
    text.map(|text| ejson::parse_document(text, label).map_err(AppError::from))
        .transpose()
}

fn read_pipeline(arguments: &PipelineArgs) -> Result<Vec<Document>, AppError> {
    Ok(match (&arguments.pipeline, &arguments.file) {
        (Some(text), _) => ejson::parse_document_array(text, "--pipeline")?,
        (None, Some(path)) => ejson::parse_document_array(&read_input(path)?, "--file")?,
        (None, None) => unreachable!("clap requires --pipeline or --file"),
    })
}

fn explain_verbosity(arguments: &VerbosityArgs) -> Verbosity {
    match arguments.verbosity {
        ExplainVerbosity::QueryPlanner => Verbosity::QueryPlanner,
        ExplainVerbosity::QueryPlannerExtended => Verbosity::QueryPlannerExtended,
        ExplainVerbosity::ExecutionStats => Verbosity::ExecutionStats,
        ExplainVerbosity::AllPlansExecution => Verbosity::AllPlansExecution,
    }
}

fn non_empty(value: &str, label: &str) -> Result<String, AppError> {
    if value.is_empty() {
        return Err(AppError::validation(format!(
            "the {label} must not be empty"
        )));
    }
    Ok(value.to_owned())
}

pub(crate) fn read_input(path: &Path) -> Result<String, AppError> {
    if path == Path::new("-") {
        let mut text = String::new();
        io::stdin()
            .read_to_string(&mut text)
            .map_err(|error| AppError::validation(error.to_string()))?;
        Ok(text)
    } else {
        fs::read_to_string(path).map_err(|error| {
            AppError::validation(format!("cannot read {}: {error}", path.display()))
        })
    }
}

fn local(command: &str, data: Value) -> DispatchResult {
    DispatchResult {
        command: command.to_owned(),
        data,
        meta: Meta::default(),
        failure_code: None,
        emitted: false,
    }
}

pub(crate) fn config_path() -> Result<PathBuf, AppError> {
    std::env::var_os("MONGODB_CLI_CONFIG")
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(|| default_config_path().map_err(AppError::config))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn cli(arguments: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("mongodb-cli").chain(arguments.iter().copied()))
            .expect("arguments parse")
    }

    fn operation(arguments: &[&str]) -> Result<Operation, AppError> {
        Operation::parse(&cli(arguments).command)
    }

    /// The checks `remote` runs before reading credentials, for a read-write profile.
    fn authorized(arguments: &[&str], settings: &Settings) -> Result<(), AppError> {
        let cli = cli(arguments);
        authorize(
            &Operation::parse(&cli.command)?,
            Some("prod"),
            Access::ReadWrite,
            settings,
            cli.yes,
        )
    }

    fn disabling(entries: &[&str]) -> Settings {
        Settings {
            disabled_tools: entries.iter().map(|entry| (*entry).to_owned()).collect(),
            ..Settings::default()
        }
    }

    #[test]
    fn reads_never_write_and_writes_are_classified() {
        for arguments in [
            vec!["documents", "find", "app", "users"],
            vec![
                "documents",
                "aggregate",
                "app",
                "users",
                "--pipeline",
                r#"[{"$match": {}}]"#,
            ],
            vec!["explain", "aggregate", "app", "users", "--pipeline", "[]"],
            vec!["collections", "schema", "app", "users"],
            vec![
                "databases",
                "aggregate",
                "admin",
                "--pipeline",
                r#"[{"$currentOp": {}}]"#,
            ],
            vec!["documents", "export", "app", "users"],
        ] {
            let operation = operation(&arguments).unwrap();
            assert!(!operation.writes(), "{arguments:?}");
            assert!(!operation.destructive(), "{arguments:?}");
        }
        for (arguments, destructive) in [
            (
                vec!["documents", "insert", "app", "users", "--documents", "{}"],
                false,
            ),
            (
                vec![
                    "documents",
                    "update",
                    "app",
                    "users",
                    "--filter",
                    "{}",
                    "--update",
                    r#"{"$set": {"a": 1}}"#,
                ],
                false,
            ),
            (vec!["collections", "create", "app", "users"], false),
            (vec!["collections", "rename", "app", "a", "b"], false),
            (
                vec!["indexes", "create", "app", "users", "--keys", r#"{"a": 1}"#],
                false,
            ),
            (
                vec!["collections", "rename", "app", "a", "b", "--drop-target"],
                true,
            ),
            (
                vec!["documents", "delete", "app", "users", "--filter", "{}"],
                true,
            ),
            (vec!["collections", "drop", "app", "users"], true),
            (vec!["databases", "drop", "app"], true),
            (vec!["indexes", "drop", "app", "users", "a_1"], true),
            (
                vec![
                    "documents",
                    "aggregate",
                    "app",
                    "users",
                    "--pipeline",
                    r#"[{"$out": "copy"}]"#,
                ],
                true,
            ),
        ] {
            let operation = operation(&arguments).unwrap();
            assert!(operation.writes(), "{arguments:?}");
            assert_eq!(operation.destructive(), destructive, "{arguments:?}");
            let refusal = authorized(&arguments, &Settings::default()).err();
            assert_eq!(refusal.is_some(), destructive, "{arguments:?}");
            if let Some(error) = refusal {
                assert_eq!(error.code, ErrorCode::Validation, "{arguments:?}");
            }
            // The global --yes confirms from any position.
            let confirmed = [&arguments[..], &["--yes"]].concat();
            assert!(authorized(&confirmed, &Settings::default()).is_ok());
            let leading = [&["--yes"][..], &arguments[..]].concat();
            assert!(authorized(&leading, &Settings::default()).is_ok());
        }
        assert!(
            operation(&[
                "explain",
                "aggregate",
                "app",
                "users",
                "--pipeline",
                r#"[{"$merge": "x"}]"#
            ])
            .unwrap()
            .writes(),
            "explaining a write pipeline still needs a read-write connection"
        );
    }

    #[test]
    fn operations_carry_mcp_tool_names_and_operation_types() {
        for (arguments, name, tool, kind) in [
            (
                vec!["server", "info"],
                "server.info",
                "server.info",
                "metadata",
            ),
            (
                vec!["server", "logs"],
                "server.logs",
                "mongodb-logs",
                "metadata",
            ),
            (
                vec!["auth", "check"],
                "auth.check",
                "auth.check",
                "metadata",
            ),
            (
                vec!["databases", "list"],
                "databases.list",
                "list-databases",
                "metadata",
            ),
            (
                vec!["databases", "stats", "app"],
                "databases.stats",
                "db-stats",
                "metadata",
            ),
            (
                vec![
                    "databases",
                    "aggregate",
                    "app",
                    "--pipeline",
                    r#"[{"$documents": []}]"#,
                ],
                "databases.aggregate",
                "aggregate-db",
                "read",
            ),
            (
                vec!["databases", "drop", "app"],
                "databases.drop",
                "drop-database",
                "delete",
            ),
            (
                vec!["collections", "list", "app"],
                "collections.list",
                "list-collections",
                "metadata",
            ),
            (
                vec!["collections", "schema", "app", "c"],
                "collections.schema",
                "collection-schema",
                "metadata",
            ),
            (
                vec!["collections", "stats", "app", "c"],
                "collections.stats",
                "collection-storage-size",
                "metadata",
            ),
            (
                vec!["collections", "create", "app", "c"],
                "collections.create",
                "create-collection",
                "create",
            ),
            (
                vec!["collections", "rename", "app", "c", "d"],
                "collections.rename",
                "rename-collection",
                "update",
            ),
            (
                vec!["collections", "drop", "app", "c"],
                "collections.drop",
                "drop-collection",
                "delete",
            ),
            (
                vec!["indexes", "list", "app", "c"],
                "indexes.list",
                "collection-indexes",
                "metadata",
            ),
            (
                vec!["indexes", "create", "app", "c", "--keys", r#"{"a": 1}"#],
                "indexes.create",
                "create-index",
                "create",
            ),
            (
                vec!["indexes", "drop", "app", "c", "a_1"],
                "indexes.drop",
                "drop-index",
                "delete",
            ),
            (
                vec!["documents", "find", "app", "c"],
                "documents.find",
                "find",
                "read",
            ),
            (
                vec!["documents", "count", "app", "c"],
                "documents.count",
                "count",
                "read",
            ),
            (
                vec!["documents", "export", "app", "c"],
                "documents.export",
                "export",
                "read",
            ),
            (
                vec!["documents", "aggregate", "app", "c", "--pipeline", "[]"],
                "documents.aggregate",
                "aggregate",
                "read",
            ),
            (
                vec!["documents", "insert", "app", "c", "--documents", "{}"],
                "documents.insert",
                "insert-many",
                "create",
            ),
            (
                vec![
                    "documents",
                    "update",
                    "app",
                    "c",
                    "--filter",
                    "{}",
                    "--update",
                    "{}",
                ],
                "documents.update",
                "update-many",
                "update",
            ),
            (
                vec!["documents", "delete", "app", "c", "--filter", "{}"],
                "documents.delete",
                "delete-many",
                "delete",
            ),
            (
                vec!["explain", "find", "app", "c"],
                "explain.find",
                "explain",
                "metadata",
            ),
            (
                vec!["explain", "aggregate", "app", "c", "--pipeline", "[]"],
                "explain.aggregate",
                "explain",
                "metadata",
            ),
            (
                vec!["explain", "count", "app", "c"],
                "explain.count",
                "explain",
                "metadata",
            ),
        ] {
            let operation = operation(&arguments).unwrap();
            assert_eq!(
                (
                    operation.name(),
                    operation.tool(),
                    operation.operation_type()
                ),
                (name, tool, kind),
                "{arguments:?}"
            );
            for entry in [name, tool, kind, "mongodb"] {
                assert!(operation.matches(entry), "{arguments:?} {entry}");
            }
        }
    }

    #[test]
    fn disabled_tools_match_names_types_and_category_and_refuse_linked_writes() {
        let find = ["documents", "find", "app", "users"];
        for entry in ["find", "documents.find", "read", "mongodb"] {
            let error = authorized(&find, &disabling(&[entry])).unwrap_err();
            assert_eq!(error.code, ErrorCode::Forbidden, "{entry}");
            assert!(error.message.contains(entry), "{entry}: {}", error.message);
            assert!(!error.hint.unwrap().contains("--profile"));
        }
        for entry in [
            "count",
            "documents.count",
            "metadata",
            "create",
            "documents",
        ] {
            assert!(authorized(&find, &disabling(&[entry])).is_ok(), "{entry}");
        }
        // Like the MCP server, disabling any write type refuses pipelines that write.
        let out = [
            "documents",
            "aggregate",
            "app",
            "users",
            "--pipeline",
            r#"[{"$out": "copy"}]"#,
            "--yes",
        ];
        let explained = [
            "explain",
            "aggregate",
            "app",
            "users",
            "--pipeline",
            r#"[{"$merge": "copy"}]"#,
        ];
        let read_pipeline = [
            "documents",
            "aggregate",
            "app",
            "users",
            "--pipeline",
            r#"[{"$match": {}}]"#,
        ];
        for kind in ["create", "update", "delete"] {
            for arguments in [&out[..], &explained[..]] {
                let error = authorized(arguments, &disabling(&[kind])).unwrap_err();
                assert_eq!(error.code, ErrorCode::Forbidden);
                assert!(
                    error.message.contains("$out or $merge"),
                    "{}",
                    error.message
                );
            }
            assert!(authorized(&read_pipeline, &disabling(&[kind])).is_ok());
        }
        let rename = ["collections", "rename", "app", "a", "b", "--yes"];
        assert!(authorized(&rename, &disabling(&["delete"])).is_ok());
        let error = authorized(
            &[&rename[..], &["--drop-target"]].concat(),
            &disabling(&["delete"]),
        )
        .unwrap_err();
        assert!(error.message.contains("--drop-target"), "{}", error.message);
        // Disabled tools are refused before the read-only check.
        let cli = cli(&["documents", "insert", "app", "users", "--documents", "{}"]);
        let error = authorize(
            &Operation::parse(&cli.command).unwrap(),
            Some("prod"),
            Access::ReadOnly,
            &disabling(&["insert-many"]),
            false,
        )
        .unwrap_err();
        assert!(error.message.contains("is disabled"), "{}", error.message);
    }

    #[test]
    fn confirmation_required_tools_add_yes_requirements_with_the_same_matching() {
        let insert = ["documents", "insert", "app", "users", "--documents", "{}"];
        assert!(authorized(&insert, &Settings::default()).is_ok());
        for entry in ["insert-many", "documents.insert", "create", "mongodb"] {
            let settings = Settings {
                confirmation_required_tools: vec![entry.into()],
                ..Settings::default()
            };
            let error = authorized(&insert, &settings).unwrap_err();
            assert_eq!(error.code, ErrorCode::Validation, "{entry}");
            assert!(error.message.contains(entry), "{entry}: {}", error.message);
            assert!(authorized(&[&insert[..], &["--yes"]].concat(), &settings).is_ok());
        }
        // A write pipeline's refusal says what it would overwrite.
        let error = authorized(
            &[
                "documents",
                "aggregate",
                "app",
                "users",
                "--pipeline",
                r#"[{"$match": {}}, {"$out": "copy"}]"#,
            ],
            &Settings::default(),
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(
            error
                .message
                .contains("$out replaces every document in app.copy"),
            "{}",
            error.message
        );
    }

    #[test]
    fn server_side_javascript_is_refused_before_connecting_unless_allowed() {
        let javascript = r#"{"$where": "this.a > 1"}"#;
        let function =
            r#"[{"$project": {"x": {"$function": {"body": "f", "args": [], "lang": "js"}}}}]"#;
        for arguments in [
            vec!["documents", "find", "app", "users", "--filter", javascript],
            vec![
                "documents",
                "find",
                "app",
                "users",
                "--projection",
                r#"{"x": {"$function": {}}}"#,
            ],
            vec!["documents", "count", "app", "users", "--filter", javascript],
            vec!["explain", "find", "app", "users", "--filter", javascript],
            vec!["explain", "count", "app", "users", "--filter", javascript],
            vec![
                "documents",
                "aggregate",
                "app",
                "users",
                "--pipeline",
                function,
            ],
            vec![
                "databases",
                "aggregate",
                "app",
                "--pipeline",
                r#"[{"$documents": [{"a": 1}]}, {"$match": {"$where": "true"}}]"#,
            ],
            vec![
                "explain",
                "aggregate",
                "app",
                "users",
                "--pipeline",
                function,
            ],
            vec![
                "documents",
                "export",
                "app",
                "users",
                "--filter",
                javascript,
            ],
            vec![
                "documents",
                "export",
                "app",
                "users",
                "--pipeline",
                function,
            ],
            vec![
                "documents",
                "update",
                "app",
                "users",
                "--filter",
                javascript,
                "--update",
                r#"{"$set": {"a": 1}}"#,
            ],
            vec![
                "documents",
                "update",
                "app",
                "users",
                "--filter",
                "{}",
                "--update",
                r#"[{"$set": {"a": {"$function": {}}}}]"#,
            ],
            vec![
                "documents",
                "delete",
                "app",
                "users",
                "--filter",
                javascript,
                "--yes",
            ],
        ] {
            let error = authorized(&arguments, &Settings::default()).unwrap_err();
            assert_eq!(error.code, ErrorCode::Forbidden, "{arguments:?}");
            assert!(
                error
                    .message
                    .starts_with("server-side JavaScript is disabled: the \"$"),
                "{arguments:?}: {}",
                error.message
            );
            assert!(!error.hint.unwrap().contains("profile"), "{arguments:?}");
            let allowed = Settings {
                server_side_js: true,
                ..Settings::default()
            };
            assert!(authorized(&arguments, &allowed).is_ok(), "{arguments:?}");
        }
        let error = authorized(
            &["documents", "find", "app", "users", "--filter", javascript],
            &Settings::default(),
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            r#"server-side JavaScript is disabled: the "$where" operator is not allowed"#
        );
        // Inserted documents and sort specifications are not scanned, as in the MCP server.
        for arguments in [
            vec![
                "documents",
                "insert",
                "app",
                "users",
                "--documents",
                javascript,
            ],
            vec![
                "documents",
                "find",
                "app",
                "users",
                "--sort",
                r#"{"$where": 1}"#,
            ],
        ] {
            assert!(
                authorized(&arguments, &Settings::default()).is_ok(),
                "{arguments:?}"
            );
        }
    }

    #[test]
    fn invalid_inputs_fail_before_any_connection() {
        for (arguments, expected) in [
            (
                vec!["documents", "find", "app", "users", "--filter", "{bad"],
                "--filter: invalid JSON",
            ),
            (
                vec!["documents", "find", "app", "users", "--sort", "[1]"],
                "--sort: expected a JSON object",
            ),
            (
                vec!["documents", "insert", "app", "users", "--documents", "[]"],
                "no documents",
            ),
            (
                vec!["indexes", "create", "app", "users"],
                "--keys is required",
            ),
            (
                vec![
                    "indexes", "create", "app", "users", "--type", "search", "--keys", "{}",
                ],
                "apply only to classic",
            ),
            (
                vec![
                    "indexes",
                    "create",
                    "app",
                    "users",
                    "--type",
                    "vector-search",
                ],
                "--definition is required",
            ),
            (
                vec![
                    "databases",
                    "aggregate",
                    "app",
                    "--pipeline",
                    r#"[{"$match": {}}]"#,
                ],
                "first stage",
            ),
            (vec!["documents", "count", "", "users"], "must not be empty"),
            (
                vec![
                    "documents",
                    "export",
                    "app",
                    "users",
                    "--pipeline",
                    r#"[{"$match": {}}, {"$merge": "copy"}]"#,
                ],
                "export cannot run pipelines with $out or $merge; use documents aggregate",
            ),
        ] {
            let error = operation(&arguments).err().expect("input is rejected");
            assert_eq!(error.code, ErrorCode::Validation, "{arguments:?}");
            assert!(
                error.message.contains(expected),
                "{arguments:?}: {}",
                error.message
            );
        }
    }

    #[test]
    fn explain_limits_default_to_ten_and_zero_sends_none() {
        for (arguments, expected) in [
            (vec!["explain", "find", "app", "users"], Some(10)),
            (
                vec!["explain", "find", "app", "users", "--limit", "0"],
                None,
            ),
            (
                vec!["explain", "find", "app", "users", "--limit", "3"],
                Some(3),
            ),
        ] {
            let Operation::ExplainFind { limit, .. } = operation(&arguments).unwrap() else {
                panic!("{arguments:?} is not an explained find");
            };
            assert_eq!(limit, expected, "{arguments:?}");
        }
        let Operation::ExplainCount { verbosity, .. } = operation(&[
            "explain",
            "count",
            "app",
            "users",
            "--verbosity",
            "query-planner-extended",
        ])
        .unwrap() else {
            panic!("not an explained count");
        };
        assert_eq!(verbosity, Verbosity::QueryPlannerExtended);
    }

    #[test]
    fn exports_stream_to_stdout_unless_a_path_is_given() {
        for (arguments, expected) in [
            (vec!["documents", "export", "app", "users"], None),
            (
                vec!["documents", "export", "app", "users", "--out", "-"],
                None,
            ),
            (
                vec!["documents", "export", "app", "users", "--out", "users.json"],
                Some(PathBuf::from("users.json")),
            ),
        ] {
            let Operation::DocumentsExport { out, canonical, .. } = operation(&arguments).unwrap()
            else {
                panic!("{arguments:?} is not an export");
            };
            assert_eq!(out, expected, "{arguments:?}");
            assert!(!canonical, "relaxed is the default format");
        }
    }

    #[test]
    fn read_only_refusals_name_the_connection_and_never_suggest_escalation() {
        let error = read_only_refusal("documents.delete", Some("prod"));
        assert_eq!(error.code, ErrorCode::Forbidden);
        assert!(error.message.contains("profile 'prod' is read-only"));
        assert!(error.hint.unwrap().contains("do not change"));
        let error = read_only_refusal("documents.insert", None);
        assert!(error.message.contains(URI_ENV));
    }

    #[test]
    fn profile_flags_become_changes_and_empty_lists_clear() {
        let changes = |arguments: &[&str]| {
            let Command::Profile {
                command: ProfileCommand::Update { settings, .. },
            } = cli(&[&["profile", "update", "prod"][..], arguments].concat()).command
            else {
                panic!("not a profile update");
            };
            profile_changes(&settings)
        };
        assert!(changes(&[]).is_empty());
        let cleared = changes(&["--disabled-tools", "", "--confirmation-required-tools", ""]);
        assert_eq!(cleared.disabled_tools, Some(Vec::new()));
        assert_eq!(cleared.confirmation_required_tools, Some(Vec::new()));
        let listed = changes(&["--disabled-tools", " delete, find,", "--index-check", "off"]);
        assert_eq!(
            listed.disabled_tools,
            Some(vec!["delete".to_owned(), "find".to_owned()])
        );
        assert_eq!(listed.index_check, Some(false));
        let mut profile = ProfileConfig::default();
        profile.apply(&listed);
        assert_eq!(
            profile_value("prod", &profile),
            json!({"name": "prod", "access": "read-only", "index_check": false, "disabled_tools": ["delete", "find"]})
        );
    }

    #[test]
    fn pages_are_capped_by_the_tighter_document_and_byte_limits() {
        let page = |limit, response_bytes_limit| PageArgs {
            limit,
            offset: 5,
            response_bytes_limit,
        };
        let cap = Duration::from_secs(10);
        let (request, limits) = page_request(&page(150, 1_048_576), &Settings::default(), cap);
        assert_eq!(
            request,
            PageRequest {
                limit: 100,
                offset: 5,
                max_bytes: 1_048_576,
                count_time_cap: cap,
            }
        );
        assert_eq!(
            limits,
            PageLimits {
                documents: Some("maxDocumentsPerQuery"),
                bytes: Some("responseBytesLimit"),
            }
        );
        let (request, limits) = page_request(&page(100, 0), &Settings::default(), cap);
        assert_eq!((request.limit, limits.documents), (100, None));
        assert_eq!(
            (request.max_bytes, limits.bytes),
            (16 * 1024 * 1024, Some("maxBytesPerQuery"))
        );
        let uncapped = Settings {
            max_documents_per_query: 0,
            ..Settings::default()
        };
        assert_eq!(page_request(&page(150, 1), &uncapped, cap).0.limit, 150);
        for (response, configured, expected) in [
            (
                1_048_576,
                16_777_216,
                (1_048_576, Some("responseBytesLimit")),
            ),
            (
                33_554_432,
                16_777_216,
                (16_777_216, Some("maxBytesPerQuery")),
            ),
            (0, 16_777_216, (16_777_216, Some("maxBytesPerQuery"))),
            (5, 0, (5, Some("responseBytesLimit"))),
            (7, 7, (7, Some("responseBytesLimit"))),
            (0, 0, (usize::MAX, None)),
        ] {
            assert_eq!(
                byte_limit(response, configured),
                expected,
                "{response} {configured}"
            );
        }
    }

    #[test]
    fn pages_report_continuation_and_the_limits_that_shortened_them() {
        let request = PageRequest {
            limit: 2,
            offset: 4,
            max_bytes: 100,
            count_time_cap: Duration::from_secs(10),
        };
        let both = PageLimits {
            documents: Some("maxDocumentsPerQuery"),
            bytes: Some("maxBytesPerQuery"),
        };
        let (_, meta) = paged(
            request,
            both,
            Page {
                documents: vec![json!({}), json!({})],
                has_more: true,
                truncated: true,
                total: Some(9),
                warnings: Vec::new(),
            },
        );
        assert_eq!(meta.next_offset, Some(6));
        assert_eq!(meta.total, Some(9));
        assert_eq!(meta.limit, Some(2));
        assert_eq!(
            meta.applied_limits,
            ["maxDocumentsPerQuery", "maxBytesPerQuery"]
        );
        assert_eq!(meta.warnings.len(), 1, "{:?}", meta.warnings);
        // A byte limit that did not cut the page is not reported.
        let (_, meta) = paged(
            request,
            PageLimits {
                documents: None,
                bytes: Some("responseBytesLimit"),
            },
            Page::default(),
        );
        assert_eq!(meta.has_more, Some(false));
        assert_eq!(meta.next_offset, None);
        assert!(meta.applied_limits.is_empty());
        assert!(meta.warnings.is_empty());
    }
}
