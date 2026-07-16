use std::borrow::Cow;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use shoal_eval::Evaluator;
use shoal_syntax::{ParseError, parse};
use shoal_value::{ErrorVal, Value};

mod adapters;
mod args;
mod completer;
mod highlight;
mod keybindings;
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

/// Set once at startup from the loaded config's `render.color` (docs/CONFIG.md
/// §5/§6) — `shoal_config::load` already folds `NO_COLOR`/`SHOAL_RENDER_COLOR`
/// into that value (§3), so this is the one flag `no_color()` needs to also
/// honor a plain `render.color = false` in `shoal.toml` with no env var
/// involved. `false` (color enabled) until [`apply_render_color_config`] runs.
static CONFIG_COLOR_DISABLED: AtomicBool = AtomicBool::new(false);

/// Shared test-only lock serializing EVERY test in this bin that mutates or
/// reads process-global env (`NO_COLOR`/`XDG_CONFIG_HOME`/`HOME`). All such
/// tests — across `main`, `highlight`, and any other module — must hold this
/// single lock, so a setter in one module can't interleave with a
/// save-modify-restore in another and leak env state into a later assertion
/// (which flaked the highlighter color tests under parallel `--workspace` runs).
#[cfg(test)]
pub(crate) static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Feed the loaded config's color decision into `no_color()`. Call once,
/// right after loading config, before any colorized output — both `run_source`
/// and `repl()` do this immediately after `shoal_config::load`.
pub(crate) fn apply_render_color_config(color_enabled: bool) {
    CONFIG_COLOR_DISABLED.store(!color_enabled, Ordering::Relaxed);
}

/// NO_COLOR (https://no-color.org): disable ANSI escapes whenever the
/// variable is present at all, regardless of its value. Checked lazily on
/// every call rather than cached, so tests (and users) can flip it
/// mid-process. Also disabled when config's `render.color = false` has been
/// applied via [`apply_render_color_config`] (redundant with the env check
/// when `NO_COLOR` is what drove it — `shoal_config` already folds that in —
/// but keeps this function correct standalone for callers/tests that never
/// go through config loading at all).
pub(crate) fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some() || CONFIG_COLOR_DISABLED.load(Ordering::Relaxed)
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

/// The user-scope reef manifest path (docs/REEF.md §1: `[reef]` in
/// `shoal.toml` is the user scope): `$XDG_CONFIG_HOME/shoal/shoal.toml`,
/// falling back to `~/.config/shoal/shoal.toml` when unset. Mirrors
/// `shoal_config::LoadOptions::discover`'s own user-layer resolution and
/// `shoal_leash::Policy::user_leash_path`'s identical pattern for
/// `leash.toml`, so all three agree on one user config root. A missing file
/// is fine: `Evaluator::set_reef_user_manifest` tolerates an absent path (no
/// user scope, zero regression) exactly like the config/leash loaders do.
pub(crate) fn reef_user_manifest_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(dir).join("shoal").join("shoal.toml"));
    }
    std::env::var_os("HOME").filter(|s| !s.is_empty()).map(|h| {
        PathBuf::from(h)
            .join(".config")
            .join("shoal")
            .join("shoal.toml")
    })
}

/// Declare each `[aliases]`/`[env]` entry (docs/CONFIG.md §5) in `evaluator`
/// exactly as if the user had typed the equivalent session statement at
/// startup — `alias <name> = <target>` / `env.<NAME> = "<value>"`. Neither
/// has a dedicated seeding API on `Evaluator` (`Stmt::Alias` and the
/// `env.NAME = …` assignment form are the only paths that ever bind them —
/// see `shoal-eval/src/stmt.rs`), so this synthesizes and evaluates one
/// statement per entry: the simplest way to reuse the exact machinery a
/// typed statement goes through, per docs/CONFIG.md §6's integrator note. A
/// name/value that can't be expressed this way (e.g. an alias or env name
/// that isn't a valid identifier — config validation only requires
/// non-empty/no-whitespace, not identifier-shaped) never aborts startup: it
/// is reported as a warning, the same way a config-load warning is, and
/// simply skipped.
pub(crate) fn seed_config_bindings(evaluator: &mut Evaluator, config: &shoal_config::Config) {
    for (name, target) in &config.aliases {
        let src = format!("alias {name} = {target}\n");
        if let Err(message) = eval_seed_statement(evaluator, &src) {
            eprintln!(
                "{}",
                maybe_strip(format!(
                    "\x1b[33;1mwarning:\x1b[0m aliases.{name}: {message}"
                ))
            );
        }
    }
    for (name, value) in &config.env {
        let src = format!("env.{name} = {}\n", quote_shoal_string(value));
        if let Err(message) = eval_seed_statement(evaluator, &src) {
            eprintln!(
                "{}",
                maybe_strip(format!("\x1b[33;1mwarning:\x1b[0m env.{name}: {message}"))
            );
        }
    }
}

fn eval_seed_statement(evaluator: &mut Evaluator, src: &str) -> Result<(), String> {
    let program = parse(src).map_err(|e| e.msg)?;
    evaluator
        .eval_program(&program)
        .map(|_| ())
        .map_err(|e| e.msg)
}

/// Render `value` as a shoal double-quoted string literal that reproduces it
/// byte-for-byte once parsed back — escaping exactly the characters the
/// lexer's string scanner treats specially (`crates/shoal-syntax/src/lexer/
/// string.rs::escape`): `\`, `"`, `{`/`}` (interpolation sigils — an
/// unescaped `{` in a config value would otherwise splice in an expression),
/// plus the usual control-character escapes. Used to synthesize `env.NAME =
/// "…"` seed statements from arbitrary config-file text.
pub(crate) fn quote_shoal_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn run_source(
    src: &str,
    source: Option<&Path>,
    interactive: bool,
    args: Vec<OsString>,
) -> Result<i32, String> {
    // Config loads before the parse attempt (rather than after, as a syntax-
    // error fast path might suggest) specifically so `render.color = false`
    // governs even a *parse* error's colorized diagnostic — the very first
    // thing this function might print.
    let cwd = std::env::current_dir().map_err(|e| format!("cannot determine cwd: {e}"))?;
    let loaded = shoal_config::load(&shoal_config::LoadOptions::discover(&cwd))?;
    apply_render_color_config(loaded.config.render.color);
    for warning in &loaded.warnings {
        eprintln!(
            "{}",
            maybe_strip(format!("\x1b[33;1mwarning:\x1b[0m config error: {warning}"))
        );
    }
    let program = match parse(src) {
        Ok(program) => program,
        Err(error) => {
            eprint!("{}", format_parse_error(src, source, &error));
            return Ok(2);
        }
    };
    let mut evaluator = Evaluator::new(cwd);
    evaluator.interactive = interactive;
    // Wire the user reef scope (docs/REEF.md §1) so `~/.config/shoal/
    // shoal.toml`'s `[reef]` table actually engages — without this call the
    // documented user scope never exists in the real binary, no matter what
    // the user writes there (`Evaluator::set_reef_user_manifest` has no other
    // caller). Additive: an absent/empty file is exactly today's no-user-
    // scope behavior.
    if let Some(path) = reef_user_manifest_path() {
        evaluator.set_reef_user_manifest(path);
    }
    // `[aliases]`/`[env]` (docs/CONFIG.md §5): declare each the same way a
    // typed `alias name = cmd` / `env.NAME = "v"` statement would, before any
    // user source runs.
    seed_config_bindings(&mut evaluator, &loaded.config);
    // Render every non-final statement the same way the final result is
    // rendered (structured `.out` as a table, text verbatim), so a script's
    // intermediate and last statements look identical. Without this the
    // no-sink default renders intermediate outcomes as a compact inline blob
    // while the final one gets the full block treatment. The REPL installs
    // its own equivalent sink.
    evaluator.set_statement_sink(Box::new(|v: &Value| {
        let _ = repl::print_value(v);
    }));
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

    /// `render.color = false` from config must suppress ANSI the same way
    /// `NO_COLOR` does (docs/CONFIG.md §6), without the env var being set.
    /// Serialized against every other test that reads `no_color()`/`NO_COLOR`
    /// in this binary (shared with the `reef_user_manifest_path` env test).
    #[test]
    fn config_color_disabled_suppresses_ansi_like_no_color() {
        let _guard = crate::ENV_TEST_LOCK.lock().unwrap();
        let prev = std::env::var_os("NO_COLOR");
        unsafe { std::env::remove_var("NO_COLOR") };

        assert!(!no_color(), "color should be enabled by default");
        apply_render_color_config(false);
        assert!(no_color(), "render.color = false must disable color");
        assert_eq!(maybe_strip("\x1b[31;1mred\x1b[0m"), "red");
        apply_render_color_config(true);
        assert!(
            !no_color(),
            "restoring render.color = true re-enables color"
        );

        match prev {
            Some(v) => unsafe { std::env::set_var("NO_COLOR", v) },
            None => unsafe { std::env::remove_var("NO_COLOR") },
        }
    }

    #[test]
    fn quote_shoal_string_round_trips_through_the_real_lexer() {
        for value in [
            "plain",
            "has \"quotes\" and \\backslash\\",
            "brace {interp} braces",
            "line1\nline2\ttabbed\r",
            "",
        ] {
            let quoted = quote_shoal_string(value);
            let program = parse(&quoted).expect("quoted literal must parse");
            let shoal_ast::Stmt::Expr {
                expr: shoal_ast::Expr::Str { value: parsed, .. },
                ..
            } = &program.stmts[0]
            else {
                panic!("expected a single string-literal statement, got {program:?}");
            };
            assert_eq!(parsed, value, "quoting round-trip failed for {value:?}");
        }
    }

    #[test]
    fn seed_config_bindings_declares_aliases_and_env() {
        let cwd = std::env::current_dir().unwrap();
        let mut evaluator = Evaluator::new(cwd);
        let mut config = shoal_config::Config::default();
        config
            .aliases
            .insert("myalias".to_string(), "echo hi from alias".to_string());
        config
            .env
            .insert("MY_SHOAL_VAR".to_string(), "brace {x} value".to_string());
        seed_config_bindings(&mut evaluator, &config);

        let program = parse("myalias").unwrap();
        let value = evaluator.eval_program(&program).unwrap();
        // A bare command at statement position renders as an `Outcome`
        // (unlike a zero-arg lookup in value position, which returns the raw
        // value directly) — assert on its captured output instead of the
        // exact `Value` shape.
        let Value::Outcome(outcome) = &value else {
            panic!("expected an Outcome from the seeded alias, got {value:?}");
        };
        assert!(outcome.ok, "seeded alias command should have succeeded");
        assert!(
            String::from_utf8_lossy(&outcome.stdout).contains("hi from alias"),
            "alias should have expanded to the seeded `echo` command, got {value:?}"
        );

        let program = parse("env.MY_SHOAL_VAR").unwrap();
        let value = evaluator.eval_program(&program).unwrap();
        assert_eq!(value, Value::Str("brace {x} value".to_string()));
    }

    #[test]
    fn seed_config_bindings_warns_but_does_not_panic_on_unseedable_names() {
        // An alias/env name that isn't identifier-shaped (config validation
        // only requires non-empty/no-whitespace, not identifier-shaped)
        // can't be expressed as `alias <name> = …` / `env.<NAME> = …` text —
        // must degrade to a warning, never a panic or aborted startup.
        let cwd = std::env::current_dir().unwrap();
        let mut evaluator = Evaluator::new(cwd);
        let mut config = shoal_config::Config::default();
        config
            .aliases
            .insert("9bad".to_string(), "echo hi".to_string());
        seed_config_bindings(&mut evaluator, &config);
        // Must not have bound anything under that name, and must not panic.
        assert!(!evaluator.env.is_bound("9bad"));
    }

    /// Fix 1: the user reef manifest path mirrors `shoal_config`'s own user
    /// layer (`$XDG_CONFIG_HOME/shoal/shoal.toml`, falling back to
    /// `~/.config/shoal/shoal.toml`) — the exact path
    /// `Evaluator::set_reef_user_manifest` needs so the documented user
    /// `[reef]` scope actually engages. Serialized on a lock (rather than
    /// relying on test-harness single-threading) since it mutates process
    /// env vars that other tests in this binary could read concurrently.
    #[test]
    fn reef_user_manifest_path_prefers_xdg_then_home() {
        let _guard = crate::ENV_TEST_LOCK.lock().unwrap();
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_home = std::env::var_os("HOME");

        unsafe { std::env::set_var("XDG_CONFIG_HOME", "/xdg-cfg") };
        assert_eq!(
            reef_user_manifest_path(),
            Some(PathBuf::from("/xdg-cfg/shoal/shoal.toml"))
        );

        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        unsafe { std::env::set_var("HOME", "/home/shoaluser") };
        assert_eq!(
            reef_user_manifest_path(),
            Some(PathBuf::from("/home/shoaluser/.config/shoal/shoal.toml"))
        );

        match prev_xdg {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        match prev_home {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}
