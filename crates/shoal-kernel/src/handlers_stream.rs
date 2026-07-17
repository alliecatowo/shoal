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
        evaluator.reset_cancel();
        let entry = session.stream_cursor(&params.cursor)?;
        let mut cursor = entry.lock().unwrap();
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

    fn attached(kernel: &Arc<Kernel>) -> (Arc<Session>, Option<Attachment>) {
        let principal = "wire-stream-test".to_string();
        let session = kernel.session("wire-stream", &principal).unwrap();
        let attachment = Attachment {
            session: session.clone(),
            principal,
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
            let mut first = first.lock().unwrap();
            first.upstream.take();
            first.done = true;
        }
        drop(first);
        session
            .stream_cursor(&cursor_refs[MAX_WIRE_STREAM_CURSORS])
            .expect("terminal cursor should be reaped at admission");
    }
}
