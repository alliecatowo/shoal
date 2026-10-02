//! Filesystem, name, and network grant matching.

use super::*;

pub(super) fn has_glob_meta(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c, '*' | '?' | '[' | ']' | '{' | '}'))
}

/// The longest concrete (glob-free) leading path of a policy grant, expanding a
/// leading `~/`. `/work/**` → `/work`; `/**` → `/`; `/etc/hosts` → `/etc/hosts`.
/// `None` when the grant has no concrete anchor (e.g. `**/foo`).
///
/// `pub(crate)` rather than private: exercised directly by the crate's own
/// unit tests in `lib.rs`.
pub(crate) fn grant_root(grant: &str) -> Option<PathBuf> {
    let expanded = if let Some(rest) = grant.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(rest)
    } else {
        PathBuf::from(grant)
    };
    let mut root = PathBuf::new();
    for comp in expanded.components() {
        match comp {
            Component::RootDir | Component::Prefix(_) => root.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                root.pop();
            }
            Component::Normal(seg) => {
                if has_glob_meta(&seg.to_string_lossy()) {
                    break;
                }
                root.push(seg);
            }
        }
    }
    (!root.as_os_str().is_empty()).then_some(root)
}

/// Does any grant in `grants` reduce to the filesystem root `/`?
pub(super) fn grants_include_root(grants: &[String]) -> bool {
    grants
        .iter()
        .any(|g| grant_root(g).as_deref() == Some(Path::new("/")))
}

/// Concrete, existing, canonical subtree roots for a set of grants (sorted,
/// de-duped). Canonicalizing here is load-bearing: Landlock opens the supplied
/// path and therefore grants the object reached through a symbolic link, while
/// semantic policy matching must describe that same object rather than the
/// link's lexical spelling.
///
/// Non-existent roots are dropped: Landlock/Seatbelt open each path, so a grant
/// for a path that is not there yet must not fail the whole spawn — it simply
/// grants nothing, which is the fail-closed direction.
pub(super) fn grant_roots(grants: &[String]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = grants
        .iter()
        .filter_map(|g| grant_root(g))
        .filter_map(|p| fs::canonicalize(p).ok())
        .collect();
    out.sort();
    out.dedup();
    out
}

pub(super) fn bool_verdict(ok: bool) -> Verdict {
    if ok { Verdict::Allow } else { Verdict::Deny }
}
pub(super) fn names_verdict(names: &[String], grants: &[String]) -> Verdict {
    bool_verdict(
        names
            .iter()
            .all(|n| grants.iter().any(|g| g == "*" || g == n)),
    )
}
pub(super) fn paths_verdict(paths: &[PathBuf], grants: &[String]) -> Verdict {
    bool_verdict(
        paths
            .iter()
            .all(|p| grants.iter().any(|g| path_grant(g, p))),
    )
}

pub(super) fn path_grant(grant: &str, path: &Path) -> bool {
    let expanded = if let Some(rest) = grant.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(rest)
            .to_string_lossy()
            .into_owned()
    } else {
        grant.to_owned()
    };
    let Some(pattern) = canonical_grant_pattern(&expanded) else {
        return false;
    };
    let Some(resolved) = canonicalize_existing_prefix(path) else {
        return false;
    };
    Pattern::new(&pattern).is_ok_and(|p| p.matches_path(&resolved))
}

/// Canonicalize the concrete prefix of a glob grant and append its glob-bearing
/// suffix unchanged. `/work/link/**`, where `link -> /srv/data`, therefore
/// means `/srv/data/**` both to semantic matching and to the OS sandbox.
pub(super) fn canonical_grant_pattern(grant: &str) -> Option<String> {
    let root = grant_root(grant)?;
    let canonical = canonicalize_existing_prefix(&root)?;
    let expanded = Path::new(grant);
    let mut suffix = PathBuf::new();
    let mut in_suffix = false;
    for component in expanded.components() {
        if !in_suffix
            && matches!(component, Component::Normal(segment) if has_glob_meta(&segment.to_string_lossy()))
        {
            in_suffix = true;
        }
        if in_suffix {
            suffix.push(component.as_os_str());
        }
    }
    let pattern = if suffix.as_os_str().is_empty() {
        canonical
    } else {
        canonical.join(suffix)
    };
    pattern.to_str().map(str::to_owned)
}

/// Resolve every existing component of an effect path while retaining a
/// not-yet-created suffix. This prevents an allowed lexical subtree from
/// escaping through an existing symlink, without making ordinary create/write
/// effects fail merely because their final path does not exist yet.
pub(super) fn canonicalize_existing_prefix(path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        normalize(path)
    } else {
        normalize(&std::env::current_dir().ok()?.join(path))
    };
    let mut prefix = absolute.as_path();
    let mut suffix = Vec::new();
    loop {
        match fs::canonicalize(prefix) {
            Ok(canonical) => {
                return Some(
                    suffix
                        .iter()
                        .rev()
                        .fold(canonical, |path, component| path.join(component)),
                );
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = prefix.file_name()?.to_os_string();
                suffix.push(name);
                prefix = prefix.parent()?;
            }
            Err(_) => return None,
        }
    }
}

pub(super) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            x => out.push(x.as_os_str()),
        }
    }
    out
}

pub(super) fn host_grant(grant: &str, host: &str, port: u16) -> bool {
    let Some((host_pat, port_pat)) = grant.rsplit_once(':') else {
        return false;
    };
    if port_pat != "*" && port_pat.parse::<u16>().ok() != Some(port) {
        return false;
    }
    Pattern::new(host_pat).is_ok_and(|p| p.matches(host))
}
