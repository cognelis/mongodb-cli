//! Informational doctor checks: never affect connection health or print credentials.
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

const APP: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const PREFIX: &str = "MONGODB_CLI";
const MAX_BYTES: usize = 64 * 1024;

fn check(name: &str, status: &str, message: String) -> Value {
    json!({"name": name, "status": status, "message": message})
}

pub(crate) async fn checks(skip_update: bool, timeout: Duration) -> Vec<Value> {
    let endpoint = std::env::var(format!("{PREFIX}_RELEASE_API"))
        .unwrap_or_else(|_| format!("https://api.github.com/repos/cognelis/{APP}/releases/latest"));
    let package = if skip_update {
        check("package", "skipped", "disabled by --no-update-check".into())
    } else {
        package_check(&endpoint, timeout.min(Duration::from_secs(5))).await
    };
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|base| base.home_dir().to_path_buf()));
    let cwd = std::env::current_dir().ok();
    let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let mut roots = Vec::new();
    for base in [home.as_deref(), cwd.as_deref()].into_iter().flatten() {
        roots.extend([
            base.join(".agents/skills"),
            base.join(".codex/skills"),
            base.join(".claude/skills"),
        ]);
    }
    if let Some(claude) = claude {
        roots.push(claude.join("skills"));
    }
    roots.sort();
    roots.dedup();
    let loaded = std::env::var(format!("{PREFIX}_SKILL")).unwrap_or_default();
    vec![
        package,
        skill_check(&roots, &loaded, &crate::skill_install::bundled_skills()),
    ]
}

fn version(value: &str) -> Option<[u64; 3]> {
    let value = value.strip_prefix('v').unwrap_or(value);
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut result = [0; 3];
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty()
            || !part.bytes().all(|c| c.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
        {
            return None;
        }
        result[index] = part.parse().ok()?;
    }
    Some(result)
}

async fn package_check(endpoint: &str, timeout: Duration) -> Value {
    let unavailable = || {
        check(
            "package",
            "skipped",
            format!(
                "unavailable: installed {VERSION}; release lookup failed; retry later or use --no-update-check"
            ),
        )
    };
    let Ok(url) = reqwest::Url::parse(endpoint) else {
        return unavailable();
    };
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return unavailable();
    }
    let Ok(client) = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(format!("{APP}/{VERSION}"))
        .build()
    else {
        return unavailable();
    };
    let request = async {
        let mut response = client
            .get(url)
            .header("accept", "application/vnd.github+json")
            .send()
            .await
            .ok()?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|n| n > MAX_BYTES as u64)
        {
            return None;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if bytes.len() + chunk.len() > MAX_BYTES {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice::<Value>(&bytes).ok()
    };
    match tokio::time::timeout(timeout, request).await {
        Ok(Some(body)) => release_check(VERSION, &body),
        _ => unavailable(),
    }
}

fn release_check(current: &str, body: &Value) -> Value {
    let latest = body["tag_name"].as_str().unwrap_or_default();
    let (Some(current_number), Some(latest_number)) = (version(current), version(latest)) else {
        return check(
            "package",
            "skipped",
            format!("unknown: installed {current}; release response has no valid stable version"),
        );
    };
    if body["draft"] == true || body["prerelease"] == true {
        return check(
            "package",
            "skipped",
            "unknown: release response is not a stable release".into(),
        );
    }
    let state = match current_number.cmp(&latest_number) {
        std::cmp::Ordering::Less => "update_available",
        std::cmp::Ordering::Equal => "current",
        std::cmp::Ordering::Greater => "ahead",
    };
    let mut message = format!(
        "{state}: installed {current}; latest {}",
        latest.trim_start_matches('v')
    );
    if state == "update_available" {
        message.push_str(&format!("; upgrade {APP} (npm: npm install -g @cognelis/{APP}@latest), inspect `{APP} skills status`, review changes, run `{APP} skills install`, then reload the agent context"));
    }
    check("package", "pass", message)
}

pub(crate) fn skill_version(text: &str) -> Option<&str> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let mut found = None;
    for line in lines {
        if line.trim() == "---" {
            return found;
        }
        if let Some(value) = line.strip_prefix("version:") {
            if found.is_some() {
                return None;
            }
            let value = value.trim();
            let value = match value.chars().next() {
                Some(quote @ ('\'' | '"')) => value.strip_prefix(quote)?.strip_suffix(quote)?,
                _ => value,
            };
            version(value)?;
            found = Some(value.trim_start_matches('v'));
        }
    }
    None
}

fn read_skill(path: &Path) -> Result<Option<String>, ()> {
    match fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Ok(meta) if meta.is_file() && meta.len() <= MAX_BYTES as u64 => {
            fs::read_to_string(path).map(Some).map_err(|_| ())
        }
        _ => Err(()),
    }
}

fn skill_check(roots: &[PathBuf], loaded: &str, bundled: &[(&str, &str)]) -> Value {
    if bundled.is_empty()
        || bundled
            .iter()
            .any(|(_, text)| skill_version(text) != Some(VERSION))
    {
        return check(
            "skill",
            "skipped",
            format!(
                "embedded_mismatch: bundled Skills do not match CLI {VERSION}; reinstall {APP}"
            ),
        );
    }
    let loaded = loaded.trim();
    let loaded_version = if version(loaded).is_some() {
        loaded.trim_start_matches('v')
    } else if loaded.is_empty() {
        "none"
    } else {
        "unknown"
    };
    let mut installed = 0;
    let mut current = 0;
    let mut complete = false;
    let mut issues = Vec::new();
    for root in roots {
        let mut aligned = 0;
        for (name, _) in bundled {
            let path = root.join(name).join("SKILL.md");
            match read_skill(&path) {
                Ok(Some(text)) => {
                    installed += 1;
                    let actual = skill_version(&text).unwrap_or("unknown");
                    if actual == VERSION {
                        current += 1;
                        aligned += 1;
                    } else {
                        issues.push(format!("{}: {actual}", path.display()));
                    }
                }
                Ok(None) => {}
                Err(()) => issues.push(format!("{}: unreadable", path.display())),
            }
        }
        complete |= aligned == bundled.len();
    }
    let state = if loaded_version == VERSION {
        "current"
    } else if !loaded.is_empty() && complete {
        "reload_required"
    } else if !loaded.is_empty() {
        if loaded_version == "unknown" {
            "unknown"
        } else {
            "outdated"
        }
    } else if complete {
        "installed_not_loaded"
    } else if installed > 0 {
        "outdated"
    } else if !issues.is_empty() {
        "invalid"
    } else {
        "not_installed"
    };
    let mut message = format!(
        "{state}: bundled {VERSION} ({} Skills); loaded {loaded_version}; installed {installed}, current {current}",
        bundled.len()
    );
    if !issues.is_empty() {
        message.push_str(&format!("; {}", issues.join("; ")));
    }
    if !complete || !issues.is_empty() {
        message.push_str(&format!("; inspect `{APP} skills status`, then run `{APP} skills install` for an intended refresh"));
    }
    if loaded_version != VERSION {
        message.push_str("; reload the agent context (or load the Skill if not yet loaded)");
    }
    check("skill", "pass", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn release_versions_are_compared_numerically_and_only_provide_advice() {
        for (current, tag, expected) in [
            ("1.9.0", "v1.10.0", "update_available"),
            ("2.0.0", "v2.0.0", "current"),
            ("3.0.0", "v2.0.0", "ahead"),
        ] {
            let check = release_check(current, &json!({"tag_name": tag}));
            assert_eq!(check["status"], "pass");
            assert!(check["message"].as_str().unwrap().starts_with(expected));
        }
        for tag in ["", "not-a-version", "1.2.3-beta", "01.2.3"] {
            assert_eq!(
                release_check("1.2.3", &json!({"tag_name": tag}))["status"],
                "skipped"
            );
        }
    }
    #[test]
    fn skills_distinguish_missing_old_unknown_installed_and_loaded_versions() {
        let temp = tempfile::tempdir().unwrap();
        let roots = [temp.path().to_path_buf()];
        let text = format!("---\nversion: {VERSION}\n---\n");
        let bundle = [(APP, text.as_str())];
        let state = |loaded: &str| {
            skill_check(&roots, loaded, &bundle)["message"]
                .as_str()
                .unwrap()
                .split(':')
                .next()
                .unwrap()
                .to_owned()
        };
        assert_eq!(state(""), "not_installed");
        let dir = temp.path().join(APP);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "---\nname: old\n---\n").unwrap();
        assert_eq!(state(""), "outdated");
        fs::write(dir.join("SKILL.md"), &text).unwrap();
        assert_eq!(state(""), "installed_not_loaded");
        assert_eq!(state("0.0.0"), "reload_required");
        assert_eq!(state(VERSION), "current");
        fs::remove_file(dir.join("SKILL.md")).unwrap();
        fs::create_dir(dir.join("SKILL.md")).unwrap();
        assert_eq!(state(""), "invalid");
        assert!(skill_version("version: 1.2.3").is_none());
        assert!(skill_version("---\nversion: \"1.2.3\n---").is_none());
        assert!(skill_version("---\nversion: 1.2.3\nversion: 2.0.0\n---").is_none());
    }
    #[tokio::test]
    async fn slow_and_oversized_release_responses_are_skipped() {
        use std::io::{Read, Write};
        for slow in [true, false] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let worker = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 4096];
                let _ = socket.read(&mut request);
                if slow {
                    let body = r#"{"tag_name":"v99.0.0"}"#;
                    let _ = write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    std::thread::sleep(Duration::from_millis(200));
                    let _ = socket.write_all(body.as_bytes());
                } else {
                    let _ = write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: 65537\r\nConnection: close\r\n\r\n"
                    );
                }
            });
            let timeout = if slow {
                Duration::from_millis(50)
            } else {
                Duration::from_secs(2)
            };
            let result = package_check(&endpoint, timeout).await;
            worker.join().unwrap();
            assert_eq!(result["status"], "skipped");
        }
    }

    #[tokio::test]
    async fn package_lookup_is_bounded_and_sends_no_credentials() {
        use std::io::{Read, Write};
        for (status, body, expected) in [
            ("200 OK", "{\"tag_name\":\"v99.0.0\"}", "pass"),
            ("503 Down", "unavailable", "skipped"),
            ("200 OK", "broken", "skipped"),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let worker = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 4096];
                let count = socket.read(&mut request).unwrap();
                assert!(
                    !String::from_utf8_lossy(&request[..count])
                        .to_ascii_lowercase()
                        .contains("authorization:")
                );
                write!(
                    socket,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            });
            let result = package_check(&endpoint, Duration::from_secs(2)).await;
            worker.join().unwrap();
            assert_eq!(result["status"], expected);
        }
    }
}
