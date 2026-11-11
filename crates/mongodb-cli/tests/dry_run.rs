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

#[test]
fn update_file_and_stdin_share_inline_parsing() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("broken.toml");
    std::fs::write(&config, "broken = [").unwrap();
    let file = dir.path().join("update.json");
    let update = r#"[{"$set":{"id":{"$numberLong":"9007199254740993"}}}]"#;
    std::fs::write(&file, update).unwrap();
    let base = ["documents", "update", "app", "items", "--filter", "{}"];
    let inline = cli(&config)
        .args(base)
        .args(["--update", update])
        .output()
        .unwrap();
    assert!(inline.status.success());
    for source in [file.to_str().unwrap(), "-"] {
        let output = cli(&config)
            .args(base)
            .args(["--update-file", source])
            .write_stdin(update)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            serde_json::from_slice::<Value>(&inline.stdout).unwrap()
        );
    }
    cli(&config).args(base).assert().code(2).stdout("");
    cli(&config)
        .args(base)
        .args(["--update", "{}", "--update-file", "-"])
        .write_stdin(update)
        .assert()
        .code(2)
        .stdout("");
    cli(&config)
        .args(base)
        .arg("--update-file")
        .arg(dir.path().join("missing.json"))
        .assert()
        .code(2)
        .stdout("");
    for invalid in ["[1]", "{", "null"] {
        cli(&config)
            .args(base)
            .args(["--update-file", "-"])
            .write_stdin(invalid)
            .assert()
            .code(2)
            .stdout("");
    }
}

#[test]
fn write_filters_can_use_files_or_stdin_without_silently_broadening_scope() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("broken.toml");
    std::fs::write(&config, "bad = [").unwrap();
    let filter = r#"{"id":{"$numberLong":"9007199254740993"}}"#;
    let file = dir.path().join("filter.json");
    std::fs::write(&file, filter).unwrap();
    for action in ["update", "delete"] {
        let mut base = vec!["documents", action, "app", "items"];
        if action == "update" {
            base.extend(["--update", r#"{"$set":{"active":true}}"#]);
        }
        let inline = cli(&config)
            .args(&base)
            .args(["--filter", filter])
            .output()
            .unwrap();
        assert!(inline.status.success());
        for source in [file.to_str().unwrap(), "-"] {
            let output = cli(&config)
                .args(&base)
                .args(["--filter-file", source])
                .write_stdin(filter)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                serde_json::from_slice::<Value>(&output.stdout).unwrap(),
                serde_json::from_slice::<Value>(&inline.stdout).unwrap()
            );
        }
        cli(&config).args(&base).assert().code(2).stdout("");
        cli(&config)
            .args(&base)
            .args(["--filter", "{}", "--filter-file", "-"])
            .write_stdin(filter)
            .assert()
            .code(2)
            .stdout("");
        cli(&config)
            .args(&base)
            .arg("--filter-file")
            .arg(dir.path().join("missing.json"))
            .assert()
            .code(2)
            .stdout("");
        for invalid in ["", "null", "[]", "{"] {
            cli(&config)
                .args(&base)
                .args(["--filter-file", "-"])
                .write_stdin(invalid)
                .assert()
                .code(2)
                .stdout("");
        }
    }
    cli(&config)
        .args([
            "documents",
            "update",
            "app",
            "items",
            "--filter-file",
            "-",
            "--update-file",
            "-",
        ])
        .write_stdin("{}")
        .assert()
        .code(2)
        .stdout("")
        .stderr(predicates::str::contains("cannot both read stdin"));
}
