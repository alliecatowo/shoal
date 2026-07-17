//! CLI argument parsing and top-level subcommand dispatch: the `Action` the
//! process should take (interactive REPL, run a script/`-c` source, or one
//! of the developer subcommands `fmt`/`doctor`/`lsp`/`mcp`/`completions`/
//! `prompt`), plus the handlers for the non-REPL, non-`run_source` actions.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use shoal_syntax::parse;

use crate::prompt;

pub(crate) const USAGE: &str = "shoal 0.1.0\n\nUsage: shoal [OPTIONS] [SCRIPT]\n       shoal <fmt|doctor|kernel|lsp|mcp|completions|prompt> ...\n\nOptions:\n  -c, --command <SOURCE>  Evaluate source and exit\n  --standalone            Run an explicit embedded/offline REPL\n  -h, --help              Print help\n  -V, --version           Print version\n\nCommands:\n  kernel start|status|stop [--json]  Manage the resident kernel\n\nDeveloper commands:\n  fmt [--check] [FILES]   Format .shl source (stdin when no files)\n  doctor [--json]         Diagnose the installation\n  lsp                     Run the language server companion\n  mcp                     Run the MCP companion\n  completions SHELL       Print bash, zsh, or fish completions\n  prompt explain|bench|print [--side left|right|continuation|transient] [--n N]";

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
    Help,
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
                if a == "--check" {
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
            let args = iter.filter_map(|a| a.into_string().ok());
            Ok(Action::Prompt(prompt::parse_action(args)?))
        }
        Some("lsp") => no_trailing(iter, Action::Companion("shoal-lsp")),
        Some("mcp") => no_trailing(iter, Action::Companion("shoal-mcp")),
        Some("completions") => {
            let shell = iter
                .next()
                .ok_or("completions requires bash, zsh, or fish")?
                .into_string()
                .map_err(|_| "shell name is not UTF-8")?;
            if iter.next().is_some() {
                return Err("unexpected completion argument".into());
            }
            Ok(Action::Completions(shell))
        }
        Some("-h" | "--help") => no_trailing(iter, Action::Help),
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

fn no_trailing(mut iter: impl Iterator<Item = OsString>, action: Action) -> Result<Action, String> {
    if iter.next().is_some() {
        Err("unexpected argument".into())
    } else {
        Ok(action)
    }
}

pub(crate) fn fmt_command(check: bool, files: Vec<PathBuf>) -> Result<i32, String> {
    if files.is_empty() {
        let src = read_source_stream(io::stdin().lock(), "stdin")?;
        let ast = parse(&src).map_err(|e| format!("stdin: {e}"))?;
        let formatted = shoal_syntax::format_program(&ast);
        if check {
            return Ok(i32::from(formatted != src));
        }
        print!("{formatted}");
        return Ok(0);
    }
    let mut changed = false;
    for path in files {
        let src = read_source_path(&path)?;
        let ast = parse(&src).map_err(|e| format!("{}: {e}", path.display()))?;
        let formatted = shoal_syntax::format_program(&ast);
        if formatted != src {
            changed = true;
            if !check {
                atomic_write(&path, formatted.as_bytes())?
            }
        }
    }
    Ok(if check && changed { 1 } else { 0 })
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

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| format!("invalid path {}", path.display()))?
        .to_string_lossy();
    let tmp = parent.join(format!(".{name}.shoal-fmt-{}", std::process::id()));
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e: io::Error| format!("cannot write {}: {e}", path.display()))
}
pub(crate) fn run_companion(name: &str) -> Result<i32, String> {
    let status = std::process::Command::new(name).status().map_err(|e| {
        format!("cannot launch `{name}`: {e}; install the companion binary or add it to PATH")
    })?;
    Ok(status.code().unwrap_or(1))
}
pub(crate) fn completion_script(shell: &str) -> Result<&'static str, String> {
    match shell {
        "bash" => Ok(
            "_shoal(){ COMPREPLY=( $(compgen -W 'fmt doctor kernel lsp mcp completions --help --version --command --standalone' -- \"${COMP_WORDS[COMP_CWORD]}\") ); }\ncomplete -F _shoal shoal\n",
        ),
        "zsh" => Ok(
            "#compdef shoal\n_arguments '--standalone[run embedded/offline REPL]' '1:command:(fmt doctor kernel lsp mcp completions)' '*:file:_files'\n",
        ),
        "fish" => Ok(
            "complete -c shoal -f -a 'fmt doctor kernel lsp mcp completions'\ncomplete -c shoal -s c -l command -r\ncomplete -c shoal -l standalone\n",
        ),
        _ => Err(format!(
            "unsupported shell `{shell}`; expected bash, zsh, or fish"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
