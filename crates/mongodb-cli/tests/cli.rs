use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use clap::{CommandFactory, ValueEnum};
use clap_complete::Shell;
use mongodb_cli::args::Cli;
use predicates::prelude::*;
use serde_json::{Value, json};

const CANONICAL_SKILL: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../plugins/mongodb-cli/skills/mongodb-cli/SKILL.md"
));
const CANONICAL_OPENAI_METADATA: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../plugins/mongodb-cli/skills/mongodb-cli/agents/openai.yaml"
));
const SKILLS_SOURCE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../plugins/mongodb-cli/skills"
);
/// Nothing listens on the discard port; refusals must happen before any connection attempt.
const UNREACHABLE_URI: &str = "mongodb://127.0.0.1:9/?serverSelectionTimeoutMS=30000";

fn cli(config: &Path) -> Command {
    let mut command = Command::cargo_bin("mongodb-cli").unwrap();
    // Connection settings from the developer's environment must not change the results.
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("MONGODB_CLI_") {
            command.env_remove(name);
        }
    }
    command
        .env("MONGODB_CLI_CONFIG", config)
        .env_remove("MONGODB_URI")
        .env_remove("MONGODB_ATLAS_CLIENT_ID")
        .env_remove("MONGODB_ATLAS_CLIENT_SECRET")
        .timeout(Duration::from_secs(20));
    command
}

fn envelope(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not JSON ({error}): {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn error_envelope(output: &std::process::Output) -> Value {
    assert!(output.stdout.is_empty(), "errors keep stdout empty");
    serde_json::from_slice(&output.stderr).unwrap()
}

#[test]
fn command_tree_is_valid_and_complete() {
    Cli::command().debug_assert();
    let names: Vec<_> = Cli::command()
        .get_subcommands()
        .map(|command| command.get_name().to_owned())
        .collect();
    for expected in [
        "version",
        "completion",
        "init",
        "doctor",
        "config",
        "skills",
        "auth",
        "profile",
        "server",
        "databases",
        "collections",
        "indexes",
        "documents",
        "explain",
        "atlas",
        "local",
        "knowledge",
    ] {
        assert!(
            names.iter().any(|name| name == expected),
            "missing {expected}"
        );
    }
}

#[test]
fn menu_descriptions_stay_short() {
    fn check(command: &clap::Command) {
        for subcommand in command.get_subcommands() {
            let about = subcommand
                .get_about()
                .map(ToString::to_string)
                .unwrap_or_default();
            assert!(
                !about.trim().is_empty(),
                "{} has no description",
                subcommand.get_name()
            );
            assert!(
                about.chars().count() <= 48,
                "{}: {about}",
                subcommand.get_name()
            );
            check(subcommand);
        }
        for argument in command
            .get_arguments()
            .filter(|argument| argument.get_long().is_some())
        {
            let help = argument
                .get_help()
                .map(ToString::to_string)
                .unwrap_or_default();
            let name = argument.get_long().unwrap();
            assert!(!help.trim().is_empty(), "--{name} has no description");
            assert!(help.chars().count() <= 48, "--{name}: {help}");
        }
    }
    check(&Cli::command());
}

#[test]
fn version_and_usage_errors_follow_the_output_contract() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    let output = cli(&config)
        .args(["--output", "json", "version"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        envelope(&output)["data"]["version"],
        env!("CARGO_PKG_VERSION")
    );

    let output = cli(&config)
        .args(["--output", "json", "documents", "find"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let error = error_envelope(&output);
    assert_eq!(error["error"]["code"], "validation");
    assert!(error["error"]["hint"].as_str().unwrap().contains("--help"));
}

#[test]
fn text_usage_errors_keep_their_line_breaks() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    // A group without a subcommand shows its help, as `mongodb-cli` alone does on a terminal.
    let output = cli(&config)
        .args(["--output", "table", "documents"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.starts_with("Query and modify documents\n"),
        "{stderr}"
    );
    assert!(
        stderr.contains("\nUsage: mongodb-cli documents"),
        "{stderr}"
    );
    assert!(!stderr.contains("\\n"), "{stderr}");

    let output = cli(&config)
        .args(["--output", "table", "frobnicate"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.starts_with("error: unrecognized subcommand 'frobnicate'\n"),
        "{stderr}"
    );
}

#[test]
fn completion_scripts_are_plain_shell_code_for_every_shell() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("broken.toml");
    std::fs::write(&config, "not valid = [").unwrap();
    for shell in Shell::value_variants() {
        let output = cli(&config)
            .args(["--output", "json", "completion", &shell.to_string()])
            .env("MONGODB_URI", "mongodb://user:must-not-appear@example.com")
            .output()
            .unwrap();
        assert!(output.status.success(), "{shell}");
        assert!(output.stderr.is_empty());
        let script = String::from_utf8(output.stdout).unwrap();
        assert!(script.contains("mongodb-cli"), "{shell}");
        assert!(script.contains("documents"), "{shell}");
        assert!(!script.contains("schemaVersion"));
        assert!(!script.contains("must-not-appear"));
    }
}

#[test]
fn bash_completes_nested_commands_flags_and_enum_values() {
    if ProcessCommand::new("bash")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("completion.bash");
    std::fs::write(&script, mongodb_cli::completion::generate(Shell::Bash)).unwrap();
    for (words, current, previous, expected) in [
        (
            vec!["mongodb-cli", "docu"],
            "docu",
            "mongodb-cli",
            "documents",
        ),
        (
            vec!["mongodb-cli", "documents", "fi"],
            "fi",
            "documents",
            "find",
        ),
        (
            vec!["mongodb-cli", "--out"],
            "--out",
            "mongodb-cli",
            "--output",
        ),
        (
            vec!["mongodb-cli", "--output", "j"],
            "j",
            "--output",
            "json",
        ),
        (
            vec!["mongodb-cli", "profile", "add", "x", "--access", "read-w"],
            "read-w",
            "--access",
            "read-write",
        ),
    ] {
        let output = ProcessCommand::new("bash")
            .args([
                "--noprofile",
                "--norc",
                "-c",
                r#"
source "$1"
current=$2
previous=$3
shift 3
COMP_WORDS=("$@")
COMP_CWORD=$((${#COMP_WORDS[@]} - 1))
_mongodb__cli mongodb-cli "$current" "$previous"
printf '%s\n' "${COMPREPLY[@]}"
"#,
                "completion-test",
            ])
            .arg(&script)
            .args([current, previous])
            .args(words)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let candidates = String::from_utf8(output.stdout).unwrap();
        assert!(
            candidates.lines().any(|line| line == expected),
            "expected {expected}, received {candidates:?}"
        );
    }
}

/// Visible files below `directory` as sorted relative paths, like the walk in build.rs.
fn visible_files(directory: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        let name = PathBuf::from(path.file_name().unwrap());
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        if path.is_dir() {
            files.extend(visible_files(&path).into_iter().map(|file| name.join(file)));
        } else {
            files.push(name);
        }
    }
    files.sort();
    files
}

#[test]
fn skill_install_default_installs_every_skill_for_codex_then_claude() {
    let home = tempfile::tempdir().unwrap();
    let claude_config = tempfile::tempdir().unwrap();
    let config = home.path().join("broken.toml");
    std::fs::write(&config, "not valid = [").unwrap();
    let codex_root = home.path().join(".agents/skills");
    let claude_root = claude_config.path().join("skills");
    let source = Path::new(SKILLS_SOURCE);
    let mut skills: Vec<String> = std::fs::read_dir(source)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    skills.sort();
    assert!(
        skills.len() > 1,
        "expected the bundled Skills, found {skills:?}"
    );

    let output = cli(&config)
        .env("HOME", home.path())
        .env("CLAUDE_CONFIG_DIR", claude_config.path())
        .args(["--output", "json", "skills", "install"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let envelope = envelope(&output);
    assert_eq!(envelope["command"], "skills.install");
    let expected: Vec<Value> = [("codex", &codex_root), ("claude", &claude_root)]
        .into_iter()
        .flat_map(|(agent, root)| {
            skills.iter().map(move |skill| {
                json!({
                    "agent": agent,
                    "skill": skill,
                    "path": root.join(skill).display().to_string(),
                    "status": "installed"
                })
            })
        })
        .collect();
    assert_eq!(envelope["data"], json!(expected));

    let codex_target = codex_root.join("mongodb-cli");
    let claude_target = claude_root.join("mongodb-cli");
    assert_eq!(
        std::fs::read(codex_target.join("SKILL.md")).unwrap(),
        CANONICAL_SKILL
    );
    assert_eq!(
        std::fs::read(codex_target.join("agents/openai.yaml")).unwrap(),
        CANONICAL_OPENAI_METADATA
    );
    assert_eq!(
        std::fs::read(claude_target.join("SKILL.md")).unwrap(),
        CANONICAL_SKILL
    );
    assert!(!claude_target.join("agents/openai.yaml").exists());

    // Every bundled file arrives byte for byte; Claude Code skips Codex's interface metadata.
    for skill in &skills {
        for relative in visible_files(&source.join(skill)) {
            let expected = std::fs::read(source.join(skill).join(&relative)).unwrap();
            let label = Path::new(skill).join(&relative);
            assert_eq!(
                std::fs::read(codex_root.join(&label)).unwrap(),
                expected,
                "{}",
                label.display()
            );
            let claude_file = claude_root.join(&label);
            if relative == Path::new("agents/openai.yaml") {
                assert!(!claude_file.exists(), "{}", label.display());
            } else {
                assert_eq!(
                    std::fs::read(claude_file).unwrap(),
                    expected,
                    "{}",
                    label.display()
                );
            }
        }
    }
}

#[test]
fn profiles_default_to_read_only_and_select_by_position_then_flag_then_default() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    let run = |arguments: &[&str]| {
        cli(&config)
            .args(["--output", "json"])
            .args(arguments)
            .output()
            .unwrap()
    };

    let output = run(&["profile", "add", "prod"]);
    assert!(output.status.success());
    assert_eq!(
        envelope(&output)["data"],
        json!({"name": "prod", "access": "read-only"})
    );
    assert!(
        run(&["profile", "add", "test", "--access", "read-write"])
            .status
            .success()
    );
    assert_eq!(run(&["profile", "add", "prod"]).status.code(), Some(2));
    assert!(run(&["profile", "use", "test"]).status.success());

    for (arguments, expected) in [
        (vec!["profile", "show"], "test"),
        (vec!["--profile", "prod", "profile", "show"], "prod"),
        (vec!["--profile", "prod", "profile", "show", "test"], "test"),
    ] {
        let output = run(&arguments);
        assert!(output.status.success(), "{arguments:?}");
        assert_eq!(envelope(&output)["data"]["name"], expected);
    }
    let listed = envelope(&run(&["profile", "list"]));
    assert_eq!(listed["data"]["prod"]["access"], "read-only");
    assert_eq!(listed["data"]["test"]["access"], "read-write");
    assert_eq!(listed["meta"]["profile"], "test");

    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("current_profile = \"test\""));
    let output = run(&["--profile", "missing", "profile", "show"]);
    assert_eq!(output.status.code(), Some(3));
}

#[test]
fn raising_a_profile_to_read_write_needs_an_interactive_terminal() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    assert!(
        cli(&config)
            .args(["profile", "add", "prod"])
            .output()
            .unwrap()
            .status
            .success()
    );

    let output = cli(&config)
        .args([
            "--output",
            "json",
            "profile",
            "update",
            "prod",
            "--access",
            "read-write",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let error = error_envelope(&output);
    assert_eq!(error["error"]["code"], "forbidden");
    assert!(
        error["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("their own terminal")
    );
    assert!(
        std::fs::read_to_string(&config)
            .unwrap()
            .contains("read-only")
    );

    assert!(
        cli(&config)
            .args(["profile", "add", "test", "--access", "read-write"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let output = cli(&config)
        .args([
            "--output",
            "json",
            "profile",
            "update",
            "test",
            "--access",
            "read-only",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "lowering access needs no confirmation"
    );
    assert_eq!(envelope(&output)["data"]["changed"], true);
}

#[test]
fn profile_settings_are_stored_shown_and_tightened_without_a_terminal() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    let run = |arguments: &[&str]| {
        cli(&config)
            .args(["--output", "json"])
            .args(arguments)
            .output()
            .unwrap()
    };

    let output = run(&[
        "profile",
        "add",
        "prod",
        "--index-check",
        "on",
        "--max-documents-per-query",
        "20",
        "--max-bytes-per-query",
        "4096",
        "--query-count-max-time-ms",
        "500",
        "--aggregation-count-max-time-ms",
        "900",
        "--disabled-tools",
        "delete, drop-database",
        "--confirmation-required-tools",
        "insert-many",
    ]);
    assert!(output.status.success());
    let stored = json!({
        "name": "prod",
        "access": "read-only",
        "index_check": true,
        "max_documents_per_query": 20,
        "max_bytes_per_query": 4096,
        "query_count_max_time_ms": 500,
        "aggregation_count_max_time_ms": 900,
        "disabled_tools": ["delete", "drop-database"],
        "confirmation_required_tools": ["insert-many"],
    });
    assert_eq!(envelope(&output)["data"], stored);
    // `profile show` adds which credentials the keyring holds, as booleans.
    let mut shown = stored.clone();
    shown["credentials"] = json!({"connectionString": false, "atlas": false});
    assert_eq!(envelope(&run(&["profile", "show", "prod"]))["data"], shown);

    // Tightening needs no terminal; every changed field is reported.
    let output = run(&[
        "profile",
        "update",
        "prod",
        "--max-documents-per-query",
        "50",
        "--server-side-js",
        "deny",
        "--disabled-tools",
        "delete,drop-database,insert-many",
        "--index-check",
        "off",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let updated = envelope(&output)["data"].clone();
    assert_eq!(updated["changed"], true);
    assert_eq!(updated["max_documents_per_query"], 50);
    assert_eq!(updated["server_side_js"], "deny");
    assert_eq!(updated["index_check"], false);
    assert_eq!(
        updated["disabled_tools"],
        json!(["delete", "drop-database", "insert-many"])
    );
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("max_documents_per_query = 50"), "{text}");

    let output = run(&["profile", "update", "prod"]);
    assert_eq!(output.status.code(), Some(2), "nothing to change");
    assert_eq!(error_envelope(&output)["error"]["code"], "validation");

    // An empty value clears a list.
    let output = run(&["profile", "add", "empty", "--disabled-tools", ""]);
    assert_eq!(
        envelope(&output)["data"],
        json!({"name": "empty", "access": "read-only"})
    );
    let output = run(&[
        "profile",
        "update",
        "empty",
        "--confirmation-required-tools",
        "",
    ]);
    assert!(output.status.success());
    assert_eq!(envelope(&output)["data"]["changed"], false);
}

#[test]
fn loosening_profile_settings_needs_an_interactive_terminal() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    let output = cli(&config)
        .args([
            "profile",
            "add",
            "prod",
            "--disabled-tools",
            "delete,drop-database",
            "--confirmation-required-tools",
            "insert-many",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let before = std::fs::read_to_string(&config).unwrap();

    for (arguments, loosened) in [
        (
            vec!["--server-side-js", "allow"],
            "server-side JavaScript becomes allowed",
        ),
        (
            vec!["--disabled-tools", ""],
            "disabled tools lose delete, drop-database",
        ),
        (
            vec!["--disabled-tools", "delete"],
            "disabled tools lose drop-database",
        ),
        (
            vec!["--confirmation-required-tools", ""],
            "confirmation-required tools lose insert-many",
        ),
    ] {
        let output = cli(&config)
            .args(["--output", "json", "profile", "update", "prod"])
            .args(&arguments)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(3), "{arguments:?}");
        let error = error_envelope(&output);
        assert_eq!(error["error"]["code"], "forbidden");
        let message = error["error"]["message"].as_str().unwrap();
        assert!(message.contains(loosened), "{arguments:?}: {message}");
        assert!(
            error["error"]["hint"]
                .as_str()
                .unwrap()
                .contains("their own terminal")
        );
    }
    assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
}

#[test]
fn refusals_and_invalid_input_happen_before_connecting() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    let read_write = ("MONGODB_CLI_ACCESS", "read-write");
    for (environment, arguments, code, error_code) in [
        (
            vec![],
            vec!["documents", "insert", "app", "users", "--documents", "{}"],
            3,
            "forbidden",
        ),
        (
            vec![],
            vec![
                "documents",
                "aggregate",
                "app",
                "users",
                "--pipeline",
                r#"[{"$out": "x"}]"#,
                "--yes",
            ],
            3,
            "forbidden",
        ),
        (
            vec![read_write],
            vec!["documents", "delete", "app", "users", "--filter", "{}"],
            2,
            "validation",
        ),
        (
            vec![read_write],
            vec!["databases", "drop", "app"],
            2,
            "validation",
        ),
        (
            vec![],
            vec!["documents", "find", "app", "users", "--filter", "{nope"],
            2,
            "validation",
        ),
        (
            vec![("MONGODB_CLI_ACCESS", "bogus")],
            vec!["documents", "find", "app", "users"],
            3,
            "config",
        ),
        (
            vec![],
            vec![
                "documents",
                "find",
                "app",
                "users",
                "--filter",
                r#"{"$where": "true"}"#,
            ],
            3,
            "forbidden",
        ),
        (
            vec![read_write],
            vec![
                "documents",
                "update",
                "app",
                "users",
                "--filter",
                "{}",
                "--update",
                r#"[{"$set": {"a": {"$function": {"body": "f", "args": [], "lang": "js"}}}}]"#,
            ],
            3,
            "forbidden",
        ),
        (
            vec![("MONGODB_CLI_DISABLED_TOOLS", "find")],
            vec!["documents", "find", "app", "users"],
            3,
            "forbidden",
        ),
        (
            vec![("MONGODB_CLI_DISABLED_TOOLS", "metadata")],
            vec!["collections", "list", "app"],
            3,
            "forbidden",
        ),
        (
            vec![read_write, ("MONGODB_CLI_DISABLED_TOOLS", "delete")],
            vec![
                "collections",
                "rename",
                "app",
                "a",
                "b",
                "--drop-target",
                "--yes",
            ],
            3,
            "forbidden",
        ),
        (
            vec![
                read_write,
                ("MONGODB_CLI_CONFIRMATION_REQUIRED_TOOLS", "insert-many"),
            ],
            vec!["documents", "insert", "app", "users", "--documents", "{}"],
            2,
            "validation",
        ),
        (
            vec![],
            vec![
                "documents",
                "export",
                "app",
                "users",
                "--pipeline",
                r#"[{"$out": "x"}]"#,
            ],
            2,
            "validation",
        ),
        (
            vec![("MONGODB_CLI_ALLOW_SERVER_SIDE_JS", "maybe")],
            vec!["documents", "find", "app", "users"],
            3,
            "config",
        ),
    ] {
        let mut command = cli(&config);
        command.env("MONGODB_URI", UNREACHABLE_URI);
        command.envs(environment.iter().copied());
        let started = Instant::now();
        let output = command
            .args(["--output", "json"])
            .args(&arguments)
            .output()
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{arguments:?} tried to connect"
        );
        assert_eq!(output.status.code(), Some(code), "{arguments:?}");
        assert_eq!(
            error_envelope(&output)["error"]["code"],
            error_code,
            "{arguments:?}"
        );
    }
}

#[test]
fn server_side_javascript_refusal_names_the_operator_and_the_environment_can_allow_it() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    let find = [
        "--output",
        "json",
        "--timeout",
        "1",
        "documents",
        "find",
        "app",
        "users",
        "--filter",
        r#"{"$where": "true"}"#,
    ];
    let output = cli(&config)
        .env("MONGODB_URI", UNREACHABLE_URI)
        .args(find)
        .output()
        .unwrap();
    let error = error_envelope(&output);
    assert_eq!(
        error["error"]["message"],
        r#"server-side JavaScript is disabled: the "$where" operator is not allowed"#
    );
    assert!(error["error"]["hint"].as_str().unwrap().contains("$expr"));

    // Allowed, the query gets as far as the unreachable server.
    let output = cli(&config)
        .env("MONGODB_URI", UNREACHABLE_URI)
        .env("MONGODB_CLI_ALLOW_SERVER_SIDE_JS", "true")
        .args(find)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(error_envelope(&output)["error"]["code"], "timeout");
}

#[test]
fn missing_configuration_explains_setup_and_doctor_reports_on_stdout() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    cli(&config)
        .args(["documents", "count", "app", "users"])
        .assert()
        .code(3)
        .stdout("")
        .stderr(predicate::str::contains("mongodb-cli init"));

    let output = cli(&config)
        .args(["--output", "json", "doctor"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let report = envelope(&output);
    let statuses: Vec<_> = report["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| check["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, ["pass", "fail", "skipped", "skipped", "skipped"]);
}

#[test]
fn doctor_reports_an_unreachable_server_without_exposing_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    let output = cli(&config)
        .args(["--output", "json", "--timeout", "2", "doctor"])
        .env(
            "MONGODB_URI",
            "mongodb://reader:never-print-this@127.0.0.1:9/?serverSelectionTimeoutMS=500",
        )
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));
    let report = envelope(&output);
    assert_eq!(report["data"][3]["name"], "server");
    assert_eq!(report["data"][3]["status"], "fail");
    assert_eq!(report["data"][4]["status"], "skipped");
    assert_eq!(report["meta"]["access"], "read-only");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!all.contains("never-print-this"));
}

#[test]
fn config_show_and_path_never_include_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    std::fs::write(
        &config,
        "current_profile = \"prod\"\n[profiles.prod]\naccess = \"read-only\"\ntarget = \"mongodb://db.example.com\"\n",
    )
    .unwrap();
    let output = cli(&config)
        .args(["--output", "json", "config", "show"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        envelope(&output)["data"],
        json!({"current_profile": "prod", "atlas_tools": false, "profiles": {"prod": {"access": "read-only", "target": "mongodb://db.example.com"}}})
    );

    std::fs::write(&config, "broken = [").unwrap();
    let output = cli(&config)
        .args(["--output", "json", "config", "path"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "config path works with a malformed file"
    );
    assert_eq!(
        envelope(&output)["data"]["path"],
        config.display().to_string()
    );
}
