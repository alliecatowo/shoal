//! Arc-based cancellation token shared between the caller and watcher threads.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A cheaply-clonable cancellation flag.
///
/// All clones share one flag: cancelling any clone cancels them all. Passing
/// a clone of the same token to [`crate::run`] / [`crate::spawn_capture`] and
/// later to [`StreamingChild::wait`](crate::StreamingChild::wait) is the
/// expected pattern; distinct tokens are also honored (either one cancels).
#[derive(Debug)]
struct CancelInner {
    own: Arc<AtomicBool>,
    checks: Vec<Arc<AtomicBool>>,
}

/// A token owns one cancellation flag and may additionally observe the flags
/// of a parent token. Clones share the exact same cancellation identity.
#[derive(Debug, Clone)]
pub struct CancelToken(Arc<CancelInner>);

impl Default for CancelToken {
    fn default() -> Self {
        let own = Arc::new(AtomicBool::new(false));
        Self(Arc::new(CancelInner {
            own: own.clone(),
            checks: vec![own],
        }))
    }
}

impl CancelToken {
    /// Create a fresh, un-cancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a fresh independently-cancellable child that also observes
    /// cancellation of `parent`. Cancelling the child never cancels the parent.
    #[must_use]
    pub fn linked(parent: &CancelToken) -> Self {
        let own = Arc::new(AtomicBool::new(false));
        let mut checks = Vec::with_capacity(parent.0.checks.len() + 1);
        checks.push(own.clone());
        checks.extend(parent.0.checks.iter().cloned());
        Self(Arc::new(CancelInner { own, checks }))
    }

    /// Trip the flag. Idempotent; never blocks.
    pub fn cancel(&self) {
        self.0.own.store(true, Ordering::SeqCst);
    }

    /// Has [`CancelToken::cancel`] been called on this token (or any clone)?
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.checks.iter().any(|flag| flag.load(Ordering::SeqCst))
    }

    /// Do `self` and `other` share the same underlying flag?
    pub(crate) fn same(&self, other: &CancelToken) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linked_token_observes_parent_without_cancelling_it() {
        let parent = CancelToken::new();
        let child = CancelToken::linked(&parent);
        child.cancel();
        assert!(child.is_cancelled());
        assert!(!parent.is_cancelled());

        let parent = CancelToken::new();
        let child = CancelToken::linked(&parent);
        parent.cancel();
        assert!(child.is_cancelled());
    }
}
