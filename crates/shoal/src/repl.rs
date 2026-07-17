//! The interactive REPL: the `read_line` loop itself, its parse-context and
//! result-rendering helpers, the `undo out[n]`/`fg` source rewrites, and the
//! signal-handling + reedline/prompt wiring that only the interactive path
//! needs.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};

use reedline::{
    ColumnarMenu, DefaultHinter, FileBackedHistory, MenuBuilder, Reedline, ReedlineMenu, Signal,
};
#[cfg(test)]
use reedline::{
    History, HistoryItem, KeyCode, KeyModifiers, ReedlineEvent, SearchDirection, SearchQuery,
};
use shoal_ast::{CmdArg, Expr, Program, Stmt, UnOp};
use shoal_eval::Evaluator;
use shoal_journal::{Journal, JournalQuery};
use shoal_syntax::{ParseCtx, parse_with_ctx};
use shoal_value::{Env, Value};

use crate::completer::{self, ShoalCompleter};
use crate::highlight::ShoalHighlighter;
use crate::kernel_repl::{KernelRpc, ProtocolOutcome, ProtocolSession};
use crate::prompt;
use crate::repl_state::{ProtocolSnapshot, RemoteEnvMirror};
use crate::{format_parse_error, maybe_strip, no_color, report_eval_error};

mod editor;
mod rendering;

use editor::{FilteredHistory, ShoalValidator, build_edit_mode, history_path};
#[cfg(test)]
use editor::{glob_match, input_is_incomplete};

pub(crate) use rendering::{PagerContext, print_value, render_result, render_result_paged};
#[cfg(test)]
use rendering::{
    pager_command, protocol_render_text, should_page, spawn_pager, wrapped_line_count,
};
use rendering::{render_protocol_outcome, report_protocol_error, terminal_width};

trait ReplProtocol {
    fn execute(
        &mut self,
        src: &str,
        interrupt: &AtomicBool,
        width: usize,
    ) -> Result<ProtocolOutcome, String>;
    fn snapshot(&mut self) -> Result<serde_json::Value, String>;
}

impl<R: KernelRpc> ReplProtocol for ProtocolSession<R> {
    fn execute(
        &mut self,
        src: &str,
        interrupt: &AtomicBool,
        width: usize,
    ) -> Result<ProtocolOutcome, String> {
        ProtocolSession::execute(self, src, interrupt, width)
    }

    fn snapshot(&mut self) -> Result<serde_json::Value, String> {
        ProtocolSession::snapshot(self)
    }
}

fn protocol_requested(standalone: bool, kernel_enabled: bool) -> bool {
    !standalone && kernel_enabled
}

fn execute_protocol_line(
    session: &mut impl ReplProtocol,
    src: &str,
    interrupt: &AtomicBool,
    width: usize,
) -> Result<ProtocolOutcome, String> {
    if parse_job_control(src).is_some() {
        return Err("fg/bg process-group job control is available only with --standalone".into());
    }
    let run_src = rewrite_fg(src).unwrap_or_else(|| src.to_string());
    session.execute(&run_src, interrupt, width)
}

fn refresh_protocol_state(
    session: &mut impl ReplProtocol,
    mirror: &mut RemoteEnvMirror,
    env: &Env,
    cwd: &Arc<Mutex<PathBuf>>,
) -> Result<ProtocolSnapshot, String> {
    let snapshot = ProtocolSnapshot::parse(session.snapshot()?)?;
    mirror.apply(&snapshot, env, cwd);
    Ok(snapshot)
}

pub(crate) fn repl(standalone: bool) -> Result<i32, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cannot determine cwd: {e}"))?;
    let bootstrap = shoal_host::SessionBootstrap::discover(&cwd).map_err(|e| e.to_string())?;
    // Before anything else prints: feed `render.color` into `no_color()` so
    // even these very warnings honor a `render.color = false` in `shoal.toml`
    // (site/content/internals/configuration-reference.md), the same way `NO_COLOR` already does.
    crate::apply_render_color_config(bootstrap.config().render.color);
    for warning in bootstrap.config_warnings() {
        eprintln!(
            "{}",
            maybe_strip(format!("\x1b[33;1mwarning:\x1b[0m config error: {warning}"))
        );
    }
    let config = bootstrap.config().clone();
    let protocol_backed = protocol_requested(standalone, config.kernel.enabled);
    let mut embedded_child = None;
    let mut protocol = if protocol_backed {
        let (client, child) =
            crate::embedded_kernel::connect(crate::embedded_kernel::EmbeddedKernelConfig {
                session: config.kernel.session.clone(),
                state_dir: shoal_state_dir(),
                policy: config.leash.policy.clone(),
                program: None,
            })?;
        embedded_child = Some(child);
        Some(ProtocolSession::new(client))
    } else {
        None
    };
    // `render.paging`/`render.pager` (site/content/internals/configuration-reference.md): resolved once, here,
    // from the loaded config — not re-read per keystroke/render. `enabled`
    // defaults to `false` (config default `"never"`), so an unconfigured
    // shoal behaves byte-for-byte like before this knob existed.
    let pager_ctx = PagerContext {
        enabled: config.render.paging == "auto",
        pager: config.render.pager.clone(),
    };
    let mut evaluator = Evaluator::new(cwd.clone());
    let bootstrap_report =
        bootstrap.apply(&mut evaluator, shoal_host::Surface::Interactive, "human")?;
    for warning in &bootstrap_report.warnings {
        eprintln!(
            "{}",
            maybe_strip(format!("\x1b[33;1mwarning:\x1b[0m {warning}"))
        );
    }
    if !protocol_backed {
        evaluator.set_statement_sink(Box::new(|v: &Value| {
            let _ = print_value(v);
        }));
    }

    // Install the command journal (site/content/internals/language-conformance-contract.md): without one, `undo`/`journal`/
    // `history` are inert (no journal means nothing is ever recorded). Open a
    // SECOND, independent handle on the exact same on-disk store (SQLite/WAL
    // supports concurrent handles fine) purely to read back each statement's
    // entry id right after it runs — that is how this host builds the
    // `out[n] -> journal entry id` map `undo out[n]` needs (site/content/internals/roadmap-and-priorities.md
    // (site/content/internals/persistence.md): the evaluator's own journal handle is private, and `out` itself is
    // just a plain REPL-side list of past values with no tie to entry ids.
    let state_dir = shoal_state_dir();
    // Enable `j`/`jump` directory-frecency recording against a store colocated
    // with the journal (`<state_dir>/jump.frecency`). Every interactive `cd`
    // now bumps directory history; best-effort, so a store write failure never
    // breaks navigation. Kept off for `-c`/scripts (they use `Evaluator::new`).
    if !protocol_backed {
        evaluator.set_jump_store(state_dir.join("jump.frecency"));
    }
    let journal_reader = if protocol_backed {
        None
    } else {
        match (Journal::open(&state_dir), Journal::open(&state_dir)) {
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
        }
    };
    // Parallels `out`'s growth 1:1 (one push per successful `record_transcript`
    // call below): `out_entries[n]` is the journal entry id (if any) the
    // statement that produced `out[n]` recorded.
    let mut out_entries = VecDeque::new();

    let catalogs = bootstrap_report.adapter_catalogs;
    let adapter_names = completer::scan_adapter_names(&bootstrap_report.adapter_dirs);
    if !protocol_backed {
        bootstrap.run_init(&mut evaluator)?;
    }

    // Ctrl-C must not kill the shell (site/content/internals/language-conformance-contract.md): install a real SIGINT
    // handler so the OS's default "terminate" disposition never fires while
    // a statement is executing (reedline's own `Signal::CtrlC` only covers
    // Ctrl-C pressed *while typing*, before Enter — the terminal is back in
    // cooked/ISIG mode by the time `eval_program` runs). The handler just
    // forwards to whichever `CancelToken` is currently active; `eval_program`
    // (and the exec layer under it) observe cancellation cooperatively and
    // unwind to an error instead of the process dying.
    let cancel_slot = Arc::new(Mutex::new(evaluator.cancellation_token()));
    let protocol_interrupt = Arc::new(AtomicBool::new(false));
    if let Ok(mut signals) =
        signal_hook::iterator::Signals::new([signal_hook::consts::signal::SIGINT])
    {
        let slot = cancel_slot.clone();
        let interrupt = protocol_interrupt.clone();
        std::thread::spawn(move || {
            for _ in signals.forever() {
                interrupt.store(true, Ordering::SeqCst);
                if let Ok(token) = slot.lock() {
                    token.cancel();
                }
            }
        });
    }
    // Job control (site/content/internals/language-conformance-contract.md): an interactive shell must ignore SIGTSTP/SIGTTOU/
    // SIGTTIN so a stray Ctrl-Z or a terminal-handoff operation never suspends
    // the shell itself — the classic bug this replaces. Gated on a real tty (the
    // same check the interactive path already relies on) so a piped/`-c` run is
    // untouched. Uses no-op *handlers*, so spawned children reset to SIG_DFL on
    // exec and can still be stopped on their own pty (see shoal-exec).
    if io::stdin().is_terminal() {
        shoal_eval::install_shell_job_control_signals();
    }

    let cwd_cell = Arc::new(Mutex::new(evaluator.cwd().to_path_buf()));
    let mut remote_env = RemoteEnvMirror::default();
    let mut protocol_snapshot = if let Some(session) = protocol.as_mut() {
        Some(refresh_protocol_state(
            session,
            &mut remote_env,
            evaluator.env(),
            &cwd_cell,
        )?)
    } else {
        None
    };
    let completer = ShoalCompleter::new(
        evaluator.env().clone(),
        cwd_cell.clone(),
        catalogs,
        adapter_names,
    )
    .configure(
        config.completion.fuzzy,
        config.completion.case_insensitive,
        config.completion.max_results,
    );
    // `editor.keybindings` (site/content/internals/configuration-reference.md): parse `chord -> action`
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
    // I/O on the render path — the whole point, site/content/internals/prompt-editor-lsp.md).
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
        // `completion.menu` (site/content/internals/configuration-reference.md): `false` asks for cycle-only
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
        .with_highlighter(Box::new(ShoalHighlighter::with_env(
            evaluator.env().clone(),
        )))
        .with_hinter(Box::new(DefaultHinter::default()))
        // `history.ignore_space` (site/content/internals/configuration-reference.md, classic
        // `HISTCONTROL=ignorespace`): reedline has this exact knob built in.
        .with_history_exclusion_prefix(if config.history.ignore_space {
            Some(" ".to_string())
        } else {
            None
        });
    if transient_enabled {
        // Transient prompt (site/content/internals/prompt-editor-lsp.md): a second ShoalPrompt sharing the same cache,
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
        match open_history(config.history.max_entries, &path) {
            Ok(history) => {
                // `history.dedup`/`history.ignore` (site/content/internals/configuration-reference.md):
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
            Err(error) => eprintln!(
                "{}",
                maybe_strip(format!(
                    "\x1b[33;1mwarning:\x1b[0m history unavailable ({}): {error}",
                    path.display()
                ))
            ),
        }
    }

    let (background_job_tx, background_job_rx) = mpsc::channel();

    loop {
        drain_background_job_events(&mut evaluator, &background_job_rx);
        // Keep the completer's cwd view and the cancel handler's active
        // token fresh for the statement about to run.
        if let Some(session) = protocol.as_mut() {
            match refresh_protocol_state(session, &mut remote_env, evaluator.env(), &cwd_cell) {
                Ok(snapshot) => {
                    protocol_snapshot = Some(snapshot);
                }
                Err(error) => report_protocol_error(&format!(
                    "cannot refresh interactive session state: {error}"
                )),
            }
        } else if let Ok(mut cell) = cwd_cell.lock() {
            *cell = evaluator.cwd().to_path_buf();
        }
        evaluator.reset_cancel();
        if let Ok(mut token) = cancel_slot.lock() {
            *token = evaluator.cancellation_token();
        }

        // Refresh the frozen prompt snapshot once, here, between commands —
        // never inside reedline's per-keystroke render (site/content/internals/prompt-editor-lsp.md).
        let width = u16::try_from(terminal_width()).unwrap_or(80);
        let ctx = match &protocol_snapshot {
            Some(snapshot) => prompt::build_context_from_protocol(snapshot, &static_facts, width),
            None => prompt::build_context(&mut evaluator, &static_facts, width),
        };
        if let Ok(mut cell) = shared_ctx.write() {
            *cell = Arc::new(ctx);
        }
        match editor.read_line(&shoal_prompt) {
            Ok(Signal::Success(src)) => {
                // A detached PTY may have changed state while Reedline owned
                // the thread. Reconcile before interpreting this line so a
                // `jobs` query cannot observe a completion that is already in
                // the notification queue as still running.
                drain_background_job_events(&mut evaluator, &background_job_rx);
                if src.trim().is_empty() {
                    continue;
                }
                if let Some(session) = protocol.as_mut() {
                    protocol_interrupt.store(false, Ordering::SeqCst);
                    match execute_protocol_line(
                        session,
                        &src,
                        &protocol_interrupt,
                        terminal_width(),
                    ) {
                        Ok(outcome) => {
                            if let Some(code) = outcome.exit_code {
                                return Ok(code);
                            }
                            if let Err(error) = render_protocol_outcome(&outcome, &pager_ctx) {
                                eprintln!(
                                    "{}",
                                    maybe_strip(format!(
                                        "\x1b[31;1merror:\x1b[0m cannot write output: {error}"
                                    ))
                                );
                            }
                        }
                        Err(error) => report_protocol_error(&error),
                    }
                    continue;
                }
                // Job control (site/content/internals/language-conformance-contract.md): `fg`/`bg` with a numeric job id (or a
                // bare `fg`/`bg` targeting the most-recent stopped job) resume a
                // Ctrl-Z'd foreground external command. Handled here, before
                // parse/eval, because it manipulates the live parked PTY + the
                // real terminal directly — not something the evaluator models.
                // A `fg <name>` (identifier) is NOT matched here and still flows
                // through `rewrite_fg` to resume a background `spawn` task.
                if let Some(jc) = parse_job_control(&src) {
                    // Fresh cancel epoch so Ctrl-C during a foreground resume is
                    // observed by that job's watcher.
                    evaluator.reset_cancel();
                    if let Ok(mut token) = cancel_slot.lock() {
                        *token = evaluator.cancellation_token();
                    }
                    handle_job_control(&mut evaluator, jc, &background_job_tx);
                    continue;
                }
                // `fg <task>` (site/content/internals/roadmap-and-priorities.md): host-level sugar, resolved
                // as plain source text before parsing since the evaluator has
                // no `fg` builtin of its own — see `rewrite_fg`.
                let run_src = rewrite_fg(&src).unwrap_or_else(|| src.clone());
                let ctx = parse_ctx_for(evaluator.env());
                match parse_with_ctx(&run_src, ctx) {
                    Ok(mut program) => {
                        // `undo out[n]` (site/content/internals/roadmap-and-priorities.md): rewrite a literal
                        // `out[n]` undo target into its recorded entry id so it
                        // resolves via the existing `undo <id>` path.
                        resolve_out_undo(&mut program, &out_entries);
                        // Hand the evaluator this line's source so each journaled
                        // top-level statement can slice its own `src` (site/content/internals/language-conformance-contract.md);
                        // without this the `history`/`journal` view shows an empty
                        // `src` column for every interactive entry, since
                        // `stmt_source` has nothing to slice from.
                        evaluator.set_source(run_src.clone());
                        let started_ns = now_ns();
                        match evaluator.eval_program(&program) {
                            Ok(value) => {
                                let entry_id = journal_reader.as_ref().and_then(|journal| {
                                    latest_entry_id(journal, REPL_PRINCIPAL, started_ns)
                                });
                                push_out_entry(&mut out_entries, entry_id);
                                evaluator.record_transcript(&value);
                                // Paging (site/content/internals/configuration-reference.md `render.paging`) applies
                                // ONLY to this final per-line result — never to
                                // `-c`/scripts (`main::run_source` calls the
                                // plain `render_result` below, which has no
                                // pager awareness at all) and never to
                                // mid-statement values inside a multi-statement
                                // line (the `statement_sink` installed above
                                // always calls `print_value` directly, never
                                // this paging-aware wrapper). A long-running
                                // multi-statement REPL line would otherwise
                                // page every intermediate value too, which
                                // reads as broken rather than helpful.
                                if let Err(error) = render_result_paged(&value, true, &pager_ctx) {
                                    eprintln!(
                                        "{}",
                                        maybe_strip(format!(
                                            "\x1b[31;1merror:\x1b[0m cannot write output: {error}"
                                        ))
                                    );
                                }
                                // Job control (site/content/internals/language-conformance-contract.md): if a foreground
                                // external command was Ctrl-Z'd during this
                                // statement, it is now a stopped job — announce
                                // it (bash's "[n]+ Stopped …") and return to the
                                // prompt. The outcome itself rendered nothing
                                // (its bytes already streamed to the terminal).
                                if let Some((id, desc)) = evaluator.take_pending_stop() {
                                    print_stopped_notice(id, &desc);
                                }
                                // `exit`/`quit` ends the REPL cleanly with its code,
                                // mirroring the Ctrl-D path (defect: no exit).
                                if let Some(code) = evaluator.take_exit() {
                                    shoal_eval::shutdown_stopped_jobs();
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
                protocol_interrupt.store(false, Ordering::SeqCst);
                println!("{}", maybe_strip("\x1b[90m^C\x1b[0m".to_string()));
            }
            Ok(Signal::CtrlD) => {
                println!();
                // Reap any Ctrl-Z'd jobs so no stopped child is orphaned.
                shoal_eval::shutdown_stopped_jobs();
                drop(embedded_child.take());
                return Ok(0);
            }
            Ok(_) => {}
            Err(error) => return Err(format!("line editor failed: {error}")),
        }
    }
}

fn open_history(max_entries: usize, path: &std::path::Path) -> Result<FileBackedHistory, String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create history directory: {error}"))?;
    }
    FileBackedHistory::with_file(max_entries, path.to_path_buf())
        .map_err(|error| format!("cannot open history file: {error}"))
}

/// Principal/session recorded on the REPL's own journal entries (site/content/internals/language-conformance-contract.md). A
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
    shoal_paths::ShoalPaths::discover()
        .state_dir()
        .to_path_buf()
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

/// `undo out[N]` resolution (site/content/internals/roadmap-and-priorities.md). The evaluator's `undo`
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
fn resolve_out_undo(program: &mut Program, out_entries: &VecDeque<Option<i64>>) {
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
            let Ok(index) = usize::try_from(n) else {
                continue;
            };
            index
        } else {
            let Some(distance) = n
                .checked_abs()
                .and_then(|distance| usize::try_from(distance).ok())
            else {
                continue;
            };
            let Some(idx) = out_entries.len().checked_sub(distance) else {
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

/// Keep the host-only journal-id mirror aligned with the evaluator's bounded
/// `out` window. Once the oldest value is evicted, index zero must refer to the
/// next retained value in both collections or `undo out[n]` targets the wrong
/// journal entry.
fn push_out_entry(out_entries: &mut VecDeque<Option<i64>>, entry_id: Option<i64>) {
    if out_entries.len() >= shoal_eval::MAX_REPL_TRANSCRIPT_VALUES {
        out_entries.pop_front();
    }
    out_entries.push_back(entry_id);
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
            Expr::Int { value, .. } => value.checked_neg(),
            _ => None,
        },
        _ => None,
    }
}

/// `fg <task>` (site/content/internals/roadmap-and-priorities.md): re-front a background task. There is no
/// `fg` builtin in the evaluator — task lifecycle methods (`.suspend()` /
/// `.resume()`) are implemented by `shoal-eval` (see
/// `site/content/internals/pty-job-control.md`). `fg` itself is host-level sugar that
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

/// Which job-control verb was typed and its optional numeric target.
enum JobKind {
    Fg,
    Bg,
}

struct JobControl {
    kind: JobKind,
    /// Explicit job id from the `jobs` table; `None` means "the current job"
    /// (the most-recently stopped one), matching the shell convention.
    id: Option<u64>,
}

#[derive(Debug)]
enum BackgroundJobEvent {
    Completed {
        id: u64,
        command: String,
        status: Option<i32>,
        signal: Option<String>,
    },
    Stopped {
        id: u64,
    },
    Failed {
        id: u64,
        error: String,
    },
}

/// Reconcile terminal notifications from detached PTY workers on the REPL
/// thread, which exclusively owns the evaluator's job map. Events are drained
/// before each prompt snapshot so `jobs` and the prompt never retain a child
/// as running after its completion has been observed.
fn drain_background_job_events(evaluator: &mut Evaluator, events: &Receiver<BackgroundJobEvent>) {
    while let Ok(event) = events.try_recv() {
        match event {
            BackgroundJobEvent::Completed {
                id,
                command,
                status,
                signal,
            } => {
                evaluator.finish_external_job_result(id, status, signal.clone());
                let terminal = match (status, signal.as_deref()) {
                    (Some(0), _) => "Done".to_string(),
                    (Some(code), _) => format!("Exit {code}"),
                    (_, Some(signal)) => signal.to_string(),
                    _ => "Failed".to_string(),
                };
                println!(
                    "{}",
                    maybe_strip(format!("\x1b[90m[{id}]+  {terminal}\x1b[0m  {command}"))
                );
            }
            BackgroundJobEvent::Stopped { id } => {
                evaluator.mark_external_stopped(id);
                if let Some((id, desc)) = evaluator.take_pending_stop() {
                    print_stopped_notice(id, &desc);
                }
            }
            BackgroundJobEvent::Failed { id, error } => {
                evaluator.fail_external_job(id, error.clone());
                eprintln!(
                    "{}",
                    maybe_strip(format!(
                        "\x1b[31;1m[{id}]+ background job failed:\x1b[0m {error}"
                    ))
                );
            }
        }
    }
}

impl JobKind {
    fn name(&self) -> &'static str {
        match self {
            JobKind::Fg => "fg",
            JobKind::Bg => "bg",
        }
    }
}

/// Recognize `fg`/`bg` job-control lines (site/content/internals/language-conformance-contract.md): bare `fg`/`bg`, or with a
/// numeric job id (optionally a bash-style `%N`). Deliberately does NOT match
/// `fg <name>` (an identifier — that resumes a `spawn` task via [`rewrite_fg`]),
/// nor unrelated commands like `fgrep`/`fg=1`; those return `None` and flow
/// through the normal parse/eval path.
fn parse_job_control(src: &str) -> Option<JobControl> {
    let trimmed = src.trim();
    let (kind, rest) = match trimmed.strip_prefix("fg") {
        Some(r) => (JobKind::Fg, r),
        None => (JobKind::Bg, trimmed.strip_prefix("bg")?),
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return Some(JobControl { kind, id: None });
    }
    // A single bare positive integer (optionally `%N`); anything else is not a
    // job-control line (e.g. `fg mytask`, `fgrep pattern`, `bg-tool`).
    let digits = rest.strip_prefix('%').unwrap_or(rest);
    let id: u64 = digits.parse().ok()?;
    Some(JobControl { kind, id: Some(id) })
}

/// The `[n]+ Stopped …` prompt notice for a Ctrl-Z'd foreground command.
fn print_stopped_notice(id: u64, desc: &str) {
    println!(
        "{}",
        maybe_strip(format!("\x1b[90m[{id}]+  Stopped\x1b[0m  {desc}"))
    );
}

/// Resume a stopped foreground external command (site/content/internals/language-conformance-contract.md). `fg` hands it the
/// terminal, SIGCONTs, and waits (WUNTRACED) for it to finish or stop again;
/// `bg` SIGCONTs it and lets it run detached. Job resources live in shoal-exec's
/// parked-job registry (the live PTY, keyed by pid) and in the evaluator's job
/// table (the listing + kernel suspend/resume) — this bridges the two by id.
fn handle_job_control(
    evaluator: &mut shoal_eval::Evaluator,
    jc: JobControl,
    background_events: &Sender<BackgroundJobEvent>,
) {
    let warn = |msg: &str| {
        eprintln!(
            "{}",
            maybe_strip(format!("\x1b[33;1m{}:\x1b[0m {msg}", jc.kind.name()))
        );
    };
    let Some(id) = jc.id.or_else(|| evaluator.last_stopped_external()) else {
        warn("no current job");
        return;
    };
    let Some(pid) = evaluator.external_job_pid(id) else {
        warn(&format!("no such stopped job [{id}]"));
        return;
    };
    let Some(job) = shoal_eval::take_stopped_job(pid) else {
        if evaluator
            .task_by_id(id)
            .is_some_and(|task| !task.is_suspended() && !task.is_done())
        {
            warn(&format!(
                "job [{id}] is already running in the background; foregrounding a live background PTY is not supported"
            ));
            return;
        }
        // The eval-side record outlived its parked PTY and no worker still owns
        // it. Retire the stale row so it stops showing up.
        warn(&format!("job [{id}] is no longer available"));
        evaluator.finish_external_job(id);
        return;
    };

    match jc.kind {
        JobKind::Fg => {
            // Echo the command being re-fronted (bash does), mark it running,
            // then hand over the terminal and wait.
            println!("{}", maybe_strip(job.command().to_string()));
            evaluator.mark_external_resumed(id);
            let cancel = evaluator.cancellation_token();
            match job.resume_foreground(&cancel) {
                Ok(res) if res.stopped => {
                    // Ctrl-Z'd again: back to a stopped job at the prompt.
                    evaluator.mark_external_stopped(id);
                    if let Some((sid, desc)) = evaluator.take_pending_stop() {
                        print_stopped_notice(sid, &desc);
                    }
                }
                Ok(res) => {
                    evaluator.finish_external_job_result(id, res.status, res.signal);
                }
                Err(error) => {
                    eprintln!("{}", maybe_strip(format!("\x1b[31;1mfg:\x1b[0m {error}")));
                    evaluator.fail_external_job(id, error.to_string());
                }
            }
        }
        JobKind::Bg => {
            evaluator.mark_external_resumed(id);
            let command = job.command().to_string();
            println!(
                "{}",
                maybe_strip(format!("\x1b[90m[{id}]+ {command} &\x1b[0m"))
            );
            // SIGCONT + detach: output keeps flowing to the terminal, stdin is
            // not forwarded. The worker reports its terminal transition back
            // to this REPL through a channel; evaluator state remains owned by
            // the prompt thread.
            let events = background_events.clone();
            job.resume_background_notify(move |result| {
                let event = match result {
                    Ok(result) if result.stopped => BackgroundJobEvent::Stopped { id },
                    Ok(result) => BackgroundJobEvent::Completed {
                        id,
                        command,
                        status: result.status,
                        signal: result.signal,
                    },
                    Err(error) => BackgroundJobEvent::Failed {
                        id,
                        error: error.to_string(),
                    },
                };
                let _ = events.send(event);
            });
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeProtocol {
        seen: Vec<String>,
        outcome: Result<ProtocolOutcome, String>,
        snapshot: Result<serde_json::Value, String>,
    }

    impl ReplProtocol for FakeProtocol {
        fn execute(
            &mut self,
            src: &str,
            _interrupt: &AtomicBool,
            _width: usize,
        ) -> Result<ProtocolOutcome, String> {
            self.seen.push(src.to_string());
            self.outcome.clone()
        }

        fn snapshot(&mut self) -> Result<serde_json::Value, String> {
            self.snapshot.clone()
        }
    }

    fn unused_snapshot() -> Result<serde_json::Value, String> {
        Err("snapshot not used by line-level test".into())
    }

    fn protocol_outcome(render: Option<&str>, state: &str) -> ProtocolOutcome {
        ProtocolOutcome {
            value_ref: Some("out:1".into()),
            render: render.map(str::to_owned),
            state: state.into(),
            exit_code: None,
            streamed: false,
        }
    }

    #[test]
    fn explicit_standalone_and_disabled_kernel_never_route_to_protocol() {
        assert!(!protocol_requested(true, true));
        assert!(!protocol_requested(true, false));
        assert!(!protocol_requested(false, false));
        assert!(protocol_requested(false, true));
    }

    #[test]
    fn protocol_line_preserves_source_and_applies_task_fg_sugar() {
        let mut protocol = FakeProtocol {
            seen: Vec::new(),
            outcome: Ok(protocol_outcome(Some("42"), "completed")),
            snapshot: unused_snapshot(),
        };
        let interrupt = AtomicBool::new(false);
        assert_eq!(
            execute_protocol_line(&mut protocol, "40 + 2", &interrupt, 120)
                .unwrap()
                .render
                .as_deref(),
            Some("42")
        );
        execute_protocol_line(&mut protocol, "fg worker", &interrupt, 120).unwrap();
        assert_eq!(protocol.seen, ["40 + 2", "worker.resume()\nworker.await()"]);
    }

    #[test]
    fn protocol_line_rejects_local_process_group_control_before_rpc() {
        let mut protocol = FakeProtocol {
            seen: Vec::new(),
            outcome: Ok(protocol_outcome(None, "completed")),
            snapshot: unused_snapshot(),
        };
        let error =
            execute_protocol_line(&mut protocol, "fg %2", &AtomicBool::new(false), 80).unwrap_err();
        assert!(error.contains("--standalone"));
        assert!(protocol.seen.is_empty());
    }

    #[test]
    fn cancelled_protocol_outcomes_never_render_stale_values() {
        let completed = protocol_outcome(Some("done"), "completed");
        assert_eq!(protocol_render_text(&completed), Some("done"));
        let cancelled = protocol_outcome(Some("stale"), "cancelled");
        assert_eq!(protocol_render_text(&cancelled), None);
        let empty = protocol_outcome(Some(""), "completed");
        assert_eq!(protocol_render_text(&empty), None);
        let mut streamed = protocol_outcome(Some("already live"), "completed");
        streamed.streamed = true;
        assert_eq!(protocol_render_text(&streamed), None);
    }

    #[test]
    fn protocol_exit_status_is_carried_to_the_ui_boundary() {
        let mut outcome = protocol_outcome(None, "completed");
        outcome.exit_code = Some(17);
        let mut protocol = FakeProtocol {
            seen: Vec::new(),
            outcome: Ok(outcome),
            snapshot: unused_snapshot(),
        };
        assert_eq!(
            execute_protocol_line(&mut protocol, "exit 17", &AtomicBool::new(false), 80)
                .unwrap()
                .exit_code,
            Some(17)
        );
    }

    #[test]
    fn protocol_snapshot_refreshes_completion_env_and_cwd() {
        let mut protocol = FakeProtocol {
            seen: Vec::new(),
            outcome: Ok(protocol_outcome(None, "completed")),
            snapshot: Ok(serde_json::json!({
                "cwd": {"display": "/remote/project"},
                "bindings": [
                    {"name": "deploy", "callable": true, "type": "command"},
                    {"name": "answer", "callable": false, "type": "int"}
                ],
                "jobs": {"running": 1, "suspended": 0, "total": 1},
                "reef": {"bindings": []},
                "last_value": {"$": "null"}
            })),
        };
        let env = Env::root();
        let cwd = Arc::new(Mutex::new(PathBuf::new()));
        let snapshot =
            refresh_protocol_state(&mut protocol, &mut RemoteEnvMirror::default(), &env, &cwd)
                .unwrap();

        assert_eq!(snapshot.jobs.running, 1);
        assert_eq!(*cwd.lock().unwrap(), PathBuf::from("/remote/project"));
        assert!(env.get("deploy").is_some_and(|value| value.is_callable()));
        assert!(matches!(env.get("answer"), Some(Value::Int(0))));
    }

    #[test]
    fn journal_id_mirror_evicts_in_lockstep_with_evaluator_out() {
        let mut entries = (0..shoal_eval::MAX_REPL_TRANSCRIPT_VALUES)
            .map(|id| Some(id as i64))
            .collect::<VecDeque<_>>();
        push_out_entry(&mut entries, Some(9_999));
        assert_eq!(entries.len(), shoal_eval::MAX_REPL_TRANSCRIPT_VALUES);
        assert_eq!(entries.front(), Some(&Some(1)));
        assert_eq!(entries.back(), Some(&Some(9_999)));
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

    /// Job-control line recognition (site/content/internals/language-conformance-contract.md): bare `fg`/`bg` and `%N`/`N`
    /// forms are job control; an identifier arg, a longer command sharing the
    /// prefix, or an assignment must fall through untouched.
    #[test]
    fn parse_job_control_matches_only_fg_bg_verbs() {
        let fg = |s: &str| parse_job_control(s).map(|jc| (matches!(jc.kind, JobKind::Fg), jc.id));
        assert_eq!(fg("fg"), Some((true, None)));
        assert_eq!(fg("  fg  "), Some((true, None)));
        assert_eq!(fg("fg 2"), Some((true, Some(2))));
        assert_eq!(fg("fg %3"), Some((true, Some(3))));
        assert_eq!(fg("bg"), Some((false, None)));
        assert_eq!(fg("bg 5"), Some((false, Some(5))));
        // Not job control: an identifier arg (that is `rewrite_fg`'s domain),
        // a command that merely starts with the letters, or an assignment.
        assert!(parse_job_control("fg mytask").is_none());
        assert!(parse_job_control("fgrep pattern").is_none());
        assert!(parse_job_control("bgtool").is_none());
        assert!(parse_job_control("fg=1").is_none());
        assert!(parse_job_control("echo hi").is_none());
    }

    #[test]
    fn background_job_events_reconcile_evaluator_rows() {
        let mut evaluator = Evaluator::new(PathBuf::from("/"));
        let completed = evaluator.register_stopped_external(41_001, 41_001, "done-job".into());
        assert!(evaluator.mark_external_resumed(completed));
        let (tx, rx) = mpsc::channel();
        tx.send(BackgroundJobEvent::Completed {
            id: completed,
            command: "done-job".into(),
            status: Some(0),
            signal: None,
        })
        .unwrap();
        drain_background_job_events(&mut evaluator, &rx);
        assert_eq!(evaluator.external_job_pid(completed), None);
        assert!(evaluator.task_by_id(completed).unwrap().is_done());

        let failed = evaluator.register_stopped_external(41_003, 41_003, "failed-job".into());
        assert!(evaluator.mark_external_resumed(failed));
        tx.send(BackgroundJobEvent::Completed {
            id: failed,
            command: "failed-job".into(),
            status: Some(7),
            signal: None,
        })
        .unwrap();
        drain_background_job_events(&mut evaluator, &rx);
        let error = evaluator
            .task_by_id(failed)
            .unwrap()
            .wait()
            .expect_err("nonzero background exit must remain a failed task");
        assert_eq!(error.code, "cmd_failed");
        assert_eq!(error.status, Some(7));

        let stopped = evaluator.register_stopped_external(41_002, 41_002, "stopped-job".into());
        assert!(evaluator.mark_external_resumed(stopped));
        tx.send(BackgroundJobEvent::Stopped { id: stopped })
            .unwrap();
        drain_background_job_events(&mut evaluator, &rx);
        assert_eq!(evaluator.external_job_pid(stopped), Some(41_002));
        assert!(evaluator.task_by_id(stopped).unwrap().is_suspended());
        assert_eq!(evaluator.last_stopped_external(), Some(stopped));
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

    #[test]
    fn open_history_reports_an_uncreatable_parent() {
        let dir = tempfile::tempdir().unwrap();
        let blocking_file = dir.path().join("not-a-directory");
        fs::write(&blocking_file, b"x").unwrap();
        let error = open_history(100, &blocking_file.join("history"))
            .expect_err("a file cannot be used as a history parent directory");
        assert!(error.contains("cannot create history directory"), "{error}");
    }

    /// `history.dedup` (site/content/internals/configuration-reference.md): a line identical to the
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

    #[test]
    fn filtered_history_clear_resets_dedup_state() {
        let dir = tempfile::tempdir().unwrap();
        let inner = FileBackedHistory::with_file(100, dir.path().join("hist")).unwrap();
        let mut history = FilteredHistory::new(Box::new(inner), true, Vec::new());
        history
            .save(HistoryItem::from_command_line("kept-after-clear"))
            .unwrap();
        history.clear().unwrap();
        history
            .save(HistoryItem::from_command_line("kept-after-clear"))
            .unwrap();

        let all = history
            .search(SearchQuery::everything(SearchDirection::Forward, None))
            .unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].command_line, "kept-after-clear");
    }

    /// `history.ignore` (site/content/internals/configuration-reference.md, `HISTIGNORE`-equivalent): a line
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

    /// `editor.mode` (site/content/internals/configuration-reference.md): `"vi"` selects reedline's `Vi` edit
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

    /// `editor.keybindings` (site/content/internals/configuration-reference.md): a custom chord actually
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

    /// `should_page` (site/content/internals/configuration-reference.md `render.paging`): the four inputs each
    /// independently gate paging — disabled, a non-TTY stdout, and output
    /// that already fits on one screen must all suppress it regardless of
    /// the others; only the conjunction of "enabled + TTY + overflowing"
    /// pages.
    #[test]
    fn should_page_requires_enabled_tty_and_overflowing_output() {
        assert!(should_page(true, true, 100, 24));
        assert!(!should_page(false, true, 100, 24), "paging = \"never\"");
        assert!(!should_page(true, false, 100, 24), "stdout is not a TTY");
        assert!(
            !should_page(true, true, 10, 24),
            "output fits on one screen"
        );
        assert!(
            !should_page(true, true, 24, 24),
            "output exactly filling the screen must not page"
        );
        assert!(
            should_page(true, true, 25, 24),
            "output one line past the screen must page"
        );
    }

    /// `pager_command` resolution order (site/content/internals/configuration-reference.md): an explicit
    /// `render.pager` config command wins over `$PAGER`, which wins over the
    /// built-in `less -R` fallback; a blank/whitespace-only value at either
    /// layer is treated as unset, not as a literal empty command.
    #[test]
    fn pager_command_resolution_order() {
        assert_eq!(
            pager_command(Some("bat --paging=always"), Some("more")),
            vec!["bat", "--paging=always"]
        );
        assert_eq!(pager_command(None, Some("most")), vec!["most"]);
        assert_eq!(pager_command(None, None), vec!["less", "-R"]);
        assert_eq!(
            pager_command(Some("   "), Some("most")),
            vec!["most"],
            "a blank config value must fall through to $PAGER"
        );
        assert_eq!(
            pager_command(Some("  "), None),
            vec!["less", "-R"],
            "a blank config value and no $PAGER must fall through to the default"
        );
    }

    /// `wrapped_line_count`: a short line is one row; a line exactly `width`
    /// columns wide is still one row; one column past `width` wraps to a
    /// second row; ANSI color escapes (what `render_block` actually emits)
    /// contribute zero width so they never inflate the wrap count.
    #[test]
    fn wrapped_line_count_accounts_for_terminal_wrapping() {
        assert_eq!(wrapped_line_count("short", 80), 1);
        assert_eq!(wrapped_line_count(&"x".repeat(80), 80), 1);
        assert_eq!(wrapped_line_count(&"x".repeat(81), 80), 2);
        assert_eq!(wrapped_line_count(&"x".repeat(160), 80), 2);
        assert_eq!(wrapped_line_count("line1\nline2\nline3", 80), 3);
        assert_eq!(
            wrapped_line_count("\x1b[34;1mkey\x1b[0m  value", 80),
            1,
            "ANSI color escapes must not count toward display width"
        );
        assert_eq!(
            wrapped_line_count(&format!("\x1b[31m{}\x1b[0m", "x".repeat(81)), 80),
            2,
            "a colorized line still wraps by its actual (non-escape) width"
        );
    }

    /// `render_result_paged` end-to-end gating: with paging disabled
    /// (`render.paging = "never"`, the default), even absurdly long output
    /// must go straight to a plain print — never touch a pager. This is the
    /// "flip the default and nothing else changes" contract the config knob
    /// promises; exercised through the real function rather than just
    /// `should_page` in isolation.
    #[test]
    fn render_result_paged_never_pages_when_disabled() {
        let pager = PagerContext {
            enabled: false,
            pager: None,
        };
        let value = Value::Str("x".repeat(10_000));
        // Must not block on a real pager / TTY prompt — disabled short-
        // circuits before any of that is even consulted.
        render_result_paged(&value, false, &pager).unwrap();
    }

    /// `spawn_pager` end-to-end (site/content/internals/configuration-reference.md: "if you can integration-test
    /// the actual pipe cheaply... do it"): spawns a real child process,
    /// writes the rendered text through its real stdin pipe, and confirms
    /// the bytes actually arrived — the same mechanics `render_result_paged`
    /// uses for a real `less`/`bat`, just redirected to a file instead of a
    /// TTY so the test needs no terminal at all.
    #[test]
    fn spawn_pager_pipes_text_through_a_real_child_process() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("captured");
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("cat > {}", out.display()),
        ];
        let ok = spawn_pager(&argv, "hello from the pager test\n");
        assert!(ok, "spawn_pager must report success for a real command");
        let captured = fs::read_to_string(&out).unwrap();
        assert_eq!(captured, "hello from the pager test\n");
    }

    /// A pager binary that doesn't exist must fail to spawn, cleanly and
    /// without panicking, so `render_result_paged` knows to fall back to a
    /// plain print rather than silently losing the output.
    #[test]
    fn spawn_pager_reports_failure_for_a_missing_binary() {
        let argv = vec!["definitely-not-a-real-pager-binary-xyz".to_string()];
        assert!(!spawn_pager(&argv, "irrelevant"));
    }

    /// The user quitting the pager early (it exits without draining stdin,
    /// so the write hits a broken pipe) must be reported as success — not a
    /// failure needing a duplicate re-print — and, crucially, must not panic
    /// (the SIGPIPE-on-write case the lane brief calls out explicitly).
    #[test]
    fn spawn_pager_survives_the_reader_quitting_before_draining_stdin() {
        // `true` exits immediately without ever reading its stdin, so the
        // subsequent `write_all` of a large buffer is virtually guaranteed
        // to observe a broken pipe on at least one write.
        let argv = vec!["true".to_string()];
        let big = "x".repeat(1024 * 1024);
        assert!(
            spawn_pager(&argv, &big),
            "a broken pipe must count as handled, not failed"
        );
    }

    /// Empty argv (a pathological `render.pager = ""` after whitespace
    /// trimming somehow reaching this far) must fail closed rather than
    /// panic on `argv[0]`.
    #[test]
    fn spawn_pager_rejects_empty_argv() {
        assert!(!spawn_pager(&[], "text"));
    }
}
