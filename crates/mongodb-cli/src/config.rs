use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use directories::ProjectDirs;
use mongodb_client::ConnectionUri;
use serde::{Deserialize, Serialize};

use crate::args::{Access, ServerSideJs};

/// A connection string for automation; it never overrides an explicitly selected profile.
pub const URI_ENV: &str = "MONGODB_URI";
/// The access mode of an environment connection; read-only unless set to `read-write`.
pub const ACCESS_ENV: &str = "MONGODB_CLI_ACCESS";
/// `enabled` or `disabled`; overrides the config file's `atlas_tools` setting.
pub const ATLAS_TOOLS_ENV: &str = "MONGODB_CLI_ATLAS_TOOLS";

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
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileConfig>,
}

pub trait CredentialStore {
    fn get(&self, name: &str) -> Result<Option<String>, String>;
    fn set(&self, name: &str, value: &str) -> Result<(), String>;
    fn delete(&self, name: &str) -> Result<(), String>;
}

pub struct SystemKeyring;

impl CredentialStore for SystemKeyring {
    fn get(&self, name: &str) -> Result<Option<String>, String> {
        match keyring::Entry::new("mongodb-cli", name)
            .map_err(|error| error.to_string())?
            .get_password()
        {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn set(&self, name: &str, value: &str) -> Result<(), String> {
        keyring::Entry::new("mongodb-cli", name)
            .map_err(|error| error.to_string())?
            .set_password(value)
            .map_err(|error| error.to_string())
    }

    fn delete(&self, name: &str) -> Result<(), String> {
        let entry = keyring::Entry::new("mongodb-cli", name).map_err(|error| error.to_string())?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
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

    pub fn use_profile(&mut self, name: &str) -> Result<(), String> {
        if !self.profiles.contains_key(name) {
            return Err(format!("profile '{name}' does not exist"));
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
                let stored = store
                    .get(name)?
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        format!(
                            "profile '{name}' has no connection string; run `mongodb-cli --profile {name} auth login`"
                        )
                    })?;
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
            .ok_or_else(|| format!("profile '{name}' does not exist"))?;
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
}
