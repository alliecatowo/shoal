//! The interactive REPL: the `read_line` loop itself, its parse-context and
//! result-rendering helpers, the `undo out[n]`/`fg` source rewrites, and the
//! signal-handling + reedline/prompt wiring that only the interactive path
//! needs.

#[cfg(test)]
use std::collections::BTreeSet;
#[cfg(test)]
use std::collections::VecDeque;
#[cfg(test)]
use std::fs;
use std::io::{self, IsTerminal};
#[cfg(test)]
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::time::Duration;

use reedline::Signal;
#[cfg(test)]
use reedline::{
    ExternalPrinter, FileBackedHistory, History, HistoryItem, KeyCode, KeyModifiers, ReedlineEvent,
    SearchDirection, SearchQuery,
};
use shoal_eval::Evaluator;
use shoal_syntax::parse_with_ctx;
#[cfg(test)]
use shoal_value::Env;
use shoal_value::Value;

use crate::completer;
#[cfg(test)]
use crate::kernel_repl::ProtocolOutcome;
use crate::{format_parse_error, maybe_strip, report_eval_error};

mod editor;
mod jobs;
mod protocol;
mod rendering;
mod transcript;
mod ui;

#[cfg(test)]
use editor::{FilteredHistory, build_edit_mode, open_history};
#[cfg(test)]
use editor::{glob_match, input_is_incomplete};

#[cfg(test)]
use crate::repl_state::RemoteEnvMirror;
#[cfg(test)]
use jobs::{
    BackgroundJobEvent, BackgroundOutputState, JobKind, MAX_PENDING_BACKGROUND_EVENTS,
    consume_task_suppression, drain_background_job_events, enqueue_background_notice,
    handle_task_watcher_launch, retain_current_task_ids, watch_new_tasks,
};
use jobs::{BackgroundJobs, fg_task_name, parse_job_control, print_stopped_notice, rewrite_fg};
use protocol::{ProtocolState, protocol_requested};
#[cfg(test)]
use protocol::{ReplProtocol, execute_protocol_line, refresh_protocol_state};
#[cfg(test)]
use rendering::{
    PagerContext, pager_command, protocol_render_text, should_page, spawn_pager, wrapped_line_count,
};
pub(crate) use rendering::{print_value, render_result, render_result_paged, terminal_width};
use rendering::{render_protocol_outcome, report_protocol_error};
#[cfg(test)]
use transcript::push_out_entry;
use transcript::{TranscriptState, effective_journal_state_dir, language_journal_requested};
use ui::ReplUi;

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
    let state_dir = effective_journal_state_dir(config.journal.state_dir.as_deref(), &cwd);
    let protocol_backed = protocol_requested(standalone, config.kernel.enabled);
    let mut protocol =
        ProtocolState::connect(protocol_backed, &config, state_dir.clone(), cwd.clone())?;
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
        let configured_width = config.render.width;
        evaluator.set_statement_sink(Box::new(move |v: &Value| {
            let width = configured_width.unwrap_or_else(terminal_width).max(1);
            let _ = print_value(v, width);
        }));
    }

    if !protocol_backed {
        evaluator.set_jump_store(state_dir.join("jump.frecency"));
    }
    let mut transcript = TranscriptState::open(
        &mut evaluator,
        &state_dir,
        language_journal_requested(config.journal.enabled, protocol_backed),
    );

    let catalogs = bootstrap_report.adapter_catalogs;
    let adapter_names = completer::scan_adapter_names(&bootstrap_report.adapter_dirs);
    if !protocol_backed {
        bootstrap.run_init(&mut evaluator)?;
    }

    let interrupts = InterruptState::install(&evaluator, protocol.interrupt_handle())?;

    protocol.refresh(&evaluator)?;
    let cwd_cell = protocol.cwd_cell();
    let completion_path_dirs = protocol.path_dirs_cell();
    let (mut ui, background_printer) = ReplUi::build(
        &config,
        &cwd,
        &evaluator,
        catalogs,
        adapter_names,
        cwd_cell.clone(),
        completion_path_dirs.clone(),
    );
    let mut background_jobs = BackgroundJobs::new(&evaluator, background_printer);

    run_repl_loop(
        &mut evaluator,
        &mut protocol,
        &mut transcript,
        &mut background_jobs,
        &mut ui,
        &interrupts,
    )
}

/// The two cancellation sinks driven by SIGINT: evaluator epochs rotate per
/// command, while the protocol flag is stable for the transport lifetime.
struct InterruptState {
    slot: Arc<Mutex<shoal_exec::CancelToken>>,
}

impl InterruptState {
    fn install(evaluator: &Evaluator, protocol: Arc<AtomicBool>) -> Result<Self, String> {
        let slot = Arc::new(Mutex::new(evaluator.cancellation_token()));
        if let Ok(mut signals) =
            signal_hook::iterator::Signals::new([signal_hook::consts::signal::SIGINT])
        {
            let signal_slot = slot.clone();
            std::thread::Builder::new()
                .name("shoal-sigint".into())
                .spawn(move || {
                    for _ in signals.forever() {
                        protocol.store(true, Ordering::SeqCst);
                        if let Ok(token) = signal_slot.lock() {
                            token.cancel();
                        }
                    }
                })
                .map_err(|error| format!("cannot start the SIGINT watcher: {error}"))?;
        }
        if io::stdin().is_terminal() {
            shoal_eval::install_shell_job_control_signals()
                .map_err(|error| format!("cannot install job-control signal handlers: {error}"))?;
        }
        Ok(Self { slot })
    }

    fn refresh(&self, evaluator: &Evaluator) {
        if let Ok(mut token) = self.slot.lock() {
            *token = evaluator.cancellation_token();
        }
    }
}

fn run_repl_loop(
    evaluator: &mut Evaluator,
    protocol: &mut ProtocolState,
    transcript: &mut TranscriptState,
    background: &mut BackgroundJobs,
    ui: &mut ReplUi,
    interrupts: &InterruptState,
) -> Result<i32, String> {
    loop {
        background.reconcile(evaluator);
        if let Err(error) = protocol.refresh(evaluator) {
            report_protocol_error(&format!(
                "cannot refresh interactive session state: {error}"
            ));
        }
        evaluator.reset_cancel();
        interrupts.refresh(evaluator);
        ui.refresh_prompt(evaluator, protocol.snapshot());
        match ui.editor.read_line(&ui.prompt) {
            Ok(Signal::Success(src)) => {
                background.reconcile(evaluator);
                if let Some(code) = handle_submitted_line(
                    src, evaluator, protocol, transcript, background, ui, interrupts,
                ) {
                    return Ok(code);
                }
            }
            Ok(Signal::CtrlC) => {
                protocol.reset_interrupt();
                println!("{}", maybe_strip("\x1b[90m^C\x1b[0m".to_string()));
            }
            Ok(Signal::CtrlD) => {
                println!();
                shoal_eval::shutdown_stopped_jobs();
                protocol.shutdown();
                return Ok(0);
            }
            Ok(_) => {}
            Err(error) => return Err(format!("line editor failed: {error}")),
        }
    }
}

fn handle_submitted_line(
    src: String,
    evaluator: &mut Evaluator,
    protocol: &mut ProtocolState,
    transcript: &mut TranscriptState,
    background: &mut BackgroundJobs,
    ui: &ReplUi,
    interrupts: &InterruptState,
) -> Option<i32> {
    if src.trim().is_empty() {
        return None;
    }
    if protocol.is_backed() {
        protocol.reset_interrupt();
        match protocol.execute(&src, ui.pager.width()) {
            Ok(outcome) => {
                if let Some(code) = outcome.exit_code {
                    return Some(code);
                }
                if let Err(error) = render_protocol_outcome(&outcome, &ui.pager) {
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
        return None;
    }
    if let Some(control) = parse_job_control(&src) {
        evaluator.reset_cancel();
        interrupts.refresh(evaluator);
        background.handle_control(evaluator, control);
        return None;
    }
    if let Some(name) = fg_task_name(&src)
        && let Some(Value::Task(task)) = evaluator.env().get(name)
    {
        background.suppress(&task);
    }
    let run_src = rewrite_fg(&src).unwrap_or(src);
    let context = evaluator.parse_context(true);
    let mut program = match parse_with_ctx(&run_src, context) {
        Ok(program) => program,
        Err(error) => {
            eprint!("{}", format_parse_error(&run_src, None, &error));
            return None;
        }
    };
    transcript.resolve_undo(&mut program);
    evaluator.set_source(run_src.clone());
    evaluator.begin_journal_execution(None);
    let evaluation = evaluator.eval_program(&program);
    let journal_entry_id = evaluator.take_last_journal_entry();
    background.watch(evaluator, evaluation.as_ref().ok());
    match evaluation {
        Ok(value) => {
            if let Err(error) = transcript.record(evaluator, &value, journal_entry_id) {
                report_eval_error(&run_src, None, &error);
                return None;
            }
            if let Err(error) = render_result_paged(&value, true, &ui.pager) {
                eprintln!(
                    "{}",
                    maybe_strip(format!(
                        "\x1b[31;1merror:\x1b[0m cannot write output: {error}"
                    ))
                );
            }
            if let Some((id, description)) = evaluator.take_pending_stop() {
                print_stopped_notice(id, &description);
            }
            if let Some(code) = evaluator.take_exit() {
                shoal_eval::shutdown_stopped_jobs();
                return Some(code);
            }
        }
        Err(error) => report_eval_error(&run_src, None, &error),
    }
    None
}

#[cfg(test)]
mod tests;
