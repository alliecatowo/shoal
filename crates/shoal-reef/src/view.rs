//! PATH synthesis — PATH as an *output*, never an input (site/content/internals/reef-resolution.md).
//!
//! Given a resolved binding set, build (or reuse) a content-addressed view
//! directory of symlinks under `$XDG_RUNTIME_DIR/shoal/views/<hash>/bin`
//! (fallback `$TMPDIR/shoal-views-$UID`), and return the synthesized PATH: the
//! view dir followed by the system tail unless `hermetic`.
//!
//! Construction is idempotent and concurrent-safe: a fresh temp dir is built and
//! atomically renamed into place; an already-present view is reused as-is.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use crate::hashcache::hash_bytes;

/// A single name → binary binding to expose on the synthesized PATH.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub name: String,
    pub path: PathBuf,
}

impl Binding {
    pub fn new(name: impl Into<String>, path: impl Into<PathBuf>) -> Binding {
        Binding {
            name: name.into(),
            path: path.into(),
        }
    }
}

/// Configuration for view synthesis.
#[derive(Debug, Clone)]
pub struct ViewConfig {
    /// Root under which `views/<hash>/bin` is created.
    pub root: PathBuf,
    /// Directories forming the system tail (appended unless `hermetic`).
    pub system_tail: Vec<PathBuf>,
    /// When true, the synthesized PATH is view-only (no system tail).
    pub hermetic: bool,
}

impl ViewConfig {
    /// Default config: `default_view_root()`, the canonical system roots, and
    /// `hermetic = false`.
    pub fn from_env(hermetic: bool) -> ViewConfig {
        ViewConfig {
            root: default_view_root(),
            system_tail: default_system_tail(),
            hermetic,
        }
    }
}

/// The result of synthesizing a view.
#[derive(Debug, Clone)]
pub struct SynthView {
    /// The `.../views/<hash>/bin` directory.
    pub view_dir: PathBuf,
    /// The full synthesized `PATH` value (view dir + system tail unless hermetic).
    pub path_var: OsString,
}

/// `$XDG_RUNTIME_DIR/shoal/views` if set, else `$TMPDIR/shoal-views-<uid>`
/// (default `/tmp`).
pub fn default_view_root() -> PathBuf {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt).join("shoal/views");
    }
    let tmp = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let uid = unsafe { libc::getuid() };
    tmp.join(format!("shoal-views-{uid}"))
}

/// The canonical system roots as a PATH tail.
pub fn default_system_tail() -> Vec<PathBuf> {
    ["/usr/local/bin", "/usr/bin", "/bin"]
        .iter()
        .map(PathBuf::from)
        .collect()
}

/// A stable content hash of a binding set (order-independent).
pub fn bindings_hash(bindings: &[Binding]) -> String {
    let mut sorted: Vec<&Binding> = bindings.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name).then(a.path.cmp(&b.path)));
    let mut buf = Vec::new();
    for b in sorted {
        buf.extend_from_slice(b.name.as_bytes());
        buf.push(0);
        buf.extend_from_slice(b.path.as_os_str().as_encoded_bytes());
        buf.push(0);
    }
    hash_bytes(&buf)
}

/// Build (or reuse) the view dir for `bindings` and return the synthesized PATH.
///
/// Idempotent: repeated calls with the same bindings return the same `view_dir`
/// and do no work if it already exists. Concurrent-safe: the dir is assembled in
/// a uniquely-named sibling temp dir and atomically renamed into place; a losing
/// racer whose rename fails because the target exists simply reuses it.
pub fn synth_path(bindings: &[Binding], cfg: &ViewConfig) -> io::Result<SynthView> {
    let hash = bindings_hash(bindings);
    let view_dir = cfg.root.join(&hash).join("bin");

    if !view_dir.is_dir() {
        build_view(&cfg.root, &hash, bindings)?;
    }

    let mut parts: Vec<PathBuf> = vec![view_dir.clone()];
    if !cfg.hermetic {
        parts.extend(cfg.system_tail.iter().cloned());
    }
    let path_var = std::env::join_paths(parts)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;

    Ok(SynthView { view_dir, path_var })
}

fn build_view(root: &Path, hash: &str, bindings: &[Binding]) -> io::Result<()> {
    use std::os::unix::fs::symlink;

    std::fs::create_dir_all(root)?;
    let final_dir = root.join(hash);
    if final_dir.is_dir() {
        return Ok(()); // Another builder won the race.
    }

    // Assemble in a unique temp sibling, then atomically rename.
    let staging = root.join(format!(
        ".staging-{}-{}",
        std::process::id(),
        next_counter()
    ));
    let _ = std::fs::remove_dir_all(&staging);
    let staging_bin = staging.join("bin");
    std::fs::create_dir_all(&staging_bin)?;
    for b in bindings {
        let link = staging_bin.join(&b.name);
        // Tolerate a duplicate name within the same set (last wins).
        let _ = std::fs::remove_file(&link);
        symlink(&b.path, &link)?;
    }

    match std::fs::rename(&staging, &final_dir) {
        Ok(()) => Ok(()),
        Err(_) if final_dir.is_dir() => {
            // Lost the race; the winner's dir is equivalent (content-addressed).
            let _ = std::fs::remove_dir_all(&staging);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(e)
        }
    }
}

fn next_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_bin(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"#!/bin/sh\n").unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&p, perm).unwrap();
        p
    }

    fn cfg(root: &Path, hermetic: bool) -> ViewConfig {
        ViewConfig {
            root: root.to_path_buf(),
            system_tail: vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
            hermetic,
        }
    }

    #[test]
    fn hash_is_order_independent() {
        let a = Binding::new("node", "/x/node");
        let b = Binding::new("python", "/y/python");
        assert_eq!(
            bindings_hash(&[a.clone(), b.clone()]),
            bindings_hash(&[b, a])
        );
    }

    #[test]
    fn synth_creates_symlinks_and_path() {
        let src = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let node = fake_bin(src.path(), "node");
        let bindings = vec![Binding::new("node", node.clone())];
        let v = synth_path(&bindings, &cfg(root.path(), false)).unwrap();

        let link = v.view_dir.join("node");
        assert!(link.exists());
        assert_eq!(std::fs::read_link(&link).unwrap(), node);

        // PATH = view dir first, then system tail.
        let dirs: Vec<PathBuf> = std::env::split_paths(&v.path_var).collect();
        assert_eq!(dirs[0], v.view_dir);
        assert!(dirs.contains(&PathBuf::from("/usr/bin")));
    }

    #[test]
    fn hermetic_omits_system_tail() {
        let src = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let node = fake_bin(src.path(), "node");
        let v = synth_path(&[Binding::new("node", node)], &cfg(root.path(), true)).unwrap();
        let dirs: Vec<PathBuf> = std::env::split_paths(&v.path_var).collect();
        assert_eq!(dirs.len(), 1, "hermetic PATH is view-only");
        assert_eq!(dirs[0], v.view_dir);
    }

    #[test]
    fn synth_is_idempotent() {
        let src = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let node = fake_bin(src.path(), "node");
        let bindings = vec![Binding::new("node", node)];
        let v1 = synth_path(&bindings, &cfg(root.path(), false)).unwrap();
        let v2 = synth_path(&bindings, &cfg(root.path(), false)).unwrap();
        assert_eq!(v1.view_dir, v2.view_dir);
        assert!(v1.view_dir.is_dir());
    }

    #[test]
    fn different_bindings_different_view() {
        let src = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let node = fake_bin(src.path(), "node");
        let py = fake_bin(src.path(), "python");
        let v1 = synth_path(
            &[Binding::new("node", node.clone())],
            &cfg(root.path(), false),
        )
        .unwrap();
        let v2 = synth_path(
            &[Binding::new("node", node), Binding::new("python", py)],
            &cfg(root.path(), false),
        )
        .unwrap();
        assert_ne!(v1.view_dir, v2.view_dir);
    }
}
