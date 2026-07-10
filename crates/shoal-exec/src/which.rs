//! `PATH` resolution — no shell involved, ever.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Resolve `name` to an executable file.
///
/// - A `name` containing `/` is checked directly (no search).
/// - Otherwise each directory of `path_var` (or the process `PATH` when
///   `None`) is tried in order; an empty `PATH` component means the current
///   directory, per POSIX.
///
/// A hit must be a regular file with at least one execute bit set. Returns
/// `None` when nothing matches. No shell is consulted at any point.
#[must_use]
pub fn which(name: &OsStr, path_var: Option<&OsStr>) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    if name.as_bytes().contains(&b'/') {
        let p = PathBuf::from(name);
        return is_executable_file(&p).then_some(p);
    }
    let process_path: OsString;
    let path = match path_var {
        Some(p) => p,
        None => {
            process_path = std::env::var_os("PATH")?;
            &process_path
        }
    };
    for dir in std::env::split_paths(path) {
        let dir = if dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dir
        };
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable_file(p: &Path) -> bool {
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// Resolve `argv[0]` for spawning: run as-is when it contains `/`, otherwise
/// search the spec's `PATH` (falling back to the process `PATH` when the spec
/// environment has none).
pub(crate) fn resolve_program(
    argv: &[OsString],
    env: &[(OsString, OsString)],
) -> io::Result<PathBuf> {
    let Some(argv0) = argv.first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty argv: nothing to execute",
        ));
    };
    if argv0.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty program name",
        ));
    }
    if argv0.as_bytes().contains(&b'/') {
        return Ok(PathBuf::from(argv0));
    }
    let spec_path = env
        .iter()
        .find(|(k, _)| k.as_os_str() == OsStr::new("PATH"))
        .map(|(_, v)| v.as_os_str());
    which(argv0, spec_path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("command not found: {}", argv0.to_string_lossy()),
        )
    })
}
