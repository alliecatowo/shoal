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

/// Resolve an executable exactly as a child rooted at `cwd` would: relative
/// and empty `PATH` entries are interpreted against that cwd rather than the
/// host process's current directory.
#[must_use]
pub fn which_in(name: &OsStr, path_var: Option<&OsStr>, cwd: &Path) -> Option<PathBuf> {
    which_from(name, path_var, cwd).ok().flatten()
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
    cwd: &Path,
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
        let path = PathBuf::from(argv0);
        return Ok(if path.is_absolute() {
            path
        } else {
            absolute_cwd(cwd)?.join(path)
        });
    }
    let spec_path = env
        .iter()
        .find(|(k, _)| k.as_os_str() == OsStr::new("PATH"))
        .map(|(_, v)| v.as_os_str());
    which_from(argv0, spec_path, cwd)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("command not found: {}", argv0.to_string_lossy()),
        )
    })
}

fn which_from(name: &OsStr, path_var: Option<&OsStr>, cwd: &Path) -> io::Result<Option<PathBuf>> {
    let process_path: OsString;
    let path = match path_var {
        Some(path) => path,
        None => {
            let Some(path) = std::env::var_os("PATH") else {
                return Ok(None);
            };
            process_path = path;
            &process_path
        }
    };
    let cwd = absolute_cwd(cwd)?;
    for directory in std::env::split_paths(path) {
        let directory = if directory.is_absolute() {
            directory
        } else {
            cwd.join(directory)
        };
        let candidate = directory.join(name);
        if is_executable_file(&candidate) {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn absolute_cwd(cwd: &Path) -> io::Result<PathBuf> {
    if cwd.is_absolute() {
        Ok(cwd.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(cwd))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn which_in_resolves_relative_path_entries_from_child_cwd() {
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("relative-bin");
        std::fs::create_dir(&bin).unwrap();
        let tool = bin.join("tool");
        std::fs::write(&tool, b"fixture").unwrap();
        let mut permissions = std::fs::metadata(&tool).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&tool, permissions).unwrap();

        assert_eq!(
            which_in(
                OsStr::new("tool"),
                Some(OsStr::new("relative-bin")),
                directory.path(),
            ),
            Some(tool)
        );
    }
}
