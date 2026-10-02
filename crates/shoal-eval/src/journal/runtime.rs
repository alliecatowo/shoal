use super::*;

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
    /// evaluator reads through the filesystem port with a `MAX + 1` sentinel
    /// and refuses a sparse, growing, replaced, or otherwise unstable source.
    /// A file above the journal's configured `output_hard_cap` is also reported
    /// as truncated. Keying a replayable `RestoreBytes` inverse on any of those
    /// would let `undo` silently overwrite the user's file with partial or
    /// stale content. Refusing leaves the operation honestly non-reversible.
    fn snapshot_prior(&mut self, entry: i64, path: &Path) -> Option<String> {
        let bytes = match self
            .host
            .fs
            .read_bounded_stable(path, MAX_JOURNAL_UNDO_SNAPSHOT_BYTES)
        {
            Ok(bytes) => bytes,
            // Limit/change refusal is an honest "not reversible" result, like
            // the prior metadata-size preflight. It must not poison the
            // journal or turn the subsequent successful mutation into an
            // indeterminate persistence failure.
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => return None,
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
}
