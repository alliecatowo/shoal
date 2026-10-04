//! Completion-aware hard execution deadline for an owned kernel task.

use super::*;

pub(super) const EXEC_DEADLINE_MAX_MS: u64 = 24 * 60 * 60 * 1_000;

pub(super) fn spawn_deadline_watchdog(task: Arc<TaskEntry>, deadline_ms: u64) -> io::Result<()> {
    std::thread::Builder::new()
        .name(format!("shoal-task-deadline-{}", task.task.0))
        .spawn(move || {
            let deadline = Instant::now() + std::time::Duration::from_millis(deadline_ms);
            let mut inner = match task.lock_inner() {
                Ok(inner) => inner,
                Err(_) => return,
            };
            while matches!(inner.state, "running" | "suspended" | "cancelling") {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let (next, wait) = match task.done.wait_timeout(inner, remaining) {
                    Ok(result) => result,
                    Err(poisoned) => {
                        let _ = task.repair_timeout_wait_poison(poisoned);
                        return;
                    }
                };
                inner = next;
                if wait.timed_out() {
                    break;
                }
            }
            let active = matches!(inner.state, "running" | "suspended" | "cancelling");
            drop(inner);
            if active {
                let _ = task.request_deadline_cancel();
            }
        })?;
    Ok(())
}
