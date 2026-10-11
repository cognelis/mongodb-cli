//! MongoDB Atlas Administration API client: service-account OAuth, versioned requests, and the
//! error mapping shared by the Atlas commands. Behavior follows the MongoDB MCP server 3.0.5.

pub mod admin;
pub mod regions;
pub mod streams;

use std::error::Error as _;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use reqwest::{Method, StatusCode, header};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::{ClientError, ErrorCode};

/// The Admin API version of every request, as in the MongoDB MCP server.
pub const ACCEPT_V2: &str = "application/vnd.atlas.2025-03-12+json";
/// `clusters/tenantUpgrade` exists only in this older API version.
pub const ACCEPT_TENANT_UPGRADE: &str = "application/vnd.atlas.2023-01-01+json";
pub const DEFAULT_BASE_URL: &str = "https://cloud.mongodb.com/";
pub const HINT_UNAUTHORIZED: &str = "the service-account credentials may be invalid, expired, or lack permissions; see https://www.mongodb.com/docs/atlas/configure-api-access/";
const HINT_PAYMENT: &str = "this action needs an active payment method for the Atlas organization; see https://www.mongodb.com/docs/atlas/billing/";
const HINT_FORBIDDEN: &str = "the service account may lack the required Atlas role, or this IP address may not be on its API access list; see https://www.mongodb.com/docs/atlas/api/service-accounts-overview/";
const HINT_RATE_LIMITED: &str = "Atlas rate-limited the request; retry later";
const ACCEPT_JSON: &str = "application/json";
const FORM: &str = "application/x-www-form-urlencoded;charset=UTF-8";
/// Tokens are refreshed this long before Atlas expires them.
const TOKEN_MARGIN: Duration = Duration::from_secs(60);
const REVOKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Atlas service-account credentials. Debug output shows neither value.
#[derive(Debug)]
pub struct AtlasCredentials {
    pub client_id: SecretString,
    pub client_secret: SecretString,
}

pub struct AtlasClient {
    http: reqwest::Client,
    origin: String,
    insecure: bool,
    credentials: AtlasCredentials,
    token: Mutex<Option<(SecretString, Instant)>>,
}

impl AtlasClient {
    /// Configure a client; nothing is sent until the first request.
    pub fn new(
        base_url: &str,
        credentials: AtlasCredentials,
        timeout: Duration,
    ) -> Result<Self, ClientError> {
        let (origin, insecure) = origin(base_url)?;
        let http = reqwest::Client::builder()
            .user_agent(concat!("mongodb-cli/", env!("CARGO_PKG_VERSION")))
            .timeout(timeout)
            .build()
            .map_err(|error| {
                ClientError::new(
                    ErrorCode::Config,
                    format!("cannot configure the Atlas HTTP client: {error}"),
                )
            })?;
        Ok(Self {
            http,
            origin,
            insecure,
            credentials,
            token: Mutex::new(None),
        })
    }

    /// Whether the base URL sends credentials without TLS.
    pub fn insecure(&self) -> bool {
        self.insecure
    }

    /// Exchange the credentials for a token, proving that Atlas accepts them.
    pub async fn verify(&self) -> Result<(), ClientError> {
        self.bearer().await.map(drop)
    }

    /// Revoke the cached token. Failures are ignored because the token expires on its own.
    pub async fn revoke(&self) {
        let Some((token, _)) = self.token.lock().await.take() else {
            return;
        };
        let request = self
            .http
            .post(format!("{}/api/oauth/revoke", self.origin))
            .basic_auth(
                self.credentials.client_id.expose_secret(),
                Some(self.credentials.client_secret.expose_secret()),
            )
            .header(header::ACCEPT, ACCEPT_JSON)
            .header(header::CONTENT_TYPE, FORM)
            .body(format!("token={}", seg(token.expose_secret())))
            .send();
        let _ = tokio::time::timeout(REVOKE_TIMEOUT, request).await;
    }

    /// Call a path below `/api/atlas/v2`. An empty success body becomes `null`.
    pub async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
        accept: &str,
    ) -> Result<Value, ClientError> {
        let mut request = self
            .http
            .request(method, format!("{}/api/atlas/v2{path}", self.origin))
            .header(header::ACCEPT, accept)
            .bearer_auth(self.bearer().await?);
        if !query.is_empty() {
            request = request.query(query);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        read(request.send().await.map_err(transport)?).await
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, ClientError> {
        self.send(Method::GET, path, query, None, ACCEPT_V2).await
    }

    pub async fn post(&self, path: &str, body: Option<&Value>) -> Result<Value, ClientError> {
        self.send(Method::POST, path, &[], body, ACCEPT_V2).await
    }

    pub async fn patch(&self, path: &str, body: &Value) -> Result<Value, ClientError> {
        self.send(Method::PATCH, path, &[], Some(body), ACCEPT_V2)
            .await
    }

    pub async fn delete(&self, path: &str) -> Result<Value, ClientError> {
        self.send(Method::DELETE, path, &[], None, ACCEPT_V2).await
    }

    /// The caller's public IPv4 address as Atlas sees it.
    pub async fn current_ip(&self) -> Result<String, ClientError> {
        let response = self
            .http
            .get(format!("{}/api/private/ipinfo", self.origin))
            .header(header::ACCEPT, ACCEPT_JSON)
            .bearer_auth(self.bearer().await?)
            .send()
            .await
            .map_err(transport)?;
        read(response).await?["currentIpv4Address"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| incompatible("Atlas did not report the current IP address"))
    }

    async fn bearer(&self) -> Result<String, ClientError> {
        let mut slot = self.token.lock().await;
        if let Some((token, until)) = slot.as_ref()
            && Instant::now() < *until
        {
            return Ok(token.expose_secret().to_owned());
        }
        // The client secret is sent exactly as stored, without URL encoding, like the MCP server.
        let response = self
            .http
            .post(format!("{}/api/oauth/token", self.origin))
            .basic_auth(
                self.credentials.client_id.expose_secret(),
                Some(self.credentials.client_secret.expose_secret()),
            )
            .header(header::ACCEPT, ACCEPT_JSON)
            .header(header::CONTENT_TYPE, FORM)
            .body("grant_type=client_credentials")
            .send()
            .await
            .map_err(transport)?;
        let status = response.status();
        let text = response.text().await.map_err(transport)?;
        if !status.is_success() {
            return Err(token_error(status, &text));
        }
        let body: Value = serde_json::from_str(&text)
            .map_err(|_| incompatible("Atlas returned an unreadable token response"))?;
        let token = body["access_token"]
            .as_str()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| incompatible("Atlas returned no access token"))?
            .to_owned();
        let lifetime = Duration::from_secs(body["expires_in"].as_u64().unwrap_or(0));
        *slot = Some((
            SecretString::from(token.clone()),
            Instant::now() + lifetime.saturating_sub(TOKEN_MARGIN),
        ));
        Ok(token)
    }
}

/// Page size, page number, and whether Atlas should count every match.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Paging {
    pub limit: u32,
    pub page: u32,
    pub include_count: bool,
}

impl Paging {
    fn query(self) -> Vec<(&'static str, String)> {
        vec![
            ("itemsPerPage", self.limit.to_string()),
            ("pageNum", self.page.to_string()),
            ("includeCount", self.include_count.to_string()),
        ]
    }
}

/// The result of one Atlas command: its data, warnings, and list continuation details.
#[derive(Debug, Default)]
pub struct Outcome {
    pub data: Value,
    pub warnings: Vec<String>,
    pub limit: Option<u32>,
    pub page: Option<u32>,
    pub total: Option<u64>,
    pub has_more: Option<bool>,
}

impl Outcome {
    pub fn new(data: Value) -> Self {
        Self {
            data,
            ..Self::default()
        }
    }

    fn warn(mut self, warning: impl Into<String>) -> Self {
        self.warnings.push(warning.into());
        self
    }

    /// Describe one requested page. Atlas omits `totalCount` without `includeCount` and some
    /// environments report 0, so only a positive count is trusted, as in the MCP server.
    fn paged(data: Value, paging: Paging, returned: usize, total_count: Option<u64>) -> Self {
        let total = total_count.filter(|total| *total > 0);
        let returned = returned as u64;
        let has_more = match total {
            Some(total) => {
                u64::from(paging.page.saturating_sub(1)) * u64::from(paging.limit) + returned
                    < total
            }
            None => returned == u64::from(paging.limit),
        };
        Self {
            data,
            limit: Some(paging.limit),
            page: Some(paging.page),
            total,
            has_more: Some(has_more),
            ..Self::default()
        }
    }

    /// Flag a call that returns only Atlas's default first page when more items exist.
    fn truncated(mut self, label: &str, response: &Value) -> Self {
        let returned = results(response).len() as u64;
        if let Some(total) = response["totalCount"].as_u64()
            && total > returned
        {
            self.has_more = Some(true);
            self.warnings
                .push(format!("showing first {returned} of {total} {label}"));
        }
        self
    }
}

/// The `results` array of an Atlas list response.
fn results(response: &Value) -> &[Value] {
    response["results"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// Drop `null` members, as JSON serialization drops JavaScript's `undefined`.
fn compact(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.retain(|_, member| !member.is_null());
    }
    value
}

/// JavaScript truthiness, which the MCP server uses for its presence checks.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Percent-encode one path segment like JavaScript's `encodeURIComponent`.
pub fn seg(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// Normalize the base URL to its origin; every Atlas path is absolute.
fn origin(base_url: &str) -> Result<(String, bool), ClientError> {
    let config = |message: String| ClientError::new(ErrorCode::Config, message);
    let url = reqwest::Url::parse(base_url)
        .map_err(|error| config(format!("invalid Atlas base URL: {error}")))?;
    let insecure = match url.scheme() {
        "https" => false,
        "http" => true,
        other => {
            return Err(config(format!(
                "the Atlas base URL must use https, not {other}"
            )));
        }
    };
    if url.host_str().is_none_or(str::is_empty) {
        return Err(config("the Atlas base URL has no host".into()));
    }
    Ok((url.origin().ascii_serialization(), insecure))
}

async fn read(response: reqwest::Response) -> Result<Value, ClientError> {
    let status = response.status();
    let text = response.text().await.map_err(transport)?;
    if !status.is_success() {
        return Err(api_error(status, &text));
    }
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text)
        .map_err(|error| incompatible(format!("Atlas returned invalid JSON: {error}")))
}

/// Map an Admin API error envelope to the CLI's categories, keeping Atlas's `errorCode`.
fn api_error(status: StatusCode, body: &str) -> ClientError {
    let parsed: Option<Map<String, Value>> = serde_json::from_str(body).ok();
    let detail = match &parsed {
        Some(error) => {
            let reason = error
                .get("reason")
                .and_then(Value::as_str)
                .filter(|reason| !reason.is_empty())
                .unwrap_or("unknown error");
            match error
                .get("detail")
                .and_then(Value::as_str)
                .filter(|detail| !detail.is_empty())
            {
                Some(detail) => format!("{reason}; {detail}"),
                None => reason.to_owned(),
            }
        }
        None if body.trim().is_empty() => "unknown error".into(),
        None => body.trim().chars().take(500).collect(),
    };
    let message = format!(
        "[{} {}] error calling Atlas API: {}",
        status.as_u16(),
        status.canonical_reason().unwrap_or("Unknown"),
        detail.trim()
    );
    let code_name = parsed
        .as_ref()
        .and_then(|error| error.get("errorCode"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let (code, hint) = match status.as_u16() {
        401 => (ErrorCode::Unauthorized, Some(HINT_UNAUTHORIZED)),
        402 => (ErrorCode::Forbidden, Some(HINT_PAYMENT)),
        403 => (ErrorCode::Forbidden, Some(HINT_FORBIDDEN)),
        404 => (ErrorCode::NotFound, None),
        429 => (ErrorCode::Upstream, Some(HINT_RATE_LIMITED)),
        500.. => (ErrorCode::Upstream, None),
        _ => (ErrorCode::RequestRejected, None),
    };
    let error = ClientError::api(code, status.as_u16(), code_name, message);
    match hint {
        Some(hint) => error.with_hint(hint),
        None => error,
    }
}

/// A failed token request means the credentials were refused, unless Atlas itself is failing.
fn token_error(status: StatusCode, body: &str) -> ClientError {
    let parsed: Value = serde_json::from_str(body).unwrap_or_default();
    let detail: Vec<&str> = ["error", "error_description"]
        .iter()
        .filter_map(|key| parsed[key].as_str())
        .collect();
    let mut message = format!(
        "Atlas rejected the service-account token request ({} {})",
        status.as_u16(),
        status.canonical_reason().unwrap_or("Unknown")
    );
    if !detail.is_empty() {
        message.push_str(": ");
        message.push_str(&detail.join(": "));
    }
    if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
        ClientError::api(ErrorCode::Upstream, status.as_u16(), None, message)
            .with_hint("Atlas could not issue a token; retry later")
    } else {
        ClientError::api(ErrorCode::Unauthorized, status.as_u16(), None, message)
            .with_hint(HINT_UNAUTHORIZED)
    }
}

fn transport(error: reqwest::Error) -> ClientError {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let _ = write!(message, ": {cause}");
        source = cause.source();
    }
    if error.is_timeout() {
        ClientError::new(
            ErrorCode::Timeout,
            format!("Atlas did not respond: {message}"),
        )
        .with_hint("raise --timeout; a timed-out change may still complete in Atlas")
    } else if error.is_decode() {
        incompatible(format!("Atlas returned an unreadable response: {message}"))
    } else {
        ClientError::new(
            ErrorCode::Network,
            format!("Atlas is unreachable: {message}"),
        )
        .with_hint(
            "check network access to MongoDB Atlas, proxy settings, and MONGODB_CLI_ATLAS_BASE_URL",
        )
    }
}

fn incompatible(message: impl Into<String>) -> ClientError {
    ClientError::new(ErrorCode::IncompatibleResponse, message)
}

fn validation(message: impl Into<String>) -> ClientError {
    ClientError::new(ErrorCode::Validation, message)
}

fn rejected(message: impl Into<String>) -> ClientError {
    ClientError::new(ErrorCode::RequestRejected, message)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn base_urls_normalize_to_their_origin() {
        for (input, expected, insecure) in [
            (
                "https://cloud.mongodb.com/",
                "https://cloud.mongodb.com",
                false,
            ),
            (
                "https://cloud.mongodb.com",
                "https://cloud.mongodb.com",
                false,
            ),
            (
                "https://cloud.mongodbgov.com/api/",
                "https://cloud.mongodbgov.com",
                false,
            ),
            ("http://127.0.0.1:8080/", "http://127.0.0.1:8080", true),
        ] {
            assert_eq!(
                origin(input).unwrap(),
                (expected.to_owned(), insecure),
                "{input}"
            );
        }
        for input in ["ftp://cloud.mongodb.com", "not a url", "file:///tmp"] {
            assert_eq!(
                origin(input).unwrap_err().code(),
                ErrorCode::Config,
                "{input}"
            );
        }
    }

    #[test]
    fn api_errors_keep_status_atlas_code_and_mcp_message_format() {
        let body = r#"{"detail":"No cluster named c9 exists in group 65f1.","error":404,"errorCode":"CLUSTER_NOT_FOUND","parameters":[],"reason":"Not Found"}"#;
        let error = api_error(StatusCode::NOT_FOUND, body);
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert_eq!(error.server_code(), Some(404));
        assert_eq!(error.code_name(), Some("CLUSTER_NOT_FOUND"));
        assert_eq!(
            error.to_string(),
            "[404 Not Found] error calling Atlas API: Not Found; No cluster named c9 exists in group 65f1."
        );
        for (status, code, hinted) in [
            (400, ErrorCode::RequestRejected, false),
            (401, ErrorCode::Unauthorized, true),
            (402, ErrorCode::Forbidden, true),
            (403, ErrorCode::Forbidden, true),
            (409, ErrorCode::RequestRejected, false),
            (429, ErrorCode::Upstream, true),
            (503, ErrorCode::Upstream, false),
        ] {
            let error = api_error(StatusCode::from_u16(status).unwrap(), "{}");
            assert_eq!(error.code(), code, "{status}");
            assert_eq!(error.hint().is_some(), hinted, "{status}");
            assert!(
                error
                    .to_string()
                    .ends_with("error calling Atlas API: unknown error")
            );
        }
        let error = api_error(StatusCode::BAD_GATEWAY, " <html>proxy</html> ");
        assert!(error.to_string().ends_with(": <html>proxy</html>"));
        assert_eq!(error.code_name(), None);
    }

    #[test]
    fn token_failures_are_authentication_errors_unless_atlas_fails() {
        let error = token_error(
            StatusCode::UNAUTHORIZED,
            r#"{"error":"invalid_client","error_description":"bad secret"}"#,
        );
        assert_eq!(error.code(), ErrorCode::Unauthorized);
        assert!(error.to_string().ends_with("invalid_client: bad secret"));
        assert_eq!(error.hint(), Some(HINT_UNAUTHORIZED));
        assert_eq!(
            token_error(StatusCode::SERVICE_UNAVAILABLE, "").code(),
            ErrorCode::Upstream
        );
    }

    #[test]
    fn path_segments_are_encoded_like_encode_uri_component() {
        assert_eq!(seg("c0-x_y.z"), "c0-x_y.z");
        assert_eq!(seg("a b/c?d"), "a%20b%2Fc%3Fd");
        assert_eq!(seg("ü"), "%C3%BC");
    }

    #[test]
    fn pages_trust_only_positive_totals() {
        let paging = |page| Paging {
            limit: 2,
            page,
            include_count: false,
        };
        for (page, returned, total, has_more, trusted) in [
            (1, 2, None, true, None),
            (1, 1, None, false, None),
            (1, 2, Some(0), true, None),
            (1, 2, Some(3), true, Some(3)),
            (2, 1, Some(3), false, Some(3)),
        ] {
            let outcome = Outcome::paged(Value::Null, paging(page), returned, total);
            assert_eq!(
                outcome.has_more,
                Some(has_more),
                "{page} {returned} {total:?}"
            );
            assert_eq!(outcome.total, trusted);
            assert_eq!(outcome.page, Some(page));
        }
        let outcome = Outcome::new(Value::Null)
            .truncated("clusters", &json!({"results": [{}], "totalCount": 3}));
        assert_eq!(outcome.has_more, Some(true));
        assert_eq!(outcome.warnings, ["showing first 1 of 3 clusters"]);
    }

    #[test]
    fn truthiness_and_compaction_follow_javascript() {
        for (value, expected) in [
            (json!(null), false),
            (json!(""), false),
            (json!(0), false),
            (json!(false), false),
            (json!("x"), true),
            (json!([]), true),
            (json!({}), true),
        ] {
            assert_eq!(truthy(&value), expected, "{value}");
        }
        assert_eq!(compact(json!({"a": 1, "b": null})), json!({"a": 1}));
    }
}
