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

pub(crate) const USAGE: &str = "Shoal language and interactive shell\n\nUsage: shoal [OPTIONS] [SCRIPT [ARGS...]]\n       shoal <COMMAND> [ARGS...]\n\nOptions:\n  -c, --command SOURCE  Evaluate source\n  --standalone          Run in-process without kernel protocol\n  -h, --help            Print help\n  -V, --version         Print version\n\nCommands:\n  kernel      Manage the resident kernel\n  fmt         Format .shl source\n  doctor      Diagnose the installation\n  lsp         Run the language server\n  mcp         Run the MCP server\n  completions Generate shell completions\n  prompt      Inspect and benchmark the prompt";
pub(crate) const FMT_USAGE: &str = "Format Shoal source\n\nUsage: shoal fmt [--check] [FILE...]\n\nWith no files, reads standard input.";
pub(crate) const DOCTOR_USAGE: &str =
    "Diagnose the Shoal installation\n\nUsage: shoal doctor [--json]";
pub(crate) const KERNEL_USAGE: &str =
    "Manage the resident kernel\n\nUsage: shoal kernel <start|status|stop> [--json]";
pub(crate) const LSP_USAGE: &str = "Run the language server\n\nUsage: shoal lsp";
pub(crate) const MCP_USAGE: &str = "Run the MCP server\n\nUsage: shoal mcp";
pub(crate) const COMPLETIONS_USAGE: &str =
    "Generate shell completions\n\nUsage: shoal completions <bash|zsh|fish>";
pub(crate) const PROMPT_USAGE: &str = "Inspect and benchmark the prompt\n\nUsage: shoal prompt <explain|print|bench> [--side SIDE] [--n N]\n\nSIDE is left, right, continuation, or transient.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KernelAction {
    Start { json: bool },
    Status { json: bool },
    Stop { json: bool },
}

pub(crate) enum Action {
    Command(String, Vec<OsString>),
    Script(PathBuf, Vec<OsString>),
    Stdin,
    Interactive { standalone: bool },
    Help(&'static str),
    Version,
    Fmt { check: bool, files: Vec<PathBuf> },
    Doctor { json: bool },
    Kernel(KernelAction),
    Companion(&'static str),
    Completions(String),
    Prompt(prompt::PromptAction),
}

pub(crate) fn parse_args(args: Vec<OsString>, stdin_is_tty: bool) -> Result<Action, String> {
    let mut iter = args.into_iter();
    let Some(first) = iter.next() else {
        return Ok(if stdin_is_tty {
            Action::Interactive { standalone: false }
        } else {
            Action::Stdin
        });
    };
    match first.to_str() {
        Some("--standalone") => no_trailing(iter, Action::Interactive { standalone: true }),
        Some("fmt") => {
            let mut check = false;
            let mut files = vec![];
            for a in iter {
                if a == "-h" || a == "--help" {
                    return Ok(Action::Help(FMT_USAGE));
                } else if a == "--check" {
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
                return Ok(Action::Help(DOCTOR_USAGE));
            }
            if args.iter().any(|a| a != "--json") {
                return Err("doctor accepts only --json".into());
            }
            Ok(Action::Doctor {
                json: !args.is_empty(),
            })
        }
        Some("kernel") => {
            let args = iter
                .map(|arg| {
                    arg.into_string()
                        .map_err(|_| "kernel arguments must be UTF-8".to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            if args.as_slice() == ["-h"] || args.as_slice() == ["--help"] {
                return Ok(Action::Help(KERNEL_USAGE));
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
                .filter_map(|a| a.into_string().ok())
                .collect::<Vec<_>>();
            if args.as_slice() == ["-h"] || args.as_slice() == ["--help"] {
                Ok(Action::Help(PROMPT_USAGE))
            } else {
                Ok(Action::Prompt(prompt::parse_action(args.into_iter())?))
            }
        }
        Some("lsp") => companion_or_help(iter, "shoal-lsp", LSP_USAGE),
        Some("mcp") => companion_or_help(iter, "shoal-mcp", MCP_USAGE),
        Some("completions") => {
            let first = iter
                .next()
                .ok_or("completions requires bash, zsh, or fish")?
                .into_string()
                .map_err(|_| "shell name is not UTF-8")?;
            if first == "-h" || first == "--help" {
                return no_trailing(iter, Action::Help(COMPLETIONS_USAGE));
            }
            let shell = first;
            if iter.next().is_some() {
                return Err("unexpected completion argument".into());
            }
            Ok(Action::Completions(shell))
        }
        Some("-h" | "--help") => no_trailing(iter, Action::Help(USAGE)),
        Some("-V" | "--version") => no_trailing(iter, Action::Version),
        Some("-c" | "--command") => {
            let source = iter
                .next()
                .ok_or_else(|| "-c/--command requires source".to_string())?
                .into_string()
                .map_err(|_| "command source is not valid UTF-8".to_string())?;
            Ok(Action::Command(source, iter.collect()))
        }
        Some("--") => {
            let path = iter
                .next()
                .ok_or_else(|| "-- must be followed by a script path".to_string())?;
            Ok(Action::Script(path.into(), iter.collect()))
        }
        Some(s) if s.starts_with('-') => Err(format!("unknown option `{s}`\n\n{USAGE}")),
        _ => Ok(Action::Script(first.into(), iter.collect())),
    }
}

fn companion_or_help(
    mut iter: impl Iterator<Item = OsString>,
    name: &'static str,
    usage: &'static str,
) -> Result<Action, String> {
    match iter.next() {
        None => Ok(Action::Companion(name)),
        Some(arg) if arg == "-h" || arg == "--help" => no_trailing(iter, Action::Help(usage)),
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
        assert!(USAGE.contains("--standalone          Run in-process without kernel protocol"));
        assert!(!USAGE.contains("embedded kernel"));
        assert!(matches!(
            parse_args(vec![], true).unwrap(),
            Action::Interactive { standalone: false }
        ));
        assert!(matches!(
            parse_args(vec!["--standalone".into()], true).unwrap(),
            Action::Interactive { standalone: true }
        ));
        assert!(matches!(parse_args(vec![], false).unwrap(), Action::Stdin));
        assert!(matches!(
            parse_args(vec!["-c".into(), "1 + 1".into()], true).unwrap(),
            Action::Command(_, _)
        ));
        assert!(parse_args(vec!["--wat".into()], true).is_err());
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
        assert!(matches!(
            parse_args(vec!["lsp".into()], true).unwrap(),
            Action::Companion("shoal-lsp")
        ));
        assert!(completion_script("wat").is_err());
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
