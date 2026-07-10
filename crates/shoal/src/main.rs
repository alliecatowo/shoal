use std::borrow::Cow;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use reedline::{
    DefaultCompleter, DefaultHinter, DefaultPrompt, DefaultPromptSegment,
    FileBackedHistory, Reedline, Signal, ValidationResult, Validator,
};
use shoal_eval::Evaluator;
use shoal_syntax::{ParseError, parse};
use shoal_value::{ErrorVal, Value};

mod highlight;
use highlight::ShoalHighlighter;

const USAGE: &str = "shoal 0.1.0\n\nUsage: shoal [OPTIONS] [SCRIPT]\n       shoal <fmt|doctor|lsp|mcp|completions> ...\n\nOptions:\n  -c, --command <SOURCE>  Evaluate source and exit\n  -h, --help              Print help\n  -V, --version           Print version\n\nDeveloper commands:\n  fmt [--check] [FILES]   Format .shl source (stdin when no files)\n  doctor [--json]         Diagnose the installation\n  lsp                     Run the language server companion\n  mcp                     Run the MCP companion\n  completions SHELL       Print bash, zsh, or fish completions";

enum Action {
    Command(String, Vec<OsString>),
    Script(PathBuf, Vec<OsString>),
    Stdin,
    Interactive,
    Help,
    Version,
    Fmt { check: bool, files: Vec<PathBuf> },
    Doctor { json: bool },
    Companion(&'static str),
    Completions(String),
}

fn main() {
    if let Err(error) = run() {
        eprintln!("\x1b[31;1merror:\x1b[0m {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let code = real_main(std::env::args_os().skip(1).collect())?;
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
        Action::Fmt { check, files } => fmt_command(check, files),
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
        Action::Companion(name) => run_companion(name),
        Action::Completions(shell) => {
            print!("{}", completion_script(&shell)?);
            Ok(0)
        }
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

fn fmt_command(check: bool, files: Vec<PathBuf>) -> Result<i32, String> {
    if files.is_empty() {
        let mut src = String::new();
        io::stdin()
            .read_to_string(&mut src)
            .map_err(|e| format!("cannot read stdin: {e}"))?;
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
        let src = fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
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
fn run_companion(name: &str) -> Result<i32, String> {
    let status = std::process::Command::new(name).status().map_err(|e| {
        format!("cannot launch `{name}`: {e}; install the companion binary or add it to PATH")
    })?;
    Ok(status.code().unwrap_or(1))
}
fn completion_script(shell: &str) -> Result<&'static str, String> {
    match shell {
        "bash" => Ok(
            "_shoal(){ COMPREPLY=( $(compgen -W 'fmt doctor lsp mcp completions --help --version --command' -- \"${COMP_WORDS[COMP_CWORD]}\") ); }\ncomplete -F _shoal shoal\n",
        ),
        "zsh" => Ok(
            "#compdef shoal\n_arguments '1:command:(fmt doctor lsp mcp completions)' '*:file:_files'\n",
        ),
        "fish" => Ok(
            "complete -c shoal -f -a 'fmt doctor lsp mcp completions'\ncomplete -c shoal -s c -l command -r\n",
        ),
        _ => Err(format!(
            "unsupported shell `{shell}`; expected bash, zsh, or fish"
        )),
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
            report_eval_error(src, source, &error);
            Ok(error
                .status
                .filter(|code| (1..=255).contains(code))
                .unwrap_or(1))
        }
    }
}

fn repl() -> Result<i32, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cannot determine cwd: {e}"))?;
    let loaded = shoal_config::load(&shoal_config::LoadOptions::discover(&cwd))?;
    for warning in &loaded.warnings {
        eprintln!("\x1b[33;1mwarning:\x1b[0m config error: {warning}");
    }
    let config = loaded.config;
    let mut evaluator = Evaluator::new(cwd);
    evaluator.interactive = true;
    for dir in &config.adapters.dirs {
        let (catalog, warnings) = shoal_adapters::AdapterCatalog::load_dir(dir);
        for warning in warnings {
            eprintln!("\x1b[33;1mwarning:\x1b[0m failed to load adapter: {warning}");
        }
        evaluator.set_adapters(catalog);
    }
    for init in &config.init.files {
        let src = fs::read_to_string(init)
            .map_err(|e| format!("cannot read init {}: {e}", init.display()))?;
        let program = parse(&src).map_err(|e| format!("init {}: {e}", init.display()))?;
        evaluator
            .eval_program(&program)
            .map_err(|e| format!("init {}: {e}", init.display()))?;
    }

    let completions = completion_candidates(evaluator.cwd());
    let mut editor = Reedline::create()
        .use_bracketed_paste(config.editor.bracketed_paste)
        .with_validator(Box::new(ShoalValidator))
        .with_completer(Box::new(DefaultCompleter::new_with_wordlen(
            completions.clone(),
            1,
        )))
        .with_highlighter(Box::new(ShoalHighlighter))
        .with_hinter(Box::new(DefaultHinter::default()));
    if config.history.enabled
        && let Some(path) = config.history.path.clone().or_else(history_path)
    {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(history) = FileBackedHistory::with_file(config.history.max_entries, path) {
            editor = editor.with_history(Box::new(history));
        }
    }

    loop {
        let prompt_text = config
            .prompt
            .template
            .replace("{cwd}", &short_cwd(evaluator.cwd()));
        let prompt = DefaultPrompt::new(
            DefaultPromptSegment::Basic(prompt_text),
            DefaultPromptSegment::Empty,
        );
        match editor.read_line(&prompt) {
            Ok(Signal::Success(src)) => {
                if src.trim().is_empty() {
                    continue;
                }
                match shoal_syntax::parse_with_scope(&src, evaluator.env.visible_names()) {
                    Ok(program) => match evaluator.eval_program(&program) {
                        Ok(value) => {
                            if let Err(error) = render_result(&value, true) {
                                eprintln!("\x1b[31;1merror:\x1b[0m cannot write output: {error}");
                            }
                        }
                        Err(error) => report_eval_error(&src, None, &error),
                    },
                    Err(error) => eprint!("{}", format_parse_error(&src, None, &error)),
                }
            }
            Ok(Signal::CtrlC) => {
                println!("\x1b[90m^C\x1b[0m");
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
    // External commands executed interactively stream their output to the
    // PTY tee. Rendering them here would duplicate the command's output.
    if pty_was_live && matches!(value, Value::Outcome(_)) {
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
    format!("\x1b[32;1mshoal\x1b[0m \x1b[36;1m{}\x1b[0m\x1b[95m{}\x1b[0m", display.display(), git_suffix(cwd))
}

fn completion_candidates(cwd: &Path) -> Vec<String> {
    let mut values: std::collections::BTreeSet<String> = [
        "let", "var", "fn", "alias", "use", "export", "return", "break", "continue", "if", "else",
        "match", "for", "in", "while", "try", "catch", "true", "false", "null", "cd", "pwd", "ls",
        "echo", "run", "spawn", "parallel", "jobs", "history",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten().take(4000) {
                    if let Some(name) = entry.file_name().to_str() {
                        values.insert(name.into());
                    }
                }
            }
        }
    }
    if let Ok(entries) = fs::read_dir(cwd) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                values.insert(if entry.path().is_dir() {
                    format!("{name}/")
                } else {
                    name.into()
                });
            }
        }
    }
    values.into_iter().collect()
}

fn git_suffix(cwd: &Path) -> String {
    let output = std::process::Command::new("git")
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    match output {
        Ok(out) if out.status.success() => {
            format!(" ({})", String::from_utf8_lossy(&out.stdout).trim())
        }
        _ => String::new(),
    }
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

fn report_eval_error(src: &str, source: Option<&Path>, error: &ErrorVal) {
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
    let name_styled = format!("\x1b[36m{name}:{line_no}:{column}\x1b[0m");
    let kind_styled = format!("\x1b[31;1m{kind}\x1b[0m");
    let msg_styled = format!("\x1b[1m{message}\x1b[0m");
    let mut rendered = format!(
        "{name_styled}: {kind_styled}: {msg_styled}\n  {line}\n  \x1b[31;1m{}^\x1b[0m\n",
        " ".repeat(column - 1)
    );
    if let Some(h) = hint {
        rendered.push_str(&format!("  \x1b[33;1mhint:\x1b[0m \x1b[33m{h}\x1b[0m\n"));
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

    #[test]
    fn completion_catalog_has_language_and_filesystem_context() {
        let values = completion_candidates(Path::new("."));
        assert!(values.iter().any(|v| v == "match"));
        assert!(values.iter().any(|v| v == "shoal" || v == "cargo"));
    }

    #[test]
    fn developer_subcommands_dispatch() {
        assert!(matches!(
            parse_args(vec!["fmt".into(), "--check".into(), "x.shl".into()], true).unwrap(),
            Action::Fmt { check: true, .. }
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
}
