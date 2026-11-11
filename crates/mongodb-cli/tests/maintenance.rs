use assert_cmd::Command;
use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    time::{Duration, Instant},
};

const APP: &str = "mongodb-cli";
const PREFIX: &str = "MONGODB_CLI";

fn cli(home: &std::path::Path) -> Command {
    let mut command = Command::cargo_bin(APP).unwrap();
    command
        .current_dir(home)
        .env("HOME", home)
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env(format!("{PREFIX}_CONFIG"), home.join("missing.toml"))
        .env_remove("SIGNOZ_URL")
        .env_remove("SIGNOZ_API_KEY")
        .env_remove("MONGODB_URI")
        .env_remove("MONGODB_ATLAS_CLIENT_ID")
        .env_remove("MONGODB_ATLAS_CLIENT_SECRET")
        .env("MONGODB_CLI_ATLAS_TOOLS", "disabled")
        .env_remove(format!("{PREFIX}_SKILL"))
        .args(["--output", "json", "doctor"])
        .timeout(Duration::from_secs(8));
    command
}

fn checks(output: &std::process::Output) -> Vec<Value> {
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice::<Value>(&output.stdout).unwrap()["data"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn missing_connection_still_reports_skills_and_explicit_update_skip() {
    let home = tempfile::tempdir().unwrap();
    let output = cli(home.path()).arg("--no-update-check").output().unwrap();
    let checks = checks(&output);
    assert_eq!(checks.len(), 7);
    assert_eq!(checks[1]["status"], "fail");
    assert_eq!(checks[5]["name"], "package");
    assert_eq!(checks[5]["message"], "disabled by --no-update-check");
    assert!(
        checks[6]["message"]
            .as_str()
            .unwrap()
            .starts_with("not_installed:")
    );
}

#[test]
fn project_skill_copies_and_loaded_version_are_independent_of_connection_health() {
    let home = tempfile::tempdir().unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins")
        .join(APP)
        .join("skills");
    for item in fs::read_dir(source).unwrap() {
        let item = item.unwrap();
        if !item.path().join("SKILL.md").is_file() {
            continue;
        }
        let target = home.path().join(".agents/skills").join(item.file_name());
        fs::create_dir_all(&target).unwrap();
        fs::copy(item.path().join("SKILL.md"), target.join("SKILL.md")).unwrap();
    }
    for (loaded, state) in [
        ("", "installed_not_loaded"),
        ("0.0.0", "reload_required"),
        (env!("CARGO_PKG_VERSION"), "current"),
    ] {
        let output = cli(home.path())
            .arg("--no-update-check")
            .env(format!("{PREFIX}_SKILL"), loaded)
            .output()
            .unwrap();
        let checks = checks(&output);
        assert!(
            checks[6]["message"].as_str().unwrap().starts_with(state),
            "{}",
            checks[6]
        );
    }
}

#[test]
fn release_update_advice_is_available_even_when_connection_is_missing() {
    let home = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    socket.set_nonblocking(false).unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut bytes = [0; 4096];
                    let count = socket.read(&mut bytes).unwrap();
                    assert!(
                        !String::from_utf8_lossy(&bytes[..count])
                            .to_ascii_lowercase()
                            .contains("authorization:")
                    );
                    let body = r#"{"tag_name":"v99.0.0"}"#;
                    write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "no release lookup arrived");
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("{error}"),
            }
        }
    });
    let output = cli(home.path())
        .env(format!("{PREFIX}_RELEASE_API"), endpoint)
        .output()
        .unwrap();
    worker.join().unwrap();
    let checks = checks(&output);
    assert_eq!(checks[5]["status"], "pass");
    assert!(
        checks[5]["message"]
            .as_str()
            .unwrap()
            .starts_with("update_available:")
    );
    assert!(
        checks[5]["message"]
            .as_str()
            .unwrap()
            .contains("skills install")
    );
}
