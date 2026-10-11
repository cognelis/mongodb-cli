//! Atlas commands against an in-process HTTP stub; the real Atlas API is never called. Every
//! command runs with Atlas tools enabled unless a test says otherwise.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use assert_cmd::Command;
use serde_json::{Value, json};

const PROJECT: &str = "65f1a2b3c4d5e6f708192a3b";
const ORG: &str = "5e2211c17a3e5a48f5497de3";
const CLIENT_ID: &str = "mdb_sa_id_test";
const CLIENT_SECRET: &str = "mdb_sa_sk_s3cr3t";
/// base64("mdb_sa_id_test:mdb_sa_sk_s3cr3t"); underscores are sent unencoded, as Atlas expects.
const BASIC: &str = "Basic bWRiX3NhX2lkX3Rlc3Q6bWRiX3NhX3NrX3MzY3IzdA==";
const V2: &str = "application/vnd.atlas.2025-03-12+json";

#[derive(Clone, Debug)]
struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Request {
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or_default()
    }

    fn query(&self) -> &str {
        self.target.split_once('?').map_or("", |(_, query)| query)
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|_| panic!("not JSON: {}", self.body))
    }
}

/// `(method, path prefix, status, body)`; the first matching route answers.
type Route = (&'static str, String, u16, Value);

struct Stub {
    base: String,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl Stub {
    fn start(mut routes: Vec<Route>) -> Self {
        routes.push((
            "POST",
            "/api/oauth/token".into(),
            200,
            json!({"access_token": "test-token", "token_type": "Bearer", "expires_in": 3600}),
        ));
        routes.push(("POST", "/api/oauth/revoke".into(), 200, Value::Null));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                if let Some(request) = read_request(&stream) {
                    let (status, body) = routes
                        .iter()
                        .find(|(method, prefix, _, _)| *method == request.method && request.path().starts_with(prefix.as_str()))
                        .map(|(_, _, status, body)| (*status, body.clone()))
                        .unwrap_or((404, json!({"error": 404, "errorCode": "RESOURCE_NOT_FOUND", "reason": "Not Found", "detail": "no stub route"})));
                    recorded.lock().unwrap().push(request);
                    respond(stream, status, &body);
                }
            }
        });
        Self { base, requests }
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    fn api(&self) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|request| request.path().starts_with("/api/atlas/v2"))
            .collect()
    }
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next()?.to_owned(), parts.next()?.to_owned());
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':')?;
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Request {
        method,
        target,
        headers,
        body: String::from_utf8(body).ok()?,
    })
}

fn respond(mut stream: TcpStream, status: u16, body: &Value) {
    let body = if body.is_null() {
        String::new()
    } else {
        body.to_string()
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} Stub\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn atlas(config: &Path, base: &str) -> Command {
    let mut command = Command::cargo_bin("mongodb-cli").unwrap();
    // Settings and proxies from the developer's environment must not change the results.
    for (name, _) in std::env::vars_os() {
        let name_text = name.to_string_lossy();
        if name_text.starts_with("MONGODB_CLI_")
            || name_text.to_ascii_lowercase().ends_with("_proxy")
        {
            command.env_remove(name);
        }
    }
    command
        .env("MONGODB_CLI_CONFIG", config)
        .env("MONGODB_CLI_ATLAS_TOOLS", "enabled")
        .env("MONGODB_ATLAS_CLIENT_ID", CLIENT_ID)
        .env("MONGODB_ATLAS_CLIENT_SECRET", CLIENT_SECRET)
        .env("MONGODB_CLI_ATLAS_BASE_URL", base)
        .env_remove("MONGODB_URI")
        .args(["--output", "json"])
        .timeout(Duration::from_secs(30));
    command
}

fn success(output: &std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "exit {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn failure(output: &std::process::Output, code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty(), "errors keep stdout empty");
    serde_json::from_slice(&output.stderr).unwrap()
}

fn printed(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn dedicated_cluster() -> Value {
    json!({
        "id": "6600aaaaaaaaaaaaaaaaaaaa", "name": "c0", "clusterType": "REPLICASET", "stateName": "IDLE", "mongoDBVersion": "8.0.4",
        "replicationSpecs": [{"regionConfigs": [{"providerName": "AWS", "regionName": "US_EAST_1", "electableSpecs": {"instanceSize": "M10", "nodeCount": 3}}]}],
        "connectionStrings": {"standard": "mongodb://h1.example.net:27017,h2.example.net:27017/?ssl=true", "standardSrv": "mongodb+srv://c0.example.net"}
    })
}

#[test]
fn atlas_commands_are_refused_while_atlas_tools_are_disabled() {
    let stub = Stub::start(Vec::new());
    let directory = tempfile::tempdir().unwrap();
    let output = atlas(&directory.path().join("config.toml"), &stub.base)
        .env_remove("MONGODB_CLI_ATLAS_TOOLS")
        .args(["atlas", "orgs", "list"])
        .output()
        .unwrap();
    let error = failure(&output, 3);
    assert_eq!(error["error"]["code"], "config");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Atlas tools are disabled")
    );
    assert!(stub.requests().is_empty());
}

#[test]
fn safety_settings_refuse_atlas_commands_before_any_request() {
    let stub = Stub::start(vec![(
        "GET",
        "/api/atlas/v2/orgs".into(),
        200,
        json!({"results": []}),
    )]);
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    // A profile's disabled tools refuse before its keyring entry is read.
    std::fs::write(
        &config,
        "[profiles.admin]\naccess = \"read-write\"\ndisabled_tools = [\"atlas\"]\n",
    )
    .unwrap();
    let output = atlas(&config, &stub.base)
        .args(["--profile", "admin", "atlas", "orgs", "list"])
        .output()
        .unwrap();
    let error = failure(&output, 3);
    assert_eq!(error["error"]["code"], "forbidden");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("\"atlas\" is listed in the disabled tools")
    );
    // Environment credentials take the settings variables, as MONGODB_URI connections do.
    let output = atlas(&config, &stub.base)
        .env("MONGODB_CLI_DISABLED_TOOLS", "atlas-list-orgs")
        .args(["atlas", "orgs", "list"])
        .output()
        .unwrap();
    assert_eq!(failure(&output, 3)["error"]["code"], "forbidden");
    let output = atlas(&config, &stub.base)
        .env("MONGODB_CLI_CONFIRMATION_REQUIRED_TOOLS", "read")
        .args(["atlas", "orgs", "list"])
        .output()
        .unwrap();
    assert_eq!(failure(&output, 2)["error"]["code"], "validation");
    assert!(
        stub.requests().is_empty(),
        "refusals happen before any request"
    );
    let envelope = success(
        &atlas(&config, &stub.base)
            .env("MONGODB_CLI_CONFIRMATION_REQUIRED_TOOLS", "read")
            .args(["atlas", "orgs", "list", "--yes"])
            .output()
            .unwrap(),
    );
    assert_eq!(envelope["command"], "atlas.orgs.list");
}

#[test]
fn doctor_adds_atlas_checks_only_while_atlas_tools_are_enabled() {
    let stub = Stub::start(vec![(
        "GET",
        "/api/atlas/v2/orgs".into(),
        200,
        json!({"results": []}),
    )]);
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    // Disabled by default: the five MongoDB checks only, and Atlas is never contacted.
    let output = atlas(&config, &stub.base)
        .env_remove("MONGODB_CLI_ATLAS_TOOLS")
        .arg("doctor")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let names: Vec<_> = report["data"]
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
    assert!(stub.requests().is_empty());

    // Enabled: Atlas credentials alone satisfy the doctor, and the MongoDB checks are skipped.
    let envelope = success(&atlas(&config, &stub.base).arg("doctor").output().unwrap());
    let checks: Vec<_> = envelope["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| {
            (
                check["name"].as_str().unwrap(),
                check["status"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        checks[5..],
        [
            ("atlas-credentials", "pass"),
            ("atlas-token", "pass"),
            ("atlas-api", "pass")
        ]
    );
    assert_eq!(stub.api().len(), 1);
}

#[test]
fn reads_reuse_one_token_send_versioned_requests_and_revoke_it() {
    let stub = Stub::start(vec![
        (
            "GET",
            format!("/api/atlas/v2/groups/{PROJECT}/clusters"),
            200,
            json!({"results": [dedicated_cluster()], "totalCount": 1}),
        ),
        (
            "GET",
            format!("/api/atlas/v2/groups/{PROJECT}/flexClusters"),
            200,
            json!({"results": [], "totalCount": 0}),
        ),
        (
            "GET",
            format!("/api/atlas/v2/groups/{PROJECT}"),
            200,
            json!({"id": PROJECT, "name": "demo"}),
        ),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let output = atlas(&directory.path().join("config.toml"), &stub.base)
        .args(["atlas", "clusters", "list", "--project-id", PROJECT])
        .output()
        .unwrap();
    let envelope = success(&output);
    assert_eq!(envelope["command"], "atlas.clusters.list");
    assert_eq!(envelope["data"]["projectName"], "demo");
    assert_eq!(envelope["data"]["totalCount"], 1);
    let cluster = &envelope["data"]["clusters"][0];
    assert_eq!(cluster["instanceType"], "DEDICATED");
    assert_eq!(cluster["instanceSize"], "M10");
    assert_eq!(
        cluster["processIds"],
        json!(["h1.example.net:27017", "h2.example.net:27017"])
    );
    assert_eq!(envelope["meta"]["access"], "read-only");
    assert!(envelope["meta"].get("profile").is_none());
    assert!(
        envelope["meta"]["warnings"][0]
            .as_str()
            .unwrap()
            .contains("plain HTTP")
    );

    let requests = stub.requests();
    let tokens: Vec<_> = requests
        .iter()
        .filter(|request| request.path() == "/api/oauth/token")
        .collect();
    assert_eq!(
        tokens.len(),
        1,
        "one token serves every request of a process"
    );
    assert_eq!(tokens[0].header("authorization"), Some(BASIC));
    assert_eq!(tokens[0].body, "grant_type=client_credentials");
    assert!(
        tokens[0]
            .header("content-type")
            .unwrap()
            .starts_with("application/x-www-form-urlencoded")
    );
    let api = stub.api();
    assert_eq!(api.len(), 3);
    for request in &api {
        assert_eq!(request.method, "GET");
        assert_eq!(request.header("authorization"), Some("Bearer test-token"));
        assert_eq!(request.header("accept"), Some(V2));
        assert!(
            request
                .header("user-agent")
                .unwrap()
                .starts_with("mongodb-cli/")
        );
    }
    let revoke = requests.last().unwrap();
    assert_eq!(revoke.path(), "/api/oauth/revoke");
    assert_eq!(revoke.body, "token=test-token");
    assert_eq!(revoke.header("authorization"), Some(BASIC));
    assert!(!printed(&output).contains(CLIENT_SECRET));
}

#[test]
fn paged_lists_send_paging_and_report_continuation() {
    let stub = Stub::start(vec![(
        "GET",
        "/api/atlas/v2/orgs".into(),
        200,
        json!({"results": [{"id": ORG, "name": "a"}, {"id": ORG, "name": "b"}]}),
    )]);
    let directory = tempfile::tempdir().unwrap();
    let envelope = success(
        &atlas(&directory.path().join("config.toml"), &stub.base)
            .args(["atlas", "orgs", "list", "--limit", "2"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        envelope["data"]["organizations"][1],
        json!({"name": "b", "id": ORG})
    );
    assert_eq!(envelope["data"]["totalCount"], 2);
    assert_eq!(envelope["meta"]["limit"], 2);
    assert_eq!(envelope["meta"]["page"], 1);
    assert_eq!(envelope["meta"]["hasMore"], true);
    assert_eq!(envelope["meta"]["nextPage"], 2);
    assert_eq!(
        stub.api()[0].query(),
        "itemsPerPage=2&pageNum=1&includeCount=false"
    );
}

#[test]
fn changes_are_refused_before_any_request_without_read_write_access_or_yes() {
    let stub = Stub::start(Vec::new());
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    for (access, arguments, code, expected) in [
        (
            None,
            vec![
                "atlas", "projects", "create", "Shop", "--org-id", ORG, "--yes",
            ],
            3,
            "forbidden",
        ),
        (
            None,
            vec![
                "atlas",
                "streams",
                "workspaces",
                "delete",
                "w",
                "--project-id",
                PROJECT,
                "--yes",
            ],
            3,
            "forbidden",
        ),
        (
            Some("read-write"),
            vec!["atlas", "projects", "create", "Shop", "--org-id", ORG],
            2,
            "validation",
        ),
        (
            Some("read-write"),
            vec!["atlas", "clusters", "pause", "c0", "--project-id", PROJECT],
            2,
            "validation",
        ),
        (
            Some("read-write"),
            vec![
                "atlas",
                "db-users",
                "create",
                "app",
                "--project-id",
                PROJECT,
                "--role",
                "read",
            ],
            2,
            "validation",
        ),
        (
            Some("read-write"),
            vec![
                "atlas",
                "streams",
                "processors",
                "start",
                "w",
                "p",
                "--project-id",
                PROJECT,
            ],
            2,
            "validation",
        ),
        (
            Some("read-write"),
            vec![
                "atlas",
                "access-list",
                "add",
                "--project-id",
                PROJECT,
                "--yes",
            ],
            2,
            "validation",
        ),
        (
            Some("read-write"),
            vec![
                "atlas",
                "clusters",
                "inspect",
                "c0",
                "--project-id",
                "short",
            ],
            2,
            "validation",
        ),
    ] {
        let mut command = atlas(&config, &stub.base);
        if let Some(access) = access {
            command.env("MONGODB_CLI_ACCESS", access);
        }
        let error = failure(&command.args(&arguments).output().unwrap(), code);
        assert_eq!(error["error"]["code"], expected, "{arguments:?}");
        if code == 3 {
            assert!(
                error["error"]["hint"]
                    .as_str()
                    .unwrap()
                    .contains("MONGODB_CLI_ACCESS=read-write")
            );
        }
        if arguments.contains(&"Shop") && code == 2 {
            assert!(error["error"]["hint"].as_str().unwrap().contains("--yes"));
        }
    }
    assert!(
        stub.requests().is_empty(),
        "refusals happen before any request"
    );
}

#[test]
fn confirmed_changes_send_the_documented_bodies() {
    let stub = Stub::start(vec![(
        "POST",
        "/api/atlas/v2/groups".into(),
        201,
        json!({"id": PROJECT, "name": "Shop", "orgId": ORG}),
    )]);
    let directory = tempfile::tempdir().unwrap();
    let envelope = success(
        &atlas(&directory.path().join("config.toml"), &stub.base)
            .env("MONGODB_CLI_ACCESS", "read-write")
            .args([
                "atlas", "projects", "create", "Shop", "--org-id", ORG, "--yes",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(envelope["command"], "atlas.projects.create");
    assert_eq!(
        envelope["data"],
        json!({"projectName": "Shop", "orgId": ORG, "id": PROJECT})
    );
    assert_eq!(envelope["meta"]["access"], "read-write");
    let request = &stub.api()[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.json(), json!({"name": "Shop", "orgId": ORG}));
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(request.header("accept"), Some(V2));
}

#[test]
fn free_cluster_upgrades_use_the_older_api_version() {
    let free = json!({"id": "c-id", "name": "c0", "replicationSpecs": [{"regionConfigs": [{
        "providerName": "TENANT", "backingProviderName": "AWS", "regionName": "US_EAST_1", "electableSpecs": {"instanceSize": "M0"}}]}]});
    let stub = Stub::start(vec![
        (
            "POST",
            format!("/api/atlas/v2/groups/{PROJECT}/clusters/tenantUpgrade"),
            200,
            json!({"id": "c-id"}),
        ),
        (
            "GET",
            format!("/api/atlas/v2/groups/{PROJECT}/clusters/c0"),
            200,
            free,
        ),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let envelope = success(
        &atlas(&directory.path().join("config.toml"), &stub.base)
            .env("MONGODB_CLI_ACCESS", "read-write")
            .args([
                "atlas",
                "clusters",
                "upgrade",
                "c0",
                "--project-id",
                PROJECT,
                "--yes",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(
        envelope["data"],
        json!({"originalTier": "FREE", "targetTier": "FLEX", "resolvedProvider": "AWS", "resolvedRegion": "US_EAST_1", "clusterId": "c-id"})
    );
    let upgrade = stub
        .api()
        .into_iter()
        .find(|request| request.method == "POST")
        .unwrap();
    assert_eq!(
        upgrade.header("accept"),
        Some("application/vnd.atlas.2023-01-01+json")
    );
    assert_eq!(
        upgrade.json(),
        json!({"name": "c0", "providerSettings": {"providerName": "FLEX", "instanceSizeName": "FLEX", "backingProviderName": "AWS", "regionName": "US_EAST_1"}})
    );
}

#[test]
fn atlas_errors_keep_the_status_and_atlas_error_code() {
    let missing = json!({"detail": format!("No cluster named c9 exists in group {PROJECT}."), "error": 404, "errorCode": "CLUSTER_NOT_FOUND", "reason": "Not Found"});
    let stub = Stub::start(vec![
        (
            "GET",
            format!("/api/atlas/v2/groups/{PROJECT}/clusters/c9"),
            404,
            missing,
        ),
        (
            "GET",
            format!("/api/atlas/v2/groups/{PROJECT}/flexClusters/c9"),
            400,
            json!({"reason": "Bad Request"}),
        ),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let error = failure(
        &atlas(&directory.path().join("config.toml"), &stub.base)
            .args([
                "atlas",
                "clusters",
                "inspect",
                "c9",
                "--project-id",
                PROJECT,
            ])
            .output()
            .unwrap(),
        5,
    );
    assert_eq!(
        error["error"],
        json!({
            "code": "not_found",
            "message": format!("[404 Not Found] error calling Atlas API: Not Found; No cluster named c9 exists in group {PROJECT}."),
            "serverCode": 404,
            "codeName": "CLUSTER_NOT_FOUND"
        })
    );
}

#[test]
fn rejected_credentials_fail_authentication_without_printing_secrets() {
    let stub = Stub::start(vec![(
        "POST",
        "/api/oauth/token".into(),
        401,
        json!({"error": "invalid_client", "error_description": "bad client secret"}),
    )]);
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    let output = atlas(&config, &stub.base)
        .args(["atlas", "orgs", "list"])
        .output()
        .unwrap();
    let error = failure(&output, 3);
    assert_eq!(error["error"]["code"], "unauthorized");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid_client")
    );
    assert!(
        error["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("configure-api-access")
    );
    assert!(!printed(&output).contains(CLIENT_SECRET));
    assert!(stub.api().is_empty());

    // Login verifies the credentials before anything is stored.
    std::fs::write(&config, "[profiles.admin]\naccess = \"read-write\"\n").unwrap();
    let output = atlas(&config, &stub.base)
        .env_remove("MONGODB_ATLAS_CLIENT_ID")
        .env_remove("MONGODB_ATLAS_CLIENT_SECRET")
        .args([
            "--profile",
            "admin",
            "auth",
            "login",
            "--atlas",
            "--credentials-stdin",
        ])
        .write_stdin(format!("{CLIENT_ID}\n{CLIENT_SECRET}\n"))
        .output()
        .unwrap();
    assert_eq!(failure(&output, 3)["error"]["code"], "unauthorized");
    assert!(!printed(&output).contains(CLIENT_SECRET));
    assert!(!std::fs::read_to_string(&config).unwrap().contains("atlas"));
    let output = atlas(&config, &stub.base)
        .args(["--profile", "admin", "auth", "login", "--atlas"])
        .output()
        .unwrap();
    assert_eq!(failure(&output, 2)["error"]["code"], "validation");
}

#[test]
fn regions_are_listed_without_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let output = atlas(&directory.path().join("config.toml"), "http://127.0.0.1:9/")
        .env_remove("MONGODB_ATLAS_CLIENT_ID")
        .env_remove("MONGODB_ATLAS_CLIENT_SECRET")
        .args(["atlas", "regions", "list", "--provider", "gcp"])
        .output()
        .unwrap();
    let envelope = success(&output);
    assert_eq!(envelope["data"]["provider"], "GCP");
    assert_eq!(envelope["data"]["regions"].as_array().unwrap().len(), 41);
    assert_eq!(
        envelope["data"]["regions"][0],
        json!({"name": "CENTRAL_US", "location": "Iowa, USA"})
    );
}

#[test]
fn db_user_passwords_come_from_stdin_and_are_never_printed() {
    let stub = Stub::start(vec![
        (
            "GET",
            "/api/private/ipinfo".into(),
            200,
            json!({"currentIpv4Address": "203.0.113.7"}),
        ),
        (
            "POST",
            format!("/api/atlas/v2/groups/{PROJECT}/accessList"),
            409,
            json!({"errorCode": "DUPLICATE_ACCESS_LIST_ENTRY", "reason": "Conflict"}),
        ),
        (
            "POST",
            format!("/api/atlas/v2/groups/{PROJECT}/databaseUsers"),
            201,
            json!({"username": "app"}),
        ),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let output = atlas(&directory.path().join("config.toml"), &stub.base)
        .env("MONGODB_CLI_ACCESS", "read-write")
        .args([
            "atlas",
            "db-users",
            "create",
            "app",
            "--project-id",
            PROJECT,
            "--role",
            "readWrite@shop.orders",
            "--cluster",
            "c0",
            "--password-stdin",
            "--yes",
        ])
        .write_stdin("pa55-word-from-stdin\n")
        .output()
        .unwrap();
    let envelope = success(&output);
    assert_eq!(
        envelope["data"],
        json!({"username": "app", "accessList": "already-present"})
    );
    let api = stub.api();
    assert_eq!(
        api[0].json(),
        json!([{"groupId": PROJECT, "ipAddress": "203.0.113.7", "comment": "Added by mongodb-cli to enable tool access"}])
    );
    let user = api[1].json();
    assert_eq!(user["password"], "pa55-word-from-stdin");
    assert_eq!(
        user["roles"],
        json!([{"roleName": "readWrite", "databaseName": "shop", "collectionName": "orders"}])
    );
    assert_eq!(user["scopes"], json!([{"type": "CLUSTER", "name": "c0"}]));
    assert_eq!(user["databaseName"], "admin");
    assert!(!printed(&output).contains("pa55-word-from-stdin"));
}

#[test]
fn processors_referencing_missing_connections_are_rejected() {
    let stub = Stub::start(vec![(
        "GET",
        format!("/api/atlas/v2/groups/{PROJECT}/streams/w1/connections"),
        200,
        json!({"results": [{"name": "src", "type": "Kafka"}]}),
    )]);
    let directory = tempfile::tempdir().unwrap();
    let error = failure(
        &atlas(&directory.path().join("config.toml"), &stub.base)
            .env("MONGODB_CLI_ACCESS", "read-write")
            .args([
                "atlas", "streams", "processors", "create", "w1", "p1", "--project-id", PROJECT, "--yes", "--pipeline",
                r#"[{"$source": {"connectionName": "src"}}, {"$emit": {"connectionName": "sink", "topic": "t"}}]"#,
            ])
            .output()
            .unwrap(),
        5,
    );
    assert_eq!(error["error"]["code"], "request_rejected");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Missing: sink")
    );
    let api = stub.api();
    assert_eq!(api.len(), 1, "nothing is created");
    assert_eq!(api[0].query(), "itemsPerPage=100&pageNum=1");
}

#[test]
fn connect_deletes_the_temporary_user_when_verification_fails() {
    let unreachable = json!({
        "id": "6600aaaaaaaaaaaaaaaaaaaa", "name": "c0", "stateName": "IDLE",
        "replicationSpecs": [{"regionConfigs": [{"providerName": "AWS", "regionName": "US_EAST_1", "electableSpecs": {"instanceSize": "M10"}}]}],
        "connectionStrings": {"standard": "mongodb://127.0.0.1:9/?serverSelectionTimeoutMS=500"}
    });
    let stub = Stub::start(vec![
        (
            "GET",
            "/api/private/ipinfo".into(),
            200,
            json!({"currentIpv4Address": "203.0.113.7"}),
        ),
        (
            "POST",
            format!("/api/atlas/v2/groups/{PROJECT}/accessList"),
            201,
            json!({"results": []}),
        ),
        (
            "GET",
            format!("/api/atlas/v2/groups/{PROJECT}/clusters/c0"),
            200,
            unreachable,
        ),
        (
            "POST",
            format!("/api/atlas/v2/groups/{PROJECT}/databaseUsers"),
            201,
            json!({}),
        ),
        (
            "DELETE",
            format!("/api/atlas/v2/groups/{PROJECT}/databaseUsers/admin/"),
            204,
            Value::Null,
        ),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    let output = atlas(&config, &stub.base)
        .env("MONGODB_CLI_ACCESS", "read-write")
        .args([
            "--timeout",
            "2",
            "atlas",
            "clusters",
            "connect",
            "c0",
            "--project-id",
            PROJECT,
            "--save-profile",
            "shop",
            "--yes",
        ])
        .output()
        .unwrap();
    let error = failure(&output, 4);
    assert_eq!(error["error"]["code"], "timeout");
    assert!(error["error"]["hint"].as_str().unwrap().contains("deleted"));
    let api = stub.api();
    let created = api
        .iter()
        .find(|request| request.method == "POST" && request.path().ends_with("/databaseUsers"))
        .unwrap()
        .json();
    let username = created["username"].as_str().unwrap();
    assert!(
        username.starts_with("mongodbCli") && username.len() == 18,
        "{username}"
    );
    assert_eq!(
        created["roles"],
        json!([{"roleName": "readAnyDatabase", "databaseName": "admin"}])
    );
    assert_eq!(
        created["scopes"],
        json!([{"type": "CLUSTER", "name": "c0"}])
    );
    assert_eq!(
        created["description"],
        "mongodb-cli temporary user for profile shop"
    );
    assert!(created["deleteAfterDate"].as_str().unwrap().ends_with('Z'));
    let deleted = api
        .iter()
        .find(|request| request.method == "DELETE")
        .expect("the temporary user is deleted");
    assert_eq!(
        deleted.path(),
        format!("/api/atlas/v2/groups/{PROJECT}/databaseUsers/admin/{username}")
    );
    let password = created["password"].as_str().unwrap();
    assert_eq!(password.len(), 22);
    assert!(!printed(&output).contains(password));
    assert!(!config.exists() || !std::fs::read_to_string(&config).unwrap().contains("shop"));
}

#[test]
fn profile_show_reports_credential_presence_as_booleans() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    std::fs::write(
        &config,
        "[profiles.both]\naccess = \"read-only\"\ntarget = \"mongodb://db.example.com\"\natlas = true\n[profiles.none]\n",
    )
    .unwrap();
    for (name, connection_string, atlas_credentials) in
        [("both", true, true), ("none", false, false)]
    {
        let envelope = success(
            &atlas(&config, "http://127.0.0.1:9/")
                .args(["profile", "show", name])
                .output()
                .unwrap(),
        );
        assert_eq!(
            envelope["data"]["credentials"],
            json!({"connectionString": connection_string, "atlas": atlas_credentials}),
            "{name}"
        );
    }
}
