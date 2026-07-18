//! Bounded executable discovery from the executing session's PATH.

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
/// Completion is advisory, but a single keystroke must not enumerate or
/// retain an attacker-sized directory before the configured result cap is
/// applied.
pub(super) const MAX_COMPLETION_SCAN_ENTRIES: usize = 4_096;
pub(super) const MAX_COMPLETION_RETAINED_BYTES: usize = 4 * 1024 * 1024;
const MAX_PATH_REQUEST_SCAN_ENTRIES: usize = 16_384;

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

    pub(super) fn path_names(
        &mut self,
        cwd: &Path,
        prefix: &str,
        max_results: usize,
        mut matches: impl FnMut(&str, &str) -> bool,
    ) -> Vec<String> {
        let mut out = BTreeSet::new();
        let mut retained_bytes = 0usize;
        let mut visited_names = 0usize;
        let configured = self
            .session_dirs
            .as_ref()
            .and_then(|dirs| dirs.lock().ok().and_then(|dirs| dirs.clone()));
        let dirs = if let Some(dirs) = configured {
            dirs
        } else {
            let Some(path_var) = std::env::var_os("PATH") else {
                return out.into_iter().collect();
            };
            std::env::split_paths(&path_var)
                .map(|dir| {
                    if dir.is_absolute() {
                        dir
                    } else {
                        cwd.join(dir)
                    }
                })
                .take(MAX_PATH_CACHE_DIRS)
                .collect()
        };
        for dir in dirs.into_iter().take(MAX_PATH_CACHE_DIRS) {
            for name in self.path_dir_names(&dir) {
                if visited_names >= MAX_PATH_REQUEST_SCAN_ENTRIES {
                    return out.into_iter().collect();
                }
                visited_names += 1;
                if !matches(&name, prefix) {
                    continue;
                }
                let Some(next) = retained_bytes.checked_add(name.len()) else {
                    return out.into_iter().collect();
                };
                if next > MAX_COMPLETION_RETAINED_BYTES {
                    return out.into_iter().collect();
                }
                if out.insert(name) {
                    retained_bytes = next;
                }
                if out.len() > max_results
                    && let Some(removed) = out.pop_last()
                {
                    retained_bytes = retained_bytes.saturating_sub(removed.len());
                }
            }
        }
        out.into_iter().collect()
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
            let mut retained_bytes = 0usize;
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.take(MAX_COMPLETION_SCAN_ENTRIES) {
                    let Ok(entry) = entry else { continue };
                    let executable = entry.metadata().is_ok_and(|metadata| {
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    });
                    if executable && let Some(name) = entry.file_name().to_str() {
                        let Some(next) = retained_bytes.checked_add(name.len()) else {
                            break;
                        };
                        if next > MAX_COMPLETION_RETAINED_BYTES {
                            break;
                        }
                        retained_bytes = next;
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
