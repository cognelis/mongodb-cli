use std::io::{self, IsTerminal, Read};
use std::path::Path;
use std::time::{Duration, Instant};

use mongodb_client::{ConnectionUri, ErrorCode, MongoClient};
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use serde_json::{Value, json};

use crate::args::{Access, OutputFormat, SkillTarget};
use crate::config::{ConfigFile, CredentialStore, validate_profile_name};
use crate::dispatch::{AppError, DispatchResult};
use crate::doctor::{self, DoctorReport};
use crate::output::Meta;
use crate::skill_install::{self, InstallResult};

const NON_INTERACTIVE_HINT: &str = "For automation, run `mongodb-cli profile add <name>`, then pipe the connection string to `mongodb-cli --profile <name> auth login --uri-stdin`";
const VERIFICATION_HINT: &str = "Check the connection string, network access, and TLS settings, or use --no-verify only when storing it offline is intentional";

struct SetupDraft {
    profile: String,
    secret: SecretString,
    uri: ConnectionUri,
    access: Access,
    skill_target: SkillChoice,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SkillChoice {
    All,
    Codex,
    Claude,
    Skip,
}

impl SkillChoice {
    fn label(self) -> &'static str {
        match self {
            Self::All => "Codex and Claude Code",
            Self::Codex => "Codex only",
            Self::Claude => "Claude Code only",
            Self::Skip => "Skip Skill installation",
        }
    }

    fn target(self) -> Option<SkillTarget> {
        match self {
            Self::All => Some(SkillTarget::All),
            Self::Codex => Some(SkillTarget::Codex),
            Self::Claude => Some(SkillTarget::Claude),
            Self::Skip => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ProfileChoice {
    Create,
    Existing(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VerificationFailureAction {
    Retry,
    Edit,
    Cancel,
}

pub(crate) struct Verification {
    server: Value,
    status: Value,
    elapsed_ms: u128,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitResult {
    profile: String,
    target: String,
    access: Access,
    verified: bool,
    skills: Vec<InstallResult>,
    doctor: Value,
}

pub struct LoginRequest<'a> {
    pub selected_profile: Option<&'a str>,
    pub uri_stdin: bool,
    pub no_verify: bool,
    pub timeout: u64,
    pub output: OutputFormat,
}

pub async fn run_init(
    selected_profile: Option<&str>,
    timeout: u64,
    config_path: &Path,
    store: &dyn CredentialStore,
) -> Result<DispatchResult, AppError> {
    if !interactive_terminal() {
        return Err(
            AppError::validation("`mongodb-cli init` requires an interactive terminal")
                .with_hint(NON_INTERACTIVE_HINT),
        );
    }

    let mut config = ConfigFile::load(config_path).map_err(AppError::config)?;
    prompt_io(cliclack::intro("MongoDB CLI setup"))?;

    let mut draft = prompt_setup(&config, selected_profile, None)?;
    let verification = loop {
        let spinner = cliclack::spinner();
        spinner.start("Verifying the MongoDB connection");
        match verify(&draft.uri, timeout).await {
            Ok(verification) => {
                spinner.stop("Connection verified");
                break verification;
            }
            Err(error) => {
                spinner.error(format!("Verification failed: {}", error.message));
                let action = prompt_io(
                    cliclack::select("How would you like to continue?")
                        .item(
                            VerificationFailureAction::Retry,
                            "Retry verification",
                            "Use the same connection string",
                        )
                        .item(
                            VerificationFailureAction::Edit,
                            "Edit connection details",
                            "Re-enter the profile, connection string, and access",
                        )
                        .item(
                            VerificationFailureAction::Cancel,
                            "Cancel setup",
                            "Do not save configuration",
                        )
                        .interact(),
                )?;
                match action {
                    VerificationFailureAction::Retry => {}
                    VerificationFailureAction::Edit => {
                        draft = prompt_setup(&config, Some(&draft.profile), Some(draft.access))?;
                    }
                    VerificationFailureAction::Cancel => return cancelled("Setup cancelled"),
                }
            }
        }
    };

    let skills = match draft.skill_target.target() {
        Some(target) => {
            let spinner = cliclack::spinner();
            spinner.start("Installing the bundled mongodb-cli Skills for coding agents");
            match skill_install::install(target) {
                Ok(results) => {
                    spinner.stop("Coding-agent Skills installed");
                    results
                }
                Err(error) => {
                    spinner.error("Skill installation failed");
                    return Err(AppError::config(error));
                }
            }
        }
        None => Vec::new(),
    };

    let (profile, access, target) = (
        draft.profile.clone(),
        draft.access,
        draft.uri.display().to_owned(),
    );
    persist_credential(
        &mut config,
        config_path,
        store,
        &profile,
        &draft.secret,
        |config| {
            config.upsert_profile(&profile, access, &target)?;
            config.use_profile(&profile)
        },
    )?;
    let mut report = DoctorReport::new(env!("CARGO_PKG_VERSION"));
    report.configuration_passed();
    report.credentials_passed("the connection string you entered");
    report.server_passed(doctor::server_message(&verification.server));
    report.authentication_passed(doctor::identity(&verification.status));
    let warnings = if access == Access::ReadOnly {
        doctor::read_only_role_warning(&verification.status)
            .into_iter()
            .collect()
    } else {
        Vec::new()
    };
    prompt_io(cliclack::outro(format!(
        "Ready — profile '{profile}' ({}) is active. Run `mongodb-cli doctor` to diagnose it anytime.",
        access.as_str()
    )))?;

    Ok(DispatchResult {
        command: "init".into(),
        data: serde_json::to_value(InitResult {
            profile,
            target,
            access,
            verified: true,
            skills,
            doctor: report.into_value(),
        })
        .expect("setup results contain only serializable values"),
        meta: Meta {
            profile: config.current_profile,
            access: Some(access),
            elapsed_ms: Some(verification.elapsed_ms),
            warnings,
            ..Meta::default()
        },
        failure_code: None,
        emitted: false,
    })
}

pub async fn run_login(
    request: LoginRequest<'_>,
    config: &mut ConfigFile,
    config_path: &Path,
    store: &dyn CredentialStore,
) -> Result<DispatchResult, AppError> {
    let interactive = interactive_terminal();
    let profile = select_login_profile(config, request.selected_profile, interactive)?;
    let access = config
        .profiles
        .get(&profile)
        .ok_or_else(|| AppError::config(format!("profile '{profile}' does not exist")))?
        .access;

    let secret = if request.uri_stdin {
        if io::stdin().is_terminal() {
            return Err(AppError::validation(
                "--uri-stdin expects a pipe or redirected standard input",
            )
            .with_hint("Pipe it with `printf '%s\\n' \"$MONGODB_URI\" | mongodb-cli auth login --uri-stdin`"));
        }
        SecretString::from(read_secret(&mut io::stdin())?)
    } else {
        if !interactive {
            return Err(AppError::validation(
                "interactive connection-string entry requires an interactive terminal",
            )
            .with_hint("Pipe the connection string to `mongodb-cli auth login --uri-stdin`"));
        }
        SecretString::from(prompt_io(
            cliclack::password(format!("Connection string for profile '{profile}'"))
                .validate(|value: &String| ConnectionUri::parse(value).map(|_| ()))
                .interact(),
        )?)
    };
    let uri = ConnectionUri::parse(secret.expose_secret()).map_err(AppError::validation)?;

    let verification = if request.no_verify {
        None
    } else if interactive && !request.uri_stdin && request.output != OutputFormat::Json {
        let spinner = cliclack::spinner();
        spinner.start("Verifying the MongoDB connection");
        match verify(&uri, request.timeout).await {
            Ok(verification) => {
                spinner.stop("Connection verified");
                Some(verification)
            }
            Err(error) => {
                spinner.error("Connection verification failed");
                return Err(error.with_hint(VERIFICATION_HINT));
            }
        }
    } else {
        Some(
            verify(&uri, request.timeout)
                .await
                .map_err(|error| error.with_hint(VERIFICATION_HINT))?,
        )
    };

    persist_credential(config, config_path, store, &profile, &secret, |config| {
        config.set_target(&profile, uri.display())
    })?;

    let mut meta = Meta {
        profile: Some(profile.clone()),
        access: Some(access),
        ..Meta::default()
    };
    if request.no_verify {
        meta.warnings.push(
            "connection string was stored without verification because --no-verify was explicitly set"
                .into(),
        );
    }
    if let Some(verification) = &verification {
        meta.elapsed_ms = Some(verification.elapsed_ms);
        if access == Access::ReadOnly {
            meta.warnings
                .extend(doctor::read_only_role_warning(&verification.status));
        }
    }
    Ok(DispatchResult {
        command: "auth.login".into(),
        data: json!({
            "profile": profile,
            "stored": true,
            "verified": verification.is_some(),
            "target": uri.display(),
            "users": verification.as_ref().map(|verification| verification.status["users"].clone()),
        }),
        meta,
        failure_code: None,
        emitted: false,
    })
}

/// Only a person at a terminal may loosen a profile's safety settings; agents run without one.
pub fn confirm_loosening(name: &str, loosened: &[String]) -> Result<(), AppError> {
    let summary = loosened.join("; ");
    if !interactive_terminal() {
        return Err(AppError::new(
            ErrorCode::Forbidden,
            format!("loosening profile '{name}' ({summary}) requires confirmation in an interactive terminal"),
        )
        .with_hint(format!(
            "ask the user to run the same `mongodb-cli profile update {name} ...` command in their own terminal"
        )));
    }
    let confirmed = prompt_io(
        cliclack::confirm(format!("Loosen profile '{name}': {summary}?"))
            .initial_value(false)
            .interact(),
    )?;
    if confirmed {
        Ok(())
    } else {
        Err(AppError::validation("profile change cancelled"))
    }
}

/// Only a person in an interactive terminal may move credentials into a plain-text file.
pub fn confirm_plaintext_store() -> Result<(), AppError> {
    if !interactive_terminal() {
        return Err(AppError::new(
            ErrorCode::Forbidden,
            "keeping credentials in a plain-text file requires confirmation in an interactive terminal",
        )
        .with_hint(
            "ask the user to run `mongodb-cli config set credential-store file` in their own terminal",
        ));
    }
    let confirmed = prompt_io(
        cliclack::confirm(
            "Keep connection strings and Atlas credentials in a plain-text file only you can read?",
        )
        .initial_value(false)
        .interact(),
    )?;
    if confirmed {
        Ok(())
    } else {
        Err(AppError::validation("credential store change cancelled"))
    }
}

fn prompt_setup(
    config: &ConfigFile,
    selected_profile: Option<&str>,
    default_access: Option<Access>,
) -> Result<SetupDraft, AppError> {
    let profile = prompt_profile(config, selected_profile)?;
    let secret = SecretString::from(prompt_io(
        cliclack::password("MongoDB connection string")
            .validate(|value: &String| ConnectionUri::parse(value).map(|_| ()))
            .interact(),
    )?);
    let uri = ConnectionUri::parse(secret.expose_secret()).map_err(AppError::validation)?;
    let initial_access = default_access
        .or_else(|| config.profiles.get(&profile).map(|profile| profile.access))
        .unwrap_or_default();
    let access = prompt_io(
        cliclack::select("What may this profile do?")
            .item(
                Access::ReadOnly,
                "Read-only",
                "Recommended; every write is refused",
            )
            .item(
                Access::ReadWrite,
                "Read-write",
                "Allows writes; destructive commands still need --yes",
            )
            .initial_value(initial_access)
            .interact(),
    )?;
    let skill_target = prompt_io(
        cliclack::select("Install the bundled mongodb-cli Skills for coding agents?")
            .item(
                SkillChoice::All,
                "Codex and Claude Code",
                "Recommended; installs them for both agents",
            )
            .item(SkillChoice::Codex, "Codex only", "Install for Codex")
            .item(
                SkillChoice::Claude,
                "Claude Code only",
                "Install for Claude Code",
            )
            .item(SkillChoice::Skip, "Skip", "Do not install Skills now")
            .initial_value(SkillChoice::All)
            .interact(),
    )?;

    prompt_io(cliclack::note(
        "Configuration summary",
        format!(
            "Profile: {profile}\nTarget: {}\nAccess: {}\nSkills: {}\nCredential: stored in the operating-system credential store",
            uri.display(),
            access.as_str(),
            skill_target.label()
        ),
    ))?;
    if !prompt_io(
        cliclack::confirm("Verify and save this configuration?")
            .initial_value(true)
            .interact(),
    )? {
        return cancelled("Setup cancelled; no configuration was saved");
    }

    Ok(SetupDraft {
        profile,
        secret,
        uri,
        access,
        skill_target,
    })
}

fn prompt_profile(config: &ConfigFile, selected_profile: Option<&str>) -> Result<String, AppError> {
    if let Some(name) = selected_profile {
        validate_profile_name(name).map_err(AppError::validation)?;
        return Ok(name.to_owned());
    }

    let choice = if config.profiles.is_empty() {
        ProfileChoice::Create
    } else {
        let mut prompt = cliclack::select("Which profile should be configured?").item(
            ProfileChoice::Create,
            "Create a new profile",
            "Add another MongoDB deployment",
        );
        for (name, profile) in &config.profiles {
            let hint = format!(
                "{} · {}",
                profile.access.as_str(),
                profile.target.as_deref().unwrap_or("no connection string")
            );
            prompt = prompt.item(ProfileChoice::Existing(name.clone()), name, hint);
        }
        if let Some(current) = config.current_profile.as_ref() {
            prompt = prompt.initial_value(ProfileChoice::Existing(current.clone()));
        }
        prompt_io(prompt.interact())?
    };

    match choice {
        ProfileChoice::Existing(name) => Ok(name),
        ProfileChoice::Create => prompt_io(
            cliclack::input("Profile name")
                .default_input("default")
                .validate(|value: &String| validate_profile_name(value))
                .interact(),
        ),
    }
}

pub(crate) fn select_login_profile(
    config: &ConfigFile,
    selected_profile: Option<&str>,
    interactive: bool,
) -> Result<String, AppError> {
    if let Some(name) = selected_profile {
        return Ok(name.to_owned());
    }
    if let Some(name) = &config.current_profile {
        return Ok(name.clone());
    }
    if config.profiles.len() == 1 {
        return Ok(config.profiles.keys().next().expect("one profile").clone());
    }
    if config.profiles.is_empty() {
        return Err(AppError::config("no profiles are configured")
            .with_hint("Run `mongodb-cli init` or `mongodb-cli profile add <name>`"));
    }
    if !interactive {
        return Err(
            AppError::config("multiple profiles exist and none is selected")
                .with_hint("Pass `--profile <name>` or run `mongodb-cli profile use <name>`"),
        );
    }

    let mut prompt = cliclack::select("Which profile should receive the connection string?");
    for (name, profile) in &config.profiles {
        prompt = prompt.item(name.clone(), name, profile.access.as_str());
    }
    prompt_io(prompt.interact())
}

pub(crate) async fn verify(uri: &ConnectionUri, timeout: u64) -> Result<Verification, AppError> {
    let started = Instant::now();
    let client = MongoClient::connect(uri, Duration::from_secs(timeout.max(1))).await?;
    client.ping().await?;
    let server = client.server_info().await?;
    let status = client.connection_status().await?;
    Ok(Verification {
        server,
        status,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

/// Store a connection string and the matching config change, or restore both. A connection string
/// without secrets is kept in the config file instead, so reading it never prompts.
pub(crate) fn persist_credential(
    config: &mut ConfigFile,
    config_path: &Path,
    store: &dyn CredentialStore,
    profile: &str,
    secret: &SecretString,
    update: impl FnOnce(&mut ConfigFile) -> Result<(), String>,
) -> Result<(), AppError> {
    let previous_config = config.clone();
    let secret_free = crate::config::is_secret_free(secret.expose_secret());
    // The previous secret is only needed to roll back a store write.
    let previous_secret = if secret_free {
        None
    } else {
        store.get(profile).map_err(AppError::config)?
    };
    if let Err(error) = update(config) {
        *config = previous_config;
        return Err(AppError::validation(error));
    }
    let Some(entry) = config.profiles.get_mut(profile) else {
        *config = previous_config;
        return Err(AppError::validation(format!(
            "profile '{profile}' does not exist"
        )));
    };
    entry.connection_in_config = secret_free;
    if secret_free {
        entry.target = Some(secret.expose_secret().to_owned());
        if let Err(error) = config.save(config_path) {
            *config = previous_config;
            return Err(AppError::config(error));
        }
        let held_secret = previous_config
            .profiles
            .get(profile)
            .is_some_and(|previous| previous.target.is_some() && !previous.connection_in_config);
        if held_secret {
            // Best effort: the config file now takes precedence, so a stale secret is never read.
            let _ = store.delete(profile);
        }
        return Ok(());
    }
    if let Err(error) = store.set(profile, secret.expose_secret()) {
        *config = previous_config;
        return Err(AppError::config(error));
    }
    if let Err(save_error) = config.save(config_path) {
        *config = previous_config;
        let rollback = match previous_secret {
            Some(previous) => store.set(profile, &previous),
            None => store.delete(profile),
        };
        return Err(AppError::config(match rollback {
            Ok(()) => format!(
                "failed to save configuration; the credential change was rolled back: {save_error}"
            ),
            Err(rollback_error) => format!(
                "failed to save configuration ({save_error}) and failed to restore the previous credential ({rollback_error})"
            ),
        }));
    }
    Ok(())
}

pub(crate) fn interactive_terminal() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal() && io::stderr().is_terminal()
}

fn read_secret(reader: &mut impl Read) -> Result<String, AppError> {
    const MAX_SECRET_BYTES: u64 = 64 * 1024;
    let mut value = String::new();
    reader
        .take(MAX_SECRET_BYTES + 1)
        .read_to_string(&mut value)
        .map_err(|error| AppError::config(error.to_string()))?;
    if value.len() as u64 > MAX_SECRET_BYTES {
        return Err(AppError::validation(format!(
            "connection string from stdin exceeds the {MAX_SECRET_BYTES}-byte limit"
        )));
    }
    let value = value
        .strip_suffix("\r\n")
        .or_else(|| value.strip_suffix('\n'))
        .unwrap_or(&value)
        .to_owned();
    if value.is_empty() {
        return Err(AppError::validation(
            "connection string from stdin is empty",
        ));
    }
    if value.contains(['\r', '\n', '\0']) {
        return Err(AppError::validation(
            "connection string from stdin must be exactly one non-empty line",
        ));
    }
    Ok(value)
}

pub(crate) fn prompt_io<T>(result: io::Result<T>) -> Result<T, AppError> {
    result.map_err(|error| {
        if error.kind() == io::ErrorKind::Interrupted {
            AppError::validation("operation cancelled")
        } else {
            AppError::config(format!("terminal interaction failed: {error}"))
        }
    })
}

fn cancelled<T>(message: &str) -> Result<T, AppError> {
    let _ = cliclack::outro_cancel(message);
    Err(AppError::validation(message))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

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

    const SECRET_URI: &str = "mongodb://writer:top-secret@db.example.com:27017";

    fn store_prod(
        config: &mut ConfigFile,
        path: &Path,
        store: &MemoryStore,
    ) -> Result<(), AppError> {
        let secret = SecretString::from(SECRET_URI.to_owned());
        let target = ConnectionUri::parse(SECRET_URI)
            .unwrap()
            .display()
            .to_owned();
        persist_credential(config, path, store, "prod", &secret, |config| {
            config.upsert_profile("prod", Access::ReadOnly, &target)?;
            config.use_profile("prod")
        })
    }

    #[test]
    fn setup_persists_the_profile_without_serializing_the_connection_string() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let store = MemoryStore::default();
        let mut config = ConfigFile::default();

        store_prod(&mut config, &path, &store).unwrap();

        assert_eq!(config.current_profile.as_deref(), Some("prod"));
        assert_eq!(store.get("prod").unwrap().as_deref(), Some(SECRET_URI));
        let text = std::fs::read_to_string(path).unwrap();
        assert!(!text.contains("top-secret"));
        assert!(text.contains("mongodb://db.example.com:27017"));
    }

    #[test]
    fn failed_config_save_restores_the_previous_credential_and_config() {
        let directory = tempfile::tempdir().unwrap();
        let blocker = directory.path().join("not-a-directory");
        std::fs::write(&blocker, "block").unwrap();
        let path = blocker.join("config.toml");
        let store = MemoryStore::default();
        store.set("prod", "mongodb://old.example.com").unwrap();
        let mut config = ConfigFile::default();
        config.add_profile("prod", Access::ReadOnly).unwrap();

        let error = store_prod(&mut config, &path, &store).unwrap_err();

        assert!(error.message.contains("rolled back"));
        assert_eq!(
            store.get("prod").unwrap().as_deref(),
            Some("mongodb://old.example.com")
        );
        assert_eq!(config.profiles["prod"].target, None);
        assert_eq!(config.current_profile, None);
    }

    #[test]
    fn piped_secrets_accept_one_line_and_trim_only_its_ending() {
        let mut input = io::Cursor::new(b"mongodb://h \r\n");
        assert_eq!(read_secret(&mut input).unwrap(), "mongodb://h ");
    }

    #[test]
    fn piped_secrets_reject_empty_multiline_and_oversized_input() {
        for value in [b"".as_slice(), b"\n", b"first\nsecond\n"] {
            let error = read_secret(&mut io::Cursor::new(value)).unwrap_err();
            assert_eq!(error.code, ErrorCode::Validation);
        }
        let oversized = vec![b'x'; 64 * 1024 + 1];
        let error = read_secret(&mut io::Cursor::new(oversized)).unwrap_err();
        assert!(error.message.contains("exceeds"));
    }
}
