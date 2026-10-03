//! Process-wide admission for active command-capture memory and spill files.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub const MAX_CAPTURE_HARD_CAP: usize = 64 * 1024 * 1024;
pub const DEFAULT_CAPTURE_AGGREGATE_MEMORY_CAP: usize = 256 * 1024 * 1024;
pub const MAX_CAPTURE_AGGREGATE_MEMORY_CAP: usize = 512 * 1024 * 1024;

pub const MAX_CAPTURE_SPILL_CAP: u64 = 512 * 1024 * 1024;
pub const DEFAULT_CAPTURE_AGGREGATE_SPILL_CAP: u64 = 512 * 1024 * 1024;
pub const MAX_CAPTURE_AGGREGATE_SPILL_CAP: u64 = 1024 * 1024 * 1024;
pub const MAX_ACTIVE_CAPTURE_SPILL_FILES: usize = 16;

static MEMORY_CAP: AtomicUsize = AtomicUsize::new(0);
static MEMORY_USED: AtomicUsize = AtomicUsize::new(0);
static SPILL_CAP: AtomicU64 = AtomicU64::new(0);
static SPILL_USED: AtomicU64 = AtomicU64::new(0);
static SPILL_FILES: AtomicUsize = AtomicUsize::new(0);

fn env_usize(name: &str, default: usize, maximum: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
        .min(maximum)
}

fn env_u64(name: &str, default: u64, maximum: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
        .min(maximum)
}

pub fn aggregate_memory_cap() -> usize {
    let cached = MEMORY_CAP.load(Ordering::Acquire);
    if cached != 0 {
        return cached;
    }
    let resolved = env_usize(
        "SHOAL_CAPTURE_AGGREGATE_CAP_BYTES",
        DEFAULT_CAPTURE_AGGREGATE_MEMORY_CAP,
        MAX_CAPTURE_AGGREGATE_MEMORY_CAP,
    );
    let _ = MEMORY_CAP.compare_exchange(0, resolved, Ordering::AcqRel, Ordering::Acquire);
    MEMORY_CAP.load(Ordering::Acquire)
}

pub fn set_aggregate_memory_cap(bytes: usize) {
    MEMORY_CAP.store(
        bytes.clamp(1, MAX_CAPTURE_AGGREGATE_MEMORY_CAP),
        Ordering::Release,
    );
}

pub fn aggregate_spill_cap() -> u64 {
    let cached = SPILL_CAP.load(Ordering::Acquire);
    if cached != 0 {
        return cached;
    }
    let resolved = env_u64(
        "SHOAL_CAPTURE_AGGREGATE_SPILL_CAP_BYTES",
        DEFAULT_CAPTURE_AGGREGATE_SPILL_CAP,
        MAX_CAPTURE_AGGREGATE_SPILL_CAP,
    );
    let _ = SPILL_CAP.compare_exchange(0, resolved, Ordering::AcqRel, Ordering::Acquire);
    SPILL_CAP.load(Ordering::Acquire)
}

pub fn set_aggregate_spill_cap(bytes: u64) {
    SPILL_CAP.store(
        bytes.clamp(1, MAX_CAPTURE_AGGREGATE_SPILL_CAP),
        Ordering::Release,
    );
}

#[derive(Debug)]
pub(crate) struct MemoryLease {
    bytes: usize,
}

impl MemoryLease {
    pub(crate) fn empty() -> Self {
        Self { bytes: 0 }
    }

    pub(crate) fn reserve_up_to(&mut self, requested: usize) -> usize {
        let limit = aggregate_memory_cap();
        let mut used = MEMORY_USED.load(Ordering::Acquire);
        loop {
            let granted = requested.min(limit.saturating_sub(used));
            match MEMORY_USED.compare_exchange_weak(
                used,
                used.saturating_add(granted),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.bytes = self.bytes.saturating_add(granted);
                    return granted;
                }
                Err(actual) => used = actual,
            }
        }
    }

    pub(crate) fn rollback(&mut self, bytes: usize) {
        let released = bytes.min(self.bytes);
        self.bytes -= released;
        MEMORY_USED.fetch_sub(released, Ordering::AcqRel);
    }
}

impl Drop for MemoryLease {
    fn drop(&mut self) {
        MEMORY_USED.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[derive(Debug)]
pub(crate) struct SpillLease {
    bytes: u64,
}

impl SpillLease {
    pub(crate) fn acquire_file() -> Option<Self> {
        SPILL_FILES
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |files| {
                (files < MAX_ACTIVE_CAPTURE_SPILL_FILES).then_some(files + 1)
            })
            .ok()?;
        Some(Self { bytes: 0 })
    }

    pub(crate) fn reserve_up_to(&mut self, requested: u64) -> u64 {
        let limit = aggregate_spill_cap();
        let mut used = SPILL_USED.load(Ordering::Acquire);
        loop {
            let granted = requested.min(limit.saturating_sub(used));
            match SPILL_USED.compare_exchange_weak(
                used,
                used.saturating_add(granted),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.bytes = self.bytes.saturating_add(granted);
                    return granted;
                }
                Err(actual) => used = actual,
            }
        }
    }

    pub(crate) fn rollback(&mut self, bytes: u64) {
        let released = bytes.min(self.bytes);
        self.bytes -= released;
        SPILL_USED.fetch_sub(released, Ordering::AcqRel);
    }
}

impl Drop for SpillLease {
    fn drop(&mut self) {
        SPILL_USED.fetch_sub(self.bytes, Ordering::AcqRel);
        SPILL_FILES.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
pub(crate) fn usage() -> (usize, u64, usize) {
    (
        MEMORY_USED.load(Ordering::Acquire),
        SPILL_USED.load(Ordering::Acquire),
        SPILL_FILES.load(Ordering::Acquire),
    )
}
