//! `leash`: shoal's semantic effect and policy engine.
//!
//! This crate evaluates concrete effects. It does not claim to enforce them at
//! the OS boundary; [`EnforcementStatus::detect`] reports that distinction
//! explicitly until a platform backend is installed by the host.

use glob::Pattern;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Effect {
    FsRead { paths: Vec<PathBuf> },
    FsWrite { paths: Vec<PathBuf> },
    FsDelete { paths: Vec<PathBuf> },
    ProcSpawn { bin_hash: String, argv0: String },
    NetConnect { host: String, port: u16 },
    NetListen { port: u16 },
    EnvRead { names: Vec<String> },
    EnvWrite { names: Vec<String> },
    SecretUse { names: Vec<String> },
    SessionWrite,
    JournalRead,
    Time,
    Opaque,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reversibility {
    Reversible,
    Irreversible,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Estimates {
    pub bytes: Option<u64>,
    pub items: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub plan_ref: String,
    pub effects: Vec<Effect>,
    pub reversibility: Reversibility,
    pub estimates: Estimates,
}

impl Plan {
    pub fn new(effects: Vec<Effect>, reversibility: Reversibility, estimates: Estimates) -> Self {
        let canonical =
            serde_json::to_vec(&(&effects, reversibility, &estimates)).expect("serializable plan");
        let plan_ref = format!("plan:{}", &blake3::hash(&canonical).to_hex()[..16]);
        Self {
            plan_ref,
            effects,
            reversibility,
            estimates,
        }
    }
}

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
    /// TDD §8 hermetic intent: when `true`, a child spawn built from this
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

    /// The default-permissive policy for `principal` (TDD §8): allow every
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

    /// Path of the per-user leash policy (TDD §8): `$XDG_CONFIG_HOME/shoal/leash.toml`
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
    /// degrades to permissive so normal use keeps working (TDD §8: honesty is
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
fn grant_root(grant: &str) -> Option<PathBuf> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum EnforcementTier {
    A,
    B,
    C,
    D,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnforcementStatus {
    pub available_tier: EnforcementTier,
    pub active_tier: Option<EnforcementTier>,
    pub enforced: bool,
    pub detail: String,
    pub landlock_abi: Option<i32>,
    pub filesystem_enforced: bool,
    pub spawn_exec_enforced: bool,
    pub network_enforced: bool,
}

impl EnforcementStatus {
    /// Detect the strongest plausible platform tier. Since this crate installs
    /// no backend, `enforced` remains false and `active_tier` remains `None`.
    pub fn detect() -> Self {
        let (available_tier, detail) = if cfg!(target_os = "linux") {
            match landlock_abi() {
                Some(abi) => (
                    EnforcementTier::A,
                    format!(
                        "Landlock ABI {abi} available; seccomp/network enforcement unavailable"
                    ),
                ),
                None => (
                    EnforcementTier::B,
                    "Landlock unavailable; namespace fallback not installed".into(),
                ),
            }
        } else if cfg!(target_os = "macos") {
            (EnforcementTier::C, "Seatbelt backend not installed".into())
        } else {
            (EnforcementTier::D, "advisory policy only".into())
        };
        Self {
            available_tier,
            active_tier: None,
            enforced: false,
            detail,
            landlock_abi: landlock_abi(),
            filesystem_enforced: false,
            spawn_exec_enforced: false,
            network_enforced: false,
        }
    }
}

/// Concrete filesystem grants for a child process. Calling [`apply_landlock`]
/// irreversibly restricts the current thread/process and must only happen after
/// fork in the child, immediately before exec.
#[derive(Debug, Clone, Default)]
pub struct FsSandbox {
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
    pub delete: Vec<PathBuf>,
}

/// Coarse network policy carried by [`SandboxPolicy`]. This crate has no
/// seccomp/netns backend, so `Deny` is never independently OS-enforced today
/// — [`EnforcementStatus::network_enforced`] reports that honestly rather
/// than pretending the restriction took effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetPolicy {
    #[default]
    Unrestricted,
    Deny,
}

/// A concrete, resolved enforcement request for one child spawn: filesystem
/// scopes, a coarse network policy, an optional pinned spawn hash (TDD §8
/// content-hash pinning), and a `hermetic` intent flag.
///
/// `hermetic: true` means the caller wants a hard guarantee: the consumer
/// (`shoal-exec::run`/`spawn_capture`) must refuse to spawn rather than run
/// with any requested dimension unenforced. `hermetic: false` (default)
/// means best-effort: the strongest available OS mechanism is applied, and
/// anything that cannot be enforced on this host is reported truthfully via
/// [`EnforcementStatus`] rather than silently granted.
#[derive(Debug, Clone, Default)]
pub struct SandboxPolicy {
    pub fs: FsSandbox,
    pub net: NetPolicy,
    pub spawn_hash: Option<String>,
    pub hermetic: bool,
}

#[cfg(target_os = "linux")]
pub fn landlock_abi() -> Option<i32> {
    const VERSION: u32 = 1;
    let value = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<u8>(),
            0usize,
            VERSION,
        )
    };
    (value > 0).then_some(value as i32)
}
#[cfg(not(target_os = "linux"))]
pub fn landlock_abi() -> Option<i32> {
    None
}

#[cfg(target_os = "linux")]
pub fn apply_landlock(grants: &FsSandbox) -> Result<EnforcementStatus, String> {
    use landlock::{
        ABI, Access, AccessFs, CompatLevel, Compatible, LandlockStatus, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus, path_beneath_rules,
    };
    let abi = ABI::V7;
    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| e.to_string())?
        .create()
        .map_err(|e| e.to_string())?;
    ruleset = ruleset
        .add_rules(path_beneath_rules(
            grants.read.iter(),
            AccessFs::from_read(abi),
        ))
        .map_err(|e| e.to_string())?;
    ruleset = ruleset
        .add_rules(path_beneath_rules(
            grants.write.iter(),
            AccessFs::from_all(abi),
        ))
        .map_err(|e| e.to_string())?;
    let delete = AccessFs::RemoveDir | AccessFs::RemoveFile;
    ruleset = ruleset
        .add_rules(path_beneath_rules(grants.delete.iter(), delete))
        .map_err(|e| e.to_string())?;
    let status = ruleset
        .set_compatibility(CompatLevel::HardRequirement)
        .restrict_self()
        .map_err(|e| e.to_string())?;
    let active = matches!(status.landlock, LandlockStatus::Available { .. })
        && matches!(status.ruleset, RulesetStatus::FullyEnforced);
    if !active {
        return Err(format!(
            "Landlock restriction was not fully enforced: {status:?}"
        ));
    }
    Ok(EnforcementStatus {
        available_tier: EnforcementTier::A,
        active_tier: Some(EnforcementTier::A),
        enforced: true,
        detail: format!(
            "Landlock active ({:?}); spawn hash preflight is TOCTOU-prone; seccomp/netns unavailable",
            status.landlock
        ),
        landlock_abi: landlock_abi(),
        filesystem_enforced: true,
        spawn_exec_enforced: false,
        network_enforced: false,
    })
}
#[cfg(not(target_os = "linux"))]
pub fn apply_landlock(_: &FsSandbox) -> Result<EnforcementStatus, String> {
    Err("Landlock is only available on Linux".into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnPreflight {
    pub hash: String,
    pub allowed: bool,
    pub assurance: &'static str,
}
pub fn preflight_spawn(binary: &Path, allowlist: &[String]) -> std::io::Result<SpawnPreflight> {
    use std::io::Read;
    let mut f = fs::File::open(binary)?;
    let mut h = blake3::Hasher::new();
    let mut b = [0; 65536];
    loop {
        let n = f.read(&mut b)?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    let hash = h.finalize().to_hex().to_string();
    let name = binary.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let allowed = allowlist.iter().any(|a| a == &hash || a == name);
    Ok(SpawnPreflight {
        hash,
        allowed,
        assurance: "content hashed before exec; TOCTOU remains until exec-time BPF-LSM pinning",
    })
}

/// Deterministic deny-by-default Seatbelt profile. Grants must exist so
/// canonicalization cannot silently broaden a symlinked or lexical path.
pub fn seatbelt_profile(grants: &FsSandbox) -> Result<String, String> {
    let mut read = canonical_grants(&grants.read)?;
    let mut write = canonical_grants(&grants.write)?;
    let mut delete = canonical_grants(&grants.delete)?;
    read.sort();
    read.dedup();
    write.sort();
    write.dedup();
    delete.sort();
    delete.dedup();
    let mut out = String::from(
        "(version 1)\n(deny default)\n(allow process*)\n(allow signal (target self))\n",
    );
    for p in read {
        out.push_str(&format!(
            "(allow file-read* (subpath \"{}\"))\n",
            seatbelt_escape(&p)?
        ));
    }
    for p in write {
        out.push_str(&format!(
            "(allow file-read* file-write* (subpath \"{}\"))\n",
            seatbelt_escape(&p)?
        ));
    }
    for p in delete {
        out.push_str(&format!(
            "(allow file-read-metadata file-write-unlink (subpath \"{}\"))\n",
            seatbelt_escape(&p)?
        ));
    }
    Ok(out)
}
fn canonical_grants(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    paths
        .iter()
        .map(|p| {
            fs::canonicalize(p)
                .map_err(|e| format!("cannot canonicalize Seatbelt grant {}: {e}", p.display()))
        })
        .collect()
}
fn seatbelt_escape(path: &Path) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("Seatbelt cannot encode non-UTF-8 path {}", path.display()))?;
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => return Err("Seatbelt grant contains a control character".into()),
            c => out.push(c),
        }
    }
    Ok(out)
}

#[cfg(target_os = "macos")]
pub fn apply_macos_sandbox(grants: &FsSandbox) -> Result<EnforcementStatus, String> {
    use std::ffi::{CStr, CString, c_char};
    let profile =
        CString::new(seatbelt_profile(grants)?).map_err(|_| "Seatbelt profile contains NUL")?;
    let mut error: *mut c_char = std::ptr::null_mut();
    let result = unsafe { sandbox_init(profile.as_ptr(), 0, &mut error) };
    if result != 0 {
        let message = if error.is_null() {
            "sandbox_init failed".into()
        } else {
            unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned()
        };
        if !error.is_null() {
            unsafe { sandbox_free_error(error) }
        }
        return Err(message);
    }
    Ok(EnforcementStatus{available_tier:EnforcementTier::C,active_tier:Some(EnforcementTier::C),enforced:true,detail:"Seatbelt filesystem profile active; spawn preflight is TOCTOU-prone; network enforcement unavailable".into(),landlock_abi:None,filesystem_enforced:true,spawn_exec_enforced:false,network_enforced:false})
}
#[cfg(target_os = "macos")]
#[link(name = "sandbox")]
unsafe extern "C" {
    fn sandbox_init(
        profile: *const std::ffi::c_char,
        flags: u64,
        error: *mut *mut std::ffi::c_char,
    ) -> std::ffi::c_int;
    fn sandbox_free_error(error: *mut std::ffi::c_char);
}
#[cfg(not(target_os = "macos"))]
pub fn apply_macos_sandbox(_: &FsSandbox) -> Result<EnforcementStatus, String> {
    Err("Seatbelt enforcement is only available on macOS".into())
}

/// Apply the strongest OS filesystem sandbox this platform has to the current
/// process, immediately before exec in a child: Linux → [`apply_landlock`],
/// macOS → [`apply_macos_sandbox`] (Seatbelt), otherwise an honest error.
///
/// This is the single per-platform entry point a spawn launcher (the
/// `shoal-sandbox-exec` helper the exec layer wraps children through) should
/// call, so the Seatbelt path on macOS is exercised exactly as Landlock is on
/// Linux — macOS is first-class, not a stub. Like the backend it delegates to,
/// this irreversibly restricts the calling thread/process and must only run in
/// the child after fork.
pub fn apply_sandbox(grants: &FsSandbox) -> Result<EnforcementStatus, String> {
    #[cfg(target_os = "linux")]
    {
        apply_landlock(grants)
    }
    #[cfg(target_os = "macos")]
    {
        apply_macos_sandbox(grants)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = grants;
        Err("no OS sandbox backend for this platform".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy() -> Policy {
        Policy::from_toml(
            r#"
[principal.agent]
fs.read = ["/work/**", "/usr/**"]
fs.write = ["/work/generated/**"]
fs.delete = ["/work/generated/tmp/**"]
net_connect = ["github.com:443", "*.docs.rs:443"]
net_listen = [8080]
proc_spawn = ["cargo", "deadbeef"]
env_read = ["PATH", "HOME"]
secret_use = []
session_write = true
journal_read = true
time = true
auto_apply = "reversible"
opaque = "ask"
"#,
        )
        .unwrap()
    }

    #[test]
    fn effect_codec_roundtrip() {
        let e = Effect::ProcSpawn {
            bin_hash: "abc".into(),
            argv0: "cargo".into(),
        };
        let j = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<Effect>(&j).unwrap(), e);
    }
    #[test]
    fn path_scope_does_not_allow_siblings_or_dotdot() {
        let p = policy();
        assert_eq!(
            p.evaluate_effect(
                "agent",
                &Effect::FsRead {
                    paths: vec!["/work/src/a.rs".into()]
                }
            ),
            Verdict::Allow
        );
        assert_eq!(
            p.evaluate_effect(
                "agent",
                &Effect::FsWrite {
                    paths: vec!["/work/generated/../src/a".into()]
                }
            ),
            Verdict::Deny
        );
    }
    #[test]
    fn network_wildcards_and_ports() {
        let p = policy();
        assert_eq!(
            p.evaluate_effect(
                "agent",
                &Effect::NetConnect {
                    host: "foo.docs.rs".into(),
                    port: 443
                }
            ),
            Verdict::Allow
        );
        assert_eq!(
            p.evaluate_effect(
                "agent",
                &Effect::NetConnect {
                    host: "foo.docs.rs".into(),
                    port: 80
                }
            ),
            Verdict::Deny
        );
    }
    #[test]
    fn spawn_matches_hash_or_name() {
        let p = policy();
        for e in [
            Effect::ProcSpawn {
                bin_hash: "x".into(),
                argv0: "/usr/bin/cargo".into(),
            },
            Effect::ProcSpawn {
                bin_hash: "deadbeef".into(),
                argv0: "renamed".into(),
            },
        ] {
            assert_eq!(p.evaluate_effect("agent", &e), Verdict::Allow)
        }
    }
    #[test]
    fn opaque_and_unknown_principal() {
        let p = policy();
        assert_eq!(
            p.evaluate_effect("agent", &Effect::Opaque),
            Verdict::ApprovalRequired
        );
        assert_eq!(p.evaluate_effect("missing", &Effect::Time), Verdict::Deny);
    }
    #[test]
    fn plan_ref_is_stable_and_auto_apply_respects_reversibility() {
        let p = policy();
        let a = Plan::new(
            vec![Effect::FsWrite {
                paths: vec!["/work/generated/a".into()],
            }],
            Reversibility::Reversible,
            Estimates {
                bytes: Some(4),
                items: Some(1),
            },
        );
        let b = Plan::new(a.effects.clone(), a.reversibility, a.estimates.clone());
        assert_eq!(a.plan_ref, b.plan_ref);
        assert_eq!(p.evaluate_plan("agent", &a), Verdict::Allow);
        let irreversible = Plan::new(
            a.effects.clone(),
            Reversibility::Irreversible,
            Estimates::default(),
        );
        assert_eq!(
            p.evaluate_plan("agent", &irreversible),
            Verdict::ApprovalRequired
        );
    }
    #[test]
    fn denial_dominates_plan() {
        let p = policy();
        let plan = Plan::new(
            vec![
                Effect::Opaque,
                Effect::FsDelete {
                    paths: vec!["/etc/passwd".into()],
                },
            ],
            Reversibility::Unknown,
            Estimates::default(),
        );
        assert_eq!(p.evaluate_plan("agent", &plan), Verdict::Deny);
    }
    #[test]
    fn enforcement_is_honest() {
        let s = EnforcementStatus::detect();
        assert!(!s.enforced);
        assert_eq!(s.active_tier, None);
        // `detect()`'s wording of *why* nothing is enforced is intentionally
        // platform-specific (this crate installs no backend itself, so the
        // message just describes what host mechanism is missing): Linux
        // phrases it as a landlock/seccomp/network mechanism being
        // "unavailable"; macOS phrases it as the Seatbelt backend being "not
        // installed"; anything else falls back to "advisory". All three
        // honestly report the same underlying fact (no OS-level enforcement
        // is active) in the phrasing appropriate to that platform's branch
        // in `detect()`.
        if cfg!(target_os = "linux") {
            assert!(s.detail.contains("unavailable"), "{}", s.detail);
        } else if cfg!(target_os = "macos") {
            assert!(s.detail.contains("not installed"), "{}", s.detail);
        } else {
            assert!(s.detail.contains("advisory"), "{}", s.detail);
        }
    }
    #[test]
    fn spawn_preflight_hashes_content_and_labels_toctou() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("tool");
        fs::write(&p, b"binary").unwrap();
        let first = preflight_spawn(&p, &[]).unwrap();
        assert!(!first.allowed);
        assert!(first.assurance.contains("TOCTOU"));
        assert!(
            preflight_spawn(&p, std::slice::from_ref(&first.hash))
                .unwrap()
                .allowed
        );
    }
    #[test]
    fn seatbelt_profile_is_canonical_sorted_and_escaped() {
        let d = tempfile::tempdir().unwrap();
        let weird = d.path().join("quote\"and\\slash");
        fs::create_dir(&weird).unwrap();
        let profile = seatbelt_profile(&FsSandbox {
            read: vec![weird.clone(), weird.clone()],
            write: vec![],
            delete: vec![],
        })
        .unwrap();
        assert!(profile.starts_with("(version 1)\n(deny default)"));
        assert_eq!(profile.matches("file-read* (subpath").count(), 1);
        assert!(profile.contains("quote\\\"and\\\\slash"));
    }
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn seatbelt_application_is_explicitly_unavailable() {
        assert!(
            apply_macos_sandbox(&FsSandbox::default())
                .unwrap_err()
                .contains("only available on macOS")
        );
    }
    #[test]
    fn sandbox_policy_defaults_are_unrestricted_and_advisory() {
        let p = SandboxPolicy::default();
        assert_eq!(p.net, NetPolicy::Unrestricted);
        assert!(p.spawn_hash.is_none());
        assert!(!p.hermetic);
        assert!(p.fs.read.is_empty());
    }

    #[test]
    fn grant_root_reduces_globs_to_concrete_prefix() {
        assert_eq!(grant_root("/work/**"), Some(PathBuf::from("/work")));
        assert_eq!(grant_root("/**"), Some(PathBuf::from("/")));
        assert_eq!(grant_root("/etc/hosts"), Some(PathBuf::from("/etc/hosts")));
        assert_eq!(grant_root("/a/b*/c"), Some(PathBuf::from("/a")));
        assert_eq!(grant_root("**/foo"), None);
    }

    #[test]
    fn permissive_policy_is_fs_unrestricted_and_yields_no_sandbox() {
        // The default-permissive policy must never wrap a spawn — that is the
        // zero-regression guarantee. `sandbox_for` returns None so the child
        // runs exactly as it does today.
        let p = Policy::permissive("uid:1000");
        assert!(p.principal("uid:1000").unwrap().is_fs_unrestricted());
        assert!(p.sandbox_for("uid:1000").is_none());
        // Every effect a normal command needs is allowed.
        assert_eq!(
            p.evaluate_effect(
                "uid:1000",
                &Effect::FsRead {
                    paths: vec!["/anywhere/at/all".into()]
                }
            ),
            Verdict::Allow
        );
    }

    #[test]
    fn scoped_policy_yields_a_sandbox_with_existing_roots_only() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("work");
        fs::create_dir(&real).unwrap();
        let src = format!(
            "[principal.agent]\nhermetic=true\n\n[principal.agent.fs]\n\
             read=[\"{}/**\", \"/does/not/exist/**\"]\nwrite=[\"{}/**\"]\n",
            real.display(),
            real.display()
        );
        let policy = Policy::from_toml(&src).unwrap();
        let sandbox = policy.sandbox_for("agent").expect("scoped → Some sandbox");
        // The existing root survives; the non-existent one is dropped.
        assert_eq!(sandbox.fs.read, vec![real.clone()]);
        assert_eq!(sandbox.fs.write, vec![real.clone()]);
        assert!(sandbox.fs.delete.is_empty());
        // `hermetic` is carried through from the principal.
        assert!(sandbox.hermetic);
        assert!(!policy.principal("agent").unwrap().is_fs_unrestricted());
    }

    #[test]
    fn unscoped_or_unknown_principal_yields_no_sandbox() {
        // A principal that declares no fs scope has nothing concrete to
        // confine to (the plan layer denies its fs effects instead), and an
        // unknown principal likewise never wraps a spawn.
        let policy = Policy::from_toml("[principal.agent]\nopaque='deny'\n").unwrap();
        assert!(policy.sandbox_for("agent").is_none());
        assert!(policy.sandbox_for("nobody").is_none());
    }

    #[test]
    fn load_user_or_permissive_reads_file_then_falls_back() {
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("shoal");
        fs::create_dir_all(&cfg).unwrap();
        fs::write(
            cfg.join("leash.toml"),
            "[principal.agent]\n\n[principal.agent.fs]\nread=[\"/srv/**\"]\n",
        )
        .unwrap();
        // Point XDG_CONFIG_HOME at the fixture for the duration of this test.
        // Serialized against other env-touching tests via a process-global lock.
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", d.path()) };
        let loaded = Policy::load_user_or_permissive("uid:0");
        // The file defines `agent`, not `uid:0`; the loaded policy reflects the
        // file, so `uid:0` is unknown there (not permissive).
        assert!(loaded.principal("agent").is_some());
        assert!(loaded.principal("uid:0").is_none());
        // With no config file, we get the permissive fallback for the principal.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", d.path().join("empty")) };
        let fallback = Policy::load_user_or_permissive("uid:0");
        assert!(fallback.principal("uid:0").unwrap().is_fs_unrestricted());
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
