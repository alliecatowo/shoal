//! Typed metadata for Shoal's builtin command heads.
//!
//! This is deliberately richer than a help-text table: positional arity and
//! types, flags, subcommands, results, errors, and examples are executable data
//! for runtime validation, completion, LSP, generated manuals, and tests. The
//! renderer is only one consumer.

use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamArity {
    Required,
    Optional,
    Variadic,
    OneOrMore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandParamSpec {
    pub name: &'static str,
    pub ty: &'static str,
    pub arity: ParamArity,
    pub description: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandFlagSpec {
    pub long: &'static str,
    /// Every accepted short spelling (for example both `-r` and `-R`).
    pub short: &'static [char],
    pub value: Option<&'static str>,
    pub description: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSubcommandSpec {
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinCommandSpec {
    pub name: &'static str,
    pub canonical: &'static str,
    pub summary: &'static str,
    pub params: &'static [CommandParamSpec],
    pub flags: &'static [CommandFlagSpec],
    pub subcommands: &'static [CommandSubcommandSpec],
    pub result: &'static str,
    pub errors: &'static [&'static str],
    pub examples: &'static [&'static str],
}

const fn param(
    name: &'static str,
    ty: &'static str,
    arity: ParamArity,
    description: &'static str,
) -> CommandParamSpec {
    CommandParamSpec {
        name,
        ty,
        arity,
        description,
    }
}

const fn flag(
    long: &'static str,
    short: &'static [char],
    value: Option<&'static str>,
    description: &'static str,
) -> CommandFlagSpec {
    CommandFlagSpec {
        long,
        short,
        value,
        description,
    }
}

const PATHS: &[CommandParamSpec] = &[param(
    "path",
    "path",
    ParamArity::OneOrMore,
    "One or more filesystem paths.",
)];
const OPTIONAL_PATHS: &[CommandParamSpec] = &[param(
    "path",
    "path",
    ParamArity::Variadic,
    "Filesystem paths; defaults to the current directory.",
)];
const SRC_DEST: &[CommandParamSpec] = &[
    param(
        "source",
        "path",
        ParamArity::OneOrMore,
        "One or more source paths.",
    ),
    param(
        "destination",
        "path",
        ParamArity::Required,
        "Destination path; must be a directory for multiple sources.",
    ),
];
const OPTIONAL_DIR: &[CommandParamSpec] = &[param(
    "directory",
    "path",
    ParamArity::Optional,
    "Target directory; cd defaults to HOME and pushd swaps when omitted.",
)];
const OPTIONAL_QUERY: &[CommandParamSpec] = &[param(
    "query",
    "str|path",
    ParamArity::Optional,
    "Directory path or frecency query.",
)];
const HELP_ERRORS: &[&str] = &["Invalid arity or value types produce arg_error/type_error."];
const LS_FLAGS: &[CommandFlagSpec] = &[flag(
    "all",
    &['a'],
    None,
    "Include entries whose names begin with a dot.",
)];
const MKDIR_FLAGS: &[CommandFlagSpec] = &[flag(
    "parents",
    &['p'],
    None,
    "Create missing parent directories.",
)];
const RECURSIVE_FLAGS: &[CommandFlagSpec] = &[flag(
    "recursive",
    &['r', 'R'],
    None,
    "Copy directory trees recursively (-R is also accepted).",
)];
const RM_FLAGS: &[CommandFlagSpec] = &[
    flag(
        "recursive",
        &['r', 'R'],
        None,
        "Remove directory trees recursively (-R is also accepted).",
    ),
    flag(
        "permanent",
        &[],
        None,
        "Delete permanently instead of moving entries to Shoal trash.",
    ),
];
const LN_FLAGS: &[CommandFlagSpec] = &[flag(
    "symbolic",
    &['s'],
    None,
    "Create a symbolic link instead of a hard link.",
)];
const WHICH_FLAGS: &[CommandFlagSpec] = &[flag(
    "all",
    &['a'],
    None,
    "List every candidate from every Reef provider.",
)];
const REEF_FLAGS: &[CommandFlagSpec] = &[flag(
    "refresh",
    &[],
    None,
    "Refresh resolved versions while running `reef lock`.",
)];
const JOURNAL_FLAGS: &[CommandFlagSpec] = &[
    flag(
        "head",
        &[],
        Some("COMMAND"),
        "Filter entries by source command head.",
    ),
    flag(
        "principal",
        &[],
        Some("NAME"),
        "Filter entries by journal principal.",
    ),
    flag(
        "limit",
        &[],
        Some("COUNT"),
        "Return at most COUNT recent entries.",
    ),
];
const REEF_SUBCOMMANDS: &[CommandSubcommandSpec] = &[
    CommandSubcommandSpec {
        name: "add",
        usage: "reef add <tool>@<version>",
        description: "Add or update a project tool constraint and lock it.",
    },
    CommandSubcommandSpec {
        name: "lock",
        usage: "reef lock [--refresh]",
        description: "Resolve constraints and persist the project lock.",
    },
    CommandSubcommandSpec {
        name: "fetch",
        usage: "reef fetch <tool>",
        description: "Fetch the locked tool through its provider.",
    },
    CommandSubcommandSpec {
        name: "doctor",
        usage: "reef doctor",
        description: "Report provider, lock, and ambient-shadow health.",
    },
];

macro_rules! spec {
    ($name:literal, $canonical:literal, $summary:literal, $params:expr, $flags:expr,
     $subs:expr, $result:literal, $errors:expr, [$($example:literal),+ $(,)?]) => {
        BuiltinCommandSpec {
            name: $name,
            canonical: $canonical,
            summary: $summary,
            params: $params,
            flags: $flags,
            subcommands: $subs,
            result: $result,
            errors: $errors,
            examples: &[$($example),+],
        }
    };
}

// Keep entries alphabetic. Aliases have their own head entry so help renders
// the spelling the user invoked while sharing the canonical semantic owner.
const BUILTINS: &[BuiltinCommandSpec] = &[
    spec!(
        "apply",
        "apply",
        "Apply an approved effect plan.",
        &[param(
            "plan",
            "ref",
            ParamArity::Required,
            "Plan reference returned by `plan`."
        )],
        &[],
        &[],
        "The applied program's value.",
        &["Unknown, stale, or unapproved plan references are rejected."],
        ["apply (plan { rm old.log })"]
    ),
    spec!(
        "assert",
        "assert",
        "Require a condition to be true.",
        &[
            param(
                "condition",
                "bool",
                ParamArity::Required,
                "Condition that must hold."
            ),
            param("message", "str", ParamArity::Optional, "Failure message.")
        ],
        &[],
        &[],
        "null when the assertion succeeds.",
        &["A false condition raises assert_failed."],
        ["assert (tests.len() > 0) \"no tests found\""]
    ),
    spec!(
        "cat",
        "cat",
        "Read files as one bounded byte value.",
        PATHS,
        &[],
        &[],
        "bytes containing the files in order.",
        &["Missing/unreadable paths fail; retained output is bounded."],
        ["cat README.md LICENSE"]
    ),
    spec!(
        "cd",
        "cd",
        "Change the session working directory.",
        OPTIONAL_DIR,
        &[],
        &[],
        "The canonical destination path.",
        &["The target must resolve to a directory; session cwd cannot change inside a function."],
        ["cd ./workspace", "cd -"]
    ),
    spec!(
        "cp",
        "cp",
        "Copy files or directory trees.",
        SRC_DEST,
        RECURSIVE_FLAGS,
        &[],
        "A list of destination paths.",
        &[
            "Directories require --recursive; unsafe same-file and nested destinations are rejected."
        ],
        ["cp report.txt archive/", "cp --recursive src backup/"]
    ),
    spec!(
        "dirs",
        "dirs",
        "Show the session directory stack.",
        &[],
        &[],
        &[],
        "A list of paths, current directory first.",
        HELP_ERRORS,
        ["dirs"]
    ),
    spec!(
        "echo",
        "echo",
        "Render values separated by spaces.",
        &[param(
            "value",
            "any",
            ParamArity::Variadic,
            "Values to render."
        )],
        &[],
        &[],
        "A string containing the rendered values.",
        &["Output is rejected if it exceeds the builtin output limit."],
        ["echo \"build\" (jobs.len())"]
    ),
    spec!(
        "env",
        "env",
        "Read the injected process environment snapshot.",
        &[param(
            "name",
            "str",
            ParamArity::Optional,
            "Variable name; omit to return the full environment."
        )],
        &[],
        &[],
        "A record, one string value, or null when the name is absent.",
        &["More than one name is rejected; full output is bounded."],
        ["env PATH", "env"]
    ),
    spec!(
        "exit",
        "exit",
        "Request that the current Shoal host stop.",
        &[param(
            "status",
            "int",
            ParamArity::Optional,
            "Exit status, default 0."
        )],
        &[],
        &[],
        "null; the host observes the pending exit status.",
        HELP_ERRORS,
        ["exit 0"]
    ),
    spec!(
        "explain",
        "explain",
        "Derive and render effects for source text.",
        &[param(
            "source",
            "str",
            ParamArity::Required,
            "Shoal source to inspect without executing."
        )],
        &[],
        &[],
        "A structured effect plan.",
        &["Invalid source or non-literal arguments are rejected."],
        ["explain \"cp a b\""]
    ),
    spec!(
        "head",
        "head",
        "Read the first lines of a text file.",
        &[
            param("file", "path", ParamArity::Required, "Text file to read."),
            param(
                "count",
                "int",
                ParamArity::Optional,
                "Number of lines, default 10."
            )
        ],
        &[],
        &[],
        "A list of strings without line terminators.",
        &["The count must be non-negative; output is bounded."],
        ["head server.log 25"]
    ),
    spec!(
        "history",
        "journal",
        "Show the session journal (alias of journal).",
        &[],
        JOURNAL_FLAGS,
        &[],
        "A table of journal entries.",
        &["Requires an installed session journal."],
        ["history --limit=20"]
    ),
    spec!(
        "interact",
        "interact",
        "Run a command attached to the interactive terminal.",
        &[
            param(
                "command",
                "str",
                ParamArity::Required,
                "Executable command head."
            ),
            param(
                "argument",
                "any",
                ParamArity::Variadic,
                "Arguments passed to the command."
            )
        ],
        &[],
        &[],
        "A structured process outcome.",
        &["Unavailable commands and denied process capabilities fail."],
        ["interact ssh host"]
    ),
    spec!(
        "j",
        "jump",
        "Jump by path or frecency (alias of jump).",
        OPTIONAL_QUERY,
        &[],
        &[],
        "The canonical destination path.",
        &["No matching history raises not_found; session cwd cannot change inside a function."],
        ["j shoal"]
    ),
    spec!(
        "jobs",
        "jobs",
        "Show tasks and process jobs owned by the session.",
        &[],
        &[],
        &[],
        "A table of job ids, states, and results.",
        HELP_ERRORS,
        ["jobs"]
    ),
    spec!(
        "journal",
        "journal",
        "Show the session transaction journal.",
        &[],
        JOURNAL_FLAGS,
        &[],
        "A table of journal entries.",
        &["Requires an installed session journal."],
        ["journal --principal=interactive --limit=20"]
    ),
    spec!(
        "jump",
        "jump",
        "Jump to a directory by path or frecency.",
        OPTIONAL_QUERY,
        &[],
        &[],
        "The canonical destination path.",
        &["No matching history raises not_found; session cwd cannot change inside a function."],
        ["jump projects"]
    ),
    spec!(
        "ln",
        "ln",
        "Create a hard or symbolic link.",
        &[
            param(
                "target",
                "path",
                ParamArity::Required,
                "Existing hard-link target or symbolic target text."
            ),
            param("link", "path", ParamArity::Required, "New link path.")
        ],
        LN_FLAGS,
        &[],
        "A record describing target, link, and link kind.",
        &["Exactly two paths are required; filesystem failures are reported."],
        ["ln target.txt alias.txt", "ln --symbolic ../target alias"]
    ),
    spec!(
        "ls",
        "ls",
        "List typed filesystem metadata.",
        OPTIONAL_PATHS,
        LS_FLAGS,
        &[],
        "A path-sorted table with path, name, type, size, and modified fields.",
        &["Missing/unreadable paths fail; directory results are bounded."],
        ["ls --all . src"]
    ),
    spec!(
        "mkdir",
        "mkdir",
        "Create directories.",
        PATHS,
        MKDIR_FLAGS,
        &[],
        "A list of created paths.",
        &["At least one path is required; existing/missing-parent errors are reported."],
        ["mkdir --parents build/release"]
    ),
    spec!(
        "mv",
        "mv",
        "Move files or directories.",
        SRC_DEST,
        &[],
        &[],
        "A list of destination paths.",
        &["Multiple sources require a directory destination; rename failures are reported."],
        ["mv draft.md docs/final.md"]
    ),
    spec!(
        "open",
        "open",
        "Open a path with the platform desktop handler.",
        &[param("path", "path", ParamArity::Required, "Path to open.")],
        &[],
        &[],
        "null after the opener is launched.",
        &["Unsupported platforms, desktop-handler failures, or denied opener capabilities fail."],
        ["open report.html"]
    ),
    spec!(
        "plan",
        "plan",
        "Derive an effect plan without executing a block.",
        &[param(
            "block",
            "block",
            ParamArity::Required,
            "Shoal block whose effects should be planned."
        )],
        &[],
        &[],
        "A typed plan reference and effect description.",
        &["Unplannable dynamic behavior is marked opaque."],
        ["plan { cp artifact dist/ }"]
    ),
    spec!(
        "popd",
        "popd",
        "Pop and enter the newest stacked directory.",
        &[],
        &[],
        &[],
        "The remaining directory stack.",
        &["An empty stack fails; session cwd cannot change inside a function."],
        ["popd"]
    ),
    spec!(
        "pushd",
        "pushd",
        "Push the current directory and enter another.",
        OPTIONAL_DIR,
        &[],
        &[],
        "The updated directory stack.",
        &["With no argument an empty stack fails; session cwd cannot change inside a function."],
        ["pushd ../other"]
    ),
    spec!(
        "pwd",
        "pwd",
        "Return the session working directory.",
        &[],
        &[],
        &[],
        "The current path.",
        HELP_ERRORS,
        ["pwd"]
    ),
    spec!(
        "quit",
        "exit",
        "Request that the current Shoal host stop (alias of exit).",
        &[param(
            "status",
            "int",
            ParamArity::Optional,
            "Exit status, default 0."
        )],
        &[],
        &[],
        "null; the host observes the pending exit status.",
        HELP_ERRORS,
        ["quit 0"]
    ),
    spec!(
        "reef",
        "reef",
        "Manage reproducible tool resolution.",
        &[
            param(
                "subcommand",
                "str",
                ParamArity::Optional,
                "add, lock, fetch, or doctor; omit to list bindings."
            ),
            param(
                "argument",
                "str",
                ParamArity::Variadic,
                "Subcommand arguments."
            )
        ],
        REEF_FLAGS,
        REEF_SUBCOMMANDS,
        "A binding/diagnostic table or the subcommand result.",
        &[
            "Provider, constraint, lock, and policy failures are typed and leave invalid writes uncommitted."
        ],
        ["reef doctor", "reef add rust@1.90", "reef lock --refresh"]
    ),
    spec!(
        "rm",
        "rm",
        "Remove paths through recoverable Shoal trash by default.",
        PATHS,
        RM_FLAGS,
        &[],
        "A bounded list of removal reports.",
        &["Directories require --recursive; protected/unsafe paths are rejected before mutation."],
        ["rm old.log", "rm --recursive cache/"]
    ),
    spec!(
        "run",
        "run",
        "Run a script by interpreter or invoke a dynamic command.",
        &[
            param(
                "target",
                "path|str",
                ParamArity::Required,
                "Script path or command name."
            ),
            param(
                "argument",
                "any",
                ParamArity::Variadic,
                "Arguments passed to the target."
            )
        ],
        &[],
        &[],
        "The script value or structured process outcome.",
        &["Unknown extensions, missing targets, and denied process capabilities fail."],
        ["run tools/release.py --dry-run", "run git status"]
    ),
    spec!(
        "save",
        "save",
        "Write a value to a path.",
        &[
            param("path", "path", ParamArity::Required, "Destination path."),
            param(
                "value",
                "any",
                ParamArity::Required,
                "Value to serialize and write."
            )
        ],
        &[],
        &[],
        "The saved value.",
        &[
            "Filesystem denial and unsupported serialization fail without bypassing the injected port."
        ],
        ["save report.json (json.stringify(data))"]
    ),
    spec!(
        "sleep",
        "sleep",
        "Pause cooperatively for a duration.",
        &[param(
            "duration",
            "duration|int",
            ParamArity::Required,
            "Duration literal or non-negative seconds."
        )],
        &[],
        &[],
        "null when elapsed or cancelled.",
        &["Exactly one non-negative duration is required."],
        ["sleep 250ms"]
    ),
    spec!(
        "source",
        "source",
        "Evaluate a Shoal script in the current session scope.",
        &[
            param("script", "path", ParamArity::Required, "Shoal source file."),
            param("argument", "any", ParamArity::Variadic, "Script arguments.")
        ],
        &[],
        &[],
        "The sourced program's final value.",
        &["Unreadable or invalid source fails; sourced top-level code may have arbitrary effects."],
        ["source ./env.shl"]
    ),
    spec!(
        "stat",
        "stat",
        "Read typed metadata for paths.",
        PATHS,
        &[],
        &[],
        "One metadata record, or a table for multiple paths.",
        &["At least one existing readable path is required."],
        ["stat Cargo.toml crates/"]
    ),
    spec!(
        "touch",
        "touch",
        "Create files or update their modification times.",
        PATHS,
        &[],
        &[],
        "A list of touched paths.",
        &["At least one path is required; filesystem failures are reported."],
        ["touch build.stamp"]
    ),
    spec!(
        "undo",
        "undo",
        "Undo a journaled filesystem transaction.",
        &[param(
            "entry",
            "ref|int",
            ParamArity::Optional,
            "Journal entry; omit for the newest undoable action."
        )],
        &[],
        &[],
        "A record describing the restored state.",
        &["Missing journals, stale identities, and unsafe restores are rejected."],
        ["undo"]
    ),
    spec!(
        "which",
        "which",
        "Explain command resolution through Shoal and Reef.",
        &[param(
            "command",
            "str",
            ParamArity::Required,
            "Command head to resolve."
        )],
        WHICH_FLAGS,
        &[],
        "A resolution record, or a candidate table with --all.",
        &["Exactly one command is required; provider failures are reported."],
        ["which cargo", "which --all node"]
    ),
];

pub fn builtin_specs() -> &'static [BuiltinCommandSpec] {
    BUILTINS
}

pub fn builtin_spec(name: &str) -> Option<&'static BuiltinCommandSpec> {
    BUILTINS
        .binary_search_by_key(&name, |spec| spec.name)
        .ok()
        .map(|index| &BUILTINS[index])
}

pub fn builtin_help(name: &str) -> Option<String> {
    builtin_spec(name).map(render_help)
}

fn render_help(spec: &BuiltinCommandSpec) -> String {
    let mut out = String::new();
    writeln!(out, "{} — {}", spec.name, spec.summary).expect("string writes cannot fail");
    writeln!(out, "\nUsage:\n  {}{}", spec.name, usage_suffix(spec))
        .expect("string writes cannot fail");
    if !spec.params.is_empty() {
        out.push_str("\nArguments:\n");
        for param in spec.params {
            writeln!(
                out,
                "  {:<18} {} ({})",
                param_label(param),
                param.description,
                param.ty
            )
            .expect("string writes cannot fail");
        }
    }
    out.push_str("\nOptions:\n");
    for option in spec.flags {
        let short = option
            .short
            .iter()
            .map(|short| format!("-{short}"))
            .collect::<Vec<_>>()
            .join(", ");
        let short = if short.is_empty() {
            short
        } else {
            format!("{short}, ")
        };
        let value = option.value.map_or_else(String::new, |v| format!(" <{v}>"));
        writeln!(
            out,
            "  {short}--{}{value:<8} {}",
            option.long, option.description
        )
        .expect("string writes cannot fail");
    }
    out.push_str("  -h, --help         Show this help without executing the command.\n");
    if !spec.subcommands.is_empty() {
        out.push_str("\nSubcommands:\n");
        for sub in spec.subcommands {
            writeln!(out, "  {:<24} {}", sub.usage, sub.description)
                .expect("string writes cannot fail");
        }
    }
    writeln!(out, "\nReturns:\n  {}", spec.result).expect("string writes cannot fail");
    out.push_str("\nErrors:\n");
    for error in spec.errors {
        writeln!(out, "  - {error}").expect("string writes cannot fail");
    }
    out.push_str("\nExamples:\n");
    for example in spec.examples {
        writeln!(out, "  {example}").expect("string writes cannot fail");
    }
    out.pop();
    out
}

fn usage_suffix(spec: &BuiltinCommandSpec) -> String {
    let mut suffix = String::new();
    for param in spec.params {
        let label = param.name.to_ascii_uppercase();
        match param.arity {
            ParamArity::Required => write!(suffix, " <{label}>").unwrap(),
            ParamArity::Optional => write!(suffix, " [{label}]").unwrap(),
            ParamArity::Variadic => write!(suffix, " [{label}...]").unwrap(),
            ParamArity::OneOrMore => write!(suffix, " <{label}>...").unwrap(),
        }
    }
    suffix
}

fn param_label(param: &CommandParamSpec) -> String {
    let name = param.name.to_ascii_uppercase();
    match param.arity {
        ParamArity::Required => format!("<{name}>"),
        ParamArity::Optional => format!("[{name}]"),
        ParamArity::Variadic => format!("[{name}...]"),
        ParamArity::OneOrMore => format!("<{name}>..."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_sorted_unique_complete_and_useful() {
        assert_eq!(BUILTINS.len(), 37);
        for pair in BUILTINS.windows(2) {
            assert!(
                pair[0].name < pair[1].name,
                "metadata must be sorted and unique"
            );
        }
        for spec in BUILTINS {
            assert!(!spec.summary.trim().is_empty(), "{} summary", spec.name);
            assert!(!spec.result.trim().is_empty(), "{} result", spec.name);
            assert!(!spec.errors.is_empty(), "{} errors", spec.name);
            assert!(!spec.examples.is_empty(), "{} examples", spec.name);
            assert!(
                builtin_spec(spec.canonical).is_some(),
                "{} canonical",
                spec.name
            );
            let help = builtin_help(spec.name).expect("registered help");
            for section in ["Usage:", "Options:", "Returns:", "Errors:", "Examples:"] {
                assert!(help.contains(section), "{} lacks {section}", spec.name);
            }
            assert!(help.contains("--help"), "{} help option", spec.name);
            assert!(help.contains(spec.examples[0]), "{} example", spec.name);

            let mut params = std::collections::BTreeSet::new();
            for param in spec.params {
                assert!(
                    params.insert(param.name),
                    "{} duplicate parameter",
                    spec.name
                );
                assert!(!param.ty.is_empty(), "{} empty parameter type", spec.name);
                assert!(
                    !param.description.is_empty(),
                    "{} empty parameter docs",
                    spec.name
                );
            }
            let mut longs = std::collections::BTreeSet::new();
            let mut shorts = std::collections::BTreeSet::new();
            for flag in spec.flags {
                assert!(
                    longs.insert(flag.long),
                    "{} duplicate --{}",
                    spec.name,
                    flag.long
                );
                assert_ne!(flag.long, "help", "help is the shared standard flag");
                assert!(
                    !flag.description.is_empty(),
                    "{} empty flag docs",
                    spec.name
                );
                for short in flag.short {
                    assert!(shorts.insert(*short), "{} duplicate -{short}", spec.name);
                    assert_ne!(*short, 'h', "help is the shared standard flag");
                }
            }
            let mut subcommands = std::collections::BTreeSet::new();
            for subcommand in spec.subcommands {
                assert!(
                    subcommands.insert(subcommand.name),
                    "{} duplicate subcommand {}",
                    spec.name,
                    subcommand.name
                );
                assert!(subcommand.usage.starts_with(spec.name));
                assert!(!subcommand.description.is_empty());
            }
        }
    }

    #[test]
    fn aliases_point_to_their_canonical_schema_owner() {
        assert_eq!(builtin_spec("j").unwrap().canonical, "jump");
        assert_eq!(builtin_spec("quit").unwrap().canonical, "exit");
        assert_eq!(builtin_spec("history").unwrap().canonical, "journal");
    }
}
