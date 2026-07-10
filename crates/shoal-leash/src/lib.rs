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
        assert!(s.detail.contains("unavailable"));
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
}
