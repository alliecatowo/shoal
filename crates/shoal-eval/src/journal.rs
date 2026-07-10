//! Journal integration for the tree-walk evaluator (TDD §9, docs/VISION.md).
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
use shoal_journal::{
    EntryRecord, FileFingerprint, Journal, JournalQuery, UndoError, UndoInverse, UndoReport,
    UndoStatus,
};
use std::os::unix::ffi::OsStrExt;
use std::time::Instant;

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

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as i64
}

fn elapsed_ns(start: Instant) -> i64 {
    start.elapsed().as_nanos().min(i64::MAX as u128) as i64
}

/// The default per-user state dir the journal lives in, mirroring the kernel's
/// `state_dir()` exactly so the REPL and kernel agree on one journal on disk.
fn default_state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("shoal")
}

impl Evaluator {
    /// Install a command journal and the session/principal recorded on each
    /// entry (TDD §9). Additive: without this call `journal` stays `None` and
    /// nothing is ever recorded.
    pub fn set_journal(
        &mut self,
        journal: Journal,
        session: impl Into<String>,
        principal: impl Into<String>,
    ) {
        self.journal = Some(journal);
        self.session_id = session.into();
        self.principal = principal.into();
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
    /// top-level statement's `src` can be sliced from it for the journal.
    pub fn set_source(&mut self, src: impl Into<String>) {
        self.source = Some(src.into());
    }

    /// Whether a journal is installed (for hosts/tests).
    pub fn has_journal(&self) -> bool {
        self.journal.is_some()
    }

    // --- per-statement recording ------------------------------------------

    /// Append a journal entry for `stmt` and mark it current, returning the
    /// entry id + start instant to finish it later. `None` (no journal) makes
    /// the whole statement-recording path a no-op.
    pub(crate) fn journal_begin_stmt(&mut self, stmt: &Stmt) -> Option<(i64, Instant)> {
        // Cheap gate: nothing to record without a journal (scripts/-c/tests).
        if !self.has_journal() {
            return None;
        }
        let src = self.stmt_source(stmt);
        let ast_json = serde_json::to_string(stmt).unwrap_or_default();
        let (effects_json, opaque) = self.stmt_effects(stmt);
        let record = EntryRecord {
            session: self.session_id.clone(),
            principal: self.principal.clone(),
            ts_ns: now_ns(),
            cwd: self.cwd.as_os_str().as_bytes().to_vec(),
            src,
            ast_json,
            effects_json,
            opaque,
        };
        let id = self.journal.as_ref()?.append(&record).ok()?;
        self.current_entry = Some(id);
        Some((id, Instant::now()))
    }

    /// Finish the entry opened by [`Evaluator::journal_begin_stmt`]: record the
    /// success verdict/status/duration and capture outputs (rendered value +
    /// stdout/stderr, or an error's stderr). Always clears `current_entry`.
    pub(crate) fn journal_finish_stmt(
        &mut self,
        opened: Option<(i64, Instant)>,
        result: &VResult<Flow>,
    ) {
        let Some((id, start)) = opened else {
            return;
        };
        self.current_entry = None;
        let Some(journal) = self.journal.as_ref() else {
            return;
        };
        let dur = elapsed_ns(start);
        match result {
            Ok(flow) => {
                let value = match flow {
                    Flow::Value(v) | Flow::Return(v) => Some(v),
                    _ => None,
                };
                let (ok, status) = match value {
                    Some(Value::Outcome(o)) => (o.ok, o.status),
                    _ => (true, Some(0)),
                };
                let _ = journal.finish(id, status, ok, dur);
                if let Some(v) = value
                    && *v != Value::Null
                {
                    let render = shoal_value::render::render_block(v, 80);
                    if !render.is_empty() {
                        let _ = journal.record_output(id, "render", render.as_bytes());
                    }
                    if let Value::Outcome(o) = v {
                        if !o.stdout.is_empty() {
                            let _ = journal.record_output(id, "stdout", &o.stdout);
                        }
                        if !o.stderr.is_empty() {
                            let _ = journal.record_output(id, "stderr", &o.stderr);
                        }
                    }
                }
            }
            Err(err) => {
                let _ = journal.finish(id, err.status, false, dur);
                if let Some(stderr) = &err.stderr {
                    let _ = journal.record_output(id, "stderr", stderr.as_bytes());
                }
            }
        }
    }

    /// Slice the statement's source text from the program source, if provided.
    fn stmt_source(&self, stmt: &Stmt) -> String {
        let Some(src) = &self.source else {
            return String::new();
        };
        let span = stmt.span();
        src.get(span.start as usize..span.end as usize)
            .unwrap_or("")
            .to_string()
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
                let json = serde_json::to_string(&plan.effects).unwrap_or_else(|_| "[]".into());
                (json, opaque)
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
        let Some(entry) = self.current_entry else {
            return Vec::new();
        };
        if self.journal.is_none() || !matches!(head, "cp" | "mv") {
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
            let target = if dest.is_dir() {
                match src.file_name() {
                    Some(name) => dest.join(name),
                    None => continue,
                }
            } else {
                dest.clone()
            };
            if target.is_file()
                && let Some(hash) = self.snapshot_prior(entry, &target)
            {
                out.push(FsUndoPre::Overwrite {
                    path: target,
                    prior_hash: hash,
                });
            } else if head == "mv" && !target.exists() {
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
        let Some(entry) = self.current_entry else {
            return;
        };
        let Some(journal) = self.journal.as_ref() else {
            return;
        };
        if head == "rm" {
            record_trash_inverses(journal, entry, result);
            return;
        }
        for item in pre {
            match item {
                FsUndoPre::Overwrite { path, prior_hash } => {
                    if let Ok(fp) = FileFingerprint::capture(&path) {
                        let _ = journal.record_undo_inverse(
                            entry,
                            &UndoInverse::RestoreBytes {
                                path,
                                prior_hash,
                                expected_current: fp,
                            },
                        );
                    }
                }
                FsUndoPre::Moved { src, dest } => {
                    if let Ok(fp) = FileFingerprint::capture(&dest) {
                        let _ = journal.record_undo_inverse(
                            entry,
                            &UndoInverse::MoveBack {
                                from: dest,
                                to: src,
                                expected_from: fp,
                            },
                        );
                    }
                }
            }
        }
    }

    /// `save`-specific pre-capture: snapshot the prior bytes of `path` if it is
    /// an existing file under an active journal.
    pub(crate) fn save_undo_pre(&mut self, path: &Value) -> Option<FsUndoPre> {
        let entry = self.current_entry?;
        self.journal.as_ref()?;
        let target = self.value_to_path(path)?;
        if !target.is_file() {
            return None;
        }
        let hash = self.snapshot_prior(entry, &target)?;
        Some(FsUndoPre::Overwrite {
            path: target,
            prior_hash: hash,
        })
    }

    /// `save`-specific post-record: turn the snapshot into a restore inverse.
    pub(crate) fn save_undo_post(&mut self, pre: Option<FsUndoPre>) {
        let (Some(entry), Some(FsUndoPre::Overwrite { path, prior_hash })) =
            (self.current_entry, pre)
        else {
            return;
        };
        let Some(journal) = self.journal.as_ref() else {
            return;
        };
        if let Ok(fp) = FileFingerprint::capture(&path) {
            let _ = journal.record_undo_inverse(
                entry,
                &UndoInverse::RestoreBytes {
                    path,
                    prior_hash,
                    expected_current: fp,
                },
            );
        }
    }

    /// Read a file's current bytes and store them in the CAS, returning the
    /// blake3 hash to key an undo restore on. The output row keeps the blob
    /// referenced (safe from GC).
    fn snapshot_prior(&self, entry: i64, path: &Path) -> Option<String> {
        let bytes = std::fs::read(path).ok()?;
        self.journal
            .as_ref()?
            .record_output(entry, "undo-snapshot", &bytes)
            .ok()
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
            out.push(if p.is_absolute() { p } else { self.cwd.join(p) });
        }
        Some(out)
    }

    fn value_to_path(&self, v: &Value) -> Option<PathBuf> {
        let p = match v {
            Value::Path(p) => p.clone(),
            Value::Str(s) => PathBuf::from(s),
            _ => return None,
        };
        Some(if p.is_absolute() { p } else { self.cwd.join(p) })
    }

    // --- undo / journal builtins ------------------------------------------

    /// The `undo` builtin (TDD §9, §13.16). Bare `undo` reverses the most recent
    /// reversible journaled entry; `undo <id>` targets a specific entry. Replays
    /// the entry's typed inverses newest-first, refusing loudly if a target has
    /// changed since it was recorded.
    pub(crate) fn builtin_undo(&mut self, call: &CmdCall) -> VResult<Value> {
        if self.journal.is_none() {
            return Err(ErrorVal::new(
                "custom",
                "undo requires a journaled session; none is active",
            )
            .with_span(call.span));
        }
        let target = self.undo_target_id(call)?;
        let journal = self.journal.as_ref().expect("checked");
        let entry_id = match target {
            Some(id) => id,
            None => last_reversible_entry(journal).ok_or_else(|| {
                ErrorVal::new(
                    "custom",
                    "nothing to undo: no reversible entry in the journal",
                )
                .with_span(call.span)
            })?,
        };
        let root = self.cwd.clone();
        let report = journal.undo_entry(entry_id, &root).map_err(|e| {
            let code = match e {
                UndoError::Stale(_) => "stale_undo",
                _ => "custom",
            };
            ErrorVal::new(code, format!("undo of out:{entry_id} refused: {e}")).with_span(call.span)
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
        let Some(journal) = self.journal.as_ref() else {
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
fn record_trash_inverses(journal: &Journal, entry: i64, result: &Value) {
    let Value::List(rows) = result else {
        return;
    };
    for row in rows {
        let Value::Record(r) = row else { continue };
        let (Some(Value::Path(original)), Some(Value::Path(trash))) =
            (r.get("path"), r.get("trash"))
        else {
            continue;
        };
        if let Ok(fp) = FileFingerprint::capture(trash) {
            let _ = journal.record_undo_inverse(
                entry,
                &UndoInverse::TrashMove {
                    original: original.clone(),
                    trash: trash.clone(),
                    trash_fingerprint: fp,
                },
            );
        }
    }
}

/// The newest journal entry that has at least one recorded undo inverse.
fn last_reversible_entry(journal: &Journal) -> Option<i64> {
    let rows = journal
        .query(&JournalQuery {
            limit: 500,
            ..Default::default()
        })
        .ok()?;
    rows.into_iter()
        .find(|r| {
            journal
                .undos_for(r.id)
                .map(|u| !u.is_empty())
                .unwrap_or(false)
        })
        .map(|r| r.id)
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
    let head = e.src.split_whitespace().next().unwrap_or("").to_string();
    r.insert("src".into(), Value::Str(head));
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
    use shoal_journal::Journal;

    /// Build a journaled evaluator rooted at `cwd` with an in-memory journal.
    ///
    /// `cwd` is canonicalized first: `undo_entry` canonicalizes its `root`
    /// argument before checking that a recorded undo target is contained in
    /// it (`checked_target`), and on macOS a tempdir's path is a symlink
    /// alias (`/var/folders/...` -> `/private/var/folders/...`). Rooting the
    /// evaluator at the already-canonical path keeps every path it records
    /// (via `self.cwd.join(...)`) on the same prefix `undo_entry` compares
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
        let journal = ev.journal.as_ref().unwrap();
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

    #[test]
    fn rm_records_trash_undo_and_undo_restores() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("victim"), b"payload").unwrap();
        let mut ev = journaled(dir.path());
        run_journaled(&mut ev, "rm victim").unwrap();
        assert!(!dir.path().join("victim").exists(), "rm trashed the file");
        // A trash inverse was recorded.
        let rows = ev
            .journal
            .as_ref()
            .unwrap()
            .query(&JournalQuery::default())
            .unwrap();
        let undos = ev.journal.as_ref().unwrap().undos_for(rows[0].id).unwrap();
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
            .journal
            .as_ref()
            .unwrap()
            .query(&JournalQuery::default())
            .unwrap();
        let undos = ev.journal.as_ref().unwrap().undos_for(rows[0].id).unwrap();
        assert_eq!(undos[0].0, "restore_bytes");
        // `undo` brings back the prior contents.
        run_journaled(&mut ev, "undo").unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("f.txt")).unwrap(),
            b"original"
        );
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
        assert!(
            rows.iter()
                .any(|r| r.get("src") == Some(&Value::Str("echo".into())))
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
}
