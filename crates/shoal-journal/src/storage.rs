//! Conservative durable-storage admission and observability.

use std::error::Error;
use std::fmt;
use std::fs;
use std::path::Path;

use rusqlite::{Transaction, TransactionBehavior};

use crate::Journal;

pub const DEFAULT_JOURNAL_DATABASE_MAX_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_JOURNAL_DATABASE_MAX_BYTES: u64 = 64 * 1024 * 1024 * 1024;
pub const DEFAULT_JOURNAL_CAS_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const MAX_JOURNAL_CAS_MAX_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// SQLite page/WAL headroom reserved before one small metadata mutation.
pub(crate) const DB_WRITE_RESERVE_BYTES: u64 = 64 * 1024;
/// Begin rows reserve completion headroom too: once effects start, finishing
/// the audit row must not depend on a second best-effort admission decision.
pub(crate) const ENTRY_COMPLETION_RESERVE_BYTES: u64 = 64 * 1024;

const MAX_STATUS_WALK_ENTRIES: usize = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalStorageLimits {
    pub database_max_bytes: u64,
    pub cas_max_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalStorageStatus {
    pub database_bytes: u64,
    pub wal_bytes: u64,
    pub shm_bytes: u64,
    pub cas_logical_bytes: u64,
    pub cas_physical_bytes: u64,
    pub spill_physical_bytes: u64,
    pub pinned_logical_bytes: u64,
    pub database_max_bytes: u64,
    pub cas_max_bytes: u64,
}

impl JournalStorageStatus {
    pub fn database_admission_bytes(self) -> u64 {
        self.database_bytes.saturating_add(self.wal_bytes)
    }

    pub fn cas_admission_bytes(self) -> u64 {
        self.cas_logical_bytes.max(
            self.cas_physical_bytes
                .saturating_add(self.spill_physical_bytes),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageDomain {
    Database,
    Cas,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageAdmissionError {
    pub domain: StorageDomain,
    pub used: u64,
    pub requested: u64,
    pub limit: u64,
}

impl fmt::Display for StorageAdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "journal {:?} storage admission refused: {} used + {} requested exceeds {} byte limit",
            self.domain, self.used, self.requested, self.limit
        )
    }
}

impl Error for StorageAdmissionError {}

pub(crate) fn admission_to_sql(error: StorageAdmissionError) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(error))
}

impl Journal {
    pub(crate) fn configure_storage_pragmas(
        conn: &rusqlite::Connection,
        limits: JournalStorageLimits,
    ) -> rusqlite::Result<()> {
        let page_size: i64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
        let page_count: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
        let page_size = u64::try_from(page_size).unwrap_or(4096).max(1);
        let configured_pages = limits.database_max_bytes / page_size;
        let max_pages = configured_pages
            .max(u64::try_from(page_count).unwrap_or(u64::MAX))
            .min(i64::MAX as u64) as i64;
        conn.pragma_update(None, "max_page_count", max_pages)?;
        // Frequent passive checkpoints keep ordinary WAL growth small. A long
        // reader can still delay recycling, which is why every admitted write
        // also meters the actual WAL file rather than claiming this is a hard
        // WAL quota.
        conn.pragma_update(None, "wal_autocheckpoint", 128i64)?;
        let journal_limit = limits
            .database_max_bytes
            .saturating_div(8)
            .clamp(64 * 1024, 64 * 1024 * 1024)
            .min(i64::MAX as u64) as i64;
        conn.pragma_update(None, "journal_size_limit", journal_limit)?;
        Ok(())
    }

    pub fn storage_limits(&self) -> JournalStorageLimits {
        self.storage_limits
    }

    pub fn storage_status(&self) -> rusqlite::Result<JournalStorageStatus> {
        self.storage_status_on(&self.conn)
    }

    pub(crate) fn with_database_admission<T>(
        &self,
        requested: u64,
        write: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        // IMMEDIATE serializes the physical-size check and mutation with every
        // cooperating Shoal process that writes this database.
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let (database_bytes, wal_bytes, _) = database_file_bytes(&tx)?;
        let used = database_bytes.saturating_add(wal_bytes);
        let requested = requested.saturating_add(ENTRY_COMPLETION_RESERVE_BYTES);
        if requested > self.storage_limits.database_max_bytes.saturating_sub(used) {
            return Err(admission_to_sql(StorageAdmissionError {
                domain: StorageDomain::Database,
                used,
                requested,
                limit: self.storage_limits.database_max_bytes,
            }));
        }
        let value = write(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    pub(crate) fn admit_cas_growth(
        &self,
        tx: &Transaction<'_>,
        hash: &[u8],
        requested: u64,
    ) -> rusqlite::Result<bool> {
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM blob WHERE hash=?1)",
            [hash],
            |row| row.get(0),
        )?;
        if exists {
            return Ok(false);
        }
        let logical: i64 =
            tx.query_row("SELECT COALESCE(SUM(stored_len), 0) FROM blob", [], |row| {
                row.get(0)
            })?;
        let logical = u64::try_from(logical).unwrap_or(u64::MAX);
        let physical = self.reconciled_cas_physical_bytes;
        let used = logical.max(physical);
        if requested > self.storage_limits.cas_max_bytes.saturating_sub(used) {
            return Err(admission_to_sql(StorageAdmissionError {
                domain: StorageDomain::Cas,
                used,
                requested,
                limit: self.storage_limits.cas_max_bytes,
            }));
        }
        Ok(true)
    }

    fn storage_status_on(
        &self,
        conn: &rusqlite::Connection,
    ) -> rusqlite::Result<JournalStorageStatus> {
        let (database_bytes, wal_bytes, shm_bytes) = database_file_bytes(conn)?;
        let cas_logical: i64 =
            conn.query_row("SELECT COALESCE(SUM(stored_len), 0) FROM blob", [], |row| {
                row.get(0)
            })?;
        let pinned_logical: i64 = conn.query_row(
            "SELECT COALESCE(SUM(b.stored_len), 0) FROM blob b
             WHERE EXISTS(SELECT 1 FROM pin p WHERE p.hash=b.hash)
                OR EXISTS(SELECT 1 FROM pin_lease l WHERE l.hash=b.hash)",
            [],
            |row| row.get(0),
        )?;
        Ok(JournalStorageStatus {
            database_bytes,
            wal_bytes,
            shm_bytes,
            cas_logical_bytes: u64::try_from(cas_logical).unwrap_or(u64::MAX),
            cas_physical_bytes: directory_bytes(&self.cas_root)?,
            spill_physical_bytes: directory_bytes(
                &self
                    .cas_root
                    .parent()
                    .unwrap_or(self.cas_root.as_path())
                    .join("spill"),
            )?,
            pinned_logical_bytes: u64::try_from(pinned_logical).unwrap_or(u64::MAX),
            database_max_bytes: self.storage_limits.database_max_bytes,
            cas_max_bytes: self.storage_limits.cas_max_bytes,
        })
    }
}

fn file_bytes(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

fn database_file_bytes(conn: &rusqlite::Connection) -> rusqlite::Result<(u64, u64, u64)> {
    match conn.path() {
        Some(path) => {
            let path = Path::new(path);
            Ok((
                file_bytes(path),
                file_bytes(&path.with_extension("db-wal")),
                file_bytes(&path.with_extension("db-shm")),
            ))
        }
        None => {
            let page_count: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
            let page_size: i64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
            Ok((
                u64::try_from(page_count)
                    .unwrap_or(u64::MAX)
                    .saturating_mul(u64::try_from(page_size).unwrap_or(u64::MAX)),
                0,
                0,
            ))
        }
    }
}

fn directory_bytes(root: &Path) -> rusqlite::Result<u64> {
    let mut pending = vec![root.to_path_buf()];
    let mut entries = 0usize;
    let mut bytes = 0u64;
    while let Some(directory) = pending.pop() {
        let read = match fs::read_dir(directory) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(crate::io_to_sql(error)),
        };
        for entry in read {
            let entry = entry.map_err(crate::io_to_sql)?;
            entries = entries.saturating_add(1);
            if entries > MAX_STATUS_WALK_ENTRIES {
                return Ok(u64::MAX);
            }
            let file_type = entry.file_type().map_err(crate::io_to_sql)?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                bytes = bytes.saturating_add(entry.metadata().map_err(crate::io_to_sql)?.len());
            }
        }
    }
    Ok(bytes)
}

pub(crate) fn reconciled_physical_bytes(cas_root: &Path) -> rusqlite::Result<u64> {
    Ok(directory_bytes(cas_root)?.saturating_add(directory_bytes(
        &cas_root.parent().unwrap_or(cas_root).join("spill"),
    )?))
}
