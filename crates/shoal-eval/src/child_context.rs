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
    /// The whole host-services bundle (ports, adapters, event bus, reef
    /// resolution inputs) as one shared `Arc` — a child inherits every Class-1
    /// capability by cloning the refcount, so config/adapters/reef inputs can no
    /// longer be dropped individually at a child site (HR-J2 step 4).
    host: Arc<HostServices>,
    leash: Option<(LeashPolicy, String)>,
    reef: ReefState,
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
            cwd: self.exec.cwd.clone(),
            env: self.exec.env.clone(),
            process_env: self.exec.process_env.clone(),
            host: self.host.clone(),
            leash: self.session.leash.clone(),
            reef: self.exec.reef.clone(),
            session_id: self.session.session_id.clone(),
            principal: self.session.principal.clone(),
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
            host,
            leash,
            reef,
            session_id,
            principal,
        } = self;

        let mut child = Evaluator::new(cwd);

        // --- Inherited by construction -------------------------------------
        // Lexical env: closure/spawn/parallel/on bodies inherit the caller's
        // bindings; a `.shl` script keeps `Evaluator::new`'s fresh root.
        if let ChildScope::Inherit = scope {
            child.exec.env = env;
        }
        child.exec.cancel = cancel;
        child.exec.process_env = process_env;
        // Host services (effect ports, adapters, event bus, and the reef
        // resolution inputs) travel as one shared `Arc<HostServices>` (HR-J2
        // step 4): a child must see the same fakes/host adapters/config or
        // in-process effects diverge, and constrained tool resolution must
        // resolve identically inside a child, or a pinned tool diverges.
        child.host = host;
        // Leash policy/principal: the security fix — a child must not escape the
        // parent's confinement (spawn-hash gate + OS sandbox selection).
        child.session.leash = leash;
        // Reef dynamic overlay + per-cwd cache: constrained tool resolution must
        // resolve identically inside a child. The overlay + per-cwd cache travels
        // as one [`ReefState`] unit (HR-J2); the resolver + user manifest are the
        // separate resolution inputs, now inside the shared `host` bundle above.
        child.exec.reef = reef;
        // Session identity: journal ATTRIBUTION (session_id/principal) is
        // inherited even though the journal handle itself is not (see below).
        child.session.session_id = session_id;
        child.session.principal = principal;

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

#[cfg(test)]
mod decomposition_characterization {
    //! Field-level child-inheritance characterization for the evaluator
    //! decomposition (HR-J2, step 1). These pin, directly at the
    //! `child_context().build` seam, the inheritance facts that steps 2
    //! (`SessionCtx`) and 3 (`ReefState`) regroup: journal *identity* (not the
    //! handle), the reef overlay, the leash, config, and the shared event bus —
    //! plus the deliberately-fresh fields (journal handle, sink, interactive).
    //! White-box, because journal identity has no in-language surface to observe
    //! black-box; the behavioral half of step 1 lives in
    //! `tests/child_context_propagation.rs`. After steps 2/3 move these fields
    //! into sub-structs the assertions read through the new paths but their
    //! values are unchanged — that identity is the proof each step is a no-op.
    use super::ChildScope;
    use crate::Evaluator;
    use shoal_exec::CancelToken;
    use shoal_value::{ConfigSnapshot, Record, Value};
    use std::sync::Arc;

    /// A one-tool `with reef:` overlay record (`{faketool: "*"}`).
    fn override_record() -> Record {
        let mut r = Record::new();
        r.insert("faketool".into(), Value::Str("*".into()));
        r
    }

    /// Build a parent carrying every inheritable capability, then assert a child
    /// of `scope` inherits identity/authority/reef-overlay/bus/config and starts
    /// the deliberately-fresh fields fresh.
    fn assert_inheritance(scope: ChildScope) {
        let dir = tempfile::tempdir().unwrap();
        let mut parent = Evaluator::new(dir.path().to_path_buf());

        // Session identity via an installed journal (a root-only handle).
        let journal = shoal_journal::Journal::open(dir.path()).expect("open journal");
        parent.set_journal(journal, "sess-characterize", "agent:tester");
        // Authority.
        parent.set_leash_policy(
            shoal_leash::Policy::permissive("agent:tester"),
            "agent:tester",
        );
        // Presentation state that must NOT reach a child.
        parent.set_statement_sink(Box::new(|_v: &Value| {}));
        parent.set_interactive(true);
        // Config + reef resolution inputs + a `with reef:` overlay layer.
        let mut cfg = Record::new();
        cfg.insert("k".into(), Value::Int(7));
        parent.set_config(Arc::new(ConfigSnapshot::new(Value::Record(cfg))));
        parent.set_reef_resolver(Arc::new(shoal_reef::Resolver::with_defaults()));
        parent
            .push_reef_override(&override_record())
            .expect("override pushes");

        let child = parent.child_context().build(scope, CancelToken::new());

        // --- Inherited: journal IDENTITY (session_id + principal) -----------
        assert_eq!(child.session.session_id, "sess-characterize");
        assert_eq!(child.session.principal, "agent:tester");
        // --- Inherited: authority (the security core) ----------------------
        assert!(
            child.session.leash.is_some(),
            "leash policy must reach the child"
        );
        assert_eq!(child.session.leash.as_ref().unwrap().1, "agent:tester");
        // --- Inherited: reef overlay + resolver (the step-3 bundle) ---------
        assert_eq!(
            child.exec.reef.overrides.len(),
            1,
            "with reef: overlay inherited"
        );
        assert!(
            child.exec.reef.overrides[0]
                .manifest
                .tools
                .contains_key("faketool"),
            "the overlay's tool constraint is carried into the child"
        );
        assert!(
            child.host.reef_resolver.get().is_some(),
            "reef resolver inherited"
        );
        // --- Inherited (shared Arc): the whole HostServices bundle ---------
        // Config, the event bus, and the reef resolution inputs now travel as
        // one shared `Arc<HostServices>` (HR-J2 step 4), so proving the bundle
        // is `ptr_eq` proves config + bus + resolver are all shared at once.
        assert!(
            Arc::ptr_eq(&parent.host, &child.host),
            "host services bundle shared"
        );

        // --- Deliberately NOT inherited (fresh per child) ------------------
        assert!(
            child.session.journal.is_none(),
            "journal HANDLE stays root-only"
        );
        assert!(
            child.session.sink.is_none(),
            "no competing mutable renderer"
        );
        assert!(
            !child.session.interactive,
            "a child never owns the terminal"
        );
    }

    #[test]
    fn closure_child_inherits_session_and_reef_context() {
        assert_inheritance(ChildScope::Inherit);
    }

    #[test]
    fn script_child_inherits_session_and_reef_context() {
        assert_inheritance(ChildScope::Fresh);
    }
}
