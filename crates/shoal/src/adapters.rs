//! Shared adapter-catalog loading for every execution path (`-c`, script
//! file, and the REPL).
//!
//! Before this module existed, `-c`/script-file runs (`main.rs::run_source`)
//! built their `Evaluator` with no adapters loaded at all — a fresh
//! `Evaluator` starts with `AdapterCatalog::empty()` (`shoal-eval`'s own
//! default) and nothing ever called `set_adapters` on it, so `shoal -c "git
//! status"` ran raw system `git` (adapter scope `system`, no structured
//! table) instead of engaging the bundled `git` adapter. Only the REPL
//! (`repl.rs`) called `set_adapters` at all, and even it only loaded
//! `config.adapters.dirs` (user-declared extra adapter directories) — never
//! the bundled `adapters/` pack shipped with shoal itself, so a bare `shoal`
//! with no config file loaded no adapters either. This factors the ONE
//! loading sequence (bundled pack + configured extra dirs) that every path
//! now shares.
//!
//! Eval-side adapter dispatch itself (`shoal-eval`'s `AdapterCatalog`/
//! `set_adapters` machinery) is untouched — this only decides WHAT gets
//! loaded and WHEN, the same way `repl.rs` already did for config dirs.

use std::path::{Path, PathBuf};

use shoal_adapters::AdapterCatalog;
use shoal_eval::Evaluator;

/// The `adapters/` pack shipped alongside the repo/install tree, resolved at
/// compile time relative to this crate's own manifest dir
/// (`crates/shoal/../../adapters` == `<repo>/adapters`) — the same relative
/// depth `shoal_eval::Evaluator::load_bundled_adapters` already uses from
/// `crates/shoal-eval`, so every path through the binary agrees on one
/// on-disk bundled pack.
pub(crate) fn bundled_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../adapters")
}

/// Load the bundled pack, then each `config.adapters.dirs` entry in order,
/// engaging every one on `evaluator` via `set_adapters`. Returns the loaded
/// catalogs (bundled first) for the REPL completer's flag/subcommand lookup
/// (`ShoalCompleter` needs every catalog, not just the last one still active
/// on the evaluator), plus any per-file load warnings — a malformed adapter
/// file/command is non-fatal, siblings still load, per
/// `AdapterCatalog::load_dir`.
///
/// `adapters.dirs` is empty by default (no config file present), so in the
/// common case this loads just the bundled pack. NOTE: `set_adapters`
/// replaces the evaluator's active catalog wholesale (it has no merge), so
/// with more than one directory configured the LAST directory's commands are
/// what the evaluator itself dispatches through — mirrors the "later file
/// wins" rule `AdapterCatalog::load_dir` already applies to files within a
/// single directory, now extended across directories. Pre-existing
/// behavior carried over from `repl.rs`, not something this fix changes.
pub(crate) fn load_adapters(
    evaluator: &mut Evaluator,
    config_dirs: &[PathBuf],
) -> (Vec<AdapterCatalog>, Vec<String>) {
    let mut catalogs = Vec::new();
    let mut warnings = Vec::new();
    for dir in std::iter::once(bundled_dir()).chain(config_dirs.iter().cloned()) {
        let (catalog, dir_warnings) = AdapterCatalog::load_dir(&dir);
        warnings.extend(dir_warnings);
        evaluator.set_adapters(catalog.clone());
        catalogs.push(catalog);
    }
    (catalogs, warnings)
}

/// Directories to scan for adapter *names* (REPL tab-completion only — real
/// flag/subcommand data still goes through the catalogs `load_adapters`
/// returns): the bundled pack plus every `config.adapters.dirs` entry, same
/// order as `load_adapters`.
pub(crate) fn name_scan_dirs(config_dirs: &[PathBuf]) -> Vec<PathBuf> {
    std::iter::once(bundled_dir())
        .chain(config_dirs.iter().cloned())
        .collect()
}

/// Print each adapter-load warning the same yellow `warning:` style used
/// elsewhere at this crate's terminal-output boundaries (NO_COLOR-aware via
/// `maybe_strip`).
pub(crate) fn print_warnings(warnings: &[String]) {
    for warning in warnings {
        eprintln!(
            "{}",
            crate::maybe_strip(format!(
                "\x1b[33;1mwarning:\x1b[0m failed to load adapter: {warning}"
            ))
        );
    }
}
