//! The single authoritative child-evaluator constructor (HR-B1, deep-audit
//! finding B1–B4/H1).
//!
//! Every route that runs Shoal code in a fresh `Evaluator` derived from a
//! running session — `spawn { }` (`script.rs`), a `.shl` script
//! (`script.rs::run_script_file`), `parallel(...)` (`host.rs`), and
//! `on(channel, handler)` (`channels.rs`) — MUST build that child through
//! [`Evaluator::child_context`] + [`ChildContext::build`]. Before this seam
//! existed each site hand-copied a *subset* of fields, and the copies drifted:
//! the audit found the active leash policy/principal, reef scope/resolver/
//! overrides, and (for some routes) config and the event bus silently dropped,
//! so a command a policy forbids foreground could run unconfined inside a
//! `spawn`/`parallel`/handler/script.
//!
//! [`ChildContext`] captures the *whole* inheritable session context in one
//! place ([`Evaluator::child_context`]) and re-applies it in one place
//! ([`ChildContext::build`], which destructures the struct so the compiler
//! forces every captured field to be handled). Adding a new inheritable field
//! is therefore a two-line edit at exactly these two sites, not a hunt across
//! four call sites — and forgetting to *re-apply* a captured field is a compile
//! error, not a silent security regression.
//!
//! Fields that are deliberately NOT inherited (a child gets fresh state) are
//! documented inline in [`ChildContext::build`], each with the rule for why.

use super::*;

/// Whether a child evaluator inherits the parent's lexical environment handle
/// or starts from a fresh session root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildScope {
    /// Share the parent's [`Env`] handle: `spawn`/`parallel`/`on` bodies are
    /// closures/blocks that must see the caller's bindings. The handle is
    /// interior-mutable and parent-linked, so the child observes the same scope.
    Inherit,
    /// Start from a fresh session root (`Env::root()`): a `.shl` script is a
    /// separate program whose `let`s must not leak back into the caller session.
    Fresh,
}

/// A snapshot of every inheritable session capability, captured from a parent
/// [`Evaluator`] via [`Evaluator::child_context`] and consumed exactly once by
/// [`ChildContext::build`] to construct a child. All fields are cheap to clone
/// (`Arc` handles or small owned data) and `Send`, so the context can be moved
/// into a worker thread and built there.
pub(crate) struct ChildContext {
    cwd: PathBuf,
    env: Env,
    process_env: Vec<(OsString, OsString)>,
    adapters: AdapterCatalog,
    bus: Arc<channels::EventBus>,
    fs: Arc<dyn Fs>,
    exec: Arc<dyn Exec>,
    clock: Arc<dyn Clock>,
    opener: Arc<dyn Opener>,
    secrets: Arc<dyn SecretPort>,
    config: Arc<dyn ConfigPort>,
    leash: Option<(LeashPolicy, String)>,
    reef_chain: Option<(PathBuf, shoal_reef::ScopeChain)>,
    reef_resolver: Option<Arc<shoal_reef::Resolver>>,
    reef_lock: shoal_reef::Lockfile,
    reef_lock_path: Option<PathBuf>,
    reef_user_manifest: Option<PathBuf>,
    reef_overrides: Vec<shoal_reef::ScopeEntry>,
    session_id: String,
    principal: String,
}

impl Evaluator {
    /// Capture the full inheritable session context for a child evaluator (the
    /// ONLY supported way to seed a child — see [`ChildContext`]). Cheap: `Arc`
    /// clones plus small owned data. The returned context is `Send`, so a route
    /// may move it into a worker thread and call [`ChildContext::build`] there.
    pub(crate) fn child_context(&self) -> ChildContext {
        ChildContext {
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            process_env: self.process_env.clone(),
            adapters: self.adapters.clone(),
            bus: self.bus.clone(),
            fs: self.fs.clone(),
            exec: self.exec.clone(),
            clock: self.clock.clone(),
            opener: self.opener.clone(),
            secrets: self.secrets.clone(),
            config: self.config.clone(),
            leash: self.leash.clone(),
            reef_chain: self.reef_chain.clone(),
            reef_resolver: self.reef_resolver.clone(),
            reef_lock: self.reef_lock.clone(),
            reef_lock_path: self.reef_lock_path.clone(),
            reef_user_manifest: self.reef_user_manifest.clone(),
            reef_overrides: self.reef_overrides.clone(),
            session_id: self.session_id.clone(),
            principal: self.principal.clone(),
        }
    }
}

impl ChildContext {
    /// Build the child evaluator. `scope` selects lexical-environment
    /// inheritance; `cancel` is the cancellation token the child consults —
    /// callers pass either the parent's token (`Evaluator::cancellation_token`,
    /// so parent cancellation reaches a synchronous script or a `parallel`
    /// batch) or a FRESH token wired to a spawned task's cancel hook
    /// (`spawn`/`on`, so cancelling the task cancels its child).
    ///
    /// Every OTHER inherited capability is applied here by construction: the
    /// method destructures the whole [`ChildContext`], so the compiler rejects
    /// any captured field that is not re-applied to the child.
    pub(crate) fn build(self, scope: ChildScope, cancel: CancelToken) -> Evaluator {
        let ChildContext {
            cwd,
            env,
            process_env,
            adapters,
            bus,
            fs,
            exec,
            clock,
            opener,
            secrets,
            config,
            leash,
            reef_chain,
            reef_resolver,
            reef_lock,
            reef_lock_path,
            reef_user_manifest,
            reef_overrides,
            session_id,
            principal,
        } = self;

        let mut child = Evaluator::new(cwd);

        // --- Inherited by construction -------------------------------------
        // Lexical env: closure/spawn/parallel/on bodies inherit the caller's
        // bindings; a `.shl` script keeps `Evaluator::new`'s fresh root.
        if let ChildScope::Inherit = scope {
            child.env = env;
        }
        child.cancel = cancel;
        child.process_env = process_env;
        child.adapters = adapters;
        child.bus = bus;
        // Effect ports (Fs/Exec/Clock/Opener/Secret/Config): a child must see
        // the same fakes/host adapters, or in-process effects diverge.
        child.fs = fs;
        child.exec = exec;
        child.clock = clock;
        child.opener = opener;
        child.secrets = secrets;
        child.config = config;
        // Leash policy/principal: the security fix — a child must not escape the
        // parent's confinement (spawn-hash gate + OS sandbox selection).
        child.leash = leash;
        // Reef scope/resolver/lock/overrides: constrained tool resolution must
        // resolve identically inside a child, or a pinned tool diverges.
        child.reef_chain = reef_chain;
        child.reef_resolver = reef_resolver;
        child.reef_lock = reef_lock;
        child.reef_lock_path = reef_lock_path;
        child.reef_user_manifest = reef_user_manifest;
        child.reef_overrides = reef_overrides;
        // Session identity: journal ATTRIBUTION (session_id/principal) is
        // inherited even though the journal handle itself is not (see below).
        child.session_id = session_id;
        child.principal = principal;

        // --- Deliberately NOT inherited (fresh state per child) ------------
        // journal handle:  `Journal` is single-handle / not `Sync`; sharing it
        //                  across a worker thread is unsound. A child journals
        //                  nothing (`None`), but keeps the parent's session_id/
        //                  principal so any attribution it does derive matches.
        // interactive:     false — a child never owns the real terminal.
        // sink:            no competing mutable renderer; a child returns its
        //                  value through its task/return channel, not a sink.
        // it / plans / modules / jobs / dir_stack / oldpwd / current_entry /
        // source / pending_exit / pending_stop / external_jobs / call_depth /
        // in_fn_body:      per-evaluator session state; a child gets its own.
        // jump_store:      None — a child never writes interactive cd frecency.
        // echo_mode:       fresh default (`All`); only `run_script_file` runs a
        //                  full program, and it keeps the standalone-script
        //                  echo default rather than adopting a host's Quiet mode
        //                  (inheriting it would change script rendering).
        child
    }
}
