//! PATH synthesis — PATH as an *output*, never an input (site/content/internals/reef-resolution.md).
//!
//! Given a resolved binding set, build (or reuse) a content-addressed view
//! directory of symlinks under `$XDG_RUNTIME_DIR/shoal/views/<hash>/bin`
//! (fallback `$TMPDIR/shoal-views-$UID`), and return the synthesized PATH: the
//! view dir followed by the system tail unless `hermetic`.
//!
//! Construction is idempotent and concurrent-safe: a fresh temp dir is built and
//! atomically renamed into place; an already-present view is reused as-is.

use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Component, Path, PathBuf};

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
    validate_bindings(bindings)?;
    ensure_private_root(&cfg.root)?;
    let hash = bindings_hash(bindings);
    let final_dir = cfg.root.join(&hash);
    let view_dir = cfg.root.join(&hash).join("bin");

    if !view_matches(&final_dir, bindings) {
        quarantine_invalid_view(&cfg.root, &final_dir)?;
        build_view(&cfg.root, &hash, bindings)?;
    }
    if !view_matches(&final_dir, bindings) {
        return Err(io::Error::other(
            "content-addressed Reef view failed post-publication validation",
        ));
    }

    let mut parts: Vec<PathBuf> = vec![view_dir.clone()];
    if !cfg.hermetic {
        parts.extend(cfg.system_tail.iter().cloned());
    }
    let path_var = std::env::join_paths(parts)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;

    Ok(SynthView { view_dir, path_var })
}

fn validate_bindings(bindings: &[Binding]) -> io::Result<()> {
    let mut names = HashSet::new();
    for binding in bindings {
        let path = Path::new(&binding.name);
        let mut components = path.components();
        let valid_name = matches!(components.next(), Some(Component::Normal(name)) if name == OsStr::new(&binding.name))
            && components.next().is_none();
        if binding.name.is_empty() || !valid_name {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid Reef view binding name {:?}", binding.name),
            ));
        }
        if !names.insert(binding.name.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("duplicate Reef view binding name {:?}", binding.name),
            ));
        }
        if !binding.path.is_absolute() || !crate::provider::is_executable(&binding.path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Reef view target for {:?} is not an absolute executable regular file",
                    binding.name
                ),
            ));
        }
    }
    Ok(())
}

fn ensure_private_root(root: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    std::fs::create_dir_all(root)?;
    let metadata = std::fs::symlink_metadata(root)?;
    if !metadata.file_type().is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Reef view root is not an owned real directory",
        ));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn expected_links(bindings: &[Binding]) -> BTreeMap<&str, &Path> {
    bindings
        .iter()
        .map(|binding| (binding.name.as_str(), binding.path.as_path()))
        .collect()
}

fn view_matches(final_dir: &Path, bindings: &[Binding]) -> bool {
    let Ok(final_metadata) = std::fs::symlink_metadata(final_dir) else {
        return false;
    };
    let bin = final_dir.join("bin");
    let Ok(bin_metadata) = std::fs::symlink_metadata(&bin) else {
        return false;
    };
    if !final_metadata.file_type().is_dir() || !bin_metadata.file_type().is_dir() {
        return false;
    }
    let expected = expected_links(bindings);
    let Ok(entries) = std::fs::read_dir(&bin) else {
        return false;
    };
    let mut seen = HashSet::new();
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return false;
        };
        let Some(target) = expected.get(name.as_str()) else {
            return false;
        };
        let Ok(file_type) = entry.file_type() else {
            return false;
        };
        if !file_type.is_symlink()
            || std::fs::read_link(entry.path()).ok().as_deref() != Some(*target)
        {
            return false;
        }
        seen.insert(name);
    }
    seen.len() == expected.len()
}

fn quarantine_invalid_view(root: &Path, final_dir: &Path) -> io::Result<()> {
    if std::fs::symlink_metadata(final_dir).is_err() {
        return Ok(());
    }
    let quarantine = root.join(format!(
        ".quarantine-{}-{}",
        std::process::id(),
        next_counter()
    ));
    match std::fs::rename(final_dir, &quarantine) {
        Ok(()) => remove_tree_or_link(&quarantine),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_tree_or_link(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            std::fs::remove_file(path)
        }
        Ok(_) => std::fs::remove_dir_all(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn build_view(root: &Path, hash: &str, bindings: &[Binding]) -> io::Result<()> {
    use std::os::unix::fs::symlink;

    let final_dir = root.join(hash);
    if view_matches(&final_dir, bindings) {
        return Ok(()); // Another equivalent builder won the race.
    }

    // Assemble in a unique temp sibling, then atomically rename.
    let staging = root.join(format!(
        ".staging-{}-{}",
        std::process::id(),
        next_counter()
    ));
    remove_tree_or_link(&staging)?;
    let staging_bin = staging.join("bin");
    std::fs::create_dir_all(&staging_bin)?;
    for b in bindings {
        let link = staging_bin.join(&b.name);
        symlink(&b.path, &link)?;
    }

    match std::fs::rename(&staging, &final_dir) {
        Ok(()) => Ok(()),
        Err(_) if view_matches(&final_dir, bindings) => {
            // Lost the race; reuse only an exactly equivalent winner.
            remove_tree_or_link(&staging)
        }
        Err(e) => {
            let _ = remove_tree_or_link(&staging);
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
    use std::os::unix::fs::symlink;

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
    fn tampered_reused_view_is_quarantined_and_rebuilt() {
        let source = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let intended = fake_bin(source.path(), "intended");
        let replacement = fake_bin(source.path(), "replacement");
        let bindings = vec![Binding::new("tool", intended.clone())];
        let first = synth_path(&bindings, &cfg(root.path(), false)).unwrap();
        let link = first.view_dir.join("tool");
        std::fs::remove_file(&link).unwrap();
        symlink(&replacement, &link).unwrap();
        std::fs::write(first.view_dir.join("extra"), b"not a symlink").unwrap();

        let repaired = synth_path(&bindings, &cfg(root.path(), false)).unwrap();
        assert_eq!(repaired.view_dir, first.view_dir);
        assert_eq!(
            std::fs::read_link(repaired.view_dir.join("tool")).unwrap(),
            intended
        );
        assert!(!repaired.view_dir.join("extra").exists());
        assert!(std::fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".quarantine-")
        }));
    }

    #[test]
    fn binding_names_cannot_escape_or_collide() {
        let source = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let binary = fake_bin(source.path(), "binary");
        for name in ["", ".", "..", "../escape", "nested/tool", "/absolute"] {
            let error = synth_path(
                &[Binding::new(name, binary.clone())],
                &cfg(root.path(), false),
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{name:?}");
        }
        assert!(
            synth_path(
                &[
                    Binding::new("same", binary.clone()),
                    Binding::new("same", binary),
                ],
                &cfg(root.path(), false),
            )
            .is_err()
        );
        assert!(!root.path().join("escape").exists());
    }

    #[test]
    fn view_root_must_be_an_owned_real_directory() {
        let source = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let real_root = parent.path().join("real");
        std::fs::create_dir(&real_root).unwrap();
        let linked_root = parent.path().join("linked");
        symlink(&real_root, &linked_root).unwrap();
        let binary = fake_bin(source.path(), "binary");
        let error =
            synth_path(&[Binding::new("tool", binary)], &cfg(&linked_root, false)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
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
