use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};
use tower_lsp::lsp_types::Url;

pub(crate) const MAX_ANALYSIS_CONCURRENCY: usize = 4;
const MAX_ANALYSIS_JOBS: usize = 64;
const MAX_ANALYSIS_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct AnalysisJob {
    pub(crate) uri: Url,
    pub(crate) text: String,
    pub(crate) version: i32,
}

struct AnalysisSlot {
    active_version: i32,
    active_bytes: usize,
    pending: Option<AnalysisJob>,
}

#[derive(Default)]
struct AnalysisQueueState {
    slots: HashMap<Url, AnalysisSlot>,
    retained_bytes: usize,
}

pub(crate) struct AnalysisQueue {
    state: Mutex<AnalysisQueueState>,
    pub(crate) permits: Arc<Semaphore>,
}

pub(crate) enum AnalysisAdmission {
    Start,
    Queued,
    Ignored,
    Rejected(&'static str),
}

impl AnalysisQueue {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(AnalysisQueueState::default()),
            permits: Arc::new(Semaphore::new(MAX_ANALYSIS_CONCURRENCY)),
        }
    }

    pub(crate) async fn admit(&self, job: &AnalysisJob) -> AnalysisAdmission {
        let mut state = self.state.lock().await;
        if let Some(slot) = state.slots.get(&job.uri) {
            if slot
                .pending
                .as_ref()
                .is_some_and(|pending| pending.version == job.version && pending.text == job.text)
            {
                return AnalysisAdmission::Ignored;
            }
            let old_pending_bytes = slot
                .pending
                .as_ref()
                .map_or(0, |pending| pending.text.len());
            let projected = state
                .retained_bytes
                .saturating_sub(old_pending_bytes)
                .saturating_add(job.text.len());
            if projected > MAX_ANALYSIS_BYTES {
                if let Some(slot) = state.slots.get_mut(&job.uri) {
                    slot.pending = None;
                }
                state.retained_bytes = state.retained_bytes.saturating_sub(old_pending_bytes);
                return AnalysisAdmission::Rejected("analysis pending-byte budget reached");
            }
            state.retained_bytes = projected;
            state
                .slots
                .get_mut(&job.uri)
                .expect("analysis slot remains present")
                .pending = Some(job.clone());
            return AnalysisAdmission::Queued;
        }
        if state.slots.len() >= MAX_ANALYSIS_JOBS {
            return AnalysisAdmission::Rejected("analysis job budget reached");
        }
        if state.retained_bytes.saturating_add(job.text.len()) > MAX_ANALYSIS_BYTES {
            return AnalysisAdmission::Rejected("analysis pending-byte budget reached");
        }
        state.retained_bytes += job.text.len();
        state.slots.insert(
            job.uri.clone(),
            AnalysisSlot {
                active_version: job.version,
                active_bytes: job.text.len(),
                pending: None,
            },
        );
        AnalysisAdmission::Start
    }

    pub(crate) async fn complete(&self, uri: &Url, version: i32) -> Option<AnalysisJob> {
        let mut state = self.state.lock().await;
        let (active_bytes, next) = {
            let slot = state.slots.get_mut(uri)?;
            if slot.active_version != version {
                return None;
            }
            (slot.active_bytes, slot.pending.take())
        };
        state.retained_bytes = state.retained_bytes.saturating_sub(active_bytes);
        if let Some(next) = next {
            let slot = state
                .slots
                .get_mut(uri)
                .expect("analysis slot remains present");
            slot.active_version = next.version;
            slot.active_bytes = next.text.len();
            Some(next)
        } else {
            state.slots.remove(uri);
            None
        }
    }

    pub(crate) async fn cancel_pending(&self, uri: &Url) {
        let mut state = self.state.lock().await;
        let pending_bytes = state
            .slots
            .get_mut(uri)
            .and_then(|slot| slot.pending.take())
            .map_or(0, |pending| pending.text.len());
        state.retained_bytes = state.retained_bytes.saturating_sub(pending_bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MAX_DOCUMENT_BYTES;

    fn job(uri: &str, version: i32, bytes: usize) -> AnalysisJob {
        AnalysisJob {
            uri: Url::parse(uri).unwrap(),
            text: "x".repeat(bytes),
            version,
        }
    }

    #[tokio::test]
    async fn coalesces_each_uri_to_its_latest_arrival() {
        let queue = AnalysisQueue::new();
        let first = job("file:///tmp/coalesce.shl", 1, 1024);
        assert!(matches!(
            queue.admit(&first).await,
            AnalysisAdmission::Start
        ));
        assert!(matches!(
            queue.admit(&job("file:///tmp/coalesce.shl", 2, 2048)).await,
            AnalysisAdmission::Queued
        ));
        assert!(matches!(
            queue.admit(&job("file:///tmp/coalesce.shl", 3, 4096)).await,
            AnalysisAdmission::Queued
        ));
        let next = queue.complete(&first.uri, 1).await.unwrap();
        assert_eq!(next.version, 3);
        assert_eq!(next.text.len(), 4096);
        assert!(queue.complete(&next.uri, 3).await.is_none());
        let state = queue.state.lock().await;
        assert!(state.slots.is_empty());
        assert_eq!(state.retained_bytes, 0);
    }

    #[tokio::test]
    async fn bounds_cross_uri_flood_and_cancels_pending() {
        let queue = AnalysisQueue::new();
        let jobs = MAX_ANALYSIS_BYTES / MAX_DOCUMENT_BYTES;
        for index in 0..jobs {
            let job = job(
                &format!("file:///tmp/flood-{index}.shl"),
                1,
                MAX_DOCUMENT_BYTES,
            );
            assert!(matches!(queue.admit(&job).await, AnalysisAdmission::Start));
        }
        let rejected = job("file:///tmp/rejected.shl", 1, MAX_DOCUMENT_BYTES);
        assert!(matches!(
            queue.admit(&rejected).await,
            AnalysisAdmission::Rejected(_)
        ));
        let uri = Url::parse("file:///tmp/flood-0.shl").unwrap();
        assert!(queue.complete(&uri, 1).await.is_none());
        let active = job("file:///tmp/cancel.shl", 1, 1);
        assert!(matches!(
            queue.admit(&active).await,
            AnalysisAdmission::Start
        ));
        assert!(matches!(
            queue.admit(&job("file:///tmp/cancel.shl", 2, 4096)).await,
            AnalysisAdmission::Queued
        ));
        queue.cancel_pending(&active.uri).await;
        assert!(queue.complete(&active.uri, 1).await.is_none());
    }

    #[tokio::test]
    async fn blocking_panic_releases_global_permit() {
        let queue = AnalysisQueue::new();
        let permit = Arc::clone(&queue.permits).acquire_owned().await.unwrap();
        let panicked = tokio::task::spawn_blocking(|| panic!("injected analysis panic")).await;
        assert!(panicked.is_err());
        drop(permit);
        assert_eq!(queue.permits.available_permits(), MAX_ANALYSIS_CONCURRENCY);
    }
}
