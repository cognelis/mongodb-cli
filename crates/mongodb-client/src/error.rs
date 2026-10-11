use mongodb::error::{Error as DriverError, ErrorKind, WriteFailure};
use serde::Serialize;
use thiserror::Error;

use crate::uri::scrub;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Validation,
    Config,
    Unauthorized,
    Forbidden,
    NotFound,
    RequestRejected,
    Network,
    Timeout,
    Upstream,
    FeatureUnsupported,
    ResponseTooLarge,
    IncompatibleResponse,
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct ClientError {
    code: ErrorCode,
    message: String,
    server_code: Option<i32>,
    code_name: Option<String>,
    hint: Option<String>,
}

impl ClientError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            server_code: None,
            code_name: None,
            hint: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn code(&self) -> ErrorCode {
        self.code
    }

    /// The numeric MongoDB server error code, when the server rejected the operation.
    pub fn server_code(&self) -> Option<i32> {
        self.server_code
    }

    pub fn code_name(&self) -> Option<&str> {
        self.code_name.as_deref()
    }

    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    /// An HTTP API rejection that keeps the status and the API's own error code.
    pub fn api(
        code: ErrorCode,
        status: u16,
        code_name: Option<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            server_code: Some(i32::from(status)),
            code_name,
            hint: None,
        }
    }

    fn server(server_code: i32, code_name: &str, message: &str) -> Self {
        let code = classify(server_code);
        let error = Self {
            code,
            message: scrub(message),
            server_code: Some(server_code),
            code_name: (!code_name.is_empty()).then(|| code_name.to_owned()),
            hint: None,
        };
        match code {
            ErrorCode::Forbidden => {
                error.with_hint("the database user lacks the privilege for this operation")
            }
            ErrorCode::NotFound => {
                error.with_hint("check the database, collection, and index names")
            }
            ErrorCode::FeatureUnsupported => {
                error.with_hint("this MongoDB deployment does not support the requested feature")
            }
            _ => error,
        }
    }
}

/// Map MongoDB server error codes to the CLI's stable failure categories.
fn classify(server_code: i32) -> ErrorCode {
    match server_code {
        // Unauthorized and the Atlas proxy's "user is not allowed" error.
        13 | 8000 => ErrorCode::Forbidden,
        // UserNotFound and AuthenticationFailed.
        11 | 18 => ErrorCode::Unauthorized,
        // MaxTimeMSExpired and ExceededTimeLimit.
        50 | 262 => ErrorCode::Timeout,
        // CommandNotFound, CommandNotSupported, CommandNotSupportedOnView, NotImplemented,
        // SearchNotEnabled, and unrecognized aggregation stages.
        59 | 115 | 166 | 238 | 31082 | 40324 => ErrorCode::FeatureUnsupported,
        // NamespaceNotFound and IndexNotFound.
        26 | 27 => ErrorCode::NotFound,
        // Replica-set transitions, shutdown, and server-side network failures.
        6 | 7 | 89 | 91 | 189 | 9001 | 10107 | 11600 | 11602 | 13435 | 13436 => ErrorCode::Upstream,
        _ => ErrorCode::RequestRejected,
    }
}

impl From<DriverError> for ClientError {
    fn from(error: DriverError) -> Self {
        match error.kind.as_ref() {
            ErrorKind::InvalidArgument { message, .. } => {
                Self::new(ErrorCode::Validation, scrub(message))
            }
            ErrorKind::Authentication { message, .. } => Self::new(
                ErrorCode::Unauthorized,
                format!("MongoDB authentication failed: {}", scrub(message)),
            )
            .with_hint("check the connection string's user, password, and authSource"),
            ErrorKind::Command(command) => {
                Self::server(command.code, &command.code_name, &command.message)
            }
            ErrorKind::Write(WriteFailure::WriteError(write)) => Self::server(
                write.code,
                write.code_name.as_deref().unwrap_or_default(),
                &write.message,
            ),
            ErrorKind::Write(WriteFailure::WriteConcernError(concern)) => {
                let mut error = Self::server(concern.code, &concern.code_name, &concern.message);
                error.code = ErrorCode::Upstream;
                error.with_hint(
                    "the write may have been applied without satisfying its write concern",
                )
            }
            ErrorKind::InsertMany(insert) => {
                match insert
                    .write_errors
                    .as_ref()
                    .and_then(|errors| errors.first())
                {
                    Some(first) => {
                        let mut error = Self::server(
                            first.code,
                            first.code_name.as_deref().unwrap_or_default(),
                            &format!("document {} was rejected: {}", first.index, first.message),
                        );
                        error.hint = Some(
                            "documents before the rejected one were inserted; ordered inserts stop at the first error".into(),
                        );
                        error
                    }
                    None => Self::new(ErrorCode::Upstream, scrub(&error.to_string())),
                }
            }
            ErrorKind::ServerSelection { message, .. } => Self::new(
                ErrorCode::Network,
                format!("MongoDB server is unreachable: {}", scrub(message)),
            )
            .with_hint("check the hosts, network access list, replica set name, and TLS settings"),
            ErrorKind::Io(_)
            | ErrorKind::DnsResolve { .. }
            | ErrorKind::ConnectionPoolCleared { .. } => {
                Self::new(ErrorCode::Network, scrub(&error.to_string()))
            }
            ErrorKind::InvalidTlsConfig { message, .. } => Self::new(
                ErrorCode::Config,
                format!("invalid TLS configuration: {}", scrub(message)),
            ),
            ErrorKind::InvalidResponse { .. }
            | ErrorKind::BsonDeserialization(_)
            | ErrorKind::BsonSerialization(_) => {
                Self::new(ErrorCode::IncompatibleResponse, scrub(&error.to_string()))
            }
            ErrorKind::IncompatibleServer { message, .. } => {
                Self::new(ErrorCode::FeatureUnsupported, scrub(message))
            }
            _ => Self::new(ErrorCode::Upstream, scrub(&error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_codes_map_to_stable_categories() {
        for (code, expected) in [
            (13, ErrorCode::Forbidden),
            (18, ErrorCode::Unauthorized),
            (50, ErrorCode::Timeout),
            (26, ErrorCode::NotFound),
            (27, ErrorCode::NotFound),
            (31082, ErrorCode::FeatureUnsupported),
            (40324, ErrorCode::FeatureUnsupported),
            (10107, ErrorCode::Upstream),
            (11000, ErrorCode::RequestRejected),
            (2, ErrorCode::RequestRejected),
        ] {
            assert_eq!(classify(code), expected, "server code {code}");
        }
    }

    #[test]
    fn server_errors_keep_code_name_and_scrub_credentials() {
        let error = ClientError::server(
            26,
            "NamespaceNotFound",
            "ns not found for mongodb://admin:hunter2@db.example.com/app",
        );
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert_eq!(error.server_code(), Some(26));
        assert_eq!(error.code_name(), Some("NamespaceNotFound"));
        assert!(!error.to_string().contains("hunter2"));
        assert!(error.hint().is_some());
    }
}
