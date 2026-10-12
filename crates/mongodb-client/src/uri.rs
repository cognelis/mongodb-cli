use std::fmt;

use mongodb::options::ConnectionString;
use secrecy::{ExposeSecret, SecretString};

/// Query options that can carry secrets and are therefore never displayed.
const SENSITIVE_OPTIONS: &[&str] = &[
    "authMechanismProperties",
    "tlsCertificateKeyFilePassword",
    "sslPEMKeyPassword",
    "password",
];

/// A validated connection string. Its `Debug` output and display form never contain credentials.
#[derive(Clone)]
pub struct ConnectionUri {
    secret: SecretString,
    display: String,
}

impl ConnectionUri {
    pub fn parse(value: &str) -> Result<Self, String> {
        if !(value.starts_with("mongodb://") || value.starts_with("mongodb+srv://")) {
            return Err("connection strings must start with mongodb:// or mongodb+srv://".into());
        }
        if value.chars().any(char::is_whitespace) {
            return Err("connection strings must not contain whitespace".into());
        }
        ConnectionString::parse(value)
            .map_err(|error| format!("invalid connection string: {}", scrub(&error.to_string())))?;
        Ok(Self {
            secret: SecretString::from(value.to_owned()),
            display: redact(value),
        })
    }

    /// The connection target without credentials or secret-bearing options.
    pub fn display(&self) -> &str {
        &self.display
    }

    pub(crate) fn expose(&self) -> &str {
        self.secret.expose_secret()
    }
}

impl fmt::Debug for ConnectionUri {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ConnectionUri")
            .field(&self.display)
            .finish()
    }
}

/// The database a connection string names in its path, if any; parsing needs no network.
pub fn default_database(uri: &str) -> Option<String> {
    ConnectionString::parse(uri).ok()?.default_database
}

/// Remove user information and secret-bearing options from a connection string.
pub fn redact(uri: &str) -> String {
    let Some((scheme, rest)) = uri.split_once("://") else {
        return "(invalid connection string)".into();
    };
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let hosts = authority
        .rsplit_once('@')
        .map_or(authority, |(_, hosts)| hosts);
    let tail = match tail.split_once('?') {
        Some((path, query)) => {
            let kept: Vec<_> = query
                .split('&')
                .filter(|pair| {
                    let key = pair.split('=').next().unwrap_or_default();
                    !key.is_empty()
                        && !SENSITIVE_OPTIONS
                            .iter()
                            .any(|sensitive| key.eq_ignore_ascii_case(sensitive))
                })
                .collect();
            if kept.is_empty() {
                path.to_owned()
            } else {
                format!("{path}?{}", kept.join("&"))
            }
        }
        None => tail.to_owned(),
    };
    format!("{scheme}://{hosts}{tail}")
}

/// Replace credentials in any connection string embedded in free text, such as a driver error.
pub(crate) fn scrub(text: &str) -> String {
    const SCHEMES: [&str; 2] = ["mongodb+srv://", "mongodb://"];
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("mongodb") {
        let candidate = &rest[start..];
        let Some(scheme) = SCHEMES
            .iter()
            .find(|scheme| candidate.starts_with(**scheme))
        else {
            result.push_str(&rest[..start + "mongodb".len()]);
            rest = &rest[start + "mongodb".len()..];
            continue;
        };
        result.push_str(&rest[..start + scheme.len()]);
        let after_scheme = &rest[start + scheme.len()..];
        let authority_end = after_scheme
            .find(|character: char| {
                matches!(character, '/' | '?' | '"' | '\'') || character.is_whitespace()
            })
            .unwrap_or(after_scheme.len());
        let authority = &after_scheme[..authority_end];
        match authority.rsplit_once('@') {
            Some((_, hosts)) => {
                result.push_str("***@");
                result.push_str(hosts);
            }
            None => result.push_str(authority),
        }
        rest = &after_scheme[authority_end..];
    }
    result.push_str(rest);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_removes_credentials_and_secret_options() {
        let uri = ConnectionUri::parse(
            "mongodb://reader:p%40ss@db1.example.com:27017,db2.example.com/app?replicaSet=rs0&authMechanismProperties=AWS_SESSION_TOKEN:secret&authSource=admin",
        )
        .unwrap();
        assert_eq!(
            uri.display(),
            "mongodb://db1.example.com:27017,db2.example.com/app?replicaSet=rs0&authSource=admin"
        );
        let debug = format!("{uri:?}");
        assert!(!debug.contains("p%40ss"));
        assert!(!debug.contains("reader"));
        assert!(!debug.contains("AWS_SESSION_TOKEN"));
    }

    #[test]
    fn srv_and_credential_free_strings_are_preserved() {
        assert_eq!(
            redact("mongodb+srv://user:pw@cluster0.example.net/?retryWrites=true"),
            "mongodb+srv://cluster0.example.net/?retryWrites=true"
        );
        assert_eq!(
            redact("mongodb://localhost:27017"),
            "mongodb://localhost:27017"
        );
        assert_eq!(
            redact("mongodb://u:p@host/?authMechanismProperties=x"),
            "mongodb://host/"
        );
    }

    #[test]
    fn parse_rejects_other_schemes_whitespace_and_invalid_options() {
        assert!(ConnectionUri::parse("https://example.com").is_err());
        assert!(ConnectionUri::parse("mongodb://host/ db").is_err());
        let error =
            ConnectionUri::parse("mongodb://user:secret@host/?maxPoolSize=abc").unwrap_err();
        assert!(!error.contains("secret"), "{error}");
    }

    #[test]
    fn scrub_replaces_embedded_credentials_only() {
        assert_eq!(
            scrub("failed: mongodb://admin:hunter2@db.example.com:27017/app timed out"),
            "failed: mongodb://***@db.example.com:27017/app timed out"
        );
        assert_eq!(
            scrub("'mongodb+srv://a:b@c.example.net' and mongodb://plain:27017"),
            "'mongodb+srv://***@c.example.net' and mongodb://plain:27017"
        );
        assert_eq!(scrub("mongodb is fine"), "mongodb is fine");
    }
}
