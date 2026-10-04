//! Trust store for project-layer `.shoal.toml` files.
//!
//! A project config can run init files, rewrite `PATH`, add aliases, load
//! plugins and set a pager, and it is discovered merely by `cd`-ing into a
//! directory. It is therefore ignored until the user explicitly trusts that
//! exact file with `shoal trust` (direnv/mise model): trust is keyed by the
//! file's canonical path and bound to the blake3 hash of its contents, so any
//! edit revokes it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Whether a project config may be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustState {
    /// Path and contents match a recorded trust entry.
    Trusted,
    /// Never trusted.
    Untrusted,
    /// Trusted before, but the contents changed since.
    Changed,
}

/// `$SHOAL_TRUST_DIR`, else `$XDG_DATA_HOME/shoal/trust`, else
/// `~/.local/share/shoal/trust`. `None` when no home can be resolved (nothing
/// is then ever trusted).
pub fn trust_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SHOAL_TRUST_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".local/share"))
        })?;
    Some(data.join("shoal/trust"))
}

fn entry_path(dir: &Path, canonical: &Path) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    dir.join(
        blake3::hash(canonical.as_os_str().as_bytes())
            .to_hex()
            .as_str(),
    )
}

fn content_hash(path: &Path) -> io::Result<String> {
    Ok(blake3::hash(&fs::read(path)?).to_hex().to_string())
}

/// Trust state of `config` against the store in `dir`.
pub fn trust_state_in(dir: &Path, config: &Path) -> TrustState {
    let Ok(canonical) = fs::canonicalize(config) else {
        return TrustState::Untrusted;
    };
    let Ok(recorded) = fs::read_to_string(entry_path(dir, &canonical)) else {
        return TrustState::Untrusted;
    };
    match content_hash(&canonical) {
        Ok(hash) if hash == recorded.trim() => TrustState::Trusted,
        _ => TrustState::Changed,
    }
}

/// Trust state of `config` against the default store.
pub fn trust_state(config: &Path) -> TrustState {
    match trust_dir() {
        Some(dir) => trust_state_in(&dir, config),
        None => TrustState::Untrusted,
    }
}

/// Record trust for the current contents of `config`; returns its canonical path.
pub fn trust_in(dir: &Path, config: &Path) -> io::Result<PathBuf> {
    let canonical = fs::canonicalize(config)?;
    let hash = content_hash(&canonical)?;
    fs::create_dir_all(dir)?;
    fs::write(entry_path(dir, &canonical), format!("{hash}\n"))?;
    Ok(canonical)
}

/// Remove trust for `config`; returns whether an entry existed.
pub fn untrust_in(dir: &Path, config: &Path) -> io::Result<bool> {
    let canonical = fs::canonicalize(config)?;
    match fs::remove_file(entry_path(dir, &canonical)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// The one-line notice shown when an untrusted project config is skipped.
pub fn untrusted_notice(config: &Path, state: TrustState) -> String {
    let why = if state == TrustState::Changed {
        "changed since it was trusted"
    } else {
        "not trusted"
    };
    format!(
        "ignoring project config {} ({why}); review it, then run `shoal trust` to enable",
        config.display()
    )
}
