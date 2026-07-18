//! Unix fd-relative, identity-guarded directory-entry mutation.

use crate::ports::FsEntryIdentity;
use std::ffi::{CString, OsStr};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

struct DirFd(RawFd);

impl DirFd {
    fn open(path: &Path) -> io::Result<Self> {
        let path = cstring(path.as_os_str())?;
        #[cfg(target_os = "linux")]
        let access = libc::O_PATH;
        #[cfg(target_vendor = "apple")]
        let access = libc::O_SEARCH;
        #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
        let access = libc::O_RDONLY;
        // SAFETY: `path` is NUL-terminated and these flags take no mode.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                access | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(fd))
        }
    }
}

impl Drop for DirFd {
    fn drop(&mut self) {
        // SAFETY: `DirFd` uniquely owns this successful `open` result.
        unsafe { libc::close(self.0) };
    }
}

pub(crate) fn rename_if_unchanged(
    from: &Path,
    to: &Path,
    expected: &FsEntryIdentity,
) -> io::Result<()> {
    let (from_parent, from_name) = split_entry(from)?;
    let (to_parent, to_name) = split_entry(to)?;
    let from_dir = DirFd::open(from_parent)?;
    let to_dir = DirFd::open(to_parent)?;
    let from_name = cstring(from_name)?;
    let to_name = cstring(to_name)?;

    rename_noreplace(from_dir.0, &from_name, to_dir.0, &to_name)?;
    match identity_at(to_dir.0, &to_name) {
        Ok(found) if expected.matches_stat(&found) => Ok(()),
        result => {
            let detail = match result {
                Ok(_) => "directory entry identity changed after preflight".to_string(),
                Err(error) => format!("cannot verify moved directory entry: {error}"),
            };
            match rename_noreplace(to_dir.0, &to_name, from_dir.0, &from_name) {
                Ok(()) => Err(io::Error::new(io::ErrorKind::InvalidData, detail)),
                Err(rollback) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{detail}; rollback failed ({rollback}); replacement is contained at {}",
                        to.display()
                    ),
                )),
            }
        }
    }
}

fn split_entry(path: &Path) -> io::Result<(&Path, &OsStr)> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "entry has no parent"))?;
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "entry has no final component")
    })?;
    Ok((parent, name))
}

fn cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "filesystem path contains an interior NUL",
        )
    })
}

fn identity_at(dir: RawFd, name: &CString) -> io::Result<libc::stat> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `name` is NUL-terminated and `stat` points to writable storage.
    let status = unsafe {
        libc::fstatat(
            dir,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if status == 0 {
        // SAFETY: successful `fstatat` initialized the structure.
        Ok(unsafe { stat.assume_init() })
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn rename_noreplace(
    from_dir: RawFd,
    from: &CString,
    to_dir: RawFd,
    to: &CString,
) -> io::Result<()> {
    // SAFETY: both names are NUL-terminated and directory fds remain live.
    let status = unsafe {
        libc::renameat2(
            from_dir,
            from.as_ptr(),
            to_dir,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_vendor = "apple")]
fn rename_noreplace(
    from_dir: RawFd,
    from: &CString,
    to_dir: RawFd,
    to: &CString,
) -> io::Result<()> {
    // SAFETY: both names are NUL-terminated and directory fds remain live.
    let status = unsafe {
        libc::renameatx_np(
            from_dir,
            from.as_ptr(),
            to_dir,
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
fn rename_noreplace(
    _from_dir: RawFd,
    _from: &CString,
    _to_dir: RawFd,
    _to: &CString,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "identity-guarded rename requires renameat2 or renameatx_np",
    ))
}
