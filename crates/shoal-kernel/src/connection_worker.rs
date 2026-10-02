use std::io;

pub(crate) type ConnectionJob = Box<dyn FnOnce() + Send + 'static>;

pub(crate) trait ConnectionSpawner: Send + Sync {
    fn spawn(&self, job: ConnectionJob) -> io::Result<()>;
}

pub(crate) struct ThreadSpawner;

impl ConnectionSpawner for ThreadSpawner {
    fn spawn(&self, job: ConnectionJob) -> io::Result<()> {
        std::thread::Builder::new()
            .name("shoal-kernel-connection".into())
            .spawn(job)
            .map(|_| ())
    }
}

pub(crate) fn failure_backoff(consecutive_failures: u32) -> std::time::Duration {
    let shift = consecutive_failures.saturating_sub(1).min(5);
    std::time::Duration::from_millis(25_u64 << shift)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_failure_backoff_is_bounded() {
        assert_eq!(failure_backoff(1), std::time::Duration::from_millis(25));
        assert_eq!(failure_backoff(2), std::time::Duration::from_millis(50));
        assert_eq!(failure_backoff(100), std::time::Duration::from_millis(800));
    }
}
