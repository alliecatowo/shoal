use shoal_kernel::{Kernel, Limits};
use shoal_leash::Policy;
use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn main() {
    if let Err(error) = run() {
        eprintln!("shoal-kernel: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse(std::env::args_os().skip(1))?;
    let limits = args.resolved_limits();
    let socket = args.socket.unwrap_or_else(|| runtime_socket(&args.session));
    prepare_socket(&socket)?;
    let state = args.state_dir.unwrap_or_else(state_dir);
    let kernel = if let Some(path) = args.policy {
        Kernel::open_with_policy(&state, Policy::load(&path)?)?
    } else {
        Kernel::open(&state)?
    };
    kernel.configure_limits(limits);
    let stop = Arc::new(AtomicBool::new(false));
    let signal = stop.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))?;
    eprintln!("shoal-kernel: ready {}", socket.display());
    kernel.serve_until(&socket, stop)?;
    Ok(())
}

fn prepare_socket(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket needs parent"))?;
    secure_socket_dir(parent)?;
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "kernel already listening",
            ));
        }
        let meta = fs::symlink_metadata(path)?;
        if !meta.file_type().is_socket() || meta.uid() != unsafe { geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing to remove unowned non-socket path",
            ));
        }
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Make sure the socket's parent directory exists and, when the kernel owns
/// it, isn't group/world-accessible. The kernel only *tightens* permissions
/// on a directory it actually has the right to change: one it just created,
/// or one it already owns. A pre-existing directory owned by someone else
/// (e.g. a shared `/tmp` when the socket path is `--socket /tmp/x.sock`) is
/// left untouched — `chmod`ing a shared root-owned directory either fails
/// `EPERM` as a non-root caller, or, run as root, would strip access from
/// every other user of that directory. Either way it is never something the
/// kernel should attempt. The real security boundary is the socket *file*
/// itself, created `0600` at bind time (see `Kernel::serve_until`) — this is
/// defense in depth, applied only where the kernel actually has standing to
/// apply it.
fn secure_socket_dir(parent: &Path) -> io::Result<()> {
    let describe = |err: io::Error| {
        io::Error::new(
            err.kind(),
            format!(
                "cannot secure socket dir {}: {err}; use a socket path inside a directory you \
                 own, e.g. $XDG_RUNTIME_DIR/shoal/... or /tmp/shoal-<uid>/...",
                parent.display()
            ),
        )
    };
    let pre_existing = parent.exists();
    fs::create_dir_all(parent).map_err(describe)?;
    let owned_by_us = fs::metadata(parent)
        .map(|m| m.uid() == unsafe { geteuid() })
        .unwrap_or(false);
    if pre_existing && !owned_by_us {
        // Not ours to chmod: skip. The socket file created inside it is
        // still 0600, which is the boundary that actually matters.
        return Ok(());
    }
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(describe)
}

unsafe extern "C" {
    fn geteuid() -> u32;
}

fn runtime_socket(session: &str) -> PathBuf {
    runtime_dir().join("shoal").join(format!("{session}.sock"))
}

/// The runtime directory for the kernel socket. `$XDG_RUNTIME_DIR` when set;
/// otherwise `$TMPDIR/shoal-{uid}` (macOS exports `TMPDIR` but not
/// `XDG_RUNTIME_DIR`), else the hard `/tmp/shoal-{uid}` fallback. `shoal-mcp`
/// mirrors this exactly so socket discovery agrees on every platform.
fn runtime_dir() -> PathBuf {
    let uid = unsafe { geteuid() };
    if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR").filter(|s| !s.is_empty()) {
        return PathBuf::from(xdg);
    }
    if let Some(tmp) = std::env::var_os("TMPDIR").filter(|s| !s.is_empty()) {
        return PathBuf::from(tmp).join(format!("shoal-{uid}"));
    }
    PathBuf::from(format!("/tmp/shoal-{uid}"))
}
fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("shoal")
}

#[derive(Debug)]
struct Args {
    session: String,
    socket: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    policy: Option<PathBuf>,
    max_connections: Option<usize>,
    max_tasks_per_session: Option<usize>,
    max_ptys_per_session: Option<usize>,
    max_ptys_per_principal: Option<usize>,
    max_ptys_global: Option<usize>,
    max_subscriptions_per_session: Option<usize>,
    frame_read_timeout_ms: Option<u64>,
}
impl Args {
    fn parse(mut it: impl Iterator<Item = std::ffi::OsString>) -> Result<Self, String> {
        let mut a = Self {
            session: "default".into(),
            socket: None,
            state_dir: None,
            policy: None,
            max_connections: None,
            max_tasks_per_session: None,
            max_ptys_per_session: None,
            max_ptys_per_principal: None,
            max_ptys_global: None,
            max_subscriptions_per_session: None,
            frame_read_timeout_ms: None,
        };
        let parse_usize = |key: &std::ffi::OsString,
                           value: std::ffi::OsString|
         -> Result<usize, String> {
            value
                .to_str()
                .and_then(|text| text.parse().ok())
                .ok_or_else(|| format!("{} requires a non-negative integer", key.to_string_lossy()))
        };
        while let Some(k) = it.next() {
            let missing = || format!("{} requires a value", k.to_string_lossy());
            match k.to_str() {
                Some("--session") => a.session = it.next().ok_or_else(&missing)?.into_string().map_err(|_| "invalid session")?,
                Some("--socket") => a.socket = Some(it.next().ok_or_else(&missing)?.into()),
                Some("--state-dir") => a.state_dir = Some(it.next().ok_or_else(&missing)?.into()),
                Some("--policy") => a.policy = Some(it.next().ok_or_else(&missing)?.into()),
                Some("--max-connections") => {
                    a.max_connections = Some(parse_usize(&k, it.next().ok_or_else(&missing)?)?)
                }
                Some("--max-tasks-per-session") => {
                    a.max_tasks_per_session =
                        Some(parse_usize(&k, it.next().ok_or_else(&missing)?)?)
                }
                Some("--max-ptys-per-session") => {
                    a.max_ptys_per_session =
                        Some(parse_usize(&k, it.next().ok_or_else(&missing)?)?)
                }
                Some("--max-ptys-per-principal") => {
                    a.max_ptys_per_principal =
                        Some(parse_usize(&k, it.next().ok_or_else(&missing)?)?)
                }
                Some("--max-ptys-global") => {
                    a.max_ptys_global =
                        Some(parse_usize(&k, it.next().ok_or_else(&missing)?)?)
                }
                Some("--max-subscriptions-per-session") => {
                    a.max_subscriptions_per_session =
                        Some(parse_usize(&k, it.next().ok_or_else(&missing)?)?)
                }
                Some("--frame-read-timeout-ms") => {
                    a.frame_read_timeout_ms = Some(
                        it.next()
                            .ok_or_else(&missing)?
                            .to_str()
                            .and_then(|text| text.parse().ok())
                            .ok_or_else(|| {
                                "--frame-read-timeout-ms requires a non-negative integer"
                                    .to_string()
                            })?,
                    )
                }
                Some("-h" | "--help") => return Err("usage: shoal-kernel [--session NAME] [--socket PATH] [--state-dir PATH] [--policy FILE] [--max-connections N] [--max-tasks-per-session N] [--max-ptys-per-session N] [--max-ptys-per-principal N] [--max-ptys-global N] [--max-subscriptions-per-session N] [--frame-read-timeout-ms N]".into()),
                _ => return Err(format!("unknown argument {}", k.to_string_lossy())),
            }
        }
        Ok(a)
    }

    fn resolved_limits(&self) -> Limits {
        let defaults = Limits::default();
        Limits {
            max_connections: self.max_connections.unwrap_or(defaults.max_connections),
            max_tasks_per_session: self
                .max_tasks_per_session
                .unwrap_or(defaults.max_tasks_per_session),
            max_ptys_per_session: self
                .max_ptys_per_session
                .unwrap_or(defaults.max_ptys_per_session),
            max_ptys_per_principal: self
                .max_ptys_per_principal
                .unwrap_or(defaults.max_ptys_per_principal),
            max_ptys_global: self.max_ptys_global.unwrap_or(defaults.max_ptys_global),
            max_subscriptions_per_session: self
                .max_subscriptions_per_session
                .unwrap_or(defaults.max_subscriptions_per_session),
            frame_read_timeout_ms: self
                .frame_read_timeout_ms
                .unwrap_or(defaults.frame_read_timeout_ms),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn we_are_root() -> bool {
        unsafe { geteuid() == 0 }
    }

    #[test]
    fn quota_flags_override_only_the_named_limits() {
        let args = Args::parse(
            [
                "--max-connections",
                "10",
                "--max-ptys-per-session",
                "3",
                "--max-ptys-per-principal",
                "5",
                "--max-ptys-global",
                "20",
                "--frame-read-timeout-ms",
                "2500",
            ]
            .into_iter()
            .map(std::ffi::OsString::from),
        )
        .unwrap();
        let limits = args.resolved_limits();
        assert_eq!(limits.max_connections, 10);
        assert_eq!(limits.max_ptys_per_session, 3);
        assert_eq!(limits.max_ptys_per_principal, 5);
        assert_eq!(limits.max_ptys_global, 20);
        assert_eq!(limits.frame_read_timeout_ms, 2500);
        assert_eq!(
            limits.max_tasks_per_session,
            Limits::default().max_tasks_per_session
        );
    }

    #[test]
    fn quota_flags_reject_non_numeric_values() {
        let result = Args::parse(
            ["--max-connections", "many"]
                .into_iter()
                .map(std::ffi::OsString::from),
        );
        assert!(result.unwrap_err().contains("--max-connections"));
    }

    /// The bug: `--socket /tmp/x.sock` puts the socket's parent at `/tmp` —
    /// a pre-existing, root-owned, shared directory. The old
    /// `prepare_socket` unconditionally `chmod`ed the parent to `0700`,
    /// which a non-root caller cannot do to a directory it doesn't own —
    /// surfaced verbatim as "Operation not permitted (os error 1)" with no
    /// diagnostic. `prepare_socket` must now boot cleanly: it owns (and
    /// therefore secures) only directories it creates or already owns, and
    /// leaves a shared parent alone — the socket file itself (0600 at bind
    /// time) is the real boundary.
    #[test]
    fn prepare_socket_survives_a_shared_not_owned_parent_dir() {
        if we_are_root() {
            eprintln!(
                "skipping: running as root, cannot exercise a parent dir this caller doesn't own"
            );
            return;
        }
        let sock =
            std::env::temp_dir().join(format!("shoal-kbug-test-{}.sock", std::process::id()));
        let _ = fs::remove_file(&sock);
        let result = prepare_socket(&sock);
        assert!(
            result.is_ok(),
            "socket bring-up must not fail on a shared, not-owned-by-us parent: {result:?}"
        );
        let _ = fs::remove_file(&sock);
    }

    /// A directory the kernel *does* own, but genuinely cannot secure (no
    /// write permission on its own parent so `create_dir_all` fails), must
    /// fail with a message that NAMES the cause and tells the caller how to
    /// route around it — never a bare, unexplained OS errno.
    #[test]
    fn secure_socket_dir_wraps_a_real_failure_descriptively() {
        if we_are_root() {
            eprintln!("skipping: root bypasses the permission check this test relies on");
            return;
        }
        let base = tempfile::tempdir().unwrap();
        let readonly = base.path().join("ro");
        fs::create_dir(&readonly).unwrap();
        fs::set_permissions(&readonly, fs::Permissions::from_mode(0o500)).unwrap();
        let child = readonly.join("shoal-sock-dir");

        let err = secure_socket_dir(&child).expect_err("a read-only parent cannot be secured");
        let msg = err.to_string();
        assert!(
            msg.contains("cannot secure socket dir"),
            "error must name the cause: {msg}"
        );
        assert!(
            msg.contains("use a socket path inside a directory you own"),
            "error must hint at a fix: {msg}"
        );

        // Restore write access so the tempdir's own Drop cleanup can remove it.
        fs::set_permissions(&readonly, fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// The happy path is unchanged: a fresh parent the kernel creates itself
    /// is still locked down to `0700`.
    #[test]
    fn prepare_socket_still_secures_a_freshly_created_parent() {
        let base = tempfile::tempdir().unwrap();
        let sock = base.path().join("run").join("kernel.sock");
        prepare_socket(&sock).unwrap();
        let parent_mode = fs::metadata(sock.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(parent_mode, 0o700, "a kernel-created parent must be 0700");
    }
}
