//! Tree-walk evaluator for shoal's canonical AST.

mod args;
mod builtins;
mod call;
mod channels;
mod child_context;
mod coerce;
mod command;
mod expr;
mod expr_access;
mod expr_binop;
mod frecency;
mod helpers;
mod host;
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
mod script;
mod stmt;
mod streams;

pub use channels::{EventBus, EventForwarder};
pub(crate) use child_context::ChildScope;
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
use shoal_exec::{CancelToken, ExecMode, ExecSpec, StdinSpec};
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
    pub env: Env,
    cwd: PathBuf,
    process_env: Vec<(OsString, OsString)>,
    pub interactive: bool,
    pub it: Value,
    cancel: CancelToken,
    adapters: AdapterCatalog,
    /// Host renderer for statement-position outcomes (site/content/internals/language-conformance-contract.md, defect #1).
    sink: Option<StatementSink>,
    /// Runtime call-stack depth guard (defect #9).
    call_depth: usize,
    /// Nesting depth inside `fn` bodies — gates `cd`/env writes (defect #10).
    in_fn_body: usize,
    /// Live task registry backing the `jobs` builtin (defect #14).
    jobs: Vec<shoal_value::TaskVal>,
    /// Map of stopped-external job id → child pid, for the REPL's `fg`/`bg`
    /// (site/content/internals/language-conformance-contract.md job control). A stopped foreground external command is recorded
    /// twice: as a suspended `TaskVal` in `jobs` (so it lists via `jobs` and the
    /// kernel `task.suspend`/`task.resume` wire methods drive its SIGTSTP/SIGCONT
    /// through the task's hooks), and here so `fg`/`bg` can locate its still-live
    /// parked PTY by pid (via [`shoal_exec::take_stopped_job`]). Empty unless an
    /// interactive foreground command has actually been Ctrl-Z'd.
    external_jobs: std::collections::HashMap<u64, u32>,
    /// The most recent foreground external command that stopped this turn
    /// (job id, display form), set by `run_argv` when the exec layer reports a
    /// stop and drained by the REPL (`take_pending_stop`) to print the
    /// "[id]+ Stopped …" notice and return to the prompt.
    pending_stop: Option<(u64, String)>,
    /// reef (site/content/internals/reef-resolution.md): cached scope chain, keyed on the cwd it was
    /// discovered for. Rebuilt only when the cwd changes (cd / `with cwd:`).
    /// `None` until the first spawn/`which`/`reef` touches it; cheap when no
    /// manifest is in scope (a pure filesystem walk with an empty result).
    reef_chain: Option<(PathBuf, shoal_reef::ScopeChain)>,
    /// reef: the provider stack, built lazily on the first *constrained*
    /// resolution — never touched on the hot path when no manifest is in scope.
    reef_resolver: Option<Arc<shoal_reef::Resolver>>,
    /// reef: the in-memory lock, loaded from (and persisted next to) the nearest
    /// manifest. Empty and inert when no manifest is in scope.
    reef_lock: shoal_reef::Lockfile,
    /// reef: filesystem path the current lock loads from / persists to.
    reef_lock_path: Option<PathBuf>,
    /// reef: optional user-scope `shoal.toml` whose `[reef]` table forms the
    /// user scope. `None` (the default) means no user scope — the zero-config,
    /// zero-regression path. Hosts wire a real path via
    /// [`Evaluator::set_reef_user_manifest`]; tests never point at real config.
    reef_user_manifest: Option<PathBuf>,
    /// reef: `with reef: {tool: constraint, …} { }` override layers (see
    /// `site/content/internals/reef-resolution.md`), nearest-first (innermost `with reef:` block wins). Empty and inert
    /// when no `with reef:` is on the dynamic stack — zero-regression.
    reef_overrides: Vec<shoal_reef::ScopeEntry>,
    /// Optional command journal (site/content/internals/language-conformance-contract.md). `None` (the default) means NOTHING is
    /// journaled — the exact pre-journal behavior, so `-c`/scripts/conformance
    /// run untouched. Only when a host installs a journal via
    /// [`Evaluator::set_journal`] (or [`Evaluator::open_default_journal`]) does
    /// per-top-level-statement recording, output capture, and fs undo-inverse
    /// recording happen. Held here (not shared) because `Journal` is single-
    /// handle / not `Sync`; spawned child evaluators never inherit it.
    journal: Option<Journal>,
    /// Session id recorded on each journal entry (site/content/internals/language-conformance-contract.md). Ignored when no
    /// journal is installed.
    session_id: String,
    /// Acting principal recorded on each journal entry: `"human"` or
    /// `"agent:<name>"`. Ignored when no journal is installed.
    principal: String,
    /// The journal entry id of the top-level statement currently executing, so
    /// nested fs mutations (rm/cp/mv/save) can attach undo inverses to it.
    /// `None` outside a journaled statement.
    current_entry: Option<i64>,
    /// Source text of the program currently being evaluated, used to slice each
    /// top-level statement's `src` for its journal entry (site/content/internals/language-conformance-contract.md). `None` when
    /// the host did not provide it — the entry's `src` is then left empty.
    source: Option<String>,
    /// Active leash policy + the principal it is evaluated for (site/content/internals/language-conformance-contract.md). `None`
    /// (the default) means no OS sandbox is ever applied to a spawn — the exact
    /// pre-activation behavior, so `Evaluator::new` and every existing test
    /// keep running unconfined. When set, each external spawn resolves a
    /// [`SandboxPolicy`] for `principal` and passes it via `ExecSpec.sandbox`;
    /// a default-permissive policy still resolves to `None` (no wrapping), so
    /// only a genuinely-scoped principal ever confines a child.
    leash: Option<(LeashPolicy, String)>,
    /// The in-language `channel(name)` event bus (site/content/internals/streams-channels.md). Shared
    /// (Arc) so spawned tasks / `on(...)` handlers publish and subscribe to the
    /// same session-scoped channels — coordination is channels, never files.
    bus: Arc<channels::EventBus>,
    /// Set by the `exit`/`quit` builtin (defect: no `exit`). `Some(code)` asks
    /// the host to stop: `eval_program` halts its statement loop, and the host
    /// (REPL loop / `-c` / script runner) ends cleanly with `code`. Kept as a
    /// value the host acts on — eval NEVER calls `std::process::exit`, which
    /// would break the kernel/embedded host.
    pending_exit: Option<i32>,
    /// Module cache (site/content/internals/roadmap-and-priorities.md): a module (keyed by canonical path) evaluates
    /// once per session; its exports record is memoized here. Empty until the
    /// first `use`.
    modules: std::collections::HashMap<PathBuf, Value>,
    /// The stack of modules currently being loaded, for circular-`use` detection.
    module_stack: Vec<PathBuf>,
    /// Derived-but-unspawned plans from the `plan { … }` REPL verb (site/content/internals/roadmap-and-priorities.md),
    /// indexed by id (`1`-based). `apply <ref>` looks a plan up here and runs it.
    plans: Vec<Program>,
    /// Persistent directory-frecency store for `j`/`jump` (`frecency.rs`).
    /// `None` (the `Evaluator::new` default, so `-c`/scripts/conformance never
    /// write) disables recording; a `j` query then still reads the shared
    /// per-user store. `Some(path)` (set by an interactive host via
    /// [`Evaluator::open_default_jump_history`], or a test via
    /// [`Evaluator::set_jump_store`]) makes every `cd` bump that file.
    jump_store: Option<PathBuf>,
    /// The directory `cd -` returns to: the cwd immediately before the last
    /// real navigation (`cd`/`pushd`/`popd`/`j`). `None` until the first cwd
    /// change. Held as a plain field exactly like `cwd` — session-scoped
    /// ambient state, never persisted to disk. `cd -` restores this *exact*
    /// `PathBuf` (byte-identical to the directory left), never a re-derived one.
    oldpwd: Option<PathBuf>,
    /// The `pushd`/`popd` directory stack: the directories *below* the current
    /// `cwd`, which is always the conceptual top. `dirs` renders `[cwd] ++
    /// dir_stack` (current first, bash's left-to-right order). Session-scoped
    /// ambient state like `cwd`; never journaled or persisted to disk.
    dir_stack: Vec<PathBuf>,
    /// Hexagonal effect ports (site/content/internals/roadmap-and-priorities.md). Every direct filesystem,
    /// spawn, clock, opener, and secret call the domain core makes routes
    /// through one of these instead of `std::*`. The defaults are the `Std*`
    /// adapters, which perform the identical inline calls — so a plain
    /// `Evaluator::new` is byte-identical to the pre-ports behavior. Held as
    /// `Arc<dyn Port>` so child evaluators (`parallel`, `source`, `spawn`)
    /// share them cheaply and a test can interpose a fake. See
    /// [`Evaluator::set_fs`] and friends.
    fs: Arc<dyn Fs>,
    exec: Arc<dyn Exec>,
    clock: Arc<dyn Clock>,
    opener: Arc<dyn Opener>,
    secrets: Arc<dyn SecretPort>,
    /// Resolved configuration snapshot backing the in-language `config`
    /// namespace (`config.get`/`config.all`). The default is an empty snapshot
    /// (no config injected → `config.get` is `null`, never a filesystem walk);
    /// a host injects the *same* resolved `shoal_config::Config` it applies to
    /// itself via [`Evaluator::set_config`], so in-language config == the
    /// host-applied config. Held as `Arc<dyn ConfigPort>` like the other ports
    /// so child evaluators inherit it cheaply (see [`ChildContext`]).
    ///
    /// [`ChildContext`]: child_context::ChildContext
    config: Arc<dyn ConfigPort>,
    /// How much of a script/`-c` run's top-level statement values auto-render
    /// (the `render.echo` knob, site/content/internals/configuration-reference.md). Default [`EchoMode::All`]
    /// (echo everything) so `Evaluator::new`, the REPL, and every conformance
    /// case keep their current behavior; the non-interactive host opts into
    /// `Quiet`.
    echo_mode: EchoMode,
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
            env: Env::root(),
            cwd,
            process_env: std::env::vars_os().collect(),
            interactive: false,
            it: Value::Null,
            cancel: CancelToken::new(),
            adapters: AdapterCatalog::empty(),
            sink: None,
            call_depth: 0,
            in_fn_body: 0,
            jobs: Vec::new(),
            external_jobs: std::collections::HashMap::new(),
            pending_stop: None,
            reef_chain: None,
            reef_resolver: None,
            reef_lock: shoal_reef::Lockfile::new(),
            reef_lock_path: None,
            reef_user_manifest: None,
            reef_overrides: Vec::new(),
            leash: None,
            journal: None,
            session_id: "default".into(),
            principal: "human".into(),
            current_entry: None,
            source: None,
            bus: channels::EventBus::shared(),
            pending_exit: None,
            modules: std::collections::HashMap::new(),
            module_stack: Vec::new(),
            plans: Vec::new(),
            jump_store: None,
            oldpwd: None,
            dir_stack: Vec::new(),
            fs: Arc::new(StdFs),
            exec: Arc::new(StdExec),
            clock: Arc::new(StdClock),
            opener: Arc::new(StdOpener),
            secrets: Arc::new(StdSecret),
            config: Arc::new(ConfigSnapshot::default()),
            echo_mode: EchoMode::default(),
        }
    }

    /// Install a custom [`Fs`] adapter (site/content/internals/roadmap-and-priorities.md). Additive: the
    /// default is [`StdFs`], which performs the exact `std::fs` calls the
    /// evaluator made inline, so this only changes behavior for a host/test
    /// that deliberately interposes a fake. Child evaluators spawned after this
    /// call inherit the adapter.
    pub fn set_fs(&mut self, fs: Arc<dyn Fs>) {
        self.fs = fs;
    }

    /// Install a custom [`Exec`] adapter (spawn seam). Default: [`StdExec`].
    pub fn set_exec(&mut self, exec: Arc<dyn Exec>) {
        self.exec = exec;
    }

    /// Install a custom [`Clock`] (for deterministic journal timestamps under
    /// test). Default: [`StdClock`].
    pub fn set_clock(&mut self, clock: Arc<dyn Clock>) {
        self.clock = clock;
    }

    /// Install a custom [`Opener`] (the `open <path>` effect). Default:
    /// [`StdOpener`].
    pub fn set_opener(&mut self, opener: Arc<dyn Opener>) {
        self.opener = opener;
    }

    /// Install a custom [`SecretPort`] (secret-store reads). Default:
    /// [`StdSecret`].
    pub fn set_secrets(&mut self, secrets: Arc<dyn SecretPort>) {
        self.secrets = secrets;
    }

    /// Install the resolved-config snapshot backing the in-language `config`
    /// namespace (`config.get`/`config.all`). Additive: the default is an
    /// empty snapshot, so an evaluator with no config injected reports `null`
    /// for every `config.get(key)` (no filesystem walk). The host passes a
    /// [`shoal_value::ConfigSnapshot`] built from the same `shoal_config`
    /// it applies to itself, so in-language config == host-applied config.
    /// Child evaluators spawned after this call inherit the snapshot.
    pub fn set_config(&mut self, config: Arc<dyn ConfigPort>) {
        self.config = config;
    }

    /// Select how much of a script/`-c` run's top-level statement values
    /// auto-render (the `render.echo` knob). Default [`EchoMode::All`] echoes
    /// every statement (the REPL/legacy behavior); the non-interactive host
    /// sets [`EchoMode::Quiet`] so intermediate pure expressions stay silent.
    pub fn set_echo_mode(&mut self, mode: EchoMode) {
        self.echo_mode = mode;
    }

    /// The session event bus (site/content/internals/streams-channels.md). Shared into spawned tasks so
    /// in-language channels are visible across `spawn`/`on(...)`. Child
    /// evaluators receive it through [`ChildContext`](child_context::ChildContext);
    /// there is no partial `set_bus`-style seam a child site could under-inherit.
    pub(crate) fn bus(&self) -> Arc<channels::EventBus> {
        self.bus.clone()
    }

    /// Install the hook that mirrors in-language `channel(x).emit(...)` onto a
    /// hosting kernel's wire bus (site/content/internals/kernel-protocol.md one-substrate promise).
    /// Only `user.*` channels cross — the same client-writable rule as the
    /// wire's `events.publish`. Standalone hosts never call this.
    pub fn set_event_forwarder(&mut self, f: EventForwarder) {
        self.bus.set_forwarder(f);
    }

    /// A shareable handle to this session's in-language event bus, for hosts
    /// that must publish into it WITHOUT taking the evaluator lock (a wire
    /// `events.publish` must not stall behind a long-running exec). Inject via
    /// [`EventBus::inject`], which never re-forwards back out.
    pub fn event_bus(&self) -> Arc<EventBus> {
        self.bus.clone()
    }

    /// Consume any pending `exit`/`quit` request. `Some(code)` means the last
    /// evaluated program asked the host to exit with `code`; the host (REPL
    /// loop, `-c`, script runner) should stop and surface that code. Clears the
    /// flag so a subsequent REPL line starts fresh.
    pub fn take_exit(&mut self) -> Option<i32> {
        self.pending_exit.take()
    }

    /// Install the active leash policy and the principal spawns are evaluated
    /// for (site/content/internals/language-conformance-contract.md). Additive: without this call there is no policy and every
    /// spawn runs unconfined exactly as before. A default-permissive policy
    /// (see [`shoal_leash::Policy::permissive`]) is safe to install — it still
    /// resolves to no OS confinement for a spawn, so normal use never
    /// regresses; only a scoped principal actually restricts a child.
    pub fn set_leash_policy(&mut self, policy: LeashPolicy, principal: impl Into<String>) {
        self.leash = Some((policy, principal.into()));
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
        let (policy, principal) = self.leash.as_ref()?;
        policy.sandbox_for(principal)
    }

    /// Point the user reef scope at a `shoal.toml` whose `[reef]` table becomes
    /// the user scope (site/content/internals/reef-resolution.md). Additive: without it, there is no user scope,
    /// which is the zero-regression default. Changing the cwd next re-discovers
    /// the chain with this path folded in.
    pub fn set_reef_user_manifest(&mut self, path: impl Into<PathBuf>) {
        self.reef_user_manifest = Some(path.into());
        self.reef_chain = None;
    }

    /// Inject the reef provider stack (resolver). Additive: without it the
    /// evaluator lazily builds [`shoal_reef::Resolver::with_defaults`] on the
    /// first constrained resolution. Hosts use this to pin providers; tests use
    /// it to point the resolver at fixture-rooted binaries instead of the real
    /// system.
    pub fn set_reef_resolver(&mut self, resolver: Arc<shoal_reef::Resolver>) {
        self.reef_resolver = Some(resolver);
    }

    /// Install the host's statement renderer (defect #1). Every statement-position
    /// command outcome (and every non-final top-level value) is routed here.
    /// When unset, a built-in default prints to real stdout so scripts behave
    /// without host wiring.
    pub fn set_statement_sink(&mut self, f: StatementSink) {
        self.sink = Some(f);
    }

    /// Bind `it` and append to the session `out` transcript list (REPL hook).
    /// `Var("it")` / `Var("out")` then resolve from the environment normally.
    pub fn record_transcript(&mut self, v: &Value) {
        self.env.declare("it", v.clone(), true);
        let mut out = match self.env.get("out") {
            Some(Value::List(xs)) => xs,
            _ => Vec::new(),
        };
        out.push(v.clone());
        self.env.declare("out", Value::List(out), true);
    }

    /// Route a value to the statement sink (or the default stdout renderer).
    pub(crate) fn emit(&mut self, v: &Value) {
        if let Some(sink) = self.sink.as_mut() {
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
        let total = self.jobs.len();
        let running = self
            .jobs
            .iter()
            .filter(|t| !t.is_done() && !t.is_suspended())
            .count();
        let suspended = self
            .jobs
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
            .jobs
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
        match self.jobs.iter().find(|t| t.id == id) {
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
        match self.jobs.iter().find(|t| t.id == id) {
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
        self.jobs.iter().find(|t| t.id == id).cloned()
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
        self.jobs.push(task);
        self.external_jobs.insert(id, pid);
        self.pending_stop = Some((id, desc));
        id
    }

    /// The child pid of a stopped-external job id, for the REPL `fg`/`bg` path to
    /// locate its parked PTY. `None` if `id` is not a stopped external command.
    pub fn external_job_pid(&self, id: u64) -> Option<u32> {
        self.external_jobs.get(&id).copied()
    }

    /// The most recently registered external command that is currently stopped —
    /// the "current job" a bare `fg`/`bg` (no id) targets, matching the shell
    /// convention. `None` when no external command is stopped.
    pub fn last_stopped_external(&self) -> Option<u64> {
        self.jobs
            .iter()
            .filter(|t| t.is_suspended() && self.external_jobs.contains_key(&t.id))
            .map(|t| t.id)
            .max()
    }

    /// The most recently stopped foreground external command (job id, display),
    /// consumed once by the REPL after each command to print the stop notice.
    pub fn take_pending_stop(&mut self) -> Option<(u64, String)> {
        self.pending_stop.take()
    }

    /// Mark a stopped-external job as running again WITHOUT signalling — the
    /// REPL `fg`/`bg` path performs the SIGCONT + terminal handoff itself, so
    /// this only updates the job-table state. Returns `false` for an unknown id.
    pub fn mark_external_resumed(&self, id: u64) -> bool {
        match self.jobs.iter().find(|t| t.id == id) {
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
        if let Some(t) = self.jobs.iter().find(|t| t.id == id) {
            t.mark_suspended();
            let desc = t.shared.desc.clone();
            self.pending_stop = Some((id, desc));
        }
    }

    /// Retire a stopped-external job once it has finished (its `fg`/`bg` resume
    /// ran to completion): mark the task done so `jobs` shows it terminal, and
    /// drop the pid mapping. Returns `false` for an unknown id.
    pub fn finish_external_job(&mut self, id: u64) -> bool {
        self.external_jobs.remove(&id);
        match self.jobs.iter().find(|t| t.id == id) {
            Some(t) => {
                t.finish(Ok(Value::Null));
                true
            }
            None => false,
        }
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The session's process environment (name → value pairs) — the same
    /// source the in-language `env` builtin reads and that seeds a spawned
    /// child's environment, including any in-session env writes. A read-only
    /// session-state accessor mirroring [`Evaluator::cwd`], used by the
    /// kernel's `shoal://session/env` resource view (site/content/internals/kernel-protocol.md).
    pub fn env_vars(&self) -> &[(OsString, OsString)] {
        &self.process_env
    }

    pub fn set_adapters(&mut self, adapters: AdapterCatalog) {
        self.adapters = adapters;
    }

    pub fn load_bundled_adapters(&mut self) -> Vec<String> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../adapters");
        let (catalog, warnings) = AdapterCatalog::load_dir(&root);
        self.adapters = catalog;
        warnings
    }

    /// Cancel the currently executing foreground process tree.
    pub fn cancel_current(&self) {
        self.cancel.cancel();
    }

    pub fn cancellation_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Install a fresh cancellation epoch before reading the next command.
    pub fn reset_cancel(&mut self) {
        self.cancel = CancelToken::new();
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
    fn cwd(&self) -> PathBuf {
        self.cwd.clone()
    }
}

pub fn eval(program: &Program, cwd: impl AsRef<Path>) -> VResult<Value> {
    Evaluator::new(cwd.as_ref().to_path_buf()).eval_program(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> VResult<Value> {
        let program = shoal_syntax::parse(src).unwrap_or_else(|e| panic!("parse failed: {e}"));
        eval(&program, std::env::current_dir().unwrap())
    }

    /// Evaluate `src` capturing everything routed to the statement sink.
    fn run_capturing(src: &str) -> (VResult<Value>, Vec<Value>) {
        use std::sync::{Arc, Mutex};
        let program = shoal_syntax::parse(src).unwrap_or_else(|e| panic!("parse failed: {e}"));
        let mut ev = Evaluator::new(std::env::current_dir().unwrap());
        let sink: Arc<Mutex<Vec<Value>>> = Arc::default();
        let sink2 = sink.clone();
        ev.set_statement_sink(Box::new(move |v: &Value| {
            sink2.lock().unwrap().push(v.clone())
        }));
        let out = ev.eval_program(&program);
        drop(ev); // release the sink's Arc clone before unwrapping
        let captured = Arc::try_unwrap(sink).unwrap().into_inner().unwrap();
        (out, captured)
    }

    fn run_in(src: &str, cwd: &Path) -> VResult<Value> {
        let program = shoal_syntax::parse(src).unwrap_or_else(|e| panic!("parse failed: {e}"));
        eval(&program, cwd)
    }

    /// The structured `.out` of a captured command outcome.
    fn out_of(v: &Value) -> Value {
        match v {
            Value::Outcome(o) => o.out_value(),
            other => other.clone(),
        }
    }

    #[test]
    fn defect1_nonfinal_and_block_commands_reach_sink() {
        // Non-final top-level statement values pass through to the sink; the
        // final value is returned. Every command now yields an outcome whose
        // `.out` carries the joined echo text (outcome unification, P1a).
        let (out, captured) = run_capturing("echo hi\necho bye");
        assert_eq!(out_of(&out.unwrap()), Value::Str("bye".into()));
        assert_eq!(captured.len(), 1);
        assert_eq!(out_of(&captured[0]), Value::Str("hi".into()));

        // Every iteration of a loop body's bare command reaches the sink.
        let (_out, captured) = run_capturing("for x in [1,2,3] { echo (x) }");
        let texts: Vec<Value> = captured.iter().map(out_of).collect();
        assert_eq!(
            texts,
            vec![
                Value::Str("1".into()),
                Value::Str("2".into()),
                Value::Str("3".into()),
            ]
        );
    }

    /// `render.echo` (site/content/internals/configuration-reference.md): [`EchoMode`] gates which non-final
    /// top-level statement values route to the statement sink. `Quiet`/
    /// `Commands` suppress intermediate pure expressions (`1+1`) but still echo
    /// intermediate bare commands; `All` (the default) echoes every
    /// intermediate. The final value is always returned to the host, never sunk.
    #[test]
    fn echo_mode_gates_intermediate_statement_rendering() {
        use std::sync::{Arc, Mutex};
        let run_mode = |src: &str, mode: EchoMode| -> (VResult<Value>, Vec<Value>) {
            let program = shoal_syntax::parse(src).unwrap();
            let mut ev = Evaluator::new(std::env::current_dir().unwrap());
            ev.set_echo_mode(mode);
            let sink: Arc<Mutex<Vec<Value>>> = Arc::default();
            let sink2 = sink.clone();
            ev.set_statement_sink(Box::new(move |v: &Value| {
                sink2.lock().unwrap().push(v.clone())
            }));
            let out = ev.eval_program(&program);
            drop(ev);
            (out, Arc::try_unwrap(sink).unwrap().into_inner().unwrap())
        };
        let sunk = |captured: &[Value]| captured.iter().map(out_of).collect::<Vec<_>>();

        // Quiet: the intermediate `1+1` is NOT sunk; the intermediate `echo hi`
        // (a bare command) still is; the final `42` is returned, never sunk.
        let (out, captured) = run_mode("1+1\necho hi\n42", EchoMode::Quiet);
        assert_eq!(out.unwrap(), Value::Int(42));
        assert_eq!(sunk(&captured), vec![Value::Str("hi".into())]);

        // Commands: same intermediate gate as Quiet (only bare commands echo).
        let (out, captured) = run_mode("1+1\necho hi\n42", EchoMode::Commands);
        assert_eq!(out.unwrap(), Value::Int(42));
        assert_eq!(sunk(&captured), vec![Value::Str("hi".into())]);

        // All (the default): every intermediate is sunk — the `1+1` too.
        let (out, captured) = run_mode("1+1\necho hi\n42", EchoMode::All);
        assert_eq!(out.unwrap(), Value::Int(42));
        assert_eq!(
            sunk(&captured),
            vec![Value::Int(2), Value::Str("hi".into())]
        );
    }

    /// Decision 2: the in-language `config` namespace reads the host-INJECTED
    /// snapshot (`set_config`), never a `shoal.toml` walked off the filesystem.
    /// So an injected snapshot wins over an on-disk file, and with NO snapshot
    /// the answer is `null` (no filesystem fallback) — the kernel-less/test
    /// default that keeps `config.get` for an unset key behaving as before.
    #[test]
    fn config_namespace_reads_injected_snapshot_not_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        // An on-disk shoal.toml the OLD filesystem-walking implementation would
        // have read; it must be ignored by both paths below.
        std::fs::write(dir.path().join("shoal.toml"), "greeting = \"from-disk\"\n").unwrap();

        let get = shoal_syntax::parse("config.get(\"greeting\")").unwrap();

        // Injected snapshot → `config.get` reads THAT, not the on-disk file.
        let mut ev = Evaluator::new(dir.path().to_path_buf());
        let mut rec = Record::new();
        rec.insert("greeting".into(), Value::Str("from-snapshot".into()));
        ev.set_config(Arc::new(ConfigSnapshot::new(Value::Record(rec))));
        assert_eq!(
            ev.eval_program(&get).unwrap(),
            Value::Str("from-snapshot".into())
        );

        // No snapshot injected → degrades to null; does NOT fall back to the
        // on-disk shoal.toml sitting in the cwd.
        let mut ev2 = Evaluator::new(dir.path().to_path_buf());
        assert_eq!(ev2.eval_program(&get).unwrap(), Value::Null);

        // `config.all()` returns the whole injected snapshot record.
        let all = shoal_syntax::parse("config.all()").unwrap();
        let mut ev3 = Evaluator::new(dir.path().to_path_buf());
        let mut rec = Record::new();
        rec.insert("k".into(), Value::Int(7));
        ev3.set_config(Arc::new(ConfigSnapshot::new(Value::Record(rec.clone()))));
        assert_eq!(ev3.eval_program(&all).unwrap(), Value::Record(rec));
    }

    #[test]
    fn outcome_unification_builtin_out_and_ok() {
        // A builtin is an outcome: `.out` is its structured result, `.ok` true.
        assert_eq!(run("(echo hi).out").unwrap(), Value::Str("hi".into()));
        assert_eq!(run("(echo hi).ok").unwrap(), Value::Bool(true));
        // Unknown fields forward to `.out` (stat record → `.size`).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"xyz").unwrap();
        assert_eq!(run_in("(stat a).size", dir.path()).unwrap(), Value::Size(3));
    }

    #[test]
    fn outcome_unification_and_or_compose_commands() {
        // `echo a && echo b` prints BOTH (P1d): `a` via the sink, `b` returned.
        let (out, captured) = run_capturing("echo a && echo b");
        assert_eq!(out_of(&out.unwrap()), Value::Str("b".into()));
        assert_eq!(
            captured.iter().map(out_of).collect::<Vec<_>>(),
            vec![Value::Str("a".into())]
        );
        // A three-stage chain prints every stage.
        let (out, captured) = run_capturing("echo a && echo b && echo c");
        assert_eq!(out_of(&out.unwrap()), Value::Str("c".into()));
        assert_eq!(
            captured.iter().map(out_of).collect::<Vec<_>>(),
            vec![Value::Str("a".into()), Value::Str("b".into())]
        );
        // `||` recovers from a failed command without raising.
        let out = run("sh { exit 1 } || echo x").unwrap();
        assert_eq!(out_of(&out), Value::Str("x".into()));
    }

    #[test]
    fn outcome_forwards_collection_methods() {
        // `ls` is an outcome; `.where`/`.sort`/`.first(n)`/`.map` forward to its
        // `.out` table (outcome unification P1b + first(n) arity fix).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big"), vec![0u8; 2048]).unwrap();
        std::fs::write(dir.path().join("small"), b"x").unwrap();
        let names = run_in("ls.where(.size > 1b).sort(.name).map(.name)", dir.path()).unwrap();
        assert_eq!(names, Value::List(vec![Value::Path("big".into())]));
        // `.first(2)` returns a LIST of two, chainable into `.map`.
        std::fs::write(dir.path().join("mid"), vec![0u8; 4]).unwrap();
        let first_two = run_in("ls.sort(.name).first(2).map(.name)", dir.path()).unwrap();
        assert!(matches!(first_two, Value::List(xs) if xs.len() == 2));
    }

    #[test]
    fn double_echo_fixed_and_bare_echo_blank_line() {
        // A fn whose last body statement is a bare command prints ONCE: the
        // trailing command is the block value, not also sunk.
        let (out, captured) = run_capturing("fn g(){ echo hi }\ng()");
        assert_eq!(out_of(&out.unwrap()), Value::Str("hi".into()));
        assert!(
            captured.is_empty(),
            "trailing command must not double-print: {captured:?}"
        );
        // Bare `echo` emits a blank line: its outcome stdout is "\n".
        let (_out, captured) = run_capturing("echo\n42");
        assert_eq!(captured.len(), 1);
        match &captured[0] {
            Value::Outcome(o) => assert_eq!(&*o.stdout, b"\n"),
            other => panic!("expected outcome, got {other:?}"),
        }
    }

    #[test]
    fn top_level_ls_renders_as_table() {
        // An outcome with a structured `.out` renders as that structure (P1c).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("only"), b"x").unwrap();
        let v = run_in("ls", dir.path()).unwrap();
        let rendered = shoal_value::render::render_block(&v, 80);
        assert!(
            rendered.contains("name"),
            "ls should render a table: {rendered:?}"
        );
        assert!(
            rendered.contains("only"),
            "ls table should list the file: {rendered:?}"
        );
    }

    #[test]
    fn defect3_forced_command_still_resolves_session_fn() {
        assert_eq!(
            run("fn greet(n:str){ (n) }\n^greet world").unwrap(),
            Value::Str("world".into())
        );
    }

    #[test]
    fn defect4_stat_modified_is_datetime() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"x").unwrap();
        let v = run_in("stat a", dir.path()).unwrap();
        let Value::Record(r) = out_of(&v) else {
            panic!("stat should be a record")
        };
        assert!(
            matches!(r.get("modified"), Some(Value::DateTime(_))),
            "modified must be a DateTime, got {:?}",
            r.get("modified")
        );
    }

    #[test]
    fn defect5_command_resolves_in_value_position() {
        // `let r = ls` invokes the builtin zero-arg in value position.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"x").unwrap();
        let v = run_in("let r = ls\nr", dir.path()).unwrap();
        // `ls` now yields an outcome; its `.out` is the table (P1a).
        assert!(matches!(out_of(&v), Value::Table(rows) if rows.len() == 1));
    }

    #[test]
    fn defect5_env_field_read_via_command() {
        // `env.PATH` reads by invoking the `env` builtin then projecting.
        unsafe { std::env::set_var("SHOAL_TEST_VAR", "hello") };
        let v = run("env.SHOAL_TEST_VAR").unwrap();
        assert_eq!(v, Value::Str("hello".into()));
    }

    #[test]
    fn defect8_redirect_applies_to_builtin() {
        let dir = tempfile::tempdir().unwrap();
        run_in("echo hi > b.txt", dir.path()).unwrap();
        let body = std::fs::read_to_string(dir.path().join("b.txt")).unwrap();
        assert_eq!(body, "hi\n");
    }

    #[test]
    fn defect9_recursion_guard_returns_error() {
        // Run on a large stack so the depth guard fires before the native stack
        // overflows (the real binary evaluates on a big main-thread stack).
        let code = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024 * 1024)
            .spawn(|| run("fn rec(n:int){ rec(n) }\nrec(1)").unwrap_err().code)
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(code, "recursion_limit");
    }

    #[test]
    fn defect10_cd_inside_fn_body_is_rejected() {
        let err = run("fn f(){ cd / }\nf()").unwrap_err();
        assert_eq!(err.code, "custom");
        assert!(err.msg.contains("with cwd:"), "{}", err.msg);
    }

    #[test]
    fn defect11_env_assignment_writes_session_env() {
        use shoal_ast::*;
        let s = Span::default();
        let target = Expr::Field {
            recv: Box::new(Expr::Var {
                name: "env".into(),
                span: s,
            }),
            name: "SHOAL_ASSIGNED".into(),
            optional: false,
            span: s,
        };
        let program = Program {
            stmts: vec![
                Stmt::Assign {
                    target,
                    op: AssignOp::Set,
                    value: Expr::Str {
                        value: "bar".into(),
                        span: s,
                    },
                    span: s,
                },
                Stmt::Expr {
                    expr: Expr::Field {
                        recv: Box::new(Expr::Var {
                            name: "env".into(),
                            span: s,
                        }),
                        name: "SHOAL_ASSIGNED".into(),
                        optional: false,
                        span: s,
                    },
                    span: s,
                },
            ],
        };
        let v = eval(&program, std::env::current_dir().unwrap()).unwrap();
        assert_eq!(v, Value::Str("bar".into()));
    }

    #[test]
    fn defect11_env_assignment_rejected_in_fn_body() {
        use shoal_ast::*;
        let s = Span::default();
        let assign = Stmt::Assign {
            target: Expr::Field {
                recv: Box::new(Expr::Var {
                    name: "env".into(),
                    span: s,
                }),
                name: "X".into(),
                optional: false,
                span: s,
            },
            op: AssignOp::Set,
            value: Expr::Str {
                value: "1".into(),
                span: s,
            },
            span: s,
        };
        // fn f() { env.X = "1" }  then  f()
        let decl = FnDecl {
            name: "f".into(),
            params: vec![],
            rest: None,
            ret: None,
            body: Block {
                stmts: vec![assign],
                span: s,
            },
            doc: None,
            exported: false,
            span: s,
        };
        let program = Program {
            stmts: vec![
                Stmt::Fn { decl },
                Stmt::Expr {
                    expr: Expr::FnCall {
                        name: "f".into(),
                        args: Args::empty(),
                        span: s,
                    },
                    span: s,
                },
            ],
        };
        let err = eval(&program, std::env::current_dir().unwrap()).unwrap_err();
        assert!(err.msg.contains("with env:"), "{}", err.msg);
    }

    #[test]
    fn defect12_builtin_word_coercion() {
        // `sleep 0ms` binds the word to a duration; `sleep 0` to seconds. The
        // builtin now yields an outcome whose `.out` is null (P1a).
        assert_eq!(out_of(&run("sleep 0ms").unwrap()), Value::Null);
        assert_eq!(out_of(&run("sleep 0").unwrap()), Value::Null);
    }

    #[test]
    fn defect12_fn_param_word_coercion() {
        // A bare CMD word binds to a typed fn param.
        let v = run("fn add1(n: int) { n + 1 }\nadd1 41").unwrap();
        assert_eq!(v, Value::Int(42));
    }

    #[test]
    fn defect12_help_synthesis_returns_null() {
        let (out, captured) = run_capturing("fn deploy(env: str) { (env) }\ndeploy --help");
        assert_eq!(out.unwrap(), Value::Null);
        assert!(
            matches!(captured.last(), Some(Value::Str(s)) if s.contains("deploy") && s.contains("env")),
            "{captured:?}"
        );
    }

    #[test]
    fn defect14_task_methods_and_jobs() {
        assert_eq!(
            run("let t = spawn { 2 + 3 }\nt.await()").unwrap(),
            Value::Int(5)
        );
        let is_done = run("let t = spawn { 7 }\nt.await()\nt.is_done()").unwrap();
        assert_eq!(is_done, Value::Bool(true));
        // `jobs` returns the registry table.
        let jobs = run("spawn { 1 }\njobs").unwrap();
        assert!(matches!(jobs, Value::Table(rows) if !rows.is_empty()));
    }

    #[test]
    fn jobs_snapshot_counts_running_and_total() {
        let mut ev = Evaluator::new(std::env::current_dir().unwrap());
        // Nothing spawned yet: a sane, zero-I/O empty snapshot.
        let empty = ev.jobs_snapshot();
        assert_eq!(empty.total, 0);
        assert_eq!(empty.running, 0);
        assert_eq!(empty.suspended, 0);

        // Awaiting every spawned task deterministically drives them to done,
        // so the post-await snapshot is a stable total/zero-running count.
        let prog = shoal_syntax::parse(
            "let a = spawn { 1 + 1 }\nlet b = spawn { 2 + 2 }\na.await()\nb.await()",
        )
        .unwrap();
        ev.eval_program(&prog).unwrap();
        let snap = ev.jobs_snapshot();
        assert_eq!(snap.total, 2, "both spawned tasks are registered");
        assert_eq!(snap.running, 0, "both were awaited to completion");
    }

    #[test]
    fn echo_renders_non_scalar_values() {
        let v = run("let items = [1,2,3]\necho (items)").unwrap();
        assert_eq!(out_of(&v), Value::Str("[1, 2, 3]".into()));
    }

    #[test]
    fn record_transcript_binds_it_and_out() {
        // `it`/`out` are REPL-only at parse time, so this transcript test
        // parses in REPL context.
        let repl = |src: &str| {
            shoal_syntax::parse_with_ctx(
                src,
                shoal_syntax::ParseCtx {
                    repl: true,
                    ..Default::default()
                },
            )
            .unwrap()
        };
        let mut ev = Evaluator::new(std::env::current_dir().unwrap());
        ev.record_transcript(&Value::Int(7));
        ev.record_transcript(&Value::Str("hi".into()));
        let it = ev.eval_program(&repl("it")).unwrap();
        assert_eq!(it, Value::Str("hi".into()));
        let out = ev.eval_program(&repl("out")).unwrap();
        assert_eq!(
            out,
            Value::List(vec![Value::Int(7), Value::Str("hi".into())])
        );
    }

    #[test]
    fn builtin_retry_and_parallel_and_save() {
        assert_eq!(run("retry(3, () => 42)").unwrap(), Value::Int(42));
        assert_eq!(
            run("parallel(() => 1, () => 2)").unwrap(),
            Value::List(vec![Value::Int(1), Value::Int(2)])
        );
        let dir = tempfile::tempdir().unwrap();
        run_in("save(\"out.txt\", \"payload\")", dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "payload"
        );
    }

    #[test]
    fn builtin_retry_eventually_surfaces_error() {
        let err = run("retry(2, () => missing_command_xyz)").unwrap_err();
        assert!(
            err.code == "undefined_var" || err.code == "not_found",
            "{}",
            err.code
        );
    }

    #[test]
    fn arithmetic_and_binding() {
        assert_eq!(run("let x = 2 + 3\nx * 4").unwrap(), Value::Int(20));
    }

    #[test]
    fn strict_conditions_and_short_circuit() {
        assert_eq!(
            run("false && missing\ntrue || missing").unwrap(),
            Value::Bool(true)
        );
        assert_eq!(run("if true { 7 } else { 9 }").unwrap(), Value::Int(7));
        assert_eq!(run("if [1] { 2 }").unwrap_err().code, "type_error");
    }

    #[test]
    fn functions_are_callable() {
        assert_eq!(
            run("fn twice(x: int) { x * 2 }\ntwice(21)").unwrap(),
            Value::Int(42)
        );
    }

    #[test]
    fn captured_external_outcome_is_structured() {
        let value = run("let r = sh { printf hello }\nr.out").unwrap();
        assert_eq!(value, Value::Str("hello".into()));
    }

    #[test]
    fn failed_statement_preserves_process_diagnostics() {
        let err = run("sh { printf boom >&2; exit 7 }").unwrap_err();
        assert_eq!(err.code, "cmd_failed");
        assert_eq!(err.status, Some(7));
        assert_eq!(err.stderr.as_deref(), Some("boom"));
    }

    #[test]
    fn typed_builtins_dispatch_before_path() {
        let dir = tempfile::tempdir().unwrap();
        let program = shoal_syntax::parse("touch a\nls").unwrap();
        let value = out_of(&eval(&program, dir.path()).unwrap());
        assert!(
            matches!(value, Value::Table(rows) if rows.len() == 1 && rows[0]["name"] == Value::Path("a".into()))
        );

        let rm = shoal_syntax::parse("rm a").unwrap();
        let value = out_of(&eval(&rm, dir.path()).unwrap());
        assert!(
            matches!(value, Value::List(rows) if matches!(&rows[0], Value::Record(r) if matches!(r.get("trash"), Some(Value::Path(_)))))
        );
        assert!(!dir.path().join("a").exists());
    }

    fn adapter_eval(toml: &str, src: &str) -> VResult<Value> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fixture.toml"), toml).unwrap();
        let (catalog, warnings) = AdapterCatalog::load_dir(dir.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut evaluator = Evaluator::new(dir.path().into());
        evaluator.set_adapters(catalog);
        evaluator.eval_program(&shoal_syntax::parse(src).unwrap())
    }

    #[test]
    fn adapters_rewrite_parse_and_honor_ok_codes() {
        let lines = adapter_eval(
            r#"[cmd.fixture]
bin="/usr/bin/printf"
invoke=["one\ntwo\n"]
output={parse="lines",type="list<str>"}
"#,
            "fixture",
        )
        .unwrap();
        assert!(
            matches!(lines, Value::Outcome(o) if o.out_value() == Value::List(vec![Value::Str("one".into()), Value::Str("two".into())]))
        );

        let accepted = adapter_eval(
            r#"[cmd.accept]
bin="/bin/sh"
ok_codes=[0,1]
invoke=["-c","exit 1"]
"#,
            "accept",
        )
        .unwrap();
        assert!(matches!(accepted, Value::Outcome(o) if o.ok && o.status == Some(1)));
    }

    #[test]
    fn adapter_typed_flags_fail_before_spawn() {
        let error = adapter_eval(
            r#"[cmd.typed]
bin="/usr/bin/printf"
params={jobs="int"}
"#,
            "typed --jobs=nope",
        )
        .unwrap_err();
        assert_eq!(error.code, "arg_error");
        assert!(error.msg.contains("expected int"));
    }

    #[test]
    fn adapter_consumed_flag_never_reaches_argv() {
        // Regression for the git-status porcelain corruption (shoal-adapters'
        // `consumed` rule, defect fix): `--short`/`-s` must stay a
        // recognized, validated flag but never be appended to argv, since
        // git's `--porcelain=v2` parser assumes an exact byte layout and
        // `--short` (last-wins) silently switches git to a different,
        // incompatible output format.
        let toml = r#"[cmd.fixture]
bin="/bin/echo"

[cmd.fixture.sub.status]
params = { short = "bool", branch = "bool" }
flags = { short = { s = "short", b = "branch" } }
invoke = ["status", "--porcelain=v2"]
consumed = ["short", "branch"]
"#;

        let long = adapter_eval(toml, "fixture status --short").unwrap();
        let Value::Outcome(o) = long else {
            panic!("expected outcome, got {long:?}")
        };
        assert_eq!(
            String::from_utf8(o.stdout.to_vec()).unwrap().trim(),
            "status --porcelain=v2",
            "--short must be accepted but dropped from argv"
        );

        let short = adapter_eval(toml, "fixture status -s").unwrap();
        let Value::Outcome(o) = short else {
            panic!("expected outcome, got {short:?}")
        };
        assert_eq!(
            String::from_utf8(o.stdout.to_vec()).unwrap().trim(),
            "status --porcelain=v2",
            "-s must be accepted but dropped from argv"
        );
    }

    #[test]
    fn forced_head_bypasses_adapter() {
        // `^name` reaches the real command (language card): a forced head
        // must skip the adapter's flag/signature gate entirely. The corpus
        // runner carries no adapters, so this lives here.
        let toml = r#"[cmd.zzzfixture]
bin="zzzfixture-no-such-binary"

[cmd.zzzfixture.sub.log]
params = { follow = "bool" }
"#;
        // Unforced: the adapter gate rejects the unknown flag before spawn.
        let err = adapter_eval(toml, "zzzfixture log --oneline").unwrap_err();
        assert_eq!(err.code, "arg_error");
        assert!(err.msg.contains("unknown flag --oneline"));
        // Forced: dispatch bypasses the adapter and reaches PATH resolution
        // (`not_found` here — the bin doesn't exist — proving the adapter's
        // arg_error gate never ran).
        let err = adapter_eval(toml, "^zzzfixture log --oneline").unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn single_char_adapter_param_emits_posix_single_dash() {
        // A single-character param (git log's `n`) must reach the child as
        // `-n`, not `--n` — the adapter used to validate `--n` and then
        // forward it verbatim, which the real tool rejects, leaving the
        // adapter's own advertised flag unusable. printf echoes its argv
        // back so the emitted spelling is directly observable.
        let toml = r#"[cmd.fixture]
bin="/usr/bin/printf"
invoke=["%s %s"]
params={ n = "int?" }
output={parse="lines",type="list<str>"}
"#;
        for src in ["fixture --n=2", "fixture --n 2"] {
            let out = adapter_eval(toml, src).unwrap();
            let Value::Outcome(o) = out else {
                panic!("expected outcome");
            };
            assert_eq!(
                o.out_value(),
                Value::List(vec![Value::Str("-n 2".into())]),
                "{src} argv spelling"
            );
        }
    }

    #[test]
    fn planning_derives_exact_builtin_paths_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"a").unwrap();
        let mut evaluator = Evaluator::new(dir.path().into());
        let program = shoal_syntax::parse("cp a b\nrm a").unwrap();
        let plan = evaluator.plan_program(&program).unwrap();
        assert!(plan.effects.contains(&Effect::FsRead {
            paths: vec![dir.path().join("a")]
        }));
        assert!(plan.effects.contains(&Effect::FsWrite {
            paths: vec![dir.path().join("b")]
        }));
        assert!(plan.effects.contains(&Effect::FsDelete {
            paths: vec![dir.path().join("a")]
        }));
        assert!(dir.path().join("a").exists());
        assert!(!dir.path().join("b").exists());
    }

    #[test]
    fn planning_substitutes_adapter_effects() {
        let dir = tempfile::tempdir().unwrap();
        let mut evaluator = Evaluator::new(dir.path().into());
        assert!(evaluator.load_bundled_adapters().is_empty());
        let plan = evaluator
            .plan_program(&shoal_syntax::parse("git push origin main").unwrap())
            .unwrap();
        assert!(plan.effects.contains(&Effect::FsRead {
            paths: vec![dir.path().into()]
        }));
        assert!(plan.effects.contains(&Effect::NetConnect {
            host: "origin".into(),
            port: 443
        }));
        assert!(
            plan.effects
                .iter()
                .any(|e| matches!(e, Effect::ProcSpawn { argv0, .. } if argv0 == "git"))
        );
    }

    #[test]
    fn planning_unknown_and_sh_are_opaque_and_spawn_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let src = format!("unknown-command\nsh {{ touch {} }}", marker.display());
        let mut evaluator = Evaluator::new(dir.path().into());
        let plan = evaluator
            .plan_program(&shoal_syntax::parse(&src).unwrap())
            .unwrap();
        assert!(plan.effects.contains(&Effect::Opaque));
        assert!(!marker.exists());
    }

    // ---- site/content/internals/language-conformance-contract.md binary-content-hash spawn pinning ------------------------

    /// `hash_resolved_bin` must produce reef/leash's exact blake3-hex so a pin
    /// an author copies from `reef`/`which` output compares equal to what the
    /// spawn gate computes. Cross-check against all three producers.
    #[test]
    fn hash_resolved_bin_matches_reef_and_leash_encoding() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("toolbin");
        std::fs::write(&bin, b"#!/bin/sh\necho hi\n").unwrap();
        let ev = Evaluator::new(dir.path().into());
        let got = ev
            .hash_resolved_bin(OsStr::new(bin.as_os_str()))
            .expect("absolute path is hashable");
        assert!(!got.is_empty());
        // Same as hashing the bytes directly (reef's `hash_bytes`)…
        assert_eq!(
            got,
            shoal_reef::hashcache::hash_bytes(b"#!/bin/sh\necho hi\n")
        );
        // …and as reef's file-hash cache…
        assert_eq!(
            got,
            shoal_reef::hashcache::HashCache::new()
                .hash_file(&bin)
                .unwrap()
        );
        // …and as leash's own preflight hasher (the exec-time verifier).
        assert_eq!(got, shoal_leash::preflight_spawn(&bin, &[]).unwrap().hash);
    }

    /// The security-critical gate, exercised directly (a full external spawn is
    /// awkward in-harness): no policy and no-`proc_spawn` policy both allow every
    /// spawn (the no-regression guarantee); a pinned allowlist admits only the
    /// matching binary and denies an unlisted one.
    #[test]
    fn spawn_gate_no_regression_then_enforces_when_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("toolbin");
        std::fs::write(&bin, b"real binary bytes").unwrap();
        let bin_os = OsStr::new(bin.as_os_str());
        let hash = shoal_reef::hashcache::hash_bytes(b"real binary bytes");

        // 1. No leash policy installed at all ⇒ allow (today's behavior).
        let ev = Evaluator::new(dir.path().into());
        assert!(ev.spawn_gate(bin_os, None, Span::default()).is_ok());

        // 2. Permissive policy (no `proc_spawn` grants) ⇒ allow. This is the
        //    default a human principal gets; a regression here would break the
        //    shell for everyone.
        let mut ev = Evaluator::new(dir.path().into());
        ev.set_leash_policy(LeashPolicy::permissive("human"), "human");
        assert!(ev.spawn_gate(bin_os, None, Span::default()).is_ok());

        // 3. Scoped fs policy but still no `proc_spawn` grants ⇒ allow.
        let mut ev = Evaluator::new(dir.path().into());
        ev.set_leash_policy(
            LeashPolicy::from_toml(
                "[principal.agent]\n\n[principal.agent.fs]\nread=[\"/work/**\"]\n",
            )
            .unwrap(),
            "agent",
        );
        assert!(ev.spawn_gate(bin_os, None, Span::default()).is_ok());

        // 4. Pinned to this binary's exact hash ⇒ allow it (hashed here, since
        //    reef didn't resolve it — reef_hash is None).
        let mut ev = Evaluator::new(dir.path().into());
        ev.set_leash_policy(
            LeashPolicy::from_toml(&format!("[principal.agent]\nproc_spawn = [\"{hash}\"]\n"))
                .unwrap(),
            "agent",
        );
        assert!(ev.spawn_gate(bin_os, None, Span::default()).is_ok());

        // 5. Pinned to a DIFFERENT hash (and the name is not listed) ⇒ deny.
        let mut ev = Evaluator::new(dir.path().into());
        ev.set_leash_policy(
            LeashPolicy::from_toml(&format!(
                "[principal.agent]\nproc_spawn = [\"{}\"]\n",
                "00".repeat(32)
            ))
            .unwrap(),
            "agent",
        );
        let err = ev
            .spawn_gate(bin_os, None, Span::default())
            .expect_err("unlisted binary must be denied under an active pin");
        assert_eq!(err.code, "spawn_denied");

        // 6. Reusing reef's already-computed hash takes the same allow path
        //    without touching the file (pass a bogus path but the real hash).
        let mut ev = Evaluator::new(dir.path().into());
        ev.set_leash_policy(
            LeashPolicy::from_toml(&format!("[principal.agent]\nproc_spawn = [\"{hash}\"]\n"))
                .unwrap(),
            "agent",
        );
        assert!(
            ev.spawn_gate(
                OsStr::new("/nonexistent/tool"),
                Some(&hash),
                Span::default()
            )
            .is_ok()
        );
    }

    /// `plan_derive` now emits a real, non-empty `bin_hash` for an adapter whose
    /// bin resolves to a real file — the content hash a `proc_spawn` pin checks.
    #[test]
    fn planning_emits_real_bin_hash_for_resolved_adapter() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("toolbin");
        let body = b"fixture tool bytes for planning";
        std::fs::write(&bin, body).unwrap();
        // An adapter whose `bin` is the absolute fixture path (host-independent).
        std::fs::write(
            dir.path().join("mytool.toml"),
            format!("[cmd.mytool]\nbin=\"{}\"\n", bin.display()),
        )
        .unwrap();
        let (catalog, warnings) = AdapterCatalog::load_dir(dir.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut evaluator = Evaluator::new(dir.path().into());
        evaluator.set_adapters(catalog);
        let plan = evaluator
            .plan_program(&shoal_syntax::parse("mytool").unwrap())
            .unwrap();
        let spawn = plan
            .effects
            .iter()
            .find_map(|e| match e {
                Effect::ProcSpawn { bin_hash, .. } => Some(bin_hash.clone()),
                _ => None,
            })
            .expect("adapter spawn effect present");
        assert!(!spawn.is_empty(), "bin_hash must no longer be empty");
        assert_eq!(spawn, shoal_reef::hashcache::hash_bytes(body));
    }

    #[test]
    fn planning_unions_conditional_and_static_function_effects() {
        let dir = tempfile::tempdir().unwrap();
        let src = "fn cleanup() { rm old }\nif true { cleanup() } else { touch new }";
        let mut evaluator = Evaluator::new(dir.path().into());
        let parsed = shoal_syntax::parse(src).unwrap();
        let plan = evaluator.plan_program(&parsed).unwrap();
        assert!(plan.effects.contains(&Effect::FsDelete {
            paths: vec![dir.path().join("old")]
        }));
        assert!(plan.effects.contains(&Effect::FsWrite {
            paths: vec![dir.path().join("new")]
        }));
    }

    // --- match: type / record / list patterns (site/content/internals/language-conformance-contract.md) -----------------

    #[test]
    fn match_type_pattern_binds_and_falls_through() {
        assert_eq!(
            run(r#"match 5 { int n => "int:{n}"; _ => "other" }"#).unwrap(),
            Value::Str("int:5".into())
        );
        assert_eq!(
            run(r#"match "hi" { str s => "str:{s}"; _ => "other" }"#).unwrap(),
            Value::Str("str:hi".into())
        );
        // A type mismatch falls through to the next arm.
        assert_eq!(
            run(r#"match "hi" { int n => "int:{n}"; str s => "str:{s}" }"#).unwrap(),
            Value::Str("str:hi".into())
        );
        // A bare type name with no binder is a plain bind (matches anything).
        assert_eq!(
            run(r#"match 5 { int => 1; _ => 0 }"#).unwrap(),
            Value::Int(1)
        );
    }

    #[test]
    fn match_record_pattern_shorthand_sub_and_open() {
        assert_eq!(
            run(r#"match {name: "ada", age: 30} { {name, age} => "{name} is {age}"; _ => "no" }"#)
                .unwrap(),
            Value::Str("ada is 30".into())
        );
        // Nested record sub-pattern.
        assert_eq!(
            run("match {point: {x: 1, y: 2}} { {point: {x, y}} => x + y; _ => 0 }").unwrap(),
            Value::Int(3)
        );
        // Missing field falls through (open matching only ignores *extra*).
        assert_eq!(
            run(r#"match {name: "ada"} { {name, age} => "has age"; _ => "no age" }"#).unwrap(),
            Value::Str("no age".into())
        );
        // Record + nested list sub-pattern.
        assert_eq!(
            run("match {items: [1, 2, 3]} { {items: [a, b, c]} => a + b + c; _ => 0 }").unwrap(),
            Value::Int(6)
        );
    }

    #[test]
    fn match_record_pattern_guard_composes() {
        assert_eq!(
            run(r#"match {status: 200} { {status} if status >= 200 && status < 300 => "ok"; {status} => "other:{status}" }"#)
                .unwrap(),
            Value::Str("ok".into())
        );
        assert_eq!(
            run(r#"match {status: 404} { {status} if status >= 200 && status < 300 => "ok"; {status} => "other:{status}" }"#)
                .unwrap(),
            Value::Str("other:404".into())
        );
    }

    #[test]
    fn match_list_pattern_arity_rest_and_empty() {
        assert_eq!(
            run("match [1, 2, 3] { [a, b, c] => a + b + c; _ => 0 }").unwrap(),
            Value::Int(6)
        );
        // `...rest` binds the tail as a list.
        assert_eq!(
            run("match [1, 2, 3, 4] { [first, ...rest] => rest.len(); _ => 0 }").unwrap(),
            Value::Int(3)
        );
        // Fixed arity: a length mismatch falls through.
        assert_eq!(
            run(r#"match [1, 2] { [a, b, c] => "three"; [a, b] => "two"; _ => "other" }"#).unwrap(),
            Value::Str("two".into())
        );
        assert_eq!(
            run(r#"match [] { [] => "empty"; _ => "nonempty" }"#).unwrap(),
            Value::Str("empty".into())
        );
        assert_eq!(
            run(r#"match [1] { [] => "empty"; [a] => "one:{a}"; _ => "other" }"#).unwrap(),
            Value::Str("one:1".into())
        );
    }

    #[test]
    fn match_comma_separated_arms_parse() {
        assert_eq!(
            run(r#"match 2 { 1 => "a", 2 => "b", _ => "c" }"#).unwrap(),
            Value::Str("b".into())
        );
    }

    // --- data namespaces ------------------------------------------------------

    #[test]
    fn json_namespace_roundtrips() {
        assert_eq!(run(r#"json.parse('{"a":1}').a"#).unwrap(), Value::Int(1));
        assert_eq!(
            run("json.stringify([1, 2, 3])").unwrap(),
            Value::Str("[1,2,3]".into())
        );
        // A bound name shadows the namespace.
        assert_eq!(run("let json = 7\njson").unwrap(), Value::Int(7));
        // Invalid JSON is an arg_error.
        assert_eq!(
            run(r#"json.parse('{not json}')"#).unwrap_err().code,
            "arg_error"
        );
    }

    #[test]
    fn yaml_and_toml_and_csv_namespaces() {
        // yaml round-trips a scalar map.
        assert_eq!(run("yaml.parse('a: 1').a").unwrap(), Value::Int(1));
        // toml parses a key.
        assert_eq!(run("toml.parse('a = 1').a").unwrap(), Value::Int(1));
        // csv parses a header row into a table of records.
        let v = run(r#"csv.parse("name,age\nada,30")"#).unwrap();
        let Value::Table(rows) = v else {
            panic!("csv.parse should be a table, got {v:?}")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], Value::Str("ada".into()));
        assert_eq!(rows[0]["age"], Value::Str("30".into()));
    }

    #[test]
    fn math_namespace_constants_and_fns() {
        assert_eq!(run("math.sqrt(4)").unwrap(), Value::Float(2.0));
        let Value::Float(pi) = run("math.pi").unwrap() else {
            panic!("math.pi should be a float")
        };
        assert!((pi - std::f64::consts::PI).abs() < 1e-12);
        assert_eq!(run("math.max(3, 7)").unwrap(), Value::Float(7.0));
        assert_eq!(run("math.clamp(9, 0, 5)").unwrap(), Value::Float(5.0));
        // clamp with lo > hi is an arg_error.
        assert_eq!(run("math.clamp(1, 5, 0)").unwrap_err().code, "arg_error");
    }

    #[test]
    fn os_namespace_reports_platform() {
        assert_eq!(
            run("os.platform()").unwrap(),
            Value::Str(std::env::consts::OS.into())
        );
        assert_eq!(
            run("os.arch()").unwrap(),
            Value::Str(std::env::consts::ARCH.into())
        );
        assert_eq!(
            run("os.pid()").unwrap(),
            Value::Int(std::process::id() as i64)
        );
        assert!(matches!(run("os.cpus()").unwrap(), Value::Int(n) if n >= 1));
        assert!(matches!(run("os.env()").unwrap(), Value::Record(_)));
    }

    #[test]
    #[ignore = "requires network access; gated out of CI"]
    fn http_get_is_typed() {
        let v = run(r#"http.get("https://example.com")"#).unwrap();
        let Value::Record(r) = v else { panic!() };
        assert!(matches!(r.get("status"), Some(Value::Int(_))));
        assert!(matches!(r.get("ok"), Some(Value::Bool(_))));
        assert!(matches!(r.get("body"), Some(Value::Str(_))));
    }

    // --- structured builtins head / ln ----------------------------------------

    #[test]
    fn head_returns_first_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"a\nb\nc\nd\n").unwrap();
        assert_eq!(
            out_of(&run_in("head f 2", dir.path()).unwrap()),
            Value::List(vec![Value::Str("a".into()), Value::Str("b".into())])
        );
        // Default n = 10 returns all four.
        assert!(
            matches!(out_of(&run_in("head f", dir.path()).unwrap()), Value::List(xs) if xs.len() == 4)
        );
    }

    #[test]
    fn ln_creates_symlink_and_hardlink() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("orig"), b"data").unwrap();
        run_in("ln --symbolic orig slink", dir.path()).unwrap();
        assert!(
            dir.path()
                .join("slink")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        run_in("ln orig hard", dir.path()).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("hard")).unwrap(),
            b"data".to_vec()
        );
    }

    // --- modules --------------------------------------------------------------

    #[test]
    fn use_binds_module_exports_and_runs_fns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("greet.shl"),
            "export fn hello(who: str) { \"hi {who}\" }\nexport let version = 3\nfn private() { 1 }",
        )
        .unwrap();
        // A module fn runs as a namespaced command.
        assert_eq!(
            run_in("use ./greet\ngreet.hello(\"ada\")", dir.path()).unwrap(),
            Value::Str("hi ada".into())
        );
        // A value export is a field.
        assert_eq!(
            run_in("use ./greet\ngreet.version", dir.path()).unwrap(),
            Value::Int(3)
        );
        // A non-exported decl is not visible.
        assert_eq!(
            run_in("use ./greet\ngreet.private", dir.path())
                .unwrap_err()
                .code,
            "field_missing"
        );
    }

    #[test]
    fn circular_use_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.shl"), "use ./b\nexport let x = 1").unwrap();
        std::fs::write(dir.path().join("b.shl"), "use ./a\nexport let y = 2").unwrap();
        let err = run_in("use ./a", dir.path()).unwrap_err();
        assert_eq!(err.code, "custom");
        assert!(err.msg.contains("circular"), "{}", err.msg);
    }

    #[test]
    fn missing_module_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            run_in("use ./nope", dir.path()).unwrap_err().code,
            "not_found"
        );
    }

    // --- plan / apply / explain -----------------------------------------------

    #[test]
    fn plan_renders_effects_without_running() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x"), b"x").unwrap();
        let v = run_in("plan { rm x }", dir.path()).unwrap();
        let Value::Record(r) = &v else {
            panic!("plan should be a record, got {v:?}")
        };
        // The file is untouched — plan spawns/mutates nothing.
        assert!(dir.path().join("x").exists());
        let Some(Value::List(effects)) = r.get("effects") else {
            panic!("plan record needs effects")
        };
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Value::Str(s) if s.starts_with("delete"))),
            "{effects:?}"
        );
        assert!(matches!(r.get("id"), Some(Value::Int(_))));
    }

    #[test]
    fn apply_runs_a_derived_plan() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x"), b"x").unwrap();
        // `plan { … }` derives (id 1) without mutating; `apply 1` runs it.
        let out = run_in("plan { rm x }\napply 1", dir.path()).unwrap();
        // Now the rm actually ran.
        assert!(!dir.path().join("x").exists());
        assert!(matches!(out_of(&out), Value::List(_)));
    }

    #[test]
    fn explain_reports_effects_of_source() {
        let dir = tempfile::tempdir().unwrap();
        let v = run_in(r#"explain("rm x")"#, dir.path()).unwrap();
        let Value::Record(r) = v else { panic!() };
        assert_eq!(r.get("source"), Some(&Value::Str("rm x".into())));
        assert!(matches!(r.get("effects"), Some(Value::List(_))));
    }

    // --- task suspend / resume ------------------------------------------------

    #[test]
    fn task_suspend_resume_methods_and_entrypoints() {
        // Value-method surface: `.suspend()`/`.resume()`/`.is_suspended()`.
        let v = run("let t = spawn { sleep 0ms\n1 }\nt.suspend()\nt.is_suspended()").unwrap();
        assert_eq!(v, Value::Bool(true));
        let v = run("let t = spawn { sleep 0ms\n1 }\nt.suspend()\nt.resume()\nt.is_suspended()")
            .unwrap();
        assert_eq!(v, Value::Bool(false));

        // Kernel-callable entry points + jobs snapshot accounting.
        let mut ev = Evaluator::new(std::env::current_dir().unwrap());
        let prog = shoal_syntax::parse("spawn { sleep 5s }").unwrap();
        let task = ev.eval_program(&prog).unwrap();
        let Value::Task(t) = task else { panic!() };
        assert!(ev.suspend_task(t.id));
        assert!(t.is_suspended());
        assert_eq!(ev.jobs_snapshot().suspended, 1);
        assert!(ev.resume_task(t.id));
        assert!(!t.is_suspended());
        assert!(!ev.suspend_task(999_999));
        // fg lookup resolves a live task.
        assert!(ev.task_by_id(t.id).is_some());
        t.cancel();
    }

    /// A foreground external command stopped by Ctrl-Z (site/content/internals/language-conformance-contract.md) is recorded
    /// as a `stopped` job that lists alongside spawned tasks, resolves to its
    /// pid for `fg`/`bg`, and walks running↔stopped→done as the REPL drives it —
    /// all without a real process (this test only exercises the jobs-table
    /// bookkeeping, never a SIGTSTP/SIGCONT hook). The underlying OS mechanics
    /// this bookkeeping represents — `WUNTRACED`/`WIFSTOPPED` mapping to a
    /// stopped `ExecResult`, `SIGCONT` resuming a real stopped child to
    /// completion, the `PARKED_JOBS` registry (`take_stopped_job` exactly once,
    /// `shutdown_stopped_jobs` draining without a leak), and the `reaped` guard
    /// against re-signalling an already-reaped/pid-recycled job — are covered
    /// against the OS with real child processes in
    /// `crates/shoal-exec/src/pty.rs`'s own `#[cfg(test)] mod tests`. What
    /// remains untested anywhere (needs a real controlling terminal, so it's a
    /// manual-verification gap, not an automatable one) is the live end-to-end
    /// round trip: a user's actual Ctrl-Z keystroke being turned into `SIGTSTP`
    /// by the pty line discipline, through the REPL prompt, to `fg`/`bg`.
    #[test]
    fn stopped_external_command_lists_and_transitions_in_the_jobs_table() {
        fn job_state(ev: &Evaluator, id: u64) -> Option<String> {
            let Value::Table(rows) = ev.jobs_table() else {
                return None;
            };
            rows.iter()
                .find(|r| matches!(r.get("id"), Some(Value::Int(n)) if *n as u64 == id))
                .and_then(|r| match r.get("state") {
                    Some(Value::Str(s)) => Some(s.clone()),
                    _ => None,
                })
        }

        let mut ev = Evaluator::new(std::env::current_dir().unwrap());
        let id = ev.register_stopped_external(4242, 4242, "sleep 30".into());

        // The pending-stop notice is queued for the REPL exactly once.
        assert_eq!(ev.take_pending_stop(), Some((id, "sleep 30".to_string())));
        assert_eq!(ev.take_pending_stop(), None);

        // It resolves to its pid (for `fg`/`bg`) and shows as `stopped`.
        assert_eq!(ev.external_job_pid(id), Some(4242));
        assert_eq!(job_state(&ev, id).as_deref(), Some("stopped"));
        assert_eq!(ev.jobs_snapshot().suspended, 1);

        // Resuming (`fg`/`bg`) flips it back to running without signalling.
        assert!(ev.mark_external_resumed(id));
        assert_eq!(job_state(&ev, id).as_deref(), Some("running"));
        assert_eq!(ev.jobs_snapshot().running, 1);

        // A re-stop (`fg`'d then Ctrl-Z'd again) re-arms the notice + state.
        ev.mark_external_stopped(id);
        assert_eq!(job_state(&ev, id).as_deref(), Some("stopped"));
        assert_eq!(ev.take_pending_stop(), Some((id, "sleep 30".to_string())));

        // Finishing retires it: `done`, and no longer resolvable for `fg`/`bg`.
        assert!(ev.finish_external_job(id));
        assert_eq!(job_state(&ev, id).as_deref(), Some("done"));
        assert_eq!(ev.external_job_pid(id), None);
        assert!(!ev.mark_external_resumed(999_999), "unknown id is a no-op");
    }

    #[test]
    fn now_and_today_are_live_datetime_anchors() {
        // `now`/`today` (site/content/internals/language-conformance-contract.md) resolve to a datetime, not an undefined var.
        let this_year = jiff::Zoned::now().year() as i64;
        assert_eq!(run("now.year").unwrap(), Value::Int(this_year));
        assert_eq!(run("today.year").unwrap(), Value::Int(this_year));
        assert_eq!(run("now().year").unwrap(), Value::Int(this_year));
        // `today` is midnight: hour/minute/second all zero.
        assert_eq!(run("today.hour").unwrap(), Value::Int(0));
        assert_eq!(run("today.minute").unwrap(), Value::Int(0));
        // A user binding still shadows the anchor name.
        assert_eq!(run("let now = 5\nnow").unwrap(), Value::Int(5));
    }

    #[test]
    fn duration_ago_and_from_now_compose_to_datetime() {
        // `.ago` is in the past, `.from_now` in the future (site/content/internals/language-conformance-contract.md).
        assert!(matches!(run("1h.ago").unwrap(), Value::DateTime(_)));
        assert!(matches!(run("30d.from_now").unwrap(), Value::DateTime(_)));
        // from_now is strictly after ago for the same duration.
        assert_eq!(run("1h.from_now > 1h.ago").unwrap(), Value::Bool(true));
        // Round-trips through datetime arithmetic: now + 1h ~ 1h.from_now.
        assert_eq!(run("1h.from_now > now").unwrap(), Value::Bool(true));
        assert_eq!(run("1h.ago < now").unwrap(), Value::Bool(true));
        // An unknown duration field is still a plain field_missing.
        assert_eq!(run("1h.nope").unwrap_err().code, "field_missing");
    }

    #[test]
    fn assert_builtin_raises_assert_failed() {
        // False condition → assert_failed (site/content/internals/intercrate-protocol-contracts.md).
        let e = run("assert(1 == 2)").unwrap_err();
        assert_eq!(e.code, "assert_failed");
        // Custom message is carried through.
        let e = run(r#"assert(false, "boom")"#).unwrap_err();
        assert_eq!(e.code, "assert_failed");
        assert_eq!(e.msg, "boom");
        // True condition → null, no raise.
        assert_eq!(run("assert(1 == 1)").unwrap(), Value::Null);
        // Command-head spelling works too.
        assert_eq!(run("assert (2 > 1)").unwrap(), Value::Null);
        // Non-bool condition is a type_error, not a silent pass.
        assert_eq!(run("assert(3)").unwrap_err().code, "type_error");
    }

    #[test]
    fn list_path_param_receives_all_glob_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        // A non-variadic `list<path>` param gets every sorted match (site/content/internals/language-conformance-contract.md).
        let v = run_in(
            "fn showpaths(paths: list<path>) { paths.len() }\nshowpaths *.txt",
            dir.path(),
        )
        .unwrap();
        assert_eq!(v, Value::Int(2));
    }

    #[test]
    fn glob_excludes_dotfiles_by_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".hidden.txt"), "").unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        // Plain `*.txt` skips `.hidden.txt` (site/content/internals/language-conformance-contract.md): 2, not 3.
        let v = run_in(
            "fn f(...rest: list<path>) { rest.len() }\nf *.txt",
            dir.path(),
        )
        .unwrap();
        assert_eq!(v, Value::Int(2));
        // A dot-leading pattern opts back in.
        let v = run_in(
            "fn f(...rest: list<path>) { rest.len() }\nf .*.txt",
            dir.path(),
        )
        .unwrap();
        assert_eq!(v, Value::Int(1));
    }

    #[test]
    fn alias_appends_later_flags_to_adapter_call() {
        // `alias gs = git status; (gs --short).cmd` must carry `--short`
        // through to the resolved argv (site/content/internals/language-conformance-contract.md), not drop it.
        let v = run("alias gs = git status\n(gs --short).cmd").unwrap();
        assert_eq!(v, Value::Str("git status --short".into()));
    }

    #[test]
    fn run_unresolvable_extension_raises_runner_not_found() {
        // No `[runners]` entry and no shebang for `.zzz` → runner_not_found
        // (site/content/internals/values-streams-execution.md step 3), not a bare filesystem not_found.
        let e = run(r#"run("./definitely-not-a-real-script-xyz.zzz")"#).unwrap_err();
        assert_eq!(e.code, "runner_not_found");
    }

    #[test]
    fn background_ampersand_yields_a_task() {
        // `cmd &` desugars to `spawn { cmd }` (site/content/internals/language-conformance-contract.md): yields a task.
        let v = run("let t = (echo hi &)\nt.await()\nt.is_done()").unwrap();
        assert_eq!(v, Value::Bool(true));
        // Value-position `&` produces a task handle directly.
        assert!(matches!(run("(echo hi &)").unwrap(), Value::Task(_)));
        // The awaited task's outcome is the command's stdout.
        assert_eq!(
            run("let t = (echo hi &)\nt.await().out").unwrap(),
            Value::Str("hi".into())
        );
    }

    #[test]
    fn path_filesystem_methods_read_lines_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.txt"), b"alpha\nbeta\r\ngamma\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();

        // `.read()` resolves relative to cwd and returns the whole file as str.
        assert_eq!(
            run_in(r#"path("data.txt").read()"#, dir.path()).unwrap(),
            Value::Str("alpha\nbeta\r\ngamma\n".into())
        );
        // `.read_bytes()` yields raw bytes.
        assert!(matches!(
            run_in(r#"path("data.txt").read_bytes()"#, dir.path()).unwrap(),
            Value::Bytes(b) if b.len() == 18
        ));
        // `.lines()` splits and strips CR, and composes with list methods.
        assert_eq!(
            run_in(r#"path("data.txt").lines()"#, dir.path()).unwrap(),
            Value::List(vec![
                Value::Str("alpha".into()),
                Value::Str("beta".into()),
                Value::Str("gamma".into()),
            ])
        );
        assert_eq!(
            run_in(r#"path("data.txt").lines().first(2)"#, dir.path()).unwrap(),
            Value::List(vec![Value::Str("alpha".into()), Value::Str("beta".into())])
        );
        // `.exists()`/`.is_file()`/`.is_dir()`.
        assert_eq!(
            run_in(r#"path("data.txt").exists()"#, dir.path()).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            run_in(r#"path("nope.txt").exists()"#, dir.path()).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            run_in(r#"path("data.txt").is_file()"#, dir.path()).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            run_in(r#"path("sub").is_dir()"#, dir.path()).unwrap(),
            Value::Bool(true)
        );
        // `.size()` is a size.
        assert_eq!(
            run_in(r#"path("data.txt").size()"#, dir.path()).unwrap(),
            Value::Size(18)
        );
        // `.modified()` is a datetime.
        assert!(matches!(
            run_in(r#"path("data.txt").modified()"#, dir.path()).unwrap(),
            Value::DateTime(_)
        ));
        // A missing file surfaces `not_found`, not a panic.
        assert_eq!(
            run_in(r#"path("nope.txt").read()"#, dir.path())
                .unwrap_err()
                .code,
            "not_found"
        );
    }

    #[test]
    fn path_pure_component_methods() {
        // Pure component accessors need no filesystem.
        assert_eq!(
            run(r#"path("/a/b/file.txt").name()"#).unwrap(),
            Value::Str("file.txt".into())
        );
        assert_eq!(
            run(r#"path("/a/b/file.txt").stem()"#).unwrap(),
            Value::Str("file".into())
        );
        assert_eq!(
            run(r#"path("/a/b/file.txt").ext()"#).unwrap(),
            Value::Str("txt".into())
        );
        assert_eq!(
            run(r#"path("/a/b/file.txt").parent()"#).unwrap(),
            Value::Path("/a/b".into())
        );
        assert_eq!(
            run(r#"path("/a/b").join("c")"#).unwrap(),
            Value::Path("/a/b/c".into())
        );
        // `.ext()` of an extensionless name is null.
        assert_eq!(run(r#"path("/a/README").ext()"#).unwrap(), Value::Null);
    }

    #[test]
    fn glob_value_behaves_as_collection() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), b"").unwrap();
        std::fs::write(dir.path().join("b.rs"), b"").unwrap();
        std::fs::write(dir.path().join("c.txt"), b"").unwrap();

        // `.len()` expands and counts (sorted, cwd-relative).
        assert_eq!(
            run_in(r#"glob("*.rs").len()"#, dir.path()).unwrap(),
            Value::Int(2)
        );
        // `.expand()` yields the sorted match list.
        assert_eq!(
            run_in(r#"glob("*.rs").expand().len()"#, dir.path()).unwrap(),
            Value::Int(2)
        );
        // `.pattern` (field and method) returns the source pattern.
        assert_eq!(
            run_in(r#"glob("*.rs").pattern"#, dir.path()).unwrap(),
            Value::Str("*.rs".into())
        );
        // `.map(...)` re-dispatches on the expanded list.
        assert_eq!(
            run_in(r#"glob("*.rs").map(.name())"#, dir.path()).unwrap(),
            Value::List(vec![Value::Str("a.rs".into()), Value::Str("b.rs".into())])
        );
        // `for x in <glob>` iterates the expanded matches. (The glob value is
        // parenthesized only to sidestep a parser limitation shared by every
        // `)`-terminated call in a for-in head — the iteration itself is the
        // glob-value path exercised here.)
        let (_out, captured) =
            run_capturing_in(r#"for f in (glob("*.rs")) { echo (f.name()) }"#, dir.path());
        let texts: Vec<Value> = captured.iter().map(out_of).collect();
        assert_eq!(
            texts,
            vec![Value::Str("a.rs".into()), Value::Str("b.rs".into())]
        );
    }

    /// `run_capturing`, but in an explicit cwd (for glob/fixture tests).
    fn run_capturing_in(src: &str, cwd: &Path) -> (VResult<Value>, Vec<Value>) {
        use std::sync::{Arc, Mutex};
        let program = shoal_syntax::parse(src).unwrap_or_else(|e| panic!("parse failed: {e}"));
        let mut ev = Evaluator::new(cwd.to_path_buf());
        let sink: Arc<Mutex<Vec<Value>>> = Arc::default();
        let sink2 = sink.clone();
        ev.set_statement_sink(Box::new(move |v: &Value| {
            sink2.lock().unwrap().push(v.clone())
        }));
        let out = ev.eval_program(&program);
        drop(ev);
        let captured = Arc::try_unwrap(sink).unwrap().into_inner().unwrap();
        (out, captured)
    }
}
