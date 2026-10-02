//! Semantic copy tests grouped by the invariant they exercise.

use super::path_policy::validate_root_job;
use super::*;
use shoal_value::{Fs, StdFs, Value};
use std::path::Path;

mod limits;
mod portability;
mod races;
mod relationships;

#[cfg(any(target_os = "linux", target_os = "android"))]
fn set_xattr(path: &Path) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let name = CString::new("user.shoal-copy-test").unwrap();
    let value = b"metadata";
    // SAFETY: both C strings and the byte slice remain live for the call,
    // and their explicit lengths match their buffers.
    let result = unsafe {
        libc::setxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn set_xattr(path: &Path) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let name = CString::new("user.shoal-copy-test").unwrap();
    let value = b"metadata";
    // SAFETY: both C strings and the byte slice remain live for the call,
    // and their explicit lengths match their buffers.
    let result = unsafe {
        libc::setxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            0,
        )
    };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
}

fn assert_no_copy_temporaries(directory: &Path) {
    let leftovers = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with(".shoal-copy-"))
        .collect::<Vec<_>>();
    assert!(
        leftovers.is_empty(),
        "copy temporaries leaked: {leftovers:?}"
    );
}
