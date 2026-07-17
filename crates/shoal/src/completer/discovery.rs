//! Bounded executable discovery and live filesystem candidate scans.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Directory mtimes do not change when an existing child is chmod'd, so PATH
/// entries are periodically revalidated even when metadata appears stable.
pub(super) const PATH_CACHE_REVALIDATE: Duration = Duration::from_millis(200);
/// Bound retained keys when a session repeatedly replaces its PATH.
pub(super) const MAX_PATH_CACHE_DIRS: usize = 64;

struct PathCacheEntry {
    dir_mtime: Option<SystemTime>,
    scanned_at: Instant,
    names: Vec<String>,
}

pub(super) struct PathDiscovery {
    session_dirs: Option<Arc<Mutex<Option<Vec<PathBuf>>>>>,
    cache: HashMap<PathBuf, PathCacheEntry>,
}

impl PathDiscovery {
    pub(super) fn new() -> Self {
        Self {
            session_dirs: None,
            cache: HashMap::new(),
        }
    }

    pub(super) fn set_session_dirs(&mut self, dirs: Arc<Mutex<Option<Vec<PathBuf>>>>) {
        self.session_dirs = Some(dirs);
    }

    pub(super) fn path_names(&mut self, cwd: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let configured = self
            .session_dirs
            .as_ref()
            .and_then(|dirs| dirs.lock().ok().and_then(|dirs| dirs.clone()));
        let dirs = if let Some(dirs) = configured {
            dirs
        } else {
            let Some(path_var) = std::env::var_os("PATH") else {
                return out;
            };
            std::env::split_paths(&path_var)
                .map(|dir| {
                    if dir.is_absolute() {
                        dir
                    } else {
                        cwd.join(dir)
                    }
                })
                .collect()
        };
        for dir in dirs {
            out.extend(self.path_dir_names(&dir));
        }
        out
    }

    pub(super) fn path_dir_names(&mut self, dir: &Path) -> Vec<String> {
        let mtime = fs::metadata(dir)
            .and_then(|metadata| metadata.modified())
            .ok();
        let stale = match self.cache.get(dir) {
            Some(cached) => {
                cached.dir_mtime != mtime || cached.scanned_at.elapsed() >= PATH_CACHE_REVALIDATE
            }
            None => true,
        };
        if stale {
            let mut names = Vec::new();
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten().take(4000) {
                    let executable = entry.metadata().is_ok_and(|metadata| {
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    });
                    if executable && let Some(name) = entry.file_name().to_str() {
                        names.push(name.to_string());
                    }
                }
            }
            if self.cache.len() >= MAX_PATH_CACHE_DIRS && !self.cache.contains_key(dir) {
                self.cache.clear();
            }
            self.cache.insert(
                dir.to_path_buf(),
                PathCacheEntry {
                    dir_mtime: mtime,
                    scanned_at: Instant::now(),
                    names,
                },
            );
        }
        self.cache
            .get(dir)
            .map_or_else(Vec::new, |cached| cached.names.clone())
    }

    #[cfg(test)]
    pub(super) fn cache_len(&self) -> usize {
        self.cache.len()
    }

    #[cfg(test)]
    pub(super) fn cache_contains(&self, dir: &Path) -> bool {
        self.cache.contains_key(dir)
    }
}

/// Scan one command-argument directory fresh. Filesystem completion is live;
/// only executable PATH scans retain bounded advisory state.
pub(super) fn filesystem_candidates(
    cwd: &Path,
    word: &str,
    mut matches: impl FnMut(&str, &str) -> bool,
) -> Vec<String> {
    let (dir_part, file_prefix) = split_dir_prefix(word);
    let base_dir = resolve_dir(cwd, &dir_part);
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(base_dir) else {
        return out;
    };
    let show_hidden = file_prefix.starts_with('.');
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if (!show_hidden && name.starts_with('.')) || !matches(&name, &file_prefix) {
            continue;
        }
        let mut value = format!("{dir_part}{name}");
        if entry.path().is_dir() {
            value.push('/');
        }
        out.push(value);
    }
    out
}

fn split_dir_prefix(word: &str) -> (String, String) {
    match word.rfind('/') {
        Some(index) => (word[..=index].to_string(), word[index + 1..].to_string()),
        None => (String::new(), word.to_string()),
    }
}

fn resolve_dir(cwd: &Path, dir_part: &str) -> PathBuf {
    if dir_part.is_empty() {
        return cwd.to_path_buf();
    }
    let expanded = if let Some(tail) = dir_part.strip_prefix("~/") {
        match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(tail),
            None => PathBuf::from(dir_part),
        }
    } else {
        PathBuf::from(dir_part)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

/// Enumerate adapter command names through the same bounded, validated loader
/// used by execution. Completion is advisory, so malformed files simply
/// contribute no candidates; startup reports the loader's warnings.
pub(super) fn adapter_names(dirs: &[PathBuf]) -> Vec<String> {
    let mut names = BTreeSet::new();
    for dir in dirs {
        let (catalog, _warnings) = shoal_adapters::AdapterCatalog::load_dir(dir);
        names.extend(catalog.names().map(str::to_owned));
    }
    names.into_iter().collect()
}
