//! Pins and garbage collection over the CAS `blob` table.

use std::fs::{self, File, OpenOptions};
use std::io;

use rusqlite::{Transaction, TransactionBehavior};

use crate::storage::DB_WRITE_RESERVE_BYTES;
use crate::{Journal, hash_string, hex_bytes, io_to_sql, now_ns};

#[derive(Debug, Clone, Copy, Default)]
pub struct GcOptions {
    pub ttl: Option<std::time::Duration>,
    pub max_bytes: Option<u64>,
    pub dry_run: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcBlob {
    pub hash: String,
    pub bytes: u64,
    pub referenced: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GcReport {
    pub candidates: Vec<GcBlob>,
    pub deleted: Vec<GcBlob>,
    pub reclaimed_bytes: u64,
    pub remaining_bytes: u64,
}

impl Journal {
    pub fn pin(&self, hash: &str) -> rusqlite::Result<bool> {
        let raw = hex_bytes(hash)
            .map_err(|_| rusqlite::Error::InvalidParameterName("invalid hash".into()))?;
        self.with_database_admission(DB_WRITE_RESERVE_BYTES, |tx| {
            Ok(tx.execute("INSERT OR IGNORE INTO pin(hash) VALUES(?1)", [raw])? > 0)
        })
    }

    pub fn unpin(&self, hash: &str) -> rusqlite::Result<bool> {
        let raw = hex_bytes(hash)
            .map_err(|_| rusqlite::Error::InvalidParameterName("invalid hash".into()))?;
        Ok(self.conn.execute("DELETE FROM pin WHERE hash=?1", [raw])? > 0)
    }

    pub fn pins(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT hash FROM pin ORDER BY hash")?;
        stmt.query_map([], |r| {
            let raw: Vec<u8> = r.get(0)?;
            hash_string(&raw, 0)
        })?
        .collect()
    }

    /// Every blob currently protected from GC, including permanent operator
    /// pins and automatic live-value leases. Unlike [`Journal::pins`], this is
    /// observational: a live lease cannot be removed by manual `unpin`.
    pub fn protected_hashes(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT hash FROM pin UNION SELECT hash FROM pin_lease ORDER BY hash")?;
        stmt.query_map([], |r| {
            let raw: Vec<u8> = r.get(0)?;
            hash_string(&raw, 0)
        })?
        .collect()
    }

    pub fn gc(&self, options: GcOptions) -> rusqlite::Result<GcReport> {
        // Serialize candidate selection with pin/unpin and CAS admission so a
        // blob cannot become pinned after selection but before deletion.
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if !options.dry_run {
            self.reap_stale_pin_leases(&tx)?;
        }
        let mut stmt=tx.prepare("SELECT b.hash,b.stored_len,b.last_access_ns,EXISTS(SELECT 1 FROM output o WHERE o.hash=b.hash),(EXISTS(SELECT 1 FROM pin p WHERE p.hash=b.hash) OR EXISTS(SELECT 1 FROM pin_lease l WHERE l.hash=b.hash)) FROM blob b ORDER BY 4 ASC,b.last_access_ns ASC")?;
        let blobs = stmt
            .query_map([], |r| {
                let len: i64 = r.get(1)?;
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    len.max(0) as u64,
                    r.get::<_, i64>(2)?,
                    r.get::<_, bool>(3)?,
                    r.get::<_, bool>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        let total = blobs
            .iter()
            .fold(0u64, |total, blob| total.saturating_add(blob.1));
        let cutoff = options
            .ttl
            .map(|ttl| now_ns().saturating_sub(ttl.as_nanos().min(i64::MAX as u128) as i64));
        let mut chosen = Vec::new();
        let mut chosen_bytes = 0u64;
        for (hash, bytes, access, referenced, pinned) in &blobs {
            if !pinned && cutoff.is_some_and(|c| *access <= c) {
                chosen.push((hash.clone(), *bytes, *referenced));
                chosen_bytes = chosen_bytes.saturating_add(*bytes);
            }
        }
        if let Some(budget) = options.max_bytes {
            for (hash, bytes, _, referenced, pinned) in &blobs {
                if total.saturating_sub(chosen_bytes) <= budget {
                    break;
                }
                if !pinned && !chosen.iter().any(|x| x.0 == *hash) {
                    chosen.push((hash.clone(), *bytes, *referenced));
                    chosen_bytes = chosen_bytes.saturating_add(*bytes);
                }
            }
        }
        let candidates = chosen
            .iter()
            .map(|(h, b, r)| {
                Ok(GcBlob {
                    hash: hash_string(h, 0)?,
                    bytes: *b,
                    referenced: *r,
                })
            })
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut deleted = Vec::new();
        let mut tombs = Vec::new();
        if !options.dry_run {
            let mutation = (|| {
                for (index, blob) in candidates.iter().enumerate() {
                    let path = self.blob_path(&blob.hash);
                    if path.exists() {
                        let tomb =
                            path.with_extension(format!("gc-{}-{index}", std::process::id()));
                        fs::rename(&path, &tomb).map_err(io_to_sql)?;
                        tombs.push((path, tomb));
                    }
                    let raw = hex_bytes(&blob.hash).map_err(|_| {
                        rusqlite::Error::InvalidParameterName("invalid database hash".into())
                    })?;
                    tx.execute("DELETE FROM blob WHERE hash=?1", [raw])?;
                    deleted.push(blob.clone());
                }
                Ok::<(), rusqlite::Error>(())
            })();
            if let Err(error) = mutation {
                drop(tx);
                restore_tombs(&tombs);
                return Err(error);
            }
        }
        if let Err(error) = tx.commit() {
            restore_tombs(&tombs);
            return Err(error);
        }
        for (path, tomb) in tombs {
            if let Err(error) = fs::remove_file(&tomb) {
                // The metadata deletion committed, so restoring the verified
                // content-addressed path is safer than losing the bytes. It is
                // now an orphan and remains visible to physical CAS admission.
                let _ = fs::rename(&tomb, &path);
                return Err(io_to_sql(error));
            }
        }
        Ok(GcReport {
            candidates,
            reclaimed_bytes: chosen_bytes,
            remaining_bytes: total.saturating_sub(chosen_bytes),
            deleted,
        })
    }

    /// Remove leases whose owning handle/value graph is no longer alive. The
    /// database transaction serializes this with lease admission/release; the
    /// OS lock distinguishes a crashed owner from one in another process.
    fn reap_stale_pin_leases(&self, tx: &Transaction<'_>) -> rusqlite::Result<()> {
        let mut statement = tx.prepare("SELECT DISTINCT owner FROM pin_lease")?;
        let owners = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let lease_dir = self
            .cas_root
            .parent()
            .unwrap_or(self.cas_root.as_path())
            .join("leases");
        for owner in owners {
            // Malformed authority rows stay pinned. Never turn database
            // corruption into a path traversal or an accidental unpin.
            if owner.is_empty()
                || owner.len() > 128
                || !owner
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
            {
                continue;
            }
            let lock_path = lease_dir.join(format!("{owner}.lock"));
            let stale = match OpenOptions::new().read(true).write(true).open(&lock_path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => true,
                Err(_) => false,
                Ok(lock) => match lock.try_lock() {
                    Ok(()) => {
                        let _ = File::unlock(&lock);
                        true
                    }
                    Err(std::fs::TryLockError::WouldBlock) => false,
                    Err(_) => false,
                },
            };
            if stale {
                tx.execute("DELETE FROM pin_lease WHERE owner=?1", [&owner])?;
                let _ = fs::remove_file(lock_path);
            }
        }
        Ok(())
    }
}

fn restore_tombs(tombs: &[(std::path::PathBuf, std::path::PathBuf)]) {
    for (path, tomb) in tombs.iter().rev() {
        let _ = fs::rename(tomb, path);
    }
}
