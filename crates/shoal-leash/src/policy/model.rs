//! Policy data model, effect verdicts, and sandbox lowering.

use super::*;

pub const POLICY_MAX_BYTES: usize = 1024 * 1024;
pub const POLICY_MAX_NESTING: usize = 64;
pub const POLICY_MAX_ASSIGNMENTS: usize = 8 * 1024;
pub const POLICY_MAX_PRINCIPALS: usize = 256;
pub const POLICY_MAX_GRANTS_PER_KIND: usize = 1024;
pub const POLICY_MAX_GRANT_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny,
    ApprovalRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AutoApply {
    Reversible,
    InGrant,
    #[default]
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OpaqueMode {
    #[default]
    Deny,
    Ask,
    Allow,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrincipalPolicy {
    #[serde(default, rename = "fs.read")]
    pub fs_read: Vec<String>,
    #[serde(default, rename = "fs.write")]
    pub fs_write: Vec<String>,
    #[serde(default, rename = "fs.delete")]
    pub fs_delete: Vec<String>,
    #[serde(default, alias = "net")]
    pub net_connect: Vec<String>,
    #[serde(default)]
    pub net_listen: Vec<u16>,
    #[serde(default, alias = "spawn")]
    pub proc_spawn: Vec<String>,
    #[serde(default)]
    pub env_read: Vec<String>,
    #[serde(default)]
    pub env_write: Vec<String>,
    #[serde(default, alias = "secrets")]
    pub secret_use: Vec<String>,
    #[serde(default)]
    pub session_write: bool,
    #[serde(default)]
    pub journal_read: bool,
    #[serde(default)]
    pub time: bool,
    /// Hard CPU-time ceiling inherited by each spawned process. Accounting is
    /// per process, not aggregate across a descendant tree.
    #[serde(default)]
    pub process_cpu_seconds: Option<u64>,
    /// Hard virtual-address-space ceiling inherited by each spawned process.
    /// Accounting is per process, not an aggregate principal memory budget.
    #[serde(default)]
    pub process_memory_bytes: Option<u64>,
    #[serde(default)]
    pub auto_apply: AutoApply,
    #[serde(default)]
    pub opaque: OpaqueMode,
    /// site/content/internals/language-conformance-contract.md hermetic intent: when `true`, a child spawn built from this
    /// principal demands a hard guarantee — [`crate::SandboxPolicy::hermetic`]
    /// is set, so the exec layer refuses to spawn rather than run with any
    /// requested dimension unenforced. `false` (the default) is best-effort:
    /// the strongest available backend is applied and anything unenforceable
    /// on this host is reported truthfully instead of silently granted.
    #[serde(default)]
    pub hermetic: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Policy {
    principals: HashMap<String, PrincipalPolicy>,
    fail_closed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PolicyDoc {
    #[serde(default)]
    pub(super) principal: HashMap<String, PrincipalPolicy>,
}

impl Policy {
    pub fn from_toml(src: &str) -> Result<Self, PolicyParseError> {
        validate_policy_text(src)?;
        let mut value: toml::Value = toml::from_str(src).map_err(PolicyParseError::toml)?;
        // TOML dotted keys such as `fs.read = [...]` deserialize as nested
        // tables. Flatten the policy namespaces into the wire field names.
        if let Some(principals) = value
            .get_mut("principal")
            .and_then(toml::Value::as_table_mut)
        {
            for (_, raw) in principals.iter_mut() {
                if let Some(table) = raw.as_table_mut() {
                    flatten_namespace(table, "fs", &["read", "write", "delete"]);
                    flatten_namespace(table, "env", &["read", "write"]);
                    flatten_namespace(table, "secret", &["use"]);
                    flatten_namespace(table, "proc", &["spawn"]);
                }
            }
        }
        let doc: PolicyDoc = value.try_into().map_err(PolicyParseError::toml)?;
        validate_policy_doc(&doc)?;
        Ok(Self {
            principals: doc.principal,
            fail_closed: false,
        })
    }
    pub fn load(path: &Path) -> Result<Self, PolicyLoadError> {
        let metadata = fs::metadata(path).map_err(|source| PolicyLoadError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(PolicyLoadError::NotFile {
                path: path.to_path_buf(),
            });
        }
        let file = fs::File::open(path).map_err(|source| PolicyLoadError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let src = read_policy_utf8(path, file)?;
        Self::from_toml(&src).map_err(|source| PolicyLoadError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }
    pub fn principal(&self, name: &str) -> Option<&PrincipalPolicy> {
        self.principals.get(name)
    }

    /// Whether `principal` pins process spawns — i.e. declares a non-empty
    /// `proc_spawn` allowlist. This is the explicit guard for site/content/internals/language-conformance-contract.md
    /// "empty grants ⇒ allow" contract at the *spawn* boundary.
    ///
    /// When this returns `false` (an unknown principal, or one with no
    /// `proc_spawn` grants) a caller MUST treat every spawn as allowed and MUST
    /// NOT route it through [`Policy::evaluate_effect`]: with an empty allowlist
    /// `evaluate_effect` evaluates any [`Effect::ProcSpawn`] as [`Verdict::Deny`]
    /// (nothing matches), so consulting the evaluator with no `proc_spawn`
    /// grants set would default-deny ordinary commands. The spawn path therefore
    /// gates on this predicate first and only hashes/evaluates a binary once a
    /// principal has actually opted into spawn pinning.
    pub fn spawn_pinning_active(&self, principal: &str) -> bool {
        self.fail_closed
            || self
                .principal(principal)
                .is_some_and(|p| !p.proc_spawn.is_empty())
    }

    /// Whether this principal asks Leash to restrict filesystem access.
    /// This intentionally remains true when every configured root is missing:
    /// a hermetic typo must be distinguishable from an unrestricted policy so
    /// the spawn boundary can refuse instead of silently dropping the scope.
    pub fn filesystem_scoping_active(&self, principal: &str) -> bool {
        self.fail_closed
            || self
                .principal(principal)
                .is_some_and(PrincipalPolicy::has_fs_scope)
    }

    /// Whether a network destination/listener allowlist is configured. Leash
    /// can authorize declared network effects, but the OS backends can only
    /// enforce coarse all-or-nothing denial, not hostname/port allowlists.
    pub fn network_scoping_active(&self, principal: &str) -> bool {
        self.fail_closed
            || self
                .principal(principal)
                .is_some_and(|p| !p.net_connect.is_empty() || !p.net_listen.is_empty())
    }

    /// Whether this principal requires requested OS dimensions to be hard
    /// guarantees rather than best-effort constraints.
    pub fn hermetic_active(&self, principal: &str) -> bool {
        self.fail_closed || self.principal(principal).is_some_and(|p| p.hermetic)
    }

    /// Whether this principal requests any inherited per-process ceiling.
    pub fn process_limits_active(&self, principal: &str) -> bool {
        self.principal(principal)
            .is_some_and(|p| p.process_cpu_seconds.is_some() || p.process_memory_bytes.is_some())
    }

    pub fn evaluate_effect(&self, principal: &str, effect: &Effect) -> Verdict {
        if self.fail_closed {
            return Verdict::Deny;
        }
        let Some(p) = self.principal(principal) else {
            return Verdict::Deny;
        };
        match effect {
            Effect::Opaque => match p.opaque {
                OpaqueMode::Deny => Verdict::Deny,
                OpaqueMode::Ask => Verdict::ApprovalRequired,
                OpaqueMode::Allow => Verdict::Allow,
            },
            Effect::FsRead { paths } => paths_verdict(paths, &p.fs_read),
            Effect::FsWrite { paths } => paths_verdict(paths, &p.fs_write),
            Effect::FsDelete { paths, .. } => paths_verdict(paths, &p.fs_delete),
            Effect::ProcSpawn { bin_hash, argv0 } => bool_verdict(p.proc_spawn.iter().any(|g| {
                g == bin_hash
                    || g == argv0
                    || Path::new(argv0)
                        .file_name()
                        .is_some_and(|n| n == g.as_str())
            })),
            Effect::NetConnect { host, port } => {
                bool_verdict(p.net_connect.iter().any(|g| host_grant(g, host, *port)))
            }
            Effect::NetListen { port } => bool_verdict(p.net_listen.contains(port)),
            Effect::EnvRead { names } => names_verdict(names, &p.env_read),
            Effect::EnvWrite { names } => names_verdict(names, &p.env_write),
            Effect::SecretUse { names } => names_verdict(names, &p.secret_use),
            Effect::SessionWrite => bool_verdict(p.session_write),
            Effect::JournalRead => bool_verdict(p.journal_read),
            Effect::Time => bool_verdict(p.time),
        }
    }

    /// Denial dominates approval, which dominates allow. `auto_apply` controls
    /// whether an otherwise granted plan may proceed unattended.
    pub fn evaluate_plan(&self, principal: &str, plan: &Plan) -> Verdict {
        if self.fail_closed {
            return Verdict::Deny;
        }
        let Some(policy) = self.principal(principal) else {
            return Verdict::Deny;
        };
        let mut verdict = Verdict::Allow;
        for effect in &plan.effects {
            // An empty process allowlist means spawn pinning is disabled, not
            // "deny every executable". The evaluator's concrete spawn gate
            // already follows this contract; plan evaluation must apply the
            // same semantics or a kernel rejects an ordinary command before
            // execution reaches that gate.
            if matches!(effect, Effect::ProcSpawn { .. }) && !self.spawn_pinning_active(principal) {
                continue;
            }
            match self.evaluate_effect(principal, effect) {
                Verdict::Deny => return Verdict::Deny,
                Verdict::ApprovalRequired => verdict = Verdict::ApprovalRequired,
                Verdict::Allow => {}
            }
        }
        if verdict != Verdict::Allow {
            return verdict;
        }
        match policy.auto_apply {
            AutoApply::Never => Verdict::ApprovalRequired,
            AutoApply::InGrant => Verdict::Allow,
            AutoApply::Reversible
                if plan.reversibility == Reversibility::Reversible
                    && !plan.effects.iter().any(Effect::is_permanent_delete) =>
            {
                Verdict::Allow
            }
            AutoApply::Reversible => Verdict::ApprovalRequired,
        }
    }

    /// The default-permissive policy for `principal` (site/content/internals/language-conformance-contract.md): allow every
    /// effect, filesystem read/write/delete unrestricted, so enforcement is a
    /// genuine no-op and normal use never regresses. Human principals get this
    /// by default; agent principals are the ones that get scoped down.
    pub fn permissive(principal: &str) -> Policy {
        Policy::from_toml(&format!(
            "[principal.\"{principal}\"]\nopaque='allow'\nauto_apply='in-grant'\n\
             journal_read=true\nenv_read=[\"*\"]\nenv_write=[\"*\"]\nsession_write=true\n\
             time=true\n\n\
             [principal.\"{principal}\".fs]\nread=[\"/**\"]\nwrite=[\"/**\"]\ndelete=[\"/**\"]\n"
        ))
        .expect("built-in permissive policy")
    }

    /// A quarantined policy used when an authority-bearing policy exists but
    /// cannot be trusted. It denies every effect and reports spawn pinning as
    /// active so callers cannot take the empty-allowlist bypass.
    pub fn deny_all(principal: &str) -> Policy {
        let mut principals = HashMap::new();
        principals.insert(principal.to_string(), PrincipalPolicy::default());
        Policy {
            principals,
            fail_closed: true,
        }
    }

    pub fn is_fail_closed(&self) -> bool {
        self.fail_closed
    }

    /// Path of the per-user leash policy (site/content/internals/language-conformance-contract.md): `$XDG_CONFIG_HOME/shoal/leash.toml`
    /// or, absent that, `~/.config/shoal/leash.toml`. `None` when neither
    /// `XDG_CONFIG_HOME` nor `HOME` is set (no home to anchor config to).
    pub fn user_leash_path() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
            return Some(PathBuf::from(dir).join("shoal").join("leash.toml"));
        }
        std::env::var_os("HOME").filter(|s| !s.is_empty()).map(|h| {
            PathBuf::from(h)
                .join(".config")
                .join("shoal")
                .join("leash.toml")
        })
    }

    /// Load the per-user leash policy from [`Policy::user_leash_path`]. A
    /// genuinely missing file keeps the documented permissive default; any
    /// present-but-unreadable, malformed, oversized, or non-regular policy is
    /// authority corruption and quarantines to [`Policy::deny_all`].
    pub fn load_user_or_permissive(principal: &str) -> Policy {
        let Some(path) = Self::user_leash_path() else {
            return Self::permissive(principal);
        };
        match fs::metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::permissive(principal),
            Ok(_) => Self::load(&path).unwrap_or_else(|_| Self::deny_all(principal)),
            Err(_) => Self::deny_all(principal),
        }
    }

    /// Resolve the concrete OS [`SandboxPolicy`] for `principal`'s next child
    /// spawn, or `None` when the principal is unknown and requests neither a
    /// usable filesystem scope nor process ceilings. `None` means "run the
    /// child without an enforcement launcher" — the plan-layer verdict
    /// ([`Policy::evaluate_plan`]) remains the authority in that case, and the
    /// default-permissive policy therefore never wraps a spawn (zero
    /// regression). See [`PrincipalPolicy::to_sandbox_policy`].
    pub fn sandbox_for(&self, principal: &str) -> Option<SandboxPolicy> {
        self.principal(principal)
            .and_then(PrincipalPolicy::to_sandbox_policy)
    }
}

impl PrincipalPolicy {
    /// Whether the principal declares a non-no-op filesystem scope. Empty
    /// grant lists mean no OS filesystem request (semantic effect evaluation
    /// may still deny filesystem effects); all-root grants are unrestricted.
    pub fn has_fs_scope(&self) -> bool {
        (!self.fs_read.is_empty() || !self.fs_write.is_empty() || !self.fs_delete.is_empty())
            && !self.is_fs_unrestricted()
    }

    /// True when every filesystem dimension grants the root subtree (`/**`),
    /// i.e. an OS sandbox built from this principal would confine nothing.
    pub fn is_fs_unrestricted(&self) -> bool {
        grants_include_root(&self.fs_read)
            && grants_include_root(&self.fs_write)
            && grants_include_root(&self.fs_delete)
    }

    /// Lower this principal's filesystem scopes and inherited process ceilings
    /// into a concrete [`SandboxPolicy`] for one child spawn, or `None` when
    /// there is nothing to enforce.
    ///
    /// `None` is returned when the grants are unrestricted (root subtree — a
    /// no-op sandbox), or when a non-hermetic scope has no existing root. A
    /// hermetic unresolved scope is retained as an empty request so the exec
    /// boundary can refuse it explicitly instead of losing the hard
    /// requirement. Otherwise each glob is reduced to its
    /// longest concrete leading path (`/work/**` → `/work`) and non-existent
    /// roots are dropped so the backend never fails closed on a typo'd path.
    ///
    /// A hermetic principal with no network grants lowers to coarse
    /// [`NetPolicy::Deny`], which Landlock ABI 4+ or Seatbelt can enforce. Any
    /// declared hostname/port/listener allowlist remains unrestricted here:
    /// the evaluator refuses that hermetic request before spawn because the OS
    /// backends cannot express it. `hermetic` is carried through unchanged.
    pub fn to_sandbox_policy(&self) -> Option<SandboxPolicy> {
        let filesystem_requested = self.has_fs_scope();
        let process_limits = ProcessLimits {
            cpu_seconds: self.process_cpu_seconds,
            memory_bytes: self.process_memory_bytes,
        };
        if !filesystem_requested && process_limits.is_empty() {
            return None;
        }
        let (read, write, delete) = if filesystem_requested {
            (
                grant_roots(&self.fs_read),
                grant_roots(&self.fs_write),
                grant_roots(&self.fs_delete),
            )
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };
        if filesystem_requested
            && read.is_empty()
            && write.is_empty()
            && delete.is_empty()
            && !self.hermetic
            && process_limits.is_empty()
        {
            return None;
        }
        Some(SandboxPolicy {
            fs: FsSandbox {
                read,
                write,
                delete,
            },
            filesystem_requested,
            net: if filesystem_requested
                && self.hermetic
                && self.net_connect.is_empty()
                && self.net_listen.is_empty()
            {
                NetPolicy::Deny
            } else {
                NetPolicy::Unrestricted
            },
            spawn_hash: None,
            process_limits,
            hermetic: self.hermetic,
        })
    }
}
