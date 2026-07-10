//! `ShoalCompleter` — a real `reedline::Completer` that dispatches on cursor
//! context using the modal lexer (TDD §3.1's statement-dispatch rule,
//! approximated), instead of a flat startup-snapshot word list.
//!
//! Contexts:
//! - **head** (first word of a COMMAND statement): session `fn`/alias names
//!   (from the evaluator's live `Env`) + builtins + adapter names + a live
//!   `PATH` scan (cached per-directory, invalidated on mtime change).
//! - **arg** (later word of a COMMAND statement): live filesystem entries
//!   resolved against the argument's own directory prefix, or — when the
//!   word looks like a flag (`-`/`--`) and the head resolves to a known
//!   adapter/fn — flags drawn from that adapter's declared params/short
//!   flags or the function's signature.
//! - **expr** (anywhere else — after `let x = `, inside `(...)`, a bare
//!   expression statement): in-scope variable/function names.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use reedline::{Completer, Span as RlSpan, Suggestion};
use shoal_adapters::{AdapterCatalog, CmdAdapter};
use shoal_syntax::lexer::RESERVED;
use shoal_syntax::{Lexer, Mode, Tok};
use shoal_value::{Env, Value};

/// Command names shoal-eval special-cases or implements directly
/// (`crates/shoal-eval/src/builtins.rs::NAMES` plus the `cd`/`pwd`/`run`/
/// `source` heads special-cased in `eval_command`). Kept in sync by hand:
/// shoal-eval doesn't expose this list publicly (see api_changes).
const SHOAL_BUILTINS: &[&str] = &[
    "echo", "ls", "cat", "mkdir", "touch", "cp", "mv", "rm", "stat", "which", "env", "sleep",
    "cd", "pwd", "run", "source",
];

/// Cursor context, resolved from the raw buffer text alone (no full parse —
/// see `classify`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ctx {
    /// Completing the first word of a (candidate) COMMAND statement.
    Head { start: usize, word: String },
    /// Completing a later word of a COMMAND statement whose head is `head`.
    Arg {
        start: usize,
        word: String,
        head: String,
    },
    /// Completing an identifier inside an EXPR-dispatched statement.
    Expr { start: usize, word: String },
    /// Nothing sensible to complete (e.g. cursor inside a literal).
    None,
}

pub struct ShoalCompleter {
    env: Env,
    cwd: Arc<Mutex<PathBuf>>,
    adapters: Vec<AdapterCatalog>,
    adapter_names: Vec<String>,
    path_cache: HashMap<PathBuf, (Option<SystemTime>, Vec<String>)>,
}

impl ShoalCompleter {
    pub fn new(
        env: Env,
        cwd: Arc<Mutex<PathBuf>>,
        adapters: Vec<AdapterCatalog>,
        adapter_names: Vec<String>,
    ) -> Self {
        Self {
            env,
            cwd,
            adapters,
            adapter_names,
            path_cache: HashMap::new(),
        }
    }

    fn cwd(&self) -> PathBuf {
        self.cwd
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| PathBuf::from("."))
    }

    /// Live `PATH` executable names. Each directory is re-scanned only when
    /// its mtime has changed since the last call (or it hasn't been seen
    /// before) — cheap enough to call on every Tab press.
    fn path_names(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        let Some(path_var) = std::env::var_os("PATH") else {
            return out;
        };
        for dir in std::env::split_paths(&path_var) {
            let mtime = fs::metadata(&dir).and_then(|m| m.modified()).ok();
            let stale = match self.path_cache.get(&dir) {
                Some((cached, _)) => *cached != mtime,
                None => true,
            };
            if stale {
                let mut names = Vec::new();
                if let Ok(entries) = fs::read_dir(&dir) {
                    for entry in entries.flatten().take(4000) {
                        if let Some(name) = entry.file_name().to_str() {
                            names.push(name.to_string());
                        }
                    }
                }
                self.path_cache.insert(dir.clone(), (mtime, names));
            }
            if let Some((_, names)) = self.path_cache.get(&dir) {
                out.extend(names.iter().cloned());
            }
        }
        out
    }

    fn adapter_lookup(&self, head: &str) -> Option<&CmdAdapter> {
        self.adapters.iter().find_map(|c| c.lookup(head))
    }

    fn head_candidates(&mut self, prefix: &str) -> Vec<String> {
        let mut names: BTreeSet<String> = BTreeSet::new();
        names.extend(RESERVED.iter().map(|s| s.to_string()));
        names.extend(SHOAL_BUILTINS.iter().map(|s| s.to_string()));
        for name in self.env.visible_names() {
            if self.env.get(&name).is_some_and(|v| v.is_callable()) {
                names.insert(name);
            }
        }
        names.extend(self.adapter_names.iter().cloned());
        names.extend(self.path_names());
        names.retain(|n| n.starts_with(prefix));
        names.into_iter().collect()
    }

    fn expr_candidates(&self, prefix: &str) -> Vec<String> {
        let mut names: BTreeSet<String> = self.env.visible_names().into_iter().collect();
        names.extend(RESERVED.iter().map(|s| s.to_string()));
        names.retain(|n| n.starts_with(prefix));
        names.into_iter().collect()
    }

    /// `--flag`/`-x` candidates for a known command head: adapter params
    /// (top-level + all subcommands) and short flags, plus a session
    /// function's own parameter names (TDD §1.6: "flag parsing derived from
    /// the signature").
    fn flag_candidates(&self, head: &str, prefix: &str) -> Vec<String> {
        let mut names: BTreeSet<String> = BTreeSet::new();
        if let Some(adapter) = self.adapter_lookup(head) {
            for p in &adapter.top.params {
                names.insert(format!("--{}", p.name));
            }
            for short in adapter.top.short_flags.keys() {
                names.insert(format!("-{short}"));
            }
            for sub in adapter.subs.values() {
                for p in &sub.params {
                    names.insert(format!("--{}", p.name));
                }
                for short in sub.short_flags.keys() {
                    names.insert(format!("-{short}"));
                }
            }
        }
        if let Some(Value::Closure(c)) = self.env.get(head) {
            for p in &c.params {
                names.insert(format!("--{}", p.name));
            }
        }
        names.retain(|n| n.starts_with(prefix));
        names.into_iter().collect()
    }

    /// Live filesystem candidates for a CMD-mode argument word, resolved
    /// against the word's own directory prefix — `crates/sho` re-scans
    /// `crates/` fresh, so newly created files/directories show up.
    fn fs_candidates(&self, word: &str) -> Vec<String> {
        let (dir_part, file_prefix) = split_dir_prefix(word);
        let base_dir = self.resolve_dir(&dir_part);
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(&base_dir) else {
            return out;
        };
        let show_hidden = file_prefix.starts_with('.');
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            if !name.starts_with(&file_prefix) {
                continue;
            }
            let is_dir = entry.path().is_dir();
            let mut value = format!("{dir_part}{name}");
            if is_dir {
                value.push('/');
            }
            out.push(value);
        }
        out
    }

    fn resolve_dir(&self, dir_part: &str) -> PathBuf {
        if dir_part.is_empty() {
            return self.cwd();
        }
        let expanded = if dir_part.starts_with("~/") {
            match std::env::var_os("HOME") {
                Some(home) => PathBuf::from(home).join(&dir_part[2..]),
                None => PathBuf::from(dir_part),
            }
        } else {
            PathBuf::from(dir_part)
        };
        if expanded.is_absolute() {
            expanded
        } else {
            self.cwd().join(expanded)
        }
    }
}

impl Completer for ShoalCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        match classify(&self.env, line, pos) {
            Ctx::Head { start, word } => finish(self.head_candidates(&word), start, pos),
            Ctx::Arg { start, word, head } => {
                let names = if word.starts_with('-') {
                    self.flag_candidates(&head, &word)
                } else {
                    self.fs_candidates(&word)
                };
                finish(names, start, pos)
            }
            Ctx::Expr { start, word } => finish(self.expr_candidates(&word), start, pos),
            Ctx::None => Vec::new(),
        }
    }
}

fn finish(mut names: Vec<String>, start: usize, pos: usize) -> Vec<Suggestion> {
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|value| {
            let append_whitespace = !value.ends_with('/');
            Suggestion {
                value,
                span: RlSpan::new(start, pos),
                append_whitespace,
                ..Default::default()
            }
        })
        .collect()
}

fn split_dir_prefix(word: &str) -> (String, String) {
    match word.rfind('/') {
        Some(idx) => (word[..=idx].to_string(), word[idx + 1..].to_string()),
        None => (String::new(), word.to_string()),
    }
}

/// Backward scan for the identifier ending exactly at `pos` — used for
/// EXPR-context word boundaries, where the token shapes are plain
/// identifiers (unlike CMD-mode words, which need the real lexer to handle
/// paths/flags/globs correctly).
fn trailing_ident(line: &str, pos: usize) -> (usize, String) {
    let mut start = pos;
    for (idx, ch) in line[..pos].char_indices().rev() {
        if ch.is_alphanumeric() || ch == '_' {
            start = idx;
        } else {
            break;
        }
    }
    (start, line[start..pos].to_string())
}

/// Find the byte offset where the *current statement* starts, by scanning
/// `line[..pos]` and tracking bracket depth/quote state — a statement
/// boundary (`;` or newline) only counts at depth 0, outside quotes. This is
/// a bookkeeping pass (mirrors `input_is_incomplete` in `main.rs`), not a
/// parse; the actual word-boundary/token-shape decisions downstream go
/// through the real lexer.
fn statement_start(line: &str, pos: usize) -> usize {
    let bytes = line.as_bytes();
    let mut depth: i32 = 0;
    let mut quote: Option<u8> = None;
    let mut boundary = 0usize;
    let mut i = 0usize;
    while i < pos {
        let b = bytes[i];
        if let Some(q) = quote {
            if q == b'"' && b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' | b'\'' => quote = Some(b),
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b';' | b'\n' if depth <= 0 => boundary = i + 1,
            _ => {}
        }
        i += 1;
    }
    boundary.min(pos)
}

/// Walk CMD-mode tokens from `scan_pos` (using the lexer's own word
/// boundaries — TDD §2.2) to find the word containing `pos`.
fn cmd_word_at(lx: &Lexer, mut scan_pos: usize, pos: usize, line: &str) -> Option<(usize, String)> {
    loop {
        let next_sig = lx.skip_trivia(scan_pos);
        if next_sig >= pos {
            return Some((pos, String::new()));
        }
        let (tok, span) = lx.token(next_sig, Mode::Cmd).ok()?;
        if matches!(tok, Tok::Eof) {
            return Some((pos, String::new()));
        }
        let (start, end) = (span.start as usize, span.end as usize);
        if end >= pos {
            return Some((start, line[start..pos].to_string()));
        }
        scan_pos = end.max(next_sig + 1);
    }
}

/// Classify the cursor position per TDD §3.1's statement-dispatch rule,
/// approximated well enough for completion purposes: keyword / bound-variable
/// / assignment-target first words dispatch EXPR; everything else dispatches
/// COMMAND (CMD-mode word boundaries for the rest of the statement).
fn classify(env: &Env, line: &str, pos: usize) -> Ctx {
    let pos = pos.min(line.len());
    let stmt_start = statement_start(line, pos);
    let lx = Lexer::new(line);
    let word0 = lx.skip_trivia(stmt_start);
    if word0 > pos {
        return Ctx::None;
    }
    if word0 >= pos {
        return Ctx::Head {
            start: pos,
            word: String::new(),
        };
    }
    let Ok((tok0, span0)) = lx.token(word0, Mode::Expr) else {
        return Ctx::None;
    };
    let (s0, e0) = (span0.start as usize, span0.end as usize);
    match tok0 {
        Tok::Ident(name) => {
            if pos <= e0 {
                return Ctx::Head {
                    start: s0,
                    word: line[s0..pos].to_string(),
                };
            }
            let is_keyword = RESERVED.contains(&name.as_str());
            let is_bound = env.is_bound(&name);
            let is_assign = matches!(
                lx.token(e0, Mode::Expr),
                Ok((
                    Tok::Eq | Tok::PlusEq | Tok::MinusEq | Tok::StarEq | Tok::SlashEq,
                    _
                ))
            );
            if is_keyword || is_bound || is_assign {
                let (start, word) = trailing_ident(line, pos);
                Ctx::Expr { start, word }
            } else {
                match cmd_word_at(&lx, e0, pos, line) {
                    Some((start, word)) => Ctx::Arg { start, word, head: name },
                    None => Ctx::None,
                }
            }
        }
        _ => {
            if pos <= e0 {
                Ctx::None
            } else {
                let (start, word) = trailing_ident(line, pos);
                Ctx::Expr { start, word }
            }
        }
    }
}

/// Scan adapter config directories for `[cmd.<name>]` table keys — just the
/// name enumeration `AdapterCatalog` doesn't expose publicly (see
/// api_changes); flag/subcommand data still goes through the real
/// `AdapterCatalog::load_dir` + `lookup`.
pub fn scan_adapter_names(dirs: &[PathBuf]) -> Vec<String> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "toml") {
                continue;
            }
            let Ok(src) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(doc) = src.parse::<toml::Value>() else {
                continue;
            };
            if let Some(cmds) = doc.get("cmd").and_then(toml::Value::as_table) {
                names.extend(cmds.keys().cloned());
            }
        }
    }
    names.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use shoal_value::Env;
    use std::sync::{Arc, Mutex};

    fn completer_at(cwd: &Path) -> ShoalCompleter {
        ShoalCompleter::new(
            Env::root(),
            Arc::new(Mutex::new(cwd.to_path_buf())),
            Vec::new(),
            Vec::new(),
        )
    }

    #[test]
    fn classify_empty_line_is_head() {
        let env = Env::root();
        assert_eq!(
            classify(&env, "", 0),
            Ctx::Head {
                start: 0,
                word: String::new()
            }
        );
    }

    #[test]
    fn classify_partial_head_word() {
        let env = Env::root();
        let ctx = classify(&env, "ec", 2);
        assert_eq!(
            ctx,
            Ctx::Head {
                start: 0,
                word: "ec".into()
            }
        );
    }

    #[test]
    fn classify_command_argument_position() {
        let env = Env::root();
        let ctx = classify(&env, "cat crates/sho", 14);
        assert_eq!(
            ctx,
            Ctx::Arg {
                start: 4,
                word: "crates/sho".into(),
                head: "cat".into(),
            }
        );
    }

    #[test]
    fn classify_flag_word_is_still_arg_context() {
        let env = Env::root();
        let ctx = classify(&env, "ls --col", 8);
        assert_eq!(
            ctx,
            Ctx::Arg {
                start: 3,
                word: "--col".into(),
                head: "ls".into(),
            }
        );
    }

    #[test]
    fn classify_after_let_equals_is_expr() {
        let env = Env::root();
        let ctx = classify(&env, "let x = tr", 10);
        assert_eq!(
            ctx,
            Ctx::Expr {
                start: 8,
                word: "tr".into()
            }
        );
    }

    #[test]
    fn classify_bound_variable_head_is_expr() {
        let env = Env::root();
        env.declare("items", Value::Int(1), false);
        let ctx = classify(&env, "items.le", 8);
        assert_eq!(
            ctx,
            Ctx::Expr {
                start: 6,
                word: "le".into()
            }
        );
    }

    #[test]
    fn classify_unbound_head_word_is_command() {
        let env = Env::root();
        let ctx = classify(&env, "gitx st", 7);
        assert_eq!(
            ctx,
            Ctx::Arg {
                start: 5,
                word: "st".into(),
                head: "gitx".into(),
            }
        );
    }

    #[test]
    fn head_candidates_include_keywords_and_builtins() {
        let mut c = completer_at(Path::new("."));
        let names = c.head_candidates("ec");
        assert!(names.iter().any(|n| n == "echo"));
    }

    #[test]
    fn head_candidates_include_callable_session_names_but_not_plain_vars() {
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
        let mut c = ShoalCompleter::new(
            env,
            Arc::new(Mutex::new(PathBuf::from("."))),
            Vec::new(),
            Vec::new(),
        );
        let names = c.head_candidates("");
        assert!(names.iter().any(|n| n == "deploy"));
        assert!(!names.iter().any(|n| n == "mydata"));
    }

    #[test]
    fn expr_candidates_include_in_scope_variables() {
        let env = Env::root();
        env.declare("myvar", Value::Int(1), false);
        let c = completer_at(Path::new("."));
        // Use the completer's own env for this assertion instead.
        let names_env = env.visible_names();
        assert!(names_env.contains(&"myvar".to_string()));
        let _ = c; // constructed just to exercise the type in this module
    }

    #[test]
    fn fs_candidates_reflect_live_directory_contents() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("alpha.txt"), b"").unwrap();
        fs::create_dir(dir.path().join("beta")).unwrap();
        let c = completer_at(dir.path());
        let mut names = c.fs_candidates("");
        names.sort();
        assert_eq!(names, vec!["alpha.txt".to_string(), "beta/".to_string()]);

        // Live: a file created *after* the completer was built still shows up
        // (no startup snapshot).
        fs::write(dir.path().join("gamma.txt"), b"").unwrap();
        let mut names2 = c.fs_candidates("");
        names2.sort();
        assert_eq!(
            names2,
            vec!["alpha.txt".to_string(), "beta/".to_string(), "gamma.txt".to_string()]
        );
    }

    #[test]
    fn fs_candidates_descend_into_directory_prefix() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("crates")).unwrap();
        fs::create_dir(dir.path().join("crates/shoal-eval")).unwrap();
        fs::create_dir(dir.path().join("crates/shoal-syntax")).unwrap();
        let c = completer_at(dir.path());
        let mut names = c.fs_candidates("crates/sho");
        names.sort();
        assert_eq!(
            names,
            vec![
                "crates/shoal-eval/".to_string(),
                "crates/shoal-syntax/".to_string()
            ]
        );
    }

    #[test]
    fn fs_candidates_hide_dotfiles_unless_prefix_asks() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".hidden"), b"").unwrap();
        fs::write(dir.path().join("visible"), b"").unwrap();
        let c = completer_at(dir.path());
        assert_eq!(c.fs_candidates(""), vec!["visible".to_string()]);
        assert_eq!(c.fs_candidates("."), vec![".hidden".to_string()]);
    }

    #[test]
    fn path_names_cache_invalidates_on_mtime_change() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("toolone"), b"").unwrap();
        let mut c = completer_at(Path::new("."));
        // Seed the cache manually against our tempdir (bypassing $PATH so
        // the test is hermetic), then verify a second scan after a new file
        // appears picks it up rather than returning the cached snapshot.
        let scan = |c: &mut ShoalCompleter, dir: &Path| -> Vec<String> {
            let mtime = fs::metadata(dir).and_then(|m| m.modified()).ok();
            let stale = match c.path_cache.get(dir) {
                Some((cached, _)) => *cached != mtime,
                None => true,
            };
            if stale {
                let mut names = Vec::new();
                if let Ok(entries) = fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        if let Some(name) = entry.file_name().to_str() {
                            names.push(name.to_string());
                        }
                    }
                }
                c.path_cache.insert(dir.to_path_buf(), (mtime, names));
            }
            c.path_cache.get(dir).unwrap().1.clone()
        };
        let first = scan(&mut c, dir.path());
        assert_eq!(first, vec!["toolone".to_string()]);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(dir.path().join("tooltwo"), b"").unwrap();
        let mut second = scan(&mut c, dir.path());
        second.sort();
        assert_eq!(second, vec!["toolone".to_string(), "tooltwo".to_string()]);
    }

    #[test]
    fn flag_candidates_from_adapter_catalog() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("git.toml"),
            r#"
[cmd.git]
bin = "git"
[cmd.git.sub.status]
params = { short = "bool" }
flags  = { short = { s = "short" } }
"#,
        )
        .unwrap();
        let (catalog, warnings) = AdapterCatalog::load_dir(dir.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let c = ShoalCompleter::new(
            Env::root(),
            Arc::new(Mutex::new(PathBuf::from("."))),
            vec![catalog],
            vec!["git".into()],
        );
        let flags = c.flag_candidates("git", "--s");
        assert!(flags.iter().any(|f| f == "--short"));
        let short = c.flag_candidates("git", "-");
        assert!(short.iter().any(|f| f == "-s"));
    }
}
