//! The content-addressed store (CAS): blake3-addressed, zstd-compressed
//! output blobs, deduplicated on disk and tracked in the `blob` table.

use std::fs;
use std::io;
use std::io::Write as _;
use std::path::PathBuf;

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::{Journal, hex_bytes, io_to_sql, now_ns};

/// zstd compression level for CAS blobs (3 = the zstd default: fast, good ratio).
const ZSTD_LEVEL: i32 = 3;

const DEFAULT_OUTPUT_HARD_CAP: usize = 256 * 1024 * 1024;
pub(crate) const TRUNCATION_MARKER: &[u8] = b"\n[shoal: output truncated; see journal metadata]\n";

#[derive(Debug, Clone, Copy)]
pub struct JournalOptions {
    pub output_hard_cap: usize,
}
impl Default for JournalOptions {
    fn default() -> Self {
        Self {
            output_hard_cap: DEFAULT_OUTPUT_HARD_CAP,
        }
    }
}

/// One captured output linked to an entry: a CAS blob reference.
#[derive(Debug, Clone)]
pub struct OutputRow {
    /// Output kind: `"stdout"`, `"stderr"`, `"value"`, or `"render"`.
    pub kind: String,
    /// blake3 hash of the (uncompressed) bytes, lowercase hex.
    pub hash: String,
    /// Length of the uncompressed bytes.
    pub len: i64,
    pub meta: Option<OutputMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputMeta {
    pub truncated: bool,
    pub original_len: u64,
    pub stored_len: u64,
}

impl Journal {
    /// Store `bytes` in the CAS (zstd-compressed, blake3-addressed, deduplicated)
    /// and link them to entry `id`. `kind` is `"stdout"`, `"stderr"`, `"value"`,
    /// or `"render"`. Returns the blake3 hash as lowercase hex.
    ///
    /// Identical bytes recorded twice produce two `output` rows but a single CAS
    /// file. The blob is written atomically (temp file + rename) before the row
    /// insert; a crash in between leaves at worst an unreferenced blob for GC.
    pub fn record_output(&self, id: i64, kind: &str, bytes: &[u8]) -> rusqlite::Result<String> {
        let (stored, meta) = if bytes.len() > self.output_hard_cap {
            let marker_len = TRUNCATION_MARKER.len().min(self.output_hard_cap);
            let keep = self.output_hard_cap.saturating_sub(marker_len);
            let mut stored = bytes[..keep].to_vec();
            stored.extend_from_slice(&TRUNCATION_MARKER[..marker_len]);
            let meta = OutputMeta {
                truncated: true,
                original_len: bytes.len() as u64,
                stored_len: stored.len() as u64,
            };
            (stored, Some(meta))
        } else {
            (bytes.to_vec(), None)
        };
        let hash = blake3::hash(&stored);
        let hex = hash.to_hex().to_string();
        let path = self.blob_path(&hex);
        if !path.exists() {
            let parent = path.parent().expect("blob path always has a parent");
            fs::create_dir_all(parent).map_err(io_to_sql)?;
            let compressed = zstd::encode_all(stored.as_slice(), ZSTD_LEVEL).map_err(io_to_sql)?;
            let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(io_to_sql)?;
            tmp.write_all(&compressed).map_err(io_to_sql)?;
            tmp.persist(&path).map_err(|e| io_to_sql(e.error))?;
        }
        let now = now_ns();
        self.conn.execute("INSERT OR IGNORE INTO blob(hash,stored_len,created_ns,last_access_ns) VALUES(?1,?2,?3,?3)",params![hash.as_bytes().as_slice(),stored.len() as i64,now])?;
        let meta_json = meta
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        self.conn.execute(
            "INSERT INTO output (entry_id, kind, hash, len, meta) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id,
                kind,
                hash.as_bytes().as_slice(),
                stored.len() as i64,
                meta_json
            ],
        )?;
        Ok(hex)
    }

    /// Fetch and decompress a CAS blob by its blake3 hex hash.
    ///
    /// Returns `Ok(None)` when the blob does not exist (including malformed hash
    /// strings, which cannot name a blob).
    pub fn read_blob(&self, hash: &str) -> rusqlite::Result<Option<Vec<u8>>> {
        // A hash that is not plain hex (or is too short to shard) cannot address
        // a blob; this also guards against path traversal.
        if hash.len() < 4 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(None);
        }
        let compressed = match fs::read(self.blob_path(hash)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_to_sql(e)),
        };
        let bytes = zstd::decode_all(compressed.as_slice()).map_err(io_to_sql)?;
        if let Ok(raw) = hex_bytes(hash) {
            self.conn.execute(
                "UPDATE blob SET last_access_ns=?1 WHERE hash=?2",
                params![now_ns(), raw],
            )?;
        }
        Ok(Some(bytes))
    }

    /// Absolute path of the CAS blob for a hex hash:
    /// `<cas>/<hex[0..2]>/<hex[2..4]>/<hex>.zst`.
    pub(crate) fn blob_path(&self, hex: &str) -> PathBuf {
        self.cas_root
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(format!("{hex}.zst"))
    }
}
