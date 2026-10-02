//! Parent-death launch gate for explicitly supervised kernel startup.

use std::fs::File;
use std::io::{self, Write};
#[cfg(not(target_os = "linux"))]
use std::os::fd::RawFd;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::Command;

pub(super) const RELEASE_BYTE: u8 = 1;

pub(super) struct LaunchGate {
    child_read: Option<OwnedFd>,
    parent_write: File,
}

impl LaunchGate {
    pub(super) fn attach(command: &mut Command, credential_id: &str) -> io::Result<Self> {
        let (read, write) = cloexec_pipe()?;
        let read_fd = read.as_raw_fd();
        let write_fd = write.as_raw_fd();
        command
            .arg("--launch-guard-fd")
            .arg(read_fd.to_string())
            .arg("--launch-guard-token-id")
            .arg(credential_id);
        // Only the read end crosses exec. Closing the child's copy of the
        // write end is essential: parent death must produce EOF immediately.
        // SAFETY: this closure uses only async-signal-safe fcntl/close calls
        // between fork and exec, and captures plain descriptor integers.
        unsafe {
            command.pre_exec(move || {
                if libc::close(write_fd) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::fcntl(read_fd, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let parent_write = File::from(write);
        Ok(Self {
            child_read: Some(read),
            parent_write,
        })
    }

    /// Drop the parent's unused read end after spawn, then release the child.
    pub(super) fn release(mut self) -> io::Result<()> {
        drop(self.child_read.take());
        self.parent_write.write_all(&[RELEASE_BYTE])?;
        self.parent_write.flush()
    }

    /// Close the parent's unused read end while retaining the release writer.
    pub(super) fn spawned(&mut self) {
        drop(self.child_read.take());
    }
}

fn cloexec_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    #[cfg(target_os = "linux")]
    // SAFETY: fds names writable storage for exactly two descriptors; Linux
    // creates both close-on-exec atomically.
    let result = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(not(target_os = "linux"))]
    // `pipe2` is unavailable on macOS. Create both descriptors, then apply
    // close-on-exec immediately below before constructing the Command.
    // SAFETY: fds names writable storage for exactly two descriptors.
    let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful pipe returned two fresh owned descriptors.
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: same as above, for the distinct write descriptor.
    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    #[cfg(not(target_os = "linux"))]
    {
        set_cloexec(read.as_raw_fd())?;
        set_cloexec(write.as_raw_fd())?;
    }
    Ok((read, write))
}

#[cfg(not(target_os = "linux"))]
fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: fd belongs to a live OwnedFd for this call.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
