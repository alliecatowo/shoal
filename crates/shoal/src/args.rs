//! CLI argument parsing and top-level subcommand dispatch: the `Action` the
//! process should take (interactive REPL, run a script/`-c` source, or one
//! of the developer subcommands `fmt`/`doctor`/`lsp`/`mcp`/`completions`/
//! `prompt`), plus the handlers for the non-REPL, non-`run_source` actions.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::prompt;

#[path = "args/completions.rs"]
mod completions;
#[path = "args/schema.rs"]
mod schema;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KernelAction {
    Start { json: bool },
    Status { json: bool },
    Stop { json: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecutionMode {
    Default,
    Standalone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecutionSurface {
    Interactive,
    NonInteractive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecutionHost {
    LocalEvaluator,
    PrivateKernel,
}

/// Resolve CLI execution ownership in one place. Noninteractive invocations
/// are deliberately local in both modes; `--standalone` is an explicit,
/// idempotent choice there. Only an interactive default run may select the
/// isolated private kernel, and only while `kernel.enabled` permits it.
pub(crate) const fn execution_host(
    mode: ExecutionMode,
    surface: ExecutionSurface,
    kernel_enabled: bool,
) -> ExecutionHost {
    if matches!(surface, ExecutionSurface::Interactive)
        && matches!(mode, ExecutionMode::Default)
        && kernel_enabled
    {
        ExecutionHost::PrivateKernel
    } else {
        ExecutionHost::LocalEvaluator
    }
}

pub(crate) enum Action {
    Command {
        source: String,
        args: Vec<OsString>,
        mode: ExecutionMode,
    },
    Script {
        path: PathBuf,
        args: Vec<OsString>,
        mode: ExecutionMode,
    },
    Stdin {
        mode: ExecutionMode,
    },
    Interactive {
        mode: ExecutionMode,
    },
    Help(String),
    Version,
    Fmt {
        check: bool,
        files: Vec<PathBuf>,
    },
    Doctor {
        json: bool,
    },
    Kernel(KernelAction),
    Companion(&'static str),
    Completions(String),
    Prompt(prompt::PromptAction),
}

pub(crate) fn parse_args(args: Vec<OsString>, stdin_is_tty: bool) -> Result<Action, String> {
    let mut iter = args.into_iter().peekable();
    let mut standalone = false;
    while iter
        .peek()
        .and_then(|argument| argument.to_str())
        .is_some_and(|argument| argument == "--standalone")
    {
        iter.next();
        if standalone {
            return Err("--standalone may be specified only once".into());
        }
        standalone = true;
    }
    let mode = if standalone {
        ExecutionMode::Standalone
    } else {
        ExecutionMode::Default
    };
    let Some(first) = iter.next() else {
        return Ok(if stdin_is_tty {
            Action::Interactive { mode }
        } else {
            Action::Stdin { mode }
        });
    };
    if standalone
        && matches!(
            first.to_str(),
            Some(
                "fmt"
                    | "doctor"
                    | "kernel"
                    | "prompt"
                    | "lsp"
                    | "mcp"
                    | "completions"
                    | "-h"
                    | "--help"
                    | "-V"
                    | "--version"
            )
        )
    {
        return Err(
            "--standalone applies only to the interactive shell, -c, scripts, or stdin".into(),
        );
    }
    match first.to_str() {
        Some("fmt") => {
            let args = iter.collect::<Vec<_>>();
            if args.as_slice() == ["-h"] || args.as_slice() == ["--help"] {
                return Ok(Action::Help(schema::command_help("fmt").unwrap()));
            }
            let mut check = false;
            let mut files = vec![];
            for a in args {
                if a == "--check" {
                    if check {
                        return Err("--check may be specified only once".into());
                    }
                    check = true
                } else if a.to_str().is_some_and(|s| s.starts_with('-')) {
                    return Err(format!("unknown fmt option `{}`", a.to_string_lossy()));
                } else {
                    files.push(a.into())
                }
            }
            Ok(Action::Fmt { check, files })
        }
        Some("doctor") => {
            let args = iter.collect::<Vec<_>>();
            if args.as_slice() == ["-h"] || args.as_slice() == ["--help"] {
                return Ok(Action::Help(schema::command_help("doctor").unwrap()));
            }
            match args.as_slice() {
                [] => Ok(Action::Doctor { json: false }),
                [flag] if flag == "--json" => Ok(Action::Doctor { json: true }),
                _ => Err("doctor accepts one optional --json".into()),
            }
        }
        Some("kernel") => {
            let args = iter
                .map(|arg| {
                    arg.into_string()
                        .map_err(|_| "kernel arguments must be UTF-8".to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            if let [verb, flag] = args.as_slice()
                && (flag == "-h" || flag == "--help")
                && let Some(help) = schema::action_help("kernel", verb)
            {
                return Ok(Action::Help(help));
            }
            if args.as_slice() == ["-h"] || args.as_slice() == ["--help"] {
                return Ok(Action::Help(schema::command_help("kernel").unwrap()));
            }
            let (verb, rest) = args
                .split_first()
                .ok_or("kernel requires start, status, or stop")?;
            let json = match rest {
                [] => false,
                [flag] if flag == "--json" => true,
                _ => return Err("kernel accepts only an optional --json after the action".into()),
            };
            let action = match verb.as_str() {
                "start" => KernelAction::Start { json },
                "status" => KernelAction::Status { json },
                "stop" => KernelAction::Stop { json },
                _ => return Err("kernel requires start, status, or stop".into()),
            };
            Ok(Action::Kernel(action))
        }
        Some("prompt") => {
            let args = iter
                .map(|argument| {
                    argument
                        .into_string()
                        .map_err(|_| "prompt arguments must be valid UTF-8".to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            if let [action, flag] = args.as_slice()
                && (flag == "-h" || flag == "--help")
                && let Some(help) = schema::action_help("prompt", action)
            {
                return Ok(Action::Help(help));
            }
            if args.as_slice() == ["-h"] || args.as_slice() == ["--help"] {
                Ok(Action::Help(schema::command_help("prompt").unwrap()))
            } else {
                Ok(Action::Prompt(prompt::parse_action(args.into_iter())?))
            }
        }
        Some("lsp") => companion_or_help(iter, "shoal-lsp", "lsp"),
        Some("mcp") => companion_or_help(iter, "shoal-mcp", "mcp"),
        Some("completions") => {
            let first = iter
                .next()
                .ok_or("completions requires bash, zsh, or fish")?
                .into_string()
                .map_err(|_| "shell name is not UTF-8")?;
            if first == "-h" || first == "--help" {
                return no_trailing(
                    iter,
                    Action::Help(schema::command_help("completions").unwrap()),
                );
            }
            let shell = first;
            if iter.next().is_some() {
                return Err("unexpected completion argument".into());
            }
            Ok(Action::Completions(shell))
        }
        Some("-h" | "--help") => no_trailing(iter, Action::Help(schema::root_help())),
        Some("-V" | "--version") => no_trailing(iter, Action::Version),
        Some("-c" | "--command") => {
            let source = iter
                .next()
                .ok_or_else(|| "-c/--command requires source".to_string())?
                .into_string()
                .map_err(|_| "command source is not valid UTF-8".to_string())?;
            Ok(Action::Command {
                source,
                args: iter.collect(),
                mode,
            })
        }
        Some("--") => {
            let path = iter
                .next()
                .ok_or_else(|| "-- must be followed by a script path".to_string())?;
            Ok(Action::Script {
                path: path.into(),
                args: iter.collect(),
                mode,
            })
        }
        Some(s) if s.starts_with('-') => {
            Err(format!("unknown option `{s}`\n\n{}", schema::root_help()))
        }
        _ => Ok(Action::Script {
            path: first.into(),
            args: iter.collect(),
            mode,
        }),
    }
}

fn companion_or_help(
    mut iter: impl Iterator<Item = OsString>,
    name: &'static str,
    command: &'static str,
) -> Result<Action, String> {
    match iter.next() {
        None => Ok(Action::Companion(name)),
        Some(arg) if arg == "-h" || arg == "--help" => no_trailing(
            iter,
            Action::Help(schema::command_help(command).expect("known companion command")),
        ),
        Some(_) => Err("unexpected argument".into()),
    }
}

fn no_trailing(mut iter: impl Iterator<Item = OsString>, action: Action) -> Result<Action, String> {
    if iter.next().is_some() {
        Err("unexpected argument".into())
    } else {
        Ok(action)
    }
}

pub(crate) fn fmt_command(check: bool, files: Vec<PathBuf>) -> Result<i32, String> {
    crate::format_files::run(check, files)
}

pub(crate) fn read_source_path(path: &Path) -> Result<String, String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "cannot read {}: source is not a regular file",
            path.display()
        ));
    }
    let file =
        fs::File::open(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    read_source_stream(file, &path.display().to_string())
}

pub(crate) fn read_source_stream(reader: impl Read, label: &str) -> Result<String, String> {
    let max_bytes = shoal_syntax::MAX_SOURCE_BYTES;
    let mut bytes = Vec::with_capacity(8 * 1024);
    reader
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {label}: {error}"))?;
    if bytes.len() > max_bytes {
        return Err(format!(
            "{label}: source exceeds the {max_bytes}-byte limit"
        ));
    }
    String::from_utf8(bytes).map_err(|_| format!("{label}: source is not valid UTF-8"))
}

pub(crate) fn run_companion(name: &str) -> Result<i32, String> {
    let status = std::process::Command::new(name).status().map_err(|e| {
        format!("cannot launch `{name}`: {e}; install the companion binary or add it to PATH")
    })?;
    Ok(status.code().unwrap_or(1))
}
pub(crate) fn completion_script(shell: &str) -> Result<String, String> {
    completions::generate(shell)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    struct GrowingReader(usize);

    impl Read for GrowingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = self.0.min(buffer.len());
            buffer[..count].fill(b'x');
            self.0 -= count;
            Ok(count)
        }
    }

    #[test]
    fn argument_modes_are_deterministic() {
        let help = schema::root_help();
        assert!(help.contains("--standalone"));
        assert!(help.contains("Use local evaluation"));
        assert!(!help.contains("embedded kernel"));
        assert!(matches!(
            parse_args(vec![], true).unwrap(),
            Action::Interactive {
                mode: ExecutionMode::Default
            }
        ));
        assert!(matches!(
            parse_args(vec!["--standalone".into()], true).unwrap(),
            Action::Interactive {
                mode: ExecutionMode::Standalone
            }
        ));
        assert!(matches!(
            parse_args(
                vec!["--standalone".into(), "-c".into(), "1 + 1".into()],
                true
            )
            .unwrap(),
            Action::Command {
                mode: ExecutionMode::Standalone,
                ..
            }
        ));
        assert!(matches!(
            parse_args(vec!["--standalone".into(), "script.shl".into()], true).unwrap(),
            Action::Script {
                mode: ExecutionMode::Standalone,
                ..
            }
        ));
        assert!(parse_args(vec!["--standalone".into(), "--standalone".into()], true).is_err());
        assert!(matches!(
            parse_args(vec![], false).unwrap(),
            Action::Stdin {
                mode: ExecutionMode::Default
            }
        ));
        assert!(matches!(
            parse_args(vec!["-c".into(), "1 + 1".into()], true).unwrap(),
            Action::Command {
                mode: ExecutionMode::Default,
                ..
            }
        ));
        assert_eq!(
            execution_host(ExecutionMode::Default, ExecutionSurface::Interactive, true),
            ExecutionHost::PrivateKernel
        );
        for mode in [ExecutionMode::Default, ExecutionMode::Standalone] {
            assert_eq!(
                execution_host(mode, ExecutionSurface::NonInteractive, true),
                ExecutionHost::LocalEvaluator
            );
        }
        assert!(parse_args(vec!["--wat".into()], true).is_err());
        let error = match parse_args(vec!["--standalone".into(), "doctor".into()], true) {
            Err(error) => error,
            Ok(_) => panic!("--standalone must not be ignored by a developer subcommand"),
        };
        assert!(error.contains("applies only"));
    }

    #[test]
    fn developer_subcommands_dispatch() {
        assert!(matches!(
            parse_args(vec!["fmt".into(), "--check".into(), "x.shl".into()], true).unwrap(),
            Action::Fmt { check: true, .. }
        ));
        assert!(matches!(
            parse_args(
                vec!["kernel".into(), "status".into(), "--json".into()],
                true
            )
            .unwrap(),
            Action::Kernel(KernelAction::Status { json: true })
        ));
        assert!(matches!(
            parse_args(vec!["doctor".into(), "--json".into()], true).unwrap(),
            Action::Doctor { json: true }
        ));
        assert!(
            parse_args(
                vec!["doctor".into(), "--json".into(), "--json".into()],
                true
            )
            .is_err()
        );
        assert!(matches!(
            parse_args(vec!["lsp".into()], true).unwrap(),
            Action::Companion("shoal-lsp")
        ));
        assert!(completion_script("wat").is_err());
    }

    #[test]
    fn canonical_schema_options_are_accepted_only_in_their_documented_scope() {
        let root_cases = [
            vec!["-h"],
            vec!["--help"],
            vec!["-V"],
            vec!["--version"],
            vec!["-c", "null"],
            vec!["--command", "null"],
            vec!["--standalone"],
        ];
        for case in root_cases {
            assert!(
                parse_args(case.into_iter().map(Into::into).collect(), true).is_ok(),
                "root schema option was rejected"
            );
        }
        for case in [
            vec!["fmt", "--check", "fixture.shl"],
            vec!["doctor", "--json"],
            vec!["kernel", "start", "--json"],
            vec!["kernel", "status", "--json"],
            vec!["kernel", "stop", "--json"],
            vec!["prompt", "explain", "--side", "right"],
            vec!["prompt", "print", "--side", "transient"],
            vec!["prompt", "bench", "--side", "left", "--n", "1"],
            vec!["completions", "bash"],
            vec!["completions", "zsh"],
            vec!["completions", "fish"],
        ] {
            assert!(
                parse_args(
                    case.iter().map(|value| OsString::from(*value)).collect(),
                    true
                )
                .is_ok(),
                "schema invocation was rejected: {case:?}"
            );
        }
        for invalid in [
            vec!["kernel", "--json", "status"],
            vec!["prompt", "explain", "--n", "1"],
            vec!["prompt", "print", "--n", "1"],
            vec!["doctor", "--json", "--json"],
            vec!["lsp", "extra"],
            vec!["mcp", "extra"],
            vec!["completions", "zsh", "extra"],
        ] {
            assert!(
                parse_args(
                    invalid.iter().map(|value| OsString::from(*value)).collect(),
                    true
                )
                .is_err(),
                "out-of-scope argument was silently accepted: {invalid:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn prompt_rejects_non_utf8_arguments_instead_of_silently_dropping_them() {
        use std::os::unix::ffi::OsStringExt as _;

        let error = match parse_args(
            vec!["prompt".into(), std::ffi::OsString::from_vec(vec![0xff])],
            true,
        ) {
            Err(error) => error,
            Ok(_) => panic!("invalid prompt bytes must remain observable"),
        };
        assert!(error.contains("valid UTF-8"));
    }

    #[test]
    fn fmt_check_and_atomic_write() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("x.shl");
        fs::write(&path, "let x=1").unwrap();
        assert_eq!(fmt_command(true, vec![path.clone()]).unwrap(), 1);
        assert_eq!(fmt_command(false, vec![path.clone()]).unwrap(), 0);
        assert_eq!(fmt_command(true, vec![path]).unwrap(), 0);

        let commented = t.path().join("commented.shl");
        let original = "#!/usr/bin/env shoal\nlet x=1 # keep this note\n";
        fs::write(&commented, original).unwrap();
        assert_eq!(fmt_command(false, vec![commented.clone()]).unwrap(), 0);
        assert_eq!(fs::read_to_string(commented).unwrap(), original);

        let semantic_hash = t.path().join("semantic-hash.shl");
        fs::write(&semantic_hash, "let hash=\"#\"").unwrap();
        assert_eq!(fmt_command(false, vec![semantic_hash.clone()]).unwrap(), 0);
        assert_eq!(
            fs::read_to_string(semantic_hash).unwrap(),
            "let hash = \"#\"\n"
        );
    }

    #[test]
    fn cli_source_read_is_bounded_utf8_and_path_aware() {
        let error = read_source_stream(GrowingReader(shoal_syntax::MAX_SOURCE_BYTES * 4), "stdin")
            .unwrap_err();
        assert!(error.contains("stdin"));
        assert!(error.contains("exceeds"));

        let error = read_source_stream(io::Cursor::new(vec![0xff]), "stdin").unwrap_err();
        assert!(error.contains("not valid UTF-8"));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sparse.shl");
        let file = fs::File::create(&path).unwrap();
        file.set_len((shoal_syntax::MAX_SOURCE_BYTES + 1) as u64)
            .unwrap();
        let error = read_source_path(&path).unwrap_err();
        assert!(error.contains(&path.display().to_string()));
        assert!(error.contains("exceeds"));
    }

    #[cfg(unix)]
    #[test]
    fn cli_source_path_preserves_symlink_to_regular_file() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.shl");
        let link = dir.path().join("link.shl");
        fs::write(&target, "42\n").unwrap();
        symlink(&target, &link).unwrap();
        assert_eq!(read_source_path(&link).unwrap(), "42\n");
    }

    #[test]
    fn cli_entry_points_cannot_regress_to_whole_source_reads() {
        for (name, source) in [
            ("args", include_str!("args.rs")),
            ("main", include_str!("main.rs")),
        ] {
            let production = source.split("#[cfg(test)]").next().unwrap();
            assert!(
                !production.contains("read_to_string"),
                "{name} reintroduced an unbounded whole-source read"
            );
        }
    }
}
