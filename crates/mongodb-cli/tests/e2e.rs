//! End-to-end checks against a real MongoDB server.
//!
//! `MONGODB_CLI_TEST_URI` selects an existing disposable server. Otherwise the test starts a
//! temporary `mongod` from `MONGODB_CLI_TEST_MONGOD` or `PATH`, and skips when none exists.

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Output, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use serde_json::{Value, json};

struct TestServer {
    uri: String,
    child: Option<Child>,
    _directory: Option<tempfile::TempDir>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn find_mongod() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("MONGODB_CLI_TEST_MONGOD") {
        return Some(PathBuf::from(path));
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join("mongod"))
        .find(|candidate| candidate.is_file())
}

fn start_server() -> Option<TestServer> {
    if let Ok(uri) = std::env::var("MONGODB_CLI_TEST_URI") {
        return Some(TestServer {
            uri,
            child: None,
            _directory: None,
        });
    }
    let mongod = find_mongod()?;
    let directory = tempfile::tempdir().unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let child = ProcessCommand::new(mongod)
        .args(["--dbpath"])
        .arg(directory.path())
        .args([
            "--port",
            &port.to_string(),
            "--bind_ip",
            "127.0.0.1",
            "--quiet",
        ])
        .arg("--logpath")
        .arg(directory.path().join("mongod.log"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let server = TestServer {
        uri: format!("mongodb://127.0.0.1:{port}/"),
        child: Some(child),
        _directory: Some(directory),
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            Instant::now() < deadline,
            "mongod did not start on port {port}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    Some(server)
}

struct Session<'a> {
    uri: &'a str,
    config: PathBuf,
}

impl Session<'_> {
    fn command(&self, access: &str) -> Command {
        let mut command = Command::cargo_bin("mongodb-cli").unwrap();
        // Connection settings from the developer's environment must not change the results.
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("MONGODB_CLI_") {
                command.env_remove(name);
            }
        }
        command
            .env("MONGODB_CLI_CONFIG", &self.config)
            .env("MONGODB_URI", self.uri)
            .env("MONGODB_CLI_ACCESS", access)
            .env_remove("MONGODB_ATLAS_CLIENT_ID")
            .env_remove("MONGODB_ATLAS_CLIENT_SECRET")
            .args(["--timeout", "30"])
            .timeout(Duration::from_secs(60));
        command
    }

    fn run(&self, access: &str, arguments: &[&str]) -> Output {
        self.command(access)
            .args(["--output", "json"])
            .args(arguments)
            .output()
            .unwrap()
    }

    fn ok(&self, access: &str, arguments: &[&str]) -> Value {
        let output = self.run(access, arguments);
        assert!(
            output.status.success(),
            "{arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn fails(&self, access: &str, arguments: &[&str], code: i32) -> Value {
        let output = self.run(access, arguments);
        assert_eq!(
            output.status.code(),
            Some(code),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
        serde_json::from_slice(&output.stderr).unwrap()
    }
}

#[test]
fn end_to_end_against_a_real_server() {
    let Some(server) = start_server() else {
        eprintln!("skipping: set MONGODB_CLI_TEST_URI or put mongod on PATH");
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    let session = Session {
        uri: &server.uri,
        config: directory.path().join("config.toml"),
    };
    let database = format!("mongodb_cli_e2e_{}", std::process::id());
    let db = database.as_str();
    run_scenario(&session, db);
    run_limits_and_export_scenario(&session, db, directory.path());
    // Leave an external server clean even when it is reused.
    let _ = session.run("read-write", &["databases", "drop", db, "--yes"]);
    assert_config_untouched(&session.config);
}

fn run_scenario(session: &Session<'_>, db: &str) {
    let doctor = session.ok("read-only", &["doctor"]);
    // Atlas tools are disabled by default, so only the five MongoDB checks run.
    let checks = doctor["data"].as_array().unwrap();
    assert_eq!(checks.len(), 5);
    assert!(checks.iter().all(|check| check["status"] == "pass"));
    assert_eq!(
        session.ok("read-only", &["server", "info"])["data"]["version"]
            .as_str()
            .map(|version| !version.is_empty()),
        Some(true)
    );

    // Read-only connections refuse writes before contacting the server.
    let refused = session.fails(
        "read-only",
        &["documents", "insert", db, "users", "--documents", "{}"],
        3,
    );
    assert_eq!(refused["error"]["code"], "forbidden");

    let inserted = session.ok(
        "read-write",
        &[
            "documents",
            "insert",
            db,
            "users",
            "--documents",
            r#"[{"name": "a", "n": 1, "tags": ["x"], "at": {"$date": "2026-10-10T00:00:00Z"}},
                {"name": "b", "n": 2}, {"name": "c", "n": 3, "address": {"city": "杭州"}}]"#,
        ],
    );
    assert_eq!(inserted["data"]["insertedCount"], 3);
    assert!(inserted["data"]["insertedIds"][0]["$oid"].is_string());

    let first = session.ok(
        "read-only",
        &[
            "documents",
            "find",
            db,
            "users",
            "--sort",
            r#"{"n": 1}"#,
            "--limit",
            "2",
            "--projection",
            r#"{"_id": 0}"#,
        ],
    );
    assert_eq!(
        first["data"][0],
        json!({"name": "a", "n": 1, "tags": ["x"], "at": {"$date": "2026-10-10T00:00:00Z"}})
    );
    assert_eq!(first["meta"]["hasMore"], true);
    assert_eq!(first["meta"]["nextOffset"], 2);
    assert_eq!(first["meta"]["total"], 3);
    let second = session.ok(
        "read-only",
        &[
            "documents",
            "find",
            db,
            "users",
            "--sort",
            r#"{"n": 1}"#,
            "--limit",
            "2",
            "--offset",
            "2",
        ],
    );
    assert_eq!(second["data"].as_array().unwrap().len(), 1);
    assert_eq!(second["meta"]["hasMore"], false);

    assert_eq!(
        session.ok(
            "read-only",
            &[
                "documents",
                "count",
                db,
                "users",
                "--filter",
                r#"{"n": {"$gte": 2}}"#
            ]
        )["data"],
        json!({"count": 2})
    );
    let sum = session.ok(
        "read-only",
        &[
            "documents",
            "aggregate",
            db,
            "users",
            "--pipeline",
            r#"[{"$group": {"_id": null, "sum": {"$sum": "$n"}}}]"#,
        ],
    );
    assert_eq!(sum["data"], json!([{"_id": null, "sum": 6}]));

    let updated = session.ok(
        "read-write",
        &[
            "documents",
            "update",
            db,
            "users",
            "--filter",
            r#"{"name": "a"}"#,
            "--update",
            r#"{"$set": {"n": 10}}"#,
        ],
    );
    assert_eq!(updated["data"]["modifiedCount"], 1);

    // Index key order is preserved and the server validates every option.
    let index = session.ok(
        "read-write",
        &[
            "indexes",
            "create",
            db,
            "users",
            "--keys",
            r#"{"n": 1, "name": -1}"#,
            "--options",
            r#"{"unique": true}"#,
        ],
    );
    assert_eq!(index["data"]["name"], "n_1_name_-1");
    let indexes = session.ok("read-only", &["indexes", "list", db, "users"]);
    assert_eq!(indexes["data"][1]["key"], json!({"n": 1, "name": -1}));
    let rejected = session.fails(
        "read-write",
        &[
            "indexes",
            "create",
            db,
            "users",
            "--keys",
            r#"{"x": 1}"#,
            "--options",
            r#"{"uniqe": true}"#,
        ],
        5,
    );
    assert_eq!(rejected["error"]["serverCode"], 197);
    let duplicate = session.fails(
        "read-write",
        &[
            "documents",
            "insert",
            db,
            "users",
            "--documents",
            r#"{"name": "a", "n": 10}"#,
        ],
        5,
    );
    assert_eq!(duplicate["error"]["serverCode"], 11000);

    let schema = session.ok("read-only", &["collections", "schema", db, "users"]);
    assert_eq!(schema["data"]["sampled"], 3);
    assert!(
        schema["data"]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field["path"] == "address.city")
    );
    let stats = session.ok("read-only", &["collections", "stats", db, "users"]);
    assert_eq!(stats["data"]["count"], 3);
    let plan = session.ok(
        "read-only",
        &["explain", "find", db, "users", "--filter", r#"{"n": 10}"#],
    );
    assert!(plan["data"]["queryPlanner"].is_object());
    let missing = session.fails("read-only", &["collections", "stats", db, "missing"], 5);
    assert_eq!(missing["error"]["codeName"], "NamespaceNotFound");

    // Write pipelines are destructive: they need read-write access and --yes.
    let out = [
        "documents",
        "aggregate",
        db,
        "users",
        "--pipeline",
        r#"[{"$match": {}}, {"$out": "users_copy"}]"#,
    ];
    session.fails("read-only", &[&out[..], &["--yes"]].concat(), 3);
    let unconfirmed = session.fails("read-write", &out, 2);
    let target = format!("$out replaces every document in {db}.users_copy");
    assert!(
        unconfirmed["error"]["message"]
            .as_str()
            .unwrap()
            .contains(&target),
        "{unconfirmed}"
    );
    let written = session.ok("read-write", &[&out[..], &["--yes"]].concat());
    assert_eq!(written["meta"]["writeTargets"], json!([target]));
    session.ok(
        "read-write",
        &["collections", "rename", db, "users_copy", "users_renamed"],
    );
    let collections = session.ok("read-only", &["collections", "list", db]);
    let names: Vec<_> = collections["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["users", "users_renamed"]);

    session.fails(
        "read-write",
        &["indexes", "drop", db, "users", "n_1_name_-1"],
        2,
    );
    session.ok(
        "read-write",
        &["indexes", "drop", db, "users", "n_1_name_-1", "--yes"],
    );
    let deleted = session.ok(
        "read-write",
        &[
            "documents",
            "delete",
            db,
            "users",
            "--filter",
            r#"{"n": {"$lt": 3}}"#,
            "--yes",
        ],
    );
    assert_eq!(deleted["data"]["deletedCount"], 1);
    session.ok(
        "read-write",
        &["collections", "drop", db, "users_renamed", "--yes"],
    );
    let documents = session.ok(
        "read-only",
        &[
            "databases",
            "aggregate",
            "admin",
            "--pipeline",
            r#"[{"$documents": [{"a": 1}, {"a": 2}]}]"#,
            "--limit",
            "1",
        ],
    );
    assert_eq!(documents["data"], json!([{"a": 1}]));
    assert_eq!(documents["meta"]["nextOffset"], 1);
    let logs = session.ok("read-only", &["server", "logs", "--limit", "3"]);
    assert!(logs["data"]["logs"].as_array().unwrap().len() <= 3);

    let output = session
        .command("read-only")
        .args(["--output", "table", "documents", "find", db, "users"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let table = String::from_utf8(output.stdout).unwrap();
    assert!(table.starts_with("_id"), "{table}");
    assert!(!table.contains("$oid"));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("Access: read-only")
    );
}

fn run_limits_and_export_scenario(session: &Session<'_>, db: &str, directory: &Path) {
    // 150 documents of about 8 KiB exceed both the 100-document page cap and the 1 MiB
    // response limit; they go through a file because they would not fit on a command line.
    let pad = "x".repeat(8 * 1024);
    let documents: Vec<Value> = (0..150).map(|n| json!({"n": n, "pad": pad})).collect();
    let input = directory.join("big.json");
    std::fs::write(&input, serde_json::to_vec(&documents).unwrap()).unwrap();
    let input = input.to_str().unwrap();
    let inserted = session.ok(
        "read-write",
        &["documents", "insert", db, "big", "--file", input],
    );
    assert_eq!(inserted["data"]["insertedCount"], 150);

    let small = r#"{"pad": 0}"#;
    let capped = session.ok(
        "read-only",
        &[
            "documents",
            "find",
            db,
            "big",
            "--limit",
            "150",
            "--projection",
            small,
        ],
    );
    assert_eq!(capped["data"].as_array().unwrap().len(), 100);
    assert_eq!(capped["meta"]["limit"], 100);
    assert_eq!(capped["meta"]["nextOffset"], 100);
    assert_eq!(capped["meta"]["total"], 150);
    assert_eq!(
        capped["meta"]["appliedLimits"],
        json!(["maxDocumentsPerQuery"])
    );

    let truncated = session.ok(
        "read-only",
        &[
            "documents",
            "find",
            db,
            "big",
            "--limit",
            "100",
            "--response-bytes-limit",
            "100000",
        ],
    );
    let returned = truncated["data"].as_array().unwrap().len();
    assert!((1..100).contains(&returned), "{returned}");
    assert_eq!(truncated["meta"]["nextOffset"], returned);
    assert_eq!(
        truncated["meta"]["appliedLimits"],
        json!(["responseBytesLimit"])
    );
    let too_large = session.fails(
        "read-only",
        &[
            "documents",
            "find",
            db,
            "big",
            "--response-bytes-limit",
            "1000",
        ],
        7,
    );
    assert_eq!(too_large["error"]["code"], "response_too_large");

    let aggregated = session.ok(
        "read-only",
        &[
            "documents",
            "aggregate",
            db,
            "big",
            "--pipeline",
            r#"[{"$project": {"pad": 0}}]"#,
            "--limit",
            "5",
        ],
    );
    assert_eq!(aggregated["data"].as_array().unwrap().len(), 5);
    assert_eq!(aggregated["meta"]["total"], 150);
    assert_eq!(aggregated["meta"]["hasMore"], true);

    // Exports are complete: no page cap, no byte limit.
    let output = session
        .command("read-only")
        .args(["documents", "export", db, "big", "--sort", r#"{"n": 1}"#])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.len() > 1024 * 1024);
    let exported: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(exported.len(), 150);
    assert_eq!(exported[149]["n"], 149, "relaxed numbers are plain");
    for (format, first) in [
        ("canonical", json!({"$numberInt": "0"})),
        ("relaxed", json!(0)),
    ] {
        let path = directory.join(format!("export-{format}.json"));
        let written = session.ok(
            "read-only",
            &[
                "documents",
                "export",
                db,
                "big",
                "--pipeline",
                r#"[{"$sort": {"n": 1}}]"#,
                "--format",
                format,
                "--out",
                path.to_str().unwrap(),
            ],
        );
        assert_eq!(
            written["data"],
            json!({"path": path.display().to_string(), "documents": 150, "format": format})
        );
        let file: Vec<Value> = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(file.len(), 150);
        assert_eq!(file[0]["n"], first, "{format}");
    }
    // A failed export leaves neither the file nor its temporary copy behind.
    let failed = directory.join("failed.json");
    session.fails(
        "read-only",
        &[
            "documents",
            "export",
            db,
            "big",
            "--pipeline",
            r#"[{"$project": {"x": {"$divide": ["$n", 0]}}}]"#,
            "--out",
            failed.to_str().unwrap(),
        ],
        5,
    );
    let leftovers: Vec<_> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("failed.json"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // --require-index refuses a collection scan and passes once an index exists.
    let filter = r#"{"n": 7}"#;
    let find = [
        "--require-index",
        "documents",
        "find",
        db,
        "big",
        "--filter",
        filter,
        "--projection",
        small,
    ];
    let scan = session.fails("read-only", &find, 3);
    assert_eq!(scan["error"]["code"], "forbidden");
    assert!(
        scan["error"]["message"]
            .as_str()
            .unwrap()
            .contains("collection scan")
    );
    session.ok(
        "read-write",
        &["indexes", "create", db, "big", "--keys", r#"{"n": 1}"#],
    );
    assert_eq!(session.ok("read-only", &find)["data"][0]["n"], 7);

    // The verbosity reaches the server, which may not support it.
    let explain = [
        "explain",
        "find",
        db,
        "big",
        "--filter",
        filter,
        "--verbosity",
        "query-planner-extended",
    ];
    let output = session.run("read-only", &explain);
    if output.status.success() {
        let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(plan["data"]["queryPlanner"].is_object());
    } else {
        let rejected = session.fails("read-only", &explain, 5);
        assert!(
            rejected["error"]["message"]
                .as_str()
                .unwrap()
                .contains("queryPlannerExtended"),
            "{rejected}"
        );
    }

    // Without Atlas Search, --type all still lists the classic indexes.
    let all = session.ok(
        "read-only",
        &["indexes", "list", db, "big", "--type", "all"],
    );
    assert_eq!(all["data"]["classic"].as_array().unwrap().len(), 2);
    let search = all["data"]["search"].as_array().unwrap();
    let warnings = all["meta"]["warnings"].as_array().map_or(0, Vec::len);
    assert!(
        search.is_empty() == (warnings == 1),
        "an empty search list comes with a warning: {all}"
    );

    let upserted = session.ok(
        "read-write",
        &[
            "documents",
            "update",
            db,
            "big",
            "--filter",
            r#"{"n": -1}"#,
            "--update",
            r#"{"$set": {"pad": ""}}"#,
            "--upsert",
        ],
    );
    assert_eq!(upserted["data"]["upsertedCount"], 1);
    assert_eq!(upserted["data"]["matchedCount"], 0);
    let logs = session.ok("read-only", &["server", "logs", "--limit", "2"]);
    assert_eq!(
        logs["data"]["shownCount"],
        logs["data"]["logs"].as_array().unwrap().len()
    );
    let schema = session.ok("read-only", &["collections", "schema", db, "big"]);
    assert_eq!(schema["data"]["fieldsCount"], 3, "_id, n, and pad");
    assert_eq!(schema["data"]["truncated"], false);

    let merged = session.ok(
        "read-write",
        &[
            "documents",
            "aggregate",
            db,
            "big",
            "--pipeline",
            r#"[{"$match": {"n": {"$lt": 3}}}, {"$merge": {"into": "small", "whenMatched": "replace"}}]"#,
            "--yes",
        ],
    );
    assert_eq!(
        merged["meta"]["writeTargets"],
        json!([format!(
            "$merge writes into {db}.small (whenMatched: replace)"
        )])
    );
}

fn assert_config_untouched(config: &Path) {
    assert!(
        !config.exists(),
        "environment connections never write the config file"
    );
}
