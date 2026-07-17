//! Bounded pull/close lifecycle for evaluator-backed wire streams.

use super::*;
use std::time::Duration;

const DEFAULT_PULL_ITEMS: usize = 16;
const MAX_PULL_ITEMS: usize = 64;
const MAX_PULL_WAIT: Duration = Duration::from_secs(1);

impl Kernel {
    pub(crate) fn handle_stream_pull(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let session = &attachment.session;
        let params: StreamPullParams = decode(params)?;
        let limit = params
            .limit
            .unwrap_or(DEFAULT_PULL_ITEMS)
            .clamp(1, MAX_PULL_ITEMS);
        let wait = Duration::from_millis(params.wait_ms.unwrap_or(0)).min(MAX_PULL_WAIT);
        let deadline = Instant::now() + wait;

        // Pulling may execute map/where/scan closures, so it must own the same
        // evaluator context and serialization boundary as `exec`.
        let mut evaluator = session.evaluator.lock().unwrap();
        let entry = session.stream_cursor(&params.cursor)?;
        evaluator.set_cancellation_token(entry.cancel.clone());
        let mut cursor = entry.inner.lock().unwrap();
        if cursor.done {
            return encode(StreamPullResult {
                cursor: params.cursor,
                items: Vec::new(),
                done: true,
                timed_out: false,
                error: None,
            });
        }

        let mut pulled = Vec::with_capacity(limit);
        let mut timed_out = false;
        let mut terminal_error = None;
        for _ in 0..limit {
            let timeout = deadline.saturating_duration_since(Instant::now());
            let result = cursor
                .upstream
                .as_mut()
                .expect("non-terminal stream cursor retains its upstream")
                .pull(&mut *evaluator, Some(timeout));
            match result {
                Ok(shoal_value::Pull::Item(value)) => {
                    let seq = cursor.next_seq;
                    cursor.next_seq = cursor.next_seq.saturating_add(1);
                    pulled.push((seq, value));
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
                Err(error) => {
                    terminal_error = Some(wire_value(&Value::Error(Arc::new(error))));
                    cursor.done = true;
                    cursor.upstream.take();
                    break;
                }
            }
        }
        // `stream.close` cancels before waiting for this cursor lock. Do not
        // leak a value that completed only because cancellation interrupted a
        // closure (for example `sleep` or an external command), and make the
        // racing pull's terminal state agree with the close.
        if entry.cancel.is_cancelled() {
            pulled.clear();
            timed_out = false;
            terminal_error = None;
            cursor.done = true;
            cursor.upstream.take();
        }
        let done = cursor.done;
        drop(cursor);
        drop(evaluator);

        // Every item receives its own transcript ref. This is essential for a
        // large/elided item: its returned ref remains fetchable by value.get
        // instead of pointing into an already-advanced ephemeral cursor.
        let budget = ElideBudget::from_spec(params.elide.as_ref());
        let mut items = Vec::with_capacity(pulled.len());
        for (seq, value) in pulled {
            let value_ref = Ref::new("out", session.next_value.fetch_add(1, Ordering::Relaxed));
            session.insert_transcript(value_ref.clone(), value.clone());
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
            done,
            timed_out,
            error: terminal_error,
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
        assert!(session.transcript.lock().unwrap().contains_key(&item_ref));

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
            let mut first = first.inner.lock().unwrap();
            first.upstream.take();
            first.done = true;
        }
        drop(first);
        session
            .stream_cursor(&cursor_refs[MAX_WIRE_STREAM_CURSORS])
            .expect("terminal cursor should be reaped at admission");
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
        assert_eq!(decoded, bytes);
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }
}
