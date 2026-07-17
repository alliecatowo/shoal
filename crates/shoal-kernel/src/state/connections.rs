use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Owns connection identity, admission, and framing policy. Keeping these
/// invariants together prevents unrelated kernel services from manipulating
/// half of the connection lifecycle directly.
pub(crate) struct ConnectionRegistry {
    next_client: AtomicU64,
    active: Arc<AtomicUsize>,
    max: AtomicUsize,
    frame_read_timeout_ms: AtomicU64,
}

impl ConnectionRegistry {
    pub(crate) fn new(max: usize, frame_read_timeout_ms: u64) -> Self {
        Self {
            next_client: AtomicU64::new(1),
            active: Arc::new(AtomicUsize::new(0)),
            max: AtomicUsize::new(max),
            frame_read_timeout_ms: AtomicU64::new(frame_read_timeout_ms),
        }
    }

    pub(crate) fn configure(&self, max: usize, frame_read_timeout_ms: u64) {
        self.max.store(max, Ordering::Relaxed);
        self.frame_read_timeout_ms
            .store(frame_read_timeout_ms, Ordering::Relaxed);
    }

    pub(crate) fn next_client(&self) -> u64 {
        self.next_client.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn max(&self) -> usize {
        self.max.load(Ordering::Relaxed)
    }

    pub(crate) fn frame_read_timeout_ms(&self) -> u64 {
        self.frame_read_timeout_ms.load(Ordering::Relaxed)
    }

    pub(crate) fn reserve(&self) -> Result<ConnectionPermit, ()> {
        let max = self.max();
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |current| {
                (current < max).then_some(current + 1)
            })
            .map(|_| ConnectionPermit {
                active: self.active.clone(),
            })
            .map_err(|_| ())
    }

    #[cfg(test)]
    pub(crate) fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }
}

/// One admitted connection. The permit owns only the counter it must release,
/// not an `Arc<Kernel>`, so an abandoned socket thread cannot retain every
/// unrelated service in the process.
pub(crate) struct ConnectionPermit {
    active: Arc<AtomicUsize>,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        let previous = self.active.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "connection slot underflow");
    }
}
