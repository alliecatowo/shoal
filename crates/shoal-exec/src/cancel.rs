//! Arc-based cancellation token shared between the caller and watcher threads.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A cheaply-clonable cancellation flag.
///
/// All clones share one flag: cancelling any clone cancels them all. Passing
/// a clone of the same token to [`crate::run`] / [`crate::spawn_capture`] and
/// later to [`StreamingChild::wait`](crate::StreamingChild::wait) is the
/// expected pattern; distinct tokens are also honored (either one cancels).
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// Create a fresh, un-cancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Trip the flag. Idempotent; never blocks.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Has [`CancelToken::cancel`] been called on this token (or any clone)?
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Do `self` and `other` share the same underlying flag?
    pub(crate) fn same(&self, other: &CancelToken) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
