//! Platform-specific effective-UID authentication for named Unix listeners.

use std::io;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
pub(crate) const fn supported() -> bool {
    true
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
pub(crate) const fn supported() -> bool {
    false
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn uid(stream: &UnixStream) -> io::Result<u32> {
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::zeroed();
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `stream` owns a live Unix socket and both output pointers refer
    // to correctly-sized writable stack storage for `SO_PEERCRED`.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &raw mut len,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    if len as usize != std::mem::size_of::<libc::ucred>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SO_PEERCRED returned an unexpected credential size",
        ));
    }
    // SAFETY: successful `getsockopt` initialized the full `ucred` value and
    // the exact-size check above rejects truncated output.
    Ok(unsafe { credentials.assume_init() }.uid)
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
fn uid(stream: &UnixStream) -> io::Result<u32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: `stream` owns a live Unix socket and both credential outputs are
    // valid writable stack slots. `getpeereid` does not retain the pointers.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &raw mut uid, &raw mut gid) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
fn uid(_stream: &UnixStream) -> io::Result<u32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Unix peer UID lookup is unavailable on this platform",
    ))
}

pub(crate) fn require_matching_effective_uid(stream: &UnixStream) -> io::Result<()> {
    let actual = uid(stream)?;
    // SAFETY: `geteuid` has no arguments and no memory-safety preconditions.
    let required = unsafe { libc::geteuid() };
    if actual == required {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("peer uid {actual} does not match required uid {required}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connected_pair_reports_the_current_effective_uid() {
        let (left, _right) = UnixStream::pair().unwrap();
        require_matching_effective_uid(&left).unwrap();
    }
}
