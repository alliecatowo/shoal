//! Bounded, no-follow inspection of metadata outside portable copy semantics.

use std::io;
use std::path::Path;

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) fn has_extended_attributes(path: &Path) -> io::Result<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    // SAFETY: `path` is a live NUL-terminated CString; a null output buffer
    // with length zero asks the OS only for the required xattr-list length.
    let count = unsafe { libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0) };
    if count < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOTSUP) {
            Ok(false)
        } else {
            Err(error)
        }
    } else if count == 0 {
        Ok(false)
    } else if count as usize > 64 * 1024 {
        // A hostile metadata list need not be retained to know the source is
        // outside the portable contract.
        Ok(true)
    } else {
        let mut names = vec![0u8; count as usize];
        // SAFETY: `names` is writable for exactly its advertised length and
        // `path` remains live. Growth races fail closed below.
        let read =
            unsafe { libc::llistxattr(path.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
        if read < 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ERANGE) {
                Ok(true)
            } else {
                Err(error)
            };
        }
        names.truncate(read as usize);
        Ok(names
            .split(|byte| *byte == 0)
            .any(|name| !name.is_empty() && name != b"security.selinux"))
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(super) fn has_extended_attributes(path: &Path) -> io::Result<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    // SAFETY: `path` is a live NUL-terminated CString; a null output buffer
    // with length zero asks only for the list length, and XATTR_NOFOLLOW keeps
    // this metadata query on the directory entry selected by the caller.
    let count =
        unsafe { libc::listxattr(path.as_ptr(), std::ptr::null_mut(), 0, libc::XATTR_NOFOLLOW) };
    if count < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOTSUP) {
            Ok(false)
        } else {
            Err(error)
        }
    } else {
        Ok(count != 0)
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
)))]
pub(super) fn has_extended_attributes(_: &Path) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "extended-attribute inspection is unavailable on this platform",
    ))
}
