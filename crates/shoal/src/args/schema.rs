//! Canonical public schema for `shoal` help and shell completions.

use std::fmt::Write as _;

#[derive(Clone, Copy)]
pub(super) struct OptionSpec {
    pub short: Option<&'static str>,
    pub long: &'static str,
    pub value: Option<&'static str>,
    pub help: &'static str,
}

#[derive(Clone, Copy)]
pub(super) struct CommandSpec {
    pub name: &'static str,
    pub summary: &'static str,
    pub usage: &'static str,
    pub arguments: &'static str,
    pub words: &'static [&'static str],
    pub options: &'static [OptionSpec],
    pub output: &'static str,
    pub errors: &'static str,
    pub example: &'static str,
    pub exit_status: &'static str,
    pub actions: &'static [ActionSpec],
}

#[derive(Clone, Copy)]
pub(super) struct ActionSpec {
    pub name: &'static str,
    pub summary: &'static str,
    pub usage: &'static str,
    pub options: &'static [OptionSpec],
    pub output: &'static str,
    pub errors: &'static str,
    pub example: &'static str,
    pub exit_status: &'static str,
}

pub(super) const HELP: OptionSpec = OptionSpec {
    short: Some("-h"),
    long: "--help",
    value: None,
    help: "Print this help and exit",
};

pub(super) const ROOT_OPTIONS: &[OptionSpec] = &[
    OptionSpec {
        short: Some("-c"),
        long: "--command",
        value: Some("SOURCE"),
        help: "Evaluate SOURCE instead of a file",
    },
    OptionSpec {
        short: None,
        long: "--standalone",
        value: None,
        help: "Use local evaluation (skips the private kernel interactively)",
    },
    HELP,
    OptionSpec {
        short: Some("-V"),
        long: "--version",
        value: None,
        help: "Print the version and exit",
    },
];

pub(super) const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "kernel",
        summary: "Manage the resident kernel",
        usage: "shoal kernel <start|status|stop> [--json]",
        arguments: "start starts or attaches; status inspects; stop authenticates and shuts down.",
        words: &["start", "status", "stop"],
        options: &[
            OptionSpec {
                short: None,
                long: "--json",
                value: None,
                help: "Emit the kernel response as JSON",
            },
            HELP,
        ],
        output: "Human status text by default, or the complete response object with --json.",
        errors: "Fails if the kernel cannot start, connect, authenticate, or answer the request.",
        example: "shoal kernel status --json",
        exit_status: "0 on success; 1 when kernel management fails.",
        actions: &[
            ActionSpec {
                name: "start",
                summary: "Start the resident kernel or attach to the running instance",
                usage: "shoal kernel start [--json]",
                options: &[
                    OptionSpec {
                        short: None,
                        long: "--json",
                        value: None,
                        help: "Emit the kernel status response as JSON",
                    },
                    HELP,
                ],
                output: "The running kernel identity and status, in human or JSON form.",
                errors: "Fails if startup, connection, authentication, or the status request fails.",
                example: "shoal kernel start --json",
                exit_status: "0 when the kernel answers; 1 on startup or protocol failure.",
            },
            ActionSpec {
                name: "status",
                summary: "Inspect the resident kernel",
                usage: "shoal kernel status [--json]",
                options: &[
                    OptionSpec {
                        short: None,
                        long: "--json",
                        value: None,
                        help: "Emit the complete status response as JSON",
                    },
                    HELP,
                ],
                output: "PID, uptime, socket, and authenticated principal in human or JSON form.",
                errors: "Fails if no kernel is reachable or the status request is rejected.",
                example: "shoal kernel status --json",
                exit_status: "0 when status is returned; 1 on connection or protocol failure.",
            },
            ActionSpec {
                name: "stop",
                summary: "Authenticate and stop the resident kernel",
                usage: "shoal kernel stop [--json]",
                options: &[
                    OptionSpec {
                        short: None,
                        long: "--json",
                        value: None,
                        help: "Emit the shutdown response as JSON",
                    },
                    HELP,
                ],
                output: "Shutdown acknowledgement in human or JSON form.",
                errors: "Fails if no kernel is reachable or supervisor authentication fails. A kernel started by this CLI uses its owner-only managed credential unless SHOAL_TOKEN is explicit.",
                example: "shoal kernel stop",
                exit_status: "0 when shutdown is accepted; 1 on connection or protocol failure.",
            },
        ],
    },
    CommandSpec {
        name: "fmt",
        summary: "Format .shl source",
        usage: "shoal fmt [--check] [FILE...]",
        arguments: "FILE names Shoal sources. With no FILE, source is read from standard input.",
        words: &[],
        options: &[
            OptionSpec {
                short: None,
                long: "--check",
                value: None,
                help: "Report whether formatting would change input; do not write",
            },
            HELP,
        ],
        output: "Formatted source on standard output for stdin; files are updated atomically in write mode.",
        errors: "Rejects unreadable, oversized, invalid, or unsafe-to-rewrite input; comments are left unchanged.",
        example: "shoal fmt --check scripts/check.shl",
        exit_status: "0 when formatted or already clean; 1 when --check finds changes or formatting fails.",
        actions: &[],
    },
    CommandSpec {
        name: "doctor",
        summary: "Diagnose the installation",
        usage: "shoal doctor [--json]",
        arguments: "No positional arguments.",
        words: &[],
        options: &[
            OptionSpec {
                short: None,
                long: "--json",
                value: None,
                help: "Emit the complete diagnostic report as JSON",
            },
            HELP,
        ],
        output: "A human checklist by default, or a machine-readable report with --json.",
        errors: "Individual failed checks are included in the report instead of stopping the audit early.",
        example: "shoal doctor --json",
        exit_status: "0 when required checks pass; nonzero when the report contains required failures.",
        actions: &[],
    },
    CommandSpec {
        name: "lsp",
        summary: "Run the language server",
        usage: "shoal lsp",
        arguments: "No arguments; use shoal-lsp directly for editor integration.",
        words: &[],
        options: &[HELP],
        output: "Language Server Protocol frames on standard output.",
        errors: "Fails if the shoal-lsp companion executable cannot be launched.",
        example: "shoal lsp",
        exit_status: "The shoal-lsp companion process exit status.",
        actions: &[],
    },
    CommandSpec {
        name: "mcp",
        summary: "Run the MCP server",
        usage: "shoal mcp",
        arguments: "No arguments; configure authentication and sockets with the shoal-mcp environment.",
        words: &[],
        options: &[HELP],
        output: "Model Context Protocol frames on standard output.",
        errors: "Fails if the shoal-mcp companion executable cannot be launched.",
        example: "SHOAL_SESSION=automation shoal mcp",
        exit_status: "The shoal-mcp companion process exit status.",
        actions: &[],
    },
    CommandSpec {
        name: "completions",
        summary: "Generate shell completions",
        usage: "shoal completions <bash|zsh|fish>",
        arguments: "Select exactly one supported host shell: bash, zsh, or fish.",
        words: &["bash", "zsh", "fish"],
        options: &[HELP],
        output: "A complete shell-specific completion program on standard output.",
        errors: "Rejects missing, unsupported, or additional shell arguments.",
        example: "shoal completions zsh > ~/.zfunc/_shoal",
        exit_status: "0 on success; 1 for an unsupported invocation.",
        actions: &[],
    },
    CommandSpec {
        name: "prompt",
        summary: "Inspect and benchmark the prompt",
        usage: "shoal prompt <explain|print|bench> [ACTION OPTIONS]",
        arguments: "SIDE is left, right, continuation, or transient. The default action is explain.",
        words: &["explain", "print", "bench"],
        options: &[HELP],
        output: "Rendered prompt text, per-module explanation, or benchmark percentiles.",
        errors: "Rejects unknown actions, sides, options, and non-numeric sample counts.",
        example: "shoal prompt bench --side left --n 1000",
        exit_status: "0 on success; bench returns 1 when p99 exceeds the configured deadline.",
        actions: &[
            ActionSpec {
                name: "explain",
                summary: "Explain the configured modules for one prompt side",
                usage: "shoal prompt explain [--side SIDE]",
                options: &[
                    OptionSpec {
                        short: None,
                        long: "--side",
                        value: Some("SIDE"),
                        help: "Select left, right, continuation, or transient",
                    },
                    HELP,
                ],
                output: "The format plus each placeholder's rendered value and elapsed time.",
                errors: "Rejects unknown sides or options and reports prompt configuration failures.",
                example: "shoal prompt explain --side right",
                exit_status: "0 on success; 1 when prompt inspection fails.",
            },
            ActionSpec {
                name: "print",
                summary: "Render one configured prompt side",
                usage: "shoal prompt print [--side SIDE]",
                options: &[
                    OptionSpec {
                        short: None,
                        long: "--side",
                        value: Some("SIDE"),
                        help: "Select left, right, continuation, or transient",
                    },
                    HELP,
                ],
                output: "The selected prompt side, including configured terminal styling.",
                errors: "Rejects unknown sides or options and reports prompt configuration failures.",
                example: "shoal prompt print --side transient",
                exit_status: "0 on success; 1 when prompt rendering fails.",
            },
            ActionSpec {
                name: "bench",
                summary: "Benchmark one prompt side against its render deadline",
                usage: "shoal prompt bench [--side SIDE] [--n N]",
                options: &[
                    OptionSpec {
                        short: None,
                        long: "--side",
                        value: Some("SIDE"),
                        help: "Select left, right, continuation, or transient",
                    },
                    OptionSpec {
                        short: None,
                        long: "--n",
                        value: Some("N"),
                        help: "Run N samples (default: 10000)",
                    },
                    HELP,
                ],
                output: "Sample count, p50, p99, maximum time, and configured budget.",
                errors: "Rejects unknown sides, non-numeric sample counts, and unknown options.",
                example: "shoal prompt bench --side left --n 1000",
                exit_status: "0 within budget; 1 when p99 exceeds the configured deadline.",
            },
        ],
    },
];

pub(super) fn root_help() -> String {
    let mut help = String::from(
        "Shoal language and interactive shell\n\nUsage:\n  shoal [OPTIONS] [SCRIPT [ARGS...]]\n  shoal <COMMAND> [ARGS...]\n\nOptions:\n",
    );
    write_options(&mut help, ROOT_OPTIONS);
    help.push_str("\nCommands:\n");
    for command in COMMANDS {
        writeln!(help, "  {:<12} {}", command.name, command.summary).unwrap();
    }
    help.push_str(
        "\nOutput:\n  Values render as text or structured tables. Script arguments are available in `args`.\n\nErrors:\n  Parse and evaluation diagnostics name the source location and are written to standard error.\n\nExamples:\n  shoal\n  shoal --standalone -c 'math.sqrt(9)'\n  shoal scripts/check.shl release\n\nExit status:\n  0 on success; 2 for parse errors; evaluation failures use a typed status or 1.\n  The `exit CODE` builtin returns CODE (1 through 255) without rendering a final value.",
    );
    help
}

pub(super) fn command_help(name: &str) -> Option<String> {
    let command = COMMANDS.iter().find(|command| command.name == name)?;
    let mut help = format!(
        "{}\n\nUsage:\n  {}\n\nArguments:\n  {}\n\nOptions:\n",
        command.summary, command.usage, command.arguments
    );
    write_options(&mut help, command.options);
    write!(
        help,
        "\nOutput:\n  {}\n\nErrors:\n  {}\n\nExamples:\n  {}\n\nExit status:\n  {}",
        command.output, command.errors, command.example, command.exit_status
    )
    .unwrap();
    Some(help)
}

pub(super) fn action_help(command_name: &str, action_name: &str) -> Option<String> {
    let command = COMMANDS
        .iter()
        .find(|command| command.name == command_name)?;
    let action = command
        .actions
        .iter()
        .find(|action| action.name == action_name)?;
    let mut help = format!(
        "{}\n\nUsage:\n  {}\n\nOptions:\n",
        action.summary, action.usage
    );
    write_options(&mut help, action.options);
    write!(
        help,
        "\nOutput:\n  {}\n\nErrors:\n  {}\n\nExamples:\n  {}\n\nExit status:\n  {}",
        action.output, action.errors, action.example, action.exit_status
    )
    .unwrap();
    Some(help)
}

fn write_options(output: &mut String, options: &[OptionSpec]) {
    for option in options {
        let names = match (option.short, option.value) {
            (Some(short), Some(value)) => format!("{short}, {} {value}", option.long),
            (Some(short), None) => format!("{short}, {}", option.long),
            (None, Some(value)) => format!("{} {value}", option.long),
            (None, None) => option.long.to_string(),
        };
        writeln!(output, "  {names:<24} {}", option.help).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_help_page_covers_the_operational_contract() {
        let command_pages = COMMANDS
            .iter()
            .map(|command| command_help(command.name).unwrap());
        let action_pages = COMMANDS.iter().flat_map(|command| {
            command
                .actions
                .iter()
                .map(|action| action_help(command.name, action.name).unwrap())
        });
        let pages = std::iter::once(root_help())
            .chain(command_pages)
            .chain(action_pages);
        for help in pages {
            for section in [
                "Usage:",
                "Options:",
                "Output:",
                "Errors:",
                "Examples:",
                "Exit status:",
            ] {
                assert!(help.contains(section), "help omitted {section}:\n{help}");
            }
        }
        for command in COMMANDS {
            for action in command.actions {
                assert!(
                    command.words.contains(&action.name),
                    "{} completion vocabulary omitted documented action {}",
                    command.name,
                    action.name
                );
            }
        }

        let man = include_str!("../../../../man/shoal.1");
        for option in ROOT_OPTIONS
            .iter()
            .chain(COMMANDS.iter().flat_map(|command| command.options))
            .chain(
                COMMANDS
                    .iter()
                    .flat_map(|command| command.actions)
                    .flat_map(|action| action.options),
            )
        {
            assert!(
                man.contains(&option.long.replace("--", "\\-\\-")),
                "shoal(1) omitted {}",
                option.long
            );
        }
    }
}
