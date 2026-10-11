use std::io::{self, IsTerminal, Write};

use mongodb_client::ErrorCode;
use serde::Serialize;
use serde_json::Value;

use crate::args::{Access, OutputFormat};
use crate::dispatch::AppError;

mod terminal;

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access: Option<Access>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u32>,
    /// Limits that shortened this result: maxDocumentsPerQuery, maxBytesPerQuery, responseBytesLimit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applied_limits: Vec<String>,
    /// What `$out`/`$merge` stages wrote.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_targets: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// The page of an Atlas list command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_page: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SuccessEnvelope {
    pub schema_version: &'static str,
    pub command: String,
    pub data: Value,
    pub meta: Meta,
}

impl SuccessEnvelope {
    pub fn new(command: impl Into<String>, data: Value, meta: Meta) -> Self {
        Self {
            schema_version: "1",
            command: command.into(),
            data,
            meta,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorEnvelope<'a> {
    schema_version: &'static str,
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorBody<'a> {
    code: ErrorCode,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    server_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'a str>,
}

pub fn emit_success(
    format: OutputFormat,
    raw: bool,
    command: &str,
    data: Value,
    meta: Meta,
) -> io::Result<()> {
    let effective = effective_format(format, io::stdout().is_terminal());
    let mut output = io::BufWriter::new(io::stdout().lock());
    let mut diagnostics = io::stderr().lock();
    write_success(
        &mut output,
        &mut diagnostics,
        effective,
        raw,
        command,
        data,
        meta,
    )?;
    output.flush()
}

fn write_success(
    output: &mut impl Write,
    diagnostics: &mut impl Write,
    format: OutputFormat,
    raw: bool,
    command: &str,
    data: Value,
    meta: Meta,
) -> io::Result<()> {
    if command == "completion" {
        let script = data.as_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "completion output must be a script",
            )
        })?;
        return output.write_all(script.as_bytes());
    }
    if raw {
        return write_json(output, &data);
    }
    match format {
        OutputFormat::Json | OutputFormat::Auto => {
            write_json(output, &SuccessEnvelope::new(command, data, meta))
        }
        OutputFormat::Table => {
            let shortened = terminal::write(
                output,
                command,
                &data,
                terminal::width(),
                meta.profile.as_deref(),
            )?;
            output.flush()?;
            write_table_meta(diagnostics, &meta)?;
            if shortened {
                writeln!(
                    diagnostics,
                    "hint: displayed values were shortened; use --output json for full values"
                )?;
            }
            Ok(())
        }
    }
}

fn write_json(output: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut *output, value)?;
    writeln!(output)
}

fn effective_format(format: OutputFormat, stdout_is_terminal: bool) -> OutputFormat {
    match format {
        OutputFormat::Auto if stdout_is_terminal => OutputFormat::Table,
        OutputFormat::Auto => OutputFormat::Json,
        explicit => explicit,
    }
}

fn write_table_meta(diagnostics: &mut impl Write, meta: &Meta) -> io::Result<()> {
    if let Some(profile) = &meta.profile {
        writeln!(diagnostics, "Profile: {}", terminal::safe_text(profile))?;
    }
    if let Some(access) = meta.access {
        writeln!(diagnostics, "Access: {}", access.as_str())?;
    }
    if let Some(total) = meta.total {
        writeln!(diagnostics, "Total: {total}")?;
    }
    if meta.has_more == Some(true) {
        writeln!(
            diagnostics,
            "Results: partial; more results may be available"
        )?;
        if let Some(offset) = meta.next_offset {
            writeln!(diagnostics, "hint: continue with --offset {offset}")?;
        }
        if let Some(page) = meta.next_page {
            writeln!(diagnostics, "hint: continue with --page {page}")?;
        }
    }
    for warning in &meta.warnings {
        writeln!(diagnostics, "warning: {}", terminal::safe_text(warning))?;
    }
    Ok(())
}

pub fn emit_error(format: OutputFormat, raw: bool, error: &AppError) {
    let mut diagnostics = io::stderr().lock();
    let json_mode = raw
        || matches!(
            effective_format(format, io::stdout().is_terminal()),
            OutputFormat::Json
        );
    let _ = write_error(&mut diagnostics, json_mode, error);
}

fn write_error(diagnostics: &mut impl Write, json_mode: bool, error: &AppError) -> io::Result<()> {
    if json_mode {
        write_json(
            diagnostics,
            &ErrorEnvelope {
                schema_version: "1",
                error: ErrorBody {
                    code: error.code,
                    message: &error.message,
                    server_code: error.server_code,
                    code_name: error.code_name.as_deref(),
                    hint: error.hint.as_deref(),
                },
            },
        )
    } else {
        writeln!(
            diagnostics,
            "error: {}",
            terminal::safe_text(&error.message)
        )?;
        if let Some(hint) = &error.hint {
            writeln!(diagnostics, "hint: {}", terminal::safe_text(hint))?;
        }
        Ok(())
    }
}

pub fn exit_code(code: ErrorCode) -> i32 {
    match code {
        ErrorCode::Validation => 2,
        ErrorCode::Config | ErrorCode::Unauthorized | ErrorCode::Forbidden => 3,
        ErrorCode::Network | ErrorCode::Timeout => 4,
        ErrorCode::NotFound | ErrorCode::RequestRejected => 5,
        ErrorCode::Upstream => 6,
        ErrorCode::FeatureUnsupported
        | ErrorCode::ResponseTooLarge
        | ErrorCode::IncompatibleResponse => 7,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn auto_format_depends_on_stdout_and_explicit_formats_always_win() {
        assert_eq!(
            effective_format(OutputFormat::Auto, true),
            OutputFormat::Table
        );
        assert_eq!(
            effective_format(OutputFormat::Auto, false),
            OutputFormat::Json
        );
        for terminal in [true, false] {
            assert_eq!(
                effective_format(OutputFormat::Json, terminal),
                OutputFormat::Json
            );
            assert_eq!(
                effective_format(OutputFormat::Table, terminal),
                OutputFormat::Table
            );
        }
    }

    fn partial_meta() -> Meta {
        Meta {
            profile: Some("prod".into()),
            access: Some(Access::ReadOnly),
            total: Some(42),
            has_more: Some(true),
            next_offset: Some(10),
            warnings: vec!["total count unavailable".into()],
            ..Meta::default()
        }
    }

    #[test]
    fn table_output_keeps_profile_access_completeness_and_warnings_on_stderr() {
        let mut output = Vec::new();
        let mut diagnostics = Vec::new();
        write_success(
            &mut output,
            &mut diagnostics,
            OutputFormat::Table,
            false,
            "documents.find",
            json!([{"name": "hello"}]),
            partial_meta(),
        )
        .unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "name\n-----\nhello\n");
        let diagnostics = String::from_utf8(diagnostics).unwrap();
        for expected in [
            "Profile: prod",
            "Access: read-only",
            "Total: 42",
            "Results: partial",
            "--offset 10",
            "warning: total count unavailable",
        ] {
            assert!(
                diagnostics.contains(expected),
                "missing {expected:?} in {diagnostics}"
            );
        }
    }

    #[test]
    fn json_keeps_its_envelope_and_raw_keeps_only_data_without_diagnostics() {
        for (format, raw) in [
            (OutputFormat::Json, false),
            (OutputFormat::Auto, false),
            (OutputFormat::Table, true),
        ] {
            let mut output = Vec::new();
            let mut diagnostics = Vec::new();
            let data = json!([{"name": "hello"}]);
            write_success(
                &mut output,
                &mut diagnostics,
                format,
                raw,
                "documents.find",
                data.clone(),
                partial_meta(),
            )
            .unwrap();
            let value: Value = serde_json::from_slice(&output).unwrap();
            if raw {
                assert_eq!(value, data);
            } else {
                assert_eq!(value["data"], data);
                assert_eq!(value["schemaVersion"], "1");
                assert_eq!(value["meta"]["profile"], "prod");
                assert_eq!(value["meta"]["access"], "read-only");
                assert_eq!(value["meta"]["hasMore"], true);
                assert_eq!(value["meta"]["nextOffset"], 10);
            }
            assert!(diagnostics.is_empty());
        }
    }

    #[test]
    fn json_errors_carry_server_details() {
        let error = AppError {
            code: ErrorCode::NotFound,
            message: "ns not found".into(),
            hint: Some("check the names".into()),
            server_code: Some(26),
            code_name: Some("NamespaceNotFound".into()),
        };
        let mut diagnostics = Vec::new();
        write_error(&mut diagnostics, true, &error).unwrap();
        let value: Value = serde_json::from_slice(&diagnostics).unwrap();
        assert_eq!(
            value,
            json!({"schemaVersion": "1", "error": {
                "code": "not_found",
                "message": "ns not found",
                "serverCode": 26,
                "codeName": "NamespaceNotFound",
                "hint": "check the names"
            }})
        );
        let mut text = Vec::new();
        write_error(&mut text, false, &error).unwrap();
        assert_eq!(
            String::from_utf8(text).unwrap(),
            "error: ns not found\nhint: check the names\n"
        );
    }

    #[test]
    fn exit_codes_are_stable() {
        assert_eq!(exit_code(ErrorCode::Validation), 2);
        assert_eq!(exit_code(ErrorCode::Forbidden), 3);
        assert_eq!(exit_code(ErrorCode::Timeout), 4);
        assert_eq!(exit_code(ErrorCode::NotFound), 5);
        assert_eq!(exit_code(ErrorCode::Upstream), 6);
        assert_eq!(exit_code(ErrorCode::FeatureUnsupported), 7);
    }

    #[test]
    fn serialization_and_tables_preserve_io_error_kinds() {
        struct FailedWriter(io::ErrorKind);
        impl Write for FailedWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(self.0.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        for kind in [io::ErrorKind::BrokenPipe, io::ErrorKind::PermissionDenied] {
            for (format, raw) in [
                (OutputFormat::Json, false),
                (OutputFormat::Table, false),
                (OutputFormat::Json, true),
            ] {
                let error = write_success(
                    &mut FailedWriter(kind),
                    &mut Vec::new(),
                    format,
                    raw,
                    "demo",
                    json!([{"a": 1}]),
                    Meta::default(),
                )
                .unwrap_err();
                assert_eq!(error.kind(), kind);
            }
        }
    }
}
