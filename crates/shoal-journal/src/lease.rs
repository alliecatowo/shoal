//! Crash-reapable ownership for live lazy CAS values.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior, params};

use crate::{hex_bytes, io_to_sql, now_ns};

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

/// One journal handle's liveness identity. The exclusive file lock remains
/// held while either the handle or any value lease derived from it exists.
pub(crate) struct LeaseOwner {
    id: String,
    db_path: PathBuf,
    lock_path: PathBuf,
    lock: File,
    _tempdir: Option<tempfile::TempDir>,
}

impl LeaseOwner {
    pub(crate) fn new(
        state_dir: &Path,
        db_path: PathBuf,
        tempdir: Option<tempfile::TempDir>,
    ) -> rusqlite::Result<Arc<Self>> {
        let lease_dir = state_dir.join("leases");
        fs::create_dir_all(&lease_dir).map_err(io_to_sql)?;
        for _ in 0..16 {
            let serial = NEXT_OWNER.fetch_add(1, Ordering::Relaxed);
            let id = format!("{:x}-{:x}-{:x}", std::process::id(), now_ns(), serial);
            let lock_path = lease_dir.join(format!("{id}.lock"));
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&lock_path)
            {
                Ok(lock) => {
                    lock.lock().map_err(io_to_sql)?;
                    return Ok(Arc::new(Self {
                        id,
                        db_path,
                        lock_path,
                        lock,
                        _tempdir: tempdir,
                    }));
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(io_to_sql(error)),
            }
        }
        Err(io_to_sql(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique journal lease owner",
        )))
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn lease(self: &Arc<Self>, hash: &str) -> rusqlite::Result<PinLease> {
        Ok(PinLease {
            owner: self.clone(),
            hash: hex_bytes(hash)
                .map_err(|_| rusqlite::Error::InvalidParameterName("invalid hash".into()))?,
        })
    }
}

impl Drop for LeaseOwner {
    fn drop(&mut self) {
        let _ = File::unlock(&self.lock);
        let _ = fs::remove_file(&self.lock_path);
    }
}

/// One counted live-value lease. Cloning a `CasBytesVal` clones its loader
/// `Arc`, not this guard, so the database count follows the logical lazy value
/// rather than every shallow `Value` clone.
pub struct PinLease {
    owner: Arc<LeaseOwner>,
    hash: Vec<u8>,
}

impl std::fmt::Debug for PinLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PinLease")
            .field("owner", &self.owner.id)
            .finish_non_exhaustive()
    }
}

impl Drop for PinLease {
    fn drop(&mut self) {
        // Drop must never panic or block application teardown indefinitely. A
        // failed best-effort decrement remains safe: the owner lock is released
        // after this guard, and the next GC reaps every row for that stale owner.
        let Ok(mut conn) = Connection::open(&self.owner.db_path) else {
            return;
        };
        if conn.busy_timeout(Duration::from_millis(50)).is_err() {
            return;
        }
        let Ok(tx) = conn.transaction_with_behavior(TransactionBehavior::Immediate) else {
            return;
        };
        let Ok(deleted) = tx.execute(
            "DELETE FROM pin_lease WHERE hash=?1 AND owner=?2 AND ref_count=1",
            params![self.hash, self.owner.id],
        ) else {
            return;
        };
        if deleted == 0
            && tx
                .execute(
                    "UPDATE pin_lease SET ref_count=ref_count-1
                     WHERE hash=?1 AND owner=?2 AND ref_count>1",
                    params![self.hash, self.owner.id],
                )
                .is_err()
        {
            return;
        }
        let _ = tx.commit();
    }
}
