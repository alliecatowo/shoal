//! Live command-argument filesystem completion under per-request admission.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_SCAN_ENTRIES: usize = 4_096;
const MAX_RETAINED_BYTES: usize = 4 * 1024 * 1024;

pub(super) fn filesystem_candidates(
    cwd: &Path,
    word: &str,
    max_results: usize,
    mut matches: impl FnMut(&str, &str) -> bool,
) -> Vec<String> {
    let (dir_part, file_prefix) = split_dir_prefix(word);
    let base_dir = resolve_dir(cwd, &dir_part);
    let mut out = BTreeSet::new();
    let mut retained_bytes = 0usize;
    let Ok(entries) = fs::read_dir(base_dir) else {
        return Vec::new();
    };
    let show_hidden = file_prefix.starts_with('.');
    for entry in entries.take(MAX_SCAN_ENTRIES) {
        let Ok(entry) = entry else { continue };
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
        let Some(next) = retained_bytes.checked_add(value.len()) else {
            break;
        };
        if next > MAX_RETAINED_BYTES {
            break;
        }
        if out.insert(value) {
            retained_bytes = next;
        }
        if out.len() > max_results
            && let Some(removed) = out.pop_last()
        {
            retained_bytes = retained_bytes.saturating_sub(removed.len());
        }
    }
    out.into_iter().collect()
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
