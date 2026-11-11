use assert_cmd::Command;
use serde_json::Value;
use std::{fs, path::Path, time::Duration};

const APP: &str = "mongodb-cli";
const COUNT: usize = 9;

fn cli(home: &Path) -> Command {
    let mut command = Command::cargo_bin(APP).unwrap();
    command
        .env("HOME", home)
        .env("CLAUDE_CONFIG_DIR", home.join("custom-claude"))
        .env("MONGODB_CLI_CONFIG", home.join("broken.toml"))
        .args(["--profile", "does-not-exist", "--output", "json", "skills"])
        .timeout(Duration::from_secs(10));
    command
}

fn status(home: &Path, target: &str) -> Vec<Value> {
    let output = cli(home)
        .args(["status", "--target", target])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["command"], "skills.status");
    envelope["data"].as_array().unwrap().clone()
}

#[test]
fn status_is_read_only_and_works_with_invalid_configuration() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("broken.toml"), "TOP_SECRET = [invalid").unwrap();
    let rows = status(home.path(), "codex");
    assert_eq!(rows.len(), COUNT);
    for row in rows {
        assert_eq!(row["status"], "missing");
        assert_eq!(row["bundledVersion"], env!("CARGO_PKG_VERSION"));
        assert!(
            row["missingFiles"]
                .as_array()
                .unwrap()
                .iter()
                .any(|file| file == "SKILL.md")
        );
        assert!(!row.to_string().contains("TOP_SECRET"));
    }
    assert!(!home.path().join(".agents").exists());
    assert_eq!(
        fs::read_to_string(home.path().join("broken.toml")).unwrap(),
        "TOP_SECRET = [invalid"
    );
}

#[test]
fn status_detects_same_version_edits_missing_files_and_unreadable_files() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("broken.toml"), "invalid = [").unwrap();
    cli(home.path())
        .args(["install", "--target", "codex"])
        .assert()
        .success();
    assert!(
        status(home.path(), "codex")
            .iter()
            .all(|row| row["status"] == "current")
    );
    let dir = home.path().join(".agents/skills").join(APP);
    let original = fs::read_to_string(dir.join("SKILL.md")).unwrap();
    let edited = format!("{original}\nLocal custom instructions.\n");
    fs::write(dir.join("SKILL.md"), &edited).unwrap();
    fs::remove_file(dir.join("agents/openai.yaml")).unwrap();
    fs::write(dir.join("unmanaged.txt"), "keep me").unwrap();
    let rows = status(home.path(), "codex");
    let core = rows.iter().find(|row| row["skill"] == APP).unwrap();
    assert_eq!(core["status"], "different");
    assert_eq!(core["installedVersion"], env!("CARGO_PKG_VERSION"));
    assert_eq!(core["changedFiles"], serde_json::json!(["SKILL.md"]));
    assert_eq!(
        core["missingFiles"],
        serde_json::json!(["agents/openai.yaml"])
    );
    assert_eq!(fs::read_to_string(dir.join("SKILL.md")).unwrap(), edited);
    assert_eq!(
        fs::read_to_string(dir.join("unmanaged.txt")).unwrap(),
        "keep me"
    );
    fs::remove_file(dir.join("SKILL.md")).unwrap();
    fs::create_dir(dir.join("SKILL.md")).unwrap();
    let rows = status(home.path(), "codex");
    let core = rows.iter().find(|row| row["skill"] == APP).unwrap();
    assert_eq!(core["status"], "unreadable");
    assert_eq!(core["unreadableFiles"], serde_json::json!(["SKILL.md"]));
    let rows = status(home.path(), "claude");
    assert_eq!(rows.len(), COUNT);
    assert!(
        rows.iter()
            .all(|row| row["path"].as_str().unwrap().contains("custom-claude"))
    );
    assert!(!home.path().join("custom-claude").exists());
}
