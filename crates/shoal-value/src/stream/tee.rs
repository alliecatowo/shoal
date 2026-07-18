//! Lazy `.tee(n)` forks for live streams (site/content/internals/streams-channels.md): `n` handles share one
//! upstream source, each with its own **bounded** queue.
//!
//! Under the synchronous pull model there is no background pump: whichever
//! fork pulls next drives the shared source, takes the item itself, and
//! replays a copy into every sibling fork's queue — that is how "each
//! replaying every item to its own sink" is realized without a thread. A fork
//! that falls more than [`TEE_QUEUE_CAP`] items behind gets the standing
//! coalesce/drop discipline (site/content/internals/streams-channels.md): overflowed items are dropped and
//! summarized as a single `{dropped: n}` marker element, enqueued in order as
//! soon as its queue has room again (or yielded when the queue drains). This
//! keeps memory bounded per fork while never losing items *silently*.
//!
//! Bounded (finite) streams never come through here — `.tee` on them
//! materializes the stream once and gives each fork the full list (see
//! `methods/stream.rs`), which preserves exact whole-stream replay.

use super::{CallCtx, Pull, StreamGap, StreamGapReason, Upstream, VResult, Value};
use crate::{OpaqueHandling, RetainedLimits, retained_size};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Per-fork queue capacity. site/content/internals/streams-channels.md require every buffer to be
/// bounded but name no size for tee forks; this matches the live sources'
/// default buffer cap (shoal-eval's `streams.rs`).
pub(super) const TEE_QUEUE_CAP: usize = 64;
const TEE_QUEUE_MAX_RETAINED_BYTES: usize = 1024 * 1024;

/// One fork's pending items plus the count of items coalesced away while the
/// queue was full — owed to the fork as a single `{dropped: n}` marker.
#[derive(Default)]
struct ForkQueue {
    buf: VecDeque<(Value, usize)>,
    retained_bytes: usize,
    dropped: u64,
}

impl ForkQueue {
    /// Enqueue an item pulled by a sibling fork, honoring the bound: a full
    /// queue drops the item and counts it. An owed marker is enqueued first
    /// (keeping the marker *in order*, before any post-gap item) as soon as
    /// room appears.
    fn push(&mut self, v: &Value) {
        if self.dropped > 0 && self.buf.len() < TEE_QUEUE_CAP {
            let n = std::mem::take(&mut self.dropped);
            let marker = dropped_marker(n);
            if !self.try_push(&marker, TEE_QUEUE_CAP, TEE_QUEUE_MAX_RETAINED_BYTES) {
                self.dropped = n.saturating_add(1);
                return;
            }
        }
        if !self.try_push(v, TEE_QUEUE_CAP, TEE_QUEUE_MAX_RETAINED_BYTES) {
            self.dropped = self.dropped.saturating_add(1);
        }
    }

    fn try_push(&mut self, v: &Value, max_items: usize, max_bytes: usize) -> bool {
        if self.buf.len() >= max_items {
            return false;
        }
        let Ok(retained) = retained_size(
            v,
            RetainedLimits {
                max_bytes: max_bytes.saturating_sub(self.retained_bytes),
                max_depth: 64,
                max_nodes: 16_384,
                opaque: OpaqueHandling::Charge(256),
                allow_secret: true,
            },
        ) else {
            return false;
        };
        let Some(total) = self.retained_bytes.checked_add(retained) else {
            return false;
        };
        self.retained_bytes = total;
        self.buf.push_back((v.clone(), retained));
        true
    }

    /// Next queued element: a buffered item, or the owed `{dropped: n}` marker
    /// when the queue drained while drops were still pending.
    fn pop(&mut self) -> Option<Value> {
        if let Some((v, retained)) = self.buf.pop_front() {
            self.retained_bytes = self.retained_bytes.saturating_sub(retained);
            return Some(v);
        }
        (self.dropped > 0).then(|| dropped_marker(std::mem::take(&mut self.dropped)))
    }
}

/// The site/content/internals/streams-channels.md overflow marker element: `{dropped: n}`.
fn dropped_marker(n: u64) -> Value {
    StreamGap::new(StreamGapReason::TeeOverflow, n).into_value()
}

struct TeeShared {
    up: Box<dyn Upstream>,
    /// The shared source ended naturally; forks drain their queues, then End.
    done: bool,
    queues: Vec<ForkQueue>,
}

/// One fork of a live `.tee(n)`: an [`Upstream`] view over the shared state.
pub(super) struct TeeHandle {
    shared: Arc<Mutex<TeeShared>>,
    idx: usize,
}

/// Split an upstream into `n` independently-drivable [`TeeHandle`]s.
pub(super) fn fork(up: Box<dyn Upstream>, n: usize) -> Vec<TeeHandle> {
    let shared = Arc::new(Mutex::new(TeeShared {
        up,
        done: false,
        queues: (0..n).map(|_| ForkQueue::default()).collect(),
    }));
    (0..n)
        .map(|idx| TeeHandle {
            shared: Arc::clone(&shared),
            idx,
        })
        .collect()
}

impl Upstream for TeeHandle {
    fn pull(&mut self, ctx: &mut dyn CallCtx, t: Option<Duration>) -> VResult<Pull> {
        // `up.pull` runs arbitrary evaluator/source code while this lock is
        // held. After an unwind its cursor and side effects are unknowable, so
        // the entire tee is quarantined rather than replaying potentially
        // duplicated or skipped data from a poisoned guard.
        let mut g = self
            .shared
            .lock()
            .map_err(|_| super::stream_state_poisoned())?;
        // Items already replayed to this fork by a sibling's pulls come first.
        if let Some(v) = g.queues[self.idx].pop() {
            return Ok(Pull::Item(v));
        }
        if g.done {
            return Ok(Pull::End);
        }
        // Queue empty and source still live: this fork drives the shared
        // source and replays the item to every sibling's bounded queue.
        match g.up.pull(ctx, t)? {
            Pull::Item(v) => {
                let idx = self.idx;
                for (i, q) in g.queues.iter_mut().enumerate() {
                    if i != idx {
                        q.push(&v);
                    }
                }
                Ok(Pull::Item(v))
            }
            Pull::End => {
                g.done = true;
                Ok(Pull::End)
            }
            Pull::Timeout => Ok(Pull::Timeout),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Fs, StdFs, StreamVal};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::thread;

    struct C;
    impl CallCtx for C {
        fn call_closure(&mut self, _f: &Value, _args: Vec<Value>) -> VResult<Value> {
            unreachable!("poison test source never calls a closure")
        }

        fn buffer_stream(&mut self, _stream: StreamVal, _capacity: usize) -> VResult<StreamVal> {
            unreachable!("poison test source never buffers")
        }

        fn cwd(&self) -> PathBuf {
            std::env::temp_dir()
        }

        fn fs(&self) -> &dyn Fs {
            static STD: StdFs = StdFs;
            &STD
        }
    }

    struct BlockingPanic {
        locked: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    impl Upstream for BlockingPanic {
        fn pull(&mut self, _ctx: &mut dyn CallCtx, _t: Option<Duration>) -> VResult<Pull> {
            self.locked.send(()).expect("test coordinator remains live");
            self.release.recv().expect("test coordinator releases pull");
            panic!("inject panic while tee owns its shared stream state");
        }
    }

    #[test]
    fn upstream_panic_quarantines_tee_for_waiters_repeats_and_drop() {
        let (locked_tx, locked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut handles = fork(
            Box::new(BlockingPanic {
                locked: locked_tx,
                release: release_rx,
            }),
            4,
        );
        let mut poisoner = handles.remove(0);
        let mut waiter = handles.remove(0);
        let mut repeated = handles.remove(0);
        let cancelled = handles.remove(0);

        let poisoning_thread = thread::spawn(move || {
            assert!(catch_unwind(AssertUnwindSafe(|| poisoner.pull(&mut C, None))).is_err());
        });
        locked_rx.recv().expect("upstream reports holding tee lock");

        let (waiter_started_tx, waiter_started_rx) = mpsc::channel();
        let waiting_thread = thread::spawn(move || {
            waiter_started_tx
                .send(())
                .expect("test coordinator remains live");
            waiter.pull(&mut C, None)
        });
        waiter_started_rx
            .recv()
            .expect("waiter starts while lock is held");
        drop(cancelled);
        release_tx.send(()).expect("release poison injector");

        poisoning_thread
            .join()
            .expect("upstream panic must be contained by test");
        let expected = super::super::stream_state_poisoned();
        assert_eq!(
            waiting_thread.join().expect("waiter must not panic").err(),
            Some(expected.clone())
        );
        assert_eq!(repeated.pull(&mut C, None).err(), Some(expected.clone()));
        assert_eq!(repeated.pull(&mut C, None).err(), Some(expected));
    }

    #[test]
    fn production_tee_locking_has_no_panic_path() {
        let production = include_str!("tee.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("test module marker remains present");
        assert!(!production.contains(".lock().unwrap()"));
        assert!(!production.contains(".lock().expect("));
    }

    #[test]
    fn oversized_fork_item_becomes_an_explicit_gap_without_retention() {
        let mut queue = ForkQueue::default();
        queue.push(&Value::Str("x".repeat(TEE_QUEUE_MAX_RETAINED_BYTES + 1)));
        assert!(queue.buf.is_empty());
        assert_eq!(queue.retained_bytes, 0);
        assert_eq!(queue.dropped, 1);
        let marker = queue.pop().expect("an oversized item leaves a gap marker");
        match marker {
            Value::Record(record) => assert_eq!(record["dropped"], Value::Int(1)),
            other => panic!("expected stream gap, got {other:?}"),
        }
    }
}
