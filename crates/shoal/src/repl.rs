//! The interactive REPL: the `read_line` loop itself, its parse-context and
//! result-rendering helpers, the `undo out[n]`/`fg` source rewrites, and the
//! signal-handling + reedline/prompt wiring that only the interactive path
//! needs.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use reedline::{
    ColumnarMenu, DefaultHinter, EditMode, Emacs, FileBackedHistory, History, HistoryItem,
    HistoryItemId, HistorySessionId, KeyCode, KeyModifiers, MenuBuilder, Reedline, ReedlineEvent,
    ReedlineMenu, SearchDirection, SearchQuery, Signal, ValidationResult, Validator, Vi,
    default_emacs_keybindings, default_vi_insert_keybindings, default_vi_normal_keybindings,
};
use shoal_ast::{CmdArg, Expr, Program, Stmt, UnOp};
use shoal_eval::Evaluator;
use shoal_journal::{Journal, JournalQuery};
use shoal_syntax::{ParseCtx, parse, parse_with_ctx};
use shoal_value::{Env, Value};

use crate::completer::{self, ShoalCompleter};
use crate::highlight::ShoalHighlighter;
use crate::prompt;
use crate::{format_parse_error, maybe_strip, no_color, report_eval_error};

pub(crate) fn repl() -> Result<i32, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cannot determine cwd: {e}"))?;
    let loaded = shoal_config::load(&shoal_config::LoadOptions::discover(&cwd))?;
    // Before anything else prints: feed `render.color` into `no_color()` so
    // even these very warnings honor a `render.color = false` in `shoal.toml`
    // (docs/CONFIG.md §5/§6), the same way `NO_COLOR` already does.
    crate::apply_render_color_config(loaded.config.render.color);
    for warning in &loaded.warnings {
        eprintln!(
            "{}",
            maybe_strip(format!("\x1b[33;1mwarning:\x1b[0m config error: {warning}"))
        );
    }
    let config = loaded.config;
    let mut evaluator = Evaluator::new(cwd.clone());
    evaluator.interactive = true;
    // Wire the user reef scope (docs/REEF.md §1) exactly like `run_source`
    // does (crate::reef_user_manifest_path). The REPL builds its own
    // `Evaluator` and was missing this call entirely — without it,
    // `~/.config/shoal/shoal.toml`'s `[reef]` table engaged for `-c`/scripts
    // but never for the interactive shell. Additive: an absent/empty file is
    // exactly today's no-user-scope behavior.
    if let Some(path) = crate::reef_user_manifest_path() {
        evaluator.set_reef_user_manifest(path);
    }
    // `[aliases]`/`[env]` (docs/CONFIG.md §5): same seeding `run_source` does,
    // before any init file or typed input runs.
    crate::seed_config_bindings(&mut evaluator, &config);
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

    // Bundled pack + any `adapters.dirs` the config declares — the SAME
    // sequence `-c`/script-file runs load (`crate::adapters`), so the REPL
    // and every other path agree on one adapter catalog.
    let (catalogs, adapter_warnings) =
        crate::adapters::load_adapters(&mut evaluator, &config.adapters.dirs);
    crate::adapters::print_warnings(&adapter_warnings);
    let adapter_names =
        completer::scan_adapter_names(&crate::adapters::name_scan_dirs(&config.adapters.dirs));

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
    )
    .configure(
        config.completion.fuzzy,
        config.completion.case_insensitive,
        config.completion.max_results,
    );
    // `editor.keybindings` (docs/CONFIG.md §5): parse `chord -> action`
    // strings into real reedline bindings, warning (never failing) on
    // anything unrecognized.
    let (custom_bindings, keybinding_warnings) =
        crate::keybindings::parse_bindings(&config.editor.keybindings);
    for warning in &keybinding_warnings {
        eprintln!(
            "{}",
            maybe_strip(format!("\x1b[33;1mwarning:\x1b[0m {warning}"))
        );
    }
    let edit_mode = build_edit_mode(&config, &custom_bindings);
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
        // `completion.menu` (docs/CONFIG.md §5): `false` asks for cycle-only
        // completion rather than the interactive popup. reedline has no
        // separate non-menu completion path (the `Completer` trait is only
        // ever driven through the `ReedlineMenu` system), but it exposes
        // exactly this pair of knobs for the "no popup, just complete"
        // experience: a unique match is inserted immediately
        // (`quick_completions`) and multiple matches complete their shared
        // prefix in place rather than opening the dropdown
        // (`partial_completions`) — the popup still appears only when
        // several candidates share no common prefix, since at that point
        // there is nothing else reedline can do with them.
        .with_quick_completions(!config.completion.menu)
        .with_partial_completions(!config.completion.menu)
        .with_edit_mode(edit_mode)
        .with_highlighter(Box::new(ShoalHighlighter::with_env(evaluator.env.clone())))
        .with_hinter(Box::new(DefaultHinter::default()))
        // `history.ignore_space` (docs/CONFIG.md §5, classic
        // `HISTCONTROL=ignorespace`): reedline has this exact knob built in.
        .with_history_exclusion_prefix(if config.history.ignore_space {
            Some(" ".to_string())
        } else {
            None
        });
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
            // `history.dedup`/`history.ignore` (docs/CONFIG.md §5):
            // `FileBackedHistory` has no built-in filtering, so wrap it in a
            // thin `History` adapter that applies both before ever calling
            // through to `save`.
            let history = FilteredHistory::new(
                Box::new(history),
                config.history.dedup,
                config.history.ignore.clone(),
            );
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

/// Build the reedline edit mode for `config.editor.mode` (docs/CONFIG.md
/// §5): `"emacs"` or `"vi"` — shoal-config's semantic validation already
/// rejects anything else (§4), but this defensively falls back to emacs for
/// any other value rather than panicking, since a `Config` can also be built
/// directly (tests, an embedder) bypassing that validation. Tab always
/// drives the completion menu regardless of mode. `[editor.keybindings]`
/// custom chords (§5) are layered on top of whichever mode's own default
/// table(s) are in play — both the insert and normal tables in vi mode,
/// since the config schema draws no per-mode distinction.
fn build_edit_mode(
    config: &shoal_config::Config,
    custom: &[crate::keybindings::ParsedBinding],
) -> Box<dyn EditMode> {
    let tab_event = ReedlineEvent::UntilFound(vec![
        ReedlineEvent::Menu("completion_menu".to_string()),
        ReedlineEvent::MenuNext,
    ]);
    if config.editor.mode == "vi" {
        let mut insert = default_vi_insert_keybindings();
        let mut normal = default_vi_normal_keybindings();
        insert.add_binding(KeyModifiers::NONE, KeyCode::Tab, tab_event);
        for b in custom {
            insert.add_binding(b.modifiers, b.code, b.event.clone());
            normal.add_binding(b.modifiers, b.code, b.event.clone());
        }
        Box::new(Vi::new(insert, normal))
    } else {
        let mut kb = default_emacs_keybindings();
        kb.add_binding(KeyModifiers::NONE, KeyCode::Tab, tab_event);
        for b in custom {
            kb.add_binding(b.modifiers, b.code, b.event.clone());
        }
        Box::new(Emacs::new(kb))
    }
}

/// `history.dedup`/`history.ignore` (docs/CONFIG.md §5): a `History` adapter
/// that wraps a real backend (here, `FileBackedHistory`) and filters what
/// actually reaches `save` — neither knob has any built-in support in
/// reedline's history backends. Every other `History` method delegates
/// straight through; only `save` has filtering logic.
struct FilteredHistory {
    inner: Box<dyn History>,
    dedup: bool,
    ignore: Vec<String>,
    last_recorded: Option<String>,
}

impl FilteredHistory {
    fn new(inner: Box<dyn History>, dedup: bool, ignore: Vec<String>) -> Self {
        // Seed from the most recent entry already on disk (if any), so
        // `dedup` also catches "identical to the last line of the *previous*
        // session" on a fresh process start, not just within this session.
        let last_recorded = inner
            .search(SearchQuery::everything(SearchDirection::Backward, None))
            .ok()
            .and_then(|rows| rows.into_iter().next())
            .map(|item| item.command_line);
        Self {
            inner,
            dedup,
            ignore,
            last_recorded,
        }
    }

    fn should_skip(&self, line: &str) -> bool {
        if self.dedup && self.last_recorded.as_deref() == Some(line) {
            return true;
        }
        self.ignore.iter().any(|pattern| glob_match(pattern, line))
    }
}

impl History for FilteredHistory {
    fn save(&mut self, h: HistoryItem) -> reedline::Result<HistoryItem> {
        if self.should_skip(&h.command_line) {
            // Pretend it was handled without actually persisting a
            // duplicate/ignored entry — the caller (`Reedline::submit_buffer`)
            // just records whatever id comes back for its own bookkeeping.
            return Ok(h);
        }
        self.last_recorded = Some(h.command_line.clone());
        self.inner.save(h)
    }
    fn load(&self, id: HistoryItemId) -> reedline::Result<HistoryItem> {
        self.inner.load(id)
    }
    fn count(&self, query: SearchQuery) -> reedline::Result<i64> {
        self.inner.count(query)
    }
    fn search(&self, query: SearchQuery) -> reedline::Result<Vec<HistoryItem>> {
        self.inner.search(query)
    }
    fn update(
        &mut self,
        id: HistoryItemId,
        updater: &dyn Fn(HistoryItem) -> HistoryItem,
    ) -> reedline::Result<()> {
        self.inner.update(id, updater)
    }
    fn clear(&mut self) -> reedline::Result<()> {
        self.inner.clear()
    }
    fn delete(&mut self, h: HistoryItemId) -> reedline::Result<()> {
        self.inner.delete(h)
    }
    fn sync(&mut self) -> io::Result<()> {
        self.inner.sync()
    }
    fn session(&self) -> Option<HistorySessionId> {
        self.inner.session()
    }
}

/// Minimal shell-glob matcher for `history.ignore` patterns (docs/CONFIG.md
/// §5's `HISTIGNORE`-equivalent): `*` matches any run of characters
/// (including none), `?` matches exactly one; every other character matches
/// itself literally. `shoal-config` only carries the raw pattern strings
/// (its own doc comment: "matching semantics are the host's") — this is this
/// host's choice, deliberately the simplest thing that reads like a shell
/// pattern rather than a full regex engine.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    // Classic DP: `dp[i][j]` = pattern[..i] matches text[..j].
    let mut dp = vec![vec![false; txt.len() + 1]; pat.len() + 1];
    dp[0][0] = true;
    for i in 1..=pat.len() {
        if pat[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=pat.len() {
        for j in 1..=txt.len() {
            dp[i][j] = match pat[i - 1] {
                '*' => dp[i - 1][j] || dp[i][j - 1],
                '?' => dp[i - 1][j - 1],
                c => dp[i - 1][j - 1] && c == txt[j - 1],
            };
        }
    }
    dp[pat.len()][txt.len()]
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

pub(crate) fn render_result(value: &Value, pty_was_live: bool) -> io::Result<()> {
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
pub(crate) fn print_value(value: &Value) -> io::Result<()> {
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
    fn glob_match_supports_star_and_question_wildcards() {
        assert!(glob_match("ls*", "ls -la"));
        assert!(glob_match(
            "* --password=*",
            "curl --password=secret --url=x"
        ));
        assert!(glob_match("g?t status", "git status"));
        assert!(!glob_match("g?t status", "goat status"));
        assert!(glob_match("*", ""));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exactly"));
    }

    /// `history.dedup` (docs/CONFIG.md §5): a line identical to the
    /// immediately preceding one is skipped; a different line, or the same
    /// line after a different one in between, is recorded.
    #[test]
    fn filtered_history_dedup_skips_only_immediate_repeats() {
        let dir = tempfile::tempdir().unwrap();
        let inner = FileBackedHistory::with_file(100, dir.path().join("hist")).unwrap();
        let mut history = FilteredHistory::new(Box::new(inner), true, Vec::new());
        history.save(HistoryItem::from_command_line("ls")).unwrap();
        history.save(HistoryItem::from_command_line("ls")).unwrap();
        history.save(HistoryItem::from_command_line("pwd")).unwrap();
        history.save(HistoryItem::from_command_line("ls")).unwrap();
        let all = history
            .search(SearchQuery::everything(SearchDirection::Forward, None))
            .unwrap();
        let lines: Vec<&str> = all.iter().map(|i| i.command_line.as_str()).collect();
        assert_eq!(
            lines,
            vec!["ls", "pwd", "ls"],
            "the immediate repeat must be dropped, but a later repeat after a \
             different line must not be"
        );
    }

    /// `history.ignore` (docs/CONFIG.md §5, `HISTIGNORE`-equivalent): a line
    /// matching any pattern is never recorded.
    #[test]
    fn filtered_history_ignore_patterns_are_never_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let inner = FileBackedHistory::with_file(100, dir.path().join("hist")).unwrap();
        let mut history = FilteredHistory::new(
            Box::new(inner),
            false,
            vec!["ls*".to_string(), "secret *".to_string()],
        );
        history
            .save(HistoryItem::from_command_line("ls -la"))
            .unwrap();
        history
            .save(HistoryItem::from_command_line("secret reveal"))
            .unwrap();
        history
            .save(HistoryItem::from_command_line("echo kept"))
            .unwrap();
        let all = history
            .search(SearchQuery::everything(SearchDirection::Forward, None))
            .unwrap();
        assert_eq!(all.len(), 1, "only the non-matching line should persist");
        assert_eq!(all[0].command_line, "echo kept");
    }

    #[test]
    fn filtered_history_dedup_seeds_from_the_last_persisted_entry() {
        // A fresh `FilteredHistory` built over a backend that already has
        // entries (a new process attaching to an existing history file) must
        // still dedup against the *last* one, not just entries recorded in
        // this in-memory instance's own lifetime.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hist");
        {
            let mut inner = FileBackedHistory::with_file(100, path.clone()).unwrap();
            inner.save(HistoryItem::from_command_line("ls")).unwrap();
            inner.sync().unwrap();
        }
        let inner = FileBackedHistory::with_file(100, path).unwrap();
        let mut history = FilteredHistory::new(Box::new(inner), true, Vec::new());
        history.save(HistoryItem::from_command_line("ls")).unwrap();
        let all = history
            .search(SearchQuery::everything(SearchDirection::Forward, None))
            .unwrap();
        assert_eq!(
            all.len(),
            1,
            "the repeat of the last-persisted line must be deduped"
        );
    }

    /// `editor.mode` (docs/CONFIG.md §5): `"vi"` selects reedline's `Vi` edit
    /// mode, anything else (including the default `"emacs"`) selects `Emacs`.
    #[test]
    fn build_edit_mode_selects_vi_or_emacs_from_config() {
        let mut config = shoal_config::Config::default();
        config.editor.mode = "vi".to_string();
        let vi_mode = build_edit_mode(&config, &[]);
        assert!(matches!(
            vi_mode.edit_mode(),
            reedline::PromptEditMode::Vi(_)
        ));

        config.editor.mode = "emacs".to_string();
        let emacs_mode = build_edit_mode(&config, &[]);
        assert_eq!(emacs_mode.edit_mode(), reedline::PromptEditMode::Emacs);
    }

    /// `editor.keybindings` (docs/CONFIG.md §5): a custom chord actually
    /// fires its configured action through the real `EditMode::parse_event`
    /// path, in both emacs and vi-insert mode.
    #[test]
    fn build_edit_mode_applies_custom_bindings() {
        use crossterm::event::{Event, KeyEvent};

        let custom = vec![crate::keybindings::ParsedBinding {
            modifiers: KeyModifiers::CONTROL,
            code: KeyCode::Char('g'),
            event: ReedlineEvent::ClearScreen,
        }];
        let raw_event = || -> reedline::ReedlineRawEvent {
            Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL))
                .try_into()
                .unwrap()
        };

        let config = shoal_config::Config::default();
        let mut emacs_mode = build_edit_mode(&config, &custom);
        assert_eq!(
            emacs_mode.parse_event(raw_event()),
            ReedlineEvent::ClearScreen
        );

        let mut vi_config = shoal_config::Config::default();
        vi_config.editor.mode = "vi".to_string();
        let mut vi_mode = build_edit_mode(&vi_config, &custom);
        assert_eq!(vi_mode.parse_event(raw_event()), ReedlineEvent::ClearScreen);
    }
}
