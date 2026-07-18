//! Wires shoal-leash's OS enforcement machinery into the actual spawn path
//! via [`crate::ExecSpec::sandbox`].
//!
//! Strongest-available tier, honest degrade (site/content/internals/language-conformance-contract.md): when the requested
//! [`shoal_leash::SandboxPolicy`] cannot be fully enforced on this host, the
//! child still runs (so a shell without Landlock doesn't just stop working)
//! *unless* `policy.hermetic` was set, in which case we refuse to spawn
//! rather than lie. Either way the caller gets back the true
//! [`shoal_leash::EnforcementStatus`] — never a silent "it's fine".

use crate::ExecSpec;
use crate::which::resolve_program;
use shoal_leash::{EnforcementStatus, EnforcementTier, FsSandbox, NetPolicy, ProcessLimits};
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
    let fs_has_roots =
        !policy.fs.read.is_empty() || !policy.fs.write.is_empty() || !policy.fs.delete.is_empty();
    let filesystem_requested = policy.filesystem_requested || fs_has_roots;
    if policy.hermetic && filesystem_requested && !fs_has_roots {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "hermetic filesystem scope resolved to no usable roots",
        ));
    }
    if policy.process_limits.cpu_seconds == Some(0) || policy.process_limits.memory_bytes == Some(0)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process CPU and memory limits must be greater than zero",
        ));
    }
    if policy.hermetic && policy.spawn_hash.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "hermetic executable identity is unavailable: hash verification is pre-exec and TOCTOU-prone",
        ));
    }
    let program = resolve_program(&spec.argv, &spec.env, &spec.cwd)?;
    if let Some(pin) = &policy.spawn_hash {
        verify_pin(&program, pin)?;
    }

    let os_confinement_requested = fs_has_roots || policy.net == NetPolicy::Deny;
    let linux_backend = os_confinement_requested
        && cfg!(target_os = "linux")
        && shoal_leash::landlock_abi().is_some_and(|abi| policy.net != NetPolicy::Deny || abi >= 4);
    let mut status = if linux_backend {
        spec.argv = wrap(
            sandbox_helper()?,
            &policy.fs,
            policy.net,
            policy.process_limits,
            program,
            &spec.argv,
        );
        EnforcementStatus {
            available_tier: EnforcementTier::A,
            active_tier: Some(EnforcementTier::A),
            enforced: true,
            detail: format!(
                "Landlock applied to the spawned child before exec; TCP deny {}",
                if policy.net == NetPolicy::Deny {
                    "active"
                } else {
                    "not requested"
                }
            ),
            landlock_abi: shoal_leash::landlock_abi(),
            filesystem_enforced: fs_has_roots,
            spawn_exec_enforced: policy.spawn_hash.is_some(),
            network_enforced: policy.net == NetPolicy::Deny,
            cpu_limit_enforced: policy.process_limits.cpu_seconds.is_some(),
            memory_limit_enforced: policy.process_limits.memory_bytes.is_some(),
        }
    } else if os_confinement_requested && cfg!(target_os = "macos") {
        spec.argv = wrap(
            sandbox_helper()?,
            &policy.fs,
            policy.net,
            policy.process_limits,
            program,
            &spec.argv,
        );
        EnforcementStatus {
            available_tier: EnforcementTier::C,
            active_tier: Some(EnforcementTier::C),
            enforced: true,
            detail: format!(
                "Seatbelt profile applied to the spawned child before exec; network {}",
                if policy.net == NetPolicy::Deny {
                    "denied"
                } else {
                    "unrestricted"
                }
            ),
            landlock_abi: None,
            filesystem_enforced: fs_has_roots,
            spawn_exec_enforced: policy.spawn_hash.is_some(),
            network_enforced: policy.net == NetPolicy::Deny,
            cpu_limit_enforced: policy.process_limits.cpu_seconds.is_some(),
            memory_limit_enforced: policy.process_limits.memory_bytes.is_some(),
        }
    } else {
        let mut degraded = EnforcementStatus::detect();
        if !policy.process_limits.is_empty() {
            spec.argv = wrap(
                sandbox_helper()?,
                &FsSandbox::default(),
                NetPolicy::Unrestricted,
                policy.process_limits,
                program,
                &spec.argv,
            );
            degraded.enforced = true;
            degraded.cpu_limit_enforced = policy.process_limits.cpu_seconds.is_some();
            degraded.memory_limit_enforced = policy.process_limits.memory_bytes.is_some();
            degraded
                .detail
                .push_str("; inherited per-process resource ceilings configured in child launcher");
        }
        degraded.spawn_exec_enforced = policy.spawn_hash.is_some();
        if os_confinement_requested {
            degraded.detail.push_str(
                "; requested filesystem/network sandbox was not applied — child runs without that OS confinement",
            );
        } else if filesystem_requested {
            degraded.detail.push_str(
                "; requested filesystem scope resolved to no usable roots and was not applied",
            );
        }
        degraded
    };

    if policy.net == NetPolicy::Deny && !status.network_enforced {
        status
            .detail
            .push_str("; net.deny requested but this host has no compatible OS backend");
    }

    if policy.hermetic {
        let fs_ok = !filesystem_requested || status.filesystem_enforced;
        let net_ok = policy.net != NetPolicy::Deny || status.network_enforced;
        let cpu_ok = policy.process_limits.cpu_seconds.is_none() || status.cpu_limit_enforced;
        let memory_ok =
            policy.process_limits.memory_bytes.is_none() || status.memory_limit_enforced;
        if !fs_ok || !net_ok || !cpu_ok || !memory_ok {
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
/// (site/content/internals/language-conformance-contract.md spawn-hash pin). TOCTOU remains between this check and exec —
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
    net: NetPolicy,
    process_limits: ProcessLimits,
    program: PathBuf,
    argv: &[OsString],
) -> Vec<OsString> {
    let mut out = vec![helper.into_os_string()];
    if net == NetPolicy::Deny {
        out.push("--deny-net".into());
    }
    if let Some(seconds) = process_limits.cpu_seconds {
        out.push("--cpu-seconds".into());
        out.push(seconds.to_string().into());
    }
    if let Some(bytes) = process_limits.memory_bytes {
        out.push("--memory-bytes".into());
        out.push(bytes.to_string().into());
    }
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
