use super::*;

impl Evaluator {
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
