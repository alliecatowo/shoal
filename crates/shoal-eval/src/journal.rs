//! Journal integration for the tree-walk evaluator (site/content/internals/language-conformance-contract.md, site/content/internals/system-map.md).
//!
//! The journal is what *actually happened*: every executed top-level statement
//! becomes an entry (src, canonical AST, derived effects, cwd, principal, ts),
//! its outputs are captured to the content-addressed store, and reversible fs
//! mutations record a typed undo inverse so an honest `undo` can replay them.
//!
//! # Zero regression
//!
//! Everything here is gated on an installed [`Journal`]. The default evaluator
//! carries `journal: None`, so `-c`, scripts, and the conformance corpus record
//! nothing and behave exactly as before. Only an interactive/kernel session that
//! calls [`Evaluator::set_journal`] pays any cost.
//!
//! # Secrets
//!
//! Nothing secret reaches the journal: `Value::Secret` is un-constructible in
//! argv at the type level (`argv_value` rejects it), so a recorded `src`/AST/
//! effect set names references only — never secret material.

use super::*;
use serde::Serialize;
use shoal_journal::{
    EntryRecord, FileFingerprint, Journal, JournalQuery, UndoError, UndoInverse, UndoIo,
    UndoReport, UndoStatus,
};
use std::os::unix::ffi::OsStrExt;
use std::time::Instant;

const MAX_JOURNAL_IDENTITY_BYTES: usize = 4 * 1024;
const MAX_JOURNAL_PROGRAM_SOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_JOURNAL_SOURCE_BYTES: usize = 256 * 1024;
const MAX_JOURNAL_AST_BYTES: usize = 1024 * 1024;
const MAX_JOURNAL_EFFECT_BYTES: usize = 256 * 1024;
const MAX_JOURNAL_UNDO_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
const MAX_JOURNAL_ERROR_BYTES: usize = 1024;
const TRUNCATED_TEXT: &str = "\n[shoal: journal field truncated]\n";

pub(crate) struct OpenJournalEntry {
    id: i64,
    started: Instant,
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl BoundedJsonWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(8192)),
            limit,
            exceeded: false,
        }
    }
}

impl std::io::Write for BoundedJsonWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let remaining = self.limit.saturating_sub(self.bytes.len());
        if bytes.len() > remaining {
            self.bytes.extend_from_slice(&bytes[..remaining]);
            self.exceeded = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "journal JSON field exceeds its byte limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A prior-state snapshot captured before an overwriting/moving fs mutation, to
/// be turned into a typed [`UndoInverse`] once the mutation has run.
pub(crate) enum FsUndoPre {
    /// An existing file the op is about to clobber; its prior bytes are already
    /// in the CAS under `prior_hash`.
    Overwrite { path: PathBuf, prior_hash: String },
    /// A move whose destination did not previously exist — the inverse is to
    /// move it back to `src`.
    Moved { src: PathBuf, dest: PathBuf },
}

fn elapsed_ns(start: Instant) -> i64 {
    start.elapsed().as_nanos().min(i64::MAX as u128) as i64
}

fn bounded_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut keep = limit.saturating_sub(TRUNCATED_TEXT.len()).min(text.len());
    while keep > 0 && !text.is_char_boundary(keep) {
        keep -= 1;
    }
    let mut bounded = String::with_capacity(limit);
    bounded.push_str(&text[..keep]);
    bounded.push_str(TRUNCATED_TEXT);
    bounded
}

fn bounded_json<T: Serialize + ?Sized>(
    value: &T,
    limit: usize,
    label: &str,
) -> Result<String, String> {
    let mut writer = BoundedJsonWriter::new(limit);
    match serde_json::to_writer(&mut writer, value) {
        Ok(()) => String::from_utf8(writer.bytes)
            .map_err(|_| format!("serialized {label} was not valid UTF-8")),
        Err(_) if writer.exceeded => Err(format!("{label} exceeds journal byte limit")),
        Err(error) => Err(format!("could not serialize {label}: {error}")),
    }
}

fn omitted_json(reason: String) -> String {
    serde_json::json!({ "shoal_omitted": bounded_text(&reason, MAX_JOURNAL_ERROR_BYTES) })
        .to_string()
}

fn bounded_error_detail(error: impl std::fmt::Display) -> String {
    bounded_text(&error.to_string(), MAX_JOURNAL_ERROR_BYTES)
}

fn note_failure(
    failure: &mut Option<(&'static str, String)>,
    stage: &'static str,
    error: impl std::fmt::Display,
) {
    if failure.is_none() {
        *failure = Some((stage, bounded_error_detail(error)));
    }
}

fn journal_begin_error(error: impl std::fmt::Display) -> ErrorVal {
    ErrorVal::new(
        "journal_begin_failed",
        format!(
            "journal begin row could not be persisted before statement execution: {}",
            bounded_error_detail(error)
        ),
    )
    .with_hint("no statement effects were executed; restore journal storage and retry")
}

fn finish_result(result: VResult<Flow>, failure: Option<(&'static str, String)>) -> VResult<Flow> {
    let Some((stage, detail)) = failure else {
        return result;
    };
    let primary = result.err();
    let primary_detail = primary
        .as_ref()
        .map(|error| {
            format!(
                "; primary error was {}: {}",
                error.code,
                bounded_text(&error.msg, MAX_JOURNAL_ERROR_BYTES)
            )
        })
        .unwrap_or_default();
    let mut audit = ErrorVal::new(
        "journal_commit_indeterminate",
        format!(
            "journal persistence failed at {stage} after statement execution: {detail}; effects may already have occurred{primary_detail}"
        ),
    )
    .with_hint(
        "do not blindly retry; inspect external state and repair journal storage before continuing",
    );
    if let Some(primary) = primary {
        audit.span = primary.span;
        audit.stderr = primary.stderr;
        audit.status = primary.status;
    }
    Err(audit)
}

/// Journal undo's narrow filesystem view, backed by the evaluator's injected
/// filesystem port. This keeps the journal crate independent of shoal-value
/// while ensuring recording and replay use the same mediated filesystem as the
/// mutation being journaled.
struct EvalUndoIo<'a>(&'a dyn Fs);

impl UndoIo for EvalUndoIo<'_> {
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.0.read(path)
    }

    fn symlink_metadata(&self, path: &Path) -> std::io::Result<std::fs::Metadata> {
        self.0.symlink_metadata(path)
    }

    fn exists(&self, path: &Path) -> bool {
        self.0.exists(path)
    }

    fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
        self.0.canonicalize(path)
    }

    fn create_dir(&self, path: &Path) -> std::io::Result<()> {
        self.0.create_dir(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.0.rename(from, to)
    }

    fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.0.atomic_replace(path, bytes)
    }
}

/// The default per-user state dir the journal lives in, mirroring the kernel's
/// `state_dir()` exactly so the REPL and kernel agree on one journal on disk.
/// Also the home of the `j`/`jump` frecency store (`frecency.rs`), so both
/// per-user stores live side by side.
pub(crate) fn default_state_dir() -> PathBuf {
    shoal_paths::ShoalPaths::discover()
        .state_dir()
        .to_path_buf()
}

mod builtins;
mod runtime;

/// Extract literal text from a command argument (word/path/literal string/int),
/// or `None` for a dynamic one — used to read simple `journal` filter flags
/// without evaluating side-effecting arguments.
fn literal_cmdarg_text(arg: &CmdArg) -> Option<String> {
    match arg {
        CmdArg::Word { text, .. } | CmdArg::Path { text, .. } => Some(text.clone()),
        CmdArg::Str { expr, .. } | CmdArg::Expr { expr, .. } => match expr {
            Expr::Str { value, .. } => Some(value.clone()),
            Expr::Int { value, .. } => Some(value.to_string()),
            _ => None,
        },
        _ => None,
    }
}

/// Walk an `rm` result (`[{path, trash}, …]`) and record a trash-move inverse
/// for each trashed file so `undo` can move it back.
fn record_trash_inverses(
    journal: &Journal,
    io: &dyn UndoIo,
    entry: i64,
    result: &Value,
) -> Result<(), String> {
    let Value::List(rows) = result else {
        return Ok(());
    };
    for row in rows {
        let Value::Record(r) = row else { continue };
        let (Some(Value::Path(original)), Some(Value::Path(trash))) =
            (r.get("path"), r.get("trash"))
        else {
            continue;
        };
        let fp = FileFingerprint::capture_with(io, trash).map_err(|error| error.to_string())?;
        journal
            .record_undo_inverse(
                entry,
                &UndoInverse::TrashMove {
                    original: original.clone(),
                    trash: trash.clone(),
                    trash_fingerprint: fp,
                },
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// The newest journal entry that has at least one recorded undo inverse.
fn last_reversible_entry(journal: &Journal) -> VResult<Option<i64>> {
    let rows = journal
        .query(&JournalQuery {
            limit: 500,
            ..Default::default()
        })
        .map_err(|error| {
            ErrorVal::new(
                "journal_read_failed",
                format!("could not inspect journal entries for undo: {error}"),
            )
        })?;
    for row in rows {
        let undos = journal.undos_for(row.id).map_err(|error| {
            ErrorVal::new(
                "journal_read_failed",
                format!(
                    "could not inspect undo metadata for entry {}: {error}",
                    row.id
                ),
            )
        })?;
        if !undos.is_empty() {
            return Ok(Some(row.id));
        }
    }
    Ok(None)
}

/// Build the reported value for a completed undo: the entry, the count, and a
/// human-readable action per replayed inverse.
fn undo_report_value(report: &UndoReport) -> Value {
    let actions = report
        .steps
        .iter()
        .map(|step| {
            let verb = match step.status {
                UndoStatus::Applied => "undid",
                UndoStatus::AlreadyApplied => "already-undone",
            };
            let what = match &step.inverse {
                UndoInverse::TrashMove { original, .. } => {
                    format!("restored {}", original.display())
                }
                UndoInverse::RestoreBytes { path, .. } => {
                    format!("restored prior contents of {}", path.display())
                }
                UndoInverse::MoveBack { to, .. } => format!("moved back to {}", to.display()),
            };
            Value::Str(format!("{verb}: {what}"))
        })
        .collect::<Vec<_>>();
    let mut r = Record::new();
    r.insert("entry".into(), Value::Int(report.entry_id));
    r.insert("undone".into(), Value::Int(report.steps.len() as i64));
    r.insert("actions".into(), Value::List(actions));
    Value::Record(r)
}

/// One journal `EntryRow` as a table record for the `journal`/`history` view.
fn entry_row_record(e: &shoal_journal::EntryRow) -> Record {
    let mut r = Record::new();
    r.insert("id".into(), Value::Int(e.id));
    r.insert("kind".into(), Value::Str(e.kind.as_str().into()));
    r.insert(
        "parent".into(),
        e.parent_id.map(Value::Int).unwrap_or(Value::Null),
    );
    let ts = jiff::Timestamp::from_nanosecond(e.ts_ns as i128)
        .ok()
        .map(|t| Value::DateTime(Box::new(t.to_zoned(jiff::tz::TimeZone::system()))))
        .unwrap_or(Value::Null);
    r.insert("ts".into(), ts);
    r.insert("principal".into(), Value::Str(e.principal.clone()));
    // The full recorded source, not just the head word: a `src` column showing
    // only `git` for `git push origin main` is as good as empty for a history
    // view. `--head` filtering still matches on the head in the journal query.
    r.insert("src".into(), Value::Str(e.src.clone()));
    r.insert("ok".into(), e.ok.map(Value::Bool).unwrap_or(Value::Null));
    r.insert(
        "status".into(),
        e.status
            .map(|s| Value::Int(s as i64))
            .unwrap_or(Value::Null),
    );
    r.insert("effects".into(), Value::Str(e.effects_json.clone()));
    r
}

#[cfg(test)]
#[path = "journal/tests.rs"]
mod tests;
