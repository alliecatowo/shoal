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
                    if !render.is_empty()
                        && let Err(error) = journal.record_output(id, "render", render.as_bytes())
                    {
                        note_failure(&mut failure, "render output", error);
                    }
                    if let Value::Outcome(o) = v {
                        if !o.stdout.is_empty()
                            && let Err(error) = journal.record_output(id, "stdout", &o.stdout)
                        {
                            note_failure(&mut failure, "stdout output", error);
                        }
                        if !o.stderr.is_empty()
                            && let Err(error) = journal.record_output(id, "stderr", &o.stderr)
                        {
                            note_failure(&mut failure, "stderr output", error);
                        }
                    }
                }
                (status, ok)
            }
            Err(err) => {
                if let Some(stderr) = &err.stderr
                    && let Err(error) = journal.record_output(id, "stderr", stderr.as_bytes())
                {
                    note_failure(&mut failure, "error stderr output", error);
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
        if let Err(error) = journal.finish(id, status, ok, dur) {
            note_failure(&mut failure, "finish", error);
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
mod tests {
    use super::*;
    use shoal_journal::{Journal, JournalOptions};
    use shoal_value::{ReadSeek, StdFs};
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    struct LockAfterWriteFs {
        db_path: PathBuf,
        fail_after_write: bool,
        blocker: Mutex<Option<rusqlite::Connection>>,
        writes: AtomicUsize,
    }

    impl LockAfterWriteFs {
        fn new(db_path: PathBuf, fail_after_write: bool) -> Self {
            Self {
                db_path,
                fail_after_write,
                blocker: Mutex::new(None),
                writes: AtomicUsize::new(0),
            }
        }

        fn lock_journal(&self) -> io::Result<()> {
            let connection = rusqlite::Connection::open(&self.db_path).map_err(io::Error::other)?;
            connection
                .busy_timeout(std::time::Duration::ZERO)
                .map_err(io::Error::other)?;
            connection
                .execute_batch("BEGIN IMMEDIATE")
                .map_err(io::Error::other)?;
            *self.blocker.lock().unwrap() = Some(connection);
            Ok(())
        }

        fn release(&self) {
            self.blocker.lock().unwrap().take();
        }

        fn unsupported<T>() -> io::Result<T> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "journal failure test filesystem only supports writes",
            ))
        }
    }

    impl Fs for LockAfterWriteFs {
        fn read(&self, _path: &Path) -> io::Result<Vec<u8>> {
            Self::unsupported()
        }
        fn read_to_string(&self, _path: &Path) -> io::Result<String> {
            Self::unsupported()
        }
        fn open_read(&self, _path: &Path) -> io::Result<Box<dyn ReadSeek + Send>> {
            Self::unsupported()
        }
        fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
            StdFs.write(path, data)?;
            self.writes.fetch_add(1, Ordering::SeqCst);
            self.lock_journal()?;
            if self.fail_after_write {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected primary write failure after bytes reached the filesystem",
                ))
            } else {
                Ok(())
            }
        }
        fn append(&self, _path: &Path, _data: &[u8]) -> io::Result<()> {
            Self::unsupported()
        }
        fn touch(&self, _path: &Path) -> io::Result<()> {
            Self::unsupported()
        }
        fn metadata(&self, _path: &Path) -> io::Result<std::fs::Metadata> {
            Self::unsupported()
        }
        fn symlink_metadata(&self, _path: &Path) -> io::Result<std::fs::Metadata> {
            Self::unsupported()
        }
        fn read_dir(&self, _path: &Path) -> io::Result<Vec<PathBuf>> {
            Self::unsupported()
        }
        fn create_dir(&self, _path: &Path) -> io::Result<()> {
            Self::unsupported()
        }
        fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
            Self::unsupported()
        }
        fn remove_file(&self, _path: &Path) -> io::Result<()> {
            Self::unsupported()
        }
        fn remove_dir_all(&self, _path: &Path) -> io::Result<()> {
            Self::unsupported()
        }
        fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
            Self::unsupported()
        }
        fn copy(&self, _from: &Path, _to: &Path) -> io::Result<u64> {
            Self::unsupported()
        }
        fn hard_link(&self, _src: &Path, _dst: &Path) -> io::Result<()> {
            Self::unsupported()
        }
        fn symlink(&self, _target: &Path, _link: &Path) -> io::Result<()> {
            Self::unsupported()
        }
    }

    #[derive(Default)]
    struct RecordingDenyAtomicFs {
        operations: Mutex<Vec<String>>,
    }

    impl RecordingDenyAtomicFs {
        fn record(&self, operation: &str, path: &Path) {
            self.operations
                .lock()
                .unwrap()
                .push(format!("{operation}:{}", path.display()));
        }

        fn operations(&self) -> Vec<String> {
            self.operations.lock().unwrap().clone()
        }
    }

    impl Fs for RecordingDenyAtomicFs {
        fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
            self.record("read", path);
            StdFs.read(path)
        }
        fn read_to_string(&self, path: &Path) -> io::Result<String> {
            StdFs.read_to_string(path)
        }
        fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadSeek + Send>> {
            StdFs.open_read(path)
        }
        fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
            StdFs.write(path, data)
        }
        fn append(&self, path: &Path, data: &[u8]) -> io::Result<()> {
            StdFs.append(path, data)
        }
        fn touch(&self, path: &Path) -> io::Result<()> {
            StdFs.touch(path)
        }
        fn metadata(&self, path: &Path) -> io::Result<std::fs::Metadata> {
            StdFs.metadata(path)
        }
        fn symlink_metadata(&self, path: &Path) -> io::Result<std::fs::Metadata> {
            self.record("symlink_metadata", path);
            StdFs.symlink_metadata(path)
        }
        fn exists(&self, path: &Path) -> bool {
            self.record("exists", path);
            StdFs.exists(path)
        }
        fn is_file(&self, path: &Path) -> bool {
            StdFs.is_file(path)
        }
        fn is_dir(&self, path: &Path) -> bool {
            StdFs.is_dir(path)
        }
        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            self.record("canonicalize", path);
            StdFs.canonicalize(path)
        }
        fn atomic_replace(&self, path: &Path, _data: &[u8]) -> io::Result<()> {
            self.record("atomic_replace", path);
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "test adapter denied atomic replacement",
            ))
        }
        fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
            StdFs.read_dir(path)
        }
        fn create_dir(&self, path: &Path) -> io::Result<()> {
            self.record("create_dir", path);
            StdFs.create_dir(path)
        }
        fn create_dir_all(&self, path: &Path) -> io::Result<()> {
            StdFs.create_dir_all(path)
        }
        fn remove_file(&self, path: &Path) -> io::Result<()> {
            StdFs.remove_file(path)
        }
        fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
            StdFs.remove_dir_all(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.record("rename", from);
            StdFs.rename(from, to)
        }
        fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
            StdFs.copy(from, to)
        }
        fn hard_link(&self, src: &Path, dst: &Path) -> io::Result<()> {
            StdFs.hard_link(src, dst)
        }
        fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
            StdFs.symlink(target, link)
        }
    }

    /// Build a journaled evaluator rooted at `cwd` with an in-memory journal.
    ///
    /// `cwd` is canonicalized first: `undo_entry` canonicalizes its `root`
    /// argument before checking that a recorded undo target is contained in
    /// it (`checked_target`), and on macOS a tempdir's path is a symlink
    /// alias (`/var/folders/...` -> `/private/var/folders/...`). Rooting the
    /// evaluator at the already-canonical path keeps every path it records
    /// (via `self.exec.shell.cwd.join(...)`) on the same prefix `undo_entry` compares
    /// against — mirroring the fix already applied to shoal-journal's own
    /// `root.path().canonicalize()` tests.
    fn journaled(cwd: &Path) -> Evaluator {
        let root = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let mut ev = Evaluator::new(root);
        ev.set_journal(Journal::in_memory().unwrap(), "s1", "human");
        ev
    }

    fn run_journaled(ev: &mut Evaluator, src: &str) -> VResult<Value> {
        let program = shoal_syntax::parse(src).expect("parse");
        ev.set_source(src);
        ev.eval_program(&program)
    }

    #[test]
    fn statement_records_entry_finish_and_output() {
        let dir = tempfile::tempdir().unwrap();
        let mut ev = journaled(dir.path());
        run_journaled(&mut ev, "echo hi").unwrap();
        let journal = ev.session.journal.as_ref().unwrap();
        let rows = journal.query(&JournalQuery::default()).unwrap();
        assert_eq!(rows.len(), 1, "one entry recorded");
        let entry = &rows[0];
        assert_eq!(entry.src, "echo hi");
        assert_eq!(entry.principal, "human");
        assert_eq!(entry.session, "s1");
        // finish() ran: status/ok/duration are populated.
        assert_eq!(entry.ok, Some(true));
        assert_eq!(entry.status, Some(0));
        assert!(entry.dur_ns.is_some());
        // outputs captured: a render, plus echo's stdout.
        let kinds: Vec<&str> = entry.outputs.iter().map(|o| o.kind.as_str()).collect();
        assert!(kinds.contains(&"render"), "outputs: {kinds:?}");
        assert!(kinds.contains(&"stdout"), "outputs: {kinds:?}");
    }

    fn zero_timeout_journal(state: &Path) -> Journal {
        Journal::open_with_options(
            state,
            JournalOptions {
                busy_timeout: std::time::Duration::ZERO,
                ..JournalOptions::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn installed_journal_begin_failure_prevents_statement_effects() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let journal = zero_timeout_journal(&state);
        let blocker = rusqlite::Connection::open(state.join("journal.db")).unwrap();
        blocker.busy_timeout(std::time::Duration::ZERO).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

        let target = dir.path().join("must-not-exist");
        let mut evaluator = Evaluator::new(dir.path().to_path_buf());
        evaluator.set_journal(journal, "session", "human");
        let error = run_journaled(
            &mut evaluator,
            &format!("save(\"{}\", \"payload\")", target.display()),
        )
        .unwrap_err();

        assert_eq!(error.code, "journal_begin_failed");
        assert!(error.msg.contains("before statement execution"));
        assert!(!target.exists(), "the language effect must not have run");
        drop(blocker);
        let rows = evaluator
            .session
            .journal
            .as_ref()
            .unwrap()
            .query(&JournalQuery::default())
            .unwrap();
        assert!(rows.is_empty(), "a failed begin cannot invent an entry");
    }

    #[test]
    fn exhausted_journal_budget_refuses_before_filesystem_effects() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let journal = Journal::open_with_options(
            &state,
            JournalOptions {
                database_max_bytes: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let target = dir.path().join("must-not-exist-budget");
        let mut evaluator = Evaluator::new(dir.path().to_path_buf());
        evaluator.set_journal(journal, "session", "human");
        let error = run_journaled(
            &mut evaluator,
            &format!("save(\"{}\", \"payload\")", target.display()),
        )
        .unwrap_err();
        assert_eq!(error.code, "journal_begin_failed");
        assert!(!target.exists(), "begin refusal must precede the effect");
        assert!(
            evaluator
                .session
                .journal
                .as_ref()
                .unwrap()
                .query(&JournalQuery::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn exhausted_cas_budget_marks_post_effect_completion_indeterminate() {
        let dir = tempfile::tempdir().unwrap();
        let mut evaluator = Evaluator::new(dir.path().to_path_buf());
        evaluator.set_journal(
            Journal::in_memory_with_options(JournalOptions {
                cas_max_bytes: 1,
                ..Default::default()
            })
            .unwrap(),
            "session",
            "human",
        );
        let error = run_journaled(&mut evaluator, "echo output").unwrap_err();
        assert_eq!(error.code, "journal_commit_indeterminate");
        assert!(error.msg.contains("output"), "{}", error.msg);
        let rows = evaluator
            .session
            .journal
            .as_ref()
            .unwrap()
            .query(&JournalQuery::default())
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ok, Some(false));
        assert!(rows[0].outputs.is_empty());
    }

    #[test]
    fn finish_failure_reports_that_effects_may_have_occurred() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let journal = zero_timeout_journal(&state);
        let fs = Arc::new(LockAfterWriteFs::new(state.join("journal.db"), false));
        let target = dir.path().join("written-before-finish");
        let mut evaluator = Evaluator::new(dir.path().to_path_buf());
        evaluator.set_journal(journal, "session", "human");
        evaluator.set_fs(fs.clone());

        let error = run_journaled(
            &mut evaluator,
            &format!("save(\"{}\", \"payload\")", target.display()),
        )
        .unwrap_err();
        assert_eq!(error.code, "journal_commit_indeterminate");
        assert!(error.msg.contains("effects may already have occurred"));
        assert!(error.hint.unwrap().contains("do not blindly retry"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "payload");
        assert_eq!(fs.writes.load(Ordering::SeqCst), 1);

        fs.release();
        let rows = evaluator
            .session
            .journal
            .as_ref()
            .unwrap()
            .query(&JournalQuery::default())
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ok, None, "failed finish leaves an honest open row");
    }

    #[test]
    fn primary_and_journal_failures_are_reported_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let journal = zero_timeout_journal(&state);
        let fs = Arc::new(LockAfterWriteFs::new(state.join("journal.db"), true));
        let target = dir.path().join("possibly-written");
        let mut evaluator = Evaluator::new(dir.path().to_path_buf());
        evaluator.set_journal(journal, "session", "human");
        evaluator.set_fs(fs.clone());

        let error = run_journaled(
            &mut evaluator,
            &format!("save(\"{}\", \"payload\")", target.display()),
        )
        .unwrap_err();
        assert_eq!(error.code, "journal_commit_indeterminate");
        assert!(error.msg.contains("primary error was custom"));
        assert!(error.msg.contains("injected primary write failure"));
        assert!(error.msg.contains("effects may already have occurred"));
        assert_eq!(std::fs::read_to_string(target).unwrap(), "payload");
        fs.release();
    }

    #[test]
    fn disabled_language_journal_preserves_ordinary_evaluation() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("ordinary");
        let mut evaluator = Evaluator::new(dir.path().to_path_buf());
        let source = format!("save(\"{}\", \"payload\")", target.display());
        let value = evaluator
            .eval_program(&shoal_syntax::parse(&source).unwrap())
            .unwrap();
        assert_eq!(value, Value::Str("payload".into()));
        assert_eq!(std::fs::read_to_string(target).unwrap(), "payload");
    }

    #[test]
    fn journal_entry_text_and_json_fields_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let mut evaluator = Evaluator::new(dir.path().to_path_buf());
        evaluator.set_journal(
            Journal::in_memory().unwrap(),
            "s".repeat(MAX_JOURNAL_IDENTITY_BYTES * 2),
            "p".repeat(MAX_JOURNAL_IDENTITY_BYTES * 2),
        );
        let source = format!("\"{}\"", "x".repeat(MAX_JOURNAL_AST_BYTES + 1024));
        run_journaled(&mut evaluator, &source).unwrap();

        let rows = evaluator
            .session
            .journal
            .as_ref()
            .unwrap()
            .query(&JournalQuery::default())
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].session.len() <= MAX_JOURNAL_IDENTITY_BYTES);
        assert!(rows[0].principal.len() <= MAX_JOURNAL_IDENTITY_BYTES);
        assert!(rows[0].src.len() <= MAX_JOURNAL_SOURCE_BYTES);
        assert!(rows[0].src.contains("journal field truncated"));
        assert!(rows[0].ast_json.len() <= MAX_JOURNAL_AST_BYTES);
        assert!(rows[0].ast_json.contains("AST exceeds journal byte limit"));
        assert!(rows[0].effects_json.len() <= MAX_JOURNAL_EFFECT_BYTES);
    }

    #[test]
    fn rm_records_trash_undo_and_undo_restores() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("victim"), b"payload").unwrap();
        let mut ev = journaled(dir.path());
        run_journaled(&mut ev, "rm victim").unwrap();
        assert!(!dir.path().join("victim").exists(), "rm trashed the file");
        // A trash inverse was recorded.
        let rows = ev
            .session
            .journal
            .as_ref()
            .unwrap()
            .query(&JournalQuery::default())
            .unwrap();
        let undos = ev
            .session
            .journal
            .as_ref()
            .unwrap()
            .undos_for(rows[0].id)
            .unwrap();
        assert_eq!(undos.len(), 1);
        assert_eq!(undos[0].0, "trash_move");
        // `undo` moves it back with its original bytes.
        let report = run_journaled(&mut ev, "undo").unwrap();
        assert!(dir.path().join("victim").exists(), "undo restored the file");
        assert_eq!(
            std::fs::read(dir.path().join("victim")).unwrap(),
            b"payload"
        );
        assert!(matches!(report, Value::Record(_)));
    }

    #[test]
    fn overwrite_records_restore_bytes_and_undo_restores_prior() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"original").unwrap();
        let mut ev = journaled(dir.path());
        run_journaled(&mut ev, r#"save("f.txt", "replacement")"#).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("f.txt")).unwrap(),
            b"replacement"
        );
        let rows = ev
            .session
            .journal
            .as_ref()
            .unwrap()
            .query(&JournalQuery::default())
            .unwrap();
        let undos = ev
            .session
            .journal
            .as_ref()
            .unwrap()
            .undos_for(rows[0].id)
            .unwrap();
        assert_eq!(undos[0].0, "restore_bytes");
        // `undo` brings back the prior contents.
        run_journaled(&mut ev, "undo").unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("f.txt")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn undo_replay_is_mediated_and_atomic_replace_denial_is_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("f.txt");
        std::fs::write(&target, b"original").unwrap();
        let mut ev = journaled(dir.path());
        run_journaled(&mut ev, r#"save("f.txt", "replacement")"#).unwrap();

        let fs = Arc::new(RecordingDenyAtomicFs::default());
        ev.set_fs(fs.clone());
        let error = run_journaled(&mut ev, "undo").unwrap_err();
        assert_eq!(error.code, "custom");
        assert!(
            error.msg.contains("test adapter denied atomic replacement"),
            "{}",
            error.msg
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"replacement");

        let operations = fs.operations();
        assert!(
            operations
                .iter()
                .any(|op| op == &format!("exists:{}", target.display())),
            "existence probe escaped the adapter: {operations:?}"
        );
        assert!(
            operations
                .iter()
                .any(|op| op == &format!("read:{}", target.display())),
            "fingerprint read escaped the adapter: {operations:?}"
        );
        assert!(
            operations
                .iter()
                .any(|op| op == &format!("atomic_replace:{}", target.display())),
            "restore escaped the adapter: {operations:?}"
        );
    }

    /// When prior contents exceed the journal's `output_hard_cap`,
    /// the undo snapshot would be stored truncated (partial bytes + marker).
    /// `snapshot_prior` must refuse to record a replayable `RestoreBytes`
    /// inverse in that case, so `undo` can never restore corrupt partial content
    /// over the user's file. The op is simply left non-reversible.
    #[test]
    fn overwrite_larger_than_cap_records_no_reversible_inverse() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        // Prior contents (500 bytes) exceed the 128-byte cap → snapshot truncates.
        std::fs::write(root.join("f.txt"), vec![b'a'; 500]).unwrap();
        let mut ev = Evaluator::new(root.clone());
        ev.set_journal(
            Journal::in_memory_with_options(JournalOptions {
                output_hard_cap: 128,
                ..Default::default()
            })
            .unwrap(),
            "s1",
            "human",
        );

        run_journaled(&mut ev, r#"save("f.txt", "small-replacement")"#).unwrap();
        assert_eq!(
            std::fs::read(root.join("f.txt")).unwrap(),
            b"small-replacement"
        );

        // No restore_bytes inverse was recorded for the truncated snapshot.
        let rows = ev
            .session
            .journal
            .as_ref()
            .unwrap()
            .query(&JournalQuery::default())
            .unwrap();
        let undos = ev
            .session
            .journal
            .as_ref()
            .unwrap()
            .undos_for(rows[0].id)
            .unwrap();
        assert!(
            undos.is_empty(),
            "a truncated snapshot must not key a replayable inverse; got {undos:?}"
        );

        // And `undo` therefore has nothing to restore — it never writes the
        // truncated+marker bytes over the file.
        let err = run_journaled(&mut ev, "undo").unwrap_err();
        assert_eq!(err.code, "custom", "{}", err.msg);
        assert!(err.msg.contains("nothing to undo"), "{}", err.msg);
    }

    #[test]
    fn cp_overwrite_records_restore_bytes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("src"), b"newbytes").unwrap();
        std::fs::write(dir.path().join("dst"), b"prior-dst").unwrap();
        let mut ev = journaled(dir.path());
        run_journaled(&mut ev, "cp src dst").unwrap();
        assert_eq!(std::fs::read(dir.path().join("dst")).unwrap(), b"newbytes");
        run_journaled(&mut ev, "undo").unwrap();
        assert_eq!(std::fs::read(dir.path().join("dst")).unwrap(), b"prior-dst");
    }

    #[test]
    fn redirect_out_overwrite_records_restore_bytes_and_undo_restores() {
        // `echo x > f` clobbers an existing file's contents exactly like `cp`,
        // so it must record a `RestoreBytes` inverse and be reversible.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"original\n").unwrap();
        let mut ev = journaled(dir.path());
        run_journaled(&mut ev, "echo replaced > f.txt").unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("f.txt")).unwrap(),
            b"replaced\n"
        );
        let rows = ev
            .session
            .journal
            .as_ref()
            .unwrap()
            .query(&JournalQuery::default())
            .unwrap();
        let undos = ev
            .session
            .journal
            .as_ref()
            .unwrap()
            .undos_for(rows[0].id)
            .unwrap();
        assert_eq!(undos[0].0, "restore_bytes");
        run_journaled(&mut ev, "undo").unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("f.txt")).unwrap(),
            b"original\n"
        );
    }

    #[test]
    fn redirect_append_records_restore_bytes_and_undo_restores_prior() {
        // `>>` grows the file; undo restores the full prior contents (dropping
        // the appended bytes) via the same overwrite `RestoreBytes` inverse.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("log.txt"), b"first\n").unwrap();
        let mut ev = journaled(dir.path());
        run_journaled(&mut ev, "echo second >> log.txt").unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("log.txt")).unwrap(),
            b"first\nsecond\n"
        );
        let rows = ev
            .session
            .journal
            .as_ref()
            .unwrap()
            .query(&JournalQuery::default())
            .unwrap();
        let undos = ev
            .session
            .journal
            .as_ref()
            .unwrap()
            .undos_for(rows[0].id)
            .unwrap();
        assert_eq!(undos[0].0, "restore_bytes");
        run_journaled(&mut ev, "undo").unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("log.txt")).unwrap(),
            b"first\n"
        );
    }

    #[test]
    fn external_redirect_overwrite_records_restore_bytes_and_undo_restores() {
        // The external-command redirect site (`command.rs`) must wrap its write
        // too: `some-cmd > f` / `sh -c '…' > f` is reversible like a builtin's.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"original").unwrap();
        let mut ev = journaled(dir.path());
        run_journaled(&mut ev, "sh -c 'printf replaced' > f.txt").unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("f.txt")).unwrap(),
            b"replaced"
        );
        let rows = ev
            .session
            .journal
            .as_ref()
            .unwrap()
            .query(&JournalQuery::default())
            .unwrap();
        let undos = ev
            .session
            .journal
            .as_ref()
            .unwrap()
            .undos_for(rows[0].id)
            .unwrap();
        assert_eq!(undos[0].0, "restore_bytes");
        run_journaled(&mut ev, "undo").unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("f.txt")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn redirect_to_new_file_records_no_inverse() {
        // A redirect that CREATES a new file has no reversible inverse yet:
        // `UndoInverse` carries no create/delete variant, so create-new is left
        // non-reversible (documented follow-up) rather than faked.
        let dir = tempfile::tempdir().unwrap();
        let mut ev = journaled(dir.path());
        run_journaled(&mut ev, "echo fresh > new.txt").unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("new.txt")).unwrap(),
            b"fresh\n"
        );
        let rows = ev
            .session
            .journal
            .as_ref()
            .unwrap()
            .query(&JournalQuery::default())
            .unwrap();
        let undos = ev
            .session
            .journal
            .as_ref()
            .unwrap()
            .undos_for(rows[0].id)
            .unwrap();
        assert!(
            undos.is_empty(),
            "create-new redirect must not fake an inverse; got {undos:?}"
        );
        let err = run_journaled(&mut ev, "undo").unwrap_err();
        assert!(err.msg.contains("nothing to undo"), "{}", err.msg);
    }

    #[test]
    fn redirect_overwrite_larger_than_cap_records_no_reversible_inverse() {
        // The truncation guard applies to redirects too: prior contents that
        // exceed the CAS cap would snapshot truncated, so `snapshot_prior`
        // refuses and no replayable `RestoreBytes` is keyed — undo can never
        // overwrite the file with corrupt partial bytes.
        let dir = tempfile::tempdir().unwrap();
        let root = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        std::fs::write(root.join("f.txt"), vec![b'a'; 500]).unwrap();
        let mut ev = Evaluator::new(root.clone());
        ev.set_journal(
            Journal::in_memory_with_options(JournalOptions {
                output_hard_cap: 128,
                ..Default::default()
            })
            .unwrap(),
            "s1",
            "human",
        );
        run_journaled(&mut ev, "echo small > f.txt").unwrap();
        let rows = ev
            .session
            .journal
            .as_ref()
            .unwrap()
            .query(&JournalQuery::default())
            .unwrap();
        let undos = ev
            .session
            .journal
            .as_ref()
            .unwrap()
            .undos_for(rows[0].id)
            .unwrap();
        assert!(
            undos.is_empty(),
            "a truncated prior snapshot must not key a replayable inverse; got {undos:?}"
        );
    }

    #[test]
    fn stale_target_makes_undo_refuse() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"original").unwrap();
        let mut ev = journaled(dir.path());
        run_journaled(&mut ev, r#"save("f.txt", "replacement")"#).unwrap();
        // Someone else changes the file after the recorded op.
        std::fs::write(dir.path().join("f.txt"), b"tampered-by-someone-else").unwrap();
        let err = run_journaled(&mut ev, "undo").unwrap_err();
        assert_eq!(err.code, "stale_undo", "{}", err.msg);
        // The tampered content is untouched — undo refused rather than clobber.
        assert_eq!(
            std::fs::read(dir.path().join("f.txt")).unwrap(),
            b"tampered-by-someone-else"
        );
    }

    #[test]
    fn journal_builtin_returns_entries_as_table() {
        let dir = tempfile::tempdir().unwrap();
        let mut ev = journaled(dir.path());
        run_journaled(&mut ev, "echo one").unwrap();
        run_journaled(&mut ev, "echo two").unwrap();
        let table = run_journaled(&mut ev, "journal").unwrap();
        let Value::Table(rows) = table else {
            panic!("journal should be a table, got {table:?}")
        };
        // Two prior statements are present (the `journal` call itself is the
        // third, newest-first).
        assert!(rows.len() >= 3, "rows: {}", rows.len());
        assert!(rows.iter().all(|r| r.get("id").is_some()));
        // The `src` column carries the FULL statement source, not just the head
        // word (regression: the view used to slice off everything after the
        // first space, so a populated `src` still rendered as good as empty).
        assert!(
            rows.iter()
                .any(|r| r.get("src") == Some(&Value::Str("echo one".into()))),
            "src column should show the full source line: {rows:?}"
        );
    }

    #[test]
    fn journal_view_src_column_shows_full_source_not_head() {
        // Regression (BUG: empty/head-only `src` column): the `history`/
        // `journal` view must render the ENTIRE recorded source under `src`,
        // not just the first whitespace-delimited word. A multi-token command
        // whose head alone is uninformative proves the full line survives.
        let dir = tempfile::tempdir().unwrap();
        let mut ev = journaled(dir.path());
        run_journaled(&mut ev, "echo alpha beta gamma").unwrap();
        let table = run_journaled(&mut ev, "journal").unwrap();
        let Value::Table(rows) = table else {
            panic!("journal should be a table, got {table:?}")
        };
        assert!(
            rows.iter()
                .any(|r| r.get("src") == Some(&Value::Str("echo alpha beta gamma".into()))),
            "full source expected in the src column, got: {rows:?}"
        );
    }

    #[test]
    fn no_journal_evaluator_records_nothing_and_view_is_empty() {
        // The zero-regression path: a plain evaluator journals nothing and the
        // `journal` builtin returns an empty table instead of crashing.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x"), b"x").unwrap();
        let mut ev = Evaluator::new(dir.path().to_path_buf());
        assert!(!ev.has_journal());
        let program = shoal_syntax::parse("rm x\njournal").unwrap();
        let out = ev.eval_program(&program).unwrap();
        assert_eq!(out, Value::Table(Vec::new()), "empty journal view");
        assert!(
            !dir.path().join("x").exists(),
            "rm still works with no journal"
        );
    }

    #[test]
    fn undo_without_journal_errors_clearly() {
        let dir = tempfile::tempdir().unwrap();
        let mut ev = Evaluator::new(dir.path().to_path_buf());
        let program = shoal_syntax::parse("undo").unwrap();
        let err = ev.eval_program(&program).unwrap_err();
        assert_eq!(err.code, "custom");
        assert!(err.msg.contains("journaled session"), "{}", err.msg);
    }

    /// site/content/internals/language-conformance-contract.md disk-spill: a value-position capture whose stdout exceeds the
    /// RAM cap is preserved to the CAS as a ref-backed value — `.len` is the
    /// true (full) length, the blob exists and its blake3 matches, render shows
    /// a bounded preview + the `val:blake3:…` ref (not the whole thing), and
    /// materialization loads the correct full bytes. Nothing is lost.
    #[test]
    fn value_capture_over_cap_spills_to_cas_ref_backed() {
        let dir = tempfile::tempdir().unwrap();
        let prev_cap = shoal_exec::capture_hard_cap();
        shoal_exec::set_capture_hard_cap(4096);
        let mut ev = journaled(dir.path());

        // A deterministic 200_000-byte capture (200_000 NUL bytes) past the cap.
        let v =
            run_journaled(&mut ev, "let x = sh { head -c 200000 /dev/zero }\nx.stdout").unwrap();

        // The value is ref-backed, carrying the true length + a bounded preview.
        let Value::CasBytes(c) = &v else {
            shoal_exec::set_capture_hard_cap(prev_cap);
            panic!("expected a ref-backed CasBytes, got {}", v.type_name());
        };
        assert_eq!(c.len, 200_000, ".len is the true length, not the preview");
        assert!(
            c.preview.len() <= 4096,
            "preview stays bounded by the RAM cap"
        );
        assert!(!c.truncated, "the full stream fit under the spill cap");
        let hash = c.hash.clone();

        // `.len` answers the TRUE length through method dispatch, without loading.
        assert_eq!(
            run_journaled(&mut ev, "x.stdout.len").unwrap(),
            Value::Int(200_000)
        );

        // Render is a bounded preview + the recoverable ref — not the full 200KB.
        let inline = shoal_value::render::render_inline(&v);
        assert!(
            inline.contains("val:blake3:") && inline.contains(&hash),
            "inline render carries the ref: {inline}"
        );
        let block = shoal_value::render::render_block(&v, 80);
        assert!(block.contains(&hash), "block render carries the ref");
        assert!(
            block.len() < 100_000,
            "render shows a preview, not the whole content ({} bytes)",
            block.len()
        );

        // The CAS blob exists and its blake3 matches (Cas::read re-hashes and
        // verifies the content against `hash` before returning it).
        let expected = vec![0u8; 200_000];
        let cas = ev.session.journal.as_ref().unwrap().cas();
        assert_eq!(
            cas.read(&hash).unwrap(),
            expected,
            "the CAS blob is the full, verbatim capture"
        );

        // Materialization loads the correct full bytes from the CAS.
        let loaded = run_journaled(&mut ev, "x.stdout.load()").unwrap();
        assert_eq!(loaded, Value::Bytes(std::sync::Arc::new(expected)));

        shoal_exec::set_capture_hard_cap(prev_cap);
    }

    /// Zero-regression: a sub-cap value-position capture stays fully resident —
    /// a plain `bytes`, no spill, no CAS blob — exactly as before capture spill was added.
    #[test]
    fn value_capture_under_cap_stays_resident() {
        let dir = tempfile::tempdir().unwrap();
        let mut ev = journaled(dir.path());
        let v = run_journaled(&mut ev, "let y = sh { head -c 100 /dev/zero }\ny.stdout").unwrap();
        assert!(
            matches!(v, Value::Bytes(_)),
            "sub-cap output is fully resident, got {}",
            v.type_name()
        );
        assert_eq!(
            run_journaled(&mut ev, "y.stdout.len").unwrap(),
            Value::Int(100)
        );
        assert!(
            ev.session
                .journal
                .as_ref()
                .unwrap()
                .pins()
                .unwrap()
                .is_empty(),
            "no spill blob is pinned for a sub-cap capture"
        );
    }

    /// site/content/internals/language-conformance-contract.md in-language dispatch follow-up: a bare `val:blake3:<hash>`
    /// content ref *written as a value* (the short-ref `.ref` yields) is
    /// resolvable in-language — calling a method on it loads the bytes from the
    /// session CAS and dispatches on the resulting lazy `bytes`, so a recovered
    /// ref answers `.len`, materializes, and round-trips `.ref` exactly like the
    /// capture it came from. An unknown hash is a clean `not_found`, and an
    /// ordinary (non-ref) string still dispatches string methods unchanged.
    #[test]
    fn val_blake3_ref_string_dispatches_through_the_cas() {
        let dir = tempfile::tempdir().unwrap();
        let mut ev = journaled(dir.path());
        // Seed the session CAS with a known blob directly (no spill needed):
        // `record_output` writes the blake3-addressed blob and its `blob` row,
        // which is exactly what a spilled capture leaves behind.
        let content = b"hello, cas-backed world!\n".repeat(40); // 1000 bytes, valid UTF-8
        let hash = ev
            .session
            .journal
            .as_ref()
            .unwrap()
            .record_output(1, "value", &content)
            .unwrap();
        let reference = format!("val:blake3:{hash}");

        // `.len` answers the TRUE content length from the blob metadata — the
        // ref string is resolved to a lazy CAS-backed `bytes`, never measured as
        // a plain string (which would report the 75-odd characters of the ref).
        assert_eq!(
            run_journaled(&mut ev, &format!("\"{reference}\".len")).unwrap(),
            Value::Int(content.len() as i64)
        );
        // Materialization loads the exact bytes from the CAS.
        assert_eq!(
            run_journaled(&mut ev, &format!("\"{reference}\".load.len")).unwrap(),
            Value::Int(content.len() as i64)
        );
        assert_eq!(
            run_journaled(
                &mut ev,
                &format!("\"{reference}\".str().starts_with(\"hello\")")
            )
            .unwrap(),
            Value::Bool(true)
        );
        // `.ref` round-trips the recoverable handle unchanged.
        assert_eq!(
            run_journaled(&mut ev, &format!("\"{reference}\".ref")).unwrap(),
            Value::Str(reference.clone())
        );
        // An unknown hash is a clean `not_found`, not a wrong string-length.
        let unknown = format!("val:blake3:{}", "0".repeat(64));
        let err = run_journaled(&mut ev, &format!("\"{unknown}\".len")).unwrap_err();
        assert_eq!(err.code, "not_found", "unknown hash: {}", err.msg);
        // A non-ref string still dispatches string methods verbatim.
        assert_eq!(
            run_journaled(&mut ev, "\"hello\".len").unwrap(),
            Value::Int(5)
        );
    }

    /// The same ref grammar is inert without a journal/CAS: a `val:blake3:`
    /// string then errors clearly rather than silently measuring itself as a
    /// string (the corpus / `-c` path installs no journal, so this is the
    /// common case for a stray ref-shaped literal).
    #[test]
    fn val_blake3_ref_without_journal_errors_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let mut ev = Evaluator::new(dir.path().to_path_buf());
        let program =
            shoal_syntax::parse(&format!("\"val:blake3:{}\".len", "a".repeat(64))).unwrap();
        let err = ev.eval_program(&program).unwrap_err();
        assert_eq!(err.code, "not_found");
        assert!(err.msg.contains("journal/CAS"), "{}", err.msg);
    }
}
