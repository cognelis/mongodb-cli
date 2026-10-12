use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use clap::ValueEnum;
use directories::ProjectDirs;
use mongodb_client::ConnectionUri;
use mongodb_client::uri::redact;
use serde::{Deserialize, Serialize};

use crate::args::{Access, CredentialBackend, ServerSideJs};

/// A connection string for automation; it never overrides an explicitly selected profile.
pub const URI_ENV: &str = "MONGODB_URI";
/// The access mode of an environment connection; read-only unless set to `read-write`.
pub const ACCESS_ENV: &str = "MONGODB_CLI_ACCESS";
/// `enabled` or `disabled`; overrides the config file's `atlas_tools` setting.
pub const ATLAS_TOOLS_ENV: &str = "MONGODB_CLI_ATLAS_TOOLS";
/// `keychain`, `security`, or `file`; overrides the config file's `credential_store` setting.
pub const CREDENTIAL_STORE_ENV: &str = "MONGODB_CLI_CREDENTIAL_STORE";
/// The keyring service that names every stored credential.
const SERVICE: &str = "mongodb-cli";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProfileConfig {
    #[serde(default)]
    pub access: Access,
    /// What the environment is for, so people and agents can choose the right profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The connection target without credentials, recorded when the connection string was stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Whether `target` is the whole connection string. Such a string carries no secret, so it is
    /// kept here instead of in the credential store, and reading it never prompts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub connection_in_config: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_side_js: Option<ServerSideJs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_check: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_documents_per_query: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes_per_query: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_count_max_time_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregation_count_max_time_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub confirmation_required_tools: Vec<String>,
    /// Whether Atlas service-account credentials were stored for this profile.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub atlas: bool,
    /// The temporary Atlas database user behind a connection string saved by `atlas clusters connect`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub atlas_temp_user: Option<AtlasTempUser>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AtlasTempUser {
    pub project_id: String,
    pub cluster_name: String,
    pub username: String,
    pub delete_after: String,
}

/// Safety settings in effect for one command, mirroring the MongoDB MCP server's configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub server_side_js: bool,
    pub index_check: bool,
    /// 0 disables the cap.
    pub max_documents_per_query: u32,
    /// 0 disables the cap.
    pub max_bytes_per_query: u64,
    pub query_count_max_time: Duration,
    pub aggregation_count_max_time: Duration,
    pub disabled_tools: Vec<String>,
    pub confirmation_required_tools: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server_side_js: false,
            index_check: false,
            max_documents_per_query: 100,
            max_bytes_per_query: 16 * 1024 * 1024,
            query_count_max_time: Duration::from_secs(10),
            aggregation_count_max_time: Duration::from_secs(60),
            disabled_tools: Vec::new(),
            confirmation_required_tools: Vec::new(),
        }
    }
}

/// Requested profile changes; `None` keeps the current value.
#[derive(Clone, Debug, Default)]
pub struct ProfileChanges {
    pub access: Option<Access>,
    /// `Some("")` clears the description.
    pub description: Option<String>,
    pub server_side_js: Option<ServerSideJs>,
    pub index_check: Option<bool>,
    pub max_documents_per_query: Option<u32>,
    pub max_bytes_per_query: Option<u64>,
    pub query_count_max_time_ms: Option<u64>,
    pub aggregation_count_max_time_ms: Option<u64>,
    pub disabled_tools: Option<Vec<String>>,
    pub confirmation_required_tools: Option<Vec<String>>,
}

impl ProfileChanges {
    pub fn is_empty(&self) -> bool {
        self.access.is_none()
            && self.description.is_none()
            && self.server_side_js.is_none()
            && self.index_check.is_none()
            && self.max_documents_per_query.is_none()
            && self.max_bytes_per_query.is_none()
            && self.query_count_max_time_ms.is_none()
            && self.aggregation_count_max_time_ms.is_none()
            && self.disabled_tools.is_none()
            && self.confirmation_required_tools.is_none()
    }
}

impl ProfileConfig {
    pub fn settings(&self) -> Settings {
        let defaults = Settings::default();
        Settings {
            server_side_js: self.server_side_js == Some(ServerSideJs::Allow),
            index_check: self.index_check.unwrap_or(defaults.index_check),
            max_documents_per_query: self
                .max_documents_per_query
                .unwrap_or(defaults.max_documents_per_query),
            max_bytes_per_query: self
                .max_bytes_per_query
                .unwrap_or(defaults.max_bytes_per_query),
            query_count_max_time: self
                .query_count_max_time_ms
                .map_or(defaults.query_count_max_time, Duration::from_millis),
            aggregation_count_max_time: self
                .aggregation_count_max_time_ms
                .map_or(defaults.aggregation_count_max_time, Duration::from_millis),
            disabled_tools: self.disabled_tools.clone(),
            confirmation_required_tools: self.confirmation_required_tools.clone(),
        }
    }

    /// Describe the safety settings `changes` would loosen; only a person may confirm these.
    pub fn loosened_by(&self, changes: &ProfileChanges) -> Vec<String> {
        let mut loosened = Vec::new();
        if self.access == Access::ReadOnly && changes.access == Some(Access::ReadWrite) {
            loosened.push("access becomes read-write".into());
        }
        if self.server_side_js != Some(ServerSideJs::Allow)
            && changes.server_side_js == Some(ServerSideJs::Allow)
        {
            loosened.push("server-side JavaScript becomes allowed".into());
        }
        for (label, current, requested) in [
            (
                "disabled tools",
                &self.disabled_tools,
                &changes.disabled_tools,
            ),
            (
                "confirmation-required tools",
                &self.confirmation_required_tools,
                &changes.confirmation_required_tools,
            ),
        ] {
            if let Some(requested) = requested {
                let removed: Vec<&str> = current
                    .iter()
                    .filter(|entry| !requested.contains(entry))
                    .map(String::as_str)
                    .collect();
                if !removed.is_empty() {
                    loosened.push(format!("{label} lose {}", removed.join(", ")));
                }
            }
        }
        loosened
    }

    /// Apply `changes`; returns whether anything changed.
    pub fn apply(&mut self, changes: &ProfileChanges) -> bool {
        let before = format!("{self:?}");
        if let Some(access) = changes.access {
            self.access = access;
        }
        if let Some(value) = &changes.description {
            self.description = (!value.is_empty()).then(|| value.clone());
        }
        if let Some(value) = changes.server_side_js {
            self.server_side_js = Some(value);
        }
        if let Some(value) = changes.index_check {
            self.index_check = Some(value);
        }
        if let Some(value) = changes.max_documents_per_query {
            self.max_documents_per_query = Some(value);
        }
        if let Some(value) = changes.max_bytes_per_query {
            self.max_bytes_per_query = Some(value);
        }
        if let Some(value) = changes.query_count_max_time_ms {
            self.query_count_max_time_ms = Some(value);
        }
        if let Some(value) = changes.aggregation_count_max_time_ms {
            self.aggregation_count_max_time_ms = Some(value);
        }
        if let Some(value) = &changes.disabled_tools {
            self.disabled_tools.clone_from(value);
        }
        if let Some(value) = &changes.confirmation_required_tools {
            self.confirmation_required_tools.clone_from(value);
        }
        before != format!("{self:?}")
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub current_profile: Option<String>,
    /// Whether the `atlas`, `local`, and `knowledge` command groups are enabled.
    #[serde(default)]
    pub atlas_tools: bool,
    /// Where connection strings and Atlas credentials are kept; the keychain when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_store: Option<CredentialBackend>,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileConfig>,
}

pub trait CredentialStore {
    fn get(&self, name: &str) -> Result<Option<String>, String>;
    fn set(&self, name: &str, value: &str) -> Result<(), String>;
    fn delete(&self, name: &str) -> Result<(), String>;
}

impl<T: CredentialStore + ?Sized> CredentialStore for Box<T> {
    fn get(&self, name: &str) -> Result<Option<String>, String> {
        (**self).get(name)
    }

    fn set(&self, name: &str, value: &str) -> Result<(), String> {
        (**self).set(name, value)
    }

    fn delete(&self, name: &str) -> Result<(), String> {
        (**self).delete(name)
    }
}

/// The operating-system keyring. On macOS each CLI build is approved separately.
pub struct SystemKeyring;

impl CredentialStore for SystemKeyring {
    fn get(&self, name: &str) -> Result<Option<String>, String> {
        match keyring::Entry::new(SERVICE, name)
            .map_err(|error| error.to_string())?
            .get_password()
        {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn set(&self, name: &str, value: &str) -> Result<(), String> {
        keyring::Entry::new(SERVICE, name)
            .map_err(|error| error.to_string())?
            .set_password(value)
            .map_err(|error| error.to_string())
    }

    fn delete(&self, name: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(SERVICE, name).map_err(|error| error.to_string())?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// macOS `/usr/bin/security`: the same login keychain entries as [`SystemKeyring`], approved once
/// for that Apple-signed tool instead of for every CLI build.
pub struct SecurityCli;

/// The exit status `security` returns for an entry that does not exist.
const SECURITY_NOT_FOUND: i32 = 44;

impl SecurityCli {
    fn run(arguments: &[&str], input: Option<&str>) -> Result<std::process::Output, String> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new("/usr/bin/security")
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("cannot run /usr/bin/security: {error}"))?;
        let mut stdin = child.stdin.take().expect("stdin is piped");
        if let Some(input) = input {
            // A failed write surfaces as the command's own failure.
            let _ = stdin.write_all(input.as_bytes());
        }
        drop(stdin);
        child
            .wait_with_output()
            .map_err(|error| format!("cannot run /usr/bin/security: {error}"))
    }

    fn failure(action: &str, output: &std::process::Output) -> String {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let detail = if detail.is_empty() {
            format!("exit status {:?}", output.status.code())
        } else {
            detail
        };
        format!("the macOS keychain could not {action} the credential: {detail}")
    }
}

impl CredentialStore for SecurityCli {
    fn get(&self, name: &str) -> Result<Option<String>, String> {
        let output = Self::run(
            &["find-generic-password", "-s", SERVICE, "-a", name, "-w"],
            None,
        )?;
        match output.status.code() {
            Some(0) => {
                let value = String::from_utf8(output.stdout)
                    .map_err(|_| "the stored credential is not valid UTF-8".to_owned())?;
                let value = value.strip_suffix('\n').unwrap_or(&value);
                Ok((!value.is_empty()).then(|| value.to_owned()))
            }
            Some(SECURITY_NOT_FOUND) => Ok(None),
            _ => Err(Self::failure("read", &output)),
        }
    }

    fn set(&self, name: &str, value: &str) -> Result<(), String> {
        if value.contains(['\n', '\r']) {
            return Err("credentials must not contain line breaks".into());
        }
        // `-w` without a value reads the secret twice from stdin, keeping it out of the process list.
        let output = Self::run(
            &[
                "add-generic-password",
                "-U",
                "-s",
                SERVICE,
                "-a",
                name,
                "-w",
            ],
            Some(&format!("{value}\n{value}\n")),
        )?;
        if output.status.success() {
            Ok(())
        } else {
            Err(Self::failure("store", &output))
        }
    }

    fn delete(&self, name: &str) -> Result<(), String> {
        let output = Self::run(
            &["delete-generic-password", "-s", SERVICE, "-a", name],
            None,
        )?;
        match output.status.code() {
            Some(0 | SECURITY_NOT_FOUND) => Ok(()),
            _ => Err(Self::failure("remove", &output)),
        }
    }
}

/// Credentials in `credentials.toml` beside the config file, readable only by its owner. Nothing
/// ever prompts, but the values are stored in plain text.
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn beside(config_path: &Path) -> Self {
        Self {
            path: config_path.with_file_name("credentials.toml"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> Result<BTreeMap<String, String>, String> {
        match fs::read_to_string(&self.path) {
            Ok(text) => toml::from_str(&text).map_err(|error| {
                format!("invalid credentials file {}: {error}", self.path.display())
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(format!("cannot read {}: {error}", self.path.display())),
        }
    }

    fn write(&self, credentials: &BTreeMap<String, String>) -> Result<(), String> {
        use std::io::Write;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let text = toml::to_string(credentials).map_err(|error| error.to_string())?;
        let temporary = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        let _ = fs::remove_file(&temporary);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let written = options
            .open(&temporary)
            .and_then(|mut file| file.write_all(text.as_bytes()))
            .and_then(|()| fs::rename(&temporary, &self.path));
        written.map_err(|error| {
            let _ = fs::remove_file(&temporary);
            format!("cannot write {}: {error}", self.path.display())
        })
    }
}

impl CredentialStore for FileStore {
    fn get(&self, name: &str) -> Result<Option<String>, String> {
        Ok(self.read()?.remove(name))
    }

    fn set(&self, name: &str, value: &str) -> Result<(), String> {
        let mut credentials = self.read()?;
        credentials.insert(name.to_owned(), value.to_owned());
        self.write(&credentials)
    }

    fn delete(&self, name: &str) -> Result<(), String> {
        let mut credentials = self.read()?;
        if credentials.remove(name).is_some() {
            self.write(&credentials)?;
        }
        Ok(())
    }
}

/// `MONGODB_CLI_CREDENTIAL_STORE`, if set; it takes precedence over the config file.
pub fn credential_store_override() -> Result<Option<CredentialBackend>, String> {
    parse_credential_store(
        std::env::var(CREDENTIAL_STORE_ENV)
            .ok()
            .filter(|value| !value.is_empty())
            .as_deref(),
    )
}

fn parse_credential_store(value: Option<&str>) -> Result<Option<CredentialBackend>, String> {
    value
        .map(|value| {
            CredentialBackend::from_str(value, true).map_err(|_| {
                format!("{CREDENTIAL_STORE_ENV} must be keychain, security, or file, not '{value}'")
            })
        })
        .transpose()
}

/// Open a credential store; `security` exists only on macOS.
pub fn open_credential_store(
    backend: CredentialBackend,
    config_path: &Path,
) -> Result<Box<dyn CredentialStore>, String> {
    match backend {
        CredentialBackend::Keychain => Ok(Box::new(SystemKeyring)),
        CredentialBackend::Security if cfg!(target_os = "macos") => Ok(Box::new(SecurityCli)),
        CredentialBackend::Security => {
            Err("the security credential store is available only on macOS".into())
        }
        CredentialBackend::File => Ok(Box::new(FileStore::beside(config_path))),
    }
}

/// Where a backend keeps credentials, as shown to people.
pub fn credential_store_label(backend: CredentialBackend) -> &'static str {
    match backend {
        CredentialBackend::Keychain => "the operating-system keychain",
        CredentialBackend::Security => "the macOS keychain through /usr/bin/security",
        CredentialBackend::File => "the credentials file",
    }
}

/// Whether a connection string has no user information or secret-bearing options, so it can live
/// in the config file without a credential store.
pub fn is_secret_free(uri: &str) -> bool {
    redact(uri) == uri
}

/// The selected environment and its access mode, resolved without reading credentials.
#[derive(Debug)]
pub struct ResolvedTarget {
    pub profile: Option<String>,
    pub access: Access,
    pub settings: Settings,
    pub warnings: Vec<String>,
    source: Source,
}

#[derive(Debug)]
enum Source {
    Environment(ConnectionUri),
    Profile,
}

#[derive(Debug)]
pub struct ResolvedConnection {
    pub uri: ConnectionUri,
    pub profile: Option<String>,
    pub access: Access,
    pub settings: Settings,
    pub warnings: Vec<String>,
}

impl ConfigFile {
    pub fn load(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
        toml::from_str(&text).map_err(|error| format!("invalid config: {error}"))
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let text = toml::to_string_pretty(self).map_err(|error| error.to_string())?;
        let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
        fs::write(&temporary, text).map_err(|error| error.to_string())?;
        fs::rename(&temporary, path).map_err(|error| error.to_string())
    }

    pub fn add_profile(&mut self, name: &str, access: Access) -> Result<(), String> {
        validate_profile_name(name)?;
        if self.profiles.contains_key(name) {
            return Err(format!(
                "profile '{name}' already exists; use `profile update` to change its access"
            ));
        }
        self.profiles.insert(
            name.to_owned(),
            ProfileConfig {
                access,
                ..ProfileConfig::default()
            },
        );
        Ok(())
    }

    /// Change a profile's access mode. Returns whether it changed.
    pub fn set_access(&mut self, name: &str, access: Access) -> Result<bool, String> {
        let profile = self
            .profiles
            .get_mut(name)
            .ok_or_else(|| format!("profile '{name}' does not exist"))?;
        let changed = profile.access != access;
        profile.access = access;
        Ok(changed)
    }

    pub fn set_target(&mut self, name: &str, target: &str) -> Result<(), String> {
        self.profiles
            .get_mut(name)
            .ok_or_else(|| format!("profile '{name}' does not exist"))?
            .target = Some(target.to_owned());
        Ok(())
    }

    /// Used by onboarding, which replaces the credential and rolls it back if saving fails.
    pub(crate) fn upsert_profile(
        &mut self,
        name: &str,
        access: Access,
        target: &str,
    ) -> Result<(), String> {
        validate_profile_name(name)?;
        // Re-running setup keeps the profile's other safety settings.
        let profile = self.profiles.entry(name.to_owned()).or_default();
        profile.access = access;
        profile.target = Some(target.to_owned());
        Ok(())
    }

    /// The error for a profile that is not configured, listing the ones that are.
    pub fn unknown_profile(&self, name: &str) -> String {
        if self.profiles.is_empty() {
            return format!(
                "profile '{name}' does not exist and no profiles are configured; run `mongodb-cli init`"
            );
        }
        let available: Vec<String> = self
            .profiles
            .iter()
            .map(|(profile, settings)| match &settings.description {
                Some(description) => format!("{profile} ({description})"),
                None => profile.clone(),
            })
            .collect();
        format!(
            "profile '{name}' does not exist; available: {}",
            available.join(", ")
        )
    }

    pub fn use_profile(&mut self, name: &str) -> Result<(), String> {
        if !self.profiles.contains_key(name) {
            return Err(self.unknown_profile(name));
        }
        self.current_profile = Some(name.to_owned());
        Ok(())
    }

    pub fn remove_profile(
        &mut self,
        name: &str,
        store: &dyn CredentialStore,
    ) -> Result<(), String> {
        if !self.profiles.contains_key(name) {
            return Err(format!("profile '{name}' does not exist"));
        }
        store.delete(name)?;
        store.delete(&atlas_account(name))?;
        self.profiles.remove(name);
        if self.current_profile.as_deref() == Some(name) {
            self.current_profile = None;
        }
        Ok(())
    }

    /// Whether the `atlas`, `local`, and `knowledge` command groups are enabled;
    /// `MONGODB_CLI_ATLAS_TOOLS` overrides the file's setting.
    pub fn atlas_tools_enabled(&self) -> Result<bool, String> {
        Ok(atlas_tools_override()?.unwrap_or(self.atlas_tools))
    }

    /// `MONGODB_CLI_CREDENTIAL_STORE`, then the config file, then the keychain.
    pub fn credential_backend(&self) -> Result<CredentialBackend, String> {
        Ok(credential_store_override()?
            .or(self.credential_store)
            .unwrap_or_default())
    }

    /// Select the connection: `--profile`, then `MONGODB_URI`, then the default profile.
    pub fn resolve_target(&self, selected_profile: Option<&str>) -> Result<ResolvedTarget, String> {
        let uri = std::env::var(URI_ENV)
            .ok()
            .filter(|value| !value.is_empty());
        let access = std::env::var(ACCESS_ENV)
            .ok()
            .filter(|value| !value.is_empty());
        self.resolve_target_inputs(selected_profile, uri.as_deref(), access.as_deref())
    }

    pub fn resolve_credentials(
        &self,
        target: ResolvedTarget,
        store: &dyn CredentialStore,
    ) -> Result<ResolvedConnection, String> {
        let uri = match target.source {
            Source::Environment(uri) => uri,
            Source::Profile => {
                let name = target
                    .profile
                    .as_deref()
                    .expect("profile targets always carry a profile name");
                let in_config = self
                    .profiles
                    .get(name)
                    .filter(|profile| profile.connection_in_config)
                    .and_then(|profile| profile.target.clone());
                // A connection string without secrets lives in the config file; the store is not read.
                let stored = match in_config {
                    Some(uri) => uri,
                    None => store
                        .get(name)?
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| {
                            format!(
                                "profile '{name}' has no connection string; run `mongodb-cli --profile {name} auth login`"
                            )
                        })?,
                };
                ConnectionUri::parse(&stored).map_err(|error| {
                    format!("profile '{name}' has an unusable stored connection string ({error}); run auth login again")
                })?
            }
        };
        Ok(ResolvedConnection {
            uri,
            profile: target.profile,
            access: target.access,
            settings: target.settings,
            warnings: target.warnings,
        })
    }

    fn resolve_target_inputs(
        &self,
        selected_profile: Option<&str>,
        environment_uri: Option<&str>,
        environment_access: Option<&str>,
    ) -> Result<ResolvedTarget, String> {
        if let Some(name) = selected_profile {
            let mut target = self.profile_target(name)?;
            if environment_uri.is_some() {
                target.warnings.push(format!(
                    "{URI_ENV} is ignored because --profile selected '{name}'"
                ));
            }
            return Ok(target);
        }
        if let Some(uri) = environment_uri {
            return Ok(ResolvedTarget {
                profile: None,
                access: parse_access(environment_access)?,
                settings: environment_settings()?,
                warnings: Vec::new(),
                source: Source::Environment(
                    ConnectionUri::parse(uri).map_err(|error| format!("{URI_ENV}: {error}"))?,
                ),
            });
        }
        let name = self
            .current_profile
            .as_deref()
            .ok_or_else(|| format!("no connection: run `mongodb-cli init`, or set {URI_ENV}"))?;
        self.profile_target(name)
    }

    fn profile_target(&self, name: &str) -> Result<ResolvedTarget, String> {
        let profile = self
            .profiles
            .get(name)
            .ok_or_else(|| self.unknown_profile(name))?;
        Ok(ResolvedTarget {
            profile: Some(name.to_owned()),
            access: profile.access,
            settings: profile.settings(),
            warnings: Vec::new(),
            source: Source::Profile,
        })
    }
}

/// Settings for environment connections (`MONGODB_URI` or the Atlas credential variables);
/// profiles ignore these variables.
pub(crate) fn environment_settings() -> Result<Settings, String> {
    fn value(name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|value| !value.is_empty())
    }
    fn flag(name: &str) -> Result<Option<bool>, String> {
        value(name)
            .map(|value| match value.as_str() {
                "true" | "1" => Ok(true),
                "false" | "0" => Ok(false),
                other => Err(format!("{name} must be true or false, not '{other}'")),
            })
            .transpose()
    }
    fn number<T: FromStr>(name: &str) -> Result<Option<T>, String> {
        value(name)
            .map(|value| {
                value
                    .parse()
                    .map_err(|_| format!("{name} must be a non-negative integer, not '{value}'"))
            })
            .transpose()
    }
    fn list(name: &str) -> Vec<String> {
        value(name)
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|entry| !entry.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }
    let mut settings = Settings::default();
    if let Some(allow) = flag("MONGODB_CLI_ALLOW_SERVER_SIDE_JS")? {
        settings.server_side_js = allow;
    }
    if let Some(check) = flag("MONGODB_CLI_INDEX_CHECK")? {
        settings.index_check = check;
    }
    if let Some(cap) = number("MONGODB_CLI_MAX_DOCUMENTS_PER_QUERY")? {
        settings.max_documents_per_query = cap;
    }
    if let Some(cap) = number("MONGODB_CLI_MAX_BYTES_PER_QUERY")? {
        settings.max_bytes_per_query = cap;
    }
    if let Some(cap) = number("MONGODB_CLI_QUERY_COUNT_MAX_TIME_MS")? {
        settings.query_count_max_time = Duration::from_millis(cap);
    }
    if let Some(cap) = number("MONGODB_CLI_AGGREGATION_COUNT_MAX_TIME_MS")? {
        settings.aggregation_count_max_time = Duration::from_millis(cap);
    }
    settings.disabled_tools = list("MONGODB_CLI_DISABLED_TOOLS");
    settings.confirmation_required_tools = list("MONGODB_CLI_CONFIRMATION_REQUIRED_TOOLS");
    Ok(settings)
}

pub(crate) fn parse_access(value: Option<&str>) -> Result<Access, String> {
    match value {
        None | Some("read-only") => Ok(Access::ReadOnly),
        Some("read-write") => Ok(Access::ReadWrite),
        Some(other) => Err(format!(
            "{ACCESS_ENV} must be read-only or read-write, not '{other}'"
        )),
    }
}

/// What moving credentials to another store did.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Migration {
    /// Credentials copied to the new store.
    pub moved: usize,
    /// Connection strings without secrets, now kept in the config file.
    pub kept_in_config: usize,
    /// Credentials the profiles record but the previous store did not hold.
    pub missing: usize,
    /// Entries to remove from the previous store once the config file is saved.
    #[serde(skip)]
    pub remove_from_previous: Vec<String>,
}

impl ConfigFile {
    /// Copy every profile's credentials from `from` to `to`, and move connection strings without
    /// secrets into the config file. With `same_entries` (the keychain and `security` share entries)
    /// only that move happens. Nothing is removed here: the caller saves the config file, then removes
    /// [`Migration::remove_from_previous`], so a failure leaves the previous store intact.
    pub fn migrate_credentials(
        &mut self,
        from: &dyn CredentialStore,
        to: &dyn CredentialStore,
        same_entries: bool,
    ) -> Result<Migration, String> {
        let mut migration = Migration::default();
        let mut copies = Vec::new();
        let mut into_config = Vec::new();
        for (name, profile) in &self.profiles {
            if profile.target.is_some() && !profile.connection_in_config {
                match from.get(name)? {
                    Some(uri) if is_secret_free(&uri) => into_config.push((name.clone(), uri)),
                    Some(uri) if !same_entries => copies.push((name.clone(), uri)),
                    Some(_) => {}
                    None => migration.missing += 1,
                }
            }
            if profile.atlas && !same_entries {
                let account = atlas_account(name);
                match from.get(&account)? {
                    Some(value) => copies.push((account, value)),
                    None => migration.missing += 1,
                }
            }
        }
        let mut written: Vec<&str> = Vec::new();
        for (account, value) in &copies {
            if let Err(error) = to.set(account, value) {
                for account in &written {
                    let _ = to.delete(account);
                }
                return Err(format!(
                    "could not copy '{account}' to the new store, so nothing was moved: {error}"
                ));
            }
            written.push(account);
        }
        for (name, uri) in &into_config {
            let profile = self
                .profiles
                .get_mut(name)
                .expect("collected from the profiles");
            profile.target = Some(uri.clone());
            profile.connection_in_config = true;
        }
        migration.moved = copies.len();
        migration.kept_in_config = into_config.len();
        migration.remove_from_previous = copies
            .into_iter()
            .map(|(account, _)| account)
            .chain(into_config.into_iter().map(|(name, _)| name))
            .collect();
        Ok(migration)
    }
}

/// The `MONGODB_CLI_ATLAS_TOOLS` setting, if any; it takes precedence over the config file.
pub fn atlas_tools_override() -> Result<Option<bool>, String> {
    parse_atlas_tools(
        std::env::var(ATLAS_TOOLS_ENV)
            .ok()
            .filter(|value| !value.is_empty())
            .as_deref(),
    )
}

fn parse_atlas_tools(value: Option<&str>) -> Result<Option<bool>, String> {
    match value {
        None => Ok(None),
        Some("enabled") => Ok(Some(true)),
        Some("disabled") => Ok(Some(false)),
        Some(other) => Err(format!(
            "{ATLAS_TOOLS_ENV} must be enabled or disabled, not '{other}'"
        )),
    }
}

pub fn default_config_path() -> Result<PathBuf, String> {
    ProjectDirs::from("dev", "cognelis", "mongodb-cli")
        .map(|directories| directories.config_dir().join("config.toml"))
        .ok_or_else(|| "unable to determine the operating-system config directory".to_owned())
}

/// The keyring account holding a profile's Atlas credentials; profile names cannot contain `/`.
pub fn atlas_account(profile: &str) -> String {
    format!("{profile}/atlas")
}

pub(crate) fn validate_profile_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err("profile names may contain only letters, numbers, '-' and '_'".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    const URI: &str = "mongodb://reader:never-serialize-this@db.example.com:27017/?replicaSet=rs0";

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

    fn resolve(
        config: &ConfigFile,
        profile: Option<&str>,
        uri: Option<&str>,
        access: Option<&str>,
        store: &dyn CredentialStore,
    ) -> Result<ResolvedConnection, String> {
        let target = config.resolve_target_inputs(profile, uri, access)?;
        config.resolve_credentials(target, store)
    }

    #[test]
    fn persists_profiles_without_secrets_and_defaults_to_read_only() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let store = MemoryStore::default();
        let mut config = ConfigFile::default();
        config.add_profile("prod", Access::default()).unwrap();
        config
            .set_target("prod", ConnectionUri::parse(URI).unwrap().display())
            .unwrap();
        config.use_profile("prod").unwrap();
        store.set("prod", URI).unwrap();
        config.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("never-serialize-this"));
        assert!(!text.contains("reader"));
        assert!(text.contains("access = \"read-only\""));
        let config = ConfigFile::load(&path).unwrap();
        let connection = resolve(&config, None, None, None, &store).unwrap();
        assert_eq!(connection.access, Access::ReadOnly);
        assert_eq!(connection.profile.as_deref(), Some("prod"));
        assert!(!format!("{connection:?}").contains("never-serialize-this"));
    }

    #[test]
    fn explicit_profile_wins_over_environment_uri_with_a_warning() {
        let store = MemoryStore::default();
        let mut config = ConfigFile::default();
        config.add_profile("test", Access::ReadWrite).unwrap();
        store.set("test", URI).unwrap();

        let connection = resolve(
            &config,
            Some("test"),
            Some("mongodb://other.example.com"),
            Some("read-only"),
            &store,
        )
        .unwrap();
        assert_eq!(connection.access, Access::ReadWrite);
        assert_eq!(
            connection.uri.display(),
            "mongodb://db.example.com:27017/?replicaSet=rs0"
        );
        assert_eq!(connection.warnings.len(), 1);
        assert!(connection.warnings[0].contains(URI_ENV));
    }

    #[test]
    fn environment_uri_beats_the_default_profile_and_is_read_only_by_default() {
        let store = MemoryStore::default();
        let mut config = ConfigFile::default();
        config.add_profile("test", Access::ReadWrite).unwrap();
        config.use_profile("test").unwrap();

        let connection = resolve(
            &config,
            None,
            Some("mongodb://ci.example.com"),
            None,
            &store,
        )
        .unwrap();
        assert_eq!(connection.profile, None);
        assert_eq!(connection.access, Access::ReadOnly);
        assert_eq!(connection.uri.display(), "mongodb://ci.example.com");

        let connection = resolve(
            &config,
            None,
            Some("mongodb://ci.example.com"),
            Some("read-write"),
            &store,
        )
        .unwrap();
        assert_eq!(connection.access, Access::ReadWrite);
        assert!(
            resolve(
                &config,
                None,
                Some("mongodb://ci.example.com"),
                Some("yes"),
                &store
            )
            .unwrap_err()
            .contains(ACCESS_ENV)
        );
        assert!(
            resolve(&config, None, Some("postgres://x"), None, &store)
                .unwrap_err()
                .contains(URI_ENV)
        );
    }

    #[test]
    fn missing_connection_and_missing_credentials_explain_the_next_step() {
        let store = MemoryStore::default();
        let mut config = ConfigFile::default();
        assert!(
            resolve(&config, None, None, None, &store)
                .unwrap_err()
                .contains("init")
        );
        config.add_profile("prod", Access::ReadOnly).unwrap();
        let error = resolve(&config, Some("prod"), None, None, &store).unwrap_err();
        assert!(error.contains("auth login"), "{error}");
        assert!(
            resolve(&config, Some("missing"), None, None, &store)
                .unwrap_err()
                .contains("does not exist")
        );
    }

    #[test]
    fn profile_settings_default_like_the_mcp_server_and_loosening_is_detected() {
        let mut profile = ProfileConfig::default();
        assert_eq!(profile.settings(), Settings::default());
        assert_eq!(Settings::default().max_documents_per_query, 100);
        let tighten = ProfileChanges {
            index_check: Some(true),
            disabled_tools: Some(vec!["delete".into(), "drop-database".into()]),
            max_documents_per_query: Some(20),
            ..ProfileChanges::default()
        };
        assert!(profile.loosened_by(&tighten).is_empty());
        assert!(profile.apply(&tighten));
        assert!(!profile.apply(&tighten), "reapplying changes nothing");
        assert!(profile.settings().index_check);
        assert_eq!(profile.settings().max_documents_per_query, 20);

        let loosen = ProfileChanges {
            access: Some(Access::ReadWrite),
            server_side_js: Some(ServerSideJs::Allow),
            disabled_tools: Some(vec!["delete".into()]),
            ..ProfileChanges::default()
        };
        let loosened = profile.loosened_by(&loosen);
        assert_eq!(loosened.len(), 3, "{loosened:?}");
        assert!(loosened[2].contains("drop-database"));
        assert!(ProfileChanges::default().is_empty());
    }

    #[test]
    fn profiles_change_access_and_removal_deletes_the_credential() {
        let store = MemoryStore::default();
        let mut config = ConfigFile::default();
        config.add_profile("dev", Access::ReadOnly).unwrap();
        assert!(
            config
                .add_profile("dev", Access::ReadWrite)
                .unwrap_err()
                .contains("already exists")
        );
        assert!(config.set_access("dev", Access::ReadWrite).unwrap());
        assert!(!config.set_access("dev", Access::ReadWrite).unwrap());
        config.use_profile("dev").unwrap();
        store.set("dev", URI).unwrap();
        store
            .set(
                &atlas_account("dev"),
                r#"{"clientId":"i","clientSecret":"s"}"#,
            )
            .unwrap();

        config.remove_profile("dev", &store).unwrap();
        assert_eq!(store.get("dev").unwrap(), None);
        assert_eq!(store.get("dev/atlas").unwrap(), None);
        assert_eq!(config.current_profile, None);
        assert!(config.add_profile("bad name", Access::ReadOnly).is_err());
    }

    #[test]
    fn atlas_tools_default_off_persist_and_accept_only_enabled_or_disabled() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        assert!(!ConfigFile::load(&path).unwrap().atlas_tools);
        let mut config = ConfigFile::default();
        config.add_profile("dev", Access::ReadOnly).unwrap();
        config.atlas_tools = true;
        config.save(&path).unwrap();
        assert!(ConfigFile::load(&path).unwrap().atlas_tools);

        assert_eq!(parse_atlas_tools(None), Ok(None));
        assert_eq!(parse_atlas_tools(Some("enabled")), Ok(Some(true)));
        assert_eq!(parse_atlas_tools(Some("disabled")), Ok(Some(false)));
        assert!(
            parse_atlas_tools(Some("true"))
                .unwrap_err()
                .contains(ATLAS_TOOLS_ENV)
        );
    }

    /// A store that refuses reads and writes, to prove a code path never touches credentials.
    struct UntouchableStore;

    impl CredentialStore for UntouchableStore {
        fn get(&self, _: &str) -> Result<Option<String>, String> {
            Err("the credential store must not be read".into())
        }
        fn set(&self, _: &str, _: &str) -> Result<(), String> {
            Err("the credential store must not be written".into())
        }
        fn delete(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn connection_strings_without_secrets_skip_the_credential_store() {
        assert!(is_secret_free(
            "mongodb://localhost:27017/?directConnection=true"
        ));
        assert!(is_secret_free(
            "mongodb+srv://cluster0.example.net/app?retryWrites=true"
        ));
        assert!(!is_secret_free(URI));
        assert!(!is_secret_free(
            "mongodb://db.example.com/?authMechanismProperties=AWS_SESSION_TOKEN:x"
        ));

        let mut config = ConfigFile::default();
        config.add_profile("local", Access::ReadWrite).unwrap();
        let local = config.profiles.get_mut("local").unwrap();
        local.target = Some("mongodb://localhost:27017".into());
        local.connection_in_config = true;
        let connection = resolve(&config, Some("local"), None, None, &UntouchableStore).unwrap();
        assert_eq!(connection.uri.display(), "mongodb://localhost:27017");
    }

    #[test]
    fn file_store_keeps_credentials_readable_only_by_their_owner() {
        let directory = tempfile::tempdir().unwrap();
        let store = FileStore::beside(&directory.path().join("config.toml"));
        assert_eq!(store.get("prod").unwrap(), None);
        store.set("prod", URI).unwrap();
        store.set("prod/atlas", r#"{"clientId":"id"}"#).unwrap();
        assert_eq!(store.get("prod").unwrap().as_deref(), Some(URI));
        store.delete("prod").unwrap();
        assert_eq!(store.get("prod").unwrap(), None);
        assert!(store.get("prod/atlas").unwrap().is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(store.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn migration_copies_secrets_and_moves_secret_free_strings_into_the_config() {
        let from = MemoryStore::default();
        from.set("prod", URI).unwrap();
        from.set("prod/atlas", r#"{"clientId":"id"}"#).unwrap();
        from.set("local", "mongodb://localhost:27017").unwrap();
        let mut config = ConfigFile::default();
        for (name, target) in [
            ("prod", "mongodb://db.example.com:27017/?replicaSet=rs0"),
            ("local", "mongodb://localhost:27017"),
            ("stale", "mongodb://gone.example.com"),
        ] {
            config.add_profile(name, Access::ReadOnly).unwrap();
            config.profiles.get_mut(name).unwrap().target = Some(target.into());
        }
        config.profiles.get_mut("prod").unwrap().atlas = true;

        let to = MemoryStore::default();
        let migration = config.migrate_credentials(&from, &to, false).unwrap();
        assert_eq!(
            (migration.moved, migration.kept_in_config, migration.missing),
            (2, 1, 1)
        );
        assert_eq!(to.get("prod").unwrap().as_deref(), Some(URI));
        assert!(to.get("prod/atlas").unwrap().is_some());
        assert_eq!(to.get("local").unwrap(), None);
        assert!(config.profiles["local"].connection_in_config);
        let mut removed = migration.remove_from_previous.clone();
        removed.sort();
        assert_eq!(removed, ["local", "prod", "prod/atlas"]);
        // Nothing leaves the previous store until the caller has saved the config file.
        assert!(from.get("prod").unwrap().is_some());

        // The keychain and `security` share entries, so only secret-free strings move.
        let mut config = ConfigFile::default();
        config.add_profile("prod", Access::ReadOnly).unwrap();
        config.profiles.get_mut("prod").unwrap().target = Some("target".into());
        let untouched = MemoryStore::default();
        let migration = config.migrate_credentials(&from, &untouched, true).unwrap();
        assert_eq!((migration.moved, migration.kept_in_config), (0, 0));
        assert_eq!(untouched.get("prod").unwrap(), None);
    }

    #[test]
    fn a_failed_migration_writes_nothing_and_removes_nothing() {
        struct FailsSecondWrite(MemoryStore, Mutex<usize>);
        impl CredentialStore for FailsSecondWrite {
            fn get(&self, name: &str) -> Result<Option<String>, String> {
                self.0.get(name)
            }
            fn set(&self, name: &str, value: &str) -> Result<(), String> {
                let mut writes = self.1.lock().unwrap();
                *writes += 1;
                if *writes == 2 {
                    return Err("disk full".into());
                }
                self.0.set(name, value)
            }
            fn delete(&self, name: &str) -> Result<(), String> {
                self.0.delete(name)
            }
        }
        let from = MemoryStore::default();
        from.set("a", URI).unwrap();
        from.set("b", URI).unwrap();
        let mut config = ConfigFile::default();
        for name in ["a", "b"] {
            config.add_profile(name, Access::ReadOnly).unwrap();
            config.profiles.get_mut(name).unwrap().target = Some("target".into());
        }
        let to = FailsSecondWrite(MemoryStore::default(), Mutex::new(0));
        let error = config.migrate_credentials(&from, &to, false).unwrap_err();
        assert!(error.contains("nothing was moved"), "{error}");
        assert!(to.0.0.lock().unwrap().is_empty());
        assert!(from.get("a").unwrap().is_some() && from.get("b").unwrap().is_some());
    }

    #[test]
    fn credential_store_settings_parse_and_default_to_the_keychain() {
        assert_eq!(parse_credential_store(None), Ok(None));
        assert_eq!(
            parse_credential_store(Some("security")),
            Ok(Some(CredentialBackend::Security))
        );
        assert_eq!(
            parse_credential_store(Some("FILE")),
            Ok(Some(CredentialBackend::File))
        );
        assert!(
            parse_credential_store(Some("vault"))
                .unwrap_err()
                .contains(CREDENTIAL_STORE_ENV)
        );
        assert_eq!(
            ConfigFile::default().credential_store.unwrap_or_default(),
            CredentialBackend::Keychain
        );
    }
}
