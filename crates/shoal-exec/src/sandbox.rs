//! Wires shoal-leash's OS enforcement machinery into the actual spawn path
//! via [`crate::ExecSpec::sandbox`].
//!
//! Strongest-available tier, honest degrade (TDD §8): when the requested
//! [`shoal_leash::SandboxPolicy`] cannot be fully enforced on this host, the
//! child still runs (so a shell without Landlock doesn't just stop working)
//! *unless* `policy.hermetic` was set, in which case we refuse to spawn
//! rather than lie. Either way the caller gets back the true
//! [`shoal_leash::EnforcementStatus`] — never a silent "it's fine".

use crate::ExecSpec;
use crate::which::resolve_program;
use shoal_leash::{EnforcementStatus, EnforcementTier, FsSandbox, NetPolicy};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// If `spec.sandbox` is set, resolve the program, verify any spawn-hash pin,
/// rewrite `argv` to route through the strongest enforcement helper this
/// platform has, and return the status that will actually be in effect.
/// `spec` is untouched (and `Ok(None)` returned) when no sandbox was
/// requested — the common, existing-caller path.
pub(crate) fn apply(spec: &mut ExecSpec) -> io::Result<Option<EnforcementStatus>> {
    let Some(policy) = spec.sandbox.take() else {
        return Ok(None);
    };
    let program = resolve_program(&spec.argv, &spec.env)?;
    if let Some(pin) = &policy.spawn_hash {
        verify_pin(&program, pin)?;
    }

    let mut status = if cfg!(target_os = "linux") && shoal_leash::landlock_abi().is_some() {
        spec.argv = wrap(sandbox_helper()?, &policy.fs, program, &spec.argv);
        EnforcementStatus {
            available_tier: EnforcementTier::A,
            active_tier: Some(EnforcementTier::A),
            enforced: true,
            detail: "Landlock applied to the spawned child before exec; seccomp/netns \
                      unavailable so net policy is advisory only"
                .into(),
            landlock_abi: shoal_leash::landlock_abi(),
            filesystem_enforced: true,
            spawn_exec_enforced: policy.spawn_hash.is_some(),
            network_enforced: false,
        }
    } else if cfg!(target_os = "macos") {
        spec.argv = wrap(sandbox_helper()?, &policy.fs, program, &spec.argv);
        EnforcementStatus {
            available_tier: EnforcementTier::C,
            active_tier: Some(EnforcementTier::C),
            enforced: true,
            detail: "Seatbelt profile applied to the spawned child before exec; net policy \
                      is advisory only"
                .into(),
            landlock_abi: None,
            filesystem_enforced: true,
            spawn_exec_enforced: policy.spawn_hash.is_some(),
            network_enforced: false,
        }
    } else {
        let mut degraded = EnforcementStatus::detect();
        degraded.detail = format!(
            "{}; sandbox was requested but not applied — child runs WITHOUT OS confinement",
            degraded.detail
        );
        degraded
    };

    if policy.net == NetPolicy::Deny && !status.network_enforced {
        status
            .detail
            .push_str("; net.deny requested but no seccomp/netns backend exists to enforce it");
    }

    if policy.hermetic {
        let net_ok = policy.net != NetPolicy::Deny || status.network_enforced;
        if !status.enforced || !net_ok {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "hermetic sandbox requested but cannot be fully enforced on this host: {}",
                    status.detail
                ),
            ));
        }
    }

    Ok(Some(status))
}

/// Verify the on-disk binary's content hash matches `pin` before exec
/// (TDD §8 spawn-hash pin). TOCTOU remains between this check and exec —
/// the same caveat [`shoal_leash::preflight_spawn`] documents — but a
/// mismatch here is a certain, verifiable reason to refuse.
fn verify_pin(program: &Path, pin: &str) -> io::Result<()> {
    let actual = shoal_leash::preflight_spawn(program, &[])?;
    if actual.hash != pin {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "spawn hash pin mismatch for {}: on-disk hash {} != pinned {pin} ({})",
                program.display(),
                actual.hash,
                actual.assurance
            ),
        ));
    }
    Ok(())
}

/// Locate the `shoal-sandbox-exec` launcher installed beside the current
/// executable (same directory, or its parent — covers `target/{debug,
/// release}/shoal` finding a sibling bin, and `target/{debug,release}/deps`
/// test binaries finding it one level up).
pub(crate) fn sandbox_helper() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let name = "shoal-sandbox-exec";
    for dir in [exe.parent(), exe.parent().and_then(Path::parent)]
        .into_iter()
        .flatten()
    {
        let p = dir.join(name);
        if p.is_file() {
            return Ok(p);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "shoal-sandbox-exec helper not installed beside executable",
    ))
}

/// Build `[helper, --read p, ..., --write p, ..., --delete p, ..., --, program, argv[1..]]`.
pub(crate) fn wrap(
    helper: PathBuf,
    fs: &FsSandbox,
    program: PathBuf,
    argv: &[OsString],
) -> Vec<OsString> {
    let mut out = vec![helper.into_os_string()];
    for path in &fs.read {
        out.push("--read".into());
        out.push(path.clone().into_os_string());
    }
    for path in &fs.write {
        out.push("--write".into());
        out.push(path.clone().into_os_string());
    }
    for path in &fs.delete {
        out.push("--delete".into());
        out.push(path.clone().into_os_string());
    }
    out.push("--".into());
    out.push(program.into_os_string());
    if argv.len() > 1 {
        out.extend(argv[1..].iter().cloned());
    }
    out
}
