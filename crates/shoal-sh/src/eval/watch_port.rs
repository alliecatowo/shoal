//! Host-injected filesystem watch registration.
//!
//! The evaluator consumes neutral events from this port; only the default
//! adapter knows about `notify`/inotify/kqueue. The adapter bounds its callback
//! queue and reports dropped raw events explicitly instead of letting a burst
//! grow an unbounded `mpsc::channel` behind Shoal's bounded stream buffer.

use notify::{EventKind, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError, TrySendError, sync_channel};
use std::time::Duration;

const RAW_WATCH_BUFFER: usize = 256;
const MAX_EVENT_PATHS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchKind {
    Created,
    Modified,
    Removed,
    Other,
}

#[derive(Debug)]
pub struct WatchEvent {
    pub kind: WatchKind,
    pub paths: Vec<PathBuf>,
}

#[derive(Debug)]
pub enum WatchPoll {
    Event(WatchEvent),
    Overflow(u64),
    Error(String),
    Timeout,
    Closed,
}

pub trait WatchSubscription: Send {
    fn poll(&mut self, timeout: Duration) -> WatchPoll;
}

pub trait WatchPort: Send + Sync {
    fn subscribe(&self, path: &Path, recursive: bool)
    -> Result<Box<dyn WatchSubscription>, String>;
}

#[derive(Debug, Default)]
pub struct StdWatchPort;

enum RawEvent {
    Event(WatchEvent),
    Error(String),
}

struct NotifySubscription {
    _watcher: notify::RecommendedWatcher,
    receiver: Receiver<RawEvent>,
    dropped: Arc<AtomicU64>,
}

impl WatchSubscription for NotifySubscription {
    fn poll(&mut self, timeout: Duration) -> WatchPoll {
        poll_raw(&self.receiver, &self.dropped, timeout)
    }
}

fn poll_raw(receiver: &Receiver<RawEvent>, dropped: &AtomicU64, timeout: Duration) -> WatchPoll {
    match receiver.try_recv() {
        Ok(RawEvent::Event(event)) => return WatchPoll::Event(event),
        Ok(RawEvent::Error(error)) => return WatchPoll::Error(error),
        Err(TryRecvError::Disconnected) => return WatchPoll::Closed,
        Err(TryRecvError::Empty) => {}
    }
    let overflow = dropped.swap(0, Ordering::AcqRel);
    if overflow > 0 {
        return WatchPoll::Overflow(overflow);
    }
    match receiver.recv_timeout(timeout) {
        Ok(RawEvent::Event(event)) => WatchPoll::Event(event),
        Ok(RawEvent::Error(error)) => WatchPoll::Error(error),
        Err(RecvTimeoutError::Timeout) => WatchPoll::Timeout,
        Err(RecvTimeoutError::Disconnected) => WatchPoll::Closed,
    }
}

impl WatchPort for StdWatchPort {
    fn subscribe(
        &self,
        path: &Path,
        recursive: bool,
    ) -> Result<Box<dyn WatchSubscription>, String> {
        let (sender, receiver) = sync_channel(RAW_WATCH_BUFFER);
        let dropped = Arc::new(AtomicU64::new(0));
        let callback_dropped = dropped.clone();
        let mut watcher =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                let raw = match result {
                    Ok(mut event) => {
                        let excess = event.paths.len().saturating_sub(MAX_EVENT_PATHS);
                        event.paths.truncate(MAX_EVENT_PATHS);
                        if excess > 0 {
                            record_dropped(
                                &callback_dropped,
                                u64::try_from(excess).unwrap_or(u64::MAX),
                            );
                        }
                        RawEvent::Event(WatchEvent {
                            kind: kind(event.kind),
                            paths: event.paths,
                        })
                    }
                    Err(error) => RawEvent::Error(error.to_string()),
                };
                match sender.try_send(raw) {
                    Ok(()) | Err(TrySendError::Disconnected(_)) => {}
                    Err(TrySendError::Full(_)) => {
                        record_dropped(&callback_dropped, 1);
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        watcher
            .watch(
                path,
                if recursive {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                },
            )
            .map_err(|error| error.to_string())?;
        Ok(Box::new(NotifySubscription {
            _watcher: watcher,
            receiver,
            dropped,
        }))
    }
}

fn record_dropped(counter: &AtomicU64, amount: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(amount))
    });
}

fn kind(kind: EventKind) -> WatchKind {
    match kind {
        EventKind::Create(_) => WatchKind::Created,
        EventKind::Modify(_) => WatchKind::Modified,
        EventKind::Remove(_) => WatchKind::Removed,
        _ => WatchKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_events_precede_the_overflow_that_followed_them() {
        let (sender, receiver) = sync_channel(1);
        sender
            .send(RawEvent::Event(WatchEvent {
                kind: WatchKind::Modified,
                paths: vec![PathBuf::from("first")],
            }))
            .unwrap();
        let dropped = AtomicU64::new(3);

        assert!(matches!(
            poll_raw(&receiver, &dropped, Duration::ZERO),
            WatchPoll::Event(WatchEvent { paths, .. }) if paths == [PathBuf::from("first")]
        ));
        assert!(matches!(
            poll_raw(&receiver, &dropped, Duration::ZERO),
            WatchPoll::Overflow(3)
        ));
    }

    #[test]
    fn overflow_accounting_saturates() {
        let dropped = AtomicU64::new(u64::MAX - 1);
        record_dropped(&dropped, 10);
        assert_eq!(dropped.load(Ordering::Relaxed), u64::MAX);
    }
}
