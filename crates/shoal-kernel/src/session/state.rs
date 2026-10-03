//! Session-local health, transcript, stream-cursor, and output-undo state.

use super::*;

impl Session {
    pub(crate) fn touch(&self) {
        self.last_used_ns.store(now_ns(), Ordering::Relaxed);
    }

    pub(crate) fn last_used_ns(&self) -> i64 {
        self.last_used_ns.load(Ordering::Relaxed)
    }

    pub(crate) fn quarantine(&self) {
        self.quarantined.store(true, Ordering::SeqCst);
    }

    pub(crate) fn ensure_healthy(&self) -> Result<(), RpcError> {
        if self.quarantined.load(Ordering::SeqCst)
            || self.evaluator.is_poisoned()
            || self.transcript.is_poisoned()
            || self.out_entries.is_poisoned()
            || self.stream_cursors.is_poisoned()
            || self.blob_decompressions.is_poisoned()
        {
            self.quarantine();
            Err(self.quarantined_error())
        } else {
            Ok(())
        }
    }

    pub(crate) fn reserve_blob_decompression(
        &self,
        max: usize,
        window: std::time::Duration,
    ) -> Result<(), RpcError> {
        self.ensure_healthy()?;
        let now = Instant::now();
        let mut recent = self.blob_decompressions.lock().map_err(|poisoned| {
            drop(poisoned);
            self.quarantine();
            self.quarantined_error()
        })?;
        while recent
            .front()
            .is_some_and(|started| now.duration_since(*started) >= window)
        {
            recent.pop_front();
        }
        if recent.len() >= max {
            let retry_after = recent
                .front()
                .map(|started| {
                    started
                        .checked_add(window)
                        .unwrap_or(now)
                        .saturating_duration_since(now)
                        .as_millis() as u64
                })
                .unwrap_or(0);
            return Err(RpcError {
                code: QUOTA_EXCEEDED,
                message: "CAS decompression rate limit exceeded".into(),
                data: Some(json!({
                    "limit": "blob_decompressions_per_window",
                    "max": max,
                    "window_ms": window.as_millis() as u64,
                    "retry_after_ms": retry_after,
                    "owner": {"principal": &self.key.principal, "session": &self.key.name},
                })),
            });
        }
        recent.push_back(now);
        Ok(())
    }

    fn quarantined_error(&self) -> RpcError {
        RpcError {
            code: INTERNAL_ERROR,
            message: "session is quarantined after an internal state failure".into(),
            data: Some(json!({"session": self.id, "session_quarantined": true})),
        }
    }

    pub(crate) fn lock_evaluator(&self) -> Result<std::sync::MutexGuard<'_, Evaluator>, RpcError> {
        self.ensure_healthy()?;
        match self.evaluator.lock() {
            Ok(evaluator) => Ok(evaluator),
            Err(poisoned) => {
                drop(poisoned);
                self.quarantine();
                Err(self.quarantined_error())
            }
        }
    }

    pub(crate) fn lock_transcript(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<Ref, Value>>, RpcError> {
        self.ensure_healthy()?;
        match self.transcript.lock() {
            Ok(transcript) => Ok(transcript),
            Err(poisoned) => {
                drop(poisoned);
                self.quarantine();
                Err(self.quarantined_error())
            }
        }
    }

    fn lock_out_entries(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, VecDeque<Option<i64>>>, RpcError> {
        self.ensure_healthy()?;
        match self.out_entries.lock() {
            Ok(entries) => Ok(entries),
            Err(poisoned) => {
                drop(poisoned);
                self.quarantine();
                Err(self.quarantined_error())
            }
        }
    }

    fn lock_stream_cursors(
        &self,
    ) -> Result<
        std::sync::MutexGuard<'_, HashMap<StreamCursorRef, Arc<WireStreamCursorEntry>>>,
        RpcError,
    > {
        self.ensure_healthy()?;
        match self.stream_cursors.lock() {
            Ok(cursors) => Ok(cursors),
            Err(poisoned) => {
                drop(poisoned);
                self.quarantine();
                Err(self.quarantined_error())
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn insert_transcript(&self, value_ref: Ref, value: Value) {
        let _ = self.insert_transcript_checked(value_ref, value);
    }

    pub(crate) fn insert_transcript_checked(
        &self,
        value_ref: Ref,
        value: Value,
    ) -> Result<(), RpcError> {
        let mut transcript = self.lock_transcript()?;
        transcript.try_reserve(1).map_err(|error| RpcError {
            code: INTERNAL_ERROR,
            message: format!("cannot reserve session transcript entry: {error}"),
            data: Some(json!({"resource": "session_transcript"})),
        })?;
        Self::insert_transcript_retained(&mut transcript, value_ref, value);
        Ok(())
    }

    pub(crate) fn insert_transcript_retained(
        transcript: &mut HashMap<Ref, Value>,
        value_ref: Ref,
        value: Value,
    ) {
        let id = value_ref
            .0
            .split_once(':')
            .and_then(|(_, id)| id.parse::<u64>().ok());
        transcript.insert(value_ref, value);
        if let Some(id) = id
            && id > MAX_TRANSCRIPT_PER_SESSION as u64
        {
            transcript.remove(&Ref::new("out", id - MAX_TRANSCRIPT_PER_SESSION as u64));
        }
    }

    pub(crate) fn rewrite_out_undo(&self, program: &mut Program) {
        if let Ok(mut entries) = self.lock_out_entries() {
            resolve_out_undo(program, entries.make_contiguous());
        }
    }

    pub(crate) fn push_out_entry(&self, entry_id: Option<i64>) {
        let Ok(mut entries) = self.lock_out_entries() else {
            return;
        };
        if entries.len() >= shoal_eval::MAX_REPL_TRANSCRIPT_VALUES {
            entries.pop_front();
        }
        entries.push_back(entry_id);
    }

    /// Get or lazily claim a transcript stream's single-consumer upstream.
    /// Cursor creation is serialized under the registry lock, so concurrent
    /// first pulls cannot both consume the same `StreamVal`.
    pub(crate) fn stream_cursor(
        &self,
        cursor: &StreamCursorRef,
    ) -> Result<Arc<WireStreamCursorEntry>, RpcError> {
        let mut cursors = self.lock_stream_cursors()?;
        if let Some(entry) = cursors.get(cursor) {
            return Ok(entry.clone());
        }

        // Terminal cursors retain no upstream resources. Reap them at the
        // admission boundary so clients do not need to close after observing
        // `done:true` merely to make quota progress.
        if cursors.len() >= MAX_WIRE_STREAM_CURSORS {
            cursors.retain(|_, entry| match entry.inner.lock() {
                Ok(cursor) => !cursor.done,
                Err(poisoned) => {
                    drop(poisoned);
                    entry.quarantine();
                    false
                }
            });
        }
        if cursors.len() >= MAX_WIRE_STREAM_CURSORS {
            return Err(RpcError {
                code: QUOTA_EXCEEDED,
                message: "live stream cursor quota reached".into(),
                data: Some(json!({
                    "limit": "stream_cursors_per_session",
                    "max": MAX_WIRE_STREAM_CURSORS,
                })),
            });
        }

        let stream = self.resolve_stream_value(cursor)?;
        let upstream = stream.take_upstream().map_err(stream_error)?;
        let entry = Arc::new(WireStreamCursorEntry {
            cancel: shoal_exec::CancelToken::new(),
            quarantined: AtomicBool::new(false),
            inner: Mutex::new(WireStreamCursor {
                upstream: Some(upstream),
                next_seq: 0,
                done: false,
            }),
        });
        cursors.insert(cursor.clone(), entry.clone());
        Ok(entry)
    }

    /// Explicitly release a cursor. If it has never been pulled, claim and
    /// immediately drop its upstream so source threads/resources are closed
    /// and later pulls correctly observe single consumption.
    pub(crate) fn close_stream_cursor(&self, cursor: &StreamCursorRef) -> Result<bool, RpcError> {
        if let Some(entry) = self.lock_stream_cursors()?.remove(cursor) {
            // Never wait for an in-process upstream while serving close. A
            // cooperative worker observes cancellation; a non-cooperative
            // trusted extension retains this detached Arc only until its
            // globally-leased worker eventually returns.
            entry.quarantine();
            return Ok(true);
        }
        let stream = self.resolve_stream_value(cursor)?;
        match stream.take_upstream() {
            Ok(upstream) => {
                drop(upstream);
                Ok(true)
            }
            Err(error) if error.code == "stream_consumed" => Ok(false),
            Err(error) => Err(stream_error(error)),
        }
    }

    pub(crate) fn quarantine_stream_cursor(
        &self,
        cursor: &StreamCursorRef,
        observed: &Arc<WireStreamCursorEntry>,
    ) {
        observed.quarantine();
        if let Ok(mut cursor) = observed.inner.try_lock() {
            cursor.done = true;
            cursor.upstream.take();
        }
        let removed = {
            let Ok(mut cursors) = self.lock_stream_cursors() else {
                return;
            };
            cursors
                .get(cursor)
                .is_some_and(|current| Arc::ptr_eq(current, observed))
                .then(|| cursors.remove(cursor))
                .flatten()
        };
        drop(removed);
    }

    fn resolve_stream_value(
        &self,
        cursor: &StreamCursorRef,
    ) -> Result<shoal_value::StreamVal, RpcError> {
        let transcript = self.lock_transcript()?;
        let root = transcript.get(&cursor.r#ref).ok_or_else(unknown_stream)?;
        let value = match cursor.path.as_deref() {
            Some(path) if !path.is_empty() => {
                resolve_value_path(root, path).map_err(|message| RpcError {
                    code: BAD_PATH_OR_SLICE,
                    message,
                    data: Some(json!({"ref":cursor.r#ref,"path":path})),
                })?
            }
            _ => root.clone(),
        };
        match value {
            Value::Stream(stream) => Ok(stream),
            other => Err(RpcError {
                code: BAD_PATH_OR_SLICE,
                message: format!("stream cursor addresses a {}", other.type_name()),
                data: Some(json!({"ref":cursor.r#ref,"path":cursor.path})),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn has_stream_cursor(&self, cursor: &StreamCursorRef) -> bool {
        self.lock_stream_cursors()
            .is_ok_and(|cursors| cursors.contains_key(cursor))
    }
}

fn unknown_stream() -> RpcError {
    RpcError {
        code: UNKNOWN_REF,
        message: "unknown stream cursor".into(),
        data: None,
    }
}

pub(crate) fn cursor_quarantined() -> RpcError {
    RpcError {
        code: INTERNAL_ERROR,
        message: "stream cursor is quarantined after an internal state failure".into(),
        data: Some(json!({"stream_cursor_quarantined": true})),
    }
}

fn stream_error(error: shoal_value::ErrorVal) -> RpcError {
    RpcError {
        code: RAISED,
        message: error.msg.clone(),
        data: Some(json!({
            "code": error.code,
            "span": error.span,
            "hint": error.hint,
            "status": error.status,
            "stderr": error.stderr,
        })),
    }
}

fn resolve_out_undo(program: &mut Program, out_entries: &[Option<i64>]) {
    for stmt in &mut program.stmts {
        let Stmt::Expr {
            expr: Expr::Cmd { call, .. },
            ..
        } = stmt
        else {
            continue;
        };
        if call.head != "undo" || call.args.len() != 1 {
            continue;
        }
        let Some(index) = out_index_literal(&call.args[0]) else {
            continue;
        };
        let resolved = if index >= 0 {
            usize::try_from(index).ok()
        } else {
            index
                .checked_abs()
                .and_then(|distance| usize::try_from(distance).ok())
                .and_then(|distance| out_entries.len().checked_sub(distance))
        };
        let Some(Some(entry_id)) = resolved.and_then(|index| out_entries.get(index)) else {
            continue;
        };
        let span = call.args[0].span();
        call.args[0] = CmdArg::Expr {
            expr: Expr::Int {
                value: *entry_id,
                span,
            },
            span,
        };
    }
}

fn out_index_literal(arg: &CmdArg) -> Option<i64> {
    let CmdArg::Expr {
        expr: Expr::Index { recv, index, .. },
        ..
    } = arg
    else {
        return None;
    };
    let Expr::Var { name, .. } = recv.as_ref() else {
        return None;
    };
    if name != "out" {
        return None;
    }
    match index.as_ref() {
        Expr::Int { value, .. } => Some(*value),
        Expr::Unary {
            op: UnOp::Neg,
            expr,
            ..
        } => match expr.as_ref() {
            Expr::Int { value, .. } => value.checked_neg(),
            _ => None,
        },
        _ => None,
    }
}
