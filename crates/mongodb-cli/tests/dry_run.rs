use assert_cmd::Command;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
fn cli(config: &Path) -> Command {
    let mut command = Command::cargo_bin("mongodb-cli").unwrap();
    command
        .env("MONGODB_CLI_CONFIG", config)
        .env("MONGODB_URI", "mongodb://127.0.0.1:9/")
        .env("MONGODB_CLI_ACCESS", "read-only")
        .args(["--output", "json", "--dry-run"])
        .timeout(Duration::from_secs(3));
    command
}
#[test]
fn plans_document_writes_offline_and_preserves_extended_json_types() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("broken.toml");
    std::fs::write(&config, "broken = [").unwrap();
    let output = cli(&config)
        .args([
            "documents",
            "update",
            "app",
            "items",
            "--filter",
            r#"{"id":{"$numberLong":"9007199254740993"}}"#,
            "--update",
            r#"{"$set":{"active":false}}"#,
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["command"], "documents.plan");
    assert_eq!(plan["data"]["executed"], false);
    assert_eq!(plan["data"]["authorizationChecked"], false);
    assert_eq!(
        plan["data"]["request"]["filter"]["id"],
        json!({"$numberLong":"9007199254740993"})
    );
    let output = cli(&config)
        .args(["documents", "delete", "app", "items", "--filter", "{}"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["data"]["emptyFilter"], true);
    assert_eq!(plan["data"]["requiresYesByDefault"], true);
    assert_eq!(plan["data"]["profileRulesChecked"], false);
    assert!(plan["data"]["matchedCount"].is_null());
    let output = cli(&config)
        .args([
            "documents",
            "insert",
            "app",
            "items",
            "--documents",
            r#"[{"id":{"$numberLong":"9007199254740993"}}]"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        plan["data"]["request"]["documents"][0]["id"],
        json!({"$numberLong":"9007199254740993"})
    );
    assert_eq!(std::fs::read_to_string(config).unwrap(), "broken = [");
}
#[test]
fn preview_rejects_invalid_inputs_and_unsupported_commands_before_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    cli(&config)
        .args(["profile", "add", "must-not-exist"])
        .assert()
        .code(2)
        .stdout("");
    assert!(!config.exists());
    let out = dir.path().join("must-not-be-written.json");
    cli(&config)
        .args(["documents", "export", "app", "items", "--out"])
        .arg(&out)
        .assert()
        .code(2)
        .stdout("");
    assert!(!out.exists());
    for update in ["1", "[1]", "[{}", "null"] {
        cli(&config)
            .args([
                "documents",
                "update",
                "app",
                "items",
                "--filter",
                "{}",
                "--update",
                update,
            ])
            .assert()
            .code(2)
            .stdout("");
    }
    cli(&config)
        .args(["documents", "insert", "app", "items", "--documents", "[]"])
        .assert()
        .code(2)
        .stdout("");
}
