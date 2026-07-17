//! Tree-walk evaluator for shoal's canonical AST.

mod args;
mod builtins;
mod call;
mod channels;
mod child_context;
mod coerce;
mod command;
mod exec_state;
mod expr;
mod expr_access;
mod expr_binop;
mod frecency;
mod helpers;
mod host;
mod host_services;
mod journal;
mod modules;
mod namespaces;
mod pattern;
mod plan;
mod plan_derive;
mod plan_effects;
mod ports;
mod reef;
mod reef_builtins;
mod reef_resolve;
mod reef_state;
mod resolution;
mod script;
mod session_ctx;
mod stmt;
mod streams;

pub use channels::{EventBus, EventForwarder};
pub(crate) use child_context::ChildKind;
pub(crate) use coerce::coerce_word;
pub use reef::{PromptReefBinding, PromptReefSnapshot};
// Job-control surface (site/content/internals/language-conformance-contract.md) the interactive host (the REPL) drives. Re-
// exported through the evaluator — which the REPL already depends on — so `fg`/
// `bg` and the shell's signal setup need no new `shoal` -> `shoal-exec` Cargo
// edge (the crate-map DAG in site/content/internals/intercrate-protocol-contracts.md stays as pinned; `shoal` reaches
// exec's process-control primitives via `shoal-eval`, its existing dependency).
pub use shoal_exec::{
    PtyJob, install_shell_job_control_signals, shutdown_stopped_jobs, take_stopped_job,
};

use ports::{Exec, StdExec, StdSecret};
use shoal_adapters::{AdapterCatalog, AdapterClass, SubSpec};
use shoal_ast::*;
use shoal_exec::{CancelToken, ExecMode, ExecSpec, StdinSink, StdinSpec};
use shoal_journal::Journal;
use shoal_leash::{Effect, Estimates, Plan, Policy as LeashPolicy, Reversibility, SandboxPolicy};
use shoal_value::{
    CallArgs, CallCtx, Clock, ClosureVal, ConfigPort, ConfigSnapshot, Env, ErrorVal, Fs, Opener,
    OutcomeVal, Record, SecretPort, StdClock, StdFs, StdOpener, VResult, Value,
};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Statement,
    Value,
}

/// How much of a program's top-level statement values auto-render (the
/// `render.echo` knob, site/content/internals/configuration-reference.md). Governs only what the evaluator
/// routes to the statement sink for *intermediate* (non-final) statements —
/// the final statement's value is always returned to the host, which decides
/// how to present it (see the host's `run_source` for `Commands`-mode final
/// suppression). The default is [`EchoMode::All`], which preserves the REPL's
/// and every existing test/`Evaluator::new` caller's echo-everything behavior;
/// the non-interactive runner opts into `Quiet`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EchoMode {
    /// Echo every non-null top-level statement value (the REPL default and the
    /// legacy `-c`/script behavior). This is what `Evaluator::new` starts with,
    /// so nothing regresses unless a host explicitly opts into another mode.
    #[default]
    All,
    /// Echo only bare-command output; suppress every non-command intermediate
    /// (`1+1`, `let x=…`). The host also suppresses a non-command *final*
    /// expression in this mode.
    Commands,
    /// Echo bare-command output for intermediates but keep the final
    /// statement's value (the non-interactive default): a multi-statement
    /// script shows its commands' output and its last value, but not its
    /// intermediate pure expressions.
    Quiet,
}

/// Whether `stmt` is a bare command statement (`ls`, `git status`, `a && b`) —
/// the shape whose output shows in `quiet`/`commands` echo modes even when it
/// is not the final statement. A public free function (not just the crate-
/// internal [`helpers::is_command_expr`]) so the host can apply the same
/// command-vs-expression test to a program's *final* statement when deciding
/// whether to render it under [`EchoMode::Commands`].
pub fn is_bare_command_stmt(stmt: &Stmt) -> bool {
    matches!(stmt, Stmt::Expr { expr, .. } if helpers::is_command_expr(expr))
}

/// A count/summary of the live task table, for the prompt's `jobs` segment
/// (site/content/internals/kernel-protocol.md). Zero I/O: reads the in-memory task registry
/// only, never a subprocess or the filesystem.
///
/// `suspended` is always `0` today — the task registry has no suspended state
/// yet (only `Running`/`Done`); the field exists so this is additive, not a
/// breaking change, the day a suspend state lands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobsSnapshot {
    pub running: usize,
    pub suspended: usize,
    pub total: usize,
}

/// Host renderer for statement-position outcomes (defect #1).
pub type StatementSink = Box<dyn FnMut(&Value) + Send>;

pub struct Evaluator {
    /// Host capabilities and resolution inputs are one immutable shared
    /// snapshot. Configuration setters use Arc copy-on-write, so an existing
    /// child can never observe a half-updated bundle.
    host: Arc<host_services::HostServices>,
    /// Session identity, authority, and presentation policy. Kept as one typed
    /// unit so child construction cannot copy only a subset.
    session: session_ctx::SessionCtx,
    exec: exec_state::ExecState,
}

enum Flow {
    Value(Value),
    Return(Value),
    Break,
    Continue,
}

#[derive(Clone)]
struct ExecMeta {
    ok_codes: Vec<i32>,
    class: AdapterClass,
    parse: String,
    output_type: Option<String>,
}

impl Evaluator {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            host: Arc::new(host_services::HostServices::default()),
            session: session_ctx::SessionCtx::default(),
            exec: exec_state::ExecState::root(cwd),
        }
    }

    /// The lexical environment exposed to hosts for parsing and completion.
    pub fn env(&self) -> &Env {
        &self.exec.shell.env
    }

    /// Mutable lexical environment for deliberate host-side bindings.
    pub fn env_mut(&mut self) -> &mut Env {
        &mut self.exec.shell.env
    }

    /// Most recently evaluated statement value.
    pub fn it(&self) -> &Value {
        &self.exec.control.it
    }

    /// Replace the most recently evaluated statement value.
    pub fn set_it(&mut self, value: Value) {
        self.exec.control.it = value;
    }

    /// Install a custom [`Fs`] adapter (site/content/internals/roadmap-and-priorities.md). Additive: the
    /// default is [`StdFs`], which performs the exact `std::fs` calls the
    /// evaluator made inline, so this only changes behavior for a host/test
    /// that deliberately interposes a fake. Child evaluators spawned after this
    /// call inherit the adapter.
    pub fn set_fs(&mut self, fs: Arc<dyn Fs>) {
        Arc::make_mut(&mut self.host).fs = fs;
    }

    /// Install a custom [`Exec`] adapter (spawn seam). Default: [`StdExec`].
    pub fn set_exec(&mut self, exec: Arc<dyn Exec>) {
        Arc::make_mut(&mut self.host).exec = exec;
    }

    /// Install a custom [`Clock`] (for deterministic journal timestamps under
    /// test). Default: [`StdClock`].
    pub fn set_clock(&mut self, clock: Arc<dyn Clock>) {
        Arc::make_mut(&mut self.host).clock = clock;
    }

    /// Install a custom [`Opener`] (the `open <path>` effect). Default:
    /// [`StdOpener`].
    pub fn set_opener(&mut self, opener: Arc<dyn Opener>) {
        Arc::make_mut(&mut self.host).opener = opener;
    }

    /// Install a custom [`SecretPort`] (secret-store reads). Default:
    /// [`StdSecret`].
    pub fn set_secrets(&mut self, secrets: Arc<dyn SecretPort>) {
        Arc::make_mut(&mut self.host).secrets = secrets;
    }

    /// Install the resolved-config snapshot backing the in-language `config`
    /// namespace (`config.get`/`config.all`). Additive: the default is an
    /// empty snapshot, so an evaluator with no config injected reports `null`
    /// for every `config.get(key)` (no filesystem walk). The host passes a
    /// [`shoal_value::ConfigSnapshot`] built from the same `shoal_config`
    /// it applies to itself, so in-language config == host-applied config.
    /// Child evaluators spawned after this call inherit the snapshot.
    pub fn set_config(&mut self, config: Arc<dyn ConfigPort>) {
        Arc::make_mut(&mut self.host).config = config;
    }

    /// Select how much of a script/`-c` run's top-level statement values
    /// auto-render (the `render.echo` knob). Default [`EchoMode::All`] echoes
    /// every statement (the REPL/legacy behavior); the non-interactive host
    /// sets [`EchoMode::Quiet`] so intermediate pure expressions stay silent.
    pub fn set_echo_mode(&mut self, mode: EchoMode) {
        self.session.echo_mode = mode;
    }

    /// Declare whether this evaluator owns an interactive terminal. Child
    /// construction always resets this to false.
    pub fn set_interactive(&mut self, interactive: bool) {
        self.session.interactive = interactive;
    }

    pub fn is_interactive(&self) -> bool {
        self.session.interactive
    }

    /// The session event bus (site/content/internals/streams-channels.md). Shared into spawned tasks so
    /// in-language channels are visible across `spawn`/`on(...)`. Child
    /// evaluators receive it through [`ChildContext`](child_context::ChildContext);
    /// there is no partial `set_bus`-style seam a child site could under-inherit.
    pub(crate) fn bus(&self) -> Arc<channels::EventBus> {
        self.host.bus.clone()
    }

    /// Install the hook that mirrors in-language `channel(x).emit(...)` onto a
    /// hosting kernel's wire bus (site/content/internals/kernel-protocol.md one-substrate promise).
    /// Only `user.*` channels cross — the same client-writable rule as the
    /// wire's `events.publish`. Standalone hosts never call this.
    pub fn set_event_forwarder(&mut self, f: EventForwarder) {
        self.host.bus.set_forwarder(f);
    }

    /// A shareable handle to this session's in-language event bus, for hosts
    /// that must publish into it WITHOUT taking the evaluator lock (a wire
    /// `events.publish` must not stall behind a long-running exec). Inject via
    /// [`EventBus::inject`], which never re-forwards back out.
    pub fn event_bus(&self) -> Arc<EventBus> {
        self.host.bus.clone()
    }

    /// Consume any pending `exit`/`quit` request. `Some(code)` means the last
    /// evaluated program asked the host to exit with `code`; the host (REPL
    /// loop, `-c`, script runner) should stop and surface that code. Clears the
    /// flag so a subsequent REPL line starts fresh.
    pub fn take_exit(&mut self) -> Option<i32> {
        self.exec.control.pending_exit.take()
    }

    /// Install the active leash policy and the principal spawns are evaluated
    /// for (site/content/internals/language-conformance-contract.md). Additive: without this call there is no policy and every
    /// spawn runs unconfined exactly as before. A default-permissive policy
    /// (see [`shoal_leash::Policy::permissive`]) is safe to install — it still
    /// resolves to no OS confinement for a spawn, so normal use never
    /// regresses; only a scoped principal actually restricts a child.
    pub fn set_leash_policy(&mut self, policy: LeashPolicy, principal: impl Into<String>) {
        self.session.leash = Some((policy, principal.into()));
    }

    /// Convenience over [`Evaluator::set_leash_policy`]: load the per-user leash
    /// policy from `~/.config/shoal/leash.toml` (or `$XDG_CONFIG_HOME`) if it
    /// exists, else fall back to the default-permissive policy for `principal`
    /// (site/content/internals/language-conformance-contract.md). Hosts call this once at startup so agent principals can be
    /// scoped from config while a human keeps an unrestricted, no-regression
    /// session.
    pub fn load_leash_policy(&mut self, principal: impl Into<String>) {
        let principal = principal.into();
        let policy = LeashPolicy::load_user_or_permissive(&principal);
        self.set_leash_policy(policy, principal);
    }

    /// Resolve the OS [`SandboxPolicy`] for the next external spawn under the
    /// active leash policy, or `None` when no policy is installed, the
    /// principal is unknown, or its grants are unrestricted/unscoped. `None`
    /// keeps `ExecSpec.sandbox` unset — the pre-activation, unconfined path.
    pub(crate) fn resolve_sandbox(&self) -> Option<SandboxPolicy> {
        let (policy, principal) = self.session.leash.as_ref()?;
        policy.sandbox_for(principal)
    }

    /// Point the user reef scope at a `shoal.toml` whose `[reef]` table becomes
    /// the user scope (site/content/internals/reef-resolution.md). Additive: without it, there is no user scope,
    /// which is the zero-regression default. Changing the cwd next re-discovers
    /// the chain with this path folded in.
    pub fn set_reef_user_manifest(&mut self, path: impl Into<PathBuf>) {
        Arc::make_mut(&mut self.host).reef_user_manifest = Some(path.into());
        self.exec.reef.chain = None;
    }

    /// Inject the reef provider stack (resolver). Additive: without it the
    /// evaluator lazily builds [`shoal_reef::Resolver::with_defaults`] on the
    /// first constrained resolution. Hosts use this to pin providers; tests use
    /// it to point the resolver at fixture-rooted binaries instead of the real
    /// system.
    pub fn set_reef_resolver(&mut self, resolver: Arc<shoal_reef::Resolver>) {
        let host = Arc::make_mut(&mut self.host);
        host.reef_resolver = std::sync::OnceLock::from(resolver);
    }

    /// Install the host's statement renderer (defect #1). Every statement-position
    /// command outcome (and every non-final top-level value) is routed here.
    /// When unset, a built-in default prints to real stdout so scripts behave
    /// without host wiring.
    pub fn set_statement_sink(&mut self, f: StatementSink) {
        self.session.sink = Some(f);
    }

    /// Bind `it` and append to the session `out` transcript list (REPL hook).
    /// `Var("it")` / `Var("out")` then resolve from the environment normally.
    pub fn record_transcript(&mut self, v: &Value) {
        self.exec.shell.env.declare("it", v.clone(), true);
        let mut out = match self.exec.shell.env.get("out") {
            Some(Value::List(xs)) => xs,
            _ => Vec::new(),
        };
        out.push(v.clone());
        self.exec.shell.env.declare("out", Value::List(out), true);
    }

    /// Route a value to the statement sink (or the default stdout renderer).
    pub(crate) fn emit(&mut self, v: &Value) {
        if let Some(sink) = self.session.sink.as_mut() {
            sink(v);
        } else {
            helpers::default_render(v);
        }
    }

    /// Route a statement value to the sink, skipping nulls and skipping
    /// outcomes whose bytes already reached the real terminal via PtyTee
    /// (defect #1). Builtin outcomes and captured externals carry `streamed ==
    /// false` — they stream nothing — so they must still be rendered by the
    /// sink (outcome unification; see `site/content/internals/process-execution.md`); only a PtyTee'd child was
    /// tee'd to the terminal and should be suppressed here.
    pub(crate) fn sink_value(&mut self, v: &Value) {
        if *v == Value::Null {
            return;
        }
        if let Value::Outcome(o) = v
            && o.streamed
        {
            return;
        }
        self.emit(v);
    }

    /// A count/summary of the live task table for the prompt's `jobs` segment
    /// (site/content/internals/kernel-protocol.md). Cheap and I/O-free: call it once per
    /// command when building a `PromptContext`, never per keystroke.
    pub fn jobs_snapshot(&self) -> JobsSnapshot {
        let total = self.exec.jobs.tasks.len();
        let running = self
            .exec
            .jobs
            .tasks
            .iter()
            .filter(|t| !t.is_done() && !t.is_suspended())
            .count();
        let suspended = self
            .exec
            .jobs
            .tasks
            .iter()
            .filter(|t| !t.is_done() && t.is_suspended())
            .count();
        JobsSnapshot {
            running,
            suspended,
            total,
        }
    }

    /// The task table backing the `jobs` builtin (defect #14). Rows cover both
    /// spawned tasks and stopped foreground external commands (site/content/internals/language-conformance-contract.md job
    /// control) — a Ctrl-Z'd external appears here as a `stopped` job alongside
    /// any backgrounded `spawn` tasks. The `state` column collapses the
    /// `done`/`suspended` booleans into one word (`running`/`stopped`/`done`)
    /// for legibility; the booleans remain for programmatic filtering.
    pub(crate) fn jobs_table(&self) -> Value {
        let rows = self
            .exec
            .jobs
            .tasks
            .iter()
            .map(|t| {
                let done = t.is_done();
                let suspended = t.is_suspended();
                let state = if done {
                    "done"
                } else if suspended {
                    "stopped"
                } else {
                    "running"
                };
                let mut r = Record::new();
                r.insert("id".into(), Value::Int(t.id as i64));
                r.insert("desc".into(), Value::Str(t.shared.desc.clone()));
                r.insert("state".into(), Value::Str(state.into()));
                r.insert("done".into(), Value::Bool(done));
                r.insert("suspended".into(), Value::Bool(suspended));
                r
            })
            .collect();
        Value::Table(rows)
    }

    /// Suspend a background task by id (site/content/internals/language-conformance-contract.md job control, site/content/internals/roadmap-and-priorities.md). The
    /// kernel-callable path behind the wire `task.suspend` method and the REPL
    /// `fg`/job-control flow: it flips the task's suspended state and runs its
    /// suspend hooks (`SIGTSTP` to the task's process group, when a spawner has
    /// registered one). Returns `false` if no task has that id.
    pub fn suspend_task(&self, id: u64) -> bool {
        match self.exec.jobs.tasks.iter().find(|t| t.id == id) {
            Some(t) => {
                t.suspend();
                true
            }
            None => false,
        }
    }

    /// Resume a suspended task by id (`SIGCONT`). Counterpart to
    /// [`Evaluator::suspend_task`]. Returns `false` if no task has that id.
    pub fn resume_task(&self, id: u64) -> bool {
        match self.exec.jobs.tasks.iter().find(|t| t.id == id) {
            Some(t) => {
                t.resume();
                true
            }
            None => false,
        }
    }

    /// Look up a live task by id (for the REPL `fg <task>` path, which re-fronts a
    /// background task and must first resolve it from the job table).
    pub fn task_by_id(&self, id: u64) -> Option<shoal_value::TaskVal> {
        self.exec.jobs.tasks.iter().find(|t| t.id == id).cloned()
    }

    /// Record a foreground external command that the OS just *stopped* (Ctrl-Z →
    /// SIGTSTP, site/content/internals/language-conformance-contract.md). Registers a suspended [`shoal_value::TaskVal`] in the
    /// job table so it lists via `jobs` and the kernel `task.suspend`/
    /// `task.resume` wire methods drive its SIGTSTP/SIGCONT (through the hooks
    /// installed here, which signal the child's process group `pgid`). The pid
    /// is stashed so the REPL's `fg`/`bg` can find the still-live parked PTY via
    /// [`shoal_exec::take_stopped_job`]. Returns the new job id. The stop
    /// physically already happened, so the task is marked suspended WITHOUT
    /// re-sending SIGTSTP (see [`shoal_value::TaskVal::mark_suspended`]).
    pub fn register_stopped_external(&mut self, pid: u32, pgid: i32, desc: String) -> u64 {
        let task = shoal_value::TaskVal::new(desc.clone());
        task.on_suspend(Box::new(move || shoal_exec::suspend_group(pgid)));
        task.on_resume(Box::new(move || shoal_exec::continue_group(pgid)));
        task.mark_suspended();
        let id = task.id;
        self.exec.jobs.tasks.push(task);
        self.exec.jobs.external.insert(id, pid);
        self.exec.jobs.pending_stop = Some((id, desc));
        id
    }

    /// The child pid of a stopped-external job id, for the REPL `fg`/`bg` path to
    /// locate its parked PTY. `None` if `id` is not a stopped external command.
    pub fn external_job_pid(&self, id: u64) -> Option<u32> {
        self.exec.jobs.external.get(&id).copied()
    }

    /// The most recently registered external command that is currently stopped —
    /// the "current job" a bare `fg`/`bg` (no id) targets, matching the shell
    /// convention. `None` when no external command is stopped.
    pub fn last_stopped_external(&self) -> Option<u64> {
        self.exec
            .jobs
            .tasks
            .iter()
            .filter(|t| t.is_suspended() && self.exec.jobs.external.contains_key(&t.id))
            .map(|t| t.id)
            .max()
    }

    /// The most recently stopped foreground external command (job id, display),
    /// consumed once by the REPL after each command to print the stop notice.
    pub fn take_pending_stop(&mut self) -> Option<(u64, String)> {
        self.exec.jobs.pending_stop.take()
    }

    /// Mark a stopped-external job as running again WITHOUT signalling — the
    /// REPL `fg`/`bg` path performs the SIGCONT + terminal handoff itself, so
    /// this only updates the job-table state. Returns `false` for an unknown id.
    pub fn mark_external_resumed(&self, id: u64) -> bool {
        match self.exec.jobs.tasks.iter().find(|t| t.id == id) {
            Some(t) => {
                t.mark_resumed();
                true
            }
            None => false,
        }
    }

    /// Re-mark a stopped-external job as stopped (it was `fg`'d and then Ctrl-Z'd
    /// again) and re-arm the pending-stop notice, without re-signalling.
    pub fn mark_external_stopped(&mut self, id: u64) {
        if let Some(t) = self.exec.jobs.tasks.iter().find(|t| t.id == id) {
            t.mark_suspended();
            let desc = t.shared.desc.clone();
            self.exec.jobs.pending_stop = Some((id, desc));
        }
    }

    /// Retire a stopped-external job once it has finished (its `fg`/`bg` resume
    /// ran to completion): mark the task done so `jobs` shows it terminal, and
    /// drop the pid mapping. Returns `false` for an unknown id.
    pub fn finish_external_job(&mut self, id: u64) -> bool {
        self.exec.jobs.external.remove(&id);
        match self.exec.jobs.tasks.iter().find(|t| t.id == id) {
            Some(t) => {
                t.finish(Ok(Value::Null));
                true
            }
            None => false,
        }
    }

    pub fn cwd(&self) -> &Path {
        &self.exec.shell.cwd
    }

    /// The session's process environment (name → value pairs) — the same
    /// source the in-language `env` builtin reads and that seeds a spawned
    /// child's environment, including any in-session env writes. A read-only
    /// session-state accessor mirroring [`Evaluator::cwd`], used by the
    /// kernel's `shoal://session/env` resource view (site/content/internals/kernel-protocol.md).
    pub fn env_vars(&self) -> &[(OsString, OsString)] {
        &self.exec.shell.process_env
    }

    pub fn set_adapters(&mut self, adapters: AdapterCatalog) {
        Arc::make_mut(&mut self.host).adapters = adapters;
    }

    pub fn load_bundled_adapters(&mut self) -> Vec<String> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../adapters");
        let (catalog, warnings) = AdapterCatalog::load_dir(&root);
        Arc::make_mut(&mut self.host).adapters = catalog;
        warnings
    }

    /// Cancel the currently executing foreground process tree.
    pub fn cancel_current(&self) {
        self.exec.control.cancel.cancel();
    }

    pub fn cancellation_token(&self) -> CancelToken {
        self.exec.control.cancel.clone()
    }

    /// Install the cancellation epoch owned by the host's current execution.
    ///
    /// A host that serializes several executions through one evaluator must
    /// install the token only after it owns that evaluator. Creating a token
    /// while an execution is still queued lets a later request replace the
    /// evaluator's token before the queued execution starts, disconnecting
    /// that execution from its cancellation handle.
    pub fn set_cancellation_token(&mut self, cancel: CancelToken) {
        self.exec.control.cancel = cancel;
    }

    /// Install a fresh cancellation epoch before reading the next command.
    pub fn reset_cancel(&mut self) {
        self.exec.control.cancel = CancelToken::new();
    }
}

impl CallCtx for Evaluator {
    fn call_closure(&mut self, f: &Value, args: Vec<Value>) -> VResult<Value> {
        self.call_value(
            f,
            CallArgs {
                pos: args,
                named: vec![],
            },
        )
    }
    fn buffer_stream(
        &mut self,
        stream: shoal_value::StreamVal,
        capacity: usize,
    ) -> VResult<shoal_value::StreamVal> {
        self.spawn_stream_buffer(stream, capacity)
    }
    fn cwd(&self) -> PathBuf {
        self.exec.shell.cwd.clone()
    }
    /// Hand value methods the evaluator's *injected* Fs port, not the trait's
    /// `StdFs` default, so `.save`/`.append` write sinks are mediated by
    /// whatever adapter `set_fs` installed (HR-C follow-through wire).
    fn fs(&self) -> &dyn Fs {
        &*self.host.fs
    }
}

pub fn eval(program: &Program, cwd: impl AsRef<Path>) -> VResult<Value> {
    Evaluator::new(cwd.as_ref().to_path_buf()).eval_program(program)
}

#[cfg(test)]
mod tests;
