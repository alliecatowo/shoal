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
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Per-fork queue capacity. site/content/internals/streams-channels.md require every buffer to be
/// bounded but name no size for tee forks; this matches the live sources'
/// default buffer cap (shoal-eval's `streams.rs`).
pub(super) const TEE_QUEUE_CAP: usize = 64;

/// One fork's pending items plus the count of items coalesced away while the
/// queue was full — owed to the fork as a single `{dropped: n}` marker.
#[derive(Default)]
struct ForkQueue {
    buf: VecDeque<Value>,
    dropped: u64,
}

impl ForkQueue {
    /// Enqueue an item pulled by a sibling fork, honoring the bound: a full
    /// queue drops the item and counts it. An owed marker is enqueued first
    /// (keeping the marker *in order*, before any post-gap item) as soon as
    /// room appears.
    fn push(&mut self, v: Value) {
        if self.dropped > 0 && self.buf.len() < TEE_QUEUE_CAP {
            let n = std::mem::take(&mut self.dropped);
            self.buf.push_back(dropped_marker(n));
        }
        if self.buf.len() < TEE_QUEUE_CAP {
            self.buf.push_back(v);
        } else {
            self.dropped += 1;
        }
    }

    /// Next queued element: a buffered item, or the owed `{dropped: n}` marker
    /// when the queue drained while drops were still pending.
    fn pop(&mut self) -> Option<Value> {
        if let Some(v) = self.buf.pop_front() {
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
        let mut g = self.shared.lock().unwrap();
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
                        q.push(v.clone());
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
