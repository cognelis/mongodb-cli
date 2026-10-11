//! Atlas Local deployments through the Docker Engine API, using the `atlas-local` crate that the
//! MongoDB MCP server's Atlas Local tools run.

use std::error::Error as _;
use std::future::Future;
use std::time::{Duration, Instant};

use atlas_local::bollard::errors::Error as BollardError;
use atlas_local::client::WatchDeploymentError;
use atlas_local::docker::RunCommandInContainerError;
use atlas_local::models::{
    CreateDeploymentOptions, CreationSource, GetLocalDeploymentLabelsError, ImageTag,
    IntoDeploymentError,
};
use atlas_local::{
    Client, CreateDeploymentError, DeleteDeploymentError, DockerError, GetConnectionStringError,
    GetDeploymentError,
};
use mongodb_client::{ConnectionUri, ErrorCode};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

use crate::args::{Access, Cli, DeploymentsCommand, LocalCommand};
use crate::config::{ConfigFile, SystemKeyring, validate_profile_name};
use crate::dispatch::{AppError, DispatchResult, config_path};
use crate::onboarding::{persist_credential, verify};
use crate::output::Meta;

/// The Voyage AI key passed to new deployments; read only from the environment.
pub const VOYAGE_API_KEY_ENV: &str = "MONGODB_CLI_VOYAGE_API_KEY";
/// Like the MCP server, wait up to 60 × 500 ms for Docker to publish the port.
const PORT_ATTEMPTS: u32 = 60;
const PORT_INTERVAL: Duration = Duration::from_millis(500);
const API_VERSION_HINT: &str =
    "Docker Engine may be older than API v1.53, which atlas-local 0.7.1 requires";

pub async fn run(cli: &Cli, command: &LocalCommand) -> Result<DispatchResult, AppError> {
    let LocalCommand::Deployments { command } = command;
    let started = Instant::now();
    let timeout = Duration::from_secs(cli.timeout);
    let (name, data, mut meta) = match command {
        DeploymentsCommand::List => {
            let client = docker()?;
            let deployments = bounded(timeout, client.list_deployments())
                .await?
                .map_err(|error| deployment_error(error, None))?;
            // Only the MCP tool's fields: the rest includes root passwords and API keys.
            let data: Vec<Value> = deployments
                .iter()
                .map(|deployment| {
                    json!({
                        "name": deployment.name,
                        "state": format!("{:?}", deployment.state),
                        "mongodbVersion": deployment.mongodb_version.to_string(),
                    })
                })
                .collect();
            let meta = Meta {
                total: Some(data.len() as u64),
                ..Meta::default()
            };
            ("local.deployments.list", Value::Array(data), meta)
        }
        DeploymentsCommand::Create {
            name,
            image_tag,
            load_sample_data,
            wait_timeout,
        } => {
            create(
                name.as_deref(),
                image_tag,
                *load_sample_data,
                Duration::from_secs(*wait_timeout),
            )
            .await?
        }
        DeploymentsCommand::Delete { name } => {
            validate_name(name)?;
            if !cli.yes {
                return Err(AppError::validation(
                    "local.deployments.delete is destructive and requires --yes",
                )
                .with_hint("check the deployment name, then rerun the same command with --yes"));
            }
            let client = docker()?;
            bounded(timeout, client.delete_deployment(name))
                .await?
                .map_err(|error| delete_error(error, name))?;
            let data = json!({"deleted": true, "deploymentName": name});
            ("local.deployments.delete", data, Meta::default())
        }
        DeploymentsCommand::Connect { name, access } => connect(cli, name, *access).await?,
    };
    meta.elapsed_ms = Some(started.elapsed().as_millis());
    Ok(DispatchResult {
        command: name.into(),
        data,
        meta,
        failure_code: None,
        emitted: false,
    })
}

async fn create(
    name: Option<&str>,
    image_tag: &str,
    load_sample_data: bool,
    wait: Duration,
) -> Result<(&'static str, Value, Meta), AppError> {
    if let Some(name) = name {
        validate_name(name)?;
    }
    let tag = ImageTag::try_from(image_tag).map_err(AppError::validation)?;
    let client = docker()?;
    let options = CreateDeploymentOptions {
        name: name.map(str::to_owned),
        image_tag: Some(tag),
        load_sample_data: Some(load_sample_data),
        voyage_api_key: std::env::var(VOYAGE_API_KEY_ENV)
            .ok()
            .filter(|key| !key.is_empty()),
        do_not_track: Some(true),
        creation_source: Some(CreationSource::Unknown("MONGODB_CLI".into())),
        wait_until_healthy_timeout: Some(wait),
        ..CreateDeploymentOptions::default()
    };
    let created = async {
        let deployment = client
            .create_deployment(options)
            .await
            .map_err(|error| create_error(error, name))?;
        let deployment_name = deployment
            .name
            .clone()
            .unwrap_or_else(|| deployment.container_id.clone());
        // Only whether the port is published matters here; the connection string is dropped.
        let published = wait_for_uri(
            || client.get_connection_string(deployment_name.clone()),
            PORT_INTERVAL,
        )
        .await
        .map_err(|error| connection_string_error(error, &deployment_name))?
        .is_some();
        Ok::<_, AppError>((deployment.container_id, deployment_name, published))
    };
    let (container_id, deployment_name, published) =
        tokio::time::timeout(wait, created).await.map_err(|_| {
            create_timeout(
                format!(
                    "the deployment was not ready within {} seconds",
                    wait.as_secs()
                ),
                name,
            )
        })??;
    let mut meta = Meta::default();
    if !published {
        meta.warnings.push(
            "The deployment is still initializing; if local deployments connect fails, wait a few seconds and try again."
                .into(),
        );
    }
    let data = json!({
        "deploymentName": deployment_name,
        "containerId": container_id,
        "loadSampleData": load_sample_data,
        "imageTag": image_tag,
    });
    Ok(("local.deployments.create", data, meta))
}

async fn connect(
    cli: &Cli,
    name: &str,
    access: Option<Access>,
) -> Result<(&'static str, Value, Meta), AppError> {
    validate_name(name)?;
    let profile = cli.profile.clone().unwrap_or_else(|| name.to_owned());
    validate_profile_name(&profile).map_err(|error| match cli.profile {
        Some(_) => AppError::validation(error),
        None => AppError::validation(error).with_hint("pass --profile <name> to name the profile"),
    })?;
    let path = config_path()?;
    let mut config = ConfigFile::load(&path).map_err(AppError::config)?;
    let new_access = profile_access(&config, &profile, access, cli.yes)?;
    let client = docker()?;
    let connected = async {
        let secret = wait_for_uri(
            || client.get_connection_string(name.to_owned()),
            PORT_INTERVAL,
        )
        .await
        .map_err(|error| connection_string_error(error, name))?
        .map(SecretString::from)
        .ok_or_else(|| still_starting(name))?;
        // atlas-local does not percent-encode credentials, so parsing can fail.
        let uri = ConnectionUri::parse(secret.expose_secret()).map_err(AppError::config)?;
        verify(&uri, cli.timeout).await?;
        Ok::<_, AppError>((secret, uri))
    };
    let (secret, uri) = bounded(Duration::from_secs(cli.timeout), connected).await??;
    let target = uri.display().to_owned();
    persist_credential(
        &mut config,
        &path,
        &SystemKeyring,
        &profile,
        &secret,
        |config| match new_access {
            Some(access) => config.upsert_profile(&profile, access, &target),
            None => config.set_target(&profile, &target),
        },
    )?;
    let access = config.profiles[&profile].access;
    let data = json!({
        "connected": true,
        "deploymentName": name,
        "profile": profile,
        "access": access,
        "target": target,
    });
    let meta = Meta {
        profile: Some(profile),
        access: Some(access),
        ..Meta::default()
    };
    Ok(("local.deployments.connect", data, meta))
}

/// The access mode for a new profile, or `None` to keep an existing profile's mode. Replacing an
/// existing profile's connection string requires --yes, and connect never raises its access.
fn profile_access(
    config: &ConfigFile,
    profile: &str,
    access: Option<Access>,
    yes: bool,
) -> Result<Option<Access>, AppError> {
    let Some(existing) = config.profiles.get(profile) else {
        return Ok(Some(access.unwrap_or_default()));
    };
    match access {
        Some(Access::ReadWrite) if existing.access == Access::ReadOnly => Err(AppError::new(
            ErrorCode::Forbidden,
            format!("raising profile '{profile}' to read-write requires confirmation in an interactive terminal"),
        )
        .with_hint(format!(
            "ask the user to run `mongodb-cli profile update {profile} --access read-write` in their own terminal"
        ))),
        Some(_) => Err(
            AppError::validation("--access applies only when connect creates a profile")
                .with_hint(format!("use `mongodb-cli profile update {profile} --access <mode>`")),
        ),
        None if !yes => Err(AppError::validation(format!(
            "profile '{profile}' already exists; replacing its connection string requires --yes"
        ))
        .with_hint("check the profile name, then rerun the same command with --yes")),
        None => Ok(None),
    }
}

/// Wait for Docker to publish the port like the MCP server, retrying only while the binding is
/// missing. `None` means the deployment is still starting.
async fn wait_for_uri<F, Fut>(
    mut get: F,
    interval: Duration,
) -> Result<Option<String>, GetConnectionStringError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<String, GetConnectionStringError>>,
{
    let mut attempt = 1;
    loop {
        match get().await {
            Err(GetConnectionStringError::MissingPortBinding) if attempt < PORT_ATTEMPTS => {
                attempt += 1;
                tokio::time::sleep(interval).await;
            }
            Err(GetConnectionStringError::MissingPortBinding) => return Ok(None),
            other => return other.map(Some),
        }
    }
}

/// Docker's container name rule, checked before contacting Docker; the crate puts names in URL paths.
fn validate_name(name: &str) -> Result<(), AppError> {
    let mut characters = name.chars();
    let valid = characters
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric())
        && name.len() > 1
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-')
        });
    if valid {
        Ok(())
    } else {
        Err(AppError::validation(format!(
            "invalid deployment name '{name}': use two or more letters, digits, '_', '.', or '-', starting with a letter or digit"
        )))
    }
}

async fn bounded<T>(timeout: Duration, future: impl Future<Output = T>) -> Result<T, AppError> {
    tokio::time::timeout(timeout, future).await.map_err(|_| {
        AppError::new(
            ErrorCode::Timeout,
            format!(
                "the operation did not finish within {} seconds",
                timeout.as_secs()
            ),
        )
    })
}

fn docker() -> Result<Client, AppError> {
    Client::connect_with_defaults().map_err(connect_error)
}

fn connect_error(error: BollardError) -> AppError {
    match error {
        BollardError::SocketNotFoundError(_) => not_running(),
        BollardError::UnsupportedURISchemeError { uri } => {
            AppError::config(format!("DOCKER_HOST scheme is not supported: {uri}"))
        }
        other => AppError::config(format!("cannot use DOCKER_HOST: {other}")),
    }
}

fn not_running() -> AppError {
    AppError::new(
        ErrorCode::Network,
        "Docker is not running. Please start Docker and try again. Atlas Local tools require Docker to be running.",
    )
    .with_hint("mongodb-cli connects to $DOCKER_HOST or unix:///var/run/docker.sock; Docker contexts are not read")
}

fn not_found(name: &str) -> AppError {
    AppError::new(
        ErrorCode::NotFound,
        format!(
            "The Atlas Local deployment \"{name}\" was not found. Please check the deployment name or use \"mongodb-cli local deployments list\" to see available deployments."
        ),
    )
}

fn still_starting(name: &str) -> AppError {
    AppError::new(
        ErrorCode::Upstream,
        format!(
            "Atlas Local deployment \"{name}\" is still starting up. Wait a few seconds and run the same command again."
        ),
    )
}

fn create_timeout(message: String, name: Option<&str>) -> AppError {
    AppError::new(ErrorCode::Timeout, message).with_hint(format!(
        "it may still be starting: check `mongodb-cli local deployments list`, then remove it with `mongodb-cli local deployments delete {} --yes` if needed",
        name.unwrap_or("<name>")
    ))
}

/// Classify a Docker Engine failure; `message` is the full text of the failed operation.
fn docker_error(error: &DockerError, message: String) -> AppError {
    match error {
        // atlas-local flattens transport errors into text, so these two are recognized by it.
        DockerError::Other {
            status_code: None,
            message: detail,
        } if detail.contains("client error (Connect)") => not_running(),
        DockerError::Other {
            status_code: None,
            message: detail,
        } if detail.contains("Timeout error") => AppError::new(ErrorCode::Timeout, message),
        DockerError::NotFound => AppError::new(ErrorCode::NotFound, message),
        DockerError::BadRequest => {
            AppError::new(ErrorCode::RequestRejected, message).with_hint(API_VERSION_HINT)
        }
        DockerError::Conflict => AppError::new(ErrorCode::RequestRejected, message),
        DockerError::Unauthorized | DockerError::Forbidden => {
            AppError::new(ErrorCode::Forbidden, message)
        }
        DockerError::NotModified | DockerError::ServerError | DockerError::Other { .. } => {
            AppError::new(ErrorCode::Upstream, message)
        }
    }
}

fn deployment_error(error: GetDeploymentError, name: Option<&str>) -> AppError {
    let message = error.to_string();
    match (error, name) {
        (
            GetDeploymentError::ContainerInspect(DockerError::NotFound)
            | GetDeploymentError::IntoDeployment(IntoDeploymentError::LocalDeploymentLabels(
                GetLocalDeploymentLabelsError::NotALocalDeployment,
            )),
            Some(name),
        ) => not_found(name),
        (GetDeploymentError::ContainerInspect(error), _) => docker_error(&error, message),
        (GetDeploymentError::IntoDeployment(_), _) => {
            AppError::new(ErrorCode::IncompatibleResponse, message)
        }
    }
}

fn create_error(error: CreateDeploymentError, name: Option<&str>) -> AppError {
    let message = error.to_string();
    match error {
        CreateDeploymentError::ContainerAlreadyExists(_) => {
            AppError::new(ErrorCode::RequestRejected, message)
        }
        CreateDeploymentError::PullImage(pull) => {
            match pull.source().and_then(|source| source.downcast_ref()) {
                Some(DockerError::NotFound) => AppError::new(ErrorCode::NotFound, message)
                    .with_hint("check --image-tag against https://hub.docker.com/r/mongodb/mongodb-atlas-local/tags"),
                Some(error) => docker_error(error, message),
                None => AppError::new(ErrorCode::Upstream, message),
            }
        }
        CreateDeploymentError::CreateContainer(error)
        | CreateDeploymentError::ContainerInspect(error)
        | CreateDeploymentError::WatchDeployment(WatchDeploymentError::ContainerInspect(error)) => {
            docker_error(&error, message)
        }
        CreateDeploymentError::WatchDeployment(WatchDeploymentError::Timeout { .. }) => {
            create_timeout(message, name)
        }
        CreateDeploymentError::WatchDeployment(WatchDeploymentError::UnhealthyDeployment {
            deployment_name,
            ..
        })
        | CreateDeploymentError::UnhealthyDeployment(deployment_name) => {
            AppError::new(ErrorCode::Upstream, message)
                .with_hint(format!("inspect it with `docker logs {deployment_name}`"))
        }
        CreateDeploymentError::GetDeploymentError(error) => deployment_error(error, name),
        CreateDeploymentError::ReceiveDeployment(_) | CreateDeploymentError::InvalidImage(_) => {
            AppError::new(ErrorCode::Upstream, message)
        }
    }
}

fn delete_error(error: DeleteDeploymentError, name: &str) -> AppError {
    let message = error.to_string();
    match error {
        DeleteDeploymentError::GetDeployment(error) => deployment_error(error, Some(name)),
        DeleteDeploymentError::ContainerStop(error)
        | DeleteDeploymentError::ContainerRemove(error) => docker_error(&error, message),
    }
}

fn connection_string_error(error: GetConnectionStringError, name: &str) -> AppError {
    let message = error.to_string();
    match error {
        GetConnectionStringError::GetDeployment(error) => deployment_error(error, Some(name)),
        GetConnectionStringError::MissingPortBinding => still_starting(name),
        GetConnectionStringError::GetMongodbUsername(error)
        | GetConnectionStringError::GetMongodbPassword(error) => match error {
            RunCommandInContainerError::CreateExec(error)
            | RunCommandInContainerError::StartExec(error)
            | RunCommandInContainerError::GetOutputError(error) => docker_error(&error, message),
            RunCommandInContainerError::GetOutput => AppError::new(ErrorCode::Upstream, message),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use atlas_local::{ContainerHealthStatus, PullImageError};

    use super::*;

    fn connect_failure() -> DockerError {
        DockerError::Other {
            status_code: None,
            message: "Error in the hyper legacy client: client error (Connect)".into(),
        }
    }

    #[test]
    fn names_follow_docker_rules_and_tags_follow_the_crate() {
        for name in ["dev", "a1", "my-db_1.x", "0x"] {
            assert!(validate_name(name).is_ok(), "{name}");
        }
        for name in ["", "a", "-dev", ".dev", "my db", "dev/../x", "dév", "a?b"] {
            assert_eq!(
                validate_name(name).unwrap_err().code,
                ErrorCode::Validation,
                "{name}"
            );
        }
        for tag in [
            "preview",
            "latest",
            "8",
            "8.2",
            " 8.2.4 ",
            "8.2.4-20260217T084055Z",
        ] {
            assert!(ImageTag::try_from(tag).is_ok(), "{tag}");
        }
        for (tag, expected) in [
            ("9.x", "Invalid image tag"),
            ("v8", "Invalid image tag"),
            ("256.0.0", "Invalid image tag"),
            ("8.2.4-2026", "Invalid timestamp"),
        ] {
            assert!(
                ImageTag::try_from(tag).unwrap_err().starts_with(expected),
                "{tag}"
            );
        }
    }

    #[test]
    fn crate_errors_map_to_stable_categories() {
        let missing = || GetDeploymentError::ContainerInspect(DockerError::NotFound);
        let not_local = || {
            GetDeploymentError::IntoDeployment(IntoDeploymentError::LocalDeploymentLabels(
                GetLocalDeploymentLabelsError::NotALocalDeployment,
            ))
        };
        let cases = [
            (
                deployment_error(missing(), Some("dev")),
                ErrorCode::NotFound,
            ),
            (
                deployment_error(not_local(), Some("dev")),
                ErrorCode::NotFound,
            ),
            (deployment_error(missing(), None), ErrorCode::NotFound),
            (
                deployment_error(
                    GetDeploymentError::IntoDeployment(IntoDeploymentError::LocalDeploymentLabels(
                        GetLocalDeploymentLabelsError::MissingMongodbVersion,
                    )),
                    Some("dev"),
                ),
                ErrorCode::IncompatibleResponse,
            ),
            (
                create_error(
                    CreateDeploymentError::PullImage(PullImageError::from(DockerError::NotFound)),
                    None,
                ),
                ErrorCode::NotFound,
            ),
            (
                create_error(
                    CreateDeploymentError::PullImage(PullImageError::from(DockerError::BadRequest)),
                    None,
                ),
                ErrorCode::RequestRejected,
            ),
            (
                create_error(
                    CreateDeploymentError::ContainerAlreadyExists("dev".into()),
                    Some("dev"),
                ),
                ErrorCode::RequestRejected,
            ),
            (
                create_error(
                    CreateDeploymentError::WatchDeployment(WatchDeploymentError::Timeout {
                        deployment_name: "dev".into(),
                    }),
                    Some("dev"),
                ),
                ErrorCode::Timeout,
            ),
            (
                create_error(
                    CreateDeploymentError::WatchDeployment(
                        WatchDeploymentError::UnhealthyDeployment {
                            deployment_name: "dev".into(),
                            status: ContainerHealthStatus::Unhealthy,
                        },
                    ),
                    Some("dev"),
                ),
                ErrorCode::Upstream,
            ),
            (
                delete_error(
                    DeleteDeploymentError::ContainerStop(connect_failure()),
                    "dev",
                ),
                ErrorCode::Network,
            ),
            (
                delete_error(DeleteDeploymentError::GetDeployment(missing()), "dev"),
                ErrorCode::NotFound,
            ),
            (
                delete_error(
                    DeleteDeploymentError::ContainerRemove(DockerError::Other {
                        status_code: None,
                        message: "Timeout error".into(),
                    }),
                    "dev",
                ),
                ErrorCode::Timeout,
            ),
            (
                delete_error(
                    DeleteDeploymentError::ContainerRemove(DockerError::Unauthorized),
                    "dev",
                ),
                ErrorCode::Forbidden,
            ),
            (
                delete_error(
                    DeleteDeploymentError::ContainerRemove(DockerError::ServerError),
                    "dev",
                ),
                ErrorCode::Upstream,
            ),
            (
                connection_string_error(GetConnectionStringError::MissingPortBinding, "dev"),
                ErrorCode::Upstream,
            ),
            (
                connection_string_error(
                    GetConnectionStringError::GetMongodbPassword(
                        RunCommandInContainerError::CreateExec(connect_failure()),
                    ),
                    "dev",
                ),
                ErrorCode::Network,
            ),
            (
                connect_error(BollardError::SocketNotFoundError(
                    "/nonexistent.sock".into(),
                )),
                ErrorCode::Network,
            ),
            (
                connect_error(BollardError::UnsupportedURISchemeError {
                    uri: "ssh://x".into(),
                }),
                ErrorCode::Config,
            ),
        ];
        for (index, (error, code)) in cases.iter().enumerate() {
            assert_eq!(error.code, *code, "case {index}: {}", error.message);
        }
        assert!(cases[0].0.message.contains("local deployments list"));
        assert!(cases[4].0.hint.as_deref().unwrap().contains("--image-tag"));
        assert!(cases[5].0.hint.as_deref().unwrap().contains("v1.53"));
        assert_eq!(cases[6].0.message, "Container already exists: dev");
        assert!(
            cases[8]
                .0
                .hint
                .as_deref()
                .unwrap()
                .contains("docker logs dev")
        );
        assert!(cases[9].0.message.starts_with("Docker is not running"));
        assert!(cases[14].0.message.contains("still starting up"));
        assert_eq!(
            cases[17].0.message,
            "DOCKER_HOST scheme is not supported: ssh://x"
        );
    }

    #[tokio::test]
    async fn port_waits_retry_only_missing_bindings_and_give_up_after_sixty_attempts() {
        let calls = Cell::new(0);
        let found = wait_for_uri(
            || {
                calls.set(calls.get() + 1);
                std::future::ready(if calls.get() < 3 {
                    Err(GetConnectionStringError::MissingPortBinding)
                } else {
                    Ok("mongodb://127.0.0.1:1/".to_owned())
                })
            },
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(found.as_deref(), Some("mongodb://127.0.0.1:1/"));
        assert_eq!(calls.get(), 3);

        calls.set(0);
        let failed = wait_for_uri(
            || {
                calls.set(calls.get() + 1);
                std::future::ready(Err(GetConnectionStringError::GetDeployment(
                    GetDeploymentError::ContainerInspect(DockerError::NotFound),
                )))
            },
            Duration::ZERO,
        )
        .await;
        assert!(matches!(
            failed,
            Err(GetConnectionStringError::GetDeployment(_))
        ));
        assert_eq!(calls.get(), 1);

        calls.set(0);
        let starting = wait_for_uri(
            || {
                calls.set(calls.get() + 1);
                std::future::ready(Err(GetConnectionStringError::MissingPortBinding))
            },
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(starting, None);
        assert_eq!(calls.get(), PORT_ATTEMPTS);
    }

    #[test]
    fn connect_creates_read_only_profiles_and_needs_yes_to_replace_one() {
        let mut config = ConfigFile::default();
        config.add_profile("dev", Access::ReadOnly).unwrap();
        assert_eq!(
            profile_access(&config, "new", None, false).unwrap(),
            Some(Access::ReadOnly)
        );
        assert_eq!(
            profile_access(&config, "new", Some(Access::ReadWrite), false).unwrap(),
            Some(Access::ReadWrite)
        );
        assert_eq!(profile_access(&config, "dev", None, true).unwrap(), None);
        for (access, yes, code) in [
            (None, false, ErrorCode::Validation),
            (Some(Access::ReadOnly), true, ErrorCode::Validation),
            (Some(Access::ReadWrite), true, ErrorCode::Forbidden),
        ] {
            let error = profile_access(&config, "dev", access, yes).unwrap_err();
            assert_eq!(error.code, code, "{access:?}");
        }
    }
}
