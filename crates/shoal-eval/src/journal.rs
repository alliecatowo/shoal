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

impl Evaluator {
    /// Install a command journal and the session/principal recorded on each
    /// entry (site/content/internals/language-conformance-contract.md). Additive: without this call `journal` stays `None` and
    /// nothing is ever recorded.
    pub fn set_journal(
        &mut self,
        journal: Journal,
        session: impl Into<String>,
        principal: impl Into<String>,
    ) {
        self.session.journal = Some(journal);
        self.session.session_id = session.into();
        self.session.principal = principal.into();
    }

    /// Open (creating if needed) the default per-user state-dir journal and
    /// install it. Hosts call this once for an interactive/kernel session;
    /// scripts and `-c` deliberately do not, so they keep the no-journal path.
    pub fn open_default_journal(
        &mut self,
        session: impl Into<String>,
        principal: impl Into<String>,
    ) -> Result<(), String> {
        let journal = Journal::open(&default_state_dir()).map_err(|e| e.to_string())?;
        self.set_journal(journal, session, principal);
        Ok(())
    }

    /// Provide the source text of the program about to be evaluated so each
    /// top-level statement's `src` can be sliced from it for the journal. The
    /// retained program copy is capped independently from each row's smaller
    /// source projection.
    pub fn set_source(&mut self, src: impl Into<String>) {
        let src = src.into();
        self.exec.control.source = Some(bounded_text(&src, MAX_JOURNAL_PROGRAM_SOURCE_BYTES));
    }

    /// Whether a journal is installed (for hosts/tests).
    pub fn has_journal(&self) -> bool {
        self.session.journal.is_some()
    }

    /// Start one host-visible evaluation and explicitly bind its statement
    /// rows to an optional coarse execution row. This also prevents a host
    /// from accidentally reusing the previous evaluation's final entry id.
    pub fn begin_journal_execution(&mut self, parent_id: Option<i64>) {
        self.exec.control.journal_parent_entry = parent_id;
        self.exec.control.last_completed_entry = None;
    }

    /// End the active host-visible evaluation and return its final durably
    /// completed statement id. Parentage is cleared even on evaluation error.
    pub fn take_last_journal_entry(&mut self) -> Option<i64> {
        self.exec.control.journal_parent_entry = None;
        self.exec.control.last_completed_entry.take()
    }

    /// Remember only the first persistence failure for the active statement.
    /// Later failures are usually consequences of the same unavailable store;
    /// bounding this state keeps a hostile multi-path command from amplifying
    /// error text while preserving the earliest causal stage.
    pub(crate) fn note_journal_failure(
        &mut self,
        stage: &'static str,
        error: impl std::fmt::Display,
    ) {
        note_failure(&mut self.exec.control.journal_failure, stage, error);
    }

    // --- per-statement recording ------------------------------------------

    /// Append a journal entry for `stmt` before evaluation starts. An absent
    /// journal remains a no-op; an installed journal that cannot persist the
    /// begin row rejects the statement before any effects execute.
    pub(crate) fn journal_begin_stmt(&mut self, stmt: &Stmt) -> VResult<Option<OpenJournalEntry>> {
        self.exec.control.current_entry = None;
        self.exec.control.journal_failure = None;
        // Cheap gate: nothing to record without a journal (scripts/-c/tests).
        if !self.has_journal() {
            return Ok(None);
        }
        let src = self.stmt_source(stmt);
        let ast_json =
            bounded_json(stmt, MAX_JOURNAL_AST_BYTES, "AST").unwrap_or_else(omitted_json);
        let (effects_json, opaque) = self.stmt_effects(stmt);
        let record = EntryRecord {
            kind: shoal_journal::EntryKind::Statement,
            parent_id: self.exec.control.journal_parent_entry,
            session: bounded_text(&self.session.session_id, MAX_JOURNAL_IDENTITY_BYTES),
            principal: bounded_text(&self.session.principal, MAX_JOURNAL_IDENTITY_BYTES),
            ts_ns: self.host.clock.now_ns(),
            cwd: self.exec.shell.cwd.as_os_str().as_bytes().to_vec(),
            src,
            ast_json,
            effects_json,
            opaque,
        };
        let id = self
            .session
            .journal
            .as_ref()
            .expect("journal presence checked")
            .append(&record)
            .map_err(journal_begin_error)?;
        self.exec.control.current_entry = Some(id);
        Ok(Some(OpenJournalEntry {
            id,
            started: Instant::now(),
        }))
    }

    /// Finish the entry opened by [`Evaluator::journal_begin_stmt`]: record the
    /// success verdict/status/duration and capture outputs (rendered value +
    /// stdout/stderr, or an error's stderr). Always clears `current_entry`.
    pub(crate) fn journal_finish_stmt(
        &mut self,
        opened: Option<OpenJournalEntry>,
        result: VResult<Flow>,
    ) -> VResult<Flow> {
        let Some(OpenJournalEntry { id, started }) = opened else {
            return result;
        };
        self.exec.control.current_entry = None;
        let mut failure = self.exec.control.journal_failure.take();
        let Some(journal) = self.session.journal.as_ref() else {
            note_failure(
                &mut failure,
                "finish",
                "installed journal disappeared before statement completion",
            );
            return finish_result(result, failure);
        };
        let dur = elapsed_ns(started);
        let mut outputs: Vec<(&'static str, Vec<u8>)> = Vec::new();
        let (status, ok) = match &result {
            Ok(flow) => {
                let value = match flow {
                    Flow::Value(v) | Flow::Return(v) => Some(v),
                    _ => None,
                };
                let (ok, status) = match value {
                    Some(Value::Outcome(o)) => (o.ok, o.status),
                    _ => (true, Some(0)),
                };
                if let Some(v) = value
                    && *v != Value::Null
                {
                    let render = shoal_value::render::render_block(v, 80);
                    if !render.is_empty() {
                        outputs.push(("render", render.into_bytes()));
                    }
                    if let Value::Outcome(o) = v {
                        if !o.stdout.is_empty() {
                            outputs.push(("stdout", o.stdout.to_vec()));
                        }
                        if !o.stderr.is_empty() {
                            outputs.push(("stderr", o.stderr.to_vec()));
                        }
                    }
                }
                (status, ok)
            }
            Err(err) => {
                if let Some(stderr) = &err.stderr {
                    outputs.push(("stderr", stderr.as_bytes().to_vec()));
                }
                (err.status, false)
            }
        };
        // Completion is the final persistence step. If any output/undo write
        // failed, never stamp the row as a clean success: the returned value is
        // indeterminate and the durable row is an explicit failure if this
        // final update itself succeeds.
        let (status, ok) = if failure.is_some() {
            (None, false)
        } else {
            (status, ok)
        };
        let output_refs = outputs
            .iter()
            .map(|(kind, bytes)| (*kind, bytes.as_slice()))
            .collect::<Vec<_>>();
        if let Err(error) = journal.complete_with_outputs(id, &output_refs, None, status, ok, dur) {
            note_failure(&mut failure, "output completion", error);
            if let Err(error) = journal.finish(id, None, false, dur) {
                note_failure(&mut failure, "failed completion marker", error);
            }
        } else {
            self.exec.control.last_completed_entry = Some(id);
        }
        finish_result(result, failure)
    }

    /// Slice the statement's source text from the program source, if provided.
    fn stmt_source(&self, stmt: &Stmt) -> String {
        let Some(src) = &self.exec.control.source else {
            return String::new();
        };
        let span = stmt.span();
        bounded_text(
            src.get(span.start as usize..span.end as usize)
                .unwrap_or(""),
            MAX_JOURNAL_SOURCE_BYTES,
        )
    }

    /// Derive the concrete effect set of a single statement (best-effort) as the
    /// entry's `effects_json`, plus whether it is opaque (T0 / `sh { }`).
    fn stmt_effects(&mut self, stmt: &Stmt) -> (String, bool) {
        let program = Program {
            stmts: vec![stmt.clone()],
        };
        match self.plan_program(&program) {
            Ok(plan) => {
                let opaque = plan.effects.iter().any(|e| matches!(e, Effect::Opaque));
                match bounded_json(&plan.effects, MAX_JOURNAL_EFFECT_BYTES, "effects") {
                    Ok(json) => (json, opaque),
                    Err(_) => ("[\"opaque\"]".into(), true),
                }
            }
            // A statement whose plan cannot be derived is treated as opaque.
            Err(_) => ("[\"opaque\"]".into(), true),
        }
    }

    // --- fs undo capture ---------------------------------------------------

    /// Before an overwriting `cp`/`mv`, snapshot each destination file that is
    /// about to be clobbered (and note moves whose destination is new). Returns
    /// an empty vec unless a journal + statement are active and the paths are
    /// literal (a non-literal arg is skipped rather than re-evaluated, so a
    /// command-substituted path never runs twice).
    pub(crate) fn fs_undo_pre(&mut self, head: &str, call: &CmdCall) -> Vec<FsUndoPre> {
        let Some(entry) = self.exec.control.current_entry else {
            return Vec::new();
        };
        if self.session.journal.is_none() || !matches!(head, "cp" | "mv") {
            return Vec::new();
        }
        let Some(paths) = self.literal_arg_paths(call) else {
            return Vec::new();
        };
        if paths.len() < 2 {
            return Vec::new();
        }
        let dest = paths.last().expect("len >= 2").clone();
        let sources = &paths[..paths.len() - 1];
        let mut out = Vec::new();
        for src in sources {
            let target = if self.host.fs.is_dir(&dest) {
                match src.file_name() {
                    Some(name) => dest.join(name),
                    None => continue,
                }
            } else {
                dest.clone()
            };
            if self.host.fs.is_file(&target)
                && let Some(hash) = self.snapshot_prior(entry, &target)
            {
                out.push(FsUndoPre::Overwrite {
                    path: target,
                    prior_hash: hash,
                });
            } else if head == "mv" && !self.host.fs.exists(&target) {
                out.push(FsUndoPre::Moved {
                    src: src.clone(),
                    dest: target,
                });
            }
        }
        out
    }

    /// After a `cp`/`mv`/`rm` builtin has run, record its typed undo inverses.
    pub(crate) fn fs_undo_post(&mut self, head: &str, pre: Vec<FsUndoPre>, result: &Value) {
        let Some(entry) = self.exec.control.current_entry else {
            return;
        };
        if self.session.journal.is_none() {
            return;
        }
        if head == "rm" {
            if let Err(error) = record_trash_inverses(
                self.session.journal.as_ref().expect("presence checked"),
                &EvalUndoIo(self.host.fs.as_ref()),
                entry,
                result,
            ) {
                self.note_journal_failure("trash undo inverse", error);
            }
            return;
        }
        let mut failure = None;
        for item in pre {
            match item {
                FsUndoPre::Overwrite { path, prior_hash } => {
                    match FileFingerprint::capture_with(&EvalUndoIo(self.host.fs.as_ref()), &path) {
                        Ok(fp) => {
                            if let Err(error) = self
                                .session
                                .journal
                                .as_ref()
                                .expect("presence checked")
                                .record_undo_inverse(
                                    entry,
                                    &UndoInverse::RestoreBytes {
                                        path,
                                        prior_hash,
                                        expected_current: fp,
                                    },
                                )
                            {
                                failure.get_or_insert_with(|| error.to_string());
                            }
                        }
                        Err(error) => {
                            failure.get_or_insert_with(|| error.to_string());
                        }
                    }
                }
                FsUndoPre::Moved { src, dest } => {
                    match FileFingerprint::capture_with(&EvalUndoIo(self.host.fs.as_ref()), &dest) {
                        Ok(fp) => {
                            if let Err(error) = self
                                .session
                                .journal
                                .as_ref()
                                .expect("presence checked")
                                .record_undo_inverse(
                                    entry,
                                    &UndoInverse::MoveBack {
                                        from: dest,
                                        to: src,
                                        expected_from: fp,
                                    },
                                )
                            {
                                failure.get_or_insert_with(|| error.to_string());
                            }
                        }
                        Err(error) => {
                            failure.get_or_insert_with(|| error.to_string());
                        }
                    }
                }
            }
        }
        if let Some(error) = failure {
            self.note_journal_failure("filesystem undo inverse", error);
        }
    }

    /// `save`-specific pre-capture: snapshot the prior bytes of `path` if it is
    /// an existing file under an active journal.
    pub(crate) fn save_undo_pre(&mut self, path: &Value) -> Option<FsUndoPre> {
        let target = self.value_to_path(path)?;
        self.overwrite_undo_pre(&target)
    }

    /// Redirect (`>` / `>>`) pre-capture: identical to `save`'s — if the target
    /// already exists, snapshot its prior bytes so an output redirect can be
    /// reversed by `undo` exactly like `cp`/`save` (site/content/internals/language-conformance-contract.md). A brand-new target
    /// records nothing: there is no create-inverse in [`UndoInverse`] yet, so a
    /// `>`/`>>` that creates a file is left non-reversible (documented
    /// follow-up), never faked. `>>` reuses the same overwrite inverse: undo
    /// restores the full prior contents, which drops the appended bytes.
    pub(crate) fn redirect_undo_pre(&mut self, target: &Path) -> Option<FsUndoPre> {
        self.overwrite_undo_pre(target)
    }

    /// Core overwrite pre-capture shared by `save` and output redirects: under
    /// an active journal + statement, if `target` is an existing file, snapshot
    /// its prior bytes into the CAS and yield the restore inverse to record
    /// after the write. `snapshot_prior` refuses (returns `None`) when the prior
    /// bytes would exceed the CAS cap and be stored truncated, so a corrupt
    /// partial-content inverse is never keyed.
    fn overwrite_undo_pre(&mut self, target: &Path) -> Option<FsUndoPre> {
        let entry = self.exec.control.current_entry?;
        self.session.journal.as_ref()?;
        if !self.host.fs.is_file(target) {
            return None;
        }
        let hash = self.snapshot_prior(entry, target)?;
        Some(FsUndoPre::Overwrite {
            path: target.to_path_buf(),
            prior_hash: hash,
        })
    }

    /// Turn an overwrite snapshot into a `RestoreBytes` inverse after the write
    /// has run. Shared by `save` and output-redirect (`>` / `>>`) writes.
    /// A post-write persistence failure is retained until the statement
    /// boundary, which reports an indeterminate result instead of clean success.
    pub(crate) fn overwrite_undo_post(&mut self, pre: Option<FsUndoPre>) {
        let (Some(entry), Some(FsUndoPre::Overwrite { path, prior_hash })) =
            (self.exec.control.current_entry, pre)
        else {
            return;
        };
        if self.session.journal.is_none() {
            return;
        }
        match FileFingerprint::capture_with(&EvalUndoIo(self.host.fs.as_ref()), &path) {
            Ok(fp) => {
                if let Err(error) = self
                    .session
                    .journal
                    .as_ref()
                    .expect("presence checked")
                    .record_undo_inverse(
                        entry,
                        &UndoInverse::RestoreBytes {
                            path,
                            prior_hash,
                            expected_current: fp,
                        },
                    )
                {
                    self.note_journal_failure("overwrite undo inverse", error);
                }
            }
            Err(error) => self.note_journal_failure("overwrite fingerprint", error),
        }
    }

    /// Read a file's current bytes and store them in the CAS, returning the
    /// blake3 hash to key an undo restore on. The output row keeps the blob
    /// referenced (safe from GC).
    ///
    /// Returns `None` when the snapshot could not be recorded *faithfully*: the
    /// evaluator refuses files above its own bounded-read ceiling, and a file
    /// above the journal's configured `output_hard_cap` is reported as
    /// truncated. Keying a replayable `RestoreBytes` inverse on either would
    /// let `undo` silently overwrite the user's file with partial content.
    /// Refusing leaves the operation honestly non-reversible.
    fn snapshot_prior(&mut self, entry: i64, path: &Path) -> Option<String> {
        match self.host.fs.metadata(path) {
            Ok(metadata) if metadata.len() > MAX_JOURNAL_UNDO_SNAPSHOT_BYTES as u64 => return None,
            Ok(_) => {}
            Err(error) => {
                self.note_journal_failure("undo snapshot metadata", error);
                return None;
            }
        }
        let bytes = match self.host.fs.read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.note_journal_failure("undo snapshot read", error);
                return None;
            }
        };
        let recorded =
            self.session
                .journal
                .as_ref()?
                .record_output_meta(entry, "undo-snapshot", &bytes);
        let (hash, meta) = match recorded {
            Ok(recorded) => recorded,
            Err(error) => {
                self.note_journal_failure("undo snapshot output", error);
                return None;
            }
        };
        if meta.is_some_and(|m| m.truncated) {
            return None;
        }
        Some(hash)
    }

    /// Resolve a command's non-flag args to absolute paths, but only when every
    /// one is a literal (word/path/literal string) — returns `None` on any glob
    /// or dynamic arg so the caller skips undo rather than double-evaluate.
    fn literal_arg_paths(&self, call: &CmdCall) -> Option<Vec<PathBuf>> {
        let mut out = Vec::new();
        for arg in &call.args {
            let text = match arg {
                CmdArg::Word { text, .. } | CmdArg::Path { text, .. } => text.clone(),
                CmdArg::Str { expr, .. } | CmdArg::Expr { expr, .. } => match expr {
                    Expr::Str { value, .. } => value.clone(),
                    _ => return None,
                },
                CmdArg::FlagLong { .. }
                | CmdArg::FlagShort { .. }
                | CmdArg::DashDash { .. }
                | CmdArg::Dash { .. } => continue,
                // Globs (and anything else) can expand to many paths / be
                // dynamic; skip undo entirely rather than guess.
                _ => return None,
            };
            let p = self.resolve_path(&text);
            out.push(if p.is_absolute() {
                p
            } else {
                self.exec.shell.cwd.join(p)
            });
        }
        Some(out)
    }

    fn value_to_path(&self, v: &Value) -> Option<PathBuf> {
        let p = match v {
            Value::Path(p) => p.clone(),
            Value::Str(s) => PathBuf::from(s),
            _ => return None,
        };
        Some(if p.is_absolute() {
            p
        } else {
            self.exec.shell.cwd.join(p)
        })
    }

    // --- undo / journal builtins ------------------------------------------

    /// The `undo` builtin (site/content/internals/language-conformance-contract.md). Bare `undo` reverses the most recent
    /// reversible journaled entry; `undo <id>` targets a specific entry. Replays
    /// the entry's typed inverses newest-first, refusing loudly if a target has
    /// changed since it was recorded.
    pub(crate) fn builtin_undo(&mut self, call: &CmdCall) -> VResult<Value> {
        if self.session.journal.is_none() {
            return Err(ErrorVal::new(
                "custom",
                "undo requires a journaled session; none is active",
            )
            .with_span(call.span));
        }
        let target = self.undo_target_id(call)?;
        let journal = self.session.journal.as_ref().expect("checked");
        let entry_id = match target {
            Some(id) => id,
            None => last_reversible_entry(journal)?.ok_or_else(|| {
                ErrorVal::new(
                    "custom",
                    "nothing to undo: no reversible entry in the journal",
                )
                .with_span(call.span)
            })?,
        };
        let root = self.exec.shell.cwd.clone();
        let report = journal
            .undo_entry_with(entry_id, &root, &EvalUndoIo(self.host.fs.as_ref()))
            .map_err(|e| {
                let code = match e {
                    UndoError::Stale(_) => "stale_undo",
                    _ => "custom",
                };
                ErrorVal::new(code, format!("undo of out:{entry_id} refused: {e}"))
                    .with_span(call.span)
            })?;
        Ok(undo_report_value(&report))
    }

    /// Resolve the optional undo target: an integer entry id (or its string
    /// form). `out[n]` addressing is a REPL/host concern (the evaluator has no
    /// out→entry map), so a non-integer target is a clear error.
    fn undo_target_id(&mut self, call: &CmdCall) -> VResult<Option<i64>> {
        let mut vs = self.collect_cmd_values(call)?;
        match vs.drain(..).next() {
            None => Ok(None),
            Some(Value::Int(i)) => Ok(Some(i)),
            Some(Value::Str(s)) => s
                .trim()
                .parse::<i64>()
                .map(Some)
                .map_err(|_| ErrorVal::arg_error("undo target must be a journal entry id")),
            Some(_) => Err(ErrorVal::arg_error(
                "undo target must be a journal entry id (e.g. `undo 12`)",
            )),
        }
    }

    /// The `journal` / `history` builtin: a table view over the journal
    /// (id, ts, principal, src-head, ok, status, effects). Returns an empty
    /// table when no journal is installed (never crashes). `--head <word>` and
    /// `--principal <who>` filter; `--limit <n>` caps the row count.
    pub(crate) fn builtin_journal_view(&mut self, call: &CmdCall) -> VResult<Value> {
        let Some(journal) = self.session.journal.as_ref() else {
            return Ok(Value::Table(Vec::new()));
        };
        let mut query = JournalQuery::default();
        for arg in &call.args {
            if let CmdArg::FlagLong {
                name,
                value: Some(v),
                ..
            } = arg
                && let Some(text) = literal_cmdarg_text(v)
            {
                match name.as_str() {
                    "head" => query.head = Some(text),
                    "principal" => query.principal = Some(text),
                    "limit" => {
                        if let Ok(n) = text.parse::<usize>() {
                            query.limit = n;
                        }
                    }
                    _ => {}
                }
            }
        }
        let rows = journal
            .query(&query)
            .map_err(|e| ErrorVal::new("custom", format!("journal query failed: {e}")))?;
        let table = rows.iter().map(entry_row_record).collect();
        Ok(Value::Table(table))
    }
}

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
