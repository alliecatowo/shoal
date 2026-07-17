//! Background task notices, PTY job control, and bounded lifecycle mirrors.

use std::collections::BTreeSet;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};

use reedline::ExternalPrinter;
use shoal_eval::Evaluator;
use shoal_value::Value;

use crate::maybe_strip;

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
pub(super) fn fg_task_name(src: &str) -> Option<&str> {
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
    Some(name)
}

pub(super) fn rewrite_fg(src: &str) -> Option<String> {
    let name = fg_task_name(src)?;
    Some(format!("{name}.resume()\n{name}.await()"))
}

/// Which job-control verb was typed and its optional numeric target.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum JobKind {
    Fg,
    Bg,
}

pub(super) struct JobControl {
    pub(super) kind: JobKind,
    /// Explicit job id from the `jobs` table; `None` means "the current job"
    /// (the most-recently stopped one), matching the shell convention.
    pub(super) id: Option<u64>,
}

#[derive(Debug)]
pub(super) enum BackgroundJobEvent {
    Completed {
        id: u64,
        command: String,
        status: Option<i32>,
        signal: Option<String>,
        omitted_output_bytes: usize,
        notified: bool,
    },
    Stopped {
        id: u64,
        command: String,
        omitted_output_bytes: usize,
        notified: bool,
    },
    Failed {
        id: u64,
        error: String,
        omitted_output_bytes: usize,
        notified: bool,
    },
    TaskCompleted {
        id: u64,
        description: String,
        error: Option<String>,
        notified: bool,
    },
}

impl BackgroundJobEvent {
    pub(super) fn notice(&self) -> String {
        let base = match self {
            Self::Completed {
                id,
                command,
                status,
                signal,
                ..
            } => {
                let terminal = match (status, signal.as_deref()) {
                    (Some(0), _) => "Done".to_string(),
                    (Some(code), _) => format!("Exit {code}"),
                    (_, Some(signal)) => signal.to_string(),
                    _ => "Failed".to_string(),
                };
                maybe_strip(format!("\x1b[90m[{id}]+  {terminal}\x1b[0m  {command}"))
            }
            Self::Stopped { id, command, .. } => {
                maybe_strip(format!("\x1b[90m[{id}]+  Stopped\x1b[0m  {command}"))
            }
            Self::Failed { id, error, .. } => maybe_strip(format!(
                "\x1b[31;1m[{id}]+ background job failed:\x1b[0m {error}"
            )),
            Self::TaskCompleted {
                id,
                description,
                error,
                ..
            } => match error {
                Some(error) => maybe_strip(format!(
                    "\x1b[31;1m[{id}]+ task failed:\x1b[0m {description}: {error}"
                )),
                None => maybe_strip(format!("\x1b[90m[{id}]+  Done\x1b[0m  {description}")),
            },
        };
        let omitted = match self {
            Self::Completed {
                omitted_output_bytes,
                ..
            }
            | Self::Stopped {
                omitted_output_bytes,
                ..
            }
            | Self::Failed {
                omitted_output_bytes,
                ..
            } => *omitted_output_bytes,
            Self::TaskCompleted { .. } => 0,
        };
        if omitted == 0 {
            base
        } else {
            format!("[shoal: {omitted} bytes of background output omitted]\n{base}")
        }
    }

    pub(super) fn notified(&self) -> bool {
        match self {
            Self::Completed { notified, .. }
            | Self::Stopped { notified, .. }
            | Self::Failed { notified, .. }
            | Self::TaskCompleted { notified, .. } => *notified,
        }
    }

    pub(super) fn set_notified(&mut self) {
        match self {
            Self::Completed { notified, .. }
            | Self::Stopped { notified, .. }
            | Self::Failed { notified, .. }
            | Self::TaskCompleted { notified, .. } => *notified = true,
        }
    }
}

const MAX_BACKGROUND_OUTPUT_LINE: usize = 8 * 1024;
/// Completion events carry identity-bearing transitions used to reconcile the
/// evaluator job table. They cannot be dropped or coalesced, so use bounded
/// backpressure rather than the standard library's unbounded channel.
pub(super) const MAX_PENDING_BACKGROUND_EVENTS: usize = 256;

pub(super) struct BackgroundOutputState {
    id: u64,
    printer: ExternalPrinter<String>,
    pending: Vec<u8>,
    omitted: usize,
}

impl BackgroundOutputState {
    pub(super) fn new(id: u64, printer: ExternalPrinter<String>) -> Self {
        Self {
            id,
            printer,
            pending: Vec::new(),
            omitted: 0,
        }
    }

    pub(super) fn push(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if byte == b'\n' {
                self.flush_line(true);
                continue;
            }
            self.pending.push(byte);
            if self.pending.len() >= MAX_BACKGROUND_OUTPUT_LINE {
                self.flush_line(false);
            }
        }
    }

    pub(super) fn finish(&mut self) -> usize {
        if !self.pending.is_empty() {
            self.flush_line(false);
        }
        std::mem::take(&mut self.omitted)
    }

    fn flush_line(&mut self, had_newline: bool) {
        let mut bytes = std::mem::take(&mut self.pending);
        if had_newline && bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        let safe = sanitize_background_output(&bytes);
        let prefix = if self.omitted > 0 {
            format!(
                "[shoal: {} bytes of background output omitted]\n",
                self.omitted
            )
        } else {
            String::new()
        };
        let message = format!("{prefix}[{}] {safe}", self.id);
        if self.printer.sender().try_send(message).is_ok() {
            self.omitted = 0;
        } else {
            self.omitted = self
                .omitted
                .saturating_add(bytes.len() + usize::from(had_newline));
        }
    }
}

fn sanitize_background_output(bytes: &[u8]) -> String {
    let mut safe = String::new();
    for ch in String::from_utf8_lossy(bytes).chars() {
        match ch {
            '\t' => safe.push('\t'),
            ch if ch.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(safe, "\\u{{{:x}}}", ch as u32);
            }
            ch => safe.push(ch),
        }
    }
    safe
}

/// Queue a completion notice without ever blocking a PTY reap worker. If the
/// bounded Reedline queue is full, the event remains unmarked and the REPL
/// thread prints it when it next drains state transitions.
pub(super) fn enqueue_background_notice(
    printer: &ExternalPrinter<String>,
    event: &mut BackgroundJobEvent,
) {
    if printer.sender().try_send(event.notice()).is_ok() {
        event.set_notified();
    }
}

pub(super) fn watch_new_tasks(
    evaluator: &Evaluator,
    final_value: Option<&Value>,
    watched: &mut BTreeSet<u64>,
    suppressed: &Arc<Mutex<BTreeSet<u64>>>,
    events: &SyncSender<BackgroundJobEvent>,
    printer: &ExternalPrinter<String>,
) {
    let tasks = evaluator.tasks_snapshot();
    let current_ids = tasks.iter().map(|task| task.id).collect::<BTreeSet<_>>();
    retain_current_task_ids(watched, &current_ids);
    for task in tasks {
        if !watched.insert(task.id) {
            continue;
        }
        // OS PTY jobs have their own result/status reconciliation path. Mark
        // them seen, but never create a second TaskVal completion notice.
        if evaluator.external_job_pid(task.id).is_some() {
            continue;
        }
        let returned = matches!(final_value, Some(Value::Task(value)) if value.same(&task));
        // A task already awaited within the same submitted line is ordinary
        // foreground work, not a background completion. A directly returned
        // TaskVal remains notification-worthy even if it finished very fast.
        if task.is_done() && !returned {
            continue;
        }
        let id = task.id;
        let events = events.clone();
        let watcher_printer = printer.clone();
        let suppressed = suppressed.clone();
        let launch = std::thread::Builder::new()
            .name(format!("shoal-task-watch-{id}"))
            .spawn(move || {
                let result = task.wait();
                if consume_task_suppression(&suppressed, task.id) {
                    return;
                }
                let error = result.err().map(|error| {
                    if let Some(status) = error.status {
                        format!("{}: {} (status {status})", error.code, error.msg)
                    } else {
                        format!("{}: {}", error.code, error.msg)
                    }
                });
                let mut event = BackgroundJobEvent::TaskCompleted {
                    id: task.id,
                    description: task.shared.desc.clone(),
                    error,
                    notified: false,
                };
                enqueue_background_notice(&watcher_printer, &mut event);
                let _ = events.send(event);
            });
        handle_task_watcher_launch(launch, id, watched, printer);
    }
}

pub(super) fn handle_task_watcher_launch(
    launch: std::io::Result<std::thread::JoinHandle<()>>,
    id: u64,
    watched: &mut BTreeSet<u64>,
    printer: &ExternalPrinter<String>,
) {
    if let Err(error) = launch {
        // The task itself remains owned by the evaluator. Retire only the host
        // mirror so the next prompt can retry installing its observer.
        watched.remove(&id);
        let _ = printer.sender().try_send(maybe_strip(format!(
            "\x1b[33;1mwarning:\x1b[0m cannot watch task [{id}]: {error}"
        )));
    }
}

pub(super) fn retain_current_task_ids(watched: &mut BTreeSet<u64>, current: &BTreeSet<u64>) {
    watched.retain(|id| current.contains(id));
}

/// Suppression is a one-shot rendezvous between `fg <task>` and its completion
/// watcher. Removing on observation prevents monotonic task IDs from becoming
/// a second, host-only history.
pub(super) fn consume_task_suppression(suppressed: &Arc<Mutex<BTreeSet<u64>>>, id: u64) -> bool {
    suppressed
        .lock()
        .is_ok_and(|mut suppressed| suppressed.remove(&id))
}

/// Reconcile terminal notifications from detached PTY workers on the REPL
/// thread, which exclusively owns the evaluator's job map. Events are drained
/// before each prompt snapshot so `jobs` and the prompt never retain a child
/// as running after its completion has been observed.
pub(super) fn drain_background_job_events(
    evaluator: &mut Evaluator,
    events: &Receiver<BackgroundJobEvent>,
) {
    while let Ok(event) = events.try_recv() {
        let fallback_notice = (!event.notified()).then(|| event.notice());
        match event {
            BackgroundJobEvent::Completed {
                id,
                status,
                signal,
                notified,
                ..
            } => {
                evaluator.finish_external_job_result(id, status, signal.clone());
                if !notified && let Some(notice) = fallback_notice {
                    println!("{notice}");
                }
            }
            BackgroundJobEvent::Stopped { id, notified, .. } => {
                evaluator.mark_external_stopped(id);
                if let Some((id, desc)) = evaluator.take_pending_stop()
                    && !notified
                {
                    print_stopped_notice(id, &desc);
                }
            }
            BackgroundJobEvent::Failed {
                id,
                ref error,
                notified,
                ..
            } => {
                evaluator.fail_external_job(id, error.clone());
                if !notified && let Some(notice) = fallback_notice {
                    eprintln!("{notice}");
                }
            }
            BackgroundJobEvent::TaskCompleted { .. } => {
                if let Some(notice) = fallback_notice {
                    println!("{notice}");
                }
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
pub(super) fn parse_job_control(src: &str) -> Option<JobControl> {
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
pub(super) fn print_stopped_notice(id: u64, desc: &str) {
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
pub(super) fn handle_job_control(
    evaluator: &mut shoal_eval::Evaluator,
    jc: JobControl,
    background_events: &SyncSender<BackgroundJobEvent>,
    background_printer: &ExternalPrinter<String>,
) {
    let warn = |msg: &str| {
        eprintln!(
            "{}",
            maybe_strip(format!("\x1b[33;1m{}:\x1b[0m {msg}", jc.kind.name()))
        );
    };
    let current = match jc.kind {
        JobKind::Fg => evaluator.last_external_job(),
        JobKind::Bg => evaluator.last_stopped_external(),
    };
    let Some(id) = jc.id.or(current) else {
        warn("no current job");
        return;
    };
    let Some(pid) = evaluator.external_job_pid(id) else {
        warn(&format!("no such stopped job [{id}]"));
        return;
    };
    let running = evaluator
        .task_by_id(id)
        .is_some_and(|task| !task.is_suspended() && !task.is_done());
    let (job, was_stopped) = if let Some(job) = shoal_eval::take_stopped_job(pid) {
        (job, true)
    } else if running && jc.kind == JobKind::Fg {
        match shoal_eval::take_background_job(pid) {
            Ok(Some(job)) => (job, false),
            Ok(None) => {
                warn(&format!(
                    "job [{id}] changed state before it could be foregrounded"
                ));
                return;
            }
            Err(error) => {
                warn(&format!("cannot foreground job [{id}]: {error}"));
                return;
            }
        }
    } else if running {
        warn(&format!("job [{id}] is already running in the background"));
        return;
    } else {
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
            let result = if was_stopped {
                job.resume_foreground(&cancel)
            } else {
                job.foreground_running(&cancel)
            };
            match result {
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
            let printer = background_printer.clone();
            let output_state = Arc::new(Mutex::new(BackgroundOutputState::new(
                id,
                background_printer.clone(),
            )));
            let output_sink = output_state.clone();
            job.resume_background_notify_with_output(
                move |bytes| {
                    if let Ok(mut output) = output_sink.lock() {
                        output.push(bytes);
                    }
                },
                move |result| {
                    let omitted_output_bytes =
                        output_state.lock().map_or(0, |mut output| output.finish());
                    let mut event = match result {
                        Ok(result) if result.stopped => BackgroundJobEvent::Stopped {
                            id,
                            command,
                            omitted_output_bytes,
                            notified: false,
                        },
                        Ok(result) => BackgroundJobEvent::Completed {
                            id,
                            command,
                            status: result.status,
                            signal: result.signal,
                            omitted_output_bytes,
                            notified: false,
                        },
                        Err(error) => BackgroundJobEvent::Failed {
                            id,
                            error: error.to_string(),
                            omitted_output_bytes,
                            notified: false,
                        },
                    };
                    enqueue_background_notice(&printer, &mut event);
                    let _ = events.send(event);
                },
            );
        }
    }
}
