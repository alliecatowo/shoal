//! Cross-platform connection deadline and peer-disconnect normalization.

use std::io;
use std::os::unix::net::UnixStream;

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
