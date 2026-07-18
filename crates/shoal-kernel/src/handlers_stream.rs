//! Bounded pull/close lifecycle for evaluator-backed wire streams.

use super::*;
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::time::Duration;

const DEFAULT_PULL_ITEMS: usize = 16;
const MAX_PULL_ITEMS: usize = 64;
const MAX_PULL_WAIT: Duration = Duration::from_secs(1);
const DEFAULT_PULL_DEADLINE: Duration = Duration::from_secs(1);
const MAX_PULL_DEADLINE: Duration = Duration::from_secs(30);
const MAX_WIRE_PULL_WORKERS: usize = 16;
static ACTIVE_WIRE_PULL_WORKERS: AtomicUsize = AtomicUsize::new(0);

struct PullWorkerLease;

impl Drop for PullWorkerLease {
    fn drop(&mut self) {
        ACTIVE_WIRE_PULL_WORKERS.fetch_sub(1, Ordering::Relaxed);
    }
}

fn acquire_pull_worker() -> Result<PullWorkerLease, RpcError> {
    ACTIVE_WIRE_PULL_WORKERS
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |active| {
            (active < MAX_WIRE_PULL_WORKERS).then_some(active + 1)
        })
        .map(|_| PullWorkerLease)
        .map_err(|_| RpcError {
            code: QUOTA_EXCEEDED,
            message: "wire stream pull worker quota reached".into(),
            data: Some(json!({
                "limit": "wire_stream_pull_workers_global",
                "max": MAX_WIRE_PULL_WORKERS,
            })),
        })
}

struct PulledBatch {
    values: Vec<(u64, Value)>,
    done: bool,
    timed_out: bool,
    error: Option<WireValue>,
}

impl Kernel {
    pub(crate) fn handle_stream_pull(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = attachment.session.clone();
        let params: StreamPullParams = decode(params)?;
        let limit = params
            .limit
            .unwrap_or(DEFAULT_PULL_ITEMS)
            .clamp(1, MAX_PULL_ITEMS);
        let wait = Duration::from_millis(params.wait_ms.unwrap_or(0)).min(MAX_PULL_WAIT);
        let hard_deadline = params.deadline_ms.map_or(DEFAULT_PULL_DEADLINE, |ms| {
            Duration::from_millis(ms.max(1)).min(MAX_PULL_DEADLINE)
        });
        let lease = acquire_pull_worker()?;
        let entry = session.stream_cursor(&params.cursor)?;
        let worker_session = session.clone();
        let worker_entry = entry.clone();
        let (tx, rx) = sync_channel(1);
        std::thread::Builder::new()
            .name("shoal-wire-stream-pull".into())
            .spawn(move || {
                let _lease = lease;
                let batch = drive_cursor(worker_session, worker_entry, limit, wait);
                let _ = tx.send(batch);
            })
            .map_err(internal)?;

        let deadline = Instant::now() + hard_deadline;
        let batch = loop {
            if entry.quarantined.load(Ordering::SeqCst) || entry.cancel.is_cancelled() {
                session.quarantine_stream_cursor(&params.cursor, &entry);
                break deadline_batch("stream_closed", "stream cursor was closed");
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                session.quarantine_stream_cursor(&params.cursor, &entry);
                break deadline_batch(
                    "stream_pull_deadline",
                    "stream pull exceeded its execution deadline",
                );
            }
            match rx.recv_timeout(remaining.min(Duration::from_millis(25))) {
                Ok(Ok(batch)) => break batch,
                Ok(Err(error)) => {
                    session.quarantine_stream_cursor(&params.cursor, &entry);
                    return Err(error);
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    session.quarantine_stream_cursor(&params.cursor, &entry);
                    return Err(internal("stream pull worker exited without a result"));
                }
            }
        };

        // Every item receives its own transcript ref. This is essential for a
        // large/elided item: its returned ref remains fetchable by value.get
        // instead of pointing into an already-advanced ephemeral cursor.
        let budget = ElideBudget::from_spec(params.elide.as_ref());
        let mut items = Vec::with_capacity(batch.values.len());
        for (seq, value) in batch.values {
            let value_ref = Ref::new("out", session.next_value.fetch_add(1, Ordering::Relaxed));
            session.insert_transcript_checked(value_ref.clone(), value.clone())?;
            let uri = short_ref_to_uri(&value_ref, None);
            items.push(StreamItem {
                seq,
                r#ref: value_ref,
                value: elide_wire_value(&value, &uri, &budget),
            });
        }

        encode(StreamPullResult {
            cursor: params.cursor,
            items,
            done: batch.done,
            timed_out: batch.timed_out,
            error: batch.error,
        })
    }

    pub(crate) fn handle_stream_close(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let params: StreamCloseParams = decode(params)?;
        let closed = attachment.session.close_stream_cursor(&params.cursor)?;
        encode(json!({"cursor": params.cursor, "closed": closed}))
    }
}

fn drive_cursor(
    session: Arc<Session>,
    entry: Arc<WireStreamCursorEntry>,
    limit: usize,
    wait: Duration,
) -> Result<PulledBatch, RpcError> {
    if entry.quarantined.load(Ordering::SeqCst) {
        return Ok(deadline_batch("stream_closed", "stream cursor was closed"));
    }
    // This worker boundary is the hard containment available in-process. Shoal
    // closures/builtins/processes cooperate with `CancelToken`; arbitrary host
    // extensions are trusted code and require process isolation for hard kill.
    let mut evaluator = session.lock_evaluator()?;
    evaluator.set_cancellation_token(entry.cancel.clone());
    let mut cursor = entry.lock_cursor()?;
    if cursor.done {
        return Ok(PulledBatch {
            values: Vec::new(),
            done: true,
            timed_out: false,
            error: None,
        });
    }
    let deadline = Instant::now() + wait;
    let mut values = Vec::with_capacity(limit);
    let mut timed_out = false;
    let mut error = None;
    for _ in 0..limit {
        if entry.quarantined.load(Ordering::SeqCst) || entry.cancel.is_cancelled() {
            break;
        }
        let Some(upstream) = cursor.upstream.as_mut() else {
            entry.quarantine();
            return Err(cursor_quarantined());
        };
        let result = upstream.pull(
            &mut *evaluator,
            Some(deadline.saturating_duration_since(Instant::now())),
        );
        match result {
            Ok(shoal_value::Pull::Item(value)) => {
                let seq = cursor.next_seq;
                cursor.next_seq = cursor.next_seq.saturating_add(1);
                values.push((seq, value));
            }
            Ok(shoal_value::Pull::Timeout) => {
                timed_out = true;
                break;
            }
            Ok(shoal_value::Pull::End) => {
                cursor.done = true;
                cursor.upstream.take();
                break;
            }
            Err(source_error) => {
                error = Some(wire_value(&Value::Error(Arc::new(source_error))));
                cursor.done = true;
                cursor.upstream.take();
                break;
            }
        }
    }
    if entry.quarantined.load(Ordering::SeqCst) || entry.cancel.is_cancelled() {
        values.clear();
        timed_out = false;
        error = None;
        cursor.done = true;
        cursor.upstream.take();
    }
    Ok(PulledBatch {
        values,
        done: cursor.done,
        timed_out,
        error,
    })
}

fn deadline_batch(code: &str, message: &str) -> PulledBatch {
    PulledBatch {
        values: Vec::new(),
        done: true,
        timed_out: code == "stream_pull_deadline",
        error: Some(wire_value(&Value::Error(Arc::new(
            shoal_value::ErrorVal::new(code, message),
        )))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    fn attached(kernel: &Arc<Kernel>) -> (Arc<Session>, Option<Attachment>) {
        attached_for(kernel, "wire-stream-test", "wire-stream")
    }

    fn attached_for(
        kernel: &Arc<Kernel>,
        principal: &str,
        name: &str,
    ) -> (Arc<Session>, Option<Attachment>) {
        let session = kernel.session(name, principal).unwrap();
        let attachment = Attachment {
            session: session.clone(),
            principal: principal.to_string(),
            can_approve: true,
            tty: false,
            cancel_epoch: None,
            bearer: None,
            security_epoch: ATTACH_SECURITY_EPOCH,
            connection_trust: ConnectionTrust::EmbeddedHuman,
        };
        (session, Some(attachment))
    }

    fn exec_stream(
        kernel: &Arc<Kernel>,
        attached: &mut Option<Attachment>,
        source: &str,
    ) -> StreamCursorRef {
        let response = kernel
            .handle_exec(json!({"src":source,"position":"value"}), 1, attached)
            .unwrap();
        serde_json::from_value(response["value"]["cursor"].clone()).unwrap()
    }

    #[test]
    fn pulls_bounded_addressable_batches_and_remembers_done() {
        let kernel = Kernel::with_policy(Policy::permissive("wire-stream-test"));
        let (session, mut attached) = attached(&kernel);
        let cursor = exec_stream(&kernel, &mut attached, "[1, 2, 3].stream()");

        let first = kernel
            .handle_stream_pull(json!({"cursor":cursor,"limit":2}), &mut attached)
            .unwrap();
        assert_eq!(first["items"].as_array().unwrap().len(), 2);
        assert_eq!(first["items"][0]["seq"], 0);
        assert_eq!(first["items"][0]["value"]["v"], 1);
        assert_eq!(first["done"], false);
        let item_ref: Ref = serde_json::from_value(first["items"][0]["ref"].clone()).unwrap();
        assert!(
            session
                .transcript
                .lock()
                .expect("test lock should not be poisoned")
                .contains_key(&item_ref)
        );

        let second = kernel
            .handle_stream_pull(json!({"cursor":cursor,"limit":64}), &mut attached)
            .unwrap();
        assert_eq!(second["items"].as_array().unwrap().len(), 1);
        assert_eq!(second["items"][0]["seq"], 2);
        assert_eq!(second["done"], true);

        let after_end = kernel
            .handle_stream_pull(json!({"cursor":cursor}), &mut attached)
            .unwrap();
        assert_eq!(after_end["items"], json!([]));
        assert_eq!(after_end["done"], true);
    }

    #[test]
    fn close_drops_an_unpulled_live_stream_and_is_idempotent() {
        let kernel = Kernel::with_policy(Policy::permissive("wire-stream-test"));
        let (_, mut attached) = attached(&kernel);
        let cursor = exec_stream(&kernel, &mut attached, "every(1s)");

        let closed = kernel
            .handle_stream_close(json!({"cursor":cursor}), &mut attached)
            .unwrap();
        assert_eq!(closed["closed"], true);
        let closed_again = kernel
            .handle_stream_close(json!({"cursor":cursor}), &mut attached)
            .unwrap();
        assert_eq!(closed_again["closed"], false);
    }

    #[test]
    fn a_terminal_stream_error_keeps_earlier_items_in_the_batch() {
        let kernel = Kernel::with_policy(Policy::permissive("wire-stream-test"));
        let (_, mut attached) = attached(&kernel);
        let cursor = exec_stream(&kernel, &mut attached, "[1, 0].stream().map(x => 1 / x)");

        let pulled = kernel
            .handle_stream_pull(json!({"cursor":cursor,"limit":16}), &mut attached)
            .unwrap();
        assert_eq!(pulled["items"].as_array().unwrap().len(), 1);
        assert_eq!(pulled["items"][0]["value"]["v"], 1);
        assert_eq!(pulled["done"], true);
        assert_eq!(pulled["error"]["$"], "error");
        assert_eq!(pulled["error"]["code"], "div_zero");
    }

    #[test]
    fn cursor_admission_is_bounded_and_reaps_terminal_entries() {
        let kernel = Kernel::new();
        let (session, _) = attached(&kernel);
        let mut cursor_refs = Vec::new();
        for id in 0..=MAX_WIRE_STREAM_CURSORS {
            let value_ref = Ref::new("out", id);
            session.insert_transcript(
                value_ref.clone(),
                Value::Stream(shoal_value::StreamVal::from_iter(
                    "int",
                    std::iter::once(Ok(Value::Int(id as i64))),
                )),
            );
            cursor_refs.push(StreamCursorRef {
                r#ref: value_ref,
                path: None,
            });
        }

        for cursor in cursor_refs.iter().take(MAX_WIRE_STREAM_CURSORS) {
            session.stream_cursor(cursor).unwrap();
        }
        let error = match session.stream_cursor(&cursor_refs[MAX_WIRE_STREAM_CURSORS]) {
            Ok(_) => panic!("cursor quota must fail closed"),
            Err(error) => error,
        };
        assert_eq!(error.code, QUOTA_EXCEEDED);

        let first = session.stream_cursor(&cursor_refs[0]).unwrap();
        {
            let mut first = first
                .inner
                .lock()
                .expect("test lock should not be poisoned");
            first.upstream.take();
            first.done = true;
        }
        drop(first);
        session
            .stream_cursor(&cursor_refs[MAX_WIRE_STREAM_CURSORS])
            .expect("terminal cursor should be reaped at admission");
    }

    #[test]
    fn poisoned_cursor_inner_quarantines_only_that_cursor() {
        let kernel = Kernel::with_policy(Policy::permissive("wire-stream-test"));
        let (session, mut attached) = attached(&kernel);
        let cursor = exec_stream(&kernel, &mut attached, "[1, 2].stream()");
        let entry = session.stream_cursor(&cursor).unwrap();
        let poisoner = entry.clone();
        let thread = std::thread::spawn(move || {
            let _cursor = poisoner
                .inner
                .lock()
                .expect("test lock should not be poisoned");
            panic!("inject cursor-inner poison");
        });
        assert!(thread.join().is_err());

        let error = kernel
            .handle_stream_pull(json!({"cursor":cursor}), &mut attached)
            .expect_err("poisoned cursor must fail closed");
        assert_eq!(error.code, INTERNAL_ERROR);
        assert_eq!(error.data.unwrap()["stream_cursor_quarantined"], true);
        assert!(session.ensure_healthy().is_ok());
        assert!(!session.has_stream_cursor(&cursor));

        let result = kernel
            .handle_exec(json!({"src":"1 + 1","position":"value"}), 1, &mut attached)
            .expect("the cursor poison must not quarantine its session");
        assert_eq!(result["value"]["v"], 2);
    }

    #[test]
    fn close_cancels_a_blocked_closure_pull_without_a_trailing_item() {
        let kernel = Kernel::with_policy(Policy::permissive("wire-stream-test"));
        let (session, mut attached) = attached(&kernel);
        let cursor = exec_stream(&kernel, &mut attached, "[1].stream().map(x => sleep(30s))");
        let pull_cursor = cursor.clone();
        let pull_kernel = kernel.clone();
        let pull = std::thread::spawn(move || {
            pull_kernel.handle_stream_pull(json!({"cursor":pull_cursor,"limit":1}), &mut attached)
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while !session.has_stream_cursor(&cursor) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            session.has_stream_cursor(&cursor),
            "pull never claimed cursor"
        );
        let started = Instant::now();
        assert!(session.close_stream_cursor(&cursor).unwrap());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "close must cancel closure work before waiting for the cursor lock"
        );

        let response = pull.join().unwrap().unwrap();
        assert_eq!(response["items"], json!([]));
        assert_eq!(response["done"], true);
    }

    #[test]
    fn cursor_refs_are_private_to_the_exact_principal_session() {
        let kernel = Kernel::new();
        let (owner, _) = attached_for(&kernel, "owner-a", "shared-name");
        let cursor = StreamCursorRef {
            r#ref: Ref::new("out", 1),
            path: None,
        };
        owner.insert_transcript(
            cursor.r#ref.clone(),
            Value::Stream(shoal_value::StreamVal::from_iter(
                "int",
                std::iter::once(Ok(Value::Int(1))),
            )),
        );
        let (_, mut intruder) = attached_for(&kernel, "owner-b", "shared-name");

        let error = kernel
            .handle_stream_pull(json!({"cursor":cursor}), &mut intruder)
            .unwrap_err();
        assert_eq!(error.code, UNKNOWN_REF);
    }

    #[test]
    fn cursor_lifetime_is_session_owned_across_attachment_disconnect() {
        let kernel = Kernel::new();
        let (session, first_attachment) = attached_for(&kernel, "owner-a", "reconnect");
        let value_ref = Ref::new("out", 1);
        let cursor = StreamCursorRef {
            r#ref: value_ref.clone(),
            path: None,
        };
        session.insert_transcript(
            value_ref,
            Value::Stream(shoal_value::StreamVal::from_iter(
                "int",
                std::iter::once(Ok(Value::Int(7))),
            )),
        );
        drop(first_attachment);
        drop(session);

        let (_, mut reattached) = attached_for(&kernel, "owner-a", "reconnect");
        let pulled = kernel
            .handle_stream_pull(json!({"cursor":cursor}), &mut reattached)
            .unwrap();
        assert_eq!(pulled["items"][0]["value"]["v"], 7);
    }

    struct DropSource(Arc<AtomicBool>);

    impl shoal_value::Upstream for DropSource {
        fn pull(
            &mut self,
            _ctx: &mut dyn shoal_value::CallCtx,
            _timeout: Option<Duration>,
        ) -> shoal_value::VResult<shoal_value::Pull> {
            Ok(shoal_value::Pull::Timeout)
        }
    }

    impl Drop for DropSource {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct NonCooperativeSource {
        dropped: Arc<AtomicBool>,
        rendezvous: Arc<std::sync::Barrier>,
    }

    impl shoal_value::Upstream for NonCooperativeSource {
        fn pull(
            &mut self,
            _ctx: &mut dyn shoal_value::CallCtx,
            _timeout: Option<Duration>,
        ) -> shoal_value::VResult<shoal_value::Pull> {
            // Tell the test the source is inside `pull`, then refuse to return
            // until the test explicitly releases us after the RPC response.
            // This proves deadline detachment by ordering, not runner timing.
            self.rendezvous.wait();
            self.rendezvous.wait();
            Ok(shoal_value::Pull::Item(Value::Int(1)))
        }
    }

    impl Drop for NonCooperativeSource {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn hard_deadline_detaches_a_non_cooperative_in_process_source() {
        let kernel = Kernel::new();
        let (session, attached) = attached(&kernel);
        let dropped = Arc::new(AtomicBool::new(false));
        let rendezvous = Arc::new(std::sync::Barrier::new(2));
        let value_ref = Ref::new("out", 90);
        let cursor = StreamCursorRef {
            r#ref: value_ref.clone(),
            path: None,
        };
        session.insert_transcript(
            value_ref,
            Value::Stream(shoal_value::StreamVal::from_upstream(
                "slow",
                false,
                Box::new(NonCooperativeSource {
                    dropped: dropped.clone(),
                    rendezvous: rendezvous.clone(),
                }),
            )),
        );

        let (response_tx, response_rx) = std::sync::mpsc::sync_channel(1);
        let request_kernel = kernel.clone();
        let request_cursor = cursor.clone();
        let request = std::thread::spawn(move || {
            let mut attached = attached;
            let response = request_kernel.handle_stream_pull(
                json!({"cursor":request_cursor,"deadline_ms":20}),
                &mut attached,
            );
            let _ = response_tx.send(response);
        });

        // The source is now blocked inside a pull that ignores cancellation.
        rendezvous.wait();
        let response = match response_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(response) => response.unwrap(),
            Err(error) => {
                // Always release and join the worker before reporting failure.
                rendezvous.wait();
                request.join().unwrap();
                panic!("deadline response waited for the blocked source: {error}");
            }
        };
        assert_eq!(response["done"], true);
        assert_eq!(response["timed_out"], true);
        assert_eq!(response["error"]["code"], "stream_pull_deadline");
        assert!(!session.has_stream_cursor(&cursor));

        // Only now may the detached source finish and be dropped.
        rendezvous.wait();
        request.join().unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        while !dropped.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            dropped.load(Ordering::SeqCst),
            "source was not dropped after the detached pull returned"
        );
    }

    #[test]
    fn session_eviction_drops_retained_cursor_upstreams() {
        let kernel = Kernel::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let session = kernel.session("oldest", "eviction-owner").unwrap();
        let value_ref = Ref::new("out", 1);
        let cursor = StreamCursorRef {
            r#ref: value_ref.clone(),
            path: None,
        };
        session.insert_transcript(
            value_ref,
            Value::Stream(shoal_value::StreamVal::from_upstream(
                "idle",
                false,
                Box::new(DropSource(dropped.clone())),
            )),
        );
        drop(session.stream_cursor(&cursor).unwrap());
        drop(session);
        std::thread::sleep(Duration::from_millis(1));

        for id in 0..MAX_SESSIONS_PER_PRINCIPAL {
            drop(
                kernel
                    .session(&format!("new-{id}"), "eviction-owner")
                    .unwrap(),
            );
        }
        assert!(
            dropped.load(Ordering::SeqCst),
            "evicting an unattached session must drop its live cursor sources"
        );
    }

    struct FixedLoader {
        bytes: Vec<u8>,
        loads: Arc<AtomicUsize>,
    }

    impl shoal_value::BytesLoad for FixedLoader {
        fn load(&self) -> std::io::Result<Vec<u8>> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            Ok(self.bytes.clone())
        }

        fn open(&self) -> std::io::Result<Box<dyn std::io::Read + Send>> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(std::io::Cursor::new(self.bytes.clone())))
        }
    }

    #[test]
    fn cas_stream_items_elide_without_loading_and_remain_fetchable() {
        let kernel = Kernel::new();
        let (session, mut attached) = attached(&kernel);
        let bytes = vec![b'x'; 32 * 1024];
        let loads = Arc::new(AtomicUsize::new(0));
        let cas = Value::CasBytes(Arc::new(shoal_value::CasBytesVal {
            hash: "a".repeat(64),
            len: bytes.len() as u64,
            preview: Arc::new(bytes[..64].to_vec()),
            truncated: false,
            loader: Arc::new(FixedLoader {
                bytes: bytes.clone(),
                loads: loads.clone(),
            }),
        }));
        let value_ref = Ref::new("out", 100);
        let cursor = StreamCursorRef {
            r#ref: value_ref.clone(),
            path: None,
        };
        session.insert_transcript(
            value_ref,
            Value::Stream(shoal_value::StreamVal::from_iter(
                "bytes",
                std::iter::once(Ok(cas)),
            )),
        );

        let pulled = kernel
            .handle_stream_pull(json!({"cursor":cursor,"limit":1}), &mut attached)
            .unwrap();
        assert_eq!(pulled["items"][0]["value"]["$"], "ref");
        assert_eq!(pulled["items"][0]["value"]["of"], "bytes");
        assert_eq!(loads.load(Ordering::SeqCst), 0);

        let item_ref = pulled["items"][0]["ref"].clone();
        let raw = kernel
            .handle_value_get(json!({"ref":item_ref,"format":"raw"}), &mut attached)
            .unwrap();
        let decoded = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            raw["raw_base64"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(decoded, bytes[..RAW_PAGE_MAX_BYTES]);
        assert_eq!(raw["page"]["next_offset"], RAW_PAGE_MAX_BYTES);
        assert_eq!(raw["page"]["done"], false);
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }
}
