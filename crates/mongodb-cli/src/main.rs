use std::io::{self, Write};

use clap::error::ErrorKind;
use mongodb_cli::args::OutputFormat;
use mongodb_cli::dispatch::{self, AppError};
use mongodb_cli::gate;
use mongodb_cli::output::{emit_error, emit_success, exit_code, json_mode};
use mongodb_client::ErrorCode;

#[tokio::main]
async fn main() {
    let cli = match gate::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return;
        }
        Err(error) => {
            let arguments: Vec<_> = std::env::args().collect();
            let format = requested_output_format(&arguments);
            let raw = arguments.iter().any(|argument| argument == "--raw");
            let failure = match gate::disabled_group_error(&error) {
                Some(failure) => failure,
                // Clap's rendering keeps multi-line usage errors and the help shown for a
                // missing subcommand readable; the single-line text writer would escape them.
                None if !json_mode(format, raw) => {
                    let _ = error.print();
                    std::process::exit(exit_code(ErrorCode::Validation));
                }
                None => AppError::validation(error.to_string())
                    .with_hint("Run `mongodb-cli --help` or the subcommand's `--help`"),
            };
            emit_error(format, raw, &failure);
            std::process::exit(exit_code(failure.code));
        }
    };
    match dispatch::run(&cli).await {
        Ok(result) => {
            let failure_code = result.failure_code;
            if cli.verbose {
                let _ = writeln!(
                    io::stderr().lock(),
                    "mongodb-cli: command={} elapsedMs={}",
                    result.command,
                    result.meta.elapsed_ms.unwrap_or_default()
                );
            }
            // Streaming commands such as `documents export --out -` already wrote stdout.
            if !result.emitted
                && let Err(error) = emit_success(
                    cli.output,
                    cli.raw,
                    &result.command,
                    result.data,
                    result.meta,
                )
                && error.kind() != io::ErrorKind::BrokenPipe
            {
                emit_error(
                    cli.output,
                    cli.raw,
                    &AppError::new(
                        ErrorCode::IncompatibleResponse,
                        format!("failed to write output: {error}"),
                    ),
                );
                std::process::exit(7);
            }
            if let Some(code) = failure_code {
                std::process::exit(exit_code(code));
            }
        }
        Err(error) => {
            emit_error(cli.output, cli.raw, &error);
            std::process::exit(exit_code(error.code));
        }
    }
}

fn requested_output_format(arguments: &[String]) -> OutputFormat {
    for (index, argument) in arguments.iter().enumerate() {
        let value = argument.strip_prefix("--output=").or_else(|| {
            (argument == "--output")
                .then(|| arguments.get(index + 1))
                .flatten()
                .map(String::as_str)
        });
        match value {
            Some("json") => return OutputFormat::Json,
            Some("table") => return OutputFormat::Table,
            Some("auto") => return OutputFormat::Auto,
            _ => {}
        }
    }
    OutputFormat::Auto
}
