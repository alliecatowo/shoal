use std::borrow::Cow;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use reedline::{
    ColumnarMenu, DefaultHinter, Emacs, FileBackedHistory, KeyCode, KeyModifiers, MenuBuilder,
    Reedline, ReedlineEvent, ReedlineMenu, Signal, ValidationResult, Validator,
    default_emacs_keybindings,
};
use shoal_ast::{CmdArg, Expr, Program, Stmt, UnOp};
use shoal_eval::Evaluator;
use shoal_journal::{Journal, JournalQuery};
use shoal_syntax::{ParseCtx, ParseError, parse, parse_with_ctx};
use shoal_value::{Env, ErrorVal, Value};

mod completer;
mod highlight;
mod prompt;
use completer::ShoalCompleter;
use highlight::ShoalHighlighter;

const USAGE: &str = "shoal 0.1.0\n\nUsage: shoal [OPTIONS] [SCRIPT]\n       shoal <fmt|doctor|lsp|mcp|completions|prompt> ...\n\nOptions:\n  -c, --command <SOURCE>  Evaluate source and exit\n  -h, --help              Print help\n  -V, --version           Print version\n\nDeveloper commands:\n  fmt [--check] [FILES]   Format .shl source (stdin when no files)\n  doctor [--json]         Diagnose the installation\n  lsp                     Run the language server companion\n  mcp                     Run the MCP companion\n  completions SHELL       Print bash, zsh, or fish completions\n  prompt explain|bench|print [--side left|right|continuation|transient] [--n N]";

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
    Prompt(prompt::PromptAction),
}

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
fn no_color() -> bool {
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
fn maybe_strip(s: impl Into<String>) -> String {
    let s = s.into();
    if no_color() { strip_ansi(&s) } else { s }
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
        Action::Prompt(action) => prompt::run(action),
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
            // `exit`/`quit` in a script or `-c` exits the process with its code
            // and suppresses rendering of the (null) trailing value.
            if let Some(code) = evaluator.take_exit() {
                return Ok(code);
            }
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
        eprintln!(
            "{}",
            maybe_strip(format!("\x1b[33;1mwarning:\x1b[0m config error: {warning}"))
        );
    }
    let config = loaded.config;
    let mut evaluator = Evaluator::new(cwd.clone());
    evaluator.interactive = true;
    evaluator.set_statement_sink(Box::new(|v: &Value| {
        let _ = print_value(v);
    }));

    // Install the command journal (TDD §9): without one, `undo`/`journal`/
    // `history` are inert (no journal means nothing is ever recorded). Open a
    // SECOND, independent handle on the exact same on-disk store (SQLite/WAL
    // supports concurrent handles fine) purely to read back each statement's
    // entry id right after it runs — that is how this host builds the
    // `out[n] -> journal entry id` map `undo out[n]` needs (docs/ROADMAP.md
    // R3): the evaluator's own journal handle is private, and `out` itself is
    // just a plain REPL-side list of past values with no tie to entry ids.
    let state_dir = shoal_state_dir();
    let journal_reader = match (Journal::open(&state_dir), Journal::open(&state_dir)) {
        (Ok(write_handle), Ok(read_handle)) => {
            evaluator.set_journal(write_handle, REPL_SESSION, REPL_PRINCIPAL);
            Some(read_handle)
        }
        (Err(e), _) | (_, Err(e)) => {
            eprintln!(
                "{}",
                maybe_strip(format!(
                    "\x1b[33;1mwarning:\x1b[0m journal unavailable ({e}); undo/journal/history disabled this session"
                ))
            );
            None
        }
    };
    // Parallels `out`'s growth 1:1 (one push per successful `record_transcript`
    // call below): `out_entries[n]` is the journal entry id (if any) the
    // statement that produced `out[n]` recorded.
    let mut out_entries: Vec<Option<i64>> = Vec::new();

    let mut catalogs = Vec::new();
    for dir in &config.adapters.dirs {
        let (catalog, warnings) = shoal_adapters::AdapterCatalog::load_dir(dir);
        for warning in warnings {
            eprintln!(
                "{}",
                maybe_strip(format!(
                    "\x1b[33;1mwarning:\x1b[0m failed to load adapter: {warning}"
                ))
            );
        }
        evaluator.set_adapters(catalog.clone());
        catalogs.push(catalog);
    }
    let adapter_names = completer::scan_adapter_names(&config.adapters.dirs);

    for init in &config.init.files {
        let src = fs::read_to_string(init)
            .map_err(|e| format!("cannot read init {}: {e}", init.display()))?;
        let program = parse(&src).map_err(|e| format!("init {}: {e}", init.display()))?;
        evaluator
            .eval_program(&program)
            .map_err(|e| format!("init {}: {e}", init.display()))?;
    }

    // Ctrl-C must not kill the shell (TDD §4.7): install a real SIGINT
    // handler so the OS's default "terminate" disposition never fires while
    // a statement is executing (reedline's own `Signal::CtrlC` only covers
    // Ctrl-C pressed *while typing*, before Enter — the terminal is back in
    // cooked/ISIG mode by the time `eval_program` runs). The handler just
    // forwards to whichever `CancelToken` is currently active; `eval_program`
    // (and the exec layer under it) observe cancellation cooperatively and
    // unwind to an error instead of the process dying.
    let cancel_slot = Arc::new(Mutex::new(evaluator.cancellation_token()));
    if let Ok(mut signals) =
        signal_hook::iterator::Signals::new([signal_hook::consts::signal::SIGINT])
    {
        let slot = cancel_slot.clone();
        std::thread::spawn(move || {
            for _ in signals.forever() {
                if let Ok(token) = slot.lock() {
                    token.cancel();
                }
            }
        });
    }

    let cwd_cell = Arc::new(Mutex::new(evaluator.cwd().to_path_buf()));
    let completer = ShoalCompleter::new(
        evaluator.env.clone(),
        cwd_cell.clone(),
        catalogs,
        adapter_names,
    );
    let mut keybindings = default_emacs_keybindings();
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );
    // Build the shoal-prompt pipeline: load + layer the prompt config, resolve
    // the static session facts once, and set up the shared snapshot cell that
    // the loop refreshes per command and reedline reads per keystroke (zero
    // I/O on the render path — the whole point, design §0/§1).
    let (prompt_config, prompt_warnings) = prompt::load_prompt_config(&cwd);
    for warning in &prompt_warnings {
        eprintln!(
            "{}",
            maybe_strip(format!("\x1b[33;1mwarning:\x1b[0m {warning}"))
        );
    }
    let static_facts = prompt::StaticFacts::resolve(&prompt_config, no_color());
    let transient_enabled = prompt_config.transient.enabled;
    let (renderer, renderer_warnings) = shoal_prompt::Renderer::new(prompt_config);
    for warning in &renderer_warnings {
        eprintln!(
            "{}",
            maybe_strip(format!("\x1b[33;1mwarning:\x1b[0m {warning}"))
        );
    }
    let renderer = Arc::new(renderer);
    let shared_ctx: prompt::SharedCtx = Arc::new(RwLock::new(Arc::new(
        shoal_prompt::PromptContext::empty(cwd.clone()),
    )));
    let shoal_prompt = prompt::ShoalPrompt::new(renderer.clone(), shared_ctx.clone(), false);

    let mut editor = Reedline::create()
        .use_bracketed_paste(config.editor.bracketed_paste)
        .with_validator(Box::new(ShoalValidator))
        .with_completer(Box::new(completer))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(
            ColumnarMenu::default().with_name("completion_menu"),
        )))
        .with_edit_mode(Box::new(Emacs::new(keybindings)))
        .with_highlighter(Box::new(ShoalHighlighter))
        .with_hinter(Box::new(DefaultHinter::default()));
    if transient_enabled {
        // Transient prompt (§2.5): a second ShoalPrompt sharing the same cache,
        // rendering `format.transient` post-Enter. Reedline invokes it at the
        // right moment; no custom repaint logic on our side.
        editor = editor.with_transient_prompt(Box::new(prompt::ShoalPrompt::new(
            renderer.clone(),
            shared_ctx.clone(),
            true,
        )));
    }
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
        // Keep the completer's cwd view and the cancel handler's active
        // token fresh for the statement about to run.
        if let Ok(mut cell) = cwd_cell.lock() {
            *cell = evaluator.cwd().to_path_buf();
        }
        evaluator.reset_cancel();
        if let Ok(mut token) = cancel_slot.lock() {
            *token = evaluator.cancellation_token();
        }

        // Refresh the frozen prompt snapshot once, here, between commands —
        // never inside reedline's per-keystroke render (design §0.3, §2.3).
        let width = u16::try_from(terminal_width()).unwrap_or(80);
        let ctx = prompt::build_context(&mut evaluator, &static_facts, width);
        if let Ok(mut cell) = shared_ctx.write() {
            *cell = Arc::new(ctx);
        }
        match editor.read_line(&shoal_prompt) {
            Ok(Signal::Success(src)) => {
                if src.trim().is_empty() {
                    continue;
                }
                // `fg <task>` (docs/ROADMAP.md R3): host-level sugar, resolved
                // as plain source text before parsing since the evaluator has
                // no `fg` builtin of its own — see `rewrite_fg`.
                let run_src = rewrite_fg(&src).unwrap_or_else(|| src.clone());
                let ctx = parse_ctx_for(&evaluator.env);
                match parse_with_ctx(&run_src, ctx) {
                    Ok(mut program) => {
                        // `undo out[n]` (docs/ROADMAP.md R3): rewrite a literal
                        // `out[n]` undo target into its recorded entry id so it
                        // resolves via the existing `undo <id>` path.
                        resolve_out_undo(&mut program, &out_entries);
                        let started_ns = now_ns();
                        match evaluator.eval_program(&program) {
                            Ok(value) => {
                                let entry_id = journal_reader.as_ref().and_then(|journal| {
                                    latest_entry_id(journal, REPL_PRINCIPAL, started_ns)
                                });
                                out_entries.push(entry_id);
                                evaluator.record_transcript(&value);
                                if let Err(error) = render_result(&value, true) {
                                    eprintln!(
                                        "{}",
                                        maybe_strip(format!(
                                            "\x1b[31;1merror:\x1b[0m cannot write output: {error}"
                                        ))
                                    );
                                }
                                // `exit`/`quit` ends the REPL cleanly with its code,
                                // mirroring the Ctrl-D path (defect: no exit).
                                if let Some(code) = evaluator.take_exit() {
                                    return Ok(code);
                                }
                            }
                            Err(error) => report_eval_error(&run_src, None, &error),
                        }
                    }
                    Err(error) => eprint!("{}", format_parse_error(&run_src, None, &error)),
                }
            }
            Ok(Signal::CtrlC) => {
                println!("{}", maybe_strip("\x1b[90m^C\x1b[0m".to_string()));
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

/// Principal/session recorded on the REPL's own journal entries (TDD §9). A
/// fixed, stable pair — the interactive REPL is always exactly one local
/// human session — so `latest_entry_id`'s `principal` filter is deterministic.
const REPL_PRINCIPAL: &str = "human";
const REPL_SESSION: &str = "default";

/// The per-user state dir the journal lives in. Mirrors `shoal-eval`'s
/// private `default_state_dir()` exactly (used internally by
/// `Evaluator::open_default_journal`, which this REPL does not call — it
/// needs a *second* independent handle on the store instead, see `repl`)
/// so both handles, and the kernel's own journal, agree on one on-disk
/// journal per user.
fn shoal_state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("shoal")
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as i64
}

/// The newest `principal`-recorded journal entry at or after `since_ns` — the
/// entry the statement just evaluated created, best-effort: only a *second*
/// writer under the same principal appending in that narrow window (a second
/// concurrent interactive session, say) could misattribute this, which is an
/// acceptable bound for the single-user interactive REPL.
fn latest_entry_id(journal: &Journal, principal: &str, since_ns: i64) -> Option<i64> {
    let rows = journal
        .query(&JournalQuery {
            since_ts_ns: Some(since_ns),
            principal: Some(principal.to_string()),
            limit: 1,
            ..Default::default()
        })
        .ok()?;
    rows.first().map(|row| row.id)
}

/// `undo out[N]` resolution (docs/ROADMAP.md R3). The evaluator's `undo`
/// builtin only ever accepts a bare journal entry id (`undo 12`) — per its
/// own doc comment, "`out[n]` addressing is a REPL/host concern (the
/// evaluator has no out→entry map)". `out` itself is just a plain,
/// REPL-populated list of past values (`record_transcript`) with no tie to
/// journal entry ids; only this host knows that mapping (`out_entries`,
/// built alongside `record_transcript` in the REPL loop). This rewrites a
/// literal `undo out[N]` (or `undo out[-N]`) in the freshly-parsed program
/// into `undo <entry_id>` so it resolves via the existing `undo <id>` path.
/// Any other shape — bare `undo`, `undo 12`, a non-literal index, an index
/// with no recorded entry — is left untouched and falls through to the
/// eval's existing behavior/diagnostics.
fn resolve_out_undo(program: &mut Program, out_entries: &[Option<i64>]) {
    for stmt in &mut program.stmts {
        let Stmt::Expr {
            expr: Expr::Cmd { call, .. },
            ..
        } = stmt
        else {
            continue;
        };
        if call.head != "undo" || call.args.len() != 1 {
            continue;
        }
        let Some(n) = out_index_literal(&call.args[0]) else {
            continue;
        };
        let idx = if n >= 0 {
            n as usize
        } else {
            let Some(idx) = out_entries.len().checked_sub((-n) as usize) else {
                continue;
            };
            idx
        };
        let Some(Some(entry_id)) = out_entries.get(idx) else {
            continue;
        };
        let span = call.args[0].span();
        call.args[0] = CmdArg::Expr {
            expr: Expr::Int {
                value: *entry_id,
                span,
            },
            span,
        };
    }
}

/// The literal integer index `N` out of a `CmdArg` shaped like `out[N]` /
/// `out[-N]` — `None` for anything else (a non-literal index, a receiver
/// other than the `out` variable, or a differently-shaped argument).
fn out_index_literal(arg: &CmdArg) -> Option<i64> {
    let CmdArg::Expr {
        expr: Expr::Index { recv, index, .. },
        ..
    } = arg
    else {
        return None;
    };
    let Expr::Var { name, .. } = recv.as_ref() else {
        return None;
    };
    if name != "out" {
        return None;
    }
    match index.as_ref() {
        Expr::Int { value, .. } => Some(*value),
        Expr::Unary {
            op: UnOp::Neg,
            expr,
            ..
        } => match expr.as_ref() {
            Expr::Int { value, .. } => Some(-*value),
            _ => None,
        },
        _ => None,
    }
}

/// `fg <task>` (docs/ROADMAP.md R3): re-front a background task. There is no
/// `fg` builtin in the evaluator — task lifecycle methods (`.suspend()` /
/// `.resume()`) are a `shoal-eval` addition this wave (per docs/ROADMAP.md
/// R3's task-lifecycle decision); `fg` itself is host-level sugar that
/// combines them. Recognized only as the exact shape `fg <name>` (a single
/// bare identifier, presumed bound to a task value, e.g. `let t = spawn {
/// … }&` then `fg t`) and rewritten to `<name>.resume()\n<name>.await()`
/// *before* the normal parse/eval path — so the resumed task's result
/// renders, journals, and lands in `out[]` exactly like any other line.
/// Anything else (`fg` with no argument, a real `fg` on `PATH`, `fgrep`, …)
/// passes through unchanged.
///
/// Adapter note for integration: this calls `.resume()`/`.await()` by NAME —
/// if the eval sibling's task-lifecycle methods land under different names,
/// only this rewrite's method names need updating; the plumbing (rewrite →
/// normal parse/eval/render/journal path) does not change. Until
/// `.resume()` exists, `fg` surfaces the eval's own "no such method" error
/// through the ordinary error-reporting path — never a silent no-op.
fn rewrite_fg(src: &str) -> Option<String> {
    let trimmed = src.trim();
    let after = trimmed.strip_prefix("fg")?;
    if !after.starts_with(|c: char| c.is_ascii_whitespace()) {
        return None;
    }
    let name = after.trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !first_ok || !rest_ok {
        return None;
    }
    Some(format!("{name}.resume()\n{name}.await()"))
}

/// Build the parser's dispatch context (WP1's `ParseCtx`) from the live
/// session `Env`: value-bindings (`let`/`var`) dispatch EXPR (so REPL `it`/
/// `out` — themselves plain value bindings via `record_transcript` — resolve
/// as variables), callables (`fn`/`alias`) dispatch CMD.
fn parse_ctx_for(env: &Env) -> ParseCtx {
    let mut value_bound = Vec::new();
    let mut cmd_bound = Vec::new();
    for name in env.visible_names() {
        match env.get(&name) {
            Some(v) if v.is_callable() => cmd_bound.push(name),
            Some(_) => value_bound.push(name),
            None => {}
        }
    }
    ParseCtx {
        repl: true,
        value_bound,
        cmd_bound,
    }
}

fn render_result(value: &Value, pty_was_live: bool) -> io::Result<()> {
    // Skip re-rendering only outcomes whose bytes ACTUALLY reached the real
    // terminal via the PtyTee passthrough (`streamed`). Rendering those again
    // would duplicate the command's output. Builtins (echo/ls/…) and captured
    // outcomes stream nothing, so they carry `streamed == false` and must still
    // render their `.out` here (defect #1).
    if pty_was_live
        && let Value::Outcome(o) = value
        && o.streamed
    {
        return Ok(());
    }
    print_value(value)
}

/// Render one value the same colorized way the top-level REPL result is
/// rendered — shared by `render_result` and the statement sink (WP2) so
/// non-final statement values inside a multi-statement line get the same
/// live, colorized treatment as the line's final result.
fn print_value(value: &Value) -> io::Result<()> {
    let rendered = shoal_value::render::render_block(value, terminal_width());
    if !rendered.is_empty() {
        println!("{}", maybe_strip(rendered));
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
    fn parse_ctx_splits_values_from_callables() {
        let env = Env::root();
        env.declare("mydata", Value::Int(3), false);
        env.declare(
            "deploy",
            Value::CmdRef(Arc::new(shoal_ast::CmdCall {
                head: "echo".into(),
                forced: false,
                env_prefix: Vec::new(),
                args: Vec::new(),
                redirects: Vec::new(),
                background: false,
                trailing: None,
                span: shoal_ast::Span::new(0, 0),
            })),
            false,
        );
        let ctx = parse_ctx_for(&env);
        assert!(ctx.repl);
        assert!(ctx.value_bound.iter().any(|n| n == "mydata"));
        assert!(ctx.cmd_bound.iter().any(|n| n == "deploy"));
        assert!(!ctx.cmd_bound.iter().any(|n| n == "mydata"));
    }

    #[test]
    fn no_color_strips_ansi_escapes_but_leaves_plain_text() {
        let colored = "\x1b[31;1merror:\x1b[0m bad thing";
        assert_eq!(strip_ansi(colored), "error: bad thing");
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
