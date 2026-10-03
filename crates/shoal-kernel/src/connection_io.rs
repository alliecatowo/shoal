//! Cross-platform connection deadline and peer-disconnect normalization.

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

pub(crate) fn set_read_deadline(stream: &UnixStream, timeout_ms: Option<u64>) -> io::Result<()> {
    stream.set_read_timeout(timeout_ms.map(std::time::Duration::from_millis))
}

pub(crate) fn is_read_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}

/// Once a client has authenticated, losing its socket is an ordinary peer
/// disconnect, not a kernel failure. Darwin can report `EINVAL` when a peer
/// closes concurrently with `setsockopt(SO_RCVTIMEO)`; Linux more commonly
/// reports reset/broken-pipe variants. Keep pre-auth framing/admission errors
/// observable by applying this normalization only to an attached connection.
pub(crate) fn normalize_attached_disconnect(
    result: io::Result<()>,
    attached: bool,
) -> io::Result<()> {
    match result {
        Err(error)
            if attached
                && matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::InvalidInput
                        | io::ErrorKind::UnexpectedEof
                ) =>
        {
            Ok(())
        }
        other => other,
    }
}

/// Accept failures the listener survives: aborted handshakes, signals, and
/// fd/memory pressure that clears on its own.
pub(crate) fn is_transient_accept_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::Interrupted
            | io::ErrorKind::OutOfMemory
    ) || matches!(
        error.raw_os_error(),
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM | libc::EPROTO)
    )
}

/// Bind `path` so it is never visible with permissions looser than 0600.
///
/// The socket is first bound inside a fresh 0700 directory beside `path`,
/// chmodded, and only then hard-linked to its final name (which fails with
/// `AddrInUse` if the name exists, like a plain bind). This avoids both the
/// bind-then-chmod window and a process-wide umask change.
pub(crate) fn bind_private_socket(path: &Path) -> io::Result<UnixListener> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let parent = parent.unwrap_or_else(|| Path::new("."));
    let staging = parent.join(format!(".shoal-bind-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::DirBuilder::new().mode(0o700).create(&staging)?;
    let result = (|| {
        let staged = staging.join("s");
        let listener = UnixListener::bind(&staged)?;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))?;
        match std::fs::hard_link(&staged, path) {
            Ok(()) => Ok(listener),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                Err(io::Error::from(io::ErrorKind::AddrInUse))
            }
            Err(error) => Err(error),
        }
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}
