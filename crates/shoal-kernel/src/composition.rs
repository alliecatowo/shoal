//! Kernel construction and typed ownership groups.
//!
//! The public [`KernelBuilder`] is the single composition path. `Kernel`
//! itself retains only groups whose fields share a lifecycle and invariant:
//! session resources, connection admission, persistence, authority, and
//! process lifecycle.

use super::*;

pub(crate) struct SessionRuntime {
    pub(crate) sessions: SessionRegistry,
    pub(crate) plans: PlanRegistry,
    pub(crate) tasks: TaskRegistry,
    pub(crate) ptys: Arc<PtyRegistry>,
    pub(crate) events: Arc<EventBus>,
}

pub(crate) struct ConnectionAdmission {
    pub(crate) connections: ConnectionRegistry,
    pub(crate) max_subscriptions_per_session: AtomicUsize,
    pub(crate) max_blob_decompressions_per_window: AtomicUsize,
    pub(crate) blob_decompression_window_ms: AtomicU64,
}

pub(crate) struct PersistenceRuntime {
    pub(crate) journal: Mutex<Journal>,
    /// The exact durable journal/CAS root. Ephemeral kernels have no path.
    pub(crate) state_dir: Option<PathBuf>,
}

pub(crate) struct AuthorityRuntime {
    pub(crate) policy: Policy,
    pub(crate) auth: Option<Mutex<TokenStore>>,
    pub(crate) require_public_token: AtomicBool,
    pub(crate) require_peer_uid: AtomicBool,
    pub(crate) allow_self_ack: AtomicBool,
    #[cfg(test)]
    pub(crate) fail_approval_audit: AtomicBool,
    #[cfg(test)]
    pub(crate) panic_approval_audit: AtomicBool,
}

pub(crate) struct LifecycleRuntime {
    pub(crate) shutdown_requested: AtomicBool,
    pub(crate) started_at: Instant,
}

/// The one construction path for ephemeral and durable kernels.
pub struct KernelBuilder {
    limits: Limits,
    policy: Policy,
    state_dir: Option<PathBuf>,
    token_store: Option<PathBuf>,
    require_public_token: bool,
    require_peer_uid: bool,
    allow_self_ack: bool,
}

impl Default for KernelBuilder {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            policy: permissive_policy(),
            state_dir: None,
            token_store: None,
            require_public_token: false,
            require_peer_uid: false,
            allow_self_ack: self_ack_from_env(),
        }
    }
}

impl KernelBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn policy(mut self, policy: Policy) -> Self {
        self.policy = policy;
        self
    }

    /// Select durable journal/CAS storage. The credential store defaults to
    /// `tokens.json` below this exact root unless overridden separately.
    #[must_use]
    pub fn durable(mut self, state_dir: impl Into<PathBuf>) -> Self {
        self.state_dir = Some(state_dir.into());
        self
    }

    /// Override the durable credential authority path.
    #[must_use]
    pub fn token_store(mut self, token_store: impl Into<PathBuf>) -> Self {
        self.token_store = Some(token_store.into());
        self
    }

    #[must_use]
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    #[must_use]
    pub fn listener_security(mut self, require_token: bool, require_peer_uid: bool) -> Self {
        self.require_public_token = require_token;
        self.require_peer_uid = require_peer_uid;
        self
    }

    #[must_use]
    pub fn allow_self_ack(mut self, allow: bool) -> Self {
        self.allow_self_ack = allow;
        self
    }

    pub fn build(self) -> Result<Arc<Kernel>, Box<dyn std::error::Error>> {
        let (journal, state_dir, auth) = match self.state_dir {
            Some(state_dir) => {
                let token_store = self
                    .token_store
                    .unwrap_or_else(|| state_dir.join("tokens.json"));
                let journal = Journal::open(&state_dir)?;
                let auth = TokenStore::open(token_store)?;
                (journal, Some(state_dir), Some(Mutex::new(auth)))
            }
            None if self.token_store.is_some() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a token-store override requires durable kernel storage",
                )
                .into());
            }
            None => (Journal::in_memory()?, None, None),
        };
        let limits = self.limits;
        Ok(Arc::new(Kernel {
            runtime: SessionRuntime {
                sessions: SessionRegistry::new(limits.max_sessions),
                plans: PlanRegistry::new(),
                tasks: TaskRegistry::new(limits.max_tasks_per_session),
                ptys: Arc::new(PtyRegistry::new(
                    limits.max_ptys_per_session,
                    limits.max_ptys_per_principal,
                    limits.max_ptys_global,
                )),
                events: Arc::new(EventBus::default()),
            },
            admission: ConnectionAdmission {
                connections: ConnectionRegistry::new(
                    limits.max_connections,
                    limits.frame_read_timeout_ms,
                ),
                max_subscriptions_per_session: AtomicUsize::new(
                    limits.max_subscriptions_per_session,
                ),
                max_blob_decompressions_per_window: AtomicUsize::new(
                    limits.max_blob_decompressions_per_window,
                ),
                blob_decompression_window_ms: AtomicU64::new(limits.blob_decompression_window_ms),
            },
            persistence: PersistenceRuntime {
                journal: Mutex::new(journal),
                state_dir,
            },
            authority: AuthorityRuntime {
                policy: self.policy,
                auth,
                require_public_token: AtomicBool::new(self.require_public_token),
                require_peer_uid: AtomicBool::new(self.require_peer_uid),
                allow_self_ack: AtomicBool::new(self.allow_self_ack),
                #[cfg(test)]
                fail_approval_audit: AtomicBool::new(false),
                #[cfg(test)]
                panic_approval_audit: AtomicBool::new(false),
            },
            lifecycle: LifecycleRuntime {
                shutdown_requested: AtomicBool::new(false),
                started_at: Instant::now(),
            },
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_store_override_requires_durable_storage() {
        let error = KernelBuilder::new()
            .token_store("tokens.json")
            .build()
            .err()
            .expect("invalid builder must fail");
        assert!(error.to_string().contains("requires durable"));
    }

    #[test]
    fn builder_applies_initial_limits_without_post_build_reconfiguration() {
        let kernel = KernelBuilder::new()
            .limits(Limits {
                max_connections: 7,
                max_sessions: 11,
                max_tasks_per_session: 13,
                ..Limits::default()
            })
            .build()
            .unwrap();
        assert_eq!(kernel.admission.connections.max(), 7);
        assert_eq!(kernel.runtime.sessions.configured_max(), 11);
        assert_eq!(kernel.runtime.tasks.configured_max(), 13);
    }

    #[test]
    fn builder_wires_durable_authority_and_listener_options_together() {
        let state = tempfile::tempdir().unwrap();
        let token_store = state.path().join("authority.json");
        let kernel = Kernel::builder()
            .durable(state.path())
            .token_store(&token_store)
            .listener_security(true, true)
            .allow_self_ack(true)
            .build()
            .unwrap();

        assert_eq!(
            kernel.persistence.state_dir.as_deref(),
            Some(state.path()),
            "the builder must retain the exact journal/CAS root"
        );
        assert!(kernel.authority.auth.is_some());
        assert!(kernel.authority.require_public_token.load(Ordering::SeqCst));
        assert!(kernel.authority.require_peer_uid.load(Ordering::SeqCst));
        assert!(kernel.authority.allow_self_ack.load(Ordering::SeqCst));
        assert!(
            token_store.exists(),
            "the explicit credential authority must be opened"
        );
    }
}
