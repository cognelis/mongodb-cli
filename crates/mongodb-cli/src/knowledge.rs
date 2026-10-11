//! The MongoDB Assistant knowledge base, queried like the MongoDB MCP server's assistant tools.

use std::error::Error as _;
use std::time::{Duration, Instant};

use mongodb_client::ErrorCode;
use reqwest::{RequestBuilder, StatusCode, Url};
use serde_json::{Value, json};

use crate::args::KnowledgeCommand;
use crate::dispatch::{AppError, DispatchResult};
use crate::output::Meta;

/// The API root. Endpoints are resolved against it like links, so it must end with `/`.
pub const BASE_URL_ENV: &str = "MONGODB_CLI_KNOWLEDGE_BASE_URL";
const DEFAULT_BASE_URL: &str = "https://knowledge.mongodb.com/api/v1/";

pub async fn run(command: &KnowledgeCommand, timeout: u64) -> Result<DispatchResult, AppError> {
    // Every input is checked before the network is touched.
    let search = match command {
        KnowledgeCommand::Sources => None,
        KnowledgeCommand::Search {
            query,
            limit,
            sources,
        } => Some((search_body(query, *limit, sources)?, *limit)),
    };
    let base = std::env::var(BASE_URL_ENV).ok();
    let path = if search.is_some() {
        "content/search"
    } else {
        "content/sources"
    };
    let url = endpoint(base.as_deref(), path)?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout))
        .user_agent(concat!("mongodb-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| AppError::config(format!("cannot initialize the HTTP client: {error}")))?;
    let started = Instant::now();
    let (name, data, mut meta) = match search {
        None => {
            let sources = fetch(
                http.get(url),
                "Failed to list knowledge sources",
                "dataSources",
            )
            .await?;
            (
                "knowledge.sources",
                with_current_versions(sources),
                Meta::default(),
            )
        }
        Some((body, limit)) => {
            let results = fetch(
                http.post(url).json(&body),
                "Failed to search knowledge base",
                "results",
            )
            .await?;
            let meta = Meta {
                limit: Some(limit),
                ..Meta::default()
            };
            ("knowledge.search", results, meta)
        }
    };
    meta.total = Some(data.len() as u64);
    meta.elapsed_ms = Some(started.elapsed().as_millis());
    Ok(DispatchResult {
        command: name.into(),
        data: Value::Array(data),
        meta,
        failure_code: None,
        emitted: false,
    })
}

/// Send a request and return the named array of its JSON response.
async fn fetch(
    request: RequestBuilder,
    failure: &str,
    field: &str,
) -> Result<Vec<Value>, AppError> {
    // The service rejects requests without an origin header.
    let response = request
        .header("x-request-origin", "mongodb-cli")
        .send()
        .await
        .map_err(transport_error)?;
    let status = response.status();
    if !status.is_success() {
        return Err(status_error(status, failure));
    }
    let body = response.bytes().await.map_err(transport_error)?;
    let mut body: Value = serde_json::from_slice(&body).unwrap_or_default();
    match body.get_mut(field).map(Value::take) {
        Some(Value::Array(items)) => Ok(items),
        _ => Err(AppError::new(
            ErrorCode::IncompatibleResponse,
            format!("{failure}: the response is not JSON with a {field} array"),
        )),
    }
}

fn transport_error(error: reqwest::Error) -> AppError {
    let code = if error.is_timeout() {
        ErrorCode::Timeout
    } else {
        ErrorCode::Network
    };
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message = format!("{message}: {cause}");
        source = cause.source();
    }
    AppError::new(code, message)
}

fn status_error(status: StatusCode, failure: &str) -> AppError {
    let code = match status.as_u16() {
        401 | 403 => ErrorCode::Forbidden,
        404 => ErrorCode::NotFound,
        408 | 429 => ErrorCode::Upstream,
        400..=499 => ErrorCode::RequestRejected,
        _ => ErrorCode::Upstream,
    };
    let reason = status.canonical_reason().unwrap_or(status.as_str());
    let error = AppError::new(code, format!("{failure}: {reason}"));
    if code == ErrorCode::NotFound {
        error.with_hint(format!("check {BASE_URL_ENV}; it must end with /"))
    } else {
        error
    }
}

/// Resolve an endpoint like the MCP server (WHATWG URL rules): without a trailing `/`, the
/// base's last path segment is replaced.
fn endpoint(base: Option<&str>, path: &str) -> Result<Url, AppError> {
    let base = base
        .filter(|base| !base.is_empty())
        .unwrap_or(DEFAULT_BASE_URL);
    Url::parse(base)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .and_then(|url| url.join(path).ok())
        .ok_or_else(|| AppError::config(format!("{BASE_URL_ENV} must be an http or https URL")))
}

fn search_body(query: &str, limit: u32, sources: &[String]) -> Result<Value, AppError> {
    if query.trim().is_empty() {
        return Err(AppError::validation("the search query must not be empty"));
    }
    let mut body = json!({"query": query, "limit": limit});
    if !sources.is_empty() {
        body["dataSources"] = sources
            .iter()
            .map(|source| data_source(source))
            .collect::<Result<Value, _>>()?;
    }
    Ok(body)
}

/// `NAME[@VERSION]`, split at the first `@`; version labels may contain spaces and parentheses.
fn data_source(text: &str) -> Result<Value, AppError> {
    match text.split_once('@') {
        None if !text.is_empty() => Ok(json!({"name": text})),
        Some((name, version)) if !name.is_empty() && !version.is_empty() => {
            Ok(json!({"name": name, "versionLabel": version}))
        }
        _ => Err(AppError::validation(format!(
            "--source expects NAME or NAME@VERSION, not '{text}'"
        ))),
    }
}

/// Append `currentVersion`, the label of the first current version, as the MCP server does.
fn with_current_versions(mut sources: Vec<Value>) -> Vec<Value> {
    for source in &mut sources {
        let current = source["versions"]
            .as_array()
            .and_then(|versions| versions.iter().find(|version| version["isCurrent"] == true))
            .and_then(|version| version["label"].as_str())
            .map(str::to_owned);
        if let (Some(label), Some(source)) = (current, source.as_object_mut()) {
            source.insert("currentVersion".into(), label.into());
        }
    }
    sources
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_bodies_carry_the_limit_and_split_sources_at_the_first_at_sign() {
        assert_eq!(
            search_body("vector search", 5, &[]).unwrap(),
            json!({"query": "vector search", "limit": 5})
        );
        let sources = ["docs@v4.x (current)", "devcenter", "a@b@c"].map(String::from);
        assert_eq!(
            search_body("q", 2, &sources).unwrap().to_string(),
            json!({"query": "q", "limit": 2, "dataSources": [
                {"name": "docs", "versionLabel": "v4.x (current)"},
                {"name": "devcenter"},
                {"name": "a", "versionLabel": "b@c"}
            ]})
            .to_string()
        );
        for source in ["", "@8.0", "docs@"] {
            let error = search_body("q", 5, &[source.into()]).unwrap_err();
            assert_eq!(error.code, ErrorCode::Validation, "{source:?}");
        }
        assert_eq!(
            search_body("  ", 5, &[]).unwrap_err().code,
            ErrorCode::Validation
        );
    }

    #[test]
    fn sources_gain_the_first_current_version_label_last() {
        let sources = with_current_versions(vec![
            json!({"id": "docs", "versions": [
                {"label": "v7", "isCurrent": false},
                {"label": "v8", "isCurrent": true},
                {"label": "v9", "isCurrent": true}
            ]}),
            json!({"id": "devcenter", "versions": []}),
            json!({"id": "old", "versions": [{"label": "v1", "isCurrent": false}]}),
            json!({"id": "bare"}),
        ]);
        assert_eq!(sources[0]["currentVersion"], "v8");
        assert_eq!(
            sources[0].as_object().unwrap().keys().next_back().unwrap(),
            "currentVersion"
        );
        for source in &sources[1..] {
            assert!(source.get("currentVersion").is_none(), "{source}");
        }
    }

    #[test]
    fn endpoints_resolve_like_links_against_an_http_base() {
        for (base, expected) in [
            (None, "https://knowledge.mongodb.com/api/v1/content/search"),
            (
                Some(""),
                "https://knowledge.mongodb.com/api/v1/content/search",
            ),
            (
                Some("http://127.0.0.1:8080/api/v1/"),
                "http://127.0.0.1:8080/api/v1/content/search",
            ),
            (
                Some("http://127.0.0.1:8080/api/v1"),
                "http://127.0.0.1:8080/api/content/search",
            ),
        ] {
            assert_eq!(endpoint(base, "content/search").unwrap().as_str(), expected);
        }
        for base in ["ftp://example.com/", "not a url", "file:///tmp/"] {
            assert_eq!(
                endpoint(Some(base), "content/search").unwrap_err().code,
                ErrorCode::Config,
                "{base}"
            );
        }
    }

    #[test]
    fn http_statuses_map_to_stable_categories() {
        for (status, code) in [
            (400, ErrorCode::RequestRejected),
            (401, ErrorCode::Forbidden),
            (403, ErrorCode::Forbidden),
            (404, ErrorCode::NotFound),
            (408, ErrorCode::Upstream),
            (418, ErrorCode::RequestRejected),
            (429, ErrorCode::Upstream),
            (500, ErrorCode::Upstream),
            (503, ErrorCode::Upstream),
        ] {
            let error = status_error(
                StatusCode::from_u16(status).unwrap(),
                "Failed to search knowledge base",
            );
            assert_eq!(error.code, code, "{status}");
            assert_eq!(error.hint.is_some(), status == 404, "{status}");
        }
        assert_eq!(
            status_error(StatusCode::BAD_REQUEST, "Failed to search knowledge base").message,
            "Failed to search knowledge base: Bad Request"
        );
    }
}
