//! Typed undo inverses: recording them (`record_undo*`) and replaying them
//! (`undo_entry`/`apply_inverse`) with TOCTOU-safe scope checks.

use std::fs;
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::Journal;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub size: u64,
    pub modified_ns: Option<u64>,
    pub hash: Option<String>,
}

impl FileFingerprint {
    pub fn capture(path: &Path) -> io::Result<Self> {
        let meta = fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to fingerprint symlink",
            ));
        }
        let hash = if meta.is_file() {
            Some(blake3::hash(&fs::read(path)?).to_hex().to_string())
        } else {
            None
        };
        let modified_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos().min(u64::MAX as u128) as u64);
        Ok(Self {
            size: meta.len(),
            modified_ns,
            hash,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UndoInverse {
    TrashMove {
        original: PathBuf,
        trash: PathBuf,
        trash_fingerprint: FileFingerprint,
    },
    RestoreBytes {
        path: PathBuf,
        prior_hash: String,
        expected_current: FileFingerprint,
    },
    MoveBack {
        from: PathBuf,
        to: PathBuf,
        expected_from: FileFingerprint,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoStatus {
    Applied,
    AlreadyApplied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoStep {
    pub inverse: UndoInverse,
    pub status: UndoStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoReport {
    pub entry_id: i64,
    pub steps: Vec<UndoStep>,
}

#[derive(Debug)]
pub enum UndoError {
    Sql(rusqlite::Error),
    Io(io::Error),
    Invalid(String),
    Escaped(PathBuf),
    Stale(PathBuf),
}
impl From<rusqlite::Error> for UndoError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e)
    }
}
impl From<io::Error> for UndoError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl std::fmt::Display for UndoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sql(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
            Self::Invalid(e) => write!(f, "invalid undo inverse: {e}"),
            Self::Escaped(p) => write!(f, "undo target escapes scope: {}", p.display()),
            Self::Stale(p) => write!(
                f,
                "undo target was modified since recording: {}",
                p.display()
            ),
        }
    }
}
impl std::error::Error for UndoError {}

impl Journal {
    /// Record an undo inverse for entry `id`. `op` names the inverse operation
    /// (`"trash"`, `"restore_bytes"`, …); `inverse_json` is its JSON payload.
    pub fn record_undo(&self, id: i64, op: &str, inverse_json: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO undo (entry_id, op, inverse) VALUES (?1, ?2, ?3)",
            params![id, op, inverse_json],
        )?;
        Ok(())
    }

    pub fn record_undo_inverse(&self, id: i64, inverse: &UndoInverse) -> rusqlite::Result<()> {
        let json = serde_json::to_string(inverse)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        self.record_undo(id, inverse_name(inverse), &json)
    }

    /// Replay typed inverses newest-first. Destinations must remain inside
    /// `root`; stale fingerprints and symlink traversal are hard failures.
    pub fn undo_entry(&self, id: i64, root: &Path) -> Result<UndoReport, UndoError> {
        let root = resolve_leading_symlink_prefix(root)?;
        let mut stmt = self
            .conn
            .prepare("SELECT inverse FROM undo WHERE entry_id=?1 ORDER BY rowid DESC")?;
        let encoded = stmt
            .query_map([id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut steps = Vec::new();
        for json in encoded {
            let inverse: UndoInverse =
                serde_json::from_str(&json).map_err(|e| UndoError::Invalid(e.to_string()))?;
            let status = self.apply_inverse(&inverse, &root)?;
            steps.push(UndoStep { inverse, status });
        }
        Ok(UndoReport {
            entry_id: id,
            steps,
        })
    }

    fn apply_inverse(&self, inverse: &UndoInverse, root: &Path) -> Result<UndoStatus, UndoError> {
        match inverse {
            UndoInverse::TrashMove {
                original,
                trash,
                trash_fingerprint,
            } => {
                checked_target(root, original)?;
                if !trash.exists() {
                    return if original.exists() {
                        Ok(UndoStatus::AlreadyApplied)
                    } else {
                        Err(UndoError::Stale(trash.clone()))
                    };
                }
                require_fingerprint(trash, trash_fingerprint)?;
                if original.exists() {
                    return Err(UndoError::Stale(original.clone()));
                }
                ensure_no_symlink_parents(root, original)?;
                fs::rename(trash, original)?;
                Ok(UndoStatus::Applied)
            }
            UndoInverse::RestoreBytes {
                path,
                prior_hash,
                expected_current,
            } => {
                checked_target(root, path)?;
                let prior = self
                    .read_blob(prior_hash)?
                    .ok_or_else(|| UndoError::Invalid(format!("missing CAS blob {prior_hash}")))?;
                if path.exists() {
                    let current = FileFingerprint::capture(path)?;
                    if current.hash.as_deref() == Some(blake3::hash(&prior).to_hex().as_str()) {
                        return Ok(UndoStatus::AlreadyApplied);
                    }
                    if &current != expected_current {
                        return Err(UndoError::Stale(path.clone()));
                    }
                } else {
                    return Err(UndoError::Stale(path.clone()));
                }
                ensure_no_symlink_parents(root, path)?;
                atomic_replace(path, &prior)?;
                Ok(UndoStatus::Applied)
            }
            UndoInverse::MoveBack {
                from,
                to,
                expected_from,
            } => {
                checked_target(root, from)?;
                checked_target(root, to)?;
                if !from.exists() {
                    return if to.exists() {
                        Ok(UndoStatus::AlreadyApplied)
                    } else {
                        Err(UndoError::Stale(from.clone()))
                    };
                }
                require_fingerprint(from, expected_from)?;
                if to.exists() {
                    return Err(UndoError::Stale(to.clone()));
                }
                ensure_no_symlink_parents(root, to)?;
                fs::rename(from, to)?;
                Ok(UndoStatus::Applied)
            }
        }
    }

    /// List `(op, inverse_json)` undo records for entry `id`, in recording order.
    /// (`undo out[n]` replays these newest-first — callers reverse.)
    pub fn undos_for(&self, id: i64) -> rusqlite::Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT op, inverse FROM undo WHERE entry_id = ?1 ORDER BY rowid")?;
        let rows = stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }
}

fn inverse_name(inverse: &UndoInverse) -> &'static str {
    match inverse {
        UndoInverse::TrashMove { .. } => "trash_move",
        UndoInverse::RestoreBytes { .. } => "restore_bytes",
        UndoInverse::MoveBack { .. } => "move_back",
    }
}

fn checked_target(root: &Path, path: &Path) -> Result<(), UndoError> {
    if !path.is_absolute() {
        return Err(UndoError::Escaped(path.to_owned()));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            c => normalized.push(c.as_os_str()),
        }
    }
    strip_root(root, &normalized)?;
    Ok(())
}

fn ensure_no_symlink_parents(root: &Path, path: &Path) -> Result<(), UndoError> {
    let parent = path
        .parent()
        .ok_or_else(|| UndoError::Escaped(path.to_owned()))?;
    let relative = strip_root(root, parent)?;
    let mut current = root.to_owned();
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => return Err(UndoError::Escaped(current)),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// `path`'s components below `root`.
///
/// Tries a plain [`Path::strip_prefix`] first. If that fails, `root` and
/// `path` may disagree only because one of them still carries a raw
/// OS-level symlink alias in its leading prefix (e.g. macOS's `/tmp` ->
/// `/private/tmp`) that the other has already had resolved — see
/// [`resolve_leading_symlink_prefix`] and the callers in this module that
/// canonicalize `root` up front. Re-resolve `path`'s own leading prefix the
/// same (deliberately partial) way and retry once. This never touches
/// anything past that leading run, so an intra-scope symlink swap — the
/// TOCTOU case this whole scope check exists for — still can't slip through
/// either operand.
fn strip_root(root: &Path, path: &Path) -> Result<PathBuf, UndoError> {
    if let Ok(rel) = path.strip_prefix(root) {
        return Ok(rel.to_owned());
    }
    let realigned = resolve_leading_symlink_prefix(path)?;
    realigned
        .strip_prefix(root)
        .map(PathBuf::from)
        .map_err(|_| UndoError::Escaped(path.to_owned()))
}

/// Resolve only the *leading* run of symlink components in `path` (e.g.
/// macOS's `/tmp` -> `/private/tmp`, `/var` -> `/private/var`), stopping at
/// the first component that is not itself a symlink (or doesn't exist).
///
/// This is deliberately short of a full `canonicalize`: resolving the whole
/// path would also silently follow a symlink planted *inside* the tracked
/// scope, which is exactly the TOCTOU swap `ensure_no_symlink_parents`
/// exists to catch. Restricting resolution to the unbroken run of symlinks
/// right at the front only ever reaches genuine OS-level directory aliases
/// (real filesystem hierarchies put those first, before any directory a
/// user or a session could have created) and leaves everything below —
/// scope and its descendants — exactly as given.
fn resolve_leading_symlink_prefix(path: &Path) -> io::Result<PathBuf> {
    let mut resolved = PathBuf::new();
    let mut components = path.components();
    for component in components.by_ref() {
        let is_anchor = matches!(
            component,
            std::path::Component::RootDir | std::path::Component::Prefix(_)
        );
        resolved.push(component);
        if is_anchor {
            continue;
        }
        match fs::symlink_metadata(&resolved) {
            Ok(meta) if meta.file_type().is_symlink() => {
                resolved = resolved.canonicalize()?;
            }
            _ => break,
        }
    }
    resolved.extend(components);
    Ok(resolved)
}

fn require_fingerprint(path: &Path, expected: &FileFingerprint) -> Result<(), UndoError> {
    if &FileFingerprint::capture(path)? == expected {
        Ok(())
    } else {
        Err(UndoError::Stale(path.to_owned()))
    }
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
