use serde::Serialize;
use serde_json::Value;

const CONFIGURATION: usize = 1;
const CREDENTIALS: usize = 2;
const SERVER: usize = 3;
const AUTHENTICATION: usize = 4;

/// Built-in roles that can modify data or administer the deployment.
const WRITE_CAPABLE_ROLES: &[&str] = &[
    "readWrite",
    "readWriteAnyDatabase",
    "dbAdmin",
    "dbAdminAnyDatabase",
    "dbOwner",
    "userAdmin",
    "userAdminAnyDatabase",
    "clusterAdmin",
    "clusterManager",
    "hostManager",
    "restore",
    "root",
    "atlasAdmin",
    "__system",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    Pass,
    Fail,
    Skipped,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorCheck {
    name: &'static str,
    status: DoctorStatus,
    message: String,
}

#[derive(Debug)]
pub struct DoctorReport {
    checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub fn new(version: &str) -> Self {
        Self {
            checks: vec![
                DoctorCheck {
                    name: "version",
                    status: DoctorStatus::Pass,
                    message: format!("mongodb-cli {version}"),
                },
                skipped("configuration", "connection target has not been checked"),
                skipped("credentials", "credentials have not been checked"),
                skipped("server", "server connectivity has not been checked"),
                skipped("authentication", "authentication has not been checked"),
            ],
        }
    }

    pub fn configuration_passed(&mut self) {
        self.set(
            CONFIGURATION,
            DoctorStatus::Pass,
            "connection target is configured and valid",
        );
    }

    pub fn configuration_failed(&mut self) {
        self.set(
            CONFIGURATION,
            DoctorStatus::Fail,
            "connection configuration is missing, unreadable, or invalid",
        );
        self.skip_from(CREDENTIALS, "not checked because configuration failed");
    }

    pub fn credentials_passed(&mut self) {
        self.set(
            CREDENTIALS,
            DoctorStatus::Pass,
            "a connection string is available",
        );
    }

    pub fn credentials_failed(&mut self) {
        self.set(
            CREDENTIALS,
            DoctorStatus::Fail,
            "a connection string is missing or unavailable",
        );
        self.skip_from(SERVER, "not checked because credentials failed");
    }

    pub fn server_passed(&mut self, message: String) {
        self.set(SERVER, DoctorStatus::Pass, message);
    }

    pub fn server_failed(&mut self, message: &str) {
        self.set(SERVER, DoctorStatus::Fail, message);
        self.skip_from(
            AUTHENTICATION,
            "not checked because the server probe failed",
        );
    }

    pub fn authentication_passed(&mut self, message: String) {
        self.set(AUTHENTICATION, DoctorStatus::Pass, message);
    }

    pub fn authentication_failed(&mut self, message: &str) {
        self.set(AUTHENTICATION, DoctorStatus::Fail, message);
    }

    pub fn into_value(self) -> Value {
        serde_json::to_value(self.checks).expect("doctor checks always serialize")
    }

    fn set(&mut self, index: usize, status: DoctorStatus, message: impl Into<String>) {
        self.checks[index].status = status;
        self.checks[index].message = message.into();
    }

    fn skip_from(&mut self, first: usize, message: &str) {
        for index in first..self.checks.len() {
            self.set(index, DoctorStatus::Skipped, message);
        }
    }
}

fn skipped(name: &'static str, message: &'static str) -> DoctorCheck {
    DoctorCheck {
        name,
        status: DoctorStatus::Skipped,
        message: message.into(),
    }
}

/// A check appended after the MongoDB checks, such as the Atlas checks.
pub fn check(name: &'static str, status: DoctorStatus, message: impl Into<String>) -> Value {
    serde_json::to_value(DoctorCheck {
        name,
        status,
        message: message.into(),
    })
    .expect("doctor checks always serialize")
}

/// Describe a reachable server from `MongoClient::server_info`.
pub fn server_message(info: &Value) -> String {
    format!(
        "MongoDB {} ({}) is reachable",
        info["version"].as_str().unwrap_or("unknown"),
        info["topology"].as_str().unwrap_or("unknown topology")
    )
}

/// Describe the authenticated users from `MongoClient::connection_status`.
pub fn identity(status: &Value) -> String {
    let users: Vec<_> = status["users"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|user| {
            Some(format!(
                "{}@{}",
                user["user"].as_str()?,
                user["db"].as_str()?
            ))
        })
        .collect();
    if users.is_empty() {
        "connected without authentication".into()
    } else {
        format!("authenticated as {}", users.join(", "))
    }
}

/// Warn when a profile that mongodb-cli treats as read-only connects as a user that can write.
pub fn read_only_role_warning(status: &Value) -> Option<String> {
    let roles: Vec<_> = status["roles"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|role| {
            let name = role["role"].as_str()?;
            WRITE_CAPABLE_ROLES
                .contains(&name)
                .then(|| format!("{name}@{}", role["db"].as_str().unwrap_or_default()))
        })
        .collect();
    (!roles.is_empty()).then(|| {
        format!(
            "this connection is read-only in mongodb-cli, but its database user has write-capable roles ({}); prefer a user with only the read role",
            roles.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn report_shape_is_ordered_and_stable() {
        let report = DoctorReport::new("1.2.3").into_value();
        let names: Vec<_> = report
            .as_array()
            .unwrap()
            .iter()
            .map(|check| check["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "version",
                "configuration",
                "credentials",
                "server",
                "authentication"
            ]
        );
    }

    #[test]
    fn failures_skip_every_dependent_check() {
        let mut report = DoctorReport::new("1.2.3");
        report.configuration_passed();
        report.credentials_failed();
        let report = report.into_value();
        let statuses: Vec<_> = report
            .as_array()
            .unwrap()
            .iter()
            .map(|check| check["status"].as_str().unwrap())
            .collect();
        assert_eq!(statuses, ["pass", "pass", "fail", "skipped", "skipped"]);
    }

    #[test]
    fn identity_and_role_warnings_describe_the_connection_user() {
        let status = json!({
            "users": [{"user": "reader", "db": "admin"}],
            "roles": [{"role": "read", "db": "app"}, {"role": "readWrite", "db": "app"}]
        });
        assert_eq!(identity(&status), "authenticated as reader@admin");
        let warning = read_only_role_warning(&status).unwrap();
        assert!(warning.contains("readWrite@app"));
        assert!(!warning.contains("read@app"));
        assert_eq!(
            read_only_role_warning(&json!({"users": [], "roles": [{"role": "read", "db": "app"}]})),
            None
        );
        assert_eq!(
            identity(&json!({"users": [], "roles": []})),
            "connected without authentication"
        );
    }
}
