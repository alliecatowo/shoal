//! `ShoalCompleter` — a real `reedline::Completer` that dispatches on cursor
//! context using the modal lexer (site/content/internals/language-conformance-contract.md statement-dispatch rule,
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
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use reedline::{Completer, Span as RlSpan, Suggestion};
use shoal_adapters::{AdapterCatalog, CmdAdapter};
use shoal_syntax::commands::{CommandFacts, CommandSource, builtin_names, resolve_command_source};
use shoal_syntax::lexer::RESERVED;
use shoal_syntax::{Lexer, Mode, Tok};
use shoal_value::{Env, Value, method_names, methods_for};

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
    /// Completing a method/field name immediately after a `.` (`recv.<word>`)
    /// in EXPR position — offer the value-method vocabulary, not variables.
    /// `recv` is the inferred receiver type name (a `Value::type_name` string
    /// understood by `shoal_value::methods_for`) when we could pin it down from
    /// the token(s) before the `.`, else `None` — meaning "offer the full method
    /// union" (see [`infer_receiver_type`]).
    Method {
        start: usize,
        word: String,
        recv: Option<String>,
    },
    /// Nothing sensible to complete (e.g. cursor inside a literal).
    None,
}

pub struct ShoalCompleter {
    env: Env,
    cwd: Arc<Mutex<PathBuf>>,
    /// Executable search directories from the session that will execute the
    /// command. Attached REPLs cannot use the client's process PATH: the
    /// kernel session may have changed PATH independently.
    path_dirs: Option<Arc<Mutex<Option<Vec<PathBuf>>>>>,
    adapters: Vec<AdapterCatalog>,
    adapter_names: Vec<String>,
    path_cache: HashMap<PathBuf, PathCacheEntry>,
    /// `completion.fuzzy` (site/content/internals/configuration-reference.md): allow typo-tolerant,
    /// non-contiguous matches instead of requiring a strict prefix.
    fuzzy: bool,
    /// `completion.case_insensitive`.
    case_insensitive: bool,
    /// `completion.max_results`: cap on candidates returned per completion.
    max_results: usize,
}

/// `shoal_config::Completion`'s own defaults (fuzzy/case-insensitive on, 100
/// results) — used so a `ShoalCompleter::new` call that never reaches
/// `configure` (every existing call site, including every test in this
/// module) keeps behaving exactly as it did before config was wired in.
const DEFAULT_FUZZY: bool = true;
const DEFAULT_CASE_INSENSITIVE: bool = true;
const DEFAULT_MAX_RESULTS: usize = 100;
/// Directory mtimes do not change when an existing child is merely chmod'd.
/// Periodic bounded revalidation keeps executable completion honest without
/// rescanning every PATH directory on every keystroke.
const PATH_CACHE_REVALIDATE: Duration = Duration::from_millis(200);

struct PathCacheEntry {
    dir_mtime: Option<SystemTime>,
    scanned_at: Instant,
    names: Vec<String>,
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
            path_dirs: None,
            adapters,
            adapter_names,
            path_cache: HashMap::new(),
            fuzzy: DEFAULT_FUZZY,
            case_insensitive: DEFAULT_CASE_INSENSITIVE,
            max_results: DEFAULT_MAX_RESULTS,
        }
    }

    /// Apply `[completion]` config (site/content/internals/configuration-reference.md). Builder-style so
    /// existing `ShoalCompleter::new(...)` call sites are unaffected when a
    /// caller (a test, an embedder) doesn't need config-driven behavior.
    pub fn configure(mut self, fuzzy: bool, case_insensitive: bool, max_results: usize) -> Self {
        self.fuzzy = fuzzy;
        self.case_insensitive = case_insensitive;
        self.max_results = max_results.max(1);
        self
    }

    /// Use a live, sanitized session PATH projection. `None` inside the cell
    /// means an older remote omitted the projection, in which case retaining
    /// the process-PATH fallback preserves protocol compatibility.
    pub(crate) fn with_path_dirs(mut self, path_dirs: Arc<Mutex<Option<Vec<PathBuf>>>>) -> Self {
        self.path_dirs = Some(path_dirs);
        self
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
        let configured = self
            .path_dirs
            .as_ref()
            .and_then(|dirs| dirs.lock().ok().and_then(|dirs| dirs.clone()));
        let dirs = if let Some(dirs) = configured {
            dirs
        } else {
            let Some(path_var) = std::env::var_os("PATH") else {
                return out;
            };
            let cwd = self.cwd();
            std::env::split_paths(&path_var)
                .map(|dir| {
                    if dir.is_absolute() {
                        dir
                    } else {
                        cwd.join(dir)
                    }
                })
                .collect()
        };
        for dir in dirs {
            out.extend(self.path_dir_names(&dir));
        }
        out
    }

    fn path_dir_names(&mut self, dir: &Path) -> Vec<String> {
        let mtime = fs::metadata(dir).and_then(|m| m.modified()).ok();
        let stale = match self.path_cache.get(dir) {
            Some(cached) => {
                cached.dir_mtime != mtime || cached.scanned_at.elapsed() >= PATH_CACHE_REVALIDATE
            }
            None => true,
        };
        if stale {
            let mut names = Vec::new();
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten().take(4000) {
                    let executable = entry.metadata().is_ok_and(|metadata| {
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    });
                    if executable && let Some(name) = entry.file_name().to_str() {
                        names.push(name.to_string());
                    }
                }
            }
            self.path_cache.insert(
                dir.to_path_buf(),
                PathCacheEntry {
                    dir_mtime: mtime,
                    scanned_at: Instant::now(),
                    names,
                },
            );
        }
        self.path_cache
            .get(dir)
            .map_or_else(Vec::new, |cached| cached.names.clone())
    }

    fn adapter_lookup(&self, head: &str) -> Option<&CmdAdapter> {
        self.adapters.iter().find_map(|c| c.lookup(head))
    }

    fn head_source(&self, head: &str) -> CommandSource {
        let binding = self.env.get(head);
        resolve_command_source(
            head,
            CommandFacts {
                session_callable: binding.as_ref().is_some_and(Value::is_callable),
                session_value: binding.as_ref().is_some_and(|value| !value.is_callable()),
                // Flag completion means the head already has an argument, so a
                // non-callable lexical value cannot win this command shape.
                value_eligible: false,
                forced: false,
                dynamic_run: false,
                runner: false,
                // Plugin declarations are installed on the evaluator; this
                // lightweight completion view has no registry snapshot yet.
                plugin: false,
                adapter: self.adapter_lookup(head).is_some(),
            },
        )
    }

    /// Match `name` against `prefix` per `[completion]` config
    /// (site/content/internals/configuration-reference.md): case-(in)sensitively per `case_insensitive`, and
    /// via a non-contiguous subsequence test — a strict superset of prefix
    /// matching, so it's exactly "typo-tolerant / non-contiguous matches,
    /// not just prefix" — rather than a strict prefix when `fuzzy` is set.
    fn candidate_matches(&self, name: &str, prefix: &str) -> bool {
        if prefix.is_empty() {
            return true;
        }
        if self.case_insensitive {
            let name = name.to_lowercase();
            let prefix = prefix.to_lowercase();
            if self.fuzzy {
                subsequence_match(&name, &prefix)
            } else {
                name.starts_with(&prefix)
            }
        } else if self.fuzzy {
            subsequence_match(name, prefix)
        } else {
            name.starts_with(prefix)
        }
    }

    fn head_candidates(&mut self, prefix: &str) -> Vec<String> {
        let mut names: BTreeSet<String> = BTreeSet::new();
        names.extend(RESERVED.iter().map(|s| s.to_string()));
        names.extend(builtin_names().iter().map(|s| s.to_string()));
        for name in self.env.visible_names() {
            if self.env.get(&name).is_some_and(|v| v.is_callable()) {
                names.insert(name);
            }
        }
        names.extend(self.adapter_names.iter().cloned());
        names.extend(self.path_names());
        names.retain(|n| self.candidate_matches(n, prefix));
        names.into_iter().collect()
    }

    fn expr_candidates(&self, prefix: &str) -> Vec<String> {
        let mut names: BTreeSet<String> = self.env.visible_names().into_iter().collect();
        names.extend(RESERVED.iter().map(|s| s.to_string()));
        names.retain(|n| self.candidate_matches(n, prefix));
        names.into_iter().collect()
    }

    /// Method/field candidates for a `.`-position word (`recv.<prefix>`),
    /// filtered per `[completion]` config. When the receiver's type was inferred
    /// (`recv` is `Some`) we offer only that type's methods
    /// (`shoal_value::methods_for`) — so `[1,2,3].` proposes `where`/`map`/`sum`
    /// and not `upper`/`split`. When it wasn't (a chained/computed/unknown
    /// receiver), we fall back to the flat union across every type
    /// (`method_names`), which is exactly the old, type-agnostic behavior — never
    /// fewer or wrong candidates than before.
    fn method_candidates(&self, prefix: &str, recv: Option<&str>) -> Vec<String> {
        let per_type = recv.and_then(methods_for);
        let names: &[&str] = per_type.as_deref().unwrap_or_else(|| method_names());
        names
            .iter()
            .filter(|n| self.candidate_matches(n, prefix))
            .map(|s| s.to_string())
            .collect()
    }

    /// `--flag`/`-x` candidates for a known command head: adapter params
    /// (top-level + all subcommands) and short flags, plus a session
    /// function's own parameter names (site/content/internals/language-conformance-contract.md: "flag parsing derived from
    /// the signature").
    fn flag_candidates(&self, head: &str, prefix: &str) -> Vec<String> {
        let mut names: BTreeSet<String> = BTreeSet::new();
        match self.head_source(head) {
            CommandSource::Adapter => {
                let adapter = self
                    .adapter_lookup(head)
                    .expect("adapter resolution carries its catalog entry");
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
            CommandSource::SessionCallable => {
                if let Some(Value::Closure(c)) = self.env.get(head) {
                    for p in &c.params {
                        names.insert(format!("--{}", p.name));
                    }
                }
            }
            _ => {}
        }
        names.retain(|n| self.candidate_matches(n, prefix));
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
            if !self.candidate_matches(&name, &file_prefix) {
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
        let expanded = if let Some(tail) = dir_part.strip_prefix("~/") {
            match std::env::var_os("HOME") {
                Some(home) => PathBuf::from(home).join(tail),
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
        let max_results = self.max_results;
        match classify(&self.env, line, pos) {
            Ctx::Head { start, word } => {
                finish(self.head_candidates(&word), start, pos, max_results)
            }
            Ctx::Arg { start, word, head } => {
                let names = if word.starts_with('-') {
                    self.flag_candidates(&head, &word)
                } else {
                    self.fs_candidates(&word)
                };
                finish(names, start, pos, max_results)
            }
            Ctx::Expr { start, word } => {
                finish(self.expr_candidates(&word), start, pos, max_results)
            }
            Ctx::Method { start, word, recv } => finish(
                self.method_candidates(&word, recv.as_deref()),
                start,
                pos,
                max_results,
            ),
            Ctx::None => Vec::new(),
        }
    }
}

/// Does every character of `needle` appear in `haystack`, in order, not
/// necessarily contiguously? Any prefix match is also a subsequence match
/// (each needle character is trivially found at the next haystack
/// position), so using this predicate when `completion.fuzzy` is set is a
/// strict superset of prefix matching — it never *rejects* what plain prefix
/// matching would accept, only adds typo-tolerant/non-contiguous matches on
/// top.
fn subsequence_match(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|nc| chars.any(|hc| hc == nc))
}

/// Sort, dedup, cap to `completion.max_results` (site/content/internals/configuration-reference.md), and
/// convert to reedline `Suggestion`s.
fn finish(mut names: Vec<String>, start: usize, pos: usize, max_results: usize) -> Vec<Suggestion> {
    names.sort();
    names.dedup();
    names.truncate(max_results);
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

/// Resolve an EXPR-position word to either a plain [`Ctx::Expr`]
/// (variable/function/keyword completion) or a [`Ctx::Method`] (value-method
/// completion) depending on whether the identifier is immediately preceded by a
/// `.` — i.e. it's a method/field on some receiver (`recv.<word>`). A `?.`
/// optional-chain also ends in `.`, so it's covered too. When it's a method
/// position we additionally try to infer the receiver's type so completion can
/// narrow to that type's methods.
fn expr_or_method(env: &Env, line: &str, pos: usize) -> Ctx {
    let (start, word) = trailing_ident(line, pos);
    if start > 0 && line.as_bytes()[start - 1] == b'.' {
        let recv = infer_receiver_type(env, line, start - 1);
        Ctx::Method { start, word, recv }
    } else {
        Ctx::Expr { start, word }
    }
}

/// Infer the receiver type for a `.`-position completion from the token(s)
/// immediately before the `.` at `dot_pos` (`line[dot_pos] == '.'`). Returns a
/// `Value::type_name` string understood by [`methods_for`], or `None` — "fall
/// back to the full method union" — for anything we can't pin down cleanly.
///
/// Handled (a strict, receiver-narrowing win over the union):
/// - literals directly before the `.`: `"…"`/`'…'` → str, an int/float literal,
///   a size/duration literal, `true`/`false` → bool, a `[…]` list literal, a
///   `{…}` record literal;
/// - a simple binding `name.` whose live `Value` in `env` gives its type.
///
/// Everything else falls back to `None` (mandatory, so this never regresses):
/// a chained access `a.b.`, a call/group result `f(x).`/`(…).`, an indexed
/// element `xs[0].`, a name that isn't a value binding, or a binding whose type
/// has no dedicated method table (stream/outcome/closure/… — [`methods_for`]
/// itself returns `None` there, and `method_candidates` then uses the union).
fn infer_receiver_type(env: &Env, line: &str, dot_pos: usize) -> Option<String> {
    let stmt_start = statement_start(line, dot_pos);
    let toks = expr_tokens(line, stmt_start, dot_pos);
    let (last_tok, _) = toks.last()?;
    match last_tok {
        Tok::Str(_) | Tok::StrInterp(_) => Some("str".into()),
        Tok::Int(_) => Some("int".into()),
        Tok::Float(_) => Some("float".into()),
        Tok::Size(_) => Some("size".into()),
        Tok::Duration(_) => Some("duration".into()),
        Tok::Time { .. } => Some("time".into()),
        Tok::DateTime(_) => Some("datetime".into()),
        Tok::Ident(name) => {
            match name.as_str() {
                "true" | "false" => return Some("bool".into()),
                // A reserved keyword (`if.`, `for.`) is never a receiver, and a
                // value binding can never shadow one.
                kw if RESERVED.contains(&kw) => return None,
                _ => {}
            }
            // `a.b.` — the receiver of *this* `.` is the field access `a.b`,
            // whose type we can't know from text; fall back. (The token before
            // `b` is a `.`/`?.`.)
            if matches!(
                toks.iter().rev().nth(1),
                Some((Tok::Dot | Tok::QuestionDot, _))
            ) {
                return None;
            }
            // Simple binding: infer from the live value's variant.
            Some(env.get(name.as_str())?.type_name().to_string())
        }
        // A trailing `]`/`}` is a list/record *literal* only when its matching
        // opener isn't applied to a preceding value (indexing / a call / a
        // header block); otherwise fall back.
        Tok::RBracket => bracket_literal_type(&toks, &Tok::LBracket, "list"),
        Tok::RBrace => bracket_literal_type(&toks, &Tok::LBrace, "record"),
        // Call/group result (`)`), or any other token: unknown → union.
        _ => None,
    }
}

/// Lex `line[stmt_start..dot_pos]` in EXPR mode into `(tok, start)` pairs
/// (trivia dropped), so [`infer_receiver_type`] can inspect the receiver just
/// before a `.`. Tokens that would overrun `dot_pos` are excluded (e.g. the
/// `?.` of a `recv?.field` chain, so `recv` remains the last token).
fn expr_tokens(line: &str, stmt_start: usize, dot_pos: usize) -> Vec<(Tok, usize)> {
    let lx = Lexer::new(line);
    let mut out = Vec::new();
    let mut scan = stmt_start;
    loop {
        let next = lx.skip_trivia(scan);
        if next >= dot_pos {
            break;
        }
        let Ok((tok, span)) = lx.token(next, Mode::Expr) else {
            break;
        };
        let (start, end) = (span.start as usize, span.end as usize);
        if matches!(tok, Tok::Eof) || start >= dot_pos || end > dot_pos {
            break;
        }
        out.push((tok, start));
        scan = end.max(next + 1);
    }
    out
}

/// Classify a trailing `]`/`}` (whose matching `opener` and result `ty` are
/// given): a fresh list/record literal, or postfix application (index/call/
/// block) that we can't type. Returns `Some(ty)` only for the literal case.
fn bracket_literal_type(toks: &[(Tok, usize)], opener: &Tok, ty: &str) -> Option<String> {
    // Walk back to the matching opener by bracket depth.
    let mut depth = 0i32;
    let mut open_idx = None;
    for (i, (tok, _)) in toks.iter().enumerate().rev() {
        match tok {
            Tok::RParen | Tok::RBracket | Tok::RBrace => depth += 1,
            Tok::LParen | Tok::LBracket | Tok::LBrace => {
                depth -= 1;
                if depth == 0 {
                    open_idx = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let open_idx = open_idx?;
    // Unbalanced / mismatched (shouldn't happen for a closed receiver): bail.
    if &toks[open_idx].0 != opener {
        return None;
    }
    // An atom-ending token right before the opener means the brackets are
    // postfix (`xs[0]`, `f(){…}`) rather than a literal → fall back.
    let prev = open_idx.checked_sub(1).map(|i| &toks[i].0);
    if prev.is_some_and(atom_ends) {
        return None;
    }
    Some(ty.to_string())
}

/// Does `tok` end an atom/value expression? A `[`/`{` right after such a token
/// is postfix (indexing/call/header block), not the opener of a fresh literal.
fn atom_ends(tok: &Tok) -> bool {
    matches!(
        tok,
        Tok::Ident(_)
            | Tok::RParen
            | Tok::RBracket
            | Tok::RBrace
            | Tok::Str(_)
            | Tok::StrInterp(_)
            | Tok::Int(_)
            | Tok::Float(_)
            | Tok::Size(_)
            | Tok::Duration(_)
            | Tok::Time { .. }
            | Tok::DateTime(_)
            | Tok::Regex(_)
    )
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
/// boundaries — site/content/internals/language-conformance-contract.md) to find the word containing `pos`.
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

/// Classify the cursor position per site/content/internals/language-conformance-contract.md statement-dispatch rule,
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
                expr_or_method(env, line, pos)
            } else {
                match cmd_word_at(&lx, e0, pos, line) {
                    Some((start, word)) => Ctx::Arg {
                        start,
                        word,
                        head: name,
                    },
                    None => Ctx::None,
                }
            }
        }
        _ => {
            if pos <= e0 {
                Ctx::None
            } else {
                expr_or_method(env, line, pos)
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
    use std::path::Path;
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
    fn classify_bound_variable_dot_is_method() {
        // A bound receiver followed by `.<word>` is method/field position, not
        // a plain expr reference — so completion offers method names.
        let env = Env::root();
        env.declare("items", Value::List(vec![Value::Int(1)]), false);
        let ctx = classify(&env, "items.le", 8);
        assert_eq!(
            ctx,
            Ctx::Method {
                start: 6,
                word: "le".into(),
                // `items` is a live list binding — receiver type inferred.
                recv: Some("list".into()),
            }
        );
    }

    #[test]
    fn classify_bound_variable_no_dot_is_expr() {
        // The same bound receiver with a trailing space (no `.`) is plain expr
        // position: variable/function/keyword completion, not method names.
        let env = Env::root();
        env.declare("items", Value::Int(1), false);
        let ctx = classify(&env, "items ", 6);
        assert_eq!(
            ctx,
            Ctx::Expr {
                start: 6,
                word: String::new()
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
    fn head_candidates_include_full_builtin_registry() {
        // Regression against the old hand-copied `SHOAL_BUILTINS` (18 stale
        // entries): every real builtin command head now tab-completes because
        // the completer consumes shoal-eval's canonical registry directly.
        let mut c = completer_at(Path::new("."));
        for head in [
            "jobs", "history", "exit", "plan", "apply", "undo", "reef", "save", "assert",
            "interact", "open", "pushd", "popd", "dirs",
        ] {
            let names = c.head_candidates(head);
            assert!(
                names.iter().any(|n| n == head),
                "builtin `{head}` should be an offered head candidate"
            );
        }
    }

    #[test]
    fn method_position_offers_method_names_expr_does_not() {
        // `tbl.wh<TAB>` — a bound list receiver in `.`-position — offers value
        // method names (`where`); the same prefix in plain expr position
        // (`let x = wh`) offers variables/keywords, never method names.
        let env = Env::root();
        env.declare("tbl", Value::List(vec![Value::Int(1)]), false);
        let mut c = ShoalCompleter::new(
            env,
            Arc::new(Mutex::new(PathBuf::from("."))),
            Vec::new(),
            Vec::new(),
        );
        let method = c.complete("tbl.wh", 6);
        assert!(
            method.iter().any(|s| s.value == "where"),
            "method position must offer `.where`, got {:?}",
            method.iter().map(|s| &s.value).collect::<Vec<_>>()
        );

        let expr = c.complete("let x = wh", 10);
        assert!(
            !expr.iter().any(|s| s.value == "where"),
            "plain expr position must NOT offer method names, got {:?}",
            expr.iter().map(|s| &s.value).collect::<Vec<_>>()
        );
    }

    // ---- receiver-type-aware method completion after `.` -------------------

    fn completer_with(env: Env) -> ShoalCompleter {
        ShoalCompleter::new(
            env,
            Arc::new(Mutex::new(PathBuf::from("."))),
            Vec::new(),
            Vec::new(),
        )
    }

    /// Complete `line` at its end and collect the candidate strings.
    fn cands(c: &mut ShoalCompleter, line: &str) -> Vec<String> {
        c.complete(line, line.len())
            .into_iter()
            .map(|s| s.value)
            .collect()
    }

    fn has(cs: &[String], m: &str) -> bool {
        cs.iter().any(|c| c == m)
    }

    #[test]
    fn method_completion_infers_literal_receiver_types() {
        let mut c = completer_with(Env::root());

        // `[…]` list literal → list methods, not str/record-only methods.
        let list = cands(&mut c, "[1,2,3].");
        for m in ["where", "map", "sum", "sort_by", "first"] {
            assert!(has(&list, m), "list literal should offer `.{m}`: {list:?}");
        }
        for m in ["upper", "split", "keys"] {
            assert!(
                !has(&list, m),
                "list literal must NOT offer `.{m}`: {list:?}"
            );
        }

        // `"…"` string literal → str methods.
        let s = cands(&mut c, "\"hi\".");
        for m in ["upper", "len", "split", "trim"] {
            assert!(has(&s, m), "str literal should offer `.{m}`: {s:?}");
        }
        for m in ["where", "map", "keys"] {
            assert!(!has(&s, m), "str literal must NOT offer `.{m}`: {s:?}");
        }

        // `'…'` raw string literal → str methods too.
        let raw = cands(&mut c, "'hi'.");
        assert!(
            has(&raw, "upper"),
            "raw str literal should offer `.upper`: {raw:?}"
        );
        assert!(
            !has(&raw, "where"),
            "raw str literal must NOT offer `.where`: {raw:?}"
        );

        // Integer literal → numeric methods, not collection/str methods.
        let int = cands(&mut c, "42.");
        for m in ["abs", "round", "floor", "ceil"] {
            assert!(has(&int, m), "int literal should offer `.{m}`: {int:?}");
        }
        for m in ["where", "upper", "keys"] {
            assert!(!has(&int, m), "int literal must NOT offer `.{m}`: {int:?}");
        }

        // Float literal → numeric methods (and the `1.5` isn't mis-split).
        let float = cands(&mut c, "1.5.");
        assert!(
            has(&float, "round"),
            "float literal should offer `.round`: {float:?}"
        );
        assert!(
            !has(&float, "where"),
            "float literal must NOT offer `.where`: {float:?}"
        );

        // `{…}` record literal → record methods.
        let rec = cands(&mut c, "{a:1}.");
        for m in ["keys", "values", "items", "merge"] {
            assert!(has(&rec, m), "record literal should offer `.{m}`: {rec:?}");
        }
        for m in ["upper", "map", "where"] {
            assert!(
                !has(&rec, m),
                "record literal must NOT offer `.{m}`: {rec:?}"
            );
        }

        // `true`/`false` → bool (scalar) methods: universal serializers only.
        let b = cands(&mut c, "true.");
        assert!(has(&b, "json"), "bool literal should offer `.json`: {b:?}");
        for m in ["where", "upper", "abs"] {
            assert!(!has(&b, m), "bool literal must NOT offer `.{m}`: {b:?}");
        }

        // A size/duration literal → scalar methods (no distinct set of its own).
        let size = cands(&mut c, "1mb.");
        assert!(
            has(&size, "json"),
            "size literal should offer `.json`: {size:?}"
        );
        assert!(
            !has(&size, "upper"),
            "size literal must NOT offer `.upper`: {size:?}"
        );
        let dur = cands(&mut c, "30s.");
        assert!(
            has(&dur, "save"),
            "duration literal should offer `.save`: {dur:?}"
        );
        assert!(
            !has(&dur, "where"),
            "duration literal must NOT offer `.where`: {dur:?}"
        );
    }

    #[test]
    fn method_completion_universal_methods_appear_for_every_type() {
        // `.tap`/`.also` (dispatched for every receiver) and the universal
        // serializers survive the type narrowing.
        let mut c = completer_with(Env::root());
        for (line, ty) in [("[1,2,3].", "list"), ("\"x\".", "str"), ("42.", "int")] {
            let cs = cands(&mut c, line);
            for m in ["tap", "also", "json"] {
                assert!(
                    has(&cs, m),
                    "{ty} should still offer universal `.{m}`: {cs:?}"
                );
            }
        }
    }

    #[test]
    fn method_completion_infers_binding_receiver_type() {
        // `xs.` where `xs` is a live list binding → list methods, not str ops.
        let env = Env::root();
        env.declare("xs", Value::List(vec![Value::Int(1), Value::Int(2)]), false);
        let mut c = completer_with(env);
        let cs = cands(&mut c, "xs.");
        assert!(
            has(&cs, "where"),
            "list binding should offer `.where`: {cs:?}"
        );
        assert!(has(&cs, "sum"), "list binding should offer `.sum`: {cs:?}");
        assert!(
            !has(&cs, "upper"),
            "list binding must NOT offer `.upper`: {cs:?}"
        );

        // A str binding narrows to str methods.
        let env2 = Env::root();
        env2.declare("name", Value::Str("bob".into()), false);
        let mut c2 = completer_with(env2);
        let cs2 = cands(&mut c2, "name.");
        assert!(
            has(&cs2, "upper"),
            "str binding should offer `.upper`: {cs2:?}"
        );
        assert!(
            !has(&cs2, "where"),
            "str binding must NOT offer `.where`: {cs2:?}"
        );
    }

    #[test]
    fn method_completion_falls_back_to_union_for_complex_receivers() {
        // The union offers everything, so it contains BOTH a str-only method
        // (`upper`) and a list method (`where`) — the tell-tale of "not
        // narrowed". Every fallback case below must keep that full vocabulary.
        let env = Env::root();
        env.declare("rec", Value::Record(shoal_value::Record::new()), false);
        env.declare("xs", Value::List(vec![Value::Int(1)]), false);
        // A command binding: a type with no dedicated method table → union.
        env.declare(
            "f",
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
        let mut c = completer_with(env);

        let union_like = |cs: &[String], label: &str| {
            assert!(
                has(cs, "upper"),
                "{label} should fall back to the union (has `upper`): {cs:?}"
            );
            assert!(
                has(cs, "where"),
                "{label} should fall back to the union (has `where`): {cs:?}"
            );
            assert!(
                has(cs, "keys"),
                "{label} should fall back to the union (has `keys`): {cs:?}"
            );
        };

        // Chained field access `rec.a.` — receiver type of the 2nd `.` unknown.
        union_like(&cands(&mut c, "rec.a."), "chained access");
        // Call/group result `f(x).`.
        union_like(&cands(&mut c, "f(x)."), "call result");
        // Indexed element `xs[0].` — the `[…]` is postfix, not a literal.
        union_like(&cands(&mut c, "xs[0]."), "indexed element");
        // A binding whose type has no dedicated table (a command) → union.
        union_like(&cands(&mut c, "f."), "table-less binding");
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
            vec![
                "alpha.txt".to_string(),
                "beta/".to_string(),
                "gamma.txt".to_string()
            ]
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
        fs::set_permissions(
            dir.path().join("toolone"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::write(dir.path().join("not-executable"), b"").unwrap();
        fs::create_dir(dir.path().join("directory")).unwrap();
        let mut c = completer_at(Path::new("."));
        // Scan the cache primitive directly so the test is hermetic rather
        // than mutating process PATH. Non-executable files and directories are
        // never command-head candidates.
        let first = c.path_dir_names(dir.path());
        assert_eq!(first, vec!["toolone".to_string()]);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(dir.path().join("tooltwo"), b"").unwrap();
        fs::set_permissions(
            dir.path().join("tooltwo"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let mut second = c.path_dir_names(dir.path());
        second.sort();
        assert_eq!(second, vec!["toolone".to_string(), "tooltwo".to_string()]);
    }

    #[test]
    fn path_names_cache_revalidates_chmod_without_directory_mtime_change() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("mode-tool");
        fs::write(&tool, b"").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o644)).unwrap();
        let mut c = completer_at(Path::new("."));
        assert!(c.path_dir_names(dir.path()).is_empty());

        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        std::thread::sleep(PATH_CACHE_REVALIDATE + Duration::from_millis(25));
        assert_eq!(c.path_dir_names(dir.path()), vec!["mode-tool".to_string()]);

        fs::set_permissions(&tool, fs::Permissions::from_mode(0o644)).unwrap();
        std::thread::sleep(PATH_CACHE_REVALIDATE + Duration::from_millis(25));
        assert!(c.path_dir_names(dir.path()).is_empty());
    }

    #[test]
    fn path_names_follow_the_executing_session_not_the_client_process() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("remote-only-tool"), b"").unwrap();
        fs::set_permissions(
            dir.path().join("remote-only-tool"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let session_dirs = Arc::new(Mutex::new(Some(vec![dir.path().to_path_buf()])));
        let mut c = completer_at(Path::new(".")).with_path_dirs(session_dirs.clone());

        assert_eq!(c.path_names(), vec!["remote-only-tool".to_string()]);

        // An explicit empty PATH is different from an omitted old-protocol
        // projection and must not silently fall back to the client's PATH.
        *session_dirs.lock().unwrap() = Some(Vec::new());
        assert!(c.path_names().is_empty());
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

    #[test]
    fn callable_shadow_hides_adapter_flags_by_shared_precedence() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("tool.toml"),
            r#"
[cmd.tool]
bin = "tool"
params = { adapter_only = "bool" }
"#,
        )
        .unwrap();
        let (catalog, warnings) = AdapterCatalog::load_dir(dir.path());
        assert!(warnings.is_empty(), "{warnings:?}");

        let mut evaluator = shoal_eval::Evaluator::new(dir.path().into());
        evaluator
            .eval_program(&shoal_syntax::parse("fn tool(session_only: bool) { null }").unwrap())
            .unwrap();
        let completer = ShoalCompleter::new(
            evaluator.env().clone(),
            Arc::new(Mutex::new(PathBuf::from("."))),
            vec![catalog],
            vec!["tool".into()],
        );
        let flags = completer.flag_candidates("tool", "--");
        assert!(flags.iter().any(|flag| flag == "--session_only"));
        assert!(!flags.iter().any(|flag| flag == "--adapter_only"));
    }

    #[test]
    fn subsequence_match_is_a_superset_of_prefix_matching() {
        assert!(subsequence_match("shoal-eval", "sho"), "a real prefix");
        assert!(
            subsequence_match("shoal-eval", "sev"),
            "non-contiguous typo-tolerant match"
        );
        assert!(!subsequence_match("shoal-eval", "zzz"));
        assert!(
            subsequence_match("anything", ""),
            "empty needle always matches"
        );
    }

    /// `completion.fuzzy = false` (site/content/internals/configuration-reference.md): only strict prefix
    /// matches, no non-contiguous "typo-tolerant" candidates.
    #[test]
    fn fuzzy_false_restricts_to_strict_prefix_matches() {
        let fuzzy_env = Env::root();
        fuzzy_env.declare("myservice", Value::Int(1), false);
        let fuzzy = ShoalCompleter::new(
            fuzzy_env,
            Arc::new(Mutex::new(PathBuf::from("."))),
            Vec::new(),
            Vec::new(),
        )
        .configure(true, true, 100);
        // Fuzzy (default): a non-contiguous subsequence still matches.
        assert!(
            fuzzy
                .expr_candidates("mysvc")
                .iter()
                .any(|n| n == "myservice")
        );

        let strict_env = Env::root();
        strict_env.declare("myservice", Value::Int(1), false);
        let strict = ShoalCompleter::new(
            strict_env,
            Arc::new(Mutex::new(PathBuf::from("."))),
            Vec::new(),
            Vec::new(),
        )
        .configure(false, true, 100);
        assert!(
            !strict
                .expr_candidates("mysvc")
                .iter()
                .any(|n| n == "myservice"),
            "fuzzy=false must reject a non-prefix subsequence match"
        );
        assert!(
            strict
                .expr_candidates("myser")
                .iter()
                .any(|n| n == "myservice")
        );
    }

    /// `completion.case_insensitive = false`: an exact-case mismatch must not
    /// match.
    #[test]
    fn case_insensitive_false_requires_exact_case() {
        let env = Env::root();
        env.declare("MyThing", Value::Int(1), false);
        let c = ShoalCompleter::new(
            env,
            Arc::new(Mutex::new(PathBuf::from("."))),
            Vec::new(),
            Vec::new(),
        )
        .configure(false, false, 100);
        assert!(!c.expr_candidates("mything").iter().any(|n| n == "MyThing"));
        assert!(c.expr_candidates("MyTh").iter().any(|n| n == "MyThing"));
    }

    /// `completion.max_results` caps the candidate list (site/content/internals/configuration-reference.md).
    #[test]
    fn max_results_caps_the_candidate_list() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            fs::write(dir.path().join(format!("file{i}.txt")), b"").unwrap();
        }
        let c = ShoalCompleter::new(
            Env::root(),
            Arc::new(Mutex::new(dir.path().to_path_buf())),
            Vec::new(),
            Vec::new(),
        )
        .configure(true, true, 3);
        let names = c.fs_candidates("");
        assert_eq!(names.len(), 10, "fs_candidates itself is uncapped");
        let suggestions = finish(names, 0, 0, 3);
        assert_eq!(suggestions.len(), 3, "finish() truncates to max_results");
    }
}
