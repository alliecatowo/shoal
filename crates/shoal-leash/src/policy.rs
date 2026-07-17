//! Principal policy: TOML-loaded per-principal grants, effect/plan
//! evaluation, and the lowering of filesystem grants into a concrete
//! [`crate::SandboxPolicy`] for one child spawn.

use crate::effects::{Effect, Plan, Reversibility};
use crate::enforce::{FsSandbox, NetPolicy, SandboxPolicy};
use glob::Pattern;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

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
}

#[derive(Debug, Deserialize)]
struct PolicyDoc {
    #[serde(default)]
    principal: HashMap<String, PrincipalPolicy>,
}

impl Policy {
    pub fn from_toml(src: &str) -> Result<Self, toml::de::Error> {
        let mut value: toml::Value = toml::from_str(src)?;
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
        let doc: PolicyDoc = value.try_into()?;
        Ok(Self {
            principals: doc.principal,
        })
    }
    pub fn load(path: &Path) -> Result<Self, PolicyLoadError> {
        let src = fs::read_to_string(path).map_err(PolicyLoadError::Io)?;
        Self::from_toml(&src).map_err(PolicyLoadError::Toml)
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
        self.principal(principal)
            .is_some_and(|p| !p.proc_spawn.is_empty())
    }

    pub fn evaluate_effect(&self, principal: &str, effect: &Effect) -> Verdict {
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
            Effect::FsDelete { paths } => paths_verdict(paths, &p.fs_delete),
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
        let Some(policy) = self.principal(principal) else {
            return Verdict::Deny;
        };
        let mut verdict = Verdict::Allow;
        for effect in &plan.effects {
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
            AutoApply::Reversible if plan.reversibility == Reversibility::Reversible => {
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

    /// Load the per-user leash policy from [`Policy::user_leash_path`] if it
    /// exists and parses, otherwise fall back to [`Policy::permissive`] for
    /// `principal`. A missing or malformed file never bricks the shell — it
    /// degrades to permissive so normal use keeps working (site/content/internals/language-conformance-contract.md: honesty is
    /// surfaced at attach, not by refusing to run).
    pub fn load_user_or_permissive(principal: &str) -> Policy {
        match Self::user_leash_path() {
            Some(path) if path.is_file() => {
                Self::load(&path).unwrap_or_else(|_| Self::permissive(principal))
            }
            _ => Self::permissive(principal),
        }
    }

    /// Resolve the concrete OS [`SandboxPolicy`] for `principal`'s next child
    /// spawn, or `None` when the principal is unknown, its filesystem grants
    /// are unrestricted, or it declares no filesystem scope at all. `None`
    /// means "run the child without OS confinement" — the plan-layer verdict
    /// ([`Policy::evaluate_plan`]) remains the authority in that case, and the
    /// default-permissive policy therefore never wraps a spawn (zero
    /// regression). See [`PrincipalPolicy::to_sandbox_policy`].
    pub fn sandbox_for(&self, principal: &str) -> Option<SandboxPolicy> {
        self.principal(principal)
            .and_then(PrincipalPolicy::to_sandbox_policy)
    }
}

impl PrincipalPolicy {
    /// True when every filesystem dimension grants the root subtree (`/**`),
    /// i.e. an OS sandbox built from this principal would confine nothing.
    pub fn is_fs_unrestricted(&self) -> bool {
        grants_include_root(&self.fs_read)
            && grants_include_root(&self.fs_write)
            && grants_include_root(&self.fs_delete)
    }

    /// Lower this principal's filesystem scopes into a concrete
    /// [`SandboxPolicy`] for one child spawn, or `None` when there is nothing
    /// to confine to.
    ///
    /// `None` is returned when the grants are unrestricted (root subtree — a
    /// no-op sandbox) or when no filesystem scope resolves to an existing path
    /// (an empty Landlock/Seatbelt ruleset would only stop the child from
    /// loading its own binary, not usefully confine it — the plan layer, not
    /// the OS sandbox, denies those). Otherwise each glob is reduced to its
    /// longest concrete leading path (`/work/**` → `/work`) and non-existent
    /// roots are dropped so the backend never fails closed on a typo'd path.
    ///
    /// Net policy is left [`NetPolicy::Unrestricted`] because no seccomp/netns
    /// backend exists in this build — the plan-layer `NetConnect` verdict is
    /// the honest gate; [`crate::EnforcementStatus::network_enforced`] already
    /// reports `false`. `hermetic` is carried through from the principal.
    pub fn to_sandbox_policy(&self) -> Option<SandboxPolicy> {
        if self.is_fs_unrestricted() {
            return None;
        }
        let read = grant_roots(&self.fs_read);
        let write = grant_roots(&self.fs_write);
        let delete = grant_roots(&self.fs_delete);
        if read.is_empty() && write.is_empty() && delete.is_empty() {
            return None;
        }
        Some(SandboxPolicy {
            fs: FsSandbox {
                read,
                write,
                delete,
            },
            net: NetPolicy::Unrestricted,
            spawn_hash: None,
            hermetic: self.hermetic,
        })
    }
}

fn has_glob_meta(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c, '*' | '?' | '[' | ']' | '{' | '}'))
}

/// The longest concrete (glob-free) leading path of a policy grant, expanding a
/// leading `~/`. `/work/**` → `/work`; `/**` → `/`; `/etc/hosts` → `/etc/hosts`.
/// `None` when the grant has no concrete anchor (e.g. `**/foo`).
///
/// `pub(crate)` rather than private: exercised directly by the crate's own
/// unit tests in `lib.rs`.
pub(crate) fn grant_root(grant: &str) -> Option<PathBuf> {
    let expanded = if let Some(rest) = grant.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(rest)
    } else {
        PathBuf::from(grant)
    };
    let mut root = PathBuf::new();
    for comp in expanded.components() {
        match comp {
            Component::RootDir | Component::Prefix(_) => root.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                root.pop();
            }
            Component::Normal(seg) => {
                if has_glob_meta(&seg.to_string_lossy()) {
                    break;
                }
                root.push(seg);
            }
        }
    }
    (!root.as_os_str().is_empty()).then_some(root)
}

/// Does any grant in `grants` reduce to the filesystem root `/`?
fn grants_include_root(grants: &[String]) -> bool {
    grants
        .iter()
        .any(|g| grant_root(g).as_deref() == Some(Path::new("/")))
}

/// Concrete, existing subtree roots for a set of grants (sorted, de-duped).
/// Non-existent roots are dropped: Landlock/Seatbelt open each path, so a
/// grant for a path that is not there yet must not fail the whole spawn — it
/// simply grants nothing, which is the fail-closed direction.
fn grant_roots(grants: &[String]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = grants
        .iter()
        .filter_map(|g| grant_root(g))
        .filter(|p| p.exists())
        .collect();
    out.sort();
    out.dedup();
    out
}

fn flatten_namespace(table: &mut toml::Table, namespace: &str, fields: &[&str]) {
    let Some(nested) = table.remove(namespace).and_then(|v| v.as_table().cloned()) else {
        return;
    };
    for field in fields {
        if let Some(value) = nested.get(*field) {
            table.insert(format!("{namespace}.{field}"), value.clone());
        }
    }
}

#[derive(Debug)]
pub enum PolicyLoadError {
    Io(std::io::Error),
    Toml(toml::de::Error),
}
impl std::fmt::Display for PolicyLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Toml(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for PolicyLoadError {}

fn bool_verdict(ok: bool) -> Verdict {
    if ok { Verdict::Allow } else { Verdict::Deny }
}
fn names_verdict(names: &[String], grants: &[String]) -> Verdict {
    bool_verdict(
        names
            .iter()
            .all(|n| grants.iter().any(|g| g == "*" || g == n)),
    )
}
fn paths_verdict(paths: &[PathBuf], grants: &[String]) -> Verdict {
    bool_verdict(
        paths
            .iter()
            .all(|p| grants.iter().any(|g| path_grant(g, p))),
    )
}

fn path_grant(grant: &str, path: &Path) -> bool {
    let expanded = if let Some(rest) = grant.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(rest)
            .to_string_lossy()
            .into_owned()
    } else {
        grant.to_owned()
    };
    let normalized = normalize(path);
    Pattern::new(&expanded).is_ok_and(|p| p.matches_path(&normalized))
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            x => out.push(x.as_os_str()),
        }
    }
    out
}

fn host_grant(grant: &str, host: &str, port: u16) -> bool {
    let Some((host_pat, port_pat)) = grant.rsplit_once(':') else {
        return false;
    };
    if port_pat != "*" && port_pat.parse::<u16>().ok() != Some(port) {
        return false;
    }
    Pattern::new(host_pat).is_ok_and(|p| p.matches(host))
}
