//! OS-level enforcement: platform tier detection, the concrete
//! [`SandboxPolicy`] carried into a child spawn, and the per-platform
//! backends (Landlock on Linux, Seatbelt on macOS) that apply it.

#[cfg(target_os = "macos")]
use crate::seatbelt::seatbelt_profile_with_net;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

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
    pub cpu_limit_enforced: bool,
    pub memory_limit_enforced: bool,
}

impl EnforcementStatus {
    /// Detect the strongest plausible platform tier. Detection does not apply
    /// a sandbox, so `enforced` remains false and `active_tier` remains `None`.
    pub fn detect() -> Self {
        let (available_tier, detail) = if cfg!(target_os = "linux") {
            match landlock_abi() {
                Some(abi) => (
                    EnforcementTier::A,
                    format!("Landlock ABI {abi} available; no sandbox applied to this process"),
                ),
                None => (
                    EnforcementTier::B,
                    "Landlock unavailable; namespace fallback not installed".into(),
                ),
            }
        } else if cfg!(target_os = "macos") {
            (
                EnforcementTier::C,
                "Seatbelt available; no sandbox applied".into(),
            )
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
            cpu_limit_enforced: false,
            memory_limit_enforced: false,
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

/// Coarse network policy carried by [`SandboxPolicy`]. `Deny` is enforceable
/// with Landlock ABI 4+ on Linux and deny-by-default Seatbelt on macOS. It is
/// deliberately not a hostname/port allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetPolicy {
    #[default]
    Unrestricted,
    Deny,
}

/// Inherited per-process ceilings applied immediately before the sandbox
/// launcher executes the requested program. These are deliberately named
/// per-process limits: descendants inherit them, but CPU time and address
/// space are accounted independently for each process rather than as one
/// aggregate task-tree or principal budget.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessLimits {
    pub cpu_seconds: Option<u64>,
    pub memory_bytes: Option<u64>,
}

impl ProcessLimits {
    pub fn is_empty(self) -> bool {
        self.cpu_seconds.is_none() && self.memory_bytes.is_none()
    }
}

/// A concrete, resolved enforcement request for one child spawn: filesystem
/// scopes, a coarse network policy, optional inherited process ceilings, an
/// optional pinned spawn hash (site/content/internals/language-conformance-contract.md content-hash pinning), and a
/// `hermetic` intent flag.
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
    /// Whether the originating policy requested filesystem confinement. This
    /// remains true when every configured root was unresolved, allowing a
    /// hermetic request to fail closed without confusing it with a
    /// resource-only policy.
    pub filesystem_requested: bool,
    pub net: NetPolicy,
    pub spawn_hash: Option<String>,
    pub process_limits: ProcessLimits,
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
    apply_landlock_policy(grants, NetPolicy::Unrestricted)
}

#[cfg(target_os = "linux")]
pub fn apply_landlock_policy(
    grants: &FsSandbox,
    net: NetPolicy,
) -> Result<EnforcementStatus, String> {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, LandlockStatus, Ruleset,
        RulesetAttr, RulesetCreatedAttr, RulesetStatus, path_beneath_rules,
    };
    // Build the exact rights mask supported by this kernel. Requesting V7
    // under hard compatibility would reject an otherwise usable ABI 4–6
    // kernel even though all coarse TCP rights already exist at ABI 4.
    let abi = ABI::from(landlock_abi().unwrap_or_default());
    let ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| e.to_string())?;
    let ruleset = if net == NetPolicy::Deny {
        ruleset
            .handle_access(AccessNet::from_all(abi))
            .map_err(|e| e.to_string())?
    } else {
        ruleset
    };
    let mut ruleset = ruleset.create().map_err(|e| e.to_string())?;
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
    let status = ruleset.restrict_self().map_err(|e| e.to_string())?;
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
            "Landlock active ({:?}); TCP deny {}; spawn hash preflight is TOCTOU-prone",
            status.landlock,
            if net == NetPolicy::Deny {
                "active"
            } else {
                "not requested"
            }
        ),
        landlock_abi: landlock_abi(),
        filesystem_enforced: true,
        spawn_exec_enforced: false,
        network_enforced: net == NetPolicy::Deny,
        cpu_limit_enforced: false,
        memory_limit_enforced: false,
    })
}
#[cfg(not(target_os = "linux"))]
pub fn apply_landlock(_: &FsSandbox) -> Result<EnforcementStatus, String> {
    Err("Landlock is only available on Linux".into())
}
#[cfg(not(target_os = "linux"))]
pub fn apply_landlock_policy(_: &FsSandbox, _: NetPolicy) -> Result<EnforcementStatus, String> {
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
    let expected = fs::metadata(binary)?;
    if !expected.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "spawn hash target is not a regular file",
        ));
    }
    let mut f = fs::File::open(binary)?;
    if !f.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "opened spawn hash target is not a regular file",
        ));
    }
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

#[cfg(target_os = "macos")]
pub fn apply_macos_sandbox(grants: &FsSandbox) -> Result<EnforcementStatus, String> {
    apply_macos_sandbox_policy(grants, NetPolicy::Unrestricted)
}

#[cfg(target_os = "macos")]
pub fn apply_macos_sandbox_policy(
    grants: &FsSandbox,
    net: NetPolicy,
) -> Result<EnforcementStatus, String> {
    use std::ffi::{CStr, CString, c_char};
    let profile = CString::new(seatbelt_profile_with_net(grants, net)?)
        .map_err(|_| "Seatbelt profile contains NUL")?;
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
    Ok(EnforcementStatus {
        available_tier: EnforcementTier::C,
        active_tier: Some(EnforcementTier::C),
        enforced: true,
        detail: format!(
            "Seatbelt filesystem profile active; network {}; spawn preflight is TOCTOU-prone",
            if net == NetPolicy::Deny {
                "denied"
            } else {
                "unrestricted"
            }
        ),
        landlock_abi: None,
        filesystem_enforced: true,
        spawn_exec_enforced: false,
        network_enforced: net == NetPolicy::Deny,
        cpu_limit_enforced: false,
        memory_limit_enforced: false,
    })
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
#[cfg(not(target_os = "macos"))]
pub fn apply_macos_sandbox_policy(
    _: &FsSandbox,
    _: NetPolicy,
) -> Result<EnforcementStatus, String> {
    Err("Seatbelt enforcement is only available on macOS".into())
}

/// Apply the strongest OS filesystem sandbox this platform has to the current
/// process, immediately before exec in a child: Linux → Landlock, macOS →
/// Seatbelt, otherwise an honest error.
///
/// This is the single per-platform entry point a spawn launcher (the
/// `shoal-sandbox-exec` helper the exec layer wraps children through) should
/// call, so the Seatbelt path on macOS is exercised exactly as Landlock is on
/// Linux — macOS is first-class, not a stub. Like the backend it delegates to,
/// this irreversibly restricts the calling thread/process and must only run in
/// the child after fork.
pub fn apply_sandbox(grants: &FsSandbox) -> Result<EnforcementStatus, String> {
    apply_sandbox_policy(grants, NetPolicy::Unrestricted)
}

pub fn apply_sandbox_policy(
    grants: &FsSandbox,
    net: NetPolicy,
) -> Result<EnforcementStatus, String> {
    #[cfg(target_os = "linux")]
    {
        apply_landlock_policy(grants, net)
    }
    #[cfg(target_os = "macos")]
    {
        apply_macos_sandbox_policy(grants, net)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (grants, net);
        Err("no OS sandbox backend for this platform".into())
    }
}

/// Apply the requested inherited per-process ceilings to the current process.
/// This is called by the child-only launcher immediately before `exec`, never
/// by the long-lived shell or kernel process.
pub fn apply_process_limits(limits: ProcessLimits) -> Result<(), String> {
    if let Some(seconds) = limits.cpu_seconds {
        set_limit(libc::RLIMIT_CPU, seconds, "CPU seconds")?;
    }
    if let Some(bytes) = limits.memory_bytes {
        set_limit(libc::RLIMIT_AS, bytes, "address-space bytes")?;
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
type RlimitResource = libc::__rlimit_resource_t;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
type RlimitResource = libc::c_int;

fn set_limit(resource: RlimitResource, value: u64, label: &str) -> Result<(), String> {
    let value = libc::rlim_t::try_from(value)
        .map_err(|_| format!("{label} limit {value} is not representable on this host"))?;
    let limit = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    let result = unsafe { libc::setrlimit(resource, &limit) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "setrlimit({label}) failed: {}",
            std::io::Error::last_os_error()
        ))
    }
}
