//! Atlas tools gating: the `atlas`, `local`, and `knowledge` command groups and the Atlas
//! credential flags of `auth login|logout` exist only while Atlas tools are enabled
//! (`config set atlas-tools enabled` or `MONGODB_CLI_ATLAS_TOOLS`).

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{Command, CommandFactory, FromArgMatches};

use crate::args::Cli;
use crate::config::{ConfigFile, atlas_tools_override};
use crate::dispatch::{AppError, config_path};

/// Command groups that are left out of help, completion, and parsing while Atlas tools are disabled.
pub const GATED_GROUPS: [&str; 3] = ["atlas", "local", "knowledge"];

/// Flags of `auth` subcommands that exist only while Atlas tools are enabled. While disabled, the
/// parser hides them but still accepts them, because Clap's derive reads every declared argument,
/// and dispatch refuses them with [`require_enabled`]; completion scripts leave them out.
const GATED_FLAGS: [(&str, &[&str]); 2] = [
    ("login", &["atlas", "credentials_stdin"]),
    ("logout", &["atlas"]),
];

/// Parse the process arguments against [`command`].
pub fn try_parse() -> Result<Cli, clap::Error> {
    let mut command = command();
    let mut matches = command.try_get_matches_from_mut(std::env::args_os())?;
    Cli::from_arg_matches_mut(&mut matches).map_err(|error| error.format(&mut command))
}

/// The command tree for this invocation. The setting is resolved before parsing, so an
/// unreadable config or an invalid override counts as disabled.
pub fn command() -> Command {
    let full = Cli::command();
    if enabled() {
        full
    } else {
        hide_gated_flags(without_gated_groups(&full))
    }
}

/// [`command`] for completion scripts. Their generators also list hidden arguments, so the gated
/// flags are removed instead of hidden.
pub fn completion_command() -> Command {
    let full = Cli::command();
    if enabled() {
        full
    } else {
        remove_gated_flags(without_gated_groups(&full))
    }
}

/// Whether Atlas tools are enabled for this invocation; an unreadable config or an invalid
/// override counts as disabled, as it does for parsing.
pub fn enabled() -> bool {
    resolve().unwrap_or(false)
}

/// The refusal for a usage error that names a disabled group, such as `mongodb-cli local …`.
pub fn disabled_group_error(error: &clap::Error) -> Option<AppError> {
    let Some(ContextValue::String(name)) = error.get(ContextKind::InvalidSubcommand) else {
        return None;
    };
    if error.kind() != ErrorKind::InvalidSubcommand || !GATED_GROUPS.contains(&name.as_str()) {
        return None;
    }
    Some(require_enabled().err().unwrap_or_else(disabled))
}

/// Refuse a gated flag, such as `auth login --atlas`, while Atlas tools are disabled. An invalid
/// override or an unreadable config is more useful than the generic refusal, so it is returned
/// instead.
pub fn require_enabled() -> Result<(), AppError> {
    match resolve() {
        Ok(true) => Ok(()),
        Ok(false) => Err(disabled()),
        Err(error) => Err(error),
    }
}

fn disabled() -> AppError {
    AppError::config("Atlas tools are disabled")
        .with_hint("enable them with `mongodb-cli config set atlas-tools enabled`")
}

fn resolve() -> Result<bool, AppError> {
    if let Some(enabled) = atlas_tools_override().map_err(AppError::config)? {
        return Ok(enabled);
    }
    Ok(ConfigFile::load(&config_path()?)
        .map_err(AppError::config)?
        .atlas_tools)
}

/// Clap cannot remove subcommands, so rebuild the derived root without the gated groups.
fn without_gated_groups(full: &Command) -> Command {
    Command::new("mongodb-cli")
        .version(env!("CARGO_PKG_VERSION"))
        .about(full.get_about().cloned().unwrap_or_default())
        .after_help(full.get_after_help().cloned().unwrap_or_default())
        .subcommand_required(true)
        .arg_required_else_help(true)
        .args(full.get_arguments().cloned())
        .subcommands(
            full.get_subcommands()
                .filter(|command| !GATED_GROUPS.contains(&command.get_name()))
                .cloned(),
        )
}

fn hide_gated_flags(root: Command) -> Command {
    root.mut_subcommand("auth", |auth| {
        GATED_FLAGS.into_iter().fold(auth, |auth, (name, flags)| {
            auth.mut_subcommand(name, |command| {
                flags.iter().fold(command, |command, flag| {
                    command.mut_arg(flag, |argument| argument.hide(true))
                })
            })
        })
    })
}

/// Clap cannot remove arguments either, so rebuild the commands of the gated flags without them;
/// completion needs only their names, descriptions, and remaining arguments.
fn remove_gated_flags(root: Command) -> Command {
    root.mut_subcommand("auth", |auth| {
        GATED_FLAGS.into_iter().fold(auth, |auth, (name, flags)| {
            auth.mut_subcommand(name, |command| {
                Command::new(name)
                    .about(command.get_about().cloned().unwrap_or_default())
                    .args(
                        command
                            .get_arguments()
                            .filter(|argument| !flags.contains(&argument.get_id().as_str()))
                            .cloned(),
                    )
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_disabled_tree_is_the_derived_tree_without_gated_groups() {
        let full = Cli::command();
        let mut hidden = full.clone().mut_subcommands(|command| {
            let gated = GATED_GROUPS.contains(&command.get_name());
            command.hide(gated)
        });
        let mut rebuilt = hide_gated_flags(without_gated_groups(&full));
        assert_eq!(
            rebuilt.render_long_help().to_string(),
            hidden.render_long_help().to_string()
        );
        assert_eq!(rebuilt.get_version(), full.get_version());
        for name in GATED_GROUPS {
            assert!(rebuilt.find_subcommand(name).is_none(), "{name}");
        }
        rebuilt.clone().debug_assert();

        let error = rebuilt
            .try_get_matches_from_mut(["mongodb-cli", "--output", "json", "local", "--help"])
            .unwrap_err();
        assert!(disabled_group_error(&error).is_some());
        let error = rebuilt
            .try_get_matches_from_mut(["mongodb-cli", "documents", "fnd"])
            .unwrap_err();
        assert!(disabled_group_error(&error).is_none());
        let matches = rebuilt
            .try_get_matches_from_mut(["mongodb-cli", "--profile", "p", "databases", "list"])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap();
        assert_eq!(cli.profile.as_deref(), Some("p"));
        // Hidden flags still parse, so that dispatch can refuse them.
        for arguments in [
            &[
                "mongodb-cli",
                "auth",
                "login",
                "--atlas",
                "--credentials-stdin",
            ][..],
            &["mongodb-cli", "auth", "logout", "--atlas"],
        ] {
            let matches = rebuilt.try_get_matches_from_mut(arguments).unwrap();
            Cli::from_arg_matches(&matches).unwrap();
        }
    }
}
