use std::borrow::Cow;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};

use shoal_eval::Evaluator;
use shoal_syntax::{ParseError, parse};
use shoal_value::{ErrorVal, Value};

mod adapters;
mod args;
mod completer;
mod highlight;
mod prompt;
mod repl;
use args::Action;

fn main() {
    // Run everything on a worker thread with a large stack. Deep (or runaway)
    // user recursion walks many native eval frames per shoal-level call; the
    // 8 MiB default would overflow and `abort()` the process well before the
    // interpreter's own recursion guard (10k nested calls) could raise a clean
    // `recursion_limit` error. A 1 GiB reservation is virtual (only touched
    // pages commit) and comfortably outlasts the guard.
    let worker = std::thread::Builder::new()
        .name("shoal-main".into())
        .stack_size(1 << 30)
        .spawn(|| {
            if let Err(error) = run() {
                eprintln!(
                    "{}",
                    maybe_strip(format!("\x1b[31;1merror:\x1b[0m {error}"))
                );
                std::process::exit(1);
            }
        })
        .expect("spawn main worker thread");
    worker.join().expect("main worker thread panicked");
}

/// NO_COLOR (https://no-color.org): disable ANSI escapes whenever the
/// variable is present at all, regardless of its value. Checked lazily on
/// every call rather than cached, so tests (and users) can flip it mid-process.
pub(crate) fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some()
}

/// Strip ANSI CSI escape sequences (`ESC [ ... final-byte`), leaving plain
/// text — used to make our own colorized output NO_COLOR-safe without
/// duplicating every builder as a plain/colored pair.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c2 in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c2) {
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

/// Strip ANSI from `s` when `NO_COLOR` is set; pass it through unchanged
/// otherwise. Used at every terminal-output boundary so colorized output
/// built elsewhere (this crate's diagnostics/prompt, or shoal-value's
/// renderer) still honors the user's `NO_COLOR` setting.
pub(crate) fn maybe_strip(s: impl Into<String>) -> String {
    let s = s.into();
    if no_color() { strip_ansi(&s) } else { s }
}

fn run() -> Result<(), String> {
    let code = real_main(std::env::args_os().skip(1).collect())?;
    std::process::exit(code);
}

fn real_main(args: Vec<OsString>) -> Result<i32, String> {
    match args::parse_args(args, io::stdin().is_terminal())? {
        Action::Help => {
            println!("{}", args::USAGE);
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
        Action::Interactive => repl::repl(),
        Action::Fmt { check, files } => args::fmt_command(check, files),
        Action::Doctor { json } => {
            let report = shoal_doctor::run(&shoal_doctor::Options::from_env());
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
                )
            } else {
                print!("{report}")
            }
            Ok(report.exit_code())
        }
        Action::Companion(name) => args::run_companion(name),
        Action::Completions(shell) => {
            print!("{}", args::completion_script(&shell)?);
            Ok(0)
        }
        Action::Prompt(action) => prompt::run(action),
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
    let loaded = shoal_config::load(&shoal_config::LoadOptions::discover(&cwd))?;
    for warning in &loaded.warnings {
        eprintln!(
            "{}",
            maybe_strip(format!("\x1b[33;1mwarning:\x1b[0m config error: {warning}"))
        );
    }
    let mut evaluator = Evaluator::new(cwd);
    evaluator.interactive = interactive;
    // Engage the bundled adapter pack (+ any `adapters.dirs` the config
    // declares) on every non-interactive path too — see `adapters` module
    // doc comment for the defect this closes (`-c`/scripts ran raw system
    // commands instead of adapters).
    let (_, adapter_warnings) =
        adapters::load_adapters(&mut evaluator, &loaded.config.adapters.dirs);
    adapters::print_warnings(&adapter_warnings);
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
            // `exit`/`quit` in a script or `-c` exits the process with its code
            // and suppresses rendering of the (null) trailing value.
            if let Some(code) = evaluator.take_exit() {
                return Ok(code);
            }
            repl::render_result(&value, false).map_err(|e| format!("cannot write output: {e}"))?;
            Ok(0)
        }
        Err(error) => {
            report_eval_error(src, source, &error);
            Ok(error
                .status
                .filter(|code| (1..=255).contains(code))
                .unwrap_or(1))
        }
    }
}

pub(crate) fn format_parse_error(src: &str, source: Option<&Path>, error: &ParseError) -> String {
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

pub(crate) fn report_eval_error(src: &str, source: Option<&Path>, error: &ErrorVal) {
    if let Some(stderr) = &error.stderr
        && !stderr.is_empty()
    {
        eprint!("{stderr}");
        if !stderr.ends_with('\n') {
            eprintln!();
        }
    }
    eprint!("{}", format_eval_error(src, source, error));
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
    // One contiguous colored span for the whole "name:line:col: kind: message"
    // header — no ANSI reset/switch *inside* the header text, so plain-text
    // assertions against it (and NO_COLOR stripping) both see it intact.
    let header = format!("{name}:{line_no}:{column}: {kind}: {message}");
    let header_styled = format!("\x1b[1;31m{header}\x1b[0m");
    let caret = format!("{}^", " ".repeat(column.saturating_sub(1)));
    let caret_styled = format!("\x1b[31;1m{caret}\x1b[0m");
    let mut rendered = format!("{header_styled}\n  {line}\n  {caret_styled}\n");
    if let Some(h) = hint {
        rendered.push_str(&format!("  \x1b[33mhint: {h}\x1b[0m\n"));
    }
    maybe_strip(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn no_color_strips_ansi_escapes_but_leaves_plain_text() {
        let colored = "\x1b[31;1merror:\x1b[0m bad thing";
        assert_eq!(strip_ansi(colored), "error: bad thing");
    }
}
