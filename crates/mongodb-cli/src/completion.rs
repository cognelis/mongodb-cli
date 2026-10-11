use clap::{Command, ValueHint};
use clap_complete::Shell;

/// Generate from the parser's command tree without reading credentials; disabled Atlas tool
/// groups and flags are left out.
pub fn generate(shell: Shell) -> String {
    let mut command = crate::gate::completion_command();
    // Include Clap's generated help commands before applying compact menu metadata.
    command.build();
    let mut command = completion_hints(command);
    // Some generators panic on writer errors. Generate in memory so stdout errors
    // (including a closed pipe) remain handled by the CLI's fallible output layer.
    let mut script = Vec::new();
    clap_complete::generate(shell, &mut command, "mongodb-cli", &mut script);
    let script = String::from_utf8(script).expect("clap completion scripts contain UTF-8 text");
    if shell == Shell::Bash {
        // clap_complete 4.6.9 uses `__` for the root name in its state transitions,
        // but `__subcmd__` for the root's hyphen in the generated case arms.
        // Normalize just that root prefix; nested commands and their hyphens keep
        // the generator's encoding. Real Bash candidate tests guard this workaround.
        script.replace("mongodb__subcmd__cli__subcmd__", "mongodb__cli__subcmd__")
    } else {
        script
    }
}

fn completion_hints(command: Command) -> Command {
    let command = if command.get_name() == "help" {
        command.about("Show command help")
    } else {
        command
    };
    command
        .mut_args(|argument| {
            if argument.get_action().takes_values()
                && argument.get_value_hint() == ValueHint::Unknown
            {
                // Arbitrary database names, JSON filters, and profile names are not local paths.
                argument.value_hint(ValueHint::Other)
            } else {
                argument
            }
        })
        .mut_subcommands(completion_hints)
}
