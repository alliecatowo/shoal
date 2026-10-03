//! Owned, lossless `.buffer(n)` producer pump.
//!
//! This module owns both count backpressure and retained-byte accounting. Live
//! lossy/coalescing source queues remain in the parent `streams` module because
//! their overflow contracts differ deliberately.

use super::{PUMP_POLL, acquire_stream_pump};
use crate::{ChildKind, Evaluator};
use shoal_exec::CancelToken;
use shoal_value::{
    CallCtx, ErrorVal, OpaqueHandling, Pull, RetainedLimits, StreamVal, Upstream, VResult, Value,
    retained_size,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::time::Duration;

const MAX_STREAM_BUFFER_CAPACITY: usize = 4_096;
const MAX_STREAM_BUFFER_RETAINED_BYTES: usize = 16 * 1024 * 1024;

struct BufferedDelivery {
    delivery: VResult<Value>,
    retained_bytes: usize,
}

struct OwnedBufferSource {
    rx: Receiver<BufferedDelivery>,
    retained_bytes: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    cancel: CancelToken,
}

impl OwnedBufferSource {
    fn receive(&self, delivery: BufferedDelivery) -> VResult<Pull> {
        if delivery.retained_bytes > 0 {
            self.retained_bytes
                .fetch_sub(delivery.retained_bytes, Ordering::SeqCst);
        }
        delivery.delivery.map(Pull::Item)
    }
}

impl Upstream for OwnedBufferSource {
    fn pull(&mut self, _ctx: &mut dyn CallCtx, timeout: Option<Duration>) -> VResult<Pull> {
        match timeout {
            None => match self.rx.recv() {
                Ok(delivery) => self.receive(delivery),
                Err(_) => Ok(Pull::End),
            },
            Some(duration) => match self.rx.recv_timeout(duration) {
                Ok(delivery) => self.receive(delivery),
                Err(RecvTimeoutError::Timeout) => Ok(Pull::Timeout),
                Err(RecvTimeoutError::Disconnected) => Ok(Pull::End),
            },
        }
    }
}

impl Drop for OwnedBufferSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.cancel.cancel();
    }
}

impl Evaluator {
    /// Move `stream` and a fully-owned child evaluator into a producer thread,
    /// decoupling upstream work from the downstream sink through exactly
    /// `capacity` slots. Pulls use short deadlines so dropping the receiver or
    /// cancelling the execution tears down even an idle live source promptly.
    pub(crate) fn spawn_stream_buffer(
        &mut self,
        stream: StreamVal,
        capacity: usize,
    ) -> VResult<StreamVal> {
        self.spawn_stream_buffer_with_limits(
            stream,
            capacity,
            MAX_STREAM_BUFFER_CAPACITY,
            MAX_STREAM_BUFFER_RETAINED_BYTES,
        )
    }

    pub(super) fn spawn_stream_buffer_with_limits(
        &mut self,
        stream: StreamVal,
        capacity: usize,
        max_capacity: usize,
        max_retained_bytes: usize,
    ) -> VResult<StreamVal> {
        if capacity > max_capacity {
            return Err(ErrorVal::arg_error(format!(
                "stream buffer capacity cannot exceed {max_capacity}"
            )));
        }
        let lease = acquire_stream_pump()?;
        let label = stream.label.clone();
        let bounded = stream.is_bounded();
        let mut upstream = stream.take_upstream()?;
        let (tx, rx) = sync_channel(capacity);
        let retained_bytes = Arc::new(AtomicUsize::new(0));
        let producer_retained_bytes = retained_bytes.clone();
        let parent_cancel = self.cancellation_token();
        let cancel = CancelToken::linked(&parent_cancel);
        let source_cancel = cancel.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let producer_stop = stop.clone();
        let child = self.child_context();

        std::thread::Builder::new()
            .name("shoal-stream-buffer".into())
            .spawn(move || {
                let _lease = lease;
                let mut evaluator = child.build(ChildKind::StreamPump, cancel.clone());
                loop {
                    if cancel.is_cancelled() || producer_stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let (delivery, retained, terminal) =
                        match upstream.pull(&mut evaluator, Some(PUMP_POLL)) {
                            Ok(Pull::Item(value)) => {
                                match buffered_retained_size(&value, max_retained_bytes) {
                                    Ok(retained) => (Ok(value), retained, false),
                                    Err(error) => (Err(error), 0, true),
                                }
                            }
                            Ok(Pull::Timeout) => continue,
                            Ok(Pull::End) => break,
                            Err(error) => (Err(error), 0, true),
                        };
                    if retained > 0
                        && !reserve_buffer_bytes(
                            &producer_retained_bytes,
                            retained,
                            max_retained_bytes,
                            &cancel,
                            &producer_stop,
                        )
                    {
                        break;
                    }
                    let item = BufferedDelivery {
                        delivery,
                        retained_bytes: retained,
                    };
                    if !send_to_buffer(&tx, &cancel, &producer_stop, item) {
                        if retained > 0 {
                            producer_retained_bytes.fetch_sub(retained, Ordering::SeqCst);
                        }
                        break;
                    }
                    if terminal {
                        break;
                    }
                }
            })
            .map_err(|error| ErrorVal::new("io_error", format!("spawn stream buffer: {error}")))?;

        Ok(StreamVal::from_upstream(
            label,
            bounded,
            Box::new(OwnedBufferSource {
                rx,
                retained_bytes,
                stop,
                cancel: source_cancel,
            }),
        ))
    }
}

fn buffered_retained_size(value: &Value, max_retained_bytes: usize) -> VResult<usize> {
    retained_size(
        value,
        RetainedLimits {
            max_bytes: max_retained_bytes,
            max_depth: 64,
            max_nodes: 16_384,
            opaque: OpaqueHandling::Charge(256),
            allow_secret: true,
        },
    )
    .map_err(|_| {
        ErrorVal::new(
            "stream_buffer_limit",
            format!(
                "one buffered value exceeds the {max_retained_bytes}-byte retained-value limit"
            ),
        )
        .with_hint("transform large values into smaller chunks before `.buffer(n)`")
    })
}

fn reserve_buffer_bytes(
    retained_bytes: &AtomicUsize,
    bytes: usize,
    max_retained_bytes: usize,
    cancel: &CancelToken,
    stop: &AtomicBool,
) -> bool {
    loop {
        if cancel.is_cancelled() || stop.load(Ordering::SeqCst) {
            return false;
        }
        let current = retained_bytes.load(Ordering::SeqCst);
        if current <= max_retained_bytes.saturating_sub(bytes)
            && retained_bytes
                .compare_exchange(current, current + bytes, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            return true;
        }
        std::thread::park_timeout(Duration::from_millis(2));
    }
}

fn send_to_buffer<T>(
    tx: &SyncSender<T>,
    cancel: &CancelToken,
    stop: &AtomicBool,
    mut delivery: T,
) -> bool {
    loop {
        if cancel.is_cancelled() || stop.load(Ordering::SeqCst) {
            return false;
        }
        match tx.try_send(delivery) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(returned)) => {
                delivery = returned;
                std::thread::park_timeout(Duration::from_millis(2));
            }
        }
    }
}
