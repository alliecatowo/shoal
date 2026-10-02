//! Shell completion generation from one bounded CLI vocabulary.

use super::schema::{ActionSpec, COMMANDS, CommandSpec, OptionSpec, ROOT_OPTIONS};
use std::fmt::Write as _;

pub(super) fn generate(shell: &str) -> Result<String, String> {
    match shell {
        "bash" => Ok(bash()),
        "zsh" => Ok(zsh()),
        "fish" => Ok(fish()),
        _ => Err(format!(
            "unsupported shell `{shell}`; expected bash, zsh, or fish"
        )),
    }
}

fn option_words(options: &[OptionSpec]) -> String {
    options
        .iter()
        .flat_map(|option| option.short.into_iter().chain([option.long]))
        .collect::<Vec<_>>()
        .join(" ")
}

fn command_words(command: &CommandSpec) -> String {
    let options = option_words(command.options);
    command
        .words
        .iter()
        .copied()
        .chain(options.split_whitespace())
        .collect::<Vec<_>>()
        .join(" ")
}

fn action_words(action: &ActionSpec) -> String {
    option_words(action.options)
}

fn root_words() -> String {
    COMMANDS
        .iter()
        .map(|command| command.name)
        .chain(option_words(ROOT_OPTIONS).split_whitespace())
        .collect::<Vec<_>>()
        .join(" ")
}

fn bash() -> String {
    let mut script = format!(
        "_shoal() {{\n  local cur=\"${{COMP_WORDS[COMP_CWORD]}}\"\n  local words='{}'\n  case \"${{COMP_WORDS[1]}}\" in\n",
        root_words()
    );
    for command in COMMANDS {
        if command.actions.is_empty() {
            writeln!(
                script,
                "    {}) words='{}' ;;",
                command.name,
                command_words(command)
            )
            .unwrap();
        } else {
            writeln!(
                script,
                "    {}) case \"${{COMP_WORDS[2]}}\" in",
                command.name
            )
            .unwrap();
            for action in command.actions {
                writeln!(
                    script,
                    "      {}) words='{}' ;;",
                    action.name,
                    action_words(action)
                )
                .unwrap();
            }
            writeln!(script, "      *) words='{}' ;;", command_words(command)).unwrap();
            script.push_str("    esac ;;\n");
        }
    }
    script.push_str(
        "  esac\n  COMPREPLY=( $(compgen -W \"$words\" -- \"$cur\") )\n}\ncomplete -F _shoal shoal\n",
    );
    script
}

fn zsh() -> String {
    let mut script = format!(
        "#compdef shoal\n_arguments '1:command:({})' '*::argument:->args' {}\ncase $words[2] in\n",
        COMMANDS
            .iter()
            .map(|command| command.name)
            .collect::<Vec<_>>()
            .join(" "),
        ROOT_OPTIONS
            .iter()
            .map(zsh_option)
            .collect::<Vec<_>>()
            .join(" ")
    );
    for command in COMMANDS {
        if command.actions.is_empty() {
            writeln!(
                script,
                "  {}) _values 'argument' {} ;;",
                command.name,
                command_words(command)
            )
            .unwrap();
        } else {
            writeln!(script, "  {}) case $words[3] in", command.name).unwrap();
            for action in command.actions {
                writeln!(
                    script,
                    "    {}) _values 'option' {} ;;",
                    action.name,
                    action_words(action)
                )
                .unwrap();
            }
            writeln!(
                script,
                "    *) _values 'argument' {} ;;",
                command_words(command)
            )
            .unwrap();
            script.push_str("  esac ;;\n");
        }
    }
    script.push_str("esac\n");
    script
}

fn zsh_option(option: &OptionSpec) -> String {
    let suffix = if option.value.is_some() {
        ":value:"
    } else {
        ""
    };
    match option.short {
        Some(short) => format!("'{{{short},{}}}{suffix}'", option.long),
        None => format!("'{}{suffix}'", option.long),
    }
}

fn fish() -> String {
    let mut script = String::new();
    for option in ROOT_OPTIONS {
        fish_option(&mut script, option, None);
    }
    for command in COMMANDS {
        writeln!(
            script,
            "complete -c shoal -n '__fish_use_subcommand' -a '{}' -d '{}'",
            command.name, command.summary
        )
        .unwrap();
        for word in command.words {
            writeln!(
                script,
                "complete -c shoal -n '__fish_seen_subcommand_from {}' -a '{}'",
                command.name, word
            )
            .unwrap();
        }
        for option in command.options {
            fish_option(&mut script, option, Some(command.name));
        }
        for action in command.actions {
            for option in action.options {
                fish_action_option(&mut script, option, command.name, action.name);
            }
        }
    }
    script
}

fn fish_action_option(script: &mut String, option: &OptionSpec, command: &str, action: &str) {
    write!(
        script,
        "complete -c shoal -n '__fish_seen_subcommand_from {command}; and __fish_seen_subcommand_from {action}'"
    )
    .unwrap();
    if let Some(short) = option.short {
        write!(script, " -s {}", short.trim_start_matches('-')).unwrap();
    }
    write!(script, " -l {}", option.long.trim_start_matches("--")).unwrap();
    if option.value.is_some() {
        script.push_str(" -r");
    }
    script.push('\n');
}

fn fish_option(script: &mut String, option: &OptionSpec, command: Option<&str>) {
    write!(script, "complete -c shoal").unwrap();
    if let Some(command) = command {
        write!(script, " -n '__fish_seen_subcommand_from {command}'").unwrap();
    } else {
        write!(script, " -n '__fish_use_subcommand'").unwrap();
    }
    if let Some(short) = option.short {
        write!(script, " -s {}", short.trim_start_matches('-')).unwrap();
    }
    write!(script, " -l {}", option.long.trim_start_matches("--")).unwrap();
    if option.value.is_some() {
        script.push_str(" -r");
    }
    script.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    #[test]
    fn every_shell_is_generated_from_the_complete_schema() {
        for shell in ["bash", "zsh", "fish"] {
            let script = generate(shell).unwrap();
            for command in COMMANDS {
                assert!(
                    script.contains(command.name),
                    "{shell} omitted {}",
                    command.name
                );
                for word in command.words {
                    assert!(script.contains(word), "{shell} omitted {word}");
                }
                for option in command.options {
                    assert!(
                        contains_option(&script, shell, option),
                        "{shell} omitted {}",
                        option.long
                    );
                }
                for action in command.actions {
                    for option in action.options {
                        assert!(
                            contains_option(&script, shell, option),
                            "{shell} omitted {} {}",
                            action.name,
                            option.long
                        );
                    }
                }
            }
            for option in ROOT_OPTIONS {
                assert!(
                    contains_option(&script, shell, option),
                    "{shell} omitted {}",
                    option.long
                );
            }
        }
    }

    #[test]
    fn help_and_completion_share_the_same_root_schema() {
        for command in COMMANDS {
            assert!(super::super::schema::root_help().contains(command.name));
            assert!(command.usage.contains(&format!("shoal {}", command.name)));
        }
        for option in ROOT_OPTIONS {
            assert!(super::super::schema::root_help().contains(option.long));
        }
    }

    fn contains_option(script: &str, shell: &str, option: &OptionSpec) -> bool {
        if shell == "fish" {
            script.contains(&format!("-l {}", option.long.trim_start_matches("--")))
        } else {
            script.contains(option.long)
        }
    }

    #[test]
    fn installed_shells_accept_generated_syntax() {
        for shell in ["bash", "zsh", "fish"] {
            if Command::new(shell).arg("--version").output().is_err() {
                continue;
            }
            let mut child = Command::new(shell)
                .arg("-n")
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(generate(shell).unwrap().as_bytes())
                .unwrap();
            assert!(
                child.wait().unwrap().success(),
                "{shell} rejected its completion script"
            );
        }
    }
}
