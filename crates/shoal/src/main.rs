use std::borrow::Cow;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use reedline::{
    DefaultPrompt, DefaultPromptSegment, FileBackedHistory, Reedline, Signal, ValidationResult,
    Validator,
};
use shoal_eval::Evaluator;
use shoal_syntax::{ParseError, parse};
use shoal_value::{ErrorVal, Value};

const USAGE: &str = "shoal 0.1.0\n\nUsage: shoal [OPTIONS] [SCRIPT]\n\nOptions:\n  -c, --command <SOURCE>  Evaluate source and exit\n  -h, --help              Print help\n  -V, --version           Print version";

enum Action {
    Command(String, Vec<OsString>),
    Script(PathBuf, Vec<OsString>),
    Stdin,
    Interactive,
    Help,
    Version,
}

fn main() {
    let code = match real_main(std::env::args_os().skip(1).collect()) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("shoal: {message}");
            2
        }
    };
    std::process::exit(code);
}

fn real_main(args: Vec<OsString>) -> Result<i32, String> {
    match parse_args(args, io::stdin().is_terminal())? {
        Action::Help => {
            println!("{USAGE}");
            Ok(0)
        }
        Action::Version => {
            println!("shoal {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        Action::Command(src, args) => run_source(&src, None, false, args),
        Action::Script(path, args) => {
            let src = fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            run_source(&src, Some(&path), false, args)
        }
        Action::Stdin => {
            let mut src = String::new();
            io::stdin()
                .read_to_string(&mut src)
                .map_err(|e| format!("cannot read stdin: {e}"))?;
            run_source(&src, Some(Path::new("<stdin>")), false, Vec::new())
        }
        Action::Interactive => repl(),
    }
}

fn parse_args(args: Vec<OsString>, stdin_is_tty: bool) -> Result<Action, String> {
    let mut iter = args.into_iter();
    let Some(first) = iter.next() else {
        return Ok(if stdin_is_tty {
            Action::Interactive
        } else {
            Action::Stdin
        });
    };
    match first.to_str() {
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

fn run_source(
    src: &str,
    source: Option<&Path>,
    interactive: bool,
    args: Vec<OsString>,
) -> Result<i32, String> {
    let program = match parse(src) {
        Ok(program) => program,
        Err(error) => {
            eprint!("{}", format_parse_error(src, source, &error));
            return Ok(2);
        }
    };
    let cwd = std::env::current_dir().map_err(|e| format!("cannot determine cwd: {e}"))?;
    let mut evaluator = Evaluator::new(cwd);
    evaluator.interactive = interactive;
    evaluator.env.declare(
        "args",
        Value::List(
            args.into_iter()
                .map(|arg| Value::Path(PathBuf::from(arg)))
                .collect(),
        ),
        false,
    );
    if let Some(source) = source {
        evaluator
            .env
            .declare("script", Value::Path(source.to_path_buf()), false);
    }
    match evaluator.eval_program(&program) {
        Ok(value) => {
            render_result(&value, false).map_err(|e| format!("cannot write output: {e}"))?;
            Ok(0)
        }
        Err(error) => {
            eprint!("{}", format_eval_error(src, source, &error));
            Ok(1)
        }
    }
}

fn repl() -> Result<i32, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cannot determine cwd: {e}"))?;
    let mut evaluator = Evaluator::new(cwd);
    evaluator.interactive = true;

    let mut editor = Reedline::create().with_validator(Box::new(ShoalValidator));
    if let Some(path) = history_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(history) = FileBackedHistory::with_file(10_000, path) {
            editor = editor.with_history(Box::new(history));
        }
    }

    loop {
        let prompt = DefaultPrompt::new(
            DefaultPromptSegment::Basic(short_cwd(evaluator.cwd())),
            DefaultPromptSegment::Empty,
        );
        match editor.read_line(&prompt) {
            Ok(Signal::Success(src)) => {
                if src.trim().is_empty() {
                    continue;
                }
                match parse(&src) {
                    Ok(program) => match evaluator.eval_program(&program) {
                        Ok(value) => {
                            if let Err(error) = render_result(&value, true) {
                                eprintln!("shoal: cannot write output: {error}");
                            }
                        }
                        Err(error) => eprint!("{}", format_eval_error(&src, None, &error)),
                    },
                    Err(error) => eprint!("{}", format_parse_error(&src, None, &error)),
                }
            }
            Ok(Signal::CtrlC) => {
                // Reedline has already cleared the current edit buffer.
                eprintln!("^C");
            }
            Ok(Signal::CtrlD) => {
                println!();
                return Ok(0);
            }
            Ok(_) => {}
            Err(error) => return Err(format!("line editor failed: {error}")),
        }
    }
}

fn render_result(value: &Value, pty_was_live: bool) -> io::Result<()> {
    // Statement-position outcomes were already streamed byte-for-byte by the
    // PTY tee. Rendering them here would duplicate the command's output.
    if pty_was_live && matches!(value, Value::Outcome(_)) {
        return Ok(());
    }
    if let Value::Outcome(outcome) = value {
        io::stdout().write_all(&outcome.stdout)?;
        io::stdout().flush()?;
        io::stderr().write_all(&outcome.stderr)?;
        io::stderr().flush()?;
        return Ok(());
    }
    let rendered = shoal_value::render::render_block(value, terminal_width());
    if !rendered.is_empty() {
        println!("{rendered}");
    }
    Ok(())
}

fn terminal_width() -> usize {
    crossterm::terminal::size()
        .map(|(width, _)| usize::from(width))
        .unwrap_or(80)
}

fn history_path() -> Option<PathBuf> {
    if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
        return Some(PathBuf::from(state).join("shoal/history.txt"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state/shoal/history.txt"))
}

fn short_cwd(cwd: &Path) -> String {
    let display = if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        cwd.strip_prefix(&home)
            .map(|tail| PathBuf::from("~").join(tail))
            .unwrap_or_else(|_| cwd.to_path_buf())
    } else {
        cwd.to_path_buf()
    };
    format!("shoal {}", display.display())
}

fn format_parse_error(src: &str, source: Option<&Path>, error: &ParseError) -> String {
    format_diagnostic(
        src,
        source,
        "parse error",
        &error.msg,
        error.span.start as usize,
        error.hint.as_deref(),
    )
}

fn format_eval_error(src: &str, source: Option<&Path>, error: &ErrorVal) -> String {
    let offset = error.span.map_or(0, |span| span.start as usize);
    format_diagnostic(
        src,
        source,
        &error.code,
        &error.msg,
        offset,
        error.hint.as_deref(),
    )
}

fn format_diagnostic(
    src: &str,
    source: Option<&Path>,
    kind: &str,
    message: &str,
    offset: usize,
    hint: Option<&str>,
) -> String {
    let offset = offset.min(src.len());
    let prefix = &src[..offset];
    let line_no = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
    let line_start = prefix.rfind('\n').map_or(0, |index| index + 1);
    let line_end = src[offset..].find('\n').map_or(src.len(), |n| offset + n);
    let line = &src[line_start..line_end];
    let column = src[line_start..offset].chars().count() + 1;
    let name: Cow<'_, str> = source
        .map(|path| path.to_string_lossy())
        .unwrap_or(Cow::Borrowed("<repl>"));
    let mut rendered = format!(
        "{name}:{line_no}:{column}: {kind}: {message}\n  {line}\n  {}^\n",
        " ".repeat(column - 1)
    );
    if let Some(hint) = hint {
        rendered.push_str(&format!("  hint: {hint}\n"));
    }
    rendered
}

struct ShoalValidator;

impl Validator for ShoalValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        if input_is_incomplete(line) {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Complete
        }
    }
}

fn input_is_incomplete(src: &str) -> bool {
    let mut stack = Vec::new();
    let chars: Vec<char> = src.chars().collect();
    let mut quote: Option<(char, bool)> = None;
    let mut escaped = false;
    let mut comment = false;
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if comment {
            if ch == '\n' {
                comment = false;
            }
            index += 1;
            continue;
        }
        if let Some((q, triple)) = quote {
            if triple
                && ch == q
                && chars.get(index + 1) == Some(&q)
                && chars.get(index + 2) == Some(&q)
            {
                quote = None;
                index += 3;
                continue;
            }
            if !triple && q == '"' && ch == '\\' && !escaped {
                escaped = true;
                index += 1;
                continue;
            }
            if !triple && ch == q && !escaped {
                quote = None;
            }
            escaped = false;
            index += 1;
            continue;
        }
        match ch {
            '#' => comment = true,
            '\'' | '"' => {
                let triple = chars.get(index + 1) == Some(&ch) && chars.get(index + 2) == Some(&ch);
                quote = Some((ch, triple));
                if triple {
                    index += 2;
                }
            }
            '(' | '[' | '{' => stack.push(ch),
            ')' if stack.last() == Some(&'(') => {
                stack.pop();
            }
            ']' if stack.last() == Some(&'[') => {
                stack.pop();
            }
            '}' if stack.last() == Some(&'{') => {
                stack.pop();
            }
            _ => {}
        }
        index += 1;
    }
    if quote.is_some() || !stack.is_empty() {
        return true;
    }
    let tail = src.trim_end();
    tail.ends_with('\\')
        || ["&&", "||", "??", "+", "-", "*", "/", "%", "=", ",", "."]
            .iter()
            .any(|operator| tail.ends_with(operator))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_modes_are_deterministic() {
        assert!(matches!(
            parse_args(vec![], true).unwrap(),
            Action::Interactive
        ));
        assert!(matches!(parse_args(vec![], false).unwrap(), Action::Stdin));
        assert!(matches!(
            parse_args(vec!["-c".into(), "1 + 1".into()], true).unwrap(),
            Action::Command(_, _)
        ));
        assert!(parse_args(vec!["--wat".into()], true).is_err());
    }

    #[test]
    fn multiline_detection_ignores_balanced_delimiters_in_strings_and_comments() {
        assert!(input_is_incomplete("if true {\n  1"));
        assert!(input_is_incomplete("1 +"));
        assert!(input_is_incomplete("\"unterminated"));
        assert!(input_is_incomplete("\"\"\"unterminated"));
        assert!(!input_is_incomplete("\"\"\"multiline\ntext\"\"\""));
        assert!(!input_is_incomplete("echo \"{\""));
        assert!(!input_is_incomplete("# {\n1"));
        assert!(!input_is_incomplete("[1, 2]"));
    }

    #[test]
    fn diagnostic_points_at_unicode_columns() {
        let error = ParseError {
            msg: "bad".into(),
            span: shoal_ast::Span::new(3, 4),
            hint: Some("fix it".into()),
        };
        let rendered = format_parse_error("é x", None, &error);
        assert!(rendered.contains("<repl>:1:3: parse error: bad"));
        assert!(rendered.contains("hint: fix it"));
    }
}
