//! Pins and garbage collection over the CAS `blob` table.

use std::fs;

use crate::{Journal, hex_bytes, hex_string, io_to_sql, now_ns};

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
        Ok(self
            .conn
            .execute("INSERT OR IGNORE INTO pin(hash) VALUES(?1)", [raw])?
            > 0)
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
            Ok(hex_string(&raw))
        })?
        .collect()
    }

    pub fn gc(&self, options: GcOptions) -> rusqlite::Result<GcReport> {
        let mut stmt=self.conn.prepare("SELECT b.hash,b.stored_len,b.last_access_ns,EXISTS(SELECT 1 FROM output o WHERE o.hash=b.hash),EXISTS(SELECT 1 FROM pin p WHERE p.hash=b.hash) FROM blob b ORDER BY 4 ASC,b.last_access_ns ASC")?;
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
        let total = blobs.iter().map(|x| x.1).sum::<u64>();
        let cutoff = options
            .ttl
            .map(|ttl| now_ns().saturating_sub(ttl.as_nanos().min(i64::MAX as u128) as i64));
        let mut chosen = Vec::new();
        let mut chosen_bytes = 0;
        for (hash, bytes, access, referenced, pinned) in &blobs {
            if !pinned && cutoff.is_some_and(|c| *access <= c) {
                chosen.push((hash.clone(), *bytes, *referenced));
                chosen_bytes += bytes;
            }
        }
        if let Some(budget) = options.max_bytes {
            for (hash, bytes, _, referenced, pinned) in &blobs {
                if total.saturating_sub(chosen_bytes) <= budget {
                    break;
                }
                if !pinned && !chosen.iter().any(|x| x.0 == *hash) {
                    chosen.push((hash.clone(), *bytes, *referenced));
                    chosen_bytes += bytes;
                }
            }
        }
        let candidates = chosen
            .iter()
            .map(|(h, b, r)| GcBlob {
                hash: hex_string(h),
                bytes: *b,
                referenced: *r,
            })
            .collect::<Vec<_>>();
        let mut deleted = Vec::new();
        if !options.dry_run {
            for blob in &candidates {
                let path = self.blob_path(&blob.hash);
                if path.exists() {
                    let tomb = path.with_extension(format!("gc-{}", std::process::id()));
                    fs::rename(&path, &tomb).map_err(io_to_sql)?;
                    if let Err(e) = fs::remove_file(&tomb) {
                        let _ = fs::rename(&tomb, &path);
                        return Err(io_to_sql(e));
                    }
                }
                let raw = hex_bytes(&blob.hash).expect("database hash");
                self.conn.execute("DELETE FROM blob WHERE hash=?1", [raw])?;
                deleted.push(blob.clone());
            }
        }
        Ok(GcReport {
            candidates,
            reclaimed_bytes: chosen_bytes,
            remaining_bytes: total.saturating_sub(chosen_bytes),
            deleted,
        })
    }
}
